//! A user's plan, points and settings, with the housekeeping the Python `get_user_data` did on
//! every read: expired gift points are removed, the daily allowance resets, an expired premium ends.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

use crate::plans::DAILY_DOWNLOAD_LIMIT;

#[derive(Debug, Clone)]
pub struct UserData {
    pub user_id: i64,
    pub daily_downloads_left: i64,
    pub daily_downloads_reset_at: Option<DateTime<Utc>>,
    pub referral_points: i64,
    pub referred_by: Option<i64>,
    pub referral_count: i64,
    pub is_premium: bool,
    pub premium_expiry_date: Option<DateTime<Utc>>,
    pub join_date: Option<DateTime<Utc>>,
    pub caption_mode: String,
    pub thumbnail_path: Option<String>,
    pub thumbnail_enabled: bool,
    pub force_download_enabled: bool,
    pub gift_points: i64,
}

type Row = (
    i64,
    Option<i32>,
    Option<DateTime<Utc>>,
    Option<i32>,
    Option<i64>,
    Option<i32>,
    Option<bool>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<String>,
    Option<bool>,
    Option<bool>,
);

pub async fn load(pg: &PgPool, user_id: i64) -> sqlx::Result<Option<UserData>> {
    let now = Utc::now();
    let mut tx = pg.begin().await?;
    sqlx::query("DELETE FROM user_gift_points WHERE expiry_date IS NOT NULL AND expiry_date < $1")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    let row: Option<Row> = sqlx::query_as(
        "SELECT user_id, daily_downloads_left, daily_downloads_reset_at, referral_points, referred_by, referral_count, \
         is_premium, premium_expiry_date, join_date, caption_mode, thumbnail_path, thumbnail_enabled, force_download_enabled \
         FROM users WHERE user_id = $1 FOR UPDATE",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(r) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    let gift_points: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(points), 0)::bigint FROM user_gift_points WHERE user_id = $1 AND points > 0")
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
    let mut data = UserData {
        user_id: r.0,
        daily_downloads_left: i64::from(r.1.unwrap_or(0)),
        daily_downloads_reset_at: r.2,
        referral_points: i64::from(r.3.unwrap_or(0)),
        referred_by: r.4,
        referral_count: i64::from(r.5.unwrap_or(0)),
        is_premium: r.6.unwrap_or(false),
        premium_expiry_date: r.7,
        join_date: r.8,
        caption_mode: r.9.unwrap_or_else(|| "keep_all".into()),
        thumbnail_path: r.10,
        thumbnail_enabled: r.11.unwrap_or(false),
        force_download_enabled: r.12.unwrap_or(false),
        gift_points,
    };

    if data.daily_downloads_reset_at.is_none_or(|at| now >= at) {
        let next = now + Duration::hours(24);
        sqlx::query("UPDATE users SET daily_downloads_left = $2, daily_downloads_reset_at = $3 WHERE user_id = $1")
            .bind(user_id)
            .bind(DAILY_DOWNLOAD_LIMIT as i32)
            .bind(next)
            .execute(&mut *tx)
            .await?;
        data.daily_downloads_left = DAILY_DOWNLOAD_LIMIT;
        data.daily_downloads_reset_at = Some(next);
    }

    let expired = data.is_premium && data.premium_expiry_date.is_some_and(|at| now >= at);
    if expired {
        sqlx::query(
            "UPDATE users SET is_premium = FALSE, premium_expiry_date = NULL, premium_added_by_admin = FALSE, \
             caption_mode = 'keep_all', monitor_target_id = NULL, monitor_flags = NULL, upload_dest_id = NULL, \
             force_download_enabled = FALSE WHERE user_id = $1",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        data.is_premium = false;
        data.premium_expiry_date = None;
        data.caption_mode = "keep_all".into();
        data.force_download_enabled = false;
    } else if !data.is_premium && data.force_download_enabled {
        sqlx::query("UPDATE users SET force_download_enabled = FALSE WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        data.force_download_enabled = false;
    }
    tx.commit().await?;
    Ok(Some(data))
}

/// Takes `cost` points in the order daily, gift (soonest expiry first), referral. Premium users pay nothing.
/// False when the points are short; nothing is taken then.
pub async fn spend_points(pg: &PgPool, user_id: i64, cost: i64) -> sqlx::Result<bool> {
    use crate::plans::{GiftBucket, Wallet, spend_points as plan};
    if cost <= 0 {
        return Ok(true);
    }
    let mut tx = pg.begin().await?;
    let row: Option<(Option<bool>, Option<i32>, Option<i32>)> =
        sqlx::query_as("SELECT is_premium, daily_downloads_left, referral_points FROM users WHERE user_id = $1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((premium, daily, referral)) = row else {
        return Ok(false);
    };
    if premium.unwrap_or(false) {
        return Ok(true);
    }
    let gifts: Vec<(i32, i32, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT id, points, expiry_date FROM user_gift_points WHERE user_id = $1 AND points > 0 \
         AND (expiry_date IS NULL OR expiry_date > NOW()) ORDER BY expiry_date ASC NULLS LAST, id ASC FOR UPDATE",
    )
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await?;
    let wallet = Wallet {
        daily: i64::from(daily.unwrap_or(0)),
        referral: i64::from(referral.unwrap_or(0)),
        gifts: gifts
            .iter()
            .map(|(id, points, expiry)| GiftBucket {
                id: i64::from(*id),
                points: i64::from(*points),
                expires_at: expiry.map(|e| e.timestamp()),
            })
            .collect(),
    };
    let Some(spent) = plan(&wallet, cost, Utc::now().timestamp()) else {
        return Ok(false);
    };
    if spent.daily > 0 {
        sqlx::query(
            "UPDATE users SET daily_downloads_left = daily_downloads_left - $2 WHERE user_id = $1",
        )
        .bind(user_id)
        .bind(spent.daily as i32)
        .execute(&mut *tx)
        .await?;
    }
    for (id, take) in spent.gifts {
        sqlx::query("UPDATE user_gift_points SET points = points - $2 WHERE id = $1")
            .bind(id as i32)
            .bind(take as i32)
            .execute(&mut *tx)
            .await?;
    }
    if spent.referral > 0 {
        sqlx::query("UPDATE users SET referral_points = referral_points - $2 WHERE user_id = $1")
            .bind(user_id)
            .bind(spent.referral as i32)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM user_gift_points WHERE user_id = $1 AND points <= 0")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}
