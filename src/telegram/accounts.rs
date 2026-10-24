//! The user accounts the bot downloads with: a shared pool, and each user's own linked account.
//!
//! A linked account stays connected only while it works (8 at most at once). A session that Telegram
//! no longer accepts is deleted from the store.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use grammers_client::{Client, InvocationError};
use sqlx::PgPool;
use tokio::sync::{RwLock, Semaphore};

use super::{pgsession::PgSession, session_data};
use crate::{
    media::download::{Failure, classify},
    sessions::{self, Kind},
};

pub const LINKED_SLOTS: usize = 8;
pub const LINKED_IDLE: Duration = Duration::from_secs(5 * 60);

pub struct Account {
    pub kind: Kind,
    /// Pool file name, or the owner's Telegram user id for a linked account.
    pub label: String,
    pub owner_id: Option<i64>,
    client: RwLock<Client>,
    pub session: Arc<PgSession>,
    pub telegram_id: AtomicU64,
    premium: std::sync::atomic::AtomicBool,
    last_used: Mutex<Instant>,
    ops: AtomicUsize,
    _slot: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl Account {
    pub async fn client(&self) -> Client {
        *self.last_used.lock().unwrap() = Instant::now();
        self.client.read().await.clone()
    }

    pub fn is_premium(&self) -> bool {
        self.premium.load(Ordering::Relaxed)
    }

    pub fn is_linked(&self) -> bool {
        self.kind == Kind::Linked
    }

    /// Marks work in flight, so the idle sweeper leaves the connection alone.
    pub fn busy(self: &Arc<Self>) -> Busy {
        self.ops.fetch_add(1, Ordering::Relaxed);
        Busy(self.clone())
    }

    fn idle_for(&self) -> Duration {
        self.last_used.lock().unwrap().elapsed()
    }
}

pub struct Busy(Arc<Account>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.ops.fetch_sub(1, Ordering::Relaxed);
        *self.0.last_used.lock().unwrap() = Instant::now();
    }
}

