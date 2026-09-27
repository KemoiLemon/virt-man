// Persistent per-user activity log.
//
// The SPA used to keep finished tasks in an in-memory dock that vanished on
// reload and was invisible to everyone else. Entries now live in the store, so
// they survive restarts, and every one is attributed to the account that ran
// it: a non-admin sees only their own history, an admin sees the whole log.

use axum::extract::{Query, State};
use axum::middleware;
use axum::routing::get;
use axum::Json;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use super::ApiError;
use crate::auth;
use crate::state::AppState;
use crate::store::{random_hex, ActivityEntry, User};

/// Cap on retained entries. The log is appended to forever otherwise, and an
/// unbounded JSON store would grow without limit. Oldest are dropped first.
const MAX_ENTRIES: usize = 500;

pub fn routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/activity", get(list).post(record).delete(clear))
        .layer(middleware::from_fn(auth::require_user(state.clone())))
}

fn entry_json(e: &ActivityEntry) -> Value {
    json!({
        "id": e.id,
        "user_id": e.user_id,
        "username": e.username,
        "label": e.label,
        "status": e.status,
        "message": e.message,
        "at_ms": e.at_ms,
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Whether the caller may see every user's entries. Mirrors the admin check
/// used elsewhere: an explicit scope read on "users".
fn sees_all(state: &AppState, user: &User) -> bool {
    state.read().can(user, "users", false)
}

/// Entries `user` may see, newest first: everyone's for an admin, otherwise
/// only their own. Pure, so the visibility rule is testable without a request.
fn visible<'a>(entries: &'a [ActivityEntry], user_id: &str, all: bool) -> Vec<&'a ActivityEntry> {
    entries
        .iter()
        .rev()
        .filter(|e| all || e.user_id == user_id)
        .collect()
}

/// Drop the oldest entries once the log exceeds the cap. The log is
/// append-only and chronological, so the front is always what goes.
fn trim(entries: &mut Vec<ActivityEntry>) {
    let len = entries.len();
    if len > MAX_ENTRIES {
        entries.drain(..len - MAX_ENTRIES);
    }
}

#[derive(Deserialize)]
struct ListQuery {
    /// Newest-first page size. Omitted means "all retained".
    #[serde(default)]
    limit: Option<usize>,
}

async fn list(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Query(q): Query<ListQuery>,
) -> Json<Value> {
    let all = sees_all(&state, &user);
    let limit = q.limit.unwrap_or(MAX_ENTRIES).min(MAX_ENTRIES);
    let store = state.read();
    let entries: Vec<Value> = visible(&store.activity, &user.id, all)
        .into_iter()
        .take(limit)
        .map(entry_json)
        .collect();
    Json(json!({"entries": entries, "all_users": all}))
}

#[derive(Deserialize)]
struct RecordReq {
    label: String,
    /// "done" or "failed"; anything else is recorded as-is rather than
    /// rejected, so a future status does not need a server change.
    status: String,
    #[serde(default)]
    message: String,
    /// Admin-only: attribute the entry to someone else. Without the `users`
    /// scope this is ignored, so a caller cannot forge another user's history.
    #[serde(default)]
    user_id: Option<String>,
}

async fn record(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
    Json(req): Json<RecordReq>,
) -> Result<Json<Value>, ApiError> {
    let label = req.label.trim();
    if label.is_empty() {
        return Err(ApiError::bad_request("label is required"));
    }

    // `sees_all` takes a read lock, so resolve it before the write lock below:
    // an RwLock is not reentrant and nesting them would deadlock.
    let all = sees_all(&state, &user);
    let (user_id, username) = match req.user_id {
        Some(id) if all && id != user.id => {
            let owner = state
                .read()
                .users
                .iter()
                .find(|u| u.id == id)
                .map(|u| (u.id.clone(), u.username.clone()));
            owner.ok_or_else(|| ApiError::bad_request("unknown user"))?
        }
        _ => (user.id.clone(), user.username.clone()),
    };

    let entry = ActivityEntry {
        id: random_hex(8),
        user_id,
        username,
        label: label.to_string(),
        status: req.status,
        message: req.message,
        at_ms: now_ms(),
    };
    {
        let mut store = state.write();
        store.activity.push(entry.clone());
        trim(&mut store.activity);
        store.save(&state.data_dir).map_err(ApiError::internal)?;
    }
    Ok(Json(entry_json(&entry)))
}

/// Clear the caller's own history, or everyone's for an admin. Returns how
/// many entries were removed so the UI can confirm.
async fn clear(
    State(state): State<AppState>,
    axum::extract::Extension(user): axum::extract::Extension<User>,
) -> Result<Json<Value>, ApiError> {
    let all = sees_all(&state, &user);
    let removed = {
        let mut store = state.write();
        let before = store.activity.len();
        if all {
            store.activity.clear();
        } else {
            store.activity.retain(|e| e.user_id != user.id);
        }
        let removed = before - store.activity.len();
        store.save(&state.data_dir).map_err(ApiError::internal)?;
        removed
    };
    Ok(Json(json!({ "removed": removed, "all_users": all })))
}

#[cfg(test)]
mod tests {
    use super::{trim, visible, ActivityEntry, MAX_ENTRIES};

    fn entry(id: &str, user: &str, at: u64) -> ActivityEntry {
        ActivityEntry {
            id: id.into(),
            user_id: user.into(),
            username: user.into(),
            label: format!("action {id}"),
            status: "done".into(),
            message: String::new(),
            at_ms: at,
        }
    }

    #[test]
    fn a_user_sees_only_their_own_activity() {
        let log = vec![
            entry("a", "alice", 1),
            entry("b", "bob", 2),
            entry("c", "alice", 3),
        ];
        let ids: Vec<&str> = visible(&log, "alice", false)
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, vec!["c", "a"], "own entries, newest first");
    }

    #[test]
    fn an_admin_sees_everyones_activity() {
        let log = vec![entry("a", "alice", 1), entry("b", "bob", 2)];
        let ids: Vec<&str> = visible(&log, "root", true)
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, vec!["b", "a"], "all entries, newest first");
    }

    #[test]
    fn the_log_is_capped_and_drops_the_oldest_first() {
        let mut log: Vec<ActivityEntry> = (0..MAX_ENTRIES + 5)
            .map(|i| entry(&format!("{i}"), "alice", i as u64))
            .collect();
        trim(&mut log);
        assert_eq!(log.len(), MAX_ENTRIES, "capped");
        assert_eq!(
            log.first().map(|e| e.id.as_str()),
            Some("5"),
            "oldest dropped"
        );
        assert_eq!(
            log.last().map(|e| e.id.as_str()),
            Some(format!("{}", MAX_ENTRIES + 4).as_str()),
            "newest retained"
        );
    }

    #[test]
    fn a_short_log_is_left_alone() {
        let mut log = vec![entry("a", "alice", 1)];
        trim(&mut log);
        assert_eq!(log.len(), 1);
    }
}
