//! Runs in any app that is a `dash_core::Host`.
//! Wave 8.5 (2026-05-28): Persistent OCR cache.
//!
//! Two byte-identical files at different paths used to be OCR'd twice
//! (or more — backups, sync folders, screenshot piles), burning CPU on
//! work the engine already finished. This module dedups by content
//! hash so each unique file's OCR text is computed exactly once and
//! replayed for every other path that points at the same bytes.
//!
//! Cache key: `BLAKE3(file_bytes) || langs_string` — content + the OCR
//! language profile that produced the text. Changing langs invalidates
//! cleanly; identical bytes with identical langs always hit.
//!
//! Cache lives in its own redb file (`ocr_cache.redb`) under the same
//! state dir as the search index, kept separate from `store.redb`
//! so the encrypted-config DB and the high-traffic cache don't share a
//! single writer lock.
//!
//! GC runs at the end of every successful full rebuild
//! (`search.rs::build_index_in_worker`, after the staging swap):
//!   1. drop entries last touched more than `ttl_days` ago, then
//!   2. if total cache size still exceeds `max_bytes`, drop oldest
//!      entries until under the cap.
//!
//! All cache errors are swallowed at the call sites — a cache miss /
//! write failure must never fail-stop the rebuild. The worst case is
//! one rebuild's worth of duplicate OCR work.

use redb::{Database, ReadableTable, TableDefinition};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use dash_core::dpapi;

pub const OCR_CACHE_FILE: &str = "ocr_cache.redb";

/// 32-byte BLAKE3 digest of a file's bytes.
pub type ContentHash = [u8; 32];

/// Cache table: key is `hash bytes (32) || lang bytes (utf-8)`, value
/// is a packed binary record (see `pack_value`). Raw `&[u8]` for both
/// so the lang string can be variable-length and the OCR text payload
/// stays untouched (no JSON escape blowup on multi-line OCR output).
const OCR_CACHE_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("ocr_cache_v1");

/// Default TTL: entries not touched in 90 days are dropped at the next
/// GC pass. Plenty of headroom for a "rebuild every Monday" rhythm.
pub const DEFAULT_TTL_DAYS: u64 = 90;
/// Default total cache cap: ~200 MB ≈ 50 k OCR'd images, comfortably
/// more than most users will hit. LRU pass evicts oldest entries to
/// bring total back under the cap.
pub const DEFAULT_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// Serialize cache file access at the process level. Mirrors the
/// `LOCAL_DB_LOCK` pattern in `local_db.rs` — redb has its own internal
/// locks, but serializing `Database::open` at the Rust level avoids
/// races when the dispatcher and the GC pass both want it.
static OCR_CACHE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Where the cache DB lives. Sibling of `store.redb` under the
/// search-index state dir, so the cache disappears cleanly if a user
/// nukes the app's data folder.
pub fn cache_path_for_dir(dir: &Path) -> PathBuf {
    dir.join(OCR_CACHE_FILE)
}

/// Streaming BLAKE3 of a file's bytes. Cheap (~1 GB/s) but reads the
/// whole file — only invoke on files we'd OCR anyway (images, PDFs).
/// Callers MUST treat any error as a cache miss and proceed with
/// normal extraction; never propagate.
pub fn hash_file(path: &Path) -> std::io::Result<ContentHash> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Extensions whose content we cache. The principle: cache when
/// extraction is expensive enough that the BLAKE3 + redb round-trip is
/// worth it. OCR-bound files (images) and PDFs (which may OCR-fallback)
/// qualify. Plain text and Office docs extract fast enough that the
/// hash cost dominates the saving, so we skip them.
pub fn is_cache_eligible_path(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "tif" | "tiff" | "bmp" | "webp" | "gif" | "pdf"
    )
}

/// Context handed to the extractor pool when OCR is enabled. The pool
/// uses it to hash files and look up / store OCR text. `None` (when
/// OCR is off) makes the pool behave exactly as before Wave 8.5 — no
/// hashing, no redb writes, no overhead.
#[derive(Clone, Debug)]
pub struct OcrCacheCtx {
    pub db_path: PathBuf,
    pub lang: String,
}

