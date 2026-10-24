//! What the admin panel changes: subscriptions, gifts, required channels and the admin list.

use chrono::{DateTime, Duration, Utc};
use rand::RngExt;
use sqlx::PgPool;

pub async fn admins(pg: &PgPool) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar("SELECT user_id FROM bot_admins ORDER BY added_at")
        .fetch_all(pg)
        .await
}

/// True when the user was added, false when they already were an admin.
pub async fn add_admin(pg: &PgPool, user_id: i64, added_by: i64) -> sqlx::Result<bool> {
    let done = sqlx::query("INSERT INTO bot_admins (user_id, added_by) VALUES ($1, $2) ON CONFLICT (user_id) DO NOTHING")
        .bind(user_id)
        .bind(added_by)
        .execute(pg)
        .await?;
    Ok(done.rows_affected() > 0)
}

pub async fn remove_admin(pg: &PgPool, user_id: i64) -> sqlx::Result<bool> {
    let done = sqlx::query("DELETE FROM bot_admins WHERE user_id = $1")
        .bind(user_id)
        .execute(pg)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Adds `days` to each user's subscription (to the time left, or from now). Returns `(user, new expiry)`; unknown users are left out.
pub async fn grant_premium(
    pg: &PgPool,
    user_ids: &[i64],
    days: i64,
) -> sqlx::Result<Vec<(i64, DateTime<Utc>)>> {
    sqlx::query_as(
        "UPDATE users SET is_premium = TRUE, premium_added_by_admin = TRUE, \
         premium_expiry_date = GREATEST(COALESCE(premium_expiry_date, NOW()), NOW()) + make_interval(days => $2::int) \
         WHERE user_id = ANY($1) RETURNING user_id, premium_expiry_date",
    )
    .bind(user_ids)
    .bind(days as i32)
    .fetch_all(pg)
    .await
}

/// Ends the subscription of each listed subscriber and resets what only subscribers have. Returns the users changed.
pub async fn remove_premium(pg: &PgPool, user_ids: &[i64]) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar(
        "UPDATE users SET is_premium = FALSE, premium_expiry_date = NULL, premium_added_by_admin = FALSE, force_download_enabled = FALSE, \
         caption_mode = 'keep_all', monitor_target_id = NULL, monitor_flags = NULL, upload_dest_id = NULL \
         WHERE user_id = ANY($1) AND is_premium = TRUE RETURNING user_id",
    )
    .bind(user_ids)
    .fetch_all(pg)
    .await
}

pub struct NewGift {
    pub value: i32,
    pub link_expiry: Option<DateTime<Utc>>,
    pub point_seconds: Option<i64>,
    pub only_users: Vec<i64>,
    pub exclude_users: Vec<i64>,
    pub limit: Option<i32>,
}

