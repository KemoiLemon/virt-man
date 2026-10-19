//! Container templates, Proxmox-style: a template is a rootfs tarball stored
//! as a volume in a storage pool, and a catalog lists what can be downloaded.
//!
//! A template is recognised by its file name (`*.tar.xz|zst|gz`), the same
//! way an ISO is, so one can arrive by download, by upload, or by being copied
//! into the pool directory, and every path lists it.
//!
//! The catalog merges two indexes, both fetched over HTTPS:
//!
//! - linuxcontainers.org: every distro's `default` image for this host's
//!   architecture. Its SHA256SUMS is read when a download starts.
//! - TurnKey's Proxmox index: application templates, each with a SHA-512.
//!
//! Proxmox's own index (`download.proxmox.com/images/aplinfo-*.dat`) is left
//! out on purpose: its HTTPS certificate does not match the host, so it is
//! only reachable over plain HTTP, where whoever is on the path can swap the
//! index and the checksums it carries together. Its "default" system
//! templates are the linuxcontainers images repackaged, so nothing is lost.
//!
//! Downloads reuse `fetch::begin`, so a template is spooled, checked against
//! its published digest, and only then written into a new volume.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Extension, Path};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::ApiError;
use super::fetch::{self, Expect};
use crate::pools::Pools;

const LXC_BASE: &str = "https://images.linuxcontainers.org";
const TURNKEY_INDEX: &str = "https://releases.turnkeylinux.org/pve/aplinfo.dat";
/// The catalog is re-read at most this often.
const CATALOG_TTL: Duration = Duration::from_secs(3600);

pub fn routes() -> Router {
    Router::new()
        .route("/templates/catalog", get(catalog))
        .route("/pools/{pool}/templates", post(download))
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Entry {
    /// Stable id the download request names, e.g. `lxc:debian:trixie`.
    pub id: String,
    pub source: &'static str,
    /// `system` or `turnkeylinux`, as Proxmox's index sections them.
    pub section: &'static str,
    pub os: String,
    pub release: String,
    pub description: String,
    /// Volume name the download is stored as.
    pub file: String,
    pub url: String,
    /// TurnKey publishes a SHA-512; linuxcontainers' SHA-256 is read at
    /// download time from the build's SHA256SUMS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha512: Option<String>,
    #[serde(skip)]
    sums_url: Option<String>,
}

/// This host's architecture in Debian's spelling, which both indexes use.
fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Whether a volume name is a container template (a rootfs tarball).
pub fn is_template(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [".tar.xz", ".tar.zst", ".tar.gz", ".tgz", ".tar"]
        .iter()
        .any(|s| n.ends_with(s))
}

/// What a volume holds, in Proxmox's content-type words, so the pool page
/// can split ISO images, CT templates and disk images.
pub fn content_of(name: &str) -> &'static str {
    if name.to_ascii_lowercase().ends_with(".iso") {
        "iso"
    } else if is_template(name) {
        "vztmpl"
    } else {
        "images"
    }
}

/// linuxcontainers' `meta/1.0/index-system`:
/// `dist;release;arch;variant;build;path` per line.
pub fn parse_lxc_index(text: &str, arch: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(';').collect();
            let [dist, release, a, variant, build, path] = f.as_slice() else {
                return None;
            };
            if *a != arch || *variant != "default" {
                return None;
            }
            let path = path.trim_end_matches('/');
            // `:` is not allowed in a volume name on every pool type.
            let build = build.replace(':', "-");
            Some(Entry {
                id: format!("lxc:{dist}:{release}"),
                source: "linuxcontainers",
                section: "system",
                os: dist.to_string(),
                release: release.to_string(),
                description: format!("{dist} {release} ({build})"),
                file: format!("{dist}-{release}-default_{build}_{arch}.tar.xz"),
                url: format!("{LXC_BASE}{path}/rootfs.tar.xz"),
                sha512: None,
                sums_url: Some(format!("{LXC_BASE}{path}/SHA256SUMS")),
            })
        })
        .collect()
}