pub struct Accounts {
    pg: PgPool,
    key: [u8; 32],
    api_id: u32,
    pool: RwLock<Vec<Arc<Account>>>,
    next: AtomicUsize,
    linked: tokio::sync::Mutex<HashMap<i64, Arc<Account>>>,
    slots: Arc<Semaphore>,
    /// `(chat dialog id, message id)` of every new post a pool account sees in a channel or group.
    events: Option<tokio::sync::mpsc::UnboundedSender<(i64, i32)>>,
    /// Pool labels that could not start; not retried every minute until the next start.
    failed: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// What a connection attempt found out about the session.
enum Opened {
    Ready(Arc<Account>),
    Dead,
    /// Nothing is stored under this label (a user who never linked an account).
    Missing,
}

impl Accounts {
    pub fn new(
        pg: PgPool,
        key: [u8; 32],
        api_id: u32,
        events: Option<tokio::sync::mpsc::UnboundedSender<(i64, i32)>>,
    ) -> Self {
        Self {
            pg,
            key,
            api_id,
            pool: RwLock::new(Vec::new()),
            next: AtomicUsize::new(0),
            linked: tokio::sync::Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(LINKED_SLOTS)),
            events,
            failed: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    async fn open(
        &self,
        kind: Kind,
        label: &str,
        owner_id: Option<i64>,
        slot: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<Opened> {
        let Some(stored) = sessions::get(&self.pg, &self.key, kind, label).await? else {
            return Ok(Opened::Missing);
        };
        let key = format!(
            "{}:{label}",
            match kind {
                Kind::Bot => "bot",
                Kind::Pool => "pool",
                Kind::Linked => "linked",
            }
        );
        let session =
            Arc::new(PgSession::open(self.pg.clone(), &key, session_data(&stored)).await?);
        let connection = super::connect_with(session.clone(), self.api_id);
        let client = connection.client;
        // A cheap call that proves the key is still accepted and tells whether the account is Premium.
        let me = match tokio::time::timeout(Duration::from_secs(30), client.get_me()).await {
            Ok(Ok(me)) => me,
            Ok(Err(error)) => {
                client.disconnect();
                return match classify(&error) {
                    Failure::SessionDead => Ok(Opened::Dead),
                    _ => Err(anyhow::anyhow!(
                        "account {label} could not connect: {error}"
                    )),
                };
            }
            Err(_) => {
                client.disconnect();
                anyhow::bail!("account {label} did not answer in time");
            }
        };
        let premium = matches!(&me.raw, grammers_client::tl::enums::User::User(u) if u.premium);
        // `get_me` does not cache the account itself, and an update stream only subscribes to updates when the
        // session knows it (Pyrogram's pool clients always had it).
        if let grammers_client::tl::enums::User::User(user) = &me.raw {
            use grammers_session::Session;
            if let Err(error) = session
                .cache_peer(&grammers_session::types::PeerInfo::from(user))
                .await
            {
                tracing::warn!(account = %label, %error, "could not remember the account itself");
            }
        }
        match (&self.events, kind) {
            (Some(events), Kind::Pool) => spawn_event_stream(
                client.clone(),
                connection.updates,
                events.clone(),
                label.to_string(),
            ),
            _ => drop(connection.updates),
        }
        Ok(Opened::Ready(Arc::new(Account {
            kind,
            label: label.to_string(),
            owner_id,
            client: RwLock::new(client),
            session,
            telegram_id: AtomicU64::new(me.id().bare_id().unwrap_or(0) as u64),
            premium: std::sync::atomic::AtomicBool::new(premium),
            last_used: Mutex::new(Instant::now()),
            ops: AtomicUsize::new(0),
            _slot: slot,
        })))
    }

    /// Connects every pool session. A pool account that fails is logged and skipped.
    pub async fn load_pool(&self) -> Result<usize> {
        let labels: Vec<(String,)> =
            sqlx::query_as("SELECT label FROM account_sessions WHERE kind = 'pool' ORDER BY label")
                .fetch_all(&self.pg)
                .await?;
        let mut accounts = Vec::new();
        for (label,) in labels {
            match self.open(Kind::Pool, &label, None, None).await {
                Ok(Opened::Ready(account)) => accounts.push(account),
                // Removed between listing and opening.
                Ok(Opened::Missing) => {}
                Ok(Opened::Dead) => {
                    tracing::error!(pool = %label, "pool session is no longer authorized");
                    self.failed.lock().unwrap().insert(label);
                }
                Err(error) => {
                    tracing::error!(pool = %label, %error, "pool account could not start");
                    self.failed.lock().unwrap().insert(label);
                }
            }
        }
        let count = accounts.len();
        *self.pool.write().await = accounts;
        Ok(count)
    }

    /// Connects pool sessions stored since the last load (`rcd pool-login` while the bot runs).
    pub async fn refresh_pool(&self) -> usize {
        let Ok(labels) = sqlx::query_as::<_, (String,)>(
            "SELECT label FROM account_sessions WHERE kind = 'pool' ORDER BY label",
        )
        .fetch_all(&self.pg)
        .await
        else {
            return 0;
        };
        let mut loaded: Vec<String> = self
            .pool
            .read()
            .await
            .iter()
            .map(|a| a.label.clone())
            .collect();
        loaded.extend(self.failed.lock().unwrap().iter().cloned());
        let mut added = 0;
        for (label,) in labels.into_iter().filter(|(l,)| !loaded.contains(l)) {
            match self.open(Kind::Pool, &label, None, None).await {
                Ok(Opened::Ready(account)) => {
                    tracing::info!(pool = %label, "new download account connected");
                    self.pool.write().await.push(account);
                    added += 1;
                }
                // Removed between listing and opening.
                Ok(Opened::Missing) => {}
                Ok(Opened::Dead) => {
                    tracing::error!(pool = %label, "pool session is no longer authorized");
                    self.failed.lock().unwrap().insert(label);
                }
                Err(error) => {
                    tracing::error!(pool = %label, %error, "pool account could not start");
                    self.failed.lock().unwrap().insert(label);
                }
            }
        }
        added
    }

    /// The user's own account, connected on demand. `None` when nothing is linked or the session died.
    pub async fn linked(&self, user_id: i64) -> Result<Option<Arc<Account>>> {
        let mut active = self.linked.lock().await;
        if let Some(existing) = active.get(&user_id) {
            return Ok(Some(existing.clone()));
        }
        let label = user_id.to_string();
        let slot = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .context("account slots closed")?;
        match self
            .open(Kind::Linked, &label, Some(user_id), Some(slot))
            .await
        {
            Ok(Opened::Ready(account)) => {
                active.insert(user_id, account.clone());
                Ok(Some(account))
            }
            Ok(Opened::Dead) => {
                drop(active);
                self.forget_linked(user_id).await?;
                Ok(None)
            }
            // Treating "never linked" as a dead session would silently stop the user's monitor.
            Ok(Opened::Missing) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The first pool account (the one monitors of public channels use).
    pub async fn first_pool(&self) -> Option<Arc<Account>> {
        self.pool.read().await.first().cloned()
    }

    /// Disconnects a linked account right after a short job, instead of waiting for the idle sweep.
    pub async fn release_linked(&self, user_id: i64) {
        if let Some(account) = self.linked.lock().await.remove(&user_id) {
            account.client.read().await.disconnect();
        }
    }

    /// Deletes a linked session Telegram no longer accepts, and the monitor that depended on it.
    pub async fn forget_linked(&self, user_id: i64) -> Result<()> {
        if let Some(account) = self.linked.lock().await.remove(&user_id) {
            account.client.read().await.disconnect();
        }
        sessions::remove(&self.pg, Kind::Linked, &user_id.to_string()).await?;
        sqlx::query(
            "UPDATE users SET monitor_target_id = NULL, monitor_flags = NULL WHERE user_id = $1",
        )
        .bind(user_id)
        .execute(&self.pg)
        .await?;
        Ok(())
    }

    /// The account to download with: the user's own, else the next pool account in turn.
    pub async fn for_download(&self, user_id: i64) -> Result<Arc<Account>> {
        if let Some(own) = self.linked(user_id).await? {
            return Ok(own);
        }
        if self.pool.read().await.is_empty() {
            self.refresh_pool().await;
        }
        let pool = self.pool.read().await;
        anyhow::ensure!(!pool.is_empty(), "no download account is available");
        let turn = self.next.fetch_add(1, Ordering::Relaxed);
        Ok(pool[turn % pool.len()].clone())
    }

    /// Replaces a connection that stopped answering with a fresh one from the stored session.
    pub async fn reconnect(&self, account: &Account) -> bool {
        tokio::time::sleep(Duration::from_secs(1)).await;
        match self
            .open(account.kind, &account.label, account.owner_id, None)
            .await
        {
            Ok(Opened::Ready(fresh)) => {
                let new_client = fresh.client.read().await.clone();
                let mut slot = account.client.write().await;
                slot.disconnect();
                *slot = new_client;
                true
            }
            _ => false,
        }
    }

    /// Closes linked accounts nobody has used for a while and that have no open task.
    pub async fn close_idle(&self, has_open_task: impl Fn(i64) -> bool) {
        let mut active = self.linked.lock().await;
        let idle: Vec<i64> = active
            .iter()
            .filter(|(user, account)| {
                account.ops.load(Ordering::Relaxed) == 0
                    && account.idle_for() >= LINKED_IDLE
                    && !has_open_task(**user)
            })
            .map(|(user, _)| *user)
            .collect();
        for user in idle {
            if let Some(account) = active.remove(&user)
                && let Ok(client) = account.client.try_read()
            {
                client.disconnect();
            }
        }
    }

    pub async fn shutdown(&self) {
        for (_, account) in self.linked.lock().await.drain() {
            account.client.read().await.disconnect();
        }
        for account in self.pool.write().await.drain(..) {
            account.client.read().await.disconnect();
        }
    }

    pub fn is_dead(error: &InvocationError) -> bool {
        matches!(classify(error), Failure::SessionDead)
    }
}

fn spawn_event_stream(
    client: Client,
    updates: tokio::sync::mpsc::UnboundedReceiver<grammers_client::session::updates::UpdatesLike>,
    events: tokio::sync::mpsc::UnboundedSender<(i64, i32)>,
    label: String,
) {
    use grammers_client::{client::UpdatesConfiguration, session::types::PeerKind, update::Update};
    tokio::spawn(async move {
        let config = UpdatesConfiguration {
            catch_up: false,
            update_queue_limit: Some(2000),
        };
        let mut stream = match client.stream_updates(updates, config).await {
            Ok(stream) => {
                tracing::debug!(pool = %label, "pool update stream started");
                stream
            }
            Err(error) => {
                tracing::error!(pool = %label, %error, "could not stream pool updates");
                return;
            }
        };
        loop {
            match stream.next().await {
                // Outgoing ones count too, as in the Python bot: in a channel the account administers every
                // post arrives marked outgoing.
                Ok(Update::NewMessage(message)) => {
                    let id = message.peer_id();
                    tracing::debug!(pool = %label, chat = ?id, message = message.id(), "pool saw a new message");
                    if id.kind() != PeerKind::User
                        && let Some(dialog) = id.bot_api_dialog_id()
                    {
                        let _ = events.send((dialog, message.id()));
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(pool = %label, %error, "pool update stream ended");
                    return;
                }
            }
        }
    });
}
