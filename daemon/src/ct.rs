//! Container root filesystems: making one from a template, mounting it, and
//! the few first-boot writes a template needs.
//!
//! A container's rootfs is a raw ext4 image, a volume in a storage pool, so
//! quotas, deletion logging and the in-use check treat it like a VM disk.
//! The daemon loop-mounts the image itself under `MOUNT_ROOT` and LXC is given
//! that directory (`lxc.rootfs.path = dir:...`), rather than LXC's own `loop:`
//! backend, so the mount is the daemon's to check, grow and unmount:
//!
//! - Every directory above that mount must be traversable by the mapped uid,
//!   or the container cannot pivot into it -- which is why `MOUNT_ROOT` is
//!   under `/run`, mode 0711, and not under the 0700 data dir.
//! - A mount does not survive a reboot. `ensure_mounted` runs before every
//!   start, so a stopped container is always startable and a running one's
//!   rootfs is never unmounted.
//!
//! **Unprivileged means a user namespace.** An unprivileged container's
//! `<idmap>` maps its uid 0..65536 to a range owned by nobody else on the host
//! (`ID_BASE` upwards, one `ID_COUNT` block per container, recorded in its own
//! definition). Files are written into the rootfs *from inside that same
//! mapping* (`unshare --map-users`), so the kernel does the ownership shift and
//! nothing ever runs as host root against a path the container controls: a
//! symlink planted in the image resolves inside the namespace, where host
//! files are not writable. Measured: `echo > evil/x` through a planted
//! `evil -> /etc` link is refused.
//!
//! A **privileged** container has no `<idmap>`: its root is host root. That is
//! the whole difference, and the absence of `<idmap>` in the definition is the
//! only record of it (the web server reads it back, never a flag of its own).

use std::path::{Path, PathBuf};
use std::process::Command;

pub const MOUNT_ROOT: &str = "/run/virt-man-daemon/ct";
/// Each container's namespace gets this many ids, as Proxmox and LXD do.
pub const ID_COUNT: u32 = 65536;
/// Ranges start here: far above any real account or `/etc/subuid` range, so
/// a container's uid is never a host user's.
pub const ID_BASE: u32 = 1 << 30;

/// Where a container's rootfs is mounted. Keyed by UUID, like its id range:
/// a rename changes the name, and a mount point the definition names must not
/// move with it.
pub fn mount_dir(uuid: &str) -> PathBuf {
    Path::new(MOUNT_ROOT).join(uuid)
}

// ---------------------------------------------------------------- id ranges

/// The lowest free range, given the ones in use. The ranges in use are the
/// other containers' `<idmap>` targets: the definitions are the only record,
/// so a deleted container's range is free again with nothing to release.
pub fn free_base(used: impl Iterator<Item = u32>) -> u32 {
    let used: std::collections::BTreeSet<u32> = used.collect();
    let mut b = ID_BASE;
    while used.contains(&b) {
        b += ID_COUNT;
    }
    b
}

/// Whether `base` is a range this daemon could have handed out: a whole
/// block at or above `ID_BASE`. Anything else maps the container's root onto
/// some host account (`target='0'` is host root), so it is refused rather than
/// trusted.
pub fn valid_base(base: u32) -> bool {
    base >= ID_BASE
        && (base - ID_BASE).is_multiple_of(ID_COUNT)
        && base.checked_add(ID_COUNT).is_some()
}

// ---------------------------------------------------------------- names

/// A hostname as RFC 1123 allows: what is written into `/etc/hostname`.
pub fn check_hostname(h: &str) -> Result<(), String> {
    let ok = !h.is_empty()
        && h.len() <= 63
        && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !h.starts_with('-')
        && !h.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "'{h}' is not a valid hostname (letters, digits and '-', up to 63)"
        ))
    }
}

/// A hostname from a display name: lower-case, other characters to '-'.
pub fn hostname_from(title: &str) -> String {
    let mut h: String = title
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    h.truncate(63);
    let h = h.trim_matches('-').to_string();
    if h.is_empty() { "ct".into() } else { h }
}

// ---------------------------------------------------------------- mounts

