//! A container's definition: the SPA's libvirt-shaped LXC domain XML.
//!
//! The container pages always spoke `<domain type='lxc'>`, and the web server's
//! allowlist (`xmlcheck.rs`) is written against that shape, so it stays the
//! durable definition: `parse` reads it into a [`Container`], the LXC manager
//! renders an LXC config from that at every start, and `render` writes it back
//! for `GET .../xml`. Nothing here is handed to libvirt.
//!
//! What LXC is not given is refused rather than dropped: a second filesystem
//! (a bind mount of the host), a disk, a hostdev, a non-bridge interface, an
//! init argument list, an unknown feature. Dropping one would define a
//! container that does not match what the page says it has -- and a bind
//! mount is exactly what an unprivileged container must never be handed.

use roxmltree::Node;

use crate::ct::{ID_COUNT, mount_dir, valid_base};
use crate::domainxml::{
    Bandwidth, esc, memory_mib, normalize_mac, parse_bandwidth, render_bandwidth,
};

/// The metadata namespace a container's definition carries its rootfs in.
pub const META_NS: &str = "https://virt-man.invalid/container/1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Nic {
    /// Canonical lower-case form. `lxc::Manager` assigns one to a NIC that has
    /// none when it stores the definition, so the IPAM store always sees it.
    pub mac: Option<String>,
    /// The Linux bridge the host side of the veth joins.
    pub bridge: String,
    pub bandwidth: Bandwidth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    /// `<name>` and `<uuid>`: the manager's key, never the display name.
    pub id: String,
    pub title: String,
    pub description: String,
    pub memory_mib: u32,
    /// Swap beyond memory, in MiB. libvirt spelled it `swap_hard_limit`
    /// (memory + swap); cgroup v2 limits swap on its own (`memory.swap.max`).
    pub swap_mib: u32,
    pub vcpus: u32,
    /// CPU cap in whole-CPU percent (150 = one and a half CPUs); 0 = none.
    pub cpu_limit_pct: u32,
    pub init: String,
    /// The base of the uid/gid range the container's root maps to; `None` is
    /// a privileged container.
    pub id_base: Option<u32>,
    /// The raw ext4 image in a storage pool the rootfs is mounted from.
    pub rootfs: String,
    pub nics: Vec<Nic>,
}

impl Container {
    pub fn privileged(&self) -> bool {
        self.id_base.is_none()
    }

    /// The uid the mapping gives the container's root: its range base, or 0
    /// for a privileged container.
    pub fn root_uid(&self) -> u32 {
        self.id_base.unwrap_or(0)
    }

    pub fn name(&self) -> &str {
        if self.title.is_empty() {
            &self.id
        } else {
            &self.title
        }
    }
}

/// The CFS period a CPU cap is expressed against, in microseconds.
pub const CPU_PERIOD: u64 = 100_000;

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}

fn text(n: Node) -> String {
    n.text().unwrap_or("").trim().to_string()
}

fn number<T: std::str::FromStr>(n: Node, what: &str) -> Result<T, String> {
    text(n)
        .parse()
        .map_err(|_| format!("<{what}> must be a number"))
}

