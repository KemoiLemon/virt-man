//! Disk-source parsing and the safety rules around deleting storage.
//!
//! Deletion is irreversible, so two things are deliberately refused here:
//!
//!   * **An ISO is never deleted with its guest.** An ISO shows up as a cdrom
//!     device, and it is boot media the user reused across guests — not the
//!     guest's own storage. Deleting a VM used to take its attached ISO with
//!     it. A readonly or `.iso`-suffixed source is treated the same way, since
//!     losing boot media is not something the user can undo.
//!   * **A disk another domain still references is never deleted.** Removing it
//!     would corrupt that guest's next boot. The guest's own definition is
//!     already gone by then, so the check is against *every other* domain.
//!
//! Shared by the domain-delete and volume-delete paths so the two cannot
//! disagree about what is safe to remove.

use std::collections::HashSet;
use virt::connect::Connect;
use virt::storage_vol::StorageVol;

/// A disk's backing store, from the domain XML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskSource {
    /// File-backed disk: addressed by absolute path.
    Path(String),
    /// Volume-backed disk: addressed by pool + volume name.
    Volume { pool: String, volume: String },
}

impl DiskSource {
    /// Stable identity for "is this the same backing store?" comparisons.
    /// Pool/volume names are case-sensitive in libvirt, so they compare as
    /// written; paths are compared lexically.
    pub fn label(&self) -> String {
        match self {
            DiskSource::Path(p) => format!("file:{p}"),
            DiskSource::Volume { pool, volume } => format!("volume:{pool}/{volume}"),
        }
    }

    /// Human-readable form, for diagnostics and API responses.
    pub fn describe(&self) -> String {
        match self {
            DiskSource::Path(p) => p.clone(),
            DiskSource::Volume { pool, volume } => format!("{pool}/{volume}"),
        }
    }
}

/// The absolute path behind a pool+volume reference, when libvirt can resolve
/// it.
fn volume_path(c: &Connect, pool: &str, volume: &str) -> Option<String> {
    let p = crate::libvirt::pool(c, pool).ok()?;
    let v = StorageVol::lookup_by_name(&p, volume).ok()?;
    v.get_path().ok().filter(|s| !s.is_empty())
}

/// Every label a reference answers to: its own, plus the file label of the
/// same backing store when it is a volume.
///
/// Both forms mean one file, and guests defined before paths were used still
/// name their disks as pool+volume. Without this, deleting a volume would
/// happily remove a running guest's disk because that guest spells it another
/// way — the exact corruption the in-use check exists to prevent. Volume
/// lookups cost a libvirt round trip, so this is only for the delete paths.
fn labels(c: &Connect, source: &DiskSource) -> Vec<String> {
    let mut out = vec![source.label()];
    if let DiskSource::Volume { pool, volume } = source
        && let Some(path) = volume_path(c, pool, volume)
    {
        out.push(DiskSource::Path(path).label());
    }
    out
}

/// Why a disk was deliberately left on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// Attached as a cdrom: an ISO, i.e. shared boot media.
    Iso,
    /// Attached read-only, so treat it as media rather than guest storage.
    ReadOnly,
    /// Another domain still references it.
    InUse,
}

impl KeepReason {
    /// Machine-readable token for the API response. The UI turns this into
    /// human text, so the wire format stays stable for other clients.
    pub fn as_str(self) -> &'static str {
        match self {
            KeepReason::Iso => "iso",
            KeepReason::ReadOnly => "readonly",
            KeepReason::InUse => "in_use",
        }
    }
}

/// One `<disk>` reference: its backing store, plus the attributes that decide
/// whether it is the guest's own storage or shared media.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskRef {
    pub source: DiskSource,
    /// The `device` attribute: `disk`, `cdrom`, `floppy`, …
    pub device: String,
    /// Whether the disk carries a `<readonly/>` marker.
    pub readonly: bool,
}

/// Read one attribute's value out of a tag fragment, single- or double-quoted.
/// Matches the name as a whole token so a value like `some-file.iso` cannot
/// satisfy a search for `file`.
pub fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(rel) = tag[from..].find(name) {
        let i = from + rel;
        from = i + name.len();
        // The character before the name must be whitespace, so `file` does not
        // match the tail of `somefile`.
        let prev_ok = tag[..i]
            .chars()
            .next_back()
            .is_none_or(|ch| ch.is_whitespace());
        if !prev_ok {
            continue;
        }
        let rest = tag[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let quote = rest.chars().next()?;
        if quote != '\'' && quote != '"' {
            continue;
        }
        let inner = &rest[quote.len_utf8()..];
        let j = inner.find(quote)?;
        return Some(&inner[..j]);
    }
    None
}

