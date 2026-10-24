use sqlx::PgPool;

use crate::i18n::Locales;

pub struct Profile<'a> {
    pub user_id: i64,
    pub first_name: &'a str,
    pub last_name: Option<&'a str>,
    pub username: Option<&'a str>,
}

impl Profile<'_> {
    fn stored_username(&self) -> Option<String> {
        self.username.map(|u| format!("@{u}"))
    }
}

pub async fn lang(pg: &PgPool, locales: &Locales, user_id: i64) -> String {
    let row: Result<Option<(Option<String>,)>, _> =
        sqlx::query_as("SELECT language_code FROM users WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(pg)
            .await;
    let code = row.ok().flatten().and_then(|(c,)| c);
    // A stored code without a file (one that was merged into another) still finds its language.
    locales.normalize(code.as_deref())
}

pub async fn set_lang(
    pg: &PgPool,
    locales: &Locales,
    user_id: i64,
    code: &str,
) -> sqlx::Result<()> {
    let code = if locales.has(code) {
        code
    } else {
        crate::i18n::DEFAULT_LANGUAGE
    };
    sqlx::query("UPDATE users SET language_code = $2 WHERE user_id = $1")
        .bind(user_id)
        .bind(code)
        .execute(pg)
        .await?;
    Ok(())
}

pub async fn exists(pg: &PgPool, user_id: i64) -> sqlx::Result<bool> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT user_id FROM users WHERE user_id = $1")
        .bind(user_id)
        .fetch_optional(pg)
        .await?;
    Ok(row.is_some())
}

pub async fn total(pg: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pg)
        .await
}

pub async fn has_linked_session(pg: &PgPool, user_id: i64) -> sqlx::Result<bool> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT user_id FROM account_sessions WHERE kind = 'linked' AND owner_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pg)
    .await?;
    Ok(row.is_some())
}

/// What `/start` did to the account, so the caller can notify and reward.
pub struct Registered {
    pub is_new: bool,
    pub lang: String,
    pub inviter: Option<Inviter>,
}

pub struct Inviter {
    pub id: i64,
    pub reward: i64,
    pub bonus: i64,
}

/// New users are created (with the referral reward when `inviter_id` is a real user);
/// existing users get their name refreshed and are marked active.
pub async fn register(
    pg: &PgPool,
    locales: &Locales,
    profile: &Profile<'_>,
    telegram_lang: Option<&str>,
    inviter_id: Option<i64>,
) -> sqlx::Result<Registered> {
    let mut detected = telegram_lang
        .map(str::to_string)
        .unwrap_or_else(|| crate::i18n::DEFAULT_LANGUAGE.into());
    if !locales.has(&detected) {
        detected = crate::i18n::DEFAULT_LANGUAGE.into();
    }
    let mut tx = pg.begin().await?;
    let existing: Option<(Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT language_code, referred_by FROM users WHERE user_id = $1 FOR UPDATE",
    )
    .bind(profile.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((stored_lang, _)) = existing {
        let lang = match stored_lang {
            Some(c) if locales.has(&c) => c,
            _ => detected,
        };
        sqlx::query(
            "UPDATE users SET first_name = $2, last_name = $3, username = $4, last_activity_date = NOW(), is_active = TRUE \
             WHERE user_id = $1",
        )
        .bind(profile.user_id)
        .bind(profile.first_name)
        .bind(profile.last_name)
        .bind(profile.stored_username())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(Registered {
            is_new: false,
            lang,
            inviter: None,
        });
    }

    let mut valid_inviter = None;
    if let Some(id) = inviter_id.filter(|id| *id != profile.user_id) {
        let row: Option<(bool,)> =
            sqlx::query_as("SELECT COALESCE(is_premium, FALSE) FROM users WHERE user_id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        valid_inviter = row.map(|(premium,)| (id, premium));
    }
    sqlx::query(
        "INSERT INTO users (user_id, first_name, last_name, username, language_code, join_date, last_activity_date, \
         referred_by, daily_downloads_reset_at) \
         VALUES ($1, $2, $3, $4, $5, NOW(), NOW(), $6, NOW() + INTERVAL '24 hours')",
    )
    .bind(profile.user_id)
    .bind(profile.first_name)
    .bind(profile.last_name)
    .bind(profile.stored_username())
    .bind(&detected)
    .bind(valid_inviter.map(|(id, _)| id))
    .execute(&mut *tx)
    .await?;
    let inviter = if let Some((id, premium)) = valid_inviter {
        let (reward, bonus) = crate::plans::referral_amounts(premium);
        sqlx::query("UPDATE users SET referral_points = referral_points + $2, referral_count = referral_count + 1 WHERE user_id = $1")
            .bind(id)
            .bind(reward as i32)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE users SET referral_points = referral_points + $2 WHERE user_id = $1")
            .bind(profile.user_id)
            .bind(bonus as i32)
            .execute(&mut *tx)
            .await?;
        Some(Inviter { id, reward, bonus })
    } else {
        None
    };
    tx.commit().await?;
    Ok(Registered {
        is_new: true,
        lang: detected,
        inviter,
    })
}
