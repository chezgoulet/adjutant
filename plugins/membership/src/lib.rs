//! # adjutant-membership — roster, patrols, lodges, proficiencies, stewards
//! (SPEC §7.2).
//!
//! OSG integration posture (SPEC §10): OSG owns registration and background
//! checks; this plugin **consumes** that data (CSV import baseline per Resolved
//! Decision 4) and layers troop-local reality on top — trail names, patrol
//! assignments, leadership positions, proficiencies.
//!
//! Route style note: the core supports path captures (`/api/x/{id}`), but this
//! plugin addresses records with a query parameter
//! (`/api/membership/member?id=…`) because the id is optional at several call
//! sites and sits naturally beside the other filters (`?patrol=`,
//! `?include_inactive=`).

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;
use serde::Deserialize;

pub struct MembershipPlugin {
    ctx: OnceLock<PluginContext>,
}

impl MembershipPlugin {
    pub fn new() -> Self {
        Self { ctx: OnceLock::new() }
    }

    fn ctx(&self) -> &PluginContext {
        self.ctx.get().expect("core must call init() before routes()")
    }
}

impl Default for MembershipPlugin {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// CSV parsing (SPEC: OSG CSV import is the baseline)
// ---------------------------------------------------------------------------

/// Minimal RFC-4180-ish CSV: quoted fields, `""` escape, CRLF or LF.
/// Returns `Err(line_no)` on a ragged row so the caller can report position.
pub fn parse_csv(input: &str) -> Result<Vec<Vec<String>>, usize> {
    let mut rows = Vec::new();
    let mut field = String::new();
    let mut row = Vec::new();
    let mut in_quotes = false;
    let mut chars = input.chars().peekable();
    let mut line = 1usize;

    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        field.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                }
                '\n' => {
                    line += 1;
                    field.push('\n');
                }
                _ => field.push(c),
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => row.push(std::mem::take(&mut field)),
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    if row.iter().any(|f| !f.trim().is_empty()) {
                        rows.push(std::mem::take(&mut row));
                    } else {
                        row.clear();
                    }
                    line += 1;
                }
                '\r' => {}
                _ => field.push(c),
            }
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    if in_quotes {
        return Err(line);
    }
    Ok(rows)
}

/// Header-driven mapping. Recognized columns (case-insensitive, order-free):
/// `username`, `email`, `display_name`, `trail_name`, `patrol`, `lodge`,
/// `osg_id`, `bg_check`. A `roles` column is accepted but **ignored** (role
/// assignment belongs to auth, #19) and reported in `roles_ignored`.
/// Rows missing `username` are skipped with a reason.
#[derive(Debug, serde::Serialize)]
pub struct ImportReport {
    pub created: usize,
    pub updated: usize,
    pub skipped: Vec<(usize, String)>,
    pub patrols_created: Vec<String>,
    /// Rows whose CSV `roles` column was ignored, with the roles and why. Empty
    /// when the CSV had no `roles` column.
    pub roles_ignored: Vec<RoleIgnored>,
}

/// One ignored `roles` cell. Explicit so an operator sees the column did nothing
/// instead of assuming roles were assigned.
#[derive(Debug, serde::Serialize)]
pub struct RoleIgnored {
    pub line: usize,
    pub roles: Vec<String>,
    pub reason: String,
}

fn col(header: &[String], name: &str) -> Option<usize> {
    // Normalize separators so "User Name", "user_name" and "username" all
    // match — real CSVs from OSG spreadsheets are inconsistent.
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| !matches!(c, ' ' | '_' | '-' | '\t'))
            .collect::<String>()
            .to_lowercase()
    };
    let target = norm(name);
    header.iter().position(|h| norm(h) == target)
}

/// Full member record. `lodge_id` is carried for the `read_lodge` scope check
/// (#20); `lodge` (the name) is for display.
const MEMBER_SELECT: &str = "SELECT m.id, m.username, m.email, m.display_name, m.trail_name, \
        m.osg_id, m.bg_check, m.is_active, p.name AS patrol, l.name AS lodge, l.id AS lodge_id, \
        COALESCE((SELECT json_agg(json_build_object(\
            'code', pr.code, 'title', pr.title, \
            'completed_on', mp.completed_on, \
            'signed_off_by', mp.signed_off_by\
         )) FROM member_proficiencies mp \
           JOIN proficiencies pr ON pr.id = mp.proficiency_id \
           WHERE mp.member_id = m.id), '[]') AS proficiencies, \
        COALESCE((SELECT json_agg(json_build_object(\
            'position', s.position, 'appointed_on', s.appointed_on, \
            'vacant', s.vacant\
         )) FROM stewards s WHERE s.member_id = m.id \
           AND NOT s.vacant), '[]') AS current_positions \
     FROM members m \
     LEFT JOIN patrols p ON p.id = m.patrol_id \
     LEFT JOIN lodges l ON l.id = p.lodge_id \
     WHERE m.id = $1";

/// 403 for a member the caller may not see. The **same** message covers "absent"
/// and "forbidden" so a caller without `read_all` cannot use the status to learn
/// which member ids exist.
const MEMBER_FORBIDDEN: &str =
    "reading this member requires membership:read_all, membership:read_lodge for \
     their lodge, or membership:read for your own record";

async fn fetch_member(
    c: &PluginContext,
    id: i64,
) -> Result<Option<serde_json::Value>, SdkError> {
    let rows = c
        .db
        .query(MEMBER_SELECT.to_string(), vec![SqlValue::Int(id)])
        .await?;
    Ok(rows.into_iter().next())
}

#[cfg(test)]
mod csv_tests {
    use super::*;

    #[test]
    fn parses_quoted_fields_and_crlf() {
        let rows = parse_csv("a,b\r\n\"x,1\",\"he said \"\"hi\"\"\"\r\n").unwrap();
        assert_eq!(rows[0], vec!["a", "b"]);
        assert_eq!(rows[1], vec!["x,1", "he said \"hi\""]);
    }

    #[test]
    fn skips_blank_lines_but_reports_unterminated_quotes() {
        assert_eq!(parse_csv("a,b\n\n,c\n").unwrap().len(), 2);
        assert!(parse_csv("a,\"broken").is_err());
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let h = vec!["User Name".to_string(), "Email".to_string()];
        assert_eq!(col(&h, "username"), Some(0));
        assert_eq!(col(&h, "EMAIL"), Some(1));
        assert_eq!(col(&h, "missing"), None);
    }

    /// Dogfoods `adjutant_sdk::testing`: the CSV upsert path runs against
    /// `MockDb`, proving a new member and a newly-created patrol are reported.
    #[tokio::test]
    async fn upsert_from_import_reports_a_create_and_a_new_patrol() {
        use adjutant_sdk::testing::TestHost;

        let host = TestHost::new();
        host.db.push_rows(vec![serde_json::json!({ "n": 0 })]); // patrol count: new
        host.db.push_rows(vec![serde_json::json!({ "id": 7 })]); // created patrol id
        host.db.push_rows(vec![serde_json::json!({ "inserted": true })]); // member upsert

        let ctx = host.context("membership");
        let mut report = ImportReport {
            created: 0,
            updated: 0,
            skipped: Vec::new(),
            patrols_created: Vec::new(),
            roles_ignored: Vec::new(),
        };
        upsert_from_import(
            &ctx,
            "bea",
            Some("bea@example.org".to_string()),
            "Bea",
            None,
            None,
            None,
            Some("Otters".to_string()),
            true,
            &mut report,
            2,
        )
        .await
        .unwrap();

        assert_eq!(report.created, 1);
        assert_eq!(report.updated, 0);
        assert_eq!(report.patrols_created, vec!["Otters".to_string()]);
        assert_eq!(host.db.query_count(), 3);
    }