/// Every disk reference in a domain's XML, whatever its device type.
///
/// The in-use check needs *all* of them: a source another guest happens to have
/// attached as a cdrom still means the file is referenced.
pub fn disk_refs(xml: &str) -> Vec<DiskRef> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<disk") {
        let block = &rest[i..];
        let Some(end) = block.find("</disk>") else {
            break;
        };
        let disk = &block[..end];
        let open_end = disk.find('>').map(|e| e + 1).unwrap_or(disk.len());
        let open_tag = &disk[..open_end];
        let device = attr(open_tag, "device").unwrap_or("disk").to_string();
        let readonly = disk.contains("<readonly");
        if let Some(si) = disk.find("<source") {
            let source = &disk[si..];
            // Up to the closing '>' of the <source …> element, so attr() reads
            // attributes from the right tag even when it is self-closing.
            let src_end = source.find('>').map(|e| &source[..=e]).unwrap_or(source);
            let parsed = if let Some(path) = attr(src_end, "file") {
                Some(DiskSource::Path(path.to_string()))
            } else if let (Some(pool), Some(volume)) =
                (attr(src_end, "pool"), attr(src_end, "volume"))
            {
                Some(DiskSource::Volume {
                    pool: pool.to_string(),
                    volume: volume.to_string(),
                })
            } else {
                None
            };
            if let Some(source) = parsed {
                out.push(DiskRef {
                    source,
                    device,
                    readonly,
                });
            }
        }
        rest = &block[end + 7..];
    }
    out
}

/// Whether a reference is the guest's own storage, as opposed to media.
fn is_guest_storage(r: &DiskRef) -> Option<KeepReason> {
    if r.device != "disk" {
        return Some(KeepReason::Iso);
    }
    if r.readonly {
        return Some(KeepReason::ReadOnly);
    }
    let name = match &r.source {
        DiskSource::Path(p) => p.as_str(),
        DiskSource::Volume { volume, .. } => volume.as_str(),
    };
    if name.to_ascii_lowercase().ends_with(".iso") {
        return Some(KeepReason::Iso);
    }
    None
}

/// What a cleanup would do: what it may remove, and what it must leave alone.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cleanup {
    /// Backing stores safe to delete.
    pub removable: Vec<DiskSource>,
    /// `(description, reason)` for everything deliberately kept.
    pub kept: Vec<(String, KeepReason)>,
}

/// Decide what may be removed for one domain's definition.
///
/// `used_elsewhere` holds the labels still referenced by other domains, as
/// produced by [`sources_used_elsewhere`]. `expand` maps a source to every
/// label it answers to, which is what makes the two ways of naming one file
/// comparable — a guest holding a disk by path and one holding it as
/// pool+volume must count as the same backing store, or a delete would take a
/// file that is still in use. Tests pass [`as_written`].
pub fn plan_cleanup(
    xml: &str,
    used_elsewhere: &HashSet<String>,
    expand: impl Fn(&DiskSource) -> Vec<String>,
) -> Cleanup {
    let mut plan = Cleanup::default();
    for r in disk_refs(xml) {
        if let Some(reason) = is_guest_storage(&r) {
            plan.kept.push((r.source.describe(), reason));
            continue;
        }
        if expand(&r.source).iter().any(|l| used_elsewhere.contains(l)) {
            plan.kept.push((r.source.describe(), KeepReason::InUse));
            continue;
        }
        plan.removable.push(r.source);
    }
    plan
}

/// [`plan_cleanup`] against a live connection, so a volume reference and the
/// file it points at are recognised as one backing store.
pub fn plan_cleanup_connected(c: &Connect, xml: &str, used_elsewhere: &HashSet<String>) -> Cleanup {
    plan_cleanup(xml, used_elsewhere, |s| labels(c, s))
}

/// Every backing store referenced by a domain other than `excluding`.
///
/// Both the running and the persistent definition are read: a guest that is
/// currently running holds its disks even if the persistent definition was
/// just removed.
pub fn sources_used_elsewhere(c: &Connect, excluding: Option<&str>) -> HashSet<String> {
    let mut used = HashSet::new();
    let Ok(domains) = c.list_all_domains(0) else {
        // Cannot enumerate: report nothing as in use. The caller's own ISO and
        // read-only guards still apply, and a failure here must not block a
        // legitimate delete.
        return used;
    };
    for d in domains {
        let name = d.get_name().unwrap_or_default();
        if Some(name.as_str()) == excluding {
            continue;
        }
        if let Ok(xml) = d.get_xml_desc(0) {
            for r in disk_refs(&xml) {
                used.extend(labels(c, &r.source));
            }
        }
    }
    used
}

