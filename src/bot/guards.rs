//! What runs before any command: rate limits, forced subscription, waiting states.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use grammers_client::{
    session::types::PeerId,
    tl,
    update::{CallbackQuery, Message},
};

use super::{Bot, Who, callbacks, send::html, start};
use crate::{
    ratelimit::Kind,
    store::limits,
    telegram::keyboards::{Button, Color, keyboard},
};

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

const FSUB_CHECK_SECONDS: f64 = 24.0 * 60.0 * 60.0;

pub async fn on_message(bot: &Arc<Bot>, message: Message) -> Result<()> {
    if !matches!(
        message.peer_id().kind(),
        grammers_client::session::types::PeerKind::User
    ) {
        return Ok(());
    }
    let Some(who) = Who::from_peer(message.sender()) else {
        return Ok(());
    };
    // A finished payment is recorded before any limit or subscription check, so a paying user is never ignored.
    if let Some(tl::enums::MessageAction::PaymentSentMe(payment)) = message.action() {
        return super::billing::paid(bot, who.id, &who.mention(), payment).await;
    }
    let staff = bot.is_staff(who.id).await;

    if !staff {
        let (allowed, notice) = limits::check(&bot.pg, who.id, Kind::Message, now()).await;
        if !allowed {
            if let Some(notice) = notice {
                let lang = bot.lang(who.id).await;
                bot.reply(&message, &bot.t(&lang, notice.locale_key(), &[]), None)
                    .await
                    .ok();
            }
            return Ok(());
        }
        if !forced_subscription_ok(bot, &message, &who).await? {
            return Ok(());
        }
    }

    if let Some(tl::enums::MessageAction::RequestedPeerSentMe(shared)) = message.action()
        && shared.button_id == super::download::PICK_CHAT_BUTTON
        && let Some(id) = shared_peer_id(&shared.peers)
        && super::uploadch::guide_answer(bot, &message, &who, Some(id)).await?
    {
        return Ok(());
    }
    match message.media() {
        Some(grammers_client::media::Media::Contact(contact)) => {
            return super::login::on_contact(bot, &message, &who, &contact.raw).await;
        }
        Some(grammers_client::media::Media::Photo(_)) => {
            return super::settings::on_photo(bot, &message, &who).await;
        }
        _ => {}
    }
    if !message.text().starts_with('/') && super::admin::input(bot, &message, &who).await? {
        return Ok(());
    }
    if !message.text().starts_with('/') {
        if start::handle_check_answer(bot, &message, &who).await? {
            return Ok(());
        }
        if super::cancel::is_cancel_word(message.text()) {
            let words: Vec<&str> = message.text().split_whitespace().skip(1).collect();
            return super::cancel::command(bot, &message, &who, &words).await;
        }
        if super::login::on_text(bot, &message, &who).await? {
            return Ok(());
        }
        if super::uploadch::guide_answer(bot, &message, &who, None).await? {
            return Ok(());
        }
        if super::range::guide_answer(bot, &message, &who).await? {
            return Ok(());
        }
        if super::download::guide_answer(bot, &message, &who).await? {
            return Ok(());
        }
        if super::download::pasted_link(bot, &message, &who).await? {
            return Ok(());
        }
    }
    route_command(bot, &message, &who).await
}

