//! Isolated direct-QEMU process manager: one QEMU process per VM.
//!
//! A guest's hardware is the domain XML the SPA writes (`domainxml`), stored as
//! the durable definition. The launch argv, every hotplug path and the taps are
//! derived from it; there is no second copy of the devices here.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};
use uuid::Uuid;

use crate::domainxml::{
    self, BootDev, Cpu, Disk, DiskBus, DiskDevice, Firmware, Graphics, Hostdev, IfaceKind,
    IfaceModel, Interface, SerialKind, Video, VideoModel,
};
use crate::hostnet::SYS_NET;

pub use crate::domainxml::DiskFormat;
pub use crate::hostnet::is_bridge;

#[derive(Clone, Debug)]
pub struct Manager {
    data_dir: PathBuf,
    state_root: PathBuf,
    qemu: PathBuf,
    lock: std::sync::Arc<Mutex<()>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VmConfig {
    pub id: String,
    /// What the user calls it: the IPAM name (DNS/DHCP) and what `list` shows.
    #[serde(default)]
    pub title: String,
    /// Start this guest when the daemon starts. Persisted; the daemon does not
    /// start guests yet.
    #[serde(default)]
    pub autostart: bool,
    /// The SPA's libvirt-shaped domain XML, exactly as `domainxml` renders it.
    pub xml: String,
}

impl VmConfig {
    /// The name the store and DNS use: the title, else the id.
    pub fn name(&self) -> &str {
        if self.title.is_empty() {
            &self.id
        } else {
            &self.title
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    config: VmConfig,
    /// The host taps of the current run. Kept here rather than re-derived from
    /// the definition: a hotplug or an XML edit can drop a NIC while QEMU still
    /// holds its tap, and only this list knows to remove it at stop.
    #[serde(default)]
    taps: Vec<String>,
    pid: Option<u32>,
    start_ticks: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Stopped,
    Running,
    Paused,
    Unknown,
}

impl State {
    /// The wire spelling the SPA and the web server already read.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Unknown => "unknown",
        }
    }
}

/// A point-in-time sample of one guest, shaped like the libvirt `get_info`
/// answer the SPA's summary reads.
#[derive(Clone, Debug, Serialize)]
pub struct Stats {
    pub state: &'static str,
    pub cpu_time_ns: u64,
    pub vcpus: u16,
    pub memory_kib: u64,
    pub max_memory_kib: u64,
    pub rss_kib: Option<u64>,
    pub usable_kib: Option<u64>,
}

/// One NIC as the Network tab and the summary read it. `network` is the bridge
/// the definition names; the API maps it to the stored network key, because
/// only the IPAM store knows which network owns a bridge.
#[derive(Clone, Debug, Serialize)]
pub struct InterfaceStats {
    pub dev: Option<String>,
    pub mac: Option<String>,
    pub model: &'static str,
    pub network: Option<String>,
    /// Rate limits from `<bandwidth>`, in KiB/s (`average`).
    pub rx_kib: Option<u64>,
    pub tx_kib: Option<u64>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// The guest's display server, in the shape `parse_graphics` returned.
#[derive(Clone, Debug, Serialize)]
pub struct GraphicsInfo {
    pub kind: &'static str,
    pub host: String,
    pub port: u16,
}

impl Manager {
    pub fn new(
        data_dir: impl Into<PathBuf>,
        state_root: impl Into<PathBuf>,
    ) -> Result<Self, String> {
        Self::with_qemu(data_dir, state_root, "/usr/bin/qemu-system-x86_64")
    }
    pub fn with_qemu(
        data_dir: impl Into<PathBuf>,
        state_root: impl Into<PathBuf>,
        qemu: impl Into<PathBuf>,
    ) -> Result<Self, String> {
        let manager = Self {
            data_dir: data_dir.into(),
            state_root: state_root.into(),
            qemu: qemu.into(),
            lock: Default::default(),
        };
        manager.reconcile()?;
        Ok(manager)
    }
    fn reconcile(&self) -> Result<(), String> {
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let mut changed = false;
        for rec in all.values_mut() {
            changed |= self.clear_dead(rec);
        }
        if changed {
            self.save(&all)?;
        }
        Ok(())
    }
    /// Delete the taps of every record whose process is gone. A guest that
    /// powered itself off or was killed outside the manager never went through
    /// `powerdown`, so this is the sweep the supervisor runs every pass.
    /// A live process's tap is never touched.
    pub fn sweep_dead_taps(&self) -> Result<(), String> {
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let mut changed = false;
        for rec in all.values_mut() {
            changed |= self.clear_dead(rec);
        }
        if changed {
            self.save(&all)?;
        }
        Ok(())
    }
    /// Forget a dead record's process identity and remove its NIC taps. The
    /// process identity is pid + start ticks + executable, so a reused pid is
    /// not mistaken for the guest. Returns whether there was anything to
    /// forget: the sweep runs every pass and must not rewrite the file when
    /// nothing changed.
    fn clear_dead(&self, rec: &mut Record) -> bool {
        if self.identity(rec) {
            return false;
        }
        // The guest is gone, so nothing can use its TPM any more. A live
        // guest's swtpm is never touched (the early return above).
        if let Err(e) = stop_swtpm(&self.vm_dir(&rec.config.id)) {
            tracing::warn!("qemu: stopping swtpm for {}: {e}", rec.config.id);
        }
        let had_identity = rec.pid.is_some() || rec.start_ticks.is_some();
        let taps = std::mem::take(&mut rec.taps);
        // Taps are removed even when the identity was cleared by an earlier
        // pass: a guest that powered itself off can leave one at any time.
        cleanup_taps(&taps);
        rec.pid = None;
        rec.start_ticks = None;
        had_identity || !taps.is_empty()
    }
    fn records(&self) -> PathBuf {
        self.data_dir.join("qemu-vms.json")
    }
    fn read(&self) -> Result<BTreeMap<String, Record>, String> {
        match fs::read(self.records()) {
            Ok(b) => {
                let records: BTreeMap<String, Record> =
                    serde_json::from_slice(&b).map_err(|e| e.to_string())?;
                // Never silently choose which identity wins if a record's embedded id
                // disagrees with its durable map key. Legacy non-UUID IDs remain readable.
                if records.iter().any(|(key, record)| key != &record.config.id) {
                    return Err("QEMU VM metadata key does not match record id".into());
                }
                Ok(records)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e.to_string()),
        }
    }
    fn save(&self, records: &BTreeMap<String, Record>) -> Result<(), String> {
        private_dir(&self.data_dir)?;
        let p = self.records();
        let tmp = p.with_extension("json.tmp");
        if tmp.exists() {
            fs::remove_file(&tmp).map_err(|e| format!("remove stale metadata temp: {e}"))?;
        }
        let bytes = serde_json::to_vec_pretty(records).map_err(|e| e.to_string())?;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        f.write_all(&bytes)
            .and_then(|_| f.sync_all())
            .map_err(|e| e.to_string())?;
        fs::rename(&tmp, &p).map_err(|e| e.to_string())?;
        fs::File::open(&self.data_dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| e.to_string())
    }
    fn valid_id(id: &str) -> bool {
        Uuid::parse_str(id).is_ok_and(|uuid| uuid.hyphenated().to_string() == id)
    }

    /// Store a new definition. The XML is validated and normalized first: the
    /// firmware paths are resolved for this host, every disk is named by its
    /// canonical path, and a NIC without a MAC is given one, so two daemon
    /// starts and the DHCP reservation agree about it.
    pub fn define(&self, xml: &str) -> Result<VmConfig, String> {
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let all = self.read()?;
        let d = self.prepare(xml, &all, None)?;
        if all.contains_key(&d.id) {
            return Err("VM already exists".into());
        }
        let config = VmConfig {
            id: d.id.clone(),
            title: d.title.clone(),
            autostart: false,
            xml: domainxml::render(&d),
        };
        let mut all = all;
        all.insert(
            d.id.clone(),
            Record {
                config: config.clone(),
                taps: vec![],
                pid: None,
                start_ticks: None,
            },
        );
        self.save(&all)?;
        Ok(config)
    }

    /// Fold an edited definition into the stored one. The identity (name, UUID,
    /// title when omitted) survives; autostart and the process identity do too.
    pub fn update_xml(&self, id: &str, xml: &str) -> Result<(), String> {
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let old = all.get(id).ok_or("unknown QEMU VM")?.config.xml.clone();
        let d = self.prepare(xml, &all, Some(&old))?;
        let rec = all.get_mut(id).ok_or("unknown QEMU VM")?;
        rec.config.title = d.title.clone();
        rec.config.xml = domainxml::render(&d);
        self.save(&all)
    }

    pub fn xml(&self, id: &str) -> Result<String, String> {
        self.read()?
            .remove(id)
            .map(|r| r.config.xml)
            .ok_or_else(|| "unknown QEMU VM".into())
    }

    /// Change the display name. The identity — `<name>`, `<uuid>` and therefore
    /// the URL, the MACs and the taps — is untouched: only `<title>` and the
    /// stored title move, so a rename never re-defines a guest.
    pub fn set_title(&self, id: &str, title: &str) -> Result<String, String> {
        let title = display_title(title)?;
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get_mut(id).ok_or("unknown QEMU VM")?;
        let mut d = domainxml::parse(&rec.config.xml)?;
        d.title = title.clone();
        rec.config.title = title.clone();
        rec.config.xml = domainxml::render(&d);
        self.save(&all)?;
        Ok(title)
    }

    #[allow(dead_code)] // no autostart UI route yet; the field is persisted and listed
    pub fn autostart(&self, id: &str) -> bool {
        self.read()
            .ok()
            .and_then(|all| all.get(id).map(|r| r.config.autostart))
            .unwrap_or(false)
    }

    #[allow(dead_code)] // no autostart UI route yet; the field is persisted and listed
    pub fn set_autostart(&self, id: &str, on: bool) -> Result<(), String> {
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get_mut(id).ok_or("unknown QEMU VM")?;
        if rec.config.autostart == on {
            return Ok(());
        }
        rec.config.autostart = on;
        self.save(&all)
    }

    /// Read a definition for storage: parse (or merge), give missing NICs a
    /// MAC, canonicalize every disk and refuse a file another guest holds.
    fn prepare(
        &self,
        xml: &str,
        all: &BTreeMap<String, Record>,
        old: Option<&str>,
    ) -> Result<domainxml::Domain, String> {
        let xml = crate::firmware::resolve(xml);
        let mut d = match old {
            Some(old) => domainxml::merge(old, &xml)?,
            None => domainxml::parse(&xml)?,
        };
        if let (Firmware::Uefi, Some(_)) = (d.firmware, d.nvram.as_ref()) {
            // The editor writes libvirt's variable-store path. This backend
            // keeps the guest's own beside its QMP socket.
            d.nvram = Some(self.vm_dir(&d.id).join("nvram.fd"));
        }
        let used = used_macs(all, &d.id);
        for (i, iface) in d.interfaces.iter_mut().enumerate() {
            if iface.mac.is_none() {
                iface.mac = Some(mac_for(&used, &d.id, &format!("net{i}"), "")?);
            }
        }
        let attached = attached_files(all, Some(&d.id));
        let mut seen = BTreeSet::new();
        for disk in &mut d.disks {
            let Some(source) = &disk.source else {
                continue;
            };
            let canonical = check_disk_file(source)?;
            if attached.contains(&canonical) {
                return Err(format!(
                    "disk {} is already attached to a VM",
                    canonical.display()
                ));
            }
            if !seen.insert(canonical.clone()) {
                return Err(format!(
                    "the same file is attached twice: {}",
                    canonical.display()
                ));
            }
            disk.source = Some(canonical);
        }
        Ok(d)
    }

    /// Point one stored NIC at another bridge (or make it user-mode). Stopped
    /// only: a live guest's tap is still on the old bridge.
    pub fn set_nic_bridge(&self, id: &str, mac: &str, bridge: Option<&str>) -> Result<(), String> {
        let mac = domainxml::normalize_mac(mac)?;
        if let Some(bridge) = bridge
            && !crate::hostnet::valid_name(bridge)
        {
            return Err("invalid bridge name".into());
        }
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get_mut(id).ok_or("unknown QEMU VM")?;
        if self.identity(rec) {
            return Err("VM must be stopped to change networking".into());
        }
        let mut d = domainxml::parse(&rec.config.xml)?;
        let iface = d
            .interfaces
            .iter_mut()
            .find(|i| i.mac.as_deref() == Some(mac.as_str()))
            .ok_or("unknown QEMU NIC")?;
        // The tap's name is derived from the MAC, so it stays the same; but a
        // tap left on the old bridge must not make the next start fail, and
        // `ensure_tap` refuses to move one that another bridge owns.
        if matches!(iface.kind, IfaceKind::Bridge(_)) {
            delete_nic_tap(id, &mac);
        }
        iface.kind = match bridge {
            Some(b) => IfaceKind::Bridge(b.to_string()),
            None => IfaceKind::User,
        };
        rec.config.xml = domainxml::render(&d);
        self.save(&all)
    }

    /// Attach a NIC. Its MAC (and therefore its tap and QMP ids) is generated
    /// here — or taken from `mac`, when the caller set one — and returned; the
    /// interface is appended to the definition.
    pub fn add_nic(&self, id: &str, bridge: Option<&str>, mac: &str) -> Result<String, String> {
        let mac = mac.trim();
        let mut iface = Interface {
            kind: bridge
                .map(|b| IfaceKind::Bridge(b.to_string()))
                .unwrap_or(IfaceKind::User),
            model: IfaceModel::Virtio,
            mac: (!mac.is_empty()).then(|| mac.to_string()),
            bandwidth: Default::default(),
        };
        self.attach_nic(id, &mut iface).map(|(mac, _)| mac)
    }

