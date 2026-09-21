//! Server configuration. Env-driven; every field has a development default.

use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// Address to bind the HTTP server to.
    pub bind: SocketAddr,
    /// PostgreSQL connection string.
    pub database_url: String,
    /// Directory scanned for plugin `cdylib`s at boot.
    pub plugin_dir: PathBuf,
    /// Max request body size in bytes.
    pub max_body_bytes: usize,
    /// Tracing filter (e.g. `info,adjutant_server=debug`).
    pub log_filter: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8787".parse().expect("valid default bind"),
            database_url: "postgres://adjutant@127.0.0.1:5433/adjutant_dev".into(),
            plugin_dir: PathBuf::from("plugins-built"),
            max_body_bytes: 1024 * 1024, // 1 MiB
            log_filter: "info,adjutant_server=debug".into(),
        }
    }
}

impl Config {
    /// Overlay `ADJUTANT_*` environment variables.
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("ADJUTANT_BIND") {
            c.bind = v.parse().expect("ADJUTANT_BIND must be host:port");
        }
        if let Ok(v) = std::env::var("ADJUTANT_DATABASE_URL") {
            c.database_url = v;
        }
        if let Ok(v) = std::env::var("ADJUTANT_PLUGIN_DIR") {
            c.plugin_dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("ADJUTANT_LOG") {
            c.log_filter = v;
        }
        c
    }
}
