//! A file bigger than one upload part: the user chooses how to receive it, unless a Premium account already sends to their channel.

use std::time::Duration;

use serde_json::json;

use super::{TaskCtx, download::Mode};
use crate::{
    store::{pending, queue},
    telegram::{
        accounts::Account,
        keyboards::{Button, Color, keyboard},
    },
};

pub const KIND: &str = "large_media";
const TTL: i64 = 24 * 60 * 60;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum Policy {
    #[default]
    Normal,
    /// Premium account uploads the whole file to the user's channel.
    Direct,
    /// Premium account uploads the whole file to Saved Messages.
    Saved,
    /// Numbered parts of about 2 GB each.
    Split,
}

pub enum Asked {
    Chosen(Policy, Option<i64>),
    Pending,
    Cancelled,
}

fn parse(choice: &str) -> Option<Policy> {
    match choice {
        "saved" => Some(Policy::Saved),
        "split" => Some(Policy::Split),
        _ => None,
    }
}

/// With `wait` the call returns once the user has chosen; without it a choice not yet made gives `Pending`.
pub async fn ask(
    ctx: &TaskCtx,
    account: &Account,
    mode: Mode,
    source: (i64, i32),
    size: u64,
    wait: bool,
) -> Asked {
    let bot = &ctx.bot;
    let premium_account = account.is_linked() && account.is_premium();
    if premium_account && mode == Mode::Channel {
        return Asked::Chosen(Policy::Direct, None);
    }
    let user_id = ctx.user_id;
    let existing: Option<(i64,)> = sqlx::query_as(
        "SELECT action_id FROM pending_actions WHERE user_id = $1 AND kind = $2 AND expires_at > NOW() \
         AND data->>'source_chat_id' = $3 AND data->>'source_message_id' = $4 \
         AND data->>'queue_id' IS NOT DISTINCT FROM $5 ORDER BY action_id DESC LIMIT 1",
    )
    .bind(user_id)
    .bind(KIND)
    .bind(source.0.to_string())
    .bind(source.1.to_string())
    .bind(ctx.queue_id.map(|id| id.to_string()))
    .fetch_optional(&bot.pg)
    .await
    .ok()
    .flatten();
    let action_id = match existing {
        Some((id,)) => id,
        None => {
            let original = match ctx.queue_id {
                Some(queue_id) => queue::spec(&bot.pg, queue_id, user_id)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|spec| spec.get("message_id").and_then(|v| v.as_i64()))
                    .unwrap_or(0),
                None => 0,
            };
            let data = json!({
                "source_chat_id": source.0,
                "source_message_id": source.1,
                "original_message_id": original,
                "queue_id": ctx.queue_id,
                "premium_account": premium_account,
                "choice": null,
            });
            match pending::create(&bot.pg, user_id, KIND, &data, TTL).await {
                Ok(id) => id,
                Err(error) => {
                    tracing::error!(user_id, %error, "could not save the large file question");
                    return Asked::Cancelled;
                }
            }
        }
    };
    let row = pending::get(&bot.pg, action_id, user_id)
        .await
        .ok()
        .flatten();
    let asked = row.as_ref().is_some_and(|(_, data)| {
        data.get("notice_message_id").is_some_and(|v| !v.is_null())
            || data.get("choice").is_some_and(|v| !v.is_null())
    });
    if !asked {
        let lang = &ctx.lang;
        let mut rows = Vec::new();
        if premium_account {
            rows.push(vec![Button::callback(
                &bot.t(lang, "large.saved", &[]),
                &format!("lg_{action_id}_saved"),
                Color::Green,
            )]);
        }
        rows.push(vec![Button::callback(
            &bot.t(lang, "large.split", &[]),
            &format!("lg_{action_id}_split"),
            Color::Green,
        )]);
        rows.push(vec![
            Button::callback(
                &bot.t(lang, "buttons.cancel", &[]),
                &format!("lg_{action_id}_cancel"),
                Color::Red,
            ),
            Button::callback(
                &bot.t(lang, "large.help_button", &[]),
                &format!("lg_{action_id}_help"),
                Color::Blue,
            ),
        ]);
        let key = if premium_account {
            "large.premium_notice"
        } else {
            "large.split_notice"
        };
        let text = bot.t(lang, key, &[("size", &bot.size(lang, size))]);
        match bot.send_to(user_id, &text, Some(keyboard(&rows))).await {
            Ok(notice) => {
                pending::merge(
                    &bot.pg,
                    action_id,
                    &json!({"notice_message_id": notice.id()}),
                )
                .await
            }
            Err(error) => tracing::warn!(user_id, %error, "could not send the large file question"),
        }
    }
    loop {
        let data = pending::get(&bot.pg, action_id, user_id)
            .await
            .ok()
            .flatten()
            .map(|(_, data)| data);
        let Some(data) = data else {
            return Asked::Cancelled;
        };
        match data.get("choice").and_then(|v| v.as_str()) {
            Some("cancel") => {
                pending::delete(&bot.pg, action_id).await;
                return Asked::Cancelled;
            }
            Some(choice) => {
                return Asked::Chosen(parse(choice).unwrap_or_default(), Some(action_id));
            }
            None if !wait => return Asked::Pending,
            None => {}
        }
        tokio::select! {
            _ = ctx.cancel.cancelled() => return Asked::Cancelled,
            _ = bot.large_wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
        }
    }
}
