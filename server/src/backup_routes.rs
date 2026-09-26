//! The backup routes: the picker's options, the schedule, the button, and the
//! bundle itself.
//!
//! All four ask for `core:backup` rather than `core:admin`. A bundle is a
//! complete copy of everything the troop has, so it is the sort of thing a troop
//! should be able to delegate — and the sort of thing it should be able to
//! revoke — without also handing over plugin installation.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::server::{audit_state_change, error_response, internal_error, AppState};

const PERM: &str = "core:backup";

/// How many runs the list shows. The directory is the record of what exists, so
/// this bounds the page rather than the history.
const RUN_LIMIT: i64 = 50;

/// The picker's options, the current schedule, and the recent runs.
pub async fn list(State(state): State<Arc<AppState>>, req: Request) -> Response {
    if let Some(resp) = state.require_permission(req.headers(), PERM).await {
        return resp;
    }
    let schedule = match crate::backup::read_schedule(state.pool.as_ref()).await {
        Ok(s) => s,
        Err(e) => return internal_error("backup schedule", &e),
    };
    let runs = match crate::backup::list_runs(state.pool.as_ref(), RUN_LIMIT).await {
        Ok(r) => r,
        Err(e) => return internal_error("backup runs", &e),
    };
    Json(serde_json::json!({
        "presets": crate::backup::PRESETS
            .iter()
            .map(|(secs, label)| serde_json::json!({ "seconds": secs, "label": label }))
            .collect::<Vec<_>>(),
        "schedule": schedule,
        "runs": runs,
        // Shown in the UI, because "where did the bundle go" is the first
        // question an admin asks and the container's path is not obvious.
        "directory": crate::backup::backup_dir().display().to_string(),
    }))
    .into_response()
}

/// Set the cadence, the retention and whether the timer runs at all.
pub async fn set_schedule(State(state): State<Arc<AppState>>, req: Request) -> Response {
    if let Some(resp) = state.require_permission(req.headers(), PERM).await {
        return resp;
    }
    let identity = state.resolve_identity(req.headers()).await;
    let actor = identity.as_ref().map(|i| i.user_id.clone());

    let (_, body) = req.into_parts();
    let bytes = to_bytes(body, state.config.max_body_bytes).await.unwrap_or_default();
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")),
    };

    // An absent key is not the same as an explicit null for cadence: the former
    // leaves the cadence alone, the latter turns scheduled backups off. Sending
    // `null` deliberately is how the UI says "no schedule".
    let cadence = value.get("cadence_secs").and_then(|v| {
        if v.is_null() {
            Some(None)
        } else {
            v.as_i64().map(Some)
        }
    });
    let keep = value.get("keep").and_then(|v| v.as_i64()).map(|k| k as i32);
    let enabled = value.get("enabled").and_then(|v| v.as_bool());

    let current = match crate::backup::read_schedule(state.pool.as_ref()).await {
        Ok(s) => s,
        Err(e) => return internal_error("backup schedule", &e),
    };
    let cadence = cadence.unwrap_or(current.cadence_secs);
    let keep = keep.unwrap_or(current.keep);
    let enabled = enabled.unwrap_or(current.enabled);

    if let Some(resp) = audit_state_change(
        &state.audit,
        identity.as_ref(),
        "backup.schedule",
        "backup",
        "schedule",
        serde_json::json!({ "cadence_secs": cadence, "keep": keep, "enabled": enabled }),
    )
    .await
    {
        return resp;
    }

    match crate::backup::write_schedule(
        state.pool.as_ref(),
        cadence,
        keep,
        enabled,
        actor.as_deref(),
    )
    .await
    {
        Ok(s) => Json(serde_json::json!({ "schedule": s })).into_response(),
        // A cadence the picker cannot display, or a retention out of range, is a
        // bad request and says which — not a 500, and not a silent clamp.
        Err(why) => error_response(StatusCode::BAD_REQUEST, why),
    }
}

/// The button. Runs a bundle now, whoever pressed it, and reports what happened.
pub async fn run_now(State(state): State<Arc<AppState>>, req: Request) -> Response {
    if let Some(resp) = state.require_permission(req.headers(), PERM).await {
        return resp;
    }
    let identity = state.resolve_identity(req.headers()).await;
    let actor = identity.as_ref().map(|i| i.user_id.clone());

    if let Some(resp) = audit_state_change(
        &state.audit,
        identity.as_ref(),
        "backup.run",
        "backup",
        "manual",
        serde_json::json!({}),
    )
    .await
    {
        return resp;
    }

    match crate::backup::run(
        state.pool.as_ref(),
        &state.config.database_url,
        crate::backup::TRIGGER_MANUAL,
        actor.as_deref(),
    )
    .await
    {
        Ok(run) => Json(serde_json::json!({ "run": run })).into_response(),
        // The failure is already recorded on the run row, so the caller gets the
        // reason and the admin surface already shows it. 409 rather than 500:
        // "the dump could not be made" is a state of the world, not a bug here.
        Err(why) => error_response(StatusCode::CONFLICT, why),
    }
}

/// Hand the bundle over. A copy of everything the troop has leaving the building
/// is worth an audit row, so this is recorded even though it changes nothing.
pub async fn download(
    State(state): State<Arc<AppState>>,
    Path(filename): Path<String>,
    req: Request,
) -> Response {
    if let Some(resp) = state.require_permission(req.headers(), PERM).await {
        return resp;
    }
    let identity = state.resolve_identity(req.headers()).await;

    let dir = crate::backup::backup_dir();
    // The filename comes from a URL, so it is resolved against the directory and
    // never joined blindly: `bundle_path` refuses separators, `..`, and anything
    // that is not a `.dump`.
    let Some(path) = crate::backup::bundle_path(&dir, &filename) else {
        return error_response(StatusCode::NOT_FOUND, "no such bundle");
    };
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => return internal_error("reading the bundle", &e),
    };

    if let Some(resp) = audit_state_change(
        &state.audit,
        identity.as_ref(),
        "backup.download",
        "backup",
        &filename,
        serde_json::json!({ "bytes": bytes.len() }),
    )
    .await
    {
        return resp;
    }

    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
    );
    // `attachment` with the same name, so a browser saves it rather than trying
    // to render it, and the client's share sheet gets a real filename.
    if let Ok(v) = header::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    resp
}
