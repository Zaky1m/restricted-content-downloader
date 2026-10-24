//! `/start`, the main menu and the referral check a new invited user solves first.

use std::sync::Arc;

use anyhow::Result;
use grammers_client::{
    message::{InputMessage, ReplyMarkup},
    update::{CallbackQuery, Message},
};

use super::{
    Bot, Who,
    send::{escape, html},
};
use crate::{
    referral_check::{self, Check, Verdict},
    store::{settings, states, users},
    telegram::keyboards::{Button, Color, keyboard},
};

/// The inviter in `/start <id>`, if the argument is a number other than the user's own id.
fn parse_inviter(arg: Option<&&str>, user_id: i64) -> Option<i64> {
    let arg = arg.filter(|a| !a.is_empty() && a.chars().all(|c| c.is_ascii_digit()))?;
    arg.parse::<i64>().ok().filter(|id| *id != user_id)
}

pub async fn main_menu(bot: &Bot, user_id: i64, lang: &str) -> (String, ReplyMarkup) {
    let connected = users::has_linked_session(&bot.pg, user_id)
        .await
        .unwrap_or(false);
    let status = bot.t(
        lang,
        if connected {
            "accounts.status.connected"
        } else {
            "accounts.status.not_connected"
        },
        &[],
    );
    let text = bot.t(lang, "start.welcome", &[("account_status", &status)]);
    let mut rows = Vec::new();
    if let Some(link) = &bot.cfg.channel_link {
        rows.push(vec![Button::url(
            &bot.t(lang, "buttons.channel", &[]),
            link,
            Color::Blue,
        )]);
    }
    rows.push(vec![
        Button::callback(&bot.t(lang, "buttons.help", &[]), "help_menu", Color::Blue),
        Button::callback(
            &bot.t(lang, "buttons.profile", &[]),
            "my_profile",
            Color::Green,
        ),
    ]);
    let markup = keyboard(&rows);
    (text, markup)
}

/// The welcome text with the menu under it, on the start photo when one is set up.
pub async fn send_welcome(bot: &Bot, user_id: i64, text: &str, markup: ReplyMarkup) -> Result<()> {
    let peer = bot
        .user_ref(user_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("user {user_id} is not known to the bot"))?;
    if let Some(photo) = super::start_image::media(bot).await {
        let sent = bot
            .client
            .send_message(
                peer,
                InputMessage::new()
                    .html(text)
                    .copy_media(&photo)
                    .reply_markup(ReplyMarkup {
                        raw: markup.raw.clone(),
                    }),
            )
            .await;
        match sent {
            Ok(_) => return Ok(()),
            Err(error) => {
                tracing::warn!(%error, "start image could not be sent, sending text");
                super::start_image::forget(bot).await;
            }
        }
    }
    bot.client
        .send_message(peer, html(text, Some(markup)))
        .await?;
    Ok(())
}

fn detect_lang(bot: &Bot, who: &Who) -> String {
    match &who.lang_code {
        Some(code) if bot.locales.has(code) => code.clone(),
        _ => crate::i18n::DEFAULT_LANGUAGE.into(),
    }
}

pub async fn start(bot: &Arc<Bot>, message: &Message, who: &Who, args: &[&str]) -> Result<()> {
    let gift = args
        .first()
        .filter(|a| a.starts_with("gift_"))
        .map(|a| a.to_string());
    let chat_link = args
        .first()
        .filter(|a| a.starts_with("ul_"))
        .map(|a| a.to_string());
    let inviter = parse_inviter(args.first(), who.id);
    if let Some(inviter) = inviter {
        let is_new = !users::exists(&bot.pg, who.id).await?;
        if is_new && users::exists(&bot.pg, inviter).await? {
            return ask_check(bot, message, who, inviter).await;
        }
    }
    finish_start(bot, who, inviter, gift).await?;
    if let Some(payload) = chat_link {
        super::uploadch::start_payload(bot, who.id, &payload).await;
    }
    Ok(())
}

