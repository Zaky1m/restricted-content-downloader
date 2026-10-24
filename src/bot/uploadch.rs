//! The channel (or group) that receives a premium user's downloads: `/uploadch`, `/rmuploadch`,
//! and the confirm flow that starts when the bot is added to a chat.

use std::sync::Arc;

use anyhow::Result;
use grammers_client::{
    message::ReplyMarkup,
    session::types::{PeerAuth, PeerId, PeerKind, PeerRef},
    tl,
    update::{CallbackQuery, Message},
};
use serde_json::{Value, json};

use super::{Bot, Who, download::begin_guide};
use crate::{
    links::{Chat, parse_chat_reference},
    store::{account, pending},
    telegram::keyboards::{Button, Color, keyboard},
};

const KIND: &str = "chat_link";
/// An add that is not confirmed within a day makes the bot leave.
const TTL: i64 = 24 * 60 * 60;
const MAX_TITLE: usize = 200;

fn names(
    bot: &Bot,
    lang: &str,
    title: Option<&str>,
    username: Option<&str>,
    chat_id: i64,
) -> Vec<(&'static str, String)> {
    let parts: Vec<String> = [title.map(str::to_string), username.map(|u| format!("@{u}"))]
        .into_iter()
        .flatten()
        .collect();
    let shown = if parts.is_empty() {
        bot.t(lang, "common.not_available", &[])
    } else {
        parts.join(" ")
    };
    vec![
        (
            "channel_title",
            crate::bot::escape(&shown.chars().take(MAX_TITLE).collect::<String>()),
        ),
        ("channel_id", chat_id.to_string()),
    ]
}

fn as_args<'a>(owned: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
    owned.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

