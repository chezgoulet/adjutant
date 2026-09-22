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
    /// IPs allowed to set `x-forwarded-for` (direct peers of a reverse proxy).
    trusted_proxies: Arc<Vec<String>>,
    hits: Arc<Mutex<HashMap<String, (Instant, u32)>>>,
}

impl RateLimiter {
    pub fn new(cfg: RateConfig, trusted_proxies: Vec<String>) -> Self {
        Self {
            cfg,
            trusted_proxies: Arc::new(trusted_proxies),
            hits: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Client key for the window.
    ///
    /// `x-forwarded-for` is CLIENT-CONTROLLED. It is honoured only when the
    /// direct peer is a configured trusted proxy; the list is then walked from
    /// the right, skipping trusted hops, so entries a client injected on the
    /// left are never used. With no trusted proxies configured (the default)
    /// the header is ignored entirely — otherwise any client can reset its own
    /// budget by rotating a header value.
    pub fn client_key(&self, peer: &str, xff: Option<&str>) -> String {
        if self.trusted_proxies.is_empty() || !self.is_trusted(peer) {
            return peer.to_string();
        }
        let hops: Vec<&str> = xff
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        hops.iter()
            .rev()
            .find(|hop| !self.is_trusted(hop))
            .map(|hop| (*hop).to_string())
            .unwrap_or_else(|| peer.to_string())
    }

    fn is_trusted(&self, ip: &str) -> bool {
        self.trusted_proxies.iter().any(|t| t == ip)
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
    // by the server via `into_make_service_with_connect_info`). The header is
    // only meaningful when the peer is a configured trusted proxy — see
    // `RateLimiter::client_key`.
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let xff = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok());
    let key = limiter.client_key(&peer, xff);

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
        let rl = RateLimiter::new(RateConfig { window_secs: 60, max_requests: 3 }, vec![]);
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
        let rl = RateLimiter::new(RateConfig { window_secs: 60, max_requests: 0 }, vec![]);
        let t = Instant::now();
        for _ in 0..1000 {
            assert!(rl.check("x", t));
        }
    }

    #[test]
    fn forwarded_for_is_ignored_without_trusted_proxies() {
        let rl = RateLimiter::new(RateConfig::default(), vec![]);
        // Client-supplied header must not change the key: rotating it would
        // otherwise reset the window.
        assert_eq!(rl.client_key("203.0.113.7", Some("1.2.3.4")), "203.0.113.7");
    }

    #[test]
    fn forwarded_for_used_only_from_a_trusted_peer() {
        let rl = RateLimiter::new(RateConfig::default(), vec!["10.0.0.5".into()]);
        // Untrusted peer: header ignored even if present.
        assert_eq!(rl.client_key("203.0.113.7", Some("1.2.3.4")), "203.0.113.7");
        // Trusted peer: the rightmost non-trusted hop is the client.
        assert_eq!(rl.client_key("10.0.0.5", Some("1.2.3.4")), "1.2.3.4");
        // Injected left-hand entries are never used.
        assert_eq!(
            rl.client_key("10.0.0.5", Some("9.9.9.9, 1.2.3.4")),
            "1.2.3.4"
        );
        // All-trusted chain falls back to the peer.
        assert_eq!(rl.client_key("10.0.0.5", Some("10.0.0.5")), "10.0.0.5");
        // Malformed/empty header falls back to the peer.
        assert_eq!(rl.client_key("10.0.0.5", Some("")), "10.0.0.5");
    }

    #[test]
    fn request_ids_are_unique() {
        let a = new_request_id();
        let b = new_request_id();
        assert_ne!(a, b);
        assert!(a.contains('-'));
    }
}
