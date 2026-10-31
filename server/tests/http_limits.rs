//! Issue #172: the HTTP surface must not let anonymous clients hold sockets or
//! bodies forever, and a plugin route must not read a body before its gate.
//!
//! These are intentionally not DB-gated: the route under test is refused by the
//! permission gate before any database query, and the timeouts are transport
//! behaviour. CI's bare `cargo test --workspace` therefore runs them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use adjutant_sdk::{
    async_trait, AdjutantPlugin, AuditService, EventSubscription, HostDb, Method, Migration,
    Permission, PermissionService, PluginContext, PluginResponse, RouteDefinition, SdkError,
    SqlValue,
};
use adjutant_server::config::{Config, HttpConfig, RateConfig};
use adjutant_server::plugin_runtime::{LoadedPlugin, PluginInfo, PluginRegistry, PluginSlot};
use adjutant_server::server::{AppState, InFlight, LifecycleLocks};
use adjutant_server::{host, identity, outbox, scheduler, scope_hierarchy, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A database that is never reached: the anonymous gate answers 401 before the
/// permission query, and the public route never asks.
struct NoDb;

#[async_trait]
impl HostDb for NoDb {
    async fn execute(&self, _sql: String, _params: Vec<SqlValue>) -> Result<u64, SdkError> {
        Err(SdkError::Db("unexpected database write".into()))
    }
    async fn query(
        &self,
        _sql: String,
        _params: Vec<SqlValue>,
    ) -> Result<Vec<serde_json::Value>, SdkError> {
        Err(SdkError::Db("unexpected database query".into()))
    }
}

struct SlowBodyPlugin;

#[async_trait]
impl AdjutantPlugin for SlowBodyPlugin {
    fn id(&self) -> &str {
        "slow"
    }
    fn name(&self) -> &str {
        "Slow body fixture"
    }
    fn version(&self) -> &str {
        "0.0.1"
    }
    async fn init(&mut self, _ctx: PluginContext) -> Result<(), SdkError> {
        Ok(())
    }
    fn routes(&self) -> Vec<RouteDefinition> {
        let handle = adjutant_sdk::route_handler(|req| async move {
            Ok(PluginResponse::text(
                200,
                format!("read {} bytes", req.body.len()),
            ))
        });
        vec![
            RouteDefinition {
                method: Method::Post,
                path: "/api/slow/protected".into(),
                required_permission: Some("slow:write".into()),
                required_scope: Some(adjutant_sdk::Scope::troop()),
                handler: handle.clone(),
            },
            RouteDefinition {
                method: Method::Post,
                path: "/api/slow/open".into(),
                required_permission: None,
                required_scope: None,
                handler: handle,
            },
        ]
    }
    fn migrations(&self) -> Vec<Migration> {
        Vec::new()
    }
    fn permissions_granted(&self) -> Vec<Permission> {
        Vec::new()
    }
    fn subscriptions(&self) -> Vec<EventSubscription> {
        Vec::new()
    }
}

fn state(http: HttpConfig) -> Arc<AppState> {
    let info = PluginInfo {
        id: "slow".into(),
        name: "Slow body fixture".into(),
        version: "0.0.1".into(),
        enabled: true,
        routes: 2,
        kind: "native".into(),
        isolated: true,
        loaded: true,
        last_error: None,
        permissions: Vec::new(),
        schedules: Vec::new(),
        route_list: Vec::new(),
    };
    let loaded = LoadedPlugin {
        plugin: Box::new(SlowBodyPlugin),
        library: None,
        pool: None,
        routes: SlowBodyPlugin.routes(),
        path: std::path::PathBuf::new(),
        info,
    };
    let cfg = Config {
        allow_dev_headers: false,
        http,
        rate: RateConfig {
            window_secs: 60,
            max_requests: 0,
        },
        ..Config::default()
    };
    Arc::new(AppState {
        pool: Arc::new(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://adjutant:***@127.0.0.1:1/adjutant_test")
                .expect("lazy pool URL parses"),
        ),
        permissions: PermissionService::new(Arc::new(NoDb)),
        audit: AuditService::new(Arc::new(NoDb), "core".into()),
        registry: tokio::sync::RwLock::new(PluginRegistry::new(vec![PluginSlot::Live(loaded)])),
        bus: adjutant_server::events::EventBus::new(),
        config: Arc::new(cfg),
        identity: identity::IdentityHub::new(),
        http: host::CoreHttp::new(),
        hierarchy: tokio::sync::RwLock::new(scope_hierarchy::ScopeHierarchy::default()),
        scheduler: scheduler::Scheduler::new(),
        outbox_mismatches: std::sync::Mutex::new(0),
        relay: outbox::Relay::new(),
        backups: adjutant_server::backup::Timer::new(),
        in_flight: InFlight::new(),
        lifecycles: LifecycleLocks::new(),
    })
}

/// Start the real production router and serve loop on an ephemeral port.
struct TestServer {
    addr: std::net::SocketAddr,
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl TestServer {
    async fn start(http: HttpConfig) -> Self {
        let state = state(http);
        let app = adjutant_server::server::build_router(state);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral listener");
        let addr = listener.local_addr().expect("local addr");
        let (shutdown, rx) = tokio::sync::oneshot::channel::<()>();
        let header_timeout = Duration::from_secs(http.header_timeout_secs);
        let max_connections = http.max_connections;
        let task = tokio::spawn(async move {
            serve::serve(
                listener,
                app,
                async {
                    let _ = rx.await;
                },
                header_timeout,
                max_connections,
            )
            .await
        });
        Self {
            addr,
            shutdown,
            task,
        }
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
    }
}

fn headers(path: &str, content_length: Option<usize>) -> String {
    let length = content_length
        .map(|n| format!("Content-Length: {n}\r\n"))
        .unwrap_or_default();
    format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n{length}\r\n"
    )
}

async fn read_response(stream: &mut TcpStream, limit: Duration) -> (Duration, Vec<u8>) {
    let start = Instant::now();
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut buf = [0u8; 2048];
        match tokio::time::timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                got.extend_from_slice(&buf[..n]);
                if got.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }
    (start.elapsed(), got)
}

