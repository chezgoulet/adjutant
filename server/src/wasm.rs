//! WASM plugin host (SPEC §14-R1, Milestone 4 W3).
//!
//! Native plugins are trusted `cdylib`s sharing the core's address space. WASM
//! plugins run sandboxed in `wasmtime`: no filesystem, no network, a memory cap,
//! and a fuel budget that traps a runaway loop. The guest reuses the *same*
//! host-mediated I/O boundary as native code (SPEC §5.2a): instead of linkable
//! trait objects, a single imported function `adjutant_host_call` carries every
//! DB/event/HTTP/permission/audit operation as JSON.
//!
//! ## Sandbox
//!
//! WASI preview1 is linked (Rust `std` needs it) but **no directories are
//! preopened** and preview1 has no socket API, so a guest cannot reach the
//! filesystem or the network. On top of that: a 64 MiB linear-memory cap and a
//! fuel budget per entry-point call. See the tests at the bottom.
//!
//! The host wraps a guest in [`WasmPlugin`], which implements the ordinary
//! [`AdjutantPlugin`] trait. The registry, validation, permission gate, and
//! dispatch path are unchanged — WASM is just another plugin implementation.
//!
//! ## Guest ABI (prototype)
//!
//! Exports (all on the guest's linear memory; i32 = u32 pointer/length):
//! - `adjutant_alloc(len) -> ptr`
//! - `adjutant_free(ptr, len)`
//! - `adjutant_describe(ptr, cap) -> i32` — writes the JSON manifest; returns
//!   bytes written, or `-needed` when `cap` is too small (call with `cap = 0`).
//! - `adjutant_handle(req_ptr, req_len, out_ptr, out_cap) -> i32` — writes the
//!   JSON response for one request; same negative-means-needed convention.
//!
//! Import:
//! - `env.adjutant_host_call(method_ptr, method_len, payload_ptr, payload_len,
//!   out_ptr, out_cap) -> i32` — one generic host call, result JSON.
//!
//! Response buffers are fixed (1 MiB) and single-shot, so an operation never
//! executes twice: overflow is an error, not a retry. A typed WIT /
//! component-model ABI is the likely end state; this prototype proves the
//! boundary and the sandbox.

use std::sync::{Arc, Mutex, OnceLock};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::runtime::Handle;
use wasmtime::{
    Caller, Config, Engine, Instance, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, TypedFunc,
};
use wasmtime_wasi::p1::WasiP1Ctx;

use adjutant_sdk::{
    async_trait, AdjutantPlugin, Identity, Method, Migration, Permission, PluginContext,
    PluginRequest, PluginResponse, RouteDefinition, Scope, ScopeType, SdkError, SqlValue,
};

/// Fixed-size guest buffers (manifest and response). Large enough for the
/// prototype's JSON; overflow is reported, never retried.
const GUEST_BUF: usize = 1 << 20; // 1 MiB
/// Guest linear-memory cap.
const GUEST_MEMORY_BYTES: usize = 64 << 20; // 64 MiB
/// Fuel per guest entry-point call. A tight `loop {}` exhausts this and traps.
const GUEST_FUEL: u64 = 50_000_000;

// ---------------------------------------------------------------------------
// Host state
// ---------------------------------------------------------------------------

/// Data stored in the wasmtime `Store` for one guest.
struct HostState {
    /// The plugin context, set during `init` (before any handler runs). Empty at
    /// describe time, when the guest must not call the host.
    ctx: Arc<OnceLock<PluginContext>>,
    /// Identity of the request currently being handled, for `permissions.has` /
    /// `audit.log` (host calls don't carry it themselves).
    current_identity: Mutex<Option<Identity>>,
    /// Runtime handle for driving the core's async services from the (blocking)
    /// wasmtime host call.
    runtime: Handle,
    limits: StoreLimits,
    /// WASI preview1 context. Built with no preopened directories, so the guest
    /// has no filesystem; preview1 exposes no sockets, so no network either.
    wasi: WasiP1Ctx,
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// A bind parameter on the wire. Tagged, because `SqlValue`'s own (untagged)
/// JSON form cannot distinguish a uuid from text or a typed null.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum WireValue {
    Null,
    Nullint,
    Nullbool,
    Nulluuid,
    Bool { value: bool },
    Int { value: i64 },
    Float { value: f64 },
    Text { value: String },
    Uuid { value: String },
    Intarray { value: Vec<i64> },
    Textarray { value: Vec<String> },
    Json { value: Value },
}

