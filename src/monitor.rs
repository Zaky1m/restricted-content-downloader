//! Channel monitoring: a premium user names a channel and every new post is delivered to them.
//!
//! Public channels are watched live by the first pool account (it joins them). Private ones are checked
//! hourly with the user's own account, one account open at a time. New posts become rows in
//! `monitor_jobs` and are delivered one after another.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use grammers_client::{
    message::ReplyMarkup,
    session::types::{PeerId, PeerRef},
    tl,
    update::{CallbackQuery, Message},
};
use serde_json::json;
use tokio::sync::{Mutex, Notify, mpsc::UnboundedReceiver};

use crate::{
    bot::{Bot, Who},
    filters::{extract_filter_settings, should_process},
    links::{Chat, parse_chat_reference},
    media::info,
    store::account,
    tasks::{
        TaskCtx,
        access::{self, is_private_access_error},
        download::{Job, raw_message},
    },
    telegram::{
        accounts::Account,
        keyboards::{Button, Color, keyboard},
    },
};

const SCAN_LIMIT: i64 = 200;
const SOURCE_LOST_CHECKS: i32 = 24;
const QUEUE_LOCK_KEY: i64 = 7_310_001;

/// Opening, resolving and baselining share one account at a time.
static ACCOUNT_LOCK: Mutex<()> = Mutex::const_new(());
static WAKE: Notify = Notify::const_new();

pub fn spawn(bot: Arc<Bot>, events: UnboundedReceiver<(i64, i32)>) {
    tokio::spawn(consume_events(bot.clone(), events));
    tokio::spawn(scan_worker(bot.clone()));
    tokio::spawn(delivery_worker(bot));
}

async fn enqueue(pg: &sqlx::PgPool, user_id: i64, target: i64, message_id: i64) {
    let _ = sqlx::query("INSERT INTO monitor_jobs (user_id, target_id, message_id) VALUES ($1, $2, $3) ON CONFLICT (user_id, target_id, message_id) DO NOTHING")
        .bind(user_id)
        .bind(target)
        .bind(message_id)
        .execute(pg)
        .await;
    WAKE.notify_waiters();
}

async fn consume_events(bot: Arc<Bot>, mut events: UnboundedReceiver<(i64, i32)>) {
    while let Some((chat, message_id)) = events.recv().await {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT user_id FROM users WHERE monitor_target_id = $1 AND is_premium AND (premium_expiry_date IS NULL OR premium_expiry_date > NOW())",
        )
        .bind(chat)
        .fetch_all(&bot.pg)
        .await
        .unwrap_or_default();
        for (user_id,) in rows {
            enqueue(&bot.pg, user_id, chat, i64::from(message_id)).await;
        }
    }
}

/// The account's reference for the monitored chat.
async fn peer_in(account: &Account, dialog_id: i64) -> Option<PeerRef> {
    let id = PeerId::from_bot_api_dialog_id(dialog_id)?;
    Some(crate::tasks::access::peer_for(account, id).await)
}

/// The account that reads `target` for `user_id`, and whether it is the user's own (to close afterwards).
async fn open_account(bot: &Bot, user_id: i64, target: i64) -> Result<(Arc<Account>, bool)> {
    if let Some(pool) = bot.accounts.first_pool().await
        && let Some(peer) = peer_in(&pool, target).await
        && let Ok(found) = pool.client().await.resolve_peer(peer).await
        && found.username().is_some()
    {
        return Ok((pool, false));
    }
    match bot.accounts.linked(user_id).await? {
        Some(account) => Ok((account, true)),
        None => anyhow::bail!("session revoked"),
    }
}

async fn describe(bot: &Bot, lang: &str, title: Option<&str>, username: Option<&str>) -> String {
    let parts: Vec<String> = [title.map(str::to_string), username.map(|u| format!("@{u}"))]
        .into_iter()
        .flatten()
        .collect();
    if parts.is_empty() {
        bot.t(lang, "common.not_available", &[])
    } else {
        crate::bot::escape(&parts.join(" "))
    }
}

