//! The bot: update loop, shared state and the send helpers every handler uses.

mod account;
mod admin;
mod billing;
mod callbacks;
mod cancel;
mod commands;
mod download;
mod guards;
mod help;
mod housekeeping;
mod large;
mod login;
mod range;
mod send;
mod settings;
mod start;
mod start_image;
mod stats;
pub mod uploadch;

use std::sync::Arc;

use anyhow::{Context, Result};
use grammers_client::session::SessionData;
use grammers_client::{
    Client,
    client::UpdatesConfiguration,
    session::{Session, types::PeerId},
    update::Update,
};
use sqlx::PgPool;

pub use download::{begin_guide, start_cooldown};
pub use send::{Who, escape, html};

use crate::{
    config::Config,
    db,
    i18n::Locales,
    sessions::{self, Kind},
    telegram::{self, pgsession::PgSession},
};

pub struct Bot {
    pub cfg: Config,
    pub pg: PgPool,
    pub locales: Locales,
    pub client: Client,
    pub session: Arc<PgSession>,
    pub started: std::time::Instant,
    pub me_id: i64,
    pub accounts: telegram::accounts::Accounts,
    pub running:
        std::sync::Mutex<std::collections::HashMap<i64, tokio_util::sync::CancellationToken>>,
    pub queue_wake: tokio::sync::Notify,
    /// Wakes tasks waiting for an answer to the large file question.
    pub large_wake: tokio::sync::Notify,
    /// The archived start photo, read once per run.
    pub start_image: tokio::sync::Mutex<Option<grammers_client::media::Media>>,
    pub broadcasts:
        std::sync::Mutex<std::collections::HashMap<i64, tokio_util::sync::CancellationToken>>,
    shutting_down: std::sync::atomic::AtomicBool,
}

impl Bot {
    pub fn t(&self, lang: &str, key: &str, args: &[(&str, &str)]) -> String {
        self.locales.text(Some(lang), key, args)
    }

    pub fn size(&self, lang: &str, bytes: u64) -> String {
        self.locales.size(Some(lang), bytes)
    }

    pub fn duration(&self, lang: &str, seconds: i64) -> String {
        self.locales.duration(Some(lang), seconds)
    }

    pub async fn lang(&self, user_id: i64) -> String {
        crate::store::users::lang(&self.pg, &self.locales, user_id).await
    }

    pub fn is_owner(&self, user_id: i64) -> bool {
        self.cfg.owner_ids.contains(&user_id)
    }

    /// Owners and the admins they appointed.
    pub async fn is_staff(&self, user_id: i64) -> bool {
        if self.is_owner(user_id) {
            return true;
        }
        let row: Result<Option<(i64,)>, _> =
            sqlx::query_as("SELECT user_id FROM bot_admins WHERE user_id = $1")
                .bind(user_id)
                .fetch_optional(&self.pg)
                .await;
        matches!(row, Ok(Some(_)))
    }

    /// A reference Telegram accepts for a user the bot has already seen.
    pub async fn user_ref(&self, user_id: i64) -> Option<grammers_client::session::types::PeerRef> {
        let id = PeerId::user(user_id)?;
        self.session.peer_ref(id).await.ok().flatten()
    }

