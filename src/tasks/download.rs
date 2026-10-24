//! Delivering one Telegram message: copy it when allowed, else download and upload it.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use grammers_client::{
    InvocationError,
    message::Message,
    session::types::{PeerAuth, PeerId, PeerRef},
    tl,
};

use super::{
    Outcome, TaskCtx,
    large::{self, Asked, Policy},
    progress::{self, Action},
};
use crate::{
    caption::{CaptionMode, process_caption},
    format::quote_plus,
    media::{
        download::{self, Failure},
        ffmpeg,
        files::{self, is_valid_file},
        info::{self, Item, Kind},
        upload::{self, Outgoing, SendAs},
    },
    store::account,
    telegram::{
        accounts::Account,
        keyboards::{Button, Color, keyboard},
    },
};

/// Telegram's real upload limits: 4000 parts of 512 KiB (2000 MiB), and 8000 parts for Premium.
pub const MAX_FILE: u64 = 2000 * 1024 * 1024;
pub const MAX_PREMIUM_FILE: u64 = 4000 * 1024 * 1024;
/// A normal upload is cut into parts of this size.
pub const SPLIT_SIZE: u64 = 1990 * 1024 * 1024;
const MAX_FLOOD_SINGLE: u64 = 300;
const MAX_FLOOD_TOTAL: u64 = 900;

pub fn raw_message(message: &Message) -> Option<&tl::types::Message> {
    match &message.raw {
        tl::enums::Message::Message(m) => Some(m),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Private,
    SavedMessages,
    Channel,
}

impl Mode {
    pub fn key(self) -> &'static str {
        match self {
            Mode::Private => "private",
            Mode::SavedMessages => "saved_messages",
            Mode::Channel => "channel",
        }
    }
}

pub struct Destination {
    pub mode: Mode,
    pub peer: PeerRef,
    /// True when the linked account itself must send (Saved Messages).
    pub by_account: bool,
}

pub async fn destination(ctx: &TaskCtx, account: &Account) -> Result<Destination> {
    let channel: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT upload_dest_id FROM users WHERE user_id = $1")
            .bind(ctx.user_id)
            .fetch_optional(&ctx.bot.pg)
            .await?;
    if let Some(dest) = channel.and_then(|c| c.0).filter(|d| *d != 0)
        && let Some(id) = PeerId::from_bot_api_dialog_id(dest)
    {
        use crate::bot::uploadch::{ChannelCheck, check_upload_channel};
        match check_upload_channel(&ctx.bot, dest).await {
            ChannelCheck::Ready => {
                let peer = ctx.bot.peer_ref_for(id).await.unwrap_or(PeerRef {
                    id,
                    auth: PeerAuth::default(),
                });
                return Ok(Destination {
                    mode: Mode::Channel,
                    peer,
                    by_account: false,
                });
            }
            ChannelCheck::Lost => {
                tracing::warn!(
                    user = ctx.user_id,
                    chat = dest,
                    "channel upload was disabled"
                );
                sqlx::query("UPDATE users SET upload_dest_id = NULL WHERE user_id = $1")
                    .bind(ctx.user_id)
                    .execute(&ctx.bot.pg)
                    .await?;
                ctx.bot
                    .send_to(
                        ctx.user_id,
                        &ctx.bot.t(&ctx.lang, "uploads.channel.disabled", &[]),
                        None,
                    )
                    .await
                    .ok();
            }
            ChannelCheck::Unknown => {}
        }
    }
    if account.is_linked() {
        return Ok(Destination {
            mode: Mode::SavedMessages,
            peer: PeerRef {
                id: PeerId::self_user(),
                auth: PeerAuth::default(),
            },
            by_account: true,
        });
    }
    let peer = ctx
        .bot
        .user_ref(ctx.user_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("user is not known to the bot"))?;
    Ok(Destination {
        mode: Mode::Private,
        peer,
        by_account: false,
    })
}