pub async fn is_premium_now(bot: &Bot, user_id: i64) -> Option<bool> {
    let row: Option<(Option<bool>,)> = sqlx::query_as(
        "SELECT is_premium AND (premium_expiry_date IS NULL OR premium_expiry_date > NOW()) FROM users WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(&bot.pg)
    .await
    .ok()?;
    row.map(|r| r.0.unwrap_or(false))
}

pub async fn leave_quietly(bot: &Bot, dialog_id: i64, reason: &str) {
    let Some(id) = PeerId::from_bot_api_dialog_id(dialog_id) else {
        return;
    };
    let result = match id.kind() {
        PeerKind::Channel => match bot.peer_ref_for(id).await {
            Some(peer) => bot
                .client
                .invoke(&tl::functions::channels::LeaveChannel {
                    channel: (&peer).into(),
                })
                .await
                .map(drop),
            None => return,
        },
        PeerKind::Chat => bot
            .client
            .invoke(&tl::functions::messages::DeleteChatUser {
                revoke_history: false,
                chat_id: id.bare_id_unchecked(),
                user_id: tl::enums::InputUser::UserSelf,
            })
            .await
            .map(drop),
        PeerKind::User => return,
    };
    match result {
        Ok(()) => tracing::info!(chat = dialog_id, reason, "left a chat"),
        Err(error) => tracing::info!(chat = dialog_id, reason, %error, "could not leave a chat"),
    }
}

/// Whether the bot may still post in a user's upload channel.
pub enum ChannelCheck {
    Ready,
    /// The bot was removed or lost its admin rights: the setting is dropped.
    Lost,
    /// Telegram could not answer now: this upload goes elsewhere, the setting stays.
    Unknown,
}

/// Checked before every upload to a channel, as the Python bot did.
pub async fn check_upload_channel(bot: &Bot, dialog_id: i64) -> ChannelCheck {
    const PERMANENT: [&str; 4] = [
        "CHAT_ADMIN_REQUIRED",
        "CHANNEL_PRIVATE",
        "CHANNEL_INVALID",
        "USER_NOT_PARTICIPANT",
    ];
    let Some(chat) = PeerId::from_bot_api_dialog_id(dialog_id) else {
        return ChannelCheck::Lost;
    };
    if chat.kind() == PeerKind::Channel {
        let Some(peer) = bot.peer_ref_for(chat).await else {
            return ChannelCheck::Unknown;
        };
        let found = bot
            .client
            .invoke(&tl::functions::channels::GetParticipant {
                channel: (&peer).into(),
                participant: tl::enums::InputPeer::PeerSelf,
            })
            .await;
        return match found {
            Ok(tl::enums::channels::ChannelParticipant::Participant(p)) => match p.participant {
                tl::enums::ChannelParticipant::Creator(_)
                | tl::enums::ChannelParticipant::Admin(_) => ChannelCheck::Ready,
                _ => ChannelCheck::Lost,
            },
            Err(error) if PERMANENT.iter().any(|name| error.is(name)) => ChannelCheck::Lost,
            Err(error) => {
                tracing::warn!(chat = dialog_id, %error, "upload channel could not be checked");
                ChannelCheck::Unknown
            }
        };
    }
    match role_in(bot, chat, tl::enums::InputPeer::PeerSelf).await {
        Some(Role::Owner | Role::Admin { .. }) => ChannelCheck::Ready,
        Some(_) => ChannelCheck::Lost,
        None => ChannelCheck::Unknown,
    }
}

enum Role {
    Owner,
    Admin { post: bool },
    Member,
    Gone,
}

async fn role_in(bot: &Bot, chat: PeerId, member: tl::enums::InputPeer) -> Option<Role> {
    match chat.kind() {
        PeerKind::Channel => {
            let peer = bot.peer_ref_for(chat).await?;
            let found = bot
                .client
                .invoke(&tl::functions::channels::GetParticipant {
                    channel: (&peer).into(),
                    participant: member,
                })
                .await;
            let tl::enums::channels::ChannelParticipant::Participant(p) = match found {
                Ok(p) => p,
                Err(error) if error.is("USER_NOT_PARTICIPANT") => return Some(Role::Gone),
                Err(_) => return None,
            };
            Some(match p.participant {
                tl::enums::ChannelParticipant::Creator(_) => Role::Owner,
                tl::enums::ChannelParticipant::Admin(a) => {
                    let tl::enums::ChatAdminRights::Rights(rights) = a.admin_rights;
                    Role::Admin {
                        post: rights.post_messages,
                    }
                }
                tl::enums::ChannelParticipant::Left(_)
                | tl::enums::ChannelParticipant::Banned(_) => Role::Gone,
                _ => Role::Member,
            })
        }
        PeerKind::Chat => {
            let full = bot
                .client
                .invoke(&tl::functions::messages::GetFullChat {
                    chat_id: chat.bare_id_unchecked(),
                })
                .await
                .ok()?;
            let tl::enums::messages::ChatFull::Full(full) = full;
            let tl::enums::ChatFull::Full(chat_full) = full.full_chat else {
                return None;
            };
            let tl::enums::ChatParticipants::Participants(list) = chat_full.participants else {
                return None;
            };
            let wanted = match member {
                tl::enums::InputPeer::PeerSelf => bot.client.get_me().await.ok()?.id().bare_id()?,
                tl::enums::InputPeer::User(u) => u.user_id,
                _ => return None,
            };
            for participant in list.participants {
                match participant {
                    tl::enums::ChatParticipant::Creator(c) if c.user_id == wanted => {
                        return Some(Role::Owner);
                    }
                    tl::enums::ChatParticipant::Admin(a) if a.user_id == wanted => {
                        return Some(Role::Admin { post: true });
                    }
                    tl::enums::ChatParticipant::Participant(p) if p.user_id == wanted => {
                        return Some(Role::Member);
                    }
                    _ => {}
                }
            }
            Some(Role::Gone)
        }
        PeerKind::User => None,
    }
}

async fn user_input(bot: &Bot, user_id: i64) -> Option<tl::enums::InputPeer> {
    bot.user_ref(user_id).await.map(|p| (&p).into())
}

/// The locale key explaining why a chat cannot be the destination, or `None` when it can.
async fn rights_problem(
    bot: &Bot,
    chat: PeerId,
    user_id: i64,
    is_channel: bool,
) -> Option<&'static str> {
    let bot_role = role_in(bot, chat, tl::enums::InputPeer::PeerSelf).await;
    let bot_ok = if is_channel {
        matches!(
            bot_role,
            Some(Role::Owner) | Some(Role::Admin { post: true })
        )
    } else {
        matches!(
            bot_role,
            Some(Role::Owner | Role::Admin { .. } | Role::Member)
        )
    };
    if !bot_ok {
        return Some("uploads.auto.no_rights");
    }
    let user = user_input(bot, user_id).await?;
    match role_in(bot, chat, user).await {
        Some(Role::Owner | Role::Admin { .. }) => None,
        _ => Some("uploads.auto.user_not_admin"),
    }
}