pub fn gift_pid() -> String {
    let bytes: [u8; 8] = rand::rng().random();
    format!(
        "gift_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

pub async fn create_gift(pg: &PgPool, gift: &NewGift) -> sqlx::Result<String> {
    let pid = gift_pid();
    let list = |ids: &[i64]| (!ids.is_empty()).then(|| serde_json::json!(ids));
    sqlx::query(
        "INSERT INTO gifts (gift_pid, value, link_expiry, gift_expiry_duration_seconds, allowed_users, excluded_users, usage_limit) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&pid)
    .bind(gift.value)
    .bind(gift.link_expiry)
    .bind(gift.point_seconds)
    .bind(list(&gift.only_users))
    .bind(list(&gift.exclude_users))
    .bind(gift.limit)
    .execute(pg)
    .await?;
    Ok(pid)
}

pub type GiftRow = (
    String,
    i32,
    Option<DateTime<Utc>>,
    Option<i32>,
    Option<i32>,
    DateTime<Utc>,
);

pub async fn active_gifts(pg: &PgPool, limit: i64, offset: i64) -> sqlx::Result<Vec<GiftRow>> {
    sqlx::query_as(
        "SELECT gift_pid, value, link_expiry, usage_limit, usage_count, created_at FROM gifts WHERE is_active \
         ORDER BY created_at DESC, gift_pid LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(pg)
    .await
}

pub async fn gift_summary(pg: &PgPool, pid: &str) -> sqlx::Result<Option<(i32, Option<i32>)>> {
    sqlx::query_as("SELECT value, usage_count FROM gifts WHERE gift_pid = $1 AND is_active")
        .bind(pid)
        .fetch_optional(pg)
        .await
}

pub async fn deactivate_gift(pg: &PgPool, pid: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE gifts SET is_active = FALSE WHERE gift_pid = $1")
        .bind(pid)
        .execute(pg)
        .await?;
    Ok(())
}

pub async fn add_fsub(
    pg: &PgPool,
    channel_id: i64,
    link: &str,
    limit: i32,
    members: i32,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO fsub_channels (channel_id, link, limit_val, joined_count, date_added, initial_members) VALUES ($1, $2, $3, 0, NOW(), $4) \
         ON CONFLICT (channel_id) DO UPDATE SET link = EXCLUDED.link, limit_val = EXCLUDED.limit_val, initial_members = EXCLUDED.initial_members",
    )
    .bind(channel_id)
    .bind(link)
    .bind(limit)
    .bind(members)
    .execute(pg)
    .await?;
    Ok(())
}

pub async fn remove_fsub(pg: &PgPool, channel_id: i64) -> sqlx::Result<bool> {
    let done = sqlx::query("DELETE FROM fsub_channels WHERE channel_id = $1")
        .bind(channel_id)
        .execute(pg)
        .await?;
    Ok(done.rows_affected() > 0)
}

pub type FsubRow = (i64, String, Option<i32>, Option<i32>);

pub async fn fsub_channels(pg: &PgPool) -> sqlx::Result<Vec<FsubRow>> {
    sqlx::query_as(
        "SELECT channel_id, link, limit_val, joined_count FROM fsub_channels ORDER BY date_added",
    )
    .fetch_all(pg)
    .await
}

pub fn parse_duration_seconds(text: &str) -> Option<i64> {
    crate::plans::parse_duration(text.trim_matches(['[', ']']))
}

pub fn link_expiry_from(seconds: Option<i64>) -> Option<DateTime<Utc>> {
    seconds.map(|s| Utc::now() + Duration::seconds(s))
}

/// One CSV row per user, every cell quoted.
pub async fn users_csv(pg: &PgPool) -> sqlx::Result<(String, usize)> {
    type Row = (
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<bool>,
        Option<bool>,
        Option<DateTime<Utc>>,
        Option<i32>,
        Option<i32>,
        Option<i32>,
        Option<bool>,
        Option<i64>,
        Option<i64>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT user_id, first_name, last_name, username, language_code, join_date, last_activity_date, is_active, is_premium, \
         premium_expiry_date, referral_points, referral_count, daily_downloads_left, force_download_enabled, upload_dest_id, monitor_target_id \
         FROM users ORDER BY user_id",
    )
    .fetch_all(pg)
    .await?;
    let headers = [
        "UserID",
        "FirstName",
        "LastName",
        "Username",
        "Language",
        "JoinDate",
        "LastActivity",
        "IsActive",
        "IsPremium",
        "PremiumExpiry",
        "ReferralPoints",
        "ReferralCount",
        "DailyPoints",
        "ForceDownload",
        "UploadDestination",
        "MonitorTarget",
    ];
    let mut out = headers.join(",");
    out.push('\n');
    let cell =
        |value: Option<String>| format!("\"{}\"", value.unwrap_or_default().replace('"', "\"\""));
    let when = |d: Option<DateTime<Utc>>| d.map(|d| d.to_string());
    let flag = |b: Option<bool>| b.map(|b| if b { "True" } else { "False" }.to_string());
    for r in &rows {
        let cells = [
            cell(Some(r.0.to_string())),
            cell(r.1.clone()),
            cell(r.2.clone()),
            cell(r.3.clone()),
            cell(r.4.clone()),
            cell(when(r.5)),
            cell(when(r.6)),
            cell(flag(r.7)),
            cell(flag(r.8)),
            cell(when(r.9)),
            cell(r.10.map(|v| v.to_string())),
            cell(r.11.map(|v| v.to_string())),
            cell(r.12.map(|v| v.to_string())),
            cell(flag(r.13)),
            cell(r.14.map(|v| v.to_string())),
            cell(r.15.map(|v| v.to_string())),
        ];
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    Ok((out, rows.len()))
}
