//! #152: the ledger and receipts are records **by privilege**, not only by a
//! trigger the writing role can switch off.
//!
//! The issue's reproduction is exactly the asymmetry this file pins:
//!
//! * `finance.transactions` had no trigger at all, so the plugin role could
//!   UPDATE `amount_cents` or DELETE a row carrying an `external_ref`;
//! * `finance.receipts` had a trigger, but the plugin role owned the table, so it
//!   could `ALTER TABLE … DISABLE TRIGGER receipts_immutable` and rewrite a
//!   receipt.
//!
//! Migration 4 installs the missing ledger guard and the TRUNCATE guards. The
//! core then moves ownership of the record objects to the deployment role and
//! leaves the plugin INSERT/SELECT, so the role that writes records is no longer
//! the role that can rewrite them or disarm their guards.
//!
//! ## Running it
//!
//! `#[ignore]`d like the other DB probes (issue #25). The database must be one a
//! boot has prepared with the new code, because the ownership move happens after
//! the plugin's migrations:
//!
//! ```text
//! ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin
//! ADJUTANT_TEST_DATABASE_URL=postgres://…/adjutant_dev_test \
//!   cargo test -p adjutant-finance --test ledger_immutability -- --ignored --nocapture
//! ```

use sqlx::PgPool;

/// The plugin's role, as `bootstrap-isolation` creates it.
const PLUGIN_ROLE: &str = "adjutant_plugin_finance";
/// Every row this file writes carries it, so cleanup can be exact.
const PROBE_MARK: &str = "probe:ledger-immutability";

fn test_database_url() -> String {
    let url = std::env::var("ADJUTANT_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "the ledger immutability probes are DB-gated: set ADJUTANT_TEST_DATABASE_URL to a \
             test database a boot has already prepared (they are #[ignore]d; run with `-- --ignored`)"
        )
    });
    assert!(
        !url.trim().is_empty(),
        "the database URL is set but empty; set it to a _test database or unset it"
    );
    url
}

async fn admin_pool() -> PgPool {
    PgPool::connect(&test_database_url())
        .await
        .expect("the database URL is set but unreachable")
}

/// The same URL with the plugin role's credentials, so `session_user` is the
/// role every plugin statement actually runs as.
async fn plugin_pool(admin: &PgPool) -> PgPool {
    let secret: Option<String> =
        sqlx::query_scalar("SELECT db_secret FROM core.plugins WHERE id = 'finance'")
            .fetch_optional(admin)
            .await
            .expect("read the plugin's stored secret")
            .flatten();
    let Some(secret) = secret else {
        panic!(
            "this test database has no bootstrapped `finance` role (no core.plugins.db_secret). \
             Run the live ladder first: `ADJUTANT_PLUGIN_DIR=plugins-built ./target/debug/adjutant test-plugin`"
        );
    };
    let url = test_database_url();
    let with_role = match url.split_once("://") {
        Some((scheme, rest)) => {
            let after = match rest.split_once('@') {
                Some((_, host)) => host,
                None => rest,
            };
            format!("{scheme}://{PLUGIN_ROLE}:{secret}@{after}")
        }
        None => url,
    };
    PgPool::connect(&with_role)
        .await
        .unwrap_or_else(|e| panic!("could not connect as {PLUGIN_ROLE}: {e}"))
}

async fn owner_of(admin: &PgPool, relation: &str) -> String {
    sqlx::query_scalar(
        "SELECT pg_get_userbyid(c.relowner)
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'finance' AND c.relname = $1",
    )
    .bind(relation)
    .fetch_one(admin)
    .await
    .unwrap_or_else(|e| panic!("read the owner of finance.{relation}: {e}"))
}

