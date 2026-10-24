use sqlx::PgPool;

use crate::ratelimit::{self, Kind, Notice, State};

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Message => "message",
        Kind::Button => "button",
    }
}

/// Counts one message or button press under a row lock. A database failure lets the user through,
/// so an outage never locks everyone out.
pub async fn check(pg: &PgPool, user_id: i64, kind: Kind, now: f64) -> (bool, Option<Notice>) {
    match try_check(pg, user_id, kind, now).await {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(user_id, %error, "rate limit check failed");
            (true, None)
        }
    }
}

async fn try_check(
    pg: &PgPool,
    user_id: i64,
    kind: Kind,
    now: f64,
) -> sqlx::Result<(bool, Option<Notice>)> {
    let name = kind_name(kind);
    let mut tx = pg.begin().await?;
    sqlx::query("INSERT INTO rate_limits (user_id, kind) VALUES ($1, $2) ON CONFLICT (user_id, kind) DO NOTHING")
        .bind(user_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    let row: (f64, i32, f64, i32, f64, i32, f64) = sqlx::query_as(
        "SELECT second_start, second_count, minute_start, minute_count, blocked_until, strikes, last_strike_at \
         FROM rate_limits WHERE user_id = $1 AND kind = $2 FOR UPDATE",
    )
    .bind(user_id)
    .bind(name)
    .fetch_one(&mut *tx)
    .await?;
    let mut state = State {
        second_start: row.0,
        second_count: row.1 as u32,
        minute_start: row.2,
        minute_count: row.3 as u32,
        blocked_until: row.4,
        strikes: row.5 as u32,
        last_strike_at: row.6,
    };
    let result = ratelimit::check(&mut state, kind, now);
    sqlx::query(
        "UPDATE rate_limits SET second_start = $3, second_count = $4, minute_start = $5, minute_count = $6, \
         blocked_until = $7, strikes = $8, last_strike_at = $9 WHERE user_id = $1 AND kind = $2",
    )
    .bind(user_id)
    .bind(name)
    .bind(state.second_start)
    .bind(state.second_count as i32)
    .bind(state.minute_start)
    .bind(state.minute_count as i32)
    .bind(state.blocked_until)
    .bind(state.strikes as i32)
    .bind(state.last_strike_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result)
}