    /// Attach a NIC from an `<interface>` element (the edit form's XML), so the
    /// caller's model and MAC are what the definition gets. Returns the chosen
    /// MAC and whether the device went into a running guest.
    pub fn add_nic_xml(&self, id: &str, xml: &str) -> Result<(String, bool), String> {
        let mut iface = domainxml::parse_interface_xml(xml)?;
        self.attach_nic(id, &mut iface)
    }

    fn attach_nic(&self, id: &str, iface: &mut Interface) -> Result<(String, bool), String> {
        if let IfaceKind::Bridge(bridge) = &iface.kind
            && !crate::hostnet::valid_name(bridge)
        {
            return Err("invalid bridge name".into());
        }
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get(id).ok_or("unknown QEMU VM")?;
        let mut d = domainxml::parse(&rec.config.xml)?;
        let used = used_macs(&all, id);
        let mac = mac_for(
            &used,
            id,
            &format!("net{}", d.interfaces.len()),
            iface.mac.as_deref().unwrap_or(""),
        )?;
        iface.mac = Some(mac.clone());
        let live = self.identity(rec);
        let netdev = netdev_id(&mac);
        let device = nic_device_id(&mac);
        let mut tap = None;
        if live {
            let sock = self.vm_dir(id).join("qmp.sock");
            let add = match &iface.kind {
                IfaceKind::User => serde_json::json!({"type":"user", "id":netdev}),
                IfaceKind::Bridge(bridge) => {
                    let name = tap_name(id, &mac);
                    // Before QMP: a device whose host tap is missing must never
                    // be published to the guest.
                    ensure_tap(&name, bridge)?;
                    tap = Some(name.clone());
                    serde_json::json!({
                        "type":"tap", "id":netdev, "ifname":name,
                        "script":"no", "downscript":"no"
                    })
                }
            };
            if let Err(e) = qmp_execute(&sock, "netdev_add", add) {
                if let Some(tap) = &tap {
                    cleanup_taps(std::slice::from_ref(tap));
                }
                return Err(e);
            }
            let device = serde_json::json!({
                "driver": iface.model.driver(), "id":device, "netdev":netdev, "mac":mac
            });
            if let Err(e) = qmp_execute(&sock, "device_add", device) {
                let _ = qmp_execute(&sock, "netdev_del", serde_json::json!({"id":netdev}));
                if let Some(tap) = &tap {
                    cleanup_taps(std::slice::from_ref(tap));
                }
                return Err(e);
            }
        }
        d.interfaces.push(iface.clone());
        let rec = all.get_mut(id).unwrap();
        rec.config.xml = domainxml::render(&d);
        if let Some(t) = &tap {
            rec.taps.push(t.clone());
        }
        if let Err(e) = self.save(&all) {
            if live {
                let sock = self.vm_dir(id).join("qmp.sock");
                let _ = qmp_execute(&sock, "device_del", serde_json::json!({"id":device}));
                let _ = qmp_execute(&sock, "netdev_del", serde_json::json!({"id":netdev}));
                // QEMU may hold the device until the asynchronous `device_del`
                // completes, so the tap is not deleted here. The definition did
                // not persist either, so record the tap separately; otherwise
                // nothing would ever remove it.
                if let Some(tap) = &tap
                    && let Ok(mut old) = self.read()
                {
                    if let Some(rec) = old.get_mut(id) {
                        rec.taps.push(tap.clone());
                    }
                    let _ = self.save(&old);
                }
            }
            return Err(format!("live NIC added but persistence failed: {e}"));
        }
        Ok((mac, live))
    }

    /// Detach the NIC with this MAC. A live guest's `device_del` is a request,
    /// so the caller is told it is pending; the tap itself is removed when the
    /// guest stops, because QEMU may still hold it.
    pub fn remove_nic(&self, id: &str, mac: &str) -> Result<bool, String> {
        let mac = domainxml::normalize_mac(mac)?;
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get_mut(id).ok_or("unknown QEMU VM")?;
        let mut d = domainxml::parse(&rec.config.xml)?;
        let Some(index) = d
            .interfaces
            .iter()
            .position(|i| i.mac.as_deref() == Some(mac.as_str()))
        else {
            return Err("unknown QEMU NIC".into());
        };
        let live = self.identity(rec);
        if live {
            let sock = self.vm_dir(id).join("qmp.sock");
            qmp_execute(
                &sock,
                "device_del",
                serde_json::json!({"id": nic_device_id(&mac)}),
            )?;
            // QMP `device_del` completes asynchronously; the tap stays in the
            // record so stop/sweep removes it once QEMU has let go.
        } else {
            delete_nic_tap(id, &mac);
        }
        d.interfaces.remove(index);
        rec.config.xml = domainxml::render(&d);
        self.save(&all)?;
        Ok(live)
    }

    /// Set one NIC's `<bandwidth>` in the stored definition. Returns the tap to
    /// shape when the guest is running and the NIC is on a bridge: `tc` is the
    /// kernel side, so only the caller with the host commands applies it. A
    /// stopped (or user-mode) NIC keeps the definition and nothing else.
    pub fn set_nic_bandwidth(
        &self,
        id: &str,
        mac: &str,
        rx_kib: Option<u64>,
        tx_kib: Option<u64>,
    ) -> Result<Option<String>, String> {
        let mac = domainxml::normalize_mac(mac)?;
        let rate = |kib: Option<u64>| {
            kib.filter(|k| *k > 0)
                .map(|average| crate::domainxml::Rate {
                    average: Some(average),
                    ..Default::default()
                })
        };
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get_mut(id).ok_or("unknown QEMU VM")?;
        let mut d = domainxml::parse(&rec.config.xml)?;
        let iface = d
            .interfaces
            .iter_mut()
            .find(|i| i.mac.as_deref() == Some(mac.as_str()))
            .ok_or("unknown QEMU NIC")?;
        iface.bandwidth = crate::domainxml::Bandwidth {
            inbound: rate(rx_kib),
            outbound: rate(tx_kib),
        };
        let bridged = matches!(&iface.kind, IfaceKind::Bridge(_));
        let live = self.identity(rec);
        rec.config.xml = domainxml::render(&d);
        self.save(&all)?;
        Ok((live && bridged).then(|| tap_name(id, &mac)))
    }

    /// Attach a disk to a stopped guest by persisting it, or to a running one by
    /// adding its block nodes and device through QMP first. The target device
    /// name (`vdb`) is derived from the definition and returned.
    pub fn add_disk(
        &self,
        id: &str,
        path: &Path,
        format: DiskFormat,
    ) -> Result<(String, PathBuf), String> {
        let path = check_disk_file(path)?;
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get(id).ok_or("unknown QEMU VM")?;
        let mut d = domainxml::parse(&rec.config.xml)?;
        // One file backs one device: the same image attached twice is written
        // by two guests and corrupts both.
        if attached_files(&all, Some(id)).contains(&path)
            || d.disks.iter().any(|x| x.source.as_deref() == Some(&path))
        {
            return Err("disk path is already attached to a VM".into());
        }
        let target = next_disk_target(&d)?;
        let live = self.identity(rec);
        let sock = self.vm_dir(id).join("qmp.sock");
        if live {
            let file_node = format!("file-{target}");
            let drive_node = format!("drive-{target}");
            qmp_execute(
                &sock,
                "blockdev-add",
                serde_json::json!({
                    "driver":"file", "filename":path.to_str().ok_or("disk path is not valid UTF-8")?,
                    "node-name":&file_node
                }),
            )?;
            if let Err(e) = qmp_execute(
                &sock,
                "blockdev-add",
                serde_json::json!({
                    "driver":format.as_str(), "file":&file_node, "node-name":&drive_node
                }),
            ) {
                let _ = qmp_execute(
                    &sock,
                    "blockdev-del",
                    serde_json::json!({"node-name":&file_node}),
                );
                return Err(e);
            }
            if let Err(e) = qmp_execute(
                &sock,
                "device_add",
                serde_json::json!({
                    "driver":"virtio-blk-pci", "id":&target, "drive":&drive_node
                }),
            ) {
                let _ = qmp_execute(
                    &sock,
                    "blockdev-del",
                    serde_json::json!({"node-name":&drive_node}),
                );
                let _ = qmp_execute(
                    &sock,
                    "blockdev-del",
                    serde_json::json!({"node-name":&file_node}),
                );
                return Err(e);
            }
        }
        d.disks.push(Disk {
            device: DiskDevice::Disk,
            bus: DiskBus::Virtio,
            format,
            target: target.clone(),
            source: Some(path.clone()),
            readonly: false,
        });
        all.get_mut(id).unwrap().config.xml = domainxml::render(&d);
        if let Err(e) = self.save(&all) {
            if live {
                // The device still references the node until the asynchronous
                // `device_del` completes, so a `blockdev-del` may be refused;
                // the guest keeps the disk rather than having it pulled away.
                let _ = qmp_execute(&sock, "device_del", serde_json::json!({"id":&target}));
                let _ = qmp_execute(
                    &sock,
                    "blockdev-del",
                    serde_json::json!({"node-name":format!("drive-{target}")}),
                );
                let _ = qmp_execute(
                    &sock,
                    "blockdev-del",
                    serde_json::json!({"node-name":format!("file-{target}")}),
                );
            }
            return Err(format!("disk added but persistence failed: {e}"));
        }
        Ok((target, path))
    }

    /// Detach the disk whose `<target dev>` is `target`. A live guest's
    /// `device_del` is a request, so `pending` reports it. The file is never
    /// deleted: it holds the guest's data, not the device's identity.
    pub fn remove_disk(&self, id: &str, target: &str) -> Result<bool, String> {
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get_mut(id).ok_or("unknown QEMU VM")?;
        let mut d = domainxml::parse(&rec.config.xml)?;
        let Some(index) = d.disks.iter().position(|x| x.target == target) else {
            return Err("unknown QEMU disk".into());
        };
        let live = self.identity(rec);
        if live {
            let sock = self.vm_dir(id).join("qmp.sock");
            qmp_execute(&sock, "device_del", serde_json::json!({"id": target}))?;
        }
        d.disks.remove(index);
        rec.config.xml = domainxml::render(&d);
        // A failed save keeps the old file: the caller sees the error and the
        // disk is still in the definition (it may be live until the VM stops).
        self.save(&all)?;
        Ok(live)
    }

    pub fn list(&self) -> Result<Vec<VmConfig>, String> {
        Ok(self.read()?.into_values().map(|r| r.config).collect())
    }
    fn vm_dir(&self, id: &str) -> PathBuf {
        self.state_root.join(id)
    }

