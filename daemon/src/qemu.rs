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
    time::Duration,
};
use uuid::Uuid;

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
    pub disk: PathBuf,
    pub memory_mib: u32,
    pub vcpus: u16,
    #[serde(default)]
    pub network: crate::qemu_network::UserNetwork,
    /// Explicit inbound host-forward allowlist; empty means deny all inbound.
    #[serde(default)]
    pub firewall: crate::qemu_network::InboundPolicy,
    #[serde(default)]
    pub extra_nics: Vec<String>,
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
            if !self.identity(rec) {
                rec.pid = None;
                rec.start_ticks = None;
                changed = true;
            }
        }
        if changed {
            self.save(&all)?;
        }
        Ok(())
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
    pub fn set_network(
        &self,
        id: &str,
        network: crate::qemu_network::UserNetwork,
    ) -> Result<(), String> {
        network.validate()?;
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get_mut(id).ok_or("unknown QEMU VM")?;
        if self.identity(rec) {
            return Err("VM must be stopped to change networking".into());
        }
        // hostfwd is policy, never an independent networking knob.
        rec.config.network.hostfwd = rec.config.firewall.hostfwd.clone();
        rec.config.network = network;
        rec.config.network.hostfwd = rec.config.firewall.hostfwd.clone();
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
        rec.config.network.hostfwd = firewall.hostfwd;
        self.save(&all)
    }
    pub fn add_nic(&self, id: &str, device_id: &str) -> Result<(), String> {
        validate_device_id(device_id)?;
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut all = self.read()?;
        let rec = all.get_mut(id).ok_or("unknown QEMU VM")?;
        if rec.config.extra_nics.iter().any(|x| x == device_id) {
            return Err("device identifier already exists".into());
        }
        let net_id = format!("net-{device_id}");
        if self.identity(rec) {
            let sock = self.vm_dir(id).join("qmp.sock");
            qmp_execute(
                &sock,
                "netdev_add",
                serde_json::json!({"type":"user", "id":net_id}),
            )?;
            if let Err(e) = qmp_execute(
                &sock,
                "device_add",
                serde_json::json!({"driver":"virtio-net-pci", "id":device_id, "netdev":net_id}),
            ) {
                let _ = qmp_execute(&sock, "netdev_del", serde_json::json!({"id":net_id}));
                return Err(e);
            }
        }
        rec.config.extra_nics.push(device_id.to_owned());
        let live = self.identity(rec);
        if let Err(e) = self.save(&all) {
            if live {
                let sock = self.vm_dir(id).join("qmp.sock");
                let _ = qmp_execute(&sock, "device_del", serde_json::json!({"id":device_id}));
                let _ = qmp_execute(&sock, "netdev_del", serde_json::json!({"id":net_id}));
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
        if !rec.config.extra_nics.iter().any(|x| x == device_id) {
            return Err("unknown QEMU NIC".into());
        }
        if self.identity(rec) {
            let sock = self.vm_dir(id).join("qmp.sock");
            qmp_execute(&sock, "device_del", serde_json::json!({"id":device_id}))?;
            // QMP device_del completes asynchronously; keep durable metadata until an
            // authoritative DEVICE_DELETED event can be observed. Never infer from
            // query-pci's human-oriented topology dump.
            return Ok(true);
        }
        rec.config.extra_nics.retain(|x| x != device_id);
        self.save(&all)?;
        Ok(false)
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
        let mut network = config.network.clone();
        network.hostfwd = config.firewall.hostfwd.clone();
        let netdev = network.qemu_netdev()?;
        let mut child = Command::new(&self.qemu)
            .args([
                "-name",
                id,
                "-m",
                &config.memory_mib.to_string(),
                "-smp",
                &config.vcpus.to_string(),
            ])
            .arg("-drive")
            .arg(format!("file={},if=virtio", escape_drive_path(&disk)?))
            .arg("-qmp")
            .arg(format!("unix:{},server=on,wait=off", sock.display()))
            .arg("-display")
            .arg("none")
            .arg("-netdev")
            .arg(netdev)
            .args(["-device", "virtio-net-pci,netdev=net0"])
            .args(config.extra_nics.iter().flat_map(|nic| {
                [
                    "-netdev".to_owned(),
                    format!("user,id=net-{nic}"),
                    "-device".to_owned(),
                    format!("virtio-net-pci,id={nic},netdev=net-{nic}"),
                ]
            }))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("launch QEMU: {e}"))?;
        let pid = child.id();
        if fs::canonicalize(format!("/proc/{pid}/exe")).ok().as_deref()
            != fs::canonicalize(&self.qemu).ok().as_deref()
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err("QEMU executable identity mismatch".into());
        }
        let ticks = match process_ticks(pid) {
            Some(ticks) => ticks,
            None => {
                let _ = child.kill();
                let _ = child.wait();
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
            return Err(e);
        }
        // Do not publish a stale PID if QEMU died while metadata was being saved.
        if !self.identity(all.get(id).unwrap()) || qmp(&sock, "query-status").is_err() {
            let _ = child.kill();
            let _ = child.wait();
            let rec = all.get_mut(id).unwrap();
            rec.pid = None;
            rec.start_ticks = None;
            let _ = self.save(&all);
            return Err("QEMU failed readiness check".into());
        }
        drop(child);
        Ok(())
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
            drop(guard);
            let until = std::time::Instant::now() + Duration::from_secs(30);
            while process_ticks(pid) == Some(ticks) && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(100));
            }
            if process_ticks(pid) == Some(ticks) {
                return Err("QEMU did not exit after graceful shutdown request".into());
            }
            let _guard = self.lock.lock().map_err(|e| e.to_string())?;
            let mut all = self.read()?;
            if let Some(rec) = all
                .get_mut(id)
                .filter(|r| r.pid == Some(pid) && r.start_ticks == Some(ticks))
            {
                rec.pid = None;
                rec.start_ticks = None;
                self.save(&all)?;
            }
        }
        Ok(())
    }
    pub fn force_stop(&self, id: &str) -> Result<(), String> {
        if !Self::valid_id(id) {
            return Err("invalid VM id".into());
        }
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let all = self.read()?;
        let rec = all.get(id).ok_or("unknown QEMU VM")?;
        if self.identity(rec) {
            signal_verified(rec, libc::SIGKILL)?;
        }
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
                disk: PathBuf::from("/disk"),
                memory_mib: 1,
                vcpus: 1,
                network: Default::default(),
                firewall: Default::default(),
                extra_nics: vec![],
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
}
