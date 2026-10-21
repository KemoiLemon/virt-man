pub mod capture;
pub mod console;
pub mod domains;
pub mod fetch;
pub mod firewall;
pub mod health;
pub mod host;
pub mod metrics;
pub mod networks;
pub mod nics;
pub mod pools;
pub mod qemu;
pub mod shell;
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

/// Run a manager call that waits on a process off the async workers.
///
/// A graceful stop waits up to 30 s for QEMU to exit. Running that on a worker
/// thread stalls the runtime instead: on a one-CPU host the daemon then answers
/// nothing for those 30 s, so the web server's health probe marks the host
/// offline and unrelated work — a network start or stop, the IPAM supervisor —
/// fails on screen. The work itself is a blocking poll of `/proc`, so a
/// dedicated blocking thread is where it belongs.
pub(crate) async fn off_runtime<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("lifecycle worker failed: {e}")))?
        .map_err(|e| {
            if e == "unknown QEMU VM" {
                ApiError::not_found(e)
            } else {
                ApiError::conflict(e)
            }
        })
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

pub fn build(
    auth: Auth,
    ipam: crate::ipam::server::Ipam,
    pools: crate::pools::Pools,
    qemu_manager: crate::qemu::Manager,
) -> Router {
    let fw = firewall::FirewallState::new(ipam.clone());
    let authed = Router::new()
        .merge(health::routes())
        .route("/features", axum::routing::get(feature_status))
        .merge(host::routes())
        .merge(metrics::routes())
        .merge(domains::routes(
            qemu_manager.clone(),
            pools.clone(),
            ipam.clone(),
        ))
        .merge(capture::routes(qemu_manager.clone()))
        // One firewall state for the rules and a NIC change, which re-applies
        // the ruleset and must take the same lock as a rules PUT.
        .merge(firewall::routes(fw.clone()))
        .merge(nics::routes(qemu_manager.clone(), ipam.clone(), fw.clone()))
        .merge(shell::routes())
        .merge(networks::routes(ipam.clone(), fw.clone()))
        .merge(pools::routes())
        .merge(volumes::routes())
        .merge(fetch::routes())
        .merge(qemu::routes(qemu_manager.clone(), ipam.clone(), fw))
        .layer(middleware::from_fn(crate::auth::require_token));

    // console authenticates via ?token= (browsers can't set WS headers), so it
    // is merged after the middleware layer. Auth is shared via Extension.
    Router::new()
        .merge(authed)
        .merge(console::routes(qemu_manager.clone()))
        .layer(Extension(auth))
        // Storage pools and their volumes are the daemon's own.
        .layer(Extension(pools))
        // The direct-QEMU manager: a volume list and a volume delete ask it
        // which guests hold a disk.
        .layer(Extension(qemu_manager))
}

/// What this daemon runs: VMs on direct QEMU.
async fn feature_status() -> Json<serde_json::Value> {
    Json(serde_json::json!({"vms": "qemu"}))
}

#[cfg(test)]
mod feature_tests {
    #[tokio::test]
    async fn feature_status_reports_direct_qemu_vms_only() {
        let status = super::feature_status().await.0;
        assert_eq!(status, serde_json::json!({"vms": "qemu"}));
    }
}

#[cfg(test)]
mod uuid_route_tests {
    /// Through the real router: a network reached by its id (the stored key,
    /// a UUID) and by its display name (the title an old link carries) gets
    /// the same answer, and an unknown segment is not found.
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
}