pub fn parse(xml: &str) -> Result<Container, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("invalid XML: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "domain" || root.attribute("type") != Some("lxc") {
        return Err(
            "a container's definition must stay an LXC domain (<domain type='lxc'>)".into(),
        );
    }
    let name = child(root, "name").map(text).unwrap_or_default();
    let uuid = child(root, "uuid").map(text).unwrap_or_default();
    let id = match (name.as_str(), uuid.as_str()) {
        ("", "") => return Err("the definition has no <uuid>".into()),
        (n, "") => n.to_string(),
        ("", u) => u.to_string(),
        (n, u) if n == u => u.to_string(),
        (n, u) => {
            return Err(format!(
                "<name> '{n}' does not match <uuid> '{u}': a container is named by its UUID"
            ));
        }
    };
    let mut c = Container {
        id,
        title: child(root, "title").map(text).unwrap_or_default(),
        description: child(root, "description").map(text).unwrap_or_default(),
        memory_mib: memory_mib(child(root, "memory").ok_or("the definition has no <memory>")?)?,
        swap_mib: 0,
        vcpus: 1,
        cpu_limit_pct: 0,
        init: "/sbin/init".into(),
        id_base: None,
        rootfs: String::new(),
        nics: Vec::new(),
    };
    for e in root.children().filter(Node::is_element) {
        match e.tag_name().name() {
            "name" | "uuid" | "title" | "description" | "memory" | "devices" => {}
            // libvirt bookkeeping a container has no use for.
            "currentMemory" | "on_poweroff" | "on_reboot" | "on_crash" | "clock" | "resource" => {}
            "metadata" => {
                for m in e.children().filter(Node::is_element) {
                    if m.tag_name().namespace() == Some(META_NS)
                        && m.tag_name().name() == "container"
                    {
                        c.rootfs = m.attribute("rootfs").unwrap_or("").to_string();
                    }
                }
            }
            "memtune" => {
                for t in e.children().filter(Node::is_element) {
                    match t.tag_name().name() {
                        "swap_hard_limit" => {
                            let hard = memory_mib(t)?;
                            c.swap_mib = hard.saturating_sub(c.memory_mib);
                        }
                        other => return Err(format!("<memtune><{other}> is not supported")),
                    }
                }
            }
            "vcpu" => {
                c.vcpus = number(e, "vcpu")?;
                if !(1..=256).contains(&c.vcpus) {
                    return Err("<vcpu> must be 1-256".into());
                }
            }
            "cputune" => {
                let mut period = CPU_PERIOD as i64;
                let mut quota = -1i64;
                for t in e.children().filter(Node::is_element) {
                    match t.tag_name().name() {
                        "period" => period = number(t, "period")?,
                        "quota" => quota = number(t, "quota")?,
                        "shares" => {}
                        other => return Err(format!("<cputune><{other}> is not supported")),
                    }
                }
                if period <= 0 {
                    return Err("<cputune><period> must be positive".into());
                }
                if quota > 0 {
                    c.cpu_limit_pct = u32::try_from(quota * 100 / period)
                        .map_err(|_| "<cputune><quota> is too large".to_string())?
                        .max(1);
                }
            }
            "features" => {
                for f in e.children().filter(Node::is_element) {
                    // A container always has its own network namespace; the
                    // flag is accepted because the editor has always written it.
                    if f.tag_name().name() != "privnet" {
                        return Err(format!(
                            "container feature <{}> is not supported",
                            f.tag_name().name()
                        ));
                    }
                }
            }
            "os" => {
                for o in e.children().filter(Node::is_element) {
                    match o.tag_name().name() {
                        "type" if text(o) == "exe" => {}
                        "type" => return Err("a container's <os><type> is 'exe'".into()),
                        "init" => c.init = text(o),
                        other => return Err(format!("<os><{other}> is not supported")),
                    }
                }
                if !c.init.starts_with('/') || c.init.contains(['\n', '\r', '\0']) {
                    return Err("<init> must be an absolute path".into());
                }
            }
            "idmap" => c.id_base = Some(parse_idmap(e)?),
            other => return Err(format!("domain element <{other}> is not supported")),
        }
    }
    if c.rootfs.is_empty() || !c.rootfs.starts_with('/') {
        return Err("this container records no rootfs image".into());
    }
    let devices = child(root, "devices").ok_or("the definition has no <devices>")?;
    let mut filesystems = 0;
    for d in devices.children().filter(Node::is_element) {
        match d.tag_name().name() {
            "filesystem" => {
                filesystems += 1;
                check_rootfs(d, &c.id)?;
            }
            "interface" => c.nics.push(parse_nic(d)?),
            "console" => {
                if d.attribute("type").is_some_and(|t| t != "pty") {
                    return Err("a container's console is a pty".into());
                }
            }
            other => {
                return Err(format!("container device <{other}> is not supported"));
            }
        }
    }
    if filesystems != 1 {
        return Err(format!(
            "a container has exactly one filesystem, its rootfs at {}",
            mount_dir(&c.id).display()
        ));
    }
    let mut macs = std::collections::BTreeSet::new();
    for n in &c.nics {
        if let Some(m) = &n.mac
            && !macs.insert(m.clone())
        {
            return Err(format!("MAC {m} is used twice"));
        }
    }
    if c.nics.len() > 16 {
        return Err("a container has at most 16 interfaces".into());
    }
    Ok(c)
}

