//! Metric history: sampled server-side, kept for two weeks, deleted with its
//! subject.
//!
//! The SPA's charts used to be the only record: sixty seconds held in page
//! memory, lost on reload, and nothing at all for a view nobody had open. A
//! collector here samples every connection's `GET /metrics` every
//! [`SAMPLE_MS`] whether or not a browser is open, and the charts read ranges
//! back through `GET /history`.
//!
//! Rows are keyed by libvirt UUID, not name, so a renamed guest keeps its
//! history and a new guest reusing a deleted one's name starts empty. Deletion
//! is by absence: anything a reachable daemon no longer reports (guest, NIC,
//! disk) loses its history on that tick, and a removed connection loses
//! everything — see [`History::keep_only`]. An *unreachable* daemon reports
//! nothing, so it deletes nothing.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;

pub const SAMPLE_MS: u64 = 10_000;
pub const RETAIN_MS: i64 = 14 * 24 * 3600 * 1000;

// ------------------------------------------------------------ daemon payload

#[derive(Debug, Deserialize)]
pub struct Metrics {
    pub host: HostM,
    #[serde(default)]
    pub pools: Vec<PoolM>,
    #[serde(default)]
    pub domains: Vec<DomM>,
}

#[derive(Debug, Deserialize)]
pub struct HostM {
    pub cpu_total: u64,
    pub cpu_idle: u64,
    pub mem_total_kib: u64,
    pub mem_available_kib: u64,
}

