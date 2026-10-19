//! Firewall API: per-network rules + NAT forwards and per-VM rules, persisted
//! to disk and enforced as an nftables table (see `crate::firewall`).

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::ApiError;
use crate::firewall::{self, FirewallStore, NetworkFirewall, PolicySet, VmFirewall};
use crate::ipam::server::Ipam;
use crate::ipam::store::Mode;

#[derive(Clone)]
pub struct FirewallState {
    pub path: std::path::PathBuf,
    /// Serialises save + re-apply so concurrent PUTs cannot interleave the
    /// persisted file and the live ruleset.
    pub lock: Arc<Mutex<()>>,
    /// The stored network definitions: what a network is, which bridge it
    /// owns, and whether it is up all come from here, not from libvirt.
    pub ipam: Ipam,
}

impl FirewallState {
    pub fn new(ipam: Ipam) -> Self {
        Self {
            path: firewall::FIREWALL_PATH.into(),
            lock: Arc::new(Mutex::new(())),
            ipam,
        }
    }

    /// Rebuild the live ruleset from the persisted store. A NIC change adds or
    /// removes a MAC, and the per-VM rules are scoped to MACs, so the change
    /// must reach nftables. Serialised with rule writes for the same reason.
    pub async fn reapply(&self) -> Result<(), String> {
        let _guard = self.lock.lock().await;
        let store = firewall::load(&self.path)?;
        apply_store(&self.ipam, &store).await
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

/// The stored key a path segment names (a UUID, else its title), or 404. A
/// title link then reaches the same rules as the UUID key.
fn network_key(state: &FirewallState, seg: &str) -> Result<String, ApiError> {
    state
        .ipam
        .resolve_key(seg)
        .ok_or_else(|| ApiError::not_found(format!("network '{seg}' is not managed here")))
}

async fn get_network(
    State(state): State<FirewallState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    // An unknown network is a 404, like every other network route; a network
    // that exists but has no rules still answers the empty default.
    let name = network_key(&state, &name)?;
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

/// Whether per-VM rules can be scoped to this guest: a libvirt domain, or a
/// direct-QEMU VM the IPAM snapshot knows. QEMU rules are keyed by the VM's id
/// (the name `GET /qemu/vms` lists), never by its title.
async fn known_guest(ipam: &Ipam, name: &str) -> bool {
    let n = name.to_string();
    if crate::libvirt::with_conn(move |c| {
        crate::libvirt::domain(c, &n).map(|_| ())?;
        Ok(())
    })
    .await
    .is_ok()
    {
        return true;
    }
    ipam.has_vm(name)
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
    let name = network_key(&state, &name)?;
    let net = state.ipam.lock().networks.get(&name).cloned();
    let Some(net) = net else {
        return Err(ApiError::not_found(format!(
            "network '{name}' is not managed here"
        )));
    };
    // A network this host does not serve (an operator's bridge, or one with no
    // subnet) renders no rules at all: `apply_store` skips it. Saving here
    // would answer "applied" for rules that never reach nftables.
    if !net.is_served() {
        return Err(ApiError::bad_request(format!(
            "network '{name}' has no IPv4 subnet served by this host, so it has no firewall rules: a bridge network's policy belongs to its own bridge"
        )));
    }

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
    apply_store(&state.ipam, &store)
        .await
        .map_err(apply_failed)?;
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
    // persist but never apply. Fail fast instead. A direct-QEMU VM is not a
    // libvirt domain: its rules are keyed by its id, and its MACs come from
    // the IPAM snapshot.
    if !known_guest(&state.ipam, &name).await {
        return Err(ApiError::not_found(format!("domain or QEMU VM '{name}'")));
    }

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
    apply_store(&state.ipam, &store)
        .await
        .map_err(apply_failed)?;
    Ok(Json(json!({"name": name, "saved": true, "applied": true})))
}

/// A network's rules as `apply_store` renders them: its stored definition plus
/// its firewall policy.
struct Net {
    subnet: String,
    mode: Mode,
    bridge: String,
    active: bool,
    fw: NetworkFirewall,
}

/// A guest's rules with the MACs they are scoped to.
struct Vm {
    macs: Vec<String>,
    fw: VmFirewall,
}

/// The guests whose rules can be scoped: a stored policy on a guest whose MACs
/// are known. Both libvirt domains and direct-QEMU bridge NICs reach `macs`
/// through the IPAM snapshot, so a QEMU VM's rules are rendered too.
fn scoped_vms(
    macs: std::collections::BTreeMap<String, Vec<String>>,
    store: &HashMap<String, VmFirewall>,
) -> Vec<Vm> {
    macs.into_iter()
        .filter_map(|(name, macs)| {
            let fw = store.get(&name)?.clone();
            (!macs.is_empty()).then_some(Vm { macs, fw })
        })
        .collect()
}

/// A save that reached disk but not the kernel. The response says "saved" and
/// names the failure; it never claims the rules are applied.
fn apply_failed(e: String) -> ApiError {
    ApiError::internal(format!("rules saved, apply failed: {e}"))
}

/// Rebuild the whole `virt-man` nftables table from the persisted store.
///
/// The networks come from the daemon's own definitions: each one's subnet,
/// mode, bridge and whether its bridge is up. Guest MACs come from the IPAM
/// guest snapshot (libvirt domains and direct-QEMU VMs alike), since a VM's
/// rules are scoped to the MACs it is defined with. A QEMU VM's key is its id —
/// the name `GET /qemu/vms` lists — never its title, which can change. All of
/// it has to be read at apply time — the rules match live identities.
///
/// The network API calls this on every lifecycle change (start, stop, create,
/// delete, edit); nothing else re-installs a network's rules.
pub async fn apply_store(ipam: &Ipam, store: &FirewallStore) -> Result<(), String> {
    // Snapshot the definitions and drop the lock: nothing below may hold it
    // across an await.
    let nets: Vec<Net> = {
        let s = ipam.lock();
        s.networks
            .iter()
            .filter_map(|(id, net)| {
                Some(Net {
                    subnet: net.subnet()?,
                    mode: net.mode,
                    bridge: crate::hostnet::bridge_for(net, id),
                    // The bridge, not dnsmasq: rules belong to the interface,
                    // and a dnsmasq restart must not drop every rule.
                    active: crate::hostnet::bridge_up(net, id),
                    fw: store.networks.get(id).cloned().unwrap_or_default(),
                })
            })
            .collect()
    };

    let vm_store = store.vms.clone();
    // Per-VM rules are scoped to the MACs a guest is defined with. The IPAM
    // snapshot is the one list that covers both libvirt domains and
    // direct-QEMU VMs, so a QEMU VM's bridge NIC gets its rules too.
    let mut macs: std::collections::BTreeMap<String, Vec<String>> =
        ipam.guests_by_vm().into_iter().collect();
    // libvirt is read as well, for a domain the snapshot has not reached yet
    // (it runs every few seconds) or one the supervisor could not see.
    let wanted = vm_store.clone();
    let libvirt_macs = crate::libvirt::with_conn(move |c| {
        let mut found: Vec<(String, Vec<String>)> = Vec::new();
        for d in crate::libvirt::all_domains(c, 0) {
            let name = d.get_name().unwrap_or_default();
            if wanted.contains_key(&name) {
                let xml = d
                    .get_xml_desc(virt::sys::VIR_DOMAIN_XML_INACTIVE)
                    .unwrap_or_default();
                found.push((name, domain_macs(&xml)));
            }
        }
        Ok(found)
    })
    .await;
    match libvirt_macs {
        Ok(found) => {
            for (name, guest_macs) in found {
                let entry = macs.entry(name).or_default();
                for mac in guest_macs {
                    if !entry.contains(&mac) {
                        entry.push(mac);
                    }
                }
            }
        }
        Err(e) => {
            // The network half must still be applied: losing it over an
            // unrelated libvirt outage would drop every network's NAT and
            // isolation. The next apply restores the per-guest rules.
            tracing::warn!("firewall: cannot read guests, applying the known MACs only: {e}");
        }
    }
    // A guest with no MAC (none was recorded) cannot be matched by a rule.
    let vms = scoped_vms(macs, &vm_store);

    let rendered_nets: Vec<firewall::RenderedNet> = nets
        .iter()
        .map(|n| firewall::RenderedNet {
            subnet: n.subnet.clone(),
            mode: n.mode,
            bridge: n.bridge.clone(),
            active: n.active,
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
    // Forwarding is only turned on once the NAT rules exist: with a bare
    // bridge and no masquerade, a forwarded packet would leave with a private
    // source address. It is never turned off — the sysctl is host-wide and
    // shared with libvirt's LXC driver, Docker and anything else on the host.
    if nets.iter().any(|n| n.active && n.mode == Mode::Nat) {
        crate::hostnet::enable_ip_forward()?;
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
    use super::*;
    use axum::response::IntoResponse;

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

    /// A save that reached the file but not the kernel must not look applied.
    #[tokio::test]
    async fn an_apply_failure_is_reported_as_saved_not_applied() {
        let res = apply_failed("nft: Operation not permitted".into()).into_response();
        assert_eq!(res.status(), 500);
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("rules saved, apply failed"), "{text}");
        assert!(!text.contains("applied"), "{text}");
    }

    fn state_with(dir: &std::path::Path, id: &str, xml: &str) -> FirewallState {
        let ipam = Ipam::load(dir.join("networks.json"), dir.to_path_buf()).unwrap();
        ipam.lock()
            .networks
            .insert(id.into(), crate::ipam::store::import(xml).unwrap());
        FirewallState {
            path: dir.join("firewall.json"),
            lock: Arc::new(Mutex::new(())),
            ipam,
        }
    }

    /// A bridge network (or one without a subnet) is skipped by `apply_store`,
    /// so a save that claimed "applied" would be a lie. The title in the path
    /// also pins the key/title resolution: an unresolved name would be a 404.
    #[tokio::test]
    async fn a_network_without_a_subnet_refuses_firewall_rules() {
        let dir = std::env::temp_dir().join(format!("fw-nosubnet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = state_with(
            &dir,
            "5b92a1a5-4f00-4a1c-9f2e-1d3c5b7a9e00",
            "<network><title>Lab</title><forward mode='bridge'/><bridge name='br0'/></network>",
        );
        let err = put_network(
            State(state.clone()),
            Path("Lab".into()),
            Json(NetworkReq {
                rules: Vec::new(),
                forwards: Vec::new(),
                policy: PolicySet::default(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.into_response().status(), 400);
        assert!(!state.path.exists(), "a refused write saves nothing");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_title_link_reaches_the_same_rules_and_an_unknown_network_is_a_404() {
        let dir = std::env::temp_dir().join(format!("fw-title-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = "5b92a1a5-4f00-4a1c-9f2e-1d3c5b7a9e00";
        let state = state_with(
            &dir,
            id,
            "<network><title>Lab</title><ip address='10.9.0.1' netmask='255.255.255.0'/></network>",
        );
        let mut store = crate::firewall::FirewallStore::default();
        store.networks.insert(
            id.into(),
            crate::firewall::NetworkFirewall {
                rules: vec![crate::firewall::Rule {
                    direction: "in".into(),
                    action: "accept".into(),
                    proto: "tcp".into(),
                    dst_port: "22".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        crate::firewall::save(&state.path, &store).unwrap();

        let Json(by_title) = get_network(State(state.clone()), Path("Lab".into()))
            .await
            .ok()
            .expect("the title resolves");
        assert_eq!(by_title["rules"][0]["dst_port"], "22");
        let Json(by_key) = get_network(State(state.clone()), Path(id.into()))
            .await
            .ok()
            .expect("the key resolves");
        assert_eq!(by_key, by_title);

        let err = get_network(State(state), Path("nope".into()))
            .await
            .unwrap_err();
        assert_eq!(err.into_response().status(), 404);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A direct-QEMU VM can own per-VM rules: the gate accepts it by its id
    /// (the `GET /qemu/vms` name), and the IPAM MAC merge is keyed by that same
    /// id. An unknown name is still a 404.
    #[tokio::test]
    async fn a_qemu_guest_can_own_per_vm_rules() {
        let dir = std::env::temp_dir().join(format!("fw-qemu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let vm = "550e8400-e29b-41d4-a716-446655440000";
        let state = FirewallState {
            path: dir.join("firewall.json"),
            lock: Arc::new(Mutex::new(())),
            ipam: Ipam::load(dir.join("networks.json"), dir.clone()).unwrap(),
        };
        state.ipam.guests.lock().unwrap().insert(
            "52:54:00:aa:bb:01".into(),
            crate::ipam::server::GuestNic {
                uuid: vm.into(),
                name: "web".into(),
                bridge: "vbr0".into(),
            },
        );
        assert!(known_guest(&state.ipam, vm).await);
        assert_eq!(
            state.ipam.guests_by_vm()[vm],
            vec!["52:54:00:aa:bb:01".to_string()],
            "the MAC merge reads the seeded guest"
        );
        assert!(!known_guest(&state.ipam, "550e8400-e29b-41d4-a716-446655440001").await);

        // The same snapshot feeds the renderer: a QEMU bridge MAC is the scope
        // of its per-VM rules, exactly as a libvirt domain's is.
        let mut policies = HashMap::new();
        policies.insert(
            vm.to_string(),
            VmFirewall {
                rules: vec![firewall::Rule {
                    direction: "in".into(),
                    ..Default::default()
                }],
                policy: PolicySet::default(),
            },
        );
        let vms = scoped_vms(state.ipam.guests_by_vm(), &policies);
        let rendered: Vec<firewall::RenderedVm> = vms
            .iter()
            .map(|v| firewall::RenderedVm {
                macs: v.macs.clone(),
                rules: &v.fw.rules,
                policy: v.fw.policy,
            })
            .collect();
        let out = firewall::render_ruleset(&[], &rendered);
        assert!(
            out.contains("ether daddr { 52:54:00:aa:bb:01 } accept"),
            "the QEMU NIC's MAC scopes its rules: {out}"
        );

        let err = put_vm(
            State(state),
            Path("nope".into()),
            Json(VmReq {
                rules: Vec::new(),
                policy: PolicySet::default(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.into_response().status(), 404);
        let _ = std::fs::remove_dir_all(dir);
    }
}
