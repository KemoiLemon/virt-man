//! Daemon reachability, tracked server-side.
//!
//! Every connection's daemon is probed every [`PROBE_MS`]. The result is kept
//! per connection and served with `GET /connections` (and `GET /health/daemons`
//! for a cheap poll), so every browser sees the same answer, and a page that
//! was closed during an outage still learns about it on its next load.
//!
//! A daemon is declared **down after [`DOWN_AFTER`] failed probes in a row**,
//! and up again on the first success. One lost probe is a hiccup, not an
//! outage; a restart of the daemon (a few hundred ms) must not flap the UI or
//! the activity log. Transitions are written to the activity log as the
//! system, visible to admins.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::Serialize;

pub const PROBE_MS: u64 = 3_000;
pub const DOWN_AFTER: u32 = 2;
const PROBE_TIMEOUT_MS: u64 = 4_000;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DaemonHealth {
    pub online: bool,
    /// Since when it has been in its current state (ms since epoch).
    pub since_ms: u64,
    /// Why the last probe failed, while offline.
    pub error: Option<String>,
    #[serde(skip)]
    failures: u32,
}

pub type HealthMap = Arc<RwLock<HashMap<String, DaemonHealth>>>;

/// What a probe result means for a connection's state. Returns the new state
/// and whether it is a transition worth logging (`Some(true)` = came back,
/// `Some(false)` = went down).
pub fn step(
    prev: Option<&DaemonHealth>,
    ok: Result<(), String>,
    now: u64,
) -> (DaemonHealth, Option<bool>) {
    match (prev, ok) {
        // First probe ever: take it at face value, log nothing (a restart of
        // the web server is not a daemon event).
        (None, Ok(())) => (
            DaemonHealth {
                online: true,
                since_ms: now,
                error: None,
                failures: 0,
            },
            None,
        ),
        (None, Err(e)) => (
            DaemonHealth {
                online: false,
                since_ms: now,
                error: Some(e),
                failures: DOWN_AFTER,
            },
            None,
        ),
        (Some(p), Ok(())) => {
            let back = !p.online;
            (
                DaemonHealth {
                    online: true,
                    since_ms: if back { now } else { p.since_ms },
                    error: None,
                    failures: 0,
                },
                back.then_some(true),
            )
        }
        (Some(p), Err(e)) => {
            let failures = p.failures.saturating_add(1);
            let down = p.online && failures >= DOWN_AFTER;
            (
                DaemonHealth {
                    online: p.online && !down,
                    since_ms: if down { now } else { p.since_ms },
                    error: Some(e),
                    failures,
                },
                down.then_some(false),
            )
        }
    }
}

/// A connect/timeout error, shortened to the part a person can act on.
pub fn describe(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "no answer (timed out)".into();
    }
    let mut chain = e.to_string();
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(s) = src {
        chain = s.to_string();
        src = s.source();
    }
    let lower = chain.to_lowercase();
    if lower.contains("refused") {
        "connection refused (daemon not running?)".into()
    } else if lower.contains("certificate") || lower.contains("unknownissuer") {
        "certificate does not match the pinned one".into()
    } else if lower.contains("dns")
        || lower.contains("resolve")
        || lower.contains("name or service")
    {
        "host name does not resolve".into()
    } else if lower.contains("unreachable") || lower.contains("no route") {
        "host unreachable".into()
    } else {
        chain
    }
}

async fn probe(c: &crate::store::Connection) -> Result<(), String> {
    let base = crate::daemon::parse_url(&c.url)?.base;
    let client = crate::daemon::http_client(c)?;
    let req = client
        .get(format!("{base}/health"))
        .header("authorization", format!("Bearer {}", c.token))
        .send();
    match tokio::time::timeout(std::time::Duration::from_millis(PROBE_TIMEOUT_MS), req).await {
        Err(_) => Err("no answer (timed out)".into()),
        Ok(Err(e)) => Err(describe(&e)),
        Ok(Ok(r)) if r.status() == reqwest::StatusCode::UNAUTHORIZED => {
            Err("the daemon rejected the token".into())
        }
        Ok(Ok(r)) if !r.status().is_success() => Err(format!("daemon answered {}", r.status())),
        Ok(Ok(_)) => Ok(()),
    }
}

/// Probe every connection forever, logging up/down transitions.
pub async fn watch(state: crate::state::AppState) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(PROBE_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let conns = state.read().connections.clone();
        let results = futures_util::future::join_all(conns.iter().map(probe)).await;
        let now = crate::history::now_ms();
        let mut events = Vec::new();
        {
            let mut map = state.health.write().expect("health lock poisoned");
            map.retain(|id, _| conns.iter().any(|c| &c.id == id));
            for (c, r) in conns.iter().zip(results) {
                let (next, change) = step(map.get(&c.id), r, now);
                if let Some(up) = change {
                    events.push((c.name.clone(), up, next.error.clone()));
                }
                map.insert(c.id.clone(), next);
            }
        }
        for (name, up, err) in events {
            let (label, status, msg) = if up {
                (
                    format!("Server {name} is back online"),
                    "done",
                    String::new(),
                )
            } else {
                (
                    format!("Server {name} went offline"),
                    "failed",
                    err.unwrap_or_default(),
                )
            };
            tracing::warn!("{label} {msg}");
            if let Err(e) =
                crate::api::activity::append(&state, "system", "system", &label, status, &msg)
            {
                tracing::warn!("activity: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(seq: &[bool]) -> Vec<(bool, Option<bool>)> {
        let mut prev: Option<DaemonHealth> = None;
        let mut out = Vec::new();
        for (i, ok) in seq.iter().enumerate() {
            let r = if *ok { Ok(()) } else { Err("refused".into()) };
            let (next, change) = step(prev.as_ref(), r, i as u64);
            out.push((next.online, change));
            prev = Some(next);
        }
        out
    }

    #[test]
    fn one_lost_probe_is_not_an_outage() {
        let r = run(&[true, false, true, true]);
        assert!(
            r.iter().all(|(online, change)| *online && change.is_none()),
            "{r:?}"
        );
    }

    #[test]
    fn two_in_a_row_is_an_outage_and_it_is_logged_once() {
        let r = run(&[true, false, false, false, false]);
        assert_eq!(r[2], (false, Some(false)), "down on the second failure");
        assert_eq!(r[3], (false, None), "and logged once, not every probe");
        assert_eq!(r[4], (false, None));
    }

    #[test]
    fn the_first_success_after_an_outage_brings_it_back_and_is_logged() {
        let r = run(&[true, false, false, true, true]);
        assert_eq!(r[3], (true, Some(true)));
        assert_eq!(r[4], (true, None));
    }

    #[test]
    fn the_first_probe_after_a_web_server_start_logs_nothing() {
        assert_eq!(
            run(&[false])[0],
            (false, None),
            "down from the start, not a transition"
        );
        assert_eq!(run(&[true])[0], (true, None));
        // …but coming up from that is a real transition.
        assert_eq!(run(&[false, true])[1], (true, Some(true)));
    }

    #[test]
    fn since_marks_the_transition_not_the_last_probe() {
        let mut prev = None;
        for (i, ok) in [true, true, false, false, false].iter().enumerate() {
            let r = if *ok { Ok(()) } else { Err("x".into()) };
            prev = Some(step(prev.as_ref(), r, i as u64 * 1000).0);
        }
        let h = prev.unwrap();
        assert_eq!(h.since_ms, 3000, "offline since the probe that tipped it");
        assert_eq!(h.error.as_deref(), Some("x"));
    }
}
