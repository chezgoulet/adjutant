//! Where a deployment's secrets live.
//!
//! Never the database. `core.plugins.config` is admin-editable JSONB, it is
//! copied into every backup bundle, and it is readable by anything that can read
//! the row — so a `sk_live_…` or a `whsec_…` in that column is a payment
//! credential in a backup archive. A secret belongs to the deployment, not to
//! the row: it comes from the environment or from a file the operator controls,
//! and the core injects it into the plugin's config at load time.
//!
//! Two sources, environment first:
//!
//! * `ADJUTANT_SECRET_<PLUGIN>_<PATH>` — plugin id and the dotted config path,
//!   upper-cased with `.` and `-` flattened to `_`. `stripe`'s `secret_key` is
//!   `ADJUTANT_SECRET_STRIPE_SECRET_KEY`; the auth plugin's
//!   `oidc.client_secret` is `ADJUTANT_SECRET_AUTH_OIDC_CLIENT_SECRET`.
//! * A TOML file (`ADJUTANT_SECRETS_FILE`, `--secrets-file`, or `./secrets.toml`):
//!
//!   ```toml
//!   [stripe]
//!   secret_key = "sk_live_…"
//!   webhook_secret = "whsec_…"
//!
//!   [auth.oidc]
//!   client_secret = "…"
//!   ```
//!
//! The file is re-read on every plugin load, so a rotated secret takes effect on
//! `POST /api/plugins/reload` without a restart. Nothing here is ever logged or
//! returned: [`SecretSource`] hands values to the plugin's config and keeps no
//! other copy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Environment variable naming the secrets file.
pub const SECRETS_FILE_ENV: &str = "ADJUTANT_SECRETS_FILE";

/// Prefix for a single secret in the environment. The rest is the plugin id and
/// the dotted config path, upper-cased.
pub const SECRET_ENV_PREFIX: &str = "ADJUTANT_SECRET_";

/// The file a deployment's secrets live in when nothing names another.
pub const DEFAULT_SECRETS_FILE: &str = "secrets.toml";

/// Where secrets come from. Cheap to clone, holds no secrets of its own beyond
/// the parsed file.
#[derive(Debug, Default, Clone)]
pub struct SecretSource {
    path: Option<PathBuf>,
    /// `plugin → dotted path → value`, flattened once at load.
    file: BTreeMap<String, BTreeMap<String, String>>,
}

impl SecretSource {
    /// Load from `explicit`, else `ADJUTANT_SECRETS_FILE`, else
    /// [`DEFAULT_SECRETS_FILE`] if that file exists.
    ///
    /// A file that is absent is not an error — a deployment may keep everything
    /// in the environment. A file that exists but does not parse **is** an
    /// error: silently running with no secrets after a typo is how a plugin
    /// boots "configured" and fails at the first payment.
    pub fn load(explicit: Option<&Path>) -> Result<Self, String> {
        let path = resolved_path(explicit);
        let mut source = Self {
            path,
            file: BTreeMap::new(),
        };
        source.read()?;
        Ok(source)
    }

    /// The file being read, for an error message that names it.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Re-read the file. Called on every plugin load, so `reload` re-resolves.
    pub fn read(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            self.file.clear();
            return Ok(());
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // Absent is fine; unreadable is not (permissions, a directory, EIO).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.file.clear();
                return Ok(());
            }
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        let value: toml::Value = toml::from_str(&text)
            .map_err(|e| format!("parse {}: {e}", path.display()))?;
        self.file = flatten(&value);
        Ok(())
    }

    /// The value of `dotted` in `plugin`'s section, environment first.
    ///
    /// `None` covers "not configured anywhere" and "configured as a non-string":
    /// the caller decides whether an unconfigured secret is fatal (most plugins
    /// already fail closed at the point of use).
    pub fn resolve(&self, plugin: &str, dotted: &str) -> Option<String> {
        if let Ok(value) = std::env::var(Self::env_name(plugin, dotted)) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
        self.file
            .get(plugin)
            .and_then(|section| section.get(dotted))
            .cloned()
    }

    /// Whether the operator configured this secret anywhere.
    pub fn has(&self, plugin: &str, dotted: &str) -> bool {
        self.resolve(plugin, dotted).is_some()
    }

    /// `stripe` + `webhook_secret` → `ADJUTANT_SECRET_STRIPE_WEBHOOK_SECRET`.
    pub fn env_name(plugin: &str, dotted: &str) -> String {
        let flatten = |s: &str| {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_uppercase()
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        };
        format!(
            "{SECRET_ENV_PREFIX}{}_{}",
            flatten(plugin),
            flatten(dotted).trim_matches('_')
        )
    }
}

