//! `/bdl` (a range of posts) and `/clone` (a whole channel): one loop, progress saved after every item.

use std::{collections::HashSet, sync::Arc, time::Duration};

use grammers_client::{message::Message, session::types::PeerRef};
use serde_json::{Value, json};

use super::{
    Outcome, TaskCtx,
    access::{self, AccessError},
    download::{self, Job},
    notices,
};
use crate::{
    filters::{FilterSettings, duration_allowed, extract_filter_settings, should_process},
    links::{parse_chat_reference, parse_message_link},
    media::info,
    store::{account, active},
};

const MAX_TRACKED_FAILED: usize = 500;
/// Ids fetched per request.
const FETCH_CHUNK: i64 = 50;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Bdl,
    Clone,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Bdl => "bdl",
            Kind::Clone => "clone",
        }
    }

    fn key(self, bdl: &'static str, clone: &'static str) -> &'static str {
        match self {
            Kind::Bdl => bdl,
            Kind::Clone => clone,
        }
    }
}

#[derive(Clone, Default)]
pub struct Resume {
    pub from: i64,
    pub task_id: i64,
    pub failed: Vec<i64>,
    pub retry_phase: bool,
}

pub struct Range {
    pub kind: Kind,
    pub payload: Value,
    pub flags: Vec<String>,
    pub resume: Option<Resume>,
}

fn flag_refs(flags: &[String]) -> Vec<&str> {
    flags.iter().map(String::as_str).collect()
}

fn item_duration(message: &Message) -> u64 {
    download::raw_message(message)
        .and_then(info::downloadable)
        .map_or(0, |i| i.duration.max(0.0) as u64)
}

fn passes(message: &Message, settings: &FilterSettings) -> Option<&'static str> {
    // Some(reason) means skipped; None means it passes.
    let raw = download::raw_message(message)?;
    if !should_process(info::message_type(raw), settings) {
        return Some("type");
    }
    if (settings.min_duration.is_some() || settings.max_duration.is_some())
        && !duration_allowed(item_duration(message), settings)
    {
        return Some("duration");
    }
    None
}

struct Source {
    peer: PeerRef,
    /// Link shown on the "open source" button.
    url: String,
}

async fn resolve_source(
    ctx: &Arc<TaskCtx>,
    range: &Range,
    account: &crate::telegram::accounts::Account,
) -> Option<Source> {
    let (chat, url) = match range.kind {
        Kind::Bdl => {
            let start = range.payload["link_start"].as_str().unwrap_or("");
            (parse_message_link(start).ok()?.chat, start.to_string())
        }
        Kind::Clone => {
            let channel = range.payload["channel_url"].as_str().unwrap_or("");
            (parse_chat_reference(channel).ok()?, channel.to_string())
        }
    };
    match access::resolve_chat(account, &chat).await {
        Ok(peer) => Some(Source { peer, url }),
        Err(AccessError::Private) => {
            if account.is_linked() {
                notices::not_member(ctx, Some(&url)).await;
            } else {
                notices::login_required(ctx).await;
            }
            None
        }
        Err(AccessError::NotFound) => {
            ctx.report(&ctx.bot.t(&ctx.lang, "downloads.post_not_found", &[]), None)
                .await;
            None
        }
        Err(AccessError::Rpc(error)) => {
            tracing::warn!(user = ctx.user_id, %error, "could not resolve the source chat");
            None
        }
    }
}

