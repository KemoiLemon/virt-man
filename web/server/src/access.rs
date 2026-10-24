//! Access log: what each guest connected to, kept two weeks.
//!
//! The daemons record connections (from the kernel's connection tracker) and
//! DNS lookups (from their own resolver) in a small ring; this pulls them every
//! few seconds into SQLite (`access.db`) and answers the log view's queries.
//! Pulled with a per-connection cursor, so nothing is fetched twice and a web
//! server restart resumes where it left off.
//!
//! Rows are deleted with their subject, like the chart history: a removed
//! server's rows go at once. A deleted VM's rows are kept, deliberately: "what
//! did that VM talk to before it was deleted" is exactly when this log is
//! wanted, and they age out with everything else.
//!
//! Storage is bounded two ways: rows older than [`RETAIN_MS`], and, when the
//! disk runs low or the file grows past [`MAX_DB_BYTES`], the oldest rows
//! until it is back under. The log must never be what fills the disk.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const PULL_MS: u64 = 5_000;
pub const RETAIN_MS: i64 = 14 * 24 * 3600 * 1000;
/// The log never grows past this, whatever the disk has free.
pub const MAX_DB_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Purge while the filesystem holding the log has less than this free.
pub const MIN_FREE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// …or less than this fraction free, whichever is larger. A 20 GB disk with
/// 5 GB free is already in trouble.
pub const MIN_FREE_FRACTION: f64 = 0.05;

/// One record from a daemon's `GET /flows`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Flow {
    pub seq: u64,
    #[serde(default)]
    pub ct_id: u32,
    pub opened_ms: u64,
    pub closed_ms: u64,
    pub network: String,
    pub mac: Option<String>,
    pub vm: Option<String>,
    pub vm_name: Option<String>,
    pub proto: u8,
    pub src: String,
    pub sport: u16,
    pub dst: String,
    pub dport: u16,
    pub domain: Option<String>,
    pub bytes_out: u64,
    pub bytes_in: u64,
    /// "in" or "out": whether the guest opened the connection or was
    /// reached. Absent from a daemon that predates the field, and everything
    /// such a daemon recorded is outbound.
    #[serde(default = "outbound")]
    pub dir: String,
    #[serde(default)]
    pub packets_out: u64,
    #[serde(default)]
    pub packets_in: u64,
}

fn outbound() -> String {
    "out".into()
}

#[derive(Debug, Deserialize)]
struct Batch {
    flows: Vec<Flow>,
    last: u64,
}

#[derive(Clone)]
pub struct Access {
    db: Arc<Mutex<Connection>>,
    path: PathBuf,
}

/// A log query from the view. Every filter is optional.
#[derive(Debug, Default, Deserialize)]
pub struct Query {
    #[serde(default)]
    pub conn: String,
    /// VM name (what the UI knows). Empty: every VM.
    #[serde(default)]
    pub vm: String,
    /// `vm` names a container (its access is checked as one).
    #[serde(default)]
    pub ct: bool,
    #[serde(default)]
    pub proto: String,
    /// Substring of the destination IP or domain.
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub since_ms: Option<i64>,
    /// none | proto | domain | ip | port | vm
    #[serde(default)]
    pub group: String,
    /// time | count | bytes | domain | ip | port | proto | vm
    #[serde(default)]
    pub sort: String,
    /// asc | desc (default desc)
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
    /// dns | connections | both (default both). DNS rows are the guests'
    /// lookups through the network's resolver; connections are everything the
    /// kernel tracked.
    #[serde(default)]
    pub kind: String,
    /// Older form of `kind`: `dns=false` means connections only.
    #[serde(default)]
    pub dns: Option<bool>,
    /// in | out | "" (both). Spelled out because `dir` is the sort direction.
    #[serde(default)]
    pub direction: String,
}

pub fn proto_name(p: u8) -> &'static str {
    match p {
        1 => "icmp",
        6 => "tcp",
        17 => "udp",
        58 => "icmpv6",
        132 => "sctp",
        _ => "other",
    }
}

fn proto_num(s: &str) -> Option<u8> {
    Some(match s {
        "icmp" => 1,
        "tcp" => 6,
        "udp" => 17,
        "sctp" => 132,
        _ => return None,
    })
}

