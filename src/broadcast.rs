//! Broadcast helpers: who receives a message, the per-language JSON file, and the send loop.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, bail};
use grammers_client::{InvocationError, message::InputMessage};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{bot::Bot, tasks::status::Status};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Filters {
    pub only_users: Vec<i64>,
    pub exclude_users: Vec<i64>,
    pub exclude_premium: bool,
    pub only_premium: bool,
}

/// `-only <ids>` and `-rm <ids> [--prem]`, as `/bc` takes them.
pub fn parse_filters(arguments: &[&str]) -> Filters {
    let mut filters = Filters::default();
    let mut mode = "";
    for argument in arguments {
        match *argument {
            "/bc" | "." => continue,
            "-only" => mode = "only",
            "-rm" => mode = "remove",
            "--prem" if mode == "remove" => filters.exclude_premium = true,
            number if number.chars().all(|c| c.is_ascii_digit()) && !number.is_empty() => {
                let Ok(id) = number.parse::<i64>() else {
                    continue;
                };
                match mode {
                    "only" => filters.only_users.push(id),
                    "remove" => filters.exclude_users.push(id),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    filters
}

/// One text per language; `en` is required and nothing may exceed Telegram's 4096 characters.
pub fn normalize_payload(payload: &Value) -> Result<BTreeMap<String, String>> {
    let Some(object) = payload.as_object() else {
        bail!("JSON root must be an object")
    };
    let mut out = BTreeMap::new();
    for (code, value) in object {
        if code.starts_with('_') {
            continue;
        }
        let code = code.trim().replace('_', "-");
        let text = match value {
            Value::String(text) => text.clone(),
            Value::Object(map) => match map.get("text") {
                Some(Value::String(text)) => text.clone(),
                _ => continue,
            },
            _ => continue,
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        if text.chars().count() > 4096 {
            bail!("Message for {code} is longer than 4096 characters");
        }
        out.insert(code, text);
    }
    let english = out.get("en").or_else(|| out.get("EN")).cloned();
    let Some(english) = english else {
        bail!("English language en is required")
    };
    out.insert("en".into(), english);
    Ok(out)
}

/// The user's language, else its base language, else English.
pub fn select_text<'a>(language: Option<&str>, messages: &'a BTreeMap<String, String>) -> &'a str {
    let english = messages.get("en").map(String::as_str).unwrap_or("");
    let Some(code) = language
        .map(|l| l.trim().replace('_', "-"))
        .filter(|l| !l.is_empty())
    else {
        return english;
    };
    let base = code.split('-').next().unwrap_or(&code).to_string();
    let lowered: BTreeMap<String, &String> = messages
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v))
        .collect();
    for candidate in [
        code.clone(),
        code.to_lowercase(),
        base.clone(),
        base.to_lowercase(),
        "en".to_string(),
    ] {
        if let Some(text) = messages
            .get(&candidate)
            .or_else(|| lowered.get(&candidate.to_lowercase()).copied())
        {
            return text;
        }
    }
    english
}

struct Target {
    user_id: i64,
    language: Option<String>,
    active: bool,
}

async fn targets(bot: &Bot, filters: &Filters) -> Result<Vec<Target>> {
    type Row = (
        i64,
        Option<String>,
        Option<bool>,
        Option<bool>,
        Option<chrono::DateTime<chrono::Utc>>,
    );
    let rows: Vec<Row> =
        sqlx::query_as("SELECT user_id, language_code, is_active, is_premium, premium_expiry_date FROM users ORDER BY user_id").fetch_all(&bot.pg).await?;
    let only: HashSet<i64> = filters.only_users.iter().copied().collect();
    let excluded: HashSet<i64> = filters.exclude_users.iter().copied().collect();
    let now = chrono::Utc::now();
    let mut active = Vec::new();
    let mut inactive = Vec::new();
    for (user_id, language, is_active, premium, expiry) in rows {
        let premium_now = premium.unwrap_or(false) && expiry.is_none_or(|e| e > now);
        if (!only.is_empty() && !only.contains(&user_id))
            || excluded.contains(&user_id)
            || (filters.exclude_premium && premium_now)
            || (filters.only_premium && !premium_now)
        {
            continue;
        }
        let target = Target {
            user_id,
            language,
            active: is_active.unwrap_or(true),
        };
        if target.active {
            active.push(target)
        } else {
            inactive.push(target)
        }
    }
    active.extend(inactive);
    Ok(active)
}

async fn set_active(bot: &Bot, user_id: i64, active: bool) {
    let _ = sqlx::query("UPDATE users SET is_active = $2 WHERE user_id = $1")
        .bind(user_id)
        .bind(active)
        .execute(&bot.pg)
        .await;
}

fn dead_user(error: &InvocationError) -> bool {
    matches!(error, InvocationError::Rpc(rpc) if matches!(rpc.name.as_str(), "USER_IS_BLOCKED" | "USER_DEACTIVATED" | "PEER_ID_INVALID" | "INPUT_USER_DEACTIVATED"))
}

pub enum Payload {
    /// Copy this message (chat peer id, message id) to everyone.
    Copy {
        chat: i64,
        message_id: i32,
        pin: bool,
    },
    /// Send each user the text for their language.
    Localized(BTreeMap<String, String>),
}

/// Runs the broadcast, reporting on `status` every twenty users. Returns when finished or cancelled.
pub async fn run(
    bot: &Arc<Bot>,
    admin_id: i64,
    payload: Payload,
    filters: Filters,
    status: Arc<Status>,
    cancel: CancellationToken,
) {
    let lang = bot.lang(admin_id).await;
    let users = match targets(bot, &filters).await {
        Ok(users) => users,
        Err(error) => {
            tracing::error!(%error, "broadcast could not read the users");
            return;
        }
    };
    let total = users.len();
    let (mut processed, mut ok, mut failed) = (0usize, 0usize, 0usize);
    for target in users {
        if cancel.is_cancelled() {
            status
                .edit(
                    bot,
                    &bot.t(
                        &lang,
                        "broadcast.cancelled",
                        &[("processed", &processed.to_string())],
                    ),
                    None,
                )
                .await;
            return;
        }
        let Some(peer) = bot.user_ref(target.user_id).await else {
            failed += 1;
            processed += 1;
            continue;
        };
        loop {
            let sent = match &payload {
                Payload::Copy {
                    chat,
                    message_id,
                    pin,
                } => copy_one(bot, admin_id, *chat, *message_id, peer, *pin).await,
                Payload::Localized(messages) => {
                    let text = select_text(target.language.as_deref(), messages);
                    bot.client
                        .send_message(peer, InputMessage::new().html(text).link_preview(false))
                        .await
                        .map(|_| ())
                }
            };
            match sent {
                Ok(()) => {
                    ok += 1;
                    if !target.active {
                        set_active(bot, target.user_id, true).await;
                    }
                    break;
                }
                Err(InvocationError::Rpc(rpc)) if rpc.name.starts_with("FLOOD") => {
                    tokio::time::sleep(Duration::from_secs(u64::from(rpc.value.unwrap_or(30)) + 1))
                        .await;
                }
                Err(error) => {
                    if dead_user(&error) && target.active {
                        set_active(bot, target.user_id, false).await;
                    }
                    if !dead_user(&error) {
                        tracing::warn!(user = target.user_id, %error, "broadcast failed for a user");
                    }
                    failed += 1;
                    break;
                }
            }
        }
        processed += 1;
        if processed % 20 == 0 {
            let text = bot.t(
                &lang,
                "broadcast.progress",
                &[
                    ("total", &total.to_string()),
                    ("processed", &processed.to_string()),
                    ("successful", &ok.to_string()),
                    ("failed", &failed.to_string()),
                ],
            );
            status.edit(bot, &text, None).await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let text = bot.t(
        &lang,
        "broadcast.completed",
        &[
            ("total", &total.to_string()),
            ("successful", &ok.to_string()),
            ("failed", &failed.to_string()),
        ],
    );
    status.edit(bot, &text, None).await;
}

/// Copies one message to a user (a forward without the author line), pinning it when asked.
async fn copy_one(
    bot: &Bot,
    admin_id: i64,
    chat: i64,
    message_id: i32,
    to: grammers_client::session::types::PeerRef,
    pin: bool,
) -> Result<(), InvocationError> {
    use grammers_client::tl;
    let from = bot
        .user_ref(chat)
        .await
        .or(bot.user_ref(admin_id).await)
        .ok_or(InvocationError::Dropped)?;
    let random_id: i64 = rand::random();
    let request = tl::functions::messages::ForwardMessages {
        silent: false,
        background: false,
        with_my_score: false,
        drop_author: true,
        drop_media_captions: false,
        from_peer: (&from).into(),
        id: vec![message_id],
        random_id: vec![random_id],
        to_peer: (&to).into(),
        top_msg_id: None,
        reply_to: None,
        schedule_date: None,
        schedule_repeat_period: None,
        send_as: None,
        noforwards: false,
        quick_reply_shortcut: None,
        allow_paid_floodskip: false,
        effect: None,
        video_timestamp: None,
        allow_paid_stars: None,
        suggested_post: None,
    };
    let updates = bot.client.invoke(&request).await?;
    if pin {
        let sent_id = match &updates {
            tl::enums::Updates::Updates(u) => u.updates.iter().find_map(|x| match x {
                tl::enums::Update::MessageId(m) if m.random_id == random_id => Some(m.id),
                _ => None,
            }),
            tl::enums::Updates::Combined(u) => u.updates.iter().find_map(|x| match x {
                tl::enums::Update::MessageId(m) if m.random_id == random_id => Some(m.id),
                _ => None,
            }),
            _ => None,
        };
        if let Some(id) = sent_id {
            let _ = bot
                .client
                .invoke(&tl::functions::messages::UpdatePinnedMessage {
                    silent: false,
                    unpin: false,
                    pm_oneside: false,
                    peer: (&to).into(),
                    id,
                })
                .await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filters_follow_the_bc_syntax() {
        let f = parse_filters(&["/bc", "-only", "1", "2", "-rm", "3", "--prem"]);
        assert_eq!(
            (f.only_users, f.exclude_users, f.exclude_premium),
            (vec![1, 2], vec![3], true)
        );
        assert_eq!(parse_filters(&["/bc", "."]), Filters::default());
        assert!(
            !parse_filters(&["--prem"]).exclude_premium,
            "--prem only counts after -rm"
        );
    }

    #[test]
    fn payload_needs_english_and_short_texts() {
        let ok = normalize_payload(&json!({"_instructions": ["x"], "en": {"text": " Hi "}, "ar_EG": "مرحبا", "fr": {"text": ""}})).unwrap();
        assert_eq!(ok.get("en").map(String::as_str), Some("Hi"));
        assert!(ok.contains_key("ar-EG") && !ok.contains_key("fr"));
        assert!(normalize_payload(&json!({"ar": "x"})).is_err());
        assert!(normalize_payload(&json!([1])).is_err());
        assert!(normalize_payload(&json!({"en": "x".repeat(4097)})).is_err());
    }

    #[test]
    fn text_falls_back_to_base_language_then_english() {
        let messages = normalize_payload(&json!({"en": "E", "pt": "P", "ar": "A"})).unwrap();
        assert_eq!(select_text(Some("pt-BR"), &messages), "P");
        assert_eq!(select_text(Some("ar"), &messages), "A");
        assert_eq!(select_text(Some("xx"), &messages), "E");
        assert_eq!(select_text(None, &messages), "E");
    }
}
