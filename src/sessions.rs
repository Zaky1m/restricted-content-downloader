//! Encrypted session store and the idempotent import of the Python bot's sessions.

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::{
    PgPool, Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use crate::{crypto, pyrogram_session::PyrogramSession};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Bot,
    Pool,
    Linked,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Bot => "bot",
            Kind::Pool => "pool",
            Kind::Linked => "linked",
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct ImportReport {
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub skipped: usize,
}

fn aad(kind: Kind, label: &str) -> Vec<u8> {
    format!("{}:{label}", kind.as_str()).into_bytes()
}

pub struct Account<'a> {
    pub kind: Kind,
    pub label: &'a str,
    pub owner_id: Option<i64>,
    pub phone: Option<&'a str>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Stored {
    Added,
    Updated,
    Unchanged,
}

pub async fn put(
    pg: &PgPool,
    key: &[u8; 32],
    account: &Account<'_>,
    session: &PyrogramSession,
) -> Result<Stored> {
    let plain = session.encode();
    let existing = sqlx::query(
        "SELECT nonce, ciphertext FROM account_sessions WHERE kind = $1 AND label = $2",
    )
    .bind(account.kind.as_str())
    .bind(account.label)
    .fetch_optional(pg)
    .await?;
    if let Some(row) = &existing {
        let nonce: Vec<u8> = row.get("nonce");
        let cipher: Vec<u8> = row.get("ciphertext");
        let same = crypto::open(key, &aad(account.kind, account.label), &nonce, &cipher)
            .map(|p| p == plain.as_bytes())
            .unwrap_or(false);
        if same {
            return Ok(Stored::Unchanged);
        }
    }
    let sealed = crypto::seal(key, &aad(account.kind, account.label), plain.as_bytes());
    sqlx::query(
        "INSERT INTO account_sessions (kind, label, owner_id, phone, telegram_user_id, nonce, ciphertext) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (kind, label) DO UPDATE SET owner_id = EXCLUDED.owner_id, phone = EXCLUDED.phone, \
         telegram_user_id = EXCLUDED.telegram_user_id, nonce = EXCLUDED.nonce, \
         ciphertext = EXCLUDED.ciphertext, updated_at = NOW()",
    )
    .bind(account.kind.as_str())
    .bind(account.label)
    .bind(account.owner_id)
    .bind(account.phone)
    .bind(session.user_id as i64)
    .bind(sealed.nonce.as_slice())
    .bind(&sealed.ciphertext)
    .execute(pg)
    .await?;
    Ok(if existing.is_some() {
        Stored::Updated
    } else {
        Stored::Added
    })
}

pub async fn get(
    pg: &PgPool,
    key: &[u8; 32],
    kind: Kind,
    label: &str,
) -> Result<Option<PyrogramSession>> {
    let Some(row) = sqlx::query(
        "SELECT nonce, ciphertext FROM account_sessions WHERE kind = $1 AND label = $2",
    )
    .bind(kind.as_str())
    .bind(label)
    .fetch_optional(pg)
    .await?
    else {
        return Ok(None);
    };
    let nonce: Vec<u8> = row.get("nonce");
    let cipher: Vec<u8> = row.get("ciphertext");
    let plain = crypto::open(key, &aad(kind, label), &nonce, &cipher)?;
    let text = String::from_utf8(plain).context("stored session is not text")?;
    Ok(Some(PyrogramSession::decode(&text)?))
}

pub async fn remove(pg: &PgPool, kind: Kind, label: &str) -> Result<bool> {
    let done = sqlx::query("DELETE FROM account_sessions WHERE kind = $1 AND label = $2")
        .bind(kind.as_str())
        .bind(label)
        .execute(pg)
        .await?;
    Ok(done.rows_affected() > 0)
}

fn tally(report: &mut ImportReport, stored: Stored) {
    match stored {
        Stored::Added => report.added += 1,
        Stored::Updated => report.updated += 1,
        Stored::Unchanged => report.unchanged += 1,
    }
}