/// `[plugin] a.b = "x"` → `plugin → "a.b" → "x"`.
fn flatten(root: &toml::Value) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let Some(table) = root.as_table() else {
        return out;
    };
    for (plugin, section) in table {
        let mut flat = BTreeMap::new();
        flatten_into(section, "", &mut flat);
        if !flat.is_empty() {
            out.insert(plugin.clone(), flat);
        }
    }
    out
}

fn flatten_into(value: &toml::Value, prefix: &str, out: &mut BTreeMap<String, String>) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                let dotted = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_into(child, &dotted, out);
            }
        }
        toml::Value::String(text) => {
            if !prefix.is_empty() {
                out.insert(prefix.to_string(), text.clone());
            }
        }
        toml::Value::Integer(_) | toml::Value::Boolean(_) if prefix.is_empty() => {}
        toml::Value::Integer(n) => {
            out.insert(prefix.to_string(), n.to_string());
        }
        toml::Value::Boolean(b) => {
            out.insert(prefix.to_string(), b.to_string());
        }
        // Arrays of secrets are not a shape this supports: one path, one value.
        _ => {}
    }
}

/// Where the secrets file is, without reading it.
///
/// `explicit` (the config's `secrets_file`, which already folds in the flag and
/// `ADJUTANT_SECRETS_FILE`) wins; then the environment on its own, for the CLI
/// paths that build a `Config` from defaults; then `./secrets.toml` when it
/// exists.
pub fn resolved_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    if let Ok(value) = std::env::var(SECRETS_FILE_ENV) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    let default = PathBuf::from(DEFAULT_SECRETS_FILE);
    default.exists().then_some(default)
}

/// Write one plugin's database password into the secrets file, creating the
/// file (`0600`) and the section when needed.
///
/// A read-modify-write rather than a rewrite: the same file holds the operator's
/// Stripe keys and OIDC secret, and `bootstrap-isolation` must not delete what it
/// did not put there.
pub fn write_plugin_password(path: &Path, plugin: &str, password: &str) -> Result<(), String> {
    let mut root: toml::Value = match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(toml::map::Map::new())
        }
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let table = root
        .as_table_mut()
        .ok_or_else(|| format!("{}: the file must be a TOML table", path.display()))?;
    let section = table
        .entry(plugin.to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let section = section
        .as_table_mut()
        .ok_or_else(|| format!("{}: [{plugin}] must be a table", path.display()))?;
    section.insert(
        "db_password".to_string(),
        toml::Value::String(password.to_string()),
    );
    let rendered = toml::to_string_pretty(&root)
        .map_err(|e| format!("render {}: {e}", path.display()))?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
    }
    // Written through a temp file in the same directory, then renamed: a crash
    // mid-write must not leave a truncated secrets file that a restart then
    // refuses to parse.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, rendered).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("install {}: {e}", path.display()))?;
    Ok(())
}

/// Read a dotted path out of a JSON config (`oidc.client_secret`).
pub fn config_get<'a>(config: &'a serde_json::Value, dotted: &str) -> Option<&'a serde_json::Value> {
    let mut node = config;
    for segment in dotted.split('.') {
        node = node.get(segment)?;
    }
    Some(node)
}

