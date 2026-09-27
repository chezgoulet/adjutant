//! The operator's choice of which plugins this deployment runs.
//!
//! Issue #90's acceptance requires that the scripted path and the wizard path
//! "produce the same state — verified as one probe, not two". The cheapest way to
//! be wrong about that is to write the rules twice: the CLI and the API each
//! deciding what a valid set is, drifting apart, and a probe that happens to use
//! an input where they agree. So the rules live here once, and both doors call
//! this.
//!
//! Three refusals, all before anything is written, all naming what is wrong:
//! an id that is not on disk, a required plugin left off, and a dependent
//! without its dependency. The last two are the server's own rules
//! ([`crate::server::REQUIRED_PLUGINS`], [`crate::server::PLUGIN_DEPENDENCIES`]),
//! read from the same constants the enable/disable handlers read — not restated
//! here, because a second copy is a second thing to drift.

use sqlx::PgPool;

use crate::server::{PLUGIN_DEPENDENCIES, REQUIRED_PLUGINS};

/// Which door the choice came through. Recorded, because the two paths are
/// required to produce the same *state* — and knowing which one wrote a given
/// state is what turns a divergence into a diagnosis instead of a mystery.
pub const SOURCE_CLI: &str = "cli";
pub const SOURCE_WIZARD: &str = "wizard";

/// Set the enabled flags and record that a choice was made, as one act.
///
/// The two are deliberately inseparable. A flag written without the record would
/// leave the server unable to tell "the operator chose exactly this" from "nobody
/// has chosen yet" — and the first-run wizard would then either reappear for
/// someone who already decided, or never appear at all.
///
/// Returns the resulting enabled set, sorted.
pub async fn apply(
    pool: &PgPool,
    chosen: &[String],
    source: &str,
    by: Option<&str>,
) -> Result<Vec<String>, String> {
    // What exists is what `bootstrap-isolation` provisioned; reading it here means
    // the caller cannot pass a stale or partial idea of the plugin set.
    let discovered: Vec<String> = sqlx::query_scalar("SELECT id FROM core.plugins ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(|e| format!("read the plugin set: {e}"))?;

    if discovered.is_empty() {
        return Err(
            "no plugins are provisioned in this database — run `bootstrap-isolation` first"
                .to_string(),
        );
    }

    let has = |id: &str| chosen.iter().any(|c| c == id);

    // --- what is wrong, all of it, before anything is written ---------------
    let unknown: Vec<&str> = chosen
        .iter()
        .map(String::as_str)
        .filter(|c| !discovered.iter().any(|id| id == c))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "not a plugin on disk: {}. Discovered: {}",
            unknown.join(", "),
            discovered.join(", ")
        ));
    }

    // Required plugins first: if a set is wrong in both ways, "you disabled auth"
    // is the more urgent sentence.
    // Only required plugins that are actually on disk. A directory without `auth`
    // has nothing to protect, and demanding it would make every set illegal for
    // such a deployment — the rule is "may not be turned off", not "must be
    // installed", which is how the server's own disable refusal reads it too.
    if let Some((id, why)) = REQUIRED_PLUGINS
        .iter()
        .find(|(id, _)| discovered.iter().any(|d| d == id) && !has(id))
    {
        return Err(format!("{id} is required and cannot be turned off: {why}"));
    }

    for (dependent, dependency) in PLUGIN_DEPENDENCIES {
        if has(dependent) && !has(dependency) {
            return Err(format!(
                "{dependent} needs {dependency}, which is not in the set"
            ));
        }
    }

    // --- the write ----------------------------------------------------------
    //
    // Both directions in one statement, and the record in the same transaction:
    // an interrupted run cannot leave the flags half-applied, or a recorded
    // choice that no flag reflects. Re-running converges, so an operator who hit
    // an error can simply run it again.
    let mut tx = pool.begin().await.map_err(|e| format!("begin: {e}"))?;

    sqlx::query(
        "UPDATE core.plugins
            SET enabled = (id = ANY($1)), updated_at = now()
          WHERE id = ANY($2)",
    )
    .bind(chosen)
    .bind(&discovered)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("set the enabled set: {e}"))?;

    sqlx::query(
        "INSERT INTO core.plugin_choice (id, chosen_at, chosen_by, source, plugin_ids)
         VALUES (TRUE, now(), $1, $2, $3)
         ON CONFLICT (id) DO UPDATE
            SET chosen_at = now(), chosen_by = $1, source = $2, plugin_ids = $3",
    )
    .bind(by)
    .bind(source)
    .bind(chosen)
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("record the choice: {e}"))?;

    tx.commit().await.map_err(|e| format!("commit: {e}"))?;

    let mut enabled = discovered
        .into_iter()
        .filter(|id| has(id))
        .collect::<Vec<String>>();
    enabled.sort();
    Ok(enabled)
}

/// Whether an operator has chosen, and what they chose.
///
/// `None` means nobody has — which is the only state in which the first-run
/// wizard may appear. Distinguishing that from "chose everything" is the whole
/// reason this is recorded rather than inferred from the flags.
pub async fn recorded(pool: &PgPool) -> Result<Option<Recorded>, String> {
    sqlx::query_as::<_, Recorded>(
        "SELECT chosen_at, chosen_by, source, plugin_ids
           FROM core.plugin_choice
          WHERE id",
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("read the recorded choice: {e}"))
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Recorded {
    pub chosen_at: chrono::DateTime<chrono::Utc>,
    pub chosen_by: Option<String>,
    pub source: String,
    pub plugin_ids: Vec<String>,
}
