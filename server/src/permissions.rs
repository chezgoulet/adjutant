//! Identity extraction + route-level permission enforcement (SPEC §9).
//!
//! Milestone 1 stub: identity comes from `x-dev-user` / `x-dev-role` headers.
//! The auth plugin (Milestone 2) replaces `extract_identity` with real session
//! validation — the enforcement half below does not change.

use axum::http::HeaderMap;

use adjutant_sdk::{Identity, PermissionService, Scope};

/// Extract the stub identity from dev headers. Every role is granted at
/// **troop scope** (`Identity::new`), which is exactly why the dev stub is more
/// permissive than a real session and stays off unless explicitly enabled.
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
    Some(Identity::new(user_id, roles))
}

/// Check one permission for the identity on a request, at the route's declared
/// scope.
///
/// `required_scope = Some(scope)` — the caller must hold the permission through a
/// grant that **covers** `scope` (ordinary routes pass `Scope::troop()`);
/// `None` — "any scope": the caller must hold the permission somewhere and the
/// handler checks the object's scope. Denials are logged with the permission,
/// the required scope and the caller's grant scopes. Identity is extracted in
/// the dispatch wrapper (no middleware layer — avoids the `from_fn` state
/// signature trap and the route-ordering gotcha).
pub async fn authorize(
    identity: Option<&Identity>,
    permissions: &PermissionService,
    required: &str,
    required_scope: Option<&Scope>,
) -> Result<(), u16> {
    let allowed = match required_scope {
        Some(scope) => permissions.has_in_scope(identity, required, scope).await,
        None => permissions.has_any_scope(identity, required).await,
    };
    if allowed {
        return Ok(());
    }
    tracing::warn!(
        permission = required,
        required_scope = ?required_scope,
        grants = ?identity.map(|i| i.grants.as_slice()),
        "permission denied"
    );
    if identity.is_none() {
        Err(401) // authenticated identity required
    } else {
        Err(403) // authenticated but not authorized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    /// Permits every check (query returns `n = 1`), so the gate's *scope*
    /// decision is what the tests exercise.
    struct AllowDb;

    #[async_trait::async_trait]
    impl adjutant_sdk::HostDb for AllowDb {
        async fn execute(
            &self,
            _sql: String,
            _params: Vec<adjutant_sdk::SqlValue>,
        ) -> Result<u64, adjutant_sdk::SdkError> {
            Ok(1)
        }
        async fn query(
            &self,
            _sql: String,
            _params: Vec<adjutant_sdk::SqlValue>,
        ) -> Result<Vec<serde_json::Value>, adjutant_sdk::SdkError> {
            Ok(vec![serde_json::json!({ "n": 1 })])
        }
    }

    fn svc() -> PermissionService {
        PermissionService::new(std::sync::Arc::new(AllowDb))
    }

    #[test]
    fn identity_parses_from_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-dev-user", HeaderValue::from_static("beatrice"));
        headers.insert("x-dev-role", HeaderValue::from_static("chief,scout"));
        let id = extract_identity(&headers).expect("identity");
        assert_eq!(id.user_id, "beatrice");
        assert_eq!(id.roles(), vec!["chief", "scout"]);
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
        assert!(id.roles().is_empty());
    }

    /// Conformance matrix at the gate: an ordinary route requires a
    /// **troop-covering** grant; a lodge grant does not open it.
    #[tokio::test]
    async fn ordinary_route_requires_troop_coverage() {
        let svc = svc();
        let troop = Identity::new("u", vec!["chief".into()]);
        assert!(authorize(Some(&troop), &svc, "x", Some(&Scope::troop())).await.is_ok());

        let lodge = Identity::from_grants(
            "u",
            vec![adjutant_sdk::RoleGrant { role_id: "lc".into(), scope: Scope::lodge("1") }],
        );
        assert_eq!(
            authorize(Some(&lodge), &svc, "x", Some(&Scope::troop())).await,
            Err(403),
            "a lodge grant must not open a troop route"
        );
    }

    /// An `any_scope` route (required_scope = None) accepts the permission at
    /// some scope; the handler then checks the object. No identity is 401.
    #[tokio::test]
    async fn any_scope_route_accepts_a_lodge_grant() {
        let svc = svc();
        let lodge = Identity::from_grants(
            "u",
            vec![adjutant_sdk::RoleGrant { role_id: "lc".into(), scope: Scope::lodge("1") }],
        );
        assert!(authorize(Some(&lodge), &svc, "x", None).await.is_ok());
        assert_eq!(authorize(None, &svc, "x", None).await, Err(401));
    }
}
