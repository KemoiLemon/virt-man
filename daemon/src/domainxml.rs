//! The direct-QEMU VM definition: the SPA's libvirt-shaped domain XML.
//!
//! The editor writes domain XML (`web/src/xml.ts`, `web/src/hardware.ts`), so
//! that XML is the one durable definition of a guest: `parse` reads it for the
//! launch argv and for every hotplug path, and `render` writes it back in the
//! same shapes for `GET .../xml`. There is no second copy of the hardware.
//!
//! Deliberately not read: libvirt's `<address>`, `<alias>` and PCIe hotplug
//! `<controller>`s (QEMU assigns its own topology) and `<emulator>` (the daemon
//! chooses the binary). They are annotations, not hardware. Root-level libvirt
//! bookkeeping (`<metadata>`, `<clock>`, `<on_poweroff>`) is ignored too; a
//! *device* is not, because dropping one changes what the guest has.
//!
//! What a direct QEMU launch cannot express is refused rather than dropped:
//! vhost-user and libvirt-network interfaces, mediated devices, non-PCI/USB
//! hostdevs, pool/volume and block disk sources, a second display adapter, an
//! unknown controller or device element. Silently ignoring one would define a
//! guest that does not match what the page says it has.

use std::{collections::BTreeSet, path::PathBuf};

use roxmltree::Node;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskFormat {
    #[default]
    Raw,
    Qcow2,
}

impl DiskFormat {
    /// The QEMU block driver name, which is also the wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Qcow2 => "qcow2",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskBus {
    Virtio,
    Sata,
    Scsi,
}

impl DiskBus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Virtio => "virtio",
            Self::Sata => "sata",
            Self::Scsi => "scsi",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskDevice {
    Disk,
    Cdrom,
}

impl DiskDevice {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Cdrom => "cdrom",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    pub device: DiskDevice,
    pub bus: DiskBus,
    pub format: DiskFormat,
    /// The guest-side name, and the QEMU device id: `vda`, `sdb`.
    pub target: String,
    /// `None` is an empty CD/DVD tray.
    pub source: Option<PathBuf>,
    pub readonly: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IfaceKind {
    /// QEMU's own user-mode NAT: no host interface and no stored address.
    User,
    /// The host bridge the guest is plugged into.
    Bridge(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IfaceModel {
    Virtio,
    E1000e,
    Rtl8139,
}

impl IfaceModel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Virtio => "virtio",
            Self::E1000e => "e1000e",
            Self::Rtl8139 => "rtl8139",
        }
    }

