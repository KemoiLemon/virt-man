//! What a non-administrator may ask libvirt to define.
//!
//! Domain and network XML reach libvirt as root, so a guest definition is a
//! request to the host: a `<hostdev>` hands over a PCI device, a `<disk>` can
//! name any file on the host (`/etc/shadow`, another user's disk), a
//! `<qemu:commandline>` runs arbitrary arguments, a `<filesystem>` mounts a
//! host directory. Ownership of the guest does not make any of that the
//! user's to ask for.
//!
//! So this is an allowlist, not a blocklist: every element must be one the
//! app's own forms produce, and anything it does not recognise is refused. A
//! new libvirt feature therefore fails closed, and an admin (who is never
//! checked here) still has the whole of libvirt.
//!
//! The allowlist is in step with `web/src/xml.ts` (`domainXml`, `networkXml`,
//! `volumeXml`) and `web/src/hardware.ts` (`deviceXml`): anything those write
//! for a non-admin must pass, and the tests below build their exact output.

use roxmltree::{Document, Node, ParsingOptions};
use std::collections::HashSet;

/// What a checked domain asks for, so the caller can apply quotas and check
/// the named resources against what the user may use.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DomainSpec {
    pub name: String,
    /// The `<uuid>`, if the definition gives one.
    pub uuid: Option<String>,
    pub vcpus: u64,
    pub memory_mb: u64,
    pub networks: Vec<String>,
    pub disks: Vec<DiskSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskSource {
    Path(String),
    Volume { pool: String, volume: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskSpec {
    pub source: DiskSource,
    /// "raw" or "qcow2".
    pub format: String,
    /// A disk the guest can write. Read-only disks and CD-ROMs cannot change
    /// the file, so they may be shared media; a writable one must be the
    /// user's own.
    pub writable: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct NetworkSpec {
    pub name: String,
    pub uuid: Option<String>,
    /// "nat", "bridge" or "none" (isolated).
    pub forward: String,
    pub bridge: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct VolumeSpec {
    pub name: String,
    pub capacity: u64,
    pub format: String,
}

fn parse(xml: &str) -> Result<Document<'_>, String> {
    // No DTD: an entity declaration is a way to smuggle content past a
    // checker that reads the expanded document while libvirt reads another.
    Document::parse_with_options(
        xml,
        ParsingOptions {
            allow_dtd: false,
            ..Default::default()
        },
    )
    .map_err(|e| format!("invalid XML: {e}"))
}

fn elements<'a, 'i>(n: Node<'a, 'i>) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children().filter(Node::is_element)
}

fn refuse(what: &str) -> String {
    format!("{what} is only available to administrators")
}

/// Every attribute on `n` must be in `allowed`.
fn attrs_only(n: Node, allowed: &[&str]) -> Result<(), String> {
    for a in n.attributes() {
        if a.namespace().is_some() || !allowed.contains(&a.name()) {
            return Err(refuse(&format!(
                "attribute '{}' on <{}>",
                a.name(),
                n.tag_name().name()
            )));
        }
    }
    Ok(())
}

/// Audio backends that name nothing on the host.
///
/// libvirt picks the backend to match the display: a guest with only a SPICE
/// `<graphics>` gets `type='spice'` on its `<audio>` (and a VNC-only guest
/// gets `type='none'`, or the host's own sound server on older libvirt).
/// Both are the *default* pairing, not something the user asked for -- and
/// that is the trap: `spice` routes the guest's sound through the SPICE
/// display, which is on loopback and reachable only by the .vv relay, while
/// every other backend here (`pulseaudio`, `pipewire`, `alsa`, `oss`, `sdl`,
/// `dbus`) writes to a sound server on the host. So the allowlist is exactly
/// the display-coupled ones; a host backend is a guest writing to the host
/// and stays an administrator's to ask for.
const AUDIO_TYPES: &[&str] = &["none", "spice"];

/// `n` must have no child elements.
fn leaf(n: Node) -> Result<(), String> {
    match elements(n).next() {
        Some(c) => Err(refuse(&format!(
            "<{}> inside <{}>",
            c.tag_name().name(),
            n.tag_name().name()
        ))),
        None => Ok(()),
    }
}

/// `n` must be an element with only the given attributes and no children.
fn plain(n: Node, allowed: &[&str]) -> Result<(), String> {
    attrs_only(n, allowed)?;
    leaf(n)
}

fn attr<'a>(n: Node<'a, '_>, name: &str) -> &'a str {
    n.attribute(name).unwrap_or("")
}

fn one_of(n: Node, name: &str, allowed: &[&str]) -> Result<(), String> {
    let v = attr(n, name);
    if allowed.contains(&v) {
        Ok(())
    } else {
        Err(refuse(&format!(
            "{name}='{v}' on <{}>",
            n.tag_name().name()
        )))
    }
}

fn text(n: Node) -> String {
    n.text().unwrap_or("").trim().to_string()
}

fn number(n: Node) -> Result<u64, String> {
    text(n)
        .parse()
        .map_err(|_| format!("<{}> must be a number", n.tag_name().name()))
}

/// KiB, MiB or GiB (libvirt's default unit is KiB) to MiB.
fn to_mb(n: Node) -> Result<u64, String> {
    let v = number(n)?;
    Ok(match attr(n, "unit") {
        "" | "KiB" | "k" | "K" => v / 1024,
        "MiB" | "M" | "m" => v,
        "GiB" | "G" | "g" => v * 1024,
        u => return Err(format!("unsupported memory unit '{u}'")),
    })
}

/// Pages libvirt adds on its own when it reads a definition back: the UUID,
/// the resolved memory figures, and the like. A user re-saving what they
/// were shown must not be refused for them.
const DOMAIN_TOP: &[&str] = &[
    "name",
    "title",
    "description",
    "uuid",
    "memory",
    "currentMemory",
    "vcpu",
    "os",
    "features",
    "cpu",
    "clock",
    "on_poweroff",
    "on_reboot",
    "on_crash",
    "pm",
    "devices",
    "resource",
];

/// Fixed paths the forms use for UEFI. The loader is read-only firmware
/// and the NVRAM template is copied from, so naming another file would
/// either boot the guest from it or copy it into a place the user controls.
const FIRMWARE: &[&str] = &[
    "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
    "/usr/share/OVMF/OVMF_CODE_4M.fd",
    "/usr/share/OVMF/OVMF_CODE.fd",
];
const NVRAM_TEMPLATES: &[&str] = &[
    "/usr/share/edk2/x64/OVMF_VARS.4m.fd",
    "/usr/share/OVMF/OVMF_VARS_4M.fd",
    "/usr/share/OVMF/OVMF_VARS.fd",
];
const NVRAM_DIR: &str = "/var/lib/libvirt/qemu/nvram/";

fn check_os(os: Node, name: &str) -> Result<(), String> {
    // `firmware='efi'` is libvirt's autoselection; it picks from the host's
    // own firmware descriptors, so it names no file.
    attrs_only(os, &["firmware"])?;
    for c in elements(os) {
        match c.tag_name().name() {
            "type" => {
                plain(c, &["arch", "machine"])?;
                if text(c) != "hvm" {
                    return Err(refuse("a non-hvm guest"));
                }
            }
            "boot" => {
                plain(c, &["dev"])?;
                one_of(c, "dev", &["hd", "cdrom", "network", "fd"])?;
            }
            "bootmenu" => plain(c, &["enable", "timeout"])?,
            // Which autoselected firmware features to require.
            "firmware" => {
                attrs_only(c, &[])?;
                for f in elements(c) {
                    if f.tag_name().name() != "feature" {
                        return Err(refuse("that firmware setting"));
                    }
                    plain(f, &["enabled", "name"])?;
                }
            }
            "loader" => {
                plain(c, &["readonly", "type", "format", "secure"])?;
                if !FIRMWARE.contains(&text(c).as_str()) {
                    return Err(refuse("a custom firmware file"));
                }
            }
            "nvram" => {
                plain(c, &["template", "templateFormat", "format"])?;
                let t = attr(c, "template");
                if !t.is_empty() && !NVRAM_TEMPLATES.contains(&t) {
                    return Err(refuse("a custom NVRAM template"));
                }
                // The guest's own variable store, named for the guest, so
                // it cannot overwrite another guest's.
                if text(c) != format!("{NVRAM_DIR}{name}_VARS.fd") {
                    return Err(format!("the NVRAM file must be {NVRAM_DIR}{name}_VARS.fd"));
                }
            }
            other => return Err(refuse(&format!("<{other}> in <os>"))),
        }
    }
    Ok(())
}

/// Leaf-only subtrees whose contents cannot reach outside the guest.
fn harmless_subtree(n: Node) -> Result<(), String> {
    for d in n.descendants().filter(Node::is_element) {
        if d.tag_name().namespace().is_some() {
            return Err(refuse("a namespaced element"));
        }
        for a in d.attributes() {
            if a.namespace().is_some() {
                return Err(refuse("a namespaced attribute"));
            }
        }
    }
    Ok(())
}

fn check_disk(d: Node, spec: &mut DomainSpec) -> Result<(), String> {
    attrs_only(d, &["type", "device"])?;
    one_of(d, "device", &["disk", "cdrom"])?;
    let kind = attr(d, "type");
    let mut format = String::new();
    let mut source = None;
    let mut readonly = attr(d, "device") == "cdrom";
    for c in elements(d) {
        match c.tag_name().name() {
            "driver" => {
                plain(c, &["name", "type", "cache", "discard", "io"])?;
                one_of(c, "name", &["qemu"])?;
                one_of(c, "type", &["raw", "qcow2"])?;
                format = attr(c, "type").to_string();
            }
            "source" => {
                source = Some(c);
                leaf(c)?;
            }
            "target" => {
                plain(c, &["dev", "bus"])?;
                one_of(c, "bus", &["virtio", "sata", "scsi"])?;
            }
            "readonly" => {
                plain(c, &[])?;
                readonly = true;
            }
            "shareable" => plain(c, &[])?,
            "boot" => plain(c, &["order"])?,
            "alias" => plain(c, &["name"])?,
            "address" => plain(
                c,
                &[
                    "type",
                    "controller",
                    "bus",
                    "target",
                    "unit",
                    "domain",
                    "slot",
                    "function",
                ],
            )?,
            other => return Err(refuse(&format!("<{other}> in a disk"))),
        }
    }
    if format.is_empty() {
        // Without a driver type qemu probes the format, and a file that
        // claims to be qcow2 can then name any host file as its backing.
        return Err("a disk must declare its format (<driver type=...>)".into());
    }
    let Some(src) = source else {
        // An empty CD-ROM drive.
        return if attr(d, "device") == "cdrom" {
            Ok(())
        } else {
            Err("a disk needs a source".into())
        };
    };
    let source = match kind {
        "file" => {
            attrs_only(src, &["file"])?;
            DiskSource::Path(attr(src, "file").to_string())
        }
        "volume" => {
            attrs_only(src, &["pool", "volume"])?;
            DiskSource::Volume {
                pool: attr(src, "pool").to_string(),
                volume: attr(src, "volume").to_string(),
            }
        }
        other => return Err(refuse(&format!("a '{other}' disk"))),
    };
    spec.disks.push(DiskSpec {
        source,
        format,
        writable: !readonly,
    });
    Ok(())
}

fn check_interface(i: Node, spec: &mut DomainSpec) -> Result<(), String> {
    attrs_only(i, &["type"])?;
    // Only libvirt networks: a `bridge` or `direct` interface would plug the
    // guest into a host device without going through a network the user may
    // use. Bridge access is granted through a network that uses the bridge.
    one_of(i, "type", &["network"])?;
    let mut net = None;
    for c in elements(i) {
        match c.tag_name().name() {
            "source" => {
                // Libvirt adds `bridge` (the resolved device) to a running
                // guest's live XML; it is informational, the network decides.
                plain(c, &["network", "bridge", "portid"])?;
                net = Some(attr(c, "network").to_string());
            }
            "model" => plain(c, &["type"])?,
            "mac" => plain(c, &["address"])?,
            "target" => plain(c, &["dev"])?,
            "alias" => plain(c, &["name"])?,
            "link" => plain(c, &["state"])?,
            "address" => plain(c, &["type", "domain", "bus", "slot", "function"])?,
            "bandwidth" => {
                // Set by the daemon's rate-limit endpoint, and carried through
                // the hardware editor verbatim. Changing it here is the same
                // as changing it there.
                attrs_only(c, &[])?;
                for b in elements(c) {
                    if !matches!(b.tag_name().name(), "inbound" | "outbound") {
                        return Err(refuse("that bandwidth setting"));
                    }
                    plain(b, &["average", "peak", "burst", "floor"])?;
                }
            }
            other => return Err(refuse(&format!("<{other}> in an interface"))),
        }
    }
    // A NIC may name no network. libvirt's schema requires a `<source>`, not a
    // named one, so `<source network=""/>` is how a NIC attaches to nothing --
    // and it grants access to no network, so it is not a hole: every non-empty
    // source below is still checked against the caller's privileges. Without
    // this branch the hardware editor's "None" would work for an admin and be
    // refused for everyone else, and a non-admin could never clear a NIC.
    match net {
        Some(n) if !n.is_empty() => spec.networks.push(n),
        // Absent (`<source/>`) and empty both mean no network.
        _ => {}
    }
    Ok(())
}

fn check_devices(devs: Node, spec: &mut DomainSpec) -> Result<(), String> {
    attrs_only(devs, &[])?;
    for d in elements(devs) {
        match d.tag_name().name() {
            // Whatever the path, libvirt runs its own configured emulator
            // check; still, only the stock one.
            "emulator" => {
                leaf(d)?;
                if !matches!(
                    text(d).as_str(),
                    "/usr/bin/qemu-system-x86_64" | "/usr/bin/kvm" | "/usr/libexec/qemu-kvm"
                ) {
                    return Err(refuse("a custom emulator"));
                }
            }
            "disk" => check_disk(d, spec)?,
            "interface" => check_interface(d, spec)?,
            "graphics" => {
                attrs_only(d, &["type", "port", "autoport", "listen", "passwd"])?;
                one_of(d, "type", &["vnc", "spice"])?;
                // Loopback only: the display has no auth of its own, and the
                // web console and the .vv relay both reach it on the host.
                if !matches!(attr(d, "listen"), "" | "127.0.0.1" | "::1" | "localhost") {
                    return Err("a guest display must listen on 127.0.0.1".into());
                }
                for c in elements(d) {
                    match c.tag_name().name() {
                        "listen" => {
                            plain(c, &["type", "address"])?;
                            one_of(c, "type", &["address"])?;
                            if !matches!(attr(c, "address"), "" | "127.0.0.1" | "::1") {
                                return Err("a guest display must listen on 127.0.0.1".into());
                            }
                        }
                        "image" | "gl" => plain(c, &["compression", "enable"])?,
                        other => return Err(refuse(&format!("<{other}> in graphics"))),
                    }
                }
            }
            // `<audio>` is a property of the display, and libvirt rewrites
            // it whenever a `<graphics>` is added, removed or retyped: saving
            // hardware for a guest with no display of its own (the SPA used
            // to write one, and a guest defined by hand may have none) makes
            // libvirt *add* a `<graphics type='spice'>` plus a matching
            // `<audio type='spice'>`. Refusing `spice` therefore meant a
            // non-admin could never save such a guest at all, and the refusal
            // named an attribute the user never chose. The allowlist carries
            // the pairing, so the round trip works; the host's own sound
            // servers do not.
            "audio" => {
                plain(d, &["id", "type"])?;
                one_of(d, "type", AUDIO_TYPES)?;
            }
            // Guest-side emulated hardware. Each is held to the types that
            // name nothing on the host: an `evdev` or `passthrough` input
            // reads a host input device, and no host path or source may
            // appear anywhere beneath.
            "input" => {
                plain(d, &["type", "bus"])?;
                one_of(d, "type", &["mouse", "keyboard", "tablet"])?;
            }
            // `sound` is the pre-1.10 spelling of `<audio>`; the same rule.
            "sound" => {
                attrs_only(d, &["model"])?;
                one_of(d, "model", AUDIO_TYPES)?;
                harmless_subtree(d)?;
            }
            "video" | "watchdog" | "memballoon" | "controller" => {
                harmless_subtree(d)?;
                let host_ref = d.descendants().any(|x| {
                    matches!(x.tag_name().name(), "source" | "backend" | "driver")
                        || x.attributes().any(|a| a.value().starts_with('/'))
                });
                if host_ref {
                    return Err(refuse("a device that names something on the host"));
                }
            }
            "serial" | "console" => {
                attrs_only(d, &["type"])?;
                one_of(d, "type", &["pty"])?;
                for c in elements(d) {
                    match c.tag_name().name() {
                        "target" => {
                            attrs_only(c, &["port", "type"])?;
                            for m in elements(c) {
                                if m.tag_name().name() != "model" {
                                    return Err(refuse("that serial target"));
                                }
                                plain(m, &["name"])?;
                            }
                        }
                        "alias" => plain(c, &["name"])?,
                        other => return Err(refuse(&format!("<{other}> in a serial port"))),
                    }
                }
            }
            "channel" => {
                // The guest agent's socket; libvirt picks its path.
                attrs_only(d, &["type"])?;
                one_of(d, "type", &["unix"])?;
                for c in elements(d) {
                    match c.tag_name().name() {
                        "target" => {
                            plain(c, &["type", "name", "state"])?;
                            one_of(c, "name", &["org.qemu.guest_agent.0"])?;
                        }
                        "source" => plain(c, &["mode"])?,
                        "alias" => plain(c, &["name"])?,
                        "address" => plain(c, &["type", "controller", "bus", "port"])?,
                        other => return Err(refuse(&format!("<{other}> in a channel"))),
                    }
                }
            }
            "rng" => {
                attrs_only(d, &["model"])?;
                for c in elements(d) {
                    match c.tag_name().name() {
                        "backend" => {
                            plain(c, &["model"])?;
                            one_of(c, "model", &["random"])?;
                            if !matches!(text(c).as_str(), "/dev/urandom" | "/dev/random") {
                                return Err(refuse("a custom entropy source"));
                            }
                        }
                        "alias" | "address" => harmless_subtree(c)?,
                        other => return Err(refuse(&format!("<{other}> in rng"))),
                    }
                }
            }
            "tpm" => {
                attrs_only(d, &["model"])?;
                for c in elements(d) {
                    match c.tag_name().name() {
                        // An emulated TPM; `passthrough` would hand over the
                        // host's own.
                        "backend" => {
                            attrs_only(c, &["type", "version"])?;
                            one_of(c, "type", &["emulator"])?;
                            leaf(c)?;
                        }
                        "alias" => plain(c, &["name"])?,
                        other => return Err(refuse(&format!("<{other}> in a TPM"))),
                    }
                }
            }
            other => return Err(refuse(&format!("<{other}>"))),
        }
    }
    Ok(())
}

pub fn check_domain(xml: &str) -> Result<DomainSpec, String> {
    let doc = parse(xml)?;
    let root = doc.root_element();
    if root.tag_name().name() != "domain" || root.tag_name().namespace().is_some() {
        return Err("not a domain definition".into());
    }
    attrs_only(root, &["type", "id"])?;
    one_of(root, "type", &["kvm", "qemu"])?;
    let mut spec = DomainSpec::default();
    let mut seen = HashSet::new();
    for c in elements(root) {
        let tag = c.tag_name().name();
        if c.tag_name().namespace().is_some() || !DOMAIN_TOP.contains(&tag) {
            // Includes <qemu:commandline>, <seclabel>, <sysinfo>, <iothreads>
            // pinned to host CPUs, <memoryBacking> with a host path, ...
            return Err(refuse(&format!("<{tag}>")));
        }
        if !seen.insert(tag) {
            return Err(format!("<{tag}> appears twice"));
        }
        match tag {
            "name" => spec.name = text(c),
            "uuid" => {
                leaf(c)?;
                spec.uuid = Some(text(c));
            }
            "memory" => spec.memory_mb = to_mb(c)?,
            "currentMemory" => {
                attrs_only(c, &["unit"])?;
            }
            "vcpu" => {
                attrs_only(c, &["placement", "current"])?;
                spec.vcpus = number(c)?;
            }
            "os" => {} // checked once the name is known
            "devices" => check_devices(c, &mut spec)?,
            "features" | "pm" | "clock" | "cpu" => {
                harmless_subtree(c)?;
                // `cpu mode='host-passthrough'` exposes the host model, which
                // is harmless; pinning (`cputune`) is not in this list.
            }
            "resource" => {
                harmless_subtree(c)?;
            }
            _ => leaf(c)?,
        }
    }
    if spec.name.is_empty() {
        return Err("the guest has no name".into());
    }
    uuid_name_agrees(&spec.name, spec.uuid.as_deref(), "guest")?;
    if let Some(os) = elements(root).find(|c| c.tag_name().name() == "os") {
        check_os(os, &spec.name)?;
    }
    if spec.vcpus == 0 || spec.memory_mb == 0 {
        return Err("a guest needs <vcpu> and <memory>".into());
    }
    Ok(spec)
}

/// What a checked container definition asks for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ContainerSpec {
    pub name: String,
    pub uuid: Option<String>,
    pub vcpus: u64,
    pub memory_mb: u64,
    pub networks: Vec<String>,
    /// No `<idmap>`: the container's root is the host's root. Allowed only
    /// with `CT.Privileged` -- the caller checks that, this only reports it.
    pub privileged: bool,
    /// Every `<uid>`/`<gid>` entry as (tag, start, target, count), so a
    /// change to the mapping can be told apart from any other edit.
    pub idmap: Vec<[String; 4]>,
}

/// A container definition, as a non-admin may redefine it: the shape the
/// daemon writes (`daemon/src/ct.rs` `domain_xml`) plus what libvirt adds on
/// read-back. The daemon separately insists the rootfs and its mount are
/// unchanged; here the point is that nothing else reaches the host: no
/// second `<filesystem>` (a bind mount of `/` is an escape), no `<hostdev>`,
/// no `<features><capabilities>` (which grants the container host
/// capabilities), no `<seclabel>` changes.
pub fn check_container(xml: &str) -> Result<ContainerSpec, String> {
    let doc = parse(xml)?;
    let root = doc.root_element();
    if root.tag_name().name() != "domain" || root.tag_name().namespace().is_some() {
        return Err("not a domain definition".into());
    }
    attrs_only(root, &["type", "id"])?;
    one_of(root, "type", &["lxc"])?;
    let mut spec = ContainerSpec {
        privileged: true,
        ..Default::default()
    };
    let mut seen = HashSet::new();
    let mut vm = DomainSpec::default();
    for c in elements(root) {
        let tag = c.tag_name().name();
        if c.tag_name().namespace().is_some() {
            return Err(refuse(&format!("<{tag}>")));
        }
        if !seen.insert(tag) {
            return Err(format!("<{tag}> appears twice"));
        }
        match tag {
            "name" => spec.name = text(c),
            "uuid" => {
                leaf(c)?;
                spec.uuid = Some(text(c));
            }
            "title" | "description" => leaf(c)?,
            // The daemon's own record of the rootfs; it checks it is unchanged.
            "metadata" => {
                for m in elements(c) {
                    if m.tag_name().name() != "container"
                        || m.tag_name().namespace() != Some("https://virt-man.invalid/container/1")
                    {
                        return Err(refuse("that metadata"));
                    }
                }
            }
            "memory" => spec.memory_mb = to_mb(c)?,
            "currentMemory" => attrs_only(c, &["unit"])?,
            "memtune" => {
                for m in elements(c) {
                    if m.tag_name().name() != "swap_hard_limit" {
                        return Err(refuse(&format!("<{}> in memtune", m.tag_name().name())));
                    }
                    plain(m, &["unit"])?;
                }
            }
            "vcpu" => {
                attrs_only(c, &["placement", "current"])?;
                spec.vcpus = number(c)?;
            }
            "resource" | "clock" => harmless_subtree(c)?,
            "on_poweroff" | "on_reboot" | "on_crash" => leaf(c)?,
            // Only `privnet` (a private network namespace). `capabilities`
            // hands the container host capabilities.
            "features" => {
                attrs_only(c, &[])?;
                for f in elements(c) {
                    if f.tag_name().name() != "privnet" {
                        return Err(refuse(&format!("<{}> in <features>", f.tag_name().name())));
                    }
                    plain(f, &[])?;
                }
            }
            "os" => {
                attrs_only(c, &[])?;
                for o in elements(c) {
                    match o.tag_name().name() {
                        "type" => plain(o, &["arch"])?,
                        // The container's own init, inside its rootfs.
                        "init" => leaf(o)?,
                        other => return Err(refuse(&format!("<{other}> in <os>"))),
                    }
                }
            }
            "idmap" => {
                attrs_only(c, &[])?;
                for m in elements(c) {
                    if !matches!(m.tag_name().name(), "uid" | "gid") {
                        return Err(refuse("that idmap entry"));
                    }
                    plain(m, &["start", "target", "count"])?;
                    spec.idmap.push([
                        m.tag_name().name().to_string(),
                        attr(m, "start").to_string(),
                        attr(m, "target").to_string(),
                        attr(m, "count").to_string(),
                    ]);
                }
                spec.privileged = spec.idmap.is_empty();
            }
            "devices" => {
                attrs_only(c, &[])?;
                let mut fs = 0;
                for d in elements(c) {
                    match d.tag_name().name() {
                        "emulator" => leaf(d)?,
                        // Exactly one, the rootfs; the daemon checks it is
                        // the same directory it mounted.
                        "filesystem" => {
                            fs += 1;
                            harmless_subtree(d)?;
                        }
                        "interface" => check_interface(d, &mut vm)?,
                        "console" => {
                            plain_or_target(d)?;
                        }
                        other => return Err(refuse(&format!("<{other}> in a container"))),
                    }
                }
                if fs != 1 {
                    return Err(refuse("a filesystem besides the container's rootfs"));
                }
            }
            other => return Err(refuse(&format!("<{other}> in a container"))),
        }
    }
    if spec.name.is_empty() {
        return Err("the container has no name".into());
    }
    uuid_name_agrees(&spec.name, spec.uuid.as_deref(), "container")?;
    if spec.vcpus == 0 || spec.memory_mb == 0 {
        return Err("a container needs <vcpu> and <memory>".into());
    }
    spec.networks = vm.networks;
    Ok(spec)
}

/// A pty console, with the target libvirt adds.
fn plain_or_target(d: Node) -> Result<(), String> {
    attrs_only(d, &["type"])?;
    one_of(d, "type", &["pty"])?;
    for c in elements(d) {
        match c.tag_name().name() {
            "target" => plain(c, &["type", "port"])?,
            "alias" => plain(c, &["name"])?,
            other => return Err(refuse(&format!("<{other}> in a console"))),
        }
    }
    Ok(())
}

pub fn check_network(xml: &str) -> Result<NetworkSpec, String> {
    let doc = parse(xml)?;
    let root = doc.root_element();
    if root.tag_name().name() != "network" || root.tag_name().namespace().is_some() {
        return Err("not a network definition".into());
    }
    attrs_only(root, &["connections"])?;
    let mut spec = NetworkSpec {
        forward: "none".into(),
        ..Default::default()
    };
    for c in elements(root) {
        match c.tag_name().name() {
            "name" => spec.name = text(c),
            "uuid" => {
                leaf(c)?;
                spec.uuid = Some(text(c));
            }
            // The display name of a network named by its UUID.
            "title" => leaf(c)?,
            "forward" => {
                attrs_only(c, &["mode", "dev"])?;
                // `route` and `open` put the network straight onto the host's
                // routing; `hostdev`, `passthrough` and friends hand over NICs.
                one_of(c, "mode", &["nat", "bridge"])?;
                spec.forward = attr(c, "mode").to_string();
                if c.attribute("dev").is_some() {
                    return Err(refuse("choosing the forward device"));
                }
                for n in elements(c) {
                    if n.tag_name().name() != "nat" {
                        return Err(refuse(&format!("<{}> in <forward>", n.tag_name().name())));
                    }
                    attrs_only(n, &[])?;
                    for p in elements(n) {
                        if p.tag_name().name() != "port" {
                            return Err(refuse("that NAT setting"));
                        }
                        plain(p, &["start", "end"])?;
                    }
                }
            }
            "bridge" => {
                plain(c, &["name", "stp", "delay"])?;
                let b = attr(c, "name");
                if !b.is_empty() {
                    spec.bridge = Some(b.to_string());
                }
            }
            "mac" => plain(c, &["address"])?,
            "domain" => plain(c, &["name", "localOnly"])?,
            "dns" => {
                attrs_only(c, &["enable"])?;
                for f in elements(c) {
                    if f.tag_name().name() != "forwarder" {
                        return Err(refuse(&format!("<{}> in <dns>", f.tag_name().name())));
                    }
                    plain(f, &["addr"])?;
                }
            }
            "ip" => {
                attrs_only(c, &["address", "netmask", "prefix", "family"])?;
                for d in elements(c) {
                    if d.tag_name().name() != "dhcp" {
                        return Err(refuse(&format!("<{}> in <ip>", d.tag_name().name())));
                    }
                    attrs_only(d, &[])?;
                    for r in elements(d) {
                        match r.tag_name().name() {
                            "range" => plain(r, &["start", "end"])?,
                            "host" => plain(r, &["mac", "ip", "name"])?,
                            other => return Err(refuse(&format!("<{other}> in <dhcp>"))),
                        }
                    }
                }
            }
            other => return Err(refuse(&format!("<{other}> in a network"))),
        }
    }
    if spec.name.is_empty() {
        return Err("the network has no name".into());
    }
    uuid_name_agrees(&spec.name, spec.uuid.as_deref(), "network")?;
    if spec.forward == "bridge" && spec.bridge.is_none() {
        return Err("a bridge network must name its bridge".into());
    }
    Ok(spec)
}

/// A resource named like a UUID must be named by *its own* UUID.
///
/// Guests and networks are named by their UUID now, and the UUID is what the
/// web server's records and a URL rely on. A name that merely looks like one
/// -- someone else's UUID, say -- would make a link, a grant or an ownership
/// record by UUID point at the wrong thing. So a UUID-shaped name is only
/// accepted with a matching `<uuid>`, and one without a `<uuid>` is refused
/// (libvirt would give it a different, random one).
fn uuid_name_agrees(name: &str, uuid: Option<&str>, what: &str) -> Result<(), String> {
    if !crate::guard::is_uuid(name) {
        return Ok(());
    }
    match uuid {
        Some(u) if u.eq_ignore_ascii_case(name) => Ok(()),
        _ => Err(format!(
            "a {what} named like a UUID must be named by its own <uuid>"
        )),
    }
}

pub fn check_volume(xml: &str) -> Result<VolumeSpec, String> {
    let doc = parse(xml)?;
    let root = doc.root_element();
    if root.tag_name().name() != "volume" || root.tag_name().namespace().is_some() {
        return Err("not a volume definition".into());
    }
    attrs_only(root, &["type"])?;
    let mut spec = VolumeSpec::default();
    for c in elements(root) {
        match c.tag_name().name() {
            "name" => spec.name = text(c),
            "capacity" => {
                spec.capacity = match attr(c, "unit") {
                    "" | "bytes" | "B" => number(c)?,
                    "KiB" | "K" => number(c)? << 10,
                    "MiB" | "M" => number(c)? << 20,
                    "GiB" | "G" => number(c)? << 30,
                    u => return Err(format!("unsupported capacity unit '{u}'")),
                };
                attrs_only(c, &["unit"])?;
            }
            "allocation" => plain(c, &["unit"])?,
            "target" => {
                // No <path> or <permissions>: the pool decides where the file
                // goes, and a <backingStore> would read another file.
                attrs_only(c, &[])?;
                for t in elements(c) {
                    if t.tag_name().name() != "format" {
                        return Err(refuse(&format!(
                            "<{}> in a volume target",
                            t.tag_name().name()
                        )));
                    }
                    plain(t, &["type"])?;
                    one_of(t, "type", &["raw", "qcow2"])?;
                    spec.format = attr(t, "type").to_string();
                }
            }
            other => return Err(refuse(&format!("<{other}> in a volume"))),
        }
    }
    if spec.name.is_empty() || spec.name.contains('/') {
        return Err("a volume needs a plain name".into());
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `domainXml` in web/src/xml.ts writes for a UEFI guest with a new
    /// disk, an ISO and one NIC.
    fn form_domain() -> String {
        let ports: String = (1..=14)
            .map(|i| format!(r#"<controller type="pci" index="{i}" model="pcie-root-port"/>"#))
            .collect();
        format!(
            r#"<domain type="kvm">
  <name>web</name>
  <memory unit="KiB">2097152</memory>
  <vcpu>2</vcpu>
  <os>
    <type arch="x86_64" machine="q35">hvm</type>
    <loader readonly="yes" type="pflash" format="raw">/usr/share/edk2/x64/OVMF_CODE.4m.fd</loader>
    <nvram template="/usr/share/edk2/x64/OVMF_VARS.4m.fd" format="raw">/var/lib/libvirt/qemu/nvram/web_VARS.fd</nvram>
  </os>
  <features><acpi/><apic/></features>
  <devices>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    <disk type="file" device="disk">
      <driver name="qemu" type="qcow2"/>
      <source file="/var/lib/libvirt/images/web.qcow2"/>
      <target dev="vda" bus="virtio"/>
    </disk>
    <disk type="volume" device="cdrom">
      <driver name="qemu" type="raw"/>
      <source pool="ISOs" volume="debian.iso"/>
      <target dev="sda" bus="sata"/>
      <readonly/>
    </disk>
    <interface type="network">
      <source network="default"/>
      <model type="virtio"/>
      <bandwidth><inbound average="12800"/></bandwidth>
    </interface>
    <graphics type="vnc" port="-1" autoport="yes" listen="127.0.0.1"/>
    <video><model type="virtio"/></video>
    <serial type="pty"><target port="0"/></serial>
    <console type="pty"><target type="serial" port="0"/></console>
    <channel type="unix"><target type="virtio" name="org.qemu.guest_agent.0"/></channel>
    <rng model="virtio"><backend model="random">/dev/urandom</backend></rng>
    <tpm model="tpm-crb"><backend type="emulator" version="2.0"/></tpm>
    <memballoon model="virtio"/>
    <watchdog model="i6300esb" action="reset"/>
    <controller type="pci" index="0" model="pcie-root"/>
    {ports}
  </devices>
</domain>"#
        )
    }

    #[test]
    fn what_the_forms_write_passes_and_is_measured() {
        let s = check_domain(&form_domain()).unwrap();
        assert_eq!(s.name, "web");
        assert_eq!((s.vcpus, s.memory_mb), (2, 2048));
        assert_eq!(s.networks, ["default"]);
        assert_eq!(
            s.disks,
            [
                DiskSpec {
                    source: DiskSource::Path("/var/lib/libvirt/images/web.qcow2".into()),
                    format: "qcow2".into(),
                    writable: true,
                },
                DiskSpec {
                    source: DiskSource::Volume {
                        pool: "ISOs".into(),
                        volume: "debian.iso".into()
                    },
                    format: "raw".into(),
                    writable: false,
                },
            ]
        );
    }

    fn with_device(dev: &str) -> String {
        form_domain().replace("<memballoon model=\"virtio\"/>", dev)
    }

    /// What libvirt hands back for guests defined through the forms, verbatim
    /// from a real host (a UEFI guest, and a BIOS one read back after
    /// creation). The hardware editor and the XML tab start from exactly this,
    /// so it must pass as it is, or a non-admin could never save a change.
    #[test]
    fn libvirts_own_read_back_passes() {
        for (file, name) in [
            (include_str!("testdata/libvirt-uefi.xml"), "web"),
            (include_str!("testdata/libvirt-readback.xml"), "bobvm"),
        ] {
            let s = check_domain(file).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(s.name, name);
        }
    }

    #[test]
    fn the_hosts_sound_server_is_refused_but_the_display_pairing_is_not() {
        // What libvirt writes on its own for a SPICE guest: it must pass, or
        // a non-admin cannot re-save a guest libvirt has touched. The old
        // refusal named `type='spice'` on `<audio>` and blocked exactly this.
        for good in [
            r#"<audio id="1" type="none"/>"#,
            r#"<audio id="1" type="spice"/><graphics type="spice" port="5901" autoport="yes" listen="127.0.0.1"/>"#,
            r#"<sound model="spice"/><graphics type="spice" port="5901" autoport="yes" listen="127.0.0.1"/>"#,
        ] {
            assert!(check_domain(&with_device(good)).is_ok(), "{good}");
        }
        for (bad, why) in [
            (r#"<audio id="1" type="pulseaudio"/>"#, "host pulse server"),
            (r#"<audio id="1" type="pipewire"/>"#, "host pipewire server"),
            (r#"<audio id="1" type="alsa"/>"#, "host ALSA"),
            (r#"<sound model="es1370"/>"#, "the old spelling"),
        ] {
            assert!(check_domain(&with_device(bad)).is_err(), "{why} allowed");
        }
    }

    /// The real thing: define a guest through the app's own form XML, let
    /// libvirt read it back, and re-save that. Uses a stub so it fails if the
    /// pairing ever stops round-tripping.
    #[test]
    fn a_spice_guest_read_back_can_be_saved_by_a_non_admin() {
        let vnc = form_domain();
        let spice = vnc.replace(
            r#"<graphics type="vnc" port="-1" autoport="yes" listen="127.0.0.1"/>"#,
            r#"<graphics type="spice" port="5901" autoport="yes" listen="127.0.0.1"/>
    <audio id="1" type="spice"/>"#,
        );
        assert_ne!(vnc, spice, "the form XML changed shape");
        assert!(
            check_domain(&spice).is_ok(),
            "libvirt's read-back of a SPICE guest"
        );
    }


    /// Live verification helper: `cargo test -- --ignored live_audio <file>`
    /// feeds a real read-back XML with the audio type swapped for each kind
    /// named in `VIRT_MAN_AUDIO_TYPES`, so the verdicts are the checker's and
    /// not a test's own idea of them.
    #[test]
    #[ignore]
    fn live_audio_verdicts() {
        let path = std::env::var("VIRT_MAN_AUDIO_XML").unwrap();
        let xml = std::fs::read_to_string(&path).unwrap();
        assert!(xml.contains("<audio"), "the file has no <audio> to swap");
        for kind in ["none", "spice", "dbus", "pipewire", "alsa", "pulseaudio"] {
            let cur = if xml.contains("<audio id='1' type='spice'/>") {
                "<audio id='1' type='spice'/>"
            } else {
                "<audio id='1' type='none'/>"
            };
            let probe = xml.replace(cur, &format!("<audio id='1' type='{kind}'/>"));
            assert!(
                probe != xml || kind == "spice",
                "nothing was swapped: no {cur} in {path}"
            );
            println!("audio type={kind:<11} -> {:?}", check_domain(&probe).map(|s| s.name));
        }
    }

    #[test]
    fn host_access_is_refused() {
        for (dev, why) in [
            (
                r#"<hostdev mode="subsystem" type="pci" managed="yes"><source><address domain="0x0000" bus="0x01" slot="0x00" function="0x0"/></source></hostdev>"#,
                "PCI passthrough",
            ),
            (
                r#"<hostdev mode="subsystem" type="usb" managed="yes"><source><vendor id="0x1"/><product id="0x2"/></source></hostdev>"#,
                "USB passthrough",
            ),
            (
                r#"<filesystem type="mount"><source dir="/"/><target dir="root"/></filesystem>"#,
                "host directory",
            ),
            (
                r#"<disk type="block" device="disk"><driver name="qemu" type="raw"/><source dev="/dev/sda"/><target dev="vdb"/></disk>"#,
                "block device",
            ),
            (
                r#"<disk type="network" device="disk"><driver name="qemu" type="raw"/><source protocol="nbd" name="x"/><target dev="vdb"/></disk>"#,
                "network disk",
            ),
            (
                r#"<interface type="bridge"><source bridge="br0"/></interface>"#,
                "bridge NIC",
            ),
            (
                r#"<interface type="direct"><source dev="eth0" mode="bridge"/></interface>"#,
                "macvtap",
            ),
            (
                r#"<serial type="file"><source path="/etc/shadow"/><target port="1"/></serial>"#,
                "host file serial",
            ),
            (
                r#"<channel type="unix"><source mode="bind" path="/run/x"/><target type="virtio" name="evil"/></channel>"#,
                "named socket",
            ),
            (
                r#"<rng model="virtio"><backend model="random">/dev/sda</backend></rng>"#,
                "rng from a disk",
            ),
            (
                r#"<tpm model="tpm-tis"><backend type="passthrough"><device path="/dev/tpm0"/></backend></tpm>"#,
                "host TPM",
            ),
            (
                r#"<graphics type="vnc" port="5900" listen="0.0.0.0"/>"#,
                "public display",
            ),
            (
                r#"<disk type="file" device="disk"><source file="/x.img"/><target dev="vdb"/></disk>"#,
                "unformatted disk",
            ),
            (
                r#"<disk type="file" device="disk"><driver name="qemu" type="qcow2"/><source file="/x"><seclabel relabel="no"/></source><target dev="vdb"/></disk>"#,
                "seclabel",
            ),
            (
                r#"<smartcard mode="passthrough" type="tcp"/>"#,
                "unknown device",
            ),
        ] {
            assert!(check_domain(&with_device(dev)).is_err(), "{why} allowed");
        }
    }

    #[test]
    fn top_level_escapes_are_refused() {
        let base = form_domain();
        for (bad, why) in [
            (
                base.replace("<domain type=\"kvm\">", "<domain type=\"kvm\" xmlns:qemu=\"http://libvirt.org/schemas/domain/qemu/1.0\">")
                    .replace("</devices>", "</devices><qemu:commandline><qemu:arg value=\"-x\"/></qemu:commandline>"),
                "qemu:commandline",
            ),
            (base.replace("</devices>", "</devices><seclabel type=\"none\"/>"), "seclabel"),
            (base.replace("</devices>", "</devices><memoryBacking><source type=\"file\"/></memoryBacking>"), "memoryBacking"),
            (base.replace("<domain type=\"kvm\">", "<domain type=\"xen\">"), "another hypervisor"),
            (base.replace("OVMF_CODE.4m.fd", "../../../etc/shadow"), "firmware path"),
            (base.replace("web_VARS.fd", "other_VARS.fd"), "another guest's nvram"),
            (base.replace("<vcpu>2</vcpu>", "<vcpu>2</vcpu><vcpu>9</vcpu>"), "duplicate vcpu"),
            (format!("<!DOCTYPE d [<!ENTITY x \"y\">]>{base}"), "DTD"),
        ] {
            assert!(check_domain(&bad).is_err(), "{why} allowed");
        }
    }

    /// `networkXml` in web/src/xml.ts, NAT with DNS and DHCP.
    #[test]
    fn a_form_network_passes() {
        let xml = r#"<network>
  <name>lan</name>
  <forward mode="nat"><nat><port start="1024" end="65535"/></nat></forward>
  <bridge name="virbr5"/>
  <dns>
    <forwarder addr="1.1.1.1"/>
  </dns>
  <ip address="10.9.0.1" netmask="255.255.255.0">
    <dhcp><range start="10.9.0.10" end="10.9.0.99"/><host mac="52:54:00:00:00:01" ip="10.9.0.5" name="a"/></dhcp>
  </ip>
</network>"#;
        let s = check_network(xml).unwrap();
        assert_eq!((s.name.as_str(), s.forward.as_str()), ("lan", "nat"));
        assert_eq!(s.bridge.as_deref(), Some("virbr5"));
        let b = check_network(
            r#"<network><name>up</name><forward mode="bridge"/><bridge name="br0"/></network>"#,
        )
        .unwrap();
        assert_eq!(
            (b.forward.as_str(), b.bridge.as_deref()),
            ("bridge", Some("br0"))
        );
    }

    /// A NIC may name no network: libvirt's schema requires a `<source>`, not a
    /// named one, so `<source network=""/>` is a NIC attached to nothing. The
    /// hardware editor offers "None" for exactly this, and it must be allowed
    /// for a non-admin -- but it must not become a way to reach a network the
    /// caller may not use, so a named source is still recorded for the check.
    #[test]
    fn an_interface_may_name_no_network_but_a_named_one_is_still_recorded() {
        let with_nic = |nic: &str| {
            form_domain().replace(
                "    <interface type=\"network\">\n      <source network=\"default\"/>\n      <model type=\"virtio\"/>\n      <bandwidth><inbound average=\"12800\"/></bandwidth>\n    </interface>",
                nic,
            )
        };
        // The blank one is accepted, and contributes no network to check --
        // there is nothing for a privilege to be checked against.
        let none = with_nic(
            r#"<interface type="network">
      <source network=""/>
      <model type="virtio"/>
    </interface>"#,
        );
        assert_ne!(
            none,
            form_domain(),
            "the fixture must actually be substituted"
        );
        let s = check_domain(&none).expect("a NIC with no network is allowed");
        assert!(
            s.networks.is_empty(),
            "an unnamed source must not be recorded as a network: {:?}",
            s.networks
        );
        // A named one is still recorded, so the caller's privileges cover it.
        let named = with_nic(
            r#"<interface type="network">
      <source network="lab"/>
      <model type="virtio"/>
    </interface>"#,
        );
        assert_eq!(
            check_domain(&named).unwrap().networks,
            vec!["lab".to_string()]
        );
    }

    #[test]
    fn host_network_modes_are_refused() {
        for (xml, why) in [
            (
                r#"<network><name>x</name><forward mode="route"/></network>"#,
                "route",
            ),
            (
                r#"<network><name>x</name><forward mode="hostdev"><pf dev="eth0"/></forward></network>"#,
                "hostdev",
            ),
            (
                r#"<network><name>x</name><forward mode="nat" dev="eth0"/></network>"#,
                "forward dev",
            ),
            (
                r#"<network><name>x</name><forward mode="bridge"/></network>"#,
                "bridge without a name",
            ),
            (
                r#"<network><name>x</name><dns><host ip="1.2.3.4"><hostname>a</hostname></host></dns></network>"#,
                "dns host",
            ),
            (
                r#"<network><name>x</name><portgroup name="p"/></network>"#,
                "portgroup",
            ),
        ] {
            assert!(check_network(xml).is_err(), "{why} allowed");
        }
    }

    /// What the forms write now: named by a fresh UUID, the display name as
    /// the title. A name that only looks like a UUID -- not its own -- is
    /// refused, so a record keyed by UUID can never point at the wrong thing.
    #[test]
    fn a_resource_named_by_its_uuid_passes_and_a_borrowed_one_does_not() {
        const U: &str = "ea15249b-a713-41dc-a808-05619e42eb2a";
        const OTHER: &str = "6f4d2a3b-0c9e-4d8f-b2a1-3e4c5d6e7f80";
        let dom = form_domain()
            .replace(
                "<name>web</name>",
                &format!("<name>{U}</name>\n  <uuid>{U}</uuid>\n  <title>My Web</title>"),
            )
            .replace("web_VARS.fd", &format!("{U}_VARS.fd"));
        let s = check_domain(&dom).unwrap();
        assert_eq!((s.name.as_str(), s.uuid.as_deref()), (U, Some(U)));
        let borrowed = dom.replacen(
            &format!("<uuid>{U}</uuid>"),
            &format!("<uuid>{OTHER}</uuid>"),
            1,
        );
        assert!(
            check_domain(&borrowed).is_err(),
            "someone else's UUID as the name"
        );
        let unanchored = dom.replacen(&format!("<uuid>{U}</uuid>"), "", 1);
        assert!(
            check_domain(&unanchored).is_err(),
            "a UUID name with no <uuid>"
        );
        // A plain name needs no <uuid>, as before.
        assert!(check_domain(&form_domain()).is_ok());

        let net = format!("<network><name>{U}</name><uuid>{U}</uuid><title>Lab</title><forward mode='nat'/></network>");
        assert_eq!(check_network(&net).unwrap().name, U);
        let borrowed =
            format!("<network><name>{U}</name><uuid>{OTHER}</uuid><forward mode='nat'/></network>");
        assert!(check_network(&borrowed).is_err());
    }

    #[test]
    fn a_form_volume_passes_and_escapes_do_not() {
        let v = check_volume("<volume>\n  <name>d.qcow2</name>\n  <capacity>21474836480</capacity>\n  <target><format type=\"qcow2\"/></target>\n</volume>").unwrap();
        assert_eq!(
            (v.name.as_str(), v.capacity, v.format.as_str()),
            ("d.qcow2", 20 << 30, "qcow2")
        );
        assert_eq!(
            check_volume("<volume><name>a</name><capacity unit=\"GiB\">2</capacity></volume>")
                .unwrap()
                .capacity,
            2 << 30
        );
        for bad in [
            "<volume><name>a</name><capacity>1</capacity><backingStore><path>/etc/shadow</path></backingStore></volume>",
            "<volume><name>a</name><capacity>1</capacity><target><path>/etc/x</path></target></volume>",
            "<volume><name>../a</name><capacity>1</capacity></volume>",
        ] {
            assert!(check_volume(bad).is_err(), "{bad}");
        }
    }
    /// The definition the daemon writes and libvirt reads back, verbatim
    /// (`virsh dumpxml --inactive` of a container made by `POST /containers`).
    const CT: &str = r#"<domain type='lxc'>
  <name>4b5ae5de-d025-467a-b415-faaa4a3a25ce</name>
  <uuid>4b5ae5de-d025-467a-b415-faaa4a3a25ce</uuid>
  <title>Probe CT</title>
  <metadata>
    <virtman:container xmlns:virtman="https://virt-man.invalid/container/1" rootfs="/var/lib/libvirt/images/ct-x.raw"/>
  </metadata>
  <memory unit='KiB'>262144</memory>
  <currentMemory unit='KiB'>262144</currentMemory>
  <vcpu placement='static'>1</vcpu>
  <resource><partition>/machine</partition></resource>
  <features><privnet/></features>
  <os><type arch='x86_64'>exe</type><init>/sbin/init</init></os>
  <idmap>
    <uid start='0' target='1073741824' count='65536'/>
    <gid start='0' target='1073741824' count='65536'/>
  </idmap>
  <clock offset='utc'/>
  <on_poweroff>destroy</on_poweroff><on_reboot>restart</on_reboot><on_crash>destroy</on_crash>
  <devices>
    <emulator>/usr/lib/libvirt/libvirt_lxc</emulator>
    <filesystem type='mount' accessmode='passthrough'>
      <source dir='/run/virt-man/ct/4b5ae5de-d025-467a-b415-faaa4a3a25ce'/><target dir='/'/>
    </filesystem>
    <interface type='network'><mac address='52:54:00:76:cf:2b'/><source network='internet'/></interface>
    <console type='pty'><target type='lxc' port='0'/></console>
  </devices>
</domain>"#;

    #[test]
    fn a_container_as_the_daemon_writes_it_passes() {
        let s = check_container(CT).unwrap();
        assert!(!s.privileged);
        assert_eq!((s.vcpus, s.memory_mb), (1, 256));
        assert_eq!(s.networks, vec!["internet"]);
    }

    #[test]
    fn dropping_the_idmap_is_reported_as_privileged() {
        let i = CT.find("<idmap>").unwrap();
        let j = CT.find("</idmap>").unwrap() + "</idmap>".len();
        let priv_ = format!("{}{}", &CT[..i], &CT[j..]);
        assert!(check_container(&priv_).unwrap().privileged);
    }

    #[test]
    fn a_container_cannot_reach_the_host() {
        let with = |from: &str, to: &str| CT.replace(from, to);
        let host_mount = with(
            "<interface",
            "<filesystem type='mount'><source dir='/'/><target dir='/host'/></filesystem><interface",
        );
        for (what, xml) in [
            ("host bind mount", host_mount),
            ("hostdev", with("<console", "<hostdev mode='capabilities' type='storage'><source><block>/dev/sda</block></source></hostdev><console")),
            ("capabilities", with("<privnet/>", "<privnet/><capabilities policy='allow'/>")),
            ("a bridge interface", with("type='network'><mac", "type='bridge'><mac")),
            ("a VM", with("type='lxc'>", "type='kvm'>")),
            ("seclabel", with("<clock", "<seclabel type='none'/><clock")),
        ] {
            assert!(check_container(&xml).is_err(), "{what} passed");
        }
        assert!(
            check_domain(CT).is_err(),
            "a container is not a VM definition"
        );
    }
}
