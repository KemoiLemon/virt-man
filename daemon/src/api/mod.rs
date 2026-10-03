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
/// `/domains/{name}/...`, `/networks/{name}/...`, `/pools/{name}/...` (a
/// pool's volumes and fetch included). Collection routes -- `/networks`,
/// `/networks/bridges` -- have no name and are left alone.
type NameOf = fn(&virt::connect::Connect, &str) -> Option<String>;

fn named_segment(path: &str) -> Option<(usize, NameOf)> {
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match seg.as_slice() {
        ["domains", _, ..] => Some((1, |c, id| {
            crate::libvirt::domain(c, id).ok()?.get_name().ok()
        })),
        // A container is a domain on the LXC driver: the lookup must open
        // that one, since this runs before any handler picks a driver.
        ["containers", _, ..] => Some((1, |_, id| {
            let mut c = virt::connect::Connect::open(Some(crate::libvirt::LXC_URI)).ok()?;
            let n = crate::libvirt::domain(&c, id)
                .ok()
                .and_then(|d| d.get_name().ok());
            let _ = c.close();
            n
        })),
        ["networks", "bridges"] => None,
        ["networks", _, ..] => Some((1, |c, id| {
            crate::libvirt::network(c, id).ok()?.get_name().ok()
        })),
        ["pools", _, ..] => Some((1, |c, id| crate::libvirt::pool(c, id).ok()?.get_name().ok())),
        _ => None,
    }
}

/// Let every route take a UUID where it takes a name.
///
/// A handler knows its guest by name: firewall rules, IPAM reservations and
/// the web server's records are keyed on it, and some handlers must compare it
/// (renaming, redefining). So rather than teach sixty handlers a second form,
/// the one UUID segment is swapped for the libvirt name before any handler
/// runs. A UUID that matches nothing is passed through unchanged, so the
/// handler answers "not found" exactly as it would for a wrong name. Only the
/// canonical UUID shape is looked up; anything else is a name already.
async fn resolve_uuids(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path().to_string();
    if let Some((i, lookup)) = named_segment(&path) {
        let mut seg: Vec<String> = path
            .trim_start_matches('/')
            .split('/')
            .map(str::to_string)
            .collect();
        if crate::libvirt::is_uuid(&seg[i]) {
            let id = seg[i].clone();
            let name = crate::libvirt::with_conn(move |c| Ok(lookup(c, &id)))
                .await
                .ok()
                .flatten();
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

pub fn build(auth: Auth, ipam: crate::ipam::server::Ipam) -> Router {
    let fw = firewall::FirewallState::new();
    // The guest routes, shared by VMs (`/domains`) and containers, which
    // reach them through `containers::forward` under the LXC driver.
    let guest = Router::new()
        .merge(domains::routes())
        .merge(capture::routes())
        // One firewall state for both: a NIC change re-applies the ruleset,
        // and must take the same lock as a rules PUT.
        .merge(firewall::routes(fw.clone()))
        .merge(nics::routes(fw));
    let for_ct = guest.clone();
    let authed = Router::new()
        .merge(health::routes())
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
        .merge(networks::routes())
        .merge(networks::ipam_routes(ipam))
        .merge(filters::routes())
        .merge(pools::routes())
        .merge(volumes::routes())
        .merge(fetch::routes())
        .layer(middleware::from_fn(crate::auth::require_token));

    // console authenticates via ?token= (browsers can't set WS headers), so it
    // is merged after the middleware layer. Auth is shared via Extension.
    Router::new()
        .merge(authed)
        .merge(console::routes())
        // Outermost, so the rewrite happens before routing picks a handler.
        .layer(middleware::from_fn(resolve_uuids))
        .layer(Extension(auth))
}

#[cfg(test)]
mod uuid_route_tests {
    /// Through the real router and real libvirt: a request naming a network by
    /// its UUID reaches the same handler, with the same answer, as one naming
    /// it by name -- for a GET and a POST -- and an unknown UUID is not found.
    /// Uses a throwaway, never-started network; nothing else is touched.
    /// `sudo -E cargo test --bins -- --ignored a_uuid_reaches`
    #[tokio::test]
    #[ignore = "needs root and a reachable qemu:///system"]
    async fn a_uuid_reaches_the_same_handler_as_the_name() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let (name, uuid) = ("uuidroute-probe", "3d9f1e2a-7b6c-4d5e-9f8a-1b2c3d4e5f60");
        crate::libvirt::with_conn(move |c| {
            virt::network::Network::define_xml(
                c,
                &format!("<network><name>{name}</name><uuid>{uuid}</uuid></network>"),
            )?;
            Ok(())
        })
        .await
        .expect("define probe");

        // A router with just the network routes behind the rewrite, as `build` has it.
        let app = || {
            axum::Router::new()
                .merge(super::networks::routes())
                .layer(axum::middleware::from_fn(super::resolve_uuids))
        };
        let get = |id: &str| {
            let app = app();
            let req = Request::get(format!("/networks/{id}"))
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
        let (by_name, by_uuid, unknown) = (
            get(name).await,
            get(uuid).await,
            get("9e8d7c6b-5a49-4382-b1a0-f9e8d7c6b5a4").await,
        );
        // A write, by UUID: autostart on, then off (the network is never started).
        let post = |on: bool| {
            let app = app();
            let req = Request::post(format!("/networks/{uuid}/autostart"))
                .header("content-type", "application/json")
                .body(Body::from(format!("{{\"autostart\":{on}}}")))
                .unwrap();
            async move { app.oneshot(req).await.unwrap().status() }
        };
        let (set_on, set_off) = (post(true).await, post(false).await);

        crate::libvirt::with_conn(move |c| {
            crate::libvirt::network(c, name)?.undefine()?;
            Ok(())
        })
        .await
        .expect("undefine probe");

        assert_eq!(by_name.0, 200, "by name: {:?}", by_name.1);
        assert_eq!(by_uuid.0, 200, "by uuid: {:?}", by_uuid.1);
        assert_eq!(by_uuid.1, by_name.1, "the same answer either way");
        assert_eq!(by_uuid.1["name"], name);
        assert_ne!(unknown.0, 200, "an unknown UUID is not found");
        assert!(
            set_on.is_success() && set_off.is_success(),
            "writes by UUID: {set_on} {set_off}"
        );
    }

    #[test]
    fn named_routes_are_recognised_and_collections_are_not() {
        let at = |p: &str| super::named_segment(p).map(|(i, _)| i);
        assert_eq!(at("/domains/x"), Some(1));
        assert_eq!(at("/domains/x/console/frame"), Some(1));
        assert_eq!(at("/networks/x/hosts/aa:bb"), Some(1));
        assert_eq!(at("/pools/x/volumes/v.qcow2"), Some(1));
        assert_eq!(at("/domains"), None);
        assert_eq!(at("/networks"), None);
        assert_eq!(
            at("/networks/bridges"),
            None,
            "a collection, not a network named 'bridges'"
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
