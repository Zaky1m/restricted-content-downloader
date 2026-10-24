//! `task_queue`: every user task, its place in line and its end state. An advisory lock keeps two
//! tasks from taking the same last slot.

use serde_json::Value;
use sqlx::PgPool;

use crate::queue::{Counts, Turn, next_turn, open_task_limit};

const LOCK_KEY: i64 = 7_310_001;

#[derive(Debug, Clone)]
pub struct OpenTask {
    pub queue_id: i64,
    pub kind: String,
    pub status: String,
}

/// Adds a task unless the user already has the allowed number open. `None` means the limit was reached.
pub async fn add(
    pg: &PgPool,
    user_id: i64,
    kind: &str,
    spec: &Value,
    is_premium: bool,
) -> sqlx::Result<Option<i64>> {
    let mut tx = pg.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_queue WHERE user_id = $1 AND status IN ('queued', 'running')",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    if open >= open_task_limit(is_premium) {
        tx.commit().await?;
        return Ok(None);
    }
    let id: i64 = sqlx::query_scalar("INSERT INTO task_queue (user_id, kind, spec, is_premium) VALUES ($1, $2, $3, $4) RETURNING queue_id")
        .bind(user_id)
        .bind(kind)
        .bind(spec)
        .bind(is_premium)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(id))
}

pub async fn status(pg: &PgPool, queue_id: i64) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT status FROM task_queue WHERE queue_id = $1")
        .bind(queue_id)
        .fetch_optional(pg)
        .await
}

/// Starts the task if its turn has come. `Turn::Wait(0)` means the row is gone or no longer queued.
pub async fn try_start(pg: &PgPool, queue_id: i64) -> sqlx::Result<Turn> {
    let mut tx = pg.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let row: Option<(i64, bool, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT user_id, is_premium, status, created_at FROM task_queue WHERE queue_id = $1",
    )
    .bind(queue_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((user_id, is_premium, status, created_at)) = row else {
        tx.commit().await?;
        return Ok(Turn::Wait(0));
    };
    if status != "queued" {
        tx.commit().await?;
        return Ok(Turn::Wait(0));
    }
    let mut ahead_of_own: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_queue WHERE user_id = $1 AND queue_id <> $2 \
         AND (status = 'running' OR (status = 'queued' AND created_at < $3))",
    )
    .bind(user_id)
    .bind(queue_id)
    .bind(created_at)
    .fetch_one(&mut *tx)
    .await?;
    ahead_of_own += sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM monitor_jobs WHERE user_id = $1 AND status = 'running'",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    let running_free: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_queue WHERE status = 'running' AND NOT is_premium",
    )
    .fetch_one(&mut *tx)
    .await?;
    let queued_free_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM task_queue WHERE status = 'queued' AND NOT is_premium AND created_at < $1")
            .bind(created_at)
            .fetch_one(&mut *tx)
            .await?;
    let turn = next_turn(&Counts {
        is_premium,
        ahead_of_own,
        running_free,
        queued_free_before,
    });
    if turn == Turn::Start {
        sqlx::query(
            "UPDATE task_queue SET status = 'running', started_at = NOW() WHERE queue_id = $1",
        )
        .bind(queue_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(turn)
}

pub async fn finish(pg: &PgPool, queue_id: i64, status: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE task_queue SET status = $2, finished_at = NOW() WHERE queue_id = $1 AND status IN ('queued', 'running')")
        .bind(queue_id)
        .bind(status)
        .execute(pg)
        .await?;
    Ok(())
}

/// Merges `values` into the task's saved spec.
pub async fn set_spec(pg: &PgPool, queue_id: i64, values: &Value) -> sqlx::Result<()> {
    sqlx::query("UPDATE task_queue SET spec = spec || $2 WHERE queue_id = $1")
        .bind(queue_id)
        .bind(values)
        .execute(pg)
        .await?;
    Ok(())
}

pub async fn spec(pg: &PgPool, queue_id: i64, user_id: i64) -> sqlx::Result<Option<Value>> {
    let row: Option<(Value,)> =
        sqlx::query_as("SELECT spec FROM task_queue WHERE queue_id = $1 AND user_id = $2")
            .bind(queue_id)
            .bind(user_id)
            .fetch_optional(pg)
            .await?;
    Ok(row.map(|r| r.0))
}

/// Marks the user's open tasks (or one of them) cancelled and returns their ids.
pub async fn cancel(pg: &PgPool, user_id: i64, queue_id: Option<i64>) -> sqlx::Result<Vec<i64>> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        "UPDATE task_queue SET status = 'cancelled', finished_at = NOW() \
         WHERE user_id = $1 AND ($2::bigint IS NULL OR queue_id = $2) AND status IN ('queued', 'running') RETURNING queue_id",
    )
    .bind(user_id)
    .bind(queue_id)
    .fetch_all(pg)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

pub async fn open_tasks(pg: &PgPool, user_id: i64) -> sqlx::Result<Vec<OpenTask>> {
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT queue_id, kind, status FROM task_queue WHERE user_id = $1 AND status IN ('queued', 'running') ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(pg)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(queue_id, kind, status)| OpenTask {
            queue_id,
            kind,
            status,
        })
        .collect())
}

/// Rows left `queued` or `running` whose owner has none of them in memory (after a crash).
pub async fn all_open(pg: &PgPool) -> sqlx::Result<Vec<(i64, i64, String, String, Value)>> {
    sqlx::query_as("SELECT queue_id, user_id, kind, status, spec FROM task_queue WHERE status IN ('queued', 'running') ORDER BY queue_id")
        .fetch_all(pg)
        .await
}