    // --- #19: the CSV `roles` column is ignored, never written ---------------

    /// A `membership:manage` caller importing a CSV with a `roles` column must
    /// not touch `core.user_roles`, and the report must say the column was
    /// ignored.
    #[tokio::test]
    async fn import_ignores_the_roles_column_and_reports_it() {
        use adjutant_sdk::testing::{response_json, TestHost, TestRequest};

        let host = TestHost::new();
        host.db.push_rows(vec![serde_json::json!({ "inserted": true })]); // member upsert

        let mut plugin = MembershipPlugin::new();
        plugin.init(host.context("membership")).await.unwrap();
        let routes = plugin.routes();
        let import = routes
            .iter()
            .find(|r| r.path == "/api/membership/import")
            .expect("import route");

        let csv = "username,display_name,roles\nbea,Bea,scout;chief\n";
        let mut req = TestRequest::post("/api/membership/import").build();
        req.body = csv.as_bytes().to_vec();
        let resp = (import.handler)(req).await.expect("handler");
        assert_eq!(resp.status, 200);

        let body = response_json(&resp);
        assert_eq!(body["created"], serde_json::json!(1));
        let ignored = body["roles_ignored"].as_array().expect("roles_ignored");
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0]["line"], serde_json::json!(2));
        assert_eq!(ignored[0]["roles"], serde_json::json!(["scout", "chief"]));
        assert!(ignored[0]["reason"].as_str().unwrap().contains("auth"));

        // The importer must not read or write core.user_roles at all.
        let queried = host.db.queried.lock().unwrap();
        assert!(
            queried.iter().all(|c| !c.sql.contains("core.user_roles")),
            "the importer must not query core.user_roles"
        );
        drop(queried);
        assert!(
            host.db.executed_sql().iter().all(|s| !s.contains("core.user_roles")),
            "the importer must not write core.user_roles"
        );
    }

    // --- #20: per-member authorization ladder --------------------------------

    async fn call_member(
        host: &adjutant_sdk::testing::TestHost,
        identity: Option<Identity>,
        id: i64,
    ) -> (u16, serde_json::Value) {
        use adjutant_sdk::testing::{response_json, TestRequest};

        let mut plugin = MembershipPlugin::new();
        plugin.init(host.context("membership")).await.unwrap();
        let routes = plugin.routes();
        let route = routes
            .iter()
            .find(|r| r.method.as_str() == "GET" && r.path == "/api/membership/member")
            .expect("member route");
        let mut req = TestRequest::get("/api/membership/member")
            .query_param("id", &id.to_string())
            .build();
        req.identity = identity;
        let resp = (route.handler)(req).await.expect("handler");
        (resp.status, response_json(&resp))
    }

    fn grant(role: &str, scope: Scope) -> Identity {
        Identity::from_grants(
            "11111111-1111-1111-1111-111111111111",
            vec![RoleGrant { role_id: role.into(), scope }],
        )
    }

    /// `membership:read_all` is troop-wide and reaches any member.
    #[tokio::test]
    async fn read_all_reaches_another_member() {
        let host = adjunct_sdk_test_host();
        host.db.push_rows(vec![serde_json::json!({ "n": 1 })]); // has(read_all)
        host.db
            .push_rows(vec![serde_json::json!({ "id": 1, "username": "mallory", "lodge_id": 7 })]);
        let (status, body) = call_member(&host, Some(grant("chief", Scope::troop())), 1).await;
        assert_eq!(status, 200);
        assert_eq!(body["member"]["username"], serde_json::json!("mallory"));
    }

    /// `membership:read` reaches the caller's **own** record (case-insensitive).
    #[tokio::test]
    async fn read_reaches_self_only() {
        let host = adjunct_sdk_test_host();
        host.db.push_rows(vec![serde_json::json!({ "n": 0 })]); // has(read_all) -> deny
        host.db.push_rows(vec![serde_json::json!({ "username": "bea" })]); // caller
        host.db.push_rows(vec![serde_json::json!({ "id": 1, "username": "Bea", "lodge_id": null })]);
        host.db.push_rows(vec![serde_json::json!({ "n": 1 })]); // has(read)
        let (status, _) = call_member(&host, Some(grant("scout", Scope::troop())), 1).await;
        assert_eq!(status, 200);
    }

    /// `membership:read` must **not** reach another member.
    #[tokio::test]
    async fn read_denies_another_member() {
        let host = adjunct_sdk_test_host();
        host.db.push_rows(vec![serde_json::json!({ "n": 0 })]); // has(read_all)
        host.db.push_rows(vec![serde_json::json!({ "username": "bea" })]);
        host.db
            .push_rows(vec![serde_json::json!({ "id": 1, "username": "mallory", "lodge_id": 7 })]);
        host.db.push_rows(vec![serde_json::json!({ "n": 0 })]); // has_in_scope(read_lodge)
        let (status, body) = call_member(&host, Some(grant("scout", Scope::troop())), 1).await;
        assert_eq!(status, 403);
        assert!(body["error"].as_str().unwrap().contains("membership:read_all"));
    }

    /// A lodge-scoped `read_lodge` reaches its own lodge and is denied another.
    /// A lodge grant does not cover troop, so the `read_all` check makes **no**
    /// query (there is no covering role).
    #[tokio::test]
    async fn read_lodge_reaches_its_lodge_only() {
        let own = adjunct_sdk_test_host();
        own.db.push_rows(vec![serde_json::json!({ "username": "bea" })]); // caller
        own.db
            .push_rows(vec![serde_json::json!({ "id": 1, "username": "mallory", "lodge_id": 7 })]);
        own.db.push_rows(vec![serde_json::json!({ "n": 1 })]); // covered -> has permission
        let (status, _) = call_member(&own, Some(grant("lodge_commander", Scope::lodge("7"))), 1)
            .await;
        assert_eq!(status, 200, "own lodge is reachable");

        let other = adjunct_sdk_test_host();
        other.db.push_rows(vec![serde_json::json!({ "username": "bea" })]);
        other.db
            .push_rows(vec![serde_json::json!({ "id": 1, "username": "mallory", "lodge_id": 8 })]);
        // lodge "8" is not covered: no permission query is even made.
        let (status, _) = call_member(
            &other,
            Some(grant("lodge_commander", Scope::lodge("7"))),
            1,
        )
        .await;
        assert_eq!(status, 403, "another lodge is denied");
    }

    /// A caller without `read_all` gets 403 for an absent member — the same as
    /// for a forbidden one, so existence is not leaked.
    #[tokio::test]
    async fn absent_member_is_403_not_404_for_a_restricted_caller() {
        let host = adjunct_sdk_test_host();
        host.db.push_rows(vec![serde_json::json!({ "n": 0 })]); // has(read_all)
        host.db.push_rows(vec![serde_json::json!({ "username": "bea" })]);
        host.db.push_rows(vec![]); // member not found
        let (status, _) = call_member(&host, Some(grant("scout", Scope::troop())), 999).await;
        assert_eq!(status, 403);
    }

    /// The reference-data routes moved off the self-service permission.
    #[tokio::test]
    async fn reference_routes_require_read_lodge() {
        use adjutant_sdk::testing::TestHost;
        let mut plugin = MembershipPlugin::new();
        let host = TestHost::new();
        plugin.init(host.context("membership")).await.unwrap();
        for path in [
            "/api/membership/lodges",
            "/api/membership/proficiencies",
            "/api/membership/stewards",
        ] {
            let r = plugin
                .routes()
                .into_iter()
                .find(|r| r.path == path)
                .unwrap_or_else(|| panic!("route {path}"));
            assert_eq!(
                r.required_permission.as_deref(),
                Some("membership:read_lodge"),
                "{path} must require membership:read_lodge"
            );
        }
    }

    fn adjunct_sdk_test_host() -> adjutant_sdk::testing::TestHost {
        adjutant_sdk::testing::TestHost::new()
    }

    // --- #146: collection routes narrow to the caller's lodge scopes ----------

    /// Call a GET collection route with an explicit scoped identity.
    ///
    /// A handler may refuse with `Err(SdkError)` or `Ok(PluginResponse::error)`;
    /// both are collapsed here so a `?`-refusal is a test failure, not a panic.
    async fn call_collection(
        host: &adjutant_sdk::testing::TestHost,
        path: &str,
        identity: Identity,
    ) -> (u16, serde_json::Value) {
        use adjutant_sdk::testing::{response_json, TestRequest};

        let mut plugin = MembershipPlugin::new();
        plugin.init(host.context("membership")).await.unwrap();
        let route = plugin
            .routes()
            .into_iter()
            .find(|r| r.method.as_str() == "GET" && r.path == path)
            .unwrap_or_else(|| panic!("route {path}"));
        let mut req = TestRequest::get(path).build();
        req.identity = Some(identity);
        match (route.handler)(req).await {
            Ok(resp) => (resp.status, response_json(&resp)),
            Err(e) => (e.status(), serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// One row as `PermissionService::scopes_for` returns it.
    fn held_at(scope_type: &str, scope_id: &str) -> serde_json::Value {
        serde_json::json!({
            "permission": "membership:read_lodge",
            "scope_type": scope_type,
            "scope_id": scope_id,
        })
    }

    /// The rendered bind parameters of the last query containing `needle`.
    /// `SqlValue` deliberately has no `PartialEq`, so its `Debug` form is the
    /// house way to assert a bind.
    fn param_text(host: &adjutant_sdk::testing::TestHost, needle: &str) -> String {
        let params = host
            .db
            .last_query_params(needle)
            .unwrap_or_else(|| panic!("no query whose SQL contains {needle:?}"));
        params.iter().map(|p| format!("{p:?}")).collect::<Vec<_>>().join(" | ")
    }

    /// A lodge-scoped grant sees its own lodge's stewards and **not** another
    /// lodge's.
    ///
    /// `MockDb` replays rows in call order and never evaluates SQL, so the rows
    /// below are what PostgreSQL returns *after* the predicate is applied:
    /// `scopes_for` first, then the stewards query. The negative half of the
    /// assertion is therefore guarded by the bind check — a predicate weakened
    /// to "all" would bind `Bool(true)` / no lodge id and fail here.
    #[tokio::test]
    async fn stewards_are_filtered_to_the_callers_lodge() {
        let host = adjunct_sdk_test_host();
        host.db.push_rows(vec![held_at("lodge", "1")]); // scopes_for
        host.db.push_rows(vec![serde_json::json!({
            "id": 10, "position": "Steward-1", "appointed_on": "2026-01-01",
            "vacant": false, "display_name": "Bea", "trail_name": null,
            "lodge": "Lodge One",
        })]); // stewards, as the filtered query returns them

        let (status, body) = call_collection(
            &host,
            "/api/membership/stewards",
            grant("lodge_commander", Scope::lodge("1")),
        )
        .await;

        assert_eq!(status, 200);
        let rows = body["stewards"].as_array().expect("stewards");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["lodge"], serde_json::json!("Lodge One"));
        assert!(
            rows.iter().all(|r| r["lodge"] != serde_json::json!("Lodge Two")),
            "lodge 2's stewards must not be in the response: {rows:?}"
        );
        assert!(
            host.db.queried_sql().iter().any(|s| s.contains("s.lodge_id = ANY(")),
            "the stewards query must filter on the steward's lodge"
        );
        let binds = param_text(&host, "FROM stewards");
        assert!(binds.contains("Bool(false)"), "not a troop audience: {binds}");
        assert!(binds.contains("IntArray([1])"), "only lodge 1: {binds}");
    }

    /// The lodges list is narrowed the same way: one lodge grant names one
    /// lodge, and two grants bind both ids.
    #[tokio::test]
    async fn lodges_are_filtered_to_the_callers_lodges() {
        let host = adjunct_sdk_test_host();
        host.db.push_rows(vec![held_at("lodge", "1"), held_at("lodge", "2")]); // scopes_for
        host.db.push_rows(vec![
            serde_json::json!({ "id": 1, "name": "Lodge One", "patrols": [] }),
            serde_json::json!({ "id": 2, "name": "Lodge Two", "patrols": [] }),
        ]); // lodges, as the filtered query returns them

        let (status, body) = call_collection(
            &host,
            "/api/membership/lodges",
            Identity::from_grants(
                "bea",
                vec![
                    RoleGrant { role_id: "lodge_commander".into(), scope: Scope::lodge("1") },
                    RoleGrant { role_id: "lodge_commander".into(), scope: Scope::lodge("2") },
                ],
            ),
        )
        .await;

        assert_eq!(status, 200);
        assert_eq!(body["lodges"].as_array().expect("lodges").len(), 2);
        assert!(
            host.db.queried_sql().iter().any(|s| s.contains("l.id = ANY(")),
            "the lodges query must filter on the lodge id"
        );
        let binds = param_text(&host, "FROM lodges");
        assert!(binds.contains("Bool(false)"), "not a troop audience: {binds}");
        assert!(binds.contains("IntArray([1, 2])"), "both lodges: {binds}");

        // A caller granted only lodge 1 must not receive lodge 2's row. The
        // replay is what PostgreSQL returns with the predicate (`WHERE … l.id =
        // ANY($2)`), so the bind above is the guard that the predicate exists.
        let host = adjunct_sdk_test_host();
        host.db.push_rows(vec![held_at("lodge", "1")]);
        host.db
            .push_rows(vec![serde_json::json!({ "id": 1, "name": "Lodge One", "patrols": [] })]);
        let (status, body) = call_collection(
            &host,
            "/api/membership/lodges",
            grant("lodge_commander", Scope::lodge("1")),
        )
        .await;
        assert_eq!(status, 200);
        let rows = body["lodges"].as_array().expect("lodges");
        assert_eq!(rows.len(), 1);
        assert!(
            rows.iter().all(|r| r["name"] != serde_json::json!("Lodge Two")),
            "lodge 2 must not be in the response: {rows:?}"
        );
    }

    /// A troop-scoped grant keeps the old behaviour: no row filter at all.
    #[tokio::test]
    async fn a_troop_scope_grant_sees_every_steward_and_every_lodge() {
        let host = adjunct_sdk_test_host();
        host.db.push_rows(vec![held_at("troop", "")]); // scopes_for
        host.db.push_rows(vec![
            serde_json::json!({ "id": 10, "lodge": "Lodge One" }),
            serde_json::json!({ "id": 11, "lodge": "Lodge Two" }),
        ]);
        let (status, body) = call_collection(
            &host,
            "/api/membership/stewards",
            grant("chief", Scope::troop()),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["stewards"].as_array().expect("stewards").len(), 2);
        let binds = param_text(&host, "FROM stewards");
        assert!(binds.contains("Bool(true)"), "troop audience sees all: {binds}");

        let host = adjunct_sdk_test_host();
        host.db.push_rows(vec![held_at("troop", "")]);
        host.db.push_rows(vec![
            serde_json::json!({ "id": 1, "name": "Lodge One", "patrols": [] }),
            serde_json::json!({ "id": 2, "name": "Lodge Two", "patrols": [] }),
        ]);
        let (status, body) = call_collection(
            &host,
            "/api/membership/lodges",
            grant("chief", Scope::troop()),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["lodges"].as_array().expect("lodges").len(), 2);
        let binds = param_text(&host, "FROM lodges");
        assert!(binds.contains("Bool(true)"), "troop audience sees all: {binds}");
    }

    /// The proficiency catalogue has no lodge column, so it is troop curriculum:
    /// a troop grant sees it, a lodge-only grant addresses none of it, and the
    /// predicate fails closed instead of leaking the catalogue.
    #[tokio::test]
    async fn proficiencies_are_troop_curriculum_not_a_lodge_list() {
        let host = adjunct_sdk_test_host();
        host.db.push_rows(vec![held_at("troop", "")]); // scopes_for
        host.db
            .push_rows(vec![serde_json::json!({ "id": 1, "code": "T-1", "title": "Knots", "domain": "troop" })]);
        let (status, body) = call_collection(
            &host,
            "/api/membership/proficiencies",
            grant("chief", Scope::troop()),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["proficiencies"].as_array().expect("proficiencies").len(), 1);
        let binds = param_text(&host, "FROM proficiencies");
        assert!(binds.contains("Bool(true)"), "troop audience sees the catalogue: {binds}");

        let host = adjunct_sdk_test_host();
        host.db.push_rows(vec![held_at("lodge", "1")]); // scopes_for
        host.db.push_rows(vec![]); // `WHERE $1::bool` with false matches nothing
        let (status, body) = call_collection(
            &host,
            "/api/membership/proficiencies",
            grant("lodge_commander", Scope::lodge("1")),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["proficiencies"], serde_json::json!([]));
        let binds = param_text(&host, "FROM proficiencies");
        assert!(binds.contains("Bool(false)"), "lodge-only sees none: {binds}");
    }

    /// A grant that names no membership lodge opens nothing, and the handler
    /// refuses rather than returning an empty 200.
    #[tokio::test]
    async fn a_grant_with_no_usable_lodge_scope_is_refused() {
        // (identity, the scopes_for rows it resolves to)
        let cases = [
            // A patrol grant covers patrols, never the lodge that contains it.
            (grant("patrol_leader", Scope::patrol("3")), vec![held_at("patrol", "3")]),
            // A scope id that is not a membership lodge id names no row.
            (
                grant("lodge_commander", Scope::lodge("not-a-lodge-id")),
                vec![held_at("lodge", "not-a-lodge-id")],
            ),
            // No grants at all: scopes_for answers without a query.
            (Identity::from_grants("bea", vec![]), Vec::new()),
        ];
        for (identity, held) in cases {
            let host = adjunct_sdk_test_host();
            if !identity.grants.is_empty() {
                host.db.push_rows(held); // scopes_for
            }
            let (status, body) =
                call_collection(&host, "/api/membership/stewards", identity).await;
            assert_eq!(status, 403, "no usable scope must be refused: {body}");
        }
    }

    /// The hierarchy declaration replaces our own lodge→patrol edges from
    /// `patrols.lodge_id` (scoped DELETE, then the checked declare function).
    #[tokio::test]
    async fn declare_scope_hierarchy_syncs_from_patrols() {
        let host = adjutant_sdk::testing::TestHost::new();
        let ctx = host.context("membership");
        declare_scope_hierarchy(&ctx).await.unwrap();
        let sql = host.db.executed_sql();
        assert_eq!(sql.len(), 2);
        assert!(sql[0].contains("DELETE FROM core.scope_hierarchy"));
        assert!(sql[0].contains("'lodge'"));
        assert!(sql[1].contains("core.declare_scope_parent"));
        assert!(sql[1].contains("FROM patrols"));
        assert!(sql[1].contains("lodge_id"));
    }
}

#[async_trait]
impl AdjutantPlugin for MembershipPlugin {
    fn id(&self) -> &str {
        "membership"
    }
    fn name(&self) -> &str {
        "Membership"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
        // (Re)declare the lodge→patrol scope edges from our own roster on every
        // load, so the core's hierarchical coverage cannot drift from
        // `patrols.lodge_id`. On the very first boot the core calls `init`
        // before our migrations, so `patrols` may not exist yet: log and
        // continue — the first patrol write (and every reload) re-declares.
        if let Err(e) = declare_scope_hierarchy(&ctx).await {
            eprintln!("[adjutant-membership] scope hierarchy re-declare skipped: {e}");
        }
        let _ = self.ctx.set(ctx);
        Ok(())
    }

    fn permissions_granted(&self) -> Vec<Permission> {
        vec![
            Permission::new("membership:read", "View own profile"),
            Permission::new("membership:read_lodge", "View Lodge membership (Lodge Commander+)"),
            Permission::new("membership:read_all", "View all membership (Chief+)"),
            Permission::new("membership:manage", "Manage membership (Chief+)"),
        ]
    }

    fn migrations(&self) -> Vec<Migration> {
        vec![Migration::new(
            1,
            "roster_schema",
            "CREATE TABLE IF NOT EXISTS lodges (\
                 id BIGSERIAL PRIMARY KEY, \
                 name TEXT NOT NULL UNIQUE, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE TABLE IF NOT EXISTS patrols (\
                 id BIGSERIAL PRIMARY KEY, \
                 name TEXT NOT NULL UNIQUE, \
                 lodge_id BIGINT REFERENCES lodges(id) ON DELETE SET NULL, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE TABLE IF NOT EXISTS members (\
                 id BIGSERIAL PRIMARY KEY, \
                 username TEXT NOT NULL UNIQUE, \
                 email TEXT, \
                 display_name TEXT NOT NULL, \
                 trail_name TEXT, \
                 osg_id TEXT, \
                 bg_check TEXT, \
                 patrol_id BIGINT REFERENCES patrols(id) ON DELETE SET NULL, \
                 is_active BOOLEAN NOT NULL DEFAULT true, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 updated_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE INDEX IF NOT EXISTS idx_members_patrol ON members(patrol_id);\
             CREATE TABLE IF NOT EXISTS proficiencies (\
                 id BIGSERIAL PRIMARY KEY, \
                 code TEXT NOT NULL UNIQUE, \
                 title TEXT NOT NULL, \
                 domain TEXT NOT NULL DEFAULT 'troop', \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE TABLE IF NOT EXISTS member_proficiencies (\
                 member_id BIGINT NOT NULL REFERENCES members(id) ON DELETE CASCADE, \
                 proficiency_id BIGINT NOT NULL REFERENCES proficiencies(id) ON DELETE CASCADE, \
                 completed_on DATE NOT NULL DEFAULT current_date, \
                 signed_off_by TEXT, \
                 PRIMARY KEY (member_id, proficiency_id)\
             );\
             CREATE TABLE IF NOT EXISTS stewards (\
                 id BIGSERIAL PRIMARY KEY, \
                 member_id BIGINT NOT NULL REFERENCES members(id) ON DELETE CASCADE, \
                 position TEXT NOT NULL, \
                 lodge_id BIGINT REFERENCES lodges(id) ON DELETE SET NULL, \
                 appointed_on DATE NOT NULL DEFAULT current_date, \
                 vacant BOOLEAN NOT NULL DEFAULT false\
             );",
        )]
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        let ctx = self.ctx();

        // --- roster read (membership:read_all scopes to Chief) ---------------
        let c = ctx.clone();
        let list_members = RouteDefinition::get_protected(
            "/api/membership/members",
            "membership:read_all",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let patrol = req.query_param("patrol").map(String::from);
                    let show_inactive = req.query_param("include_inactive") == Some("1");
                    let rows = c
                        .db
                        .query(
                            "SELECT m.id, m.username, m.email, m.display_name, m.trail_name, \
                                    m.osg_id, m.bg_check, m.is_active, p.name AS patrol, l.name AS lodge \
                             FROM members m \
                             LEFT JOIN patrols p ON p.id = m.patrol_id \
                             LEFT JOIN lodges l ON l.id = p.lodge_id \
                             WHERE ($1::bool OR m.is_active) \
                               AND ($2::text IS NULL OR p.name = $2) \
                             ORDER BY m.display_name",
                            vec![
                                SqlValue::Bool(show_inactive),
                                patrol.map(SqlValue::Text).unwrap_or(SqlValue::Null),
                            ],
                        )
                        .await?;
                    PluginResponse::json(200, &serde_json::json!({ "members": rows }))
                }
            }),
        );

        // --- single member ----------------------------------------------------
        // Object route: the core gate requires `membership:read` at *some*
        // scope; the handler decides whether the caller may see this member.
        let c = ctx.clone();
        let get_member = RouteDefinition::get_protected_any_scope(
            "/api/membership/member",
            "membership:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let Some(id) = req.query_param("id").and_then(|v| v.parse::<i64>().ok()) else {
                        return PluginResponse::error(400, "id query parameter must be a number");
                    };
                    let identity = req.identity.as_ref();

                    // `read_all` is troop-wide: it reaches any member.
                    if c.permissions
                        .has_in_scope(identity, "membership:read_all", &Scope::troop())
                        .await
                    {
                        return match fetch_member(&c, id).await? {
                            Some(m) => {
                                PluginResponse::json(200, &serde_json::json!({ "member": m }))
                            }
                            None => PluginResponse::error(404, "no such member"),
                        };
                    }

                    // Resolve the caller's username for the self check. This is
                    // an equality test, not a scope.
                    let caller = match identity {
                        Some(ident) => {
                            let rows = c
                                .db
                                .query(
                                    "SELECT username FROM core.users WHERE id = $1::uuid",
                                    vec![SqlValue::Text(ident.user_id.clone())],
                                )
                                .await?;
                            rows.first()
                                .and_then(|r| r["username"].as_str())
                                .map(str::to_lowercase)
                        }
                        None => None,
                    };

                    // Fetch before deciding: `read_lodge` needs the target's
                    // lodge. A caller without `read_all` gets the same 403 for an
                    // absent member as for a forbidden one, so existence is not
                    // leaked.
                    let Some(member) = fetch_member(&c, id).await? else {
                        return PluginResponse::error(403, MEMBER_FORBIDDEN);
                    };

                    let is_self = match (&caller, member["username"].as_str()) {
                        (Some(cu), Some(mu)) => cu.eq_ignore_ascii_case(mu),
                        _ => false,
                    };
                    if is_self
                        && c.permissions
                            .has_in_scope(identity, "membership:read", &Scope::troop())
                            .await
                    {
                        return PluginResponse::json(
                            200,
                            &serde_json::json!({ "member": member }),
                        );
                    }

                    // A lodge-scoped `read_lodge` grant that covers the member's
                    // lodge. `core.user_roles.scope_id` became TEXT in core
                    // migration 5, so a lodge grant stores the bigint lodge id as
                    // text and this check can match.
                    if let Some(lodge_id) = member["lodge_id"].as_i64() {
                        if c.permissions
                            .has_in_scope(
                                identity,
                                "membership:read_lodge",
                                &Scope::lodge(lodge_id.to_string()),
                            )
                            .await
                        {
                            return PluginResponse::json(
                                200,
                                &serde_json::json!({ "member": member }),
                            );
                        }
                    }

                    PluginResponse::error(403, MEMBER_FORBIDDEN)
                }
            }),
        );

        // --- upsert member (OSG import path: osg_id + bg_check come along) ----
        let c = ctx.clone();
        let upsert_member = RouteDefinition::post_protected(
            "/api/membership/member",
            "membership:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    #[derive(Deserialize)]
                    struct Body {
                        username: String,
                        #[serde(default)]
                        email: Option<String>,
                        display_name: String,
                        #[serde(default)]
                        trail_name: Option<String>,
                        #[serde(default)]
                        osg_id: Option<String>,
                        #[serde(default)]
                        bg_check: Option<String>,
                        #[serde(default)]
                        patrol: Option<String>,
                        #[serde(default)]
                        is_active: Option<bool>,
                    }
                    let b: Body = req.json()?;
                    // Resolve/create the patrol by name (troop-local layer).
                    let patrol_id = match &b.patrol {
                        Some(name) if !name.trim().is_empty() => {
                            let rows = c
                                .db
                                .query(
                                    "INSERT INTO patrols (name) VALUES ($1) \
                                     ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name \
                                     RETURNING id",
                                    vec![SqlValue::Text(name.trim().to_string())],
                                )
                                .await?;
                            rows.first().and_then(|r| r["id"].as_i64())
                        }
                        _ => None,
                    };
                    let rows = c
                        .db
                        .query(
                            "INSERT INTO members \
                               (username, email, display_name, trail_name, osg_id, bg_check, \
                                patrol_id, is_active) \
                             VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8::boolean, true)) \
                             ON CONFLICT (username) DO UPDATE SET \
                               email = COALESCE(EXCLUDED.email, members.email), \
                               display_name = EXCLUDED.display_name, \
                               trail_name = COALESCE(EXCLUDED.trail_name, members.trail_name), \
                               osg_id = COALESCE(EXCLUDED.osg_id, members.osg_id), \
                               bg_check = COALESCE(EXCLUDED.bg_check, members.bg_check), \
                               patrol_id = COALESCE(EXCLUDED.patrol_id, members.patrol_id), \
                               is_active = COALESCE($8::boolean, members.is_active), \
                               updated_at = now() \
                             RETURNING id, (xmax = 0) AS inserted",
                            vec![
                                SqlValue::Text(b.username.clone()),
                                b.email.clone().into(),
                                SqlValue::Text(b.display_name.clone()),
                                b.trail_name.clone().into(),
                                b.osg_id.clone().into(),
                                b.bg_check.clone().into(),
                                patrol_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                                b.is_active.map(SqlValue::Bool).unwrap_or(SqlValue::NullBool),
                            ],
                        )
                        .await?;
                    let row = rows
                        .first()
                        .ok_or_else(|| SdkError::Internal("upsert returned no row".into()))?;
                    let inserted = row["inserted"] == serde_json::json!(true);
                    PluginResponse::json(
                        if inserted { 201 } else { 200 },
                        &serde_json::json!({
                            "id": row["id"],
                            "created": inserted,
                            "username": b.username,
                        }),
                    )
                }
            }),
        );

        // --- OSG CSV import ----------------------------------------------------
        let c = ctx.clone();
        let import = RouteDefinition::post_protected(
            "/api/membership/import",
            "membership:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let csv = String::from_utf8_lossy(&req.body).into_owned();
                    let table = parse_csv(&csv)
                        .map_err(|line| SdkError::BadRequest(format!("unterminated quote near line {line}")))?;
                    if table.len() < 2 {
                        return PluginResponse::error(
                            400,
                            "CSV needs a header row plus at least one data row",
                        );
                    }
                    let header = table[0].clone();
                    let i_user = col(&header, "username")
                        .or_else(|| col(&header, "email"))
                        .ok_or_else(|| SdkError::BadRequest("CSV needs a username or email column".into()))?;
                    let i_disp = col(&header, "display_name");
                    let i_trail = col(&header, "trail_name");
                    let i_patrol = col(&header, "patrol");
                    let i_osg = col(&header, "osg_id");
                    let i_bg = col(&header, "bg_check");
                    let i_roles = col(&header, "roles");

                    let mut report = ImportReport {
                        created: 0,
                        updated: 0,
                        skipped: Vec::new(),
                        patrols_created: Vec::new(),
                        roles_ignored: Vec::new(),
                    };

                    for (n, row) in table.iter().skip(1).enumerate() {
                        let lineno = n + 2;
                        let get = |idx: Option<usize>| -> Option<String> {
                            idx.and_then(|i| row.get(i))
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                        };
                        let Some(username) = get(Some(i_user)).filter(|s| !s.contains('@')) else {
                            // username column held an email, or the cell was blank
                            match get(Some(i_user)) {
                                Some(email) if email.contains('@') => {
                                    // synthesize a stable username from the email
                                    let synth = email.split('@').next().unwrap_or("member").to_string();
                                    let email = Some(email);
                                    upsert_from_import(
                                        &c,
                                        &synth,
                                        email,
                                            &get(i_disp).unwrap_or_else(|| synth.to_string()),
                                            get(i_trail),
                                            get(i_osg),
                                            get(i_bg),
                                            get(i_patrol),
                                            i_patrol.is_some(),
                                            &mut report,
                                            lineno,
                                        )
                                        .await?;
                                    continue;
                                }
                                _ => {
                                    report
                                        .skipped
                                        .push((lineno, "missing username/email".into()));
                                    continue;
                                }
                            }
                        };
                        let display = get(i_disp).unwrap_or_else(|| username.clone());
                        upsert_from_import(
                            &c,
                            &username,
                            None,
                                &display,
                                get(i_trail),
                                get(i_osg),
                                get(i_bg),
                                get(i_patrol),
                                i_patrol.is_some(),
                                &mut report,
                                lineno,
                            )
                            .await?;

                        // `roles` is deliberately ignored: role assignment is
                        // auth's domain (`auth:manage_users`, POST /api/auth/roles).
                        // Membership previously wrote core.user_roles here, which
                        // made `membership:manage` a path to granting `chief`
                        // (#19). Report it so the operator sees the column did
                        // nothing rather than assuming it worked.
                        if let Some(roles_col) = i_roles {
                            if let Some(roles) = get(Some(roles_col)) {
                                let names: Vec<String> = roles
                                    .split(';')
                                    .map(str::trim)
                                    .filter(|r| !r.is_empty())
                                    .map(String::from)
                                    .collect();
                                if !names.is_empty() {
                                    report.roles_ignored.push(RoleIgnored {
                                        line: lineno,
                                        roles: names,
                                        reason: "role assignment belongs to auth; \
                                                 use POST /api/auth/roles"
                                            .into(),
                                    });
                                }
                            }
                        }
                    }

                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "membership.import",
                            "roster",
                            "csv",
                            serde_json::json!({
                                "created": report.created,
                                "updated": report.updated,
                                "skipped": report.skipped.len(),
                            }),
                        )
                        .await?;
                    PluginResponse::json(200, &report)
                }
            }),
        );

        // --- lodges ------------------------------------------------------------
        // Reference data gated at *some* scope, then narrowed in the handler to
        // the lodges the caller's `membership:read_lodge` grant actually names:
        // a lodge commander sees their lodge, a troop-scoped grant sees all.
        // The core gate alone cannot make that distinction (it only knows the
        // permission, not the row), so the predicate is bound below.
        let c = ctx.clone();
        let list_lodges = RouteDefinition::get_protected_any_scope(
            "/api/membership/lodges",
            "membership:read_lodge",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let audience = lodge_audience(&c, &req).await?;
                    let rows = c
                        .db
                        .query(
                            "SELECT l.id, l.name, \
                                    COALESCE((SELECT json_agg(json_build_object(\
                                        'id', p.id, 'name', p.name\
                                     ) ORDER BY p.name) FROM patrols p \
                                       WHERE p.lodge_id = l.id), '[]') AS patrols \
                             FROM lodges l \
                             WHERE ($1::bool OR l.id = ANY($2::int8[])) \
                             ORDER BY l.name",
                            audience.params(),
                        )
                        .await?;
                    PluginResponse::json(200, &serde_json::json!({ "lodges": rows }))
                }
            }),
        );

        let c = ctx.clone();
        let create_lodge = RouteDefinition::post_protected(
            "/api/membership/lodge",
            "membership:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    #[derive(Deserialize)]
                    struct Body {
                        name: String,
                    }
                    let b: Body = req.json()?;
                    let rows = c
                        .db
                        .query(
                            "INSERT INTO lodges (name) VALUES ($1) \
                             ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name \
                             RETURNING id, name",
                            vec![SqlValue::Text(b.name.trim().to_string())],
                        )
                        .await?;
                    let r = rows.first().unwrap();
                    PluginResponse::json(
                        201,
                        &serde_json::json!({ "id": r["id"], "name": r["name"] }),
                    )
                }
            }),
        );

        // --- patrols -------------------------------------------------------------
        let c = ctx.clone();
        let create_patrol = RouteDefinition::post_protected(
            "/api/membership/patrol",
            "membership:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    #[derive(Deserialize)]
                    struct Body {
                        name: String,
                        #[serde(default)]
                        lodge: Option<String>,
                    }
                    let b: Body = req.json()?;
                    let lodge_id = match &b.lodge {
                        Some(name) if !name.trim().is_empty() => {
                            let rows = c
                                .db
                                .query(
                                    "INSERT INTO lodges (name) VALUES ($1) \
                                     ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name \
                                     RETURNING id",
                                    vec![SqlValue::Text(name.trim().to_string())],
                                )
                                .await?;
                            rows.first().and_then(|r| r["id"].as_i64())
                        }
                        _ => None,
                    };
                    let rows = c
                        .db
                        .query(
                            "INSERT INTO patrols (name, lodge_id) VALUES ($1, $2) \
                             ON CONFLICT (name) DO UPDATE SET lodge_id = COALESCE(EXCLUDED.lodge_id, patrols.lodge_id) \
                             RETURNING id, name",
                            vec![
                                SqlValue::Text(b.name.trim().to_string()),
                                lodge_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            ],
                        )
                        .await?;
                    // Keep the declared hierarchy fresh at runtime too, so a
                    // patrol created/moved via the API is covered before the
                    // next reload.
                    if let Err(e) = declare_scope_hierarchy(&c).await {
                        eprintln!("[adjutant-membership] scope hierarchy re-declare failed after patrol write: {e}");
                    }
                    match rows.first() {
                        Some(r) => PluginResponse::json(
                            201,
                            &serde_json::json!({ "id": r["id"], "name": r["name"] }),
                        ),
                        None => PluginResponse::error(500, "insert returned no row"),
                    }
                }
            }),
        );

        // --- proficiencies ----------------------------------------------------------
        // The proficiency catalogue carries no lodge: it is troop curriculum, not
        // a per-lodge list. So a troop-scoped `read_lodge` grant sees it and a
        // lodge-only grant addresses none of it — the `$1` flag is the whole
        // filter, and it fails closed rather than exposing troop-wide rows to a
        // lodge-scoped caller.
        let c = ctx.clone();
        let list_proficiencies = RouteDefinition::get_protected_any_scope(
            "/api/membership/proficiencies",
            "membership:read_lodge",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let audience = lodge_audience(&c, &req).await?;
                    let rows = c
                        .db
                        .query(
                            "SELECT id, code, title, domain FROM proficiencies \
                             WHERE $1::bool ORDER BY domain, code",
                            vec![SqlValue::Bool(audience.is_troop())],
                        )
                        .await?;
                    PluginResponse::json(200, &serde_json::json!({ "proficiencies": rows }))
                }
            }),
        );

        let c = ctx.clone();
        let create_proficiency = RouteDefinition::post_protected(
            "/api/membership/proficiency",
            "membership:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    #[derive(Deserialize)]
                    struct Body {
                        code: String,
                        title: String,
                        #[serde(default)]
                        domain: Option<String>,
                    }
                    let b: Body = req.json()?;
                    let rows = c
                        .db
                        .query(
                            "INSERT INTO proficiencies (code, title, domain) \
                             VALUES ($1, $2, COALESCE($3, 'troop')) \
                             ON CONFLICT (code) DO UPDATE SET \
                               title = EXCLUDED.title, domain = EXCLUDED.domain \
                             RETURNING id, code, title",
                            vec![
                                SqlValue::Text(b.code.trim().to_uppercase()),
                                SqlValue::Text(b.title.trim().to_string()),
                                b.domain.clone().into(),
                            ],
                        )
                        .await?;
                    let r = rows.first().unwrap();
                    PluginResponse::json(
                        201,
                        &serde_json::json!({ "id": r["id"], "code": r["code"], "title": r["title"] }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let complete_proficiency = RouteDefinition::post_protected(
            "/api/membership/proficiency/complete",
            "membership:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    #[derive(Deserialize)]
                    struct Body {
                        member_id: i64,
                        proficiency_id: i64,
                        #[serde(default)]
                        signed_off_by: Option<String>,
                    }
                    let b: Body = req.json()?;
                    let n = c
                        .db
                        .execute(
                            "INSERT INTO member_proficiencies (member_id, proficiency_id, signed_off_by) \
                             VALUES ($1, $2, $3) \
                             ON CONFLICT (member_id, proficiency_id) DO UPDATE \
                               SET signed_off_by = COALESCE(EXCLUDED.signed_off_by, \
                                                            member_proficiencies.signed_off_by)",
                            vec![
                                SqlValue::Int(b.member_id),
                                SqlValue::Int(b.proficiency_id),
                                b.signed_off_by.clone().into(),
                            ],
                        )
                        .await?;
                    let _ = n;
                    PluginResponse::json(
                        201,
                        &serde_json::json!({
                            "member_id": b.member_id,
                            "proficiency_id": b.proficiency_id,
                        }),
                    )
                }
            }),
        );

        // --- stewards (leadership positions) ------------------------------------------
        let c = ctx.clone();
        let appoint = RouteDefinition::post_protected(
            "/api/membership/steward",
            "membership:manage",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    #[derive(Deserialize)]
                    struct Body {
                        member_id: i64,
                        position: String,
                        #[serde(default)]
                        lodge: Option<String>,
                    }
                    let b: Body = req.json()?;
                    let lodge_id = match &b.lodge {
                        Some(name) if !name.trim().is_empty() => {
                            let rows = c
                                .db
                                .query(
                                    "INSERT INTO lodges (name) VALUES ($1) \
                                     ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name \
                                     RETURNING id",
                                    vec![SqlValue::Text(name.trim().to_string())],
                                )
                                .await?;
                            rows.first().and_then(|r| r["id"].as_i64())
                        }
                        _ => None,
                    };
                    let rows = c
                        .db
                        .query(
                            "INSERT INTO stewards (member_id, position, lodge_id) \
                             VALUES ($1, $2, $3) RETURNING id",
                            vec![
                                SqlValue::Int(b.member_id),
                                SqlValue::Text(b.position.trim().to_string()),
                                lodge_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                            ],
                        )
                        .await?;
                    let r = rows.first().unwrap();
                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "steward.appoint",
                            "member",
                            &b.member_id.to_string(),
                            serde_json::json!({ "position": b.position }),
                        )
                        .await?;
                    PluginResponse::json(
                        201,
                        &serde_json::json!({ "id": r["id"], "position": b.position }),
                    )
                }
            }),
        );

        let c = ctx.clone();
        let stewards = RouteDefinition::get_protected_any_scope(
            "/api/membership/stewards",
            "membership:read_lodge",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let audience = lodge_audience(&c, &req).await?;
                    let rows = c
                        .db
                        .query(
                            "SELECT s.id, s.position, s.appointed_on, s.vacant, \
                                    m.display_name, m.trail_name, l.name AS lodge \
                             FROM stewards s \
                             JOIN members m ON m.id = s.member_id \
                             LEFT JOIN lodges l ON l.id = s.lodge_id \
                             WHERE ($1::bool OR s.lodge_id = ANY($2::int8[])) \
                             ORDER BY s.vacant, s.position",
                            audience.params(),
                        )
                        .await?;
                    PluginResponse::json(200, &serde_json::json!({ "stewards": rows }))
                }
            }),
        );

        vec![
            list_members,
            get_member,
            upsert_member,
            import,
            list_lodges,
            create_lodge,
            create_patrol,
            list_proficiencies,
            create_proficiency,
            complete_proficiency,
            appoint,
            stewards,
        ]
    }
}

