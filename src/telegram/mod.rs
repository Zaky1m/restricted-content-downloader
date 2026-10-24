//! MTProto connections. A session is the home DC and its auth key; the Python bot's Pyrogram
//! sessions convert to it directly, so no account has to log in again.

pub mod accounts;
pub mod keyboards;
pub mod pgsession;

use std::sync::Arc;

use grammers_client::Client;
use grammers_mtsender::SenderPool;
use grammers_session::{Session, SessionData, storages::MemorySession, types::DcOption};

use crate::pyrogram_session::PyrogramSession;

pub fn session_data(session: &PyrogramSession) -> SessionData {
    let mut data = SessionData::default();
    let dc_id = i32::from(session.dc_id);
    data.home_dc = dc_id;
    if let Some(option) = data.dc_options.get_mut(&dc_id) {
        option.auth_key = Some(session.auth_key);
    }
    data
}

/// How every connection introduces itself to Telegram. grammers would take the language from the server's
/// `LANG` (`C.UTF-8` becomes the invalid code `C`), and Telegram then withholds login codes; the values below
/// are real ones, like the Python bot sent.
fn connection_params() -> grammers_mtsender::ConnectionParams {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|r| r.trim().to_string())
        .unwrap_or_default();
    grammers_mtsender::ConnectionParams {
        device_model: "PC 64bit".to_string(),
        system_version: format!("Linux {release}").trim().to_string(),
        app_version: format!("RCD {}", env!("CARGO_PKG_VERSION")),
        system_lang_code: "en-US".to_string(),
        lang_code: "en".to_string(),
        ..Default::default()
    }
}

/// A running client with its update stream, which only a bot consumes.
pub struct Connection {
    pub client: Client,
    pub updates: tokio::sync::mpsc::UnboundedReceiver<grammers_session::updates::UpdatesLike>,
}

pub fn connect_with<S>(session: Arc<S>, api_id: u32) -> Connection
where
    S: Session,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let SenderPool {
        runner,
        handle,
        updates,
    } = SenderPool::with_configuration(session, api_id as i32, connection_params());
    tokio::spawn(runner.run());
    Connection {
        client: Client::new(handle),
        updates,
    }
}

/// Pool and linked accounts keep no state beyond the auth key.
pub fn connect(session: &PyrogramSession) -> (Connection, Arc<MemorySession>) {
    let memory = Arc::new(MemorySession::from(session_data(session)));
    (connect_with(memory.clone(), session.api_id), memory)
}

/// The session as Pyrogram stores it, read back from a connected `MemorySession`
/// (after a fresh login, to persist it encrypted).
pub fn export(
    session: &MemorySession,
    api_id: u32,
    user_id: u64,
    is_bot: bool,
) -> Option<PyrogramSession> {
    let dc_id = session.home_dc_id().ok()?;
    let option: DcOption = session.dc_option(dc_id).ok()??;
    Some(PyrogramSession {
        dc_id: u8::try_from(dc_id).ok()?,
        api_id,
        test_mode: false,
        auth_key: option.auth_key?,
        user_id,
        is_bot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pyrogram_session_becomes_grammers_session_and_back() {
        let source = PyrogramSession {
            dc_id: 4,
            api_id: 99,
            test_mode: false,
            auth_key: [3; 256],
            user_id: 5,
            is_bot: true,
        };
        let data = session_data(&source);
        assert_eq!(data.home_dc, 4);
        assert_eq!(data.dc_options[&4].auth_key, Some([3; 256]));
        assert!(
            data.dc_options
                .iter()
                .filter(|(id, _)| **id != 4)
                .all(|(_, o)| o.auth_key.is_none())
        );
        let back = export(&MemorySession::from(data), 99, 5, true).unwrap();
        assert_eq!(back.encode(), source.encode());
    }
}