fn run(cmd: &mut Command) -> Result<(), String> {
    let out = cmd.output().map_err(|e| format!("{cmd:?}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{} failed: {}",
            cmd.get_program().to_string_lossy(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn is_mounted(dir: &Path) -> bool {
    let Ok(m) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let d = dir.to_string_lossy();
    m.lines()
        .any(|l| l.split(' ').nth(4).is_some_and(|p| p == d))
}

/// Mount a container's rootfs if it is not already. Idempotent: called before
/// every start, and by the supervisor for every defined container.
pub fn ensure_mounted(uuid: &str, image: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(MOUNT_ROOT).map_err(|e| format!("{MOUNT_ROOT}: {e}"))?;
    // 0711: the mapped root must be able to walk to its own rootfs.
    use std::os::unix::fs::PermissionsExt;
    for d in ["/run/virt-man-daemon", MOUNT_ROOT] {
        let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o711));
    }
    let dir = mount_dir(uuid);
    if is_mounted(&dir) {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    run(Command::new("mount")
        .args(["-o", "loop,nodev"])
        .arg(image)
        .arg(&dir))?;
    Ok(dir)
}

/// Make the files agree with the mapping before a start.
///
/// A container's files are owned by the uids its root maps to. Switching it
/// between privileged and unprivileged (an XML edit, allowed with
/// CT.Privileged) changes the mapping but not the files, and the container
/// would boot with a root that owns nothing -- no login, no writes to its own
/// `/etc`. So when the rootfs root's owner is not the mapped root, every file
/// is shifted by the difference, preserving the relative ids (a service user's
/// files stay that user's).
///
/// The shift runs as host root over a tree the container controls, so it
/// never follows a symlink (`lchown` on the link itself) and never leaves the
/// filesystem (it is one mount, walked with `-xdev`-equivalent checks).
pub fn reconcile_owner(dir: &Path, want: u32) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;
    let have = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .uid();
    if have == want {
        return Ok(false);
    }
    let dev = std::fs::symlink_metadata(dir)
        .map_err(|e| e.to_string())?
        .dev();
    let shift = |id: u32| -> u32 {
        // Ids inside the old 64k range move with it; anything outside (a
        // file someone chowned by hand) is mapped to the new root.
        let rel = id.wrapping_sub(have);
        if rel < ID_COUNT { want + rel } else { want }
    };
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let m = std::fs::symlink_metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        if m.dev() != dev {
            continue;
        }
        std::os::unix::fs::lchown(&p, Some(shift(m.uid())), Some(shift(m.gid())))
            .map_err(|e| format!("{}: {e}", p.display()))?;
        // chown clears setuid/setgid; put the mode back (not through a link).
        if !m.file_type().is_symlink() {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(m.mode() & 0o7777));
        }
        if m.is_dir() {
            for e in std::fs::read_dir(&p).map_err(|e| format!("{}: {e}", p.display()))? {
                stack.push(e.map_err(|e| e.to_string())?.path());
            }
        }
    }
    Ok(true)
}

/// Grow the ext4 inside a rootfs image to fill it, after the volume grew.
/// Online when mounted: the loop device is told its backing file changed
/// (`losetup -c`), then `resize2fs` grows the mounted filesystem. Offline,
/// `resize2fs` runs on the image itself (after the `e2fsck` it insists on).
pub fn grow_fs(image: &str) -> Result<(), String> {
    let out = Command::new("losetup")
        .args(["-j", image])
        .output()
        .map_err(|e| format!("losetup: {e}"))?;
    let dev = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|l| l.split(':').next())
        .map(str::to_string);
    match dev {
        Some(dev) => {
            run(Command::new("losetup").args(["-c", &dev]))?;
            run(Command::new("resize2fs").arg(&dev))
        }
        None => {
            // e2fsck exits 1 when it fixed something; that is not a failure.
            let _ = Command::new("e2fsck").args(["-f", "-p", image]).status();
            run(Command::new("resize2fs").arg(image))
        }
    }
}

/// Unmount a deleted container's rootfs. A busy mount is left alone.
pub fn unmount(uuid: &str) {
    let dir = mount_dir(uuid);
    if is_mounted(&dir) && run(Command::new("umount").arg(&dir)).is_err() {
        return;
    }
    let _ = std::fs::remove_dir(&dir);
}

