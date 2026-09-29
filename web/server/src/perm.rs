//! Proxmox-style permissions: privileges, roles, ACL paths, ownership and
//! per-user resource limits.
//!
//! A *privilege* is one action ("VM.PowerMgmt"). A *role* is a named set of
//! them. An *ACL entry* grants a role to a user or group at a path; paths form
//! a tree (`/` > `/nodes/<conn>` > `/nodes/<conn>/vms` > `/nodes/<conn>/vms/<vm>`,
//! likewise `networks`, `storage` and `bridges`), and the deepest
//! path carrying an entry for the user decides — a grant lower down replaces
//! what was inherited rather than adding to it, as in Proxmox. That is what
//! makes "admin at /, but NoAccess on this one guest" expressible.
//!
//! On top of the ACL, whoever *created* a VM, network or volume owns it and
//! holds every privilege on it, so users see what they make without anyone
//! sharing it. A suspended resource drops everyone but administrators to
//! read-only, including its owner.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

/// Every privilege the backend checks. The UI renders exactly this list.
///
/// Managing accounts, groups, roles and ACLs is one privilege,
/// `Permissions.Modify`, deliberately: anyone who can edit accounts can reset
/// an administrator's password, so a separate "User.Modify" would only look
/// narrower than it is.
pub const PRIVILEGES: [&str; 22] = [
    "Sys.Audit",
    "Sys.Modify",
    "VM.Audit",
    "VM.Allocate",
    "VM.Config",
    "VM.PowerMgmt",
    "VM.Console",
    "CT.Audit",
    "CT.Allocate",
    "CT.Config",
    "CT.PowerMgmt",
    "CT.Console",
    "CT.Privileged",
    "Network.Audit",
    "Network.Allocate",
    "Network.Config",
    "Network.Use",
    "Datastore.Audit",
    "Datastore.AllocateSpace",
    "Datastore.Allocate",
    "Bridge.Use",
    "Permissions.Modify",
];

/// What the owner of a resource may do to it. Allocate covers deletion.
const VM_OWNER: [&str; 5] = [
    "VM.Audit",
    "VM.Allocate",
    "VM.Config",
    "VM.PowerMgmt",
    "VM.Console",
];
/// What the owner of a container may do to it. Never `CT.Privileged`: that
/// would let anyone who can create a container make it privileged -- its root
/// the host's root -- by editing it. Privilege is granted, not owned.
const CT_OWNER: [&str; 5] = [
    "CT.Audit",
    "CT.Allocate",
    "CT.Config",
    "CT.PowerMgmt",
    "CT.Console",
];
const CT_PRIVILEGED: [&str; 6] = [
    "CT.Audit",
    "CT.Allocate",
    "CT.Config",
    "CT.PowerMgmt",
    "CT.Console",
    "CT.Privileged",
];
const NET_OWNER: [&str; 4] = [
    "Network.Audit",
    "Network.Allocate",
    "Network.Config",
    "Network.Use",
];

pub type Privs = BTreeSet<String>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleDef {
    pub name: String,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub privs: Vec<String>,
    /// Pre-ACL scope map, read only to migrate an old store.
    #[serde(default, skip_serializing)]
    pub scopes: HashMap<String, String>,
}

fn role(name: &str, privs: &[&str]) -> RoleDef {
    RoleDef {
        name: name.into(),
        builtin: true,
        privs: privs.iter().map(|s| s.to_string()).collect(),
        scopes: HashMap::new(),
    }
}

/// The role a new account gets at `/`: see daemons and pools, create VMs,
/// networks and volumes — and so, through ownership, only its own.
pub const DEFAULT_ROLE: &str = "User";
pub const ADMIN_ROLE: &str = "Administrator";

