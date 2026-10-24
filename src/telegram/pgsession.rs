//! grammers session that keeps its state in memory and writes the peer access hashes to Postgres,
//! so the bot can message its users again after a restart.

use grammers_session::{
    BoxFuture, Session, SessionData,
    storages::MemorySession,
    types::{DcOption, PeerAuth, PeerId, PeerInfo, UpdateState, UpdatesState},
};
use sqlx::PgPool;

pub struct PgSession {
    inner: MemorySession,
    pg: PgPool,
    key: String,
    /// The account itself. grammers asks for it as `PeerId::self_user()`, which `MemorySession` does not map,
    /// and without it an update stream never asks Telegram for updates (a pool account would see no posts).
    me: std::sync::Mutex<Option<PeerInfo>>,
}

/// grammers needs a concrete `Error` type, which a boxed error is not.
#[derive(Debug)]
pub struct SessionFailure(Box<dyn std::error::Error + Send + Sync + 'static>);

impl std::fmt::Display for SessionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for SessionFailure {}

fn wrap<E: std::error::Error + Send + Sync + 'static>(error: E) -> SessionFailure {
    SessionFailure(Box::new(error))
}

type Err = SessionFailure;

fn entry(peer: &PeerInfo) -> Option<(i64, i64)> {
    match *peer {
        PeerInfo::User {
            id,
            auth: Some(auth),
            ..
        } => PeerId::user(id)?
            .bot_api_dialog_id()
            .map(|d| (d, auth.hash())),
        PeerInfo::Channel {
            id,
            auth: Some(auth),
            ..
        } => PeerId::channel(id)?
            .bot_api_dialog_id()
            .map(|d| (d, auth.hash())),
        _ => None,
    }
}

impl PgSession {
    /// `key` names the session in `peer_cache` (for example `bot:rcd`). Cached peers are loaded now.
    pub async fn open(pg: PgPool, key: &str, mut data: SessionData) -> Result<Self, sqlx::Error> {
        let rows: Vec<(i64, i64)> =
            sqlx::query_as("SELECT dialog_id, access_hash FROM peer_cache WHERE session_key = $1")
                .bind(key)
                .fetch_all(&pg)
                .await?;
        for (dialog_id, hash) in rows {
            let Some(peer) = PeerId::from_bot_api_dialog_id(dialog_id) else {
                continue;
            };
            let Some(id) = peer.bare_id() else { continue };
            let auth = Some(PeerAuth::from_hash(hash));
            let info = if dialog_id > 0 {
                PeerInfo::User {
                    id,
                    auth,
                    bot: None,
                    is_self: None,
                }
            } else {
                PeerInfo::Channel {
                    id,
                    auth,
                    kind: None,
                }
            };
            data.peer_infos.insert(peer, info);
        }
        Ok(Self {
            inner: MemorySession::from(data),
            pg,
            key: key.to_string(),
            me: std::sync::Mutex::new(None),
        })
    }

    pub fn memory(&self) -> &MemorySession {
        &self.inner
    }
}

impl Session for PgSession {
    type Error = Err;

    fn home_dc_id(&self) -> Result<i32, Err> {
        self.inner.home_dc_id().map_err(wrap)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), Err>> {
        Box::pin(async move { self.inner.set_home_dc_id(dc_id).await.map_err(wrap) })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, Err> {
        self.inner.dc_option(dc_id).map_err(wrap)
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), Err>> {
        let dc = dc_option.clone();
        Box::pin(async move { self.inner.set_dc_option(&dc).await.map_err(wrap) })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, Err>> {
        Box::pin(async move {
            if peer == PeerId::self_user() {
                return Ok(self.me.lock().unwrap().clone());
            }
            self.inner.peer(peer).await.map_err(wrap)
        })
    }

    fn cache_peer(&self, peer: &PeerInfo) -> BoxFuture<'_, Result<(), Err>> {
        let peer = peer.clone();
        Box::pin(async move {
            self.inner.cache_peer(&peer).await.map_err(wrap)?;
            if let PeerInfo::User {
                is_self: Some(true),
                ..
            } = peer
            {
                *self.me.lock().unwrap() = self.inner.peer(peer.id()).await.map_err(wrap)?;
            }
            if let Some((dialog_id, hash)) = entry(&peer) {
                let result = sqlx::query(
                    "INSERT INTO peer_cache (session_key, dialog_id, access_hash) VALUES ($1, $2, $3) \
                     ON CONFLICT (session_key, dialog_id) DO UPDATE SET access_hash = EXCLUDED.access_hash \
                     WHERE peer_cache.access_hash <> EXCLUDED.access_hash",
                )
                .bind(&self.key)
                .bind(dialog_id)
                .bind(hash)
                .execute(&self.pg)
                .await;
                if let Err(error) = result {
                    tracing::warn!(%error, "could not save a peer access hash");
                }
            }
            Ok(())
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, Err>> {
        Box::pin(async move { self.inner.updates_state().await.map_err(wrap) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), Err>> {
        Box::pin(async move { self.inner.set_update_state(update).await.map_err(wrap) })
    }
}

impl PgSession {
    /// Reference for a cached user or channel.
    pub async fn peer_ref_of(&self, peer: PeerId) -> Option<grammers_session::types::PeerRef> {
        Session::peer_ref(self, peer).await.ok().flatten()
    }

    /// The chat a username named within the last eight hours, as Pyrogram's session remembered it.
    pub async fn username(&self, username: &str) -> Option<grammers_session::types::PeerRef> {
        let dialog_id: i64 = sqlx::query_scalar(
            "SELECT dialog_id FROM username_cache WHERE session_key = $1 AND username = $2 \
             AND resolved_at > NOW() - INTERVAL '8 hours'",
        )
        .bind(&self.key)
        .bind(username.to_lowercase())
        .fetch_optional(&self.pg)
        .await
        .ok()??;
        self.peer_ref_of(PeerId::from_bot_api_dialog_id(dialog_id)?)
            .await
    }

    pub async fn remember_username(&self, username: &str, peer: PeerId) {
        let Some(dialog_id) = peer.bot_api_dialog_id() else {
            return;
        };
        let result = sqlx::query(
            "INSERT INTO username_cache (session_key, username, dialog_id) VALUES ($1, $2, $3) \
             ON CONFLICT (session_key, username) DO UPDATE SET dialog_id = EXCLUDED.dialog_id, resolved_at = NOW()",
        )
        .bind(&self.key)
        .bind(username.to_lowercase())
        .bind(dialog_id)
        .execute(&self.pg)
        .await;
        if let Err(error) = result {
            tracing::warn!(%error, "could not save a username");
        }
    }
}
