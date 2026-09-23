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
/// `roles` (semicolon-separated), `osg_id`, `bg_check`.
/// Rows missing `username` are skipped with a reason.
#[derive(Debug, serde::Serialize)]
pub struct ImportReport {
    pub created: usize,
    pub updated: usize,
    pub skipped: Vec<(usize, String)>,
    pub patrols_created: Vec<String>,
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
        let c = ctx.clone();
        let get_member = RouteDefinition::get_protected(
            "/api/membership/member",
            "membership:read",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    let Some(id) = req.query_param("id").and_then(|v| v.parse::<i64>().ok()) else {
                        return PluginResponse::error(400, "id query parameter must be a number");
                    };
                    let rows = c
                        .db
                        .query(
                            "SELECT m.id, m.username, m.email, m.display_name, m.trail_name, \
                                    m.osg_id, m.bg_check, m.is_active, p.name AS patrol, l.name AS lodge, \
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
                             WHERE m.id = $1",
                            vec![SqlValue::Int(id)],
                        )
                        .await?;
                    match rows.into_iter().next() {
                        Some(m) => PluginResponse::json(200, &serde_json::json!({ "member": m })),
                        None => PluginResponse::error(404, "no such member"),
                    }
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

                        // roles: semicolon-separated → core.user_roles if the
                        // user already exists in core.users (auth owns that).
                        if let (Some(_), Some(roles_col)) = (i_roles, i_roles) {
                            if let Some(roles) = get(Some(roles_col)) {
                                let uid_rows = c
                                    .db
                                    .query(
                                        "SELECT id::text AS id FROM core.users \
                                         WHERE lower(username) = lower($1)",
                                        vec![SqlValue::Text(username.clone())],
                                    )
                                    .await?;
                                if let Some(uid) = uid_rows.first().and_then(|r| r["id"].as_str()) {
                                    let uid = uid.to_string();
                                    for role in roles.split(';').map(str::trim).filter(|r| !r.is_empty()) {
                                        c.db
                                            .execute(
                                                "INSERT INTO core.user_roles (user_id, role_id) \
                                                 VALUES ($1::uuid, $2) ON CONFLICT DO NOTHING",
                                                vec![
                                                    SqlValue::Text(uid.clone()),
                                                    SqlValue::Text(role.to_string()),
                                                ],
                                            )
                                            .await?;
                                    }
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
        let c = ctx.clone();
        let list_lodges = RouteDefinition::get_protected(
            "/api/membership/lodges",
            "membership:read",
            route_handler(move |_req| {
                let c = c.clone();
                async move {
                    let rows = c
                        .db
                        .query(
                            "SELECT l.id, l.name, \
                                    COALESCE((SELECT json_agg(json_build_object(\
                                        'id', p.id, 'name', p.name\
                                     ) ORDER BY p.name) FROM patrols p \
                                       WHERE p.lodge_id = l.id), '[]') AS patrols \
                             FROM lodges l ORDER BY l.name",
                            vec![],
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
        let c = ctx.clone();
        let list_proficiencies = RouteDefinition::get_protected(
            "/api/membership/proficiencies",
            "membership:read",
            route_handler(move |_req| {
                let c = c.clone();
                async move {
                    let rows = c
                        .db
                        .query(
                            "SELECT id, code, title, domain FROM proficiencies ORDER BY domain, code",
                            vec![],
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
        let stewards = RouteDefinition::get_protected(
            "/api/membership/stewards",
            "membership:read",
            route_handler(move |_req| {
                let c = c.clone();
                async move {
                    let rows = c
                        .db
                        .query(
                            "SELECT s.id, s.position, s.appointed_on, s.vacant, \
                                    m.display_name, m.trail_name, l.name AS lodge \
                             FROM stewards s \
                             JOIN members m ON m.id = s.member_id \
                             LEFT JOIN lodges l ON l.id = s.lodge_id \
                             ORDER BY s.vacant, s.position",
                            vec![],
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
