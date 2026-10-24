//! `/bdl`, `/clone`, the filter menu they open, and resuming unfinished ones after a restart.

use std::sync::Arc;

use anyhow::Result;
use grammers_client::{
    message::ReplyMarkup,
    update::{CallbackQuery, Message},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    Bot, Who,
    download::{GUIDE, begin_guide, flow_cancel, permission_ok},
    html,
};
use crate::{
    filters::FILTER_TYPES,
    links::{is_supported_link, parse_chat_reference, parse_message_link},
    store::{account, active, queue, states},
    tasks::{
        self, Outcome,
        range::{Kind, Range, Resume},
    },
    telegram::keyboards::{Button, Color, keyboard},
};

const FILTER_KIND: &str = "filter";
const FILTER_TTL: i64 = 10 * 60;

#[derive(Serialize, Deserialize, Clone)]
struct FilterMenu {
    task_type: String,
    payload: Value,
    mode: String,
    types: Vec<String>,
}

fn filter_keyboard(bot: &Bot, lang: &str, menu: &FilterMenu) -> ReplyMarkup {
    let color = |on: bool, c: Color| if on { c } else { Color::Plain };
    let mode_row = vec![
        Button::callback(
            &bot.t(lang, "filters.modes.all", &[]),
            "flt_mode_all",
            color(menu.mode == "all", Color::Blue),
        ),
        Button::callback(
            &bot.t(lang, "filters.modes.save", &[]),
            "flt_mode_sv",
            color(menu.mode == "sv", Color::Green),
        ),
        Button::callback(
            &bot.t(lang, "filters.modes.skip", &[]),
            "flt_mode_rm",
            color(menu.mode == "rm", Color::Red),
        ),
    ];
    let mut rows = vec![mode_row];
    if menu.mode != "all" {
        let active = if menu.mode == "sv" {
            Color::Green
        } else {
            Color::Red
        };
        for pair in [["videos", "photos"], ["audio", "files"], ["text", "other"]] {
            rows.push(
                pair.iter()
                    .map(|kind| {
                        let selected = menu.types.iter().any(|t| t == kind);
                        Button::callback(
                            &bot.t(lang, &format!("filters.types.{kind}"), &[]),
                            &format!("flt_type_{kind}"),
                            color(selected, active),
                        )
                    })
                    .collect(),
            );
        }
    }
    rows.push(vec![
        Button::callback(
            &bot.t(lang, "buttons.start", &[]),
            "flt_start",
            Color::Green,
        ),
        bot.button(lang, "❌", "flt_cancel", Color::Red),
    ]);
    keyboard(&rows)
}

async fn open_filter_menu(
    bot: &Bot,
    who: &Who,
    lang: &str,
    task_type: &str,
    payload: Value,
) -> Result<()> {
    let menu = FilterMenu {
        task_type: task_type.into(),
        payload,
        mode: "all".into(),
        types: Vec::new(),
    };
    states::set(&bot.pg, who.id, FILTER_KIND, &menu, FILTER_TTL).await?;
    bot.send_to(
        who.id,
        &bot.t(lang, "filters.prompt", &[]),
        Some(filter_keyboard(bot, lang, &menu)),
    )
    .await?;
    Ok(())
}

fn flags_of(menu: &FilterMenu) -> Vec<String> {
    if menu.mode == "all" {
        return Vec::new();
    }
    let mut flags = vec![format!("-{}", menu.mode)];
    flags.extend(
        menu.types
            .iter()
            .filter(|t| FILTER_TYPES.contains(&t.as_str()))
            .map(|t| format!("--{t}")),
    );
    flags
}

fn has_manual_filters(flags: &[&str]) -> bool {
    flags
        .iter()
        .any(|f| matches!(*f, "-sv" | "-rm" | "--min" | "--max"))
}

