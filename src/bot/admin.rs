//! The admin panel (`/admin`), the commands that stay beside it (`/show`, `/bc`, `/bcjson`) and who may use them.
//!
//! Owners (from `OWNER_IDS`) can do everything, including appointing admins. Admins can do everything but that.

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use grammers_client::{
    message::{InputMessage, ReplyMarkup},
    tl,
    update::{CallbackQuery, Message},
};
use serde_json::{Value, json};

use super::{Bot, Who, html};
use crate::{
    broadcast::{self, Filters, Payload},
    store::{admin as store, states},
    tasks::status::Status,
    telegram::keyboards::{Button, Color, keyboard},
};

const PANEL: &str = "admin_panel";
const TTL: i64 = 24 * 60 * 60;
const PICK_USERS_BUTTON: i32 = 7822;
const PICK_FSUB_BUTTON: i32 = 7823;
const MAX_DAYS: i64 = 36_500;

fn is_rtl(lang: &str) -> bool {
    matches!(
        lang.split('-').next().unwrap_or(lang),
        "ar" | "fa" | "he" | "ur"
    )
}

fn ids_text(ids: &[i64]) -> String {
    ids.iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn ids_of(state: &Value, key: &str) -> Vec<i64> {
    state[key]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// Reads `[1, 2]`, `1,2` or `1 2`; any non-positive number is refused.
pub fn parse_ids(text: &str) -> Option<Vec<i64>> {
    let cleaned = text.replace(['[', ']'], "").replace(' ', ",");
    let mut ids = Vec::new();
    for part in cleaned.split(',').filter(|p| !p.is_empty()) {
        let id = part.parse::<i64>().ok()?;
        if id <= 0 {
            return None;
        }
        ids.push(id);
    }
    Some(ids)
}

async fn staff_only(bot: &Bot, who: &Who) -> bool {
    bot.is_staff(who.id).await
}

pub async fn panel_command(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<()> {
    if !staff_only(bot, who).await {
        return Ok(());
    }
    states::delete(&bot.pg, who.id, PANEL).await?;
    render(bot, who.id, None).await?;
    let _ = message;
    Ok(())
}

async fn get_state(bot: &Bot, user_id: i64) -> Option<Value> {
    states::get::<Value>(&bot.pg, user_id, PANEL)
        .await
        .ok()
        .flatten()
}

async fn put_state(bot: &Bot, user_id: i64, state: &Value) -> Result<()> {
    Ok(states::set(&bot.pg, user_id, PANEL, state, TTL).await?)
}

fn blue(text: &str, data: &str) -> Button {
    Button::callback(text, data, Color::Blue)
}

fn green(text: &str, data: &str) -> Button {
    Button::callback(text, data, Color::Green)
}

fn red(text: &str, data: &str) -> Button {
    Button::callback(text, data, Color::Red)
}

/// Builds the panel text and keyboard for the user's current state.
async fn view(bot: &Bot, user_id: i64) -> (String, Vec<Vec<Button>>) {
    let lang = bot.lang(user_id).await;
    let t = |key: &str, args: &[(&str, &str)]| bot.t(&lang, key, args);
    let owner = bot.is_owner(user_id);
    let mut rows: Vec<Vec<Button>> = Vec::new();
    let text = match get_state(bot, user_id).await {
        None => {
            let title = t("panel.title", &[]);
            let mut items = vec![
                ("premium", t("panel.premium", &[])),
                ("gift", t("panel.gift", &[])),
                ("fsub", t("panel.fsub", &[])),
                ("broadcast", t("panel.broadcast", &[])),
                ("logs", t("commands.logs.description", &[])),
                ("users", t("panel.users", &[])),
                ("language", t("panel.language", &[])),
                ("gifts_0", t("panel.gifts", &[])),
            ];
            if owner {
                items.push(("admins", t("panel.admins", &[])));
            }
            for pair in items.chunks(2) {
                rows.push(
                    pair.iter()
                        .map(|(name, label)| blue(label, &format!("ad_{name}")))
                        .collect(),
                );
            }
            return (title, rows);
        }
        Some(state) => {
            let kind = state["kind"].as_str().unwrap_or("").to_string();
            let mut body = t(&format!("panel.{kind}"), &[]);
            match kind.as_str() {
                "premium" => {
                    let users = ids_of(&state, "users");
                    if users.is_empty() {
                        body.push_str(&format!("\n\n{}", t("panel.choose_user", &[])));
                    } else {
                        body.push_str(&format!(
                            "\n\n{}",
                            t("panel.selected_users", &[("users", &ids_text(&users))])
                        ));
                        for group in [[1, 3, 7], [30, 90, 365]] {
                            rows.push(
                                group
                                    .iter()
                                    .map(|d| green(&d.to_string(), &format!("ad_days_{d}")))
                                    .collect(),
                            );
                        }
                        rows.push(vec![
                            blue(&t("panel.custom", &[]), "ad_custom_days"),
                            red(&t("panel.remove_premium", &[]), "ad_remove_premium"),
                        ]);
                        let days = state["days"].as_i64().unwrap_or(0);
                        let remove = state["remove"].as_bool().unwrap_or(false);
                        if days > 0 || remove {
                            let line = if remove {
                                t("panel.remove_premium", &[])
                            } else {
                                t("panel.days", &[("days", &days.to_string())])
                            };
                            body.push_str(&format!("\n{line}"));
                            rows.push(vec![green(&t("buttons.confirm", &[]), "ad_confirm")]);
                        }
                    }
                }
                "gift" => {
                    for field in ["value", "link_days", "point_days", "limit"] {
                        body.push_str(&format!(
                            "\n{} ↣ {}",
                            t(&format!("panel.{field}"), &[]),
                            state[field].as_i64().unwrap_or(0)
                        ));
                        rows.push(vec![blue(
                            &t(&format!("panel.{field}"), &[]),
                            &format!("ad_field_{field}"),
                        )]);
                    }
                    for field in ["only_users", "exclude_users"] {
                        let ids = ids_of(&state, field);
                        if !ids.is_empty() {
                            body.push_str(&format!(
                                "\n{} ↣ {}",
                                t(&format!("panel.{field}"), &[]),
                                ids_text(&ids)
                            ));
                        }
                    }
                    rows.push(vec![
                        blue(&t("panel.only_users", &[]), "ad_custom_only_users"),
                        blue(&t("panel.exclude_users", &[]), "ad_custom_exclude_users"),
                    ]);
                    rows.push(vec![green(&t("buttons.confirm", &[]), "ad_confirm")]);
                }
                "fsub" => {
                    if let Some(channel) = state["channel_id"].as_i64() {
                        body.push_str(&format!(
                            "\n\n{}",
                            t(
                                "panel.selected_channel",
                                &[("channel_id", &channel.to_string())]
                            )
                        ));
                        rows.push(
                            [0, 100, 500, 1000]
                                .iter()
                                .map(|n| green(&n.to_string(), &format!("ad_limit_{n}")))
                                .collect(),
                        );
                        rows.push(vec![blue(&t("panel.custom", &[]), "ad_custom_limit")]);
                        body.push_str(&format!(
                            "\n{} ↣ {}",
                            t("panel.limit", &[]),
                            state["limit"].as_i64().unwrap_or(0)
                        ));
                        rows.push(vec![green(&t("buttons.confirm", &[]), "ad_confirm")]);
                    } else {
                        body.push_str(&format!("\n\n{}", t("flow.pick_channel", &[])));
                        for (channel_id, ..) in
                            store::fsub_channels(&bot.pg).await.unwrap_or_default()
                        {
                            rows.push(vec![red(
                                &channel_id.to_string(),
                                &format!("ad_remove_fsub_{channel_id}"),
                            )]);
                        }
                    }
                }
                "broadcast" => {
                    body.push_str(&format!("\n\n{}", t("panel.send_broadcast", &[])));
                    if state["source_id"].is_number() {
                        let chosen = state["audience"].as_str().unwrap_or("all");
                        for audience in ["all", "premium", "free"] {
                            let label = t(&format!("panel.audience_{audience}"), &[]);
                            rows.push(vec![if chosen == audience {
                                green(&label, &format!("ad_audience_{audience}"))
                            } else {
                                blue(&label, &format!("ad_audience_{audience}"))
                            }]);
                        }
                        rows.push(vec![green(&t("buttons.confirm", &[]), "ad_confirm")]);
                    }
                }
                "language" => body.push_str(&format!("\n\n{}", t("panel.send_language", &[]))),
                "admins" => {
                    let list = store::admins(&bot.pg).await.unwrap_or_default();
                    body.push_str(&format!("\n\n{}", t("panel.send_admin", &[])));
                    if list.is_empty() {
                        body.push_str(&format!("\n\n{}", t("panel.no_admins", &[])));
                    }
                    for id in list {
                        rows.push(vec![red(&id.to_string(), &format!("ad_rmadmin_{id}"))]);
                    }
                }
                _ => {}
            }
            if state["waiting"].is_string() {
                body.push_str(&format!("\n\n{}", t("panel.enter_value", &[])));
            }
            body
        }
    };
    rows.push(vec![red(&t("buttons.back", &[]), "ad_home")]);
    let text = if is_rtl(&lang) {
        text.replace('↣', "↢")
    } else {
        text
    };
    (text, rows)
}

/// Draws the panel: edits `menu` when given, else sends a new message and remembers its id.
async fn render(bot: &Arc<Bot>, user_id: i64, menu: Option<i32>) -> Result<()> {
    render_with(bot, user_id, menu, None).await
}

/// The panel, with the result of the last action (if any) on top instead of in a message of its own.
async fn render_with(
    bot: &Arc<Bot>,
    user_id: i64,
    menu: Option<i32>,
    notice: Option<&str>,
) -> Result<()> {
    let (text, rows) = view(bot, user_id).await;
    let text = match notice {
        Some(notice) => format!("{notice}\n\n{text}"),
        None => text,
    };
    let markup = keyboard(&rows);
    let Some(peer) = bot.user_ref(user_id).await else {
        return Ok(());
    };
    let menu_id = match menu {
        Some(id) => {
            match bot
                .client
                .edit_message(peer, id, html(&text, Some(markup)))
                .await
            {
                Ok(()) => {}
                Err(error) if error.is("MESSAGE_NOT_MODIFIED") => {}
                Err(error) => return Err(error.into()),
            }
            id
        }
        None => bot
            .client
            .send_message(peer, html(&text, Some(markup)))
            .await?
            .id(),
    };
    if let Some(mut state) = get_state(bot, user_id).await {
        state["menu_id"] = json!(menu_id);
        put_state(bot, user_id, &state).await?;
    }
    Ok(())
}

fn request_users_keyboard(label: &str) -> ReplyMarkup {
    let button: tl::enums::KeyboardButton = tl::types::KeyboardButtonRequestPeer {
        style: Some(
            tl::types::KeyboardButtonStyle {
                bg_primary: true,
                bg_danger: false,
                bg_success: false,
                icon: None,
            }
            .into(),
        ),
        text: label.to_string(),
        button_id: PICK_USERS_BUTTON,
        peer_type: tl::types::RequestPeerTypeUser {
            bot: Some(false),
            premium: None,
        }
        .into(),
        max_quantity: 10,
    }
    .into();
    one_button(button)
}

fn request_channel_keyboard(label: &str) -> ReplyMarkup {
    let button: tl::enums::KeyboardButton = tl::types::KeyboardButtonRequestPeer {
        style: Some(
            tl::types::KeyboardButtonStyle {
                bg_primary: true,
                bg_danger: false,
                bg_success: false,
                icon: None,
            }
            .into(),
        ),
        text: label.to_string(),
        button_id: PICK_FSUB_BUTTON,
        peer_type: tl::types::RequestPeerTypeBroadcast {
            creator: false,
            has_username: None,
            user_admin_rights: None,
            bot_admin_rights: None,
        }
        .into(),
        max_quantity: 1,
    }
    .into();
    one_button(button)
}

fn one_button(button: tl::enums::KeyboardButton) -> ReplyMarkup {
    ReplyMarkup {
        raw: tl::enums::ReplyMarkup::ReplyKeyboardMarkup(tl::types::ReplyKeyboardMarkup {
            resize: true,
            single_use: true,
            selective: false,
            persistent: false,
            rows: vec![
                tl::types::KeyboardButtonRow {
                    buttons: vec![button],
                }
                .into(),
            ],
            placeholder: None,
        }),
    }
}

fn gift_link(bot: &Bot, pid: &str) -> String {
    format!("https://t.me/{}?start={pid}", bot.cfg.bot_username)
}

/// Gift list page: twenty gifts, each a remove button.
async fn gifts_page(bot: &Arc<Bot>, query: &CallbackQuery, user_id: i64, page: i64) -> Result<()> {
    let lang = bot.lang(user_id).await;
    let entries = store::active_gifts(&bot.pg, 21, page * 20).await?;
    let mut rows: Vec<Vec<Button>> = entries
        .iter()
        .take(20)
        .map(|(pid, value, _, limit, used, _)| {
            let cap =
                limit.map_or_else(|| bot.t(&lang, "common.unlimited", &[]), |l| l.to_string());
            vec![red(
                &format!("{pid} {value} {}/{cap}", used.unwrap_or(0)),
                &format!("ad_giftremove_{pid}"),
            )]
        })
        .collect();
    let mut nav = Vec::new();
    if page > 0 {
        nav.push(blue(&page.to_string(), &format!("ad_gifts_{}", page - 1)));
    }
    if entries.len() > 20 {
        nav.push(blue(
            &(page + 2).to_string(),
            &format!("ad_gifts_{}", page + 1),
        ));
    }
    if !nav.is_empty() {
        rows.push(nav);
    }
    rows.push(vec![red(&bot.t(&lang, "buttons.back", &[]), "ad_home")]);
    let title = if entries.is_empty() {
        bot.t(&lang, "admin.gifts.empty", &[])
    } else {
        bot.t(&lang, "admin.gifts.list_title", &[])
    };
    query
        .load_message()
        .await?
        .edit(html(&title, Some(keyboard(&rows))))
        .await
        .ok();
    Ok(())
}

pub async fn callback(bot: &Arc<Bot>, query: &CallbackQuery, who: &Who, data: &str) -> Result<()> {
    let lang = bot.lang(who.id).await;
    if !staff_only(bot, who).await {
        query.answer().send().await.ok();
        return Ok(());
    }
    let data = data.strip_prefix("ad_").unwrap_or(data);
    let message = query.load_message().await?;
    if data == "home" {
        states::delete(&bot.pg, who.id, PANEL).await?;
        render(bot, who.id, Some(message.id())).await?;
        query.answer().send().await.ok();
        return Ok(());
    }
    if let Some(page) = data
        .strip_prefix("gifts_")
        .and_then(|p| p.parse::<i64>().ok())
    {
        gifts_page(bot, query, who.id, page).await?;
        query.answer().send().await.ok();
        return Ok(());
    }
    if let Some(pid) = data
        .strip_prefix("giftremove_")
        .filter(|p| p.starts_with("gift_") && p.len() == 21)
    {
        gift_remove_prompt(bot, query, &lang, pid).await?;
        query.answer().send().await.ok();
        return Ok(());
    }
    if matches!(
        data,
        "premium" | "gift" | "fsub" | "broadcast" | "language" | "admins"
    ) {
        if data == "admins" && !bot.is_owner(who.id) {
            query
                .answer()
                .alert(bot.t(&lang, "panel.owner_only", &[]))
                .send()
                .await
                .ok();
            return Ok(());
        }
        let mut state = json!({"kind": data});
        match data {
            "gift" => {
                state = json!({"kind": "gift", "value": 50, "link_days": 1, "point_days": 0, "limit": 1})
            }
            "premium" => state["waiting"] = json!("users"),
            "admins" => state["waiting"] = json!("admin"),
            _ => {}
        }
        put_state(bot, who.id, &state).await?;
        render(bot, who.id, Some(message.id())).await?;
        if data == "premium" {
            let label = bot.t(&lang, "panel.choose_user", &[]);
            bot.send_to(who.id, &label, Some(request_users_keyboard(&label)))
                .await?;
        } else if data == "fsub" {
            let label = bot.t(&lang, "flow.pick_channel", &[]);
            bot.send_to(who.id, &label, Some(request_channel_keyboard(&label)))
                .await?;
        }
        query.answer().send().await.ok();
        return Ok(());
    }
    if data == "logs" {
        send_logs(bot, who.id, &lang).await?;
        query.answer().send().await.ok();
        return Ok(());
    }
    if data == "users" {
        send_users_csv(bot, who.id, &lang).await?;
        query.answer().send().await.ok();
        return Ok(());
    }
    let Some(mut state) = get_state(bot, who.id).await.filter(|s| {
        s["menu_id"].as_i64() == Some(i64::from(message.id()))
            && !s["processing"].as_bool().unwrap_or(false)
    }) else {
        query
            .answer()
            .alert(bot.t(&lang, "common.state_expired_popup", &[]))
            .send()
            .await
            .ok();
        return Ok(());
    };
    let kind = state["kind"].as_str().unwrap_or("").to_string();
    let mut notice: Option<String> = None;
    if let Some(id) = data
        .strip_prefix("rmadmin_")
        .and_then(|i| i.parse::<i64>().ok())
    {
        if !bot.is_owner(who.id) {
            query
                .answer()
                .alert(bot.t(&lang, "panel.owner_only", &[]))
                .send()
                .await
                .ok();
            return Ok(());
        }
        store::remove_admin(&bot.pg, id).await?;
        notice = Some(bot.t(
            &lang,
            "panel.admin_removed",
            &[("user_id", &id.to_string())],
        ));
    } else if let Some(id) = data
        .strip_prefix("remove_fsub_")
        .and_then(|i| i.parse::<i64>().ok())
        .filter(|_| kind == "fsub")
    {
        let removed = store::remove_fsub(&bot.pg, id).await?;
        let key = if removed {
            "admin.force_subscription.removed"
        } else {
            "admin.force_subscription.not_found"
        };
        notice = Some(bot.t(&lang, key, &[]));
    } else if let Some(field) = data.strip_prefix("field_").filter(|_| kind == "gift") {
        if !["value", "link_days", "point_days", "limit"].contains(&field) {
            return Ok(());
        }
        state["field"] = json!(field);
        put_state(bot, who.id, &state).await?;
        let values: &[i64] = match field {
            "value" => &[50, 100, 200, 500],
            "limit" => &[1, 10, 50, 100],
            _ => &[0, 1, 3, 7, 30],
        };
        let rows = vec![
            values
                .iter()
                .map(|n| green(&n.to_string(), &format!("ad_set_{n}")))
                .collect::<Vec<_>>(),
            vec![blue(
                &bot.t(&lang, "panel.custom", &[]),
                &format!("ad_custom_{field}"),
            )],
            vec![red(&bot.t(&lang, "buttons.back", &[]), "ad_summary")],
        ];
        message
            .edit(html(
                &bot.t(&lang, &format!("panel.{field}"), &[]),
                Some(keyboard(&rows)),
            ))
            .await
            .ok();
        query.answer().send().await.ok();
        return Ok(());
    } else if let Some(field) = data.strip_prefix("custom_") {
        if ![
            "days",
            "value",
            "link_days",
            "point_days",
            "limit",
            "only_users",
            "exclude_users",
        ]
        .contains(&field)
        {
            return Ok(());
        }
        state["waiting"] = json!(field);
        put_state(bot, who.id, &state).await?;
    } else if let Some(n) = data
        .strip_prefix("set_")
        .and_then(|n| n.parse::<i64>().ok())
        .filter(|_| kind == "gift")
    {
        if let Some(field) = state["field"].as_str().map(str::to_string) {
            state[field] = json!(n);
            state.as_object_mut().map(|o| o.remove("waiting"));
            put_state(bot, who.id, &state).await?;
        }
    } else if let Some(d) = data
        .strip_prefix("days_")
        .and_then(|n| n.parse::<i64>().ok())
        .filter(|_| kind == "premium")
    {
        state["remove"] = json!(false);
        state["days"] = json!(d);
        put_state(bot, who.id, &state).await?;
    } else if let Some(n) = data
        .strip_prefix("limit_")
        .and_then(|n| n.parse::<i64>().ok())
        .filter(|_| kind == "fsub")
    {
        state["limit"] = json!(n);
        put_state(bot, who.id, &state).await?;
    } else if let Some(a) = data
        .strip_prefix("audience_")
        .filter(|_| kind == "broadcast")
    {
        state["audience"] = json!(a);
        put_state(bot, who.id, &state).await?;
    } else if data == "remove_premium" && kind == "premium" {
        state["remove"] = json!(true);
        state["days"] = json!(0);
        put_state(bot, who.id, &state).await?;
    } else if data == "confirm" {
        notice = confirm(bot, who, &lang).await?;
    }
    render_with(bot, who.id, Some(message.id()), notice.as_deref()).await?;
    query.answer().send().await.ok();
    Ok(())
}

/// Takes the panel's one-shot "processing" mark, so a double tap cannot run the action twice.
async fn claim_confirm(bot: &Bot, user_id: i64) -> Result<Option<Value>> {
    let row: Option<(Value,)> = sqlx::query_as(
        "UPDATE user_states SET data = data || '{\"processing\":true}'::jsonb WHERE user_id = $1 AND kind = 'admin_panel' \
         AND expires_at > NOW() AND NOT COALESCE((data->>'processing')::boolean, FALSE) RETURNING data",
    )
    .bind(user_id)
    .fetch_optional(&bot.pg)
    .await?;
    Ok(row.map(|r| r.0))
}

/// Runs the panel's action once. Returns what to tell the admin, shown on top of the panel.
async fn confirm(bot: &Arc<Bot>, who: &Who, lang: &str) -> Result<Option<String>> {
    let Some(state) = claim_confirm(bot, who.id).await? else {
        return Ok(None);
    };
    let outcome = apply(bot, who, lang, &state).await;
    match outcome {
        Ok(notice) => {
            states::delete(&bot.pg, who.id, PANEL).await?;
            Ok(notice)
        }
        Err(error) => {
            let mut back = state.clone();
            back["processing"] = json!(false);
            put_state(bot, who.id, &back).await?;
            Err(error)
        }
    }
}

async fn apply(bot: &Arc<Bot>, who: &Who, lang: &str, state: &Value) -> Result<Option<String>> {
    let kind = state["kind"].as_str().unwrap_or("");
    match kind {
        "premium" => {
            let users = ids_of(state, "users");
            if state["remove"].as_bool().unwrap_or(false) {
                let changed = store::remove_premium(&bot.pg, &users).await?;
                for id in &changed {
                    let target_lang = bot.lang(*id).await;
                    bot.send_to(*id, &bot.t(&target_lang, "premium.removed", &[]), None)
                        .await
                        .ok();
                }
                let missing: Vec<i64> = users
                    .iter()
                    .copied()
                    .filter(|u| !changed.contains(u))
                    .collect();
                let none = bot.t(lang, "common.none", &[]);
                Ok(Some(bot.t(
                    lang,
                    "admin.premium.removed",
                    &[
                        ("users", &or_none(&changed, &none)),
                        ("missing", &or_none(&missing, &none)),
                    ],
                )))
            } else {
                let days = state["days"].as_i64().unwrap_or(0);
                if !(1..=MAX_DAYS).contains(&days) {
                    anyhow::bail!("Subscription duration is out of range");
                }
                let granted = store::grant_premium(&bot.pg, &users, days).await?;
                let mut changed = Vec::new();
                for (id, expiry) in &granted {
                    changed.push(*id);
                    let target_lang = bot.lang(*id).await;
                    let text = bot.t(
                        &target_lang,
                        "premium.granted_days",
                        &[
                            ("days", &days.to_string()),
                            (
                                "expiry_date",
                                &expiry.format("%Y-%m-%d %H:%M UTC").to_string(),
                            ),
                        ],
                    );
                    bot.send_to(*id, &text, None).await.ok();
                }
                let missing: Vec<i64> = users
                    .iter()
                    .copied()
                    .filter(|u| !changed.contains(u))
                    .collect();
                let none = bot.t(lang, "common.none", &[]);
                Ok(Some(bot.t(
                    lang,
                    "admin.premium.added",
                    &[
                        ("users", &or_none(&changed, &none)),
                        ("missing", &or_none(&missing, &none)),
                    ],
                )))
            }
        }
        "gift" => {
            let day = |field: &str| state[field].as_i64().filter(|d| *d > 0).map(|d| d * 86_400);
            let gift = store::NewGift {
                value: state["value"].as_i64().unwrap_or(0) as i32,
                link_expiry: store::link_expiry_from(day("link_days")),
                point_seconds: day("point_days"),
                only_users: ids_of(state, "only_users"),
                exclude_users: ids_of(state, "exclude_users"),
                limit: state["limit"].as_i64().map(|l| l as i32),
            };
            let pid = store::create_gift(&bot.pg, &gift).await?;
            // The link is something to copy and share, so it gets a message the panel will not overwrite.
            bot.send_to(
                who.id,
                &bot.t(
                    lang,
                    "admin.gifts.created",
                    &[("gift_link", &gift_link(bot, &pid))],
                ),
                None,
            )
            .await?;
            Ok(None)
        }
        "fsub" => Ok(add_fsub(bot, lang, state).await),
        "broadcast" => {
            start_copy_broadcast(bot, who, lang, state).await?;
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn or_none(ids: &[i64], none: &str) -> String {
    if ids.is_empty() {
        none.to_string()
    } else {
        ids_text(ids)
    }
}

async fn add_fsub(bot: &Arc<Bot>, lang: &str, state: &Value) -> Option<String> {
    use grammers_client::session::types::PeerId;
    let channel_id = state["channel_id"].as_i64()?;
    let limit = state["limit"].as_i64().unwrap_or(0) as i32;
    let outcome: Result<String> = async {
        let id = PeerId::from_bot_api_dialog_id(channel_id)
            .ok_or_else(|| anyhow::anyhow!("bad channel id"))?;
        let peer = bot
            .peer_ref_for(id)
            .await
            .ok_or_else(|| anyhow::anyhow!("channel is unknown to the bot"))?;
        let found = bot.client.resolve_peer(peer).await?;
        let channel: tl::enums::InputChannel = (&peer).into();
        let me = bot
            .client
            .invoke(&tl::functions::channels::GetParticipant {
                channel: channel.clone(),
                participant: tl::enums::InputPeer::PeerSelf,
            })
            .await?;
        let tl::enums::channels::ChannelParticipant::Participant(p) = me;
        let can_invite = match p.participant {
            tl::enums::ChannelParticipant::Creator(_) => true,
            tl::enums::ChannelParticipant::Admin(a) => {
                let tl::enums::ChatAdminRights::Rights(r) = a.admin_rights;
                r.invite_users
            }
            _ => false,
        };
        if !can_invite {
            anyhow::bail!("PERMISSION");
        }
        let link = match found.username() {
            Some(name) => format!("https://t.me/{name}"),
            None => {
                let exported = bot
                    .client
                    .invoke(&tl::functions::messages::ExportChatInvite {
                        legacy_revoke_permanent: false,
                        request_needed: false,
                        peer: (&peer).into(),
                        expire_date: None,
                        usage_limit: None,
                        title: None,
                        subscription_pricing: None,
                    })
                    .await?;
                match exported {
                    tl::enums::ExportedChatInvite::ChatInviteExported(i) => i.link,
                    _ => anyhow::bail!("no invite link"),
                }
            }
        };
        let members = match bot
            .client
            .invoke(&tl::functions::channels::GetFullChannel { channel })
            .await?
        {
            tl::enums::messages::ChatFull::Full(full) => match full.full_chat {
                tl::enums::ChatFull::ChannelFull(c) => c.participants_count.unwrap_or(0),
                _ => 0,
            },
        };
        store::add_fsub(&bot.pg, channel_id, &link, limit, members).await?;
        Ok(found
            .name()
            .map(str::to_string)
            .unwrap_or_else(|| channel_id.to_string()))
    }
    .await;
    Some(match outcome {
        Ok(title) => {
            let shown = if limit > 0 {
                limit.to_string()
            } else {
                bot.t(lang, "common.unlimited", &[])
            };
            bot.t(
                lang,
                "admin.force_subscription.added",
                &[("channel_title", &super::escape(&title)), ("limit", &shown)],
            )
        }
        Err(error) if error.to_string() == "PERMISSION" => {
            bot.t(lang, "admin.force_subscription.permission_required", &[])
        }
        Err(error) => {
            tracing::error!(%error, "could not add a required channel");
            bot.t(lang, "admin.force_subscription.add_failed", &[])
        }
    })
}

async fn start_copy_broadcast(bot: &Arc<Bot>, who: &Who, lang: &str, state: &Value) -> Result<()> {
    let Some(source_id) = state["source_id"].as_i64() else {
        return Ok(());
    };
    let audience = state["audience"].as_str().unwrap_or("all");
    let filters = Filters {
        exclude_premium: audience == "free",
        only_premium: audience == "premium",
        ..Filters::default()
    };
    let status = send_status(bot, who.id, lang).await?;
    spawn_broadcast(
        bot,
        who.id,
        Payload::Copy {
            chat: who.id,
            message_id: source_id as i32,
            pin: false,
        },
        filters,
        status,
    );
    Ok(())
}

async fn send_status(bot: &Bot, user_id: i64, lang: &str) -> Result<Arc<Status>> {
    let peer = bot
        .user_ref(user_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("admin is not known to the bot"))?;
    let sent = bot
        .client
        .send_message(peer, html(&bot.t(lang, "common.processing", &[]), None))
        .await?;
    Ok(Arc::new(Status::new(peer, sent.id(), None, lang)))
}

fn spawn_broadcast(
    bot: &Arc<Bot>,
    admin_id: i64,
    payload: Payload,
    filters: Filters,
    status: Arc<Status>,
) {
    let token = tokio_util::sync::CancellationToken::new();
    bot.broadcasts
        .lock()
        .unwrap()
        .insert(admin_id, token.clone());
    let bot = bot.clone();
    tokio::spawn(async move {
        broadcast::run(&bot, admin_id, payload, filters, status, token).await;
        bot.broadcasts.lock().unwrap().remove(&admin_id);
    });
}

/// `/cancel` also stops a broadcast the user started.
pub fn cancel_broadcast(bot: &Bot, user_id: i64) -> bool {
    match bot.broadcasts.lock().unwrap().get(&user_id) {
        Some(token) => {
            token.cancel();
            true
        }
        None => false,
    }
}

/// Replaces the gift list with the removal question; Back returns to the list.
async fn gift_remove_prompt(
    bot: &Arc<Bot>,
    query: &CallbackQuery,
    lang: &str,
    pid: &str,
) -> Result<()> {
    let back = || vec![red(&bot.t(lang, "buttons.back", &[]), "ad_gifts_0")];
    let message = query.load_message().await?;
    let Some((value, used)) = store::gift_summary(&bot.pg, pid).await? else {
        message
            .edit(html(
                &bot.t(lang, "admin.gifts.not_found", &[]),
                Some(keyboard(&[back()])),
            ))
            .await
            .ok();
        return Ok(());
    };
    let markup = keyboard(&[
        vec![green(
            &bot.t(lang, "buttons.confirm", &[]),
            &format!("confirm_del_gift_{pid}"),
        )],
        back(),
    ]);
    let text = bot.t(
        lang,
        "admin.gifts.remove_confirm",
        &[
            ("gift_pid", pid),
            ("value", &value.to_string()),
            ("usage_count", &used.unwrap_or(0).to_string()),
        ],
    );
    message.edit(html(&text, Some(markup))).await.ok();
    Ok(())
}

/// `confirm_del_gift_<pid>` from the removal prompt.
pub async fn confirm_gift_delete(
    bot: &Arc<Bot>,
    query: &CallbackQuery,
    who: &Who,
    data: &str,
) -> Result<()> {
    let lang = bot.lang(who.id).await;
    if !staff_only(bot, who).await {
        return Ok(());
    }
    let pid = data.trim_start_matches("confirm_del_gift_");
    store::deactivate_gift(&bot.pg, pid).await?;
    query
        .load_message()
        .await?
        .edit(html(
            &bot.t(&lang, "admin.gifts.deactivated", &[("gift_pid", pid)]),
            Some(keyboard(&[vec![red(
                &bot.t(&lang, "buttons.back", &[]),
                "ad_gifts_0",
            )]])),
        ))
        .await
        .ok();
    query.answer().send().await.ok();
    Ok(())
}

async fn send_users_csv(bot: &Arc<Bot>, user_id: i64, lang: &str) -> Result<()> {
    let (csv, count) = store::users_csv(&bot.pg).await?;
    let path = bot
        .cfg
        .data_dir
        .join(format!("users_{}.csv", chrono::Utc::now().timestamp()));
    tokio::fs::create_dir_all(&bot.cfg.data_dir).await?;
    tokio::fs::write(&path, csv).await?;
    let result = send_file(
        bot,
        user_id,
        &path,
        &bot.t(
            lang,
            "admin.users.exported",
            &[("count", &count.to_string())],
        ),
    )
    .await;
    tokio::fs::remove_file(&path).await.ok();
    result
}

async fn send_file(
    bot: &Arc<Bot>,
    user_id: i64,
    path: &std::path::Path,
    caption: &str,
) -> Result<()> {
    let peer = bot
        .user_ref(user_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("not known"))?;
    let uploaded = bot.client.upload_file(path).await?;
    bot.client
        .send_message(peer, InputMessage::new().html(caption).document(uploaded))
        .await?;
    Ok(())
}

async fn send_logs(bot: &Arc<Bot>, user_id: i64, lang: &str) -> Result<()> {
    let dir = bot.cfg.data_dir.clone();
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("logs")
                && entry
                    .metadata()
                    .await
                    .is_ok_and(|m| m.is_file() && m.len() > 0)
            {
                files.push(entry.path());
            }
        }
    } else {
        bot.send_to(user_id, &bot.t(lang, "admin.logs.not_found", &[]), None)
            .await?;
        return Ok(());
    }
    files.sort();
    if files.is_empty() {
        bot.send_to(user_id, &bot.t(lang, "admin.logs.empty", &[]), None)
            .await?;
        return Ok(());
    }
    for path in files {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Err(error) = send_file(
            bot,
            user_id,
            &path,
            &bot.t(lang, "admin.logs.file_caption", &[("filename", &name)]),
        )
        .await
        {
            bot.send_to(
                user_id,
                &bot.t(
                    lang,
                    "admin.logs.upload_failed",
                    &[
                        ("filename", &name),
                        ("error", &super::escape(&error.to_string())),
                    ],
                ),
                None,
            )
            .await
            .ok();
        }
    }
    Ok(())
}

/// Text sent to the admin while a panel step waits for input. True when the message was consumed.
pub async fn input(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<bool> {
    if !staff_only(bot, who).await {
        return Ok(false);
    }
    let Some(mut state) = get_state(bot, who.id).await else {
        return Ok(false);
    };
    let lang = bot.lang(who.id).await;
    if state["processing"].as_bool().unwrap_or(false) {
        return Ok(true);
    }
    let kind = state["kind"].as_str().unwrap_or("").to_string();
    let waiting = state["waiting"].as_str().map(str::to_string);
    let mut notice: Option<String> = None;
    if kind == "broadcast" && waiting.is_none() {
        state["source_id"] = json!(message.id());
        state["audience"] = json!("all");
        put_state(bot, who.id, &state).await?;
        if let (Some(peer), true) = (bot.user_ref(who.id).await, true) {
            // Show the admin a copy of what will be sent.
            let ids = [message.id()];
            let _ = bot.client.forward_messages(peer, &ids, peer).await;
        }
    } else if kind == "language" && message.media().is_some() {
        language_upload(bot, message, &lang).await?;
        states::delete(&bot.pg, who.id, PANEL).await?;
    } else if kind == "admins" && bot.is_owner(who.id) {
        let id = forwarded_or_typed_user(message);
        let Some(target) = id else {
            bot.reply(message, &bot.t(&lang, "panel.invalid_value", &[]), None)
                .await?;
            return Ok(true);
        };
        let text = if store::add_admin(&bot.pg, target, who.id).await? {
            bot.t(
                &lang,
                "panel.admin_added",
                &[("user_id", &target.to_string())],
            )
        } else {
            bot.t(&lang, "panel.admin_exists", &[])
        };
        message.delete().await.ok();
        notice = Some(text);
    } else if let Some(field) = waiting {
        match read_value(message, &field, &kind) {
            Some((field_name, value)) => {
                state[&field_name] = value;
                if field_name == "days" {
                    state["remove"] = json!(false);
                }
                if field_name == "only_users" {
                    state["exclude_users"] = json!([]);
                }
                if field_name == "exclude_users" {
                    state["only_users"] = json!([]);
                }
                state.as_object_mut().map(|o| o.remove("waiting"));
                put_state(bot, who.id, &state).await?;
                message.delete().await.ok();
            }
            None => {
                bot.reply(message, &bot.t(&lang, "panel.invalid_value", &[]), None)
                    .await?;
                return Ok(true);
            }
        }
    } else {
        return Ok(false);
    }
    let menu = get_state(bot, who.id)
        .await
        .and_then(|s| s["menu_id"].as_i64())
        .map(|i| i as i32);
    render_with(bot, who.id, menu, notice.as_deref()).await?;
    Ok(true)
}

fn forwarded_or_typed_user(message: &Message) -> Option<i64> {
    if let Some(tl::enums::MessageFwdHeader::Header(h)) = message.forward_header()
        && let Some(tl::enums::Peer::User(u)) = h.from_id
    {
        return Some(u.user_id);
    }
    parse_ids(message.text()).and_then(|ids| ids.first().copied())
}

/// The value a waiting field expects: a shared user list, a shared channel, a number or user ids.
fn read_value(message: &Message, field: &str, kind: &str) -> Option<(String, Value)> {
    if let Some(tl::enums::MessageAction::RequestedPeerSentMe(shared)) = message.action() {
        if field == "users" && shared.button_id == PICK_USERS_BUTTON {
            let ids: Vec<i64> = shared
                .peers
                .iter()
                .filter_map(|p| match p {
                    tl::enums::RequestedPeer::User(u) => Some(u.user_id),
                    _ => None,
                })
                .collect();
            return Some(("users".into(), json!(ids)));
        }
        if kind == "fsub" && shared.button_id == PICK_FSUB_BUTTON {
            use grammers_client::session::types::PeerId;
            if let Some(tl::enums::RequestedPeer::Channel(c)) = shared.peers.first() {
                let id = PeerId::channel(c.channel_id)?.bot_api_dialog_id()?;
                return Some(("channel_id".into(), json!(id)));
            }
        }
        return None;
    }
    match field {
        "users" | "only_users" | "exclude_users" => {
            let ids = forwarded_or_typed_user(message)
                .map(|i| vec![i])
                .or_else(|| parse_ids(message.text()))?;
            if ids.is_empty() && field == "users" {
                return None;
            }
            Some((field.into(), json!(ids)))
        }
        "days" | "value" | "link_days" | "point_days" | "limit" => {
            let n: i64 = message.text().trim().parse().ok()?;
            let minimum = if matches!(field, "days" | "value" | "limit") {
                1
            } else {
                0
            };
            (minimum..=MAX_DAYS)
                .contains(&n)
                .then(|| (field.to_string(), json!(n)))
        }
        _ => None,
    }
}

async fn language_upload(bot: &Arc<Bot>, message: &Message, lang: &str) -> Result<()> {
    use grammers_client::media::Media;
    let Some(Media::Document(document)) = message.media() else {
        bot.reply(
            message,
            &bot.t(lang, "admin.languages.json_required", &[]),
            None,
        )
        .await?;
        return Ok(());
    };
    let name = document.name().unwrap_or("").to_string();
    let Some(code) = name
        .strip_suffix(".json")
        .map(str::to_string)
        .filter(|c| is_language_code(c))
    else {
        bot.reply(
            message,
            &bot.t(lang, "admin.languages.json_required", &[]),
            None,
        )
        .await?;
        return Ok(());
    };
    let dir = bot.cfg.locales_dir.clone();
    let target = dir.join(format!("{code}.json"));
    let existed = target.exists();
    let temp = dir.join(format!(".{code}.{}.tmp", rand::random::<u32>()));
    let result: Result<()> = async {
        let downloaded = message.download_media(&temp).await?;
        anyhow::ensure!(downloaded, "the file could not be downloaded");
        let text = tokio::fs::read_to_string(&temp).await?;
        let value: Value = serde_json::from_str(&text)?;
        anyhow::ensure!(value.is_object(), "the JSON root must be an object");
        tokio::fs::rename(&temp, &target).await?;
        bot.locales.reload(&dir)?;
        Ok(())
    }
    .await;
    tokio::fs::remove_file(&temp).await.ok();
    match result {
        Ok(()) => {
            let key = if existed {
                "admin.languages.updated"
            } else {
                "admin.languages.added"
            };
            bot.reply(
                message,
                &bot.t(lang, key, &[("language_code", &code)]),
                None,
            )
            .await?;
        }
        Err(error) => {
            bot.reply(
                message,
                &bot.t(
                    lang,
                    "admin.languages.invalid_json",
                    &[("error", &super::escape(&error.to_string()))],
                ),
                None,
            )
            .await?;
        }
    }
    Ok(())
}

fn is_language_code(code: &str) -> bool {
    let mut parts = code.split(['-', '_']);
    let Some(first) = parts.next() else {
        return false;
    };
    (2..=3).contains(&first.len())
        && first.chars().all(|c| c.is_ascii_alphabetic())
        && parts.all(|p| (2..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn filters_summary(bot: &Bot, lang: &str, f: &Filters) -> String {
    if f.only_users.is_empty() && f.exclude_users.is_empty() && !f.exclude_premium {
        return bot.t(lang, "broadcast.filters.none", &[]);
    }
    bot.t(
        lang,
        "broadcast.filters.summary",
        &[
            ("only_count", &f.only_users.len().to_string()),
            ("excluded_count", &f.exclude_users.len().to_string()),
            (
                "exclude_premium",
                &bot.t(
                    lang,
                    if f.exclude_premium {
                        "common.yes"
                    } else {
                        "common.no"
                    },
                    &[],
                ),
            ),
        ],
    )
}

/// `/bc` replying to a message (copy to everyone) or `/bc .` replying to a JSON file (one text per language).
pub async fn bc(bot: &Arc<Bot>, message: &Message, who: &Who, args: &[&str]) -> Result<()> {
    if !staff_only(bot, who).await {
        return Ok(());
    }
    let lang = bot.lang(who.id).await;
    let reply = message.get_reply().await.ok().flatten();
    if args.first() == Some(&".") {
        return bc_json(bot, message, who, &lang, args, reply).await;
    }
    let Some(source) = reply else {
        let markup = keyboard(&[vec![blue(
            &bot.t(&lang, "broadcast.buttons.get_template", &[]),
            "bc_get_template",
        )]]);
        let text = format!(
            "{}\n\n{}",
            bot.t(&lang, "broadcast.usage", &[]),
            bot.t(&lang, "broadcast.json.how_to", &[])
        );
        bot.reply(message, &text, Some(markup)).await?;
        return Ok(());
    };
    let mut all = vec!["/bc"];
    all.extend_from_slice(args);
    let filters = broadcast::parse_filters(&all);
    let state = json!({
        "chat_id": who.id, "message_id": source.id(),
        "only_users": filters.only_users, "exclude_users": filters.exclude_users, "exclude_prem": filters.exclude_premium,
    });
    states::set(&bot.pg, who.id, "broadcast", &state, 10 * 60).await?;
    let id = source.id();
    let markup = keyboard(&[
        vec![green(
            &bot.t(&lang, "broadcast.buttons.normal", &[]),
            &format!("bc_normal_{id}"),
        )],
        vec![blue(
            &bot.t(&lang, "broadcast.buttons.pin", &[]),
            &format!("bc_pin_{id}"),
        )],
        vec![bot.button(&lang, "❌", "cancel_action", Color::Red)],
    ]);
    let text = bot.t(
        &lang,
        "broadcast.confirm",
        &[
            ("message_id", &id.to_string()),
            ("filters", &filters_summary(bot, &lang, &filters)),
        ],
    );
    bot.reply(message, &text, Some(markup)).await?;
    Ok(())
}

async fn bc_json(
    bot: &Arc<Bot>,
    message: &Message,
    who: &Who,
    lang: &str,
    args: &[&str],
    reply: Option<grammers_client::message::Message>,
) -> Result<()> {
    use grammers_client::media::Media;
    let document = reply.as_ref().and_then(|r| match r.media() {
        Some(Media::Document(d)) => Some(d),
        _ => None,
    });
    let Some(document) = document else {
        bot.reply(
            message,
            &bot.t(lang, "broadcast.json.file_required", &[]),
            None,
        )
        .await?;
        return Ok(());
    };
    if !document
        .name()
        .unwrap_or("")
        .to_lowercase()
        .ends_with(".json")
    {
        bot.reply(
            message,
            &bot.t(lang, "broadcast.json.invalid_file", &[]),
            None,
        )
        .await?;
        return Ok(());
    }
    let reply = reply.expect("a document implies a reply");
    let temp = bot.cfg.data_dir.join(format!(
        "broadcast_{}_{:08x}.json",
        who.id,
        rand::random::<u32>()
    ));
    tokio::fs::create_dir_all(&bot.cfg.data_dir).await?;
    let parsed: Result<std::collections::BTreeMap<String, String>> = async {
        anyhow::ensure!(
            reply.download_media(&temp).await?,
            "the file could not be downloaded"
        );
        let value: Value = serde_json::from_str(&tokio::fs::read_to_string(&temp).await?)?;
        broadcast::normalize_payload(&value)
    }
    .await;
    tokio::fs::remove_file(&temp).await.ok();
    let messages = match parsed {
        Ok(m) => m,
        Err(error) => {
            bot.reply(
                message,
                &bot.t(
                    lang,
                    "broadcast.json.validation_failed",
                    &[("error", &super::escape(&error.to_string()))],
                ),
                None,
            )
            .await?;
            return Ok(());
        }
    };
    let mut all = vec!["/bc"];
    all.extend_from_slice(args);
    let filters = broadcast::parse_filters(&all);
    let state = json!({
        "mode": "localized_json", "messages": messages, "source_message_id": reply.id(),
        "only_users": filters.only_users, "exclude_users": filters.exclude_users, "exclude_prem": filters.exclude_premium,
    });
    states::set(&bot.pg, who.id, "broadcast", &state, 10 * 60).await?;
    let markup = keyboard(&[
        vec![green(
            &bot.t(lang, "broadcast.buttons.normal", &[]),
            &format!("bcjson_send_{}", reply.id()),
        )],
        vec![bot.button(lang, "❌", "bcjson_cancel", Color::Red)],
    ]);
    let text = bot.t(
        lang,
        "broadcast.json.confirm",
        &[
            ("language_count", &messages.len().to_string()),
            ("filters", &filters_summary(bot, lang, &filters)),
        ],
    );
    bot.reply(message, &text, Some(markup)).await?;
    Ok(())
}

fn filters_from_state(state: &Value) -> Filters {
    Filters {
        only_users: ids_of(state, "only_users"),
        exclude_users: ids_of(state, "exclude_users"),
        exclude_premium: state["exclude_prem"].as_bool().unwrap_or(false),
        only_premium: false,
    }
}

/// `bc_normal_<id>`, `bc_pin_<id>`, `bcjson_send_<id>`, `bcjson_cancel`, `bc_get_template`, `cancel_action`.
pub async fn broadcast_callback(
    bot: &Arc<Bot>,
    query: &CallbackQuery,
    who: &Who,
    data: &str,
) -> Result<()> {
    let lang = bot.lang(who.id).await;
    if !staff_only(bot, who).await {
        query.answer().send().await.ok();
        return Ok(());
    }
    if data == "bc_get_template" {
        query.answer().send().await.ok();
        return send_template(bot, who.id, &lang).await;
    }
    if data == "bcjson_cancel" || data == "cancel_action" {
        states::delete(&bot.pg, who.id, "broadcast").await?;
        if data == "cancel_action" {
            cancel_broadcast(bot, who.id);
        }
        query.answer().send().await.ok();
        query
            .load_message()
            .await?
            .edit(html(&bot.t(&lang, "common.action_cancelled", &[]), None))
            .await
            .ok();
        return Ok(());
    }
    let Some(state) = states::take::<Value>(&bot.pg, who.id, "broadcast").await? else {
        query
            .answer()
            .alert(bot.t(&lang, "common.state_expired_popup", &[]))
            .send()
            .await
            .ok();
        return Ok(());
    };
    let expected: i64 = data
        .rsplit('_')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(-1);
    let state_id = state["message_id"]
        .as_i64()
        .or(state["source_message_id"].as_i64());
    if state_id != Some(expected) {
        query
            .answer()
            .alert(bot.t(&lang, "common.state_expired_popup", &[]))
            .send()
            .await
            .ok();
        return Ok(());
    }
    let filters = filters_from_state(&state);
    let payload = if data.starts_with("bcjson_send_") {
        let messages: std::collections::BTreeMap<String, String> =
            serde_json::from_value(state["messages"].clone()).unwrap_or_default();
        Payload::Localized(messages)
    } else {
        Payload::Copy {
            chat: state["chat_id"].as_i64().unwrap_or(who.id),
            message_id: expected as i32,
            pin: data.starts_with("bc_pin_"),
        }
    };
    query.answer().send().await.ok();
    let message = query.load_message().await?;
    message
        .edit(html(&bot.t(&lang, "broadcast.starting", &[]), None))
        .await
        .ok();
    let peer = bot
        .user_ref(who.id)
        .await
        .ok_or_else(|| anyhow::anyhow!("admin is not known"))?;
    let status = Arc::new(Status::new(peer, message.id(), None, &lang));
    spawn_broadcast(bot, who.id, payload, filters, status);
    Ok(())
}

/// A JSON file with an empty text for every installed language, ready to fill in.
pub async fn send_template(bot: &Arc<Bot>, user_id: i64, lang: &str) -> Result<()> {
    let _ = bot.locales.reload(&bot.cfg.locales_dir);
    let mut codes = bot.locales.codes();
    codes.sort();
    codes.retain(|c| c != "en");
    codes.insert(0, "en".to_string());
    let mut template = serde_json::Map::new();
    template.insert(
        "_instructions".into(),
        json!([
            "Fill the text value for every language you want to send.",
            "A language left empty is skipped and falls back to English.",
            "HTML is allowed: <b> <i> <u> <s> <code> <pre> <a href>.",
            "The en entry is required and must not be empty.",
            "Maximum 4096 characters per language."
        ]),
    );
    let names: std::collections::HashMap<String, String> =
        bot.locales.available().into_iter().collect();
    for code in &codes {
        template.insert(code.clone(), json!({"_language": names.get(code).cloned().unwrap_or_else(|| code.clone()), "text": ""}));
    }
    let dir = bot.cfg.data_dir.clone();
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join("broadcast_template.json");
    tokio::fs::write(
        &path,
        serde_json::to_string_pretty(&Value::Object(template))?,
    )
    .await?;
    let result = send_file(
        bot,
        user_id,
        &path,
        &bot.t(
            lang,
            "broadcast.json.template_sent",
            &[("language_count", &codes.len().to_string())],
        ),
    )
    .await;
    tokio::fs::remove_file(&path).await.ok();
    result
}

pub async fn bcjson(bot: &Arc<Bot>, _message: &Message, who: &Who) -> Result<()> {
    if !staff_only(bot, who).await {
        return Ok(());
    }
    let lang = bot.lang(who.id).await;
    send_template(bot, who.id, &lang).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_id_lists() {
        assert_eq!(parse_ids("[1, 2,3]"), Some(vec![1, 2, 3]));
        assert_eq!(parse_ids("5 6"), Some(vec![5, 6]));
        assert_eq!(parse_ids("0"), None);
        assert_eq!(parse_ids("-4"), None);
        assert_eq!(parse_ids("x"), None);
        assert_eq!(parse_ids(""), Some(vec![]));
    }

    #[test]
    fn language_codes() {
        for ok in ["ar", "pt-br", "zh_hans", "fil"] {
            assert!(is_language_code(ok), "{ok}");
        }
        for bad in ["a", "english", "../x", "a-b", ""] {
            assert!(!is_language_code(bad), "{bad}");
        }
    }
}