pub fn builtin_roles() -> Vec<RoleDef> {
    vec![
        role(ADMIN_ROLE, &PRIVILEGES),
        role(
            DEFAULT_ROLE,
            &[
                "Sys.Audit",
                "Datastore.Audit",
                "Datastore.AllocateSpace",
                "VM.Allocate",
                "CT.Allocate",
                "Network.Allocate",
            ],
        ),
        role("VMUser", &["VM.Audit", "VM.Console", "VM.PowerMgmt"]),
        role("VMAdmin", &VM_OWNER),
        role("CTUser", &["CT.Audit", "CT.Console", "CT.PowerMgmt"]),
        role("CTAdmin", &CT_OWNER),
        // Grant on `/nodes/<c>/containers` (or one container) to let someone
        // run containers without user-namespace isolation.
        role("CTPrivileged", &CT_PRIVILEGED),
        role("NetworkUser", &["Network.Audit", "Network.Use"]),
        role("NetworkAdmin", &NET_OWNER),
        role(
            "DatastoreUser",
            &["Datastore.Audit", "Datastore.AllocateSpace"],
        ),
        role("BridgeUser", &["Bridge.Use"]),
        role(
            "Auditor",
            &[
                "Sys.Audit",
                "VM.Audit",
                "CT.Audit",
                "Network.Audit",
                "Datastore.Audit",
            ],
        ),
        role("NoAccess", &[]),
    ]
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub members: Vec<String>,
}

/// `ugid` is `user:<id>` or `group:<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AclEntry {
    pub path: String,
    pub ugid: String,
    pub role: String,
    #[serde(default = "yes")]
    pub propagate: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Vm,
    Ct,
    Network,
    Volume,
}

/// Ownership record. Anything a daemon has that is not recorded belongs to
/// `Store::default_owner` (the first administrator), which is how guests that
/// predate ownership, or were made outside this app, are treated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub kind: Kind,
    pub conn: String,
    /// Volumes only: the pool the volume lives in.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pool: String,
    pub name: String,
    pub owner: String,
    #[serde(default)]
    pub suspended: bool,
    /// Volumes: host path and size in bytes, for disk checks and the quota.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default)]
    pub capacity: u64,
    /// Volumes: created empty through the volume API, so the file holds
    /// nothing a user put there. Only such a volume may be attached as qcow2
    /// by a non-admin: an uploaded "qcow2" can name any host file as its
    /// backing store, and qemu would open it.
    #[serde(default)]
    pub blank: bool,
    /// When the record was made. A list fetched just before a create lands
    /// after it, and must not prune the new record as missing.
    #[serde(default)]
    pub created_ms: u64,
}

/// One set of ceilings. `None` is unlimited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limit {
    #[serde(default)]
    pub vms: Option<u64>,
    #[serde(default)]
    pub vcpus: Option<u64>,
    #[serde(default)]
    pub memory_mb: Option<u64>,
    #[serde(default)]
    pub storage_gb: Option<u64>,
    #[serde(default)]
    pub networks: Option<u64>,
}

/// A user's resource limits.
///
/// `per_daemon` mode applies `default` on every daemon separately; `shared`
/// pools `default` across all daemons together. Either way an entry in
/// `daemons` sets that daemon's own ceiling (in shared mode, on top of the
/// pool).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    #[serde(default)]
    pub shared: bool,
    #[serde(default)]
    pub default: Limit,
    #[serde(default)]
    pub daemons: HashMap<String, Limit>,
}

/// What a user has (or would have) on one daemon.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub vms: u64,
    pub vcpus: u64,
    pub memory_mb: u64,
    pub storage_bytes: u64,
    pub networks: u64,
}

impl Usage {
    pub fn add(self, o: Usage) -> Usage {
        Usage {
            vms: self.vms + o.vms,
            vcpus: self.vcpus + o.vcpus,
            memory_mb: self.memory_mb + o.memory_mb,
            storage_bytes: self.storage_bytes + o.storage_bytes,
            networks: self.networks + o.networks,
        }
    }
}

const GIB: u64 = 1024 * 1024 * 1024;

