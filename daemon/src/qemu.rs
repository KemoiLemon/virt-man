//! Isolated direct-QEMU process manager; intentionally separate from libvirt.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
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

use crate::hostnet::SYS_NET;
pub use crate::hostnet::is_bridge;
pub use crate::qemu_network::HostForward;

#[derive(Clone, Debug)]
pub struct Manager {
    data_dir: PathBuf,
    state_root: PathBuf,
    qemu: PathBuf,
    lock: std::sync::Arc<Mutex<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Network {
    /// QEMU's own user-mode NAT: no host interface, no address from the store.
    User {
        #[serde(default)]
        hostfwd: Vec<HostForward>,
    },
    /// A host tap plugged into a stored network's bridge.
    Bridge {
        /// The stored network key the NIC is on, for the address reservation.
        network: String,
        bridge: String,
        mac: String,
    },
}

/// The first release wrote user-mode networking as a bare block
/// (`{"hostfwd":[...]}`), before a NIC could be on a bridge. That shape must
/// still load, or a daemon upgrade would refuse the file it finds.
impl<'de> Deserialize<'de> for Network {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "mode", rename_all = "snake_case")]
        enum Tagged {
            User {
                #[serde(default)]
                hostfwd: Vec<HostForward>,
            },
            Bridge {
                network: String,
                bridge: String,
                mac: String,
            },
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyUser {
            #[serde(default)]
            hostfwd: Vec<HostForward>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Tagged(Tagged),
            // `deny_unknown_fields` keeps a malformed tagged value from
            // silently becoming user-mode NAT.
            LegacyUser(LegacyUser),
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Tagged(Tagged::User { hostfwd }) | Repr::LegacyUser(LegacyUser { hostfwd }) => {
                Network::User { hostfwd }
            }
            Repr::Tagged(Tagged::Bridge {
                network,
                bridge,
                mac,
            }) => Network::Bridge {
                network,
                bridge,
                mac,
            },
        })
    }
}