fn premium_markup(bot: &Bot, lang: &str) -> ReplyMarkup {
    keyboard(&[vec![Button::callback(
        &bot.t(lang, "buttons.subscribe", &[]),
        "show_premium",
        Color::Green,
    )]])
}

/// Second check and the confirm prompt, for a user who already started the bot.
pub async fn continue_link(bot: &Arc<Bot>, user_id: i64, info: &Value) {
    let lang = bot.lang(user_id).await;
    let Some(chat_id) = info["chat_id"].as_i64() else {
        return;
    };
    let Some(chat) = PeerId::from_bot_api_dialog_id(chat_id) else {
        return;
    };
    let owned = names(
        bot,
        &lang,
        info["title"].as_str(),
        info["username"].as_str(),
        chat_id,
    );
    let args = as_args(&owned);
    if !is_premium_now(bot, user_id).await.unwrap_or(false) {
        bot.send_to(
            user_id,
            &bot.t(&lang, "uploads.auto.premium_only", &args),
            Some(premium_markup(bot, &lang)),
        )
        .await
        .ok();
        leave_quietly(bot, chat_id, &format!("user {user_id} is not premium")).await;
        return;
    }
    if let Some(problem) = rights_problem(
        bot,
        chat,
        user_id,
        info["is_channel"].as_bool().unwrap_or(false),
    )
    .await
    {
        bot.send_to(user_id, &bot.t(&lang, problem, &args), None)
            .await
            .ok();
        leave_quietly(
            bot,
            chat_id,
            &format!("cannot upload there for user {user_id}"),
        )
        .await;
        return;
    }
    let mut data = info.clone();
    data["step"] = json!("confirm");
    let Ok(action_id) = pending::create(&bot.pg, user_id, KIND, &data, TTL).await else {
        return;
    };
    let markup = keyboard(&[vec![
        Button::callback(
            &bot.t(&lang, "buttons.confirm", &[]),
            &format!("ul_y_{action_id}"),
            Color::Green,
        ),
        Button::callback(
            &bot.t(&lang, "buttons.reject", &[]),
            &format!("ul_n_{action_id}"),
            Color::Red,
        ),
    ]]);
    match bot
        .send_to(
            user_id,
            &bot.t(&lang, "uploads.auto.confirm", &args),
            Some(markup),
        )
        .await
    {
        Ok(prompt) => pending::merge(&bot.pg, action_id, &json!({"message_id": prompt.id()})).await,
        Err(error) => {
            pending::delete(&bot.pg, action_id).await;
            tracing::info!(user_id, chat_id, %error, "could not ask the user to confirm");
            leave_quietly(bot, chat_id, &format!("user {user_id} cannot be reached")).await;
        }
    }
}