/// Copies every `user_sessions.session_string` into the encrypted store. Read only on the source.
pub async fn import_linked(pg: &PgPool, key: &[u8; 32]) -> Result<ImportReport> {
    let rows = sqlx::query(
        "SELECT user_id, phone_number, session_string FROM user_sessions ORDER BY user_id",
    )
    .fetch_all(pg)
    .await?;
    let mut report = ImportReport::default();
    for row in rows {
        let user_id: i64 = row.get("user_id");
        let phone: String = row.get("phone_number");
        let text: String = row.get("session_string");
        let Ok(session) = PyrogramSession::decode(&text) else {
            tracing::warn!(user_id, "linked session string is not valid, skipped");
            report.skipped += 1;
            continue;
        };
        let label = user_id.to_string();
        let account = Account {
            kind: Kind::Linked,
            label: &label,
            owner_id: Some(user_id),
            phone: Some(&phone),
        };
        tally(&mut report, put(pg, key, &account, &session).await?);
    }
    Ok(report)
}

async fn read_sqlite_session(path: &Path) -> Result<PyrogramSession> {
    let options = SqliteConnectOptions::new().filename(path).read_only(true);
    let lite: SqlitePool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let row = sqlx::query(
        "SELECT dc_id, api_id, test_mode, auth_key, user_id, is_bot FROM sessions LIMIT 1",
    )
    .fetch_one(&lite)
    .await?;
    let auth: Vec<u8> = row.get("auth_key");
    let session = PyrogramSession {
        dc_id: row.get::<i64, _>("dc_id") as u8,
        api_id: row.get::<i64, _>("api_id") as u32,
        test_mode: row.get::<i64, _>("test_mode") != 0,
        auth_key: auth
            .try_into()
            .map_err(|_| anyhow::anyhow!("auth key is not 256 bytes"))?,
        user_id: row.get::<i64, _>("user_id") as u64,
        is_bot: row.get::<i64, _>("is_bot") != 0,
    };
    lite.close().await;
    Ok(session)
}

/// Imports Pyrogram `.session` files. The file named `bot_file` becomes the bot session, other
/// user accounts become pool sessions, and anything in `skip` or any other bot is left out.
pub async fn import_pool_files(
    pg: &PgPool,
    key: &[u8; 32],
    dir: &Path,
    bot_file: &str,
    skip: &[&str],
) -> Result<ImportReport> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "session"))
        .collect();
    paths.sort();
    let mut report = ImportReport::default();
    for path in paths {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if skip.contains(&stem.as_str()) {
            report.skipped += 1;
            continue;
        }
        let session = match read_sqlite_session(&path).await {
            Ok(s) => s,
            Err(error) => {
                tracing::warn!(file = %stem, %error, "session file could not be read, skipped");
                report.skipped += 1;
                continue;
            }
        };
        let kind = match (session.is_bot, stem == bot_file) {
            (true, true) => Kind::Bot,
            (false, _) => Kind::Pool,
            (true, false) => {
                report.skipped += 1;
                continue;
            }
        };
        let account = Account {
            kind,
            label: &stem,
            owner_id: None,
            phone: None,
        };
        tally(&mut report, put(pg, key, &account, &session).await?);
        let peer_key = format!("{}:{stem}", kind.as_str());
        match import_peers(pg, &path, &peer_key).await {
            Ok(count) => tracing::info!(file = %stem, count, "peer access hashes imported"),
            Err(error) => {
                tracing::warn!(file = %stem, %error, "peer access hashes were not imported")
            }
        }
    }
    Ok(report)
}

/// Copies the access hashes a Pyrogram session file has seen (users, bots and channels).
pub async fn import_peers(pg: &PgPool, path: &Path, session_key: &str) -> Result<usize> {
    let options = SqliteConnectOptions::new().filename(path).read_only(true);
    let lite: SqlitePool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let rows = sqlx::query("SELECT id, access_hash FROM peers WHERE type IN ('user', 'bot', 'channel', 'supergroup', 'forum') AND access_hash IS NOT NULL")
        .fetch_all(&lite)
        .await?;
    lite.close().await;
    let ids: Vec<i64> = rows.iter().map(|r| r.get::<i64, _>("id")).collect();
    let hashes: Vec<i64> = rows
        .iter()
        .map(|r| r.get::<i64, _>("access_hash"))
        .collect();
    for (id_chunk, hash_chunk) in ids.chunks(5000).zip(hashes.chunks(5000)) {
        sqlx::query(
            "INSERT INTO peer_cache (session_key, dialog_id, access_hash) \
             SELECT $1, d, h FROM unnest($2::bigint[], $3::bigint[]) AS t(d, h) \
             ON CONFLICT (session_key, dialog_id) DO UPDATE SET access_hash = EXCLUDED.access_hash",
        )
        .bind(session_key)
        .bind(id_chunk)
        .bind(hash_chunk)
        .execute(pg)
        .await?;
    }
    Ok(ids.len())
}
