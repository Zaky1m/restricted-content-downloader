//! `/forcedownload`, `/thumb`, `/caption` and the photo a user sends to set a thumbnail.

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use grammers_client::{
    message::{InputMessage, ReplyMarkup},
    update::{CallbackQuery, Message},
};
use serde::{Deserialize, Serialize};

use super::{Bot, Who, send::html};
use crate::{
    format::safe_filename,
    store::{
        account::{self, UserData},
        states,
    },
    telegram::keyboards::{Button, Color, keyboard},
};

const CONFIRM_KIND: &str = "confirm";
const CONFIRM_TTL: i64 = 10 * 60;

fn subscribe_keyboard(bot: &Bot, lang: &str) -> ReplyMarkup {
    keyboard(&[
        vec![Button::callback(
            &bot.t(lang, "buttons.subscribe", &[]),
            "show_premium",
            Color::Green,
        )],
        vec![bot.button(lang, "❌", "cancel_download", Color::Red)],
    ])
}

fn toggle_label(bot: &Bot, lang: &str, scope: &str, enabled: bool) -> String {
    bot.t(
        lang,
        &format!(
            "{scope}.buttons.{}",
            if enabled { "disable" } else { "enable" }
        ),
        &[],
    )
}

fn status(bot: &Bot, lang: &str, scope: &str, enabled: bool) -> String {
    bot.t(
        lang,
        &format!(
            "{scope}.status.{}",
            if enabled { "enabled" } else { "disabled" }
        ),
        &[],
    )
}

