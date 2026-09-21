//! Identity extraction + route-level permission enforcement (SPEC §9).
//!
//! Milestone 1 stub: identity comes from `x-dev-user` / `x-dev-role` headers.
//! The auth plugin (Milestone 2) replaces `extract_identity` with real session
//! validation — the enforcement half below does not change.

use axum::http::HeaderMap;

use adjutant_sdk::{Identity, PermissionService};

/// Extract the stub identity from dev headers.
pub fn extract_identity(headers: &HeaderMap) -> Option<Identity> {
    let user_id = headers.get("x-dev-user")?.to_str().ok()?.to_string();
    if user_id.is_empty() {
        return None;
    }
    let roles = headers
        .get("x-dev-role")
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    Some(Identity { user_id, roles })
}

/// Check one permission for the identity on a request. Identity is extracted
/// in the dispatch wrapper (no middleware layer — avoids the from_fn state
/// signature trap and the route-ordering gotcha).
pub async fn authorize(
    identity: Option<&Identity>,
    permissions: &PermissionService,
    required: &str,
) -> Result<(), u16> {
    if permissions.has(identity, required).await {
        Ok(())
    } else if identity.is_none() {
        Err(401) // authenticated identity required
    } else {
        Err(403) // authenticated but not authorized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn identity_parses_from_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-dev-user", HeaderValue::from_static("beatrice"));
        headers.insert("x-dev-role", HeaderValue::from_static("chief,scout"));
        let id = extract_identity(&headers).expect("identity");
        assert_eq!(id.user_id, "beatrice");
        assert_eq!(id.roles, vec!["chief", "scout"]);
    }

    #[test]
    fn identity_absent_without_user_header() {
        let headers = HeaderMap::new();
        assert!(extract_identity(&headers).is_none());
    }

    #[test]
    fn identity_handles_empty_role_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-dev-user", HeaderValue::from_static("shannon"));
        let id = extract_identity(&headers).expect("identity");
        assert!(id.roles.is_empty());
    }
}
