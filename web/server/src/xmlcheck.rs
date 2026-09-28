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
    let net = net
        .filter(|n| !n.is_empty())
        .ok_or("an interface needs a network")?;
    spec.networks.push(net);
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
            // `audio type='none'` is what libvirt adds to every guest with a
            // display; any other backend writes to a host sound server.
            "audio" => {
                plain(d, &["id", "type"])?;
                one_of(d, "type", &["none"])?;
            }
            // Guest-side emulated hardware. Each is held to the types that
            // name nothing on the host: an `evdev` or `passthrough` input
            // reads a host input device, and no host path or source may
            // appear anywhere beneath.
            "input" => {
                plain(d, &["type", "bus"])?;
                one_of(d, "type", &["mouse", "keyboard", "tablet"])?;
            }
            "video" | "sound" | "watchdog" | "memballoon" | "controller" => {
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
    if let Some(os) = elements(root).find(|c| c.tag_name().name() == "os") {
        check_os(os, &spec.name)?;
    }
    if spec.vcpus == 0 || spec.memory_mb == 0 {
        return Err("a guest needs <vcpu> and <memory>".into());
    }
    Ok(spec)
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
            "uuid" => leaf(c)?,
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
    if spec.forward == "bridge" && spec.bridge.is_none() {
        return Err("a bridge network must name its bridge".into());
    }
    Ok(spec)
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
}