    pub fn start(&self, id: &str) -> Result<(), String> {
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        if !Self::valid_id(id) {
            return Err("invalid VM id".into());
        }
        let mut all = self.read()?;
        if !all.contains_key(id) {
            return Err("unknown QEMU VM".into());
        }
        if self.identity(all.get(id).unwrap()) {
            return Err("VM already running".into());
        }
        let config = all.get(id).unwrap().config.clone();
        let d = domainxml::parse(&config.xml)?;
        // A disk was validated when it was defined; re-check the stored path
        // now, or a symlink swapped in since would be followed by QEMU.
        for disk in &d.disks {
            let Some(source) = &disk.source else {
                continue;
            };
            if check_disk_file(source)? != *source {
                return Err(format!(
                    "disk '{}' changed since it was defined; re-add it",
                    disk.target
                ));
            }
        }
        let dir = self.vm_dir(id);
        private_dir(&self.state_root)?;
        private_dir(&dir)?;
        prepare_nvram(&d)?;
        let sock = dir.join("qmp.sock");
        if sock.as_os_str().len() > 100 {
            return Err("QMP socket path exceeds Unix socket limit".into());
        }
        let _ = fs::remove_file(&sock);
        // A killed run leaves the serial server socket behind, and QEMU refuses
        // to bind an existing file.
        let _ = fs::remove_file(dir.join("serial.sock"));
        // Build the argv before starting swtpm: a definition QEMU would refuse
        // must not leave a TPM emulator running with no guest.
        let argv = self.argv(id, &d)?;
        let tpm = d.tpm.is_some();
        if tpm {
            // The TPM is a sibling process of QEMU: it must be up before QEMU
            // connects to its control socket, and stopped with the guest.
            self.ensure_swtpm(&d, &dir)?;
        }
        // Taps next: a bridge NIC whose host interface is missing must fail the
        // start, not hand QEMU a device with nowhere to send packets.
        let mut created: Vec<String> = Vec::new();
        let mut taps: Vec<String> = Vec::new();
        for iface in &d.interfaces {
            let IfaceKind::Bridge(bridge) = &iface.kind else {
                continue;
            };
            let mac = iface
                .mac
                .as_deref()
                .ok_or("a bridge NIC has no MAC address")?;
            let tap = tap_name(id, mac);
            let existed = Path::new(SYS_NET).join(&tap).exists();
            if let Err(e) = ensure_tap(&tap, bridge) {
                if tpm {
                    let _ = stop_swtpm(&dir);
                }
                cleanup_taps(&created);
                return Err(e);
            }
            if !existed {
                created.push(tap.clone());
            }
            taps.push(tap);
        }
        let mut child = match Command::new(&self.qemu)
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                if tpm {
                    let _ = stop_swtpm(&dir);
                }
                cleanup_taps(&created);
                return Err(format!("launch QEMU: {e}"));
            }
        };
        let pid = child.id();
        if fs::canonicalize(format!("/proc/{pid}/exe")).ok().as_deref()
            != fs::canonicalize(&self.qemu).ok().as_deref()
        {
            let _ = child.kill();
            let _ = child.wait();
            if tpm {
                let _ = stop_swtpm(&dir);
            }
            cleanup_taps(&created);
            return Err("QEMU executable identity mismatch".into());
        }
        let ticks = match process_ticks(pid) {
            Some(ticks) => ticks,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                if tpm {
                    let _ = stop_swtpm(&dir);
                }
                cleanup_taps(&created);
                return Err("QEMU exited before process identity was recorded".into());
            }
        };
        {
            let rec = all.get_mut(id).unwrap();
            rec.pid = Some(pid);
            rec.start_ticks = Some(ticks);
            rec.taps = taps;
        }
        if let Err(e) = self.save(&all) {
            let _ = child.kill();
            let _ = child.wait();
            if tpm {
                let _ = stop_swtpm(&dir);
            }
            cleanup_taps(&created);
            return Err(e);
        }
        // Do not publish a stale PID if QEMU died while metadata was being saved.
        if !self.identity(all.get(id).unwrap()) || qmp(&sock, "query-status").is_err() {
            let _ = child.kill();
            let _ = child.wait();
            if tpm {
                let _ = stop_swtpm(&dir);
            }
            cleanup_taps(&created);
            let rec = all.get_mut(id).unwrap();
            rec.pid = None;
            rec.start_ticks = None;
            // The taps stay recorded with no process: the next sweep removes
            // them, including one a previous run left behind and this start
            // reused (which `created` does not name).
            let _ = self.save(&all);
            return Err("QEMU failed readiness check".into());
        }
        drop(child);
        Ok(())
    }

    /// The QEMU launch argv for a stored guest: pure apart from reading the
    /// record and checking that a TPM's swtpm socket exists, so the device
    /// shape is asserted without QEMU or root. Taps are created by `start`.
    /// Test-only: `start` builds the argv before it starts swtpm.
    #[cfg(test)]
    pub fn args(&self, id: &str) -> Result<Vec<String>, String> {
        let record = self.read()?.remove(id).ok_or("unknown QEMU VM")?;
        let d = domainxml::parse(&record.config.xml)?;
        let args = self.argv(&record.config.id, &d)?;
        if d.tpm.is_some() {
            let ctrl = self.vm_dir(&record.config.id).join("swtpm.sock");
            // QEMU's tpm emulator backend is a chardev into swtpm's control
            // socket; without a running swtpm the chardev would point at
            // nothing and the guest would fail at start with a QEMU error that
            // names neither swtpm nor the definition.
            if !is_socket(&ctrl) {
                return Err(format!(
                    "TPM needs a running swtpm: no control socket at {}",
                    ctrl.display()
                ));
            }
        }
        Ok(args)
    }

    /// Start the guest's swtpm before QEMU, unless it is already up. State is
    /// per guest, so a live socket from an earlier run (a daemon restart) is
    /// reused rather than replaced.
    fn ensure_swtpm(&self, d: &domainxml::Domain, dir: &Path) -> Result<(), String> {
        let Some(tpm) = &d.tpm else {
            return Ok(());
        };
        let sock = dir.join("swtpm.sock");
        let pid_file = dir.join("swtpm.pid");
        let state = dir.join("tpm");
        if socket_live(&sock) {
            return Ok(());
        }
        // A pid file whose socket never came up, or a stale socket file, must
        // not make the new swtpm fail to bind.
        stop_swtpm(dir).map_err(|e| format!("restart swtpm: {e}"))?;
        private_dir(&state)?;
        let argv = swtpm_argv(&tpm.version, &state, &sock, &pid_file)?;
        // stderr is a file rather than a pipe: swtpm daemonizes and may keep
        // the descriptor open, which would make `.output()` wait on it.
        let log = dir.join("swtpm.log");
        let err = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&log)
            .map_err(|e| format!("prepare swtpm log: {e}"))?;
        let status = Command::new(SWTPM)
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(err))
            .status();
        let detail = || {
            fs::read_to_string(&log)
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                let msg = detail();
                let _ = stop_swtpm(dir);
                let _ = fs::remove_file(&log);
                return Err(if msg.is_empty() {
                    format!("start swtpm: {s}")
                } else {
                    format!("start swtpm: {msg}")
                });
            }
            Err(e) => {
                let _ = stop_swtpm(dir);
                let _ = fs::remove_file(&log);
                return Err(format!("start swtpm: {e}"));
            }
        }
        if !wait_socket(&sock, Duration::from_secs(2)) {
            let msg = detail();
            let _ = stop_swtpm(dir);
            let _ = fs::remove_file(&log);
            return Err(format!(
                "start swtpm: {}",
                if msg.is_empty() {
                    "no control socket appeared".to_string()
                } else {
                    msg
                }
            ));
        }
        let _ = fs::remove_file(&log);
        Ok(())
    }

    fn argv(&self, id: &str, d: &domainxml::Domain) -> Result<Vec<String>, String> {
        let dir = self.vm_dir(id);
        let mut machine = d.machine.clone();
        if d.domain_type == "kvm" && !machine.contains("accel=") {
            machine.push_str(",accel=kvm");
        }
        let cpu = match &d.cpu {
            Cpu::Host => "host".to_string(),
            Cpu::Max => "max".to_string(),
            Cpu::Model(model) => model.clone(),
        };
        let mut args: Vec<String> = vec![
            "-name".into(),
            id.to_string(),
            "-machine".into(),
            machine,
            "-cpu".into(),
            cpu,
            "-smp".into(),
            d.vcpus.to_string(),
            "-m".into(),
            d.memory_mib.to_string(),
        ];
        if !d.boot.is_empty() {
            let order: String = d
                .boot
                .iter()
                .map(|b| match b {
                    // QEMU's letters: c hard disk, d CD-ROM, n network, a floppy.
                    BootDev::Hd => 'c',
                    BootDev::Cdrom => 'd',
                    BootDev::Network => 'n',
                    BootDev::Floppy => 'a',
                })
                .collect();
            args.extend(["-boot".into(), format!("order={order}")]);
        }
        if d.firmware == Firmware::Uefi
            && let Some(loader) = &d.loader
        {
            args.extend([
                "-drive".into(),
                format!(
                    "if=pflash,format=raw,readonly=on,file={}",
                    escape_drive_path(loader)?
                ),
            ]);
        }
        if d.firmware == Firmware::Uefi
            && let Some(nvram) = &d.nvram
        {
            args.extend([
                "-drive".into(),
                format!("if=pflash,format=raw,file={}", escape_drive_path(nvram)?),
            ]);
        }
        if d.disks.iter().any(|x| x.bus == DiskBus::Scsi) {
            args.extend(["-device".into(), "virtio-scsi-pci,id=scsi0".into()]);
        }
        let mut sata = 0usize;
        for disk in &d.disks {
            let target = &disk.target;
            if let Some(source) = &disk.source {
                args.extend([
                    "-blockdev".into(),
                    format!(
                        "driver=file,filename={},node-name=file-{target}",
                        escape_blockdev_path(source)?
                    ),
                ]);
                args.extend([
                    "-blockdev".into(),
                    format!(
                        "driver={},file=file-{target},node-name=drive-{target}{}",
                        disk.format.as_str(),
                        if disk.readonly { ",read-only=on" } else { "" }
                    ),
                ]);
            }
            let drive = if disk.source.is_some() {
                format!(",drive=drive-{target}")
            } else {
                String::new()
            };
            let device = match disk.bus {
                DiskBus::Virtio => format!("virtio-blk-pci,id={target}{drive}"),
                DiskBus::Sata => {
                    if sata >= 6 {
                        return Err("a guest can have at most six sata devices".into());
                    }
                    let bus = sata;
                    sata += 1;
                    let driver = match disk.device {
                        DiskDevice::Disk => "ide-hd",
                        DiskDevice::Cdrom => "ide-cd",
                    };
                    format!("{driver},id={target},bus=ide.{bus}{drive}")
                }
                DiskBus::Scsi => {
                    let driver = match disk.device {
                        DiskDevice::Disk => "scsi-hd",
                        DiskDevice::Cdrom => "scsi-cd",
                    };
                    format!("{driver},id={target},bus=scsi0.0{drive}")
                }
            };
            args.extend(["-device".into(), device]);
        }
        for iface in &d.interfaces {
            // `<bandwidth>` is carried in the definition; `api::nics` installs
            // the `tc` shaping from it when a limit changes on a running guest.
            let mac = iface
                .mac
                .as_deref()
                .ok_or("a NIC has no MAC address; re-define the guest")?;
            let netdev = netdev_id(mac);
            let device = nic_device_id(mac);
            let netdev_arg = match &iface.kind {
                IfaceKind::User => format!("user,id={netdev}"),
                IfaceKind::Bridge(bridge) => {
                    if !crate::hostnet::valid_name(bridge) {
                        return Err(format!("'{bridge}' is not a usable bridge name"));
                    }
                    format!(
                        "tap,id={netdev},ifname={},script=no,downscript=no",
                        tap_name(id, mac)
                    )
                }
            };
            args.extend([
                "-netdev".into(),
                netdev_arg,
                "-device".into(),
                format!(
                    "{},id={device},netdev={netdev},mac={mac}",
                    iface.model.driver()
                ),
            ]);
        }
        let mut gl_video = false;
        if let Some(video) = &d.video {
            gl_video = video.model == VideoModel::Virtio && video.accel3d;
            if let Some(device) = video_device(video)? {
                args.extend(["-device".into(), device]);
            }
        }
        match &d.graphics {
            Some(Graphics::Vnc { listen, port }) => {
                // QEMU names a VNC display by number, and the TCP port is
                // 5900 + number; the definition carries the port.
                let display = port.checked_sub(5900).ok_or_else(|| {
                    format!("VNC port {port} has no display number; use 5900 or above")
                })?;
                args.extend(["-vnc".into(), format!("{listen}:{display}")]);
            }
            Some(Graphics::Spice { listen, port }) => {
                args.extend([
                    "-spice".into(),
                    format!("port={port},addr={listen},disable-ticketing=on"),
                ]);
            }
            None => {
                // A -gl device needs a GL-capable display of its own; with no
                // remote display, egl-headless is the one that can back it. It
                // needs a working host GL stack, which the start reported.
                args.extend([
                    "-display".into(),
                    if gl_video { "egl-headless" } else { "none" }.into(),
                ]);
            }
        }
        let usb = d
            .hostdevs
            .iter()
            .filter(|h| matches!(h, Hostdev::Usb { .. }))
            .count();
        if usb > 0 {
            args.extend(["-device".into(), "qemu-xhci,id=xhci".into()]);
        }
        for hostdev in &d.hostdevs {
            match hostdev {
                Hostdev::Usb { vendor, product } => args.extend([
                    "-device".into(),
                    format!(
                        "usb-host,bus=xhci.0,vendorid=0x{vendor:04x},productid=0x{product:04x}"
                    ),
                ]),
                Hostdev::Pci {
                    domain,
                    bus,
                    slot,
                    function,
                } => args.extend([
                    "-device".into(),
                    format!("vfio-pci,host={domain:04x}:{bus:02x}:{slot:02x}.{function:x}"),
                ]),
            }
        }
        if let Some(rng) = &d.rng {
            args.extend([
                "-object".into(),
                format!("rng-random,id=rng0,filename={}", escape_blockdev_path(rng)?),
                "-device".into(),
                "virtio-rng-pci,rng=rng0".into(),
            ]);
        }
        if d.memballoon {
            args.extend(["-device".into(), "virtio-balloon-pci".into()]);
        }
        if d.guest_agent {
            args.extend([
                "-device".into(),
                "virtio-serial-pci,id=virtio-serial0".into(),
                "-chardev".into(),
                format!(
                    "socket,id=qga0,path={},server=on,wait=off",
                    escape_blockdev_path(&dir.join("qga.sock"))?
                ),
                "-device".into(),
                "virtserialport,bus=virtio-serial0.0,chardev=qga0,id=qga0,name=org.qemu.guest_agent.0"
                    .into(),
            ]);
        }
        if d.tpm.is_some() {
            let ctrl = dir.join("swtpm.sock");
            args.extend([
                "-chardev".into(),
                format!("socket,id=chrtpm,path={}", escape_blockdev_path(&ctrl)?),
                "-tpmdev".into(),
                "emulator,id=tpm0,chardev=chrtpm".into(),
                "-device".into(),
                "tpm-crb,tpmdev=tpm0".into(),
            ]);
        }
        if let Some(watchdog) = &d.watchdog {
            args.extend([
                "-watchdog-action".into(),
                watchdog.action.clone(),
                "-device".into(),
                watchdog.model.clone(),
            ]);
        }
        match d.serial {
            // A stable socket in the guest's runtime directory: the console
            // view and the live suites open it, and `-serial pty` would give a
            // path nobody can find again.
            Some(SerialKind::Pty) => args.extend([
                "-serial".into(),
                format!(
                    "unix:{},server=on,wait=off",
                    escape_blockdev_path(&dir.join("serial.sock"))?
                ),
            ]),
            Some(SerialKind::Null) => args.extend(["-serial".into(), "none".into()]),
            None => {}
        }
        args.extend([
            "-qmp".into(),
            format!("unix:{},server=on,wait=off", dir.join("qmp.sock").display()),
        ]);
        Ok(args)
    }

    fn identity(&self, r: &Record) -> bool {
        r.pid
            .zip(r.start_ticks)
            .is_some_and(|(p, t)| process_ticks(p) == Some(t) && process_exe_matches(p, &self.qemu))
    }
    pub fn state(&self, id: &str) -> Result<State, String> {
        if !Self::valid_id(id) {
            return Err("invalid VM id".into());
        }
        let r = self.read()?.remove(id).ok_or("unknown QEMU VM")?;
        if !self.identity(&r) {
            return Ok(State::Stopped);
        }
        let path = self.vm_dir(id).join("qmp.sock");
        match qmp(&path, "query-status") {
            Ok(v) => match serde_json::from_str::<serde_json::Value>(&v)
                .ok()
                .and_then(|v| {
                    v.get("status")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .as_deref()
            {
                Some("running") => Ok(State::Running),
                Some(
                    "paused" | "prelaunch" | "inmigrate" | "postmigrate" | "finish-migrate"
                    | "restore-vm" | "save-vm",
                ) => Ok(State::Paused),
                Some("shutdown") | Some("suspended") => Ok(State::Stopped),
                _ => Ok(State::Unknown),
            },
            Err(_) => Ok(State::Unknown),
        }
    }
    /// QEMU's own reset, which both the guest's reboot request and the UI's
    /// reset button use: a direct launch has no ACPI reboot event separate
    /// from `system_reset`.
    pub fn reboot(&self, id: &str) -> Result<(), String> {
        self.qmp_reset(id)
    }

    pub fn reset(&self, id: &str) -> Result<(), String> {
        self.qmp_reset(id)
    }

    fn qmp_reset(&self, id: &str) -> Result<(), String> {
        let rec = self.read()?.remove(id).ok_or("unknown QEMU VM")?;
        if !self.identity(&rec) {
            return Err("VM is not running".into());
        }
        qmp_execute(
            &self.vm_dir(id).join("qmp.sock"),
            "system_reset",
            serde_json::json!({}),
        )
        .map(|_| ())
    }

    /// CPU time and RSS from `/proc`, plus the configured size, so the summary
    /// charts keep the shape they had under libvirt. A stopped guest has no
    /// process: its counters read zero and only the definition's size is real.
    pub fn stats(&self, id: &str) -> Result<Stats, String> {
        let rec = self.read()?.remove(id).ok_or("unknown QEMU VM")?;
        let d = domainxml::parse(&rec.config.xml)?;
        let state = self.state(id)?;
        let live = matches!(state, State::Running | State::Paused) && self.identity(&rec);
        let (cpu_time_ns, rss_kib) = match (live, rec.pid) {
            (true, Some(pid)) => (
                process_cpu_ns(pid).unwrap_or(0),
                process_rss_kib(pid).or(Some(0)),
            ),
            _ => (0, Some(0)),
        };
        let memory_kib = u64::from(d.memory_mib) * 1024;
        Ok(Stats {
            state: state.as_str(),
            cpu_time_ns,
            vcpus: d.vcpus,
            memory_kib,
            max_memory_kib: memory_kib,
            rss_kib,
            // A direct launch has no balloon driver to report guest-usable
            // memory; the summary falls back to RSS, as it does when libvirt
            // reports none.
            usable_kib: None,
        })
    }

    /// One row per NIC in the definition. The host-side name is the derived
    /// tap, so a stopped guest reports none; counters come from that tap's
    /// sysfs statistics and are only meaningful while the process runs.
    pub fn interfaces(&self, id: &str) -> Result<Vec<InterfaceStats>, String> {
        let rec = self.read()?.remove(id).ok_or("unknown QEMU VM")?;
        let d = domainxml::parse(&rec.config.xml)?;
        let live = self.identity(&rec);
        let mut out = Vec::new();
        for iface in &d.interfaces {
            let mac = iface.mac.clone();
            let (dev, network) = match &iface.kind {
                IfaceKind::Bridge(bridge) => (
                    mac.as_deref().filter(|_| live).map(|mac| tap_name(id, mac)),
                    Some(bridge.clone()),
                ),
                IfaceKind::User => (None, None),
            };
            let (rx_bytes, tx_bytes) = match &dev {
                Some(tap) => (net_counter(tap, "rx_bytes"), net_counter(tap, "tx_bytes")),
                None => (0, 0),
            };
            out.push(InterfaceStats {
                dev,
                mac,
                model: iface.model.as_str(),
                network,
                rx_kib: iface.bandwidth.inbound.as_ref().and_then(|r| r.average),
                tx_kib: iface.bandwidth.outbound.as_ref().and_then(|r| r.average),
                rx_bytes,
                tx_bytes,
            });
        }
        Ok(out)
    }

    /// Per-disk bytes moved, keyed by the target dev (`vda`). A stopped guest
    /// has no QMP socket, so every disk reads zero; the collector keeps the
    /// zero as a real sample rather than treating it as a missing device.
    pub fn block_stats(&self, id: &str) -> Result<BTreeMap<String, (u64, u64)>, String> {
        let rec = self.read()?.remove(id).ok_or("unknown QEMU VM")?;
        if !self.identity(&rec) {
            return Ok(BTreeMap::new());
        }
        let raw = qmp(&self.vm_dir(id).join("qmp.sock"), "query-blockstats")?;
        let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        let mut out = BTreeMap::new();
        for block in v.as_array().into_iter().flatten() {
            let Some(dev) = block.get("device").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(stats) = block.get("stats") else {
                continue;
            };
            let num = |name: &str| {
                stats
                    .get(name)
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
            };
            out.insert(dev.to_string(), (num("rd_bytes"), num("wr_bytes")));
        }
        Ok(out)
    }

    pub fn graphics(&self, id: &str) -> Result<Option<GraphicsInfo>, String> {
        Ok(domainxml::parse(&self.xml(id)?)?.graphics.map(|g| match g {
            Graphics::Vnc { listen, port } => GraphicsInfo {
                kind: "vnc",
                host: listen,
                port,
            },
            Graphics::Spice { listen, port } => GraphicsInfo {
                kind: "spice",
                host: listen,
                port,
            },
        }))
    }

    pub fn powerdown(&self, id: &str, force: bool) -> Result<(), String> {
        if !Self::valid_id(id) {
            return Err("invalid VM id".into());
        }
        let guard = self.lock.lock().map_err(|e| e.to_string())?;
        let all = self.read()?;
        let rec = all.get(id).ok_or("unknown QEMU VM")?;
        let identity = rec
            .pid
            .zip(rec.start_ticks)
            .filter(|(p, t)| process_ticks(*p) == Some(*t));
        let Some((pid, ticks)) = identity else {
            return Ok(());
        };
        let path = self.vm_dir(id).join("qmp.sock");
        if force {
            signal_verified(rec, libc::SIGKILL)?;
        } else {
            qmp(&path, "system_powerdown")?;
        }
        drop(guard);
        // The lock is released for the wait: a graceful shutdown can take
        // seconds. Re-acquiring it is what makes the tap cleanup safe — a new
        // start must not have made the same tap meanwhile, and if it did, this
        // stop must leave that tap alone. A tap also outlives the process, so
        // it is deleted only after QEMU is confirmed gone, never before.
        let secs = if force { 5 } else { 30 };
        if !wait_exit(pid, ticks, secs) {
            return Err("QEMU did not exit after the stop request".into());
        }
        let guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let mut stopped = None;
        if let Some(rec) = all.get_mut(id)
            && rec.pid == Some(pid)
            && rec.start_ticks == Some(ticks)
        {
            rec.pid = None;
            rec.start_ticks = None;
            stopped = Some(std::mem::take(&mut rec.taps));
        }
        if let Some(taps) = stopped {
            self.save(&all)?;
            // Inside the lock: a start cannot create the same tap until this
            // cleanup is done.
            cleanup_taps(&taps);
            if let Err(e) = stop_swtpm(&self.vm_dir(id)) {
                tracing::warn!("qemu: stopping swtpm for {id}: {e}");
            }
        }
        drop(guard);
        Ok(())
    }
    pub fn force_stop(&self, id: &str) -> Result<(), String> {
        if !Self::valid_id(id) {
            return Err("invalid VM id".into());
        }
        let guard = self.lock.lock().map_err(|e| e.to_string())?;
        let all = self.read()?;
        let rec = all.get(id).ok_or("unknown QEMU VM")?;
        let Some((pid, ticks)) = rec
            .pid
            .zip(rec.start_ticks)
            .filter(|(p, t)| process_ticks(*p) == Some(*t))
        else {
            return Ok(());
        };
        signal_verified(rec, libc::SIGKILL)?;
        drop(guard);
        if !wait_exit(pid, ticks, 5) {
            return Err("QEMU did not exit after SIGKILL".into());
        }
        // Same check as `powerdown`: only clean up if this record is still the
        // process that was waited on.
        let guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let mut stopped = None;
        if let Some(rec) = all.get_mut(id)
            && rec.pid == Some(pid)
            && rec.start_ticks == Some(ticks)
        {
            rec.pid = None;
            rec.start_ticks = None;
            stopped = Some(std::mem::take(&mut rec.taps));
        }
        if let Some(taps) = stopped {
            self.save(&all)?;
            cleanup_taps(&taps);
            if let Err(e) = stop_swtpm(&self.vm_dir(id)) {
                tracing::warn!("qemu: stopping swtpm for {id}: {e}");
            }
        }
        drop(guard);
        Ok(())
    }
    /// Removes metadata/runtime only after the managed process is gone; disk files are never removed.
    pub fn delete(&self, id: &str) -> Result<(), String> {
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        if !Self::valid_id(id) {
            return Err("invalid VM id".into());
        }
        let mut all = self.read()?;
        let Some(r) = all.get(id) else {
            return Ok(());
        };
        if self.identity(r) {
            return Err("VM is running; stop it before deletion".into());
        }
        cleanup_taps(&r.taps);
        // Stop the TPM emulator before its state directory is removed. A pid
        // that is not a swtpm for this guest is never signalled.
        stop_swtpm(&self.vm_dir(id)).map_err(|e| format!("stop the guest's swtpm: {e}"))?;
        let runtime = self.vm_dir(id);
        let staged = runtime.with_extension("deleting");
        if runtime.exists() {
            fs::rename(&runtime, &staged)
                .map_err(|e| format!("runtime staging failed; metadata retained: {e}"))?;
        }
        all.remove(id);
        if let Err(e) = self.save(&all) {
            if staged.exists() {
                let _ = fs::rename(&staged, &runtime);
            }
            return Err(e);
        }
        if staged.exists() {
            fs::remove_dir_all(staged)
                .map_err(|e| format!("metadata deleted; staged runtime cleanup failed: {e}"))?;
        }
        Ok(())
    }
}

fn process_ticks(pid: u32) -> Option<u64> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = s.rsplit_once(") ")?.1;
    tail.split_whitespace().nth(19)?.parse().ok()
}
fn clock_ticks() -> u64 {
    static TICKS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TICKS.get_or_init(|| {
        let n = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if n > 0 { n as u64 } else { 100 }
    })
}
fn process_cpu_ns(pid: u32) -> Option<u64> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = s.rsplit_once(") ")?.1;
    let mut f = tail.split_whitespace();
    // The fields after the command: state is 3, so utime (14) is index 11 and
    // stime (15) index 12.
    let utime: u64 = f.nth(11)?.parse().ok()?;
    let stime: u64 = f.next()?.parse().ok()?;
    Some((utime + stime).saturating_mul(1_000_000_000 / clock_ticks()))
}
fn process_rss_kib(pid: u32) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k == "VmRSS").then(|| v.split_whitespace().next()?.parse().ok())?
    })
}
fn net_counter(iface: &str, which: &str) -> u64 {
    fs::read_to_string(
        Path::new(SYS_NET)
            .join(iface)
            .join("statistics")
            .join(which),
    )
    .ok()
    .and_then(|s| s.trim().parse().ok())
    .unwrap_or(0)
}
/// A display name: trimmed, non-empty, no control characters and no `/`, so it
/// still fits one URL segment now that a title is also a link target.
fn display_title(title: &str) -> Result<String, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("new name is empty".into());
    }
    if title.chars().count() > 128 {
        return Err("name is longer than 128 characters".into());
    }
    if title.contains('/') || title.chars().any(char::is_control) {
        return Err("name cannot contain '/' or control characters".into());
    }
    Ok(title.to_string())
}
fn process_exe_matches(pid: u32, executable: &Path) -> bool {
    fs::canonicalize(format!("/proc/{pid}/exe")).ok() == fs::canonicalize(executable).ok()
}
fn wait_exit(pid: u32, ticks: u64, secs: u64) -> bool {
    let until = Instant::now() + Duration::from_secs(secs);
    while process_ticks(pid) == Some(ticks) {
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

const SWTPM: &str = "/usr/bin/swtpm";

/// The argv for a guest's TPM emulator. `--tpm2` is the only switch between a
/// 1.2 and a 2.0 TPM; everything else is per-guest state.
fn swtpm_argv(
    version: &str,
    state: &Path,
    sock: &Path,
    pid_file: &Path,
) -> Result<Vec<String>, String> {
    let path = |p: &Path| {
        p.to_str()
            .map(str::to_string)
            .ok_or_else(|| format!("TPM path is not valid UTF-8: {}", p.display()))
    };
    let mut args = vec!["socket".to_string()];
    if version != "1.2" {
        args.push("--tpm2".into());
    }
    args.extend([
        "--tpmstate".into(),
        format!("dir={}", path(state)?),
        "--ctrl".into(),
        format!("type=unixio,path={}", path(sock)?),
        "--daemon".into(),
        "--pid".into(),
        format!("file={}", path(pid_file)?),
    ]);
    Ok(args)
}

/// Whether a `/proc/<pid>/cmdline` is the swtpm for *this* guest. Both the
/// state directory and the control socket must be named: the pid file alone is
/// not enough to kill a process, since a pid can be reused.
fn swtpm_cmdline_is_ours(cmdline: &[u8], state: &Path, sock: &Path) -> bool {
    let exe = std::str::from_utf8(cmdline.split(|b| *b == 0).next().unwrap_or(&[]))
        .ok()
        .and_then(|s| {
            Path::new(s)
                .file_name()
                .map(|n| n == std::ffi::OsStr::new("swtpm"))
        })
        .unwrap_or(false);
    if !exe {
        return false;
    }
    let state_arg = format!("dir={}", state.display());
    let sock_arg = format!("path={}", sock.display());
    let mut has_state = false;
    let mut has_sock = false;
    for arg in cmdline.split(|b| *b == 0).filter(|a| !a.is_empty()) {
        if arg == state_arg.as_bytes() {
            has_state = true;
        }
        // `--ctrl` carries a comma-separated type=...,path=... value.
        if arg
            .split(|b| *b == b',')
            .any(|part| part == sock_arg.as_bytes())
        {
            has_sock = true;
        }
    }
    has_state && has_sock
}

fn read_pid_file(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn is_socket(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket())
}

/// A socket with a listener behind it. A stale socket file from a crashed
/// swtpm accepts no connection and is replaced.
fn socket_live(path: &Path) -> bool {
    is_socket(path) && UnixStream::connect(path).is_ok()
}

fn wait_socket(path: &Path, timeout: Duration) -> bool {
    let until = Instant::now() + timeout;
    loop {
        if socket_live(path) {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Stop a guest's swtpm and remove its pid/socket. Never signals a pid that is
/// not a swtpm naming this guest's state and socket.
fn stop_swtpm(runtime: &Path) -> Result<(), String> {
    let sock = runtime.join("swtpm.sock");
    let pid_file = runtime.join("swtpm.pid");
    let state = runtime.join("tpm");
    let Some(pid) = read_pid_file(&pid_file) else {
        if !socket_live(&sock) {
            let _ = fs::remove_file(&sock);
        }
        return Ok(());
    };
    // Capture the start time before checking the cmdline: the same pid could
    // otherwise be reused between the check and the signal.
    let Some(ticks) = process_ticks(pid) else {
        let _ = fs::remove_file(&pid_file);
        if !socket_live(&sock) {
            let _ = fs::remove_file(&sock);
        }
        return Ok(());
    };
    let ours = fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .is_some_and(|cmdline| swtpm_cmdline_is_ours(&cmdline, &state, &sock))
        && process_ticks(pid) == Some(ticks);
    if !ours {
        // A stale pid file (or a reused pid) is removed, but the process is
        // left alone: it is not our TPM emulator.
        let _ = fs::remove_file(&pid_file);
        if !socket_live(&sock) {
            let _ = fs::remove_file(&sock);
        }
        return Ok(());
    }
    if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("stop swtpm {pid}: {e}"));
        }
    }
    if !wait_exit(pid, ticks, 5) {
        if process_ticks(pid) == Some(ticks)
            && unsafe { libc::kill(pid as i32, libc::SIGKILL) } != 0
        {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!("kill swtpm {pid}: {e}"));
            }
        }
        if !wait_exit(pid, ticks, 5) {
            return Err(format!("swtpm {pid} did not exit"));
        }
    }
    let _ = fs::remove_file(&pid_file);
    let _ = fs::remove_file(&sock);
    Ok(())
}

