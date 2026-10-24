//! Gift links (`/start gift_<id>`): points that may expire, with limits on who and how often.

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    Granted {
        value: i32,
        expires: Option<DateTime<Utc>>,
    },
    AlreadyClaimed,
    Invalid,
    Inactive,
    LinkExpired,
    LimitReached,
    NotAllowed,
    Excluded,
}

impl Claim {
    pub fn locale_key(&self) -> &'static str {
        match self {
            Claim::Granted {
                expires: Some(_), ..
            } => "gifts.claim.completed_with_expiry",
            Claim::Granted { .. } => "gifts.claim.completed",
            Claim::AlreadyClaimed => "gifts.claim.already_claimed",
            Claim::Invalid => "gifts.claim.invalid",
            Claim::Inactive => "gifts.claim.inactive",
            Claim::LinkExpired => "gifts.claim.link_expired",
            Claim::LimitReached => "gifts.claim.limit_reached",
            Claim::NotAllowed => "gifts.claim.not_allowed",
            Claim::Excluded => "gifts.claim.excluded",
        }
    }
}

fn ids(value: &Option<Value>) -> Vec<i64> {
    match value {
        Some(Value::Array(list)) => list.iter().filter_map(Value::as_i64).collect(),
        Some(Value::String(text)) => serde_json::from_str::<Vec<i64>>(text).unwrap_or_default(),
        _ => Vec::new(),
    }
}

type GiftRow = (
    i32,
    Option<DateTime<Utc>>,
    Option<i64>,
    Option<Value>,
    Option<Value>,
    Option<bool>,
    Option<i32>,
    Option<i32>,
);

pub async fn claim(pg: &PgPool, user_id: i64, gift_pid: &str) -> sqlx::Result<Claim> {
    let mut tx = pg.begin().await?;
    let claimed: Option<(i32,)> =
        sqlx::query_as("SELECT claim_id FROM claimed_gifts WHERE user_id = $1 AND gift_pid = $2")
            .bind(user_id)
            .bind(gift_pid)
            .fetch_optional(&mut *tx)
            .await?;
    if claimed.is_some() {
        return Ok(Claim::AlreadyClaimed);
    }
    let gift: Option<GiftRow> = sqlx::query_as(
        "SELECT value, link_expiry, gift_expiry_duration_seconds, allowed_users, excluded_users, is_active, usage_limit, usage_count \
         FROM gifts WHERE gift_pid = $1 FOR UPDATE",
    )
    .bind(gift_pid)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((value, link_expiry, duration, allowed, excluded, active, limit, used)) = gift else {
        return Ok(Claim::Invalid);
    };
    let now = Utc::now();
    if !active.unwrap_or(true) {
        return Ok(Claim::Inactive);
    }
    if link_expiry.is_some_and(|e| now > e) {
        return Ok(Claim::LinkExpired);
    }
    if limit.is_some_and(|l| used.unwrap_or(0) >= l) {
        return Ok(Claim::LimitReached);
    }
    let allowed = ids(&allowed);
    if !allowed.is_empty() && !allowed.contains(&user_id) {
        return Ok(Claim::NotAllowed);
    }
    if ids(&excluded).contains(&user_id) {
        return Ok(Claim::Excluded);
    }
    let expires = duration
        .filter(|d| *d > 0)
        .map(|d| now + Duration::seconds(d));
    sqlx::query("INSERT INTO user_gift_points (user_id, points, expiry_date) VALUES ($1, $2, $3)")
        .bind(user_id)
        .bind(value)
        .bind(expires)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO claimed_gifts (user_id, gift_pid) VALUES ($1, $2)")
        .bind(user_id)
        .bind(gift_pid)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE gifts SET usage_count = usage_count + 1 WHERE gift_pid = $1")
        .bind(gift_pid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Claim::Granted { value, expires })
}
