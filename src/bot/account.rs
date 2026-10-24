//! `/profile`, `/premium` and `/language`.

use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use grammers_client::{message::ReplyMarkup, update::Message};

use super::{Bot, Who};
use crate::{
    plans::DAILY_DOWNLOAD_LIMIT,
    store::account::{self, UserData},
    telegram::keyboards::{Button, Color, keyboard},
};

fn date(value: Option<chrono::DateTime<Utc>>) -> Option<String> {
    value.map(|d| d.format("%Y-%m-%d").to_string())
}

pub async fn profile_text(bot: &Bot, lang: &str, user_id: i64) -> Result<String> {
    let na = bot.t(lang, "common.not_available", &[]);
    let Some(user) = account::load(&bot.pg, user_id).await? else {
        return Ok(bot.t(lang, "profile.unavailable", &[]));
    };
    let (plan, expiry) = if user.is_premium {
        (
            bot.t(lang, "plans.premium", &[]),
            date(user.premium_expiry_date).unwrap_or_else(|| na.clone()),
        )
    } else {
        (bot.t(lang, "plans.free", &[]), na.clone())
    };
    let reset = match user.daily_downloads_reset_at {
        Some(at) => {
            let remaining = (at - Utc::now()).num_seconds();
            if remaining > 0 {
                bot.duration(lang, remaining)
            } else {
                bot.t(lang, "limits.reset_now", &[])
            }
        }
        None => na,
    };
    let force = bot.t(
        lang,
        if user.force_download_enabled {
            "forcedownload.status.enabled"
        } else {
            "forcedownload.status.disabled"
        },
        &[],
    );
    Ok(bot.t(
        lang,
        "profile.details",
        &[
            ("user_id", &user_id.to_string()),
            ("plan_status", &plan),
            ("expiry_date", &expiry),
            ("daily_points", &user.daily_downloads_left.to_string()),
            ("daily_limit", &DAILY_DOWNLOAD_LIMIT.to_string()),
            ("reset_time", &reset),
            ("gift_points", &user.gift_points.to_string()),
            ("referral_points", &user.referral_points.to_string()),
            ("referral_count", &user.referral_count.to_string()),
            ("referral_link", &bot.referral_link(user_id)),
            ("force_download_status", &force),
        ],
    ))
}

pub async fn profile(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let text = profile_text(bot, &lang, who.id).await?;
    bot.reply(message, &text, None).await?;
    Ok(())
}

pub enum Premium {
    Already { popup: String, text: String },
    Menu { text: String, markup: ReplyMarkup },
}

pub async fn premium_view(bot: &Bot, lang: &str, user_id: i64) -> Result<Premium> {
    let user: Option<UserData> = account::load(&bot.pg, user_id).await?;
    if let Some(user) = user.filter(|u| u.is_premium) {
        let expiry = date(user.premium_expiry_date)
            .unwrap_or_else(|| bot.t(lang, "common.not_available", &[]));
        return Ok(Premium::Already {
            popup: bot.t(
                lang,
                "premium.already_active_popup",
                &[("expiry_date", &expiry)],
            ),
            text: bot.t(lang, "premium.already_active", &[("expiry_date", &expiry)]),
        });
    }
    let markup = keyboard(&[
        vec![Button::callback(
            &bot.t(lang, "premium.buttons.monthly", &[]),
            "buy_premium_monthly",
            Color::Green,
        )],
        vec![Button::callback(
            &bot.t(lang, "premium.buttons.yearly", &[]),
            "buy_premium_yearly",
            Color::Green,
        )],
        vec![Button::url(
            &bot.t(lang, "buttons.contact_admin", &[]),
            &format!("https://t.me/{}", bot.cfg.admin_username),
            Color::Blue,
        )],
        vec![bot.button(lang, "❌", "cancel_download", Color::Red)],
    ]);
    Ok(Premium::Menu {
        text: bot.t(lang, "premium.details", &[]),
        markup,
    })
}

pub async fn premium(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<()> {
    let lang = bot.lang(who.id).await;
    match premium_view(bot, &lang, who.id).await? {
        Premium::Already { text, .. } => bot.reply(message, &text, None).await?,
        Premium::Menu { text, markup } => bot.reply(message, &text, Some(markup)).await?,
    };
    Ok(())
}

pub async fn language(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<()> {
    // New language files are picked up without a restart.
    if let Err(error) = bot.locales.reload(&bot.cfg.locales_dir) {
        tracing::error!(user = who.id, %error, "could not reload the language files");
    }
    let lang = bot.lang(who.id).await;
    let languages = bot.locales.available();
    let mut rows: Vec<Vec<Button>> = languages
        .chunks(2)
        .map(|pair| {
            pair.iter()
                .map(|(code, name)| {
                    let color = if *code == lang {
                        Color::Green
                    } else {
                        Color::Blue
                    };
                    Button::callback(name, &format!("set_lang_{code}"), color)
                })
                .collect()
        })
        .collect();
    rows.push(vec![Button::callback(
        &bot.t(&lang, "buttons.cancel", &[]),
        "cancel_download",
        Color::Red,
    )]);
    bot.reply(
        message,
        &bot.t(&lang, "language.select", &[]),
        Some(keyboard(&rows)),
    )
    .await?;
    Ok(())
}