/// A deterministic MAC for one VM interface: QEMU's `52:54:00` prefix (unicast
/// and locally administered) plus 24 bits of a hash of the VM and the slot.
/// Stable across restarts and identical for every caller, so the address
/// reservation, the DHCP host entry and QEMU itself all name the same NIC.
pub fn stable_mac(id: &str, slot: &str) -> String {
    stable_mac_salted(id, slot, 0)
}

/// The base with a salt mixed in, used when the deterministic MAC is already
/// taken. Only 24 bits are available, so a collision is re-derived rather than
/// handed to dnsmasq as a second guest's address.
fn stable_mac_salted(id: &str, slot: &str, salt: u32) -> String {
    let mut h = crate::hostnet::fnv1a(&format!("{id}/{slot}"));
    if salt != 0 {
        h = h.wrapping_add(u64::from(salt).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    }
    format!(
        "52:54:00:{:02x}:{:02x}:{:02x}",
        (h >> 16) as u8,
        (h >> 8) as u8,
        h as u8
    )
}

/// The host tap for one VM interface: `tap` + 10 hex, 13 characters. Derived
/// from the MAC — the NIC's identity in the definition — so removing another
/// interface never renames it. Never stored, so a restart finds it again.
pub fn tap_name(id: &str, mac: &str) -> String {
    let h = crate::hostnet::fnv1a(&format!("tap/{id}/{mac}"));
    format!("tap{:010x}", h & 0xff_ffff_ffff)
}

/// The QMP netdev id of a NIC, distinct from its device id (QEMU allows both
/// but keeping them apart avoids depending on that).
fn netdev_id(mac: &str) -> String {
    format!("nd{}", mac.replace(':', ""))
}

/// The QMP device id of a NIC: derived from its MAC, so it survives another
/// interface being removed (positions do not).
fn nic_device_id(mac: &str) -> String {
    format!("net{}", mac.replace(':', ""))
}

/// The QMP device id of the target dev a new disk would get.
fn next_disk_target(d: &domainxml::Domain) -> Result<String, String> {
    for letter in b'a'..=b'z' {
        let target = format!("vd{}", letter as char);
        if !d.disks.iter().any(|x| x.target == target) {
            return Ok(target);
        }
    }
    Err("no free virtio disk name is left (vda-vdz)".into())
}

/// A disk file, canonicalised. A symlink is refused rather than followed: the
/// stored path is what QEMU opens, so it must not point somewhere that can be
/// re-pointed later.
fn check_disk_file(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("disk must be an absolute file path".into());
    }
    let link_meta = fs::symlink_metadata(path).map_err(|e| format!("inspect disk: {e}"))?;
    if !link_meta.file_type().is_file() {
        return Err("disk must be a regular file, not a symlink".into());
    }
    let canonical = fs::canonicalize(path).map_err(|e| format!("canonicalize disk: {e}"))?;
    let meta = fs::metadata(&canonical).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() == 0 {
        return Err("disk must be a non-empty regular file".into());
    }
    // QEMU's argv is UTF-8 text: a path it cannot spell would define a VM that
    // can never start, so it is refused while the VM is still stoppable.
    escape_blockdev_path(&canonical)?;
    Ok(canonical)
}

