//! Messages a task sends when it cannot reach its source.

use super::TaskCtx;
use crate::telegram::keyboards::{Button, Color, keyboard};

pub async fn login_required(ctx: &TaskCtx) {
    let markup = keyboard(&[
        vec![Button::callback(
            &ctx.bot.t(&ctx.lang, "buttons.link_account", &[]),
            "start_login",
            Color::Green,
        )],
        vec![
            ctx.bot
                .button(&ctx.lang, "❌", "cancel_download", Color::Red),
        ],
    ]);
    send(ctx, "accounts.access.login_required", markup).await;
}

pub async fn not_member(ctx: &TaskCtx, source_url: Option<&str>) {
    let mut rows = Vec::new();
    if let Some(url) = source_url.filter(|u| u.starts_with("http")) {
        rows.push(vec![Button::url(
            &ctx.bot.t(&ctx.lang, "buttons.open_source", &[]),
            url,
            Color::Blue,
        )]);
    }
    rows.push(vec![ctx.bot.button(
        &ctx.lang,
        "❌",
        "cancel_download",
        Color::Red,
    )]);
    send(ctx, "accounts.access.not_member", keyboard(&rows)).await;
}

pub async fn session_expired(ctx: &TaskCtx) {
    let markup = keyboard(&[
        vec![Button::callback(
            &ctx.bot.t(&ctx.lang, "buttons.link_account", &[]),
            "start_login",
            Color::Green,
        )],
        vec![
            ctx.bot
                .button(&ctx.lang, "❌", "cancel_download", Color::Red),
        ],
    ]);
    send(ctx, "accounts.session_expired", markup).await;
}

async fn send(ctx: &TaskCtx, key: &str, markup: grammers_client::message::ReplyMarkup) {
    let text = ctx.bot.t(&ctx.lang, key, &[]);
    match &ctx.status {
        Some(status) if ctx.queue_id.is_some() => {
            // The buttons are the task's last word, so they stay when the cancel button is taken away.
            status.set_final(true, Some(text.clone()), Some(markup.raw.clone()));
            status.edit(&ctx.bot, &text, Some(markup)).await;
        }
        _ => {
            ctx.bot.send_to(ctx.user_id, &text, Some(markup)).await.ok();
        }
    }
}
