//! HTTP middleware: request ID + structured access log, fixed-window rate
//! limiting (SPEC §15 M2 middleware stack). CORS lives in `server.rs` (it is a
//! tower layer, not a from_fn).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::http::StatusCode;

use crate::config::RateConfig;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Monotonic-enough request id: hex(epoch_ms)-hex(counter).
pub fn new_request_id() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    format!("{ms:013x}-{:06x}", REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Assign an `x-request-id` and emit one structured line per request after the
/// response is produced (status + latency, no headers/body — never log secrets).
pub async fn request_log(request: Request, next: Next) -> Response {
    let rid = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_else(new_request_id);

    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let start = Instant::now();

    let mut response = next.run(request).await;

    if let (Ok(name), Ok(val)) =
        (HeaderName::from_bytes(b"x-request-id"), HeaderValue::from_str(&rid))
    {
        response.headers_mut().insert(name, val);
    }

    tracing::info!(
        target: "adjutant::http",
        request_id = %rid,
        method = %method,
        path = %path,
        status = response.status().as_u16(),
        latency_ms = start.elapsed().as_millis() as u64,
        "request"
    );
    response
}

/// Fixed-window counter keyed by client IP. `max_requests == 0` disables.
#[derive(Clone)]
pub struct RateLimiter {
    cfg: RateConfig,
    hits: Arc<Mutex<HashMap<String, (Instant, u32)>>>,
}

impl RateLimiter {
    pub fn new(cfg: RateConfig) -> Self {
        Self { cfg, hits: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Record a hit for `key`; false when the key exceeded its window budget.
    pub fn check(&self, key: &str, now: Instant) -> bool {
        if self.cfg.max_requests == 0 {
            return true;
        }
        let mut map = self.hits.lock().expect("rate limiter poisoned");
        // opportunistic prune so a scan-flood can't grow the map unbounded
        if map.len() > 10_000 {
            let window = Duration::from_secs(self.cfg.window_secs);
            map.retain(|_, (started, _)| now.duration_since(*started) < window);
        }
        let window = Duration::from_secs(self.cfg.window_secs);
        let entry = map.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= self.cfg.max_requests
    }
}

/// Core check: pick the client key, enforce the window, run or reject.
pub async fn rate_limit_inner(
    limiter: RateLimiter,
    request: Request,
    next: Next,
) -> Response {
    // Prefer the proxy-set IP, else the direct peer (ConnectInfo is layered in
    // by the server via `into_make_service_with_connect_info`).
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let key = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or(peer);

    if limiter.check(&key, Instant::now()) {
        next.run(request).await
    } else {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [
                (HeaderName::from_static("content-type"), HeaderValue::from_static("application/json")),
                (
                    HeaderName::from_static("retry-after"),
                    HeaderValue::from_str(&limiter.cfg.window_secs.to_string())
                        .unwrap_or_else(|_| HeaderValue::from_static("60")),
                ),
            ],
            r#"{"error":"rate limit exceeded"}"#,
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_window_budget_and_rollover() {
        let rl = RateLimiter::new(RateConfig { window_secs: 60, max_requests: 3 });
        let t0 = Instant::now();
        assert!(rl.check("1.1.1.1", t0));
        assert!(rl.check("1.1.1.1", t0 + Duration::from_secs(1)));
        assert!(rl.check("1.1.1.1", t0 + Duration::from_secs(2)));
        assert!(!rl.check("1.1.1.1", t0 + Duration::from_secs(3)), "4th in window must fail");

        // keys are isolated
        assert!(rl.check("2.2.2.2", t0 + Duration::from_secs(3)));

        // window rollover resets the budget
        assert!(rl.check("1.1.1.1", t0 + Duration::from_secs(61)));
    }

    #[test]
    fn rate_limiter_disabled_when_zero() {
        let rl = RateLimiter::new(RateConfig { window_secs: 60, max_requests: 0 });
        let t = Instant::now();
        for _ in 0..1000 {
            assert!(rl.check("x", t));
        }
    }

    #[test]
    fn request_ids_are_unique() {
        let a = new_request_id();
        let b = new_request_id();
        assert_ne!(a, b);
        assert!(a.contains('-'));
    }
}