impl Access {
    pub fn open(path: &Path) -> Result<Self, String> {
        let db = Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA auto_vacuum=INCREMENTAL;
             CREATE TABLE IF NOT EXISTS flows(
               id INTEGER PRIMARY KEY,
               conn TEXT NOT NULL,
               ct_id INTEGER NOT NULL,
               opened INTEGER NOT NULL,
               closed INTEGER NOT NULL,
               network TEXT NOT NULL,
               mac TEXT, vm TEXT, vm_name TEXT,
               proto INTEGER NOT NULL,
               src TEXT NOT NULL, sport INTEGER NOT NULL,
               dst TEXT NOT NULL, dport INTEGER NOT NULL,
               domain TEXT,
               bytes_out INTEGER NOT NULL, bytes_in INTEGER NOT NULL,
               dir TEXT NOT NULL DEFAULT 'out');
             CREATE INDEX IF NOT EXISTS flows_time ON flows(conn, opened);
             CREATE INDEX IF NOT EXISTS flows_vm ON flows(conn, vm_name, opened);
             CREATE UNIQUE INDEX IF NOT EXISTS flows_ct ON flows(conn, ct_id, opened) WHERE ct_id <> 0;
             CREATE TABLE IF NOT EXISTS cursors(conn TEXT PRIMARY KEY, seq INTEGER NOT NULL);",
        )
        .map_err(|e| format!("init access db: {e}"))?;
        // A database written before rows carried a direction: everything in it
        // is outbound, which is what the column's default says.
        let known: i64 = db
            .query_row(
                "SELECT count(*) FROM pragma_table_info('flows') WHERE name = 'dir'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if known == 0 {
            db.execute_batch("ALTER TABLE flows ADD COLUMN dir TEXT NOT NULL DEFAULT 'out'")
                .map_err(|e| format!("add the dir column: {e}"))?;
        }
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            path: path.to_path_buf(),
        })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().expect("access lock poisoned")
    }

    pub fn cursor(&self, conn: &str) -> u64 {
        self.db()
            .query_row("SELECT seq FROM cursors WHERE conn=?1", [conn], |r| {
                r.get::<_, i64>(0)
            })
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0) as u64
    }

    /// Store a batch and move the cursor, in one transaction: a crash between
    /// the two would re-fetch (harmless, rows are upserts) rather than skip.
    ///
    /// A connection arrives twice, open then closed, with the same kernel id
    /// and open time: the close fills in the row the open made.
    pub fn record(&self, conn: &str, flows: &[Flow], last: u64) -> Result<usize, String> {
        let mut db = self.db();
        let tx = db.transaction().map_err(|e| e.to_string())?;
        {
            let mut ins = tx
                .prepare_cached(
                    "INSERT INTO flows(conn, ct_id, opened, closed, network, mac, vm, vm_name, proto,
                       src, sport, dst, dport, domain, bytes_out, bytes_in, dir)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
                     ON CONFLICT(conn, ct_id, opened) WHERE ct_id <> 0 DO UPDATE SET
                       closed = excluded.closed,
                       bytes_out = excluded.bytes_out,
                       bytes_in = excluded.bytes_in,
                       domain = COALESCE(flows.domain, excluded.domain)",
                )
                .map_err(|e| e.to_string())?;
            for f in flows {
                ins.execute(params![
                    conn,
                    f.ct_id,
                    f.opened_ms as i64,
                    f.closed_ms as i64,
                    f.network,
                    f.mac,
                    f.vm,
                    f.vm_name,
                    f.proto,
                    f.src,
                    f.sport,
                    f.dst,
                    f.dport,
                    f.domain,
                    f.bytes_out as i64,
                    f.bytes_in as i64,
                    // The close record of the same connection carries the same
                    // direction, so the insert's value stands.
                    f.dir,
                ])
                .map_err(|e| e.to_string())?;
            }
        }
        tx.execute(
            "INSERT INTO cursors(conn, seq) VALUES (?1, ?2) ON CONFLICT(conn) DO UPDATE SET seq=excluded.seq",
            params![conn, last as i64],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(flows.len())
    }

    /// A daemon restarted: its sequence numbers start over, so the cursor must
    /// too, or every new record would look already-pulled.
    pub fn reset_cursor(&self, conn: &str) -> Result<(), String> {
        self.db()
            .execute("DELETE FROM cursors WHERE conn=?1", [conn])
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Drop everything from connections no longer configured.
    pub fn keep_only(&self, live: &[String]) -> Result<usize, String> {
        let db = self.db();
        let conns: Vec<String> = {
            let mut st = db
                .prepare("SELECT DISTINCT conn FROM cursors")
                .map_err(|e| e.to_string())?;
            let rows = st.query_map([], |r| r.get(0)).map_err(|e| e.to_string())?;
            rows.filter_map(Result::ok).collect()
        };
        let mut n = 0;
        for c in conns.iter().filter(|c| !live.contains(c)) {
            n += db
                .execute("DELETE FROM flows WHERE conn=?1", [c])
                .map_err(|e| e.to_string())?;
            db.execute("DELETE FROM cursors WHERE conn=?1", [c])
                .map_err(|e| e.to_string())?;
        }
        Ok(n)
    }

    /// Rows older than the retention window.
    pub fn expire(&self, now_ms: i64) -> Result<usize, String> {
        self.db()
            .execute("DELETE FROM flows WHERE opened < ?1", [now_ms - RETAIN_MS])
            .map_err(|e| e.to_string())
    }

    /// Bytes the database takes on disk (file + WAL).
    pub fn size(&self) -> u64 {
        let f = |p: PathBuf| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let mut wal = self.path.clone().into_os_string();
        wal.push("-wal");
        f(self.path.clone()) + f(wal.into())
    }

    /// Delete the oldest `fraction` of rows and give the space back.
    pub fn purge_oldest(&self, fraction: f64) -> Result<usize, String> {
        let db = self.db();
        let total: i64 = db
            .query_row("SELECT count(*) FROM flows", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let n = ((total as f64) * fraction).ceil() as i64;
        if n <= 0 {
            return Ok(0);
        }
        let cut: Option<i64> = db
            .query_row(
                "SELECT opened FROM flows ORDER BY opened LIMIT 1 OFFSET ?1",
                [n - 1],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some(cut) = cut else { return Ok(0) };
        let gone = db
            .execute("DELETE FROM flows WHERE opened <= ?1", [cut])
            .map_err(|e| e.to_string())?;
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA incremental_vacuum;")
            .map_err(|e| e.to_string())?;
        Ok(gone)
    }

    /// Trim until the log is under its cap and the disk has room, oldest
    /// first. Returns how many rows went.
    pub fn enforce_space(&self, free: &dyn Fn() -> Option<(u64, u64)>) -> Result<usize, String> {
        let mut gone = 0;
        for _ in 0..20 {
            let too_big = self.size() > MAX_DB_BYTES;
            let low = free().is_some_and(|(avail, total)| low_on_space(avail, total));
            if !too_big && !low {
                break;
            }
            let n = self.purge_oldest(0.10)?;
            if n == 0 {
                break;
            }
            gone += n;
        }
        Ok(gone)
    }

    /// Rows (group = "") or groups, for the view.
    pub fn query(&self, q: &Query, now_ms: i64) -> Result<serde_json::Value, String> {
        let mut wh = vec!["conn = ?".to_string()];
        let mut args: Vec<rusqlite::types::Value> = vec![q.conn.clone().into()];
        let since = q.since_ms.unwrap_or(now_ms - RETAIN_MS);
        wh.push("opened >= ?".into());
        args.push(since.into());
        if !q.vm.is_empty() {
            wh.push("vm_name = ?".into());
            args.push(q.vm.clone().into());
        }
        if let Some(p) = proto_num(&q.proto) {
            wh.push("proto = ?".into());
            args.push(i64::from(p).into());
        }
        if let Some(p) = q.port {
            wh.push("dport = ?".into());
            args.push(i64::from(p).into());
        }
        // A lookup row is the resolver's record: no kernel id, port 53.
        if !q.q.trim().is_empty() {
            wh.push("(dst LIKE ? ESCAPE '\\' OR domain LIKE ? ESCAPE '\\')".into());
            let like = format!(
                "%{}%",
                q.q.trim()
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            args.push(like.clone().into());
            args.push(like.into());
        }
        // A lookup row is the resolver's record: no kernel id, port 53.
        match (q.kind.as_str(), q.dns) {
            ("dns", _) => wh.push("(dport = 53 AND ct_id = 0)".into()),
            ("connections", _) | ("", Some(false)) => {
                wh.push("NOT (dport = 53 AND ct_id = 0)".into())
            }
            ("" | "both", _) => {}
            (other, _) => return Err(format!("unknown kind '{other}'")),
        }
        match q.direction.as_str() {
            "in" | "out" => {
                wh.push("dir = ?".into());
                args.push(q.direction.clone().into());
            }
            "" | "both" => {}
            other => return Err(format!("unknown direction '{other}'")),
        }
        let wh = wh.join(" AND ");
        let dir = if q.dir == "asc" { "ASC" } else { "DESC" };
        let limit = q.limit.unwrap_or(200).clamp(1, 1000);
        let offset = q.offset.unwrap_or(0).max(0);
        let db = self.db();

        let key = match q.group.as_str() {
            "proto" => Some("proto"),
            "domain" => Some("COALESCE(domain, '')"),
            "ip" => Some("dst"),
            "port" => Some("dport"),
            "vm" => Some("COALESCE(vm_name, '')"),
            "" | "none" => None,
            other => return Err(format!("unknown grouping '{other}'")),
        };
        if let Some(key) = key {
            let order = match q.sort.as_str() {
                "bytes" => "bytes",
                "key" | "name" => "k",
                "time" => "last",
                _ => "hits",
            };
            let sql = format!(
                "SELECT {key} AS k, count(*) AS hits, sum(bytes_out + bytes_in) AS bytes,
                        count(DISTINCT vm_name) AS vms, max(opened) AS last
                 FROM flows WHERE {wh} GROUP BY k ORDER BY {order} {dir}, k LIMIT {limit} OFFSET {offset}"
            );
            let total_sql = format!(
                "SELECT count(*) FROM (SELECT {key} AS k FROM flows WHERE {wh} GROUP BY k)"
            );
            let total: i64 = db
                .query_row(&total_sql, params_from_iter(args.iter()), |r| r.get(0))
                .map_err(|e| e.to_string())?;
            let mut st = db.prepare(&sql).map_err(|e| e.to_string())?;
            let rows = st
                .query_map(params_from_iter(args.iter()), |r| {
                    let k: rusqlite::types::Value = r.get(0)?;
                    let key = match k {
                        rusqlite::types::Value::Integer(i) if q.group == "proto" => {
                            proto_name(i as u8).to_string()
                        }
                        rusqlite::types::Value::Integer(i) => i.to_string(),
                        rusqlite::types::Value::Text(s) => s,
                        _ => String::new(),
                    };
                    Ok(serde_json::json!({
                        "key": key,
                        "count": r.get::<_, i64>(1)?,
                        "bytes": r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        "vms": r.get::<_, i64>(3)?,
                        "last_ms": r.get::<_, i64>(4)?,
                    }))
                })
                .map_err(|e| e.to_string())?;
            let groups: Vec<_> = rows.filter_map(Result::ok).collect();
            return Ok(serde_json::json!({"group": q.group, "total": total, "groups": groups}));
        }

        let order = match q.sort.as_str() {
            "bytes" => "(bytes_out + bytes_in)",
            "domain" => "domain",
            "ip" => "dst",
            "port" => "dport",
            "proto" => "proto",
            "vm" => "vm_name",
            _ => "opened",
        };
        let total: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM flows WHERE {wh}"),
                params_from_iter(args.iter()),
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        let sql = format!(
            "SELECT opened, closed, vm_name, network, proto, src, sport, dst, dport, domain, bytes_out, bytes_in, ct_id, dir
             FROM flows WHERE {wh} ORDER BY {order} {dir}, id {dir} LIMIT {limit} OFFSET {offset}"
        );
        let mut st = db.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = st
            .query_map(params_from_iter(args.iter()), |r| {
                let dport: i64 = r.get(8)?;
                let ct: i64 = r.get(12)?;
                Ok(serde_json::json!({
                    "opened_ms": r.get::<_, i64>(0)?,
                    "closed_ms": r.get::<_, i64>(1)?,
                    "vm": r.get::<_, Option<String>>(2)?,
                    "network": r.get::<_, String>(3)?,
                    "proto": proto_name(r.get::<_, i64>(4)? as u8),
                    "src": r.get::<_, String>(5)?,
                    "sport": r.get::<_, i64>(6)?,
                    "dst": r.get::<_, String>(7)?,
                    "dport": dport,
                    "domain": r.get::<_, Option<String>>(9)?,
                    "bytes_out": r.get::<_, i64>(10)?,
                    "bytes_in": r.get::<_, i64>(11)?,
                    "dir": r.get::<_, String>(13)?,
                    // A lookup through the network's resolver, not a connection.
                    "kind": if ct == 0 && dport == 53 { "dns" } else { "connection" },
                }))
            })
            .map_err(|e| e.to_string())?;
        let rows: Vec<_> = rows.filter_map(Result::ok).collect();
        Ok(serde_json::json!({"total": total, "rows": rows}))
    }

    /// VM names that have rows, for the filter.
    pub fn vms(&self, conn: &str) -> Result<Vec<String>, String> {
        let db = self.db();
        let mut st = db
            .prepare("SELECT DISTINCT vm_name FROM flows WHERE conn=?1 AND vm_name IS NOT NULL ORDER BY vm_name")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([conn], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        Ok(rows.filter_map(Result::ok).collect())
    }
}

/// Whether the disk is low enough that the log should give space back.
pub fn low_on_space(avail: u64, total: u64) -> bool {
    let need = MIN_FREE_BYTES.max((total as f64 * MIN_FREE_FRACTION) as u64);
    avail < need
}

/// (available, total) bytes of the filesystem holding `path`.
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let bs = s.f_frsize as u64;
    Some((s.f_bavail as u64 * bs, s.f_blocks as u64 * bs))
}

// ------------------------------------------------------------ collector

/// Pull every connection's new flows forever, and keep the store in bounds.
pub async fn collect(state: crate::state::AppState) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(PULL_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n: u64 = 0;
    loop {
        tick.tick().await;
        let conns = state.read().connections.clone();
        for c in &conns {
            if let Err(e) = pull(&state, c).await {
                tracing::debug!("access log {}: {e}", c.name);
            }
        }
        let a = state.access.clone();
        let live: Vec<String> = conns.iter().map(|c| c.id.clone()).collect();
        let data = state.data_dir.clone();
        // Housekeeping once a minute: cheap, and a burst cannot fill the disk
        // faster than that at a few hundred bytes a row.
        let housekeep = n.is_multiple_of(60_000 / PULL_MS);
        n += 1;
        if housekeep {
            let res = tokio::task::spawn_blocking(move || -> Result<(), String> {
                a.keep_only(&live)?;
                a.expire(crate::history::now_ms() as i64)?;
                let gone = a.enforce_space(&|| disk_space(&data))?;
                if gone > 0 {
                    tracing::warn!("access log: purged {gone} oldest rows to keep disk space free");
                }
                Ok(())
            })
            .await;
            if let Ok(Err(e)) = res {
                tracing::warn!("access log: {e}");
            }
        }
    }
}

async fn pull(state: &crate::state::AppState, c: &crate::store::Connection) -> Result<(), String> {
    let base = crate::daemon::parse_url(&c.url)?.base;
    let client = crate::daemon::http_client(c)?;
    loop {
        let after = state.access.cursor(&c.id);
        let req = client
            .get(format!("{base}/flows?after={after}&limit=5000"))
            .header("authorization", format!("Bearer {}", c.token))
            .send();
        let resp = tokio::time::timeout(std::time::Duration::from_secs(10), req)
            .await
            .map_err(|_| "timed out".to_string())?
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("daemon answered {}", resp.status()));
        }
        let b: Batch = serde_json::from_slice(&resp.bytes().await.map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        if b.last < after {
            // The daemon restarted and its sequence began again.
            state.access.reset_cursor(&c.id)?;
            continue;
        }
        let a = state.access.clone();
        let id = c.id.clone();
        let full = b.flows.len() >= 5000;
        let last = b.flows.last().map_or(b.last, |f| f.seq);
        tokio::task::spawn_blocking(move || a.record(&id, &b.flows, last))
            .await
            .map_err(|e| e.to_string())??;
        if !full {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Access {
        let p = std::env::temp_dir().join(format!(
            "access-test-{}-{}.db",
            std::process::id(),
            rand::random::<u32>()
        ));
        Access::open(&p).unwrap()
    }

    // Test fixture: one argument per column is the readable form here.
    #[allow(clippy::too_many_arguments)]
    fn flow(
        seq: u64,
        ct: u32,
        opened: u64,
        closed: u64,
        vm: &str,
        proto: u8,
        dst: &str,
        dport: u16,
        domain: Option<&str>,
        bytes: u64,
    ) -> Flow {
        Flow {
            seq,
            ct_id: ct,
            opened_ms: opened,
            closed_ms: closed,
            network: "internet".into(),
            mac: Some("52:54:00:00:00:01".into()),
            vm: Some(format!("uuid-{vm}")),
            vm_name: Some(vm.into()),
            proto,
            src: "10.0.0.5".into(),
            sport: 40000,
            dst: dst.into(),
            dport,
            domain: domain.map(Into::into),
            bytes_out: bytes / 10,
            bytes_in: bytes - bytes / 10,
            dir: "out".into(),
            packets_out: 1,
            packets_in: 1,
        }
    }

    fn q(conn: &str) -> Query {
        Query {
            conn: conn.into(),
            since_ms: Some(0),
            ..Default::default()
        }
    }

    const NOW: i64 = 1_000_000;

    #[test]
    fn an_open_connection_is_filled_in_by_its_close() {
        let a = db();
        a.record(
            "c1",
            &[flow(
                1,
                7,
                100,
                0,
                "web",
                6,
                "1.1.1.1",
                443,
                Some("one.one.one.one"),
                0,
            )],
            1,
        )
        .unwrap();
        let r = a.query(&q("c1"), NOW).unwrap();
        assert_eq!(r["total"], 1);
        assert_eq!(r["rows"][0]["closed_ms"], 0, "listed while still open");
        a.record(
            "c1",
            &[flow(2, 7, 100, 900, "web", 6, "1.1.1.1", 443, None, 5000)],
            2,
        )
        .unwrap();
        let r = a.query(&q("c1"), NOW).unwrap();
        assert_eq!(r["total"], 1, "the close updates the same row");
        assert_eq!(r["rows"][0]["closed_ms"], 900);
        assert_eq!(r["rows"][0]["bytes_in"], 4500);
        assert_eq!(
            r["rows"][0]["domain"], "one.one.one.one",
            "a name seen at open is kept"
        );
        assert_eq!(a.cursor("c1"), 2);
    }

    fn seed(a: &Access) {
        let f = vec![
            flow(
                1,
                1,
                10,
                20,
                "web",
                6,
                "140.82.112.3",
                443,
                Some("github.com"),
                1000,
            ),
            flow(
                2,
                2,
                11,
                21,
                "web",
                6,
                "140.82.112.3",
                443,
                Some("github.com"),
                1000,
            ),
            flow(
                3,
                3,
                12,
                22,
                "web",
                6,
                "140.82.112.4",
                443,
                Some("github.com"),
                50_000,
            ),
            flow(4, 4, 13, 23, "db", 17, "1.1.1.1", 123, None, 90),
            flow(
                5,
                0,
                14,
                14,
                "db",
                17,
                "140.82.112.3",
                53,
                Some("github.com"),
                0,
            ),
            flow(6, 5, 15, 25, "db", 1, "8.8.8.8", 8, None, 84),
        ];
        a.record("c1", &f, 6).unwrap();
        a.record(
            "c2",
            &[flow(1, 1, 10, 20, "other", 6, "9.9.9.9", 22, None, 1)],
            1,
        )
        .unwrap();
    }

    fn keys(r: &serde_json::Value) -> Vec<(String, i64)> {
        r["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| {
                (
                    g["key"].as_str().unwrap().to_string(),
                    g["count"].as_i64().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn most_accessed_domain_ip_protocol_and_port_in_either_order() {
        let a = db();
        seed(&a);
        let mut g = q("c1");
        g.group = "domain".into();
        assert_eq!(
            keys(&a.query(&g, NOW).unwrap())[0],
            ("github.com".into(), 4),
            "four rows reached github.com"
        );
        g.group = "proto".into();
        assert_eq!(
            keys(&a.query(&g, NOW).unwrap()),
            vec![("tcp".into(), 3), ("udp".into(), 2), ("icmp".into(), 1)]
        );
        g.dir = "asc".into();
        assert_eq!(
            keys(&a.query(&g, NOW).unwrap())[0],
            ("icmp".into(), 1),
            "least accessed first"
        );
        g.group = "ip".into();
        g.dir = "desc".into();
        g.sort = "bytes".into();
        assert_eq!(
            keys(&a.query(&g, NOW).unwrap())[0].0,
            "140.82.112.4",
            "most bytes, not most hits"
        );
        g.group = "port".into();
        g.sort = "count".into();
        assert_eq!(keys(&a.query(&g, NOW).unwrap())[0], ("443".into(), 3));
    }

    #[test]
    fn filters_narrow_by_vm_protocol_port_and_text_and_never_cross_servers() {
        let a = db();
        seed(&a);
        let mut f = q("c1");
        f.vm = "db".into();
        assert_eq!(a.query(&f, NOW).unwrap()["total"], 3);
        f.proto = "udp".into();
        assert_eq!(a.query(&f, NOW).unwrap()["total"], 2);
        f.port = Some(123);
        assert_eq!(a.query(&f, NOW).unwrap()["total"], 1);
        let mut t = q("c1");
        t.q = "github".into();
        assert_eq!(a.query(&t, NOW).unwrap()["total"], 4);
        t.q = "140.82.112".into();
        assert_eq!(a.query(&t, NOW).unwrap()["total"], 4);
        t.q = "100%".into();
        assert_eq!(
            a.query(&t, NOW).unwrap()["total"],
            0,
            "LIKE wildcards in the search are literal"
        );
        let mut n = q("c1");
        n.dns = Some(false);
        assert_eq!(
            a.query(&n, NOW).unwrap()["total"],
            5,
            "lookups can be hidden"
        );
        n.dns = None;
        n.kind = "connections".into();
        assert_eq!(a.query(&n, NOW).unwrap()["total"], 5, "connections only");
        n.kind = "dns".into();
        let d = a.query(&n, NOW).unwrap();
        assert_eq!(d["total"], 1, "lookups only");
        assert_eq!(d["rows"][0]["kind"], "dns");
        n.kind = "both".into();
        assert_eq!(a.query(&n, NOW).unwrap()["total"], 6);
        n.kind = "bogus".into();
        assert!(a.query(&n, NOW).is_err());
        assert_eq!(a.query(&q("c2"), NOW).unwrap()["total"], 1);
        assert_eq!(a.vms("c1").unwrap(), vec!["db", "web"]);
    }

    #[test]
    fn rows_can_be_narrowed_to_one_direction() {
        let a = db();
        let mut inbound = flow(1, 1, 10, 20, "web", 6, "203.0.113.5", 2222, None, 1000);
        inbound.dir = "in".into();
        a.record(
            "c1",
            &[
                flow(2, 2, 11, 21, "web", 6, "1.1.1.1", 443, None, 10),
                inbound,
            ],
            2,
        )
        .unwrap();
        let mut d = q("c1");
        assert_eq!(a.query(&d, NOW).unwrap()["total"], 2, "both by default");
        d.direction = "in".into();
        let r = a.query(&d, NOW).unwrap();
        assert_eq!(r["total"], 1);
        // The remote end is the destination whichever way the connection
        // went, so a search and the ip grouping keep working on an in row.
        assert_eq!(
            (r["rows"][0]["dir"].as_str(), r["rows"][0]["dst"].as_str()),
            (Some("in"), Some("203.0.113.5"))
        );
        d.direction = "out".into();
        assert_eq!(a.query(&d, NOW).unwrap()["total"], 1);
        d.direction = "sideways".into();
        assert!(a.query(&d, NOW).is_err());
    }

    #[test]
    fn a_database_from_before_the_direction_column_still_opens() {
        let p = std::env::temp_dir().join(format!(
            "access-old-{}-{}.db",
            std::process::id(),
            rand::random::<u32>()
        ));
        let old = Connection::open(&p).unwrap();
        old.execute_batch(
            "CREATE TABLE flows(
               id INTEGER PRIMARY KEY, conn TEXT NOT NULL, ct_id INTEGER NOT NULL,
               opened INTEGER NOT NULL, closed INTEGER NOT NULL, network TEXT NOT NULL,
               mac TEXT, vm TEXT, vm_name TEXT, proto INTEGER NOT NULL,
               src TEXT NOT NULL, sport INTEGER NOT NULL, dst TEXT NOT NULL,
               dport INTEGER NOT NULL, domain TEXT,
               bytes_out INTEGER NOT NULL, bytes_in INTEGER NOT NULL);
             INSERT INTO flows(conn, ct_id, opened, closed, network, proto, src, sport,
                               dst, dport, bytes_out, bytes_in)
             VALUES ('c1', 1, 10, 20, 'internet', 6, '10.0.0.5', 40000, '1.1.1.1', 443, 1, 2);",
        )
        .unwrap();
        drop(old);
        let a = Access::open(&p).unwrap();
        let r = a.query(&q("c1"), NOW).unwrap();
        assert_eq!(r["total"], 1);
        assert_eq!(r["rows"][0]["dir"], "out", "everything old is outbound");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn rows_sort_both_ways_and_page() {
        let a = db();
        seed(&a);
        let mut s = q("c1");
        s.sort = "time".into();
        s.dir = "asc".into();
        s.limit = Some(2);
        let r = a.query(&s, NOW).unwrap();
        assert_eq!(r["rows"][0]["opened_ms"], 10);
        assert_eq!(r["total"], 6);
        s.offset = Some(4);
        assert_eq!(a.query(&s, NOW).unwrap()["rows"][0]["opened_ms"], 14);
        s.dir = "desc".into();
        s.offset = None;
        assert_eq!(a.query(&s, NOW).unwrap()["rows"][0]["opened_ms"], 15);
        assert!(a
            .query(
                &Query {
                    group: "bogus".into(),
                    ..q("c1")
                },
                NOW
            )
            .is_err());
    }

    #[test]
    fn old_rows_expire_and_a_removed_server_takes_its_rows() {
        let a = db();
        seed(&a);
        a.record(
            "c1",
            &[flow(
                7,
                9,
                (RETAIN_MS + 500) as u64,
                0,
                "web",
                6,
                "1.2.3.4",
                80,
                None,
                0,
            )],
            7,
        )
        .unwrap();
        assert_eq!(
            a.expire(RETAIN_MS + 100).unwrap(),
            7,
            "everything opened before the window, on both servers"
        );
        assert_eq!(
            a.query(
                &Query {
                    since_ms: Some(0),
                    ..q("c1")
                },
                NOW
            )
            .unwrap()["total"],
            1
        );
        a.keep_only(&["c1".into()]).unwrap();
        assert_eq!(a.query(&q("c2"), NOW).unwrap()["total"], 0);
        assert_eq!(a.cursor("c2"), 0);
    }

    #[test]
    fn low_disk_purges_the_oldest_rows_first() {
        let a = db();
        seed(&a);
        let calls = std::cell::Cell::new(0);
        // Low on space for the first two checks, then fine.
        let free = || {
            calls.set(calls.get() + 1);
            Some(if calls.get() <= 2 {
                (1, 1_000_000_000_000)
            } else {
                (u64::MAX / 2, u64::MAX)
            })
        };
        let gone = a.enforce_space(&free).unwrap();
        assert!(gone >= 2, "{gone}");
        let mut s = q("c1");
        s.sort = "time".into();
        s.dir = "asc".into();
        let oldest = a.query(&s, NOW).unwrap()["rows"][0]["opened_ms"]
            .as_i64()
            .unwrap();
        assert!(oldest > 10, "the oldest went first ({oldest})");
        assert!(
            a.enforce_space(&|| Some((u64::MAX / 2, u64::MAX))).unwrap() == 0,
            "a healthy disk purges nothing"
        );
    }

    #[test]
    fn the_space_threshold_scales_with_the_disk() {
        assert!(low_on_space(4 << 30, 100 << 30), "under 5 GiB free");
        assert!(!low_on_space(6 << 30, 100 << 30));
        assert!(low_on_space(40 << 30, 1000 << 30), "under 5% of a big disk");
        assert!(!low_on_space(60 << 30, 1000 << 30));
    }
}
