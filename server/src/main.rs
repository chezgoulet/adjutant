//! `adjutant` — the core server binary.

use adjutant_server::config::{self, CliArgs, LogFormat};
use adjutant_server::build_app;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // CLI first: --help must work without a database.
    let cli = CliArgs::parse(std::env::args().skip(1)).unwrap_or_else(|e| {
        eprintln!("error: {e}\n\n{}", config::USAGE);
        std::process::exit(2);
    });
    if cli.help {
        print!("{}", config::USAGE);
        return Ok(());
    }
    let cfg = config::load(&cli)?;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| cfg.log_filter.clone().into());
    match cfg.log_format {
        LogFormat::Pretty => tracing_subscriber::fmt().with_env_filter(filter).init(),
        LogFormat::Json => tracing_subscriber::fmt().with_env_filter(filter).json().init(),
    }

    tracing::info!(bind = %cfg.bind, plugin_dir = %cfg.plugin_dir.display(), "starting adjutant");

    let (app, _state) = build_app(&cfg).await?;

    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");

    // ConnectInfo feeds the rate limiter's per-IP key.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    tracing::info!("adjutant stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("install ctrl-c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
