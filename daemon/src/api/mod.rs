pub mod capture;
pub mod console;
pub mod containers;
pub mod ct_console;
pub mod domains;
pub mod fetch;
pub mod filters;
pub mod firewall;
pub mod health;
pub mod host;
pub mod metrics;
pub mod networks;
pub mod nics;
pub mod pools;
pub mod qemu;
pub mod shell;
pub mod templates;
pub mod volumes;

use axum::Extension;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, middleware};
use serde_json::json;

use crate::auth::Auth;

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl std::fmt::Display) -> Self {
        Self {
            status,
            message: message.to_string(),
        }
    }

    pub fn bad_request(message: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    /// The request is well-formed but refused because of current state — e.g.
    /// deleting a disk a guest still has attached. Distinct from a bad request
    /// so the UI can tell "you asked something wrong" from "that is in use".
    pub fn conflict(message: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

/// Which path segment names a guest, network or pool, and of which kind.
/// `/domains/{name}/...` and `/pools/{name}/...` (a pool's volumes and fetch
/// included). Collection routes have no name and are left alone. A network is
/// not here: its handler resolves the stored key itself, since `Path` params
/// are captured by the router before any middleware can rewrite them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Named {
    Domain,
    Container,
    Pool,
}

fn named_segment(path: &str) -> Option<(usize, Named)> {
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match seg.as_slice() {
        ["domains", _, ..] => Some((1, Named::Domain)),
        ["containers", _, ..] => Some((1, Named::Container)),
        ["pools", _, ..] => Some((1, Named::Pool)),
        _ => None,
    }
}

/// The libvirt name a UUID segment means. Containers are domains on the LXC
/// driver: that connection must be opened, since this runs before any handler
/// picks a driver. A pool is not libvirt's any more: its stored key is found
/// directly.
fn libvirt_name(c: &virt::connect::Connect, kind: Named, id: &str) -> Option<String> {
    match kind {
        Named::Domain => crate::libvirt::domain(c, id).ok()?.get_name().ok(),
        Named::Container => {
            let mut lxc = virt::connect::Connect::open(Some(crate::libvirt::LXC_URI)).ok()?;
            let n = crate::libvirt::domain(&lxc, id)
                .ok()
                .and_then(|d| d.get_name().ok());
            let _ = lxc.close();
            n
        }
        Named::Pool => None,
    }
}

/// Let every route take a UUID where it takes a name.
///
/// A handler knows its guest or pool by name: firewall rules, titles and the
/// web server's records are keyed on it, and some handlers must compare it.
/// So the one UUID segment is swapped for the stored name before the handler
/// runs. A UUID that matches nothing is passed through unchanged, so the
/// handler answers "not found" exactly as it would for a wrong name. Only the
/// canonical UUID shape is looked up; anything else is a name already.
async fn resolve_uuids(
    pools: crate::pools::Pools,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path().to_string();
    if let Some((i, kind)) = named_segment(&path) {
        let mut seg: Vec<String> = path
            .trim_start_matches('/')
            .split('/')
            .map(str::to_string)
            .collect();
        let id = seg[i].clone();
        let name = if crate::libvirt::is_uuid(&id) {
            match kind {
                Named::Pool => pools.resolve_key(&id),
                _ => crate::libvirt::with_conn(move |c| Ok(libvirt_name(c, kind, &id)))
                    .await
                    .ok()
                    .flatten(),
            }
        } else {
            None
        };
        if let Some(name) = name {
            seg[i] = encode(&name);
            let mut new = format!("/{}", seg.join("/"));
            if let Some(q) = req.uri().query() {
                new.push('?');
                new.push_str(q);
            }
            if let Ok(uri) = new.parse() {
                *req.uri_mut() = uri;
            }
        }
    }
    next.run(req).await
}

/// Percent-encode one path segment (a libvirt name may hold spaces).
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn build(
    auth: Auth,
    ipam: crate::ipam::server::Ipam,
    pools: crate::pools::Pools,
    qemu_manager: crate::qemu::Manager,
) -> Router {
    let fw = firewall::FirewallState::new(ipam.clone());
    // The guest routes, shared by VMs (`/domains`) and containers, which
    // reach them through `containers::forward` under the LXC driver.
    let guest = Router::new()
        .merge(domains::routes())
        .merge(capture::routes())
        // One firewall state for both: a NIC change re-applies the ruleset,
        // and must take the same lock as a rules PUT.
        .merge(firewall::routes(fw.clone()))
        .merge(nics::routes(fw.clone()));
    // Containers reach these handlers through `containers::forward`, which
    // oneshots this router directly rather than the outer one, so the stores
    // they need must be on this router too.
    let for_ct = guest
        .clone()
        .layer(Extension(ipam.clone()))
        .layer(Extension(pools.clone()));
    let authed = Router::new()
        .merge(health::routes())
        .route("/features", axum::routing::get(feature_status))
        .merge(host::routes())
        .merge(metrics::routes())
        .merge(guest)
        .merge(containers::routes())
        .merge(ct_console::routes())
        .merge(shell::routes())
        .route(
            "/containers/{name}/{*rest}",
            axum::routing::any(move |req: axum::extract::Request| {
                containers::forward(for_ct.clone(), req)
            }),
        )
        .merge(templates::routes())
        .merge(networks::routes(ipam.clone(), fw.clone()))
        .merge(filters::routes())
        .merge(pools::routes())
        .merge(volumes::routes())
        .merge(fetch::routes())
        .merge(qemu::routes(qemu_manager, ipam.clone(), fw))
        .layer(middleware::from_fn(crate::auth::require_token));

    // console authenticates via ?token= (browsers can't set WS headers), so it
    // is merged after the middleware layer. Auth is shared via Extension.
    Router::new()
        .merge(authed)
        .merge(console::routes())
        // Outermost, so the rewrite happens before routing picks a handler.
        .layer(middleware::from_fn({
            let pools = pools.clone();
            move |req, next| resolve_uuids(pools.clone(), req, next)
        }))
        .layer(Extension(auth))
        // The networks store: a guest's NIC names a bridge, and only the store
        // knows which network that is. Handlers take it as an extractor.
        .layer(Extension(ipam))
        // Storage pools and their volumes are the daemon's own.
        .layer(Extension(pools))
}

/// Report the runtime boundary explicitly: container endpoints currently use
/// libvirt's LXC driver; the direct-QEMU manager is VM-only.
async fn feature_status() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "containers": {
            "status": "supported",
            "backend": "libvirt-lxc",
            "direct_qemu": "unsupported"
        }
    }))
}