impl OcrCacheCtx {
    pub fn new(state_dir: &Path, lang: &str) -> Self {
        Self {
            db_path: cache_path_for_dir(state_dir),
            lang: lang.to_string(),
        }
    }
}

fn pack_key(hash: &ContentHash, lang: &str) -> Vec<u8> {
    let lang_bytes = lang.as_bytes();
    let mut key = Vec::with_capacity(32 + lang_bytes.len());
    key.extend_from_slice(hash);
    key.extend_from_slice(lang_bytes);
    key
}

/// Pack a cache entry. Layout (little-endian):
///   [u64 last_used_at_secs][u64 created_at_secs][u32 text_len][text utf-8]
fn pack_value(text: &str, last_used_at: u64, created_at: u64) -> Vec<u8> {
    let text_bytes = text.as_bytes();
    let mut value = Vec::with_capacity(8 + 8 + 4 + text_bytes.len());
    value.extend_from_slice(&last_used_at.to_le_bytes());
    value.extend_from_slice(&created_at.to_le_bytes());
    value.extend_from_slice(&(text_bytes.len() as u32).to_le_bytes());
    value.extend_from_slice(text_bytes);
    value
}

/// Reverse of `pack_value`. Returns `(text, last_used_at, created_at)`,
/// or `None` if the bytes are malformed (older format, truncation, etc.)
/// — caller should treat as miss and overwrite.
fn unpack_value(bytes: &[u8]) -> Option<(String, u64, u64)> {
    if bytes.len() < 20 {
        return None;
    }
    let last_used_at = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
    let created_at = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
    let text_len = u32::from_le_bytes(bytes[16..20].try_into().ok()?) as usize;
    if bytes.len() != 20 + text_len {
        return None;
    }
    let text = String::from_utf8(bytes[20..].to_vec()).ok()?;
    Some((text, last_used_at, created_at))
}

/// DPAPI-protect a packed cache value on Windows so extracted OCR text —
/// which can hold IDs, receipts, or document scans — is encrypted at rest
/// like the rest of the local DB. Returns `None` if protection fails so the
/// caller skips the write rather than persisting plaintext. Non-Windows
/// builds pass through (the app ships Windows-only).
fn encrypt_value(packed: &[u8]) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        dpapi::protect(packed).ok()
    }
    #[cfg(not(windows))]
    {
        Some(packed.to_vec())
    }
}

/// Reverse of `encrypt_value`. DPAPI's `unprotect` passes legacy plaintext
/// entries through unchanged (no version byte), so caches written before this
/// change keep working; a genuine decrypt failure returns `None`, which the
/// caller treats as a cache miss (the file is simply re-OCR'd and re-stored,
/// now encrypted).
fn decrypt_value(stored: &[u8]) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        dpapi::unprotect(stored).ok()
    }
    #[cfg(not(windows))]
    {
        Some(stored.to_vec())
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn open_db(path: &Path) -> Result<Database, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create OCR cache directory: {e}"))?;
    }

    // Graceful corruption recovery mirrors local_db.rs. A partially
    // written cache file caused by an OS crash mid-write must not stop
    // the indexer from running — we move the bad file aside and start
    // a fresh cache. Worst case: one rebuild of duplicate OCR work
    // next time.
    let db = if path.exists() {
        match Database::open(path) {
            Ok(db) => db,
            Err(error) => {
                eprintln!(
                    "ocr_cache: cannot open existing cache ({error}); quarantining and starting fresh."
                );
                quarantine(path);
                Database::create(path).map_err(|e| format!("Cannot create OCR cache: {e}"))?
            }
        }
    } else {
        Database::create(path).map_err(|e| format!("Cannot create OCR cache: {e}"))?
    };

    let write_txn = db
        .begin_write()
        .map_err(|e| format!("Cannot initialize OCR cache: {e}"))?;
    {
        let _ = write_txn
            .open_table(OCR_CACHE_TABLE)
            .map_err(|e| format!("Cannot initialize OCR cache table: {e}"))?;
    }
    write_txn
        .commit()
        .map_err(|e| format!("Cannot commit OCR cache init: {e}"))?;

    Ok(db)
}

