//! Encryption at rest for what Dash keeps on disk (clipboard history, the
//! local database, the OCR cache).
//!
//! Windows: DPAPI. macOS / Linux: `XChaCha20-Poly1305` with a random key kept
//! in the system keychain (the macOS Keychain; the Secret Service on Linux,
//! i.e. GNOME Keyring or `KWallet`). Either way a value written before
//! encryption (no version byte) reads back unchanged, and the next write
//! protects it. `DASH_NO_KEYCHAIN` set keeps the keychain out (tests on CI,
//! where nobody can answer its access prompt) and values stay plain.

#[cfg(windows)]
pub use crate::dpapi::{protect, unprotect};

#[cfg(not(windows))]
pub use keychain::{protect, unprotect};

#[cfg(not(windows))]
mod keychain {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;

    /// Starts an encrypted value. DPAPI's mark is another byte, so a file
    /// copied between systems is never misread.
    const VERSION_BYTE: u8 = 0xD5;
    const NONCE_LEN: usize = 24;
    const SERVICE: &str = "io.github.topurrra.dash";
    const ACCOUNT: &str = "at-rest-key";

    /// Encrypts `plaintext`. Where the system has no keychain (a Linux
    /// without a Secret Service, say a bare server) it stays plain, as it
    /// was before encryption existed here.
    ///
    /// # Errors
    /// Returns `Err` if no random nonce can be had or encryption fails.
    pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, String> {
        match cipher() {
            Some(cipher) => seal(cipher, plaintext),
            None => Ok(plaintext.to_vec()),
        }
    }

    /// Decrypts what [`protect`] wrote; a value without the version byte
    /// (written before encryption) comes back unchanged.
    ///
    /// # Errors
    /// Returns `Err` if the keychain can't give the key, or the value was
    /// changed or encrypted with another key.
    pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
        if blob.first() != Some(&VERSION_BYTE) {
            return Ok(blob.to_vec());
        }
        let cipher = cipher().ok_or("at-rest decrypt: the keychain has no key for Dash")?;
        open(cipher, blob)
    }

    /// The key, read from the keychain (or made and stored there) once per
    /// run. A failure isn't kept: the keychain may just not be unlocked yet.
    fn cipher() -> Option<&'static XChaCha20Poly1305> {
        static CIPHER: OnceLock<XChaCha20Poly1305> = OnceLock::new();
        static WARNED: AtomicBool = AtomicBool::new(false);
        if let Some(cipher) = CIPHER.get() {
            return Some(cipher);
        }
        if std::env::var_os("DASH_NO_KEYCHAIN").is_some() {
            return None;
        }
        match load_or_create_key() {
            Ok(key) => Some(CIPHER.get_or_init(|| XChaCha20Poly1305::new(Key::from_slice(&key)))),
            Err(error) => {
                if !WARNED.swap(true, Ordering::Relaxed) {
                    eprintln!("at_rest: no key from the keychain ({error}); writing unencrypted");
                }
                None
            }
        }
    }

    fn load_or_create_key() -> Result<[u8; 32], String> {
        let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| e.to_string())?;
        let stored = match entry.get_secret() {
            Ok(secret) => secret,
            Err(keyring::Error::NoEntry) => {
                let mut key = [0u8; 32];
                getrandom::fill(&mut key).map_err(|e| e.to_string())?;
                entry.set_secret(&key).map_err(|e| e.to_string())?;
                // Read back: if another Dash process stored its key at the
                // same moment, both use whichever the keychain kept.
                entry.get_secret().map_err(|e| e.to_string())?
            }
            Err(error) => return Err(error.to_string()),
        };
        <[u8; 32]>::try_from(stored.as_slice()).map_err(|_| "the stored key has the wrong length".to_string())
    }

    /// `VERSION_BYTE`, a random nonce, then the ciphertext and its tag.
    fn seal(cipher: &XChaCha20Poly1305, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|e| format!("at-rest encrypt: {e}"))?;
        let sealed = cipher.encrypt(XNonce::from_slice(&nonce), plaintext).map_err(|_| "at-rest encrypt failed")?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + sealed.len());
        out.push(VERSION_BYTE);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    fn open(cipher: &XChaCha20Poly1305, blob: &[u8]) -> Result<Vec<u8>, String> {
        let body = blob.get(1..).unwrap_or_default();
        if body.len() < NONCE_LEN {
            return Err("at-rest decrypt: value too short".into());
        }
        let (nonce, sealed) = body.split_at(NONCE_LEN);
        cipher.decrypt(XNonce::from_slice(nonce), sealed).map_err(|_| "at-rest decrypt failed".into())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // The keychain itself isn't touched here (CI machines may not have
        // one); the sealing is.
        fn test_cipher(fill: u8) -> XChaCha20Poly1305 {
            XChaCha20Poly1305::new(Key::from_slice(&[fill; 32]))
        }

        #[test]
        fn sealed_values_open_again() {
            let cipher = test_cipher(7);
            let plain = br#"{"entries":[],"retentionDays":14}"#;
            let blob = seal(&cipher, plain).unwrap();
            assert_eq!(blob[0], VERSION_BYTE, "sealed values start with the version byte");
            assert!(!blob.windows(plain.len()).any(|w| w == plain), "the plaintext doesn't show");
            assert_eq!(open(&cipher, &blob).unwrap(), plain, "round trip");
            assert_eq!(open(&cipher, &seal(&cipher, b"").unwrap()).unwrap(), b"", "empty round trip");
        }

        #[test]
        fn changed_or_foreign_values_fail() {
            let cipher = test_cipher(7);
            let mut blob = seal(&cipher, b"secret").unwrap();
            assert!(open(&test_cipher(8), &blob).is_err(), "another key can't open it");
            let last = blob.len() - 1;
            blob[last] ^= 1;
            assert!(open(&cipher, &blob).is_err(), "a changed value is refused");
            assert!(open(&cipher, &[VERSION_BYTE, 1, 2]).is_err(), "a cut-off value is refused");
        }

        #[test]
        fn plain_values_pass_through() {
            assert_eq!(unprotect(b"{\"a\":1}").unwrap(), b"{\"a\":1}", "legacy JSON reads as is");
            assert_eq!(unprotect(b"").unwrap(), b"", "empty reads as is");
        }
    }
}