// ---------------------------------------------------------------- building

/// Run `argv` as root of the container's namespace (or as host root for a
/// privileged one), with `stdin` piped from a file.
fn in_ns(id_base: Option<u32>, argv: &[&str], stdin: Option<&Path>) -> Result<(), String> {
    let mut cmd = match id_base {
        Some(b) => {
            let map = format!("0:{b}:{ID_COUNT}");
            let mut c = Command::new("unshare");
            c.args([
                "--user",
                &format!("--map-users={map}"),
                &format!("--map-groups={map}"),
                "--setuid",
                "0",
                "--setgid",
                "0",
                "--",
            ]);
            c.args(argv);
            c
        }
        None => {
            let mut c = Command::new(argv[0]);
            c.args(&argv[1..]);
            c
        }
    };
    if let Some(p) = stdin {
        cmd.stdin(std::fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?);
    }
    run(&mut cmd)
}

/// The decompressor flag tar needs for a template.
fn tar_flag(template: &str) -> Result<&'static str, String> {
    let t = template.to_ascii_lowercase();
    Ok(if t.ends_with(".tar.xz") {
        "-J"
    } else if t.ends_with(".tar.zst") {
        "--zstd"
    } else if t.ends_with(".tar.gz") || t.ends_with(".tgz") {
        "-z"
    } else if t.ends_with(".tar") {
        "--no-auto-compress"
    } else {
        return Err(format!("'{template}' is not a container template"));
    })
}

/// First-boot settings written into a fresh rootfs.
pub struct Setup<'a> {
    pub hostname: &'a str,
    /// A crypt(3) hash (`openssl passwd -6`), never the password itself.
    pub root_hash: Option<&'a str>,
    pub ssh_keys: &'a str,
}

/// The shell run inside the new rootfs. Only POSIX sh and coreutils/busybox,
/// which every template has; values arrive as positional arguments, never
/// interpolated into the script, so a hostname or key cannot inject.
pub const SETUP_SH: &str = r##"set -e
R="$1"; HOST="$2"; HASH="$3"; KEYS="$4"
printf '%s\n' "$HOST" > "$R/etc/hostname"
if [ -f "$R/etc/hosts" ] && ! grep -q "[[:space:]]$HOST\$" "$R/etc/hosts"; then
  printf '127.0.1.1\t%s\n' "$HOST" >> "$R/etc/hosts"
fi
if [ -n "$HASH" ] && [ -f "$R/etc/shadow" ]; then
  awk -v h="$HASH" 'BEGIN{FS=OFS=":"} $1=="root"{$2=h} {print}' "$R/etc/shadow" > "$R/etc/shadow.vm" \
    && cat "$R/etc/shadow.vm" > "$R/etc/shadow" && rm -f "$R/etc/shadow.vm"
fi
if [ -n "$KEYS" ]; then
  mkdir -p "$R/root/.ssh"; chmod 700 "$R/root/.ssh"
  printf '%s\n' "$KEYS" >> "$R/root/.ssh/authorized_keys"; chmod 600 "$R/root/.ssh/authorized_keys"
fi
# A container has one console and no tty2-6. Alpine's stock inittab respawns a
# getty on each, and every failure lands on the console a person is typing in
# ("can't open /dev/tty2", several a second). Keep only the console's getty.
if [ -f "$R/etc/inittab" ]; then
  sed -i -e 's|^\(tty[1-9]:.*getty.*\)$|#\1|' "$R/etc/inittab" 2>/dev/null \
    || { awk '/^tty[1-9]:.*getty/{print "#" $0; next} {print}' "$R/etc/inittab" > "$R/etc/inittab.vm" \
         && cat "$R/etc/inittab.vm" > "$R/etc/inittab" && rm -f "$R/etc/inittab.vm"; }
fi
# DHCP on eth0, in each family's own way. Templates mostly ship it already;
# this only fills the gap, and never replaces a config that is there.
if [ -d "$R/etc/network" ] && ! grep -qs 'eth0' "$R/etc/network/interfaces"; then
  printf 'auto lo\niface lo inet loopback\n\nauto eth0\niface eth0 inet dhcp\n' > "$R/etc/network/interfaces"
