//! Backups: the cadence an admin chooses, the bundle a button produces, and the
//! ledger of what actually ran.
//!
//! Two producers, one list. The **app** runs `pg_dump` when an admin presses the
//! button (or when the in-app timer fires, if enabled); the **host** can run
//! `deploy/backup.sh` on its own timer, which is the one that still works when
//! the app is down. A backup ledger that only knows about runs the app performed
//! would therefore be wrong exactly when it matters, so [`list_runs`] reconciles
//! the bundle directory with `core.backups`: files nothing claims appear as runs
//! nobody recorded, and the admin sees one list either way.
//!
//! What a bundle is *not*: a complete backup. A `pg_dump` carries the database
//! and not the cluster's roles, so restoring one onto a clean host needs
//! `bootstrap-isolation` afterwards. That is `deploy/restore.sh`'s job and
//! `docs/deployment.md` § Backup and restore says why.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sqlx::PgPool;

/// The cadences the admin may choose, and the names the client shows.
///
/// A fixed list rather than a free-form interval: the value is a *policy* the
/// server enforces (there is a one-hour floor in the schema too, because the
/// client is not the only thing that can write the row), and a preset makes the
/// choice legible in the audit log afterwards.
pub const PRESETS: &[(i64, &str)] = &[
    (3_600, "hourly"),
    (21_600, "every 6 hours"),
    (86_400, "daily"),
    (604_800, "weekly"),
];

/// How a run was asked for. `Scheduled` covers both the in-app timer and the
/// host's — the auditor's question is whether a person pressed something.
pub const TRIGGER_MANUAL: &str = "manual";
pub const TRIGGER_SCHEDULED: &str = "scheduled";

#[derive(Debug, Clone, serde::Serialize)]
pub struct Schedule {
    pub cadence_secs: Option<i64>,
    pub keep: i32,
    pub enabled: bool,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_by: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BackupRun {
    /// `Some` for a run the app performed and recorded. `None` for a bundle
    /// found in the directory that no row claims — produced by the host's own
    /// timer while the app was down, which is a thing that is supposed to happen.
    pub id: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub trigger: String,
    pub requested_by: Option<String>,
    pub status: String,
    pub filename: String,
    pub bytes: Option<i64>,
    pub sha256: Option<String>,
    pub error: Option<String>,
    /// True when this row was reconstructed from the directory rather than read
    /// from `core.backups`. The client says so, because "we know this exists but
    /// did not record it happening" is information.
    pub unrecorded: bool,
}

/// Where bundles live. Set by `ADJUTANT_BACKUP_DIR`; the compose file mounts it.
pub fn backup_dir() -> PathBuf {
    std::env::var("ADJUTANT_BACKUP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/backups/adjutant"))
}

pub async fn read_schedule(pool: &PgPool) -> Result<Schedule, sqlx::Error> {
    let row = sqlx::query_as::<_, (Option<i64>, i32, bool, Option<chrono::DateTime<chrono::Utc>>, Option<String>)>(
        "SELECT cadence_secs, keep, enabled, updated_at, updated_by
           FROM core.backup_schedule WHERE id",
    )
    .fetch_one(pool)
    .await?;
    Ok(Schedule {
        cadence_secs: row.0,
        keep: row.1,
        enabled: row.2,
        updated_at: row.3,
        updated_by: row.4,
    })
}

