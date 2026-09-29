//! Firewall API: per-network rules + NAT forwards and per-VM rules, persisted
//! to disk and enforced as an nftables table (see `crate::firewall`).

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::ApiError;
use crate::firewall::{self, FirewallStore, NetworkFirewall, PolicySet, VmFirewall};

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
            get(get_network).put(put_network),
        )
        .route("/domains/{name}/firewall", get(get_vm).put(put_vm))
        .with_state(state)
}

fn load_store(state: &FirewallState) -> Result<FirewallStore, ApiError> {
    firewall::load(&state.path).map_err(ApiError::internal)
}

fn save_store(state: &FirewallState, store: &FirewallStore) -> Result<(), ApiError> {
    firewall::save(&state.path, store).map_err(ApiError::internal)
}

async fn get_network(
    State(state): State<FirewallState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let store = load_store(&state)?;
    Ok(Json(json!(
        store.networks.get(&name).cloned().unwrap_or_default()
    )))
}

async fn get_vm(
    State(state): State<FirewallState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let store = load_store(&state)?;
    Ok(Json(json!(
        store.vms.get(&name).cloned().unwrap_or_default()
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NetworkReq {
    #[serde(default)]
    rules: Vec<firewall::Rule>,
    #[serde(default)]
    forwards: Vec<firewall::Forward>,
    #[serde(default)]
    policy: PolicySet,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VmReq {
    #[serde(default)]
    rules: Vec<firewall::Rule>,
    #[serde(default)]
    policy: PolicySet,
}

async fn put_network(
    State(state): State<FirewallState>,
    Path(name): Path<String>,
    Json(req): Json<NetworkReq>,
) -> Result<Json<Value>, ApiError> {
    let _guard = state.lock.lock().await;
    let mut store = load_store(&state)?;

    // Rules are scoped to this network's subnet, so a typo'd name would
    // silently persist but never apply. Fail fast instead.
    let n = name.clone();
    crate::libvirt::with_conn(move |c| {
        crate::libvirt::network(c, &n).map(|_| ())?;
        Ok(())
    })
    .await
    .map_err(|e| ApiError::not_found(format!("network '{name}': {e}")))?;

    let mut fw = NetworkFirewall::default();
    for r in &req.rules {
        fw.rules
            .push(firewall::validate_rule(r).map_err(ApiError::bad_request)?);
    }
    for f in &req.forwards {
        fw.forwards
            .push(firewall::validate_forward(f).map_err(ApiError::bad_request)?);
    }
    fw.policy = req.policy;

    if fw.is_empty() {
        store.networks.remove(&name);
    } else {
        store.networks.insert(name.clone(), fw);
    }
    save_store(&state, &store)?;
    apply_store(store)
        .await
        .map_err(|e| ApiError::internal(format!("rules saved, apply failed: {e}")))?;
    Ok(Json(json!({"name": name, "saved": true, "applied": true})))
}

async fn put_vm(
    State(state): State<FirewallState>,
    Path(name): Path<String>,
    Json(req): Json<VmReq>,
) -> Result<Json<Value>, ApiError> {
    let _guard = state.lock.lock().await;
    let mut store = load_store(&state)?;

    // Rules are scoped to this guest's MACs, so a typo'd name would silently
    // persist but never apply. Fail fast instead.
    let n = name.clone();
    crate::libvirt::with_conn(move |c| {
        crate::libvirt::domain(c, &n).map(|_| ())?;
        Ok(())
    })
    .await
    .map_err(|e| ApiError::not_found(format!("domain '{name}': {e}")))?;

    let mut fw = VmFirewall::default();
    for r in &req.rules {
        fw.rules
            .push(firewall::validate_rule(r).map_err(ApiError::bad_request)?);
    }
    fw.policy = req.policy;

    if fw.is_empty() {
        store.vms.remove(&name);
    } else {
        store.vms.insert(name.clone(), fw);
    }
    save_store(&state, &store)?;
    apply_store(store)
        .await
        .map_err(|e| ApiError::internal(format!("rules saved, apply failed: {e}")))?;
    Ok(Json(json!({"name": name, "saved": true, "applied": true})))
}

/// Rebuild the whole `virt-man` nftables table from the persisted store, plus
/// the live libvirt subnets and domain MACs (rules are scoped to them, so they
/// have to be read at apply time).
pub async fn apply_store(store: FirewallStore) -> Result<(), String> {
    struct Net {
        subnet: String,
        fw: NetworkFirewall,
    }
    struct Vm {
        macs: Vec<String>,
        fw: VmFirewall,
    }

    let (nets, vms) = crate::libvirt::with_conn(move |c| {
        let mut nets: Vec<Net> = Vec::new();
        for n in c.list_all_networks(0)? {
            let name = n.get_name().unwrap_or_default();
            let xml = n.get_xml_desc(0).unwrap_or_default();
            if let Some(cidr) = crate::api::networks::network_subnet(&xml)
                && let Some(subnet) = firewall::network_prefix(&cidr)
            {
                let fw = store.networks.get(&name).cloned().unwrap_or_default();
                nets.push(Net { subnet, fw });
            }
        }

        let mut vms: Vec<Vm> = Vec::new();
        for d in crate::libvirt::all_domains(c, 0) {
            let name = d.get_name().unwrap_or_default();
            if let Some(fw) = store.vms.get(&name).cloned() {
                let xml = d
                    .get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)
                    .unwrap_or_default();
                vms.push(Vm {
                    macs: domain_macs(&xml),
                    fw,
                });
            }
        }
        Ok((nets, vms))
    })
    .await?;

    let rendered_nets: Vec<firewall::RenderedNet> = nets
        .iter()
        .map(|n| firewall::RenderedNet {
            subnet: n.subnet.clone(),
            rules: &n.fw.rules,
            forwards: &n.fw.forwards,
            policy: n.fw.policy,
        })
        .collect();
    let rendered_vms: Vec<firewall::RenderedVm> = vms
        .iter()
        .map(|v| firewall::RenderedVm {
            macs: v.macs.clone(),
            rules: &v.fw.rules,
            policy: v.fw.policy,
        })
        .collect();
    let script = firewall::render_ruleset(&rendered_nets, &rendered_vms);
    firewall::apply_script(&script).await?;
    // The script's own accepts cannot save a forwarded packet from libvirt's
    // per-network reject — a verdict in one chain does not stop a later chain
    // of the same hook from rejecting — so DNAT'd traffic is admitted inside
    // libvirt's chain too. Done here so a rule change takes effect at once
    // rather than on the supervisor's next pass; the supervisor re-asserts it
    // because libvirt rewrites that chain on a network restart.
    for n in &nets {
        if let Err(e) = firewall::allow_forwarded(&n.subnet).await {
            tracing::warn!("firewall: {e}");
        }
    }
    Ok(())
}

/// Every `<mac address='...'/>` in a domain XML, in document order.
pub fn domain_macs(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<mac") {
        let tag = &rest[i..];
        let end = tag.find('>').unwrap_or(tag.len());
        let t = &tag[..=end];
        if let Some(addr) = attr_value(t, "address") {
            out.push(addr.to_string());
        }
        rest = &tag[end..];
    }
    out
}

/// Read one attribute's value out of a tag fragment, single- or double-quoted.
fn attr_value<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(rel) = tag[from..].find(name) {
        let i = from + rel;
        from = i + name.len();
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

#[cfg(test)]
mod tests {
    use super::domain_macs;

    #[test]
    fn macs_are_parsed_from_domain_xml() {
        let xml = r#"<domain><devices>
          <interface type='network'><mac address='52:54:00:aa:bb:cc'/></interface>
          <interface type='network'><mac address='52:54:00:dd:ee:ff'/></interface>
        </devices></domain>"#;
        assert_eq!(
            domain_macs(xml),
            vec!["52:54:00:aa:bb:cc", "52:54:00:dd:ee:ff"]
        );
    }

    #[test]
    fn a_domain_without_interfaces_yields_no_macs() {
        assert!(domain_macs("<domain><devices/></domain>").is_empty());
    }
}
