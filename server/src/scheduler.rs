//! The core scheduler (#45): plugins declare schedules, the core runs them.
//!
//! The event bus is reactive; nothing schedules work. A plugin that needs
//! periodic work (an expiry sweep, a daily digest) must not grow its own
//! background thread — that thread is unauditable, unbounded and unstoppable.
//! So a plugin **declares** a [`Schedule`] (name, interval, handler) exactly as
//! it declares routes and subscriptions, and the core runs it with the same
//! discipline it applies to a request:
//!
//! - the handler runs on the plugin's **own pool**, bound by its isolation role
//!   (the handler captures its `ctx.db`, so this is automatic);
//! - a per-run timeout and **one attempt per tick** — a failure is recorded and
//!   logged, never retried in a loop; the next tick is the retry;
//! - every run is written to `core.scheduled_runs`, so an operator can answer
//!   "did the timer actually run?" after a restart, which in-memory state cannot;
//! - schedules start when the plugin loads and are **aborted** when it is
//!   disabled, uninstalled or reloaded (the same rule as #30's pool close).
//!
//! **Catch-up policy (stated, not silent):** at start, if the last successful
//! run is older than the cadence (or there is none), the schedule runs **once**
//! immediately and then resumes the cadence. Missed ticks are never backfilled.
//!
//! **Cadence is an interval, not cron** (v1): `every` is measured from the last
//! completed run, so a slow run delays the next rather than overlapping it, and
//! there is no cron parser to half-implement.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tokio::task::JoinHandle;

use adjutant_sdk::Schedule;

use crate::plugin_runtime::ScheduleInfo;

/// Per-run timeout: a handler that hangs past this is recorded as a failure and
/// the connection is returned, rather than pinning a pool connection forever.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Runtime {
    last_run: Option<DateTime<Utc>>,
    last_error: Option<String>,
    next_run: Option<DateTime<Utc>>,
}

/// Runs declared schedules. One instance lives in `AppState`.
#[derive(Default)]
pub struct Scheduler {
    /// Live schedule tasks, keyed by `(plugin_id, schedule)`.
    tasks: Mutex<HashMap<(String, String), JoinHandle<()>>>,
    /// Declared schedules per plugin: `(name, every_secs)`. Kept after a stop so
    /// the admin surface still lists them.
    declared: Mutex<HashMap<String, Vec<(String, u64)>>>,
    /// Last run / error / next run per `(plugin_id, schedule)`.
    runtime: Mutex<HashMap<(String, String), Runtime>>,
}

impl Scheduler {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Start (or restart) a plugin's schedules. Any existing tasks for the
    /// plugin are aborted first, so a reload cannot double-run.
    pub fn start(self: &Arc<Self>, plugin_id: &str, schedules: Vec<Schedule>, pool: Arc<PgPool>) {
        self.stop(plugin_id);
        {
            let mut declared = self.declared.lock().expect("scheduler declared poisoned");
            declared.insert(
                plugin_id.to_string(),
                schedules.iter().map(|s| (s.name.clone(), s.every.as_secs())).collect(),
            );
        }
        let mut tasks = self.tasks.lock().expect("scheduler tasks poisoned");
        for schedule in schedules {
            let key = (plugin_id.to_string(), schedule.name.clone());
            let scheduler = self.clone();
            let pid = plugin_id.to_string();
            let pool = pool.clone();
            let handle = tokio::spawn(async move {
                run_loop(scheduler, pid, schedule, pool).await;
            });
            tasks.insert(key, handle);
        }
    }

    /// Abort every task for `plugin_id` (disable/uninstall). Leaves the declared
    /// list and last-run state for the admin surface; clears `next_run`.
    pub fn stop(&self, plugin_id: &str) {
        {
            let mut tasks = self.tasks.lock().expect("scheduler tasks poisoned");
            let keys: Vec<(String, String)> = tasks
                .keys()
                .filter(|(p, _)| p == plugin_id)
                .cloned()
                .collect();
            for key in keys {
                if let Some(handle) = tasks.remove(&key) {
                    handle.abort();
                }
            }
        }
        let mut runtime = self.runtime.lock().expect("scheduler runtime poisoned");
        for ((p, _), r) in runtime.iter_mut() {
            if p == plugin_id {
                r.next_run = None;
            }
        }
    }