/// Set the cadence. Refuses a value that is not a preset, so the row cannot hold
/// a number the picker has no way to show.
pub async fn write_schedule(
    pool: &PgPool,
    cadence_secs: Option<i64>,
    keep: i32,
    enabled: bool,
    by: Option<&str>,
) -> Result<Schedule, String> {
    if let Some(secs) = cadence_secs {
        if !PRESETS.iter().any(|(p, _)| *p == secs) {
            return Err(format!(
                "{secs} is not one of the offered cadences: {}",
                PRESETS.iter().map(|(p, _)| p.to_string()).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    if !(1..=365).contains(&keep) {
        return Err("keep must be between 1 and 365".to_string());
    }
    // Enabled with no cadence is not a state the UI can produce, and it would
    // mean "scheduled backups are on and there is no schedule".
    if enabled && cadence_secs.is_none() {
        return Err("enabling scheduled backups needs a cadence".to_string());
    }

    sqlx::query(
        "UPDATE core.backup_schedule
            SET cadence_secs = $1, keep = $2, enabled = $3, updated_at = now(), updated_by = $4
          WHERE id",
    )
    .bind(cadence_secs)
    .bind(keep)
    .bind(enabled)
    .bind(by)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    read_schedule(pool).await.map_err(|e| e.to_string())
}

/// Every bundle the admin should see: the rows, plus files in the directory that
/// no row claims.
pub async fn list_runs(pool: &PgPool, limit: i64) -> Result<Vec<BackupRun>, sqlx::Error> {
    let rows = sqlx::query_as::<
        _,
        (
            i64,
            chrono::DateTime<chrono::Utc>,
            Option<chrono::DateTime<chrono::Utc>>,
            String,
            Option<String>,
            String,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
        ),
    >(
        "SELECT id, created_at, finished_at, trigger, requested_by, status,
                filename, bytes, sha256, error
           FROM core.backups
          ORDER BY created_at DESC
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut out: Vec<BackupRun> = rows
        .into_iter()
        .map(|r| BackupRun {
            id: Some(r.0),
            created_at: r.1,
            finished_at: r.2,
            trigger: r.3,
            requested_by: r.4,
            status: r.5,
            filename: r.6.clone().unwrap_or_default(),
            bytes: r.7,
            sha256: r.8,
            error: r.9,
            unrecorded: false,
        })
        .collect();

    // Reconcile: a bundle on disk that no row claims was produced outside the
    // app — the host's timer, which is meant to work when the app is down.
    let claimed: std::collections::HashSet<String> =
        out.iter().map(|r| r.filename.clone()).filter(|f| !f.is_empty()).collect();
    if let Ok(entries) = std::fs::read_dir(backup_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".dump") || claimed.contains(&name) {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let created = meta
                .modified()
                .ok()
                .map(chrono::DateTime::<chrono::Utc>::from)
                .unwrap_or_else(chrono::Utc::now);
            out.push(BackupRun {
                id: None,
                created_at: created,
                finished_at: Some(created),
                trigger: TRIGGER_SCHEDULED.to_string(),
                requested_by: None,
                status: "done".to_string(),
                filename: name,
                bytes: Some(meta.len() as i64),
                sha256: None,
                error: None,
                unrecorded: true,
            });
        }
        // Newest first. `Reverse` rather than a descending comparator because
        // `-D warnings` treats the latter as a lint, and rightly: one way to say
        // "descending" is enough.
        out.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    }
    out.truncate(limit as usize);
    Ok(out)
}

/// Resolve a bundle for download, refusing anything that is not a file directly
/// inside the backup directory — a name from a URL must never become a path.
pub fn bundle_path(dir: &Path, filename: &str) -> Option<PathBuf> {
    if filename.is_empty()
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || !filename.ends_with(".dump")
    {
        return None;
    }
    let p = dir.join(filename);
    p.is_file().then_some(p)
}

/// sha256 of a bundle, as `sha256sum` prints it (bare hex), so `deploy/backup.sh`
/// and the app agree on the sidecar format.
fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
}

async fn mark_failed(pool: &PgPool, id: i64, why: &str) {
    let _ = sqlx::query(
        "UPDATE core.backups SET status = 'failed', finished_at = now(), error = $2 WHERE id = $1",
    )
    .bind(id)
    .bind(why)
    .execute(pool)
    .await;
}

/// Delete bundles beyond the schedule's `keep`, oldest first.
///
/// Only files this directory holds, and never a `.dump` whose row says it is
/// still running — pruning a bundle the running dump is still writing to would
/// be a spectacular own goal.
pub async fn prune(pool: &PgPool, dir: &Path) -> Result<usize, String> {
    let keep = read_schedule(pool).await.map_err(|e| e.to_string())?.keep as usize;
    let mut bundles: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "dump").unwrap_or(false))
        .collect();
    // Newest first by mtime; the names sort too, but the filesystem is the truth
    // about what exists and mtime is the truth about when.
    bundles.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    bundles.reverse();

    let running: std::collections::HashSet<String> = sqlx::query_scalar(
        "SELECT filename FROM core.backups WHERE status = 'running' AND filename IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?
    .into_iter()
    .collect();

    let mut removed = 0;
    for old in bundles.into_iter().skip(keep) {
        let name = old.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if running.contains(&name) {
            continue;
        }
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::remove_file(old.with_extension("dump.sha256"));
        let _ = sqlx::query("DELETE FROM core.backups WHERE filename = $1 AND status <> 'running'")
            .bind(&name)
            .execute(pool)
            .await;
        removed += 1;
    }
    Ok(removed)
}

/// Produce a bundle now, record it, prune, and hand back the row.
///
/// `pg_dump` runs as the same role the server uses: `adjutant_app` is a member
/// of every plugin role, so it can read every schema — which is why a single
/// connection string is enough and the app never needs a superuser URL.
pub async fn run(
    pool: &PgPool,
    database_url: &str,
    trigger: &str,
    requested_by: Option<&str>,
) -> Result<BackupRun, String> {
    let dir = backup_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let filename = format!("adjutant-{}.dump", chrono::Utc::now().format("%F-%H%M%S"));
    let path = dir.join(&filename);

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO core.backups (trigger, requested_by, status, filename)
         VALUES ($1, $2, 'running', $3) RETURNING id",
    )
    .bind(trigger)
    .bind(requested_by)
    .bind(&filename)
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;

    let output = tokio::process::Command::new("pg_dump")
        .arg("--dbname")
        .arg(database_url)
        .arg("--format=custom")
        .arg("--file")
        .arg(&path)
        .output()
        .await;

    let outcome = match output {
        Ok(o) if o.status.success() => {
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if bytes == 0 {
                // A zero-byte dump is a failed backup, not a small one. Same rule
                // as deploy/backup.sh, for the same reason.
                Err("pg_dump wrote an empty file".to_string())
            } else {
                match sha256_file(&path) {
                    Ok(sha) => {
                        let _ = std::fs::write(dir.join(format!("{filename}.sha256")), format!("{sha}  {filename}\n"));
                        sqlx::query(
                            "UPDATE core.backups
                                SET status = 'done', finished_at = now(), bytes = $2, sha256 = $3
                              WHERE id = $1",
                        )
                        .bind(id)
                        .bind(bytes as i64)
                        .bind(&sha)
                        .execute(pool)
                        .await
                        .map_err(|e| e.to_string())?;
                        Ok(())
                    }
                    Err(e) => Err(format!("cannot read the bundle back to hash it: {e}")),
                }
            }
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            Err(if err.is_empty() { format!("pg_dump exited {}", o.status) } else { err })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(
            "pg_dump is not on this server. The container image must carry a client \
             new enough for the database it dumps — see the Dockerfile."
                .to_string(),
        ),
        Err(e) => Err(format!("cannot run pg_dump: {e}")),
    };

    if let Err(why) = outcome {
        let _ = std::fs::remove_file(&path);
        mark_failed(pool, id, &why).await;
        return Err(why);
    }

    let _ = prune(pool, &dir).await;
    list_runs(pool, 1)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|r| r.id == Some(id))
        .ok_or_else(|| "the run was recorded but cannot be read back".to_string())
}

/// How often the timer looks at the schedule. A minute against an hourly floor is
/// sixty chances to be punctual, and a tick costs two queries.
const TICK_SECS: u64 = 60;

/// The in-app timer: one loop per process.
///
/// Deliberately the same shape as [`crate::outbox::Relay`] — a `Weak` to the app
/// state so the task cannot keep the process alive, and a handle `shutdown`
/// aborts. A second pattern for the same job would be a second thing to get
/// wrong, and this one already handles the two hard parts: no reference cycle,
/// and no dangling task at exit.
pub struct Timer {
    handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Timer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            handle: std::sync::Mutex::new(None),
        })
    }

    pub fn start(&self, state: &Arc<crate::server::AppState>) {
        let weak = Arc::downgrade(state);
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(TICK_SECS)).await;
                let Some(state) = weak.upgrade() else {
                    // The state is gone; nothing left to serve.
                    break;
                };
                if let Err(e) = tick(&state).await {
                    // A failed tick is not fatal — the next one tries again, and
                    // if a dump was attempted the run row already says why. Going
                    // quiet here would be the wrong kind of quiet.
                    tracing::warn!(error = %e, "backup timer tick");
                }
            }
        });
        if let Ok(mut slot) = self.handle.lock() {
            *slot = Some(handle);
        }
    }

    pub fn stop(&self) {
        if let Ok(mut slot) = self.handle.lock() {
            if let Some(h) = slot.take() {
                h.abort();
            }
        }
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self {
            handle: std::sync::Mutex::new(None),
        }
    }
}