    pub fn driver(self) -> &'static str {
        match self {
            Self::Virtio => "virtio-net-pci",
            Self::E1000e => "e1000e",
            Self::Rtl8139 => "rtl8139",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rate {
    pub average: Option<u64>,
    pub peak: Option<u64>,
    pub burst: Option<u64>,
    pub floor: Option<u64>,
}

impl Rate {
    fn is_empty(&self) -> bool {
        self.average.is_none()
            && self.peak.is_none()
            && self.burst.is_none()
            && self.floor.is_none()
    }
}

/// A NIC's `<bandwidth>`. Recorded so a hardware save carries it through; the
/// QEMU manager installs the `tc` shaping from it at start and `api::nics` when
/// a limit changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bandwidth {
    pub inbound: Option<Rate>,
    pub outbound: Option<Rate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub kind: IfaceKind,
    pub model: IfaceModel,
    /// Canonical lower-case form, when the definition names one. `define`
    /// generates one for every NIC that has none.
    pub mac: Option<String>,
    pub bandwidth: Bandwidth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Graphics {
    Vnc { listen: String, port: u16 },
    Spice { listen: String, port: u16 },
}

impl Graphics {
    pub fn port(&self) -> u16 {
        match self {
            Self::Vnc { port, .. } | Self::Spice { port, .. } => *port,
        }
    }

    pub fn listen(&self) -> &str {
        match self {
            Self::Vnc { listen, .. } | Self::Spice { listen, .. } => listen,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoModel {
    None,
    Vga,
    Cirrus,
    Vmware,
    Qxl,
    Virtio,
    Bochs,
    Ramfb,
}

impl VideoModel {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Vga => "vga",
            Self::Cirrus => "cirrus",
            Self::Vmware => "vmvga",
            Self::Qxl => "qxl",
            Self::Virtio => "virtio",
            Self::Bochs => "bochs",
            Self::Ramfb => "ramfb",
        }
    }
}

/// The one `<video>` device, with the SPA's model options. libvirt ties each
/// option to a model, so only those the model accepts are kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Video {
    pub model: VideoModel,
    /// The concrete QEMU frontend for `virtio` (`virtio-vga`, `virtio-gpu`…).
    pub device: Option<String>,
    pub vram_kib: Option<u32>,
    pub ram_kib: Option<u32>,
    pub vgamem_kib: Option<u32>,
    pub vram64_kib: Option<u32>,
    pub heads: Option<u32>,
    pub blob: Option<bool>,
    pub edid: Option<bool>,
    pub accel3d: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hostdev {
    Pci {
        domain: u16,
        bus: u8,
        slot: u8,
        function: u8,
    },
    Usb {
        vendor: u16,
        product: u16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Firmware {
    Bios,
    Uefi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootDev {
    Hd,
    Cdrom,
    Network,
    Floppy,
}

impl BootDev {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hd => "hd",
            Self::Cdrom => "cdrom",
            Self::Network => "network",
            Self::Floppy => "fd",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cpu {
    /// `mode='host-passthrough'`/`'host-model'`, and the default.
    Host,
    /// `mode='maximum'`.
    Max,
    /// `mode='custom'` with a `<model>`.
    Model(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerialKind {
    Pty,
    Null,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tpm {
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watchdog {
    pub model: String,
    pub action: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Domain {
    /// The guest's identity: `<name>` and `<uuid>`. Canonical UUID form.
    pub id: String,
    pub uuid: String,
    pub title: String,
    pub description: String,
    pub arch: String,
    pub machine: String,
    pub domain_type: String,
    pub memory_mib: u32,
    pub vcpus: u16,
    pub firmware: Firmware,
    pub loader: Option<PathBuf>,
    /// The guest's own variable store, and the template to create it from.
    pub nvram: Option<PathBuf>,
    pub nvram_template: Option<PathBuf>,
    pub boot: Vec<BootDev>,
    pub cpu: Cpu,
    pub disks: Vec<Disk>,
    pub interfaces: Vec<Interface>,
    pub graphics: Option<Graphics>,
    pub video: Option<Video>,
    pub hostdevs: Vec<Hostdev>,
    pub rng: Option<PathBuf>,
    pub memballoon: bool,
    pub guest_agent: bool,
    pub tpm: Option<Tpm>,
    pub watchdog: Option<Watchdog>,
    pub serial: Option<SerialKind>,
}

/// Read one `<interface>…</interface>` element, as a hot-add request writes it.
pub fn parse_interface_xml(xml: &str) -> Result<Interface, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("invalid interface XML: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "interface" {
        return Err("the interface's root element is not <interface>".into());
    }
    parse_interface(root)
}

/// A NIC the daemon serves an address to: a bridge interface with a MAC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeNic {
    pub mac: String,
    pub bridge: String,
}

pub fn bridges(xml: &str) -> Result<Vec<BridgeNic>, String> {
    Ok(parse(xml)?
        .interfaces
        .into_iter()
        .filter_map(|i| match (i.kind, i.mac) {
            (IfaceKind::Bridge(bridge), Some(mac)) => Some(BridgeNic { mac, bridge }),
            _ => None,
        })
        .collect())
}

/// Read a domain definition. The identity comes from the XML; `merge` is the
/// editor's path, where it may be omitted.
pub fn parse(xml: &str) -> Result<Domain, String> {
    read(xml, None)
}

/// Fold an edit into the stored definition. The editor may omit `<name>`,
/// `<uuid>` and `<title>` (they are not hardware), which keep the stored
/// values; a different `<name>` is refused, because it is the identity and not
/// a label. An interface whose `<mac>` the editor dropped keeps the stored one
/// at the same position, or its address would silently change.
pub fn merge(old: &str, new: &str) -> Result<Domain, String> {
    let old = parse(old)?;
    let mut d = read(new, Some(&old))?;
    for (i, iface) in d.interfaces.iter_mut().enumerate() {
        if iface.mac.is_none() {
            iface.mac = old.interfaces.get(i).and_then(|o| o.mac.clone());
        }
    }
    Ok(d)
}

fn canonical_uuid(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok_and(|u| u.hyphenated().to_string() == s)
}

fn read(xml: &str, old: Option<&Domain>) -> Result<Domain, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("invalid domain XML: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "domain" {
        return Err("the definition's root element is not <domain>".into());
    }
    let domain_type = root.attribute("type").unwrap_or("kvm").to_string();
    if !matches!(domain_type.as_str(), "kvm" | "qemu") {
        return Err(format!("domain type '{domain_type}' is not supported"));
    }

    let name = child(root, "name").map(text).filter(|s| !s.is_empty());
    let id = match name {
        Some(n) => {
            if let Some(old) = old
                && n != old.id
            {
                return Err(format!(
                    "the <name> cannot change from '{}' to '{n}'; the display name is the <title>",
                    old.id
                ));
            }
            if !canonical_uuid(&n) {
                return Err(format!("<name> '{n}' is not a canonical UUID"));
            }
            n
        }
        None => old
            .map(|o| o.id.clone())
            .ok_or("the domain has no <name>")?,
    };
    let uuid_text = child(root, "uuid").map(text).filter(|s| !s.is_empty());
    let uuid = match uuid_text {
        Some(u) => {
            if u != id {
                return Err(format!("<uuid> '{u}' does not match <name> '{id}'"));
            }
            u
        }
        None => old.map(|o| o.uuid.clone()).unwrap_or_else(|| id.clone()),
    };
    if !canonical_uuid(&uuid) {
        return Err(format!("<uuid> '{uuid}' is not a canonical UUID"));
    }
    let title = match child(root, "title") {
        Some(t) => text(t),
        None => old.map(|o| o.title.clone()).unwrap_or_default(),
    };
    // Absent means cleared: the Options form drops the element to clear the
    // notes, so falling back to the old value would make them unclearable.
    let description = match child(root, "description") {
        Some(t) => text(t),
        None => String::new(),
    };

    let memory_mib = parse_memory(root)?;
    let vcpus = match child(root, "vcpu") {
        Some(v) => text(v)
            .parse::<u16>()
            .map_err(|_| "<vcpu> must be a number".to_string())?,
        None => 1,
    };
    if vcpus == 0 || vcpus > 256 {
        return Err("<vcpu> must be between 1 and 256".into());
    }

    let os = child(root, "os").ok_or("the domain has no <os>")?;
    let (arch, machine) = parse_os(os)?;
    let (firmware, loader, nvram, nvram_template) = parse_firmware(os)?;
    let mut boot = Vec::new();
    for b in children(os, "boot") {
        let dev = b.attribute("dev").unwrap_or("").trim();
        let dev = match dev {
            "hd" => BootDev::Hd,
            "cdrom" => BootDev::Cdrom,
            "network" => BootDev::Network,
            "fd" => BootDev::Floppy,
            other => return Err(format!("boot device '{other}' is not supported")),
        };
        if !boot.contains(&dev) {
            boot.push(dev);
        }
    }
    let cpu = parse_cpu(root)?;

    let mut d = Domain {
        id,
        uuid,
        description,
        title,
        arch,
        machine,
        domain_type,
        memory_mib,
        vcpus,
        firmware,
        loader,
        nvram,
        nvram_template,
        boot,
        cpu,
        disks: Vec::new(),
        interfaces: Vec::new(),
        graphics: None,
        video: None,
        hostdevs: Vec::new(),
        rng: None,
        memballoon: false,
        guest_agent: false,
        tpm: None,
        watchdog: None,
        serial: None,
    };

    if let Some(devices) = child(root, "devices") {
        for dev in devices.children().filter(Node::is_element) {
            match dev.tag_name().name() {
                "disk" => d.disks.push(parse_disk(dev)?),
                "interface" => d.interfaces.push(parse_interface(dev)?),
                "graphics" => {
                    if d.graphics.is_some() {
                        return Err("only one display adapter is supported".into());
                    }
                    d.graphics = Some(parse_graphics(dev)?);
                }
                "video" => {
                    if d.video.is_some() {
                        return Err("only one <video> adapter is supported".into());
                    }
                    d.video = Some(parse_video(dev)?);
                }
                "hostdev" => d.hostdevs.push(parse_hostdev(dev)?),
                "rng" => {
                    if d.rng.is_some() {
                        return Err("only one <rng> device is supported".into());
                    }
                    d.rng = Some(parse_rng(dev)?);
                }
                "memballoon" => {
                    let model = dev.attribute("model").unwrap_or("virtio");
                    d.memballoon = match model {
                        "virtio" | "virtio-transitional" | "virtio-non-transitional" => true,
                        "none" => false,
                        other => {
                            return Err(format!("memballoon model '{other}' is not supported"));
                        }
                    };
                }
                "channel" => d.guest_agent = parse_channel(dev)?,
                "tpm" => {
                    if d.tpm.is_some() {
                        return Err("only one <tpm> device is supported".into());
                    }
                    d.tpm = Some(parse_tpm(dev)?);
                }
                "watchdog" => {
                    if d.watchdog.is_some() {
                        return Err("only one <watchdog> is supported".into());
                    }
                    d.watchdog = Some(parse_watchdog(dev)?);
                }
                "serial" => {
                    if d.serial.is_some() {
                        return Err("only one serial port is supported".into());
                    }
                    d.serial = Some(parse_serial(dev)?);
                }
                "console" => {
                    // `<serial>` and `<console>` name the same QEMU device; the
                    // console is accepted only when it is the one that named it.
                    if d.serial.is_none() {
                        d.serial = Some(parse_serial(dev)?);
                    }
                }
                // Annotations the daemon owns or QEMU decides itself.
                "address" | "alias" | "emulator" | "audio" => {}
                // libvirt's PCIe root ports exist to give hotplug a slot;
                // QEMU assigns its own, so they are not carried.
                "controller" if dev.attribute("type") == Some("pci") => {}
                "controller" => {
                    return Err(format!(
                        "controller type '{}' is not supported by the direct-QEMU backend",
                        dev.attribute("type").unwrap_or("")
                    ));
                }
                other => {
                    return Err(format!(
                        "the <devices> element <{other}> is not supported by the direct-QEMU backend"
                    ));
                }
            }
        }
    }

    validate(&d)?;
    Ok(d)
}

fn parse_memory(root: Node) -> Result<u32, String> {
    let mem = child(root, "memory").ok_or("the domain has no <memory>")?;
    memory_mib(mem)
}

/// A libvirt size element (`<memory>`, `<swap_hard_limit>`) in whole MiB.
pub(crate) fn memory_mib(mem: Node) -> Result<u32, String> {
    let tag = mem.tag_name().name();
    let unit = mem.attribute("unit").unwrap_or("KiB");
    let value: u64 = text(mem)
        .parse()
        .map_err(|_| format!("<{tag}> must be a number"))?;
    let kib = match unit {
        "KiB" | "kib" | "Ki" | "k" => value,
        "MiB" | "mib" | "M" | "m" => value.saturating_mul(1024),
        "GiB" | "gib" | "G" | "g" => value.saturating_mul(1024 * 1024),
        "b" | "bytes" => value / 1024,
        other => return Err(format!("memory unit '{other}' is not supported")),
    };
    if kib == 0 {
        return Err(format!("<{tag}> must be positive"));
    }
    if kib % 1024 != 0 {
        return Err(format!("<{tag}> must be a whole number of MiB"));
    }
    let mib = kib / 1024;
    if mib > 1_048_576 {
        return Err(format!("<{tag}> exceeds the supported limit"));
    }
    Ok(mib as u32)
}

fn parse_os(os: Node) -> Result<(String, String), String> {
    let ty = child(os, "type").ok_or("the domain has no <os><type>")?;
    let arch = ty.attribute("arch").unwrap_or("x86_64").to_string();
    if !matches!(arch.as_str(), "x86_64" | "i686") {
        return Err(format!("architecture '{arch}' is not supported"));
    }
    let machine = ty.attribute("machine").unwrap_or("q35").trim().to_string();
    if machine.is_empty() || machine.contains(',') {
        return Err("<os><type machine=...> must name one machine type".into());
    }
    for bad in ["kernel", "initrd", "cmdline"] {
        if child(os, bad).is_some() {
            return Err("direct kernel boot is not supported by the direct-QEMU backend".into());
        }
    }
    Ok((arch, machine))
}

#[allow(clippy::type_complexity)]
fn parse_firmware(
    os: Node,
) -> Result<(Firmware, Option<PathBuf>, Option<PathBuf>, Option<PathBuf>), String> {
    let Some(loader) = child(os, "loader") else {
        return Ok((Firmware::Bios, None, None, None));
    };
    if loader.attribute("type").is_some_and(|t| t != "pflash") {
        return Err("only a pflash <loader> is supported".into());
    }
    let path = absolute(text(loader), "<loader>")?;
    let nvram = child(os, "nvram");
    let (nvram, template) = match nvram {
        Some(n) => (
            Some(absolute(text(n), "<nvram>")?),
            n.attribute("template")
                .map(|t| absolute(t.to_string(), "<nvram template>"))
                .transpose()?,
        ),
        None => (None, None),
    };
    Ok((Firmware::Uefi, Some(path), nvram, template))
}

fn parse_cpu(root: Node) -> Result<Cpu, String> {
    let Some(cpu) = child(root, "cpu") else {
        return Ok(Cpu::Host);
    };
    Ok(match cpu.attribute("mode").unwrap_or("") {
        "host-passthrough" | "host-model" => Cpu::Host,
        "maximum" => Cpu::Max,
        "custom" | "" => match child(cpu, "model").map(text).filter(|m| !m.is_empty()) {
            Some(model) => Cpu::Model(model),
            None if cpu.attribute("mode") == Some("custom") => {
                return Err("a custom CPU needs a <model>".into());
            }
            None => Cpu::Host,
        },
        other => return Err(format!("CPU mode '{other}' is not supported")),
    })
}

fn parse_disk(n: Node) -> Result<Disk, String> {
    let dtype = n
        .attribute("type")
        .ok_or("a <disk> must have a type")?
        .trim();
    let device = match n.attribute("device").unwrap_or("disk") {
        "disk" => DiskDevice::Disk,
        "cdrom" => DiskDevice::Cdrom,
        other => return Err(format!("disk device '{other}' is not supported")),
    };
    match dtype {
        "file" => {}
        "volume" => {
            return Err(
                "a disk is addressed by pool/volume; attach it by its absolute path instead".into(),
            );
        }
        other => return Err(format!("disk type '{other}' is not supported")),
    }
    let format = match child(n, "driver")
        .and_then(|d| d.attribute("type"))
        .unwrap_or("raw")
    {
        "raw" => DiskFormat::Raw,
        "qcow2" => DiskFormat::Qcow2,
        other => return Err(format!("disk format '{other}' is not supported")),
    };
    let source = match child(n, "source") {
        Some(s) => {
            if s.attribute("pool").is_some() || s.attribute("volume").is_some() {
                return Err(
                    "a disk is addressed by pool/volume; attach it by its absolute path instead"
                        .into(),
                );
            }
            s.attribute("file")
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .map(|f| absolute(f.to_string(), "disk source"))
                .transpose()?
        }
        None => None,
    };
    let target = child(n, "target").ok_or("a <disk> needs a <target>")?;
    let dev = target.attribute("dev").unwrap_or("").trim().to_string();
    if dev.is_empty()
        || dev.len() > 8
        || !dev
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return Err(format!("disk target '{dev}' is not a usable device name"));
    }
    let bus = match target.attribute("bus").unwrap_or("") {
        "virtio" => DiskBus::Virtio,
        "sata" => DiskBus::Sata,
        "scsi" => DiskBus::Scsi,
        other => {
            return Err(format!(
                "disk bus '{other}' is not supported; use virtio, sata or scsi"
            ));
        }
    };
    if device == DiskDevice::Cdrom && bus == DiskBus::Virtio {
        return Err(format!(
            "CD/DVD drive '{dev}' is on virtio; use a sata or scsi bus"
        ));
    }
    if device == DiskDevice::Disk && source.is_none() {
        return Err(format!("disk '{dev}' has no source file"));
    }
    Ok(Disk {
        device,
        bus,
        format,
        target: dev,
        source,
        readonly: child(n, "readonly").is_some(),
    })
}

fn parse_interface(n: Node) -> Result<Interface, String> {
    let kind = match n.attribute("type").unwrap_or("") {
        "user" => IfaceKind::User,
        "bridge" => {
            let bridge = child(n, "source")
                .and_then(|s| s.attribute("bridge"))
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .ok_or("a bridge NIC must name its bridge")?;
            if !crate::hostnet::valid_name(bridge) {
                return Err(format!("'{bridge}' is not a usable bridge name"));
            }
            IfaceKind::Bridge(bridge.to_string())
        }
        "network" => {
            return Err("a NIC names a network; attach it to that network's bridge instead".into());
        }
        other => {
            return Err(format!(
                "interface type '{other}' is not supported by the direct-QEMU backend"
            ));
        }
    };
    let model = match child(n, "model")
        .and_then(|m| m.attribute("type"))
        .unwrap_or("virtio")
    {
        "virtio" => IfaceModel::Virtio,
        "e1000e" => IfaceModel::E1000e,
        "rtl8139" => IfaceModel::Rtl8139,
        other => return Err(format!("NIC model '{other}' is not supported")),
    };
    let mac = child(n, "mac")
        .and_then(|m| m.attribute("address"))
        .map(normalize_mac)
        .transpose()?;
    let bandwidth = match child(n, "bandwidth") {
        Some(b) => parse_bandwidth(b)?,
        None => Bandwidth::default(),
    };
    for e in n.children().filter(Node::is_element) {
        if !matches!(
            e.tag_name().name(),
            "source" | "mac" | "model" | "bandwidth" | "address" | "alias" | "target" | "driver"
        ) {
            return Err(format!(
                "interface element <{}> is not supported",
                e.tag_name().name()
            ));
        }
    }
    Ok(Interface {
        kind,
        model,
        mac,
        bandwidth,
    })
}

/// A `<bandwidth>` element. Shared with container definitions (`ctxml`),
/// whose NICs are shaped by the same host `tc` plan.
pub(crate) fn parse_bandwidth(b: Node) -> Result<Bandwidth, String> {
    let mut bw = Bandwidth::default();
    for dir in b.children().filter(Node::is_element) {
        let rate = parse_rate(dir)?;
        match dir.tag_name().name() {
            "inbound" => bw.inbound = (!rate.is_empty()).then_some(rate),
            "outbound" => bw.outbound = (!rate.is_empty()).then_some(rate),
            other => {
                return Err(format!("<bandwidth> element <{other}> is not supported"));
            }
        }
    }
    Ok(bw)
}

fn parse_rate(n: Node) -> Result<Rate, String> {
    let num = |name: &str| -> Result<Option<u64>, String> {
        match n.attribute(name) {
            Some(v) => v
                .trim()
                .parse::<u64>()
                .map(Some)
                .map_err(|_| format!("<{} {name}=...> must be a number", n.tag_name().name())),
            None => Ok(None),
        }
    };
    Ok(Rate {
        average: num("average")?,
        peak: num("peak")?,
        burst: num("burst")?,
        floor: num("floor")?,
    })
}

fn parse_graphics(n: Node) -> Result<Graphics, String> {
    let spice = match n.attribute("type").unwrap_or("") {
        "vnc" => false,
        "spice" => true,
        other => {
            return Err(format!(
                "graphics type '{other}' is not supported; use vnc or spice"
            ));
        }
    };
    let listen = n
        .attribute("listen")
        .unwrap_or("127.0.0.1")
        .trim()
        .to_string();
    if listen.is_empty() || listen.contains(',') || listen.chars().any(char::is_whitespace) {
        return Err(format!("'{listen}' is not a usable display address"));
    }
    let port = n
        .attribute("port")
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != "-1")
        .ok_or("a display needs an explicit port; autoport is not supported")?
        .parse::<u16>()
        .map_err(|_| "the display port must be a number".to_string())?;
    if port == 0 {
        return Err("the display port must be between 1 and 65535".into());
    }
    Ok(if spice {
        Graphics::Spice { listen, port }
    } else {
        Graphics::Vnc { listen, port }
    })
}

fn parse_video(n: Node) -> Result<Video, String> {
    let m = child(n, "model").ok_or("a <video> needs a <model>")?;
    let model = match m.attribute("type").unwrap_or("") {
        "none" => VideoModel::None,
        "vga" => VideoModel::Vga,
        "cirrus" => VideoModel::Cirrus,
        "vmvga" => VideoModel::Vmware,
        "qxl" => VideoModel::Qxl,
        "virtio" => VideoModel::Virtio,
        "bochs" => VideoModel::Bochs,
        "ramfb" => VideoModel::Ramfb,
        other => {
            return Err(format!(
                "video model '{other}' is not supported by the direct-QEMU backend"
            ));
        }
    };
    let num = |name: &str| -> Result<Option<u32>, String> {
        match m.attribute(name) {
            Some(v) => v
                .trim()
                .parse::<u32>()
                .map(Some)
                .map_err(|_| format!("a video's {name} must be a number")),
            None => Ok(None),
        }
    };
    let flag = |name: &str| -> Result<Option<bool>, String> {
        match m.attribute(name) {
            Some("on" | "yes") => Ok(Some(true)),
            Some("off" | "no") => Ok(Some(false)),
            Some(other) => Err(format!("a video's {name} must be on or off, not '{other}'")),
            None => Ok(None),
        }
    };
    let accel3d = m
        .children()
        .filter(Node::is_element)
        .find(|c| c.tag_name().name() == "acceleration")
        .and_then(|a| a.attribute("accel3d"))
        .is_some_and(|v| v == "yes" || v == "on");
    let device = m
        .attribute("device")
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string);
    let device = if model == VideoModel::Virtio {
        // The frontend is part of the model: keep the SPA's default explicit so
        // the parsed view and a re-render describe the same device.
        Some(device.unwrap_or_else(|| "virtio-vga".into()))
    } else {
        device
    };
    if model == VideoModel::Virtio {
        let dev = device.as_deref().unwrap_or("virtio-vga");
        if !matches!(
            dev,
            "virtio-vga" | "virtio-vga-gl" | "virtio-gpu" | "virtio-gpu-gl"
        ) {
            return Err(format!("virtio video device '{dev}' is not supported"));
        }
        if dev.ends_with("-gl") != accel3d {
            return Err(format!(
                "video device '{dev}' and 3D acceleration must be enabled together"
            ));
        }
    }
    let vram_kib = num("vram")?;
    let ram_kib = num("ram")?;
    let vgamem_kib = num("vgamem")?;
    let vram64_kib = num("vram64")?;
    let heads = num("heads")?;
    let blob = flag("blob")?;
    let edid = flag("edid")?;
    // QEMU ties each attribute to a device. Writing one elsewhere would be
    // refused at start, so the mismatch is reported with the definition still
    // in front of the user.
    if model != VideoModel::Qxl
        && (ram_kib.is_some() || vgamem_kib.is_some() || vram64_kib.is_some())
    {
        return Err(format!(
            "video {} takes ram/vgamem/vram64 only as qxl",
            model.as_str()
        ));
    }
    if matches!(
        model,
        VideoModel::Virtio | VideoModel::Ramfb | VideoModel::None
    ) && vram_kib.is_some()
    {
        return Err(format!(
            "QEMU's {} display has no vram size; clear Video VRAM",
            model.as_str()
        ));
    }
    if model != VideoModel::Virtio && blob.is_some() {
        return Err(format!(
            "video {} takes blob resources only as virtio",
            model.as_str()
        ));
    }
    if edid.is_some()
        && !matches!(
            model,
            VideoModel::Vga | VideoModel::Bochs | VideoModel::Virtio
        )
    {
        return Err(format!("video {} takes no edid setting", model.as_str()));
    }
    if heads.is_some() && !matches!(model, VideoModel::Qxl | VideoModel::Virtio) {
        return Err(format!("video {} takes no head count", model.as_str()));
    }
    Ok(Video {
        model,
        device,
        vram_kib,
        ram_kib,
        vgamem_kib,
        vram64_kib,
        heads,
        blob,
        edid,
        accel3d,
    })
}

fn parse_hostdev(n: Node) -> Result<Hostdev, String> {
    if n.attribute("mode").unwrap_or("subsystem") != "subsystem" {
        return Err("only subsystem <hostdev> devices are supported".into());
    }
    match n.attribute("type").unwrap_or("") {
        "pci" => {
            let address = child(n, "source")
                .and_then(|s| child(s, "address"))
                .ok_or("a PCI hostdev needs an <address>")?;
            let domain = hex(address.attribute("domain").unwrap_or("0"), "domain")?;
            let bus = hex(
                address
                    .attribute("bus")
                    .ok_or("a PCI address needs a bus")?,
                "bus",
            )?;
            let slot = hex(
                address
                    .attribute("slot")
                    .ok_or("a PCI address needs a slot")?,
                "slot",
            )?;
            let function = hex(
                address
                    .attribute("function")
                    .ok_or("a PCI address needs a function")?,
                "function",
            )?;
            if domain > 0xffff || bus > 0xff || slot > 0xff || function > 7 {
                return Err("a PCI address is out of range".into());
            }
            Ok(Hostdev::Pci {
                domain: domain as u16,
                bus: bus as u8,
                slot: slot as u8,
                function: function as u8,
            })
        }
        "usb" => {
            let source = child(n, "source").ok_or("a USB hostdev needs a <source>")?;
            let vendor = hex(
                child(source, "vendor")
                    .and_then(|v| v.attribute("id"))
                    .ok_or("a USB hostdev needs a vendor id")?,
                "vendor",
            )?;
            let product = hex(
                child(source, "product")
                    .and_then(|v| v.attribute("id"))
                    .ok_or("a USB hostdev needs a product id")?,
                "product",
            )?;
            if vendor > 0xffff || product > 0xffff {
                return Err("a USB vendor/product id must fit in 16 bits".into());
            }
            Ok(Hostdev::Usb {
                vendor: vendor as u16,
                product: product as u16,
            })
        }
        "mdev" => Err("mediated devices are not supported by the direct-QEMU backend".into()),
        other => Err(format!("hostdev type '{other}' is not supported")),
    }
}

fn parse_rng(n: Node) -> Result<PathBuf, String> {
    match n.attribute("model").unwrap_or("virtio") {
        "virtio" => {}
        other => return Err(format!("rng model '{other}' is not supported")),
    }
    let backend = child(n, "backend").ok_or("an <rng> needs a <backend>")?;
    if backend.attribute("model").unwrap_or("random") != "random" {
        return Err("only an rng backed by /dev/random or /dev/urandom is supported".into());
    }
    absolute(text(backend), "rng backend")
}

fn parse_channel(n: Node) -> Result<bool, String> {
    let target = child(n, "target").ok_or("a <channel> needs a <target>")?;
    if target.attribute("type").unwrap_or("") != "virtio"
        || target.attribute("name") != Some("org.qemu.guest_agent.0")
    {
        return Err(
            "only the QEMU guest-agent channel (org.qemu.guest_agent.0) is supported".into(),
        );
    }
    if n.attribute("type").unwrap_or("") != "unix" {
        return Err("the guest-agent channel must be a unix socket".into());
    }
    if child(n, "source").is_some() {
        return Err("the guest-agent socket path is owned by the daemon".into());
    }
    Ok(true)
}

fn parse_tpm(n: Node) -> Result<Tpm, String> {
    if n.attribute("model").unwrap_or("") != "tpm-crb" {
        return Err("only a tpm-crb model is supported".into());
    }
    let backend = child(n, "backend").ok_or("a <tpm> needs a <backend>")?;
    if backend.attribute("type").unwrap_or("") != "emulator" {
        return Err("only an emulated TPM (swtpm) is supported".into());
    }
    let version = backend.attribute("version").unwrap_or("2.0");
    if !matches!(version, "1.2" | "2.0") {
        return Err(format!("TPM version '{version}' is not supported"));
    }
    Ok(Tpm {
        version: version.to_string(),
    })
}

fn parse_watchdog(n: Node) -> Result<Watchdog, String> {
    let model = n.attribute("model").unwrap_or("i6300esb");
    if !matches!(model, "i6300esb" | "ib700") {
        return Err(format!("watchdog model '{model}' is not supported"));
    }
    let action = n.attribute("action").unwrap_or("reset");
    if !matches!(
        action,
        "reset" | "shutdown" | "poweroff" | "pause" | "none" | "debug"
    ) {
        return Err(format!("watchdog action '{action}' is not supported"));
    }
    Ok(Watchdog {
        model: model.to_string(),
        action: action.to_string(),
    })
}

fn parse_serial(n: Node) -> Result<SerialKind, String> {
    match n.attribute("type").unwrap_or("") {
        "pty" => Ok(SerialKind::Pty),
        "null" => Ok(SerialKind::Null),
        other => Err(format!(
            "serial type '{other}' is not supported; use pty or null"
        )),
    }
}

fn validate(d: &Domain) -> Result<(), String> {
    let mut targets = BTreeSet::new();
    let mut sources = BTreeSet::new();
    for disk in &d.disks {
        if !targets.insert(disk.target.as_str()) {
            return Err(format!("two disks share the target '{}'", disk.target));
        }
        if let Some(source) = &disk.source
            && !sources.insert(source)
        {
            return Err(format!(
                "the same file is attached twice: {}",
                source.display()
            ));
        }
    }
    let mut macs = BTreeSet::new();
    for iface in &d.interfaces {
        if let Some(mac) = &iface.mac
            && !macs.insert(mac.clone())
        {
            return Err(format!("two NICs share the MAC address {mac}"));
        }
    }
    // One PCI address is one device: a second pass-through of it can only be
    // a duplicated block (an editor append), which QEMU would refuse at start.
    let mut pci = BTreeSet::new();
    for hostdev in &d.hostdevs {
        if let Hostdev::Pci {
            domain,
            bus,
            slot,
            function,
        } = hostdev
            && !pci.insert((domain, bus, slot, function))
        {
            return Err(format!(
                "PCI device {domain:04x}:{bus:02x}:{slot:02x}.{function:x} is assigned twice"
            ));
        }
    }
    Ok(())
}

/// A MAC in canonical lower-case colon form, unicast and not all-zero.
pub fn normalize_mac(mac: &str) -> Result<String, String> {
    let m = crate::ipam::store::normalize_mac(mac)
        .ok_or_else(|| format!("'{mac}' is not a MAC address"))?;
    let first = u8::from_str_radix(&m[..2], 16).map_err(|e| e.to_string())?;
    if first & 1 == 1 {
        return Err(format!("'{mac}' is a multicast address"));
    }
    if m == "00:00:00:00:00:00" {
        return Err("'00:00:00:00:00:00' is the all-zero address".into());
    }
    Ok(m)
}

fn absolute(s: String, what: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(s);
    if !path.is_absolute() {
        return Err(format!("{what} must be an absolute path"));
    }
    Ok(path)
}

fn hex(s: &str, what: &str) -> Result<u32, String> {
    let t = s.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u32::from_str_radix(t, 16).map_err(|_| format!("'{s}' is not a hexadecimal {what}"))
}

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}

fn children<'a, 'i>(n: Node<'a, 'i>, name: &'a str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children()
        .filter(move |c| c.is_element() && c.tag_name().name() == name)
}

fn text(n: Node) -> String {
    n.text().unwrap_or("").trim().to_string()
}

// ---------------------------------------------------------------- rendering

/// The SPA-shaped definition: what `GET .../xml` returns and what the manager
/// stores. Every element here round-trips through `parse`.
pub fn render(d: &Domain) -> String {
    let mut s = format!(
        "<domain type=\"{}\">\n  <name>{}</name>\n  <uuid>{}</uuid>\n",
        esc(&d.domain_type),
        esc(&d.id),
        esc(&d.uuid)
    );
    if !d.title.is_empty() {
        s.push_str(&format!("  <title>{}</title>\n", esc(&d.title)));
    }
    if !d.description.is_empty() {
        s.push_str(&format!("  <description>{}</description>\n", esc(&d.description)));
    }
    s.push_str(&format!(
        "  <memory unit=\"KiB\">{}</memory>\n  <vcpu>{}</vcpu>\n  <os>\n    <type arch=\"{}\" machine=\"{}\">hvm</type>\n",
        d.memory_mib * 1024,
        d.vcpus,
        esc(&d.arch),
        esc(&d.machine)
    ));
    if let Some(loader) = &d.loader {
        s.push_str(&format!(
            "    <loader readonly=\"yes\" type=\"pflash\" format=\"raw\">{}</loader>\n",
            esc(&loader.to_string_lossy())
        ));
    }
    if let Some(nvram) = &d.nvram {
        let template = d
            .nvram_template
            .as_ref()
            .map(|t| format!(" template=\"{}\"", esc(&t.to_string_lossy())))
            .unwrap_or_default();
        s.push_str(&format!(
            "    <nvram{template} format=\"raw\">{}</nvram>\n",
            esc(&nvram.to_string_lossy())
        ));
    }
    for boot in &d.boot {
        s.push_str(&format!("    <boot dev=\"{}\"/>\n", boot.as_str()));
    }
    s.push_str("  </os>\n  <features><acpi/><apic/></features>\n");
    match &d.cpu {
        Cpu::Host => s.push_str("  <cpu mode=\"host-passthrough\"/>\n"),
        Cpu::Max => s.push_str("  <cpu mode=\"maximum\"/>\n"),
        Cpu::Model(model) => s.push_str(&format!(
            "  <cpu mode=\"custom\"><model>{}</model></cpu>\n",
            esc(model)
        )),
    }
    s.push_str("  <devices>\n");
    for disk in &d.disks {
        s.push_str(&render_disk(disk));
    }
    for iface in &d.interfaces {
        s.push_str(&render_interface(iface));
    }
    if let Some(g) = &d.graphics {
        let kind = match g {
            Graphics::Vnc { .. } => "vnc",
            Graphics::Spice { .. } => "spice",
        };
        s.push_str(&format!(
            "    <graphics type=\"{kind}\" port=\"{}\" autoport=\"yes\" listen=\"{}\"/>\n",
            g.port(),
            esc(g.listen())
        ));
    }
    if let Some(v) = &d.video {
        s.push_str(&render_video(v));
    }
    for h in &d.hostdevs {
        s.push_str(&render_hostdev(h));
    }
    if let Some(rng) = &d.rng {
        s.push_str(&format!(
            "    <rng model=\"virtio\">\n      <backend model=\"random\">{}</backend>\n    </rng>\n",
            esc(&rng.to_string_lossy())
        ));
    }
    if d.memballoon {
        s.push_str("    <memballoon model=\"virtio\"/>\n");
    }
    if d.guest_agent {
        s.push_str(
            "    <channel type=\"unix\">\n      <target type=\"virtio\" name=\"org.qemu.guest_agent.0\"/>\n    </channel>\n",
        );
    }
    if let Some(t) = &d.tpm {
        s.push_str(&format!(
            "    <tpm model=\"tpm-crb\">\n      <backend type=\"emulator\" version=\"{}\"/>\n    </tpm>\n",
            esc(&t.version)
        ));
    }
    if let Some(w) = &d.watchdog {
        s.push_str(&format!(
            "    <watchdog model=\"{}\" action=\"{}\"/>\n",
            esc(&w.model),
            esc(&w.action)
        ));
    }
    match d.serial {
        Some(SerialKind::Pty) => s.push_str(
            "    <serial type=\"pty\"><target port=\"0\"/></serial>\n    <console type=\"pty\"><target type=\"serial\" port=\"0\"/></console>\n",
        ),
        Some(SerialKind::Null) => s.push_str("    <serial type=\"null\"/>\n"),
        None => {}
    }
    s.push_str("  </devices>\n</domain>\n");
    s
}

fn render_disk(d: &Disk) -> String {
    let mut s = format!(
        "    <disk type=\"file\" device=\"{}\">\n      <driver name=\"qemu\" type=\"{}\"/>\n",
        d.device.as_str(),
        d.format.as_str()
    );
    if let Some(source) = &d.source {
        s.push_str(&format!(
            "      <source file=\"{}\"/>\n",
            esc(&source.to_string_lossy())
        ));
    }
    s.push_str(&format!(
        "      <target dev=\"{}\" bus=\"{}\"/>\n",
        esc(&d.target),
        d.bus.as_str()
    ));
    if d.readonly {
        s.push_str("      <readonly/>\n");
    }
    s.push_str("    </disk>\n");
    s
}

fn render_interface(i: &Interface) -> String {
    let mut s = match &i.kind {
        IfaceKind::User => "    <interface type=\"user\">\n".to_string(),
        IfaceKind::Bridge(bridge) => format!(
            "    <interface type=\"bridge\">\n      <source bridge=\"{}\"/>\n",
            esc(bridge)
        ),
    };
    s.push_str(&format!("      <model type=\"{}\"/>\n", i.model.as_str()));
    if let Some(mac) = &i.mac {
        s.push_str(&format!("      <mac address=\"{}\"/>\n", esc(mac)));
    }
    if i.bandwidth != Bandwidth::default() {
        s.push_str("      ");
        s.push_str(&render_bandwidth(&i.bandwidth));
        s.push('\n');
    }
    s.push_str("    </interface>\n");
    s
}

fn render_video(v: &Video) -> String {
    let mut attrs = String::new();
    let num = |name: &str, value: Option<u32>| match value {
        Some(v) => format!(" {name}=\"{v}\""),
        None => String::new(),
    };
    match v.model {
        VideoModel::Qxl => {
            for (name, value) in [
                ("ram", v.ram_kib),
                ("vgamem", v.vgamem_kib),
                ("vram64", v.vram64_kib),
                ("heads", v.heads),
                ("vram", v.vram_kib),
            ] {
                attrs.push_str(&num(name, value));
            }
        }
        VideoModel::Virtio => {
            if let Some(device) = &v.device
                && device != "virtio-vga"
            {
                attrs.push_str(&format!(" device=\"{}\"", esc(device)));
            }
            if let Some(b) = v.blob {
                attrs.push_str(&format!(" blob=\"{}\"", if b { "on" } else { "off" }));
            }
            if let Some(e) = v.edid {
                attrs.push_str(&format!(" edid=\"{}\"", if e { "on" } else { "off" }));
            }
            attrs.push_str(&num("heads", v.heads));
        }
        _ => {
            attrs.push_str(&num("heads", v.heads));
            attrs.push_str(&num("vram", v.vram_kib));
            if let Some(e) = v.edid {
                attrs.push_str(&format!(" edid=\"{}\"", if e { "on" } else { "off" }));
            }
        }
    }
    let model = format!("<model type=\"{}\"{attrs}", v.model.as_str());
    let model = if v.model == VideoModel::Virtio && v.accel3d {
        format!("{model}>\n        <acceleration accel3d=\"yes\"/>\n      </model>")
    } else {
        format!("{model}/>")
    };
    format!("    <video>\n      {model}\n    </video>\n")
}

fn render_hostdev(h: &Hostdev) -> String {
    match h {
        Hostdev::Pci {
            domain,
            bus,
            slot,
            function,
        } => format!(
            "    <hostdev mode=\"subsystem\" type=\"pci\" managed=\"yes\">\n      <source><address domain=\"0x{domain:04x}\" bus=\"0x{bus:02x}\" slot=\"0x{slot:02x}\" function=\"0x{function:x}\"/></source>\n    </hostdev>\n"
        ),
        Hostdev::Usb { vendor, product } => format!(
            "    <hostdev mode=\"subsystem\" type=\"usb\" managed=\"yes\">\n      <source><vendor id=\"0x{vendor:04x}\"/><product id=\"0x{product:04x}\"/></source>\n    </hostdev>\n"
        ),
    }
}

/// A `<bandwidth>` element, on one line; empty for no limits.
pub(crate) fn render_bandwidth(b: &Bandwidth) -> String {
    if *b == Bandwidth::default() {
        return String::new();
    }
    let mut s = String::from("<bandwidth>");
    for (tag, rate) in [("inbound", &b.inbound), ("outbound", &b.outbound)] {
        if let Some(r) = rate {
            s.push_str(&format!("<{tag}"));
            for (name, value) in [
                ("average", r.average),
                ("peak", r.peak),
                ("burst", r.burst),
                ("floor", r.floor),
            ] {
                if let Some(v) = value {
                    s.push_str(&format!(" {name}=\"{v}\""));
                }
            }
            s.push_str("/>");
        }
    }
    s.push_str("</bandwidth>");
    s
}

/// XML text and attribute escaping; safe in both positions.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Undo XML's five escapes, for text read out of a definition.
pub fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    /// What `domainXml` writes for a BIOS guest: one file disk, a sata ISO, a
    /// VNC display, a virtio video and a bridge NIC with a rate limit.
    fn spa_bios() -> String {
        format!(
            r#"<domain type="kvm">
  <name>{ID}</name>
  <uuid>{ID}</uuid>
  <title>web &amp; db</title>
  <memory unit="KiB">2097152</memory>
  <vcpu>2</vcpu>
  <os>
    <type arch="x86_64" machine="q35">hvm</type>
    <boot dev="hd"/>
    <boot dev="cdrom"/>
  </os>
  <features><acpi/><apic/></features>
  <devices>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    <disk type="file" device="disk">
      <driver name="qemu" type="qcow2"/>
      <source file="/var/lib/virt-man/images/web.qcow2"/>
      <target dev="vda" bus="virtio"/>
    </disk>
    <disk type="file" device="cdrom">
      <driver name="qemu" type="raw"/>
      <source file="/var/lib/virt-man/images/debian.iso"/>
      <target dev="sda" bus="sata"/>
      <readonly/>
    </disk>
    <interface type="bridge">
      <source bridge="vbr5b92a1a54f00"/>
      <model type="virtio"/>
      <mac address="52:54:00:aa:bb:01"/>
      <bandwidth><inbound average="1000" peak="2000"/></bandwidth>
    </interface>
    <graphics type="vnc" port="5900" autoport="yes" listen="127.0.0.1"/>
    <video>
      <model type="virtio" device="virtio-vga" edid="on"/>
      <address type="pci"/>
    </video>
    <serial type="pty"><target port="0"/></serial>
    <console type="pty"><target type="serial" port="0"/></console>
    <controller type="pci" index="0" model="pcie-root"/>
  </devices>
</domain>"#
        )
    }

    fn spa_uefi() -> String {
        format!(
            r#"<domain type="kvm">
  <name>{ID}</name>
  <uuid>{ID}</uuid>
  <title>fw</title>
  <memory unit="KiB">1048576</memory>
  <vcpu>1</vcpu>
  <os>
    <type arch="x86_64" machine="q35">hvm</type>
    <loader readonly="yes" type="pflash" format="raw">/usr/share/OVMF/OVMF_CODE_4M.fd</loader>
    <nvram template="/usr/share/OVMF/OVMF_VARS_4M.fd" format="raw">/var/lib/libvirt/qemu/nvram/{ID}_VARS.fd</nvram>
    <boot dev="hd"/>
  </os>
  <devices>
    <graphics type="spice" port="5901" autoport="yes" listen="127.0.0.1"/>
  </devices>
</domain>"#
        )
    }

    /// `hardware.ts` for every catalogue device at once.
    fn hardware() -> String {
        format!(
            r#"<domain type="kvm">
  <name>{ID}</name>
  <uuid>{ID}</uuid>
  <title>all</title>
  <memory unit="KiB">4194304</memory>
  <vcpu>4</vcpu>
  <os>
    <type arch="x86_64" machine="q35">hvm</type>
    <loader readonly="yes" type="pflash" format="raw">/usr/share/OVMF/OVMF_CODE_4M.fd</loader>
    <nvram template="/usr/share/OVMF/OVMF_VARS_4M.fd" format="raw">/run/vm/nvram.fd</nvram>
    <boot dev="network"/>
    <boot dev="hd"/>
  </os>
  <cpu mode="custom"><model>Skylake-Client</model></cpu>
  <devices>
    <disk type="file" device="disk">
      <driver name="qemu" type="raw"/>
      <source file="/img/a.raw"/>
      <target dev="vdb" bus="scsi"/>
    </disk>
    <interface type="user"><model type="e1000e"/></interface>
    <interface type="bridge">
      <source bridge="vbrx"/>
      <model type="rtl8139"/>
      <mac address="52-54-00-aa-bb-02"/>
    </interface>
    <graphics type="spice" port="5901" autoport="yes" listen="0.0.0.0"/>
    <video>
      <model type="qxl" ram="65536" vgamem="16384" vram64="32768" heads="2"/>
    </video>
    <hostdev mode="subsystem" type="pci" managed="yes">
      <source><address domain="0x0000" bus="0x01" slot="0x00" function="0x0"/></source>
    </hostdev>
    <hostdev mode="subsystem" type="usb" managed="yes">
      <source><vendor id="0x1234"/><product id="0x5678"/></source>
    </hostdev>
    <rng model="virtio"><backend model="random">/dev/urandom</backend></rng>
    <memballoon model="virtio"/>
    <channel type="unix"><target type="virtio" name="org.qemu.guest_agent.0"/></channel>
    <tpm model="tpm-crb"><backend type="emulator" version="2.0"/></tpm>
    <watchdog model="i6300esb" action="poweroff"/>
    <serial type="null"/>
  </devices>
</domain>"#
        )
    }

    #[test]
    fn description_survives_a_save_and_clears_when_dropped() {
        let with = spa_bios().replace("</title>", "</title><description>a &amp; b\nline 2</description>");
        let d = parse(&with).unwrap();
        assert_eq!(d.description, "a & b\nline 2");
        assert_eq!(parse(&render(&d)).unwrap().description, d.description);
        assert_eq!(parse(&spa_bios()).unwrap().description, "");
    }

    #[test]
    fn the_spa_bios_definition_parses_and_round_trips() {
        let d = parse(&spa_bios()).unwrap();
        assert_eq!((d.id.as_str(), d.title.as_str()), (ID, "web & db"));
        assert_eq!((d.memory_mib, d.vcpus), (2048, 2));
        assert_eq!((d.arch.as_str(), d.machine.as_str()), ("x86_64", "q35"));
        assert_eq!(d.firmware, Firmware::Bios);
        assert_eq!(d.boot, [BootDev::Hd, BootDev::Cdrom]);
        assert_eq!(d.cpu, Cpu::Host);
        assert_eq!(d.disks.len(), 2);
        let disk = &d.disks[0];
        assert_eq!(
            (
                disk.device,
                disk.bus,
                disk.format,
                disk.target.as_str(),
                disk.readonly
            ),
            (
                DiskDevice::Disk,
                DiskBus::Virtio,
                DiskFormat::Qcow2,
                "vda",
                false
            )
        );
        assert_eq!(
            disk.source.as_deref(),
            Some(Path::new("/var/lib/virt-man/images/web.qcow2"))
        );
        assert_eq!(d.disks[1].device, DiskDevice::Cdrom);
        assert_eq!((d.disks[1].bus, d.disks[1].readonly), (DiskBus::Sata, true));
        let nic = &d.interfaces[0];
        assert_eq!(nic.kind, IfaceKind::Bridge("vbr5b92a1a54f00".into()));
        assert_eq!(nic.mac.as_deref(), Some("52:54:00:aa:bb:01"));
        assert_eq!(nic.bandwidth.inbound.unwrap().average, Some(1000));
        assert_eq!(nic.bandwidth.outbound, None);
        assert!(matches!(
            d.graphics,
            Some(Graphics::Vnc { ref listen, port: 5900 }) if listen == "127.0.0.1"
        ));
        let video = d.video.as_ref().unwrap();
        assert_eq!(
            (video.model, video.device.as_deref()),
            (VideoModel::Virtio, Some("virtio-vga"))
        );
        assert_eq!((video.ram_kib, video.edid), (None, Some(true)));
        assert_eq!(d.serial, Some(SerialKind::Pty));
        // The ignored annotations are gone; everything else survives.
        let back = parse(&render(&d)).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn the_spa_uefi_definition_keeps_loader_and_nvram() {
        let d = parse(&spa_uefi()).unwrap();
        assert_eq!(d.firmware, Firmware::Uefi);
        assert_eq!(
            d.loader.as_deref(),
            Some(Path::new("/usr/share/OVMF/OVMF_CODE_4M.fd"))
        );
        assert_eq!(
            d.nvram.as_deref(),
            Some(Path::new(&format!(
                "/var/lib/libvirt/qemu/nvram/{ID}_VARS.fd"
            )))
        );
        assert_eq!(
            d.nvram_template.as_deref(),
            Some(Path::new("/usr/share/OVMF/OVMF_VARS_4M.fd"))
        );
        assert_eq!(d.serial, None, "no serial in the definition");
        assert_eq!(parse(&render(&d)).unwrap(), d);
    }

    #[test]
    fn every_hardware_editor_device_round_trips() {
        let d = parse(&hardware()).unwrap();
        assert_eq!(d.cpu, Cpu::Model("Skylake-Client".into()));
        assert_eq!(d.boot, [BootDev::Network, BootDev::Hd]);
        assert_eq!(
            (d.disks[0].bus, d.disks[0].target.as_str()),
            (DiskBus::Scsi, "vdb")
        );
        assert_eq!(d.interfaces.len(), 2);
        assert_eq!(d.interfaces[0].kind, IfaceKind::User);
        assert_eq!(d.interfaces[0].model, IfaceModel::E1000e);
        assert_eq!(d.interfaces[0].mac, None);
        assert_eq!(d.interfaces[1].model, IfaceModel::Rtl8139);
        assert_eq!(d.interfaces[1].mac.as_deref(), Some("52:54:00:aa:bb:02"));
        assert!(matches!(
            d.graphics,
            Some(Graphics::Spice { ref listen, port: 5901 }) if listen == "0.0.0.0"
        ));
        let v = d.video.as_ref().unwrap();
        assert_eq!(v.model, VideoModel::Qxl);
        assert_eq!(
            (v.ram_kib, v.vgamem_kib, v.vram64_kib, v.heads),
            (Some(65536), Some(16384), Some(32768), Some(2))
        );
        assert_eq!(
            d.hostdevs,
            [
                Hostdev::Pci {
                    domain: 0,
                    bus: 1,
                    slot: 0,
                    function: 0
                },
                Hostdev::Usb {
                    vendor: 0x1234,
                    product: 0x5678
                }
            ]
        );
        assert_eq!(d.rng.as_deref(), Some(Path::new("/dev/urandom")));
        assert!(d.memballoon && d.guest_agent);
        assert_eq!(d.tpm.as_ref().unwrap().version, "2.0");
        assert_eq!(
            d.watchdog.as_ref().unwrap(),
            &Watchdog {
                model: "i6300esb".into(),
                action: "poweroff".into()
            }
        );
        assert_eq!(d.serial, Some(SerialKind::Null));
        assert_eq!(parse(&render(&d)).unwrap(), d);
    }

    #[test]
    fn a_virtio_3d_video_must_match_its_device() {
        let gl = format!(
            r#"<domain type="kvm"><name>{ID}</name><uuid>{ID}</uuid>
  <memory unit="KiB">1048576</memory><vcpu>1</vcpu>
  <os><type arch="x86_64" machine="q35">hvm</type></os>
  <devices><video><model type="virtio" device="virtio-vga-gl"><acceleration accel3d="yes"/></model></video></devices>
</domain>"#
        );
        let d = parse(&gl).unwrap();
        assert!(d.video.as_ref().unwrap().accel3d);
        assert_eq!(parse(&render(&d)).unwrap(), d);

        // A -gl device without the flag (or the flag on a plain device) is a
        // mismatch QEMU refuses at start; refuse it at define instead.
        let bad = gl.replace("<acceleration accel3d=\"yes\"/>", "");
        assert!(parse(&bad).unwrap_err().contains("3D"));
        let bad = gl.replace("virtio-vga-gl", "virtio-vga");
        assert!(parse(&bad).unwrap_err().contains("3D"));
    }

    #[test]
    fn unsupported_constructs_are_refused_by_name() {
        let base = |devices: &str| {
            format!(
                r#"<domain type="kvm"><name>{ID}</name><uuid>{ID}</uuid>
  <memory unit="KiB">1048576</memory><vcpu>1</vcpu>
  <os><type arch="x86_64" machine="q35">hvm</type></os>
  <devices>{devices}</devices></domain>"#
            )
        };
        let cases = [
            (
                r#"<interface type="vhostuser"><source type="unix" path="/run/x"/><model type="virtio"/></interface>"#,
                "vhostuser",
            ),
            (
                r#"<interface type="network"><source network="default"/><model type="virtio"/></interface>"#,
                "network",
            ),
            (
                r#"<hostdev mode="subsystem" type="mdev"><source><address uuid="x"/></source></hostdev>"#,
                "mediated",
            ),
            (
                r#"<hostdev mode="subsystem" type="scsi"><source/></hostdev>"#,
                "hostdev type",
            ),
            (
                r#"<controller type="usb" index="0" model="qemu-xhci"/>"#,
                "controller type",
            ),
            (
                r#"<filesystem type="mount"><source dir="/x"/></filesystem>"#,
                "not supported",
            ),
            (
                r#"<disk type="block" device="disk"><source dev="/dev/sda"/><target dev="vda" bus="virtio"/></disk>"#,
                "disk type",
            ),
            (
                r#"<disk type="volume" device="disk"><source pool="p" volume="v"/><target dev="vda" bus="virtio"/></disk>"#,
                "absolute path",
            ),
            (r#"<graphics type="sdl"/>"#, "vnc or spice"),
            (r#"<graphics type="vnc" autoport="yes"/>"#, "explicit port"),
            (r#"<video><model type="gpu"/></video>"#, "video model"),
            (
                r#"<video><model type="virtio" device="virtio-vga" vram="65536"/></video>"#,
                "vram",
            ),
            (
                r#"<channel type="unix"><target type="virtio" name="org.spice.spicevmc"/></channel>"#,
                "guest-agent",
            ),
            (
                r#"<tpm model="tpm-tis"><backend type="emulator" version="2.0"/></tpm>"#,
                "tpm-crb",
            ),
            (
                r#"<watchdog model="diag288" action="reset"/>"#,
                "watchdog model",
            ),
            (
                r#"<serial type="tcp"><source mode="bind"/></serial>"#,
                "serial type",
            ),
        ];
        for (devices, needle) in cases {
            let e = parse(&base(devices)).unwrap_err();
            assert!(
                e.contains(needle),
                "expected '{needle}' in the error for {devices}: {e}"
            );
        }
    }

    #[test]
    fn a_definition_that_is_not_a_direct_qemu_guest_is_refused() {
        let base = format!(
            r#"<domain type="kvm"><name>{ID}</name><uuid>{ID}</uuid>
  <memory unit="KiB">1048576</memory><vcpu>1</vcpu>
  <os><type arch="x86_64" machine="q35">hvm</type></os><devices/></domain>"#
        );
        // Not XML.
        assert!(parse("<domain>").is_err());
        // A non-UUID name.
        assert!(
            parse(&base.replace(ID, "web"))
                .unwrap_err()
                .contains("canonical UUID")
        );
        // A different uuid.
        assert!(
            parse(&base.replace("<uuid>550e", "<uuid>650e"))
                .unwrap_err()
                .contains("does not match")
        );
        // No memory, no vcpus, zero vcpus.
        assert!(parse(&base.replace("<memory unit=\"KiB\">1048576</memory>", "")).is_err());
        assert!(parse(&base.replace("<vcpu>1</vcpu>", "<vcpu>0</vcpu>")).is_err());
        // A non-x86 architecture.
        assert!(
            parse(&base.replace("arch=\"x86_64\"", "arch=\"aarch64\""))
                .unwrap_err()
                .contains("architecture")
        );
        // A disk source that is not absolute.
        let relative = base.replace(
            "<devices/>",
            r#"<devices><disk type="file" device="disk"><source file="disk.qcow2"/><target dev="vda" bus="virtio"/></disk></devices>"#,
        );
        assert!(parse(&relative).unwrap_err().contains("absolute"));
        // The same target twice.
        let twice = base.replace(
            "<devices/>",
            r#"<devices><disk type="file" device="disk"><source file="/a"/><target dev="vda" bus="virtio"/></disk>
            <disk type="file" device="disk"><source file="/b"/><target dev="vda" bus="virtio"/></disk></devices>"#,
        );
        assert!(parse(&twice).unwrap_err().contains("target"));
        // Two video adapters.
        let videos = base.replace(
            "<devices/>",
            r#"<devices><video><model type="vga"/></video><video><model type="cirrus"/></video></devices>"#,
        );
        assert!(parse(&videos).unwrap_err().contains("one <video>"));
        // The same PCI device assigned twice (an editor append, not hardware).
        let hostdev = r#"<hostdev mode="subsystem" type="pci"><source><address domain="0x0000" bus="0x01" slot="0x00" function="0x0"/></source></hostdev>"#;
        let twice = base.replace(
            "<devices/>",
            &format!("<devices>{hostdev}{hostdev}</devices>"),
        );
        assert!(parse(&twice).unwrap_err().contains("assigned twice"));
    }

    #[test]
    fn merge_keeps_the_identity_and_refuses_a_rename() {
        let old = spa_bios();
        let edited = old
            .replace("<title>web &amp; db</title>", "")
            .replace("<vcpu>2</vcpu>", "<vcpu>4</vcpu>")
            .replace(&format!("<name>{ID}</name>\n  <uuid>{ID}</uuid>"), "");
        let d = merge(&old, &edited).unwrap();
        assert_eq!(d.id, ID);
        assert_eq!(d.title, "web & db", "the editor may omit the title");
        assert_eq!(d.vcpus, 4);

        let renamed = old.replace(&format!("<name>{ID}</name>"), "<name>web</name>");
        assert!(merge(&old, &renamed).unwrap_err().contains("cannot change"));
    }

    #[test]
    fn merge_keeps_a_mac_the_editor_dropped() {
        let old = spa_bios();
        let edited = old.replace("      <mac address=\"52:54:00:aa:bb:01\"/>\n", "");
        assert_eq!(
            merge(&old, &edited).unwrap().interfaces[0].mac.as_deref(),
            Some("52:54:00:aa:bb:01")
        );
    }

    #[test]
    fn bridges_are_read_from_the_definition() {
        let d = hardware();
        assert_eq!(
            bridges(&d).unwrap(),
            [BridgeNic {
                mac: "52:54:00:aa:bb:02".into(),
                bridge: "vbrx".into()
            }],
            "a user-mode NIC has no bridge"
        );
    }
}