impl Default for Network {
    fn default() -> Self {
        Self::User { hostfwd: vec![] }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Nic {
    /// QEMU device id; the primary NIC is "net0", extras are user-supplied.
    pub id: String,
    pub network: Network,
}

/// The first release stored an extra NIC as a bare device id. Both that shape
/// and the object form must load.
impl<'de> Deserialize<'de> for Nic {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Id(String),
            Full {
                id: String,
                #[serde(default)]
                network: Network,
            },
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Id(id) => Nic {
                id,
                network: Network::default(),
            },
            Repr::Full { id, network } => Nic { id, network },
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskFormat {
    #[default]
    Raw,
    Qcow2,
}

impl DiskFormat {
    /// The QEMU driver name, which is also the wire spelling of the format.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Qcow2 => "qcow2",
        }
    }
}

/// An extra disk, hot-added through QMP. The primary `VmConfig::disk` predates
/// this and keeps its `-drive ... if=virtio` launch shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    /// QEMU device id; names the block nodes `file-<id>`/`drive-<id>` too.
    pub id: String,
    pub path: PathBuf,
    #[serde(default)]
    pub format: DiskFormat,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VmConfig {
    pub id: String,
    /// What the user calls it: the IPAM name (DNS/DHCP) and what `list` shows.
    #[serde(default)]
    pub title: String,
    pub disk: PathBuf,
    pub memory_mib: u32,
    pub vcpus: u16,
    #[serde(default)]
    pub network: Network,
    /// Explicit inbound host-forward allowlist; empty means deny all inbound.
    #[serde(default)]
    pub firewall: crate::qemu_network::InboundPolicy,
    #[serde(default)]
    pub extra_nics: Vec<Nic>,
    #[serde(default)]
    pub extra_disks: Vec<Disk>,
}

impl VmConfig {
    /// The primary NIC, then every extra one, as (id, network). One path for
    /// the launch argv, the host taps and the address reservations.
    pub fn nics(&self) -> impl Iterator<Item = (&str, &Network)> {
        std::iter::once(("net0", &self.network))
            .chain(self.extra_nics.iter().map(|n| (n.id.as_str(), &n.network)))
    }

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
    #[serde(default)]
    disk_size: u64,
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
    /// not mistaken for the guest. Returns whether there was an identity to
    /// forget: the sweep runs every pass and must not rewrite the file when
    /// nothing changed.
    fn clear_dead(&self, rec: &mut Record) -> bool {
        if self.identity(rec) {
            return false;
        }
        let had_identity = rec.pid.is_some() || rec.start_ticks.is_some();
        // Taps are removed even when the identity was cleared by an earlier
        // pass: a guest that powered itself off can leave one at any time.
        delete_nic_taps(&rec.config);
        rec.pid = None;
        rec.start_ticks = None;
        had_identity
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
    pub fn define(&self, config: VmConfig) -> Result<(), String> {
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        if !Self::valid_id(&config.id) {
            return Err("invalid VM id".into());
        }
        if config.memory_mib == 0 || config.vcpus == 0 {
            return Err("memory and vCPU count must be positive".into());
        }
        if !config.disk.is_absolute() {
            return Err("disk must be an existing absolute file path".into());
        }
        let link_meta =
            fs::symlink_metadata(&config.disk).map_err(|e| format!("inspect disk: {e}"))?;
        if !link_meta.file_type().is_file() {
            return Err("disk must be a regular file, not a symlink".into());
        }
        if config.memory_mib > 1_048_576
            || config.vcpus > 256
            || link_meta.len() > 16 * 1024 * 1024 * 1024 * 1024
        {
            return Err("memory or vCPU count exceeds the supported limit".into());
        }
        let canonical_disk =
            fs::canonicalize(&config.disk).map_err(|e| format!("canonicalize disk: {e}"))?;
        let disk_meta = fs::metadata(&canonical_disk).map_err(|e| e.to_string())?;
        if !disk_meta.is_file() || disk_meta.len() == 0 {
            return Err("disk must be a non-empty regular file".into());
        }
        let mut config = config;
        config.disk = canonical_disk;
        // QEMU's argv is UTF-8 text: refuse a primary path it cannot spell now,
        // rather than defining a VM that can never start.
        escape_drive_path(&config.disk)?;
        let mut all = self.read()?;
        if all.contains_key(&config.id) {
            return Err("VM already exists".into());
        }
        all.insert(
            config.id.clone(),
            Record {
                config,
                disk_size: disk_meta.len(),
                pid: None,
                start_ticks: None,
            },
        );
        self.save(&all)
    }
    pub fn set_network(&self, id: &str, network: Network) -> Result<(), String> {
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get(id).ok_or("unknown QEMU VM")?;
        if self.identity(rec) {
            return Err("VM must be stopped to change networking".into());
        }
        // hostfwd is policy, never an independent networking knob.
        let hostfwd = rec.config.firewall.hostfwd.clone();
        let old = rec.config.network.clone();
        let network = match network {
            Network::User { .. } => Network::User { hostfwd },
            Network::Bridge {
                network,
                bridge,
                mac,
            } => {
                if network.is_empty() {
                    return Err("a bridge NIC must name its network".into());
                }
                // The bridge need not exist yet: a NIC may be configured before
                // its network is started. `start` verifies it via `ensure_tap`.
                Network::Bridge {
                    network,
                    bridge,
                    mac: mac_for(&all, id, "net0", &mac)?,
                }
            }
        };
        // The primary tap's name is derived from the VM and interface, so a
        // switch to another network (or to user mode) must drop the old one or
        // the next start would find it on the wrong bridge.
        if matches!(old, Network::Bridge { .. }) {
            delete_nic_tap(id, "net0");
        }
        all.get_mut(id).unwrap().config.network = network;
        self.save(&all)
    }

    /// Point one stored bridge NIC at the bridge its network has now. Called
    /// before a start, because a network's mode edit or re-create changes the
    /// derived bridge and a cached one would attach the tap to the old network.
    /// Stopped only: a live guest's tap is still on the old bridge.
    pub fn rebind_bridge(&self, id: &str, nic_id: &str, bridge: &str) -> Result<(), String> {
        if !crate::hostnet::valid_name(bridge) {
            return Err("invalid bridge name".into());
        }
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get_mut(id).ok_or("unknown QEMU VM")?;
        if self.identity(rec) {
            return Err("VM must be stopped to change networking".into());
        }
        match nic_network_mut(&mut rec.config, nic_id) {
            Some(Network::Bridge { bridge: b, .. }) => *b = bridge.to_string(),
            Some(Network::User { .. }) => return Err("NIC is not on a bridge network".into()),
            None => return Err("unknown QEMU NIC".into()),
        }
        self.save(&all)
    }

    pub fn set_firewall(
        &self,
        id: &str,
        firewall: crate::qemu_network::InboundPolicy,
    ) -> Result<(), String> {
        firewall.validate()?;
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get_mut(id).ok_or("unknown QEMU VM")?;
        if self.identity(rec) {
            return Err("VM must be stopped to change firewall policy".into());
        }
        rec.config.firewall = firewall.clone();
        // hostfwd only exists on a user-mode NIC; a bridge NIC keeps none.
        if let Network::User { hostfwd } = &mut rec.config.network {
            *hostfwd = firewall.hostfwd;
        }
        self.save(&all)
    }
    pub fn add_nic(&self, id: &str, device_id: &str, network: Network) -> Result<(), String> {
        validate_new_nic_id(device_id)?;
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        {
            let rec = all.get(id).ok_or("unknown QEMU VM")?;
            if device_id_taken(&rec.config, device_id) {
                return Err("device identifier already exists".into());
            }
        }
        let network = match network {
            // hostfwd belongs to the primary NIC's firewall policy only, so an
            // extra user-mode NIC never carries one.
            Network::User { .. } => Network::User { hostfwd: vec![] },
            Network::Bridge {
                network,
                bridge,
                mac,
            } => {
                if network.is_empty() {
                    return Err("a bridge NIC must name its network".into());
                }
                // The bridge need not exist yet; `start`/this call's live path
                // verifies it via `ensure_tap`.
                Network::Bridge {
                    network,
                    bridge,
                    mac: mac_for(&all, id, device_id, &mac)?,
                }
            }
        };
        let live = self.identity(all.get(id).ok_or("unknown QEMU VM")?);
        let net_id = netdev_id(device_id);
        let mut tap = None;
        if live {
            let sock = self.vm_dir(id).join("qmp.sock");
            let add = match &network {
                Network::User { .. } => serde_json::json!({"type":"user", "id":net_id}),
                Network::Bridge { bridge, .. } => {
                    let name = tap_name(id, device_id);
                    // Before QMP: a device whose host tap is missing must never
                    // be published to the guest.
                    ensure_tap(&name, bridge)?;
                    tap = Some(name.clone());
                    serde_json::json!({
                        "type":"tap", "id":net_id, "ifname":name,
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
            let device = match &network {
                Network::Bridge { mac, .. } => serde_json::json!({
                    "driver":"virtio-net-pci", "id":device_id, "netdev":net_id, "mac":mac
                }),
                Network::User { .. } => serde_json::json!({
                    "driver":"virtio-net-pci", "id":device_id, "netdev":net_id
                }),
            };
            if let Err(e) = qmp_execute(&sock, "device_add", device) {
                let _ = qmp_execute(&sock, "netdev_del", serde_json::json!({"id":net_id}));
                if let Some(tap) = &tap {
                    cleanup_taps(std::slice::from_ref(tap));
                }
                return Err(e);
            }
        }
        all.get_mut(id).unwrap().config.extra_nics.push(Nic {
            id: device_id.to_owned(),
            network,
        });
        if let Err(e) = self.save(&all) {
            if live {
                let sock = self.vm_dir(id).join("qmp.sock");
                let _ = qmp_execute(&sock, "device_del", serde_json::json!({"id":device_id}));
                let _ = qmp_execute(&sock, "netdev_del", serde_json::json!({"id":net_id}));
                // QEMU still holds the device until the asynchronous
                // `device_del` completes; deleting its tap here would pull the
                // backing out from under the guest. The tap name is derived,
                // so stop/reconcile removes it.
            }
            return Err(format!("live NIC added but persistence failed: {e}"));
        }
        Ok(())
    }
    pub fn remove_nic(&self, id: &str, device_id: &str) -> Result<bool, String> {
        validate_device_id(device_id)?;
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get_mut(id).ok_or("unknown QEMU VM")?;
        if !rec.config.extra_nics.iter().any(|n| n.id == device_id) {
            return Err("unknown QEMU NIC".into());
        }
        if self.identity(rec) {
            let sock = self.vm_dir(id).join("qmp.sock");
            qmp_execute(&sock, "device_del", serde_json::json!({"id":device_id}))?;
            // QMP device_del completes asynchronously; keep durable metadata until an
            // authoritative DEVICE_DELETED event can be observed. Never infer from
            // query-pci's human-oriented topology dump.
            // The tap is left alone for the same reason: QEMU still holds the
            // device, and deleting its backing would break the guest. Its name is
            // derived from the VM and interface, so stop/reconcile removes it.
            return Ok(true);
        }
        if let Some(nic) = rec.config.extra_nics.iter().find(|n| n.id == device_id)
            && matches!(nic.network, Network::Bridge { .. })
        {
            delete_nic_tap(id, device_id);
        }
        rec.config.extra_nics.retain(|n| n.id != device_id);
        self.save(&all)?;
        Ok(false)
    }
    /// Attach a disk to a stopped guest by persisting it, or to a running one by
    /// adding its block nodes and device through QMP first. The device is a
    /// `virtio-blk-pci` over an explicit file → format node chain, so a live
    /// guest can have exactly what the next launch will build.
    pub fn add_disk(
        &self,
        id: &str,
        disk_id: &str,
        path: &Path,
        format: DiskFormat,
    ) -> Result<PathBuf, String> {
        validate_new_disk_id(disk_id)?;
        let path = check_disk_file(path)?;
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get(id).ok_or("unknown QEMU VM")?;
        if device_id_taken(&rec.config, disk_id) {
            return Err("device identifier already exists".into());
        }
        // One file backs one device: the same image attached twice is written
        // by two guests and corrupts both.
        if all.values().any(|rec| {
            rec.config.disk == path || rec.config.extra_disks.iter().any(|d| d.path == path)
        }) {
            return Err("disk path is already attached to a VM".into());
        }
        let live = self.identity(rec);
        let file_node = format!("file-{disk_id}");
        let drive_node = format!("drive-{disk_id}");
        let sock = self.vm_dir(id).join("qmp.sock");
        if live {
            let filename = path.to_str().ok_or("disk path is not valid UTF-8")?;
            qmp_execute(
                &sock,
                "blockdev-add",
                serde_json::json!({
                    "driver":"file", "filename":filename, "node-name":&file_node
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
                    "driver":"virtio-blk-pci", "id":disk_id, "drive":&drive_node
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
        all.get_mut(id).unwrap().config.extra_disks.push(Disk {
            id: disk_id.to_owned(),
            path: path.clone(),
            format,
        });
        if let Err(e) = self.save(&all) {
            if live {
                // The device still references the node until the asynchronous
                // `device_del` completes, so a `blockdev-del` may be refused;
                // the guest keeps the disk rather than having it pulled away.
                let _ = qmp_execute(&sock, "device_del", serde_json::json!({"id":disk_id}));
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
            }
            return Err(format!("live disk added but persistence failed: {e}"));
        }
        Ok(path)
    }
    /// Detach a disk. A live guest's `device_del` is a request, so its metadata
    /// stays until the guest acknowledges it — the same rule as a NIC. The file
    /// is never deleted: it holds the guest's data, not the device's identity.
    pub fn remove_disk(&self, id: &str, disk_id: &str) -> Result<bool, String> {
        validate_device_id(disk_id)?;
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get_mut(id).ok_or("unknown QEMU VM")?;
        let Some(index) = rec.config.extra_disks.iter().position(|d| d.id == disk_id) else {
            return Err("unknown QEMU disk".into());
        };
        let live = self.identity(rec);
        if live {
            let sock = self.vm_dir(id).join("qmp.sock");
            qmp_execute(&sock, "device_del", serde_json::json!({"id": disk_id}))?;
            // QMP `device_del` completes asynchronously: the device may stay
            // attached until the guest acknowledges it or the VM stops, which
            // is what `pending` reports. Removing it from the stored config now
            // is what keeps it from coming back at the next start, and makes a
            // repeated DELETE an honest "unknown disk" instead of a QMP error.
        }
        rec.config.extra_disks.remove(index);
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
        let mut config = all.get(id).unwrap().config.clone();
        let link_meta =
            fs::symlink_metadata(&config.disk).map_err(|e| format!("inspect disk: {e}"))?;
        if !link_meta.file_type().is_file() {
            return Err("disk must remain a regular file, not a symlink".into());
        }
        let disk = fs::canonicalize(&config.disk).map_err(|e| format!("canonicalize disk: {e}"))?;
        if disk != config.disk {
            return Err("disk path changed since VM definition".into());
        }
        let original = fs::metadata(&disk).map_err(|e| e.to_string())?;
        if !original.is_file() {
            return Err("disk must be a regular file".into());
        }
        if original.len() == 0 || original.len() != all.get(id).unwrap().disk_size {
            return Err("disk is empty or its size changed since VM definition".into());
        }
        // An extra disk was validated when it was added; re-check the stored
        // path now, or a symlink swapped in since would be followed by QEMU.
        for extra in &config.extra_disks {
            if check_disk_file(&extra.path)? != extra.path {
                return Err(format!(
                    "disk '{}' changed since it was added; re-add it",
                    extra.id
                ));
            }
        }
        let dir = self.vm_dir(id);
        private_dir(&self.state_root)?;
        private_dir(&dir)?;
        let sock = dir.join("qmp.sock");
        if sock.as_os_str().len() > 100 {
            return Err("QMP socket path exceeds Unix socket limit".into());
        }
        private_dir(&self.state_root)?;
        private_dir(&dir)?;
        let _ = fs::remove_file(&sock);
        // hostfwd is policy: the firewall allowlist decides it, not the NIC's
        // stored network object (which is `User` when the guest is on NAT).
        let hostfwd = config.firewall.hostfwd.clone();
        if let Network::User { hostfwd: h } = &mut config.network {
            *h = hostfwd;
        }
        let argv = self.args(&config)?;
        // Taps first: a bridge NIC whose host interface is missing must fail the
        // start, not hand QEMU a device with nowhere to send packets.
        let mut created: Vec<String> = Vec::new();
        for (nic_id, network) in config.nics() {
            let Network::Bridge { bridge, .. } = network else {
                continue;
            };
            let tap = tap_name(&config.id, nic_id);
            let existed = Path::new(SYS_NET).join(&tap).exists();
            if let Err(e) = ensure_tap(&tap, bridge) {
                cleanup_taps(&created);
                return Err(e);
            }
            if !existed {
                created.push(tap);
            }
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
            cleanup_taps(&created);
            return Err("QEMU executable identity mismatch".into());
        }
        let ticks = match process_ticks(pid) {
            Some(ticks) => ticks,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                cleanup_taps(&created);
                return Err("QEMU exited before process identity was recorded".into());
            }
        };
        {
            let rec = all.get_mut(id).unwrap();
            rec.pid = Some(pid);
            rec.start_ticks = Some(ticks);
        }
        if let Err(e) = self.save(&all) {
            let _ = child.kill();
            let _ = child.wait();
            cleanup_taps(&created);
            return Err(e);
        }
        // Do not publish a stale PID if QEMU died while metadata was being saved.
        if !self.identity(all.get(id).unwrap()) || qmp(&sock, "query-status").is_err() {
            let _ = child.kill();
            let _ = child.wait();
            cleanup_taps(&created);
            let rec = all.get_mut(id).unwrap();
            rec.pid = None;
            rec.start_ticks = None;
            let _ = self.save(&all);
            return Err("QEMU failed readiness check".into());
        }
        drop(child);
        Ok(())
    }
    /// The QEMU launch argv: pure, so the device shape can be asserted without
    /// a QEMU or root. The taps it names are created by the caller.
    fn args(&self, config: &VmConfig) -> Result<Vec<String>, String> {
        let sock = self.vm_dir(&config.id).join("qmp.sock");
        let mut args: Vec<String> = vec![
            "-name".into(),
            config.id.clone(),
            "-m".into(),
            config.memory_mib.to_string(),
            "-smp".into(),
            config.vcpus.to_string(),
            "-drive".into(),
            format!("file={},if=virtio", escape_drive_path(&config.disk)?),
            "-qmp".into(),
            format!("unix:{},server=on,wait=off", sock.display()),
            "-display".into(),
            "none".into(),
        ];
        for (nic_id, network) in config.nics() {
            let netdev = netdev_id(nic_id);
            // The primary NIC has no device id (QEMU invents one); an extra
            // NIC is addressed by QMP by the user's id.
            let id = if nic_id == "net0" {
                String::new()
            } else {
                format!("id={nic_id},")
            };
            let (netdev_arg, device) = match network {
                Network::User { hostfwd } => {
                    // hostfwd is the primary NIC's firewall policy; an extra
                    // NIC has none, or it would open ports the policy never saw.
                    let hostfwd = if nic_id == "net0" {
                        hostfwd.clone()
                    } else {
                        Vec::new()
                    };
                    let user = crate::qemu_network::UserNetwork { hostfwd };
                    (
                        user.qemu_netdev_for(&netdev)?,
                        format!("virtio-net-pci,{id}netdev={netdev}"),
                    )
                }
                Network::Bridge { mac, .. } => {
                    if mac.is_empty() {
                        return Err(format!("NIC '{nic_id}' has no MAC address"));
                    }
                    let tap = tap_name(&config.id, nic_id);
                    (
                        format!("tap,id={netdev},ifname={tap},script=no,downscript=no"),
                        format!("virtio-net-pci,{id}netdev={netdev},mac={mac}"),
                    )
                }
            };
            args.extend(["-netdev".into(), netdev_arg, "-device".into(), device]);
        }
        args.extend(disk_args(config)?);
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
            stopped = Some(rec.config.clone());
        }
        if let Some(config) = stopped {
            self.save(&all)?;
            // Inside the lock: a start cannot create the same tap until this
            // cleanup is done.
            delete_nic_taps(&config);
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
            stopped = Some(rec.config.clone());
        }
        if let Some(config) = stopped {
            self.save(&all)?;
            delete_nic_taps(&config);
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
        delete_nic_taps(&r.config);
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

/// A deterministic MAC for one VM interface: QEMU's `52:54:00` prefix (unicast
/// and locally administered) plus 24 bits of a hash of the VM and interface.
/// Stable across restarts and identical for every caller, so the address
/// reservation, the DHCP host entry and QEMU itself all name the same NIC.
pub fn stable_mac(id: &str, nic_id: &str) -> String {
    stable_mac_salted(id, nic_id, 0)
}

/// The base with a salt mixed in, used when the deterministic MAC is already
/// taken. Only 24 bits are available, so a collision is re-derived rather than
/// handed to dnsmasq as a second guest's address.
fn stable_mac_salted(id: &str, nic_id: &str, salt: u32) -> String {
    let mut h = crate::hostnet::fnv1a(&format!("{id}/{nic_id}"));
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

/// The host tap for one VM interface: `tap` + 10 hex, 13 characters. Derived,
/// never stored, so a restart finds the same interface again.
pub fn tap_name(id: &str, nic_id: &str) -> String {
    let h = crate::hostnet::fnv1a(&format!("tap/{id}/{nic_id}"));
    format!("tap{:010x}", h & 0xff_ffff_ffff)
}

fn netdev_id(nic_id: &str) -> String {
    if nic_id == "net0" {
        "net0".into()
    } else {
        format!("net-{nic_id}")
    }
}

/// A new extra NIC's identifier: a token QEMU and `ip` both accept, and never
/// the primary NIC's reserved `net0` slot.
fn validate_new_nic_id(id: &str) -> Result<(), String> {
    if id == "net0" {
        return Err("'net0' is the primary NIC's identifier and cannot be reused".into());
    }
    // QEMU device ids are one namespace, and `disk0` is the primary disk's
    // reserved slot: a NIC may use neither.
    if id == "disk0" {
        return Err("'disk0' is the primary disk's identifier and cannot be reused".into());
    }
    if id.is_empty()
        || id.len() > 20
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(
            "a NIC identifier is up to 20 characters of lowercase letters, digits and '-'".into(),
        );
    }
    Ok(())
}

/// A new extra disk's identifier: a token QEMU and QMP both accept, and never
/// the primary disk's reserved `disk0` slot (it has no QMP device id).
pub fn validate_new_disk_id(id: &str) -> Result<(), String> {
    if id == "disk0" {
        return Err("'disk0' is the primary disk's identifier and cannot be reused".into());
    }
    // QEMU device ids are one namespace, and `net0` is the primary NIC's
    // reserved slot: a disk may use neither.
    if id == "net0" {
        return Err("'net0' is the primary NIC's identifier and cannot be reused".into());
    }
    if id.is_empty()
        || id.len() > 20
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(
            "a disk identifier is up to 20 characters of lowercase letters, digits and '-'".into(),
        );
    }
    Ok(())
}

/// QEMU device ids are one namespace across device classes: a disk and a NIC
/// cannot share one, or the second `device_add` and the launch argv collide.
fn device_id_taken(config: &VmConfig, device_id: &str) -> bool {
    config.extra_nics.iter().any(|n| n.id == device_id)
        || config.extra_disks.iter().any(|d| d.id == device_id)
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

/// A bridge NIC's MAC, generated or validated. A user-supplied MAC in canonical
/// form is used as given (it is how a guest keeps its address across metadata
/// rewrites), but it must be unicast, not all-zero, and free in this manager.
fn mac_for(
    all: &BTreeMap<String, Record>,
    id: &str,
    nic_id: &str,
    mac: &str,
) -> Result<String, String> {
    let used = used_macs(all, id, nic_id);
    if mac.trim().is_empty() {
        // The deterministic base first; only if another NIC already holds it
        // does a salted re-derivation run (24 bits is small but not zero).
        let base = stable_mac(id, nic_id);
        if !used.contains(&base) {
            return Ok(base);
        }
        for salt in 1..=1024 {
            let candidate = stable_mac_salted(id, nic_id, salt);
            if !used.contains(&candidate) {
                return Ok(candidate);
            }
        }
        return Err("cannot derive a free MAC address".into());
    }
    let m = crate::ipam::store::normalize_mac(mac)
        .ok_or_else(|| format!("'{mac}' is not a MAC address"))?;
    let first = u8::from_str_radix(&m[..2], 16).map_err(|e| e.to_string())?;
    if first & 1 == 1 {
        return Err(format!("'{mac}' is a multicast address"));
    }
    if m == "00:00:00:00:00:00" {
        return Err("'00:00:00:00:00:00' is the all-zero address".into());
    }
    if used.contains(&m) {
        return Err(format!("MAC address {m} is already used by another NIC"));
    }
    Ok(m)
}

/// The MACs this manager's other NICs already have. The slot being assigned is
/// excluded, so re-setting a stored MAC is not a collision with itself. One MAC
/// belongs to one NIC: dnsmasq's reservation and the firewall's rules are keyed
/// by it.
fn used_macs(
    all: &BTreeMap<String, Record>,
    id: &str,
    nic_id: &str,
) -> std::collections::BTreeSet<String> {
    let mut used = std::collections::BTreeSet::new();
    for (vm, rec) in all {
        for (nic, network) in rec.config.nics() {
            if vm == id && nic == nic_id {
                continue;
            }
            if let Network::Bridge { mac, .. } = network
                && !mac.is_empty()
            {
                used.insert(mac.clone());
            }
        }
    }
    used
}

/// The stored network of one NIC, primary (`net0`) or extra.
fn nic_network_mut<'a>(config: &'a mut VmConfig, nic_id: &str) -> Option<&'a mut Network> {
    if nic_id == "net0" {
        return Some(&mut config.network);
    }
    config
        .extra_nics
        .iter_mut()
        .find(|n| n.id == nic_id)
        .map(|n| &mut n.network)
}

fn interface_exists(name: &str) -> bool {
    crate::hostnet::valid_name(name) && Path::new(SYS_NET).join(name).exists()
}

/// The interface is a tun/tap. Anything else (a bridge, a physical NIC, a
/// veth) is never deleted or enslaved by these helpers.
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

fn delete_nic_tap(id: &str, nic_id: &str) {
    cleanup_taps(&[tap_name(id, nic_id)]);
}

/// Remove every tap a config's bridge NICs own. User-mode NICs have none.
fn delete_nic_taps(config: &VmConfig) {
    let taps: Vec<String> = config
        .nics()
        .filter(|(_, n)| matches!(n, Network::Bridge { .. }))
        .map(|(nic, _)| tap_name(&config.id, nic))
        .collect();
    cleanup_taps(&taps);
}
/// The launch argv for every extra disk. Each is an explicit file → format node
/// pair plus a device, the same shape `add_disk` builds through QMP, so a live
/// guest and the next launch name the same block nodes.
fn disk_args(config: &VmConfig) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for disk in &config.extra_disks {
        if !seen.insert(disk.id.as_str()) {
            return Err(format!("duplicate disk identifier '{}'", disk.id));
        }
        let file = format!("file-{}", disk.id);
        let drive = format!("drive-{}", disk.id);
        args.extend([
            "-blockdev".into(),
            format!(
                "driver=file,filename={},node-name={file}",
                escape_blockdev_path(&disk.path)?
            ),
            "-blockdev".into(),
            format!(
                "driver={},file={file},node-name={drive}",
                disk.format.as_str()
            ),
            "-device".into(),
            format!("virtio-blk-pci,id={},drive={drive}", disk.id),
        ]);
    }
    Ok(args)
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
fn validate_device_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 31
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err("invalid QEMU device identifier".into());
    }
    Ok(())
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

    #[test]
    fn qemu_ids_must_be_canonical_uuids() {
        assert!(Manager::valid_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!Manager::valid_id("legacy-vm"));
        assert!(!Manager::valid_id("550E8400-E29B-41D4-A716-446655440000"));
        assert!(!Manager::valid_id("550e8400e29b41d4a716446655440000"));
    }

    #[test]
    fn metadata_rejects_key_id_disagreement() {
        let root = std::env::temp_dir().join(format!("virt-man-qemu-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let manager = Manager {
            data_dir: root.clone(),
            state_root: root.join("state"),
            qemu: PathBuf::from("qemu"),
            lock: Default::default(),
        };
        let record = Record {
            config: VmConfig {
                id: "another-id".into(),
                title: String::new(),
                disk: PathBuf::from("/disk"),
                memory_mib: 1,
                vcpus: 1,
                network: Default::default(),
                firewall: Default::default(),
                extra_nics: vec![],
                extra_disks: vec![],
            },
            disk_size: 0,
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

    fn manager(tag: &str) -> (Manager, PathBuf) {
        let root = std::env::temp_dir().join(format!("virt-man-qemu-{tag}-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let manager = Manager {
            data_dir: root.clone(),
            state_root: root.join("state"),
            qemu: PathBuf::from("qemu"),
            lock: Default::default(),
        };
        (manager, root)
    }

    fn config(id: &str) -> VmConfig {
        VmConfig {
            id: id.into(),
            title: "web".into(),
            disk: PathBuf::from("/disk"),
            memory_mib: 512,
            vcpus: 2,
            network: Network::User { hostfwd: vec![] },
            firewall: Default::default(),
            extra_nics: vec![],
            extra_disks: vec![],
        }
    }

    /// The values given to one argv flag, in order.
    fn values(args: &[String], flag: &str) -> Vec<String> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].clone())
            .collect()
    }

    const ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn a_mac_and_a_tap_are_stable_and_distinct_per_interface() {
        let mac = stable_mac(ID, "net0");
        assert_eq!(mac, stable_mac(ID, "net0"), "same NIC, same MAC");
        assert_eq!(mac.len(), 17);
        assert!(mac.starts_with("52:54:00:"), "{mac}");
        assert_eq!(mac, mac.to_ascii_lowercase());
        assert_eq!(
            u8::from_str_radix(&mac[..2], 16).unwrap() & 1,
            0,
            "a NIC's MAC is unicast"
        );
        assert_ne!(mac, stable_mac(ID, "nic1"));
        assert_ne!(
            mac,
            stable_mac("550e8400-e29b-41d4-a716-446655440001", "net0")
        );

        let tap = tap_name(ID, "net0");
        assert_eq!(tap, tap_name(ID, "net0"), "same NIC, same tap");
        assert!(tap.starts_with("tap"), "{tap}");
        assert!(tap.len() <= 15, "the Linux interface-name limit: {tap}");
        assert!(tap[3..].chars().all(|c| c.is_ascii_hexdigit()), "{tap}");
        assert_ne!(tap, tap_name(ID, "nic1"));
    }

    #[test]
    fn a_supplied_mac_is_normalized_and_must_be_unicast() {
        let none = BTreeMap::new();
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
        let (a, b) = (ID, "550e8400-e29b-41d4-a716-446655440001");
        let base = stable_mac(b, "net0");
        let mut taken = config(a);
        taken.network = Network::Bridge {
            network: "lab".into(),
            bridge: "vbr0".into(),
            mac: base.clone(),
        };
        let all = BTreeMap::from([(a.to_string(), record(taken))]);

        let generated = mac_for(&all, b, "net0", "").unwrap();
        assert_ne!(generated, base, "the taken MAC is skipped");
        assert_eq!(
            generated,
            stable_mac_salted(b, "net0", 1),
            "the first free salt wins, deterministically"
        );
        assert!(
            mac_for(&all, b, "net0", &base).is_err(),
            "a supplied duplicate is refused"
        );
        // The slot being re-set is not a collision with itself.
        assert_eq!(mac_for(&all, a, "net0", &base).unwrap(), base);
    }

    fn record(config: VmConfig) -> Record {
        Record {
            config,
            disk_size: 0,
            pid: None,
            start_ticks: None,
        }
    }

    /// `net0` is the primary NIC's slot and an identifier is a lowercase token.
    /// A NIC's bridge is stored even before its network exists, so a guest can
    /// be configured before the network is started; `start` verifies it.
    #[test]
    fn an_extra_nic_is_named_safely_and_may_precede_its_network() {
        let (manager, root) = manager("nicid");
        let disk = root.join("disk");
        fs::write(&disk, b"x").unwrap();
        let mut cfg = config(ID);
        cfg.disk = disk;
        manager.define(cfg).unwrap();

        for bad in ["net0", "nic 1", "Nic1", "net_1", "nic.1", ""] {
            let e = manager
                .add_nic(ID, bad, Network::User { hostfwd: vec![] })
                .unwrap_err();
            assert!(!e.is_empty(), "{bad}");
        }
        assert!(
            manager
                .add_nic(ID, "net0", Network::User { hostfwd: vec![] })
                .unwrap_err()
                .contains("primary"),
            "the primary slot is reserved"
        );

        manager
            .add_nic(ID, "nic1", Network::User { hostfwd: vec![] })
            .unwrap();
        manager
            .add_nic(
                ID,
                "nic2",
                Network::Bridge {
                    network: "lab".into(),
                    bridge: "vbrnosuch".into(),
                    mac: String::new(),
                },
            )
            .unwrap();
        let stored = manager.list().unwrap().remove(0);
        assert_eq!(
            stored.extra_nics[1].network,
            Network::Bridge {
                network: "lab".into(),
                bridge: "vbrnosuch".into(),
                mac: stable_mac(ID, "nic2"),
            },
            "a stopped network's bridge is accepted and a MAC is generated"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// Every NIC goes through one argv builder: user-mode NAT stays exactly as
    /// it was, and a bridge NIC names the deterministic tap and MAC, with no
    /// scripting hook (the daemon owns the tap, not QEMU).
    #[test]
    fn the_argv_builds_one_netdev_and_device_per_nic() {
        let (manager, root) = manager("argv");

        let args = manager.args(&config(ID)).unwrap();
        assert_eq!(values(&args, "-netdev"), ["user,id=net0"]);
        assert_eq!(values(&args, "-device"), ["virtio-net-pci,netdev=net0"]);

        let mut cfg = config(ID);
        cfg.network = Network::Bridge {
            network: "lab".into(),
            bridge: "vbr5b92a1a54f00".into(),
            mac: "52:54:00:aa:bb:01".into(),
        };
        cfg.extra_nics = vec![
            Nic {
                id: "nic1".into(),
                network: Network::User {
                    hostfwd: vec![HostForward {
                        protocol: crate::qemu_network::Protocol::Tcp,
                        host_port: 2222,
                        guest_ip: "192.0.2.2".parse().unwrap(),
                        guest_port: 22,
                    }],
                },
            },
            Nic {
                id: "nic2".into(),
                network: Network::Bridge {
                    network: "lab".into(),
                    bridge: "vbr5b92a1a54f00".into(),
                    mac: "52:54:00:aa:bb:02".into(),
                },
            },
        ];
        let args = manager.args(&cfg).unwrap();
        assert_eq!(
            values(&args, "-netdev"),
            [
                format!(
                    "tap,id=net0,ifname={},script=no,downscript=no",
                    tap_name(ID, "net0")
                ),
                // hostfwd is the primary's policy: an extra user NIC never
                // opens a port the firewall allowlist did not see.
                "user,id=net-nic1".to_string(),
                format!(
                    "tap,id=net-nic2,ifname={},script=no,downscript=no",
                    tap_name(ID, "nic2")
                ),
            ]
        );
        assert_eq!(
            values(&args, "-device"),
            [
                "virtio-net-pci,netdev=net0,mac=52:54:00:aa:bb:01".to_string(),
                "virtio-net-pci,id=nic1,netdev=net-nic1".to_string(),
                "virtio-net-pci,id=nic2,netdev=net-nic2,mac=52:54:00:aa:bb:02".to_string(),
            ]
        );
        // Nothing is handed to a shell: each argument is one argv element.
        assert!(args.iter().all(|a| !a.contains(" -")));
        fs::remove_dir_all(root).unwrap();
    }

    /// A disk identifier is a lowercase token and never the primary's reserved
    /// `disk0`; its path must be a real, non-empty file, and neither the id nor
    /// the file may be attached twice. Removal never deletes the file.
    #[test]
    fn an_extra_disk_is_validated_persisted_and_never_deletes_its_file() {
        let (manager, root) = manager("diskid");
        let disk = root.join("disk");
        fs::write(&disk, b"x").unwrap();
        let mut cfg = config(ID);
        cfg.disk = disk;
        manager.define(cfg).unwrap();
        let extra = root.join("extra.qcow2");
        fs::write(&extra, b"qcow2").unwrap();

        for bad in ["disk0", "d 1", "D1", "d_1", "d.1", ""] {
            assert!(
                manager
                    .add_disk(ID, bad, &extra, DiskFormat::Qcow2)
                    .is_err(),
                "{bad}"
            );
        }
        assert!(
            manager
                .add_disk(ID, "disk0", &extra, DiskFormat::Qcow2)
                .unwrap_err()
                .contains("primary"),
            "the primary slot is reserved"
        );

        assert!(
            manager
                .add_disk(ID, "data", Path::new("extra.qcow2"), DiskFormat::Raw)
                .is_err(),
            "a relative path is refused"
        );
        assert!(
            manager
                .add_disk(ID, "data", &root.join("missing"), DiskFormat::Raw)
                .is_err()
        );
        assert!(
            manager
                .add_disk(ID, "data", &root, DiskFormat::Raw)
                .is_err(),
            "a directory is not a disk"
        );
        let empty = root.join("empty");
        fs::write(&empty, b"").unwrap();
        assert!(
            manager
                .add_disk(ID, "data", &empty, DiskFormat::Raw)
                .is_err(),
            "an empty file is not a disk"
        );
        let link = root.join("link");
        std::os::unix::fs::symlink(&extra, &link).unwrap();
        assert!(
            manager
                .add_disk(ID, "data", &link, DiskFormat::Raw)
                .is_err(),
            "a symlink is refused, never followed"
        );

        let path = manager
            .add_disk(ID, "data", &extra, DiskFormat::Qcow2)
            .unwrap();
        assert_eq!(path, fs::canonicalize(&extra).unwrap());
        let stored = manager.list().unwrap().remove(0);
        assert_eq!(
            stored.extra_disks,
            vec![Disk {
                id: "data".into(),
                path: fs::canonicalize(&extra).unwrap(),
                format: DiskFormat::Qcow2,
            }]
        );
        // The same id, and the same file under another id, are both refused.
        assert_eq!(
            manager
                .add_disk(ID, "data", &extra, DiskFormat::Raw)
                .unwrap_err(),
            "device identifier already exists"
        );
        assert!(
            manager
                .add_disk(ID, "data2", &extra, DiskFormat::Raw)
                .is_err()
        );
        let other = "550e8400-e29b-41d4-a716-446655440001";
        let other_disk = root.join("disk2");
        fs::write(&other_disk, b"x").unwrap();
        let mut cfg = config(other);
        cfg.disk = other_disk;
        manager.define(cfg).unwrap();
        assert!(
            manager
                .add_disk(other, "data", &extra, DiskFormat::Raw)
                .is_err(),
            "one file never backs two guests"
        );

        // A stopped guest's removal is durable and leaves the bytes alone. A
        // live removal takes the same path after `device_del` (which may leave
        // the device attached until the guest acks), so the definition no
        // longer lists it and a repeat DELETE is an honest unknown-disk.
        assert!(!manager.remove_disk(ID, "data").unwrap());
        assert!(manager.list().unwrap()[0].extra_disks.is_empty());
        assert!(extra.exists(), "removing a disk never deletes its file");
        assert_eq!(
            manager.remove_disk(ID, "data").unwrap_err(),
            "unknown QEMU disk"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// A disk identifier and a NIC identifier share one QEMU namespace, so a
    /// hot-add may not name a device of the other kind — nor either primary's
    /// reserved slot. The check runs before any QMP call.
    #[test]
    fn a_nic_and_a_disk_never_share_a_device_id() {
        let (manager, root) = manager("idns");
        let primary = root.join("disk");
        fs::write(&primary, b"x").unwrap();
        let extra = root.join("extra");
        fs::write(&extra, b"y").unwrap();
        let mut cfg = config(ID);
        cfg.disk = primary;
        manager.define(cfg).unwrap();

        assert!(
            manager
                .add_nic(ID, "disk0", Network::User { hostfwd: vec![] })
                .is_err(),
            "the primary disk's slot is reserved"
        );
        assert!(
            manager
                .add_disk(ID, "net0", &extra, DiskFormat::Raw)
                .is_err(),
            "the primary NIC's slot is reserved"
        );

        manager
            .add_nic(ID, "x", Network::User { hostfwd: vec![] })
            .unwrap();
        assert_eq!(
            manager
                .add_disk(ID, "x", &extra, DiskFormat::Raw)
                .unwrap_err(),
            "device identifier already exists"
        );
        manager.add_disk(ID, "y", &extra, DiskFormat::Raw).unwrap();
        assert_eq!(
            manager
                .add_nic(ID, "y", Network::User { hostfwd: vec![] })
                .unwrap_err(),
            "device identifier already exists"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// QEMU's argv is UTF-8, so a path that cannot be spelled is refused at
    /// definition/add time instead of yielding a VM that cannot start.
    #[test]
    fn a_non_utf8_disk_path_is_refused_before_it_is_stored() {
        use std::os::unix::ffi::OsStringExt;
        let (manager, root) = manager("nonutf8");
        let bad = root.join(std::ffi::OsString::from_vec(b"bad\xff.img".to_vec()));
        fs::write(&bad, b"x").unwrap();
        let mut cfg = config(ID);
        cfg.disk = bad.clone();
        assert!(
            manager.define(cfg).is_err(),
            "a primary disk QEMU cannot spell is refused"
        );

        let good = root.join("disk");
        fs::write(&good, b"x").unwrap();
        let mut cfg = config(ID);
        cfg.disk = good;
        manager.define(cfg).unwrap();
        assert!(manager.add_disk(ID, "data", &bad, DiskFormat::Raw).is_err());
        assert!(manager.list().unwrap()[0].extra_disks.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    /// Extras are explicit block nodes so QMP can add and drop them by name,
    /// while the primary keeps the `-drive` shape older guests were defined
    /// with. Every value is one argv element, never a shell string.
    #[test]
    fn the_argv_builds_one_block_node_pair_per_extra_disk() {
        let (manager, root) = manager("diskargs");
        let mut cfg = config(ID);
        cfg.disk = PathBuf::from("/img/primary.raw");
        cfg.extra_disks = vec![
            Disk {
                id: "data".into(),
                path: PathBuf::from("/img/data.qcow2"),
                format: DiskFormat::Qcow2,
            },
            Disk {
                id: "scratch".into(),
                path: PathBuf::from("/img/a,b.raw"),
                format: DiskFormat::Raw,
            },
        ];
        let args = manager.args(&cfg).unwrap();
        assert_eq!(
            values(&args, "-drive"),
            ["file=/img/primary.raw,if=virtio"],
            "the primary disk is unchanged"
        );
        assert_eq!(
            values(&args, "-blockdev"),
            [
                "driver=file,filename=/img/data.qcow2,node-name=file-data",
                "driver=qcow2,file=file-data,node-name=drive-data",
                // A literal comma is doubled for QEMU's option parser.
                "driver=file,filename=/img/a,,b.raw,node-name=file-scratch",
                "driver=raw,file=file-scratch,node-name=drive-scratch",
            ]
        );
        let devices: Vec<String> = values(&args, "-device")
            .into_iter()
            .filter(|d| d.starts_with("virtio-blk-pci"))
            .collect();
        assert_eq!(
            devices,
            [
                "virtio-blk-pci,id=data,drive=drive-data",
                "virtio-blk-pci,id=scratch,drive=drive-scratch",
            ]
        );
        assert!(args.iter().all(|a| !a.contains(" -")));

        // A duplicated id would give two devices one node namespace.
        cfg.extra_disks.push(Disk {
            id: "data".into(),
            path: PathBuf::from("/img/other.raw"),
            format: DiskFormat::Raw,
        });
        assert!(manager.args(&cfg).is_err());
        fs::remove_dir_all(root).unwrap();
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

    /// Metadata written before bridge mode existed must still load, byte for
    /// byte: user-mode networking was a bare block and an extra NIC was a bare
    /// device id. New files keep the tagged/object shapes.
    #[test]
    fn old_metadata_shapes_load_and_new_ones_round_trip() {
        let old: VmConfig = serde_json::from_value(serde_json::json!({
            "id": ID,
            "disk": "/disk",
            "memory_mib": 512,
            "vcpus": 1,
            "network": {"hostfwd": [{
                "protocol": "tcp", "host_port": 2222,
                "guest_ip": "192.0.2.2", "guest_port": 22
            }]},
            "extra_nics": ["nic1", "nic2"]
        }))
        .unwrap();
        assert_eq!(old.title, "");
        assert_eq!(
            old.network,
            Network::User {
                hostfwd: vec![HostForward {
                    protocol: crate::qemu_network::Protocol::Tcp,
                    host_port: 2222,
                    guest_ip: "192.0.2.2".parse().unwrap(),
                    guest_port: 22,
                }]
            }
        );
        assert_eq!(
            old.extra_nics,
            vec![
                Nic {
                    id: "nic1".into(),
                    network: Network::User { hostfwd: vec![] },
                },
                Nic {
                    id: "nic2".into(),
                    network: Network::User { hostfwd: vec![] },
                },
            ]
        );
        // A missing network defaults to user-mode NAT, not a failed VM list.
        let bare: VmConfig = serde_json::from_value(serde_json::json!({
            "id": ID, "disk": "/disk", "memory_mib": 512, "vcpus": 1
        }))
        .unwrap();
        assert_eq!(bare.network, Network::User { hostfwd: vec![] });
        assert!(bare.extra_nics.is_empty());
        assert!(bare.extra_disks.is_empty(), "no extras means none");
        assert!(old.extra_disks.is_empty(), "metadata predates extra disks");

        // A stored disk without a format is raw, not a failed VM list.
        let legacy: VmConfig = serde_json::from_value(serde_json::json!({
            "id": ID, "disk": "/disk", "memory_mib": 512, "vcpus": 1,
            "extra_disks": [{"id": "data", "path": "/img/data"}]
        }))
        .unwrap();
        assert_eq!(
            legacy.extra_disks,
            vec![Disk {
                id: "data".into(),
                path: PathBuf::from("/img/data"),
                format: DiskFormat::Raw,
            }]
        );
        assert_eq!(
            serde_json::to_value(DiskFormat::Qcow2).unwrap(),
            serde_json::json!("qcow2")
        );

        // A malformed tagged value must not silently become user-mode NAT.
        assert!(
            serde_json::from_value::<Network>(serde_json::json!({"mode":"bridge","network":"lab"}))
                .is_err(),
            "a bridge block missing its bridge/mac is refused"
        );

        let mut cfg = config(ID);
        cfg.network = Network::Bridge {
            network: "lab".into(),
            bridge: "vbr5b92a1a54f00".into(),
            mac: "52:54:00:aa:bb:01".into(),
        };
        cfg.extra_nics = vec![Nic {
            id: "nic1".into(),
            network: Network::User {
                hostfwd: vec![HostForward {
                    protocol: crate::qemu_network::Protocol::Tcp,
                    host_port: 2222,
                    guest_ip: "192.0.2.2".parse().unwrap(),
                    guest_port: 22,
                }],
            },
        }];
        cfg.extra_disks = vec![Disk {
            id: "data".into(),
            path: PathBuf::from("/img/data.qcow2"),
            format: DiskFormat::Qcow2,
        }];
        let back: VmConfig = serde_json::from_value(serde_json::to_value(&cfg).unwrap()).unwrap();
        assert_eq!(back.network, cfg.network);
        assert_eq!(back.extra_nics, cfg.extra_nics);
        assert_eq!(back.extra_disks, cfg.extra_disks);
        assert_eq!(
            serde_json::to_value(&cfg.network).unwrap(),
            serde_json::json!({
                "mode":"bridge","network":"lab","bridge":"vbr5b92a1a54f00",
                "mac":"52:54:00:aa:bb:01"
            })
        );
    }
}
