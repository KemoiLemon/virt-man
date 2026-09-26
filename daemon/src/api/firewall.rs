//! Per-network firewall API: rules and NAT port forwards, persisted to disk
//! and enforced as an nftables table (see `crate::firewall`).

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::ApiError;
use crate::firewall::{self, FirewallMap, NetworkFirewall};

#[derive(Clone)]
pub struct FirewallState {
    pub path: std::path::PathBuf,
    /// Serialises save + re-apply so concurrent PUTs cannot interleave the
    /// persisted file and the live ruleset.
    pub lock: Arc<Mutex<()>>,
}

impl FirewallState {
    pub fn new() -> Self {
        Self {
            path: firewall::FIREWALL_PATH.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }
}

pub fn routes(state: FirewallState) -> Router {
    Router::new()
        .route(
            "/networks/{name}/firewall",
            get(get_firewall).put(put_firewall),
        )
        .with_state(state)
}

fn load_map(state: &FirewallState) -> Result<FirewallMap, ApiError> {
    firewall::load(&state.path).map_err(ApiError::internal)
}

async fn get_firewall(
    State(state): State<FirewallState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let map = load_map(&state)?;
    Ok(Json(json!(map.get(&name).cloned().unwrap_or_default())))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FirewallReq {
    #[serde(default)]
    rules: Vec<firewall::Rule>,
    #[serde(default)]
    forwards: Vec<firewall::Forward>,
}

async fn put_firewall(
    State(state): State<FirewallState>,
    Path(name): Path<String>,
    Json(req): Json<FirewallReq>,
) -> Result<Json<Value>, ApiError> {
    let _guard = state.lock.lock().await;
    let mut map = load_map(&state)?;

    // Validate and normalise before anything is written or applied.
    let mut fw = NetworkFirewall::default();
    for r in &req.rules {
        fw.rules
            .push(firewall::validate_rule(r).map_err(ApiError::bad_request)?);
    }
    for f in &req.forwards {
        fw.forwards
            .push(firewall::validate_forward(f).map_err(ApiError::bad_request)?);
    }

    if fw.is_empty() {
        map.remove(&name);
    } else {
        map.insert(name.clone(), fw);
    }
    firewall::save(&state.path, &map).map_err(ApiError::internal)?;

    if let Err(e) = reapply(map).await {
        // The file says what we want; the live apply failed, so surface it but
        // do not roll back (a syntax bug in one network must not lose the
        // others — the operator sees the error and the file is intact).
        return Err(ApiError::internal(format!(
            "rules saved, apply failed: {e}"
        )));
    }
    Ok(Json(json!({"name": name, "saved": true, "applied": true})))
}

/// Rebuild the whole `virt-man` nftables table from the persisted map plus the
/// live libvirt network subnets (rules match the guest subnet, so the subnet
/// has to be read from the network XML at apply time).
pub async fn reapply(map: FirewallMap) -> Result<(), String> {
    struct Net {
        subnet: String,
        fw: NetworkFirewall,
    }
    let nets = crate::libvirt::with_conn(move |c| {
        let mut out: Vec<Net> = Vec::new();
        for n in c.list_all_networks(0)? {
            let name = n.get_name().unwrap_or_default();
            let xml = n.get_xml_desc(0).unwrap_or_default();
            if let Some(cidr) = crate::api::networks::network_subnet(&xml)
                && let Some(subnet) = firewall::network_prefix(&cidr)
            {
                let fw = map.get(&name).cloned().unwrap_or_default();
                out.push(Net { subnet, fw });
            }
        }
        Ok(out)
    })
    .await?;

    let rendered: Vec<firewall::RenderedNet> = nets
        .iter()
        .map(|n| firewall::RenderedNet {
            subnet: n.subnet.clone(),
            rules: &n.fw.rules,
            forwards: &n.fw.forwards,
        })
        .collect();
    let script = firewall::render_ruleset(&rendered);
    firewall::apply_script(&script).await
}
