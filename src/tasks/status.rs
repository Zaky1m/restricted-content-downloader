//! The one message a task keeps editing: queue position, progress and the final result.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use grammers_client::{
    Client,
    message::{InputMessage, ReplyMarkup},
    session::types::PeerRef,
    tl,
};

use crate::{
    bot::Bot,
    telegram::keyboards::{Button, Color, keyboard},
};

/// Progress edits closer together than this are skipped, so a long transfer cannot flood Telegram.
const PROGRESS_EDIT_GAP: Duration = Duration::from_secs(12);
/// A flood wait this short is waited out so the text is not lost; longer ones fall back to a new message.
const SHORT_FLOOD: u64 = 15;

#[derive(Default)]
struct Memory {
    last_text: String,
    last_edit: Option<Instant>,
    flood_until: Option<Instant>,
}

#[derive(Default)]
struct Final {
    is_final: bool,
    text: Option<String>,
    markup: Option<tl::enums::ReplyMarkup>,
}

pub struct Status {
    pub peer: PeerRef,
    pub id: i32,
    /// Set for the message of a queued task: it gets the task number and the cancel button.
    pub queue_id: Option<i64>,
    pub lang: String,
    header: Mutex<Option<String>>,
    memory: tokio::sync::Mutex<Memory>,
    done: Mutex<Final>,
}

pub fn queue_keyboard(bot: &Bot, lang: &str, queue_id: i64, is_premium: bool) -> ReplyMarkup {
    let mut rows = vec![vec![Button::callback(
        &bot.t(lang, "queue.buttons.cancel", &[]),
        &format!("qc_{queue_id}"),
        Color::Red,
    )]];
    if !is_premium {
        rows.insert(
            0,
            vec![Button::callback(
                &bot.t(lang, "queue.buttons.upgrade", &[]),
                "show_premium",
                Color::Green,
            )],
        );
    }
    keyboard(&rows)
}

fn is_rtl(lang: &str) -> bool {
    matches!(
        lang.split('-').next().unwrap_or(lang),
        "ar" | "fa" | "he" | "ur"
    )
}

impl Status {
    pub fn new(peer: PeerRef, id: i32, queue_id: Option<i64>, lang: &str) -> Self {
        Self {
            peer,
            id,
            queue_id,
            lang: lang.to_string(),
            header: Mutex::new(None),
            memory: tokio::sync::Mutex::new(Memory::default()),
            done: Mutex::new(Final::default()),
        }
    }

    /// Text that heads every progress update (batch and clone show their counters here).
    pub fn set_header(&self, header: Option<String>) {
        *self.header.lock().unwrap() = header;
    }

    pub fn header(&self) -> Option<String> {
        self.header.lock().unwrap().clone()
    }

    pub fn is_final(&self) -> bool {
        self.done.lock().unwrap().is_final
    }

    pub fn set_final(
        &self,
        is_final: bool,
        text: Option<String>,
        markup: Option<tl::enums::ReplyMarkup>,
    ) {
        let mut done = self.done.lock().unwrap();
        done.is_final = is_final;
        if is_final {
            done.text = text;
            done.markup = markup;
        }
    }

    pub fn final_text(&self) -> Option<String> {
        self.done.lock().unwrap().text.clone()
    }

    pub fn terminal_markup(&self) -> Option<ReplyMarkup> {
        self.done
            .lock()
            .unwrap()
            .markup
            .clone()
            .map(|raw| ReplyMarkup { raw })
    }

    fn message(&self, bot: &Bot, text: &str, markup: Option<ReplyMarkup>) -> InputMessage {
        let mut text = text.to_string();
        let mut markup = markup;
        if let Some(queue_id) = self.queue_id {
            let number = bot.t(
                &self.lang,
                "queue.task_number",
                &[("task_id", &queue_id.to_string())],
            );
            if !text.contains(&number) {
                let mark = if is_rtl(&self.lang) {
                    '\u{200f}'
                } else {
                    '\u{200e}'
                };
                text.push_str(&format!("\n\n{mark}✦ {number}"));
            }
            if markup.is_none() {
                markup = Some(queue_keyboard(bot, &self.lang, queue_id, true));
            }
        }
        crate::bot::html(&text, markup)
    }

    /// Edits the message. Returns false when nothing was sent (same text, too soon, or an error).
    pub async fn edit(&self, bot: &Bot, text: &str, markup: Option<ReplyMarkup>) -> bool {
        if text.is_empty() {
            return false;
        }
        let header = self.header();
        let is_progress = header
            .as_deref()
            .is_some_and(|h| text == h || text.starts_with(&format!("{h}\n")));
        let mut memory = self.memory.lock().await;
        if memory.last_text == text {
            return false;
        }
        if is_progress
            && memory
                .last_edit
                .is_some_and(|at| at.elapsed() < PROGRESS_EDIT_GAP)
        {
            return false;
        }
        if let Some(until) = memory.flood_until.filter(|u| *u > Instant::now()) {
            if is_progress {
                return false;
            }
            let remaining = until - Instant::now();
            if remaining.as_secs() <= SHORT_FLOOD || self.queue_id.is_some() {
                tokio::time::sleep(remaining).await;
            } else {
                return self.reply_instead(bot, text).await;
            }
        }
        let client: &Client = &bot.client;
        match client
            .edit_message(self.peer, self.id, self.message(bot, text, markup))
            .await
        {
            Ok(()) => {
                memory.last_text = text.to_string();
                memory.last_edit = Some(Instant::now());
                memory.flood_until = None;
                true
            }
            Err(error) if error.is("MESSAGE_NOT_MODIFIED") || error.is("MESSAGE_ID_INVALID") => {
                memory.last_text = text.to_string();
                false
            }
            Err(grammers_client::InvocationError::Rpc(rpc))
                if rpc.name == "FLOOD_WAIT" || rpc.name == "FLOOD_PREMIUM_WAIT" =>
            {
                let wait = u64::from(rpc.value.unwrap_or(5).max(1));
                memory.flood_until = Some(Instant::now() + Duration::from_secs(wait + 1));
                memory.last_edit = Some(Instant::now());
                if is_progress {
                    return false;
                }
                if wait <= SHORT_FLOOD || self.queue_id.is_some() {
                    tokio::time::sleep(Duration::from_secs(wait + 1)).await;
                    let retried = client
                        .edit_message(self.peer, self.id, self.message(bot, text, None))
                        .await
                        .is_ok();
                    if retried {
                        memory.last_text = text.to_string();
                        memory.last_edit = Some(Instant::now());
                        memory.flood_until = None;
                    }
                    retried
                } else {
                    self.reply_instead(bot, text).await
                }
            }
            Err(error) => {
                tracing::warn!(chat = ?self.peer.id, message = self.id, %error, "could not edit a Telegram message");
                false
            }
        }
    }

    async fn reply_instead(&self, bot: &Bot, text: &str) -> bool {
        bot.client
            .send_message(self.peer, self.message(bot, text, None))
            .await
            .is_ok()
    }

    pub async fn delete(&self, bot: &Bot) {
        // The status message of a queued task is the task's own record and stays until it ends.
        if self.queue_id.is_some() {
            return;
        }
        let _ = bot.client.delete_messages(self.peer, &[self.id]).await;
    }
}