/// Shared upsert used by the CSV importer. One arg per recognized CSV column
/// (SPEC §7.2 roster fields) — grouping them would just rename the fields.
#[allow(clippy::too_many_arguments)]
async fn upsert_from_import(
    c: &PluginContext,
    username: &str,
    email: Option<String>,
    display_name: &str,
    trail_name: Option<String>,
    osg_id: Option<String>,
    bg_check: Option<String>,
    patrol: Option<String>,
    _patrol_col_present: bool,
        report: &mut ImportReport,
        lineno: usize,
    ) -> Result<(), SdkError> {
        let patrol_id = match &patrol {
            Some(name) if !name.trim().is_empty() => {
                let name = name.trim().to_string();
                let existed: i64 = c
                    .db
                    .query(
                        "SELECT COUNT(*) AS n FROM patrols WHERE name = $1",
                        vec![SqlValue::Text(name.clone())],
                    )
                    .await?
                    .first()
                    .and_then(|r| r["n"].as_i64())
                    .unwrap_or(0);
                if existed == 0 && !report.patrols_created.contains(&name) {
                    report.patrols_created.push(name.clone());
                }
                let rows = c
                    .db
                    .query(
                        "INSERT INTO patrols (name) VALUES ($1) \
                         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name RETURNING id",
                        vec![SqlValue::Text(name)],
                    )
                    .await?;
                rows.first().and_then(|r| r["id"].as_i64())
            }
            _ => None,
        };

        let rows = c
            .db
            .query(
                "INSERT INTO members (username, email, display_name, trail_name, osg_id, bg_check, patrol_id) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT (username) DO UPDATE SET \
                   email = COALESCE(EXCLUDED.email, members.email), \
                   display_name = EXCLUDED.display_name, \
                   trail_name = COALESCE(EXCLUDED.trail_name, members.trail_name), \
                   osg_id = COALESCE(EXCLUDED.osg_id, members.osg_id), \
                   bg_check = COALESCE(EXCLUDED.bg_check, members.bg_check), \
                   patrol_id = COALESCE(EXCLUDED.patrol_id, members.patrol_id), \
                   updated_at = now() \
                 RETURNING (xmax = 0) AS inserted",
                vec![
                    SqlValue::Text(username.to_string()),
                    email.into(),
                    SqlValue::Text(display_name.to_string()),
                    trail_name.into(),
                    osg_id.into(),
                    bg_check.into(),
                    patrol_id.map(SqlValue::Int).unwrap_or(SqlValue::NullInt),
                ],
            )
            .await?;
        let inserted = rows
            .first()
            .map(|r| r["inserted"] == serde_json::json!(true))
            .unwrap_or(false);
        if inserted {
            report.created += 1;
        } else {
            report.updated += 1;
        }
        let _ = lineno;
        Ok(())
    }

