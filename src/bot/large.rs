//! Buttons under the large file question.

use std::sync::Arc;

use anyhow::Result;
use grammers_client::update::CallbackQuery;

use super::{Bot, Who, html};
use crate::{store::pending, tasks};

pub async fn callback(bot: &Arc<Bot>, query: &CallbackQuery, who: &Who, data: &str) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let parts: Vec<&str> = data.split('_').collect();
    let (action_id, choice) = match parts.as_slice() {
        [_, id, choice @ ("saved" | "split" | "cancel" | "help")] => match id.parse::<i64>() {
            Ok(id) => (id, *choice),
            Err(_) => {
                query.answer().send().await.ok();
                return Ok(());
            }
        },
        _ => {
            query.answer().send().await.ok();
            return Ok(());
        }
    };
    let row = pending::get(&bot.pg, action_id, who.id)
        .await?
        .filter(|(kind, _)| kind == tasks::large::KIND);
    let Some((_, action)) = row else {
        expired(bot, query, &lang).await;
        return Ok(());
    };
    if choice == "help" {
        if let Some(peer) = bot.user_ref(who.id).await {
            let text = bot.t(&lang, "large.help", &[]);
            bot.client
                .send_message(peer, html(&text, None).link_preview(false))
                .await
                .ok();
        }
        query.answer().send().await.ok();
        return Ok(());
    }
    let premium = action["premium_account"].as_bool().unwrap_or(false);
    if choice == "saved" && !premium {
        query.answer().send().await.ok();
        return Ok(());
    }
    // Only the first tap counts: the choice is written only while it is still empty.
    let claimed: Option<(i64,)> = sqlx::query_as(
        "UPDATE pending_actions SET data = jsonb_set(data, '{choice}', to_jsonb($3::text)) \
         WHERE action_id = $1 AND user_id = $2 AND expires_at > NOW() \
         AND (data->>'choice') IS NULL RETURNING action_id",
    )
    .bind(action_id)
    .bind(who.id)
    .bind(choice)
    .fetch_optional(&bot.pg)
    .await?;
    if claimed.is_none() {
        expired(bot, query, &lang).await;
        return Ok(());
    }
    bot.large_wake.notify_waiters();
    if let Ok(message) = query.load_message().await {
        message.delete().await.ok();
    }
    if choice == "cancel" {
        let original = action["original_message_id"].as_i64().unwrap_or(0);
        if original != 0
            && let Some(peer) = bot.user_ref(who.id).await
        {
            bot.client
                .delete_messages(peer, &[original as i32])
                .await
                .ok();
        }
        if let Some(queue_id) = action["queue_id"].as_i64() {
            tasks::cancel(bot, who.id, Some(queue_id)).await?;
            pending::delete(&bot.pg, action_id).await;
        }
    }
    query.answer().send().await.ok();
    Ok(())
}

async fn expired(bot: &Bot, query: &CallbackQuery, lang: &str) {
    query
        .answer()
        .alert(bot.t(lang, "common.state_expired_popup", &[]))
        .send()
        .await
        .ok();
}