fi
if [ -d "$R/etc/systemd/network" ] && [ ! -d "$R/etc/network" ] && ! ls "$R/etc/systemd/network/" 2>/dev/null | grep -q .; then
  printf '[Match]\nName=eth0\n\n[Network]\nDHCP=yes\n' > "$R/etc/systemd/network/eth0.network"
fi
"##;

/// Create a rootfs image at `image` (already allocated, `size` bytes) from a
/// template: format, mount, unpack as the container's root, apply `setup`.
/// Leaves it mounted at `mount_dir(uuid)`.
pub fn build(
    uuid: &str,
    image: &str,
    template: &str,
    id_base: Option<u32>,
    setup: &Setup,
) -> Result<(), String> {
    let flag = tar_flag(template)?;
    run(Command::new("mkfs.ext4").args(["-q", "-F", image]))?;
    let dir = ensure_mounted(uuid, image)?;
    let d = dir.to_string_lossy().to_string();
    if let Some(b) = id_base {
        // The mount point itself is the container's root directory.
        std::os::unix::fs::chown(&dir, Some(b), Some(b)).map_err(|e| format!("{d}: {e}"))?;
    }
    let lost = dir.join("lost+found");
    let _ = std::fs::remove_dir(&lost);
    in_ns(
        id_base,
        &["tar", "-C", &d, "--numeric-owner", "-xp", flag, "-f", "-"],
        Some(Path::new(template)),
    )?;
    in_ns(
        id_base,
        &[
            "sh",
            "-c",
            SETUP_SH,
            "setup",
            &d,
            setup.hostname,
            setup.root_hash.unwrap_or(""),
            setup.ssh_keys,
        ],
        None,
    )
}