export_plugin!(MembershipPlugin);

// ---------------------------------------------------------------------------
// Collection visibility (#146)
// ---------------------------------------------------------------------------

/// Which lodges a `membership:read_lodge` holder may see on the collection
/// routes.
///
/// Those routes are gated `any_scope`, so the core gate proves only that the
/// caller holds the permission *somewhere*. `docs/design/scoped-permissions.md`
/// §2 makes the handler responsible for the row filter: a troop-covering grant
/// reaches every lodge, a lodge-scoped one reaches exactly its lodges. This is
/// the same split announcements draws with [`PermissionService::scopes_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum LodgeAudience {
    /// A troop-scoped grant: every lodge row.
    Troop,
    /// Lodge-scoped grants: exactly these membership lodge ids.
    Lodges(Vec<i64>),
}

impl LodgeAudience {
    /// Holds the permission at troop scope, so no row filter applies.
    fn is_troop(&self) -> bool {
        matches!(self, Self::Troop)
    }

    /// The `$1` (all lodges) / `$2` (`int8[]` of lodge ids) bind pair every
    /// filtered collection query takes. A troop audience still binds an empty
    /// array: the boolean is the whole predicate, so the filter cannot leak
    /// through an unbound parameter.
    fn params(&self) -> Vec<SqlValue> {
        match self {
            Self::Troop => vec![SqlValue::Bool(true), SqlValue::IntArray(Vec::new())],
            Self::Lodges(ids) => vec![SqlValue::Bool(false), SqlValue::IntArray(ids.clone())],
        }
    }
}