impl WireValue {
    fn into_sql(self) -> SqlValue {
        match self {
            WireValue::Null => SqlValue::Null,
            WireValue::Nullint => SqlValue::NullInt,
            WireValue::Nullbool => SqlValue::NullBool,
            WireValue::Nulluuid => SqlValue::NullUuid,
            WireValue::Bool { value } => SqlValue::Bool(value),
            WireValue::Int { value } => SqlValue::Int(value),
            WireValue::Float { value } => SqlValue::Float(value),
            WireValue::Text { value } => SqlValue::Text(value),
            WireValue::Uuid { value } => SqlValue::Uuid(value),
            WireValue::Intarray { value } => SqlValue::IntArray(value),
            WireValue::Textarray { value } => SqlValue::TextArray(value),
            WireValue::Json { value } => SqlValue::Json(value.to_string()),
        }
    }
}

fn parse_params(v: &Value) -> Result<Vec<SqlValue>, String> {
    let arr = v.as_array().cloned().unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let w: WireValue = serde_json::from_value(item).map_err(|e| format!("bad param: {e}"))?;
        out.push(w.into_sql());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Host-call dispatch (async services, driven synchronously)
// ---------------------------------------------------------------------------

/// Execute one guest host call. Returns the result JSON
/// (`{"ok":true,...}` / `{"ok":false,...}`).
fn dispatch(
    runtime: &Handle,
    ctx: Option<PluginContext>,
    identity: Option<Identity>,
    method: &str,
    payload: &str,
) -> String {
    let Some(ctx) = ctx else {
        return json!({"ok": false, "error": "plugin not initialized"}).to_string();
    };
    let p: Value = serde_json::from_str(payload).unwrap_or(Value::Null);

    let result: Result<Value, String> = runtime.block_on(async {
        match method {
            "db.query" => {
                let sql = p["sql"].as_str().ok_or("db.query: missing sql")?;
                let params = parse_params(&p["params"])?;
                let rows = ctx.db.query(sql, params).await.map_err(|e| e.to_string())?;
                Ok(json!({ "rows": rows }))
            }
            "db.execute" => {
                let sql = p["sql"].as_str().ok_or("db.execute: missing sql")?;
                let params = parse_params(&p["params"])?;
                let n = ctx.db.execute(sql, params).await.map_err(|e| e.to_string())?;
                Ok(json!({ "rows_affected": n }))
            }
            "events.publish" => {
                let et = p["event_type"].as_str().ok_or("events.publish: missing event_type")?;
                let body = p.get("payload").cloned().unwrap_or(Value::Null);
                ctx.events.publish(et, body).await.map_err(|e| e.to_string())?;
                Ok(json!({}))
            }
            "http.request" => {
                let method = p["method"].as_str().unwrap_or("GET").to_string();
                let url = p["url"].as_str().ok_or("http.request: missing url")?.to_string();
                let headers: Vec<(String, String)> = p["headers"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|h| {
                                Some((h[0].as_str()?.to_string(), h[1].as_str()?.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let body = p["body"]
                    .as_str()
                    .map(|s| ("application/json".to_string(), s.as_bytes().to_vec()));
                let resp = ctx
                    .http
                    .request(method, url, headers, body)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(json!({
                    "status": resp.status,
                    "headers": resp.headers.into_iter().collect::<Vec<_>>(),
                    "body": String::from_utf8_lossy(&resp.body),
                }))
            }
            "permissions.has" => {
                // Back-compat alias: **troop-only**. The unscoped check is gone
                // from the plugin SDK; a guest wanting a scoped check calls
                // `permissions.has_in_scope` with `{permission, scope}`.
                let perm = p["permission"].as_str().ok_or("permissions.has: missing permission")?;
                let has = ctx
                    .permissions
                    .has_in_scope(identity.as_ref(), perm, &Scope::troop())
                    .await;
                Ok(json!({ "has": has }))
            }
            "permissions.has_in_scope" => {
                let perm = p["permission"]
                    .as_str()
                    .ok_or("permissions.has_in_scope: missing permission")?;
                let scope = parse_scope(p.get("scope"))
                    .ok_or("permissions.has_in_scope: missing or invalid scope")?;
                let has = ctx.permissions.has_in_scope(identity.as_ref(), perm, &scope).await;
                Ok(json!({ "has": has }))
            }
            "audit.log" => {
                let action = p["action"].as_str().ok_or("audit.log: missing action")?;
                let rt = p["resource_type"].as_str().unwrap_or("");
                let rid = p["resource_id"].as_str().unwrap_or("");
                let details = p.get("details").cloned().unwrap_or(Value::Null);
                ctx.audit
                    .log(identity.as_ref(), action, rt, rid, details)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(json!({}))
            }
            other => Err(format!("unknown host method {other:?}")),
        }
    });

    match result {
        Ok(data) => json!({ "ok": true, "data": data }).to_string(),
        Err(e) => json!({ "ok": false, "error": e }).to_string(),
    }
}

// ---------------------------------------------------------------------------
// Guest handle
// ---------------------------------------------------------------------------

/// A loaded, instantiated guest plus the engine/module that own its code.
struct Guest {
    /// Kept alive so the guest's compiled code outlives its instance.
    #[allow(dead_code)]
    engine: Engine,
    #[allow(dead_code)]
    module: Module,
    store: Store<HostState>,
    #[allow(dead_code)]
    instance: Instance,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    free: TypedFunc<(u32, u32), ()>,
    describe: TypedFunc<(u32, u32), i32>,
    handle: TypedFunc<(u32, u32, u32, u32), i32>,
}

fn engine() -> Engine {
    let mut config = Config::new();
    config.consume_fuel(true);
    Engine::new(&config).expect("wasmtime engine")
}

fn new_store(engine: &Engine, runtime: Handle) -> Store<HostState> {
    let limits = StoreLimitsBuilder::new()
        .memory_size(GUEST_MEMORY_BYTES)
        .instances(1)
        .build();
    Store::new(
        engine,
        HostState {
            ctx: Arc::new(OnceLock::new()),
            current_identity: Mutex::new(None),
            runtime,
            limits,
            wasi: wasmtime_wasi::WasiCtxBuilder::new().build_p1(),
        },
    )
}

fn make_linker(engine: &Engine) -> Result<Linker<HostState>, wasmtime::Error> {
    let mut linker = Linker::new(engine);
    // WASI preview1 with no preopens: `std` works, filesystem does not.
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |h: &mut HostState| &mut h.wasi)?;
    linker.func_wrap(
        "env",
        "adjutant_host_call",
        |mut caller: Caller<'_, HostState>,
         method_ptr: u32,
         method_len: u32,
         payload_ptr: u32,
         payload_len: u32,
         out_ptr: u32,
         out_cap: u32|
         -> Result<i32, wasmtime::Error> {
            let mem = caller
                .get_export("memory")
                .and_then(|e| e.into_memory())
                .ok_or_else(|| wasmtime::Error::msg("guest has no exported memory"))?;

            let mut method = vec![0u8; method_len as usize];
            mem.read(&caller, method_ptr as usize, &mut method)?;
            let mut payload = vec![0u8; payload_len as usize];
            mem.read(&caller, payload_ptr as usize, &mut payload)?;

            // Clone out of the caller borrow before driving the runtime.
            let (slot, runtime, identity) = {
                let host = caller.data();
                (
                    host.ctx.clone(),
                    host.runtime.clone(),
                    host.current_identity.lock().unwrap().clone(),
                )
            };
            let result = dispatch(
                &runtime,
                slot.get().cloned(),
                identity,
                &String::from_utf8_lossy(&method),
                &String::from_utf8_lossy(&payload),
            );

            let bytes = result.as_bytes();
            if bytes.len() > out_cap as usize {
                return Ok(-(bytes.len() as i32));
            }
            mem.write(&mut caller, out_ptr as usize, bytes)?;
            Ok(bytes.len() as i32)
        },
    )?;
    Ok(linker)
}

impl Guest {
    /// Instantiate a guest module. Must run on a blocking thread: host calls
    /// during describe then use `Handle::block_on` safely.
    fn instantiate(path: &std::path::Path, runtime: Handle) -> Result<Self, wasmtime::Error> {
        let engine = engine();
        let module = Module::from_file(&engine, path)?;
        let mut store = new_store(&engine, runtime);
        store.limiter(|s| &mut s.limits);
        let instance = make_linker(&engine)?.instantiate(&mut store, &module)?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| wasmtime::Error::msg("guest must export `memory`"))?;
        let alloc = instance.get_typed_func::<u32, u32>(&mut store, "adjutant_alloc")?;
        let free = instance.get_typed_func::<(u32, u32), ()>(&mut store, "adjutant_free")?;
        let describe =
            instance.get_typed_func::<(u32, u32), i32>(&mut store, "adjutant_describe")?;
        let handle =
            instance.get_typed_func::<(u32, u32, u32, u32), i32>(&mut store, "adjutant_handle")?;

        Ok(Self { engine, module, store, instance, memory, alloc, free, describe, handle })
    }

    fn describe_json(&mut self) -> Result<Vec<u8>, wasmtime::Error> {
        self.store.set_fuel(GUEST_FUEL)?;
        let needed = self.describe.call(&mut self.store, (0, 0))?;
        if needed >= 0 {
            return Err(wasmtime::Error::msg("adjutant_describe did not report a length"));
        }
        let cap = (-needed) as u32;
        let ptr = self.alloc.call(&mut self.store, cap)?;
        self.store.set_fuel(GUEST_FUEL)?;
        let written = self.describe.call(&mut self.store, (ptr, cap))?;
        if written < 0 {
            return Err(wasmtime::Error::msg("adjutant_describe buffer too small"));
        }
        let mut buf = vec![0u8; written as usize];
        self.memory.read(&self.store, ptr as usize, &mut buf)?;
        self.free.call(&mut self.store, (ptr, cap))?;
        Ok(buf)
    }

    /// Invoke the guest handler. Must run on a blocking thread.
    fn handle_request(
        &mut self,
        req_bytes: &[u8],
        identity: Option<Identity>,
    ) -> Result<Vec<u8>, wasmtime::Error> {
        *self.store.data().current_identity.lock().unwrap() = identity;
        self.store.set_fuel(GUEST_FUEL)?;

        let req_len = req_bytes.len() as u32;
        let req_ptr = self.alloc.call(&mut self.store, req_len)?;
        self.memory.write(&mut self.store, req_ptr as usize, req_bytes)?;
        let out_ptr = self.alloc.call(&mut self.store, GUEST_BUF as u32)?;

        self.store.set_fuel(GUEST_FUEL)?;
        let written = self
            .handle
            .call(&mut self.store, (req_ptr, req_len, out_ptr, GUEST_BUF as u32))?;

        let result = (|| {
            if written < 0 {
                return Err(wasmtime::Error::msg(format!(
                    "guest response exceeds {GUEST_BUF} bytes"
                )));
            }
            let mut buf = vec![0u8; written as usize];
            self.memory.read(&self.store, out_ptr as usize, &mut buf)?;
            Ok(buf)
        })();

        let _ = self.free.call(&mut self.store, (req_ptr, req_len));
        let _ = self.free.call(&mut self.store, (out_ptr, GUEST_BUF as u32));
        *self.store.data().current_identity.lock().unwrap() = None;
        result
    }
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Manifest {
    id: String,
    name: String,
    version: String,
    #[serde(default)]
    permissions: Vec<PermissionMeta>,
    #[serde(default)]
    migrations: Vec<MigrationMeta>,
    #[serde(default)]
    routes: Vec<RouteMeta>,
}

#[derive(Debug, Deserialize)]
struct PermissionMeta {
    id: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize)]
struct MigrationMeta {
    version: i64,
    name: String,
    sql: String,
}

#[derive(Debug, Deserialize)]
struct RouteMeta {
    method: String,
    path: String,
    #[serde(default)]
    permission: Option<String>,
}

fn route_method(s: &str) -> Option<Method> {
    match s {
        "GET" => Some(Method::Get),
        "POST" => Some(Method::Post),
        "PUT" => Some(Method::Put),
        "PATCH" => Some(Method::Patch),
        "DELETE" => Some(Method::Delete),
        "HEAD" => Some(Method::Head),
        _ => None,
    }
}

/// Parse a guest `{"type":"lodge","id":"1"}` scope. Fails closed: an unknown
/// type, or a non-troop scope with no id, is `None` (never widened to troop).
fn parse_scope(v: Option<&Value>) -> Option<Scope> {
    let v = v?;
    let scope_type = match v["type"].as_str()? {
        "troop" => ScopeType::Troop,
        "lodge" => ScopeType::Lodge,
        "patrol" => ScopeType::Patrol,
        _ => return None,
    };
    let id = v["id"].as_str().map(str::to_string).filter(|s| !s.is_empty());
    if scope_type != ScopeType::Troop && id.is_none() {
        return None;
    }
    Some(Scope {
        scope_type,
        scope_id: if scope_type == ScopeType::Troop { None } else { id },
    })
}

// ---------------------------------------------------------------------------
// WasmPlugin
// ---------------------------------------------------------------------------

/// A sandboxed plugin: an ordinary [`AdjutantPlugin`] whose handlers call a
/// WASM guest.
pub struct WasmPlugin {
    guest: Arc<Mutex<Guest>>,
    ctx_slot: Arc<OnceLock<PluginContext>>,
    manifest: Manifest,
}

impl WasmPlugin {
    /// Instantiate a guest and read its manifest. Instantiation and describe run
    /// on a blocking thread (the guest may call the host).
    pub async fn open(path: &std::path::Path) -> Result<Self, String> {
        let path = path.to_path_buf();
        let runtime = Handle::current();
        let (guest, manifest) = tokio::task::spawn_blocking(move || {
            let mut guest = Guest::instantiate(&path, runtime).map_err(|e| e.to_string())?;
            let raw = guest.describe_json().map_err(|e| e.to_string())?;
            let manifest: Manifest =
                serde_json::from_slice(&raw).map_err(|e| format!("invalid manifest JSON: {e}"))?;
            Ok::<_, String>((guest, manifest))
        })
        .await
        .map_err(|e| format!("wasm task failed: {e}"))??;

        let ctx_slot = guest.store.data().ctx.clone();
        Ok(Self {
            guest: Arc::new(Mutex::new(guest)),
            ctx_slot,
            manifest,
        })
    }
}

#[async_trait]
impl AdjutantPlugin for WasmPlugin {
    fn id(&self) -> &str {
        &self.manifest.id
    }

    fn name(&self) -> &str {
        &self.manifest.name
    }

    fn version(&self) -> &str {
        &self.manifest.version
    }

    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
        let _ = self.ctx_slot.set(ctx);
        Ok(())
    }

    fn permissions_granted(&self) -> Vec<Permission> {
        self.manifest
            .permissions
            .iter()
            .map(|p| Permission::new(&p.id, &p.description))
            .collect()
    }

    fn migrations(&self) -> Vec<Migration> {
        self.manifest
            .migrations
            .iter()
            .map(|m| Migration::new(m.version, &m.name, &m.sql))
            .collect()
    }

    fn routes(&self) -> Vec<RouteDefinition> {
        let mut out = Vec::with_capacity(self.manifest.routes.len());
        for meta in &self.manifest.routes {
            let Some(method) = route_method(&meta.method) else {
                continue;
            };
            let guest = self.guest.clone();
            let handler = adjutant_sdk::route_handler(move |req: PluginRequest| {
                let guest = guest.clone();
                async move { call_guest(guest, req).await }
            });
            out.push(RouteDefinition {
                method,
                path: meta.path.clone(),
                required_permission: meta.permission.clone(),
                // A guest manifest does not declare a scope yet; the safe
                // default is troop coverage (a protected guest route is not
                // opened by a lodge-scoped grant).
                required_scope: Some(adjutant_sdk::Scope::troop()),
                handler,
            });
        }
        out
    }
}

/// Run one guest handler on a blocking thread and translate its response.
async fn call_guest(
    guest: Arc<Mutex<Guest>>,
    req: PluginRequest,
) -> Result<PluginResponse, SdkError> {
    let identity = req.identity.clone();
    let req_json = serde_json::to_vec(&json!({
        "method": req.method,
        "path": req.path,
        "params": req.params,
        "query": req.query,
        "headers": req.headers,
        "body": String::from_utf8_lossy(&req.body),
        "identity": req.identity,
    }))
    .map_err(|e| SdkError::Internal(format!("encode request: {e}")))?;

    let raw = tokio::task::spawn_blocking(move || {
        let mut g = guest.lock().expect("guest mutex poisoned");
        g.handle_request(&req_json, identity)
    })
    .await
    .map_err(|e| SdkError::Internal(format!("wasm task failed: {e}")))?
    .map_err(|e| SdkError::Internal(format!("guest trapped: {e}")))?;

    #[derive(Deserialize)]
    struct WasmResponse {
        status: u16,
        #[serde(default)]
        headers: Vec<(String, String)>,
        #[serde(default)]
        body: String,
    }
    let resp: WasmResponse = serde_json::from_slice(&raw)
        .map_err(|e| SdkError::Internal(format!("invalid guest response: {e}")))?;
    Ok(PluginResponse {
        status: resp.status,
        headers: resp.headers,
        body: resp.body.into_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instantiate_wat(wat: &str) -> Result<(Store<HostState>, Instance), String> {
        let engine = engine();
        let module = Module::new(&engine, wat).map_err(|e| e.to_string())?;
        let mut store = new_store(&engine, Handle::current());
        store.limiter(|s| &mut s.limits);
        let instance = make_linker(&engine)
            .map_err(|e| e.to_string())?
            .instantiate(&mut store, &module)
            .map_err(|e| e.to_string())?;
        Ok((store, instance))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fuel_traps_a_runaway_guest() {
        let (mut store, instance) =
            instantiate_wat(r#"(module (func (export "spin") (loop br 0)))"#).expect("instantiate");
        let spin = instance
            .get_typed_func::<(), ()>(&mut store, "spin")
            .expect("typed func");
        store.set_fuel(GUEST_FUEL).unwrap();
        // A tight infinite loop must exhaust fuel and trap, not hang the host.
        assert!(spin.call(&mut store, ()).is_err(), "runaway loop must trap on fuel");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_filesystem_is_preopened() {
        // WASI is linked (std needs it), but with zero preopened directories:
        // `fd_prestat_get(3)` (the first potential preopen) must fail.
        let wat = r#"(module
            (import "wasi_snapshot_preview1" "fd_prestat_get"
                (func $prestat (param i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "prestat") (result i32)
                (call $prestat (i32.const 3) (i32.const 0)))
        )"#;
        let (mut store, instance) = instantiate_wat(wat).expect("instantiate");
        let prestat = instance
            .get_typed_func::<(), i32>(&mut store, "prestat")
            .expect("typed func");
        store.set_fuel(GUEST_FUEL).unwrap();
        let errno = prestat.call(&mut store, ()).unwrap();
        assert_ne!(errno, 0, "fd 3 must not be a preopened directory");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn memory_growth_is_capped() {
        let (mut store, instance) = instantiate_wat(
            r#"(module
                (memory (export "memory") 1)
                (func (export "grow") (param i32) (result i32) (memory.grow (local.get 0)))
            )"#,
        )
        .expect("instantiate");
        let grow = instance
            .get_typed_func::<i32, i32>(&mut store, "grow")
            .expect("typed func");
        // Ask for far more than the 64 MiB cap; the limiter denies it (-1).
        store.set_fuel(GUEST_FUEL).unwrap();
        let got = grow.call(&mut store, 4096).unwrap();
        assert_eq!(got, -1, "memory growth beyond the cap must be denied");
    }
}