/// A crypt(3) SHA-512 hash of a password, for `/etc/shadow`.
pub fn hash_password(pw: &str) -> Result<String, String> {
    use std::io::Write;
    let mut child = Command::new("openssl")
        .args(["passwd", "-6", "-stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("openssl: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("openssl: no stdin")?
        .write_all(pw.as_bytes())
        .map_err(|e| format!("openssl: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("openssl: {e}"))?;
    let h = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || !h.starts_with("$6$") {
        return Err("could not hash the password".into());
    }
    Ok(h)
}

/// Whether the host can run containers at all, and if not why, in words a
/// person can act on.
pub fn missing_tools() -> Vec<&'static str> {
    [
        "lxc-start",
        "lxc-stop",
        "lxc-info",
        "lxc-console",
        "unshare",
        "mkfs.ext4",
        "tar",
        "mount",
        "openssl",
        "losetup",
        "resize2fs",
    ]
    .into_iter()
    .filter(|t| {
        !std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(t).is_file()))
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostnames_are_checked_and_derived() {
        assert!(check_hostname("web-1").is_ok());
        for bad in ["", "-a", "a-", "a b", "a.b", "a;rm", &"x".repeat(64)] {
            assert!(check_hostname(bad).is_err(), "{bad}");
        }
        assert_eq!(hostname_from("My Web Server!"), "my-web-server");
        assert_eq!(hostname_from("***"), "ct");
        assert!(check_hostname(&hostname_from(&"é".repeat(80))).is_ok());
    }

    #[test]
    fn id_ranges_are_disjoint_and_only_whole_blocks_are_valid() {
        assert_eq!(free_base(std::iter::empty()), ID_BASE);
        assert_eq!(free_base([ID_BASE].into_iter()), ID_BASE + ID_COUNT);
        // A hole left by a deletion is filled first.
        assert_eq!(free_base([ID_BASE + ID_COUNT].into_iter()), ID_BASE);
        assert!(valid_base(ID_BASE) && valid_base(ID_BASE + 3 * ID_COUNT));
        for bad in [0, 1000, 100_000, ID_BASE - ID_COUNT, ID_BASE + 1, u32::MAX] {
            assert!(!valid_base(bad), "{bad}");
        }
    }

    /// Switching a container's mapping re-owns its files by the difference,
    /// relative ids kept, and a symlink is re-owned itself, never followed.
    /// Needs root (chown to arbitrary ids): `sudo -E cargo test --bins -- --ignored reowning`
    #[test]
    #[ignore = "needs root to chown to arbitrary ids"]
    fn reowning_shifts_every_file_and_never_follows_links() {
        use std::os::unix::fs::MetadataExt;
        let r = std::env::temp_dir().join(format!("vm-reown-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        std::fs::create_dir_all(r.join("etc")).unwrap();
        std::fs::write(r.join("etc/shadow"), "x").unwrap();
        let outside = std::env::temp_dir().join(format!("vm-reown-out-{}", std::process::id()));
        std::fs::write(&outside, "x").unwrap();
        std::os::unix::fs::symlink(&outside, r.join("evil")).unwrap();
        let b = ID_BASE;
        for p in [r.clone(), r.join("etc")] {
            std::os::unix::fs::lchown(&p, Some(b), Some(b)).unwrap();
        }
        std::os::unix::fs::lchown(r.join("etc/shadow"), Some(b), Some(b + 42)).unwrap();
        std::os::unix::fs::lchown(r.join("evil"), Some(b), Some(b)).unwrap();
        assert!(reconcile_owner(&r, 0).unwrap(), "privileged: back to 0");
        let m = |p: &Path| std::fs::symlink_metadata(p).unwrap();
        assert_eq!(
            (m(&r).uid(), m(&r.join("etc/shadow")).gid()),
            (0, 42),
            "relative ids kept"
        );
        assert_eq!(m(&r.join("evil")).uid(), 0, "the link itself");
        assert_eq!(
            m(&outside).uid(),
            unsafe { libc::getuid() },
            "its target untouched"
        );
        assert!(
            !reconcile_owner(&r, 0).unwrap(),
            "already right: nothing to do"
        );
        let _ = std::fs::remove_dir_all(&r);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn a_template_is_unpacked_with_the_right_decompressor() {
        assert_eq!(
            tar_flag("/p/debian-13-standard_13.6-1_amd64.tar.zst").unwrap(),
            "--zstd"
        );
        assert_eq!(tar_flag("/p/alpine.tar.xz").unwrap(), "-J");
        assert_eq!(tar_flag("/p/tk.tar.gz").unwrap(), "-z");
        assert!(tar_flag("/p/disk.qcow2").is_err());
    }

    /// The first-boot script against a scratch tree, run as this user: the
    /// hostname, shadow line and keys land, and hostile values stay data.
    #[test]
    fn setup_writes_hostname_password_and_keys_without_injection() {
        let r = std::env::temp_dir().join(format!("vm-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        std::fs::create_dir_all(r.join("etc/network")).unwrap();
        std::fs::write(
            r.join("etc/shadow"),
            "root:*:19000:0:::::\nbin:!:19000::::::\n",
        )
        .unwrap();
        std::fs::write(r.join("etc/hosts"), "127.0.0.1\tlocalhost\n").unwrap();
        std::fs::write(
            r.join("etc/inittab"),
            "::respawn:/sbin/getty 38400 console\ntty1::respawn:/sbin/getty 38400 tty1\ntty2::respawn:/sbin/getty 38400 tty2\n",
        )
        .unwrap();
        let host = "x$(touch pwned)";
        let st = Command::new("sh")
            .args(["-c", SETUP_SH, "setup"])
            .arg(&r)
            .args([host, "$6$salt$hash", "ssh-ed25519 AAAA k"])
            .current_dir(&r)
            .status()
            .unwrap();
        assert!(st.success());
        let rd = |p: &str| std::fs::read_to_string(r.join(p)).unwrap();
        assert_eq!(rd("etc/hostname"), format!("{host}\n"));
        assert!(
            rd("etc/shadow").starts_with("root:$6$salt$hash:19000:"),
            "{}",
            rd("etc/shadow")
        );
        assert!(
            rd("etc/shadow").contains("\nbin:!:"),
            "other lines untouched"
        );
        assert!(rd("root/.ssh/authorized_keys").contains("ssh-ed25519 AAAA k"));
        assert!(rd("etc/network/interfaces").contains("iface eth0 inet dhcp"));
        assert!(
            !r.join("pwned").exists(),
            "the hostname was run as a command"
        );
        let _ = std::fs::remove_dir_all(&r);
    }
}
