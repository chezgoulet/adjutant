//! DB-gated probes for the backup feature (#88), against a real database and a
//! real `pg_dump`.
//!
//! This is the file the feature needed before it could be called done. The routes,
//! the timer, the dump and the pruning were written and never executed — the unit
//! tests beside them cover a path-guard and a constant, and nothing else had run.
//!
//! It exercises the **real router** (`build_app`) over a real socket, so what is
//! asserted is what an admin's request would produce, and the dump it produces is
//! a real archive from the `pg_dump` on `PATH` rather than a stub.
//!
//! Run with `ADJUTANT_TEST_DATABASE_URL=…` and `-- --ignored`. A missing, empty or
//! unreachable URL is a hard failure (issue #25).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use adjutant_server::config::Config;

/// Serializes the probes: they set the process-wide backup directory, and two of
/// them sharing it would be two tests writing to one place.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn base_url() -> String {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").expect(
        "ADJUTANT_TEST_DATABASE_URL must be set to run the backup probes \
         (they are #[ignore]d; pass `-- --ignored` and set the variable)",
    );
    assert!(
        !url.trim().is_empty(),
        "ADJUTANT_TEST_DATABASE_URL is set but empty; set it to a _test database or unset it"
    );
    url
}

/// A scratch plugin dir and a scratch backup dir, under names that include the
/// test's tag so a failing run leaves its evidence behind rather than overwriting
/// the last one.
fn scratch(tag: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("adjutant-backups-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let plugins = root.join("plugins");
    let backups = root.join("backups");
    std::fs::create_dir_all(&plugins).expect("plugin dir");
    std::fs::create_dir_all(&backups).expect("backup dir");
    (plugins, backups)
}

/// Migrate, then empty the backup ledger and reset the schedule.
///
/// The two probes share one test database, and `core.backups` otherwise carries
/// the other's rows — so a probe reading the list would be asserting on whatever
/// ran first. (Found exactly that way: the retention probe's list held two runs,
/// one of them the other probe's, whose bundle was in a different directory.) The
/// sibling probe file solves the same problem by starting its fixture from
/// nothing; this is the same discipline for a table rather than a schema.
async fn fresh_ledger(url: &str, plugins: &Path) -> Arc<sqlx::PgPool> {
    let cfg = Config {
        database_url: url.to_string(),
        plugin_dir: plugins.to_path_buf(),
        ..Default::default()
    };
    let pool = adjutant_server::db::connect_and_migrate(&cfg)
        .await
        .expect("core migrations on the test database");
    sqlx::query("DELETE FROM core.backups")
        .execute(pool.as_ref())
        .await
        .expect("clear the backup ledger");
    sqlx::query(
        "UPDATE core.backup_schedule SET cadence_secs = NULL, keep = 14, enabled = false",
    )
    .execute(pool.as_ref())
    .await
    .expect("reset the schedule");
    pool
}

/// Boot the real application on an ephemeral port, with its backup directory
/// pointed at `backups`.
async fn spawn_app(plugins: &Path, backups: &Path, url: &str) -> (String, tokio::task::JoinHandle<()>) {
    // Process-wide, because `backup::backup_dir()` and the encryption settings
    // read the environment the same way the server does. The caller holds SERIAL
    // for the whole test.
    std::env::set_var("ADJUTANT_BACKUP_DIR", backups);
    // These probes assert the dump/retention/download mechanics, not age; a test
    // host is not required to have a key pair. The opt-in is explicit, and the
    // fail-closed default has its own probe below.
    std::env::remove_var("ADJUTANT_BACKUP_AGE_RECIPIENT");
    std::env::set_var("ADJUTANT_BACKUP_ALLOW_PLAINTEXT", "true");

    let cfg = Config {
        database_url: url.to_string(),
        plugin_dir: plugins.to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind address"),
        allow_dev_headers: true,
        allow_superuser: true,
        // The probes are the only thing on this database; no limiter to trip.
        rate: adjutant_server::config::RateConfig {
            window_secs: 60,
            max_requests: 0,
        },
        ..Default::default()
    };
    let (app, _state) = adjutant_server::build_app(&cfg)
        .await
        .expect("the real app boots");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("an ephemeral port");
    let addr = listener.local_addr().expect("local address");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), handle)
}