/// The bot's own membership changed in a channel or group.
pub async fn bot_membership(
    bot: &Arc<Bot>,
    chat: PeerId,
    actor_id: i64,
    was_out: bool,
    now_in: bool,
) {
    if !now_in || !was_out {
        return;
    }
    let Some(dialog_id) = chat.bot_api_dialog_id() else {
        return;
    };
    if dialog_id == bot.cfg.log_channel_id {
        return;
    }
    let forced: Option<(i64,)> =
        sqlx::query_as("SELECT channel_id FROM fsub_channels WHERE channel_id = $1")
            .bind(dialog_id)
            .fetch_optional(&bot.pg)
            .await
            .ok()
            .flatten();
    if forced.is_some() || bot.is_staff(actor_id).await {
        return;
    }
    // Title and username come from the cached peer, if the session has it.
    let (title, username, is_channel) = match bot
        .client
        .resolve_peer(PeerRef {
            id: chat,
            auth: bot
                .peer_ref_for(chat)
                .await
                .map(|p| p.auth)
                .unwrap_or(PeerAuth::default()),
        })
        .await
    {
        Ok(peer) => (
            peer.name().map(str::to_string),
            peer.username().map(str::to_string),
            matches!(peer, grammers_client::peer::Peer::Channel(_)),
        ),
        Err(_) => (None, None, chat.kind() == PeerKind::Channel),
    };
    let info = json!({"chat_id": dialog_id, "title": title, "username": username, "is_channel": is_channel});
    if is_premium_now(bot, actor_id).await.is_some() {
        continue_link(bot, actor_id, &info).await;
        return;
    }
    // A bot can only write to people who started it: a delivered message lets a Start press finish the link.
    let lang = match bot.user_ref(actor_id).await {
        Some(_) => bot.lang(actor_id).await,
        None => crate::i18n::DEFAULT_LANGUAGE.to_string(),
    };
    let mut data = info.clone();
    data["step"] = json!("start");
    let Ok(action_id) = pending::create(&bot.pg, actor_id, KIND, &data, TTL).await else {
        return;
    };
    let owned = names(bot, &lang, title.as_deref(), username.as_deref(), dialog_id);
    let markup = keyboard(&[vec![Button::url(
        &bot.t(&lang, "buttons.start", &[]),
        &format!("https://t.me/{}?start=ul_{action_id}", bot.cfg.bot_username),
        Color::Green,
    )]]);
    match bot
        .send_to(
            actor_id,
            &bot.t(&lang, "uploads.auto.start_first", &as_args(&owned)),
            Some(markup),
        )
        .await
    {
        Ok(prompt) => pending::merge(&bot.pg, action_id, &json!({"message_id": prompt.id()})).await,
        Err(error) => {
            pending::delete(&bot.pg, action_id).await;
            leave_quietly(
                bot,
                dialog_id,
                &format!("adder {actor_id} never started the bot ({error})"),
            )
            .await;
        }
    }
}

/// `/start ul_<id>` after the adder pressed Start.
pub async fn start_payload(bot: &Arc<Bot>, user_id: i64, payload: &str) {
    let Some(action_id) = payload
        .strip_prefix("ul_")
        .and_then(|d| d.parse::<i64>().ok())
    else {
        return;
    };
    let Ok(Some((kind, info))) = pending::take(&bot.pg, action_id, user_id).await else {
        return;
    };
    if kind != KIND || info["step"] != "start" {
        return;
    }
    if let (Some(message_id), Some(peer)) =
        (info["message_id"].as_i64(), bot.user_ref(user_id).await)
    {
        bot.client
            .delete_messages(peer, &[message_id as i32])
            .await
            .ok();
    }
    continue_link(bot, user_id, &info).await;
}