/// One pass: is a scheduled run due, and if so take it.
async fn tick(state: &Arc<crate::server::AppState>) -> Result<(), String> {
    let pool = state.pool.as_ref();

    let schedule = read_schedule(pool).await.map_err(|e| e.to_string())?;
    if !schedule.enabled {
        return Ok(());
    }
    let Some(cadence) = schedule.cadence_secs else {
        return Ok(());
    };

    // Due when the last *finished* run is older than the cadence — or when there
    // has never been one, so a fresh schedule proves itself with a bundle
    // instead of waiting a full interval before anyone learns whether it works.
    let last: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT max(created_at) FROM core.backups WHERE status = 'done'")
            .fetch_one(pool)
            .await
            .map_err(|e| e.to_string())?;
    let due = match last {
        None => true,
        Some(t) => (chrono::Utc::now() - t).num_seconds() >= cadence,
    };
    if !due {
        return Ok(());
    }

    // Never two dumps at once. The manual button leaves a `running` row too, so
    // this also stops the timer from stacking a second `pg_dump` on top of an
    // admin's — the two producers share one ledger precisely so they can see
    // each other.
    let running: i64 =
        sqlx::query_scalar("SELECT count(*) FROM core.backups WHERE status = 'running'")
            .fetch_one(pool)
            .await
            .map_err(|e| e.to_string())?;
    if running > 0 {
        tracing::debug!("a backup is already running; the timer will try again next tick");
        return Ok(());
    }

    tracing::info!(cadence_secs = cadence, "scheduled backup starting");
    run(pool, &state.config.database_url, TRIGGER_SCHEDULED, None).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_path_refuses_to_escape_its_directory() {
        for bad in [
            "",
            "../../etc/passwd",
            "/etc/passwd",
            "sub/dir.dump",
            "..\\windows.dump",
            "adjutant.dump.bak",
        ] {
            assert!(
                bundle_path(Path::new("/var/backups/adjutant"), bad).is_none(),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn presets_are_the_ones_the_client_offers() {
        let secs: Vec<i64> = PRESETS.iter().map(|(s, _)| *s).collect();
        assert_eq!(secs, vec![3_600, 21_600, 86_400, 604_800]);
    }
}