fn truncate_caption(caption: &str) -> String {
    if caption.chars().count() > 950 {
        let cut: String = caption.chars().take(947).collect();
        format!("{}...", cut.trim_end())
    } else {
        caption.to_string()
    }
}

struct Settings {
    mode: CaptionMode,
    thumbnail: Option<PathBuf>,
}

async fn user_settings(ctx: &TaskCtx) -> Settings {
    match account::load(&ctx.bot.pg, ctx.user_id).await {
        Ok(Some(user)) => Settings {
            mode: CaptionMode::from_setting(&user.caption_mode),
            thumbnail: user
                .thumbnail_path
                .filter(|_| user.thumbnail_enabled)
                .map(PathBuf::from)
                .filter(|p| is_valid_file(p)),
        },
        _ => Settings {
            mode: CaptionMode::KeepAll,
            thumbnail: None,
        },
    }
}

async fn record(ctx: &TaskCtx, message: &tl::types::Message, chat_id: i64, file_size: u64) {
    let kind = info::message_type(message).unwrap_or("other");
    let column = match kind {
        "videos" => Some("total_videos"),
        "photos" => Some("total_photos"),
        "audio" => Some("total_audio"),
        "files" => Some("total_files"),
        "text" => Some("total_text"),
        "other" => Some("total_other"),
        _ => None,
    };
    let result = async {
        let mut tx = ctx.bot.pg.begin().await?;
        sqlx::query("INSERT INTO downloads (user_id, message_id, chat_id, media_type, file_size) VALUES ($1, $2, $3, $4, $5)")
            .bind(ctx.user_id)
            .bind(i64::from(message.id))
            .bind(chat_id)
            .bind(kind)
            .bind(file_size as i64)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE bot_stats SET stat_value = stat_value + 1 WHERE stat_key = 'total_downloads'").execute(&mut *tx).await?;
        if let Some(column) = column {
            sqlx::query("UPDATE bot_stats SET stat_value = stat_value + 1 WHERE stat_key = $1").bind(column).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE users SET last_activity_date = NOW() WHERE user_id = $1").bind(ctx.user_id).execute(&mut *tx).await?;
        tx.commit().await
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(user = ctx.user_id, %error, "could not record a completed download");
    }
}

pub struct Delivered {
    pub ok: bool,
    pub mode: Option<Mode>,
    pub last: Option<Message>,
    pub skip: Option<Skip>,
}

/// Why an item was left alone without being a failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Skip {
    /// The user cancelled at the large file question.
    Cancelled,
    /// The user has not answered the large file question yet.
    Unanswered,
}

impl Delivered {
    fn failed(mode: Option<Mode>) -> Self {
        Self {
            ok: false,
            mode,
            last: None,
            skip: None,
        }
    }

    fn skipped(skip: Skip) -> Self {
        Self {
            skip: Some(skip),
            ..Self::failed(None)
        }
    }
}

fn share_button(ctx: &TaskCtx) -> grammers_client::message::ReplyMarkup {
    let link = ctx.bot.referral_link(ctx.user_id);
    let text = ctx.bot.t(&ctx.lang, "referrals.share_text", &[]);
    let url = format!(
        "https://t.me/share/url?url={}&text={}",
        quote_plus(&link),
        quote_plus(&text)
    );
    keyboard(&[vec![Button::url(
        &ctx.bot.t(&ctx.lang, "buttons.share_and_earn", &[]),
        &url,
        Color::Blue,
    )]])
}

pub fn message_link(message: &Message) -> String {
    let Some(raw) = raw_message(message) else {
        return String::new();
    };
    let Some(chat) = message.peer() else {
        return String::new();
    };
    if let Some(username) = chat.username() {
        return format!("https://t.me/{username}/{}", raw.id);
    }
    match message.peer_id().bot_api_dialog_id() {
        Some(id) if id < -1_000_000_000_000 => {
            format!("https://t.me/c/{}/{}", -id - 1_000_000_000_000, raw.id)
        }
        _ => String::new(),
    }
}

