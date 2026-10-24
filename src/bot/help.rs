use std::sync::Arc;

use anyhow::Result;
use grammers_client::{message::ReplyMarkup, update::Message};

use super::{Bot, Who};
use crate::telegram::keyboards::{Button, Color, keyboard};

pub fn menu(bot: &Bot, lang: &str) -> (String, ReplyMarkup) {
    let b = |key: &str, data: &str, color| Button::callback(&bot.t(lang, key, &[]), data, color);
    let markup = keyboard(&[
        vec![
            b("help.buttons.single", "help_feature_dl", Color::Blue),
            b("help.buttons.batch", "help_feature_bdl", Color::Blue),
        ],
        vec![
            b("help.buttons.clone", "help_feature_clone", Color::Blue),
            b("help.buttons.monitor", "help_feature_monitor", Color::Blue),
        ],
        vec![
            b(
                "help.buttons.channel_upload",
                "help_feature_channel_upload",
                Color::Blue,
            ),
            b(
                "help.buttons.forcedownload",
                "help_feature_forcedownload",
                Color::Blue,
            ),
        ],
        vec![
            b("help.buttons.login", "help_feature_login", Color::Blue),
            b("help.buttons.points", "help_feature_points", Color::Blue),
        ],
        vec![
            b("help.buttons.premium", "help_feature_premium", Color::Green),
            b(
                "help.buttons.other_uses",
                "help_feature_other_uses",
                Color::Green,
            ),
        ],
        vec![b("buttons.back", "main_menu", Color::Red)],
    ]);
    (bot.t(lang, "help.menu.title", &[]), markup)
}

pub fn feature(bot: &Bot, lang: &str, user_id: i64, name: &str) -> (String, ReplyMarkup) {
    let text = bot.t(
        lang,
        &format!("help.features.{name}"),
        &[("referral_link", &bot.referral_link(user_id))],
    );
    let markup = keyboard(&[vec![Button::callback(
        &bot.t(lang, "buttons.back", &[]),
        "help_menu",
        Color::Red,
    )]]);
    (text, markup)
}

pub async fn help(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let (text, markup) = menu(bot, &lang);
    bot.reply(message, &text, Some(markup)).await?;
    Ok(())
}
