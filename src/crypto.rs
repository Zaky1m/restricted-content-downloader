//! Session encryption at rest (S1). XChaCha20-Poly1305 with a random 24-byte nonce.
//! The row identity is bound as associated data, so a ciphertext copied to another row fails to open.

use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};

#[derive(Debug, thiserror::Error)]
#[error("session could not be decrypted (wrong key, wrong row or damaged data)")]
pub struct DecryptError;

pub struct Sealed {
    pub nonce: [u8; 24],
    pub ciphertext: Vec<u8>,
}

pub fn seal(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Sealed {
    let nonce: [u8; 24] = rand::random();
    let cipher = XChaCha20Poly1305::new(key.into());
    let ciphertext = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("encryption of an in-memory buffer cannot fail");
    Sealed { nonce, ciphertext }
}

pub fn open(
    key: &[u8; 32],
    aad: &[u8],
    nonce: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, DecryptError> {
    if nonce.len() != 24 {
        return Err(DecryptError);
    }
    XChaCha20Poly1305::new(key.into())
        .decrypt(
            &XNonce::try_from(nonce).map_err(|_| DecryptError)?,
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| DecryptError)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];

    #[test]
    fn round_trip() {
        let sealed = seal(&KEY, b"linked:1", b"secret");
        assert_ne!(sealed.ciphertext, b"secret");
        assert_eq!(
            open(&KEY, b"linked:1", &sealed.nonce, &sealed.ciphertext).unwrap(),
            b"secret"
        );
    }

    #[test]
    fn rejects_wrong_key_row_and_tampering() {
        let sealed = seal(&KEY, b"linked:1", b"secret");
        assert!(open(&[8; 32], b"linked:1", &sealed.nonce, &sealed.ciphertext).is_err());
        assert!(open(&KEY, b"linked:2", &sealed.nonce, &sealed.ciphertext).is_err());
        let mut bad = sealed.ciphertext.clone();
        bad[0] ^= 1;
        assert!(open(&KEY, b"linked:1", &sealed.nonce, &bad).is_err());
        assert!(open(&KEY, b"linked:1", &sealed.nonce[..23], &sealed.ciphertext).is_err());
    }

    #[test]
    fn nonces_differ() {
        assert_ne!(seal(&KEY, b"a", b"x").nonce, seal(&KEY, b"a", b"x").nonce);
    }
}
