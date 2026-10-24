//! LXC containers, run by LXC's own tools rather than by libvirt.
//!
//! The daemon keeps each container's definition in `<data_dir>/containers.json`
//! (mode 0600, atomic temp+rename): the libvirt-shaped XML the SPA edits
//! (`ctxml`) and whether it autostarts. That store is the only source of
//! truth. An LXC config is *rendered* from it into `LXC_PATH/<id>/config` at
//! every start, the way dnsmasq's config is rendered for a network, so there is
//! no second copy of the hardware to drift: an edit here takes effect at the
//! next start, and nothing ever reads the generated file back.
//!
//! The tools are the stock `lxc-start`, `lxc-stop` and `lxc-console`, the
//! same ones Proxmox's `pct` drives:
//!
//! - `lxc-start -d` daemonizes LXC's monitor, which holds the container, its
//!   console and its veths. It is not a child of this daemon's service in any
//!   way that matters: the unit's `KillMode=process` and LXC's own
//!   `lxc.monitor.<id>`/`lxc.payload.<id>` cgroups mean a daemon restart leaves
//!   every container running, as it does a QEMU guest.
//! - Running is decided by the init pid `lxc-start -p` records *and* that pid
//!   still being in the container's `lxc.payload.<id>` cgroup, so a recycled
//!   pid is never mistaken for the container -- and the 1 s poll costs a file
//!   read instead of an `lxc-info` process per container.
//! - A graceful stop is a signal to the container's init, chosen by which
//!   init it runs: measured, busybox ignores LXC's default SIGPWR and only
//!   powers off on SIGUSR2 (an Alpine container otherwise sits out the whole
//!   timeout), while systemd halts on SIGRTMIN+3. A forced stop is
//!   `lxc-stop -k`.
//! - Resource limits are cgroup v2 files, so memory, swap, the CPU cap and the
//!   CPU set all change on a running container at once (`apply_limits`).
//!
//! LXC is never given a config from `/etc/lxc` or the distro's `lxc-net`
//! bridge: every network is one of this daemon's bridges, and the installer
//! disables `lxc-net` and `lxc.service` so neither starts a second dnsmasq or
//! autostarts anything from `/var/lib/lxc`.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::ctxml::{self, CPU_PERIOD, Container};
use crate::qemu::{InterfaceStats, Stats};

/// Where the rendered LXC configs, logs and init pid files live. Under `/run`:
/// they are derived at every start and mean nothing after a reboot. Mode 0711
/// on the way down, like the rootfs mounts, so nothing above a container's
/// directory stops LXC working for a mapped root.
pub const LXC_PATH: &str = "/run/virt-man-daemon/lxc";
const SYS_NET: &str = "/sys/class/net";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    xml: String,
    #[serde(default)]
    autostart: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    containers: BTreeMap<String, Record>,
}

/// One stored container, parsed.
#[derive(Clone, Debug)]
pub struct Entry {
    pub def: Container,
    pub autostart: bool,
}

impl Entry {
    pub fn id(&self) -> &str {
        &self.def.id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Stopped,
}

impl State {
    /// The wire spelling a VM's state uses too.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

struct Inner {
    store: PathBuf,
    lxc_path: PathBuf,
    /// Serialises every read-modify-write of the store.
    lock: Mutex<()>,
}

static GLOBAL: OnceLock<Manager> = OnceLock::new();

/// Make this the manager the rest of the daemon consults. The IPAM snapshot,
/// the in-use checks, the firewall and `/metrics` all need to see containers
/// beside VMs; they reach them here rather than each growing a parameter that
/// only `main` could fill in. Tests that never install one see no containers.
pub fn install(m: Manager) {
    let _ = GLOBAL.set(m);
}

pub fn global() -> Option<&'static Manager> {
    GLOBAL.get()
}

impl Manager {
    pub fn new(data_dir: &Path) -> Self {
        Self::with_paths(data_dir.join("containers.json"), LXC_PATH)
    }

