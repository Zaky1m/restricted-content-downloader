//! Pyrogram's session string, so linked accounts move over without a new
//! login: `>BI?256sQ?` packed, base64url without padding.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

const PACKED_LEN: usize = 1 + 4 + 1 + 256 + 8 + 1;

/// No `Debug`: the auth key is a full login to the account.
#[derive(Clone, PartialEq, Eq)]
pub struct PyrogramSession {
    pub dc_id: u8,
    pub api_id: u32,
    pub test_mode: bool,
    pub auth_key: [u8; 256],
    pub user_id: u64,
    pub is_bot: bool,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionStringError {
    #[error("session string is not base64url")]
    Encoding,
    #[error("session string has {0} bytes, expected {PACKED_LEN}")]
    Length(usize),
}

impl PyrogramSession {
    pub fn decode(text: &str) -> Result<Self, SessionStringError> {
        let raw = URL_SAFE_NO_PAD
            .decode(text.trim().trim_end_matches('='))
            .map_err(|_| SessionStringError::Encoding)?;
        if raw.len() != PACKED_LEN {
            return Err(SessionStringError::Length(raw.len()));
        }
        let mut auth_key = [0u8; 256];
        auth_key.copy_from_slice(&raw[6..262]);
        Ok(Self {
            dc_id: raw[0],
            api_id: u32::from_be_bytes([raw[1], raw[2], raw[3], raw[4]]),
            test_mode: raw[5] != 0,
            auth_key,
            user_id: u64::from_be_bytes(
                raw[262..270]
                    .try_into()
                    .map_err(|_| SessionStringError::Length(raw.len()))?,
            ),
            is_bot: raw[270] != 0,
        })
    }

    pub fn encode(&self) -> String {
        let mut raw = Vec::with_capacity(PACKED_LEN);
        raw.push(self.dc_id);
        raw.extend_from_slice(&self.api_id.to_be_bytes());
        raw.push(u8::from(self.test_mode));
        raw.extend_from_slice(&self.auth_key);
        raw.extend_from_slice(&self.user_id.to_be_bytes());
        raw.push(u8::from(self.is_bot));
        URL_SAFE_NO_PAD.encode(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PyrogramSession {
        let mut auth_key = [0u8; 256];
        for (i, byte) in auth_key.iter_mut().enumerate() {
            *byte = (i * 7 % 256) as u8;
        }
        PyrogramSession {
            dc_id: 4,
            api_id: 123_456,
            test_mode: false,
            auth_key,
            user_id: 7_000_000_001,
            is_bot: false,
        }
    }

    #[test]
    fn round_trip() {
        let encoded = sample().encode();
        assert!(!encoded.contains('='));
        assert!(PyrogramSession::decode(&encoded).expect("decodes") == sample());
    }

    /// Same bytes Python's struct.pack(">BI?256sQ?") produces for `sample()`.
    #[test]
    fn layout_matches_python_struct() {
        let raw = URL_SAFE_NO_PAD.decode(sample().encode()).expect("base64");
        assert_eq!(raw.len(), 271);
        assert_eq!(&raw[..6], &[4, 0x00, 0x01, 0xE2, 0x40, 0]);
        assert_eq!(&raw[262..], &[0, 0, 0, 1, 0xA1, 0x3B, 0x86, 0x01, 0]);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(
            PyrogramSession::decode("***").err(),
            Some(SessionStringError::Encoding)
        );
        assert_eq!(
            PyrogramSession::decode("AAAA").err(),
            Some(SessionStringError::Length(3))
        );
        assert!(PyrogramSession::decode(&format!("{}==", sample().encode())).is_ok());
    }
}