/// The completion text for a task of `kind` (`single`, `batch`, `clone`, `monitor`).
pub async fn completion_text(
    ctx: &TaskCtx,
    kind: &str,
    mode: Mode,
    count: usize,
    sent: Option<&Message>,
) -> String {
    let title = sent
        .and_then(|m| m.peer())
        .map(|p| p.name().unwrap_or("").to_string())
        .unwrap_or_default();
    let link = sent.map(message_link).unwrap_or_default();
    ctx.bot.t(
        &ctx.lang,
        &format!("tasks.completion.{kind}.{}", mode.key()),
        &[
            ("count", &count.to_string()),
            ("channel_title", &crate::bot::escape(&title)),
            ("message_link", &link),
        ],
    )
}

pub struct Job<'a> {
    pub ctx: &'a Arc<TaskCtx>,
    pub account: Arc<Account>,
    pub force_download: bool,
    /// Batch and clone count results themselves and keep the status message for their counters.
    pub is_batch: bool,
    /// False for background work (monitor) that must not hold its turn while the user decides.
    pub wait_for_choice: bool,
    pub policy: std::sync::Mutex<super::large::Policy>,
}

/// Why an item produced nothing; the caller decides what the user is told.
pub enum Fail {
    Disk,
    TooLarge,
    Download,
    Upload(String),
    Other,
}