/// Copy the UEFI variable-store template into the guest's runtime directory on
/// the first start. QEMU cannot create a variable store from a template; only
/// this daemon knows to.
fn prepare_nvram(d: &domainxml::Domain) -> Result<(), String> {
    let (Some(nvram), Some(template)) = (&d.nvram, &d.nvram_template) else {
        return Ok(());
    };
    if nvram.exists() {
        return Ok(());
    }
    if !template.is_file() {
        return Err(format!(
            "the UEFI NVRAM template {} does not exist on this host",
            template.display()
        ));
    }
    if let Some(parent) = nvram.parent() {
        private_dir(parent)?;
    }
    fs::copy(template, nvram)
        .and_then(|_| fs::set_permissions(nvram, fs::Permissions::from_mode(0o600)))
        .map_err(|e| format!("copy the UEFI NVRAM template: {e}"))
}

/// The QEMU device for the one `<video>` adapter, with only the options its
/// model accepts.
fn video_device(v: &Video) -> Result<Option<String>, String> {
    let base = match v.model {
        VideoModel::None => return Ok(None),
        VideoModel::Vga => "VGA",
        VideoModel::Cirrus => "cirrus-vga",
        VideoModel::Vmware => "vmware-svga",
        VideoModel::Bochs => "bochs-display",
        VideoModel::Ramfb => "ramfb",
        VideoModel::Qxl => "qxl-vga",
        VideoModel::Virtio => match v.device.as_deref().unwrap_or("virtio-vga") {
            "virtio-vga" => "virtio-vga",
            "virtio-vga-gl" => "virtio-vga-gl",
            "virtio-gpu" => "virtio-gpu-pci",
            "virtio-gpu-gl" => "virtio-gpu-gl-pci",
            other => return Err(format!("virtio video device '{other}' is not supported")),
        },
    };
    let mut s = base.to_string();
    match v.model {
        VideoModel::Qxl => {
            if let Some(ram) = v.ram_kib {
                s.push_str(&format!(",ram_size={}", u64::from(ram) * 1024));
            }
            if let Some(vram) = v.vram_kib {
                s.push_str(&format!(",vram_size={}", u64::from(vram) * 1024));
            }
            if let Some(vgamem) = v.vgamem_kib {
                s.push_str(&format!(",vgamem_mb={}", vgamem.div_ceil(1024).max(1)));
            }
            if let Some(vram64) = v.vram64_kib {
                s.push_str(&format!(",vram64_size_mb={}", vram64.div_ceil(1024).max(1)));
            }
            if let Some(heads) = v.heads {
                s.push_str(&format!(",max_outputs={heads}"));
            }
        }
        VideoModel::Virtio => {
            if let Some(blob) = v.blob {
                s.push_str(if blob { ",blob=on" } else { ",blob=off" });
            }
            if let Some(heads) = v.heads {
                s.push_str(&format!(",max_outputs={heads}"));
            }
        }
        VideoModel::Bochs => {
            if let Some(vram) = v.vram_kib {
                s.push_str(&format!(",vgamem={}", u64::from(vram) * 1024));
            }
        }
        VideoModel::Vga | VideoModel::Cirrus | VideoModel::Vmware => {
            // libvirt's `vram` is the framebuffer size; QEMU spells it
            // `vgamem_mb` for the VGA family.
            if let Some(vram) = v.vram_kib {
                s.push_str(&format!(",vgamem_mb={}", vram.div_ceil(1024).max(1)));
            }
        }
        _ => {}
    }
    if let Some(edid) = v.edid {
        s.push_str(if edid { ",edid=on" } else { ",edid=off" });
    }
    Ok(Some(s))
}

/// A bridge NIC's MAC, generated or validated. A user-supplied MAC in canonical
/// form is used as given (it is how a guest keeps its address across metadata
/// rewrites), but it must be unicast, not all-zero, and free in this manager.
fn mac_for(used: &BTreeSet<String>, id: &str, slot: &str, mac: &str) -> Result<String, String> {
    if mac.trim().is_empty() {
        // The deterministic base first; only if another NIC already holds it
        // does a salted re-derivation run (24 bits is small but not zero).
        let base = stable_mac(id, slot);
        if !used.contains(&base) {
            return Ok(base);
        }
        for salt in 1..=1024 {
            let candidate = stable_mac_salted(id, slot, salt);
            if !used.contains(&candidate) {
                return Ok(candidate);
            }
        }
        return Err("cannot derive a free MAC address".into());
    }
    let m = domainxml::normalize_mac(mac)?;
    if used.contains(&m) {
        return Err(format!("MAC address {m} is already used by another NIC"));
    }
    Ok(m)
}

/// The MACs this manager's other NICs already hold. The VM being edited is
/// excluded: its definition is about to be replaced, and duplicates inside one
/// definition are refused by `domainxml`.
fn used_macs(all: &BTreeMap<String, Record>, exclude: &str) -> BTreeSet<String> {
    let mut used = BTreeSet::new();
    for (vm, rec) in all {
        if vm == exclude {
            continue;
        }
        if let Ok(d) = domainxml::parse(&rec.config.xml) {
            for iface in d.interfaces {
                if let Some(mac) = iface.mac {
                    used.insert(mac);
                }
            }
        }
    }
    used
}

/// Every disk file another guest already holds, canonicalised.
fn attached_files(all: &BTreeMap<String, Record>, exclude: Option<&str>) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    for (vm, rec) in all {
        if Some(vm.as_str()) == exclude {
            continue;
        }
        if let Ok(d) = domainxml::parse(&rec.config.xml) {
            for disk in d.disks {
                if let Some(source) = disk.source {
                    out.insert(source);
                }
            }
        }
    }
    out
}

fn interface_exists(name: &str) -> bool {
    crate::hostnet::valid_name(name) && Path::new(SYS_NET).join(name).exists()
}

/// The interface is a tun/tap. Anything else (a bridge, a physical NIC, a veth)
/// is never deleted or enslaved by these helpers.
fn is_tap(name: &str) -> bool {
    crate::hostnet::valid_name(name) && Path::new(SYS_NET).join(name).join("tun_flags").exists()
}

/// The bridge a tap is enslaved to, from sysfs. None means free (or gone).
fn tap_master(name: &str) -> Option<String> {
    std::fs::read_link(Path::new(SYS_NET).join(name).join("master"))
        .ok()?
        .file_name()?
        .to_str()
        .map(str::to_string)
}

pub fn tap_attached(tap: &str, bridge: &str) -> bool {
    is_tap(tap) && tap_master(tap).as_deref() == Some(bridge)
}

