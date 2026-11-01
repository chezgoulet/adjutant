//! Regression for #189: the DB-gated suite must survive a second run against
//! the same database.
//!
//! It is a separate test binary on purpose. Running `host_db` from inside
//! `host_db` would execute a second copy of the suite alongside the first (the
//! outer binary's probes run in parallel), so the fixture state the regression
//! is meant to observe would be racing itself. Here the harness has one job:
//! invoke the real `host_db` target twice, sequentially, against whatever
//! `ADJUTANT_TEST_DATABASE_URL` names. The first run warms the database; the
//! second is the run that used to fail in
//! `probe_scope_id_is_opaque_text_and_round_trips`.
//!
//! `#[ignore]`d like the suite it guards: `cargo test --workspace` reports it as
//! ignored, and CI/developers run it explicitly with `-- --ignored`.
//!
//! ```text
//! ADJUTANT_TEST_DATABASE_URL=postgres://…/adjutant_dev_test \
//!   cargo test -p adjutant-server --test rerunnable_suite -- --ignored --nocapture
//! ```
use std::process::Command;

#[test]
#[ignore = "DB-gated: runs host_db twice against ADJUTANT_TEST_DATABASE_URL; run with `-- --ignored`"]
fn the_db_gated_suite_runs_twice_against_one_database() {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").expect(
        "ADJUTANT_TEST_DATABASE_URL must be set to rerun the DB-gated suite \
         (they are #[ignore]d; pass `-- --ignored` and set the variable)",
    );
    assert!(
        !url.trim().is_empty(),
        "ADJUTANT_TEST_DATABASE_URL is set but empty; set it to a _test database or unset it"
    );

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    for run in 1..=2 {
        println!("[#189] host_db run {run} against the same database");
        let status = Command::new(&cargo)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args([
                "test",
                "-p",
                "adjutant-server",
                "--test",
                "host_db",
                "--",
                "--ignored",
            ])
            .status()
            .unwrap_or_else(|e| panic!("run {run}: cannot execute {cargo}: {e}"));
        assert!(
            status.success(),
            "host_db run {run} failed against the same database (#189)"
        );
    }
}
