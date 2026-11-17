//! The HTTP server loop.
//!
//! `axum::serve` is deliberately simple and exposes no HTTP/1 header-read
//! deadline (issue #172 measured a partial header held for 25 s). This loop
//! borrows its shape — same `IntoMakeServiceWithConnectInfo`, same hyper
//! connection builder, same graceful-shutdown handshake — and adds the two
//! limits that belong below the router:
//!
//! * `header_read_timeout` on hyper's HTTP/1 builder, so an incomplete header
//!   block cannot occupy a socket indefinitely;
//! * a semaphore around `accept`, so the number of live connection tasks (and
//!   therefore held sockets the application is responsible for) is bounded.
//!
//! Everything a request sees after its headers arrive stays in the router:
//! body deadline, body size and whole-request timeout are layers there.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::Router;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};
use tower::{Service, ServiceExt as _};

/// Serve `app` on `listener` with the given first-byte/header deadline and
/// connection cap. `shutdown` resolves when the process should stop accepting;
/// live connections then get hyper's graceful shutdown and are allowed to
/// finish.
pub async fn serve<F>(
    listener: TcpListener,
    app: Router,
    shutdown: F,
    header_timeout: Duration,
    max_connections: usize,
) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let make_service = app.into_make_service_with_connect_info::<SocketAddr>();
    let slots = Arc::new(Semaphore::new(max_connections));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        let _ = shutdown_tx.send(true);
    });

    let (close_tx, close_rx) = watch::channel(());

    loop {
        // Take a slot before accepting: at the cap, new TCP handshakes wait in
        // the kernel backlog instead of becoming unbounded tasks and buffers.
        let permit = tokio::select! {
            permit = slots.clone().acquire_owned() => {
                permit.expect("the connection semaphore is never closed")
            }
            _ = wait_for_shutdown(shutdown_rx.clone()) => break,
        };

        let (stream, remote_addr) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(e) => {
                    drop(permit);
                    handle_accept_error(e).await;
                    continue;
                }
            },
            _ = wait_for_shutdown(shutdown_rx.clone()) => break,
        };

        let mut make_service = make_service.clone();
        let conn_shutdown = shutdown_rx.clone();
        let conn_close = close_rx.clone();
        tokio::spawn(async move {
            let _permit = permit;
            // Hyper's HTTP/1 header timer needs a read poll to start. A socket
            // that sends nothing at all would otherwise wait for the first byte
            // indefinitely, so bound that byte here and let hyper's timer bound
            // the rest of the headers.
            let mut first_byte = [0u8; 1];
            let first = tokio::select! {
                result = tokio::time::timeout(header_timeout, stream.peek(&mut first_byte)) => result,
                _ = wait_for_shutdown(conn_shutdown.clone()) => return,
            };
            match first {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    tracing::debug!(%remote_addr, error = %e, "connection failed before headers");
                    return;
                }
                Err(_) => {
                    tracing::debug!(%remote_addr, "no request bytes before the header deadline");
                    return;
                }
            }
            let io = TokioIo::new(stream);
            let tower_service = make_service
                .call(remote_addr)
                .await
                .unwrap_or_else(|err| match err {});
            let tower_service =
                tower_service.map_request(|req: hyper::Request<Incoming>| req.map(Body::new));
            let hyper_service = TowerToHyperService::new(tower_service);

            let mut builder = http1::Builder::new();
            // Adjutant serves plain HTTP/1. The timer is required whenever
            // header_read_timeout is set, and this is the timeout the review
            // measured.
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(Some(header_timeout));

            let mut conn = pin!(builder.serve_connection(io, hyper_service));
            let mut gracefully = false;
            loop {
                if gracefully {
                    if let Err(e) = conn.as_mut().await {
                        tracing::debug!(%remote_addr, error = %e, "connection ended");
                    }
                    break;
                }
                let mut shutdown = pin!(wait_for_shutdown(conn_shutdown.clone()));
                tokio::select! {
                    result = conn.as_mut() => {
                        if let Err(e) = result {
                            tracing::debug!(%remote_addr, error = %e, "connection ended");
                        }
                        break;
                    }
                    _ = &mut shutdown => {
                        tracing::debug!(%remote_addr, "draining connection");
                        conn.as_mut().graceful_shutdown();
                        gracefully = true;
                    }
                }
            }
            drop(conn_close);
        });
    }

    drop(close_rx);
    drop(listener);
    // Every connection task holds a receiver; this waits for the last one.
    close_tx.closed().await;
    Ok(())
}

/// Resolve when the shutdown flag is set. A closed sender (the process is
/// already going down) counts as shutdown too, so a task can never linger.
async fn wait_for_shutdown(mut rx: watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow_and_update() {
            return;
        }
    }
}

async fn handle_accept_error(e: io::Error) {
    if matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    ) {
        return;
    }
    tracing::error!(error = %e, "accept error");
    tokio::time::sleep(Duration::from_secs(1)).await;
}