pub async fn set_force_download(bot: &Bot, user_id: i64, enabled: bool) -> Result<bool> {
    let premium: Option<(Option<bool>,)> =
        sqlx::query_as("SELECT is_premium FROM users WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(&bot.pg)
            .await?;
    let enabled = enabled && premium.and_then(|p| p.0).unwrap_or(false);
    sqlx::query("UPDATE users SET force_download_enabled = $2 WHERE user_id = $1")
        .bind(user_id)
        .bind(enabled)
        .execute(&bot.pg)
        .await?;
    Ok(enabled)
}

fn force_keyboard(bot: &Bot, lang: &str, enabled: bool) -> ReplyMarkup {
    keyboard(&[
        vec![Button::callback(
            &toggle_label(bot, lang, "forcedownload", enabled),
            if enabled {
                "forcedownload_disable"
            } else {
                "forcedownload_enable"
            },
            if enabled { Color::Red } else { Color::Green },
        )],
        vec![bot.button(lang, "❌", "cancel_download", Color::Red)],
    ])
}

pub async fn forcedownload(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let user = account::load(&bot.pg, who.id).await?;
    let Some(user) = user.filter(|u| u.is_premium) else {
        bot.reply(
            message,
            &bot.t(&lang, "forcedownload.premium_required", &[]),
            Some(subscribe_keyboard(bot, &lang)),
        )
        .await?;
        return Ok(());
    };
    let enabled = user.force_download_enabled;
    let text = bot.t(
        &lang,
        "forcedownload.details",
        &[("status", &status(bot, &lang, "forcedownload", enabled))],
    );
    bot.reply(message, &text, Some(force_keyboard(bot, &lang, enabled)))
        .await?;
    Ok(())
}

fn thumb_keyboard(bot: &Bot, lang: &str, enabled: bool) -> ReplyMarkup {
    keyboard(&[
        vec![Button::callback(
            &toggle_label(bot, lang, "thumbnail", enabled),
            "thumb_toggle",
            if enabled { Color::Red } else { Color::Green },
        )],
        vec![bot.button(lang, "❌", "thumb_cancel", Color::Red)],
    ])
}

fn is_valid_file(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

pub async fn thumb(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let user = account::load(&bot.pg, who.id).await?;
    let path = user
        .as_ref()
        .and_then(|u| u.thumbnail_path.clone())
        .filter(|p| is_valid_file(p));
    let Some(path) = path else {
        bot.reply(message, &bot.t(&lang, "thumbnail.not_set", &[]), None)
            .await?;
        return Ok(());
    };
    let enabled = user.is_some_and(|u| u.thumbnail_enabled);
    let caption = bot.t(
        &lang,
        "thumbnail.current",
        &[("status", &status(bot, &lang, "thumbnail", enabled))],
    );
    let uploaded = bot.client.upload_file(&path).await?;
    message
        .reply(
            InputMessage::new()
                .html(&caption)
                .photo(uploaded)
                .reply_markup(thumb_keyboard(bot, &lang, enabled)),
        )
        .await?;
    Ok(())
}

fn caption_keyboard(bot: &Bot, lang: &str, current: &str) -> ReplyMarkup {
    let modes = [
        ("keep_all", "caption_keep_all"),
        ("remove_all", "caption_remove_all"),
        ("remove_links_users", "caption_remove_links"),
    ];
    let mut rows: Vec<Vec<Button>> = modes
        .iter()
        .map(|(mode, data)| {
            let selected = *mode == current;
            vec![Button::callback(
                &bot.t(lang, &format!("captions.buttons.{mode}"), &[]),
                if selected { "caption_noop" } else { data },
                if selected { Color::Green } else { Color::Blue },
            )]
        })
        .collect();
    rows.push(vec![bot.button(lang, "❌", "cancel_download", Color::Red)]);
    keyboard(&rows)
}

pub async fn caption(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let user = account::load(&bot.pg, who.id).await?;
    let Some(user) = user.filter(|u| u.is_premium) else {
        bot.reply(
            message,
            &bot.t(&lang, "premium.feature_required", &[]),
            Some(subscribe_keyboard(bot, &lang)),
        )
        .await?;
        return Ok(());
    };
    let current = bot.t(&lang, &format!("captions.modes.{}", user.caption_mode), &[]);
    let text = bot.t(&lang, "captions.settings", &[("current", &current)]);
    bot.reply(
        message,
        &text,
        Some(caption_keyboard(bot, &lang, &user.caption_mode)),
    )
    .await?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct PendingThumb {
    path: String,
    original_name: String,
}

fn thumb_dirs(bot: &Bot) -> (PathBuf, PathBuf) {
    (
        bot.cfg.data_dir.join("temp_thumbs"),
        bot.cfg.data_dir.join("thumbnails"),
    )
}

fn unique_id() -> String {
    format!(
        "{:016x}{:016x}",
        rand::random::<u64>(),
        rand::random::<u64>()
    )
}

/// A photo sent to the bot becomes a thumbnail candidate the user confirms with a button.
pub async fn on_photo(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let (temp_dir, _) = thumb_dirs(bot);
    tokio::fs::create_dir_all(&temp_dir).await?;
    let temp = temp_dir.join(format!("thumb_{}_{}.jpg", who.id, unique_id()));
    let downloaded = message.download_media(&temp).await;
    let path = temp.to_string_lossy().into_owned();
    if !matches!(downloaded, Ok(true)) || !is_valid_file(&path) {
        tokio::fs::remove_file(&temp).await.ok();
        bot.reply(
            message,
            &bot.t(&lang, "thumbnail.download_failed", &[]),
            None,
        )
        .await?;
        return Ok(());
    }
    if let Some(previous) = states::take::<PendingThumb>(&bot.pg, who.id, CONFIRM_KIND).await? {
        tokio::fs::remove_file(&previous.path).await.ok();
    }
    let pending = PendingThumb {
        path,
        original_name: format!("{}.jpg", unique_id()),
    };
    states::set(&bot.pg, who.id, CONFIRM_KIND, &pending, CONFIRM_TTL).await?;
    let markup = keyboard(&[vec![
        bot.button(&lang, "✅", "thumb_confirm_yes", Color::Green),
        bot.button(&lang, "❌", "thumb_confirm_no", Color::Red),
    ]]);
    let caption = bot.t(&lang, "thumbnail.confirm", &[]);
    let mut reply = InputMessage::new().html(&caption).reply_markup(markup);
    if let Some(media) = message.media() {
        reply = reply.copy_media(&media);
    }
    message.reply(reply).await?;
    Ok(())
}

async fn edit(query: &CallbackQuery, text: &str, markup: Option<ReplyMarkup>) -> Result<()> {
    match query.load_message().await?.edit(html(text, markup)).await {
        Ok(()) => Ok(()),
        Err(error) if error.is("MESSAGE_NOT_MODIFIED") => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn require_premium(
    bot: &Bot,
    query: &CallbackQuery,
    lang: &str,
    user_id: i64,
) -> Result<Option<UserData>> {
    let user = account::load(&bot.pg, user_id)
        .await?
        .filter(|u| u.is_premium);
    if user.is_none() {
        query
            .answer()
            .alert(bot.t(lang, "premium.required_popup", &[]))
            .send()
            .await
            .ok();
    }
    Ok(user)
}

pub async fn on_callback(
    bot: &Arc<Bot>,
    query: &CallbackQuery,
    who: &Who,
    data: &str,
) -> Result<()> {
    let lang = bot.lang(who.id).await;
    match data {
        "forcedownload_enable" | "forcedownload_disable" => {
            if require_premium(bot, query, &lang, who.id).await?.is_none() {
                return Ok(());
            }
            let enabled = data == "forcedownload_enable";
            set_force_download(bot, who.id, enabled).await?;
            edit(
                query,
                &bot.t(
                    &lang,
                    if enabled {
                        "forcedownload.enabled"
                    } else {
                        "forcedownload.disabled"
                    },
                    &[],
                ),
                None,
            )
            .await?;
            query.answer().send().await.ok();
        }
        "thumb_confirm_yes" | "thumb_confirm_no" => {
            let Some(pending) = states::take::<PendingThumb>(&bot.pg, who.id, CONFIRM_KIND).await?
            else {
                query
                    .answer()
                    .alert(bot.t(&lang, "common.state_expired_popup", &[]))
                    .send()
                    .await
                    .ok();
                return Ok(());
            };
            if data == "thumb_confirm_no" {
                tokio::fs::remove_file(&pending.path).await.ok();
                query.answer().send().await.ok();
                query.load_message().await?.delete().await.ok();
                return Ok(());
            }
            if !is_valid_file(&pending.path) {
                edit(query, &bot.t(&lang, "thumbnail.temp_missing", &[]), None).await?;
                return Ok(());
            }
            let (_, final_dir) = thumb_dirs(bot);
            tokio::fs::create_dir_all(&final_dir).await?;
            let name = safe_filename(&pending.original_name, &format!("thumbnail_{}.bin", who.id));
            let final_path = final_dir.join(format!("{}_{}_{}", who.id, unique_id(), name));
            tokio::fs::rename(&pending.path, &final_path).await?;
            let previous = account::load(&bot.pg, who.id)
                .await?
                .and_then(|u| u.thumbnail_path);
            let stored = final_path.to_string_lossy().into_owned();
            sqlx::query(
                "UPDATE users SET thumbnail_path = $2, thumbnail_enabled = TRUE WHERE user_id = $1",
            )
            .bind(who.id)
            .bind(&stored)
            .execute(&bot.pg)
            .await?;
            if let Some(previous) = previous.filter(|p| *p != stored) {
                tokio::fs::remove_file(previous).await.ok();
            }
            edit(query, &bot.t(&lang, "thumbnail.saved", &[]), None).await?;
            query.answer().send().await.ok();
        }
        "thumb_toggle" => {
            let user = account::load(&bot.pg, who.id).await?;
            let path = user
                .as_ref()
                .and_then(|u| u.thumbnail_path.clone())
                .filter(|p| is_valid_file(p));
            if path.is_none() {
                query
                    .answer()
                    .alert(bot.t(&lang, "thumbnail.not_set_popup", &[]))
                    .send()
                    .await
                    .ok();
                return Ok(());
            }
            let enabled = !user.is_some_and(|u| u.thumbnail_enabled);
            sqlx::query("UPDATE users SET thumbnail_enabled = $2 WHERE user_id = $1")
                .bind(who.id)
                .bind(enabled)
                .execute(&bot.pg)
                .await?;
            let text = bot.t(
                &lang,
                "thumbnail.current",
                &[("status", &status(bot, &lang, "thumbnail", enabled))],
            );
            edit(query, &text, Some(thumb_keyboard(bot, &lang, enabled))).await?;
            query.answer().send().await.ok();
        }
        "thumb_cancel" => {
            query.answer().send().await.ok();
            query.load_message().await?.delete().await.ok();
        }
        "caption_noop" => {
            query
                .answer()
                .text(bot.t(&lang, "captions.already_selected_popup", &[]))
                .send()
                .await
                .ok();
        }
        "caption_keep_all" | "caption_remove_all" | "caption_remove_links" => {
            if require_premium(bot, query, &lang, who.id).await?.is_none() {
                return Ok(());
            }
            let mode = match data {
                "caption_keep_all" => "keep_all",
                "caption_remove_all" => "remove_all",
                _ => "remove_links_users",
            };
            sqlx::query("UPDATE users SET caption_mode = $2 WHERE user_id = $1")
                .bind(who.id)
                .bind(mode)
                .execute(&bot.pg)
                .await?;
            let current = bot.t(&lang, &format!("captions.modes.{mode}"), &[]);
            let text = bot.t(&lang, "captions.settings", &[("current", &current)]);
            edit(query, &text, Some(caption_keyboard(bot, &lang, mode))).await?;
            query
                .answer()
                .text(bot.t(&lang, "captions.updated_popup", &[]))
                .send()
                .await
                .ok();
        }
        _ => {}
    }
    Ok(())
}
