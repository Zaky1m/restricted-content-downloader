//! `pending_actions`: one-shot choices a user is asked to make (confirm a channel, pick how to send a big file).
//! `take` reads and deletes in one statement, so a double tap acts only once.

use serde_json::Value;
use sqlx::PgPool;

const MAX_TTL: i64 = 24 * 60 * 60;

pub async fn create(
    pg: &PgPool,
    user_id: i64,
    kind: &str,
    data: &Value,
    ttl_seconds: i64,
) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "INSERT INTO pending_actions (user_id, kind, data, expires_at) VALUES ($1, $2, $3, NOW() + make_interval(secs => $4)) RETURNING action_id",
    )
    .bind(user_id)
    .bind(kind)
    .bind(data)
    .bind(ttl_seconds.min(MAX_TTL) as f64)
    .fetch_one(pg)
    .await
}

pub async fn get(
    pg: &PgPool,
    action_id: i64,
    user_id: i64,
) -> sqlx::Result<Option<(String, Value)>> {
    sqlx::query_as("SELECT kind, data FROM pending_actions WHERE action_id = $1 AND user_id = $2 AND expires_at > NOW()")
        .bind(action_id)
        .bind(user_id)
        .fetch_optional(pg)
        .await
}

pub async fn take(
    pg: &PgPool,
    action_id: i64,
    user_id: i64,
) -> sqlx::Result<Option<(String, Value)>> {
    sqlx::query_as("DELETE FROM pending_actions WHERE action_id = $1 AND user_id = $2 AND expires_at > NOW() RETURNING kind, data")
        .bind(action_id)
        .bind(user_id)
        .fetch_optional(pg)
        .await
}

pub async fn delete(pg: &PgPool, action_id: i64) {
    let _ = sqlx::query("DELETE FROM pending_actions WHERE action_id = $1")
        .bind(action_id)
        .execute(pg)
        .await;
}

/// Merges fields into the saved data.
pub async fn merge(pg: &PgPool, action_id: i64, values: &Value) {
    let _ = sqlx::query("UPDATE pending_actions SET data = data || $2 WHERE action_id = $1")
        .bind(action_id)
        .bind(values)
        .execute(pg)
        .await;
}

/// Removes and returns the expired rows of one kind.
pub async fn take_expired(pg: &PgPool, kind: &str) -> sqlx::Result<Vec<(i64, Value)>> {
    sqlx::query_as("DELETE FROM pending_actions WHERE kind = $1 AND expires_at <= NOW() RETURNING user_id, data").bind(kind).fetch_all(pg).await
}
