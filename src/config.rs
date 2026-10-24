use std::{env, path::PathBuf};

use anyhow::{Context, Result, bail};

pub struct Config {
    pub api_id: i32,
    pub api_hash: String,
    pub bot_token: String,
    pub bot_username: String,
    pub owner_ids: Vec<i64>,
    pub admin_username: String,
    pub log_channel_id: i64,
    pub database_url: String,
    pub session_key: [u8; 32],
    pub pool_phones: Vec<String>,
    /// Link of the channel behind the Channel button; the button is left out when unset.
    pub channel_link: Option<String>,
    pub data_dir: PathBuf,
    pub locales_dir: PathBuf,
    /// Welcome photo: a direct link or a local file path. Without it or without an archive channel `/start` sends text only.
    pub start_image: Option<String>,
    /// Channel where the bot keeps files it reuses (the welcome photo).
    pub archive_channel_id: Option<i64>,
}

fn required(name: &str) -> Result<String> {
    match env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
        _ => bail!("environment variable {name} is missing"),
    }
}

fn optional(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse_owner_ids(raw: &str) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        ids.push(
            part.parse::<i64>()
                .with_context(|| "OWNER_IDS must be comma separated integers")?,
        );
    }
    if ids.is_empty() {
        bail!("OWNER_IDS must list at least one user id");
    }
    Ok(ids)
}

fn parse_key(raw: &str) -> Result<[u8; 32]> {
    let bytes =
        hex_decode(raw.trim()).context("SESSION_ENCRYPTION_KEY must be 64 hex characters")?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("SESSION_ENCRYPTION_KEY must be 64 hex characters"))
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn database_url() -> Result<String> {
    if let Ok(url) = env::var("DATABASE_URL")
        && !url.trim().is_empty()
    {
        return Ok(url);
    }
    let user = required("DB_USER")?;
    let password = required("DB_PASSWORD")?;
    let host = required("DB_HOST")?;
    let port = required("DB_PORT")?;
    let name = required("DB_NAME")?;
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    Ok(format!(
        "postgres://{}:{}@{host}:{port}/{name}",
        enc(&user),
        enc(&password)
    ))
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            api_id: required("API_ID")?
                .parse()
                .context("API_ID must be an integer")?,
            api_hash: required("API_HASH")?,
            bot_token: required("BOT_TOKEN")?,
            bot_username: required("BOT_USERNAME")?
                .trim_start_matches('@')
                .to_string(),
            owner_ids: parse_owner_ids(&required("OWNER_IDS")?)?,
            admin_username: required("ADMIN_USERNAME")?
                .trim_start_matches('@')
                .to_string(),
            log_channel_id: required("LOG_CHANNEL_ID")?
                .parse()
                .context("LOG_CHANNEL_ID must be an integer")?,
            database_url: database_url()?,
            session_key: parse_key(&required("SESSION_ENCRYPTION_KEY")?)?,
            pool_phones: env::var("POOL_PHONES")
                .unwrap_or_default()
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect(),
            channel_link: env::var("CHANNEL_LINK")
                .ok()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty()),
            data_dir: PathBuf::from(env::var("DATA_DIR").unwrap_or_else(|_| "data".into())),
            start_image: optional("START_IMAGE"),
            archive_channel_id: match optional("ARCHIVE_CHANNEL_ID") {
                Some(id) => Some(
                    id.parse()
                        .context("ARCHIVE_CHANNEL_ID must be an integer")?,
                ),
                None => None,
            },
            locales_dir: PathBuf::from(
                env::var("LOCALES_DIR").unwrap_or_else(|_| "locales".into()),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_ids_parse() {
        assert_eq!(parse_owner_ids("1, 2,3").unwrap(), vec![1, 2, 3]);
        assert!(parse_owner_ids("").is_err());
        assert!(parse_owner_ids("a").is_err());
    }

    #[test]
    fn key_must_be_32_bytes_hex() {
        assert!(parse_key(&"ab".repeat(32)).is_ok());
        assert!(parse_key("abcd").is_err());
        assert!(parse_key(&"zz".repeat(32)).is_err());
    }
}