async fn scan(bot: &Arc<Bot>, user_id: i64, target: i64) -> Result<()> {
    let _guard = ACCOUNT_LOCK.lock().await;
    let (account, owned) = open_account(bot, user_id, target).await?;
    let outcome = async {
        let client = account.client().await;
        let peer = peer_in(&account, target).await.ok_or_else(|| anyhow::anyhow!("source is inaccessible"))?;
        let Some(latest) = access::edge_id(&client, peer, false).await? else { anyhow::bail!("source is inaccessible or empty") };
        let progress: Option<(i64, i64, Option<String>)> =
            sqlx::query_as("SELECT target_id, last_message_id, target_title FROM monitor_progress WHERE user_id = $1")
                .bind(user_id)
                .fetch_optional(&bot.pg)
                .await?;
        if let Some((progress_target, _, None)) = &progress
            && *progress_target == target
            && let Ok(found) = client.resolve_peer(peer).await
        {
            sqlx::query("UPDATE monitor_progress SET target_title = $2, target_username = $3 WHERE user_id = $1")
                .bind(user_id)
                .bind(found.name())
                .bind(found.username())
                .execute(&bot.pg)
                .await?;
        }
        let needs_baseline = progress.as_ref().is_none_or(|(t, last, _)| *t != target || *last < 0);
        if needs_baseline {
            sqlx::query(
                "INSERT INTO monitor_progress (user_id, target_id, last_message_id, last_checked_at) VALUES ($1, $2, $3, NOW()) \
                 ON CONFLICT (user_id) DO UPDATE SET target_id = EXCLUDED.target_id, last_message_id = EXCLUDED.last_message_id, last_checked_at = NOW()",
            )
            .bind(user_id)
            .bind(target)
            .bind(latest)
            .execute(&bot.pg)
            .await?;
            return Ok(());
        }
        let start = progress.map_or(0, |p| p.1) + 1;
        let end = latest.min(start + SCAN_LIMIT - 1);
        if start > end {
            sqlx::query("UPDATE monitor_progress SET last_checked_at = NOW(), fail_count = 0 WHERE user_id = $1").bind(user_id).execute(&bot.pg).await?;
            return Ok(());
        }
        let ids: Vec<i32> = (start..=end).map(|i| i as i32).collect();
        let posts = access::get_many(&client, peer, &ids).await?;
        let mut tx = bot.pg.begin().await?;
        for post in posts.into_iter().flatten() {
            sqlx::query(
                "INSERT INTO monitor_jobs (user_id, target_id, message_id) SELECT $1, $2, $3 \
                 WHERE EXISTS (SELECT 1 FROM users WHERE user_id = $1 AND monitor_target_id = $2) \
                 ON CONFLICT (user_id, target_id, message_id) DO NOTHING",
            )
            .bind(user_id)
            .bind(target)
            .bind(i64::from(post.id()))
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE monitor_progress SET last_message_id = $3, fail_count = 0, \
             last_checked_at = CASE WHEN $3 >= $4 THEN NOW() ELSE NULL END WHERE user_id = $1 AND target_id = $2",
        )
        .bind(user_id)
        .bind(target)
        .bind(end)
        .bind(latest)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        WAKE.notify_waiters();
        anyhow::Ok(())
    }
    .await;
    if owned {
        bot.accounts.release_linked(user_id).await;
    }
    outcome
}

async fn stop_monitor(bot: &Bot, user_id: i64) {
    let _ = sqlx::query(
        "UPDATE users SET monitor_target_id = NULL, monitor_flags = NULL WHERE user_id = $1",
    )
    .bind(user_id)
    .execute(&bot.pg)
    .await;
}