/// Every disk label (volume reference and path) mapped to the guests that
/// hold it, built from one pass over the domains. For a volume list: asking
/// `referencing_domains` per volume walks every guest once per volume.
pub fn holders_by_label(c: &Connect) -> std::collections::HashMap<String, Vec<String>> {
    let mut out: std::collections::HashMap<String, Vec<String>> = Default::default();
    let Ok(domains) = c.list_all_domains(0) else {
        return out;
    };
    for d in domains {
        let (Ok(xml), name) = (d.get_xml_desc(0), d.get_name().unwrap_or_default()) else {
            continue;
        };
        for r in disk_refs(&xml) {
            for l in labels(c, &r.source) {
                let v = out.entry(l).or_default();
                if !v.contains(&name) {
                    v.push(name.clone());
                }
            }
        }
    }
    out
}

/// The guests holding a volume, given `holders_by_label`: by its reference
/// and by its path, since guests may name the same disk either way.
pub fn holders_of(
    index: &std::collections::HashMap<String, Vec<String>>,
    pool: &str,
    volume: &str,
    path: &str,
) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let by_ref = DiskSource::Volume {
        pool: pool.into(),
        volume: volume.into(),
    }
    .label();
    let by_path = DiskSource::Path(path.into()).label();
    for l in [by_ref, by_path] {
        for n in index.get(&l).into_iter().flatten() {
            if !out.contains(n) {
                out.push(n.clone());
            }
        }
    }
    out
}