fn run_ip(args: &[&str]) -> Result<(), String> {
    let out = Command::new("ip")
        .args(args)
        .output()
        .map_err(|e| format!("run ip: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let msg = String::from_utf8_lossy(&out.stderr);
    Err(format!("ip {}: {}", args.join(" "), msg.trim()))
}

/// Give this VM interface a host tap on `bridge`, reusing the one a previous
/// run or a daemon restart left behind. A tap another bridge owns is never
/// moved, and an interface of any other kind is never adopted.
pub fn ensure_tap(tap: &str, bridge: &str) -> Result<(), String> {
    if !crate::hostnet::valid_name(tap) || !crate::hostnet::valid_name(bridge) {
        return Err("invalid tap or bridge name".into());
    }
    if !is_bridge(bridge) {
        return Err(format!(
            "'{bridge}' is not a Linux bridge; start the network first"
        ));
    }
    // Already ours: a daemon restart must reuse the tap, not fail on "exists".
    if tap_attached(tap, bridge) {
        return run_ip(&["link", "set", tap, "up"]);
    }
    if interface_exists(tap) {
        if !is_tap(tap) {
            return Err(format!("'{tap}' exists and is not a tun/tap"));
        }
        if let Some(master) = tap_master(tap)
            && master != bridge
        {
            return Err(format!("tap '{tap}' is attached to bridge '{master}'"));
        }
    } else {
        run_ip(&["tuntap", "add", "dev", tap, "mode", "tap"])?;
    }
    // Both steps are idempotent, so the reuse path takes them too: a tap left
    // free by a recreated bridge is plugged back in here.
    run_ip(&["link", "set", tap, "master", bridge])?;
    run_ip(&["link", "set", tap, "up"])
}

/// Remove a tap. Only a tun/tap is ever deleted; an absent one is success.
pub fn delete_tap(tap: &str) -> Result<(), String> {
    if !is_tap(tap) {
        return Ok(());
    }
    match run_ip(&["link", "del", tap]) {
        Err(e) if e.contains("Cannot find device") => Ok(()),
        other => other,
    }
}

fn cleanup_taps(taps: &[String]) {
    for tap in taps {
        if let Err(e) = delete_tap(tap) {
            tracing::warn!("qemu: removing tap {tap}: {e}");
        }
    }
}

fn delete_nic_tap(id: &str, mac: &str) {
    cleanup_taps(&[tap_name(id, mac)]);
}

/// A `-blockdev` value: QEMU's option parser separates on commas and escapes a
/// literal one by doubling it (the `-drive` backslash form is not this parser).
fn escape_blockdev_path(path: &Path) -> Result<String, String> {
    let s = path.to_str().ok_or("disk path is not valid UTF-8")?;
    Ok(s.replace(',', ",,"))
}

fn escape_drive_path(path: &Path) -> Result<String, String> {
    let s = path.to_str().ok_or("disk path is not valid UTF-8")?;
    Ok(s.replace('\\', "\\\\")
        .replace(',', "\\,")
        .replace(':', "\\:"))
}
fn private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| e.to_string())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())
}
fn signal_verified(r: &Record, sig: i32) -> Result<(), String> {
    let (pid, ticks) = r.pid.zip(r.start_ticks).ok_or("missing process identity")?;
    if process_ticks(pid) != Some(ticks) {
        return Err("process identity no longer matches".into());
    }
    if unsafe { libc::kill(pid as i32, sig) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}
fn qmp(path: &Path, command: &str) -> Result<String, String> {
    qmp_execute(path, command, serde_json::json!({}))
}
fn qmp_execute(path: &Path, command: &str, args: serde_json::Value) -> Result<String, String> {
    let mut s = None;
    for _ in 0..100 {
        if let Ok(x) = UnixStream::connect(path) {
            s = Some(x);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut s = s.ok_or("QMP socket unavailable")?;
    let md = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !md.file_type().is_socket() || md.permissions().mode() & 0o077 != 0 {
        return Err("QMP socket has unsafe type or permissions".into());
    }
    s.set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let mut lines = BufReader::new(s.try_clone().map_err(|e| e.to_string())?);
    let greeting: serde_json::Value = read_qmp(&mut lines)?;
    if greeting.get("QMP").is_none() {
        return Err("invalid QMP greeting".into());
    }
    qmp_command(
        &mut s,
        &mut lines,
        "qmp_capabilities",
        serde_json::json!({}),
    )?;
    qmp_command(&mut s, &mut lines, command, args).map(|v| v.to_string())
}
fn read_qmp(r: &mut BufReader<UnixStream>) -> Result<serde_json::Value, String> {
    loop {
        let mut line = String::new();
        r.read_line(&mut line).map_err(|e| e.to_string())?;
        let v: serde_json::Value =
            serde_json::from_str(&line).map_err(|e| format!("invalid QMP JSON line: {e}"))?;
        if v.get("event").is_none() {
            return Ok(v);
        }
    }
}
fn qmp_command(
    s: &mut UnixStream,
    r: &mut BufReader<UnixStream>,
    cmd: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    s.write_all(
        format!(
            "{{\"execute\":{},\"arguments\":{}}}\r\n",
            serde_json::to_string(cmd).map_err(|e| e.to_string())?,
            serde_json::to_string(&args).map_err(|e| e.to_string())?
        )
        .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let v = read_qmp(r)?;
    if let Some(e) = v.get("error") {
        return Err(format!("QMP {cmd} failed: {e}"));
    }
    if v.get("return").is_none() {
        return Err(format!("QMP {cmd} returned no result"));
    }
    Ok(v["return"].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const ID2: &str = "550e8400-e29b-41d4-a716-446655440001";

    fn manager(tag: &str) -> (Manager, PathBuf) {
        // Short: a per-VM svtpm control socket must fit a Unix socket path.
        let root = std::env::temp_dir().join(format!("vmq-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let manager = Manager {
            data_dir: root.clone(),
            state_root: root.join("state"),
            qemu: PathBuf::from("qemu"),
            lock: Default::default(),
        };
        (manager, root)
    }

    fn disk(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, b"x").unwrap();
        path
    }

    /// A SPA-shaped definition: one virtio disk, a bridge NIC and a VNC
    /// display. `extra` is inserted inside `<devices>`.
    fn spa(id: &str, disk: &Path, extra: &str) -> String {
        format!(
            r#"<domain type="kvm">
  <name>{id}</name>
  <uuid>{id}</uuid>
  <title>web</title>
  <memory unit="KiB">524288</memory>
  <vcpu>2</vcpu>
  <os>
    <type arch="x86_64" machine="q35">hvm</type>
    <boot dev="hd"/>
  </os>
  <features><acpi/><apic/></features>
  <devices>
    <disk type="file" device="disk">
      <driver name="qemu" type="qcow2"/>
      <source file="{}"/>
      <target dev="vda" bus="virtio"/>
    </disk>
    <interface type="bridge">
      <source bridge="vbr5b92a1a54f00"/>
      <model type="virtio"/>
    </interface>
    <graphics type="vnc" port="5900" autoport="yes" listen="127.0.0.1"/>
    <serial type="pty"><target port="0"/></serial>
    <console type="pty"><target type="serial" port="0"/></console>
    {extra}
  </devices>
</domain>"#,
            disk.display()
        )
    }

    /// The values given to one argv flag, in order.
    fn values(args: &[String], flag: &str) -> Vec<String> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].clone())
            .collect()
    }

    fn one(args: &[String], flag: &str) -> String {
        values(args, flag).remove(0)
    }

    #[test]
    fn qemu_ids_must_be_canonical_uuids() {
        assert!(Manager::valid_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!Manager::valid_id("legacy-vm"));
        assert!(!Manager::valid_id("550E8400-E29B-41D4-A716-446655440000"));
        assert!(!Manager::valid_id("550e8400e29b41d4a716446655440000"));
    }

    #[test]
    fn metadata_rejects_key_id_disagreement() {
        let (manager, root) = manager("meta");
        let record = Record {
            config: VmConfig {
                id: "another-id".into(),
                title: String::new(),
                autostart: false,
                xml: String::new(),
            },
            taps: vec![],
            pid: None,
            start_ticks: None,
        };
        fs::write(
            manager.records(),
            serde_json::to_vec(&BTreeMap::from([("map-id", record)])).unwrap(),
        )
        .unwrap();
        assert!(manager.read().unwrap_err().contains("does not match"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn define_stores_the_normalized_xml_and_lists_it() {
        let (manager, root) = manager("define");
        let primary = disk(&root, "disk.qcow2");
        let stored = manager.define(&spa(ID, &primary, "")).unwrap();
        assert_eq!((stored.id.as_str(), stored.title.as_str()), (ID, "web"));
        assert!(!stored.autostart);
        let xml = manager.xml(ID).unwrap();
        assert!(xml.contains(&format!("<name>{ID}</name>")), "{xml}");
        assert!(
            xml.contains(&format!(
                "<source file=\"{}\"/>",
                fs::canonicalize(&primary).unwrap().display()
            )),
            "the disk is stored by its canonical path:\n{xml}"
        );
        let nic = domainxml::bridges(&xml).unwrap().remove(0);
        assert_eq!(
            nic.mac,
            stable_mac(ID, "net0"),
            "a NIC without a MAC is given the deterministic one"
        );
        assert_eq!(manager.list().unwrap().len(), 1);
        assert_eq!(
            manager.define(&spa(ID, &primary, "")).unwrap_err(),
            "VM already exists"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn define_rejects_a_bad_disk_or_a_second_guest_holding_it() {
        let (manager, root) = manager("diskcheck");
        let primary = disk(&root, "a.raw");
        manager.define(&spa(ID, &primary, "")).unwrap();
        let other = disk(&root, "b.raw");
        let e = manager.define(&spa(ID2, &other, "")).is_ok();
        assert!(e, "a different file is fine");
        // A second VM on the same file is refused.
        manager.delete(ID2).unwrap();
        manager.define(&spa(ID2, &other, "")).unwrap();
        assert!(
            manager
                .define(&spa("550e8400-e29b-41d4-a716-446655440002", &other, ""))
                .unwrap_err()
                .contains("already attached")
        );
        // A relative path and a directory are not disks.
        assert!(
            manager
                .define(&spa(ID2, Path::new("relative.raw"), ""))
                .is_err()
        );
        assert!(manager.define(&spa(ID2, &root, "")).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn update_xml_keeps_the_identity_and_autostart() {
        let (manager, root) = manager("update");
        let primary = disk(&root, "disk.raw");
        manager.define(&spa(ID, &primary, "")).unwrap();
        let edited = spa(ID, &primary, "")
            .replace("<vcpu>2</vcpu>", "<vcpu>4</vcpu>")
            .replace("<title>web</title>", "");
        manager.update_xml(ID, &edited).unwrap();
        let xml = manager.xml(ID).unwrap();
        assert!(xml.contains("<vcpu>4</vcpu>"), "{xml}");
        assert!(xml.contains("<title>web</title>"), "{xml}");
        assert_eq!(
            manager
                .update_xml(ID, &edited.replace(ID, ID2))
                .unwrap_err(),
            format!(
                "the <name> cannot change from '{ID}' to '{ID2}'; the display name is the <title>"
            )
        );
        assert!(manager.update_xml(ID2, &edited).is_err(), "unknown VM");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_title_and_autostart_are_persisted_without_touching_the_identity() {
        let (manager, root) = manager("title");
        let primary = disk(&root, "disk.raw");
        manager.define(&spa(ID, &primary, "")).unwrap();
        assert_eq!(manager.set_title(ID, "  db  ").unwrap(), "db");
        assert_eq!(manager.list().unwrap()[0].title, "db");
        assert!(manager.xml(ID).unwrap().contains("<title>db</title>"));
        assert!(manager.set_title(ID, "").is_err(), "empty");
        assert!(manager.set_title(ID, "a/b").is_err(), "not a URL segment");

        assert!(!manager.autostart(ID));
        manager.set_autostart(ID, true).unwrap();
        assert!(manager.autostart(ID));
        // A later edit keeps it.
        manager
            .update_xml(ID, &manager.xml(ID).unwrap().replace(">2<", ">4<"))
            .unwrap();
        assert!(manager.autostart(ID));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_stats_of_a_stopped_guest_come_from_its_definition() {
        let (manager, root) = manager("stats");
        let primary = disk(&root, "disk.raw");
        manager.define(&spa(ID, &primary, "")).unwrap();
        let s = manager.stats(ID).unwrap();
        assert_eq!(s.state, "stopped");
        assert_eq!(
            (s.vcpus, s.memory_kib, s.max_memory_kib),
            (2, 512 * 1024, 512 * 1024)
        );
        assert_eq!(s.cpu_time_ns, 0, "no process, no CPU time");
        assert_eq!(s.rss_kib, Some(0));
        assert!(manager.stats(ID2).is_err(), "unknown VM");
        fs::remove_dir_all(root).unwrap();
    }

    /// A bridge NIC is named by its derived tap and carries its rate limit; a
    /// user-mode NIC has no host interface. Nothing is read from sysfs for a
    /// stopped guest, so no root and no interfaces are needed.
    #[test]
    fn interfaces_report_the_tap_bridge_and_bandwidth() {
        let (manager, root) = manager("ifaces");
        let primary = disk(&root, "disk.raw");
        let xml = spa(ID, &primary, "").replace(
            "<model type=\"virtio\"/>\n    </interface>",
            "<model type=\"virtio\"/>\n      <bandwidth><inbound average=\"1250\"/><outbound average=\"2500\"/></bandwidth>\n    </interface>",
        );
        manager.define(&xml).unwrap();
        manager.add_nic(ID, None, "").unwrap();
        let nics = manager.interfaces(ID).unwrap();
        let mac = stable_mac(ID, "net0");
        assert_eq!(nics.len(), 2);
        assert_eq!(nics[0].mac.as_deref(), Some(mac.as_str()));
        assert_eq!(nics[0].model, "virtio");
        assert_eq!(nics[0].network.as_deref(), Some("vbr5b92a1a54f00"));
        assert_eq!((nics[0].rx_kib, nics[0].tx_kib), (Some(1250), Some(2500)));
        assert_eq!(nics[0].dev, None, "a stopped guest has no tap");
        assert_eq!((nics[0].rx_bytes, nics[0].tx_bytes), (0, 0));
        assert_eq!(nics[1].network, None, "user-mode has no bridge");
        assert_eq!(nics[1].dev, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn graphics_are_read_from_the_definition() {
        let (manager, root) = manager("graphics");
        let primary = disk(&root, "disk.raw");
        manager.define(&spa(ID, &primary, "")).unwrap();
        let g = manager.graphics(ID).unwrap().unwrap();
        assert_eq!(
            (g.kind, g.host.as_str(), g.port),
            ("vnc", "127.0.0.1", 5900)
        );
        manager
            .update_xml(
                ID,
                &manager.xml(ID).unwrap().replace(
                    "<graphics type=\"vnc\" port=\"5900\" autoport=\"yes\" listen=\"127.0.0.1\"/>",
                    "<graphics type=\"spice\" port=\"5901\" autoport=\"yes\" listen=\"0.0.0.0\"/>",
                ),
            )
            .unwrap();
        let g = manager.graphics(ID).unwrap().unwrap();
        assert_eq!(
            (g.kind, g.host.as_str(), g.port),
            ("spice", "0.0.0.0", 5901)
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// Delete removes the runtime (QMP socket, NVRAM) and keeps every disk.
    #[test]
    fn a_delete_removes_runtime_files_but_never_a_disk() {
        let (manager, root) = manager("delete");
        let primary = disk(&root, "disk.raw");
        manager.define(&spa(ID, &primary, "")).unwrap();
        let runtime = manager.state_root.join(ID);
        fs::create_dir_all(&runtime).unwrap();
        fs::write(runtime.join("nvram.fd"), b"vars").unwrap();
        manager.delete(ID).unwrap();
        assert!(!runtime.exists(), "the runtime directory is gone");
        assert!(primary.exists(), "the disk file is not");
        assert!(manager.list().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    /// Every NIC goes through one argv builder: user-mode NAT, a bridge NIC
    /// names the deterministic tap and MAC, and the model is honoured.
    #[test]
    fn the_argv_builds_one_netdev_and_device_per_nic() {
        let (manager, root) = manager("argv");
        let primary = disk(&root, "disk.raw");
        manager.define(&spa(ID, &primary, "")).unwrap();
        let args = manager.args(ID).unwrap();
        let mac = stable_mac(ID, "net0");
        let netdev = netdev_id(&mac);
        assert_eq!(
            values(&args, "-netdev"),
            [format!(
                "tap,id={netdev},ifname={},script=no,downscript=no",
                tap_name(ID, &mac)
            )]
        );
        assert_eq!(
            values(&args, "-device")
                .into_iter()
                .filter(|d| d.starts_with("virtio-net-pci"))
                .collect::<Vec<_>>(),
            [format!(
                "virtio-net-pci,id={},netdev={netdev},mac={mac}",
                nic_device_id(&mac)
            )]
        );

        manager.add_nic(ID, None, "").unwrap();
        manager.add_nic(ID, Some("vbr5b92a1a54f00"), "").unwrap();
        let args = manager.args(ID).unwrap();
        let nics: Vec<String> = values(&args, "-device")
            .into_iter()
            .filter(|d| d.contains("-net-pci"))
            .collect();
        assert_eq!(nics.len(), 3);
        assert!(nics[1].starts_with("virtio-net-pci,"));
        assert_eq!(
            values(&args, "-netdev")[1],
            format!("user,id={}", netdev_id(&stable_mac(ID, "net1")))
        );
        let xml = manager.xml(ID).unwrap();
        assert!(xml.contains("<interface type=\"user\">"), "{xml}");
        assert!(
            xml.contains(&format!("<mac address=\"{}\"/>", stable_mac(ID, "net1"))),
            "{xml}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// The full hardware editor reaches the argv: firmware, disk buses, display,
    /// video, hostdevs, rng, balloon, channel, tpm and watchdog.
    #[test]
    fn the_argv_maps_the_hardware_editor() {
        let (manager, root) = manager("hw");
        let primary = disk(&root, "disk.qcow2");
        let iso = disk(&root, "debian.iso");
        let extra = format!(
            r#"<disk type="file" device="cdrom">
      <driver name="qemu" type="raw"/>
      <source file="{}"/>
      <target dev="sda" bus="sata"/>
      <readonly/>
    </disk>
    <disk type="file" device="disk">
      <driver name="qemu" type="raw"/>
      <source file="{}"/>
      <target dev="sdb" bus="sata"/>
    </disk>
    <interface type="user"><model type="e1000e"/></interface>
    <video><model type="qxl" ram="65536" vgamem="16384" heads="2"/></video>
    <hostdev mode="subsystem" type="pci" managed="yes">
      <source><address domain="0x0000" bus="0x01" slot="0x00" function="0x0"/></source>
    </hostdev>
    <hostdev mode="subsystem" type="usb" managed="yes">
      <source><vendor id="0x1234"/><product id="0x5678"/></source>
    </hostdev>
    <rng model="virtio"><backend model="random">/dev/urandom</backend></rng>
    <memballoon model="virtio"/>
    <channel type="unix"><target type="virtio" name="org.qemu.guest_agent.0"/></channel>
    <watchdog model="i6300esb" action="poweroff"/>"#,
            iso.display(),
            disk(&root, "data.raw").display()
        );
        let xml = spa(ID, &primary, &extra).replace(
            "<boot dev=\"hd\"/>",
            "<loader readonly=\"yes\" type=\"pflash\" format=\"raw\">/opt/fw/OVMF_CODE.fd</loader>\n    <nvram template=\"/opt/fw/OVMF_VARS.fd\">/var/lib/libvirt/qemu/nvram/x.fd</nvram>\n    <boot dev=\"hd\"/>\n    <boot dev=\"cdrom\"/>",
        );
        manager.define(&xml).unwrap();
        let args = manager.args(ID).unwrap();
        let machine = one(&args, "-machine");
        assert_eq!(machine, "q35,accel=kvm");
        assert_eq!(one(&args, "-cpu"), "host");
        assert_eq!(one(&args, "-smp"), "2");
        assert_eq!(one(&args, "-m"), "512");
        assert_eq!(one(&args, "-boot"), "order=cd", "hd then cdrom");
        assert_eq!(
            values(&args, "-drive"),
            [
                "if=pflash,format=raw,readonly=on,file=/opt/fw/OVMF_CODE.fd".to_string(),
                format!(
                    "if=pflash,format=raw,file={}",
                    root.join("state").join(ID).join("nvram.fd").display()
                ),
            ]
        );
        let blockdevs = values(&args, "-blockdev");
        assert_eq!(
            blockdevs[0],
            format!(
                "driver=file,filename={},node-name=file-vda",
                fs::canonicalize(&primary).unwrap().display()
            )
        );
        assert_eq!(
            blockdevs[1],
            "driver=qcow2,file=file-vda,node-name=drive-vda"
        );
        let devices = values(&args, "-device");
        assert!(devices.contains(&"virtio-blk-pci,id=vda,drive=drive-vda".to_string()));
        assert!(devices.contains(&"ide-hd,id=sdb,bus=ide.1,drive=drive-sdb".to_string()));
        assert!(devices.contains(&"ide-cd,id=sda,bus=ide.0,drive=drive-sda".to_string()));
        assert!(
            blockdevs
                .contains(&"driver=raw,file=file-sda,node-name=drive-sda,read-only=on".to_string())
        );
        assert!(devices.iter().any(|d| d.starts_with("e1000e,id=")));
        assert!(
            devices.contains(&"qxl-vga,ram_size=67108864,vgamem_mb=16,max_outputs=2".to_string())
        );
        assert!(devices.contains(&"vfio-pci,host=0000:01:00.0".to_string()));
        assert!(devices.contains(&"qemu-xhci,id=xhci".to_string()));
        assert!(
            devices.contains(&"usb-host,bus=xhci.0,vendorid=0x1234,productid=0x5678".to_string())
        );
        assert!(devices.contains(&"virtio-rng-pci,rng=rng0".to_string()));
        assert!(devices.contains(&"virtio-balloon-pci".to_string()));
        assert!(devices.contains(
            &"virtserialport,bus=virtio-serial0.0,chardev=qga0,id=qga0,name=org.qemu.guest_agent.0".to_string()
        ));
        assert_eq!(values(&args, "-watchdog-action"), ["poweroff"]);
        assert!(devices.contains(&"i6300esb".to_string()));
        assert_eq!(one(&args, "-vnc"), "127.0.0.1:0");
        assert!(values(&args, "-display").is_empty());
        assert_eq!(
            values(&args, "-serial"),
            [format!(
                "unix:{},server=on,wait=off",
                root.join("state").join(ID).join("serial.sock").display()
            )]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_argv_binds_a_pty_serial_to_a_stable_socket() {
        let (manager, root) = manager("serial");
        let primary = disk(&root, "a.raw");
        manager.define(&spa(ID, &primary, "")).unwrap();
        let socket = root.join("state").join(ID).join("serial.sock");
        assert_eq!(
            values(&manager.args(ID).unwrap(), "-serial"),
            [format!("unix:{},server=on,wait=off", socket.display())]
        );

        // No `<serial>` or `<console>`: no serial device, not a socket nobody
        // asked for.
        let bare = manager
            .xml(ID)
            .unwrap()
            .replace("<serial type=\"pty\"><target port=\"0\"/></serial>", "")
            .replace(
                "<console type=\"pty\"><target type=\"serial\" port=\"0\"/></console>",
                "",
            );
        manager.update_xml(ID, &bare).unwrap();
        assert!(values(&manager.args(ID).unwrap(), "-serial").is_empty());

        // A null serial still maps to QEMU's own "none".
        manager
            .update_xml(
                ID,
                &bare.replace("</devices>", "<serial type=\"null\"/></devices>"),
            )
            .unwrap();
        assert_eq!(values(&manager.args(ID).unwrap(), "-serial"), ["none"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_argv_maps_spice_scsi_and_no_display() {
        let (manager, root) = manager("display");
        let primary = disk(&root, "a.raw");
        let xml = spa(ID, &primary, "")
            .replace(
                "<graphics type=\"vnc\" port=\"5900\" autoport=\"yes\" listen=\"127.0.0.1\"/>",
                "<graphics type=\"spice\" port=\"5901\" listen=\"0.0.0.0\"/>",
            )
            .replace(
                "<target dev=\"vda\" bus=\"virtio\"/>",
                "<target dev=\"vda\" bus=\"scsi\"/>",
            );
        manager.define(&xml).unwrap();
        let args = manager.args(ID).unwrap();
        assert!(values(&args, "-device").contains(&"virtio-scsi-pci,id=scsi0".to_string()));
        assert!(
            values(&args, "-device")
                .contains(&"scsi-hd,id=vda,bus=scsi0.0,drive=drive-vda".to_string())
        );
        assert_eq!(
            one(&args, "-spice"),
            "port=5901,addr=0.0.0.0,disable-ticketing=on"
        );

        manager
            .update_xml(
                ID,
                &xml.replace(
                    "<graphics type=\"spice\" port=\"5901\" listen=\"0.0.0.0\"/>",
                    "",
                ),
            )
            .unwrap();
        let args = manager.args(ID).unwrap();
        assert_eq!(one(&args, "-display"), "none");
        assert!(values(&args, "-vnc").is_empty() && values(&args, "-spice").is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    /// A 3D virtio display with no VNC/SPICE needs a display of its own; with
    /// one, the remote display is kept and the -gl device rides on it.
    #[test]
    fn a_gl_video_without_a_display_gets_egl_headless() {
        let (manager, root) = manager("gl");
        let primary = disk(&root, "a.raw");
        let xml = spa(ID, &primary, "")
            .replace(
                "<graphics type=\"vnc\" port=\"5900\" autoport=\"yes\" listen=\"127.0.0.1\"/>",
                "",
            )
            .replace(
                "<devices>",
                "<devices><video><model type=\"virtio\" device=\"virtio-vga-gl\"><acceleration accel3d=\"yes\"/></model></video>",
            );
        manager.define(&xml).unwrap();
        let args = manager.args(ID).unwrap();
        assert_eq!(one(&args, "-display"), "egl-headless");
        assert!(
            values(&args, "-device").contains(&"virtio-vga-gl".to_string()),
            "{args:?}"
        );

        // With a VNC display the -gl device keeps that display.
        manager
            .update_xml(
                ID,
                &manager.xml(ID).unwrap().replace(
                    "</devices>",
                    "<graphics type=\"vnc\" port=\"5900\" listen=\"127.0.0.1\"/></devices>",
                ),
            )
            .unwrap();
        let args = manager.args(ID).unwrap();
        assert!(values(&args, "-display").is_empty());
        assert_eq!(one(&args, "-vnc"), "127.0.0.1:0");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_uefi_guest_keeps_its_variable_store_in_the_runtime_directory() {
        let (manager, root) = manager("uefi");
        let primary = disk(&root, "a.raw");
        let xml = spa(ID, &primary, "").replace(
            "<boot dev=\"hd\"/>",
            "<loader readonly=\"yes\" type=\"pflash\" format=\"raw\">/opt/fw/OVMF_CODE.fd</loader>\n    <nvram template=\"/opt/fw/OVMF_VARS.fd\">/var/lib/libvirt/qemu/nvram/x.fd</nvram>\n    <boot dev=\"hd\"/>",
        );
        manager.define(&xml).unwrap();
        let stored = manager.xml(ID).unwrap();
        let nvram = manager.state_root.join(ID).join("nvram.fd");
        assert!(
            stored.contains(&format!(">{}</nvram>", nvram.display())),
            "{stored}"
        );
        let args = manager.args(ID).unwrap();
        assert_eq!(
            values(&args, "-drive")[1],
            format!("if=pflash,format=raw,file={}", nvram.display())
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// A TPM is a chardev into a running swtpm; without one the argv is refused
    /// rather than handing QEMU a socket that does not exist.
    #[test]
    fn a_tpm_needs_its_swtpm_socket() {
        use std::os::unix::net::UnixListener;
        let (manager, root) = manager("tpm");
        let primary = disk(&root, "a.raw");
        let xml = spa(
            ID,
            &primary,
            "<tpm model=\"tpm-crb\"><backend type=\"emulator\" version=\"2.0\"/></tpm>",
        );
        manager.define(&xml).unwrap();
        let e = manager.args(ID).unwrap_err();
        assert!(e.contains("swtpm"), "{e}");

        let dir = manager.state_root.join(ID);
        fs::create_dir_all(&dir).unwrap();
        let _socket = UnixListener::bind(dir.join("swtpm.sock")).unwrap();
        let args = manager.args(ID).unwrap();
        assert_eq!(
            values(&args, "-tpmdev"),
            ["emulator,id=tpm0,chardev=chrtpm"]
        );
        assert!(values(&args, "-device").contains(&"tpm-crb,tpmdev=tpm0".to_string()));
        assert_eq!(
            values(&args, "-chardev")[0],
            format!("socket,id=chrtpm,path={}", dir.join("swtpm.sock").display())
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// The TPM version in the XML decides whether swtpm runs TPM 2
    /// functionality: only 2.0 gets `--tpm2`.
    #[test]
    fn the_swtpm_argv_follows_the_tpm_version_in_the_definition() {
        let state = Path::new("/run/vm/tpm");
        let sock = Path::new("/run/vm/swtpm.sock");
        let pid = Path::new("/run/vm/swtpm.pid");
        let tpm = |version: &str| {
            domainxml::parse(&spa(
                ID,
                Path::new("/tmp/disk.raw"),
                &format!(
                    "<tpm model=\"tpm-crb\"><backend type=\"emulator\" version=\"{version}\"/></tpm>"
                ),
            ))
            .unwrap()
            .tpm
            .unwrap()
            .version
        };

        assert_eq!(
            swtpm_argv(&tpm("2.0"), state, sock, pid).unwrap(),
            [
                "socket",
                "--tpm2",
                "--tpmstate",
                "dir=/run/vm/tpm",
                "--ctrl",
                "type=unixio,path=/run/vm/swtpm.sock",
                "--daemon",
                "--pid",
                "file=/run/vm/swtpm.pid",
            ]
        );
        assert!(
            !swtpm_argv(&tpm("1.2"), state, sock, pid)
                .unwrap()
                .contains(&"--tpm2".to_string()),
            "a 1.2 TPM is not started in TPM 2 mode"
        );
    }

    /// The pid file alone is not proof of ownership, and a process that is not
    /// this guest's swtpm must never be signalled.
    #[test]
    fn swtpm_identity_needs_this_guests_state_and_socket() {
        let state = Path::new("/run/vm/tpm");
        let sock = Path::new("/run/vm/swtpm.sock");
        let pid = Path::new("/run/vm/swtpm.pid");
        let argv = swtpm_argv("2.0", state, sock, pid).unwrap();
        let mut cmdline = b"/usr/bin/swtpm".to_vec();
        for arg in argv {
            cmdline.push(0);
            cmdline.extend_from_slice(arg.as_bytes());
        }
        cmdline.push(0);
        assert!(swtpm_cmdline_is_ours(&cmdline, state, sock));
        assert!(
            !swtpm_cmdline_is_ours(&cmdline, Path::new("/run/other/tpm"), sock),
            "another guest's state directory is not this one"
        );
        assert!(
            !swtpm_cmdline_is_ours(&cmdline, state, Path::new("/run/other.sock")),
            "another socket is not this one"
        );
        let mut not_ours = b"/usr/bin/qemu-system-x86_64".to_vec();
        not_ours.push(0);
        not_ours.extend_from_slice(&cmdline);
        assert!(
            !swtpm_cmdline_is_ours(&not_ours, state, sock),
            "a non-swtpm executable is never ours"
        );
    }

    #[test]
    fn stopping_swtpm_never_kills_a_pid_that_is_not_ours() {
        let root = std::env::temp_dir().join(format!("vm-swtpm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("swtpm.pid"), std::process::id().to_string()).unwrap();
        stop_swtpm(&root).unwrap();
        assert!(
            process_ticks(std::process::id()).is_some(),
            "this process is untouched"
        );
        assert!(
            !root.join("swtpm.pid").exists(),
            "the stale pid file is gone"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// A disk identifier is the XML target dev; a hot-added disk gets the first
    /// free one, and removal never deletes the file.
    #[test]
    fn a_disk_is_added_and_removed_by_its_target() {
        let (manager, root) = manager("disk");
        let primary = disk(&root, "a.raw");
        let extra = disk(&root, "b.qcow2");
        manager.define(&spa(ID, &primary, "")).unwrap();
        let (target, path) = manager.add_disk(ID, &extra, DiskFormat::Qcow2).unwrap();
        assert_eq!(target, "vdb");
        assert_eq!(path, fs::canonicalize(&extra).unwrap());
        let xml = manager.xml(ID).unwrap();
        assert!(
            xml.contains("<target dev=\"vdb\" bus=\"virtio\"/>"),
            "{xml}"
        );
        assert!(
            domainxml::parse(&xml).unwrap().disks[1].source == Some(path.clone()),
            "{xml}"
        );
        // The same file, and the same target, are refused.
        assert!(manager.add_disk(ID, &extra, DiskFormat::Raw).is_err());
        assert!(
            manager
                .add_disk(ID, &disk(&root, "c.raw"), DiskFormat::Raw)
                .is_ok()
        );

        assert!(!manager.remove_disk(ID, "vdb").unwrap());
        assert!(extra.exists(), "removing a disk never deletes its file");
        assert_eq!(
            manager.remove_disk(ID, "vdb").unwrap_err(),
            "unknown QEMU disk"
        );
        assert!(
            manager
                .args(ID)
                .unwrap()
                .windows(2)
                .all(|w| w[1] != "drive-vdb")
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// NICs are addressed by the MAC the definition carries, so a hot-removal
    /// does not shift another NIC's identity.
    #[test]
    fn a_nic_is_added_and_removed_by_its_mac() {
        let (manager, root) = manager("nic");
        let primary = disk(&root, "a.raw");
        manager.define(&spa(ID, &primary, "")).unwrap();
        let mac = manager.add_nic(ID, Some("vbr5b92a1a54f00"), "").unwrap();
        assert_eq!(mac, stable_mac(ID, "net1"));
        assert!(!manager.remove_nic(ID, &mac).unwrap());
        assert_eq!(
            manager.remove_nic(ID, &mac).unwrap_err(),
            "unknown QEMU NIC"
        );
        assert_eq!(
            domainxml::parse(&manager.xml(ID).unwrap())
                .unwrap()
                .interfaces
                .len(),
            1
        );
        assert!(manager.remove_nic(ID, "nope").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    /// The Network tab's add form sends an `<interface>` element: its model
    /// and MAC reach the stored definition, and a stopped guest is not live.
    #[test]
    fn a_nic_xml_keeps_the_model_and_mac() {
        let (manager, root) = manager("nicxml");
        let primary = disk(&root, "a.raw");
        manager.define(&spa(ID, &primary, "")).unwrap();
        let mac = "52:54:00:11:22:33";
        let (chosen, live) = manager
            .add_nic_xml(
                ID,
                &format!(
                    "<interface type='bridge'><mac address='{mac}'/>\
                     <source bridge='vbr5b92a1a54f00'/><model type='e1000e'/></interface>"
                ),
            )
            .unwrap();
        assert_eq!(chosen, mac);
        assert!(!live);
        let iface = domainxml::parse(&manager.xml(ID).unwrap())
            .unwrap()
            .interfaces
            .remove(1);
        assert_eq!(iface.model, IfaceModel::E1000e);
        assert_eq!(iface.mac.as_deref(), Some(mac));
        assert!(
            manager
                .add_nic_xml(
                    ID,
                    "<interface type='network'><source network='x'/></interface>"
                )
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// A rate limit is stored as libvirt's KiB/s `<bandwidth>` and a stopped
    /// guest has no tap to shape.
    #[test]
    fn nic_bandwidth_is_stored_and_a_stopped_guest_has_no_tap() {
        let (manager, root) = manager("bw");
        let primary = disk(&root, "a.raw");
        manager.define(&spa(ID, &primary, "")).unwrap();
        let mac = stable_mac(ID, "net0");
        assert_eq!(
            manager
                .set_nic_bandwidth(ID, &mac, Some(1250), None)
                .unwrap(),
            None
        );
        let xml = manager.xml(ID).unwrap();
        assert!(
            xml.contains("<bandwidth><inbound average=\"1250\"/></bandwidth>"),
            "{xml}"
        );
        manager
            .set_nic_bandwidth(ID, &mac, None, Some(2500))
            .unwrap();
        let xml = manager.xml(ID).unwrap();
        assert!(
            xml.contains("<bandwidth><outbound average=\"2500\"/></bandwidth>"),
            "{xml}"
        );
        manager.set_nic_bandwidth(ID, &mac, None, None).unwrap();
        assert!(!manager.xml(ID).unwrap().contains("<bandwidth"), "cleared");
        assert_eq!(
            manager
                .set_nic_bandwidth(ID, "52:54:00:aa:bb:99", Some(1), None)
                .unwrap_err(),
            "unknown QEMU NIC"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// A stopped guest's block counters are zero without a QMP socket.
    #[test]
    fn a_stopped_guests_disk_bytes_are_empty() {
        let (manager, root) = manager("blockstats");
        let primary = disk(&root, "a.raw");
        manager.define(&spa(ID, &primary, "")).unwrap();
        assert!(manager.block_stats(ID).unwrap().is_empty());
        assert!(manager.block_stats(ID2).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_stopped_guest_can_be_moved_between_bridges_and_user_mode() {
        let (manager, root) = manager("bridge");
        let primary = disk(&root, "a.raw");
        manager.define(&spa(ID, &primary, "")).unwrap();
        let mac = stable_mac(ID, "net0");
        manager
            .set_nic_bridge(ID, &mac, Some("vbr0000000000"))
            .unwrap();
        assert_eq!(
            domainxml::bridges(&manager.xml(ID).unwrap()).unwrap()[0].bridge,
            "vbr0000000000"
        );
        manager.set_nic_bridge(ID, &mac, None).unwrap();
        assert!(
            domainxml::bridges(&manager.xml(ID).unwrap())
                .unwrap()
                .is_empty()
        );
        assert!(manager.set_nic_bridge(ID, &mac, Some("bad name")).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_mac_and_a_tap_are_stable_and_distinct_per_interface() {
        let mac = stable_mac(ID, "net0");
        assert_eq!(mac, stable_mac(ID, "net0"), "same slot, same MAC");
        assert_eq!(mac.len(), 17);
        assert!(mac.starts_with("52:54:00:"), "{mac}");
        assert_eq!(mac, mac.to_ascii_lowercase());
        assert_eq!(
            u8::from_str_radix(&mac[..2], 16).unwrap() & 1,
            0,
            "a NIC's MAC is unicast"
        );
        assert_ne!(mac, stable_mac(ID, "net1"));
        assert_ne!(mac, stable_mac(ID2, "net0"));

        let tap = tap_name(ID, &mac);
        assert_eq!(tap, tap_name(ID, &mac), "same MAC, same tap");
        assert!(tap.starts_with("tap"), "{tap}");
        assert!(tap.len() <= 15, "the Linux interface-name limit: {tap}");
        assert!(tap[3..].chars().all(|c| c.is_ascii_hexdigit()), "{tap}");
        assert_ne!(tap, tap_name(ID, &stable_mac(ID, "net1")));
    }

    #[test]
    fn a_supplied_mac_is_normalized_and_must_be_unicast() {
        let none = BTreeSet::new();
        assert_eq!(
            mac_for(&none, ID, "net0", "52-54-00-AB-CD-EF").unwrap(),
            "52:54:00:ab:cd:ef"
        );
        assert_eq!(
            mac_for(&none, ID, "net0", " ").unwrap(),
            stable_mac(ID, "net0")
        );
        assert!(
            mac_for(&none, ID, "net0", "01:00:5e:00:00:01").is_err(),
            "multicast"
        );
        assert!(
            mac_for(&none, ID, "net0", "00:00:00:00:00:00").is_err(),
            "the all-zero address names no NIC"
        );
        assert!(mac_for(&none, ID, "net0", "52:54:00:aa:bb").is_err());
        assert!(mac_for(&none, ID, "net0", "52:54:00:aa:bb:zz").is_err());
    }

    /// A generated MAC that another NIC already holds is re-derived with a
    /// salt, and an explicitly supplied one that is taken is refused: one MAC
    /// per NIC or dnsmasq would hand one address to two guests.
    #[test]
    fn a_mac_is_never_shared_between_nics() {
        let base = stable_mac(ID, "net0");
        let mut used = BTreeSet::new();
        used.insert(base.clone());
        let generated = mac_for(&used, ID, "net0", "").unwrap();
        assert_ne!(generated, base, "the taken MAC is skipped");
        assert_eq!(
            generated,
            stable_mac_salted(ID, "net0", 1),
            "the first free salt wins, deterministically"
        );
        assert!(
            mac_for(&used, ID, "net0", &base).is_err(),
            "a supplied duplicate is refused"
        );
    }

    #[test]
    fn only_a_bridge_is_ever_attached_to_or_a_tap_deleted() {
        // `lo` exists on every host and is not a bridge, so this needs no root
        // and no interface creation.
        assert!(!is_bridge("lo"));
        assert!(ensure_tap("tap1234567890", "lo").is_err());
        assert!(!tap_attached("lo", "lo"));
        assert!(delete_tap("lo").is_ok(), "a non-tap is never deleted");
        assert!(delete_tap("tapnosuch0").is_ok(), "an absent tap is success");
    }
}