/// Proxmox's `aplinfo.dat` format (what TurnKey publishes): RFC-822-ish
/// stanzas separated by blank lines; a continuation line starts with a space.
pub fn parse_aplinfo(text: &str, arch: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for stanza in text.split("\n\n") {
        let mut fields: Vec<(String, String)> = Vec::new();
        for line in stanza.lines() {
            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some(last) = fields.last_mut() {
                    last.1.push('\n');
                    last.1.push_str(line.trim());
                }
            } else if let Some((k, v)) = line.split_once(':') {
                fields.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let get = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let (Some(pkg), Some(loc), Some(sha)) = (get("package"), get("location"), get("sha512sum"))
        else {
            continue;
        };
        if get("type") != Some("lxc") || get("architecture").is_some_and(|a| a != arch) {
            continue;
        }
        let Ok(url) = reqwest::Url::parse(loc) else {
            continue;
        };
        let Some(file) = url.path_segments().and_then(|mut s| s.next_back()) else {
            continue;
        };
        if !is_template(file) {
            continue;
        }
        // Download over TLS even where the index names http://: the mirror
        // serves both, and the digest is only as trustworthy as the channel
        // the index came over, which is HTTPS.
        let mut url = url.clone();
        if url.scheme() == "http" {
            let _ = url.set_scheme("https");
        }
        let desc = get("description").unwrap_or(pkg);
        out.push(Entry {
            id: format!("turnkey:{pkg}"),
            source: "turnkey",
            section: "turnkeylinux",
            os: get("os").unwrap_or("").to_string(),
            release: get("version").unwrap_or("").to_string(),
            description: desc.lines().next().unwrap_or(pkg).to_string(),
            file: file.to_string(),
            url: url.to_string(),
            sha512: Some(sha.to_ascii_lowercase()),
            sums_url: None,
        });
    }
    out
}

/// The line for `file` in a SHA256SUMS listing.
pub fn sum_for(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (h, f) = l.split_once(char::is_whitespace)?;
        (f.trim().trim_start_matches('*') == file
            && h.len() == 64
            && h.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| h.to_ascii_lowercase())
    })
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("virt-man-daemon/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))
}

async fn get_text(url: &str) -> Result<String, String> {
    let r = client()?
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("{url}: the server answered {}", r.status()));
    }
    r.text().await.map_err(|e| format!("{url}: {e}"))
}

type Cache = Mutex<Option<(Instant, Vec<Entry>)>>;
fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Both indexes, cached. A source that fails is reported, not fatal: the
/// other one's templates are still worth offering.
async fn load() -> (Vec<Entry>, Vec<String>) {
    if let Some((at, e)) = cache().lock().expect("catalog cache").as_ref()
        && at.elapsed() < CATALOG_TTL
    {
        return (e.clone(), vec![]);
    }
    let a = arch();
    let index = format!("{LXC_BASE}/meta/1.0/index-system");
    let (lxc, tk) = tokio::join!(get_text(&index), get_text(TURNKEY_INDEX));
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    match lxc {
        Ok(t) => entries.extend(parse_lxc_index(&t, a)),
        Err(e) => errors.push(e),
    }
    match tk {
        Ok(t) => entries.extend(parse_aplinfo(&t, a)),
        Err(e) => errors.push(e),
    }
    // Only a complete answer is cached, so a transient failure is retried on
    // the next open rather than hiding a source for an hour.
    if errors.is_empty() {
        *cache().lock().expect("catalog cache") = Some((Instant::now(), entries.clone()));
    }
    (entries, errors)
}

async fn catalog() -> Json<Value> {
    let (entries, errors) = load().await;
    Json(json!({"arch": arch(), "templates": entries, "errors": errors}))
}

#[derive(Deserialize)]
struct DownloadReq {
    id: String,
}