    pub fn with_paths(store: impl Into<PathBuf>, lxc_path: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                store: store.into(),
                lxc_path: lxc_path.into(),
                lock: Mutex::new(()),
            }),
        }
    }

    fn read(&self) -> Result<Store, String> {
        match fs::read_to_string(&self.inner.store) {
            Ok(t) => {
                serde_json::from_str(&t).map_err(|e| format!("{}: {e}", self.inner.store.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
            Err(e) => Err(format!("{}: {e}", self.inner.store.display())),
        }
    }

    fn save(&self, s: &Store) -> Result<(), String> {
        let path = &self.inner.store;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let text = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        let _ = fs::remove_file(&tmp);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        use std::io::Write;
        f.write_all(&text)
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        drop(f);
        fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
    }

    /// Every container, in id order. One that no longer parses is an error,
    /// not a missing container: the IPAM supervisor and the in-use checks
    /// would otherwise forget its addresses and its rootfs.
    pub fn list(&self) -> Result<Vec<Entry>, String> {
        self.read()?
            .containers
            .into_values()
            .map(|r| {
                Ok(Entry {
                    def: ctxml::parse(&r.xml)?,
                    autostart: r.autostart,
                })
            })
            .collect()
    }

    /// A container by its id or its display title. An ambiguous title
    /// resolves to nothing rather than the wrong container.
    pub fn resolve(&self, key: &str) -> Option<Entry> {
        let all = self.list().ok()?;
        if let Some(e) = all.iter().find(|e| e.id() == key) {
            return Some(e.clone());
        }
        let mut m = all
            .into_iter()
            .filter(|e| !e.def.title.is_empty() && e.def.title == key);
        match (m.next(), m.next()) {
            (Some(e), None) => Some(e),
            _ => None,
        }
    }

    pub fn get(&self, id: &str) -> Result<Entry, String> {
        self.list()?
            .into_iter()
            .find(|e| e.id() == id)
            .ok_or_else(|| format!("no container '{id}'"))
    }

    /// The id ranges every other container's root maps to.
    pub fn used_bases(&self, except: Option<&str>) -> Result<Vec<u32>, String> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|e| Some(e.id()) != except)
            .filter_map(|e| e.def.id_base)
            .collect())
    }

    /// Store a definition: a new container, or a replacement for an existing
    /// one (`existing` names it). What only the daemon may decide is checked
    /// here, whoever is asking: the rootfs image and its mount never move,
    /// a uid range is never another container's, and every NIC has a MAC no
    /// other guest uses (`taken`), so its DHCP reservation is its own.
    pub fn store(
        &self,
        mut c: Container,
        existing: Option<&str>,
        taken: &HashSet<String>,
    ) -> Result<Container, String> {
        let _g = self.inner.lock.lock().map_err(|e| e.to_string())?;
        let mut s = self.read()?;
        if uuid::Uuid::parse_str(&c.id)
            .map(|u| u.hyphenated().to_string() != c.id)
            .unwrap_or(true)
        {
            return Err(format!("'{}' is not a canonical UUID", c.id));
        }
        match existing {
            Some(id) => {
                let cur = s
                    .containers
                    .get(id)
                    .ok_or_else(|| format!("no container '{id}'"))?;
                let cur = ctxml::parse(&cur.xml)?;
                if c.id != cur.id {
                    return Err(format!(
                        "the definition names '{}', not this container '{id}'",
                        c.id
                    ));
                }
                if c.rootfs != cur.rootfs {
                    return Err("the container's rootfs image cannot be changed".into());
                }
            }
            None if s.containers.contains_key(&c.id) => {
                return Err(format!("container '{}' already exists", c.id));
            }
            None => {}
        }
        if let Some(b) = c.id_base {
            let clash = s.containers.iter().any(|(id, r)| {
                id != &c.id && ctxml::parse(&r.xml).is_ok_and(|o| o.id_base == Some(b))
            });
            if clash {
                return Err(format!(
                    "uid range {b} belongs to another container; remove the <idmap> target and let the daemon allocate one"
                ));
            }
        }
        let title_clash = !c.title.is_empty()
            && s.containers.iter().any(|(id, r)| {
                id != &c.id && ctxml::parse(&r.xml).is_ok_and(|o| o.title == c.title)
            });
        if title_clash {
            return Err(format!("a container named '{}' already exists", c.title));
        }
        // Other containers' MACs as well as every VM's.
        let mut used: HashSet<String> = taken.clone();
        for (id, r) in &s.containers {
            if id != &c.id
                && let Ok(o) = ctxml::parse(&r.xml)
            {
                used.extend(o.nics.into_iter().filter_map(|n| n.mac));
            }
        }
        for n in &c.nics {
            if let Some(m) = &n.mac
                && used.contains(m)
            {
                return Err(format!("MAC {m} is already used by another guest"));
            }
        }
        let mut own: HashSet<String> = c.nics.iter().filter_map(|n| n.mac.clone()).collect();
        for (i, n) in c.nics.iter_mut().enumerate() {
            if n.mac.is_some() {
                continue;
            }
            let mut slot = 0u32;
            let mac = loop {
                let m = crate::qemu::stable_mac(&c.id, &format!("ct-nic{i}-{slot}"));
                if !used.contains(&m) && !own.contains(&m) {
                    break m;
                }
                slot += 1;
            };
            own.insert(mac.clone());
            n.mac = Some(mac);
        }
        let prev = s
            .containers
            .get(&c.id)
            .map(|r| r.autostart)
            .unwrap_or(false);
        s.containers.insert(
            c.id.clone(),
            Record {
                xml: ctxml::render(&c),
                autostart: prev,
            },
        );
        self.save(&s)?;
        Ok(c)
    }

    pub fn set_autostart(&self, id: &str, on: bool) -> Result<(), String> {
        let _g = self.inner.lock.lock().map_err(|e| e.to_string())?;
        let mut s = self.read()?;
        let r = s
            .containers
            .get_mut(id)
            .ok_or_else(|| format!("no container '{id}'"))?;
        r.autostart = on;
        self.save(&s)
    }

    /// Forget the definition. The caller has stopped the container and owns
    /// what happens to its rootfs.
    pub fn remove(&self, id: &str) -> Result<(), String> {
        let _g = self.inner.lock.lock().map_err(|e| e.to_string())?;
        let mut s = self.read()?;
        if s.containers.remove(id).is_none() {
            return Err(format!("no container '{id}'"));
        }
        self.save(&s)?;
        let _ = fs::remove_dir_all(self.dir(id));
        Ok(())
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.inner.lxc_path.join(id)
    }

    fn pid_file(&self, id: &str) -> PathBuf {
        self.dir(id).join("pid")
    }

    fn log_file(&self, id: &str) -> PathBuf {
        self.dir(id).join("lxc.log")
    }

    /// LXC's monitor for this container, if it is running: the pid
    /// `lxc-start -p` recorded (the daemonized `lxc-start` itself, not init),
    /// confirmed by the title LXC gives it, which names this lxcpath and id. A
    /// stale pid file then never names some other process that reused it.
    fn monitor_pid(&self, id: &str) -> Option<u32> {
        let pid: u32 = fs::read_to_string(self.pid_file(id))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let title = String::from_utf8_lossy(&cmdline);
        let want = format!(
            "[lxc monitor] {} {id}",
            self.inner.lxc_path.to_string_lossy()
        );
        title.starts_with(&want).then_some(pid)
    }

    /// The container's init, if it is running: the monitor's child that is
    /// inside the container's `lxc.payload.<id>` cgroup.
    pub fn init_pid(&self, id: &str) -> Option<u32> {
        let monitor = self.monitor_pid(id)?;
        let children =
            fs::read_to_string(format!("/proc/{monitor}/task/{monitor}/children")).ok()?;
        children
            .split_whitespace()
            .filter_map(|p| p.parse().ok())
            .find(|&p| in_payload(p, id))
    }

    pub fn state(&self, id: &str) -> State {
        if self.init_pid(id).is_some() {
            State::Running
        } else {
            State::Stopped
        }
    }

    /// Mount the rootfs, write the LXC config and start the container. Every
    /// NIC's bridge must exist: LXC would otherwise fail with a bare "Failed
    /// to attach" deep in its log.
    pub fn start(&self, id: &str) -> Result<(), String> {
        let e = self.get(id)?;
        if self.state(id) == State::Running {
            return Err("the container is already running".into());
        }
        let missing = crate::ct::missing_tools();
        if !missing.is_empty() {
            return Err(format!(
                "this host is missing {} needed to run containers (install the lxc package)",
                missing.join(", ")
            ));
        }
        for n in &e.def.nics {
            if !crate::hostnet::is_bridge(&n.bridge) {
                return Err(format!(
                    "the network on bridge {} is not running; start it first",
                    n.bridge
                ));
            }
        }
        let rootfs = crate::ct::ensure_mounted(id, &e.def.rootfs)?;
        // A switch between privileged and unprivileged changed the mapping but
        // not the files; make them agree, or root owns nothing it boots into.
        if crate::ct::reconcile_owner(&rootfs, e.def.root_uid())? {
            tracing::info!("container {id}: rootfs re-owned for its uid mapping");
        }
        let root = self.inner.lxc_path.as_path();
        fs::create_dir_all(root).map_err(|err| format!("{}: {err}", root.display()))?;
        let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o711));
        let dir = self.dir(id);
        fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_file(self.pid_file(id));
        let _ = fs::remove_file(self.log_file(id));
        let config = render_config(&e.def, &rootfs, &self.log_file(id), host_cpus());
        fs::write(dir.join("config"), config)
            .map_err(|err| format!("{}: {err}", dir.join("config").display()))?;
        let out = Command::new("lxc-start")
            .arg("-P")
            .arg(&self.inner.lxc_path)
            .args(["-n", id, "-d", "-p"])
            .arg(self.pid_file(id))
            .output()
            .map_err(|err| format!("lxc-start: {err}"))?;
        if !out.status.success() || self.init_pid(id).is_none() {
            return Err(format!(
                "the container did not start: {}",
                self.failure(id, &String::from_utf8_lossy(&out.stderr))
            ));
        }
        // `<bandwidth>` is not something LXC applies: shape the host side of
        // each veth now, as a QEMU start does its taps.
        for n in &e.def.nics {
            let (Some(mac), rx, tx) = (
                n.mac.as_deref(),
                n.bandwidth.inbound.as_ref().and_then(|r| r.average),
                n.bandwidth.outbound.as_ref().and_then(|r| r.average),
            ) else {
                continue;
            };
            if (rx.is_some() || tx.is_some())
                && let Err(err) = crate::hostnet::apply_tc(&veth_name(id, mac), rx, tx)
            {
                let _ = self.force_stop(id);
                return Err(format!("rate limit on {mac}: {err}"));
            }
        }
        Ok(())
    }

    /// LXC's own reason, from its log, rather than "failed to start".
    fn failure(&self, id: &str, stderr: &str) -> String {
        let log = fs::read_to_string(self.log_file(id)).unwrap_or_default();
        let errors: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("ERROR"))
            .map(|l| l.rsplit_once(" - ").map_or(l, |x| x.1).trim())
            .collect();
        let tail = errors[errors.len().saturating_sub(4)..].join("; ");
        if !tail.is_empty() {
            tail
        } else if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            "lxc-start gave no reason".into()
        }
    }

    /// Ask the container's init to power off, and wait up to `timeout` for it
    /// to be gone. A container that has not stopped by then is an error the
    /// caller decides about (a delete forces it; a Shutdown click reports it).
    pub fn shutdown(&self, id: &str, timeout: Duration) -> Result<(), String> {
        let Some(pid) = self.init_pid(id) else {
            return Err("the container is not running".into());
        };
        let sig = halt_signal(pid);
        // Re-checked right before the signal, so a pid that changed hands
        // since `init_pid` is never signalled.
        if !in_payload(pid, id) || unsafe { libc::kill(pid as i32, sig) } != 0 {
            return Err("the container is not running".into());
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.state(id) == State::Stopped {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(format!(
            "the container did not shut down within {} s",
            timeout.as_secs()
        ))
    }

    pub fn force_stop(&self, id: &str) -> Result<(), String> {
        if self.state(id) == State::Stopped {
            return Ok(());
        }
        let out = Command::new("lxc-stop")
            .arg("-P")
            .arg(&self.inner.lxc_path)
            .args(["-n", id, "-k"])
            .output()
            .map_err(|e| format!("lxc-stop: {e}"))?;
        for _ in 0..50 {
            if self.state(id) == State::Stopped {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!(
            "the container could not be stopped: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }

    /// Ask init to reboot. LXC's reboot signal (SIGINT, the ctrl-alt-del a
    /// systemd, sysvinit and busybox inittab all map to a reboot) is the one
    /// every init understands, so this is `lxc-stop -r`.
    pub fn reboot(&self, id: &str) -> Result<(), String> {
        if self.state(id) == State::Stopped {
            return Err("the container is not running".into());
        }
        let out = Command::new("lxc-stop")
            .arg("-P")
            .arg(&self.inner.lxc_path)
            .args(["-n", id, "-r", "-t", "0"])
            .output()
            .map_err(|e| format!("lxc-stop: {e}"))?;
        // `-t 0` returns once the request is sent; LXC reports that as a
        // timeout, which here is success.
        if out.status.success() || self.state(id) == State::Running {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    /// The container's cgroup, while it runs. Found from init's own cgroup
    /// rather than assumed, so LXC placing it under another parent (a service
    /// cgroup, a different root) still works.
    pub fn cgroup(&self, id: &str) -> Option<PathBuf> {
        let pid = self.init_pid(id)?;
        payload_cgroup(pid, id)
    }

    /// Apply a definition's limits to a running container's cgroup. Every one
    /// is a cgroup v2 file, so all of them change live; a stopped container
    /// gets them from its config at the next start.
    pub fn apply_limits(&self, c: &Container) -> Result<(), String> {
        let cg = self.cgroup(&c.id).ok_or("the container is not running")?;
        for (file, value) in limits(c, host_cpus()) {
            // A zero-length write never reaches the kernel's handler; a bare
            // newline is how an empty cpuset ("every CPU") is written.
            let bytes = if value.is_empty() {
                "\n".to_string()
            } else {
                value.clone()
            };
            fs::write(cg.join(file), bytes).map_err(|e| format!("{file} = {value}: {e}"))?;
        }
        Ok(())
    }

    /// Shape one NIC's host veth now. A stopped container has no veth; its
    /// limit is applied at the next start.
    pub fn apply_bandwidth(
        &self,
        id: &str,
        mac: &str,
        rx: Option<u64>,
        tx: Option<u64>,
    ) -> Result<bool, String> {
        if self.state(id) != State::Running {
            return Ok(false);
        }
        crate::hostnet::apply_tc(&veth_name(id, mac), rx, tx)?;
        Ok(true)
    }

    pub fn stats(&self, e: &Entry) -> Stats {
        let memory_kib = u64::from(e.def.memory_mib) * 1024;
        let cg = self.cgroup(e.id());
        let (cpu_time_ns, rss_kib) = match &cg {
            Some(cg) => (
                cg_key(cg, "cpu.stat", "usage_usec").unwrap_or(0) * 1000,
                Some(cg_value(cg, "memory.current").unwrap_or(0) / 1024),
            ),
            None => (0, Some(0)),
        };
        Stats {
            state: if cg.is_some() {
                State::Running.as_str()
            } else {
                State::Stopped.as_str()
            },
            cpu_time_ns,
            vcpus: e.def.vcpus.min(u32::from(u16::MAX)) as u16,
            memory_kib,
            max_memory_kib: memory_kib,
            rss_kib,
            // What the container can still use: its limit less what it holds,
            // page cache excluded (the kernel reclaims that first).
            usable_kib: cg.as_ref().map(|cg| {
                let used = cg_value(cg, "memory.current")
                    .unwrap_or(0)
                    .saturating_sub(cg_key(cg, "memory.stat", "inactive_file").unwrap_or(0));
                memory_kib.saturating_sub(used / 1024)
            }),
        }
    }

    /// Bytes read and written by the container, summed over every device its
    /// cgroup touched (the rootfs loop device, in practice).
    pub fn io(&self, id: &str) -> (u64, u64) {
        let Some(cg) = self.cgroup(id) else {
            return (0, 0);
        };
        let text = fs::read_to_string(cg.join("io.stat")).unwrap_or_default();
        let mut r = 0;
        let mut w = 0;
        for f in text.split_whitespace() {
            if let Some(v) = f.strip_prefix("rbytes=") {
                r += v.parse::<u64>().unwrap_or(0);
            } else if let Some(v) = f.strip_prefix("wbytes=") {
                w += v.parse::<u64>().unwrap_or(0);
            }
        }
        (r, w)
    }

    /// One row per NIC in the definition, shaped like a VM's. The host side
    /// of the veth only exists while the container runs.
    pub fn interfaces(&self, e: &Entry) -> Vec<InterfaceStats> {
        let live = self.state(e.id()) == State::Running;
        e.def
            .nics
            .iter()
            .map(|n| {
                let dev = n
                    .mac
                    .as_deref()
                    .filter(|_| live)
                    .map(|m| veth_name(e.id(), m))
                    .filter(|v| Path::new(SYS_NET).join(v).exists());
                let counter = |which: &str| {
                    dev.as_ref()
                        .and_then(|d| {
                            fs::read_to_string(
                                Path::new(SYS_NET).join(d).join("statistics").join(which),
                            )
                            .ok()
                        })
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0)
                };
                InterfaceStats {
                    rx_bytes: counter("rx_bytes"),
                    tx_bytes: counter("tx_bytes"),
                    dev,
                    mac: n.mac.clone(),
                    model: "veth",
                    network: Some(n.bridge.clone()),
                    rx_kib: n.bandwidth.inbound.as_ref().and_then(|r| r.average),
                    tx_kib: n.bandwidth.outbound.as_ref().and_then(|r| r.average),
                }
            })
            .collect()
    }

    /// Start every autostart container that is not running. Runs once, after
    /// the networks are up, so their bridges exist.
    pub fn autostart(&self) {
        let Ok(all) = self.list() else {
            return;
        };
        for e in all.into_iter().filter(|e| e.autostart) {
            if self.state(e.id()) == State::Running {
                continue;
            }
            match self.start(e.id()) {
                Ok(()) => tracing::info!("container {}: autostarted", e.id()),
                Err(err) => tracing::warn!("container {}: autostart failed: {err}", e.id()),
            }
        }
    }

    /// The pty `lxc-console` attaches to, for a console resize: the monitor
    /// holds the container's `/dev/console` open, and setting the window on
    /// that exact file is what the shell and `apt` inside then read. The
    /// browser's size on our own pty does not get there: LXC relays the bytes
    /// through a proxy pty and does not carry the size across (measured with
    /// LXC 6.0: `stty -F /dev/console size` stayed 24 80).
    pub fn console_fd(&self, id: &str) -> Result<PathBuf, String> {
        use std::os::unix::fs::MetadataExt;
        let monitor = self.monitor_pid(id).ok_or("the container is not running")?;
        let init = self.init_pid(id).ok_or("the container is not running")?;
        // The console inside, identified by its inode on its devpts.
        let console = fs::metadata(format!("/proc/{init}/root/dev/console"))
            .map_err(|e| format!("the container's console: {e}"))?;
        let fds = fs::read_dir(format!("/proc/{monitor}/fd")).map_err(|e| e.to_string())?;
        for fd in fds.flatten() {
            if let Ok(m) = fs::metadata(fd.path())
                && m.dev() == console.dev()
                && m.ino() == console.ino()
            {
                return Ok(fd.path());
            }
        }
        Err("the container's console is not open".into())
    }

    /// `lxc-console`'s argv for this container: escape disabled (`-e -1`), so
    /// no key the browser sends detaches it, and tty 0, the console.
    pub fn console_argv(&self, id: &str) -> Vec<String> {
        vec![
            lxc_console_path(),
            "-P".into(),
            self.inner.lxc_path.to_string_lossy().into_owned(),
            "-n".into(),
            id.into(),
            "-t".into(),
            "0".into(),
            "-e".into(),
            "-1".into(),
        ]
    }

    /// Every container's NICs, for the IPAM snapshot: MAC -> (id, name, bridge).
    pub fn nics(&self) -> Result<Vec<(String, String, String, String)>, String> {
        Ok(self
            .list()?
            .into_iter()
            .flat_map(|e| {
                let (id, name) = (e.def.id.clone(), e.def.name().to_string());
                e.def
                    .nics
                    .into_iter()
                    .filter_map(move |n| Some((n.mac?, id.clone(), name.clone(), n.bridge)))
            })
            .collect())
    }

    /// Each container's rootfs as a `<disk>` the in-use checks read, so a
    /// volume list says which container holds an image and a volume delete
    /// refuses it, exactly as for a VM's disk.
    pub fn disk_definitions(&self) -> Vec<(String, String)> {
        self.list()
            .unwrap_or_default()
            .into_iter()
            .map(|e| {
                (
                    e.def.id.clone(),
                    format!(
                        "<domain><devices><disk type='file' device='disk'><source file='{}'/></disk></devices></domain>",
                        crate::domainxml::esc(&e.def.rootfs)
                    ),
                )
            })
            .collect()
    }
}

fn lxc_console_path() -> String {
    for d in ["/usr/bin", "/usr/local/bin", "/bin"] {
        let p = Path::new(d).join("lxc-console");
        if p.is_file() {
            return p.to_string_lossy().into_owned();
        }
    }
    "/usr/bin/lxc-console".into()
}

/// The host side of a container NIC's veth: `vc` + 11 hex, 13 characters.
/// Derived from the MAC, like a VM's tap, so it is never stored and a
/// restart finds it again.
pub fn veth_name(id: &str, mac: &str) -> String {
    let h = crate::hostnet::fnv1a(&format!("veth/{id}/{mac}"));
    format!("vc{:011x}", h & 0x7ff_ffff_ffff)
}

/// `pid`'s cgroup path, as the host sees it.
fn cgroup_of(pid: u32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::to_string)
}

/// Whether `pid` is inside container `id`'s cgroup (or one below it, which is
/// where a systemd init moves itself).
fn in_payload(pid: u32, id: &str) -> bool {
    cgroup_of(pid).is_some_and(|p| payload_of(&p, id).is_some())
}

/// The `.../lxc.payload.<id>` prefix of a cgroup path.
fn payload_of(path: &str, id: &str) -> Option<String> {
    let want = format!("lxc.payload.{id}");
    let mut out = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        out.push('/');
        out.push_str(part);
        if part == want {
            return Some(out);
        }
    }
    None
}

fn payload_cgroup(pid: u32, id: &str) -> Option<PathBuf> {
    let rel = payload_of(&cgroup_of(pid)?, id)?;
    let p = Path::new(CGROUP_ROOT).join(rel.trim_start_matches('/'));
    p.is_dir().then_some(p)
}

fn cg_value(cg: &Path, file: &str) -> Option<u64> {
    fs::read_to_string(cg.join(file)).ok()?.trim().parse().ok()
}

fn cg_key(cg: &Path, file: &str, key: &str) -> Option<u64> {
    fs::read_to_string(cg.join(file))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok())
}

/// The signal that makes this init power off. Read from what the kernel
/// says init is executing, not from a file in the rootfs, which the container
/// controls.
fn halt_signal(pid: u32) -> i32 {
    let exe = fs::read_link(format!("/proc/{pid}/exe"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    halt_signal_for(&exe)
}

fn halt_signal_for(exe: &str) -> i32 {
    let base = exe.rsplit('/').next().unwrap_or("");
    if base == "busybox" {
        libc::SIGUSR2
    } else if base == "systemd" || exe.contains("/systemd/") {
        libc::SIGRTMIN() + 3
    } else {
        libc::SIGPWR
    }
}

pub fn host_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

/// The CPUs a container with `vcpus` may run on: all of them when it asks
/// for as many as the host has, else a run of `vcpus` starting at an offset
/// derived from its id, so containers spread over the host instead of all
/// queueing on CPU 0.
pub fn cpuset(id: &str, vcpus: u32, host: u32) -> Option<String> {
    if vcpus == 0 || vcpus >= host {
        return None;
    }
    let off = (crate::hostnet::fnv1a(id) % u64::from(host)) as u32;
    let cpus: Vec<String> = (0..vcpus).map(|i| ((off + i) % host).to_string()).collect();
    Some(cpus.join(","))
}

/// Whether this host accounts swap per cgroup. Without it there is no
/// `memory.swap.max` to write, and a config naming one fails the start.
fn swap_accounting() -> bool {
    static S: OnceLock<bool> = OnceLock::new();
    *S.get_or_init(|| {
        fs::read_dir(CGROUP_ROOT)
            .map(|d| {
                d.flatten()
                    .any(|e| e.path().join("memory.swap.max").exists())
            })
            .unwrap_or(false)
    })
}

/// Whether the host offers the cpuset controller, which LXC then enables for
/// the container's cgroup.
fn has_cpuset() -> bool {
    fs::read_to_string(Path::new(CGROUP_ROOT).join("cgroup.controllers"))
        .is_ok_and(|c| c.split_whitespace().any(|x| x == "cpuset"))
}

/// The cgroup v2 files a definition sets, and their values.
fn limits(c: &Container, host: u32) -> Vec<(&'static str, String)> {
    limits_for(c, host, swap_accounting(), has_cpuset())
}

fn limits_for(
    c: &Container,
    host: u32,
    swap: bool,
    cpuset_ok: bool,
) -> Vec<(&'static str, String)> {
    let mut out = vec![("memory.max", format!("{}", u64::from(c.memory_mib) << 20))];
    if swap {
        out.push((
            "memory.swap.max",
            format!("{}", u64::from(c.swap_mib) << 20),
        ));
    }
    out.extend([(
        "cpu.max",
        if c.cpu_limit_pct > 0 {
            format!(
                "{} {CPU_PERIOD}",
                u64::from(c.cpu_limit_pct) * CPU_PERIOD / 100
            )
        } else {
            format!("max {CPU_PERIOD}")
        },
    )]);
    if cpuset_ok {
        // An empty cpuset.cpus means "inherit the parent's", i.e. every CPU.
        out.push((
            "cpuset.cpus",
            cpuset(&c.id, c.vcpus, host).unwrap_or_default(),
        ));
    }
    out
}

/// The LXC config for one start. Values come from the parsed definition only;
/// anything a user typed that reaches here (the hostname) has already been
/// reduced to `[a-z0-9-]`.
pub fn render_config(c: &Container, rootfs: &Path, log: &Path, host: u32) -> String {
    render_config_with(c, rootfs, log, limits(c, host))
}

fn render_config_with(
    c: &Container,
    rootfs: &Path,
    log: &Path,
    limits: Vec<(&'static str, String)>,
) -> String {
    let mut s = String::from(
        "# Rendered by virt-man-daemon from containers.json at each start.\n\
         # Edits here are lost: change the container's definition instead.\n",
    );
    s.push_str(&format!(
        "lxc.uts.name = {}\n",
        crate::ct::hostname_from(c.name())
    ));
    s.push_str("lxc.include = /usr/share/lxc/config/common.conf\n");
    if let Some(b) = c.id_base {
        s.push_str("lxc.include = /usr/share/lxc/config/userns.conf\n");
        let n = crate::ct::ID_COUNT;
        s.push_str(&format!(
            "lxc.idmap = u 0 {b} {n}\nlxc.idmap = g 0 {b} {n}\n"
        ));
    }
    s.push_str(&format!("lxc.rootfs.path = dir:{}\n", rootfs.display()));
    s.push_str(&format!("lxc.init.cmd = {}\n", c.init));
    // One console, no ttys: a getty on tty1-6 would only fail, over and over,
    // on the console a person is typing in.
    s.push_str("lxc.tty.max = 0\n");
    if c.nics.is_empty() {
        // Without any lxc.net entry LXC shares the host's network namespace.
        // No NIC means no network, never the host's.
        s.push_str("lxc.net.0.type = empty\n");
    }
    for (i, n) in c.nics.iter().enumerate() {
        let mac = n.mac.as_deref().unwrap_or_default();
        s.push_str(&format!(
            "lxc.net.{i}.type = veth\nlxc.net.{i}.link = {}\nlxc.net.{i}.flags = up\n\
             lxc.net.{i}.hwaddr = {mac}\nlxc.net.{i}.veth.pair = {}\nlxc.net.{i}.name = eth{i}\n",
            n.bridge,
            veth_name(&c.id, mac),
        ));
    }
    for (file, value) in limits {
        if !value.is_empty() {
            s.push_str(&format!("lxc.cgroup2.{file} = {value}\n"));
        }
    }
    s.push_str(&format!(
        "lxc.log.file = {}\nlxc.log.level = WARN\n",
        log.display()
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ct::ID_BASE;
    use crate::ctxml::Nic;

    const ID: &str = "0b776cc0-f6b1-4bbd-b67b-88ccc6c24a76";

    fn def() -> Container {
        Container {
            id: ID.into(),
            title: "Web 1".into(),
            description: String::new(),
            memory_mib: 512,
            swap_mib: 0,
            vcpus: 2,
            cpu_limit_pct: 50,
            init: "/sbin/init".into(),
            id_base: Some(ID_BASE),
            rootfs: "/p/ct.raw".into(),
            nics: vec![Nic {
                mac: Some("52:54:00:aa:bb:01".into()),
                bridge: "vbr1".into(),
                bandwidth: Default::default(),
            }],
        }
    }

    fn manager(name: &str) -> (Manager, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lxc-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (
            Manager::with_paths(dir.join("containers.json"), dir.join("lxc")),
            dir,
        )
    }

    #[test]
    fn the_config_maps_ids_limits_and_veths() {
        let cfg = render_config_with(
            &def(),
            Path::new("/run/x"),
            Path::new("/run/l"),
            limits_for(&def(), 8, true, true),
        );
        for want in [
            "lxc.uts.name = web-1\n",
            "lxc.include = /usr/share/lxc/config/userns.conf\n",
            &format!("lxc.idmap = u 0 {ID_BASE} 65536\n"),
            "lxc.rootfs.path = dir:/run/x\n",
            "lxc.net.0.link = vbr1\n",
            "lxc.net.0.hwaddr = 52:54:00:aa:bb:01\n",
            "lxc.cgroup2.memory.max = 536870912\n",
            "lxc.cgroup2.memory.swap.max = 0\n",
            "lxc.cgroup2.cpu.max = 50000 100000\n",
            "lxc.cgroup2.cpuset.cpus = ",
        ] {
            assert!(cfg.contains(want), "{want:?} in\n{cfg}");
        }
        assert!(!cfg.contains("type = empty"), "{cfg}");
        let privileged = Container {
            id_base: None,
            nics: vec![],
            vcpus: 8,
            ..def()
        };
        let cfg = render_config_with(
            &privileged,
            Path::new("/r"),
            Path::new("/l"),
            limits_for(&privileged, 8, false, true),
        );
        assert!(!cfg.contains("idmap") && !cfg.contains("userns"), "{cfg}");
        assert!(
            cfg.contains("lxc.net.0.type = empty\n"),
            "no NIC is no network, never the host's: {cfg}"
        );
        assert!(!cfg.contains("cpuset"), "every CPU: {cfg}");
        assert!(
            !cfg.contains("swap"),
            "no swap accounting, no swap limit: {cfg}"
        );
    }

    #[test]
    fn veth_names_fit_the_kernel_and_follow_the_mac() {
        let a = veth_name(ID, "52:54:00:aa:bb:01");
        assert!(a.len() <= 15 && a.starts_with("vc"), "{a}");
        assert!(crate::hostnet::valid_name(&a));
        assert_eq!(a, veth_name(ID, "52:54:00:aa:bb:01"));
        assert_ne!(a, veth_name(ID, "52:54:00:aa:bb:02"));
    }

    #[test]
    fn cpusets_spread_and_wrap() {
        assert_eq!(cpuset(ID, 4, 4), None);
        let s = cpuset(ID, 3, 4).unwrap();
        assert_eq!(s.split(',').count(), 3, "{s}");
        assert!(s.split(',').all(|c| c.parse::<u32>().unwrap() < 4));
    }

    #[test]
    fn the_payload_cgroup_is_matched_exactly() {
        assert_eq!(
            payload_of(&format!("/lxc.payload.{ID}/init.scope"), ID).as_deref(),
            Some(&*format!("/lxc.payload.{ID}"))
        );
        assert_eq!(
            payload_of(&format!("/system.slice/x/lxc.payload.{ID}"), ID).as_deref(),
            Some(&*format!("/system.slice/x/lxc.payload.{ID}"))
        );
        assert!(payload_of(&format!("/lxc.payload.{ID}x"), ID).is_none());
        assert!(payload_of(&format!("/lxc.monitor.{ID}"), ID).is_none());
        assert!(payload_of("/user.slice/session-1.scope", ID).is_none());
    }

    #[test]
    fn each_init_gets_the_signal_it_powers_off_on() {
        assert_eq!(halt_signal_for("/bin/busybox"), libc::SIGUSR2);
        assert_eq!(
            halt_signal_for("/usr/lib/systemd/systemd"),
            libc::SIGRTMIN() + 3
        );
        assert_eq!(halt_signal_for("/sbin/init"), libc::SIGPWR);
    }

    #[test]
    fn the_store_keeps_what_only_the_daemon_decides() {
        let (m, dir) = manager("store");
        let mut c = def();
        c.nics[0].mac = None;
        let stored = m.store(c.clone(), None, &HashSet::new()).unwrap();
        let mac = stored.nics[0].mac.clone().expect("a MAC is assigned");
        assert_eq!(m.resolve("Web 1").unwrap().id(), ID, "by title");
        assert_eq!(
            m.resolve(ID).unwrap().def.nics[0].mac.as_deref(),
            Some(&*mac)
        );
        assert!(m.store(c.clone(), None, &HashSet::new()).is_err(), "twice");

        let moved = Container {
            rootfs: "/p/other.raw".into(),
            ..stored.clone()
        };
        assert!(
            m.store(moved, Some(ID), &HashSet::new()).is_err(),
            "rootfs moved"
        );

        let other = "11111111-2222-4333-8444-555555555555";
        let clash = Container {
            id: other.into(),
            title: "other".into(),
            rootfs: "/p/o.raw".into(),
            nics: vec![],
            ..stored.clone()
        };
        let err = m.store(clash.clone(), None, &HashSet::new()).unwrap_err();
        assert!(err.contains("uid range"), "{err}");
        let same_title = Container {
            id_base: Some(ID_BASE + crate::ct::ID_COUNT),
            title: "Web 1".into(),
            ..clash.clone()
        };
        assert!(
            m.store(same_title, None, &HashSet::new()).is_err(),
            "title clash"
        );
        let taken: HashSet<String> = [mac.clone()].into();
        let dup_mac = Container {
            id_base: None,
            nics: stored.nics.clone(),
            ..clash
        };
        assert!(m.store(dup_mac, None, &taken).is_err(), "a VM's MAC");
        assert_eq!(m.used_bases(None).unwrap(), vec![ID_BASE]);

        m.set_autostart(ID, true).unwrap();
        let again = m.store(stored, Some(ID), &HashSet::new()).unwrap();
        assert!(m.get(ID).unwrap().autostart, "a redefine keeps autostart");
        assert_eq!(again.nics[0].mac.as_deref(), Some(&*mac));
        assert_eq!(m.nics().unwrap()[0].0, mac);
        assert!(m.disk_definitions()[0].1.contains("/p/ct.raw"));
        assert_eq!(m.state(ID), State::Stopped);
        m.remove(ID).unwrap();
        assert!(m.list().unwrap().is_empty());
        let _ = fs::remove_dir_all(dir);
    }
}
