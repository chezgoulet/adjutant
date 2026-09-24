//! Configuration: defaults < config file (TOML) < environment < CLI flags.
//!
//! Every knob is reachable through at least two mechanisms (SPEC §15 M2:
//! "configuration management (file, environment, CLI)"). Precedence is
//! deliberately boring: later sources override earlier ones.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
}

impl LogFormat {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "pretty" | "text" => Ok(LogFormat::Pretty),
            "json" => Ok(LogFormat::Json),
            other => Err(format!("invalid log format {other:?}: expected pretty|json")),
        }
    }
}

/// Fixed-window rate limiting. `max_requests == 0` disables limiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateConfig {
    pub window_secs: u64,
    pub max_requests: u32,
}

impl Default for RateConfig {
    fn default() -> Self {
        Self { window_secs: 60, max_requests: 120 }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub database_url: String,
    pub plugin_dir: PathBuf,
    pub max_body_bytes: usize,
    pub log_filter: String,
    pub log_format: LogFormat,
    pub rate: RateConfig,
    /// Empty = no CORS layer (same-origin only). `["*"]` allows everyone.
    pub cors_origins: Vec<String>,
    /// Dev identity headers (`x-dev-user`/`x-dev-role`). Trivially spoofable —
    /// **off by default**; enabling it is an explicit opt-in (`--allow-dev-headers`,
    /// `ADJUTANT_DEV_HEADERS=true`, or `[auth] allow_dev_headers = true`). Only
    /// consulted when no plugin identity provider answered (SPEC §7.1).
    pub allow_dev_headers: bool,
    /// Direct peers whose `x-forwarded-for` header may be trusted for rate
    /// limiting (reverse-proxy IPs). Empty = ignore the header entirely.
    pub trusted_proxies: Vec<String>,
    /// Explicit opt-out of the superuser boot refusal (`ADJUTANT_ALLOW_SUPERUSER`).
    /// A superuser connection turns a plugin escape into total compromise
    /// (design plugin-isolation §3.6), so the default is to refuse to boot; this
    /// exists only for a throwaway/dev database.
    pub allow_superuser: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8787".parse().expect("valid default bind"),
            database_url: "postgres://adjutant@127.0.0.1:5433/adjutant_dev".into(),
            plugin_dir: PathBuf::from("plugins-built"),
            max_body_bytes: 1024 * 1024, // 1 MiB
            log_filter: "info,adjutant_server=debug".into(),
            log_format: LogFormat::Pretty,
            rate: RateConfig::default(),
            cors_origins: Vec::new(),
            allow_dev_headers: false,
            trusted_proxies: Vec::new(),
            allow_superuser: false,
        }
    }
}