/// One mapping for uids and the same for gids, of the container's whole id
/// space onto a block only this daemon hands out. Anything else is refused:
/// `target='0'` would make "unprivileged" root host root, and a partial map
/// leaves files owned by ids the container cannot name.
fn parse_idmap(e: Node) -> Result<u32, String> {
    let mut uid = None;
    let mut gid = None;
    for m in e.children().filter(Node::is_element) {
        let slot = match m.tag_name().name() {
            "uid" => &mut uid,
            "gid" => &mut gid,
            other => return Err(format!("<idmap><{other}> is not supported")),
        };
        if slot.is_some() {
            return Err("<idmap> maps one range for uids and one for gids".into());
        }
        let attr = |a: &str| -> Result<u32, String> {
            m.attribute(a)
                .and_then(|v| v.trim().parse().ok())
                .ok_or_else(|| format!("<idmap><{}> needs a numeric {a}", m.tag_name().name()))
        };
        if attr("start")? != 0 || attr("count")? != ID_COUNT {
            return Err(format!(
                "<idmap> maps the container's ids 0-{} as one range",
                ID_COUNT - 1
            ));
        }
        *slot = Some(attr("target")?);
    }
    match (uid, gid) {
        (Some(u), Some(g)) if u == g && valid_base(u) => Ok(u),
        (Some(_), Some(_)) => {
            Err("<idmap> must map uids and gids onto the same range this daemon allocated".into())
        }
        _ => Err("<idmap> needs both a <uid> and a <gid> mapping".into()),
    }
}

/// The one filesystem: the daemon's mount of the rootfs image, as `/`.
fn check_rootfs(d: Node, id: &str) -> Result<(), String> {
    let dir = mount_dir(id);
    let source = child(d, "source").and_then(|s| s.attribute("dir"));
    let target = child(d, "target").and_then(|t| t.attribute("dir"));
    let ty = d.attribute("type").unwrap_or("mount");
    if ty != "mount" || source != dir.to_str() || target != Some("/") {
        return Err(format!(
            "a container has exactly one filesystem, its rootfs at {}",
            dir.display()
        ));
    }
    Ok(())
}

fn parse_nic(d: Node) -> Result<Nic, String> {
    if d.attribute("type") != Some("bridge") {
        return Err("a container's interface joins a bridge (<interface type='bridge'>)".into());
    }
    let bridge = child(d, "source")
        .and_then(|s| s.attribute("bridge"))
        .ok_or("an interface needs <source bridge=...>")?
        .to_string();
    if !crate::hostnet::valid_name(&bridge) {
        return Err(format!("'{bridge}' is not a bridge name"));
    }
    let mac = child(d, "mac")
        .and_then(|m| m.attribute("address"))
        .map(normalize_mac)
        .transpose()?;
    let bandwidth = match child(d, "bandwidth") {
        Some(b) => parse_bandwidth(b)?,
        None => Bandwidth::default(),
    };
    for e in d.children().filter(Node::is_element) {
        // `<model>` and `<target>` are what libvirt wrote back; a veth has
        // neither a model nor a name the definition chooses.
        if !matches!(
            e.tag_name().name(),
            "source" | "mac" | "bandwidth" | "model" | "target" | "alias"
        ) {
            return Err(format!(
                "interface element <{}> is not supported",
                e.tag_name().name()
            ));
        }
    }
    Ok(Nic {
        mac,
        bridge,
        bandwidth,
    })
}

