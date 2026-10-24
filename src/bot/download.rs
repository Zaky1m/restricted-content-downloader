//! `/dl`, a pasted post link, and the checks that come before any download.

use std::sync::Arc;

use anyhow::Result;
use grammers_client::{message::ReplyMarkup, update::Message};
use serde_json::json;

use super::{Bot, Who};
use crate::{
    format::quote_plus,
    links::{is_supported_link, parse_message_link},
    store::{account, states},
    tasks::{self, Outcome, single},
    telegram::keyboards::{Button, Color, keyboard},
};

pub const GUIDE: &str = "guide";
/// Id of the share-a-channel button (kept from the Python bot).
pub const PICK_CHAT_BUTTON: i32 = 7821;
pub const COOLDOWN_SECONDS: i64 = 10;
const COOLDOWN_KIND: &str = "download_cooldown";

pub fn flow_cancel(bot: &Bot, lang: &str) -> ReplyMarkup {
    keyboard(&[vec![Button::callback(
        &bot.t(lang, "buttons.cancel", &[]),
        "flow_cancel",
        Color::Red,
    )]])
}

/// Free users wait ten seconds between downloads; premium users and staff do not.
async fn cooldown_ok(bot: &Bot, message: &Message, who: &Who, lang: &str) -> Result<bool> {
    if bot.is_staff(who.id).await {
        return Ok(true);
    }
    if account::load(&bot.pg, who.id)
        .await?
        .is_some_and(|u| u.is_premium)
    {
        return Ok(true);
    }
    let last: Option<f64> = states::get(&bot.pg, who.id, COOLDOWN_KIND).await?;
    let Some(last) = last else { return Ok(true) };
    let now = chrono::Utc::now().timestamp() as f64;
    let elapsed = now - last;
    if elapsed >= COOLDOWN_SECONDS as f64 {
        return Ok(true);
    }
    let remaining = (COOLDOWN_SECONDS as f64 - elapsed).round() as i64;
    bot.reply(
        message,
        &bot.t(
            lang,
            "limits.cooldown",
            &[("remaining_time", &bot.duration(lang, remaining))],
        ),
        None,
    )
    .await?;
    Ok(false)
}

pub async fn start_cooldown(bot: &Bot, user_id: i64) {
    let now = chrono::Utc::now().timestamp() as f64;
    let _ = states::set(&bot.pg, user_id, COOLDOWN_KIND, &now, 60 * 60).await;
}

/// Tells the user why they cannot download yet. True when they have the points.
pub async fn permission_ok(bot: &Bot, who: &Who, lang: &str, cost: i64) -> Result<bool> {
    let Some(user) = account::load(&bot.pg, who.id).await? else {
        return Ok(false);
    };
    if user.is_premium {
        return Ok(true);
    }
    let total = user.daily_downloads_left + user.gift_points + user.referral_points;
    if total >= cost {
        return Ok(true);
    }
    let link = bot.referral_link(who.id);
    let text = bot.t(lang, "referrals.share_text", &[]);
    let share_url = format!(
        "https://t.me/share/url?url={}&text={}",
        quote_plus(&link),
        quote_plus(&text)
    );
    let markup = keyboard(&[
        vec![Button::callback(
            &bot.t(lang, "buttons.subscribe", &[]),
            "show_premium",
            Color::Green,
        )],
        vec![Button::url(
            &bot.t(lang, "buttons.earn_points", &[]),
            &share_url,
            Color::Blue,
        )],
    ]);
    let body = if cost > 1 {
        bot.t(
            lang,
            "limits.insufficient_points_batch",
            &[
                ("required_points", &cost.to_string()),
                ("available_points", &total.to_string()),
            ],
        )
    } else {
        let left = match user.daily_downloads_reset_at {
            Some(at) if (at - chrono::Utc::now()).num_seconds() > 0 => {
                bot.duration(lang, (at - chrono::Utc::now()).num_seconds())
            }
            Some(_) => bot.t(lang, "limits.reset_now", &[]),
            None => bot.t(lang, "common.not_available", &[]),
        };
        bot.t(lang, "limits.insufficient_points", &[("time_left", &left)])
    };
    bot.send_to(who.id, &body, Some(markup)).await?;
    Ok(false)
}

