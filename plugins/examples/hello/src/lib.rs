//! # adjutant-hello — the Milestone 1 prototype plugin
//!
//! Proves every SDK surface end to end (SPEC §15, Milestone 1 exit criteria):
//!
//! - **routes**: `GET /api/hello` (open), `GET /api/hello/greetings`
//!   (`hello:read`), `POST /api/hello/greet` (`hello:write`)
//! - **migrations**: creates `hello.greetings` + `hello.events_received`
//! - **permissions**: declares `hello:read` / `hello:write`
//! - **database**: reads/writes through the core-provided pool
//! - **events**: publishes `hello.greeted`, subscribes to `hello.` and records
//!   delivery — proving both halves of the bus

use std::sync::OnceLock;

use adjutant_sdk::prelude::*;

pub struct HelloPlugin {
    ctx: OnceLock<PluginContext>,
}

impl HelloPlugin {
    pub fn new() -> Self {
        Self { ctx: OnceLock::new() }
    }

    fn ctx(&self) -> &PluginContext {
        self.ctx.get().expect("core must call init() before routes()/subscriptions()")
    }
}

impl Default for HelloPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AdjutantPlugin for HelloPlugin {
    fn id(&self) -> &str {
        "hello"
    }

    fn name(&self) -> &str {
        "Hello World"
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
            Permission::new("hello:read", "Read greetings"),
            Permission::new("hello:write", "Create greetings"),
        ]
    }

    fn migrations(&self) -> Vec<Migration> {
        vec![Migration::new(
            1,
            "create_greetings",
            "CREATE TABLE IF NOT EXISTS greetings (\
                 id BIGSERIAL PRIMARY KEY, \
                 message TEXT NOT NULL, \
                 created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );\
             CREATE TABLE IF NOT EXISTS events_received (\
                 id BIGSERIAL PRIMARY KEY, \
                 event_type TEXT NOT NULL, \
                 payload TEXT, \
                 received_at TIMESTAMPTZ NOT NULL DEFAULT now()\
             );",
        )]
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        let ctx = self.ctx();

        // 1. Open route — proves route registration + dispatch, no permission.
        let hello = RouteDefinition::get(
            "/api/hello",
            route_handler(|_req| async {
                PluginResponse::json(200, &serde_json::json!({
                    "message": "Hello, Adjutant!",
                    "service": "adjutant",
                }))
            }),
        );

        // 2. Protected read — proves host-mediated DB + migrations + permission gate.
        let c = ctx.clone();
        let list = RouteDefinition::get_protected(
            "/api/hello/greetings",
            "hello:read",
            route_handler(move |_req| {
                let c = c.clone();
                async move {
                    let greetings = c
                        .db
                        .query(
                            format!(
                                "SELECT id, message, created_at::text AS created_at FROM {} ORDER BY id DESC LIMIT 50",
                                c.db.table("greetings")
                            ),
                            vec![],
                        )
                        .await?;
                    PluginResponse::json(200, &serde_json::json!({ "greetings": greetings }))
                }
            }),
        );

        // 3. Protected write — proves insert + event publish + permission gate.
        let c = ctx.clone();
        let greet = RouteDefinition::post_protected(
            "/api/hello/greet",
            "hello:write",
            route_handler(move |req| {
                let c = c.clone();
                async move {
                    #[derive(serde::Deserialize)]
                    struct Body {
                        #[serde(default = "default_message")]
                        message: String,
                    }
                    fn default_message() -> String {
                        "hello".to_string()
                    }

                    let body: Body = req.json()?;
                    c.db.execute(
                        format!("INSERT INTO {} (message) VALUES ($1)", c.db.table("greetings")),
                        vec![SqlValue::Text(body.message.clone())],
                    )
                    .await?;

                    c.events
                        .publish(
                            "hello.greeted",
                            serde_json::json!({
                                "message": body.message,
                                "by": req.identity.as_ref().map(|i| i.user_id.clone()),
                            }),
                        )
                        .await?;

                    c.audit
                        .log(
                            req.identity.as_ref(),
                            "greet",
                            "greeting",
                            &body.message,
                            serde_json::json!({ "message": body.message }),
                        )
                        .await?;

                    PluginResponse::json(201, &serde_json::json!({ "ok": true, "message": body.message }))
                }
            }),
        );

        vec![hello, list, greet]
    }

    fn subscriptions(&self) -> Vec<EventSubscription> {
        let ctx = self.ctx().clone(); // owned: closure must not borrow self
        vec![EventSubscription::new(
            "hello.",
            event_handler(move |ev| {
                let c = ctx.clone();
                async move {
                    c.db.execute(
                        format!(
                            "INSERT INTO {} (event_type, payload) VALUES ($1, $2)",
                            c.db.table("events_received")
                        ),
                        vec![
                            SqlValue::Text(ev.event_type.clone()),
                            SqlValue::Text(ev.payload.to_string()),
                        ],
                    )
                    .await?;
                    Ok(())
                }
            }),
        )]
    }
}

export_plugin!(HelloPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_identity_is_stable() {
        let p = HelloPlugin::new();
        assert_eq!(p.id(), "hello");
        assert_eq!(p.name(), "Hello World");
    }

    #[test]
    fn migrations_are_versioned_and_namespaced() {
        let p = HelloPlugin::new();
        let m = p.migrations();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].version, 1);
        assert!(m[0].sql.contains("greetings"));
    }

    #[test]
    fn permissions_declare_read_and_write() {
        let p = HelloPlugin::new();
        let ids: Vec<String> = p.permissions_granted().iter().map(|x| x.id.clone()).collect();
        assert_eq!(ids, vec!["hello:read".to_string(), "hello:write".to_string()]);
    }

    #[test]
    fn entry_symbol_is_exported() {
        // The core resolves this symbol via libloading; a name typo here would
        // only show up at load time, so pin it with a direct call.
        let raw = adjutant_plugin_create();
        assert!(!raw.is_null());
        let boxed = unsafe { Box::from_raw(raw) };
        assert_eq!(boxed.id(), "hello");
    }
}
