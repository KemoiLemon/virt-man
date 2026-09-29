//! What each daemon route is, for the permission check in the proxy.
//!
//! Every path the daemon serves maps to one privilege at one ACL path. A path
//! this table does not know is refused (`None`), so a route added to the
//! daemon is unreachable through the proxy until someone decides who may use
//! it — the safe direction to be wrong in.

use crate::perm::{ct_path, net_path, node_path, pool_path, vm_path};

/// How the proxy must treat a request once the privilege check passes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Special {
    None,
    /// A list to filter to what the caller may see.
    ListVms,
    ListCts,
    ListNetworks,
    ListPools,
    ListVolumes {
        pool: String,
    },
    ListBridges,
    /// Creates: check the body, record the owner.
    CreateVm,
    /// A container from a template: check privileged, the template, the
    /// networks and the quota; record the owner and its rootfs.
    CreateCt,
    CreateNetwork,
    CreateVolume {
        pool: String,
    },
    Fetch {
        pool: String,
    },
    /// A catalog template downloaded into a pool: a fetch by another name.
    FetchTemplate {
        pool: String,
    },
    /// Replaces a definition: check the body.
    PutVmXml {
        vm: String,
    },
    PutNetworkXml {
        net: String,
    },
    PutCtXml {
        ct: String,
    },
    /// A container's resources: growth is checked against the owner's quota.
    PutCtHardware {
        ct: String,
    },
    RenameVm {
        vm: String,
    },
    RenameCt {
        ct: String,
    },
    AddNic {
        vm: String,
    },
    DeleteVm {
        vm: String,
    },
    DeleteCt {
        ct: String,
    },
    DeleteNetwork {
        net: String,
    },
    DeleteVolume {
        pool: String,
        vol: String,
    },
    /// Admin-only whatever the ACL says: raw access to the host itself.
    AdminOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    pub path: String,
    pub privilege: &'static str,
    pub special: Special,
}

fn need(path: String, privilege: &'static str) -> Option<Need> {
    Some(Need {
        path,
        privilege,
        special: Special::None,
    })
}

fn with(path: String, privilege: &'static str, special: Special) -> Option<Need> {
    Some(Need {
        path,
        privilege,
        special,
    })
}

