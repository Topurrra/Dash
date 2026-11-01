//! At-rest encryption for the local redb database via Windows DPAPI.
//!
//! `CryptProtectData` / `CryptUnprotectData` derive an encryption key from
//! the user's Windows credentials. The encrypted bytes are only readable
//! from the same user account on the same machine — copying the database
//! file to another PC or another user account yields opaque bytes.
//!
//! Why DPAPI specifically:
//! - **No password to manage** — users don't have to set up or remember
//!   a master password. The OS holds the key material.
//! - **No external dependencies** — no key files, no TPM, no network. The
//!   protection is built into Windows itself.
//! - **Per-user scope** — matches our threat model: "anyone with physical
//!   access to the user's logged-in account can already see this data; we
//!   only need to protect against another user account on the same box,
//!   or someone reading the disk after a fresh OS install."
//!
//! Trade-offs:
//! - **Not portable** — the encrypted blob can't be opened from another
//!   machine. That's by design: a "local-first" app shouldn't have its
//!   data trivially portable in encrypted form either.
//! - **Reinstall Windows = data loss** — DPAPI master keys are stored in
//!   the user profile, which is wiped on a clean OS reinstall. Users who
//!   reimage should export their data first (a future Settings → Export
//!   feature can use plain JSON for portability).
//! - **Microsoft has the technical ability to decrypt** — if you log in
//!   with a Microsoft account, the recovery key is escrowed to MS. For
//!   local accounts there's no escrow. We do not advertise DPAPI as
//!   defense against a state-level adversary; it's defense against the
//!   "other user account / random disk read" threat.
//!
//! Output format:
//!   [1 byte: version=0x01][N bytes: DPAPI ciphertext]
//! Reading code checks the version byte and, if it's not 0x01, treats the
//! input as legacy plaintext JSON. This lets us migrate existing
//! installations transparently — the first write after upgrade re-protects
//! the value, and subsequent reads find the magic byte and decrypt.

#![cfg(windows)]

use windows::Win32::Foundation::LocalFree;
use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB};

/// Magic byte prepended to every DPAPI-encrypted value so reads can
/// distinguish ciphertext from legacy plaintext during migration.
const VERSION_BYTE: u8 = 0x01;

/// Wrap an arbitrary byte slice with DPAPI. The output is the magic byte
/// followed by the DPAPI envelope (which includes its own IV + integrity
/// data). Safe to store as-is in redb.
pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let len = u32::try_from(plaintext.len()).map_err(|_| "DPAPI encrypt: value too large".to_string())?;
    // SAFETY: CryptProtectData reads the input descriptor in-place and
    // writes a new heap allocation into out_blob. We own the input slice
    // for the duration of the call. On success we copy the output into a
    // Vec and LocalFree it; on failure there is no output buffer to free.
    unsafe {
        let in_blob = CRYPT_INTEGER_BLOB { cbData: len, pbData: plaintext.as_ptr() as *mut u8 };
        let mut out_blob = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };

        CryptProtectData(
            &in_blob,
            None,
            None,
            None,
            None,
            // Flag 0 = "interactive prompt off, local machine, user scope".
            // We never want a UI prompt for this — it would surface during
            // settings reads at app start, which is unacceptable.
            0,
            &mut out_blob,
        )
        .map_err(|error| format!("DPAPI encrypt failed: {error}"))?;

        if out_blob.pbData.is_null() {
            return Err("DPAPI encrypt produced a null buffer".into());
        }

        let raw = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize);
        // Reserve space for the magic byte + the ciphertext so the final
        // Vec doesn't have to grow.
        let mut output = Vec::with_capacity(1 + raw.len());
        output.push(VERSION_BYTE);
        output.extend_from_slice(raw);

        // Free the Win32-allocated buffer. Failing to do this would leak
        // memory on every encrypt call, which adds up fast over a long
        // session of clipboard captures.
        let _ = LocalFree(windows::Win32::Foundation::HLOCAL(out_blob.pbData as *mut _));

        Ok(output)
    }
}

/// Unwrap a DPAPI-protected blob. If the input doesn't start with our
/// version byte we treat it as legacy plaintext and return it unchanged —
/// this is the migration hook that lets existing installs upgrade without
/// data loss.
pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
    // Migration path: pre-DPAPI plaintext values won't have our version
    // byte at the start. Returning the input as-is lets `read_json_*`
    // continue to parse the legacy JSON, and the next write will re-
    // protect the value automatically.
    if blob.is_empty() || blob[0] != VERSION_BYTE {
        return Ok(blob.to_vec());
    }

    let ciphertext = &blob[1..];
    let len = u32::try_from(ciphertext.len()).map_err(|_| "DPAPI decrypt: value too large".to_string())?;
    // SAFETY: same contract as `protect`: the input slice outlives the call
    // and `len` is its exact length; on success DPAPI hands us a LocalAlloc'd
    // buffer of cbData bytes, which we null-check, copy out, then LocalFree.
    unsafe {
        let in_blob = CRYPT_INTEGER_BLOB { cbData: len, pbData: ciphertext.as_ptr() as *mut u8 };
        let mut out_blob = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };

        CryptUnprotectData(
            &in_blob,
            None,
            None,
            None,
            None,
            // Flag 0 = "no UI prompt". Same rationale as encrypt above.
            0,
            &mut out_blob,
        )
        .map_err(|error| format!("DPAPI decrypt failed: {error}"))?;

        if out_blob.pbData.is_null() {
            return Err("DPAPI decrypt produced a null buffer".into());
        }

        let raw = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize);
        let plaintext = raw.to_vec();
        let _ = LocalFree(windows::Win32::Foundation::HLOCAL(out_blob.pbData as *mut _));
        Ok(plaintext)
    }
}

/// Check whether a blob was produced by `protect()`. Used by tests + a
/// public helper for future migration logic in `local_db` to detect
/// which entries are still plaintext and need to be re-protected on
/// next write. Today the unprotect() path handles that automatically,
/// so this is kept available as part of the module's API without an
/// active caller.
#[allow(dead_code)]
pub fn is_protected(blob: &[u8]) -> bool {
    !blob.is_empty() && blob[0] == VERSION_BYTE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small_payload() {
        let payload = br#"{"hello":"world","n":42}"#;
        let wrapped = protect(payload).expect("encrypt");
        assert!(is_protected(&wrapped));
        let unwrapped = unprotect(&wrapped).expect("decrypt");
        assert_eq!(unwrapped, payload);
    }

    #[test]
    fn legacy_plaintext_passes_through() {
        // Anything not starting with VERSION_BYTE is treated as legacy
        // plaintext — the read path returns it unchanged so the caller
        // can keep parsing it.
        let legacy = br#"{"settings":"plain"}"#;
        let result = unprotect(legacy).expect("plaintext passthrough");
        assert_eq!(result, legacy);
        assert!(!is_protected(legacy));
    }

    #[test]
    fn empty_input_is_not_protected() {
        assert!(!is_protected(b""));
        assert_eq!(unprotect(b"").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn roundtrip_large_payload() {
        // Clipboard images can be megabytes — make sure the wrapping
        // doesn't choke on a realistic-sized payload.
        let payload = vec![0x77u8; 512 * 1024];
        let wrapped = protect(&payload).expect("encrypt large");
        let unwrapped = unprotect(&wrapped).expect("decrypt large");
        assert_eq!(unwrapped, payload);
    }
}