async fn post(base: &str, path: &str) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header("x-dev-user", "christopher")
        .header("x-dev-role", "chief")
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {path}: {e}"));
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn put(base: &str, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .put(format!("{base}{path}"))
        .header("x-dev-user", "christopher")
        .header("x-dev-role", "chief")
        .json(&body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("PUT {path}: {e}"));
    let status = resp.status().as_u16();
    let parsed = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, parsed)
}

async fn get(base: &str, path: &str) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .get(format!("{base}{path}"))
        .header("x-dev-user", "christopher")
        .header("x-dev-role", "chief")
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {path}: {e}"));
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// The bundle bytes, for the one route that answers with an archive rather than
/// JSON.
async fn download(base: &str, filename: &str) -> (u16, Vec<u8>) {
    let resp = reqwest::Client::new()
        .get(format!("{base}/api/backups/{filename}/download"))
        .header("x-dev-user", "christopher")
        .header("x-dev-role", "chief")
        .send()
        .await
        .unwrap_or_else(|e| panic!("download {filename}: {e}"));
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
    (status, bytes)
}

fn bundles(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("backup dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".dump"))
        .collect();
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_a_manual_run_produces_a_bundle_the_admin_can_download() {
    let _serial = SERIAL.lock().await;
    let (plugins, backups) = scratch("manual");
    let url = base_url();
    let _ledger = fresh_ledger(&url, &plugins).await;
    let (base, _serve) = spawn_app(&plugins, &backups, &url).await;

    // --- the list, before anything has run ---------------------------------
    let (status, body) = get(&base, "/api/backups").await;
    assert_eq!(status, 200, "the list is reachable: {body}");
    assert_eq!(body["runs"].as_array().map(Vec::len), Some(0), "nothing yet");
    assert!(
        body["presets"].as_array().is_some_and(|p| p.len() == 4),
        "the picker's options travel with the list: {body}"
    );
    assert_eq!(
        body["schedule"]["enabled"], serde_json::json!(false),
        "scheduled backups start off, so a fresh deployment does not begin dumping"
    );

    // --- the button ---------------------------------------------------------
    let (status, body) = post(&base, "/api/backups/run").await;
    assert_eq!(status, 200, "a manual run succeeds: {body}");
    let run = &body["run"];
    assert_eq!(run["status"], serde_json::json!("done"), "and finished: {run}");
    assert_eq!(run["trigger"], serde_json::json!("manual"));
    let filename = run["filename"].as_str().expect("a filename").to_string();
    let bytes = run["bytes"].as_i64().expect("a size");
    assert!(bytes > 0, "a zero-byte dump is a failed backup: {run}");
    assert!(
        run["sha256"].as_str().is_some_and(|s| s.len() == 64),
        "the checksum is recorded, not guessed: {run}"
    );

    // The bundle is really on disk, at the size and with the hash the row claims.
    let path = backups.join(&filename);
    assert!(path.is_file(), "{} exists", path.display());
    assert_eq!(
        std::fs::metadata(&path).expect("metadata").len(),
        bytes as u64,
        "the recorded size is the file's"
    );
    assert!(
        path.with_extension("dump.sha256").is_file(),
        "and the sidecar the host script also writes"
    );

    // --- the download -------------------------------------------------------
    let (status, got) = download(&base, &filename).await;
    assert_eq!(status, 200, "the bundle downloads");
    assert_eq!(got.len(), bytes as usize, "and is the whole file");
    assert_eq!(&got[..5], b"PGDMP", "and is a pg_dump archive, not JSON");

    // --- it is in the list, as the manual run it was -------------------------
    let (status, body) = get(&base, "/api/backups").await;
    assert_eq!(status, 200);
    let runs = body["runs"].as_array().expect("runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["trigger"], serde_json::json!("manual"));
    assert_eq!(runs[0]["unrecorded"], serde_json::json!(false));

    // --- a name from a URL is never a path ----------------------------------
    // The route resolves the filename against the backup directory; anything with
    // a separator or a `..` has to be a 404, not a read of something else.
    for bad in ["..%2F..%2Fetc%2Fpasswd", "nope.dump", "..%2Fescape.dump"] {
        let (status, _) = download(&base, bad).await;
        assert_eq!(status, 404, "{bad} must not resolve");
    }

    // --- the schedule validates, in words ------------------------------------
    let (status, body) = put(
        &base,
        "/api/backups/schedule",
        serde_json::json!({ "cadence_secs": 5, "keep": 14, "enabled": true }),
    )
    .await;
    assert_eq!(status, 400, "a cadence under the floor is refused: {body}");
    assert!(
        body["error"].as_str().is_some_and(|e| e.contains("cadence")),
        "and says which field: {body}"
    );

    let (status, body) = put(
        &base,
        "/api/backups/schedule",
        serde_json::json!({ "cadence_secs": 86400, "keep": 0, "enabled": true }),
    )
    .await;
    assert_eq!(status, 400, "retention out of range is refused: {body}");

    let (status, body) = put(
        &base,
        "/api/backups/schedule",
        serde_json::json!({ "cadence_secs": 86400, "keep": 7, "enabled": true }),
    )
    .await;
    assert_eq!(status, 200, "a valid schedule is accepted: {body}");
    assert_eq!(body["schedule"]["cadence_secs"], serde_json::json!(86400));
    assert_eq!(body["schedule"]["enabled"], serde_json::json!(true));

    // Turning it off sends an explicit null, which is a different intention from
    // omitting the key — the route reads absent as "leave the cadence alone".
    let (status, body) = put(
        &base,
        "/api/backups/schedule",
        serde_json::json!({ "cadence_secs": null, "keep": 7, "enabled": false }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["schedule"]["cadence_secs"], serde_json::Value::Null);
    assert_eq!(body["schedule"]["enabled"], serde_json::json!(false));

    let _ = std::fs::remove_dir_all(backups.parent().expect("root"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE + age"]
async fn probe_an_encrypted_run_writes_dump_age() {
    // A throwaway public key: encryption needs no private key, which is the
    // property being asserted. Never a real deployment's key.
    const RECIPIENT: &str = "age1jxz8v8fphxqj8qhpew3akhwsyvqug2fr8fu5n46u26ux624svujqplk9ez";
    if std::process::Command::new("age")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("age is not installed; skipping the encrypted-bundle probe");
        return;
    }

    let _serial = SERIAL.lock().await;
    let (plugins, backups) = scratch("enc");
    let url = base_url();
    let _ledger = fresh_ledger(&url, &plugins).await;
    let (base, _serve) = spawn_app(&plugins, &backups, &url).await;
    std::env::remove_var("ADJUTANT_BACKUP_ALLOW_PLAINTEXT");
    std::env::set_var("ADJUTANT_BACKUP_AGE_RECIPIENT", RECIPIENT);

    let (status, body) = post(&base, "/api/backups/run").await;
    assert_eq!(status, 200, "the encrypted run succeeds: {body}");
    let run = &body["run"];
    assert_eq!(run["status"], serde_json::json!("done"), "{run}");
    let filename = run["filename"].as_str().expect("a filename");
    assert!(
        filename.ends_with(".dump.age"),
        "the name makes the encryption state visible: {filename}"
    );
    let path = backups.join(filename);
    let bytes = std::fs::read(&path).expect("the encrypted bundle");
    assert!(
        bytes.starts_with(b"age-encryption.org/v1"),
        "the bundle is age ciphertext, not a plaintext pg_dump"
    );
    assert!(
        Path::new(&format!("{}.sha256", path.display())).is_file(),
        "the checksum sidecar follows the artifact name"
    );

    let (status, got) = download(&base, filename).await;
    assert_eq!(status, 200, "the encrypted bundle downloads");
    assert_eq!(got, bytes, "and is the whole ciphertext");

    let _ = std::fs::remove_dir_all(backups.parent().expect("root"));
}

/// A throwaway key pair. Encryption needs only the recipient, so the private
/// half proves the bundle is decrypt-only without ever putting a deployment's
/// key in the repository. Generated for this probe; it protects nothing.
const TEST_IDENTITY: &str = "AGE-SECRET-KEY-16PRCWWZZ0JW4M2C657T24MDFXZFXZRA8LPKP3GVET0KH547AR9KQYDLMWU";
const TEST_RECIPIENT: &str = "age1dggq8y3565sdgcwvn8mdyfrvyk893732vn2j07ndhqlfa5uf5y2sl7wc4d";

/// The password a `probe163` plugin role would have. Only the probe writes it.
const PLUGIN_SECRET: &str = "adjutant-163-known-plugin-password-5c1f0a7e";

/// The bundle is a real backup that only a holder of the identity can read.
///
/// This is the #163 assurance in one probe. `core.plugins.db_secret` really is
/// inside the archive (the positive control below decrypts it and finds the
/// password), so the protection is the encryption rather than an accident of
/// `pg_dump` compression. Without the identity the bundle yields nothing; with
/// it, `pg_restore` can read the credential. And because `pg_dump` is piped
/// straight into `age`, there is no plaintext intermediate anywhere in the
/// backup directory — not even one a crash could leave behind.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE + age + pg_restore"]
async fn probe_an_encrypted_bundle_carries_no_plaintext_dump() {
    // Not a silent skip: the CI job installs both, and a probe that quietly
    // passes when the tool it exists to exercise is missing proves nothing.
    for tool in ["age", "pg_restore"] {
        assert!(
            std::process::Command::new(tool)
                .arg("--version")
                .output()
                .is_ok(),
            "{tool} is required by this probe (the DB-backed CI job installs it)"
        );
    }

    let _serial = SERIAL.lock().await;
    let (plugins, backups) = scratch("sealed");
    let url = base_url();
    let _ledger = fresh_ledger(&url, &plugins).await;
    let (base, _serve) = spawn_app(&plugins, &backups, &url).await;

    // A plugin row carrying a password we can look for afterwards. The fixture's
    // plugin directory is empty, so nothing else reads it.
    sqlx::query(
        "INSERT INTO core.plugins (id, version, db_secret) VALUES ('probe163', '0.0.0', $1) \
         ON CONFLICT (id) DO UPDATE SET db_secret = EXCLUDED.db_secret",
    )
    .bind(PLUGIN_SECRET)
    .execute(_ledger.as_ref())
    .await
    .expect("seed a plugin credential");

    std::env::remove_var("ADJUTANT_BACKUP_ALLOW_PLAINTEXT");
    std::env::set_var("ADJUTANT_BACKUP_AGE_RECIPIENT", TEST_RECIPIENT);

    let (status, body) = post(&base, "/api/backups/run").await;
    assert_eq!(status, 200, "the encrypted run succeeds: {body}");
    let run = &body["run"];
    assert_eq!(run["status"], serde_json::json!("done"), "{run}");
    let filename = run["filename"].as_str().expect("a filename");
    let path = backups.join(filename);
    let bundle = std::fs::read(&path).expect("the encrypted bundle");
    assert!(
        bundle.starts_with(b"age-encryption.org/v1"),
        "the bundle is age ciphertext"
    );

    // The pipeline leaves exactly one artifact (plus its sidecar): no `.dump`,
    // no `.dump.*.plaintext`, nothing that carries the archive in the clear.
    let names: Vec<String> = std::fs::read_dir(&backups)
        .expect("backup dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        names.iter().all(|n| n.ends_with(".dump.age") || n.ends_with(".sha256")),
        "no plaintext dump or temporary was left behind: {names:?}"
    );
    // Exact, not best-effort: the secret's bytes are absent from the artifact.
    assert!(
        !bundle.windows(PLUGIN_SECRET.len()).any(|w| w == PLUGIN_SECRET.as_bytes()),
        "the known plugin password must not appear in the bundle"
    );

    // Without the identity the bundle is unreadable — the point of encrypting.
    let identity = backups.join("test-identity.txt");
    std::fs::write(&identity, TEST_IDENTITY).expect("write the test identity");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o600))
            .expect("identity is private");
    }
    let no_key = std::process::Command::new("age")
        .arg("--decrypt")
        .arg(&path)
        .output()
        .expect("run age without an identity");
    assert!(
        !no_key.status.success(),
        "age decrypted the bundle without an identity"
    );

    // Positive control: with the identity this is a complete pg_dump archive,
    // and the credential really is in it. A leak of the plaintext archive is
    // exactly the exposure #163 names; the ciphertext above is why it does not
    // leak with the bundle.
    let decrypted = std::process::Command::new("age")
        .arg("--decrypt")
        .arg("--identity")
        .arg(&identity)
        .arg(&path)
        .output()
        .expect("run age");
    assert!(
        decrypted.status.success(),
        "the test identity decrypts the bundle: {}",
        String::from_utf8_lossy(&decrypted.stderr)
    );
    assert!(
        decrypted.stdout.starts_with(b"PGDMP"),
        "the decrypted stream is a pg_dump custom-format archive"
    );

    let mut restore = std::process::Command::new("pg_restore")
        .args(["--data-only", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run pg_restore");
    {
        use std::io::Write;
        restore
            .stdin
            .take()
            .expect("pg_restore stdin")
            .write_all(&decrypted.stdout)
            .expect("feed the archive to pg_restore");
    }
    let restored = restore.wait_with_output().expect("pg_restore finishes");
    assert!(
        restored.status.success(),
        "pg_restore reads the decrypted archive: {}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert!(
        String::from_utf8_lossy(&restored.stdout).contains(PLUGIN_SECRET),
        "the known plugin password is in the decrypted archive — this probe \
         would see it if the bundle were not encrypted"
    );

    sqlx::query("DELETE FROM core.plugins WHERE id = 'probe163'")
        .execute(_ledger.as_ref())
        .await
        .expect("remove the probe's plugin row");
    let _ = std::fs::remove_dir_all(backups.parent().expect("root"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_a_run_without_a_recipient_fails_closed() {
    let _serial = SERIAL.lock().await;
    let (plugins, backups) = scratch("nokey");
    let url = base_url();
    let _ledger = fresh_ledger(&url, &plugins).await;
    let (base, _serve) = spawn_app(&plugins, &backups, &url).await;

    // No key pair, no opt-in: the one thing this must not do is write a plaintext
    // bundle because the configuration was absent.
    std::env::remove_var("ADJUTANT_BACKUP_AGE_RECIPIENT");
    std::env::remove_var("ADJUTANT_BACKUP_ALLOW_PLAINTEXT");

    let (status, body) = post(&base, "/api/backups/run").await;
    assert_eq!(status, 409, "a run without a recipient is refused: {body}");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("ADJUTANT_BACKUP_AGE_RECIPIENT"),
        "the refusal names the variable to set: {body}"
    );
    assert!(
        bundles(&backups).is_empty(),
        "no bundle — and no zero-byte placeholder — was left behind"
    );

    let _ = std::fs::remove_dir_all(backups.parent().expect("root"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_retention_prunes_and_the_ledger_does_not_lie() {
    let _serial = SERIAL.lock().await;
    let (plugins, backups) = scratch("prune");
    let url = base_url();
    let _ledger = fresh_ledger(&url, &plugins).await;
    let (base, _serve) = spawn_app(&plugins, &backups, &url).await;

    // Keep one, so a second run has to remove the first.
    let (status, _) = put(
        &base,
        "/api/backups/schedule",
        serde_json::json!({ "cadence_secs": 86400, "keep": 1, "enabled": false }),
    )
    .await;
    assert_eq!(status, 200);

    for _ in 0..2 {
        let (status, body) = post(&base, "/api/backups/run").await;
        assert_eq!(status, 200, "the run succeeds: {body}");
        // The names carry the second, so two runs inside one second would collide
        // on the file name. Wait for the clock to move rather than flaking.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    }

    let left = bundles(&backups);
    assert_eq!(left.len(), 1, "retention keeps one: {left:?}");

    // The ledger agrees with the directory: the pruned run is gone from both, so
    // the list cannot offer a download of a file that was deleted.
    let (status, body) = get(&base, "/api/backups").await;
    assert_eq!(status, 200);
    let runs = body["runs"].as_array().expect("runs");
    assert_eq!(runs.len(), 1, "and the list is one run: {runs:?}");
    assert_eq!(runs[0]["filename"].as_str(), Some(left[0].as_str()));

    let _ = std::fs::remove_dir_all(backups.parent().expect("root"));
}
