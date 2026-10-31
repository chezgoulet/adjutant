//! Audit verification, both layers (#159, #160).
//!
//! `core.audit_verify()` checks the **unkeyed** chain inside PostgreSQL: it can
//! see every row and recompute SHA-256, but it cannot hold the HMAC key. The
//! second layer is this module: it walks the same rows in id order, rebuilds the
//! canonical payload and compares `keyed_hash` with `HMAC-SHA256(key, payload)`.
//!
//! Both layers report what they actually checked. A server without a key says
//! `keyed: false`; it never presents the unkeyed result as keyed protection.

use adjutant_sdk::audit::{canonical_payload, ChainKey, AUDIT_ZERO_HASH};
use serde::Serialize;
use sqlx::PgPool;

/// The result of verifying both layers. `ok` deliberately describes the
/// **unkeyed** chain only, matching the pre-#160 meaning of the field; `keyed`
/// and the keyed fields describe the separate HMAC layer.
#[derive(Debug, Clone, Serialize)]
pub struct AuditReport {
    /// The unkeyed chain verifies (SQL `core.audit_verify()`).
    pub ok: bool,
    /// First row whose unkeyed `prev_hash`/`entry_hash` does not match.
    pub first_bad: Option<i64>,
    /// Rows the unkeyed verifier walked. Includes unkeyed-era rows.
    pub rows_checked: i64,
    /// Was a key configured? False means the keyed layer was **not** checked;
    /// it does not mean the keyed layer passed.
    pub keyed: bool,
    /// First row whose `keyed_hash` does not match the HMAC. `None` when every
    /// keyed row checked out, or when no key was configured.
    pub keyed_first_bad: Option<i64>,
    /// Rows with a non-NULL `keyed_hash` that were checked (or attempted).
    pub keyed_rows_checked: i64,
    /// Rows with a NULL `keyed_hash`: written before the key was enabled, or by
    /// a writer that had no key. They are not silently counted as protected.
    pub keyed_rows_unkeyed: i64,
}

/// Verify the unkeyed chain (SQL) and, when a key is supplied, the keyed layer
/// (Rust). Query errors propagate: a caller must fail closed rather than report
/// a healthy empty log.
pub async fn verify(
    pool: &PgPool,
    key: Option<&ChainKey>,
) -> Result<AuditReport, sqlx::Error> {
    let (first_bad, rows_checked): (Option<i64>, i64) =
        sqlx::query_as("SELECT first_bad, rows_checked FROM core.audit_verify()")
            .fetch_one(pool)
            .await?;

    let mut report = AuditReport {
        ok: first_bad.is_none(),
        first_bad,
        rows_checked,
        keyed: key.is_some(),
        keyed_first_bad: None,
        keyed_rows_checked: 0,
        keyed_rows_unkeyed: 0,
    };

    let Some(key) = key else {
        return Ok(report);
    };

    // Cast every field to the exact text form the SQL canonicaliser hashes:
    // `details::text` is jsonb's rendering, `user_id::text` is canonical
    // lowercase, and the timestamp is formatted with the same to_char pattern
    // as migration 17's core.audit_canonical().
    let rows: Vec<KeyedRow> = sqlx::query_as(
        "SELECT id, prev_hash, action, resource_type, resource_id, \
                details::text AS details_text, source, user_id::text AS user_id_text, \
                to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US') || '+00:00' \
                    AS created_at_text, \
                keyed_hash \
         FROM core.audit_log ORDER BY id",
    )
    .fetch_all(pool)
    .await?;

    for row in rows {
        let Some(stored) = row.keyed_hash else {
            report.keyed_rows_unkeyed += 1;
            continue;
        };
        report.keyed_rows_checked += 1;
        let payload = canonical_payload(
            row.prev_hash.as_deref().unwrap_or(AUDIT_ZERO_HASH),
            row.id,
            &row.action,
            &row.resource_type,
            &row.resource_id,
            &row.details_text,
            &row.source,
            row.user_id_text.as_deref(),
            &row.created_at_text,
        );
        if !key.keyed_digest_matches(&payload, &stored) && report.keyed_first_bad.is_none() {
            report.keyed_first_bad = Some(row.id);
        }
    }

    Ok(report)
}

/// One row as the app-side verifier needs it: the stored hash chain fields plus
/// the canonical text forms of the fields that are not already text.
#[derive(sqlx::FromRow)]
struct KeyedRow {
    id: i64,
    prev_hash: Option<String>,
    action: String,
    resource_type: String,
    resource_id: String,
    details_text: String,
    source: String,
    user_id_text: Option<String>,
    created_at_text: String,
    keyed_hash: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Finding 3: one changed character in the stored hash is a mismatch, and
    /// a malformed value is a mismatch too — never an error that aborts the walk.
    #[test]
    fn keyed_comparison_detects_a_one_character_change() {
        let key = ChainKey::from_bytes([7u8; 32]);
        let payload = "row|1|action";
        let stored = key.keyed_digest(payload);
        assert!(key.keyed_digest_matches(payload, &stored));

        let mut tampered = stored.into_bytes();
        tampered[0] = if tampered[0] == b'0' { b'1' } else { b'0' };
        let tampered = String::from_utf8(tampered).expect("hex is ASCII");
        assert!(!key.keyed_digest_matches(payload, &tampered));

        assert!(!key.keyed_digest_matches(payload, "not-hex"));
        assert!(!key.keyed_digest_matches(payload, "deadbeef"));
    }
}