impl Job<'_> {
    fn t(&self, key: &str, args: &[(&str, &str)]) -> String {
        self.ctx.bot.t(&self.ctx.lang, key, args)
    }

    async fn tell(&self, key: &str, args: &[(&str, &str)]) {
        // The task's own status message carries the reason, instead of a second message.
        if !self.is_batch {
            self.ctx.report(&self.t(key, args), None).await;
        }
    }

    async fn finish_single(&self, dest: &Destination, count: usize, last: Option<&Message>) {
        if self.is_batch {
            return;
        }
        let text = completion_text(self.ctx, "single", dest.mode, count, last).await;
        self.ctx.report(&text, None).await;
    }

    /// Delivers one message. Returns what happened for the batch counters.
    pub async fn deliver(&self, message: &Message) -> Result<Delivered> {
        let Some(raw) = raw_message(message).cloned() else {
            return Ok(Delivered::failed(None));
        };
        let chat_id = message.peer_id().bot_api_dialog_id().unwrap_or(0);
        let mut dest = destination(self.ctx, &self.account).await?;
        *self.policy.lock().unwrap() = Policy::Normal;
        let size = info::paid_items(&raw)
            .iter()
            .map(|(_, item)| item.size)
            .chain(info::downloadable(&raw).map(|item| item.size))
            .max()
            .unwrap_or(0);
        let mut action_id = None;
        if size > SPLIT_SIZE {
            match large::ask(
                self.ctx,
                &self.account,
                dest.mode,
                (chat_id, raw.id),
                size,
                self.wait_for_choice,
            )
            .await
            {
                Asked::Cancelled => return Ok(Delivered::skipped(Skip::Cancelled)),
                Asked::Pending => return Ok(Delivered::skipped(Skip::Unanswered)),
                Asked::Chosen(policy, id) => {
                    action_id = id;
                    *self.policy.lock().unwrap() = policy;
                    dest = self.apply_policy(dest, policy).await?;
                }
            }
        }
        let result = self.deliver_to(message, &raw, chat_id, dest).await;
        if let Some(id) = action_id {
            crate::store::pending::delete(&self.ctx.bot.pg, id).await;
        }
        result
    }

    async fn apply_policy(&self, dest: Destination, policy: Policy) -> Result<Destination> {
        Ok(match policy {
            Policy::Saved => Destination {
                mode: Mode::SavedMessages,
                peer: PeerRef {
                    id: PeerId::self_user(),
                    auth: PeerAuth::default(),
                },
                by_account: true,
            },
            Policy::Split if dest.mode == Mode::SavedMessages => Destination {
                mode: Mode::Private,
                peer: self
                    .ctx
                    .bot
                    .user_ref(self.ctx.user_id)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("user is not known to the bot"))?,
                by_account: false,
            },
            _ => dest,
        })
    }

    async fn deliver_to(
        &self,
        message: &Message,
        raw: &tl::types::Message,
        chat_id: i64,
        dest: Destination,
    ) -> Result<Delivered> {
        let raw = raw.clone();
        let settings = user_settings(self.ctx).await;
        let _busy = self.account.busy();

        let paid = info::paid_items(&raw);
        if info::has_paid_media(&raw) {
            return self
                .deliver_paid(message, &raw, &paid, &dest, &settings, chat_id)
                .await;
        }
        if !self.force_download && self.account.is_linked() {
            if let Some(sent) = self.try_copy(message, &raw, &dest, &settings).await {
                record(self.ctx, &raw, chat_id, 0).await;
                self.finish_single(&dest, 1, Some(&sent)).await;
                return Ok(Delivered {
                    ok: true,
                    mode: Some(dest.mode),
                    last: Some(sent),
                    skip: None,
                });
            }
        }

        let html_caption = |is_media| {
            let text = message_html(message);
            process_caption(&text, settings.mode, is_media)
        };
        let Some(item) = info::downloadable(&raw) else {
            // Plain text and web-page posts are sent as text.
            let text = html_caption(false);
            if text.is_empty() {
                return Ok(Delivered::failed(Some(dest.mode)));
            }
            let sender = if dest.by_account {
                self.account.client().await
            } else {
                self.ctx.bot.client.clone()
            };
            return match sender
                .send_message(dest.peer, crate::bot::html(&text, None))
                .await
            {
                Ok(sent) => {
                    record(self.ctx, &raw, chat_id, 0).await;
                    self.finish_single(&dest, 1, Some(&sent)).await;
                    Ok(Delivered {
                        ok: true,
                        mode: Some(dest.mode),
                        last: Some(sent),
                        skip: None,
                    })
                }
                Err(error) => {
                    tracing::error!(user = self.ctx.user_id, %error, "could not send text");
                    Ok(Delivered::failed(Some(dest.mode)))
                }
            };
        };

        let caption = html_caption(true);
        let name = crate::format::safe_filename(
            &info::file_name(raw.id, &item),
            &format!("download_{}.bin", self.ctx.user_id),
        );
        self.deliver_item(
            &item, &name, &caption, &raw, chat_id, &dest, &settings, None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn deliver_item(
        &self,
        item: &Item,
        name: &str,
        caption: &str,
        raw: &tl::types::Message,
        chat_id: i64,
        dest: &Destination,
        settings: &Settings,
        part_label: Option<String>,
    ) -> Result<Delivered> {
        let ctx = self.ctx;
        let single_message = part_label.is_none();
        let limit = MAX_PREMIUM_FILE;
        if item.size > limit {
            self.tell(
                "downloads.file_too_large",
                &[
                    ("maximum_size", &ctx.bot.size(&ctx.lang, limit)),
                    ("action", &self.t("downloads.actions.download", &[])),
                ],
            )
            .await;
            return Ok(Delivered::failed(Some(dest.mode)));
        }
        let dir = ctx
            .bot
            .cfg
            .data_dir
            .join("downloads")
            .join(ctx.user_id.to_string());
        let path = dir.join(name);
        let status = ctx.status.clone();
        let progress = match (&status, item.size > progress::THRESHOLD, self.is_batch) {
            (Some(status), true, false) => {
                Some(progress::start(ctx, status.clone(), Action::Downloading))
            }
            _ => None,
        };
        if let Err(error) = files::ensure_disk_space(&dir, item.size) {
            tracing::info!(user = ctx.user_id, %error, "not enough storage");
            self.tell("downloads.disk_full", &[]).await;
            return Ok(Delivered::failed(Some(dest.mode)));
        }
        let fetched = self
            .fetch_with_retries(item, &path, progress.as_ref())
            .await;
        drop(progress);
        if fetched.is_none() {
            files::cleanup(&path);
            self.tell("downloads.download_failed", &[]).await;
            return Ok(Delivered::failed(Some(dest.mode)));
        }

        let result = self
            .send_file(&path, item, caption, dest, settings, part_label)
            .await;
        files::cleanup(&path);
        match result {
            Ok(Some(sent)) => {
                record(ctx, raw, chat_id, item.size).await;
                if single_message {
                    self.finish_single(dest, 1, Some(&sent)).await;
                }
                Ok(Delivered {
                    ok: true,
                    mode: Some(dest.mode),
                    last: Some(sent),
                    skip: None,
                })
            }
            Ok(None) => Ok(Delivered::failed(Some(dest.mode))),
            Err(error) => {
                self.tell(
                    "downloads.upload_failed",
                    &[("error", &crate::bot::escape(&error.to_string()))],
                )
                .await;
                Ok(Delivered::failed(Some(dest.mode)))
            }
        }
    }
}

pub fn message_html(message: &Message) -> String {
    message.html_text()
}

impl Job<'_> {
    async fn fetch_with_retries(
        &self,
        item: &Item,
        path: &std::path::Path,
        progress: Option<&progress::Progress>,
    ) -> Option<u64> {
        let mut attempt = 1;
        let mut flood_total = 0u64;
        while attempt <= 3 {
            files::cleanup(path);
            let client = self.account.client().await;
            let result = download::fetch(&client, item, path, |done, total| {
                if let Some(progress) = progress {
                    progress.report(done, total.max(item.size));
                }
            })
            .await;
            match result {
                Ok(size) if is_valid_file(path) => return Some(size),
                Ok(size) => {
                    tracing::warn!(
                        user = self.ctx.user_id,
                        attempt,
                        size,
                        "download produced an empty file"
                    );
                }
                Err(Failure::Flood(seconds)) => {
                    let wait = u64::from(seconds);
                    flood_total += wait;
                    if wait > MAX_FLOOD_SINGLE || flood_total > MAX_FLOOD_TOTAL {
                        tracing::warn!(
                            user = self.ctx.user_id,
                            wait,
                            flood_total,
                            "download gave up on flood waits"
                        );
                        return None;
                    }
                    tokio::time::sleep(Duration::from_secs(wait + 2)).await;
                    continue;
                }
                Err(Failure::SessionDead) => {
                    tracing::error!(user = self.ctx.user_id, account = %self.account.label, "download account is no longer authorized");
                    if self.account.is_linked() {
                        self.ctx
                            .bot
                            .accounts
                            .forget_linked(self.ctx.user_id)
                            .await
                            .ok();
                    }
                    return None;
                }
                Err(failure) => {
                    tracing::warn!(user = self.ctx.user_id, attempt, %failure, "download attempt failed");
                    if matches!(failure, Failure::Stalled | Failure::Other(_)) {
                        self.ctx.bot.accounts.reconnect(&self.account).await;
                    }
                }
            }
            if attempt < 3 {
                tokio::time::sleep(Duration::from_secs((attempt * 3).min(10))).await;
            }
            attempt += 1;
        }
        None
    }

    async fn send_file(
        &self,
        path: &std::path::Path,
        item: &Item,
        caption: &str,
        dest: &Destination,
        settings: &Settings,
        part_label: Option<String>,
    ) -> Result<Option<Message>> {
        let size = std::fs::metadata(path)?.len();
        let send_as = match item.kind {
            Kind::Photo => SendAs::Photo,
            Kind::Video | Kind::VideoNote | Kind::Animation => SendAs::Video,
            Kind::Audio | Kind::Voice => SendAs::Audio,
            _ => SendAs::Document,
        };
        let policy = *self.policy.lock().unwrap();
        let direct_large = self.account.is_linked()
            && matches!(policy, Policy::Direct | Policy::Saved)
            && size > SPLIT_SIZE;
        if size > SPLIT_SIZE && !direct_large {
            return self.send_split(path, caption, dest, part_label).await;
        }
        let sender = if direct_large || dest.by_account {
            self.account.client().await
        } else {
            self.ctx.bot.client.clone()
        };
        // A whole file above 2 GB can only go out through the premium account itself.
        let peer = if direct_large && !dest.by_account && dest.mode != Mode::Channel {
            PeerRef {
                id: PeerId::self_user(),
                auth: PeerAuth::default(),
            }
        } else {
            dest.peer
        };
        let mut out = Outgoing {
            path: path.to_path_buf(),
            send_as,
            caption_html: truncate_caption(&match &part_label {
                Some(label) => format!("{label}\n\n{caption}").trim().to_string(),
                None => caption.to_string(),
            }),
            thumbnail: None,
            duration: item.duration.round() as i64,
            width: item.width,
            height: item.height,
            performer: item.performer.clone(),
            title: item.title.clone(),
        };
        let mut generated = None;
        if send_as == SendAs::Video {
            let info = ffmpeg::media_info(path).await;
            let (w, h) = ffmpeg::video_dimensions(path).await;
            out.duration = info.duration;
            out.width = w;
            out.height = h;
            out.thumbnail = settings.thumbnail.clone();
            if out.thumbnail.is_none() {
                generated =
                    ffmpeg::video_thumbnail(path, &self.ctx.bot.cfg.data_dir.join("assets")).await;
                out.thumbnail = generated.clone();
            }
        } else if send_as == SendAs::Audio {
            let info = ffmpeg::media_info(path).await;
            out.duration = info.duration;
            out.performer = info.artist.or(out.performer);
            out.title = info.title.or(out.title);
        }
        let markup = (dest.mode == Mode::Private && !direct_large).then(|| share_button(self.ctx));
        let result = self
            .upload_and_send(
                &sender,
                peer,
                &out,
                size,
                markup,
                !self.is_batch && size > progress::THRESHOLD,
            )
            .await;
        if let Some(thumb) = generated {
            tokio::fs::remove_file(thumb).await.ok();
        }
        result
    }

    async fn upload_and_send(
        &self,
        sender: &grammers_client::Client,
        peer: PeerRef,
        out: &Outgoing,
        size: u64,
        markup: Option<grammers_client::message::ReplyMarkup>,
        show_progress: bool,
    ) -> Result<Option<Message>> {
        let progress = match (&self.ctx.status, show_progress) {
            (Some(status), true) => {
                Some(progress::start(self.ctx, status.clone(), Action::Uploading))
            }
            _ => None,
        };
        let sender_tx = progress.as_ref().map(|p| p.sender());
        let mut flood_total = 0u64;
        let mut attempts = 0;
        loop {
            let tx = sender_tx.clone();
            let uploaded = upload::upload(sender, &out.path, move |done| {
                if let Some(tx) = &tx {
                    let _ = tx.send((done, size));
                }
            })
            .await;
            let file = match uploaded {
                Ok(file) => file,
                Err(error) => {
                    attempts += 1;
                    if attempts > 3 {
                        return Err(error.into());
                    }
                    tokio::time::sleep(Duration::from_secs(attempts * 2)).await;
                    continue;
                }
            };
            let thumb = match &out.thumbnail {
                Some(path) if is_valid_file(path) => sender.upload_file(path).await.ok(),
                _ => None,
            };
            match upload::send(
                sender,
                peer,
                out,
                file,
                thumb,
                markup
                    .as_ref()
                    .map(|m| grammers_client::message::ReplyMarkup { raw: m.raw.clone() }),
            )
            .await
            {
                Ok(sent) => return Ok(Some(sent)),
                Err(InvocationError::Rpc(rpc)) if rpc.name.starts_with("FLOOD") => {
                    let wait = u64::from(rpc.value.unwrap_or(30).max(1));
                    flood_total += wait;
                    if wait > MAX_FLOOD_SINGLE || flood_total > MAX_FLOOD_TOTAL {
                        return Ok(None);
                    }
                    self.ctx
                        .show(&self.t("downloads.flood_wait", &[("seconds", &wait.to_string())]))
                        .await;
                    tokio::time::sleep(Duration::from_secs(wait + 5)).await;
                }
                Err(InvocationError::Rpc(rpc)) if rpc.name.contains("CAPTION_TOO_LONG") => {
                    let mut shorter = out.clone();
                    shorter.caption_html = format!(
                        "{}...",
                        shorter
                            .caption_html
                            .chars()
                            .take(497)
                            .collect::<String>()
                            .trim_end()
                    );
                    return Ok(upload::send(
                        sender,
                        peer,
                        &shorter,
                        sender.upload_file(&out.path).await?,
                        None,
                        None,
                    )
                    .await
                    .ok());
                }
                Err(InvocationError::Rpc(rpc))
                    if rpc.name == "MEDIA_FILE_INVALID" && out.send_as != SendAs::Document =>
                {
                    let mut as_document = out.clone();
                    as_document.send_as = SendAs::Document;
                    let file = sender.upload_file(&out.path).await?;
                    return Ok(upload::send(sender, peer, &as_document, file, None, None)
                        .await
                        .ok());
                }
                Err(InvocationError::Rpc(rpc))
                    if matches!(
                        rpc.name.as_str(),
                        "USER_IS_BLOCKED" | "USER_DEACTIVATED" | "PEER_ID_INVALID"
                    ) =>
                {
                    tracing::info!(user = self.ctx.user_id, "user can no longer receive files");
                    self.ctx.cancel.cancel();
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    async fn send_split(
        &self,
        path: &std::path::Path,
        caption: &str,
        dest: &Destination,
        outer_label: Option<String>,
    ) -> Result<Option<Message>> {
        self.ctx.show(&self.t("downloads.splitting", &[])).await;
        let dir = path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join(format!("split_{:032x}", rand::random::<u128>()));
        let parts = files::split(path, &dir, SPLIT_SIZE).await?;
        let mut last = None;
        let total = parts.len();
        let sender = if dest.by_account {
            self.account.client().await
        } else {
            self.ctx.bot.client.clone()
        };
        let result: Result<()> = async {
            for (index, part) in parts.iter().enumerate() {
                let label = self.t(
                    "downloads.parts.item",
                    &[
                        ("current", &(index + 1).to_string()),
                        ("total", &total.to_string()),
                    ],
                );
                let text = match &outer_label {
                    Some(outer) => format!("{label}\n\n{outer}\n\n{caption}"),
                    None => format!("{label}\n\n{caption}"),
                };
                let out = Outgoing {
                    path: part.clone(),
                    send_as: SendAs::Document,
                    caption_html: truncate_caption(text.trim()),
                    thumbnail: None,
                    duration: 0,
                    width: 0,
                    height: 0,
                    performer: None,
                    title: None,
                };
                let size = std::fs::metadata(part)?.len();
                let markup = (dest.mode == Mode::Private).then(|| share_button(self.ctx));
                if let Some(sent) = self
                    .upload_and_send(&sender, dest.peer, &out, size, markup, false)
                    .await?
                {
                    last = Some(sent);
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Ok(())
        }
        .await;
        tokio::fs::remove_dir_all(&dir).await.ok();
        result?;
        Ok(last)
    }

    /// Copy through the user's own account when the source allows saving; `None` means download instead.
    async fn try_copy(
        &self,
        message: &Message,
        raw: &tl::types::Message,
        dest: &Destination,
        settings: &Settings,
    ) -> Option<Message> {
        let peer = message.peer_ref().await.ok().flatten()?;
        if peer.id.kind() == grammers_client::session::types::PeerKind::Channel {
            let channel: tl::enums::InputChannel = (&peer).into();
            let found = self
                .account
                .client()
                .await
                .invoke(&tl::functions::channels::GetChannels { id: vec![channel] })
                .await
                .ok()?;
            let tl::enums::messages::Chats::Chats(chats) = found else {
                return None;
            };
            let protected = chats
                .chats
                .iter()
                .any(|c| matches!(c, tl::enums::Chat::Channel(c) if c.noforwards));
            if protected {
                return None;
            }
        } else {
            return None;
        }
        let input: tl::enums::InputMedia = match raw.media.as_ref()? {
            tl::enums::MessageMedia::Photo(p) => {
                let tl::enums::Photo::Photo(photo) = p.photo.as_ref()? else {
                    return None;
                };
                tl::types::InputMediaPhoto {
                    spoiler: false,
                    id: tl::types::InputPhoto {
                        id: photo.id,
                        access_hash: photo.access_hash,
                        file_reference: photo.file_reference.clone(),
                    }
                    .into(),
                    ttl_seconds: None,
                    live_photo: false,
                    video: None,
                }
                .into()
            }
            tl::enums::MessageMedia::Document(d) => {
                let tl::enums::Document::Document(doc) = d.document.as_ref()? else {
                    return None;
                };
                tl::types::InputMediaDocument {
                    spoiler: false,
                    id: tl::types::InputDocument {
                        id: doc.id,
                        access_hash: doc.access_hash,
                        file_reference: doc.file_reference.clone(),
                    }
                    .into(),
                    video_cover: None,
                    video_timestamp: None,
                    ttl_seconds: None,
                    query: None,
                }
                .into()
            }
            _ => return None,
        };
        let caption = truncate_caption(&process_caption(
            &message_html(message),
            settings.mode,
            true,
        ));
        let sender = self.account.client().await;
        let out = grammers_client::message::InputMessage::new()
            .html(&caption)
            .media(input);
        sender.send_message(dest.peer, out).await.ok()
    }

    async fn deliver_paid(
        &self,
        message: &Message,
        raw: &tl::types::Message,
        paid: &[(usize, Item)],
        dest: &Destination,
        settings: &Settings,
        chat_id: i64,
    ) -> Result<Delivered> {
        if paid.is_empty() {
            self.tell("downloads.paid.not_purchased", &[]).await;
            return Ok(Delivered::failed(Some(dest.mode)));
        }
        let caption = process_caption(&message_html(message), settings.mode, true);
        let total = paid.len();
        let mut delivered = 0usize;
        let mut last = None;
        for (position, (index, item)) in paid.iter().enumerate() {
            let name = crate::format::safe_filename(
                &info::paid_file_name(raw.id, *index, item),
                &format!("paid_{}.bin", self.ctx.user_id),
            );
            let label = (total > 1).then(|| {
                self.t(
                    "downloads.parts.item",
                    &[
                        ("current", &(position + 1).to_string()),
                        ("total", &total.to_string()),
                    ],
                )
            });
            let result = self
                .deliver_item(item, &name, &caption, raw, chat_id, dest, settings, label)
                .await?;
            if result.ok {
                delivered += 1;
                last = result.last;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if delivered == 0 {
            self.tell("downloads.download_failed", &[]).await;
            return Ok(Delivered::failed(Some(dest.mode)));
        }
        Ok(Delivered {
            ok: true,
            mode: Some(dest.mode),
            last,
            skip: None,
        })
    }
}

pub fn outcome(delivered: &Delivered) -> Outcome {
    if delivered.skip.is_some() {
        Outcome::Skipped
    } else if delivered.ok {
        Outcome::Downloaded
    } else {
        Outcome::Failed
    }
}