pub async fn callback(bot: &Arc<Bot>, query: &CallbackQuery, who: &Who, data: &str) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let mut parts = data.splitn(3, '_');
    let (_, choice, raw_id) = (
        parts.next(),
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
    );
    let taken = match raw_id.parse::<i64>() {
        Ok(id) => pending::take(&bot.pg, id, who.id).await?,
        Err(_) => None,
    };
    let Some((kind, info)) = taken.filter(|(k, i)| k == KIND && i["step"] == "confirm") else {
        query
            .answer()
            .alert(bot.t(&lang, "uploads.auto.expired", &[]))
            .send()
            .await
            .ok();
        if let Ok(m) = query.load_message().await {
            m.delete().await.ok();
        }
        return Ok(());
    };
    let _ = kind;
    let Some(chat_id) = info["chat_id"].as_i64() else {
        return Ok(());
    };
    if choice == "n" {
        query.answer().send().await.ok();
        if let Ok(m) = query.load_message().await {
            m.delete().await.ok();
        }
        leave_quietly(bot, chat_id, &format!("user {} rejected it", who.id)).await;
        return Ok(());
    }
    let owned = names(
        bot,
        &lang,
        info["title"].as_str(),
        info["username"].as_str(),
        chat_id,
    );
    let args = as_args(&owned);
    let problem = if is_premium_now(bot, who.id).await.unwrap_or(false) {
        match PeerId::from_bot_api_dialog_id(chat_id) {
            Some(chat) => {
                rights_problem(
                    bot,
                    chat,
                    who.id,
                    info["is_channel"].as_bool().unwrap_or(false),
                )
                .await
            }
            None => Some("uploads.auto.no_rights"),
        }
    } else {
        Some("uploads.auto.premium_only")
    };
    let message = query.load_message().await?;
    if let Some(problem) = problem {
        query.answer().send().await.ok();
        message
            .edit(super::html(&bot.t(&lang, problem, &args), None))
            .await
            .ok();
        leave_quietly(
            bot,
            chat_id,
            &format!("confirm check failed for user {}", who.id),
        )
        .await;
        return Ok(());
    }
    sqlx::query("UPDATE users SET upload_dest_id = $2 WHERE user_id = $1")
        .bind(who.id)
        .bind(chat_id)
        .execute(&bot.pg)
        .await?;
    query.answer().send().await.ok();
    message
        .edit(super::html(
            &bot.t(&lang, "uploads.auto.linked", &args),
            None,
        ))
        .await
        .ok();
    Ok(())
}

/// Adds that were never confirmed do not keep the bot in the chat.
pub async fn leave_expired(bot: &Bot) {
    let Ok(rows) = pending::take_expired(&bot.pg, KIND).await else {
        return;
    };
    for (user_id, data) in rows {
        if let (Some(message_id), Some(peer)) =
            (data["message_id"].as_i64(), bot.user_ref(user_id).await)
        {
            bot.client
                .delete_messages(peer, &[message_id as i32])
                .await
                .ok();
        }
        if let Some(chat_id) = data["chat_id"].as_i64() {
            leave_quietly(
                bot,
                chat_id,
                &format!("link of user {user_id} was not confirmed in time"),
            )
            .await;
        }
    }
}