fn quarantine(path: &Path) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let quarantined = path.with_extension(format!("corrupt-{ts}.redb"));
    let _ = fs::rename(path, quarantined);
}

/// Cache lookup. `Some(text)` on hit (with `last_used_at` bumped to
/// now so the LRU pass keeps live entries alive), `None` on miss or
/// any error. Safe to ignore errors here — a miss is the correct
/// fallback (run OCR as normal).
pub fn lookup(ctx: &OcrCacheCtx, hash: &ContentHash) -> Option<String> {
    let _guard = OCR_CACHE_LOCK.lock().ok()?;
    let db = open_db(&ctx.db_path).ok()?;
    let key = pack_key(hash, &ctx.lang);

    // Read the value in its own scope so the read txn drops before we
    // open a write txn for the last_used_at touch.
    let value_bytes = {
        let read_txn = db.begin_read().ok()?;
        let table = read_txn.open_table(OCR_CACHE_TABLE).ok()?;
        let entry = table.get(key.as_slice()).ok()??;
        entry.value().to_vec()
    };

    let (text, last_used, created_at) = unpack_value(&decrypt_value(&value_bytes)?)?;

    // Touch last_used_at to keep this entry above the TTL/LRU axes,
    // but only when stale by more than a day — avoids hammering redb
    // with one write per file in a rebuild that has lots of cache hits.
    let now = now_secs();
    if now.saturating_sub(last_used) > 86_400 {
        if let Some(touched) = encrypt_value(&pack_value(&text, now, created_at)) {
            if let Ok(write_txn) = db.begin_write() {
                if let Ok(mut table) = write_txn.open_table(OCR_CACHE_TABLE) {
                    let _ = table.insert(key.as_slice(), touched.as_slice());
                }
                let _ = write_txn.commit();
            }
        }
    }

    Some(text)
}

/// Cache store. Errors are logged but never bubbled — a cache write
/// failure must not break the rebuild (the path is still indexed
/// normally; only the dedup win is lost).
pub fn store(ctx: &OcrCacheCtx, hash: &ContentHash, text: &str) {
    let _guard = match OCR_CACHE_LOCK.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let db = match open_db(&ctx.db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("ocr_cache: cannot open for store ({e})");
            return;
        }
    };
    let now = now_secs();
    let Some(value) = encrypt_value(&pack_value(text, now, now)) else {
        // Never persist OCR text we couldn't DPAPI-protect — skip the cache
        // write; the file is still indexed normally, only the dedup is lost.
        return;
    };
    let key = pack_key(hash, &ctx.lang);

    let write_txn = match db.begin_write() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ocr_cache: cannot begin write for store ({e})");
            return;
        }
    };
    {
        if let Ok(mut table) = write_txn.open_table(OCR_CACHE_TABLE) {
            let _ = table.insert(key.as_slice(), value.as_slice());
        }
    }
    let _ = write_txn.commit();
}

/// GC stats reported back to the rebuild log. All counts are advisory —
/// the cache is best-effort and surviving entries shouldn't be relied
/// on for correctness (the path is still indexed normally either way).
#[derive(Debug, Clone, Default)]
pub struct GcStats {
    pub entries_before: usize,
    pub entries_after: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub ttl_dropped: usize,
    pub lru_dropped: usize,
}