    /// A chat or user reference. When the access hash is not cached yet (a channel the bot was added to while
    /// offline, an id from `.env`), Telegram is asked with hash 0, which it accepts from bots, and the answer is cached.
    pub async fn peer_ref_for(
        &self,
        id: PeerId,
    ) -> Option<grammers_client::session::types::PeerRef> {
        use grammers_client::session::{
            Session,
            types::{PeerAuth, PeerInfo, PeerKind, PeerRef},
        };
        use grammers_client::tl;
        if let Some(found) = self.session.peer_ref_of(id).await {
            return Some(found);
        }
        match id.kind() {
            PeerKind::Chat => Some(PeerRef {
                id,
                auth: PeerAuth::default(),
            }),
            PeerKind::Channel => {
                let request = tl::functions::channels::GetChannels {
                    id: vec![
                        tl::types::InputChannel {
                            channel_id: id.bare_id()?,
                            access_hash: 0,
                        }
                        .into(),
                    ],
                };
                let chats = match self.client.invoke(&request).await {
                    Ok(tl::enums::messages::Chats::Chats(c)) => c.chats,
                    Ok(tl::enums::messages::Chats::Slice(c)) => c.chats,
                    Err(error) => {
                        tracing::warn!(
                            channel = id.bot_api_dialog_id(),
                            %error,
                            "channel is not known to the bot yet; post something in it or add the bot again"
                        );
                        return None;
                    }
                };
                for chat in chats {
                    if let tl::enums::Chat::Channel(channel) = chat {
                        let _ = self.session.cache_peer(&PeerInfo::from(&channel)).await;
                    }
                }
                self.session.peer_ref_of(id).await
            }
            _ => {
                let request = tl::functions::users::GetUsers {
                    id: vec![
                        tl::types::InputUser {
                            user_id: id.bare_id()?,
                            access_hash: 0,
                        }
                        .into(),
                    ],
                };
                if let Ok(users) = self.client.invoke(&request).await {
                    for user in users {
                        if let tl::enums::User::User(user) = user {
                            let _ = self.session.cache_peer(&PeerInfo::from(&user)).await;
                        }
                    }
                }
                self.session.peer_ref_of(id).await
            }
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A callback button whose label falls back to a translated word when the given one is only an emoji.
    pub fn button(
        &self,
        lang: &str,
        text: &str,
        data: &str,
        color: crate::telegram::keyboards::Color,
    ) -> crate::telegram::keyboards::Button {
        use crate::telegram::keyboards::{Button, fallback_key};
        let button = Button::callback(text, data, color);
        if button.is_blank() {
            Button::callback(&self.t(lang, fallback_key(data), &[]), data, color)
        } else {
            button
        }
    }

    pub fn referral_link(&self, user_id: i64) -> String {
        format!("https://t.me/{}?start={user_id}", self.cfg.bot_username)
    }
}

pub async fn serve(config: Config, logs: Arc<crate::logging::Logs>) -> Result<()> {
    let pg = db::connect(&config.database_url).await?;
    db::migrate(&pg).await?;
    let locales = Locales::load(&config.locales_dir)?;

    let stored = sessions::get(&pg, &config.session_key, Kind::Bot, "rcd").await?;
    let data = match &stored {
        Some(session) => telegram::session_data(session),
        None => SessionData::default(),
    };
    let api_id = stored.as_ref().map_or(config.api_id as u32, |s| s.api_id);
    let session = Arc::new(PgSession::open(pg.clone(), "bot:rcd", data).await?);
    let connection = telegram::connect_with(session.clone(), api_id);
    let client = connection.client;
    let me = match stored {
        Some(_) => client.get_me().await.context("the bot could not sign in")?,
        None => {
            tracing::info!("no stored bot session, signing in with BOT_TOKEN");
            let me = client
                .bot_sign_in(&config.bot_token, &config.api_hash)
                .await
                .context("bot sign-in failed")?;
            let exported = telegram::export(
                session.memory(),
                api_id,
                me.id().bare_id().unwrap_or(0) as u64,
                true,
            )
            .context("the new bot session could not be read back")?;
            let account = sessions::Account {
                kind: Kind::Bot,
                label: "rcd",
                owner_id: None,
                phone: None,
            };
            sessions::put(&pg, &config.session_key, &account, &exported).await?;
            me
        }
    };
    tracing::info!(bot = me.username().unwrap_or(""), "bot connected");

    wipe_downloads(&config.data_dir);
    crate::monitor::requeue_running(&pg).await;
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let accounts =
        telegram::accounts::Accounts::new(pg.clone(), config.session_key, api_id, Some(events_tx));
    let pool_size = accounts.load_pool().await?;
    tracing::info!(pool_size, "download accounts ready");

    let bot = Arc::new(Bot {
        cfg: config,
        pg,
        locales,
        client: client.clone(),
        session,
        started: std::time::Instant::now(),
        me_id: me.id().bare_id().unwrap_or(0),
        accounts,
        running: std::sync::Mutex::new(std::collections::HashMap::new()),
        queue_wake: tokio::sync::Notify::new(),
        large_wake: tokio::sync::Notify::new(),
        start_image: tokio::sync::Mutex::new(None),
        broadcasts: std::sync::Mutex::new(std::collections::HashMap::new()),
        shutting_down: std::sync::atomic::AtomicBool::new(false),
    });
    commands::set_default(&bot).await;
    tokio::spawn(log_channel(bot.clone(), logs.clone()));
    tokio::spawn(log_upload(bot.clone(), logs));
    tokio::spawn(housekeeping::run(bot.clone()));
    // Fewer download accounts than this makes downloads queue up; the owners are told at start, as before.
    const REQUIRED_POOL: usize = 3;
    if pool_size < REQUIRED_POOL {
        let missing = (REQUIRED_POOL - pool_size).to_string();
        tracing::warn!(pool_size, missing, "not enough download accounts");
        bot.notify_owners("admin.pool.insufficient", |_| {
            vec![
                ("current".to_string(), pool_size.to_string()),
                ("missing".to_string(), missing.clone()),
            ]
        })
        .await;
    }
    {
        let bot = bot.clone();
        tokio::spawn(async move {
            if bot.cfg.start_image.is_some() && start_image::media(&bot).await.is_some() {
                tracing::info!("start image ready");
            }
        });
    }
    let leftovers = range::Leftovers::take(&bot).await?;
    tokio::spawn(range::resume_all(bot.clone(), leftovers));
    crate::monitor::spawn(bot.clone(), events_rx);
    let mut stream = client
        .stream_updates(
            connection.updates,
            UpdatesConfiguration {
                catch_up: false,
                update_queue_limit: Some(10_000),
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    loop {
        tokio::select! {
            update = stream.next() => {
                let update = update.context("update stream ended")?;
                let bot = bot.clone();
                tokio::spawn(async move {
                    if let Err(error) = dispatch(&bot, update).await {
                        tracing::error!(%error, "update handler failed");
                    }
                });
            }
            _ = shutdown_signal() => {
                tracing::info!("stopping");
                break;
            }
        }
    }
    shutdown(&bot).await;
    Ok(())
}

/// Resolves on Ctrl-C or SIGTERM (what pm2 and systemd send).
async fn shutdown_signal() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(term) => term,
        Err(error) => {
            tracing::warn!(%error, "SIGTERM handler unavailable, only Ctrl-C will stop the bot");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// Cancels running tasks without touching their queue rows (they run again on the next start), then clears partial downloads.
async fn shutdown(bot: &Arc<Bot>) {
    bot.shutting_down
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let tokens: Vec<_> = bot.running.lock().unwrap().values().cloned().collect();
    for token in &tokens {
        token.cancel();
    }
    for _ in 0..50 {
        if bot.running.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    wipe_downloads(&bot.cfg.data_dir);
}

fn wipe_downloads(data_dir: &std::path::Path) {
    let dir = data_dir.join("downloads");
    if let Err(error) = std::fs::remove_dir_all(&dir)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "could not clear the downloads folder");
    }
}

async fn dispatch(bot: &Arc<Bot>, update: Update) -> Result<()> {
    match update {
        Update::NewMessage(message) if !message.outgoing() => {
            guards::on_message(bot, message).await
        }
        Update::CallbackQuery(query) => guards::on_callback(bot, query).await,
        Update::Raw(raw) => on_raw(bot, raw.raw).await,
        _ => Ok(()),
    }
}

/// The bot being added to or removed from a channel or group.
async fn on_raw(bot: &Arc<Bot>, update: grammers_client::tl::enums::Update) -> Result<()> {
    use grammers_client::tl::enums::{ChannelParticipant as C, ChatParticipant as G, Update as U};
    match update {
        U::ChannelParticipant(u) if u.user_id == bot.me_id => {
            let was_out = matches!(
                u.prev_participant,
                None | Some(C::Left(_)) | Some(C::Banned(_))
            );
            let now_in =
                matches!(&u.new_participant, Some(p) if !matches!(p, C::Left(_) | C::Banned(_)));
            if let Some(chat) = PeerId::channel(u.channel_id) {
                uploadch::bot_membership(bot, chat, u.actor_id, was_out, now_in).await;
            }
        }
        U::BotPrecheckoutQuery(u) => billing::precheckout(bot, &u).await?,
        U::ChatParticipant(u) if u.user_id == bot.me_id => {
            let was_out = u.prev_participant.is_none();
            let now_in = matches!(
                &u.new_participant,
                Some(G::Participant(_) | G::Admin(_) | G::Creator(_))
            );
            if let Some(chat) = PeerId::chat(u.chat_id) {
                uploadch::bot_membership(bot, chat, u.actor_id, was_out, now_in).await;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Sends real faults to the log channel, one at a time.
async fn log_channel(bot: Arc<Bot>, logs: Arc<crate::logging::Logs>) {
    let Some(mut rx) = logs.take_channel() else {
        return;
    };
    let peer = loop {
        match resolve_log_channel(&bot).await {
            Some(peer) => break peer,
            None => tokio::time::sleep(std::time::Duration::from_secs(300)).await,
        }
    };
    while let Some(line) = rx.recv().await {
        let text = format!("<pre>{}</pre>", crate::logging::escape_pre(&line));
        for attempt in 1..=5u64 {
            match bot.client.send_message(peer, html(&text, None)).await {
                Ok(_) => break,
                Err(grammers_client::InvocationError::Rpc(rpc))
                    if rpc.name.starts_with("FLOOD") =>
                {
                    tokio::time::sleep(std::time::Duration::from_secs(
                        u64::from(rpc.value.unwrap_or(30)) + 1,
                    ))
                    .await;
                }
                Err(error) => {
                    if attempt == 5 {
                        eprintln!("[log channel] could not send a log: {error}");
                    } else {
                        tokio::time::sleep(std::time::Duration::from_secs(attempt * 2)).await;
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn resolve_log_channel(bot: &Bot) -> Option<grammers_client::session::types::PeerRef> {
    let id = PeerId::from_bot_api_dialog_id(bot.cfg.log_channel_id)?;
    bot.peer_ref_for(id).await
}

/// When the log file reaches a megabyte it goes to the owners and starts again empty.
async fn log_upload(bot: Arc<Bot>, logs: Arc<crate::logging::Logs>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        interval.tick().await;
        if logs.size() < crate::logging::UPLOAD_THRESHOLD {
            continue;
        }
        let Ok(Some(part)) = logs.rotate() else {
            continue;
        };
        let mut delivered = false;
        for &owner in &bot.cfg.owner_ids {
            let lang = bot.lang(owner).await;
            let name = part
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some(peer) = bot.user_ref(owner).await
                && let Ok(file) = bot.client.upload_file(&part).await
                && bot
                    .client
                    .send_message(
                        peer,
                        grammers_client::message::InputMessage::new()
                            .html(bot.t(&lang, "admin.logs.file_caption", &[("filename", &name)]))
                            .document(file),
                    )
                    .await
                    .is_ok()
            {
                delivered = true;
            }
        }
        if delivered {
            std::fs::remove_file(&part).ok();
        } else {
            // Nothing was sent: keep the part where /admin can still reach it.
            tracing::warn!(
                "a full log part could not be sent and was kept as {}",
                part.display()
            );
        }
    }
}