#[cfg(test)]
mod feature_tests {
    #[tokio::test]
    async fn feature_status_does_not_claim_direct_qemu_container_support() {
        let status = super::feature_status().await.0;
        assert_eq!(status["containers"]["backend"], "libvirt-lxc");
        assert_eq!(status["containers"]["direct_qemu"], "unsupported");
    }
}

#[cfg(test)]
mod uuid_route_tests {
    /// Through the real router: a network reached by its id (the stored key,
    /// a UUID) and by its display name (the title an old link carries) gets
    /// the same answer, and an unknown segment is not found. No libvirt.
    #[tokio::test]
    async fn a_network_reaches_by_its_uuid_or_its_title() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let dir = std::env::temp_dir().join(format!("netroutes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ipam = crate::ipam::server::Ipam::load(dir.join("networks.json"), dir.clone()).unwrap();
        let id = "3d9f1e2a-7b6c-4d5e-9f8a-1b2c3d4e5f60";
        {
            let mut s = ipam.lock();
            s.networks.insert(
                id.into(),
                crate::ipam::store::import(
                    "<network><title>lab</title><forward mode='nat'/>
                     <ip address='10.9.0.1' netmask='255.255.255.0'/></network>",
                )
                .unwrap(),
            );
            ipam.save(&s).unwrap();
        }
        let fw = super::firewall::FirewallState::new(ipam.clone());
        let app = || super::networks::routes(ipam.clone(), fw.clone());
        let get = |seg: &str| {
            let app = app();
            let req = Request::get(format!("/networks/{seg}"))
                .body(Body::empty())
                .unwrap();
            async move {
                let r = app.oneshot(req).await.unwrap();
                let status = r.status();
                let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
                (
                    status,
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default(),
                )
            }
        };
        let (by_id, by_title, unknown) = (get(id).await, get("lab").await, get("other").await);
        assert_eq!(by_id.0, 200, "by id: {:?}", by_id.1);
        assert_eq!(by_title.0, 200, "by title: {:?}", by_title.1);
        assert_eq!(by_title.1, by_id.1, "the same answer either way");
        assert_eq!(by_id.1["name"], id);
        assert_eq!(unknown.0, 404, "an unknown segment is not found");

        // A write by title reaches the same network too.
        let req = Request::post("/networks/lab/autostart")
            .header("content-type", "application/json")
            .body(Body::from("{\"autostart\":false}"))
            .unwrap();
        let r = app().oneshot(req).await.unwrap();
        assert!(r.status().is_success(), "{}", r.status());
        assert!(!ipam.lock().networks[id].autostart);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn named_routes_are_recognised_and_collections_are_not() {
        let at = |p: &str| super::named_segment(p).map(|(i, _)| i);
        assert_eq!(at("/domains/x"), Some(1));
        assert_eq!(at("/domains/x/console/frame"), Some(1));
        assert_eq!(at("/pools/x/volumes/v.qcow2"), Some(1));
        assert_eq!(at("/domains"), None);
        assert_eq!(at("/pools"), None);
        assert_eq!(
            at("/networks/x"),
            None,
            "a network is resolved by its own handler: the router captures Path params first"
        );
        assert_eq!(at("/host/stats"), None);
    }

    #[test]
    fn a_name_is_encoded_as_one_path_segment() {
        assert_eq!(super::encode("my vm"), "my%20vm");
        assert_eq!(super::encode("a/b"), "a%2Fb");
        assert_eq!(super::encode("Debian-13"), "Debian-13");
    }
}