async fn route_command(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<()> {
    let text = message.text();
    let Some(rest) = text.strip_prefix('/') else {
        return Ok(());
    };
    let mut parts = rest.split_whitespace();
    let Some(head) = parts.next() else {
        return Ok(());
    };
    let (name, target) = head
        .split_once('@')
        .map_or((head, None), |(n, t)| (n, Some(t)));
    if target.is_some_and(|t| !t.eq_ignore_ascii_case(&bot.cfg.bot_username)) {
        return Ok(());
    }
    let args: Vec<&str> = parts.collect();
    match name.to_ascii_lowercase().as_str() {
        "start" => start::start(bot, message, who, &args).await,
        "help" => super::help::help(bot, message, who).await,
        "profile" => super::account::profile(bot, message, who).await,
        "premium" => super::account::premium(bot, message, who).await,
        "language" => super::account::language(bot, message, who).await,
        "forcedownload" => super::settings::forcedownload(bot, message, who).await,
        "thumb" => super::settings::thumb(bot, message, who).await,
        "caption" => super::settings::caption(bot, message, who).await,
        "dl" => super::download::dl(bot, message, who, &args).await,
        "cancel" => super::cancel::command(bot, message, who, &args).await,
        "bdl" => super::range::bdl(bot, message, who, &args).await,
        "clone" => super::range::clone_channel(bot, message, who, &args).await,
        "uploadch" => super::uploadch::command(bot, message, who, &args).await,
        "rmuploadch" => super::uploadch::remove(bot, message, who).await,
        "monitor" => crate::monitor::command(bot, message, who, &args).await,
        "admin" => super::admin::panel_command(bot, message, who).await,
        "bc" => super::admin::bc(bot, message, who, &args).await,
        "bcjson" => super::admin::bcjson(bot, message, who).await,
        "stats" => super::stats::stats(bot, message, who).await,
        "login" => super::login::login(bot, Some(message), who).await,
        "logout" => super::login::logout(bot, message, who).await,
        _ => Ok(()),
    }
}

pub async fn on_callback(bot: &Arc<Bot>, query: CallbackQuery) -> Result<()> {
    let Some(who) = Who::from_peer(query.sender()) else {
        return Ok(());
    };
    if !bot.is_staff(who.id).await {
        let (allowed, notice) = limits::check(&bot.pg, who.id, Kind::Button, now()).await;
        if !allowed {
            let lang = bot.lang(who.id).await;
            match notice {
                Some(notice) => {
                    query
                        .answer()
                        .alert(bot.t(&lang, notice.locale_key(), &[]))
                        .send()
                        .await
                        .ok();
                }
                None => {
                    query.answer().send().await.ok();
                }
            }
            return Ok(());
        }
    }
    let routed = callbacks::route(bot, &query, &who);
    tokio::pin!(routed);
    tokio::select! {
        result = &mut routed => return result,
        // A button that is not answered keeps spinning on the user's side until Telegram gives up
        // (BOT_RESPONSE_TIMEOUT); handlers answer first, and this covers any slow path that does not.
        _ = tokio::time::sleep(std::time::Duration::from_secs(4)) => {
            query.answer().send().await.ok();
        }
    }
    routed.await
}

struct Missing {
    link: String,
}

async fn required_channels(bot: &Bot) -> sqlx::Result<Vec<(i64, String)>> {
    sqlx::query_as("SELECT channel_id, link FROM fsub_channels ORDER BY date_added")
        .fetch_all(&bot.pg)
        .await
}

/// `Some(true)` when the user is in the channel, `None` when Telegram could not tell.
async fn is_member(bot: &Bot, channel_id: i64, user_id: i64) -> Option<bool> {
    let channel = PeerId::from_bot_api_dialog_id(channel_id)?;
    let channel_ref = bot.peer_ref_for(channel).await?;
    let user = bot.user_ref(user_id).await?;
    let request = tl::functions::channels::GetParticipant {
        channel: (&channel_ref).into(),
        participant: (&user).into(),
    };
    match bot.client.invoke(&request).await {
        Ok(tl::enums::channels::ChannelParticipant::Participant(p)) => Some(!matches!(
            p.participant,
            tl::enums::ChannelParticipant::Left(_) | tl::enums::ChannelParticipant::Banned(_)
        )),
        Err(error) if error.is("USER_NOT_PARTICIPANT") => Some(false),
        Err(error) => {
            tracing::warn!(channel_id, user_id, %error, "forced subscription check failed");
            None
        }
    }
}

async fn missing_channels(bot: &Bot, user_id: i64) -> Result<Vec<Missing>> {
    let mut missing = Vec::new();
    for (channel_id, link) in required_channels(bot).await? {
        if is_member(bot, channel_id, user_id).await == Some(false) {
            missing.push(Missing { link });
        }
    }
    Ok(missing)
}

async fn recently_passed(bot: &Bot, user_id: i64) -> bool {
    let row: Result<Option<(i64,)>, _> = sqlx::query_as(
        "SELECT 1::bigint FROM fsub_checks WHERE user_id = $1 AND passed_at > NOW() - make_interval(secs => $2)",
    )
    .bind(user_id)
    .bind(FSUB_CHECK_SECONDS)
    .fetch_optional(&bot.pg)
    .await;
    matches!(row, Ok(Some(_)))
}

pub async fn mark_passed(bot: &Bot, user_id: i64) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO fsub_checks (user_id, passed_at) VALUES ($1, NOW()) \
         ON CONFLICT (user_id) DO UPDATE SET passed_at = NOW()",
    )
    .bind(user_id)
    .execute(&bot.pg)
    .await?;
    Ok(())
}

/// False when the user was asked to join channels first.
async fn forced_subscription_ok(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<bool> {
    if required_channels(bot).await?.is_empty() || recently_passed(bot, who.id).await {
        return Ok(true);
    }
    let missing = missing_channels(bot, who.id).await?;
    if missing.is_empty() {
        mark_passed(bot, who.id).await?;
        return Ok(true);
    }
    let lang = bot.lang(who.id).await;
    let links = missing
        .iter()
        .map(|m| format!("• {}", m.link))
        .collect::<Vec<_>>()
        .join("\n");
    let markup = keyboard(&[
        vec![Button::callback(
            &bot.t(&lang, "buttons.refresh_subscription", &[]),
            "fsub_refresh",
            Color::Green,
        )],
        vec![bot.button(&lang, "❌", "cancel_download", Color::Red)],
    ]);
    message
        .reply(html(
            &bot.t(&lang, "subscription.required", &[("channels", &links)]),
            Some(markup),
        ))
        .await?;
    Ok(false)
}

/// The refresh button of the subscription notice.
pub async fn refresh_subscription(bot: &Arc<Bot>, query: &CallbackQuery, who: &Who) -> Result<()> {
    let lang = bot.lang(who.id).await;
    if missing_channels(bot, who.id).await?.is_empty() {
        mark_passed(bot, who.id).await?;
        query.answer().send().await.ok();
        if let Ok(message) = query.load_message().await {
            message.delete().await.ok();
        }
        bot.send_to(who.id, &bot.t(&lang, "subscription.completed", &[]), None)
            .await
            .ok();
    } else {
        query
            .answer()
            .alert(bot.t(&lang, "subscription.still_missing_popup", &[]))
            .send()
            .await
            .ok();
    }
    Ok(())
}

/// Bot API id of the first chat a user picked with the share-chat button.
fn shared_peer_id(peers: &[tl::enums::RequestedPeer]) -> Option<i64> {
    match peers.first()? {
        tl::enums::RequestedPeer::Channel(c) => PeerId::channel(c.channel_id)?.bot_api_dialog_id(),
        tl::enums::RequestedPeer::Chat(c) => PeerId::chat(c.chat_id)?.bot_api_dialog_id(),
        tl::enums::RequestedPeer::User(_) => None,
    }
}
