//! `user_states`: whatever the bot is waiting for from a user (a reply, a choice, a confirmation).
//! Every row expires, so an abandoned step cleans itself up.

use serde::{Serialize, de::DeserializeOwned};
use sqlx::PgPool;

pub async fn set<T: Serialize>(
    pg: &PgPool,
    user_id: i64,
    kind: &str,
    data: &T,
    ttl_seconds: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO user_states (user_id, kind, data, expires_at) \
         VALUES ($1, $2, $3, NOW() + make_interval(secs => $4)) \
         ON CONFLICT (user_id, kind) DO UPDATE SET data = EXCLUDED.data, updated_at = NOW(), expires_at = EXCLUDED.expires_at",
    )
    .bind(user_id)
    .bind(kind)
    .bind(sqlx::types::Json(data))
    .bind(ttl_seconds as f64)
    .execute(pg)
    .await?;
    Ok(())
}

pub async fn get<T: DeserializeOwned>(
    pg: &PgPool,
    user_id: i64,
    kind: &str,
) -> sqlx::Result<Option<T>> {
    let row: Option<(serde_json::Value,)> = sqlx::query_as(
        "SELECT data FROM user_states WHERE user_id = $1 AND kind = $2 AND expires_at > NOW()",
    )
    .bind(user_id)
    .bind(kind)
    .fetch_optional(pg)
    .await?;
    Ok(row.and_then(|(json,)| serde_json::from_value(json).ok()))
}

pub async fn delete(pg: &PgPool, user_id: i64, kind: &str) -> sqlx::Result<bool> {
    let done = sqlx::query("DELETE FROM user_states WHERE user_id = $1 AND kind = $2")
        .bind(user_id)
        .bind(kind)
        .execute(pg)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Reads and removes in one statement, so a double tap cannot consume the same state twice.
pub async fn take<T: DeserializeOwned>(
    pg: &PgPool,
    user_id: i64,
    kind: &str,
) -> sqlx::Result<Option<T>> {
    let row: Option<(serde_json::Value,)> = sqlx::query_as(
        "DELETE FROM user_states WHERE user_id = $1 AND kind = $2 AND expires_at > NOW() RETURNING data",
    )
    .bind(user_id)
    .bind(kind)
    .fetch_optional(pg)
    .await?;
    Ok(row.and_then(|(json,)| serde_json::from_value(json).ok()))
}

pub async fn purge_expired(pg: &PgPool) -> sqlx::Result<u64> {
    let a = sqlx::query("DELETE FROM user_states WHERE expires_at <= NOW()")
        .execute(pg)
        .await?;
    let b = sqlx::query("DELETE FROM pending_actions WHERE expires_at <= NOW()")
        .execute(pg)
        .await?;
    Ok(a.rows_affected() + b.rows_affected())
}

/// Rows that can no longer change anything: finished tasks after 30 days, subscription checks older than
/// their day of validity, rate-limit rows that are back to the starting state, and expired usernames.
pub async fn prune_history(pg: &PgPool) -> sqlx::Result<u64> {
    let tasks = sqlx::query(
        "DELETE FROM task_queue WHERE status IN ('done', 'failed', 'cancelled') AND finished_at < NOW() - INTERVAL '30 days'",
    )
    .execute(pg)
    .await?;
    let checks = sqlx::query("DELETE FROM fsub_checks WHERE passed_at < NOW() - INTERVAL '1 day'")
        .execute(pg)
        .await?;
    let limits = sqlx::query(
        "DELETE FROM rate_limits WHERE blocked_until < EXTRACT(EPOCH FROM NOW()) \
         AND last_strike_at < EXTRACT(EPOCH FROM NOW()) - 3600 AND minute_start < EXTRACT(EPOCH FROM NOW()) - 60",
    )
    .execute(pg)
    .await?;
    let usernames =
        sqlx::query("DELETE FROM username_cache WHERE resolved_at < NOW() - INTERVAL '8 hours'")
            .execute(pg)
            .await?;
    Ok(tasks.rows_affected()
        + checks.rows_affected()
        + limits.rows_affected()
        + usernames.rows_affected())
}
