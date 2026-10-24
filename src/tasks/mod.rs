//! User tasks: put in the queue, wait for a turn, run, and report the end to the task's status message.

pub mod access;
pub mod download;
pub mod large;
pub mod notices;
pub mod progress;
pub mod range;
pub mod single;
pub mod status;

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use anyhow::Result;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    bot::Bot,
    queue::{Turn, open_task_limit},
    store::{account, queue},
};
pub use status::Status;

const RECHECK: Duration = Duration::from_secs(10);
const NOTICE_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// How a job ended, in the words the Python bot used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Downloaded,
    Skipped,
    Failed,
    LoginRequired,
    NotMember,
    Done,
}

/// Everything a running task needs, passed down explicitly instead of through context variables.
pub struct TaskCtx {
    pub bot: Arc<Bot>,
    pub user_id: i64,
    pub lang: String,
    pub is_premium: bool,
    pub queue_id: Option<i64>,
    pub status: Option<Arc<Status>>,
    pub cancel: CancellationToken,
}

impl TaskCtx {
    /// Edits the task's status message with text that is not the end of the task.
    pub async fn show(&self, text: &str) {
        if let Some(status) = &self.status {
            status.edit(&self.bot, text, None).await;
        }
    }

    /// The end message of a task. For a queued task it replaces the status message; otherwise it is sent as new.
    pub async fn report(&self, text: &str, markup: Option<grammers_client::message::ReplyMarkup>) {
        match (&self.status, self.queue_id) {
            (Some(status), Some(_)) => {
                status.set_final(true, Some(text.to_string()), markup.map(|m| m.raw));
                status.edit(&self.bot, text, None).await;
            }
            _ => {
                self.bot.send_to(self.user_id, text, markup).await.ok();
            }
        }
    }
}

pub type Job =
    Box<dyn FnOnce(Arc<TaskCtx>) -> Pin<Box<dyn Future<Output = Outcome> + Send>> + Send>;

async fn wait_for_turn(bot: &Arc<Bot>, ctx: &TaskCtx, queue_id: i64) -> Result<bool> {
    let mut last_position: Option<i64> = None;
    let mut last_notice = tokio::time::Instant::now() - NOTICE_MIN_INTERVAL;
    loop {
        match queue::try_start(&bot.pg, queue_id).await? {
            Turn::Start => {
                ctx.show(&bot.t(&ctx.lang, "common.processing", &[])).await;
                return Ok(true);
            }
            Turn::Wait(0) => return Ok(false),
            Turn::Wait(position) => {
                if last_position != Some(position) && last_notice.elapsed() >= NOTICE_MIN_INTERVAL {
                    let key = if ctx.is_premium {
                        "queue.waiting_premium"
                    } else {
                        "queue.waiting_free"
                    };
                    let text = bot.t(
                        &ctx.lang,
                        key,
                        &[
                            ("task_id", &queue_id.to_string()),
                            ("position", &position.to_string()),
                            ("ahead", &(position - 1).max(0).to_string()),
                        ],
                    );
                    ctx.show(&text).await;
                    last_position = Some(position);
                    last_notice = tokio::time::Instant::now();
                }
            }
        }
        tokio::select! {
            _ = bot.queue_wake.notified() => {}
            _ = tokio::time::sleep(RECHECK) => {}
            _ = ctx.cancel.cancelled() => return Ok(false),
        }
    }
}

/// Puts a user task in the queue and runs it when its turn comes. Returns false when it was refused
/// (the user's task limit) or could not start.
///
/// `resume` carries the queue row and status message of a task found open after a restart.
pub async fn track(
    bot: &Arc<Bot>,
    user_id: i64,
    kind: &str,
    spec: Value,
    job: Job,
    resume: Option<(i64, Option<i32>)>,
) -> Result<bool> {
    let lang = bot.lang(user_id).await;
    let is_premium = bot.is_staff(user_id).await
        || account::load(&bot.pg, user_id)
            .await?
            .is_some_and(|u| u.is_premium);

    let (queue_id, saved_status) = match resume.filter(|(id, _)| *id != 0) {
        Some((id, status_id)) => (id, status_id),
        None => match queue::add(&bot.pg, user_id, kind, &spec, is_premium).await? {
            Some(id) => (id, None),
            None => {
                let limit = open_task_limit(is_premium);
                let key = if is_premium {
                    "queue.limit_premium"
                } else {
                    "queue.limit_free"
                };
                bot.send_to(
                    user_id,
                    &bot.t(&lang, key, &[("limit", &limit.to_string())]),
                    None,
                )
                .await
                .ok();
                return Ok(false);
            }
        },
    };

    let peer = bot
        .user_ref(user_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("user {user_id} is not known to the bot"))?;
    let accepted = bot.t(
        &lang,
        "queue.accepted",
        &[("task_id", &queue_id.to_string())],
    );
    let markup = status::queue_keyboard(bot, &lang, queue_id, true);
    let status = match saved_status {
        Some(id) => {
            let status = Status::new(peer, id, Some(queue_id), &lang);
            status.edit(bot, &accepted, Some(markup)).await;
            status
        }
        None => match bot
            .client
            .send_message(peer, crate::bot::html(&accepted, Some(markup)))
            .await
        {
            Ok(message) => Status::new(peer, message.id(), Some(queue_id), &lang),
            Err(error) => {
                queue::finish(&bot.pg, queue_id, "failed").await.ok();
                return Err(error.into());
            }
        },
    };
    queue::set_spec(&bot.pg, queue_id, &json!({"status_message_id": status.id})).await?;
    crate::store::states::delete(&bot.pg, user_id, "cancel_flag")
        .await
        .ok();

    let cancel = CancellationToken::new();
    bot.running.lock().unwrap().insert(queue_id, cancel.clone());
    let ctx = Arc::new(TaskCtx {
        bot: bot.clone(),
        user_id,
        lang: lang.clone(),
        is_premium,
        queue_id: Some(queue_id),
        status: Some(Arc::new(status)),
        cancel: cancel.clone(),
    });
    let bot = bot.clone();
    let kind = kind.to_string();
    tokio::spawn(async move {
        let outcome = run(&bot, &ctx, queue_id, &kind, job).await;
        bot.running.lock().unwrap().remove(&queue_id);
        bot.queue_wake.notify_waiters();
        tracing::info!(queue_id, user_id, ?outcome, "task ended");
    });
    Ok(true)
}