fn check_text(bot: &Bot, lang: &str, check: &Check, wrong_notice: bool) -> String {
    let mut text = bot.t(
        lang,
        "referral_check.question",
        &[("a", &check.a.to_string()), ("b", &check.b.to_string())],
    );
    if wrong_notice {
        let left = referral_check::MAX_WRONG - check.wrong;
        text.push_str("\n\n");
        text.push_str(&bot.t(lang, "referral_check.wrong", &[("left", &left.to_string())]));
    }
    text
}

fn check_keyboard(check: &Check) -> ReplyMarkup {
    let row: Vec<Button> = check
        .visible_options()
        .iter()
        .map(|o| Button::callback(&o.to_string(), &format!("rc_{o}"), Color::Blue))
        .collect();
    keyboard(&[row])
}

async fn ask_check(bot: &Arc<Bot>, message: &Message, who: &Who, inviter: i64) -> Result<()> {
    let lang = detect_lang(bot, who);
    let mut check = Check::new(inviter);
    let text = check_text(bot, &lang, &check, false);
    let sent = bot
        .reply(message, &text, Some(check_keyboard(&check)))
        .await?;
    check.messages = vec![sent.id()];
    states::set(
        &bot.pg,
        who.id,
        referral_check::STATE_KIND,
        &check,
        referral_check::TTL_SECONDS,
    )
    .await?;
    Ok(())
}

