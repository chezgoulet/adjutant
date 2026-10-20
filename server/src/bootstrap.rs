//! The first chief, created from the environment (#173).
//!
//! `POST /api/auth/register` still mints a chief for whoever arrives first on an
//! untouched database — that is the approved bootstrap path — and the
//! `core.bootstrap_claim` row makes exactly one caller win. This module is the
//! other, deterministic way in: a deployment that sets
//! `ADJUTANT_BOOTSTRAP_USERNAME` and `ADJUTANT_BOOTSTRAP_PASSWORD` gets its chief
//! at boot, before the listener opens, so the troop never depends on an
//! anonymous HTTP race at all.
//!
//! It never touches a database that already has users. An operator who rotates
//! the password of an existing chief must do that through the API or the
//! `adjutant grant-role`/reset paths — silently rewriting an account from the
//! environment on every boot would be a backdoor, not a bootstrap.

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::Argon2;
use sqlx::PgPool;

use crate::config::BootstrapChief;

/// Create the first chief when the deployment is untouched, and do nothing when
/// it is not.
///
/// Returns an error only for a genuine failure to create the account; "the
/// database already has users" is an `Ok(())` with an info line, because the
/// operator's environment may simply outlive the bootstrap.
pub async fn ensure_first_chief(
    pool: &PgPool,
    chief: Option<&BootstrapChief>,
) -> Result<(), sqlx::Error> {
    let Some(chief) = chief else {
        return Ok(());
    };

    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM core.users")
        .fetch_one(pool)
        .await?;
    if users > 0 {
        tracing::info!(
            username = %chief.username,
            users,
            "ADJUTANT_BOOTSTRAP_* is set but the database already has users; \
             leaving every account untouched"
        );
        return Ok(());
    }

    // Spend the bootstrap slot in the same transaction as the user. This is the
    // same row `POST /api/auth/register` claims, so the two paths cannot both
    // create a first chief even if one is in flight while the other runs.
    let mut tx = pool.begin().await?;

    // A random opaque token; the column is TEXT on purpose so this needs no UUID
    // feature and no extension.
    let mut claim_bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut claim_bytes);
    let claim_token: String = claim_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let claimed = sqlx::query(
        "INSERT INTO core.bootstrap_claim (id, claim_token) VALUES (1, $1) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(claim_token)
    .execute(&mut *tx)
    .await?;
    if claimed.rows_affected() == 0 {
        // Another path claimed the slot first; the users count above was read
        // outside the transaction, so this is the race resolving, not an error.
        tx.rollback().await?;
        tracing::info!(
            "the bootstrap slot was claimed by another path; not creating the environment chief"
        );
        return Ok(());
    }

    let salt = SaltString::generate(&mut rand::rngs::OsRng);
    let hash = Argon2::default()
        .hash_password(chief.password.as_bytes(), &salt)
        .map_err(|e| sqlx::Error::Protocol(format!("argon2 hash failed: {e}")))?
        .to_string();

    let user_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO core.users (username, email, display_name, password_hash) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(&chief.username)
    .bind(chief.email.as_deref())
    .bind(&chief.username)
    .bind(&hash)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO core.user_roles (user_id, role_id) VALUES ($1, 'chief') \
         ON CONFLICT DO NOTHING",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    tracing::warn!(
        username = %chief.username,
        "created the first chief from ADJUTANT_BOOTSTRAP_*; remove \
         ADJUTANT_BOOTSTRAP_PASSWORD from the environment now that the account exists"
    );
    Ok(())
}