/// Check one set of limits. `after` is the usage the change would leave,
/// `asked` what the change adds. The message says which limit, what was asked
/// for, what is already in use, and where -- "this would use 4 of 2" left
/// users unable to tell whether their other VMs counted, or which server.
fn over(limit: &Limit, after: &Usage, asked: &Usage, scope: &str) -> Result<(), String> {
    let mib = |m: u64| {
        if m >= 1024 && m.is_multiple_of(1024) {
            format!("{} GiB", m / 1024)
        } else {
            format!("{m} MiB")
        }
    };
    let gib = |g: u64| format!("{g} GiB");
    let count = |x: u64| x.to_string();
    type Row<'a> = (&'a str, Option<u64>, u64, u64, &'a dyn Fn(u64) -> String);
    let rows: [Row; 5] = [
        ("VM", limit.vms, after.vms, asked.vms, &count),
        ("vCPU", limit.vcpus, after.vcpus, asked.vcpus, &count),
        (
            "Memory",
            limit.memory_mb,
            after.memory_mb,
            asked.memory_mb,
            &mib,
        ),
        (
            "Storage",
            limit.storage_gb,
            after.storage_bytes.div_ceil(GIB),
            asked.storage_bytes.div_ceil(GIB),
            &gib,
        ),
        (
            "Network",
            limit.networks,
            after.networks,
            asked.networks,
            &count,
        ),
    ];
    for (label, cap, used_after, want, fmt) in rows {
        let Some(cap) = cap else { continue };
        if used_after <= cap {
            continue;
        }
        let before = used_after.saturating_sub(want);
        let left = match cap.saturating_sub(before) {
            0 => "none left".to_string(),
            free => format!("{} left", fmt(free)),
        };
        return Err(format!(
            "{label} limit reached {scope}: this needs {} more, you already use {} of your {} ({left}). \
             Free some up, or ask an administrator to raise your limit.",
            fmt(want),
            fmt(before),
            fmt(cap),
        ));
    }
    Ok(())
}

/// Would the user stay within their limits after a change?
///
/// `here` is the resulting usage on the daemon being changed, `everywhere`
/// the resulting usage summed over all daemons (only consulted in shared
/// mode), `asked` what the change adds, and `server` the daemon's name for
/// the message.
pub fn check_quota(
    q: &Quota,
    conn: &str,
    server: &str,
    here: &Usage,
    everywhere: &Usage,
    asked: &Usage,
) -> Result<(), String> {
    let on = format!("on '{server}'");
    if let Some(l) = q.daemons.get(conn) {
        over(l, here, asked, &on)?;
    }
    if q.shared {
        over(&q.default, everywhere, asked, "across all servers")
    } else if q.daemons.contains_key(conn) {
        Ok(())
    } else {
        over(&q.default, here, asked, &on)
    }
}

/// The ACL path of a resource.
///
/// Everything a daemon has lives under its node, so one grant on
/// `/nodes/<conn>` covers the whole server and one on `/` covers them all.
pub fn node_path(conn: &str) -> String {
    format!("/nodes/{conn}")
}
pub fn vm_path(conn: &str, name: &str) -> String {
    format!("/nodes/{conn}/vms/{name}")
}
pub fn ct_path(conn: &str, name: &str) -> String {
    format!("/nodes/{conn}/containers/{name}")
}
pub fn net_path(conn: &str, name: &str) -> String {
    format!("/nodes/{conn}/networks/{name}")
}
pub fn pool_path(conn: &str, name: &str) -> String {
    format!("/nodes/{conn}/storage/{name}")
}
pub fn bridge_path(conn: &str, name: &str) -> String {
    format!("/nodes/{conn}/bridges/{name}")
}

/// `/a/b/c` -> [`/a/b/c`, `/a/b`, `/a`, `/`].
fn ancestry(path: &str) -> Vec<String> {
    let mut out = vec![];
    let mut p = path.trim_end_matches('/').to_string();
    while !p.is_empty() {
        out.push(p.clone());
        p.truncate(p.rfind('/').unwrap_or(0));
    }
    out.push("/".into());
    out
}

/// Privileges from the ACL alone.
///
/// Walk from the path up to `/`. At each level, the user's own entries win
/// over their groups' (so a user can be carved out of a group grant); the
/// first level that has any entry for the user decides. Above the path
/// itself, only propagating entries count.
pub fn acl_privs(
    acl: &[AclEntry],
    roles: &[RoleDef],
    groups: &[Group],
    user_id: &str,
    path: &str,
) -> Privs {
    let me = format!("user:{user_id}");
    let mine: Vec<String> = groups
        .iter()
        .filter(|g| g.members.iter().any(|m| m == user_id))
        .map(|g| format!("group:{}", g.name))
        .collect();
    for (depth, p) in ancestry(path).iter().enumerate() {
        let here = |who: &dyn Fn(&str) -> bool| -> Vec<&AclEntry> {
            acl.iter()
                .filter(|e| e.path == *p && who(&e.ugid) && (depth == 0 || e.propagate))
                .collect()
        };
        let mut hits = here(&|u| u == me);
        if hits.is_empty() {
            hits = here(&|u| mine.iter().any(|g| g == u));
        }
        if !hits.is_empty() {
            return hits
                .iter()
                .filter_map(|e| roles.iter().find(|r| r.name == e.role))
                .flat_map(|r| r.privs.iter().cloned())
                .collect();
        }
    }
    Privs::new()
}