pub async fn bdl(bot: &Arc<Bot>, message: &Message, who: &Who, args: &[&str]) -> Result<()> {
    let lang = bot.lang(who.id).await;
    if args.contains(&"--fdl") {
        bot.reply(
            message,
            &bot.t(&lang, "forcedownload.flag_removed", &[]),
            None,
        )
        .await?;
        return Ok(());
    }
    if args.is_empty() {
        return begin_guide(bot, message, who, "bdl").await;
    }
    let links: Vec<&str> = args
        .iter()
        .copied()
        .filter(|a| is_supported_link(a))
        .collect();
    if links.len() != 2 {
        bot.reply(message, &bot.t(&lang, "downloads.batch.usage", &[]), None)
            .await?;
        return Ok(());
    }
    let (first, last) = (parse_message_link(links[0]), parse_message_link(links[1]));
    let (Ok(first), Ok(last)) = (first, last) else {
        bot.reply(
            message,
            &bot.t(&lang, "downloads.batch.invalid_links", &[]),
            None,
        )
        .await?;
        return Ok(());
    };
    if first.chat != last.chat {
        bot.reply(
            message,
            &bot.t(&lang, "downloads.batch.different_sources", &[]),
            None,
        )
        .await?;
        return Ok(());
    }
    if first.message_id > last.message_id {
        bot.reply(
            message,
            &bot.t(&lang, "downloads.batch.invalid_order", &[]),
            None,
        )
        .await?;
        return Ok(());
    }
    let flags: Vec<&str> = args
        .iter()
        .copied()
        .filter(|a| !links.contains(a))
        .collect();
    let payload = json!({"link_start": links[0], "link_end": links[1]});
    if !has_manual_filters(&flags) {
        return open_filter_menu(bot, who, &lang, "bdl", payload).await;
    }
    start(
        bot,
        who.id,
        Kind::Bdl,
        payload,
        flags.iter().map(|f| f.to_string()).collect(),
    )
    .await
}

pub async fn clone_channel(
    bot: &Arc<Bot>,
    message: &Message,
    who: &Who,
    args: &[&str],
) -> Result<()> {
    let lang = bot.lang(who.id).await;
    if !account::load(&bot.pg, who.id)
        .await?
        .is_some_and(|u| u.is_premium)
    {
        let markup = keyboard(&[
            vec![Button::callback(
                &bot.t(&lang, "buttons.subscribe", &[]),
                "show_premium",
                Color::Green,
            )],
            vec![bot.button(&lang, "❌", "cancel_download", Color::Red)],
        ]);
        bot.reply(
            message,
            &bot.t(&lang, "premium.feature_required", &[]),
            Some(markup),
        )
        .await?;
        return Ok(());
    }
    if args.contains(&"--fdl") {
        bot.reply(
            message,
            &bot.t(&lang, "forcedownload.flag_removed", &[]),
            None,
        )
        .await?;
        return Ok(());
    }
    let Some(channel) = args.first().map(|a| a.trim()) else {
        return begin_guide(bot, message, who, "clone").await;
    };
    if parse_chat_reference(channel).is_err() {
        bot.reply(message, &bot.t(&lang, "clone.usage", &[]), None)
            .await?;
        return Ok(());
    }
    let flags: Vec<&str> = args[1..]
        .iter()
        .copied()
        .filter(|a| *a != "--fdl")
        .collect();
    let payload = json!({"channel_url": channel});
    if !has_manual_filters(&flags) {
        return open_filter_menu(bot, who, &lang, "clone", payload).await;
    }
    start(
        bot,
        who.id,
        Kind::Clone,
        payload,
        flags.iter().map(|f| f.to_string()).collect(),
    )
    .await
}

/// Checks the points of a batch, then queues it.
pub async fn start(
    bot: &Arc<Bot>,
    user_id: i64,
    kind: Kind,
    payload: Value,
    flags: Vec<String>,
) -> Result<()> {
    let lang = bot.lang(user_id).await;
    if kind == Kind::Bdl {
        let first = parse_message_link(payload["link_start"].as_str().unwrap_or(""));
        let last = parse_message_link(payload["link_end"].as_str().unwrap_or(""));
        let (Ok(first), Ok(last)) = (first, last) else {
            bot.send_to(
                user_id,
                &bot.t(&lang, "downloads.batch.invalid_links", &[]),
                None,
            )
            .await?;
            return Ok(());
        };
        let cost = last.message_id - first.message_id + 1;
        let who = Who {
            id: user_id,
            first_name: String::new(),
            last_name: None,
            username: None,
            lang_code: None,
        };
        if !permission_ok(bot, &who, &lang, cost).await? {
            return Ok(());
        }
    }
    let spec = json!({"payload": payload, "flags": flags});
    let job_payload = payload.clone();
    let job_flags = flags.clone();
    let job: tasks::Job = Box::new(move |ctx| {
        Box::pin(async move {
            tasks::range::run(
                &ctx,
                Range {
                    kind,
                    payload: job_payload,
                    flags: job_flags,
                    resume: None,
                },
            )
            .await
        })
    });
    tasks::track(bot, user_id, kind.word(), spec, job, None).await?;
    Ok(())
}