/// The definition `GET .../xml` returns and the manager stores. Every element
/// round-trips through `parse`.
pub fn render(c: &Container) -> String {
    let mut s = format!(
        "<domain type='lxc'>\n  <name>{id}</name>\n  <uuid>{id}</uuid>\n",
        id = esc(&c.id)
    );
    if !c.title.is_empty() {
        s.push_str(&format!("  <title>{}</title>\n", esc(&c.title)));
    }
    if !c.description.is_empty() {
        s.push_str(&format!(
            "  <description>{}</description>\n",
            esc(&c.description)
        ));
    }
    s.push_str(&format!(
        "  <metadata>\n    <virtman:container xmlns:virtman='{META_NS}' rootfs='{}'/>\n  </metadata>\n",
        esc(&c.rootfs)
    ));
    s.push_str(&format!("  <memory unit='MiB'>{}</memory>\n", c.memory_mib));
    if c.swap_mib > 0 {
        s.push_str(&format!(
            "  <memtune>\n    <swap_hard_limit unit='MiB'>{}</swap_hard_limit>\n  </memtune>\n",
            u64::from(c.memory_mib) + u64::from(c.swap_mib)
        ));
    }
    s.push_str(&format!("  <vcpu>{}</vcpu>\n", c.vcpus));
    if c.cpu_limit_pct > 0 {
        s.push_str(&format!(
            "  <cputune>\n    <period>{CPU_PERIOD}</period>\n    <quota>{}</quota>\n  </cputune>\n",
            u64::from(c.cpu_limit_pct) * CPU_PERIOD / 100
        ));
    }
    s.push_str("  <features>\n    <privnet/>\n  </features>\n");
    s.push_str(&format!(
        "  <os>\n    <type>exe</type>\n    <init>{}</init>\n  </os>\n",
        esc(&c.init)
    ));
    if let Some(b) = c.id_base {
        s.push_str(&format!(
            "  <idmap>\n    <uid start='0' target='{b}' count='{ID_COUNT}'/>\n    <gid start='0' target='{b}' count='{ID_COUNT}'/>\n  </idmap>\n"
        ));
    }
    s.push_str("  <devices>\n");
    s.push_str(&format!(
        "    <filesystem type='mount' accessmode='passthrough'>\n      <source dir='{}'/>\n      <target dir='/'/>\n    </filesystem>\n",
        esc(&mount_dir(&c.id).to_string_lossy())
    ));
    for n in &c.nics {
        s.push_str("    <interface type='bridge'>\n");
        if let Some(m) = &n.mac {
            s.push_str(&format!("      <mac address='{}'/>\n", esc(m)));
        }
        s.push_str(&format!("      <source bridge='{}'/>\n", esc(&n.bridge)));
        let bw = render_bandwidth(&n.bandwidth);
        if !bw.is_empty() {
            s.push_str(&format!("      {bw}\n"));
        }
        s.push_str("    </interface>\n");
    }
    s.push_str("    <console type='pty'/>\n  </devices>\n</domain>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ct::ID_BASE;
    use crate::domainxml::Rate;

    const ID: &str = "0b776cc0-f6b1-4bbd-b67b-88ccc6c24a76";

    fn sample() -> Container {
        Container {
            id: ID.into(),
            title: "web <1>".into(),
            description: String::new(),
            memory_mib: 512,
            swap_mib: 256,
            vcpus: 2,
            cpu_limit_pct: 150,
            init: "/sbin/init".into(),
            id_base: Some(ID_BASE),
            rootfs: "/p/ct-x.raw".into(),
            nics: vec![Nic {
                mac: Some("52:54:00:aa:bb:01".into()),
                bridge: "vbr0123456789ab".into(),
                bandwidth: Bandwidth {
                    inbound: Some(Rate {
                        average: Some(1250),
                        ..Default::default()
                    }),
                    outbound: None,
                },
            }],
        }
    }

    #[test]
    fn a_definition_round_trips() {
        let c = sample();
        let x = render(&c);
        assert!(x.contains("<title>web &lt;1&gt;</title>"), "{x}");
        assert!(x.contains(&format!("target='{ID_BASE}'")), "{x}");
        assert!(x.contains("<swap_hard_limit unit='MiB'>768<"), "{x}");
        assert!(x.contains("<quota>150000</quota>"), "{x}");
        assert_eq!(parse(&x).unwrap(), c);
        let privileged = Container {
            id_base: None,
            nics: vec![],
            swap_mib: 0,
            cpu_limit_pct: 0,
            ..c
        };
        let x = render(&privileged);
        assert!(
            !x.contains("<idmap"),
            "privileged is the absence of an idmap"
        );
        assert!(parse(&x).unwrap().privileged());
    }

    /// What the old libvirt-backed daemon stored and the web server's
    /// allowlist is written against still parses.
    #[test]
    fn the_libvirt_shape_the_editor_writes_is_read() {
        let x = format!(
            "<domain type='lxc'><name>{ID}</name><uuid>{ID}</uuid><title>t</title>\
<metadata><virtman:container xmlns:virtman='{META_NS}' rootfs='/p/u.raw'/></metadata>\
<memory unit='KiB'>524288</memory><currentMemory unit='KiB'>524288</currentMemory>\
<vcpu placement='static'>1</vcpu><features><privnet/></features>\
<os><type arch='x86_64'>exe</type><init>/sbin/init</init></os>\
<idmap><uid start='0' target='{ID_BASE}' count='65536'/><gid start='0' target='{ID_BASE}' count='65536'/></idmap>\
<on_poweroff>destroy</on_poweroff>\
<devices><filesystem type='mount' accessmode='passthrough'><source dir='/run/virt-man-daemon/ct/{ID}'/><target dir='/'/></filesystem>\
<interface type='bridge'><mac address='52:54:00:AA:BB:02'/><source bridge='vbr1'/><target dev='veth0'/></interface>\
<console type='pty'><target type='lxc' port='0'/></console></devices></domain>"
        );
        let c = parse(&x).unwrap();
        assert_eq!((c.memory_mib, c.vcpus, c.id_base), (512, 1, Some(ID_BASE)));
        assert_eq!(c.nics[0].mac.as_deref(), Some("52:54:00:aa:bb:02"));
        assert_eq!(c.rootfs, "/p/u.raw");
    }

    #[test]
    fn what_lxc_is_not_given_is_refused() {
        let good = render(&sample());
        for (what, bad) in [
            (
                "a host bind mount",
                good.replace(
                    "    <console",
                    "    <filesystem type='mount'><source dir='/'/><target dir='/host'/></filesystem>\n    <console",
                ),
            ),
            (
                "the rootfs moved",
                good.replace(&format!("/run/virt-man-daemon/ct/{ID}"), "/etc"),
            ),
            ("a kvm domain", good.replace("type='lxc'", "type='kvm'")),
            (
                "a disk",
                good.replace("    <console", "    <disk type='file'/>\n    <console"),
            ),
            (
                "an idmap onto host root",
                good.replace(&format!("target='{ID_BASE}'"), "target='0'"),
            ),
            (
                "split uid and gid ranges",
                good.replacen(&format!("target='{ID_BASE}'"), &format!("target='{}'", ID_BASE + ID_COUNT), 1),
            ),
            (
                "a partial map",
                good.replace("count='65536'", "count='1000'"),
            ),
            (
                "no rootfs record",
                good.replace("rootfs='/p/ct-x.raw'", ""),
            ),
            ("a network interface", good.replace("type='bridge'", "type='network'")),
            (
                "a name that is not the uuid",
                good.replace(&format!("<name>{ID}</name>"), "<name>other</name>"),
            ),
            ("a relative init", good.replace("<init>/sbin/init", "<init>init")),
        ] {
            assert!(parse(&bad).is_err(), "{what}: {bad}");
        }
    }
}