/// The companion finding: a plugin route held a slow body for 40 s before its
/// permission gate. Now the gate runs first, so the refusal is immediate even
/// though the client never sends the body.
#[tokio::test]
async fn protected_plugin_route_refuses_before_reading_a_slow_body() {
    let server = TestServer::start(HttpConfig {
        header_timeout_secs: 5,
        body_timeout_secs: 2,
        request_timeout_secs: 10,
        max_connections: 16,
    })
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    stream
        .write_all(headers("/api/slow/protected", Some(100)).as_bytes())
        .await
        .expect("write headers without a body");

    let (elapsed, response) = read_response(&mut stream, Duration::from_secs(1)).await;
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 401"),
        "anonymous caller must be refused at the gate, not wait for the body: {text:?}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "the refusal must not wait for the body deadline ({elapsed:?})"
    );
    drop(stream);
    server.stop().await;
}

/// A public route (like `/api/auth/login`) still reads a body, but the total
/// body deadline bounds it. One byte per second cannot renew this deadline.
#[tokio::test]
async fn public_route_body_has_a_total_deadline() {
    let server = TestServer::start(HttpConfig {
        header_timeout_secs: 5,
        body_timeout_secs: 1,
        request_timeout_secs: 10,
        max_connections: 16,
    })
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    stream
        .write_all(headers("/api/slow/open", Some(100)).as_bytes())
        .await
        .expect("write headers without a body");

    let (elapsed, response) = read_response(&mut stream, Duration::from_secs(4)).await;
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 408"),
        "a stalled body must be answered with 408: {text:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the body deadline is total, not per chunk ({elapsed:?})"
    );
    drop(stream);
    server.stop().await;
}

/// The case measured at 25 s: a header line that never ends is cut by the
/// server's header-read deadline, below the router.
#[tokio::test]
async fn partial_headers_are_closed_by_the_header_deadline() {
    let server = TestServer::start(HttpConfig {
        header_timeout_secs: 1,
        body_timeout_secs: 5,
        request_timeout_secs: 10,
        max_connections: 16,
    })
    .await;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nX-Partial: ")
        .await
        .expect("write partial header");

    let (elapsed, response) = read_response(&mut stream, Duration::from_secs(4)).await;
    assert!(
        response.is_empty(),
        "an incomplete header must not produce a response: {response:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the header deadline closes the socket promptly ({elapsed:?})"
    );
    drop(stream);
    server.stop().await;
}

/// The size cap is a layer, not something each handler must remember: a
/// declared body over `max_body_bytes` is refused before the route runs.
#[tokio::test]
async fn oversized_body_is_refused_at_the_layer() {
    let server = TestServer::start(HttpConfig {
        header_timeout_secs: 5,
        body_timeout_secs: 5,
        request_timeout_secs: 10,
        max_connections: 16,
    })
    .await;
    let too_big = 1024 * 1024 + 1;
    let mut stream = TcpStream::connect(server.addr).await.expect("connect");
    stream
        .write_all(headers("/api/slow/open", Some(too_big)).as_bytes())
        .await
        .expect("write oversized header without a body");

    let (elapsed, response) = read_response(&mut stream, Duration::from_secs(1)).await;
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 413"),
        "a body over the layer limit must be refused as 413: {text:?}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "the layer refuses on Content-Length, before reading the body ({elapsed:?})"
    );
    drop(stream);
    server.stop().await;
}

/// The connection cap: while the only slot is held, a new connection is not
/// served; closing the holder frees the slot and the queued request completes.
#[tokio::test]
async fn connection_cap_bounds_concurrent_sockets() {
    let server = TestServer::start(HttpConfig {
        header_timeout_secs: 30,
        body_timeout_secs: 5,
        request_timeout_secs: 10,
        max_connections: 1,
    })
    .await;
    let mut held = TcpStream::connect(server.addr).await.expect("held connect");
    // Complete one keep-alive request so the slot is definitely taken by an
    // accepted connection, not still sitting in the kernel backlog.
    held.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("held request");
    let (_, first) = read_response(&mut held, Duration::from_secs(3)).await;
    assert!(
        String::from_utf8_lossy(&first).starts_with("HTTP/1.1 200"),
        "the holding connection is served first"
    );

    let mut queued = TcpStream::connect(server.addr)
        .await
        .expect("queued connect");
    queued
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("queued request");
    let (_, early) = read_response(&mut queued, Duration::from_millis(300)).await;
    assert!(
        early.is_empty(),
        "the queued request must wait while the only connection slot is held: {early:?}"
    );

    drop(held);
    let (_, response) = read_response(&mut queued, Duration::from_secs(3)).await;
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "the queued request is served once a slot frees: {text:?}"
    );
    drop(queued);
    server.stop().await;
}