async fn scan_worker(bot: Arc<Bot>) {
    loop {
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT u.user_id, u.monitor_target_id FROM users u LEFT JOIN monitor_progress p ON p.user_id = u.user_id \
             WHERE u.monitor_target_id IS NOT NULL AND u.is_premium AND (u.premium_expiry_date IS NULL OR u.premium_expiry_date > NOW()) \
             AND (p.user_id IS NULL OR p.target_id <> u.monitor_target_id OR p.last_checked_at IS NULL OR p.last_checked_at < NOW() - INTERVAL '1 hour') \
             ORDER BY p.last_checked_at NULLS FIRST, u.user_id LIMIT 1",
        )
        .fetch_all(&bot.pg)
        .await
        .unwrap_or_default();
        for (user_id, target) in &rows {
            let (user_id, target) = (*user_id, *target);
            let result =
                tokio::time::timeout(Duration::from_secs(180), scan(&bot, user_id, target)).await;
            let error = match result {
                Ok(Ok(())) => continue,
                Ok(Err(error)) => error,
                Err(_) => anyhow::anyhow!("scan timed out"),
            };
            let text = error.to_string();
            let dead = error
                .downcast_ref::<grammers_client::InvocationError>()
                .is_some_and(access::Dead::is_dead)
                || text.contains("session revoked");
            if dead {
                stop_monitor(&bot, user_id).await;
                let lang = bot.lang(user_id).await;
                bot.send_to(user_id, &bot.t(&lang, "monitor.session_stopped", &[]), None)
                    .await
                    .ok();
                continue;
            }
            let source_lost = text.contains("inaccessible")
                || error
                    .downcast_ref::<grammers_client::InvocationError>()
                    .is_some_and(is_private_access_error);
            if source_lost {
                tracing::info!(user_id, %error, "monitor source unavailable");
            } else {
                tracing::error!(user_id, %error, "monitor scan failed");
            }
            let fails: Option<(i32,)> = sqlx::query_as(
                "INSERT INTO monitor_progress (user_id, target_id, last_message_id, last_checked_at, fail_count) VALUES ($1, $2, -1, NOW(), $3) \
                 ON CONFLICT (user_id) DO UPDATE SET last_checked_at = NOW(), \
                 fail_count = CASE WHEN $4 THEN monitor_progress.fail_count + 1 ELSE 0 END RETURNING fail_count",
            )
            .bind(user_id)
            .bind(target)
            .bind(i32::from(source_lost))
            .bind(source_lost)
            .fetch_optional(&bot.pg)
            .await
            .ok()
            .flatten();
            // A source unreachable for a day of hourly checks is gone for this account.
            if source_lost && fails.is_some_and(|f| f.0 >= SOURCE_LOST_CHECKS) {
                sqlx::query("UPDATE users SET monitor_target_id = NULL, monitor_flags = NULL WHERE user_id = $1 AND monitor_target_id = $2")
                    .bind(user_id)
                    .bind(target)
                    .execute(&bot.pg)
                    .await
                    .ok();
                let lost: Option<(Option<String>, Option<String>)> =
                    sqlx::query_as("DELETE FROM monitor_progress WHERE user_id = $1 RETURNING target_title, target_username").bind(user_id).fetch_optional(&bot.pg).await.ok().flatten();
                let lang = bot.lang(user_id).await;
                let (title, username) = lost.unwrap_or((None, None));
                let shown = describe(&bot, &lang, title.as_deref(), username.as_deref()).await;
                bot.send_to(
                    user_id,
                    &bot.t(
                        &lang,
                        "monitor.source_lost",
                        &[
                            ("channel_title", &shown),
                            ("channel_id", &target.to_string()),
                        ],
                    ),
                    None,
                )
                .await
                .ok();
            }
        }
        tokio::time::sleep(Duration::from_secs(if rows.is_empty() { 60 } else { 5 })).await;
    }
}

