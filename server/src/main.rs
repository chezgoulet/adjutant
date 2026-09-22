//! `adjutant` — the core server binary.

use adjutant_server::config::{self, CliArgs, LogFormat};
use adjutant_server::build_app;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Subcommands first; anything else falls through to flag parsing (serve).
    match args.first().map(|s| s.as_str()) {
        Some("new-plugin") => {
            let name = args
                .get(1)
                .filter(|a| !a.starts_with('-'))
                .ok_or("--help")?;
            let root = find_repo_root()?;
            match adjutant_server::cli::scaffold_plugin(&root, name) {
                Ok(dir) => {
                    println!("scaffolded {}", dir.display());
                    println!("next: cargo build -p adjutant-{name}");
                    println!(
                        "      cp target/debug/libadjutant_{}.so plugins-built/ && reload",
                        name.replace('-', "_")
                    );
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            }
        }
        Some("test-plugin") => {
            let cli = config::CliArgs::parse(args[1..].iter().cloned()).unwrap_or_default();
            let cfg = config::load(&cli)?;
            // Diagnosable output: subcommands bypass the serve-path tracing init.
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| cfg.log_filter.clone().into()),
                )
                .with_writer(std::io::stderr)
                .init();
            match adjutant_server::cli::run_test_plugin(&cfg).await {
                Ok(probes) => {
                    let mut failed = 0;
                    println!("{:5} {:6} {:4}  PROBE", "RESULT", "STATUS", "EXPECT");
                    for p in &probes {
                        println!(
                            "{} {:>5} {:>6} {:>4}  {}{}",
                            if p.ok { "PASS" } else { "FAIL" },
                            if p.ok { "" } else { "x" },
                            p.status,
                            p.expect,
                            p.name,
                            if p.detail.is_empty() {
                                String::new()
                            } else {
                                format!("  ({})", p.detail)
                            }
                        );
                        if !p.ok {
                            failed += 1;
                        }
                    }
                    // Probes that were never executed (open mutating routes)
                    // are counted separately — they are not evidence.
                    let skipped = probes.iter().filter(|p| p.expect == "skipped").count();
                    let passed = probes.len() - failed - skipped;
                    println!(
                        "\n{passed}/{} probes passed against test database ({skipped} skipped, not probed)",
                        probes.len() - skipped
                    );
                    if failed > 0 {
                        std::process::exit(1);
                    }
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            }
        }
        Some("serve") => {}
        Some("-h") | Some("--help") | Some("help") => {
            print!("{}", config::USAGE);
            return Ok(());
        }
        Some(other) if !other.starts_with('-') => {
            eprintln!("error: unknown command {other:?}\n\n{}", config::USAGE);
            std::process::exit(2);
        }
        _ => {} // flag-style args → serve (back-compat)
    }

    // CLI first: --help must work without a database.
    let cli = CliArgs::parse(args).unwrap_or_else(|e| {
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

/// Walk up from cwd to the directory holding the workspace Cargo.toml
/// (so `adjutant new-plugin` works from anywhere inside the repo).
fn find_repo_root() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let mut dir = std::env::current_dir()?;
    loop {
        let ws = dir.join("Cargo.toml");
        if let Ok(content) = std::fs::read_to_string(&ws) {
            if content.contains("[workspace]") {
                return Ok(dir);
            }
        }
        if !dir.pop() {
            return Err("not inside an adjutant workspace (no [workspace] Cargo.toml found)".into());
        }
    }
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