// --- config file (every field optional; absent = inherit lower layer) --------

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    bind: Option<String>,
    database_url: Option<String>,
    plugin_dir: Option<String>,
    max_body_bytes: Option<usize>,
    log_filter: Option<String>,
    log_format: Option<String>,
    rates: Option<FileRates>,
    cors: Option<FileCors>,
    auth: Option<FileAuth>,
    trusted_proxies: Option<Vec<String>>,
    allow_superuser: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct FileRates {
    window_secs: Option<u64>,
    max_requests: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct FileCors {
    origins: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct FileAuth {
    allow_dev_headers: Option<bool>,
}

// --- CLI --------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct CliArgs {
    pub config: Option<PathBuf>,
    pub bind: Option<String>,
    pub database_url: Option<String>,
    pub plugin_dir: Option<String>,
    pub log_filter: Option<String>,
    pub log_format: Option<String>,
    /// Explicit opt-in to the spoofable dev identity headers (dev only).
    pub allow_dev_headers: bool,
    /// Explicit opt-out of the superuser boot refusal (dev/throwaway only).
    pub allow_superuser: bool,
    pub help: bool,
}

pub const USAGE: &str = "\
adjutant — sovereignty-first troop administration server

USAGE:
    adjutant [serve] [OPTIONS]     start the server (default command)
    adjutant new-plugin <name>     scaffold plugins/<name> (compiling stub)
    adjutant validate-plugin <so>  statically validate a compiled plugin (no DB)
    adjutant test-plugin [OPTIONS] boot against a pristine test DB and probe
                                   every plugin route with mock permissions
    adjutant bootstrap-isolation [--rotate] [OPTIONS]
                                   create/refresh per-plugin DB roles, schema
                                   ownership, grants and stored credentials
                                   (run once by the operator, against an admin URL)
    adjutant --help                print this help

SERVE OPTIONS:
    --config <PATH>        TOML config file (default: ./adjutant.toml if present)
    --bind <ADDR>          listen address, e.g. 127.0.0.1:8787
    --database-url <URL>   PostgreSQL connection string
    --plugin-dir <PATH>    directory scanned for plugin cdylibs at boot/reload
    --log <FILTER>         tracing filter, e.g. info,adjutant_server=debug
    --log-format <FMT>     pretty | json
    --allow-dev-headers    enable the spoofable x-dev-user/x-dev-role stub
                           (DEVELOPMENT ONLY; off by default)
    --allow-superuser      allow booting on a PostgreSQL superuser connection
                           (DEVELOPMENT/throwaway ONLY; off by default)
    -h, --help             print this help

BOOTSTRAP-ISOLATION OPTIONS:
    --database-url <URL>   admin connection string (needs CREATEROLE)
    --plugin-dir <PATH>    the plugins to create roles for
    --rotate               replace stored passwords (default: preserve them)
    --app-role <NAME>      also create a dedicated application role (no superuser,
                           no CREATEROLE) that owns the core schema and holds the
                           plugin roles; serve as it to avoid a superuser
    --app-password <PW>    set the application role's password (default: leave it)

ENVIRONMENT (override file, overridden by flags):
    ADJUTANT_CONFIG, ADJUTANT_BIND, ADJUTANT_DATABASE_URL, ADJUTANT_PLUGIN_DIR,
    ADJUTANT_LOG, ADJUTANT_LOG_FORMAT, ADJUTANT_RATE_MAX, ADJUTANT_RATE_WINDOW,
    ADJUTANT_CORS (comma-separated origins), ADJUTANT_MAX_BODY,
    ADJUTANT_DEV_HEADERS (true enables the dev stub; off by default),
    ADJUTANT_TRUSTED_PROXIES, ADJUTANT_ALLOW_SUPERUSER (true permits a superuser
    connection; off by default)
";

impl CliArgs {
    /// Parse `argv[1..]`. Returns `Err` with a message on bad usage.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Self, String> {
        let mut out = Self::default();
        let mut it = args.into_iter().peekable();
        // USAGE advertises `adjutant [serve] [OPTIONS]`; the optional subcommand is
        // not a flag, and main() matches it without stripping it from argv.
        if it.peek().map(|a| a == "serve").unwrap_or(false) {
            it.next();
        }
        while let Some(arg) = it.next() {
            // support `--flag value` and `--flag=value`
            let (flag, inline) = match arg.split_once('=') {
                Some((f, v)) => (f.to_string(), Some(v.to_string())),
                None => (arg.clone(), None),
            };
            let mut take = |name: &str| -> Result<String, String> {
                match inline.clone() {
                    Some(v) => Ok(v),
                    None => it.next().ok_or_else(|| format!("{name} requires a value")),
                }
            };
            match flag.as_str() {
                "-h" | "--help" => out.help = true,
                "--config" => out.config = Some(PathBuf::from(take("--config")?)),
                "--bind" => out.bind = Some(take("--bind")?),
                "--database-url" => out.database_url = Some(take("--database-url")?),
                "--plugin-dir" => out.plugin_dir = Some(take("--plugin-dir")?),
                "--log" => out.log_filter = Some(take("--log")?),
                "--log-format" => out.log_format = Some(take("--log-format")?),
                "--allow-dev-headers" => out.allow_dev_headers = true,
                "--allow-superuser" => out.allow_superuser = true,
                other => return Err(format!("unknown argument {other:?} (try --help)")),
            }
        }
        Ok(out)
    }
}

// --- layered load -----------------------------------------------------------