/// The reply to the `/bdl` and `/clone` guides. True when consumed.
pub async fn guide_answer(bot: &Arc<Bot>, message: &Message, who: &Who) -> Result<bool> {
    let Some(state) = states::get::<Value>(&bot.pg, who.id, GUIDE).await? else {
        return Ok(false);
    };
    let kind = state["kind"].as_str().unwrap_or("").to_string();
    if kind != "bdl" && kind != "clone" {
        return Ok(false);
    }
    let lang = bot.lang(who.id).await;
    let value = message.text().trim().to_string();
    if kind == "bdl" {
        if !is_supported_link(&value) || parse_message_link(&value).is_err() {
            bot.reply(
                message,
                &bot.t(&lang, "downloads.batch.invalid_links", &[]),
                Some(flow_cancel(bot, &lang)),
            )
            .await?;
            return Ok(true);
        }
        if state["step"] == "first" {
            states::set(
                &bot.pg,
                who.id,
                GUIDE,
                &json!({"kind": "bdl", "step": "last", "first": value}),
                24 * 3600,
            )
            .await?;
            bot.reply(
                message,
                &bot.t(&lang, "flow.last_link", &[]),
                Some(flow_cancel(bot, &lang)),
            )
            .await?;
            return Ok(true);
        }
        let first_link = state["first"].as_str().unwrap_or("").to_string();
        let (first, last) = (parse_message_link(&first_link), parse_message_link(&value));
        let (Ok(first), Ok(last)) = (first, last) else {
            return Ok(true);
        };
        if first.chat != last.chat || first.message_id > last.message_id {
            let key = if first.chat != last.chat {
                "downloads.batch.different_sources"
            } else {
                "downloads.batch.invalid_order"
            };
            bot.reply(message, &bot.t(&lang, key, &[]), None).await?;
            return Ok(true);
        }
        states::delete(&bot.pg, who.id, GUIDE).await?;
        open_filter_menu(
            bot,
            who,
            &lang,
            "bdl",
            json!({"link_start": first_link, "link_end": value}),
        )
        .await?;
    } else {
        if parse_chat_reference(&value).is_err() {
            bot.reply(message, &bot.t(&lang, "flow.invalid_channel", &[]), None)
                .await?;
            return Ok(true);
        }
        states::delete(&bot.pg, who.id, GUIDE).await?;
        clone_channel(bot, message, who, &[&value]).await?;
    }
    Ok(true)
}