#[derive(Debug, Deserialize)]
pub struct PoolM {
    pub capacity: Option<u64>,
    pub allocation: Option<u64>,
    #[serde(default)]
    pub available: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct DomM {
    pub uuid: String,
    pub name: String,
    pub state: String,
    pub cpu_time_ns: u64,
    pub vcpus: u32,
    pub memory_kib: u64,
    pub max_memory_kib: u64,
    pub rss_kib: Option<u64>,
    pub usable_kib: Option<u64>,
    #[serde(default)]
    pub nics: Vec<NicM>,
    #[serde(default)]
    pub disks: Vec<DiskM>,
}

#[derive(Debug, Deserialize)]
pub struct NicM {
    pub mac: String,
    pub rx_bytes: Option<u64>,
    pub tx_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct DiskM {
    pub dev: String,
    pub capacity: Option<u64>,
    pub allocation: Option<u64>,
}

// ------------------------------------------------------------ folding

/// One sampled series value. `kind` is host|vm|nic|disk|dc|dcdisk;
/// `owner` is the guest UUID, `sub` a NIC MAC or disk target dev.
/// `a`/`b` are cpu/mem %, rx/tx KiB/s, or fill % (b unused).
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub kind: &'static str,
    pub owner: String,
    pub sub: String,
    pub name: String,
    pub a: Option<f64>,
    pub b: Option<f64>,
}

/// Previous raw counters for one connection, to turn counters into rates.
#[derive(Debug, Default, Clone)]
pub struct Counters {
    at_ms: u64,
    host: (u64, u64),
    cpu: HashMap<String, u64>,
    nic: HashMap<(String, String), (u64, u64)>,
}

/// Host totals of one tick, summed across connections for the datacenter.
#[derive(Debug, Default, Clone, Copy)]
pub struct Totals {
    d_total: u64,
    d_idle: u64,
    mem_total: u64,
    mem_avail: u64,
    disk_cap: u64,
    disk_used: u64,
}

fn clamp_pct(n: f64) -> f64 {
    if n.is_finite() {
        n.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

fn fill_pct(alloc: Option<u64>, cap: Option<u64>) -> Option<f64> {
    match (alloc, cap) {
        (Some(a), Some(c)) if c > 0 => Some(clamp_pct(a as f64 / c as f64 * 100.0)),
        _ => None,
    }
}

/// Guest memory in use as a % of its maximum — the same reading as the SPA's
/// live chart (`memPercent` in stats.ts), so a range and the live view agree.
fn mem_pct(d: &DomM) -> f64 {
    let max = if d.max_memory_kib > 0 {
        d.max_memory_kib
    } else {
        d.memory_kib
    };
    if max == 0 {
        return 0.0;
    }
    let used = match (d.usable_kib, d.rss_kib) {
        (Some(u), _) => d.memory_kib.saturating_sub(u),
        (None, Some(r)) => r,
        _ => d.memory_kib,
    };
    clamp_pct(used as f64 / max as f64 * 100.0)
}

/// Turn one metrics answer into points. Counter-based values (CPU, NIC rates)
/// need the previous sample and are `None` on the first; a counter that went
/// backwards (guest restarted) reads as idle, as it does live.
pub fn fold(m: &Metrics, now_ms: u64, prev: Option<&Counters>) -> (Vec<Point>, Counters, Totals) {
    let mut out = Vec::new();
    let mut next = Counters {
        at_ms: now_ms,
        host: (m.host.cpu_total, m.host.cpu_idle),
        ..Default::default()
    };
    let dt_ms = prev.map(|p| now_ms.saturating_sub(p.at_ms)).unwrap_or(0);
    let mut totals = Totals {
        mem_total: m.host.mem_total_kib,
        mem_avail: m.host.mem_available_kib,
        ..Default::default()
    };

    let host_cpu = prev.and_then(|p| {
        let dt = m.host.cpu_total.checked_sub(p.host.0)?;
        let di = m.host.cpu_idle.checked_sub(p.host.1)?;
        if dt == 0 {
            return None;
        }
        totals.d_total = dt;
        totals.d_idle = di;
        Some(clamp_pct(100.0 * (1.0 - di as f64 / dt as f64)))
    });
    let host_mem = (m.host.mem_total_kib > 0).then(|| {
        clamp_pct(100.0 * (1.0 - m.host.mem_available_kib as f64 / m.host.mem_total_kib as f64))
    });
    out.push(point("host", "", "", "", host_cpu, host_mem));

    for p in &m.pools {
        totals.disk_cap += p.capacity.unwrap_or(0);
        // The filesystem's used bytes, the same scope as its total. A pool's
        // `allocation` counts only the files in its root, which understates a
        // directory pool sharing a disk with the rest of the host.
        totals.disk_used += match (p.capacity, p.available) {
            (Some(c), Some(a)) => c.saturating_sub(a),
            _ => p.allocation.unwrap_or(0),
        };
    }

    for d in &m.domains {
        let running = d.state == "running";
        next.cpu.insert(d.uuid.clone(), d.cpu_time_ns);
        let cpu = if !running {
            Some(0.0)
        } else {
            prev.and_then(|p| p.cpu.get(&d.uuid)).and_then(|&before| {
                if dt_ms == 0 {
                    return None;
                }
                let dns = d.cpu_time_ns.saturating_sub(before);
                let vcpus = d.vcpus.max(1) as f64;
                Some(clamp_pct(dns as f64 / 1e6 / dt_ms as f64 / vcpus * 100.0))
            })
        };
        let mem = if running { mem_pct(d) } else { 0.0 };
        out.push(point("vm", &d.uuid, "", &d.name, cpu, Some(mem)));

        for n in &d.nics {
            let key = (d.uuid.clone(), n.mac.clone());
            let (rx, tx) = match (n.rx_bytes, n.tx_bytes) {
                (Some(rx), Some(tx)) => {
                    next.nic.insert(key.clone(), (rx, tx));
                    let rate = |now: u64, before: u64| {
                        (now.saturating_sub(before) as f64 / 1024.0) / (dt_ms as f64 / 1000.0)
                    };
                    match prev.and_then(|p| p.nic.get(&key)) {
                        Some(&(prx, ptx)) if dt_ms > 0 => {
                            (Some(rate(rx, prx)), Some(rate(tx, ptx)))
                        }
                        _ => (None, None),
                    }
                }
                // No counters (guest off): no traffic, and nothing to diff next time.
                _ => (Some(0.0), Some(0.0)),
            };
            out.push(point("nic", &d.uuid, &n.mac, &d.name, rx, tx));
        }
        for k in &d.disks {
            out.push(point(
                "disk",
                &d.uuid,
                &k.dev,
                &d.name,
                fill_pct(k.allocation, k.capacity),
                None,
            ));
        }
    }
    (out, next, totals)
}

fn point(
    kind: &'static str,
    owner: &str,
    sub: &str,
    name: &str,
    a: Option<f64>,
    b: Option<f64>,
) -> Point {
    Point {
        kind,
        owner: owner.into(),
        sub: sub.into(),
        name: name.into(),
        a,
        b,
    }
}

/// Datacenter points from every reachable connection's totals: CPU is summed
/// busy over summed total jiffies, a true cluster figure rather than a mean
/// of per-host percentages (same as the live datacenter dashboard).
pub fn datacenter(all: &[Totals]) -> Vec<Point> {
    let s = all.iter().fold(Totals::default(), |a, t| Totals {
        d_total: a.d_total + t.d_total,
        d_idle: a.d_idle + t.d_idle,
        mem_total: a.mem_total + t.mem_total,
        mem_avail: a.mem_avail + t.mem_avail,
        disk_cap: a.disk_cap + t.disk_cap,
        disk_used: a.disk_used + t.disk_used,
    });
    let cpu =
        (s.d_total > 0).then(|| clamp_pct(100.0 * (1.0 - s.d_idle as f64 / s.d_total as f64)));
    let mem = (s.mem_total > 0)
        .then(|| clamp_pct(100.0 * (1.0 - s.mem_avail as f64 / s.mem_total as f64)));
    vec![
        point("dc", "", "", "", cpu, mem),
        point(
            "dcdisk",
            "",
            "",
            "",
            fill_pct(Some(s.disk_used), Some(s.disk_cap)),
            None,
        ),
    ]
}

// ------------------------------------------------------------ storage

#[derive(Clone)]
pub struct History(Arc<Mutex<Connection>>);

/// What a chart asks for. `name` is the guest name as the UI knows
/// it; `sub` a NIC MAC or disk dev.
#[derive(Debug, Deserialize)]
pub struct Query {
    #[serde(default)]
    pub conn: String,
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub sub: String,
    pub since_ms: i64,
    #[serde(default = "default_points")]
    pub points: i64,
}

fn default_points() -> i64 {
    240
}

pub type Row = (i64, Option<f64>, Option<f64>);

impl History {
    pub fn open(path: &Path) -> Result<Self, String> {
        let db = Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS targets(
               id INTEGER PRIMARY KEY,
               conn TEXT NOT NULL, kind TEXT NOT NULL,
               owner TEXT NOT NULL, sub TEXT NOT NULL, name TEXT NOT NULL,
               UNIQUE(conn, kind, owner, sub));
             CREATE TABLE IF NOT EXISTS samples(
               tid INTEGER NOT NULL REFERENCES targets(id) ON DELETE CASCADE,
               ts INTEGER NOT NULL, a REAL, b REAL,
               PRIMARY KEY(tid, ts)) WITHOUT ROWID;",
        )
        .map_err(|e| format!("init history db: {e}"))?;
        Ok(Self(Arc::new(Mutex::new(db))))
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().expect("history lock poisoned")
    }

    /// Store one tick of a connection's points, then drop every subject of
    /// that connection the tick did not mention: that guest, NIC or disk no
    /// longer exists. Pass `prune = false` for points that do not
    /// describe a whole connection (the datacenter aggregate).
    pub fn record(&self, conn: &str, ts: i64, points: &[Point], prune: bool) -> Result<(), String> {
        let mut db = self.db();
        let tx = db.transaction().map_err(|e| e.to_string())?;
        let mut seen = HashSet::new();
        for p in points {
            tx.execute(
                "INSERT INTO targets(conn, kind, owner, sub, name) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(conn, kind, owner, sub) DO UPDATE SET name = excluded.name",
                params![conn, p.kind, p.owner, p.sub, p.name],
            )
            .map_err(|e| e.to_string())?;
            let id: i64 = tx
                .query_row(
                    "SELECT id FROM targets WHERE conn=?1 AND kind=?2 AND owner=?3 AND sub=?4",
                    params![conn, p.kind, p.owner, p.sub],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            seen.insert(id);
            if p.a.is_some() || p.b.is_some() {
                tx.execute(
                    "INSERT OR REPLACE INTO samples(tid, ts, a, b) VALUES (?1, ?2, ?3, ?4)",
                    params![id, ts, p.a, p.b],
                )
                .map_err(|e| e.to_string())?;
            }
        }
        if prune {
            let ids: Vec<i64> = {
                let mut st = tx
                    .prepare("SELECT id FROM targets WHERE conn=?1")
                    .map_err(|e| e.to_string())?;
                let rows = st
                    .query_map([conn], |r| r.get(0))
                    .map_err(|e| e.to_string())?;
                rows.filter_map(Result::ok).collect()
            };
            for id in ids.into_iter().filter(|id| !seen.contains(id)) {
                tx.execute("DELETE FROM targets WHERE id=?1", [id])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// Drop the history of every connection not in `live` (a removed server).
    /// The datacenter rows (conn "") are kept.
    pub fn keep_only(&self, live: &[String]) -> Result<usize, String> {
        let db = self.db();
        let conns: Vec<String> = {
            let mut st = db
                .prepare("SELECT DISTINCT conn FROM targets WHERE conn <> ''")
                .map_err(|e| e.to_string())?;
            let rows = st.query_map([], |r| r.get(0)).map_err(|e| e.to_string())?;
            rows.filter_map(Result::ok).collect()
        };
        let mut n = 0;
        for c in conns.iter().filter(|c| !live.contains(c)) {
            n += db
                .execute("DELETE FROM targets WHERE conn=?1", [c])
                .map_err(|e| e.to_string())?;
        }
        Ok(n)
    }

    /// Drop samples older than the retention window.
    // ponytail: full scan of samples (the key leads with the target id); ~1M rows
    // at two weeks, run hourly. Add an index on ts if it ever shows up.
    pub fn expire(&self, now_ms: i64) -> Result<usize, String> {
        self.db()
            .execute("DELETE FROM samples WHERE ts < ?1", [now_ms - RETAIN_MS])
            .map_err(|e| e.to_string())
    }

    /// Averaged buckets for one series since `since_ms`, oldest first, and the
    /// bucket width. Buckets with no samples are absent, not zero.
    pub fn query(&self, q: &Query, now_ms: i64) -> Result<(Vec<Row>, i64), String> {
        let since = q.since_ms.max(now_ms - RETAIN_MS);
        let points = q.points.clamp(10, 2000);
        let step = ((now_ms - since) / points).max(SAMPLE_MS as i64);
        let db = self.db();
        let conn = if matches!(q.kind.as_str(), "dc" | "dcdisk") {
            ""
        } else {
            q.conn.as_str()
        };
        let tid: Option<i64> = match q.kind.as_str() {
            "host" | "dc" | "dcdisk" => db
                .query_row(
                    "SELECT id FROM targets WHERE conn=?1 AND kind=?2 AND owner='' AND sub=''",
                    params![conn, q.kind],
                    |r| r.get(0),
                )
                .optional(),
            "vm" => db
                .query_row(
                    "SELECT id FROM targets WHERE conn=?1 AND kind=?2 AND name=?3",
                    params![conn, q.kind, q.name],
                    |r| r.get(0),
                )
                .optional(),
            "nic" | "disk" => db
                .query_row(
                    "SELECT t.id FROM targets t JOIN targets v
                       ON v.conn = t.conn AND v.kind = 'vm' AND v.owner = t.owner
                     WHERE t.conn=?1 AND t.kind=?2 AND v.name=?3 AND t.sub=?4",
                    params![conn, q.kind, q.name, q.sub],
                    |r| r.get(0),
                )
                .optional(),
            other => return Err(format!("unknown series kind '{other}'")),
        }
        .map_err(|e| e.to_string())?;
        let Some(tid) = tid else {
            return Ok((Vec::new(), step));
        };
        let mut st = db
            .prepare(
                "SELECT (ts / ?2) * ?2 + ?2 / 2 AS b, avg(a), avg(b) FROM samples
                 WHERE tid = ?1 AND ts >= ?3 GROUP BY ts / ?2 ORDER BY b",
            )
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map(params![tid, step, since], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(|e| e.to_string())?;
        Ok((rows.filter_map(Result::ok).collect(), step))
    }
}

// ------------------------------------------------------------ collector

/// Sample every connection forever. Runs whether or not anyone is watching,
/// which is the point: a range chart has to show the hours nobody had open.
pub async fn collect(state: crate::state::AppState) {
    let mut prev: HashMap<String, Counters> = HashMap::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(SAMPLE_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n: u64 = 0;
    loop {
        tick.tick().await;
        let conns = state.read().connections.clone();
        let fetched = futures_util::future::join_all(conns.iter().map(fetch)).await;
        let now = now_ms();
        let mut totals = Vec::new();
        let mut jobs = Vec::new();
        for (c, m) in conns.iter().zip(fetched) {
            let Some(m) = m else {
                // Unreachable: keep its history and forget the counters, so
                // the first sample back is not diffed across the outage.
                prev.remove(&c.id);
                continue;
            };
            let (points, next, t) = fold(&m, now, prev.get(&c.id));
            prev.insert(c.id.clone(), next);
            totals.push(t);
            jobs.push((c.id.clone(), points, true));
        }
        if !totals.is_empty() {
            jobs.push((String::new(), datacenter(&totals), false));
        }
        let live: Vec<String> = conns.iter().map(|c| c.id.clone()).collect();
        prev.retain(|id, _| live.contains(id));
        let h = state.history.clone();
        let hourly = n.is_multiple_of(3_600_000 / SAMPLE_MS);
        n += 1;
        let res = tokio::task::spawn_blocking(move || -> Result<(), String> {
            for (conn, points, prune) in &jobs {
                h.record(conn, now as i64, points, *prune)?;
            }
            h.keep_only(&live)?;
            if hourly {
                h.expire(now as i64)?;
            }
            Ok(())
        })
        .await;
        match res {
            Ok(Err(e)) => tracing::warn!("history: {e}"),
            Err(e) => tracing::warn!("history task: {e}"),
            Ok(Ok(())) => {}
        }
    }
}

async fn fetch(c: &crate::store::Connection) -> Option<Metrics> {
    let base = crate::daemon::parse_url(&c.url).ok()?.base;
    let client = crate::daemon::http_client(c).ok()?;
    let req = client
        .get(format!("{base}/metrics"))
        .header("authorization", format!("Bearer {}", c.token))
        .send();
    // A hung daemon must not stall every other connection's samples.
    let resp = tokio::time::timeout(std::time::Duration::from_secs(5), req)
        .await
        .ok()?
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    serde_json::from_slice(&resp.bytes().await.ok()?).ok()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dom(uuid: &str, name: &str, cpu_ns: u64, nics: Vec<NicM>, disks: Vec<DiskM>) -> DomM {
        DomM {
            uuid: uuid.into(),
            name: name.into(),
            state: "running".into(),
            cpu_time_ns: cpu_ns,
            vcpus: 2,
            memory_kib: 1000,
            max_memory_kib: 1000,
            rss_kib: None,
            usable_kib: Some(750),
            nics,
            disks,
        }
    }

    fn metrics(total: u64, idle: u64, domains: Vec<DomM>) -> Metrics {
        Metrics {
            host: HostM {
                cpu_total: total,
                cpu_idle: idle,
                mem_total_kib: 100,
                mem_available_kib: 40,
            },
            pools: vec![PoolM {
                capacity: Some(200),
                allocation: Some(50),
                available: Some(150),
            }],
            domains,
        }
    }

    fn nic(rx: u64, tx: u64) -> NicM {
        NicM {
            mac: "52:54:00:00:00:01".into(),
            rx_bytes: Some(rx),
            tx_bytes: Some(tx),
        }
    }

    fn find<'a>(p: &'a [Point], kind: &str) -> &'a Point {
        p.iter().find(|x| x.kind == kind).expect(kind)
    }

    #[test]
    fn counters_become_rates_on_the_second_sample() {
        let m1 = metrics(1000, 800, vec![dom("u1", "vm", 0, vec![nic(0, 0)], vec![])]);
        let (p1, c1, _) = fold(&m1, 10_000, None);
        assert_eq!(find(&p1, "host").a, None, "no host CPU from one sample");
        assert_eq!(find(&p1, "host").b, Some(60.0), "memory needs no history");
        assert_eq!(find(&p1, "vm").a, None);
        assert_eq!(
            find(&p1, "vm").b,
            Some(25.0),
            "usable balloon memory, as live"
        );

        // 10 s later: 1 s of CPU on 2 vCPUs = 5 %; host busy 50 of 100 jiffies;
        // 102400 bytes in 10 s = 10 KiB/s.
        let m2 = metrics(
            1100,
            850,
            vec![dom(
                "u1",
                "vm",
                1_000_000_000,
                vec![nic(102_400, 204_800)],
                vec![],
            )],
        );
        let (p2, _, t) = fold(&m2, 20_000, Some(&c1));
        assert_eq!(find(&p2, "host").a, Some(50.0));
        assert_eq!(find(&p2, "vm").a, Some(5.0));
        let n = find(&p2, "nic");
        assert_eq!((n.a, n.b), (Some(10.0), Some(20.0)));
        assert_eq!((t.d_total, t.d_idle), (100, 50));
    }

    #[test]
    fn a_restarted_guest_reads_as_idle_not_as_a_spike() {
        let (_, c1, _) = fold(
            &metrics(
                0,
                0,
                vec![dom(
                    "u1",
                    "vm",
                    9_000_000_000,
                    vec![nic(5000, 5000)],
                    vec![],
                )],
            ),
            0,
            None,
        );
        let (p2, _, _) = fold(
            &metrics(
                0,
                0,
                vec![dom("u1", "vm", 1_000, vec![nic(10, 10)], vec![])],
            ),
            10_000,
            Some(&c1),
        );
        assert_eq!(find(&p2, "vm").a, Some(0.0));
        assert_eq!(
            (find(&p2, "nic").a, find(&p2, "nic").b),
            (Some(0.0), Some(0.0))
        );
    }

    #[test]
    fn the_datacenter_weights_hosts_by_size() {
        // A 2-core host at 100 % and a 6-core host idle: 25 %, not the 50 % a
        // mean of percentages would claim.
        let big = Totals {
            d_total: 600,
            d_idle: 600,
            mem_total: 300,
            mem_avail: 300,
            ..Default::default()
        };
        let small = Totals {
            d_total: 200,
            d_idle: 0,
            mem_total: 100,
            mem_avail: 0,
            ..Default::default()
        };
        let p = datacenter(&[big, small]);
        assert_eq!(find(&p, "dc").a, Some(25.0));
        assert_eq!(find(&p, "dc").b, Some(25.0));
    }

    #[test]
    fn the_datacenter_disk_figure_is_filesystem_usage() {
        // The disk total is the filesystem's, so the used share must be too.
        // A pool whose own files hold 5 of 1000 bytes on a disk with 760 free
        // is 24 % used, not 0.5 %.
        let mut m = metrics(0, 0, vec![]);
        m.pools = vec![PoolM {
            capacity: Some(1000),
            allocation: Some(5),
            available: Some(760),
        }];
        let (_, _, totals) = fold(&m, 0, None);
        assert_eq!(find(&datacenter(&[totals]), "dcdisk").a, Some(24.0));

        // An older daemon without `available` falls back to the pool's files.
        m.pools[0].available = None;
        let (_, _, totals) = fold(&m, 0, None);
        assert_eq!(find(&datacenter(&[totals]), "dcdisk").a, Some(0.5));
    }

    fn db() -> History {
        History::open(Path::new(":memory:")).unwrap()
    }

    fn count(h: &History, sql: &str) -> i64 {
        h.db().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn q(conn: &str, kind: &str, name: &str, sub: &str) -> Query {
        Query {
            conn: conn.into(),
            kind: kind.into(),
            name: name.into(),
            sub: sub.into(),
            since_ms: 0,
            points: 240,
        }
    }

    #[test]
    fn a_deleted_guest_loses_its_history_and_its_devices() {
        let h = db();
        let two = vec![
            dom(
                "u1",
                "keep",
                0,
                vec![nic(0, 0)],
                vec![DiskM {
                    dev: "vda".into(),
                    capacity: Some(10),
                    allocation: Some(5),
                }],
            ),
            dom(
                "u2",
                "gone",
                0,
                vec![nic(0, 0)],
                vec![DiskM {
                    dev: "vda".into(),
                    capacity: Some(10),
                    allocation: Some(5),
                }],
            ),
        ];
        let (p, _, _) = fold(&metrics(0, 0, two), 0, None);
        h.record("c1", 1_000, &p, true).unwrap();
        let now = 1_000 + RETAIN_MS / 2;
        assert_eq!(
            h.query(&q("c1", "disk", "gone", "vda"), now)
                .unwrap()
                .0
                .len(),
            1
        );

        let one = vec![dom("u1", "keep", 0, vec![nic(0, 0)], vec![])];
        let (p, _, _) = fold(&metrics(0, 0, one), 10_000, None);
        h.record("c1", 11_000, &p, true).unwrap();
        assert!(
            h.query(&q("c1", "vm", "gone", ""), now)
                .unwrap()
                .0
                .is_empty(),
            "guest gone"
        );
        assert!(
            h.query(&q("c1", "disk", "gone", "vda"), now)
                .unwrap()
                .0
                .is_empty(),
            "its disk too"
        );
        assert!(
            h.query(&q("c1", "disk", "keep", "vda"), now)
                .unwrap()
                .0
                .is_empty(),
            "a detached disk goes too"
        );
        assert_eq!(
            h.query(&q("c1", "vm", "keep", ""), now).unwrap().0.len(),
            1,
            "the other guest stays"
        );
        assert_eq!(
            count(&h, "SELECT count(*) FROM targets WHERE owner='u2'"),
            0
        );
        assert_eq!(
            count(
                &h,
                "SELECT count(*) FROM samples WHERE tid NOT IN (SELECT id FROM targets)"
            ),
            0,
            "samples cascade with their target"
        );
    }

    #[test]
    fn a_renamed_guest_keeps_its_history() {
        let h = db();
        let (p, _, _) = fold(
            &metrics(0, 0, vec![dom("u1", "old", 0, vec![], vec![])]),
            0,
            None,
        );
        h.record("c1", 1_000, &p, true).unwrap();
        let (p, _, _) = fold(
            &metrics(0, 0, vec![dom("u1", "new", 0, vec![], vec![])]),
            0,
            None,
        );
        h.record("c1", 21_000, &p, true).unwrap();
        let now = 30_000;
        assert_eq!(h.query(&q("c1", "vm", "new", ""), now).unwrap().0.len(), 2);
        assert!(h
            .query(&q("c1", "vm", "old", ""), now)
            .unwrap()
            .0
            .is_empty());
    }

    #[test]
    fn a_removed_connection_loses_everything_but_the_datacenter_stays() {
        let h = db();
        let (p, _, t) = fold(
            &metrics(0, 0, vec![dom("u1", "vm", 0, vec![], vec![])]),
            0,
            None,
        );
        h.record("c1", 1_000, &p, true).unwrap();
        h.record("c2", 1_000, &p, true).unwrap();
        h.record("", 1_000, &datacenter(&[t]), false).unwrap();
        assert!(h.keep_only(&["c2".into()]).unwrap() > 0);
        assert_eq!(count(&h, "SELECT count(*) FROM targets WHERE conn='c1'"), 0);
        assert!(count(&h, "SELECT count(*) FROM targets WHERE conn='c2'") > 0);
        assert_eq!(count(&h, "SELECT count(*) FROM targets WHERE conn=''"), 2);
    }

    #[test]
    fn a_dc_tick_does_not_prune_and_queries_bucket_and_expire() {
        let h = db();
        for i in 0..30 {
            let p = vec![point("host", "", "", "", Some(i as f64), None)];
            h.record("c1", i * 10_000, &p, true).unwrap();
        }
        let now = 300_000;
        let mut req = q("c1", "host", "", "");
        req.points = 10; // 300 s over 10 points = 30 s buckets = 3 samples each
        let (rows, step) = h.query(&req, now).unwrap();
        assert_eq!(step, 30_000);
        assert_eq!(rows.len(), 10);
        assert_eq!(
            rows[0],
            (15_000, Some(1.0), None),
            "avg of 0,1,2 at the bucket middle"
        );
        assert_eq!(
            h.expire(RETAIN_MS + 100_000).unwrap(),
            10,
            "samples before the window go"
        );
    }

    #[test]
    fn an_unknown_series_is_refused_and_a_missing_one_is_empty() {
        let h = db();
        assert!(h.query(&q("c1", "bogus", "", ""), 0).is_err());
        assert!(h.query(&q("c1", "vm", "nope", ""), 0).unwrap().0.is_empty());
    }
}