/// Resolve the caller's `membership:read_lodge` audience in **one** query, or
/// refuse when the grant covers no lodge at all.
///
/// `core.user_roles.scope_id` and `core.scope_hierarchy.scope_id` are `TEXT`
/// (core migration 5), while membership's `lodges.id` is `BIGINT`, so the two
/// are mapped here explicitly. A scope id that does not parse as a lodge id
/// names no row in this plugin and is dropped — fail closed, never widened.
///
/// A caller holding nothing usable is refused with `403`. The core gate already
/// refuses that caller on these routes; refusing again here keeps the handler
/// honest when it is exercised directly (tests) and if the gate ever changes.
async fn lodge_audience(
    c: &PluginContext,
    req: &PluginRequest,
) -> Result<LodgeAudience, SdkError> {
    let held = c
        .permissions
        .scopes_for(req.identity.as_ref(), &["membership:read_lodge"])
        .await?;
    let mut lodges = Vec::new();
    for entry in held {
        match entry.scope.scope_type {
            ScopeType::Troop => return Ok(LodgeAudience::Troop),
            ScopeType::Lodge => {
                if let Some(id) = entry.scope.scope_id.as_deref() {
                    if let Ok(id) = id.parse::<i64>() {
                        lodges.push(id);
                    }
                }
            }
            // A patrol grant covers patrols inside its lodge, never the lodge
            // itself (coverage is downward only), so it opens no collection row.
            ScopeType::Patrol => {}
        }
    }
    if lodges.is_empty() {
        return Err(SdkError::Forbidden(
            "membership:read_lodge is held at no lodge scope".into(),
        ));
    }
    lodges.sort_unstable();
    lodges.dedup();
    Ok(LodgeAudience::Lodges(lodges))
}