/// What the runner decided about a finished task.
#[derive(Debug)]
enum Ended {
    Cancelled,
    Failed,
    Result(Outcome),
    Shutdown,
}

async fn run(bot: &Arc<Bot>, ctx: &Arc<TaskCtx>, queue_id: i64, kind: &str, job: Job) -> Ended {
    let lang = &ctx.lang;
    let status = ctx
        .status
        .clone()
        .expect("a queued task always has a status message");

    let may_run = match wait_for_turn(bot, ctx, queue_id).await {
        Ok(may_run) => may_run,
        Err(error) => {
            tracing::error!(queue_id, %error, "queue wait failed");
            if !bot.is_shutting_down() {
                queue::finish(&bot.pg, queue_id, "failed").await.ok();
                ctx.report(&bot.t(lang, "common.something_went_wrong", &[]), None)
                    .await;
            }
            return Ended::Failed;
        }
    };
    if !may_run {
        if !bot.is_shutting_down() {
            ctx.report(
                &bot.t(
                    lang,
                    "queue.cancelled_one",
                    &[("task_id", &queue_id.to_string())],
                ),
                None,
            )
            .await;
        }
        return Ended::Cancelled;
    }
    queue::set_spec(&bot.pg, queue_id, &json!({"started": true}))
        .await
        .ok();
    status.set_final(false, None, None);

    let future = job(ctx.clone());
    let ended = tokio::select! {
        outcome = future => Ended::Result(outcome),
        _ = ctx.cancel.cancelled() => if bot.is_shutting_down() { Ended::Shutdown } else { Ended::Cancelled },
    };

    if matches!(ended, Ended::Shutdown) {
        // The row stays open, so the task resumes after the restart.
        return ended;
    }
    let mut final_status = "done";
    match &ended {
        Ended::Cancelled => {
            final_status = "cancelled";
            // A running batch or clone says what stops, as the Python bot did; anything else names the task.
            let text = match kind {
                "bdl" => bot.t(lang, "downloads.batch.cancelled", &[]),
                "clone" => bot.t(lang, "clone.cancelled", &[]),
                _ => bot.t(
                    lang,
                    "queue.cancelled_one",
                    &[("task_id", &queue_id.to_string())],
                ),
            };
            ctx.report(&text, None).await;
        }
        Ended::Failed => {
            final_status = "failed";
            ctx.report(&bot.t(lang, "common.something_went_wrong", &[]), None)
                .await;
        }
        Ended::Result(outcome) if !status.is_final() => {
            let text = match outcome {
                Outcome::Failed => {
                    final_status = "failed";
                    bot.t(lang, "downloads.download_failed", &[])
                }
                Outcome::Skipped => bot.t(
                    lang,
                    "downloads.batch.completed",
                    &[("downloaded", "0"), ("skipped", "1"), ("failed", "0")],
                ),
                _ => bot.t(
                    lang,
                    "tasks.completion.single.private",
                    &[("count", "1"), ("channel_title", ""), ("message_link", "")],
                ),
            };
            ctx.report(&text, None).await;
        }
        Ended::Result(Outcome::Failed) => final_status = "failed",
        _ => {}
    }
    // The cancel button goes away with the task; the final markup (if any) replaces it.
    let markup = status.terminal_markup();
    set_markup(bot, &status, markup).await;
    queue::finish(&bot.pg, queue_id, final_status).await.ok();
    ended
}

async fn set_markup(
    bot: &Bot,
    status: &Status,
    markup: Option<grammers_client::message::ReplyMarkup>,
) {
    let raw = markup.map(|m| m.raw);
    let request = grammers_client::tl::functions::messages::EditMessage {
        no_webpage: false,
        invert_media: false,
        peer: status.peer.into(),
        id: status.id,
        message: None,
        media: None,
        // Leaving the markup out removes the keyboard; an empty inline keyboard is refused as REPLY_MARKUP_INVALID.
        reply_markup: raw,
        entities: None,
        schedule_date: None,
        schedule_repeat_period: None,
        quick_reply_shortcut_id: None,
        rich_message: None,
    };
    let _ = bot.client.invoke(&request).await;
}

/// Cancels one task of a user, or all of them. Returns the cancelled ids.
pub async fn cancel(bot: &Arc<Bot>, user_id: i64, queue_id: Option<i64>) -> Result<Vec<i64>> {
    let ids = queue::cancel(&bot.pg, user_id, queue_id).await?;
    for id in &ids {
        if let Ok(Some(spec)) = queue::spec(&bot.pg, *id, user_id).await
            && let Some(active_id) = spec["active_task_id"].as_i64()
        {
            crate::store::active::clear(&bot.pg, active_id).await;
        }
    }
    for id in &ids {
        if let Some(token) = bot.running.lock().unwrap().get(id) {
            token.cancel();
        }
    }
    bot.queue_wake.notify_waiters();
    Ok(ids)
}
