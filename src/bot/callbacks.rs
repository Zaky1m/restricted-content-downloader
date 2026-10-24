use std::sync::Arc;

use anyhow::Result;
use grammers_client::update::CallbackQuery;

use super::{Bot, Who, guards, help, send::html, start};

/// Replaces the text (or photo caption) of the message the button sits on.
async fn show(
    query: &CallbackQuery,
    text: &str,
    markup: grammers_client::message::ReplyMarkup,
) -> Result<()> {
    let message = query.load_message().await?;
    match message.edit(html(text, Some(markup))).await {
        Ok(()) => Ok(()),
        Err(error) if error.is("MESSAGE_NOT_MODIFIED") => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub async fn route(bot: &Arc<Bot>, query: &CallbackQuery, who: &Who) -> Result<()> {
    let data = String::from_utf8_lossy(query.data()).into_owned();
    let lang = bot.lang(who.id).await;
    match data.as_str() {
        "main_menu" => {
            query.answer().send().await.ok();
            let (text, markup) = start::main_menu(bot, who.id, &lang).await;
            show(query, &text, markup).await
        }
        "help_menu" => {
            query.answer().send().await.ok();
            let (text, markup) = help::menu(bot, &lang);
            show(query, &text, markup).await
        }
        d if d.starts_with("help_feature_") => {
            let (text, markup) = help::feature(bot, &lang, who.id, &d["help_feature_".len()..]);
            show(query, &text, markup).await?;
            query.answer().send().await.ok();
            Ok(())
        }
        d if d.starts_with("rc_") => start::check_button(bot, query, who, &d[3..]).await,
        "show_premium" => match super::account::premium_view(bot, &lang, who.id).await? {
            super::account::Premium::Already { popup, .. } => {
                query.answer().alert(popup).send().await.ok();
                Ok(())
            }
            super::account::Premium::Menu { text, markup } => {
                query.answer().send().await.ok();
                show(query, &text, markup).await
            }
        },
        d if d.starts_with("login_")
            || matches!(d, "cancel_login" | "confirm_logout" | "cancel_logout") =>
        {
            super::login::on_callback(bot, query, who, d).await
        }
        d if d.starts_with("forcedownload_")
            || d.starts_with("thumb_")
            || d.starts_with("caption_") =>
        {
            super::settings::on_callback(bot, query, who, d).await
        }
        "flow_cancel" => {
            crate::store::states::delete(&bot.pg, who.id, super::download::GUIDE).await?;
            if let Ok(m) = query.load_message().await {
                m.delete().await.ok();
            }
            bot.send_to(
                who.id,
                &bot.t(&lang, "common.action_cancelled", &[]),
                Some(grammers_client::message::ReplyMarkup::hide()),
            )
            .await?;
            query.answer().send().await.ok();
            Ok(())
        }
        d if d.starts_with("lg_") => super::large::callback(bot, query, who, d).await,
        d if d.starts_with("qc_") => super::cancel::button(bot, query, who, d).await,
        d if d.starts_with("flt_") => super::range::filter_callback(bot, query, who, d).await,
        d if d.starts_with("ul_y_") || d.starts_with("ul_n_") => {
            super::uploadch::callback(bot, query, who, d).await
        }
        d if d.starts_with("mon_off_") => crate::monitor::stop_button(bot, query, who, d).await,
        d if d.starts_with("buy_premium_") => super::billing::buy(bot, query, who, d).await,
        d if d.starts_with("ad_") => super::admin::callback(bot, query, who, d).await,
        d if d.starts_with("confirm_del_gift_") => {
            super::admin::confirm_gift_delete(bot, query, who, d).await
        }
        d if d.starts_with("bc_") || d.starts_with("bcjson_") || d == "cancel_action" => {
            super::admin::broadcast_callback(bot, query, who, d).await
        }
        "my_profile" => {
            query.answer().send().await.ok();
            let text = super::account::profile_text(bot, &lang, who.id).await?;
            query.load_message().await?.reply(html(&text, None)).await?;
            Ok(())
        }
        d if d.starts_with("set_lang_") => {
            let code = &d["set_lang_".len()..];
            if !bot.locales.has(code) {
                query
                    .answer()
                    .alert(bot.t(&lang, "language.unavailable_popup", &[]))
                    .send()
                    .await
                    .ok();
                return Ok(());
            }
            crate::store::users::set_lang(&bot.pg, &bot.locales, who.id, code).await?;
            super::commands::set_for_user(bot, who.id, code).await;
            let name = bot.t(code, "lang_name", &[]);
            let text = bot.t(code, "language.changed", &[("language_name", &name)]);
            query.load_message().await?.edit(html(&text, None)).await?;
            query.answer().send().await.ok();
            Ok(())
        }
        "fsub_refresh" => guards::refresh_subscription(bot, query, who).await,
        "start_login" => {
            query.answer().send().await.ok();
            if let Ok(message) = query.load_message().await {
                message.delete().await.ok();
            }
            super::login::login(bot, None, who).await
        }
        "cancel_download" => {
            query.answer().send().await.ok();
            if let Ok(message) = query.load_message().await {
                message.delete().await.ok();
            }
            Ok(())
        }
        other => {
            tracing::warn!(data = other, user = who.id, "unhandled callback data");
            query.answer().send().await.ok();
            Ok(())
        }
    }
}
