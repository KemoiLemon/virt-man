//! `/domains`: VMs run by the direct-QEMU manager. The wire shapes are the
//! ones the SPA and the web server have always read, so the frontend did not
//! change when the backend stopped being libvirt.
//!
//! The identity is the manager's UUID, which is `<name>`, `<uuid>` and the URL.
//! The user's display name is `<title>`; routes accept either, because old
//! links and the web server's records may still name a guest by its title.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::collections::HashMap;

use super::{ApiError, off_runtime};
use crate::disks;
use crate::ipam::server::Ipam;
use crate::pools::Pools;
use crate::qemu::{Manager, State as GuestState, VmConfig};

#[derive(Clone)]
struct VmState {
    manager: Manager,
    pools: Pools,
    ipam: Ipam,
}

pub fn routes(manager: Manager, pools: Pools, ipam: Ipam) -> Router {
    Router::new()
        .route("/domains", get(list).post(create))
        .route("/domains/{name}", get(get_one).delete(delete))
        .route("/domains/{name}/xml", get(get_xml).put(put_xml))
        .route("/domains/{name}/state", get(get_state))
        .route("/domains/{name}/stats", get(get_stats))
        .route("/domains/{name}/interfaces", get(get_interfaces))
        .route("/domains/{name}/graphics", get(get_graphics))
        .route("/domains/{name}/start", post(start))
        .route("/domains/{name}/shutdown", post(shutdown))
        .route("/domains/{name}/reboot", post(reboot))
        .route("/domains/{name}/destroy", post(destroy))
        .route("/domains/{name}/reset", post(reset))
        .route("/domains/{name}/rename", post(rename))
        .with_state(VmState {
            manager,
            pools,
            ipam,
        })
}

fn err(e: String) -> ApiError {
    if e == "unknown QEMU VM" {
        ApiError::not_found(e)
    } else {
        ApiError::conflict(e)
    }
}

/// A guest by its id (UUID) or by its display title. The SPA uses the id;
/// titles are for old links and the web server's name-keyed records. An
/// ambiguous title resolves to nothing rather than the wrong guest.
fn resolve(manager: &Manager, key: &str) -> Result<VmConfig, ApiError> {
    let all = manager.list().map_err(ApiError::internal)?;
    if let Some(cfg) = all.iter().find(|c| c.id == key) {
        return Ok(cfg.clone());
    }
    let mut matches = all
        .into_iter()
        .filter(|c| !c.title.is_empty() && c.title == key);
    match (matches.next(), matches.next()) {
        (Some(cfg), None) => Ok(cfg),
        _ => Err(ApiError::not_found(format!("no VM '{key}'"))),
    }
}

fn summary(cfg: &VmConfig, state: &str) -> Value {
    // The SPA uses vCPUs and memory on the dashboard cards; a definition that
    // cannot be parsed is still listed rather than vanishing from the tree.
    let d = crate::domainxml::parse(&cfg.xml).ok();
    json!({
        "name": cfg.id,
        "uuid": cfg.id,
        "title": (!cfg.title.is_empty()).then(|| cfg.title.clone()),
        // libvirt's runtime id; a direct launch has no equivalent, and the UI
        // only uses it as an opaque discriminator.
        "id": 0,
        "state": state,
        "active": matches!(state, "running" | "paused"),
        "persistent": true,
        "vcpus": d.as_ref().map(|d| d.vcpus).unwrap_or(0),
        "memory_mb": d.as_ref().map(|d| d.memory_mib).unwrap_or(0),
    })
}

async fn list(State(s): State<VmState>) -> Result<Json<Value>, ApiError> {
    let mut arr = Vec::new();
    for cfg in s.manager.list().map_err(ApiError::internal)? {
        let state = s.manager.state(&cfg.id).map_err(err)?;
        arr.push(summary(&cfg, state.as_str()));
    }
    Ok(Json(json!(arr)))
}

async fn get_one(
    State(s): State<VmState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let cfg = resolve(&s.manager, &name)?;
    let state = s.manager.state(&cfg.id).map_err(err)?;
    Ok(Json(summary(&cfg, state.as_str())))
}