/// Replace membership's declared lodge → patrol edges in
/// `core.scope_hierarchy` from its own roster (`patrols.lodge_id`).
///
/// The core resolves hierarchical coverage from that table; membership owns the
/// `lodge` and `patrol` scope types (see `SCOPE_OWNERS` in
/// `server/src/schema.rs`), so it may declare these edges. Edges go through
/// `core.declare_scope_parent`, which refuses a type the caller does not own with
/// a clear message (a trigger enforces the same rule for raw SQL). The DELETE is
/// scoped to our own edge kind, so a re-run cannot duplicate or disturb another
/// plugin's edges. Called on every load and after a patrol write; a first-boot
/// call before migrations returns `Err` (no `patrols` table yet) and the caller
/// logs and continues.
async fn declare_scope_hierarchy(ctx: &PluginContext) -> Result<(), SdkError> {
    ctx.db
        .execute(
            "DELETE FROM core.scope_hierarchy \
             WHERE parent_type = 'lodge' AND child_type = 'patrol'"
                .to_string(),
            vec![],
        )
        .await?;
    ctx.db
        .execute(
            "SELECT core.declare_scope_parent('lodge', lodge_id::text, 'patrol', id::text) \
             FROM patrols WHERE lodge_id IS NOT NULL"
                .to_string(),
            vec![],
        )
        .await?;
    Ok(())
}