/// Sweep stale entries (TTL pass) and trim oldest entries to fit the
/// size cap (LRU pass). Called at the end of every successful full
/// rebuild from `search.rs::build_index_in_worker`. Safe to call when
/// the cache file doesn't exist (returns default stats).
pub fn gc(db_path: &Path, ttl_days: u64, max_bytes: u64) -> Result<GcStats, String> {
    let _guard = OCR_CACHE_LOCK
        .lock()
        .map_err(|_| "OCR cache lock was poisoned".to_string())?;
    if !db_path.exists() {
        return Ok(GcStats::default());
    }
    let db = open_db(db_path)?;
    let now = now_secs();
    let ttl_cutoff = now.saturating_sub(ttl_days.saturating_mul(86_400));

    // Pass 1: read every entry, sort survivors into "keep" (with last
    // used + len for the LRU pass) and "ttl-drop" (key only).
    let mut survivors: Vec<(Vec<u8>, u64, u64)> = Vec::new(); // (key, last_used, val_len)
    let mut ttl_drops: Vec<Vec<u8>> = Vec::new();
    let mut entries_before = 0usize;
    let mut bytes_before = 0u64;

    {
        let read_txn = db
            .begin_read()
            .map_err(|e| format!("Cannot begin GC read: {e}"))?;
        let table = read_txn
            .open_table(OCR_CACHE_TABLE)
            .map_err(|e| format!("Cannot open table for GC: {e}"))?;
        for entry in table
            .iter()
            .map_err(|e| format!("Cannot iterate table for GC: {e}"))?
        {
            let entry = entry.map_err(|e| format!("Cannot read entry for GC: {e}"))?;
            entries_before += 1;
            let key = entry.0.value().to_vec();
            let val_bytes = entry.1.value();
            let val_len = val_bytes.len() as u64;
            bytes_before += val_len;
            match unpack_value(val_bytes) {
                Some((_text, last_used, _created)) if last_used >= ttl_cutoff => {
                    survivors.push((key, last_used, val_len));
                }
                _ => {
                    // Either stale or malformed — drop it.
                    ttl_drops.push(key);
                }
            }
        }
    }

    // Pass 2: TTL drops in a single write txn.
    let ttl_dropped = ttl_drops.len();
    if !ttl_drops.is_empty() {
        let write_txn = db
            .begin_write()
            .map_err(|e| format!("Cannot begin TTL drop write: {e}"))?;
        {
            let mut table = write_txn
                .open_table(OCR_CACHE_TABLE)
                .map_err(|e| format!("Cannot open table for TTL drop: {e}"))?;
            for key in &ttl_drops {
                let _ = table.remove(key.as_slice());
            }
        }
        write_txn
            .commit()
            .map_err(|e| format!("Cannot commit TTL drop: {e}"))?;
    }

    // Pass 3: LRU trim if still over cap.
    let mut bytes_after: u64 = survivors.iter().map(|(_, _, len)| *len).sum();
    let mut lru_drops: Vec<Vec<u8>> = Vec::new();
    if max_bytes > 0 && bytes_after > max_bytes {
        // Oldest first.
        survivors.sort_by_key(|(_, last_used, _)| *last_used);
        let mut bytes_to_drop = bytes_after - max_bytes;
        for (key, _last_used, len) in &survivors {
            if bytes_to_drop == 0 {
                break;
            }
            lru_drops.push(key.clone());
            bytes_to_drop = bytes_to_drop.saturating_sub(*len);
            bytes_after = bytes_after.saturating_sub(*len);
        }
        let write_txn = db
            .begin_write()
            .map_err(|e| format!("Cannot begin LRU drop write: {e}"))?;
        {
            let mut table = write_txn
                .open_table(OCR_CACHE_TABLE)
                .map_err(|e| format!("Cannot open table for LRU drop: {e}"))?;
            for key in &lru_drops {
                let _ = table.remove(key.as_slice());
            }
        }
        write_txn
            .commit()
            .map_err(|e| format!("Cannot commit LRU drop: {e}"))?;
    }

    let entries_after = entries_before
        .saturating_sub(ttl_dropped)
        .saturating_sub(lru_drops.len());
    Ok(GcStats {
        entries_before,
        entries_after,
        bytes_before,
        bytes_after,
        ttl_dropped,
        lru_dropped: lru_drops.len(),
    })
}