/// Merge all sources. Order: defaults → file → environment → CLI.
pub fn load(cli: &CliArgs) -> Result<Config, String> {
    let mut cfg = Config::default();

    // 1. file (explicit --config, ADJUTANT_CONFIG, or ./adjutant.toml if present)
    let path = cli
        .config
        .clone()
        .or_else(|| std::env::var("ADJUTANT_CONFIG").ok().map(PathBuf::from))
        .or_else(|| {
            let p = PathBuf::from("adjutant.toml");
            p.exists().then_some(p)
        });
    if let Some(p) = path {
        merge_file(&mut cfg, &p)?;
    }

    // 2. environment
    if let Ok(v) = std::env::var("ADJUTANT_BIND") {
        cfg.bind = v.parse().map_err(|e| format!("ADJUTANT_BIND: {e}"))?;
    }
    if let Ok(v) = std::env::var("ADJUTANT_DATABASE_URL") {
        cfg.database_url = v;
    }
    if let Ok(v) = std::env::var("ADJUTANT_PLUGIN_DIR") {
        cfg.plugin_dir = PathBuf::from(v);
    }
    if let Ok(v) = std::env::var("ADJUTANT_LOG") {
        cfg.log_filter = v;
    }
    if let Ok(v) = std::env::var("ADJUTANT_LOG_FORMAT") {
        cfg.log_format = LogFormat::parse(&v)?;
    }
    if let Ok(v) = std::env::var("ADJUTANT_RATE_MAX") {
        cfg.rate.max_requests = v.parse().map_err(|e| format!("ADJUTANT_RATE_MAX: {e}"))?;
    }
    if let Ok(v) = std::env::var("ADJUTANT_RATE_WINDOW") {
        cfg.rate.window_secs = v.parse().map_err(|e| format!("ADJUTANT_RATE_WINDOW: {e}"))?;
    }
    if let Ok(v) = std::env::var("ADJUTANT_CORS") {
        cfg.cors_origins = split_origins(&v);
    }
    if let Ok(v) = std::env::var("ADJUTANT_MAX_BODY") {
        cfg.max_body_bytes = v.parse().map_err(|e| format!("ADJUTANT_MAX_BODY: {e}"))?;
    }
    if let Ok(v) = std::env::var("ADJUTANT_DEV_HEADERS") {
        cfg.allow_dev_headers = matches!(v.as_str(), "1" | "true" | "yes");
    }
    if let Ok(v) = std::env::var("ADJUTANT_TRUSTED_PROXIES") {
        cfg.trusted_proxies = split_origins(&v);
    }
    if let Ok(v) = std::env::var("ADJUTANT_ALLOW_SUPERUSER") {
        cfg.allow_superuser = matches!(v.as_str(), "1" | "true" | "yes");
    }

    // 3. CLI (highest precedence)
    if let Some(v) = &cli.bind {
        cfg.bind = v.parse().map_err(|e| format!("--bind: {e}"))?;
    }
    if let Some(v) = &cli.database_url {
        cfg.database_url = v.clone();
    }
    if let Some(v) = &cli.plugin_dir {
        cfg.plugin_dir = PathBuf::from(v);
    }
    if let Some(v) = &cli.log_filter {
        cfg.log_filter = v.clone();
    }
    if let Some(v) = &cli.log_format {
        cfg.log_format = LogFormat::parse(v)?;
    }
    if cli.allow_dev_headers {
        cfg.allow_dev_headers = true;
    }
    if cli.allow_superuser {
        cfg.allow_superuser = true;
    }

    Ok(cfg)
}

fn split_origins(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|o| !o.is_empty()).map(String::from).collect()
}