/// What a privilege lets you do, in words, for a refusal message.
pub fn describe_priv(p: &str) -> &'static str {
    match p {
        "Sys.Audit" => "view this server",
        "Sys.Modify" => "manage servers",
        "VM.Audit" => "view this VM",
        "VM.Allocate" => "create or delete VMs here",
        "VM.Config" => "change this VM's configuration",
        "VM.PowerMgmt" => "start, stop or restart this VM",
        "VM.Console" => "open this VM's console",
        "CT.Audit" => "view this container",
        "CT.Allocate" => "create or delete containers here",
        "CT.Config" => "change this container's configuration",
        "CT.PowerMgmt" => "start, stop or restart this container",
        "CT.Console" => "open this container's console",
        "CT.Privileged" => {
            "run a privileged container (no user-namespace isolation: its root is the host's root)"
        }
        "Network.Audit" => "view this network",
        "Network.Allocate" => "create or delete networks here",
        "Network.Config" => "change this network",
        "Network.Use" => "connect VMs to this network",
        "Datastore.Audit" => "view this storage pool",
        "Datastore.AllocateSpace" => "create volumes in this storage pool",
        "Datastore.Allocate" => "manage storage pools",
        "Bridge.Use" => "use this bridge",
        "Permissions.Modify" => "administer users and permissions",
        _ => "do that",
    }
}

/// A path as a person reads it: `/nodes/<id>/vms/web` becomes
/// `VM 'web' on <server name>`. `server` maps a connection id to its name.
pub fn describe_path(path: &str, server: &dyn Fn(&str) -> Option<String>) -> String {
    let seg: Vec<&str> = path.trim_matches('/').splitn(4, '/').collect();
    let srv = |c: &str| {
        server(c)
            .map(|n| format!("'{n}'"))
            .unwrap_or_else(|| "this server".into())
    };
    match seg.as_slice() {
        [""] => "the whole installation".into(),
        ["nodes"] => "every server".into(),
        ["nodes", c] => format!("server {}", srv(c)),
        ["nodes", c, "vms"] => format!("VMs on {}", srv(c)),
        ["nodes", c, "containers"] => format!("containers on {}", srv(c)),
        ["nodes", c, "networks"] => format!("networks on {}", srv(c)),
        ["nodes", c, "storage"] => format!("storage on {}", srv(c)),
        ["nodes", c, "bridges"] => format!("bridges on {}", srv(c)),
        ["nodes", c, "vms", n] => format!("VM '{n}' on {}", srv(c)),
        ["nodes", c, "containers", n] => format!("container '{n}' on {}", srv(c)),
        ["nodes", c, "networks", n] => format!("network '{n}' on {}", srv(c)),
        ["nodes", c, "storage", n] => format!("storage pool '{n}' on {}", srv(c)),
        ["nodes", c, "bridges", n] => format!("bridge '{n}' on {}", srv(c)),
        _ => path.to_string(),
    }
}

/// The resource a path names, if it names one.
pub fn path_resource(path: &str) -> Option<(Kind, &str, &str)> {
    let seg: Vec<&str> = path.trim_matches('/').splitn(4, '/').collect();
    match seg.as_slice() {
        ["nodes", conn, "vms", name] => Some((Kind::Vm, conn, name)),
        ["nodes", conn, "containers", name] => Some((Kind::Ct, conn, name)),
        ["nodes", conn, "networks", name] => Some((Kind::Network, conn, name)),
        _ => None,
    }
}

pub fn owner_privs(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Vm => &VM_OWNER,
        Kind::Ct => &CT_OWNER,
        Kind::Network => &NET_OWNER,
        Kind::Volume => &[],
    }
}

/// Whether a pre-ACL scope map could have expressed `p`. Containers came
/// after ACLs, so no old scope grants a `CT.*` privilege -- and an old
/// full-access account must still count as full, or it would lose the
/// Administrator role in migration for want of privileges it never needed.
pub fn legacy_expressible(p: &str) -> bool {
    !p.starts_with("CT.")
}

