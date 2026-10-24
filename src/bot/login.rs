//! Linking a user's own Telegram account: `/login`, `/logout` and every step between.
//!
//! The half-finished login lives in `user_states` (kind `login`) and survives a restart. Telegram
//! accepts the code only on the auth key that asked for it, so that key is stored with the step.

use std::sync::Arc;

use anyhow::Result;
use grammers_client::{
    InvocationError, SignInError,
    client::PasswordToken,
    message::ReplyMarkup,
    session::{SessionData, storages::MemorySession},
    tl,
    update::{CallbackQuery, Message},
};
use serde::{Deserialize, Serialize};

use super::{Bot, Who, send::html};
use crate::{
    phone,
    pyrogram_session::PyrogramSession,
    sessions::{self, Account, Kind},
    store::{states, users},
    telegram::{
        self,
        keyboards::{Button, Color, keyboard},
    },
};

const KIND: &str = "login";
const TTL_SECONDS: i64 = 10 * 60;
const FLOOD_MARKERS: [&str; 3] = [
    "PHONE_PASSWORD_FLOOD",
    "PHONE_NUMBER_FLOOD",
    "PHONE_CODE_FLOOD",
];

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Login {
    pub step: String,
    #[serde(default)]
    pub account_type: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub phone_code_hash: String,
    /// Pyrogram-format string of the not yet authorized connection.
    #[serde(default)]
    pub temp_session: String,
    #[serde(default)]
    pub entered_code: String,
    /// The one message the login keeps editing after the code step (password prompt, result).
    #[serde(default)]
    pub prompt_id: i32,
}

impl Login {
    fn new(step: &str, account_type: &str) -> Self {
        Self {
            step: step.into(),
            account_type: account_type.into(),
            ..Self::default()
        }
    }
}

async fn load(bot: &Bot, user_id: i64) -> Result<Option<Login>> {
    Ok(states::get::<Login>(&bot.pg, user_id, KIND).await?)
}

async fn save(bot: &Bot, user_id: i64, login: &Login) -> Result<()> {
    Ok(states::set(&bot.pg, user_id, KIND, login, TTL_SECONDS).await?)
}

async fn clear(bot: &Bot, user_id: i64) -> Result<bool> {
    Ok(states::delete(&bot.pg, user_id, KIND).await?)
}

/// Moves the login to the next step only if it is still at the expected one, so two contacts or
/// two phone numbers sent quickly can never start two code requests.
async fn claim_step(bot: &Bot, user_id: i64, expected: &str, next: &str) -> Result<bool> {
    let row: Option<(i32,)> = sqlx::query_as(
        "UPDATE user_states SET data = jsonb_set(data, '{step}', to_jsonb($1::text)), updated_at = NOW() \
         WHERE user_id = $2 AND kind = 'login' AND expires_at > NOW() AND data->>'step' = $3 RETURNING 1",
    )
    .bind(next)
    .bind(user_id)
    .bind(expected)
    .fetch_optional(&bot.pg)
    .await?;
    Ok(row.is_some())
}

fn account_type_keyboard(bot: &Bot, lang: &str) -> ReplyMarkup {
    keyboard(&[
        vec![Button::callback(
            &bot.t(lang, "accounts.login.buttons.my_account", &[]),
            "login_account_current",
            Color::Green,
        )],
        vec![Button::callback(
            &bot.t(lang, "accounts.login.buttons.other_account", &[]),
            "login_account_other",
            Color::Blue,
        )],
        vec![bot.button(lang, "❌", "cancel_login", Color::Red)],
    ])
}

fn cancel_only(bot: &Bot, lang: &str) -> ReplyMarkup {
    keyboard(&[vec![bot.button(lang, "❌", "cancel_login", Color::Red)]])
}