pub async fn command(bot: &Arc<Bot>, message: &Message, who: &Who, args: &[&str]) -> Result<()> {
    let lang = bot.lang(who.id).await;
    if !account::load(&bot.pg, who.id)
        .await?
        .is_some_and(|u| u.is_premium)
    {
        let markup = keyboard(&[
            vec![Button::callback(
                &bot.t(&lang, "buttons.subscribe", &[]),
                "show_premium",
                Color::Green,
            )],
            vec![bot.button(&lang, "❌", "cancel_download", Color::Red)],
        ]);
        bot.reply(
            message,
            &bot.t(&lang, "premium.feature_required", &[]),
            Some(markup),
        )
        .await?;
        return Ok(());
    }
    let Some(reference) = args.first() else {
        return begin_guide(bot, message, who, "uploadch").await;
    };
    let resolved = resolve_channel(bot, reference).await;
    let Some((peer, dialog_id)) = resolved else {
        bot.reply(
            message,
            &bot.t(&lang, "uploads.channel.invalid_id", &[]),
            None,
        )
        .await?;
        return Ok(());
    };
    let verdict = async {
        match role_in(bot, peer.id, tl::enums::InputPeer::PeerSelf).await {
            Some(Role::Admin { .. } | Role::Owner) => {}
            _ => return Err("uploads.channel.bot_not_admin"),
        }
        let user = user_input(bot, who.id)
            .await
            .ok_or("uploads.channel.access_denied")?;
        match role_in(bot, peer.id, user).await {
            Some(Role::Owner | Role::Admin { .. }) => Ok(()),
            Some(_) => Err("uploads.channel.user_not_admin"),
            None => Err("uploads.channel.access_denied"),
        }
    }
    .await;
    if let Err(key) = verdict {
        bot.reply(message, &bot.t(&lang, key, &[]), None).await?;
        return Ok(());
    }
    let title = match bot.client.resolve_peer(peer).await {
        Ok(p) => p
            .name()
            .map(str::to_string)
            .unwrap_or_else(|| dialog_id.to_string()),
        Err(_) => dialog_id.to_string(),
    };
    sqlx::query("UPDATE users SET upload_dest_id = $2 WHERE user_id = $1")
        .bind(who.id)
        .bind(dialog_id)
        .execute(&bot.pg)
        .await?;
    bot.reply(
        message,
        &bot.t(
            &lang,
            "uploads.channel.enabled",
            &[("channel_title", &crate::bot::escape(&title))],
        ),
        None,
    )
    .await?;
    Ok(())
}

async fn resolve_channel(bot: &Bot, reference: &str) -> Option<(PeerRef, i64)> {
    match parse_chat_reference(reference).ok()? {
        Chat::Username(name) => {
            let peer = bot.client.resolve_username(&name).await.ok()??;
            let peer_ref = peer.to_ref().await.ok()??;
            Some((peer_ref, peer_ref.id.bot_api_dialog_id()?))
        }
        Chat::Id(dialog_id) => {
            let id = PeerId::from_bot_api_dialog_id(dialog_id)?;
            Some((bot.peer_ref_for(id).await?, dialog_id))
        }
        Chat::Invite(_) => None,
    }
}

pub async fn remove(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let done = sqlx::query(
        "UPDATE users SET upload_dest_id = NULL WHERE user_id = $1 AND upload_dest_id IS NOT NULL",
    )
    .bind(who.id)
    .execute(&bot.pg)
    .await?;
    let key = if done.rows_affected() > 0 {
        "uploads.channel.removed"
    } else {
        "uploads.channel.not_set"
    };
    bot.reply(message, &bot.t(&lang, key, &[]), None).await?;
    Ok(())
}

/// The reply to the `/uploadch` guide: a typed reference, or a chat picked with the share button.
pub async fn guide_answer(
    bot: &Arc<Bot>,
    message: &Message,
    who: &Who,
    shared: Option<i64>,
) -> Result<bool> {
    let Some(state) =
        crate::store::states::get::<Value>(&bot.pg, who.id, super::download::GUIDE).await?
    else {
        return Ok(false);
    };
    let kind = state["kind"].as_str().unwrap_or("").to_string();
    if kind != "uploadch" && kind != "monitor" {
        return Ok(false);
    }
    let lang = bot.lang(who.id).await;
    let value = shared
        .map(|id| id.to_string())
        .unwrap_or_else(|| message.text().trim().to_string());
    if parse_chat_reference(&value).is_err() {
        bot.reply(message, &bot.t(&lang, "flow.invalid_channel", &[]), None)
            .await?;
        return Ok(true);
    }
    crate::store::states::delete(&bot.pg, who.id, super::download::GUIDE).await?;
    bot.send_to(
        who.id,
        &bot.t(&lang, "common.processing", &[]),
        Some(ReplyMarkup::hide()),
    )
    .await
    .ok();
    command(bot, message, who, &[&value]).await?;
    Ok(true)
}