/// A typed reply while a check is open. True when the message belonged to the check.
pub async fn handle_check_answer(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<bool> {
    let Some(check) = states::get::<Check>(&bot.pg, who.id, referral_check::STATE_KIND).await?
    else {
        return Ok(false);
    };
    if message.text().starts_with('/') {
        return Ok(false);
    }
    // The typed answer disappears at once; only the question message stays.
    message.delete().await.ok();
    resolve(bot, who, check, message.text()).await?;
    Ok(true)
}

/// A press on one of the number buttons.
pub async fn check_button(
    bot: &Arc<Bot>,
    query: &CallbackQuery,
    who: &Who,
    value: &str,
) -> Result<()> {
    query.answer().send().await.ok();
    let Some(check) = states::get::<Check>(&bot.pg, who.id, referral_check::STATE_KIND).await?
    else {
        let lang = detect_lang(bot, who);
        if let Ok(message) = query.load_message().await {
            message.delete().await.ok();
        }
        bot.send_to(who.id, &bot.t(&lang, "common.state_expired", &[]), None)
            .await
            .ok();
        return Ok(());
    };
    resolve(bot, who, check, value).await
}

async fn resolve(bot: &Arc<Bot>, who: &Who, mut check: Check, answer: &str) -> Result<()> {
    let lang = detect_lang(bot, who);
    let question_id = check.messages.first().copied();
    match check.judge(answer) {
        Verdict::Correct => {
            states::delete(&bot.pg, who.id, referral_check::STATE_KIND).await?;
            delete_question(bot, who.id, question_id).await;
            finish_start(bot, who, Some(check.inviter), None).await?;
        }
        Verdict::Wrong { .. } => {
            states::set(
                &bot.pg,
                who.id,
                referral_check::STATE_KIND,
                &check,
                referral_check::TTL_SECONDS,
            )
            .await?;
            if let (Some(id), Some(peer)) = (question_id, bot.user_ref(who.id).await) {
                let text = check_text(bot, &lang, &check, true);
                bot.client
                    .edit_message(peer, id, html(&text, Some(check_keyboard(&check))))
                    .await
                    .ok();
            }
        }
        Verdict::Failed => {
            states::delete(&bot.pg, who.id, referral_check::STATE_KIND).await?;
            delete_question(bot, who.id, question_id).await;
            bot.send_to(who.id, &bot.t(&lang, "referral_check.failed", &[]), None)
                .await?;
            finish_start(bot, who, None, None).await?;
        }
    }
    Ok(())
}

async fn delete_question(bot: &Bot, user_id: i64, id: Option<i32>) {
    let (Some(id), Some(peer)) = (id, bot.user_ref(user_id).await) else {
        return;
    };
    if let Err(error) = bot.client.delete_messages(peer, &[id]).await {
        tracing::debug!(%error, "could not delete the check question");
    }
}

/// Registers or refreshes the user, tells whoever should know, and shows the welcome menu.
async fn finish_start(
    bot: &Arc<Bot>,
    who: &Who,
    inviter: Option<i64>,
    gift: Option<String>,
) -> Result<()> {
    let profile = users::Profile {
        user_id: who.id,
        first_name: &who.first_name,
        last_name: who.last_name.as_deref(),
        username: who.username.as_deref(),
    };
    let registered = users::register(
        &bot.pg,
        &bot.locales,
        &profile,
        who.lang_code.as_deref(),
        inviter,
    )
    .await?;
    let lang = registered.lang.clone();
    super::commands::set_for_user(bot, who.id, &lang).await;

    if registered.is_new {
        announce_new_user(bot, who).await;
    }
    if let Some(inviter) = &registered.inviter {
        let inviter_lang = bot.lang(inviter.id).await;
        let text = bot.t(
            &inviter_lang,
            "referrals.new_referral",
            &[
                ("user_name", &who.mention()),
                ("user_id", &who.id.to_string()),
                ("reward", &inviter.reward.to_string()),
            ],
        );
        bot.send_to(inviter.id, &text, None).await.ok();
    }

    let gift_result = match gift {
        Some(pid) => Some(apply_gift(bot, who.id, &lang, &pid).await),
        None => None,
    };
    let (mut text, markup) = main_menu(bot, who.id, &lang).await;
    if let Some(inviter) = registered.inviter.as_ref().filter(|i| i.bonus > 0) {
        text.push_str("\n\n");
        text.push_str(&bot.t(
            &lang,
            "referrals.welcome_bonus",
            &[("bonus", &inviter.bonus.to_string())],
        ));
    }
    // The gift's result rides on the welcome, like the referral bonus, instead of a message of its own.
    if let Some(result) = gift_result {
        text.push_str("\n\n");
        text.push_str(&result);
    }
    send_welcome(bot, who.id, &text, markup).await
}

async fn announce_new_user(bot: &Bot, who: &Who) {
    if let Some(code) = &who.lang_code
        && !bot.locales.has(code)
        && settings::claim(
            &bot.pg,
            &format!("reported_language_{code}"),
            &who.id.to_string(),
        )
        .await
        .unwrap_or(false)
    {
        bot.notify_owners("admin.users.new_language", |_| {
            vec![
                ("first_name".into(), escape(&who.first_name)),
                ("user_id".into(), who.id.to_string()),
                ("language_code".into(), code.clone()),
            ]
        })
        .await;
    }
    let total = users::total(&bot.pg).await.unwrap_or(0);
    bot.notify_owners("admin.users.new_user", |lang| {
        let na = bot.t(lang, "common.not_available", &[]);
        vec![
            ("first_name".into(), escape(&who.first_name)),
            (
                "last_name".into(),
                who.last_name
                    .as_deref()
                    .map(escape)
                    .unwrap_or_else(|| na.clone()),
            ),
            ("user_id".into(), who.id.to_string()),
            (
                "username".into(),
                who.username.as_ref().map(|u| format!("@{u}")).unwrap_or(na),
            ),
            ("total_users".into(), total.to_string()),
        ]
    })
    .await;
}

/// Claims the gift and returns what to tell the user.
async fn apply_gift(bot: &Bot, user_id: i64, lang: &str, pid: &str) -> String {
    use crate::store::gifts::{Claim, claim};
    let result = claim(&bot.pg, user_id, pid).await;
    match result {
        Ok(outcome) => {
            let key = outcome.locale_key();
            match outcome {
                Claim::Granted {
                    value,
                    expires: Some(at),
                } => bot.t(
                    lang,
                    key,
                    &[
                        ("value", &value.to_string()),
                        ("expiry_date", &at.format("%Y-%m-%d %H:%M UTC").to_string()),
                    ],
                ),
                Claim::Granted {
                    value,
                    expires: None,
                } => bot.t(lang, key, &[("value", &value.to_string())]),
                _ => bot.t(lang, key, &[]),
            }
        }
        Err(error) => {
            tracing::error!(user_id, pid, %error, "could not apply a gift");
            bot.t(lang, "gifts.claim.failed", &[])
        }
    }
}