fn code_keypad(bot: &Bot, lang: &str) -> ReplyMarkup {
    let digit = |d: &str| Button::callback(d, &format!("login_key_{d}"), Color::Blue);
    keyboard(&[
        vec![digit("1"), digit("2"), digit("3")],
        vec![digit("4"), digit("5"), digit("6")],
        vec![digit("7"), digit("8"), digit("9")],
        vec![
            bot.button(lang, "🗑", "login_key_del", Color::Red),
            digit("0"),
            bot.button(lang, "✅", "login_key_ok", Color::Green),
        ],
        vec![bot.button(lang, "❌", "login_key_cancel", Color::Red)],
    ])
}

fn share_phone_keyboard(label: &str) -> ReplyMarkup {
    ReplyMarkup {
        raw: tl::enums::ReplyMarkup::ReplyKeyboardMarkup(tl::types::ReplyKeyboardMarkup {
            resize: true,
            single_use: true,
            selective: false,
            persistent: false,
            rows: vec![
                tl::types::KeyboardButtonRow {
                    buttons: vec![
                        tl::types::KeyboardButtonRequestPhone {
                            style: None,
                            text: label.to_string(),
                        }
                        .into(),
                    ],
                }
                .into(),
            ],
            placeholder: None,
        }),
    }
}

/// `/login`, or the link-an-account button (no message to reply to).
pub async fn login(bot: &Arc<Bot>, message: Option<&Message>, who: &Who) -> Result<()> {
    let lang = bot.lang(who.id).await;
    if load(bot, who.id).await?.is_some() {
        let text = bot.t(&lang, "accounts.login.already_running", &[]);
        let markup = Some(cancel_only(bot, &lang));
        match message {
            Some(message) => {
                bot.reply(message, &text, markup).await?;
            }
            None => {
                bot.send_to(who.id, &text, markup).await?;
            }
        }
        return Ok(());
    }
    clear(bot, who.id).await?;
    // The session row references the user, so the user must exist even if /start never ran.
    let profile = users::Profile {
        user_id: who.id,
        first_name: &who.first_name,
        last_name: who.last_name.as_deref(),
        username: who.username.as_deref(),
    };
    users::register(
        &bot.pg,
        &bot.locales,
        &profile,
        who.lang_code.as_deref(),
        None,
    )
    .await?;

    let phone: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT phone FROM account_sessions WHERE kind = 'linked' AND owner_id = $1",
    )
    .bind(who.id)
    .fetch_optional(&bot.pg)
    .await?;
    if let Some((phone,)) = phone {
        let phone = phone.unwrap_or_default();
        bot.send_to(
            who.id,
            &bot.t(
                &lang,
                "accounts.login.already_connected",
                &[("phone_number", &phone)],
            ),
            None,
        )
        .await?;
        return Ok(());
    }
    let mut login = Login::new("awaiting_account_type", "");
    save(bot, who.id, &login).await?;
    login.prompt_id = bot
        .send_to(
            who.id,
            &bot.t(&lang, "accounts.login.choose_account", &[]),
            Some(account_type_keyboard(bot, &lang)),
        )
        .await?
        .id();
    save(bot, who.id, &login).await?;
    Ok(())
}

pub async fn logout(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let phone: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT phone FROM account_sessions WHERE kind = 'linked' AND owner_id = $1",
    )
    .bind(who.id)
    .fetch_optional(&bot.pg)
    .await?;
    let Some((phone,)) = phone else {
        bot.reply(
            message,
            &bot.t(&lang, "accounts.logout.not_connected", &[]),
            None,
        )
        .await?;
        return Ok(());
    };
    let markup = keyboard(&[
        vec![Button::callback(
            &bot.t(&lang, "buttons.confirm", &[]),
            "confirm_logout",
            Color::Green,
        )],
        vec![bot.button(&lang, "❌", "cancel_logout", Color::Red)],
    ]);
    let text = bot.t(
        &lang,
        "accounts.logout.confirm",
        &[("phone_number", &phone.unwrap_or_default())],
    );
    bot.reply(message, &text, Some(markup)).await?;
    Ok(())
}

/// Forgets the linked account and the monitor that depended on it.
pub async fn remove_linked_account(bot: &Bot, user_id: i64) -> Result<()> {
    sessions::remove(&bot.pg, Kind::Linked, &user_id.to_string()).await?;
    sqlx::query(
        "UPDATE users SET monitor_target_id = NULL, monitor_flags = NULL WHERE user_id = $1",
    )
    .bind(user_id)
    .execute(&bot.pg)
    .await?;
    Ok(())
}