/// Names of domains that reference `target`, for refusing a direct volume
/// delete with a clear reason.
pub fn referencing_domains(c: &Connect, target: &DiskSource) -> Vec<String> {
    let wanted = labels(c, target);
    let mut out = Vec::new();
    let Ok(domains) = c.list_all_domains(0) else {
        return out;
    };
    for d in domains {
        let Ok(xml) = d.get_xml_desc(0) else {
            continue;
        };
        let held = disk_refs(&xml)
            .iter()
            .any(|r| labels(c, &r.source).iter().any(|l| wanted.contains(l)));
        if held {
            out.push(d.get_name().unwrap_or_default());
        }
    }
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_volume_is_held_whether_named_by_reference_or_by_path() {
        let mut index = std::collections::HashMap::new();
        index.insert(
            DiskSource::Volume {
                pool: "p".into(),
                volume: "a.qcow2".into(),
            }
            .label(),
            vec!["web".to_string()],
        );
        index.insert(
            DiskSource::Path("/img/a.qcow2".into()).label(),
            vec!["web".to_string(), "db".to_string()],
        );
        assert_eq!(
            holders_of(&index, "p", "a.qcow2", "/img/a.qcow2"),
            ["web", "db"]
        );
        assert!(holders_of(&index, "p", "b.qcow2", "/img/b.qcow2").is_empty());
    }

    use super::*;

    const GUEST: &str = r#"<domain><devices>
        <disk type='volume' device='disk'>
          <source pool='default' volume='Alpine.qcow2'/>
          <target dev='vda' bus='virtio'/>
        </disk>
        <disk type='volume' device='cdrom'>
          <source pool='ISOs' volume='alpine-standard-3.23.4.iso'/>
          <target dev='sda' bus='sata'/>
          <readonly/>
        </disk>
    </devices></domain>"#;

    fn empty() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn an_attached_iso_is_never_deleted_with_its_guest() {
        let plan = plan_cleanup(GUEST, &empty(), |s| vec![s.label()]);
        assert_eq!(
            plan.removable,
            vec![DiskSource::Volume {
                pool: "default".into(),
                volume: "Alpine.qcow2".into()
            }],
            "only the guest's own disk"
        );
        assert_eq!(
            plan.kept,
            vec![(
                "ISOs/alpine-standard-3.23.4.iso".to_string(),
                KeepReason::Iso
            )],
            "the ISO is kept, and says why"
        );
    }

    #[test]
    fn an_iso_attached_as_a_plain_disk_is_still_media() {
        // Some definitions omit the cdrom device marker; the extension is the
        // remaining signal that this is boot media rather than guest storage.
        let xml = r#"<domain><devices>
            <disk type='volume' device='disk'>
              <source pool='ISOs' volume='debian-13.iso'/>
              <target dev='vda' bus='virtio'/>
            </disk>
        </devices></domain>"#;
        let plan = plan_cleanup(xml, &empty(), |s| vec![s.label()]);
        assert!(
            plan.removable.is_empty(),
            "not removed: {:?}",
            plan.removable
        );
        assert_eq!(plan.kept.len(), 1);
    }

    #[test]
    fn a_readonly_disk_is_treated_as_media() {
        let xml = r#"<domain><devices>
            <disk type='file' device='disk'>
              <source file='/var/lib/libvirt/images/base.raw'/>
              <readonly/>
            </disk>
        </devices></domain>"#;
        let plan = plan_cleanup(xml, &empty(), |s| vec![s.label()]);
        assert!(plan.removable.is_empty());
        assert_eq!(plan.kept[0].1, KeepReason::ReadOnly);
    }

    #[test]
    fn a_disk_another_guest_uses_is_left_alone() {
        // The guest's own definition is gone by the time cleanup runs, so the
        // other domain still holding this volume decides.
        let xml = r#"<domain><devices>
            <disk type='volume' device='disk'>
              <source pool='default' volume='shared.qcow2'/>
            </disk>
            <disk type='volume' device='disk'>
              <source pool='default' volume='mine.qcow2'/>
            </disk>
        </devices></domain>"#;
        let mut busy = HashSet::new();
        busy.insert("volume:default/shared.qcow2".to_string());
        let plan = plan_cleanup(xml, &busy, |s| vec![s.label()]);
        assert_eq!(
            plan.removable,
            vec![DiskSource::Volume {
                pool: "default".into(),
                volume: "mine.qcow2".into()
            }]
        );
        assert_eq!(
            plan.kept,
            vec![("default/shared.qcow2".to_string(), KeepReason::InUse)]
        );
    }

    #[test]
    fn file_and_volume_sources_have_distinct_identities() {
        // A file path and a pool/volume must never collide as labels, or a
        // shared disk could look unreferenced.
        let file = DiskSource::Path("/p/x.qcow2".into()).label();
        let vol = DiskSource::Volume {
            pool: "/p".into(),
            volume: "x.qcow2".into(),
        }
        .label();
        assert_ne!(file, vol);
    }

    #[test]
    fn one_file_is_recognised_however_each_guest_spells_it() {
        // Disks we write now name the file by path; a guest defined earlier
        // still names it as pool+volume. Both mean one file, so a delete must
        // see the reference whichever way round the two guests spell it.
        //
        // The equivalence itself lives in `labels`, which needs a live
        // connection to resolve a volume, so what is pinned here is the wiring
        // it feeds: a resolver that unifies the two spellings keeps the disk,
        // and the as-written view alone does not — which is the case that
        // would have deleted a running guest's disk.
        let xml = r#"<domain><devices>
            <disk type='volume' device='disk'>
              <source pool='default' volume='shared.qcow2'/>
            </disk>
        </devices></domain>"#;
        let file = DiskSource::Path("/p/shared.qcow2".into()).label();
        let mut busy = HashSet::new();
        busy.insert(file.clone());

        let unaware = plan_cleanup(xml, &busy, |s| vec![s.label()]);
        assert_eq!(
            unaware.removable.len(),
            1,
            "as-written labels alone cannot see the other guest's path"
        );

        let resolved = plan_cleanup(xml, &busy, |s| match s {
            DiskSource::Volume { .. } => vec![s.label(), file.clone()],
            DiskSource::Path(_) => vec![s.label()],
        });
        assert!(resolved.removable.is_empty(), "the disk is still in use");
        assert_eq!(
            resolved.kept,
            vec![("default/shared.qcow2".to_string(), KeepReason::InUse)]
        );
    }

    #[test]
    fn refs_capture_device_and_readonly() {
        let refs = disk_refs(GUEST);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].device, "disk");
        assert!(!refs[0].readonly);
        assert_eq!(refs[1].device, "cdrom");
        assert!(refs[1].readonly);
    }

    #[test]
    fn an_attribute_name_inside_a_value_is_not_matched() {
        // `volume='some-file.iso'` must not satisfy a search for `file`.
        let xml = r#"<domain><devices>
            <disk type='volume' device='cdrom'>
              <source pool='ISOs' volume='some-file.iso'/>
            </disk>
        </devices></domain>"#;
        assert_eq!(
            disk_refs(xml)[0].source,
            DiskSource::Volume {
                pool: "ISOs".into(),
                volume: "some-file.iso".into()
            }
        );
    }

    #[test]
    fn a_disk_without_a_source_is_ignored() {
        // An empty cdrom (a tray with no disc) has no <source> to delete.
        let xml = r#"<domain><devices>
            <disk type='file' device='cdrom'>
              <target dev='sda' bus='sata'/><readonly/>
            </disk>
        </devices></domain>"#;
        assert!(disk_refs(xml).is_empty());
        assert_eq!(
            plan_cleanup(xml, &empty(), |s| vec![s.label()]),
            Cleanup::default()
        );
    }
}