/// Percent-decode a path segment. The ACL is keyed by real names.
pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Bytes, not str slices: a multi-byte character after a '%' would
        // otherwise be sliced mid-character and panic.
        if let (b'%', Some(&h), Some(&l)) = (b[i], b.get(i + 1), b.get(i + 2)) {
            if let (Some(h), Some(l)) = (hex(h), hex(l)) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The privilege a daemon request needs. `rest` is the path after
/// `/proxy/<conn>`, without the query.
pub fn classify(conn: &str, method: &str, rest: &str) -> Option<Need> {
    let seg: Vec<String> = rest.trim_matches('/').split('/').map(decode).collect();
    // A decoded '/' would put a second level into an ACL path: a request for
    // "a%2F..%2Fb" would be checked as if it were inside guest "a".
    if seg.iter().any(|x| x.contains('/') || x == "." || x == "..") {
        return None;
    }
    let s: Vec<&str> = seg.iter().map(String::as_str).collect();
    let get = method == "GET" || method == "HEAD";
    let node = node_path(conn);
    let vms = format!("{node}/vms");
    let nets = format!("{node}/networks");
    let cts = format!("{node}/containers");
    match (method, s.as_slice()) {
        // Host and daemon.
        (_, ["health"] | ["version"]) if get => need(node, "Sys.Audit"),
        (_, ["host", "stats"]) if get => need(node, "Sys.Audit"),
        // Every guest on the host in one answer. The web server's samplers
        // read these directly; through the proxy they are an admin's view.
        (_, ["metrics"] | ["flows"]) if get => with(node, "Sys.Audit", Special::AdminOnly),
        // nwfilters are host-wide rule sets any guest can reference.
        (_, ["filters", ..]) => with(node, "Sys.Modify", Special::AdminOnly),

        // Guests.
        (_, ["domains"]) if get => with(vms, "Sys.Audit", Special::ListVms),
        ("POST", ["domains"]) => with(vms, "VM.Allocate", Special::CreateVm),
        (_, ["domains", vm, rest @ ..]) => {
            let p = vm_path(conn, vm);
            let vm = vm.to_string();
            match (method, rest) {
                (_, [] | ["state"] | ["stats"] | ["interfaces"] | ["graphics"]) if get => {
                    need(p, "VM.Audit")
                }
                ("DELETE", []) => with(p, "VM.Allocate", Special::DeleteVm { vm }),
                // The raw definition names host paths; reading it is part of
                // configuring, and writing it is admin-only (see xmlcheck).
                (_, ["xml"]) if get => need(p, "VM.Audit"),
                ("PUT", ["xml"]) => with(p, "VM.Config", Special::PutVmXml { vm }),
                ("POST", ["start" | "shutdown" | "reboot" | "destroy" | "reset"]) => {
                    need(p, "VM.PowerMgmt")
                }
                ("POST", ["rename"]) => with(p, "VM.Config", Special::RenameVm { vm }),
                (_, ["firewall"]) if get => need(p, "VM.Audit"),
                ("PUT", ["firewall"]) => need(p, "VM.Config"),
                ("POST", ["interfaces"]) => with(p, "VM.Config", Special::AddNic { vm }),
                ("DELETE", ["interfaces", _]) => need(p, "VM.Config"),
                ("PUT", ["interfaces", _, "bandwidth"]) => need(p, "VM.Config"),
                (_, ["console", ..]) => need(p, "VM.Console"),
                // Packet capture reads the guest's traffic.
                (_, ["capture", ..]) => need(p, "VM.Console"),
                _ => None,
            }
        }

        // Containers: their own privilege set, so a VM grant reaches no
        // container and a container grant no VM.
        (_, ["containers"]) if get => with(cts, "Sys.Audit", Special::ListCts),
        ("POST", ["containers"]) => with(cts, "CT.Allocate", Special::CreateCt),
        (_, ["containers", ct, rest @ ..]) => {
            let p = ct_path(conn, ct);
            let ct = ct.to_string();
            match (method, rest) {
                (_, [] | ["state"] | ["stats"] | ["interfaces"] | ["hardware"]) if get => {
                    need(p, "CT.Audit")
                }
                ("PUT", ["hardware"]) => with(p, "CT.Config", Special::PutCtHardware { ct }),
                ("DELETE", []) => with(p, "CT.Allocate", Special::DeleteCt { ct }),
                (_, ["xml"]) if get => need(p, "CT.Audit"),
                ("PUT", ["xml"]) => with(p, "CT.Config", Special::PutCtXml { ct }),
                ("POST", ["start" | "shutdown" | "reboot" | "destroy"]) => need(p, "CT.PowerMgmt"),
                ("POST", ["rename"]) => with(p, "CT.Config", Special::RenameCt { ct }),
                (_, ["firewall"]) if get => need(p, "CT.Audit"),
                ("PUT", ["firewall"]) => need(p, "CT.Config"),
                // The NIC's network is checked like a VM's.
                ("POST", ["interfaces"]) => with(p, "CT.Config", Special::AddNic { vm: ct }),
                ("DELETE", ["interfaces", _]) => need(p, "CT.Config"),
                ("PUT", ["interfaces", _, "bandwidth"]) => need(p, "CT.Config"),
                (_, ["console", ..]) => need(p, "CT.Console"),
                (_, ["capture", ..]) => need(p, "CT.Console"),
                _ => None,
            }
        }
        // The template catalog is public information; reading it costs the
        // daemon an outbound request, so it needs a server grant.
        (_, ["templates", "catalog"]) if get => need(node.clone(), "Sys.Audit"),

        // Networks.
        (_, ["networks"]) if get => with(nets, "Sys.Audit", Special::ListNetworks),
        ("POST", ["networks"]) => with(nets, "Network.Allocate", Special::CreateNetwork),
        (_, ["networks", "bridges"]) if get => with(node, "Sys.Audit", Special::ListBridges),
        (_, ["networks", net, rest @ ..]) => {
            let p = net_path(conn, net);
            let net = net.to_string();
            match (method, rest) {
                (_, [] | ["xml"] | ["ipam"] | ["firewall"]) if get => need(p, "Network.Audit"),
                ("DELETE", []) => with(p, "Network.Allocate", Special::DeleteNetwork { net }),
                ("PUT", ["xml"]) => with(p, "Network.Config", Special::PutNetworkXml { net }),
                ("POST", ["start" | "stop" | "autostart"]) => need(p, "Network.Config"),
                ("PUT", ["ipam" | "firewall"]) => need(p, "Network.Config"),
                // Reservations: who holds which address. Changing one is
                // editing the network's allocations, which the network's
                // owner and the admin may do.
                (_, ["hosts"]) if get => need(p, "Network.Audit"),
                ("PUT", ["hosts"]) | ("DELETE", ["hosts", _]) => need(p, "Network.Config"),
                _ => None,
            }
        }

        // Storage: pools are defined by administrators only.
        (_, ["pools"]) if get => with(format!("{node}/storage"), "Sys.Audit", Special::ListPools),
        ("POST", ["pools"]) => need(format!("{node}/storage"), "Datastore.Allocate"),
        (_, ["pools", pool, rest @ ..]) => {
            let p = pool_path(conn, pool);
            let pool = pool.to_string();
            match (method, rest) {
                (_, [] | ["xml"]) if get => need(p, "Datastore.Audit"),
                ("DELETE", []) | ("PUT", ["xml"]) => need(p, "Datastore.Allocate"),
                ("POST", ["start" | "stop" | "refresh"]) => need(p, "Datastore.Allocate"),
                (_, ["volumes"]) if get => {
                    with(p, "Datastore.Audit", Special::ListVolumes { pool })
                }
                ("POST", ["volumes"]) => {
                    with(p, "Datastore.AllocateSpace", Special::CreateVolume { pool })
                }
                (_, ["volumes", _] | ["volumes", _, "hash"]) if get => need(p, "Datastore.Audit"),
                ("DELETE", ["volumes", vol]) => with(
                    p,
                    "Datastore.AllocateSpace",
                    Special::DeleteVolume {
                        pool,
                        vol: vol.to_string(),
                    },
                ),
                // Uploads go through the web server's staging (`/uploads`),
                // which checks and records ownership; a direct one could
                // overwrite an existing volume.
                ("POST", ["volumes", _, "upload"]) => {
                    with(p, "Datastore.Allocate", Special::AdminOnly)
                }
                ("POST", ["fetch"]) => with(p, "Datastore.AllocateSpace", Special::Fetch { pool }),
                ("POST", ["templates"]) => with(
                    p,
                    "Datastore.AllocateSpace",
                    Special::FetchTemplate { pool },
                ),
                _ => None,
            }
        }
        // The fetch list names every download on the host; cancelling one is
        // an admin's call.
        (_, ["fetches"]) if get => need(node, "Sys.Audit"),
        ("DELETE", ["fetches", _]) => with(node, "Datastore.Allocate", Special::AdminOnly),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `classify` takes the path as written. That is only safe because the
    /// proxy first rewrites a UUID to the resource's name (`canonical_path`):
    /// the daemon resolves a UUID to a guest, and grants, ownership and
    /// suspension are recorded under the name, so a check on the UUID path
    /// would miss every one of them (a "NoAccess on this guest" carve-out
    /// would not hold). `test/live-uuid.mjs` checks the proxy does resolve.
    #[test]
    fn the_check_is_on_the_path_as_written() {
        let by_uuid = classify(
            "c",
            "DELETE",
            "/domains/ea15249b-a713-41dc-a808-05619e42eb2a",
        )
        .unwrap();
        assert_eq!(
            by_uuid.path,
            "/nodes/c/vms/ea15249b-a713-41dc-a808-05619e42eb2a"
        );
    }

    fn priv_of(method: &str, rest: &str) -> Option<(String, &'static str)> {
        classify("c", method, rest).map(|n| (n.path, n.privilege))
    }

    #[test]
    fn guest_actions_map_to_their_own_privileges() {
        let vm = "/nodes/c/vms/web";
        assert_eq!(
            priv_of("GET", "/domains/web"),
            Some((vm.into(), "VM.Audit"))
        );
        assert_eq!(
            priv_of("POST", "/domains/web/start"),
            Some((vm.into(), "VM.PowerMgmt"))
        );
        assert_eq!(
            priv_of("PUT", "/domains/web/xml"),
            Some((vm.into(), "VM.Config"))
        );
        assert_eq!(
            priv_of("POST", "/domains/web/console/key"),
            Some((vm.into(), "VM.Console"))
        );
        assert_eq!(
            priv_of("DELETE", "/domains/web"),
            Some((vm.into(), "VM.Allocate"))
        );
        assert_eq!(
            priv_of("POST", "/domains"),
            Some(("/nodes/c/vms".into(), "VM.Allocate"))
        );
    }

    #[test]
    fn containers_use_their_own_privileges_and_path() {
        let ct = "/nodes/c/containers/web";
        for (m, p, want) in [
            ("GET", "/containers/web", "CT.Audit"),
            ("POST", "/containers/web/start", "CT.PowerMgmt"),
            ("PUT", "/containers/web/xml", "CT.Config"),
            ("POST", "/containers/web/console/connect", "CT.Console"),
            ("DELETE", "/containers/web", "CT.Allocate"),
            (
                "PUT",
                "/containers/web/interfaces/52:54:00:00:00:01/bandwidth",
                "CT.Config",
            ),
            ("GET", "/containers/web/hardware", "CT.Audit"),
            ("PUT", "/containers/web/hardware", "CT.Config"),
        ] {
            assert_eq!(priv_of(m, p), Some((ct.into(), want)), "{m} {p}");
        }
        let n = classify("c", "POST", "/containers").unwrap();
        assert_eq!(
            (n.path.as_str(), n.privilege),
            ("/nodes/c/containers", "CT.Allocate")
        );
        assert_eq!(n.special, Special::CreateCt);
        assert_eq!(
            classify("c", "PUT", "/containers/web/xml").unwrap().special,
            Special::PutCtXml { ct: "web".into() }
        );
        // No VM-only routes on a container.
        assert_eq!(priv_of("POST", "/containers/web/reset"), None);
        assert_eq!(priv_of("GET", "/containers/web/graphics"), None);
        let t = classify("c", "POST", "/pools/p/templates").unwrap();
        assert_eq!(t.privilege, "Datastore.AllocateSpace");
        assert_eq!(t.special, Special::FetchTemplate { pool: "p".into() });
        assert_eq!(priv_of("GET", "/templates/catalog").unwrap().1, "Sys.Audit");
    }

    #[test]
    fn names_are_decoded_before_they_become_paths() {
        assert_eq!(
            priv_of("GET", "/domains/my%20vm/stats"),
            Some(("/nodes/c/vms/my vm".into(), "VM.Audit"))
        );
        // An encoded slash must not reach into another guest's ACL path.
        assert_eq!(classify("c", "GET", "/domains/a%2F..%2Fb"), None);
        assert_eq!(classify("c", "GET", "/domains/a%2Fb/xml"), None);
        assert_eq!(classify("c", "GET", "/domains/%2E%2E/xml"), None);
    }

    #[test]
    fn decoding_never_panics_on_odd_input() {
        assert_eq!(decode("a%20b"), "a b");
        assert_eq!(decode("%"), "%");
        assert_eq!(decode("x%2"), "x%2");
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("%é1"), "%é1");
    }

    #[test]
    fn unknown_routes_are_refused() {
        assert_eq!(priv_of("GET", "/secret"), None);
        assert_eq!(priv_of("POST", "/domains/web/migrate"), None);
        assert_eq!(priv_of("PATCH", "/domains/web"), None);
    }

    #[test]
    fn pools_are_admin_defined_and_users_allocate_space() {
        assert_eq!(priv_of("POST", "/pools").unwrap().1, "Datastore.Allocate");
        assert_eq!(
            priv_of("DELETE", "/pools/p").unwrap().1,
            "Datastore.Allocate"
        );
        assert_eq!(
            priv_of("POST", "/pools/p/volumes").unwrap().1,
            "Datastore.AllocateSpace"
        );
        let n = classify("c", "POST", "/pools/p/volumes/x/upload").unwrap();
        assert_eq!(n.special, Special::AdminOnly, "uploads go through /uploads");
    }

    #[test]
    fn host_wide_things_are_admin_only() {
        assert_eq!(
            classify("c", "POST", "/filters").unwrap().special,
            Special::AdminOnly
        );
        assert_eq!(
            classify("c", "GET", "/filters").unwrap().special,
            Special::AdminOnly
        );
    }
}