async fn download(
    Extension(pools): Extension<Pools>,
    Path(pool): Path<String>,
    Json(req): Json<DownloadReq>,
) -> Result<Json<Value>, ApiError> {
    let (entries, errors) = load().await;
    let Some(e) = entries.into_iter().find(|e| e.id == req.id) else {
        return Err(if errors.is_empty() {
            ApiError::not_found(format!("no template '{}' in the catalog", req.id))
        } else {
            ApiError::internal(format!(
                "template catalog unavailable: {}",
                errors.join("; ")
            ))
        });
    };
    // Always verified: a template with no published digest is not offered.
    let expect = match (&e.sha512, &e.sums_url) {
        (Some(s), _) => Expect::Sha512(s.clone()),
        (None, Some(u)) => {
            let sums = get_text(u).await.map_err(ApiError::internal)?;
            Expect::Sha256(
                sum_for(&sums, "rootfs.tar.xz")
                    .ok_or_else(|| ApiError::internal(format!("{u} lists no rootfs.tar.xz")))?,
            )
        }
        (None, None) => return Err(ApiError::internal("template has no checksum")),
    };
    let url = reqwest::Url::parse(&e.url).map_err(ApiError::internal)?;
    let job = fetch::begin(pools, pool, url, e.file, Some(expect)).await?;
    Ok(Json(serde_json::to_value(job).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LXC: &str = "alpine;3.22;amd64;default;20260917_13:00;/images/alpine/3.22/amd64/default/20260917_13:00/\n\
alpine;3.22;arm64;default;20260917_13:00;/images/alpine/3.22/arm64/default/20260917_13:00/\n\
alpine;3.22;amd64;cloud;20260917_13:00;/images/alpine/3.22/amd64/cloud/20260917_13:00/\n\
debian;trixie;amd64;default;20260928_05:24;/images/debian/trixie/amd64/default/20260928_05:24/\n\
garbage line\n";

    #[test]
    fn the_lxc_index_keeps_this_arch_and_the_default_variant() {
        let e = parse_lxc_index(LXC, "amd64");
        assert_eq!(e.len(), 2, "{e:?}");
        assert_eq!(e[0].id, "lxc:alpine:3.22");
        assert_eq!(e[0].file, "alpine-3.22-default_20260917_13-00_amd64.tar.xz");
        assert_eq!(
            e[0].url,
            "https://images.linuxcontainers.org/images/alpine/3.22/amd64/default/20260917_13:00/rootfs.tar.xz"
        );
        assert!(
            e[0].sums_url.as_deref().unwrap().ends_with("/SHA256SUMS"),
            "its digest comes from the build"
        );
        assert!(!e[0].file.contains(':'), "a volume name carries no colon");
    }

    const APL: &str = "Package: turnkey-ansible\nVersion: 18.0-1\nType: lxc\nOS: debian-12\nSection: turnkeylinux\n\
Architecture: amd64\nLocation: http://mirror.turnkeylinux.org/turnkeylinux/images/proxmox/debian-12-turnkey-ansible_18.0-1_amd64.tar.gz\n\
sha512sum: ABCDEF\nDescription: TurnKey Ansible\n TurnKey Ansible - Radically simple IT automation\n\n\
Package: no-digest\nType: lxc\nArchitecture: amd64\nLocation: https://x/y.tar.gz\n\n\
Package: other-arch\nType: lxc\nArchitecture: arm64\nLocation: https://x/z.tar.gz\nsha512sum: aa\n\n\
Package: pve-web-news\nType: html\nLocation: https://x/news.html\nsha512sum: aa\n";

    #[test]
    fn aplinfo_keeps_only_verifiable_lxc_templates_for_this_arch() {
        let e = parse_aplinfo(APL, "amd64");
        assert_eq!(e.len(), 1, "{e:?}");
        assert_eq!(e[0].id, "turnkey:turnkey-ansible");
        assert_eq!(e[0].file, "debian-12-turnkey-ansible_18.0-1_amd64.tar.gz");
        assert!(e[0].url.starts_with("https://"), "upgraded to TLS");
        assert_eq!(e[0].sha512.as_deref(), Some("abcdef"));
        assert_eq!(e[0].description, "TurnKey Ansible", "first line only");
    }

    #[test]
    fn sha256sums_is_matched_by_exact_file_name() {
        let sums = format!(
            "{}  rootfs.squashfs\n{}  rootfs.tar.xz\n",
            "a".repeat(64),
            "B".repeat(64)
        );
        assert_eq!(sum_for(&sums, "rootfs.tar.xz"), Some("b".repeat(64)));
        assert_eq!(sum_for(&sums, "rootfs.tar"), None);
    }

    #[test]
    fn content_type_follows_the_file_name() {
        assert_eq!(
            content_of("debian-13-standard_13.6-1_amd64.tar.zst"),
            "vztmpl"
        );
        assert_eq!(content_of("x.TAR.GZ"), "vztmpl");
        assert_eq!(content_of("alpine.iso"), "iso");
        assert_eq!(content_of("disk.qcow2"), "images");
        assert_eq!(content_of("ct-rootfs.raw"), "images");
    }
}