pub async fn begin_guide(bot: &Bot, message: &Message, who: &Who, kind: &str) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let step = if kind == "bdl" { "first" } else { "channel" };
    states::set(
        &bot.pg,
        who.id,
        GUIDE,
        &json!({"kind": kind, "step": step}),
        24 * 60 * 60,
    )
    .await?;
    let key = match kind {
        "dl" | "bdl" => "flow.first_link",
        "uploadch" => "flow.upload_channel",
        _ => "flow.channel",
    };
    bot.reply(
        message,
        &bot.t(&lang, key, &[]),
        Some(flow_cancel(bot, &lang)),
    )
    .await?;
    Ok(())
}

/// `/dl <link>` or a link pasted on its own.
pub async fn dl(bot: &Arc<Bot>, message: &Message, who: &Who, args: &[&str]) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let Some(link) = args.first() else {
        return begin_guide(bot, message, who, "dl").await;
    };
    run_link(bot, message, who, &lang, link, args).await
}

pub async fn pasted_link(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<bool> {
    let words: Vec<&str> = message.text().split_whitespace().collect();
    let Some(first) = words.first() else {
        return Ok(false);
    };
    if !is_supported_link(first) {
        return Ok(false);
    }
    let lang = bot.lang(who.id).await;
    run_link(bot, message, who, &lang, first, &words).await?;
    Ok(true)
}

async fn run_link(
    bot: &Arc<Bot>,
    message: &Message,
    who: &Who,
    lang: &str,
    link: &str,
    words: &[&str],
) -> Result<()> {
    if words.contains(&"--fdl") {
        bot.reply(
            message,
            &bot.t(lang, "forcedownload.flag_removed", &[]),
            None,
        )
        .await?;
        return Ok(());
    }
    if !cooldown_ok(bot, message, who, lang).await? || !permission_ok(bot, who, lang, 1).await? {
        return Ok(());
    }
    let link = link.to_string();
    let user_id = who.id;
    let force = account::load(&bot.pg, user_id)
        .await?
        .is_some_and(|u| u.force_download_enabled && u.is_premium);
    let spec = json!({"url": link, "message_id": message.id()});
    let job: tasks::Job = {
        let link = link.clone();
        Box::new(move |ctx| {
            Box::pin(async move {
                let outcome = single::download_link(&ctx, &link, force).await;
                if outcome == Outcome::Downloaded {
                    start_cooldown(&ctx.bot, ctx.user_id).await;
                    let _ = account::spend_points(&ctx.bot.pg, ctx.user_id, 1).await;
                }
                outcome
            })
        })
    };
    tasks::track(bot, user_id, "dl", spec, job, None).await?;
    Ok(())
}

/// The reply to a guide prompt (`/dl` then the link). True when consumed.
pub async fn guide_answer(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<bool> {
    let Some(state) = states::get::<serde_json::Value>(&bot.pg, who.id, GUIDE).await? else {
        return Ok(false);
    };
    let lang = bot.lang(who.id).await;
    let value = message.text().trim().to_string();
    let kind = state["kind"].as_str().unwrap_or("");
    if kind != "dl" {
        // The other guides arrive with their commands.
        return Ok(false);
    }
    if !is_supported_link(&value) || parse_message_link(&value).is_err() {
        // One link is asked for here; the batch wording ("both values") belongs to /bdl.
        bot.reply(
            message,
            &bot.t(&lang, "downloads.invalid_link", &[]),
            Some(flow_cancel(bot, &lang)),
        )
        .await?;
        return Ok(true);
    }
    states::delete(&bot.pg, who.id, GUIDE).await?;
    run_link(bot, message, who, &lang, &value, &[&value]).await?;
    Ok(true)
}
