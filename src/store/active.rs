//! `active_tasks`: where a batch or clone stopped, so a restart continues from there.

use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct Active {
    pub id: i64,
    pub user_id: i64,
    pub kind: String,
    pub payload: Value,
    pub flags: Vec<String>,
    pub start_id: i64,
    pub end_id: i64,
    pub last_done_id: i64,
    pub status_message_id: i64,
    pub failed_ids: Vec<i64>,
    pub retry_phase: bool,
    pub attempts: i32,
}

/// Replaces the user's saved task of this kind.
#[allow(clippy::too_many_arguments)]
pub async fn save(
    pg: &PgPool,
    user_id: i64,
    kind: &str,
    payload: &Value,
    flags: &[String],
    start_id: i64,
    end_id: i64,
    status_id: i64,
) -> sqlx::Result<i64> {
    let mut tx = pg.begin().await?;
    sqlx::query("DELETE FROM active_tasks WHERE user_id = $1 AND task_type = $2")
        .bind(user_id)
        .bind(kind)
        .execute(&mut *tx)
        .await?;
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO active_tasks (user_id, task_type, source_ref, flags, start_id, end_id, last_done_id, progress_msg_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING task_db_id",
    )
    .bind(user_id)
    .bind(kind)
    .bind(payload.to_string())
    .bind(serde_json::to_string(flags).unwrap_or_else(|_| "[]".into()))
    .bind(start_id)
    .bind(end_id)
    .bind(start_id - 1)
    .bind(status_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(i64::from(id))
}

pub async fn progress(
    pg: &PgPool,
    id: i64,
    last_done: i64,
    failed: Option<&[i64]>,
    retry_phase: Option<bool>,
) {
    let failed_json = failed.map(|f| serde_json::to_string(f).unwrap_or_else(|_| "[]".into()));
    let result = sqlx::query(
        "UPDATE active_tasks SET last_done_id = $2, failed_ids = COALESCE($3, failed_ids), retry_phase = COALESCE($4, retry_phase) WHERE task_db_id = $1",
    )
    .bind(id as i32)
    .bind(last_done)
    .bind(failed_json)
    .bind(retry_phase)
    .execute(pg)
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, "failed to update task progress");
    }
}

pub async fn clear(pg: &PgPool, id: i64) {
    let _ = sqlx::query("DELETE FROM active_tasks WHERE task_db_id = $1")
        .bind(id as i32)
        .execute(pg)
        .await;
}

type Row = (
    i32,
    i64,
    String,
    String,
    Option<String>,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<bool>,
    Option<i32>,
);

fn convert(r: Row) -> Active {
    Active {
        id: i64::from(r.0),
        user_id: r.1,
        kind: r.2,
        payload: serde_json::from_str(&r.3).unwrap_or(Value::Null),
        flags: r
            .4
            .and_then(|f| serde_json::from_str(&f).ok())
            .unwrap_or_default(),
        start_id: r.5,
        end_id: r.6,
        last_done_id: r.7.unwrap_or(0),
        status_message_id: r.8.unwrap_or(0),
        failed_ids: r
            .9
            .and_then(|f| serde_json::from_str(&f).ok())
            .unwrap_or_default(),
        retry_phase: r.10.unwrap_or(false),
        attempts: r.11.unwrap_or(0),
    }
}

pub async fn all(pg: &PgPool) -> sqlx::Result<Vec<Active>> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT task_db_id, user_id, task_type, source_ref, flags, start_id, end_id, last_done_id, progress_msg_id, failed_ids, retry_phase, attempt_count \
         FROM active_tasks ORDER BY task_db_id",
    )
    .fetch_all(pg)
    .await?;
    Ok(rows.into_iter().map(convert).collect())
}

pub async fn bump_attempts(pg: &PgPool, id: i64) {
    let _ = sqlx::query("UPDATE active_tasks SET attempt_count = COALESCE(attempt_count, 0) + 1 WHERE task_db_id = $1").bind(id as i32).execute(pg).await;
}