/// Map a pre-ACL scope map to privileges, so an upgraded install keeps the
/// access it had (the admin can then narrow it).
pub fn legacy_privs(scopes: &HashMap<String, String>) -> Vec<String> {
    let mut out = BTreeSet::new();
    for (scope, level) in scopes {
        let (read, write): (&[&str], &[&str]) = match scope.as_str() {
            "connections" => (&["Sys.Audit"], &["Sys.Modify"]),
            "vms" => (&["VM.Audit"], &VM_OWNER),
            "networks" => (
                &["Network.Audit"],
                &[
                    "Network.Allocate",
                    "Network.Config",
                    "Network.Use",
                    "Bridge.Use",
                ],
            ),
            "storage" => (
                &["Datastore.Audit"],
                &["Datastore.AllocateSpace", "Datastore.Allocate"],
            ),
            "users" => (&[], &["Permissions.Modify"]),
            _ => (&[], &[]),
        };
        if level == "read" || level == "write" {
            out.extend(read.iter().map(|s| s.to_string()));
        }
        if level == "write" {
            out.extend(read.iter().chain(write).map(|s| s.to_string()));
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, ugid: &str, role: &str, propagate: bool) -> AclEntry {
        AclEntry {
            path: path.into(),
            ugid: ugid.into(),
            role: role.into(),
            propagate,
        }
    }

    fn has(p: &Privs, s: &str) -> bool {
        p.contains(s)
    }

    #[test]
    fn a_grant_propagates_down_the_tree() {
        let acl = [entry("/nodes/c1/vms", "user:u", "VMUser", true)];
        let p = acl_privs(&acl, &builtin_roles(), &[], "u", &vm_path("c1", "web"));
        assert!(has(&p, "VM.PowerMgmt"));
        assert!(!has(&p, "VM.Config"));
    }

    #[test]
    fn a_non_propagating_grant_stays_on_its_path() {
        let acl = [entry("/nodes/c1/vms", "user:u", "VMAdmin", false)];
        let r = builtin_roles();
        assert!(has(
            &acl_privs(&acl, &r, &[], "u", "/nodes/c1/vms"),
            "VM.Allocate"
        ));
        assert!(acl_privs(&acl, &r, &[], "u", &vm_path("c1", "web")).is_empty());
    }

    #[test]
    fn a_deeper_grant_replaces_the_inherited_one() {
        let acl = [
            entry("/", "user:u", ADMIN_ROLE, true),
            entry("/nodes/c1/vms/secret", "user:u", "NoAccess", true),
        ];
        let r = builtin_roles();
        assert!(acl_privs(&acl, &r, &[], "u", "/nodes/c1/vms/secret").is_empty());
        assert!(has(
            &acl_privs(&acl, &r, &[], "u", "/nodes/c1/vms/other"),
            "VM.Config"
        ));
    }

    #[test]
    fn group_grants_apply_and_the_users_own_entry_wins() {
        let g = [Group {
            name: "ops".into(),
            comment: String::new(),
            members: vec!["u".into()],
        }];
        let r = builtin_roles();
        let acl = [entry("/nodes", "group:ops", "VMAdmin", true)];
        assert!(has(
            &acl_privs(&acl, &r, &g, "u", "/nodes/c/vms/x"),
            "VM.Config"
        ));
        assert!(acl_privs(&acl, &r, &g, "other", "/nodes/c/vms/x").is_empty());
        let acl = [
            entry("/nodes", "group:ops", "VMAdmin", true),
            entry("/nodes", "user:u", "VMUser", true),
        ];
        let p = acl_privs(&acl, &r, &g, "u", "/nodes/c/vms/x");
        assert!(has(&p, "VM.Console") && !has(&p, "VM.Config"), "{p:?}");
    }

    #[test]
    fn refusals_name_the_thing_in_words() {
        let srv = |c: &str| (c == "c1").then(|| "hv1".to_string());
        assert_eq!(
            describe_path(&vm_path("c1", "web"), &srv),
            "VM 'web' on 'hv1'"
        );
        assert_eq!(describe_path("/nodes/c1/vms", &srv), "VMs on 'hv1'");
        assert_eq!(
            describe_path("/nodes/gone/networks/x", &srv),
            "network 'x' on this server"
        );
        assert_eq!(describe_path("/", &srv), "the whole installation");
        for p in PRIVILEGES {
            assert_ne!(describe_priv(p), "do that", "{p} has a description");
        }
    }

    /// VMs and containers are separate permission sets, and privileged
    /// containers are a third thing again: nothing implies it but a grant.
    #[test]
    fn container_privileges_are_their_own_and_privileged_is_granted_only() {
        let r = builtin_roles();
        let get = |n: &str| r.iter().find(|x| x.name == n).unwrap().privs.clone();
        assert!(get("VMAdmin").iter().all(|p| p.starts_with("VM.")));
        assert!(get("CTAdmin").iter().all(|p| p.starts_with("CT.")));
        assert!(!get("CTAdmin").contains(&"CT.Privileged".to_string()));
        assert!(get("CTPrivileged").contains(&"CT.Privileged".to_string()));
        let user = get(DEFAULT_ROLE);
        assert!(
            user.contains(&"CT.Allocate".to_string()),
            "users can create"
        );
        assert!(!user.contains(&"CT.Privileged".to_string()));
        assert!(!owner_privs(Kind::Ct).contains(&"CT.Privileged"));
        // A grant on one container does not reach its siblings.
        let acl = [entry(&ct_path("c", "a"), "user:u", "CTPrivileged", true)];
        assert!(has(
            &acl_privs(&acl, &r, &[], "u", &ct_path("c", "a")),
            "CT.Privileged"
        ));
        assert!(acl_privs(&acl, &r, &[], "u", &ct_path("c", "b")).is_empty());
        assert_eq!(
            path_resource(&ct_path("c", "a")),
            Some((Kind::Ct, "c", "a"))
        );
    }

    #[test]
    fn nothing_granted_means_nothing() {
        assert!(acl_privs(&[], &builtin_roles(), &[], "u", "/").is_empty());
    }

    #[test]
    fn resources_are_named_by_their_paths() {
        assert_eq!(
            path_resource(&vm_path("c1", "web")),
            Some((Kind::Vm, "c1", "web"))
        );
        assert_eq!(
            path_resource(&net_path("c1", "lan")),
            Some((Kind::Network, "c1", "lan"))
        );
        assert_eq!(path_resource("/nodes/c1/vms"), None);
        assert_eq!(path_resource(&pool_path("c1", "p")), None);
    }

    fn usage(vms: u64, vcpus: u64, mem: u64, gb: u64) -> Usage {
        Usage {
            vms,
            vcpus,
            memory_mb: mem,
            storage_bytes: gb * GIB,
            networks: 0,
        }
    }

    /// The two shapes from the brief: one fixed VM, or a pool of three.
    #[test]
    fn a_single_vm_quota_refuses_the_second_vm_and_bigger_sizes() {
        let q = Quota {
            shared: false,
            default: Limit {
                vms: Some(1),
                vcpus: Some(2),
                memory_mb: Some(2048),
                storage_gb: Some(20),
                networks: None,
            },
            daemons: HashMap::new(),
        };
        let ok = usage(1, 2, 2048, 20);
        assert!(check_quota(&q, "c1", "hv", &ok, &ok, &Usage::default()).is_ok());
        let e = check_quota(
            &q,
            "c1",
            "hv",
            &usage(2, 2, 2048, 20),
            &ok,
            &usage(1, 0, 0, 0),
        )
        .unwrap_err();
        assert!(e.starts_with("VM limit reached on 'hv'"), "{e}");
        assert!(
            e.contains("needs 1 more, you already use 1 of your 1 (none left)"),
            "{e}"
        );
        assert!(check_quota(
            &q,
            "c1",
            "hv",
            &usage(1, 3, 2048, 20),
            &ok,
            &Usage::default()
        )
        .is_err());
        assert!(check_quota(
            &q,
            "c1",
            "hv",
            &usage(1, 2, 2049, 20),
            &ok,
            &Usage::default()
        )
        .is_err());
        assert!(check_quota(
            &q,
            "c1",
            "hv",
            &usage(1, 2, 2048, 21),
            &ok,
            &Usage::default()
        )
        .is_err());
    }

    /// What a user reads when a create is refused: the limit, the ask, the
    /// current use, what is left, and the server -- in their units.
    #[test]
    fn a_refusal_says_what_was_asked_and_what_is_left() {
        let q = Quota {
            shared: false,
            default: Limit {
                vcpus: Some(6),
                memory_mb: Some(8192),
                storage_gb: Some(60),
                ..Limit::default()
            },
            daemons: HashMap::new(),
        };
        let ask = |c, m, g| Usage {
            vcpus: c,
            memory_mb: m,
            storage_bytes: g * GIB,
            ..Usage::default()
        };
        let e =
            check_quota(&q, "c", "hv1", &ask(8, 0, 0), &ask(8, 0, 0), &ask(4, 0, 0)).unwrap_err();
        assert_eq!(
            e,
            "vCPU limit reached on 'hv1': this needs 4 more, you already use 4 of your 6 (2 left). \
             Free some up, or ask an administrator to raise your limit."
        );
        let e = check_quota(
            &q,
            "c",
            "hv1",
            &ask(0, 10240, 0),
            &ask(0, 10240, 0),
            &ask(0, 4096, 0),
        )
        .unwrap_err();
        assert!(
            e.contains("Memory limit reached")
                && e.contains("needs 4 GiB more, you already use 6 GiB of your 8 GiB (2 GiB left)"),
            "{e}"
        );
        let e = check_quota(
            &q,
            "c",
            "hv1",
            &ask(0, 0, 70),
            &ask(0, 0, 70),
            &ask(0, 0, 20),
        )
        .unwrap_err();
        assert!(
            e.contains("Storage limit reached")
                && e.contains("needs 20 GiB more, you already use 50 GiB of your 60 GiB"),
            "{e}"
        );
    }

    #[test]
    fn per_daemon_limits_apply_separately_and_shared_ones_together() {
        let lim = Limit {
            vms: Some(3),
            vcpus: Some(6),
            memory_mb: Some(8192),
            storage_gb: Some(60),
            networks: None,
        };
        let here = usage(2, 4, 4096, 40);
        let total = usage(4, 8, 8192, 80);
        let per = Quota {
            shared: false,
            default: lim.clone(),
            daemons: HashMap::new(),
        };
        assert!(
            check_quota(&per, "c1", "hv", &here, &total, &Usage::default()).is_ok(),
            "per daemon"
        );
        let shared = Quota {
            shared: true,
            ..per.clone()
        };
        let e = check_quota(&shared, "c1", "hv", &here, &total, &Usage::default()).unwrap_err();
        assert!(e.contains("across all servers"), "{e}");
    }

    #[test]
    fn a_daemon_override_replaces_the_default_or_tightens_the_pool() {
        let mut daemons = HashMap::new();
        daemons.insert(
            "small".to_string(),
            Limit {
                vms: Some(1),
                ..Limit::default()
            },
        );
        let q = Quota {
            shared: false,
            default: Limit {
                vms: Some(5),
                ..Limit::default()
            },
            daemons,
        };
        let two = usage(2, 0, 0, 0);
        assert!(
            check_quota(&q, "small", "hv", &two, &two, &Usage::default()).is_err(),
            "override"
        );
        assert!(
            check_quota(&q, "big", "hv", &two, &two, &Usage::default()).is_ok(),
            "default"
        );
        let pooled = Quota { shared: true, ..q };
        assert!(check_quota(&pooled, "small", "hv", &two, &two, &Usage::default()).is_err());
        assert!(check_quota(
            &pooled,
            "big",
            "hv",
            &two,
            &usage(6, 0, 0, 0),
            &Usage::default()
        )
        .is_err());
    }

    #[test]
    fn legacy_scopes_keep_their_access() {
        let mut s = HashMap::new();
        s.insert("vms".to_string(), "write".to_string());
        s.insert("storage".to_string(), "read".to_string());
        s.insert("users".to_string(), "none".to_string());
        let p = legacy_privs(&s);
        for want in ["VM.Audit", "VM.Allocate", "VM.Config", "Datastore.Audit"] {
            assert!(p.contains(&want.to_string()), "{want} in {p:?}");
        }
        assert!(!p.contains(&"Datastore.Allocate".to_string()));
        assert!(!p.contains(&"Permissions.Modify".to_string()));
    }
}
