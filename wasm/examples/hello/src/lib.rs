//! # adjutant-hello-wasm
//!
//! The sandboxed counterpart of `plugins/examples/hello`. Same shape — an open
//! route, a protected read, a protected write that publishes an event and writes
//! an audit row — but every host service goes through
//! `adjutant_host_call` over the WASM boundary. Build with:
//!
//! ```bash
//! cargo build --manifest-path wasm/Cargo.toml --release --target wasm32-wasip1
//! ```

use adjutant_wasm_guest::{export_wasm_plugin, host_call, WasmPlugin};
use serde_json::{json, Value};

struct HelloWasm;

fn json_status(status: u16, body: Value) -> Value {
    json!({
        "status": status,
        "headers": [["content-type", "application/json"]],
        "body": body.to_string(),
    })
}

fn json_err(status: u16, message: &str) -> Value {
    json_status(status, json!({ "error": message }))
}

impl WasmPlugin for HelloWasm {
    fn manifest() -> Value {
        json!({
            "id": "hello_wasm",
            "name": "Hello (WASM)",
            "version": "0.1.0",
            "permissions": [
                { "id": "hello_wasm:read", "description": "Read greetings" },
                { "id": "hello_wasm:write", "description": "Create greetings" }
            ],
            "migrations": [{
                "version": 1,
                "name": "create_greetings",
                "sql": "CREATE TABLE IF NOT EXISTS greetings (\
                            id BIGSERIAL PRIMARY KEY, \
                            message TEXT NOT NULL, \
                            created_at TIMESTAMPTZ NOT NULL DEFAULT now()\
                        );"
            }],
            "routes": [
                { "method": "GET", "path": "/api/hello_wasm" },
                { "method": "GET", "path": "/api/hello_wasm/greetings",
                  "permission": "hello_wasm:read" },
                { "method": "POST", "path": "/api/hello_wasm/greet",
                  "permission": "hello_wasm:write" }
            ]
        })
    }

    fn handle(req: &Value) -> Value {
        match req["path"].as_str().unwrap_or("") {
            "/api/hello_wasm" => json_status(
                200,
                json!({ "message": "Hello from WASM!", "sandboxed": true }),
            ),

            "/api/hello_wasm/greetings" => {
                let call = json!({
                    "sql": "SELECT id, message, created_at::text AS created_at \
                            FROM greetings ORDER BY id DESC LIMIT 50",
                    "params": []
                });
                match host_call("db.query", &call.to_string()) {
                    Ok(data) => json_status(200, json!({ "greetings": data["rows"] })),
                    Err(e) => json_err(500, &e),
                }
            }

            "/api/hello_wasm/greet" => {
                let body: Value =
                    serde_json::from_str(req["body"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
                let message = body["message"].as_str().unwrap_or("hello").to_string();

                let insert = json!({
                    "sql": "INSERT INTO greetings (message) VALUES ($1)",
                    "params": [{ "kind": "text", "value": message.clone() }]
                });
                if let Err(e) = host_call("db.execute", &insert.to_string()) {
                    return json_err(500, &e);
                }
                let _ = host_call(
                    "events.publish",
                    &json!({
                        "event_type": "hello_wasm.greeted",
                        "payload": { "message": message.clone() }
                    })
                    .to_string(),
                );
                let _ = host_call(
                    "audit.log",
                    &json!({
                        "action": "greet",
                        "resource_type": "greeting",
                        "resource_id": message,
                        "details": {}
                    })
                    .to_string(),
                );
                json_status(201, json!({ "ok": true, "message": message }))
            }

            _ => json_err(404, "no such route"),
        }
    }
}

export_wasm_plugin!(HelloWasm);
