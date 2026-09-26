use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use virt::network::Network;

use super::ApiError;
use crate::libvirt::with_conn;

pub fn routes() -> Router {
    Router::new()
        .route("/networks", get(list).post(create))
        .route("/networks/bridges", get(bridges))
        .route("/networks/{name}", get(get_one).delete(delete))
        .route("/networks/{name}/xml", get(get_xml).put(put_xml))
        .route("/networks/{name}/start", post(start))
        .route("/networks/{name}/stop", post(stop))
        .route("/networks/{name}/autostart", post(set_autostart))
}

async fn list() -> Result<Json<Value>, ApiError> {
    let v = with_conn(|c| {
        let nets = c.list_all_networks(0)?;
        let mut arr = Vec::with_capacity(nets.len());
        for n in &nets {
            arr.push(net_json(n));
        }
        Ok(json!(arr))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

fn net_json(n: &Network) -> Value {
    // Forward mode and subnet live in the definition XML; the summary fields
    // otherwise cannot distinguish NAT from isolated, or show the subnet.
    let xml = n.get_xml_desc(0).unwrap_or_default();
    let forward = forward_mode(&xml).to_string();
    let ip = ip_cidr(&xml);
    json!({
        "name": n.get_name().unwrap_or_default(),
        "bridge": n.get_bridge_name().unwrap_or_default(),
        "active": n.is_active().unwrap_or(false),
        "persistent": n.is_persistent().unwrap_or(false),
        "autostart": n.get_autostart().unwrap_or(false),
        "forward": forward,
        "ip": ip,
    })
}

/// The network's forward mode: "nat", "bridge", or "none" (isolated).
fn forward_mode(xml: &str) -> &str {
    let Some(i) = xml.find("<forward") else {
        return "none";
    };
    let end = xml[i..].find('>').map_or(xml.len(), |j| i + j);
    let mode = attr_value(&xml[i..=end], "mode").unwrap_or("none");
    match mode {
        "bridge" => "bridge",
        "nat" | "route" => "nat",
        _ => "none",
    }
}

/// The first `<ip>` element's subnet, as `address/prefix` (or empty).
/// The first `<ip>` element's subnet, as `address/prefix` (or empty).
pub(crate) fn network_subnet(xml: &str) -> Option<String> {
    let c = ip_cidr(xml);
    if c.is_empty() { None } else { Some(c) }
}

fn ip_cidr(xml: &str) -> String {
    let Some(ip) = xml.find("<ip") else {
        return String::new();
    };
    let rest = &xml[ip..];
    let end = rest.find('>').unwrap_or(rest.len());
    let attrs = &rest[..=end];
    let address = attr_value(attrs, "address").unwrap_or_default();
    if address.is_empty() {
        return String::new();
    }
    let prefix = attr_value(attrs, "prefix")
        .map(|p| p.to_string())
        .or_else(|| attr_value(attrs, "netmask").map(|m| netmask_bits(m).to_string()))
        .unwrap_or_default();
    if prefix.is_empty() {
        address.to_string()
    } else {
        format!("{address}/{prefix}")
    }
}

/// Read one XML attribute's value out of a tag fragment, single- or
/// double-quoted. Matches only a real attribute position (name followed by
/// '='), never a substring inside another attribute's value.
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

/// Dotted-quad netmask to prefix length (255.255.255.0 -> 24).
fn netmask_bits(mask: &str) -> u32 {
    mask.split('.')
        .filter_map(|o| o.parse::<u32>().ok())
        .map(|o| o.count_ones())
        .sum()
}

async fn get_one(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = Network::lookup_by_name(c, &name)?;
        Ok(net_json(&n))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

/// Linux bridge devices, for the create/edit network form's bridge dropdown.
///
/// Read from /sys/class/net rather than libvirt interfaces: a bridge does not
/// have to be a libvirt interface, and scanning sysfs is what `ip link show
/// type bridge` does under the hood. A device is a bridge iff it has a
/// `bridge` subdirectory.
async fn bridges() -> Result<Json<Value>, ApiError> {
    let v = std::fs::read_dir("/sys/class/net")
        .map_err(|e| ApiError::internal(format!("read /sys/class/net: {e}")))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("bridge").is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    Ok(Json(json!(v)))
}

async fn get_xml(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = Network::lookup_by_name(c, &name)?;
        Ok(json!({"name": n.get_name().unwrap_or_default(), "xml": n.get_xml_desc(0)?}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

/// Define/replace a network. The edited XML carries the original uuid, so
/// define_xml updates it in place; a name change needs the network inactive
/// first, so destroy a running one and restart it after the define.
async fn put_xml(Path(name): Path<String>, body: String) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let was_active = Network::lookup_by_name(c, &name)
            .map(|n| n.is_active().unwrap_or(false))
            .unwrap_or(false);
        if was_active && let Ok(n) = Network::lookup_by_name(c, &name) {
            n.destroy()?;
        }
        let n = Network::define_xml(c, &body)?;
        let new_name = n.get_name().unwrap_or_default();
        let started = if was_active {
            n.create().map(|_| true).unwrap_or(false)
        } else {
            false
        };
        Ok(json!({"name": new_name, "defined": true, "started": started}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn create(body: String) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = Network::define_xml(c, &body)?;
        let name = n.get_name().unwrap_or_default();
        n.create()?;
        Ok(json!({"name": name, "started": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn delete(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = Network::lookup_by_name(c, &name)?;
        if n.is_active().unwrap_or(false) {
            n.destroy()?;
        }
        n.undefine()?;
        Ok(json!({"name": name, "undefined": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn start(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = Network::lookup_by_name(c, &name)?;
        n.create()?;
        Ok(json!({"name": name, "started": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

async fn stop(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = Network::lookup_by_name(c, &name)?;
        n.destroy()?;
        Ok(json!({"name": name, "stopped": true}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct AutostartReq {
    autostart: bool,
}

async fn set_autostart(
    Path(name): Path<String>,
    Json(req): Json<AutostartReq>,
) -> Result<Json<Value>, ApiError> {
    let v = with_conn(move |c| {
        let n = Network::lookup_by_name(c, &name)?;
        n.set_autostart(req.autostart)?;
        Ok(json!({"name": name, "autostart": req.autostart}))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_mode_reads_nat_bridge_and_none() {
        // libvirt emits single-quoted attributes.
        assert_eq!(
            forward_mode(r#"<network><forward mode='nat'/></network>"#),
            "nat"
        );
        assert_eq!(
            forward_mode(r#"<network><forward mode='bridge'/></network>"#),
            "bridge"
        );
        // No forward element = isolated.
        assert_eq!(forward_mode(r#"<network><name>x</name></network>"#), "none");
        // route behaves like nat for display purposes.
        assert_eq!(
            forward_mode(r#"<network><forward mode='route'/></network>"#),
            "nat"
        );
    }

    #[test]
    fn ip_cidr_reads_address_and_netmask_as_prefix() {
        assert_eq!(
            ip_cidr(r#"<network><ip address='192.168.100.1' netmask='255.255.255.0'/></network>"#),
            "192.168.100.1/24"
        );
        // Double quotes work too.
        assert_eq!(
            ip_cidr(r#"<network><ip address="10.9.0.1" netmask="255.255.0.0"/></network>"#),
            "10.9.0.1/16"
        );
        assert_eq!(ip_cidr(r#"<network><name>x</name></network>"#), "");
    }

    #[test]
    fn ip_cidr_prefers_an_explicit_prefix() {
        assert_eq!(
            ip_cidr(r#"<network><ip address='10.0.0.1' prefix='16'/></network>"#),
            "10.0.0.1/16"
        );
    }

    #[test]
    fn netmask_bits_sums_leading_ones() {
        assert_eq!(netmask_bits("255.255.255.0"), 24);
        assert_eq!(netmask_bits("255.255.0.0"), 16);
        assert_eq!(netmask_bits("255.255.255.128"), 25);
    }
}
