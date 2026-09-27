//! The wizard's door to the decision the CLI makes with `--enable` (#90).
//!
//! Both doors call [`crate::plugin_choice::apply`], so "the two paths produce the
//! same state" is a property of there being one implementation rather than of two
//! agreeing by luck. This module is the thin part: permission, identity, JSON in,
//! JSON out, and the audit line.

use std::sync::Arc;

use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::server::{audit_state_change, error_response, internal_error, AppState};

/// Choosing which plugins a deployment runs is plugin administration, so this asks
/// for what enable and disable already ask for — no new permission, and no way for
/// the wizard to be a quieter door than the screen beside it.
const PERM: &str = "core:admin";

/// Has an operator chosen, and what did they choose? `null` means nobody has.
///
/// Also reports the rules the core enforces on a choice — which plugins are
/// required and *why*, and which depend on which. The wizard needs those words to
/// tell a leader what turning something off costs, and it should render the
/// server's own sentences rather than a second copy in the client: the reason a
/// plugin is required is a statement about the server's behaviour, and a copy in
/// Dart would drift the first time a rule changed.
///
/// The server answers whether a choice exists rather than the device, because a
/// per-device flag would re-prompt the second admin, could not be set by a
/// scripted install, and could not tell "chose exactly auth and membership" from
/// "never chose".
pub async fn get(State(state): State<Arc<AppState>>, req: Request) -> Response {
    if let Some(resp) = state.require_permission(req.headers(), PERM).await {
        return resp;
    }
    match crate::plugin_choice::recorded(state.pool.as_ref()).await {
        Ok(choice) => Json(serde_json::json!({
            "choice": choice,
            "required": crate::server::REQUIRED_PLUGINS
                .iter()
                .map(|(id, why)| serde_json::json!({ "id": id, "why": why }))
                .collect::<Vec<_>>(),
            "dependencies": crate::server::PLUGIN_DEPENDENCIES
                .iter()
                .map(|(dependent, dependency)| {
                    serde_json::json!({ "dependent": dependent, "dependency": dependency })
                })
                .collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => internal_error("recorded plugin choice", &e),
    }
}

/// Set the enabled set and record the choice, as one act.
///
/// `plugin_ids` is required rather than defaulted: an absent key must never read
/// as "turn everything off", which is the one outcome nobody would forgive.
pub async fn set(State(state): State<Arc<AppState>>, req: Request) -> Response {
    if let Some(resp) = state.require_permission(req.headers(), PERM).await {
        return resp;
    }
    let identity = state.resolve_identity(req.headers()).await;
    let actor = identity.as_ref().map(|i| i.user_id.clone());

    let (_, body) = req.into_parts();
    let bytes = to_bytes(body, state.config.max_body_bytes)
        .await
        .unwrap_or_default();
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")),
    };

    let chosen: Vec<String> = match value.get("plugin_ids").and_then(|v| v.as_array()) {
        Some(list) => list
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "plugin_ids is required: the ids this deployment should run".to_string(),
            )
        }
    };

    if let Some(resp) = audit_state_change(
        &state.audit,
        identity.as_ref(),
        "plugin.choice",
        "plugin",
        "enabled_set",
        serde_json::json!({ "plugin_ids": chosen }),
    )
    .await
    {
        return resp;
    }

    match crate::plugin_choice::apply(
        state.pool.as_ref(),
        &chosen,
        crate::plugin_choice::SOURCE_WIZARD,
        actor.as_deref(),
    )
    .await
    {
        Ok(enabled) => {
            // The flags are the durable truth, but a running server keeps serving
            // the registry it loaded at boot. Writing them alone would leave the
            // operator's choice inert until a restart — the opposite of "enabled
            // means loaded", and exactly what the acceptance for #90 forbids
            // ("afterwards the server has those plugins loaded"). So reconcile the
            // same way the reload route does, through the same two functions.
            match crate::server::load_fresh_generation(&state).await {
                Ok((fresh, fresh_hierarchy)) => {
                    crate::server::adopt_generation(&state, fresh, fresh_hierarchy).await;
                    Json(serde_json::json!({ "enabled": enabled })).into_response()
                }
                Err(resp) => resp,
            }
        }
        // Every refusal is a statement about what was asked for — an id that is not
        // on disk, a required plugin left off, a dependent without its dependency —
        // so it is a bad request that names the reason. Not a 500, and not a silent
        // correction of the operator's intent.
        Err(why) => error_response(StatusCode::BAD_REQUEST, why),
    }
}
