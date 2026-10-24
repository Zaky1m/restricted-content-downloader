//! `/cancel`, the word "cancel", and the buttons that stop a task.

use std::sync::Arc;

use anyhow::Result;
use grammers_client::{
    message::ReplyMarkup,
    update::{CallbackQuery, Message},
};

use super::{Bot, Who, download::GUIDE, html};
use crate::{
    store::{queue, states},
    tasks,
    telegram::keyboards::{Button, Color, keyboard},
};

/// "cancel" or "cancel 12" / "cancel #12" typed without a slash.
pub fn is_cancel_word(text: &str) -> bool {
    let mut parts = text.split_whitespace();
    let Some(head) = parts.next() else {
        return false;
    };
    if !head.eq_ignore_ascii_case("cancel") {
        return false;
    }
    match (parts.next(), parts.next()) {
        (None, _) => true,
        (Some(number), None) => {
            number
                .trim_start_matches('#')
                .chars()
                .all(|c| c.is_ascii_digit())
                && !number.trim_start_matches('#').is_empty()
        }
        _ => false,
    }
}

fn task_keyboard(bot: &Bot, lang: &str, open: &[queue::OpenTask]) -> ReplyMarkup {
    let mut rows: Vec<Vec<Button>> = open
        .iter()
        .map(|task| {
            let label = bot.t(
                lang,
                "queue.buttons.cancel_task",
                &[
                    ("task_id", &task.queue_id.to_string()),
                    (
                        "kind",
                        &bot.t(lang, &format!("queue.kinds.{}", task.kind), &[]),
                    ),
                    (
                        "status",
                        &bot.t(lang, &format!("queue.status.{}", task.status), &[]),
                    ),
                ],
            );
            vec![Button::callback(
                &label,
                &format!("qc_{}", task.queue_id),
                Color::Red,
            )]
        })
        .collect();
    if open.len() > 1 {
        rows.push(vec![Button::callback(
            &bot.t(lang, "queue.buttons.cancel_all", &[]),
            "qc_all",
            Color::Red,
        )]);
    }
    keyboard(&rows)
}

pub async fn command(bot: &Arc<Bot>, message: &Message, who: &Who, args: &[&str]) -> Result<()> {
    let lang = bot.lang(who.id).await;
    states::delete(&bot.pg, who.id, GUIDE).await?;
    states::delete(&bot.pg, who.id, "admin_panel").await?;

    if let Some(reference) = args.first() {
        let reference = reference.trim_start_matches('#');
        let Ok(id) = reference.parse::<i64>() else {
            bot.reply(message, &bot.t(&lang, "queue.cancel_usage", &[]), None)
                .await?;
            return Ok(());
        };
        let cancelled = tasks::cancel(bot, who.id, Some(id)).await?;
        let key = if cancelled.is_empty() {
            "queue.not_found"
        } else {
            "queue.cancelled_one"
        };
        bot.reply(
            message,
            &bot.t(&lang, key, &[("task_id", &id.to_string())]),
            None,
        )
        .await?;
        return Ok(());
    }

    let broadcast = super::admin::cancel_broadcast(bot, who.id);
    let login = states::delete(&bot.pg, who.id, "login").await?;
    let filter = states::delete(&bot.pg, who.id, "filter").await?;
    let confirm = states::take::<serde_json::Value>(&bot.pg, who.id, "confirm").await?;
    if let Some(path) = confirm.as_ref().and_then(|c| c["path"].as_str()) {
        tokio::fs::remove_file(path).await.ok();
    }
    let open = queue::open_tasks(&bot.pg, who.id).await?;
    if !open.is_empty() {
        let text = bot.t(
            &lang,
            "queue.choose_to_cancel",
            &[("count", &open.len().to_string())],
        );
        bot.reply(message, &text, Some(task_keyboard(bot, &lang, &open)))
            .await?;
        return Ok(());
    }
    let key = if broadcast || login || filter || confirm.is_some() {
        "tasks.input_cancelled"
    } else {
        "tasks.none_to_cancel"
    };
    bot.reply(message, &bot.t(&lang, key, &[]), Some(ReplyMarkup::hide()))
        .await?;
    Ok(())
}

pub async fn button(bot: &Arc<Bot>, query: &CallbackQuery, who: &Who, data: &str) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let target = data.strip_prefix("qc_").unwrap_or("");
    // A task with a runner writes its own last words (a batch says what stops); editing here as well
    // races with it and can leave the shorter text.
    let has_runner = target
        .parse::<i64>()
        .is_ok_and(|id| bot.running.lock().unwrap().contains_key(&id));
    let cancelled = if target == "all" {
        tasks::cancel(bot, who.id, None).await?
    } else if let Ok(id) = target.parse::<i64>() {
        tasks::cancel(bot, who.id, Some(id)).await?
    } else {
        Vec::new()
    };
    let key = if cancelled.is_empty() {
        "queue.not_found_popup"
    } else {
        "queue.cancelled_popup"
    };
    if cancelled.is_empty() {
        query
            .answer()
            .alert(bot.t(&lang, key, &[]))
            .send()
            .await
            .ok();
    } else {
        query
            .answer()
            .text(bot.t(&lang, key, &[]))
            .send()
            .await
            .ok();
    }
    let Ok(message) = query.load_message().await else {
        return Ok(());
    };
    // The button on a task's own status message ends there; the task's runner writes the final text.
    if let Ok(id) = target.parse::<i64>()
        && let Some(spec) = queue::spec(&bot.pg, id, who.id).await?
        && spec["status_message_id"].as_i64() == Some(i64::from(message.id()))
    {
        if !cancelled.is_empty() && !has_runner {
            message
                .edit(html(
                    &bot.t(
                        &lang,
                        "queue.cancelled_one",
                        &[("task_id", &id.to_string())],
                    ),
                    None,
                ))
                .await
                .ok();
        }
        return Ok(());
    }
    let remaining = queue::open_tasks(&bot.pg, who.id).await?;
    if !remaining.is_empty() && target != "all" {
        let text = bot.t(
            &lang,
            "queue.choose_to_cancel",
            &[("count", &remaining.len().to_string())],
        );
        message
            .edit(html(&text, Some(task_keyboard(bot, &lang, &remaining))))
            .await
            .ok();
    } else {
        message.delete().await.ok();
    }
    Ok(())
}