pub async fn on_contact(
    bot: &Arc<Bot>,
    message: &Message,
    who: &Who,
    contact: &tl::types::MessageMediaContact,
) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let Some(login) = load(bot, who.id).await? else {
        bot.reply(
            message,
            &bot.t(&lang, "common.state_expired", &[]),
            Some(ReplyMarkup::hide()),
        )
        .await?;
        return Ok(());
    };
    if login.step != "awaiting_contact" {
        return Ok(());
    }
    // The share button stays on screen so the user can press it right away.
    if contact.user_id != 0 && contact.user_id != who.id {
        bot.reply(
            message,
            &bot.t(&lang, "accounts.login.contact_mismatch", &[]),
            None,
        )
        .await?;
        return Ok(());
    }
    if !claim_step(bot, who.id, "awaiting_contact", "sending_code").await? {
        return Ok(());
    }
    begin(bot, who, &contact.phone_number, login).await
}

/// A plain text message while a login is open. True when it was consumed.
pub async fn on_text(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<bool> {
    let Some(login) = load(bot, who.id).await? else {
        return Ok(false);
    };
    let lang = bot.lang(who.id).await;
    match login.step.as_str() {
        "awaiting_manual_phone" => {
            if claim_step(bot, who.id, "awaiting_manual_phone", "sending_code").await? {
                begin(bot, who, message.text(), login).await?;
            }
        }
        "awaiting_2fa" => password_step(bot, message, who, login).await?,
        step
        @ ("awaiting_contact" | "awaiting_account_type" | "sending_code" | "awaiting_code") => {
            bot.reply(
                message,
                &bot.t(&lang, &format!("accounts.login.hint_{step}"), &[]),
                Some(cancel_only(bot, &lang)),
            )
            .await?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn connect_for(login: &Login, api_id: u32) -> (telegram::Connection, Arc<MemorySession>) {
    match PyrogramSession::decode(&login.temp_session) {
        Ok(session) => telegram::connect(&session),
        Err(_) => {
            let memory = Arc::new(MemorySession::from(SessionData::default()));
            (telegram::connect_with(memory.clone(), api_id), memory)
        }
    }
}

fn error_text(error: &InvocationError) -> String {
    error.to_string().to_uppercase()
}

enum Sent {
    Code { hash: String, session: String },
    Failed(&'static str),
    Flood(u32),
}

async fn send_code(bot: &Bot, phone: &str) -> Sent {
    let api_id = bot.cfg.api_id as u32;
    let (connection, memory) = connect_for(&Login::default(), api_id);
    let client = connection.client;
    let request = tl::functions::auth::SendCode {
        phone_number: phone.to_string(),
        api_id: bot.cfg.api_id,
        api_hash: bot.cfg.api_hash.clone(),
        settings: tl::types::CodeSettings {
            allow_flashcall: false,
            current_number: false,
            allow_app_hash: false,
            allow_missed_call: false,
            allow_firebase: false,
            logout_tokens: None,
            token: None,
            app_sandbox: None,
            unknown_number: false,
        }
        .into(),
    };
    let mut result = client.invoke(&request).await;
    if let Err(InvocationError::Rpc(rpc)) = &result
        && rpc.code == 303
        && let Some(dc) = rpc.value
    {
        use grammers_client::session::Session;
        let _ = memory.set_home_dc_id(dc as i32).await;
        result = client.invoke(&request).await;
    }
    let outcome = match result {
        Ok(tl::enums::auth::SentCode::Code(code)) => {
            let via = format!("{:?}", code.r#type);
            let via = via.split('(').next().unwrap_or("");
            let next = code
                .next_type
                .as_ref()
                .map(|t| format!("{t:?}"))
                .unwrap_or_default();
            tracing::info!(via, next, "login code requested");
            match telegram::export(&memory, api_id, 0, false) {
                Some(session) => Sent::Code {
                    hash: code.phone_code_hash,
                    session: session.encode(),
                },
                None => Sent::Failed("accounts.login.failed"),
            }
        }
        Ok(other) => {
            let kind = format!("{other:?}");
            tracing::warn!(
                answer = kind.split('(').next().unwrap_or(""),
                "login code was not sent"
            );
            Sent::Failed("accounts.login.failed")
        }
        Err(InvocationError::Rpc(rpc)) if rpc.name == "FLOOD_WAIT" => {
            Sent::Flood(rpc.value.unwrap_or(30))
        }
        Err(error) => {
            let text = error_text(&error);
            if FLOOD_MARKERS.iter().any(|m| text.contains(m)) {
                tracing::info!(%error, "login is rate limited");
                Sent::Failed("accounts.login.too_many_attempts")
            } else if text.contains("PHONE_NUMBER_INVALID") {
                Sent::Failed("accounts.login.invalid_phone")
            } else if text.contains("PHONE_NUMBER_BANNED") {
                Sent::Failed("accounts.login.phone_banned")
            } else {
                tracing::warn!(%error, "could not send the login code");
                Sent::Failed("accounts.login.failed")
            }
        }
    };
    client.disconnect();
    outcome
}

async fn begin(bot: &Arc<Bot>, who: &Who, raw_phone: &str, mut login: Login) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let phone = phone::normalize(raw_phone);
    if !phone::is_valid_international(&phone) {
        login.step = "awaiting_manual_phone".into();
        login.prompt_id = show(
            bot,
            who,
            &login,
            &bot.t(&lang, "accounts.login.invalid_phone", &[]),
            Some(cancel_only(bot, &lang)),
        )
        .await?;
        save(bot, who.id, &login).await?;
        return Ok(());
    }
    let received = bot.t(&lang, "accounts.login.contact_received", &[]);
    if login.account_type == "current" {
        // Only a new message can take the share-phone keyboard away; the login continues in it.
        login.prompt_id = bot
            .send_to(who.id, &received, Some(ReplyMarkup::hide()))
            .await?
            .id();
    } else {
        login.prompt_id = show(bot, who, &login, &received, None).await?;
    }
    let sent = send_code(bot, &phone).await;
    if login.account_type == "current" {
        // Telegram refuses edits to a message that removed a keyboard, so it gives way to the next one.
        if let Some(peer) = bot.user_ref(who.id).await {
            bot.client
                .delete_messages(peer, &[login.prompt_id])
                .await
                .ok();
        }
        login.prompt_id = 0;
    }
    match sent {
        Sent::Code { hash, session } => {
            login.step = "awaiting_code".into();
            login.phone = phone;
            login.phone_code_hash = hash;
            login.temp_session = session;
            login.entered_code.clear();
            save(bot, who.id, &login).await?;
            let shown = show(
                bot,
                who,
                &login,
                &bot.t(&lang, "accounts.login.code_prompt", &[]),
                Some(code_keypad(bot, &lang)),
            )
            .await?;
            if shown != login.prompt_id {
                login.prompt_id = shown;
                save(bot, who.id, &login).await?;
            }
        }
        Sent::Flood(seconds) => {
            clear(bot, who.id).await?;
            let text = bot.t(
                &lang,
                "accounts.login.flood_wait",
                &[("seconds", &seconds.to_string())],
            );
            show(bot, who, &login, &text, None).await?;
        }
        Sent::Failed(key) => {
            clear(bot, who.id).await?;
            show(bot, who, &login, &bot.t(&lang, key, &[]), None).await?;
        }
    }
    Ok(())
}

async fn finish(
    bot: &Bot,
    who: &Who,
    login: &Login,
    memory: &MemorySession,
    telegram_id: u64,
) -> Result<bool> {
    let Some(session) = telegram::export(memory, bot.cfg.api_id as u32, telegram_id, false) else {
        return Ok(false);
    };
    let label = who.id.to_string();
    let account = Account {
        kind: Kind::Linked,
        label: &label,
        owner_id: Some(who.id),
        phone: Some(&login.phone),
    };
    sessions::put(&bot.pg, &bot.cfg.session_key, &account, &session).await?;
    clear(bot, who.id).await?;
    Ok(true)
}

async fn password_step(bot: &Arc<Bot>, message: &Message, who: &Who, login: Login) -> Result<()> {
    let lang = bot.lang(who.id).await;
    if login.temp_session.is_empty() {
        clear(bot, who.id).await?;
        bot.reply(message, &bot.t(&lang, "common.state_expired", &[]), None)
            .await?;
        return Ok(());
    }
    let password = message.text().trim().to_string();
    // The password must not stay visible in the chat.
    message.delete().await.ok();
    show(
        bot,
        who,
        &login,
        &bot.t(&lang, "accounts.login.processing", &[]),
        None,
    )
    .await?;

    let (connection, memory) = connect_for(&login, bot.cfg.api_id as u32);
    let client = connection.client;
    let outcome: Result<u64, Option<&'static str>> = async {
        let info = client
            .invoke(&tl::functions::account::GetPassword {})
            .await
            .map_err(|_| Some("accounts.login.failed"))?;
        let token = PasswordToken::new(info.into());
        match client.check_password(token, password.as_bytes()).await {
            Ok(user) => Ok(user.id().bare_id().unwrap_or(0) as u64),
            Err(SignInError::InvalidPassword(_)) => Err(None),
            Err(error) => {
                tracing::warn!(user = who.id, %error, "2FA verification failed");
                Err(Some("accounts.login.failed"))
            }
        }
    }
    .await;
    client.disconnect();

    match outcome {
        Ok(telegram_id) => {
            let key = if finish(bot, who, &login, &memory, telegram_id).await? {
                "accounts.login.completed"
            } else {
                "accounts.login.failed"
            };
            show(bot, who, &login, &bot.t(&lang, key, &[]), None).await?;
        }
        Err(None) => {
            save(bot, who.id, &login).await?;
            show(
                bot,
                who,
                &login,
                &bot.t(&lang, "accounts.login.invalid_password", &[]),
                Some(cancel_only(bot, &lang)),
            )
            .await?;
        }
        Err(Some(key)) => {
            clear(bot, who.id).await?;
            show(bot, who, &login, &bot.t(&lang, key, &[]), None).await?;
        }
    }
    Ok(())
}

/// Edits the login's message in place; sends a new one only when there is none to edit.
/// Puts `text` in the login's one message, or in a new one when there is none (or it cannot be edited).
/// Returns the id of the message that now shows the login.
async fn show(
    bot: &Bot,
    who: &Who,
    login: &Login,
    text: &str,
    markup: Option<ReplyMarkup>,
) -> Result<i32> {
    let again = || markup.as_ref().map(|m| ReplyMarkup { raw: m.raw.clone() });
    if login.prompt_id != 0
        && let Some(peer) = bot.user_ref(who.id).await
    {
        match bot
            .client
            .edit_message(peer, login.prompt_id, html(text, again()))
            .await
        {
            Ok(()) => return Ok(login.prompt_id),
            Err(error) if error.is("MESSAGE_NOT_MODIFIED") => return Ok(login.prompt_id),
            Err(error) => {
                tracing::info!(%error, "login message could not be edited, sending a new one")
            }
        }
    }
    Ok(bot.send_to(who.id, text, markup).await?.id())
}

/// The id of the message the button sits on, without fetching it.
fn message_id(query: &CallbackQuery) -> i32 {
    match &query.raw {
        tl::enums::Update::BotCallbackQuery(update) => update.msg_id,
        _ => 0,
    }
}

async fn expired(bot: &Bot, query: &CallbackQuery, lang: &str) {
    let _ = query
        .answer()
        .alert(bot.t(lang, "common.state_expired_popup", &[]))
        .send()
        .await;
}

/// All `login_*`, `cancel_login`, `confirm_logout` and `cancel_logout` buttons.
pub async fn on_callback(
    bot: &Arc<Bot>,
    query: &CallbackQuery,
    who: &Who,
    data: &str,
) -> Result<()> {
    let lang = bot.lang(who.id).await;
    match data {
        "login_account_current" | "login_account_other" => {
            let ready = load(bot, who.id)
                .await?
                .is_some_and(|l| l.step == "awaiting_account_type");
            if !ready {
                clear(bot, who.id).await?;
                expired(bot, query, &lang).await;
                return Ok(());
            }
            query.answer().send().await.ok();
            if data == "login_account_current" {
                save(bot, who.id, &Login::new("awaiting_contact", "current")).await?;
                if let Ok(m) = query.load_message().await {
                    m.delete().await.ok();
                }
                let label = bot.t(&lang, "accounts.login.buttons.share_phone", &[]);
                bot.send_to(
                    who.id,
                    &bot.t(&lang, "accounts.login.share_contact", &[]),
                    Some(share_phone_keyboard(&label)),
                )
                .await?;
            } else {
                let mut login = Login::new("awaiting_manual_phone", "other");
                login.prompt_id = message_id(query);
                save(bot, who.id, &login).await?;
                query
                    .load_message()
                    .await?
                    .edit(html(
                        &bot.t(&lang, "accounts.login.enter_phone", &[]),
                        Some(cancel_only(bot, &lang)),
                    ))
                    .await?;
            }
        }
        "cancel_login" => {
            let step = load(bot, who.id).await?.map(|l| l.step);
            clear(bot, who.id).await?;
            query.answer().send().await.ok();
            if let Ok(m) = query.load_message().await {
                m.edit(html(&bot.t(&lang, "accounts.login.cancelled", &[]), None))
                    .await
                    .ok();
            }
            // The share-phone keyboard goes away only with a new message.
            if step.as_deref() == Some("awaiting_contact") {
                bot.send_to(
                    who.id,
                    &bot.t(&lang, "accounts.login.restart_hint", &[]),
                    Some(ReplyMarkup::hide()),
                )
                .await?;
            }
        }
        "confirm_logout" => {
            query.answer().send().await.ok();
            remove_linked_account(bot, who.id).await?;
            query
                .load_message()
                .await?
                .edit(html(&bot.t(&lang, "accounts.logout.completed", &[]), None))
                .await?;
        }
        "cancel_logout" => {
            query.answer().send().await.ok();
            if let Ok(m) = query.load_message().await {
                m.delete().await.ok();
            }
        }
        _ => match data.strip_prefix("login_key_") {
            Some(action) => keypad(bot, query, who, &lang, action).await?,
            None => {
                query.answer().send().await.ok();
            }
        },
    }
    Ok(())
}

async fn keypad(
    bot: &Arc<Bot>,
    query: &CallbackQuery,
    who: &Who,
    lang: &str,
    action: &str,
) -> Result<()> {
    let Some(mut login) = load(bot, who.id)
        .await?
        .filter(|l| l.step == "awaiting_code")
    else {
        clear(bot, who.id).await?;
        expired(bot, query, lang).await;
        return Ok(());
    };
    let message = query.load_message().await?;
    match action {
        "cancel" => {
            clear(bot, who.id).await?;
            query.answer().send().await.ok();
            message
                .edit(html(&bot.t(lang, "accounts.login.cancelled", &[]), None))
                .await?;
            return Ok(());
        }
        "del" => {
            login.entered_code.pop();
        }
        "ok" => return submit_code(bot, query, who, lang, login).await,
        digit
            if digit.chars().all(|c| c.is_ascii_digit())
                && !digit.is_empty()
                && login.entered_code.len() < 10 =>
        {
            login.entered_code.push_str(digit);
        }
        _ => {}
    }
    save(bot, who.id, &login).await?;
    query.answer().send().await.ok();
    let shown = if login.entered_code.is_empty() {
        "—"
    } else {
        login.entered_code.as_str()
    };
    let text = bot.t(
        lang,
        "accounts.login.code_prompt_with_value",
        &[("code", shown)],
    );
    match message
        .edit(html(&text, Some(code_keypad(bot, lang))))
        .await
    {
        Ok(()) => {}
        Err(error) if error.is("MESSAGE_NOT_MODIFIED") => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

async fn submit_code(
    bot: &Arc<Bot>,
    query: &CallbackQuery,
    who: &Who,
    lang: &str,
    mut login: Login,
) -> Result<()> {
    if login.entered_code.is_empty() {
        query
            .answer()
            .alert(bot.t(lang, "accounts.login.code_empty_popup", &[]))
            .send()
            .await
            .ok();
        return Ok(());
    }
    // Signing in takes seconds; the button must not spin meanwhile.
    query.answer().send().await.ok();
    let message = query.load_message().await?;
    message
        .edit(html(&bot.t(lang, "accounts.login.processing", &[]), None))
        .await?;

    let (connection, memory) = connect_for(&login, bot.cfg.api_id as u32);
    let client = connection.client;
    let signed = client
        .invoke(&tl::functions::auth::SignIn {
            phone_number: login.phone.clone(),
            phone_code_hash: login.phone_code_hash.clone(),
            phone_code: Some(login.entered_code.clone()),
            email_verification: None,
        })
        .await;

    enum Next {
        Done(u64),
        Password(Option<String>),
        BadCode,
        Failed(&'static str),
    }
    let next = match signed {
        Ok(tl::enums::auth::Authorization::Authorization(auth)) => match auth.user {
            tl::enums::User::User(user) => Next::Done(user.id as u64),
            tl::enums::User::Empty(_) => Next::Failed("accounts.login.failed"),
        },
        Ok(_) => Next::Failed("accounts.login.failed"),
        Err(error) if error.is("SESSION_PASSWORD_NEEDED") => {
            let hint = match client.invoke(&tl::functions::account::GetPassword {}).await {
                Ok(info) => {
                    let info: tl::types::account::Password = info.into();
                    info.hint
                }
                Err(_) => None,
            };
            Next::Password(hint)
        }
        Err(error) if error.is("PHONE_CODE_*") => Next::BadCode,
        Err(error) => {
            let text = error_text(&error);
            if FLOOD_MARKERS.iter().any(|m| text.contains(m)) {
                tracing::info!(%error, "login is rate limited");
                Next::Failed("accounts.login.too_many_attempts")
            } else {
                tracing::warn!(user = who.id, %error, "login failed");
                Next::Failed("accounts.login.failed")
            }
        }
    };
    client.disconnect();

    match next {
        Next::Done(telegram_id) => {
            if finish(bot, who, &login, &memory, telegram_id).await? {
                message
                    .edit(html(&bot.t(lang, "accounts.login.completed", &[]), None))
                    .await?;
            } else {
                message
                    .edit(html(&bot.t(lang, "accounts.login.failed", &[]), None))
                    .await?;
            }
        }
        Next::Password(hint) => {
            // Signing in changed nothing on the key, but the home DC may have moved: keep the newest.
            if let Some(updated) = telegram::export(&memory, bot.cfg.api_id as u32, 0, false) {
                login.temp_session = updated.encode();
            }
            login.step = "awaiting_2fa".into();
            login.prompt_id = message.id();
            save(bot, who.id, &login).await?;
            let na = bot.t(lang, "common.not_available", &[]);
            let hint = hint.filter(|h| !h.is_empty()).unwrap_or(na);
            let text = bot.t(
                lang,
                "accounts.login.password_prompt",
                &[("hint", &super::send::escape(&hint))],
            );
            message
                .edit(html(&text, Some(cancel_only(bot, lang))))
                .await?;
        }
        Next::BadCode => {
            login.entered_code.clear();
            save(bot, who.id, &login).await?;
            query
                .answer()
                .alert(bot.t(lang, "accounts.login.invalid_code_popup", &[]))
                .send()
                .await
                .ok();
            message
                .edit(html(
                    &bot.t(lang, "accounts.login.invalid_code", &[]),
                    Some(code_keypad(bot, lang)),
                ))
                .await?;
        }
        Next::Failed(key) => {
            clear(bot, who.id).await?;
            message.edit(html(&bot.t(lang, key, &[]), None)).await?;
        }
    }
    Ok(())
}