pub async fn filter_callback(
    bot: &Arc<Bot>,
    query: &CallbackQuery,
    who: &Who,
    data: &str,
) -> Result<()> {
    let lang = bot.lang(who.id).await;
    let Some(mut menu) = states::get::<FilterMenu>(&bot.pg, who.id, FILTER_KIND).await? else {
        if let Ok(m) = query.load_message().await {
            m.delete().await.ok();
        }
        query
            .answer()
            .alert(bot.t(&lang, "common.state_expired_popup", &[]))
            .send()
            .await
            .ok();
        return Ok(());
    };
    match data {
        "flt_cancel" => {
            states::delete(&bot.pg, who.id, FILTER_KIND).await?;
            query.answer().send().await.ok();
            query.load_message().await?.delete().await.ok();
            return Ok(());
        }
        "flt_mode_all" | "flt_mode_sv" | "flt_mode_rm" => {
            menu.mode = data.trim_start_matches("flt_mode_").to_string();
            menu.types.clear();
        }
        d if d.starts_with("flt_type_") => {
            let kind = d.trim_start_matches("flt_type_").to_string();
            if FILTER_TYPES.contains(&kind.as_str()) && menu.mode != "all" {
                if let Some(at) = menu.types.iter().position(|t| *t == kind) {
                    menu.types.remove(at);
                } else {
                    menu.types.push(kind);
                }
            }
        }
        "flt_start" => {
            let flags = flags_of(&menu);
            states::delete(&bot.pg, who.id, FILTER_KIND).await?;
            query.answer().send().await.ok();
            query.load_message().await?.delete().await.ok();
            let kind = if menu.task_type == "clone" {
                Kind::Clone
            } else {
                Kind::Bdl
            };
            return start(bot, who.id, kind, menu.payload, flags).await;
        }
        _ => {}
    }
    states::set(&bot.pg, who.id, FILTER_KIND, &menu, FILTER_TTL).await?;
    query.answer().send().await.ok();
    match query
        .load_message()
        .await?
        .edit(html(
            &bot.t(&lang, "filters.prompt", &[]),
            Some(filter_keyboard(bot, &lang, &menu)),
        ))
        .await
    {
        Ok(()) => {}
        Err(error) if error.is("MESSAGE_NOT_MODIFIED") => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// The tasks left open by the previous run, read before the bot takes any update. Resuming waits a little
/// for the accounts, and a task a user starts meanwhile must not be taken for a leftover and run twice.
pub struct Leftovers {
    queue_ids: Vec<i64>,
    active_ids: std::collections::HashSet<i64>,
}

impl Leftovers {
    pub async fn take(bot: &Bot) -> Result<Self> {
        let queue_ids = queue::all_open(&bot.pg)
            .await?
            .into_iter()
            .map(|(queue_id, ..)| queue_id)
            .collect();
        let active_ids = active::all(&bot.pg)
            .await?
            .into_iter()
            .map(|task| task.id)
            .collect();
        Ok(Self {
            queue_ids,
            active_ids,
        })
    }
}

/// After a restart: every task that was queued or running is waiting again, in its old order.
pub async fn resume_all(bot: Arc<Bot>, leftovers: Leftovers) {
    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    if let Err(error) = resume(&bot, &leftovers).await {
        tracing::error!(%error, "could not resume pending tasks");
    }
}

async fn resume(bot: &Arc<Bot>, leftovers: &Leftovers) -> Result<()> {
    sqlx::query(
        "UPDATE task_queue SET status = 'queued' WHERE status = 'running' AND queue_id = ANY($1)",
    )
    .bind(&leftovers.queue_ids)
    .execute(&bot.pg)
    .await?;
    let open: Vec<_> = queue::all_open(&bot.pg)
        .await?
        .into_iter()
        .filter(|(queue_id, ..)| leftovers.queue_ids.contains(queue_id))
        .collect();
    let mut by_active: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for (queue_id, _, kind, _, spec) in &open {
        if matches!(kind.as_str(), "bdl" | "clone")
            && let Some(active_id) = spec["active_task_id"].as_i64()
        {
            by_active.insert(active_id, *queue_id);
        }
    }
    let mut resumed = std::collections::HashSet::new();

    // Single downloads: re-run the link.
    for (queue_id, user_id, kind, _, spec) in &open {
        if kind != "dl" {
            continue;
        }
        let lang = bot.lang(*user_id).await;
        let Some(link) = spec["url"].as_str().map(str::to_string) else {
            queue::finish(&bot.pg, *queue_id, "failed").await?;
            bot.send_to(
                *user_id,
                &bot.t(
                    &lang,
                    "queue.lost_after_restart",
                    &[("task_id", &queue_id.to_string())],
                ),
                None,
            )
            .await
            .ok();
            continue;
        };
        let force = account::load(&bot.pg, *user_id)
            .await?
            .is_some_and(|u| u.force_download_enabled && u.is_premium);
        let job: tasks::Job = Box::new(move |ctx| {
            Box::pin(async move {
                let outcome = tasks::single::download_link(&ctx, &link, force).await;
                if outcome == Outcome::Downloaded {
                    super::start_cooldown(&ctx.bot, ctx.user_id).await;
                    let _ = account::spend_points(&ctx.bot.pg, ctx.user_id, 1).await;
                }
                outcome
            })
        });
        let status_id = spec["status_message_id"].as_i64().map(|i| i as i32);
        if let Err(error) = tasks::track(
            bot,
            *user_id,
            "dl",
            spec.clone(),
            job,
            Some((*queue_id, status_id)),
        )
        .await
        {
            tracing::warn!(queue_id, %error, "could not resume a download");
        }
    }

    for task in active::all(&bot.pg).await? {
        if !leftovers.active_ids.contains(&task.id) {
            continue;
        }
        if task.attempts >= 3 {
            tracing::warn!(
                task = task.id,
                attempts = task.attempts,
                "dropping a task after failed resume attempts"
            );
            active::clear(&bot.pg, task.id).await;
            continue;
        }
        active::bump_attempts(&bot.pg, task.id).await;
        let resume_from = if task.last_done_id > 0 {
            task.last_done_id + 1
        } else {
            task.start_id
        };
        if resume_from > task.end_id && task.failed_ids.is_empty() {
            active::clear(&bot.pg, task.id).await;
            continue;
        }
        let lang = bot.lang(task.user_id).await;
        if bot
            .send_to(task.user_id, &bot.t(&lang, "tasks.resumed", &[]), None)
            .await
            .is_err()
        {
            active::clear(&bot.pg, task.id).await;
            continue;
        }
        let kind = match task.kind.as_str() {
            "bdl" => Kind::Bdl,
            "clone" => Kind::Clone,
            _ => {
                active::clear(&bot.pg, task.id).await;
                continue;
            }
        };
        let queue_id = by_active.get(&task.id).copied();
        if let Some(id) = queue_id {
            resumed.insert(id);
        }
        let range_resume = Resume {
            from: resume_from,
            task_id: task.id,
            failed: task.failed_ids.clone(),
            retry_phase: task.retry_phase,
        };
        let payload = task.payload.clone();
        let flags = task.flags.clone();
        let job: tasks::Job = Box::new(move |ctx| {
            Box::pin(async move {
                tasks::range::run(
                    &ctx,
                    Range {
                        kind,
                        payload,
                        flags,
                        resume: Some(range_resume),
                    },
                )
                .await
            })
        });
        let tracked = tasks::track(
            bot,
            task.user_id,
            kind.word(),
            json!({"payload": task.payload, "flags": task.flags, "active_task_id": task.id}),
            job,
            Some((
                queue_id.unwrap_or(0),
                Some(task.status_message_id as i32).filter(|i| *i > 0),
            )),
        )
        .await;
        if let Err(error) = tracked {
            tracing::error!(task = task.id, %error, "could not resume a task");
            active::clear(&bot.pg, task.id).await;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    // Queued batches whose saved progress is gone either finished or never started.
    for (queue_id, user_id, kind, _, spec) in &open {
        if !matches!(kind.as_str(), "bdl" | "clone") || resumed.contains(queue_id) {
            continue;
        }
        if spec["started"].as_bool().unwrap_or(false) || spec["active_task_id"].is_number() {
            queue::finish(&bot.pg, *queue_id, "done").await?;
            continue;
        }
        let kind_enum = if kind == "clone" {
            Kind::Clone
        } else {
            Kind::Bdl
        };
        let payload = spec["payload"].clone();
        let flags: Vec<String> = serde_json::from_value(spec["flags"].clone()).unwrap_or_default();
        let job: tasks::Job = Box::new(move |ctx| {
            Box::pin(async move {
                tasks::range::run(
                    &ctx,
                    Range {
                        kind: kind_enum,
                        payload,
                        flags,
                        resume: None,
                    },
                )
                .await
            })
        });
        let status_id = spec["status_message_id"].as_i64().map(|i| i as i32);
        if let Err(error) = tasks::track(
            bot,
            *user_id,
            kind,
            spec.clone(),
            job,
            Some((*queue_id, status_id)),
        )
        .await
        {
            tracing::error!(queue_id, %error, "could not restart a queued batch");
            queue::finish(&bot.pg, *queue_id, "failed").await?;
        }
    }
    Ok(())
}