async fn get_xml(
    State(s): State<VmState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let cfg = resolve(&s.manager, &name)?;
    let xml = s.manager.xml(&cfg.id).map_err(err)?;
    Ok(Json(json!({"name": cfg.id, "xml": xml})))
}

/// The SPA's options form rewrites `<name>` to the display name it typed. The
/// identity is the UUID, so a display name is put back to the id (the display
/// name is `<title>`); a different UUID is another guest and is refused.
fn pin_name(xml: &str, id: &str) -> Result<String, String> {
    let Some(open) = xml.find("<name>") else {
        return Ok(xml.to_string());
    };
    let close = xml[open..]
        .find("</name>")
        .map(|i| i + open)
        .ok_or("the definition has an unterminated <name>")?;
    let value = xml[open + 6..close].trim();
    if value.is_empty() || value == id {
        return Ok(xml.to_string());
    }
    if uuid::Uuid::parse_str(value).is_ok_and(|u| u.hyphenated().to_string() == value) {
        return Err(format!(
            "<name> '{value}' does not match this VM's id '{id}'"
        ));
    }
    let mut out = String::with_capacity(xml.len());
    out.push_str(&xml[..open + 6]);
    out.push_str(id);
    out.push_str(&xml[close..]);
    Ok(out)
}

async fn put_xml(
    State(s): State<VmState>,
    Path(name): Path<String>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    let cfg = resolve(&s.manager, &name)?;
    let xml = crate::firmware::resolve(&body);
    let xml = pin_name(&xml, &cfg.id).map_err(ApiError::bad_request)?;
    // A running guest keeps its live devices; the stored definition is what
    // the next start uses. Updating it never restarts the process.
    s.manager
        .update_xml(&cfg.id, &xml)
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({"name": cfg.id, "defined": true})))
}

async fn create(
    State(s): State<VmState>,
    Query(q): Query<HashMap<String, String>>,
    body: String,
) -> Result<Json<Value>, ApiError> {
    let start = q.get("start").map(|v| v == "true").unwrap_or(true);
    let xml = crate::firmware::resolve(&body);
    let cfg = s.manager.define(&xml).map_err(err)?;
    if start {
        s.manager.start(&cfg.id).map_err(err)?;
    }
    Ok(Json(json!({"name": cfg.id, "started": start})))
}

async fn delete(
    State(s): State<VmState>,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let remove_storage = q
        .get("remove_storage")
        .map(|v| v == "true")
        .unwrap_or(false);
    let cfg = resolve(&s.manager, &name)?;
    let xml = s.manager.xml(&cfg.id).map_err(err)?;
    // Decide the cleanup from the stored definition and the other guests'
    // definitions before anything is removed. Only the guest's own disks are
    // candidates: an attached ISO, a read-only disk, or anything another guest
    // references is kept.
    let plan = if remove_storage {
        let guests = disks::guest_definitions(&s.manager);
        disks::plan_cleanup_connected(
            &s.pools,
            &xml,
            &disks::sources_used_elsewhere(&guests, &s.pools, Some(&cfg.id)),
        )
    } else {
        disks::Cleanup::default()
    };

    // A delete always ends with the guest gone, not orphaned: ask it to shut
    // down, wait (bounded), then force it off if it does not.
    let running = match s.manager.state(&cfg.id).map_err(err)? {
        GuestState::Running => Some(false),
        GuestState::Paused => Some(true),
        _ => None,
    };
    let stopped = if let Some(force) = running {
        let manager = s.manager.clone();
        let id = cfg.id.clone();
        off_runtime(move || {
            if force {
                manager.force_stop(&id)
            } else {
                manager
                    .powerdown(&id, false)
                    .or_else(|_| manager.force_stop(&id))
            }
        })
        .await?;
        true
    } else {
        false
    };

    // Removes metadata, the QMP/NVRAM runtime directory and the taps. Never a
    // disk file: that is the cleanup plan's business, below.
    s.manager.delete(&cfg.id).map_err(err)?;

    let mut removed = Vec::new();
    let mut failed = Vec::new();
    for src in &plan.removable {
        // Best effort: only a file inside one of our pools is removed, so a
        // definition naming a host path (an ISO elsewhere, /etc/shadow) can
        // never make a guest delete reach it.
        let outcome: Result<(), String> = match src {
            disks::DiskSource::Path(path) => s
                .pools
                .owned_file(path)
                .ok_or_else(|| "not a file in a storage pool".to_string())
                .and_then(|p| std::fs::remove_file(p).map_err(|e| e.to_string())),
            disks::DiskSource::Volume { pool, volume } => s
                .pools
                .volume_path(pool, volume)
                .ok_or_else(|| format!("no volume '{volume}' in pool '{pool}'"))
                .and_then(|p| std::fs::remove_file(p).map_err(|e| e.to_string())),
        };
        match outcome {
            Ok(()) => removed.push(src.describe()),
            Err(e) => failed.push(json!({"disk": src.describe(), "error": e})),
        }
    }
    Ok(Json(json!({
        "name": cfg.id,
        "undefined": true,
        // Whether the guest had to be stopped first, so the UI can say so.
        "stopped": stopped,
        "storage_removed": remove_storage,
        // What went, what was deliberately kept and why, and what could not be
        // removed — so the UI can tell the whole truth.
        "removed": removed,
        "kept": plan.kept.iter()
            .map(|(disk, reason)| json!({"disk": disk, "reason": reason.as_str()}))
            .collect::<Vec<_>>(),
        "failed": failed,
    })))
}

