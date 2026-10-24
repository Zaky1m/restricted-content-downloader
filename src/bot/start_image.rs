//! The optional welcome photo. It is posted once into the archive channel (Telegram fetches a link itself,
//! a local file is uploaded) and every `/start` reuses that copy.

use grammers_client::{media::Media, message::InputMessage, session::types::PeerId};
use serde_json::json;

use super::Bot;
use crate::store::settings;

const SETTING: &str = "start_image";

/// The photo to send, or `None` when the feature is off or the image cannot be prepared.
pub async fn media(bot: &Bot) -> Option<Media> {
    let (source, channel_id) = (bot.cfg.start_image.as_ref()?, bot.cfg.archive_channel_id?);
    let is_link = source.starts_with("https://") || source.starts_with("http://");
    // A changed local file gets posted again: its size and change time are part of the key.
    let key = if is_link {
        source.clone()
    } else {
        let meta = std::fs::metadata(source).ok()?;
        let changed = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        format!("{source}:{}:{changed}", meta.len())
    };
    let mut cached = bot.start_image.lock().await;
    if let Some(media) = cached.as_ref() {
        return Some(media.clone());
    }
    let channel = bot
        .peer_ref_for(PeerId::from_bot_api_dialog_id(channel_id)?)
        .await;
    let Some(channel) = channel else {
        tracing::warn!(
            channel_id,
            "archive channel is unknown; add the bot to it as an administrator"
        );
        return None;
    };
    let stored: Option<serde_json::Value> = settings::get(&bot.pg, SETTING)
        .await
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    if let Some(stored) = stored.filter(|s| s["source"].as_str() == Some(key.as_str()))
        && let Some(id) = stored["message_id"].as_i64()
        && let Ok(messages) = bot.client.get_messages_by_id(channel, &[id as i32]).await
        && let Some(Some(message)) = messages.into_iter().next()
        && let Some(media) = message.media()
    {
        *cached = Some(media.clone());
        return Some(media);
    }
    let post = if is_link {
        InputMessage::new().photo_url(source.clone())
    } else {
        match bot.client.upload_file(source).await {
            Ok(file) => InputMessage::new().photo(file),
            Err(error) => {
                tracing::warn!(%error, "start image file could not be uploaded");
                return None;
            }
        }
    };
    match bot.client.send_message(channel, post).await {
        Ok(message) => {
            let media = message.media()?;
            let value = json!({"source": key, "message_id": message.id()}).to_string();
            if let Err(error) = settings::set(&bot.pg, SETTING, Some(&value)).await {
                tracing::warn!(%error, "could not remember the archived start image");
            }
            *cached = Some(media.clone());
            Some(media)
        }
        Err(error) => {
            tracing::warn!(%error, "start image could not be posted to the archive channel");
            None
        }
    }
}

/// Drops the remembered photo so the next `/start` reads it again (its file reference expired).
pub async fn forget(bot: &Bot) {
    *bot.start_image.lock().await = None;
}