/// Read a dotted path as a non-empty string.
pub fn config_secret_present(config: &serde_json::Value, dotted: &str) -> bool {
    config_get(config, dotted)
        .and_then(|v| v.as_str())
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Write a dotted path into a JSON config, creating intermediate objects.
///
/// Used only to inject a resolved secret; the object is what the plugin receives
/// as `ctx.config`, so a nested path must agree with the shape the plugin parses.
pub fn config_set(config: &mut serde_json::Value, dotted: &str, value: String) {
    if !config.is_object() {
        *config = serde_json::json!({});
    }
    let segments: Vec<&str> = dotted.split('.').collect();
    let mut node = config;
    for segment in &segments[..segments.len() - 1] {
        if !node.get(*segment).map(|v| v.is_object()).unwrap_or(false) {
            node[*segment] = serde_json::json!({});
        }
        node = &mut node[*segment];
    }
    node[segments[segments.len() - 1]] = serde_json::Value::String(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_with(text: &str) -> SecretSource {
        let dir = std::env::temp_dir().join(format!(
            "adjutant-secrets-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secrets.toml");
        std::fs::write(&path, text).unwrap();
        SecretSource::load(Some(&path)).expect("load")
    }

    #[test]
    fn reads_a_plugins_flat_and_nested_secrets() {
        let source = source_with(
            "[stripe]\nsecret_key = \"sk_test_abc\"\nwebhook_secret = \"whsec_def\"\n\n[auth.oidc]\nclient_secret = \"shh\"\n",
        );
        assert_eq!(
            source.resolve("stripe", "secret_key").as_deref(),
            Some("sk_test_abc")
        );
        assert_eq!(
            source.resolve("stripe", "webhook_secret").as_deref(),
            Some("whsec_def")
        );
        assert_eq!(
            source.resolve("auth", "oidc.client_secret").as_deref(),
            Some("shh")
        );
        assert_eq!(source.resolve("stripe", "nope"), None);
        assert_eq!(source.resolve("nobody", "secret_key"), None);
    }

    #[test]
    fn an_absent_file_is_empty_not_an_error() {
        let dir = std::env::temp_dir().join(format!("adjutant-secrets-absent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = SecretSource::load(Some(&dir.join("missing.toml"))).expect("absent is fine");
        assert_eq!(source.resolve("stripe", "secret_key"), None);
    }

    #[test]
    fn a_malformed_file_is_an_error() {
        let dir = std::env::temp_dir().join(format!("adjutant-secrets-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secrets.toml");
        std::fs::write(&path, "[stripe\nsecret_key = ").unwrap();
        let err = SecretSource::load(Some(&path)).expect_err("must not boot on a typo");
        assert!(err.contains("parse"), "{err}");
    }

    #[test]
    fn config_paths_are_read_and_written_without_touching_siblings() {
        let mut config = serde_json::json!({
            "issuer": "https://idp.example",
            "oidc": { "client_id": "adjutant" }
        });
        assert!(config_get(&config, "issuer").is_some());
        assert!(config_get(&config, "oidc.client_secret").is_none());
        config_set(&mut config, "oidc.client_secret", "shh".into());
        config_set(&mut config, "webhook_secret", "whsec".into());
        assert_eq!(
            config_get(&config, "oidc.client_secret").and_then(|v| v.as_str()),
            Some("shh")
        );
        assert_eq!(
            config_get(&config, "oidc.client_id").and_then(|v| v.as_str()),
            Some("adjutant"),
            "a sibling must survive the write"
        );
        assert!(config_secret_present(&config, "webhook_secret"));
        assert!(!config_secret_present(&config, "missing"));
        assert!(!config_secret_present(
            &serde_json::json!({ "secret_key": "   " }),
            "secret_key"
        ));
    }

    #[test]
    fn writing_a_password_keeps_the_operators_other_secrets() {
        let dir = std::env::temp_dir().join(format!("adjutant-secrets-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secrets.toml");
        std::fs::write(&path, "[stripe]\nsecret_key = \"sk_test_keep\"\n").unwrap();
        write_plugin_password(&path, "finance", "pw-1").unwrap();
        let source = SecretSource::load(Some(&path)).unwrap();
        assert_eq!(source.resolve("finance", "db_password").as_deref(), Some("pw-1"));
        assert_eq!(
            source.resolve("stripe", "secret_key").as_deref(),
            Some("sk_test_keep"),
            "an unrelated section must survive the write"
        );
        // Rewriting replaces the password and nothing else.
        write_plugin_password(&path, "finance", "pw-2").unwrap();
        let source = SecretSource::load(Some(&path)).unwrap();
        assert_eq!(source.resolve("finance", "db_password").as_deref(), Some("pw-2"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the secrets file must not be world-readable");
        }
    }

    #[test]
    fn env_names_are_derived_from_plugin_and_path() {
        assert_eq!(
            SecretSource::env_name("stripe", "secret_key"),
            "ADJUTANT_SECRET_STRIPE_SECRET_KEY"
        );
        assert_eq!(
            SecretSource::env_name("auth", "oidc.client_secret"),
            "ADJUTANT_SECRET_AUTH_OIDC_CLIENT_SECRET"
        );
    }

    #[test]
    fn reload_picks_up_a_rotated_secret() {
        let dir = std::env::temp_dir().join(format!("adjutant-secrets-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secrets.toml");
        std::fs::write(&path, "[stripe]\nsecret_key = \"one\"\n").unwrap();
        let mut source = SecretSource::load(Some(&path)).unwrap();
        assert_eq!(source.resolve("stripe", "secret_key").as_deref(), Some("one"));
        std::fs::write(&path, "[stripe]\nsecret_key = \"two\"\n").unwrap();
        source.read().expect("re-read");
        assert_eq!(source.resolve("stripe", "secret_key").as_deref(), Some("two"));
    }
}