#[tokio::test]
#[ignore = "DB-gated: needs ADJUTANT_TEST_DATABASE_URL prepared by a boot; run with `-- --ignored`"]
async fn ledger_and_receipts_are_append_only_by_privilege() {
    let admin = admin_pool().await;
    let plugin = plugin_pool(&admin).await;

    // The writing role must not own the record tables: ownership is what lets a
    // role ALTER TABLE ... DISABLE TRIGGER.
    for table in ["transactions", "receipts"] {
        let owner = owner_of(&admin, table).await;
        assert_ne!(
            owner, PLUGIN_ROLE,
            "finance.{table} is owned by the plugin role; it can disable the immutability trigger"
        );

        let guards: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM pg_trigger t
               JOIN pg_class c ON c.oid = t.tgrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'finance' AND c.relname = $1 AND NOT t.tgisinternal",
        )
        .bind(table)
        .fetch_one(&admin)
        .await
        .expect("read the guard triggers");
        assert!(
            guards >= 1,
            "finance.{table} has no immutability guard; the record can be rewritten"
        );
    }

    // A ledger write still works as the plugin role: the fix is privilege to
    // rewrite, not a refusal to record money.
    let fund_id: i64 = sqlx::query_scalar("SELECT id FROM finance.funds WHERE code = 'general'")
        .fetch_one(&admin)
        .await
        .expect("the general fund exists");
    let tx_id: i64 = sqlx::query_scalar(
        "INSERT INTO finance.transactions \
           (fund_id, amount_cents, kind, category, description, fiscal_year, occurred_on, recorded_by) \
         VALUES ($1, 100, 'income', 'donation', $2, 2026, '2026-01-15', 'probe') \
         RETURNING id",
    )
    .bind(fund_id)
    .bind(PROBE_MARK)
    .fetch_one(&plugin)
    .await
    .expect("the plugin role can still INSERT a ledger row");

    // The plugin role can read what it wrote.
    let found: i64 = sqlx::query_scalar("SELECT count(*) FROM finance.transactions WHERE id = $1")
        .bind(tx_id)
        .fetch_one(&plugin)
        .await
        .expect("the plugin role can still SELECT the ledger");

    assert_eq!(found, 1);

    // ... but it cannot rewrite or erase it. Either the privilege is absent or
    // the trigger fires; both are refusals, and the test accepts either so it is
    // not pinned to the mechanism.
    for (what, sql) in [
        (
            "UPDATE",
            format!("UPDATE finance.transactions SET amount_cents = 5 WHERE id = {tx_id}"),
        ),
        (
            "DELETE",
            format!("DELETE FROM finance.transactions WHERE id = {tx_id}"),
        ),
        (
            "TRUNCATE",
            "TRUNCATE finance.transactions".to_string(),
        ),
    ] {
        let err = sqlx::query(&sql)
            .execute(&plugin)
            .await
            .expect_err(&format!(
                "the plugin role must not be able to {what} a ledger record"
            ));
        let message = err.to_string();
        assert!(
            message.contains("permission denied") || message.contains("is a record"),
            "{what} was refused for an unexpected reason: {message}"
        );
    }

    // And it cannot switch the guards off. This is the half a trigger alone
    // cannot deliver.
    for table in ["transactions", "receipts"] {
        let sql = format!("ALTER TABLE finance.{table} DISABLE TRIGGER ALL");
        let err = sqlx::query(&sql)
            .execute(&plugin)
            .await
            .expect_err(&format!(
                "the plugin role must not be able to disable finance.{table}'s guards"
            ));
        let message = err.to_string();
        assert!(
            message.contains("must be owner") || message.contains("permission denied"),
            "disabling finance.{table}'s guards was refused for an unexpected reason: {message}"
        );
    }

    // Probe cleanup only: the ledger refuses DELETE even to the operator, and
    // `session_replication_role = replica` is transaction-local.
    let mut cleanup = admin.begin().await.expect("cleanup transaction");
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *cleanup)
        .await
        .expect("bypass the record guard for cleanup");
    sqlx::query("DELETE FROM finance.transactions WHERE id = $1")
        .bind(tx_id)
        .execute(&mut *cleanup)
        .await
        .expect("remove the probe row");
    cleanup.commit().await.expect("cleanup commit");
}