pub async fn run(ctx: &Arc<TaskCtx>, range: Range) -> Outcome {
    let bot = &ctx.bot;
    let t = |key: &str, args: &[(&str, &str)]| bot.t(&ctx.lang, key, args);
    let kind = range.kind;

    let account = match kind {
        Kind::Clone => match bot.accounts.linked(ctx.user_id).await {
            Ok(Some(account)) => account,
            _ => {
                notices::login_required(ctx).await;
                return Outcome::LoginRequired;
            }
        },
        Kind::Bdl => match bot.accounts.for_download(ctx.user_id).await {
            Ok(account) => account,
            Err(error) => {
                tracing::error!(user = ctx.user_id, %error, "no batch download account");
                ctx.report(&t("downloads.client_unavailable", &[]), None)
                    .await;
                return Outcome::Failed;
            }
        },
    };
    let _busy = account.busy();
    let client = account.client().await;
    let Some(source) = resolve_source(ctx, &range, &account).await else {
        return Outcome::Failed;
    };

    let (mut start_id, mut end_id) = match kind {
        Kind::Bdl => {
            let start = parse_message_link(range.payload["link_start"].as_str().unwrap_or(""))
                .map(|l| l.message_id);
            let end = parse_message_link(range.payload["link_end"].as_str().unwrap_or(""))
                .map(|l| l.message_id);
            let (Ok(start), Ok(end)) = (start, end) else {
                ctx.report(&t("downloads.batch.invalid_links", &[]), None)
                    .await;
                return Outcome::Failed;
            };
            (start, end)
        }
        Kind::Clone => {
            let newest = match access::edge_id(&client, source.peer, false).await {
                Ok(Some(id)) => id,
                Ok(None) => {
                    ctx.report(&t("clone.empty_channel", &[]), None).await;
                    return Outcome::Failed;
                }
                Err(error) if access::is_private_access_error(&error) => {
                    notices::not_member(ctx, Some(&source.url)).await;
                    return Outcome::NotMember;
                }
                Err(error) => {
                    tracing::warn!(user = ctx.user_id, %error, "could not read the channel");
                    ctx.report(&t("clone.channel_not_found", &[]), None).await;
                    return Outcome::Failed;
                }
            };
            let oldest = access::edge_id(&client, source.peer, true)
                .await
                .ok()
                .flatten()
                .unwrap_or(1);
            (oldest, newest)
        }
    };

    // The range stops at the newest post that really exists, not at a far-away id.
    if kind == Kind::Bdl
        && range.resume.is_none()
        && let Ok(Some(newest)) = access::edge_id(&client, source.peer, false).await
        && end_id > newest
    {
        end_id = newest.max(start_id);
    }
    let total_cost = end_id - start_id + 1;

    // Probe the first post so a source the account cannot read ends the task early.
    if kind == Kind::Bdl {
        match access::get_message(&client, source.peer, start_id).await {
            Err(error) if access::is_private_access_error(&error) => {
                if account.is_linked() {
                    notices::not_member(ctx, Some(&source.url)).await;
                } else {
                    notices::login_required(ctx).await;
                }
                return Outcome::Failed;
            }
            Err(error) if access::Dead::is_dead(&error) => {
                if account.is_linked() {
                    bot.accounts.forget_linked(ctx.user_id).await.ok();
                    notices::session_expired(ctx).await;
                } else {
                    ctx.report(&t("downloads.client_unavailable", &[]), None)
                        .await;
                }
                return Outcome::Failed;
            }
            _ => {}
        }
    }

    let flags = flag_refs(&range.flags);
    let settings = extract_filter_settings(&flags);
    let force = account::load(&bot.pg, ctx.user_id)
        .await
        .ok()
        .flatten()
        .is_some_and(|u| u.force_download_enabled && u.is_premium);
    let status_id = ctx.status.as_ref().map_or(0, |s| i64::from(s.id));

    let task_id = match &range.resume {
        Some(resume) => resume.task_id,
        None => {
            let saved = active::save(
                &bot.pg,
                ctx.user_id,
                kind.word(),
                &range.payload,
                &range.flags,
                start_id,
                end_id,
                status_id,
            )
            .await;
            match saved {
                Ok(id) => {
                    if let Some(queue_id) = ctx.queue_id {
                        crate::store::queue::set_spec(
                            &bot.pg,
                            queue_id,
                            &json!({"active_task_id": id}),
                        )
                        .await
                        .ok();
                    }
                    id
                }
                Err(error) => {
                    tracing::error!(user = ctx.user_id, %error, "could not save the task");
                    ctx.report(
                        &t(kind.key("downloads.batch.failed", "clone.failed"), &[]),
                        None,
                    )
                    .await;
                    return Outcome::Failed;
                }
            }
        }
    };

    if kind == Kind::Bdl && range.resume.is_none() {
        match account::spend_points(&bot.pg, ctx.user_id, total_cost).await {
            Ok(true) => {}
            _ => {
                active::clear(&bot.pg, task_id).await;
                let available = account::load(&bot.pg, ctx.user_id)
                    .await
                    .ok()
                    .flatten()
                    .map_or(0, |u| {
                        u.daily_downloads_left + u.gift_points + u.referral_points
                    });
                ctx.report(
                    &t(
                        "limits.insufficient_points_batch",
                        &[
                            ("required_points", &total_cost.to_string()),
                            ("available_points", &available.to_string()),
                        ],
                    ),
                    None,
                )
                .await;
                return Outcome::Failed;
            }
        }
    }

    // The starting message of the task.
    if range.resume.is_none() {
        let text = match kind {
            Kind::Bdl => t(
                "downloads.batch.starting",
                &[
                    ("start_id", &start_id.to_string()),
                    ("end_id", &end_id.to_string()),
                    ("total", &total_cost.to_string()),
                ],
            ),
            Kind::Clone => {
                let title = range.payload["channel_url"].as_str().unwrap_or("");
                t(
                    "clone.starting",
                    &[
                        ("channel_title", &crate::bot::escape(title)),
                        ("total", &end_id.to_string()),
                    ],
                )
            }
        };
        ctx.show(&text).await;
    }

    let resume = range.resume.clone();
    let resume_from = resume.as_ref().map(|r| r.from);
    let mut failed_tracked: Vec<i64> = resume
        .as_ref()
        .map(|r| r.failed.clone())
        .unwrap_or_default();
    let mut retrying = resume.as_ref().is_some_and(|r| r.retry_phase) && !failed_tracked.is_empty();
    if kind == Kind::Clone {
        start_id = start_id.min(end_id);
    }
    let mut pending: Vec<i64> = if retrying {
        failed_tracked.clone()
    } else {
        (resume_from.unwrap_or(start_id)..=end_id).collect()
    };
    if pending.is_empty() {
        active::clear(&bot.pg, task_id).await;
        return Outcome::Done;
    }

    let mut downloaded = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut groups: HashSet<i64> = HashSet::new();
    let mut last_sent: Option<Message> = None;
    let mut mode = None;
    let job = Job {
        ctx,
        account: account.clone(),
        force_download: force,
        is_batch: true,
        wait_for_choice: true,
        policy: Default::default(),
    };

    loop {
        let total_remaining = pending.len();
        let mut cache: std::collections::HashMap<i64, Option<Message>> =
            std::collections::HashMap::new();
        for (index, &message_id) in pending.iter().enumerate() {
            let current = index + 1;
            if kind == Kind::Clone {
                if current % 50 == 0 {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                if current % 500 == 0 {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
            let header = t(
                kind.key("downloads.batch.progress", "clone.progress"),
                &[
                    ("current", &current.to_string()),
                    ("total", &total_remaining.to_string()),
                    ("message_id", &message_id.to_string()),
                    ("downloaded", &downloaded.to_string()),
                    ("skipped", &skipped.to_string()),
                    ("failed", &failed.to_string()),
                ],
            );
            if let Some(status) = &ctx.status {
                status.set_header(Some(header.clone()));
            }
            ctx.show(&header).await;

            // Posts are fetched fifty ids at a time; a retry pass reads one by one.
            if !cache.contains_key(&message_id) {
                let ids: Vec<i32> = if retrying {
                    vec![message_id as i32]
                } else {
                    (message_id..(message_id + FETCH_CHUNK).min(end_id + 1))
                        .map(|i| i as i32)
                        .collect()
                };
                match access::get_many(&client, source.peer, &ids).await {
                    Ok(found) => {
                        for (id, message) in ids.iter().zip(found) {
                            cache.insert(i64::from(*id), message);
                        }
                    }
                    Err(error) if access::is_private_access_error(&error) => {
                        if account.is_linked() {
                            notices::not_member(ctx, Some(&source.url)).await;
                        } else {
                            notices::login_required(ctx).await;
                        }
                        active::clear(&bot.pg, task_id).await;
                        return Outcome::Failed;
                    }
                    Err(error) if access::Dead::is_dead(&error) => {
                        if account.is_linked() {
                            bot.accounts.forget_linked(ctx.user_id).await.ok();
                            notices::session_expired(ctx).await;
                        }
                        active::clear(&bot.pg, task_id).await;
                        return Outcome::Failed;
                    }
                    Err(grammers_client::InvocationError::Rpc(rpc))
                        if rpc.name.starts_with("FLOOD") =>
                    {
                        let wait = u64::from(rpc.value.unwrap_or(30));
                        let text = t(
                            kind.key("downloads.flood_wait", "clone.flood_wait"),
                            &[("seconds", &wait.to_string())],
                        );
                        ctx.show(&text).await;
                        tokio::time::sleep(Duration::from_secs(wait + 2)).await;
                        record(&mut failed, &mut failed_tracked, retrying, message_id);
                        save(
                            &bot.pg,
                            task_id,
                            message_id,
                            &failed_tracked,
                            retrying,
                            end_id,
                        )
                        .await;
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(user = ctx.user_id, message_id, %error, "could not fetch a post");
                        record(&mut failed, &mut failed_tracked, retrying, message_id);
                        save(
                            &bot.pg,
                            task_id,
                            message_id,
                            &failed_tracked,
                            retrying,
                            end_id,
                        )
                        .await;
                        continue;
                    }
                }
            }
            let Some(source_message) = cache.remove(&message_id).flatten() else {
                skipped += 1;
                save(
                    &bot.pg,
                    task_id,
                    message_id,
                    &failed_tracked,
                    retrying,
                    end_id,
                )
                .await;
                continue;
            };
            let Some(raw) = download::raw_message(&source_message) else {
                skipped += 1;
                save(
                    &bot.pg,
                    task_id,
                    message_id,
                    &failed_tracked,
                    retrying,
                    end_id,
                )
                .await;
                continue;
            };
            let _ = raw;

            if let Some(group) = source_message.grouped_id()
                && !groups.insert(group)
            {
                // Album members are delivered with the first message of their group.
                save(
                    &bot.pg,
                    task_id,
                    message_id,
                    &failed_tracked,
                    retrying,
                    end_id,
                )
                .await;
                continue;
            }

            let delivered = if source_message.grouped_id().is_some() {
                deliver_album(&job, &client, source.peer, &source_message, &settings).await
            } else {
                if passes(&source_message, &settings).is_some() {
                    skipped += 1;
                    save(
                        &bot.pg,
                        task_id,
                        message_id,
                        &failed_tracked,
                        retrying,
                        end_id,
                    )
                    .await;
                    continue;
                }
                match job.deliver(&source_message).await {
                    Ok(d) => (usize::from(d.ok), d.mode, d.last, true),
                    Err(error) => {
                        tracing::warn!(user = ctx.user_id, message_id, %error, "could not process a post");
                        (0, None, None, false)
                    }
                }
            };
            let (count, delivered_mode, delivered_last, ok) = delivered;
            if ok && count > 0 {
                downloaded += count;
                mode = delivered_mode.or(mode);
                last_sent = delivered_last.or(last_sent);
            } else if ok {
                skipped += 1;
            } else {
                record(&mut failed, &mut failed_tracked, retrying, message_id);
            }
            save(
                &bot.pg,
                task_id,
                message_id,
                &failed_tracked,
                retrying,
                end_id,
            )
            .await;
            tokio::time::sleep(Duration::from_millis(if kind == Kind::Clone {
                500
            } else {
                1000
            }))
            .await;
        }

        if retrying || kind == Kind::Clone || failed_tracked.is_empty() {
            break;
        }
        retrying = true;
        failed = 0;
        pending = failed_tracked.clone();
        active::progress(&bot.pg, task_id, end_id, Some(&pending), Some(true)).await;
        ctx.show(&t(
            "downloads.batch.retrying",
            &[("count", &pending.len().to_string())],
        ))
        .await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    if let Some(status) = &ctx.status {
        status.set_header(None);
    }
    let summary = t(
        kind.key("downloads.batch.completed", "clone.completed"),
        &[
            ("downloaded", &downloaded.to_string()),
            ("skipped", &skipped.to_string()),
            ("failed", &failed.to_string()),
        ],
    );
    active::clear(&bot.pg, task_id).await;
    let mut final_text = summary;
    if downloaded > 0 {
        let delivery_mode = match mode {
            Some(m) => m,
            None => download::destination(ctx, &account)
                .await
                .map(|d| d.mode)
                .unwrap_or(download::Mode::Private),
        };
        let completion = download::completion_text(
            ctx,
            kind.key("batch", "clone"),
            delivery_mode,
            downloaded,
            last_sent.as_ref(),
        )
        .await;
        final_text = format!("{final_text}\n\n{completion}");
        crate::bot::start_cooldown(bot, ctx.user_id).await;
    }
    ctx.report(&final_text, None).await;
    Outcome::Done
}

fn record(failed: &mut usize, tracked: &mut Vec<i64>, retrying: bool, id: i64) {
    *failed += 1;
    if !retrying && !tracked.contains(&id) && tracked.len() < MAX_TRACKED_FAILED {
        tracked.push(id);
    }
}

async fn save(
    pg: &sqlx::PgPool,
    task_id: i64,
    id: i64,
    tracked: &[i64],
    retrying: bool,
    end_id: i64,
) {
    if retrying {
        // A retried id that was handled drops out of the saved list.
        let remaining: Vec<i64> = tracked.iter().copied().filter(|t| *t != id).collect();
        active::progress(pg, task_id, end_id, Some(&remaining), Some(true)).await;
    } else {
        active::progress(pg, task_id, id, Some(tracked), None).await;
    }
}

async fn deliver_album(
    job: &Job<'_>,
    client: &grammers_client::Client,
    peer: PeerRef,
    first: &Message,
    settings: &FilterSettings,
) -> (usize, Option<download::Mode>, Option<Message>, bool) {
    let group = match access::media_group(client, peer, first).await {
        Ok(group) => group,
        Err(error) => {
            tracing::warn!(user = job.ctx.user_id, %error, "could not load an album");
            return (0, None, None, false);
        }
    };
    let mut done = 0;
    let mut mode = None;
    let mut last = None;
    for member in group {
        if passes(&member, settings).is_some() {
            continue;
        }
        if let Ok(result) = job.deliver(&member).await
            && result.ok
        {
            done += 1;
            mode = result.mode.or(mode);
            last = result.last.or(last);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    (done, mode, last, true)
}