async fn claim_job(pg: &sqlx::PgPool) -> sqlx::Result<Option<(i64, i64, i64, i64)>> {
    let mut tx = pg.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(QUEUE_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let row: Option<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT j.job_id, j.user_id, j.target_id, j.message_id FROM monitor_jobs j \
         JOIN users u ON u.user_id = j.user_id AND u.monitor_target_id = j.target_id \
         WHERE j.status = 'queued' AND j.available_at <= NOW() AND u.is_premium AND (u.premium_expiry_date IS NULL OR u.premium_expiry_date > NOW()) \
         AND NOT EXISTS (SELECT 1 FROM task_queue q WHERE q.user_id = j.user_id AND q.status = 'running') \
         AND NOT EXISTS (SELECT 1 FROM monitor_jobs m WHERE m.user_id = j.user_id AND m.status = 'running') \
         AND NOT EXISTS (SELECT 1 FROM monitor_jobs m WHERE m.user_id = j.user_id AND m.status = 'queued' AND m.job_id < j.job_id) \
         ORDER BY j.job_id FOR UPDATE OF j SKIP LOCKED LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((job_id, ..)) = &row {
        sqlx::query(
            "UPDATE monitor_jobs SET status = 'running', attempts = attempts + 1 WHERE job_id = $1",
        )
        .bind(job_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(row)
}

async fn set_status(pg: &sqlx::PgPool, job_id: i64, status: &str) {
    let _ = sqlx::query("UPDATE monitor_jobs SET status = $2 WHERE job_id = $1")
        .bind(job_id)
        .bind(status)
        .execute(pg)
        .await;
}

async fn deliver_one(
    bot: &Arc<Bot>,
    job_id: i64,
    user_id: i64,
    target: i64,
    message_id: i64,
) -> Result<()> {
    let settings: Option<(Option<i64>, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT monitor_target_id, monitor_flags, upload_dest_id FROM users WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(&bot.pg)
    .await?;
    let Some((Some(current), flags_json, upload_dest)) = settings else {
        set_status(&bot.pg, job_id, "done").await;
        return Ok(());
    };
    // A monitored channel that is also the upload channel would receive every delivery as a new post,
    // and deliver it again without end.
    if current != target || upload_dest == Some(target) {
        set_status(&bot.pg, job_id, "done").await;
        return Ok(());
    }
    let flags: Vec<String> = flags_json
        .and_then(|f| serde_json::from_str(&f).ok())
        .unwrap_or_default();
    let flags: Vec<String> = flags.into_iter().filter(|f| f != "--fdl").collect();
    let flag_refs: Vec<&str> = flags.iter().map(String::as_str).collect();
    let filter = extract_filter_settings(&flag_refs);

    let _guard = ACCOUNT_LOCK.lock().await;
    let (account, owned) = open_account(bot, user_id, target).await?;
    let result = async {
        let client = account.client().await;
        let peer = peer_in(&account, target)
            .await
            .ok_or_else(|| anyhow::anyhow!("source is inaccessible"))?;
        let Some(post) = access::get_message(&client, peer, message_id).await? else {
            set_status(&bot.pg, job_id, "done").await;
            return Ok(());
        };
        let Some(raw) = raw_message(&post) else {
            set_status(&bot.pg, job_id, "done").await;
            return Ok(());
        };
        let duration = info::downloadable(raw).map_or(0, |i| i.duration.max(0.0) as u64);
        if !should_process(info::message_type(raw), &filter)
            || !crate::filters::duration_allowed(duration, &filter)
        {
            set_status(&bot.pg, job_id, "done").await;
            return Ok(());
        }
        let lang = bot.lang(user_id).await;
        let is_premium = true;
        let ctx = Arc::new(TaskCtx {
            bot: bot.clone(),
            user_id,
            lang,
            is_premium,
            queue_id: None,
            status: None,
            cancel: tokio_util::sync::CancellationToken::new(),
        });
        let delivery = Job {
            ctx: &ctx,
            account: account.clone(),
            force_download: false,
            is_batch: true,
            wait_for_choice: false,
            policy: Default::default(),
        };
        let delivered = delivery.deliver(&post).await?;
        match delivered.skip {
            Some(crate::tasks::download::Skip::Unanswered) => {
                let _ = sqlx::query(
                    "UPDATE monitor_jobs SET status = 'queued', attempts = GREATEST(attempts - 1, 0), \
                     available_at = NOW() + INTERVAL '10 seconds' WHERE job_id = $1",
                )
                .bind(job_id)
                .execute(&bot.pg)
                .await;
                return Ok(());
            }
            Some(crate::tasks::download::Skip::Cancelled) => {
                set_status(&bot.pg, job_id, "done").await;
                return Ok(());
            }
            None => {}
        }
        if !delivered.ok {
            anyhow::bail!("monitor post {target}/{message_id} was not delivered");
        }
        set_status(&bot.pg, job_id, "done").await;
        anyhow::Ok(())
    }
    .await;
    if owned {
        bot.accounts.release_linked(user_id).await;
    }
    result
}

async fn delivery_worker(bot: Arc<Bot>) {
    loop {
        let row = match claim_job(&bot.pg).await {
            Ok(row) => row,
            Err(error) => {
                tracing::error!(%error, "could not claim a monitor job");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let Some((job_id, user_id, target, message_id)) = row else {
            tokio::select! {
                _ = WAKE.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            }
            continue;
        };
        if let Err(error) = deliver_one(&bot, job_id, user_id, target, message_id).await {
            tracing::error!(job_id, %error, "monitor delivery failed");
            let status: Option<(String,)> = sqlx::query_as(
                "UPDATE monitor_jobs SET status = CASE WHEN attempts >= 3 THEN 'failed' ELSE 'queued' END, \
                 available_at = NOW() + INTERVAL '5 minutes' WHERE job_id = $1 RETURNING status",
            )
            .bind(job_id)
            .fetch_optional(&bot.pg)
            .await
            .ok()
            .flatten();
            if status.is_some_and(|s| s.0 == "failed") {
                let lang = bot.lang(user_id).await;
                bot.send_to(
                    user_id,
                    &bot.t(&lang, "downloads.download_failed", &[]),
                    None,
                )
                .await
                .ok();
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        bot.queue_wake.notify_waiters();
    }
}

/// Jobs left running by a crash are queued again.
pub async fn requeue_running(pg: &sqlx::PgPool) {
    let _ = sqlx::query("UPDATE monitor_jobs SET status = 'queued' WHERE status = 'running'")
        .execute(pg)
        .await;
}

fn stop_markup(bot: &Bot, lang: &str, channel: i64) -> ReplyMarkup {
    keyboard(&[vec![Button::callback(
        &bot.t(lang, "monitor.stop_button", &[]),
        &format!("mon_off_{channel}"),
        Color::Red,
    )]])
}

/// `/monitor`, with no arguments, `<channel> on [filters]` or `<channel> off`.
pub async fn command(bot: &Arc<Bot>, message: &Message, who: &Who, args: &[&str]) -> Result<()> {
    run_command(bot, who.id, args, Reply::To(message)).await
}

/// Where a monitor answer goes: a reply to the user's command, the message whose button was pressed, or a
/// new message.
#[derive(Clone, Copy)]
pub enum Reply<'a> {
    To(&'a Message),
    Edit(&'a grammers_client::message::Message),
    New,
}

async fn say(
    bot: &Bot,
    message: Reply<'_>,
    user_id: i64,
    text: &str,
    markup: Option<ReplyMarkup>,
) -> Result<()> {
    match message {
        Reply::To(message) => {
            bot.reply(message, text, markup).await?;
        }
        Reply::Edit(message) => {
            message.edit(crate::bot::html(text, markup)).await?;
        }
        Reply::New => {
            bot.send_to(user_id, text, markup).await?;
        }
    }
    Ok(())
}

pub async fn run_command(
    bot: &Arc<Bot>,
    user_id: i64,
    args: &[&str],
    message: Reply<'_>,
) -> Result<()> {
    let lang = bot.lang(user_id).await;
    if !account::load(&bot.pg, user_id)
        .await?
        .is_some_and(|u| u.is_premium)
    {
        let markup = keyboard(&[vec![Button::callback(
            &bot.t(&lang, "buttons.subscribe", &[]),
            "show_premium",
            Color::Green,
        )]]);
        return say(
            bot,
            message,
            user_id,
            &bot.t(&lang, "premium.feature_required", &[]),
            Some(markup),
        )
        .await;
    }
    let current: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT monitor_target_id FROM users WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(&bot.pg)
            .await?;
    let current = current.and_then(|c| c.0);
    if args.len() < 2 {
        return match current {
            Some(target) => {
                let status = bot.t(
                    &lang,
                    "monitor.status.active",
                    &[("channel_id", &target.to_string())],
                );
                say(
                    bot,
                    message,
                    user_id,
                    &bot.t(&lang, "monitor.usage", &[("current_status", &status)]),
                    Some(stop_markup(bot, &lang, target)),
                )
                .await
            }
            None => match message {
                Reply::To(message) => {
                    let who = Who {
                        id: user_id,
                        first_name: String::new(),
                        last_name: None,
                        username: None,
                        lang_code: None,
                    };
                    crate::bot::begin_guide(bot, message, &who, "monitor").await
                }
                Reply::Edit(_) | Reply::New => Ok(()),
            },
        };
    }
    let action = args[1].to_lowercase();
    if action != "on" && action != "off" {
        return say(
            bot,
            message,
            user_id,
            &bot.t(&lang, "monitor.invalid_action", &[]),
            None,
        )
        .await;
    }
    let Ok(chat) = parse_chat_reference(args[0]) else {
        return say(
            bot,
            message,
            user_id,
            &bot.t(&lang, "monitor.access_denied", &[]),
            None,
        )
        .await;
    };
    if action == "off" {
        if current.is_none() {
            return say(
                bot,
                message,
                user_id,
                &bot.t(&lang, "monitor.not_active", &[]),
                None,
            )
            .await;
        }
        stop_monitor(bot, user_id).await;
        sqlx::query(
            "UPDATE monitor_jobs SET status = 'cancelled' WHERE user_id = $1 AND status = 'queued'",
        )
        .bind(user_id)
        .execute(&bot.pg)
        .await?;
        return say(
            bot,
            message,
            user_id,
            &bot.t(&lang, "monitor.stopped", &[]),
            None,
        )
        .await;
    }
    if current.is_some() {
        return say(
            bot,
            message,
            user_id,
            &bot.t(&lang, "monitor.already_active", &[]),
            None,
        )
        .await;
    }

    let _guard = ACCOUNT_LOCK.lock().await;
    // Public usernames are read through the pool; anything else needs the user's own account.
    let (account, owned) = match (&chat, bot.accounts.first_pool().await) {
        (Chat::Username(_), Some(pool)) => (pool, false),
        _ => match bot.accounts.linked(user_id).await {
            Ok(Some(account)) => (account, true),
            _ => {
                let markup = keyboard(&[
                    vec![Button::callback(
                        &bot.t(&lang, "buttons.link_account", &[]),
                        "start_login",
                        Color::Green,
                    )],
                    vec![bot.button(&lang, "❌", "cancel_download", Color::Red)],
                ]);
                return say(
                    bot,
                    message,
                    user_id,
                    &bot.t(&lang, "monitor.login_required", &[]),
                    Some(markup),
                )
                .await;
            }
        },
    };
    let outcome = async {
        let peer = match access::resolve_chat(&account, &chat).await {
            Ok(peer) => peer,
            Err(_) => return Ok(Err("monitor.access_denied")),
        };
        let client = account.client().await;
        let found = match client.resolve_peer(peer).await {
            Ok(found) => found,
            Err(error) if is_private_access_error(&error) => {
                return Ok(Err("monitor.access_denied"));
            }
            Err(error) => return Err(anyhow::Error::from(error)),
        };
        match access::edge_id(&client, peer, false).await {
            Ok(Some(latest)) => anyhow::Ok(Ok((
                peer,
                found.name().map(str::to_string),
                found.username().map(str::to_string),
                latest,
            ))),
            Ok(None) => Ok(Err("monitor.start_failed")),
            Err(error) if is_private_access_error(&error) => Ok(Err("monitor.access_denied")),
            Err(error) => Err(error.into()),
        }
    }
    .await;
    if owned {
        bot.accounts.release_linked(user_id).await;
    }
    let (peer, title, username, latest) = match outcome? {
        Ok(found) => found,
        Err(key) => return say(bot, message, user_id, &bot.t(&lang, key, &[]), None).await,
    };
    let Some(target) = peer.id.bot_api_dialog_id() else {
        return Ok(());
    };
    if username.is_some()
        && let Some(pool) = bot.accounts.first_pool().await
    {
        // The pool account joins public channels so it receives their posts live.
        let joined = pool
            .client()
            .await
            .invoke(&tl::functions::channels::JoinChannel {
                channel: (&peer).into(),
            })
            .await;
        if let Err(error) = joined
            && !error.is("USER_ALREADY_PARTICIPANT")
        {
            tracing::info!(channel = target, %error, "public monitor uses hourly scans");
        }
    }
    let flags: Vec<String> = args[2..]
        .iter()
        .filter(|f| **f != "--fdl")
        .map(|f| f.to_string())
        .collect();
    let mut tx = bot.pg.begin().await?;
    sqlx::query("UPDATE users SET monitor_target_id = $2, monitor_flags = $3 WHERE user_id = $1")
        .bind(user_id)
        .bind(target)
        .bind(json!(flags).to_string())
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO monitor_progress (user_id, target_id, last_message_id, last_checked_at, fail_count, target_title, target_username) \
         VALUES ($1, $2, $3, NOW(), 0, $4, $5) ON CONFLICT (user_id) DO UPDATE SET target_id = EXCLUDED.target_id, \
         last_message_id = EXCLUDED.last_message_id, last_checked_at = NOW(), fail_count = 0, \
         target_title = EXCLUDED.target_title, target_username = EXCLUDED.target_username",
    )
    .bind(user_id)
    .bind(target)
    .bind(latest)
    .bind(&title)
    .bind(&username)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let shown = title.unwrap_or_else(|| target.to_string());
    let summary = bot.t(&lang, "filters.summary.all", &[]);
    say(
        bot,
        message,
        user_id,
        &bot.t(
            &lang,
            "monitor.started",
            &[
                ("channel_title", &crate::bot::escape(&shown)),
                ("filter_status", &summary),
            ],
        ),
        Some(stop_markup(bot, &lang, target)),
    )
    .await
}

/// The stop button: `mon_off_<channel>`.
pub async fn stop_button(
    bot: &Arc<Bot>,
    query: &CallbackQuery,
    who: &Who,
    data: &str,
) -> Result<()> {
    let channel = data.trim_start_matches("mon_off_");
    query.answer().send().await.ok();
    // The answer replaces the message the button sits on.
    match query.load_message().await {
        Ok(message) => run_command(bot, who.id, &[channel, "off"], Reply::Edit(&message)).await,
        Err(_) => run_command(bot, who.id, &[channel, "off"], Reply::New).await,
    }
}
