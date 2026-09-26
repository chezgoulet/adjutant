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

/// Boot the real application on an ephemeral port, with its backup directory
/// pointed at `backups`.
async fn spawn_app(plugins: &Path, backups: &Path, url: &str) -> (String, tokio::task::JoinHandle<()>) {
    // Process-wide, because `backup::backup_dir()` reads the environment the same
    // way the server does. The caller holds SERIAL for the whole test.
    std::env::set_var("ADJUTANT_BACKUP_DIR", backups);

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
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL + CREATEROLE; run with `-- --ignored`"]
async fn probe_retention_prunes_and_the_ledger_does_not_lie() {
    let _serial = SERIAL.lock().await;
    let (plugins, backups) = scratch("prune");
    let url = base_url();
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
