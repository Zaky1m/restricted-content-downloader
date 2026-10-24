//! The command menu Telegram shows: the same list for everyone, plus the admin commands for staff only.

use grammers_client::tl;

use super::Bot;

const USER_COMMANDS: [&str; 18] = [
    "start",
    "help",
    "profile",
    "premium",
    "language",
    "login",
    "logout",
    "dl",
    "bdl",
    "clone",
    "forcedownload",
    "thumb",
    "caption",
    "cancel",
    "stats",
    "monitor",
    "uploadch",
    "rmuploadch",
];
const STAFF_COMMANDS: [&str; 3] = ["admin", "bc", "bcjson"];

fn list(bot: &Bot, lang: &str, staff: bool) -> Vec<tl::enums::BotCommand> {
    let mut names: Vec<&str> = USER_COMMANDS.to_vec();
    if staff {
        names.extend(STAFF_COMMANDS);
    }
    names
        .into_iter()
        .map(|name| {
            tl::types::BotCommand {
                command: name.to_string(),
                description: bot.t(lang, &format!("commands.{name}.description"), &[]),
            }
            .into()
        })
        .collect()
}

/// The default menu, in English, for everyone.
pub async fn set_default(bot: &Bot) {
    let request = tl::functions::bots::SetBotCommands {
        scope: tl::types::BotCommandScopeDefault {}.into(),
        lang_code: String::new(),
        commands: list(bot, crate::i18n::DEFAULT_LANGUAGE, false),
    };
    if let Err(error) = bot.client.invoke(&request).await {
        tracing::warn!(%error, "could not set the default command menu");
    }
}

/// The menu in the user's language (staff also get the admin commands).
pub async fn set_for_user(bot: &Bot, user_id: i64, lang: &str) {
    let Some(peer) = bot.user_ref(user_id).await else {
        return;
    };
    let staff = bot.is_staff(user_id).await;
    let request = tl::functions::bots::SetBotCommands {
        scope: tl::types::BotCommandScopePeer {
            peer: (&peer).into(),
        }
        .into(),
        lang_code: String::new(),
        commands: list(bot, lang, staff),
    };
    if let Err(error) = bot.client.invoke(&request).await {
        tracing::warn!(user_id, %error, "could not apply the localized commands");
    }
}