fn merge_file(cfg: &mut Config, path: &Path) -> Result<(), String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
    let f: FileConfig =
        toml::from_str(&raw).map_err(|e| format!("cannot parse config {}: {e}", path.display()))?;

    if let Some(v) = f.bind {
        cfg.bind = v.parse().map_err(|e| format!("bind: {e}"))?;
    }
    if let Some(v) = f.database_url {
        cfg.database_url = v;
    }
    if let Some(v) = f.plugin_dir {
        cfg.plugin_dir = PathBuf::from(v);
    }
    if let Some(v) = f.max_body_bytes {
        cfg.max_body_bytes = v;
    }
    if let Some(v) = f.log_filter {
        cfg.log_filter = v;
    }
    if let Some(v) = f.log_format {
        cfg.log_format = LogFormat::parse(&v)?;
    }
    if let Some(r) = f.rates {
        if let Some(v) = r.window_secs {
            cfg.rate.window_secs = v;
        }
        if let Some(v) = r.max_requests {
            cfg.rate.max_requests = v;
        }
    }
    if let Some(c) = f.cors {
        if let Some(v) = c.origins {
            cfg.cors_origins = v;
        }
    }
    if let Some(a) = f.auth {
        if let Some(v) = a.allow_dev_headers {
            cfg.allow_dev_headers = v;
        }
    }
    if let Some(v) = f.trusted_proxies {
        cfg.trusted_proxies = v;
    }
    if let Some(v) = f.allow_superuser {
        cfg.allow_superuser = v;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Precedence must hold: default < file < env < CLI. Runs all cases in one
    /// test so env-var mutation can't race between parallel tests.
    #[test]
    fn precedence_default_file_env_cli() {
        // default
        let d = Config::default();
        assert_eq!(d.bind.to_string(), "127.0.0.1:8787");
        assert_eq!(d.rate.max_requests, 120);
        assert!(d.cors_origins.is_empty());

        // file overrides default
        let dir = std::env::temp_dir().join(format!("adjutant-cfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("adjutant.toml");
        std::fs::write(
            &file,
            "bind = \"0.0.0.0:9000\"\nlog_format = \"json\"\n[rates]\nmax_requests = 5\n[cors]\norigins = [\"https://x.test\"]\n",
        )
        .unwrap();
        let cli = CliArgs { config: Some(file.clone()), ..Default::default() };
        let c = load(&cli).unwrap();
        assert_eq!(c.bind.to_string(), "0.0.0.0:9000");
        assert_eq!(c.log_format, LogFormat::Json);
        assert_eq!(c.rate.max_requests, 5);
        assert_eq!(c.cors_origins, vec!["https://x.test".to_string()]);

        // env overrides file
        std::env::set_var("ADJUTANT_RATE_MAX", "9");
        let c = load(&cli).unwrap();
        assert_eq!(c.rate.max_requests, 9, "env must beat file");
        std::env::remove_var("ADJUTANT_RATE_MAX");

        // CLI beats env
        std::env::set_var("ADJUTANT_BIND", "1.2.3.4:1111");
        let cli2 = CliArgs { bind: Some("5.6.7.8:2222".into()), ..cli.clone() };
        let c = load(&cli2).unwrap();
        assert_eq!(c.bind.to_string(), "5.6.7.8:2222", "cli must beat env");
        std::env::remove_var("ADJUTANT_BIND");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_headers_default_off_and_require_explicit_opt_in() {
        // The spoofable stub must not be silently available.
        assert!(!Config::default().allow_dev_headers);

        let dir = std::env::temp_dir().join(format!("adjutant-cfg-dev-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("adjutant.toml");
        std::fs::write(&file, "[auth]\nallow_dev_headers = true\n").unwrap();

        // File opt-in.
        let c = load(&CliArgs { config: Some(file.clone()), ..Default::default() }).unwrap();
        assert!(c.allow_dev_headers, "file must be able to opt in");

        // CLI flag is the highest layer: it beats an env that disables.
        std::env::set_var("ADJUTANT_DEV_HEADERS", "false");
        let c = load(&CliArgs {
            config: Some(file.clone()),
            allow_dev_headers: true,
            ..Default::default()
        })
        .unwrap();
        assert!(c.allow_dev_headers, "flag must beat env");
        std::env::remove_var("ADJUTANT_DEV_HEADERS");

        // And the flag parses.
        assert!(CliArgs::parse(vec!["--allow-dev-headers".to_string()]).unwrap().allow_dev_headers);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serve_subcommand_is_optional_and_not_a_flag() {
        // `adjutant serve --bind 1.2.3.4:1` and `adjutant --bind 1.2.3.4:1` must
        // behave identically; the bare `serve` form used to exit 2.
        let with = CliArgs::parse(vec!["serve".to_string(), "--bind".to_string(), "1.2.3.4:1".to_string()])
            .expect("serve form parses");
        let without = CliArgs::parse(vec!["--bind".to_string(), "1.2.3.4:1".to_string()])
            .expect("flag form parses");
        assert_eq!(with.bind, without.bind);
        assert!(CliArgs::parse(vec!["serve".to_string()]).is_ok());
        // A positional word that is not `serve` is still rejected.
        assert!(CliArgs::parse(vec!["dance".to_string()]).is_err());
    }

    #[test]
    fn cli_parses_flags_and_equals_form() {
        let args = vec![
            "--bind".to_string(),
            "0.0.0.0:1".to_string(),
            "--log-format=json".to_string(),
        ];
        let c = CliArgs::parse(args).unwrap();
        assert_eq!(c.bind.as_deref(), Some("0.0.0.0:1"));
        assert_eq!(c.log_format.as_deref(), Some("json"));
        assert!(CliArgs::parse(vec!["--nope".to_string()]).is_err());
        assert!(CliArgs::parse(vec!["--bind".to_string()]).is_err());
    }

    #[test]
    fn log_format_rejects_garbage() {
        assert!(LogFormat::parse("json").is_ok());
        assert!(LogFormat::parse("yaml").is_err());
    }
}