async fn get_state(
    State(s): State<VmState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let cfg = resolve(&s.manager, &name)?;
    let state = s.manager.state(&cfg.id).map_err(err)?;
    // libvirt's "reason" is not modelled; the SPA only displays the state.
    Ok(Json(
        json!({"name": cfg.id, "state": state.as_str(), "reason": 0}),
    ))
}

/// Point-in-time CPU/memory sample for one guest, plus the host node info the
/// caller needs to turn `cpu_time_ns` into a percentage.
async fn get_stats(
    State(s): State<VmState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let cfg = resolve(&s.manager, &name)?;
    let stats = s.manager.stats(&cfg.id).map_err(err)?;
    let (cpus, memory_kib, mhz, model) = super::host::node_info();
    Ok(Json(json!({
        "name": cfg.id,
        "state": stats.state,
        "cpu_time_ns": stats.cpu_time_ns,
        "vcpus": stats.vcpus,
        "memory_kib": stats.memory_kib,
        "max_memory_kib": stats.max_memory_kib,
        "rss_kib": stats.rss_kib,
        "usable_kib": stats.usable_kib,
        "host": {
            "cpus": cpus,
            "memory_kib": memory_kib,
            "mhz": mhz,
            "model": model,
        },
        "sampled_at_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    })))
}

async fn get_interfaces(
    State(s): State<VmState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let cfg = resolve(&s.manager, &name)?;
    let mut out = Vec::new();
    for i in s.manager.interfaces(&cfg.id).map_err(err)? {
        // The API is keyed by network, not by the bridge the definition names.
        // A bridge nothing here defines is an operator's own, and has no key;
        // a user-mode NIC has no host interface at all.
        let network = i
            .network
            .as_deref()
            .and_then(|bridge| s.ipam.network_for_bridge(bridge));
        out.push(json!({
            "dev": i.dev,
            "mac": i.mac,
            "model": i.model,
            "network": network,
            // Limits as the guest sees them: inbound is its RX.
            "rx_mbps": i.rx_kib.map(super::nics::kib_to_mbps),
            "tx_mbps": i.tx_kib.map(super::nics::kib_to_mbps),
            "rx_bytes": i.rx_bytes,
            "tx_bytes": i.tx_bytes,
        }));
    }
    Ok(Json(json!({ "name": cfg.id, "interfaces": out })))
}

async fn get_graphics(
    State(s): State<VmState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let cfg = resolve(&s.manager, &name)?;
    let graphics = s.manager.graphics(&cfg.id).map_err(err)?;
    Ok(Json(json!({"name": cfg.id, "graphics": graphics})))
}

async fn start(
    State(s): State<VmState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let cfg = resolve(&s.manager, &name)?;
    let manager = s.manager.clone();
    let id = cfg.id.clone();
    off_runtime(move || manager.start(&id)).await?;
    Ok(Json(json!({"name": cfg.id, "started": true})))
}

async fn shutdown(
    State(s): State<VmState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let cfg = resolve(&s.manager, &name)?;
    let manager = s.manager.clone();
    let id = cfg.id.clone();
    off_runtime(move || manager.powerdown(&id, false)).await?;
    Ok(Json(json!({"name": cfg.id, "shutdown": true})))
}

async fn reboot(
    State(s): State<VmState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let cfg = resolve(&s.manager, &name)?;
    s.manager.reboot(&cfg.id).map_err(err)?;
    Ok(Json(json!({"name": cfg.id, "rebooted": true})))
}

async fn destroy(
    State(s): State<VmState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let cfg = resolve(&s.manager, &name)?;
    let manager = s.manager.clone();
    let id = cfg.id.clone();
    off_runtime(move || manager.force_stop(&id)).await?;
    Ok(Json(json!({"name": cfg.id, "destroyed": true})))
}

async fn reset(
    State(s): State<VmState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let cfg = resolve(&s.manager, &name)?;
    s.manager.reset(&cfg.id).map_err(err)?;
    Ok(Json(json!({"name": cfg.id, "reset": true})))
}

#[derive(serde::Deserialize)]
struct RenameReq {
    name: String,
}

/// Rename the display name. The identity — the UUID, the URL, the MACs and the
/// definition's `<name>` — never changes, so a rename cannot orphan disks,
/// snapshots or the web server's records. Shut off only, as before: a running
/// guest's page would otherwise disagree with the name it is managed by.
async fn rename(
    State(s): State<VmState>,
    Path(name): Path<String>,
    Json(req): Json<RenameReq>,
) -> Result<Json<Value>, ApiError> {
    let cfg = resolve(&s.manager, &name)?;
    let new_title = req.name.trim().to_string();
    if new_title.is_empty() {
        return Err(ApiError::bad_request("new name is empty"));
    }
    if new_title == cfg.title {
        // Nothing to do; report success so the caller can treat rename as
        // idempotent rather than special-casing a same-name request.
        return Ok(Json(json!({"name": cfg.id, "renamed": false})));
    }
    if s.manager.state(&cfg.id).map_err(err)? != GuestState::Stopped {
        return Err(ApiError::conflict(format!(
            "rename requires '{name}' to be shut off"
        )));
    }
    s.manager
        .set_title(&cfg.id, &new_title)
        .map_err(ApiError::bad_request)?;
    Ok(Json(json!({"name": cfg.id, "renamed": true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vm-domains-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct Fixture {
        dir: std::path::PathBuf,
        manager: Manager,
        pools: Pools,
        ipam: Ipam,
        root: std::path::PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = scratch(tag);
            let manager = Manager::with_qemu(&dir, dir.join("state"), "/nonexistent/qemu").unwrap();
            let pools = Pools::load(dir.join("pools.json")).unwrap();
            let root = dir.join("images");
            std::fs::create_dir_all(&root).unwrap();
            {
                let mut s = pools.lock();
                s.pools.insert(
                    "3d9f1e2a-7b6c-4d5e-9f8a-1b2c3d4e5f60".into(),
                    crate::pools::Pool {
                        title: "test".into(),
                        path: root.clone(),
                        autostart: true,
                        active: true,
                    },
                );
                pools.save(&s).unwrap();
            }
            Self {
                ipam: Ipam::load(dir.join("networks.json"), dir.clone()).unwrap(),
                dir,
                manager,
                pools,
                root,
            }
        }

        fn disk(&self, name: &str) -> std::path::PathBuf {
            let path = self.root.join(name);
            std::fs::write(&path, b"x").unwrap();
            path
        }

        fn xml(&self, disk: &std::path::Path) -> String {
            format!(
                r#"<domain type="kvm">
  <name>{ID}</name>
  <uuid>{ID}</uuid>
  <title>web</title>
  <memory unit="KiB">524288</memory>
  <vcpu>2</vcpu>
  <os><type arch="x86_64" machine="q35">hvm</type><boot dev="hd"/></os>
  <devices>
    <disk type="file" device="disk">
      <driver name="qemu" type="raw"/>
      <source file="{}"/>
      <target dev="vda" bus="virtio"/>
    </disk>
    <interface type="bridge">
      <source bridge="vbr5b92a1a54f00"/>
      <model type="virtio"/>
    </interface>
    <graphics type="vnc" port="5900" listen="127.0.0.1"/>
  </devices>
</domain>"#,
                disk.display()
            )
        }

        fn app(&self) -> Router {
            routes(self.manager.clone(), self.pools.clone(), self.ipam.clone())
        }

        async fn call(&self, method: &str, path: &str, body: &str) -> (u16, Value) {
            let req = Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap();
            let r = self.app().oneshot(req).await.unwrap();
            let status = r.status().as_u16();
            let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
            (status, serde_json::from_slice(&body).unwrap_or_default())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The wire shape the SPA reads, through the real routes. No QEMU: the
    /// guest is defined shut off and never started.
    #[tokio::test]
    async fn a_defined_guest_reads_back_in_the_existing_wire_shape() {
        let f = Fixture::new("wire");
        let disk = f.disk("a.raw");
        let (status, body) = f.call("POST", "/domains?start=false", &f.xml(&disk)).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["name"], ID);
        assert_eq!(body["started"], false);

        let (status, body) = f.call("GET", "/domains", "").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body[0]["name"], ID, "the manager id is the name");
        assert_eq!(body[0]["uuid"], ID);
        assert_eq!(body[0]["title"], "web");
        assert_eq!(body[0]["state"], "stopped");
        assert_eq!(body[0]["active"], false);
        assert_eq!(body[0]["persistent"], true);
        assert_eq!(body[0]["vcpus"], 2);
        assert_eq!(body[0]["memory_mb"], 512);

        let (status, body) = f.call("GET", &format!("/domains/{ID}"), "").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["state"], "stopped");

        // A title link reaches the same guest, as old links do.
        let (status, by_title) = f.call("GET", "/domains/web", "").await;
        assert_eq!(status, 200, "{by_title}");
        assert_eq!(by_title["name"], ID);

        let (status, body) = f.call("GET", &format!("/domains/{ID}/xml"), "").await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body["xml"]
                .as_str()
                .unwrap()
                .contains(&format!("<name>{ID}</name>"))
        );

        let (status, body) = f.call("GET", &format!("/domains/{ID}/state"), "").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["state"], "stopped");
        assert_eq!(body["reason"], 0);

        let (status, body) = f.call("GET", &format!("/domains/{ID}/stats"), "").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["state"], "stopped");
        assert_eq!(body["cpu_time_ns"], 0);
        assert_eq!(body["vcpus"], 2);
        assert_eq!(body["memory_kib"], 524288);
        assert!(body["host"]["cpus"].as_u64().unwrap() > 0);

        let (status, body) = f
            .call("GET", &format!("/domains/{ID}/interfaces"), "")
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body["interfaces"][0]["mac"],
            crate::qemu::stable_mac(ID, "net0")
        );
        assert_eq!(body["interfaces"][0]["model"], "virtio");
        assert_eq!(body["interfaces"][0]["dev"], Value::Null);

        let (status, body) = f.call("GET", &format!("/domains/{ID}/graphics"), "").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body["graphics"],
            json!({"kind": "vnc", "host": "127.0.0.1", "port": 5900})
        );
    }

    /// An edit changes the stored definition and nothing else; the display
    /// title survives an edit that does not mention it, and a form that writes
    /// the display name into `<name>` still targets the same UUID.
    #[tokio::test]
    async fn an_xml_edit_updates_the_definition_and_keeps_the_identity() {
        let f = Fixture::new("edit");
        let disk = f.disk("a.raw");
        f.call("POST", "/domains?start=false", &f.xml(&disk)).await;

        let edited = f
            .xml(&disk)
            .replace("<vcpu>2</vcpu>", "<vcpu>4</vcpu>")
            .replace("<title>web</title>", "")
            .replace(&format!("<name>{ID}</name>"), "<name>web</name>");
        let (status, body) = f.call("PUT", &format!("/domains/{ID}/xml"), &edited).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["name"], ID);

        let xml = f.manager.xml(ID).unwrap();
        assert!(xml.contains("<vcpu>4</vcpu>"), "{xml}");
        assert!(
            xml.contains("<title>web</title>"),
            "the title survives: {xml}"
        );
        assert!(xml.contains(&format!("<name>{ID}</name>")), "{xml}");

        // A different UUID is another guest, not a display name.
        let other = edited.replace(ID, "550e8400-e29b-41d4-a716-446655440001");
        let (status, _) = f.call("PUT", &format!("/domains/{ID}/xml"), &other).await;
        assert_eq!(status, 400, "a different identity is refused");
    }

    /// A rename moves the title and nothing else, and is refused while the
    /// guest runs.
    #[tokio::test]
    async fn a_rename_keeps_the_id_and_requires_a_stopped_guest() {
        let f = Fixture::new("rename");
        let disk = f.disk("a.raw");
        f.call("POST", "/domains?start=false", &f.xml(&disk)).await;

        let (status, body) = f
            .call("POST", &format!("/domains/{ID}/rename"), r#"{"name":"db"}"#)
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, json!({"name": ID, "renamed": true}));
        assert_eq!(f.manager.list().unwrap()[0].title, "db");
        assert!(
            f.manager
                .xml(ID)
                .unwrap()
                .contains(&format!("<name>{ID}</name>"))
        );

        let (status, body) = f
            .call("POST", &format!("/domains/{ID}/rename"), r#"{"name":"db"}"#)
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["renamed"], false, "a same-name rename is idempotent");

        let (status, _) = f
            .call(
                "POST",
                &format!("/domains/{ID}/rename"),
                r#"{"name":"a/b"}"#,
            )
            .await;
        assert_eq!(status, 400, "a name with a slash is not a display name");
    }

    /// A delete removes the guest, its runtime and a disk this daemon's pools
    /// own — and never a file outside them.
    #[tokio::test]
    async fn a_delete_removes_pool_storage_but_not_a_host_file() {
        let f = Fixture::new("delete");
        let ours = f.disk("own.raw");
        f.call("POST", "/domains?start=false", &f.xml(&ours)).await;

        let (status, body) = f
            .call("DELETE", &format!("/domains/{ID}?remove_storage=true"), "")
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["undefined"], true);
        assert_eq!(body["stopped"], false);
        assert_eq!(body["removed"], json!([ours.to_string_lossy()]));
        assert!(!ours.exists(), "the guest's own disk is gone");
        assert!(f.manager.list().unwrap().is_empty());

        // A definition pointing outside the pools is deleted, but its file is
        // not: the safety rule is the pool root, not the XML.
        let outside = f.dir.join("host-file.raw");
        std::fs::write(&outside, b"x").unwrap();
        f.call("POST", "/domains?start=false", &f.xml(&outside))
            .await;
        let (status, body) = f
            .call("DELETE", &format!("/domains/{ID}?remove_storage=true"), "")
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["removed"], json!([]));
        assert_eq!(body["failed"].as_array().unwrap().len(), 1);
        assert!(
            outside.exists(),
            "a host file is never removed by a VM delete"
        );
    }

    /// The runtime directory (QMP socket, NVRAM) goes with the guest, because
    /// it lives under the manager's state root, not in a pool.
    #[tokio::test]
    async fn a_delete_removes_the_runtime_directory() {
        let f = Fixture::new("runtime");
        let disk = f.disk("a.raw");
        f.call("POST", "/domains?start=false", &f.xml(&disk)).await;
        let runtime = f.dir.join("state").join(ID);
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join("nvram.fd"), b"vars").unwrap();

        let (status, body) = f.call("DELETE", &format!("/domains/{ID}"), "").await;
        assert_eq!(status, 200, "{body}");
        assert!(!runtime.exists(), "the runtime and NVRAM are gone");
        assert!(disk.exists(), "without remove_storage the disk stays");
    }
}