    /// Abort every task (reload, shutdown).
    pub fn stop_all(&self) {
        {
            let mut tasks = self.tasks.lock().expect("scheduler tasks poisoned");
            for (_, handle) in tasks.drain() {
                handle.abort();
            }
        }
        let mut runtime = self.runtime.lock().expect("scheduler runtime poisoned");
        for r in runtime.values_mut() {
            r.next_run = None;
        }
    }

    /// Admin snapshot for one plugin: declared schedules with their last run,
    /// last error and next run.
    pub fn infos(&self, plugin_id: &str) -> Vec<ScheduleInfo> {
        let declared = self.declared.lock().expect("scheduler declared poisoned");
        let runtime = self.runtime.lock().expect("scheduler runtime poisoned");
        declared
            .get(plugin_id)
            .map(|list| {
                list.iter()
                    .map(|(name, every_secs)| {
                        let key = (plugin_id.to_string(), name.clone());
                        let rt = runtime.get(&key);
                        ScheduleInfo {
                            name: name.clone(),
                            every_secs: *every_secs,
                            last_run: rt.and_then(|r| r.last_run),
                            last_error: rt.and_then(|r| r.last_error.clone()),
                            next_run: rt.and_then(|r| r.next_run),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Clear a plugin's declared schedules and runtime state (uninstall).
    pub fn forget(&self, plugin_id: &str) {
        self.stop(plugin_id);
        self.declared.lock().expect("scheduler declared poisoned").remove(plugin_id);
        let mut runtime = self.runtime.lock().expect("scheduler runtime poisoned");
        runtime.retain(|(p, _), _| p != plugin_id);
    }

    fn set_runtime(
        &self,
        plugin_id: &str,
        schedule: &str,
        last_run: DateTime<Utc>,
        last_error: Option<String>,
        next_run: DateTime<Utc>,
    ) {
        let mut runtime = self.runtime.lock().expect("scheduler runtime poisoned");
        runtime.insert(
            (plugin_id.to_string(), schedule.to_string()),
            Runtime { last_run: Some(last_run), last_error, next_run: Some(next_run) },
        );
    }

    /// Seed the admin view from the durable record at start, so a restart shows
    /// the last run and the next one instead of "nothing yet".
    fn seed_runtime(
        &self,
        plugin_id: &str,
        schedule: &str,
        last_run: Option<DateTime<Utc>>,
        last_error: Option<String>,
        next_run: DateTime<Utc>,
    ) {
        let mut runtime = self.runtime.lock().expect("scheduler runtime poisoned");
        runtime.insert(
            (plugin_id.to_string(), schedule.to_string()),
            Runtime { last_run, last_error, next_run: Some(next_run) },
        );
    }
}

/// Delay before the first run: run now if there is no last success or it is older
/// than the cadence; otherwise wait out the remainder. This is the catch-up
/// policy — one run after downtime, then the cadence.
fn first_delay(last_ok: Option<DateTime<Utc>>, every: Duration, now: DateTime<Utc>) -> Duration {
    let Some(last) = last_ok else {
        return Duration::ZERO;
    };
    let every = chrono::Duration::from_std(every).unwrap_or_else(|_| chrono::Duration::zero());
    let due = last + every;
    if due > now {
        (due - now).to_std().unwrap_or_default()
    } else {
        Duration::ZERO
    }
}

async fn last_success(pool: &PgPool, plugin_id: &str, schedule: &str) -> Option<DateTime<Utc>> {
    sqlx::query_scalar(
        "SELECT max(finished_at) FROM core.scheduled_runs \
         WHERE plugin_id = $1 AND schedule = $2 AND ok",
    )
    .bind(plugin_id)
    .bind(schedule)
    .fetch_one(pool)
    .await
    .ok()
    .flatten()
}

/// The most recent run of any result: `(finished_at, ok, error)`.
async fn latest_run(
    pool: &PgPool,
    plugin_id: &str,
    schedule: &str,
) -> Option<(DateTime<Utc>, bool, Option<String>)> {
    sqlx::query_as(
        "SELECT finished_at, ok, error FROM core.scheduled_runs \
         WHERE plugin_id = $1 AND schedule = $2 ORDER BY finished_at DESC LIMIT 1",
    )
    .bind(plugin_id)
    .bind(schedule)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

async fn run_loop(
    scheduler: Arc<Scheduler>,
    plugin_id: String,
    schedule: Schedule,
    pool: Arc<PgPool>,
) {
    let Schedule { name, every, handler } = schedule;
    // A zero interval would busy-loop; keep a floor.
    let every = every.max(Duration::from_millis(1));

    // Catch-up + visibility: the durable record is authoritative across a
    // restart. One run if the last success is older than the cadence, then the
    // cadence; the admin view is seeded so it is never blank after a restart.
    let last_ok = last_success(&pool, &plugin_id, &name).await;
    let latest = latest_run(&pool, &plugin_id, &name).await;
    let mut delay = first_delay(last_ok, every, Utc::now());
    scheduler.seed_runtime(
        &plugin_id,
        &name,
        latest.as_ref().map(|(t, _, _)| *t),
        latest.as_ref().and_then(|(_, ok, err)| if *ok { None } else { err.clone() }),
        Utc::now() + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::zero()),
    );

    loop {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let started = Utc::now();
        let outcome = tokio::time::timeout(RUN_TIMEOUT, (handler)()).await;
        let finished = Utc::now();
        let (ok, error) = match outcome {
            Ok(Ok(())) => (true, None),
            Ok(Err(e)) => (false, Some(e.to_string())),
            Err(_) => (false, Some(format!("timed out after {}s", RUN_TIMEOUT.as_secs()))),
        };
        let next = finished
            + chrono::Duration::from_std(every).unwrap_or_else(|_| chrono::Duration::zero());

        // Record durably (best-effort: a failed audit-style write must not kill
        // the scheduler) and update the in-memory view.
        if let Err(e) = sqlx::query(
            "INSERT INTO core.scheduled_runs \
             (plugin_id, schedule, started_at, finished_at, ok, error) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&plugin_id)
        .bind(&name)
        .bind(started)
        .bind(finished)
        .bind(ok)
        .bind(error.as_deref())
        .execute(pool.as_ref())
        .await
        {
            tracing::warn!(plugin = %plugin_id, schedule = %name, error = %e,
                "failed to record a scheduled run");
        }
        if ok {
            scheduler.set_runtime(&plugin_id, &name, finished, None, next);
        } else {
            tracing::warn!(plugin = %plugin_id, schedule = %name, error = ?error,
                "scheduled run failed; the next tick is the retry");
            scheduler.set_runtime(&plugin_id, &name, finished, error, next);
        }

        // Completion + interval: no overlap, no spin when a run outlasts `every`.
        delay = every;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catch_up_runs_once_when_due_and_waits_otherwise() {
        let now = Utc::now();
        // No previous success → run now.
        assert_eq!(first_delay(None, Duration::from_secs(3600), now), Duration::ZERO);
        // Last success an hour ago with an hourly cadence → due now.
        assert_eq!(
            first_delay(Some(now - chrono::Duration::hours(1)), Duration::from_secs(3600), now),
            Duration::ZERO
        );
        // Last success a moment ago → wait nearly the cadence.
        let d = first_delay(Some(now), Duration::from_secs(3600), now);
        assert!(d > Duration::from_secs(3590) && d <= Duration::from_secs(3600), "got {d:?}");
    }

    #[tokio::test]
    async fn stop_is_idempotent_on_an_unknown_plugin() {
        let s = Scheduler::new();
        s.stop("nope");
        s.stop_all();
        assert!(s.infos("nope").is_empty());
    }
}
