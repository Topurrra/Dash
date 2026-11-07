//! Runs in any app that is a `dash_core::Host`.
//! Semantic-search vector store.
//!
//! Multiple embedding vectors per content-indexed document — one per text
//! chunk (see `embedding::chunk_text`) — keyed by absolute path. Written in
//! the background by the content watcher (`search::semantic`, behind the
//! semantic toggle) and read at query time to find the documents *closest
//! in meaning* to the query: the ones the keyword passes miss because they
//! share no literal words. Scoring a document by its BEST chunk (not one
//! averaged doc vector) is what makes long documents actually findable — a
//! single relevant paragraph is no longer diluted by the rest of the file.
//!
//! **int8 quantization.** Each chunk vector is stored as `i8[EMBED_DIM]`
//! (one byte per dimension) instead of `f32[EMBED_DIM]` — 4× less disk and,
//! more importantly, 4× less RAM, since the whole set is held in memory for
//! the brute-force scan. This directly serves the "runs on 4–8 GB machines"
//! mandate: 100 k chunks at 768-d ≈ 77 MB in RAM. Cosine is invariant
//! to positive scaling, so quantizing per-vector to the full int8 range and
//! computing cosine on the raw bytes recovers the true similarity to within
//! quantization noise — no stored scale factor needed.
//!
//! Storage: its own redb file (`vector_cache.redb`) beside the search index,
//! same sidecar pattern as `ocr_cache.rs`. Vectors are **plaintext** — a
//! lossy transform of the content index, which is itself plaintext on disk
//! by design (see the encrypt-at-rest disclosure); an int8 embedding vector
//! is strictly less recoverable than the indexed text beside it.
//!
//! Query path: `top_k` brute-forces cosine over every stored chunk, keeping
//! each document's best chunk. Decoded vectors are held in a process-global
//! cache keyed by the redb file's mtime, so a query only reloads when the
//! indexer has written since the last scan. All errors swallow to empty: a
//! vector-store failure never breaks search, it just means no semantic
//! candidates that query.
//!
//! ponytail: exact brute-force cosine over an in-RAM int8 set. Fine to
//! several hundred k chunks; past that, an ANN index (hnsw) would trade a
//! little accuracy for speed — deferred deliberately (quantization is the
//! scale lever that fits the hardware target). `top_k`'s signature stays the
//! same if that day comes.

use redb::{Database, ReadableTable, TableDefinition};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

pub const VECTOR_CACHE_FILE: &str = "vector_cache.redb";

/// Key = absolute path (utf-8), value = `[u64 modified_ms]` of the text the
/// vectors were made from, then the packed chunk set (see `pack`), which is
/// empty for a document without text. The modification time is what lets
/// the background embedder (`search::semantic`) tell a current vector from
/// a stale one. `v4`: potion-retrieval-32M, 512-d. Older tables hold another
/// model's vectors (`OLD_TABLES`) and are dropped when the store opens.
const VECTOR_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("vector_cache_v4");

/// `v2`: `EmbeddingGemma` vectors without a modification time; `v3`: with one.
const OLD_TABLES: [TableDefinition<&str, &[u8]>; 2] =
    [TableDefinition::new("vector_cache_v2"), TableDefinition::new("vector_cache_v3")];

/// One document's vectors to store: its path, the modification time of the
/// text they come from, and one f32 embedding per passage (none when the
/// document has no text).
pub struct DocVectors {
    pub path: String,
    pub modified_ms: u64,
    pub chunks: Vec<Vec<f32>>,
}

/// Serialize store access at the process level (mirrors `OCR_CACHE_LOCK`).
static VECTOR_CACHE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// mtime-guarded decode cache: `(db mtime at load, decoded chunk sets)`.
/// `top_k` reuses this whenever the redb file hasn't changed since the last
/// load, so repeated queries don't re-read/re-decode the store.
type ChunkSet = (String, Vec<Vec<i8>>);
type DecodeCache = Option<(SystemTime, Vec<ChunkSet>)>;
static DECODE_CACHE: LazyLock<Mutex<DecodeCache>> = LazyLock::new(|| Mutex::new(None));

pub fn cache_path_for_dir(dir: &Path) -> PathBuf {
    dir.join(VECTOR_CACHE_FILE)
}

/// Quantize a float vector to int8 using a per-vector max-abs scale, so the
/// full [-127, 127] range is used regardless of the vector's magnitude. The
/// scale is deliberately NOT stored: cosine is invariant to positive scaling
/// of each argument, so `cosine(q, i8)` ≈ `cosine(q, f32)`.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "each value is rounded and clamped to -127..=127 before the cast"
)]
fn quantize(vec: &[f32]) -> Vec<i8> {
    let maxabs = vec.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
    if maxabs <= 0.0 {
        return vec![0i8; vec.len()];
    }
    let scale = 127.0 / maxabs;
    vec.iter().map(|&v| (v * scale).round().clamp(-127.0, 127.0) as i8).collect()
}

/// `[u16 dim][u16 chunk_count][i8; dim]…`, little-endian. All chunks must
/// share one dimension (they do — same model). Empty input → empty output.
fn pack(chunks: &[Vec<i8>]) -> Vec<u8> {
    let dim = chunks.first().map_or(0, std::vec::Vec::len);
    let (Ok(dim_header), Ok(count_header)) = (u16::try_from(dim), u16::try_from(chunks.len())) else {
        return Vec::new();
    };
    if dim == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(4 + dim * chunks.len());
    out.extend_from_slice(&dim_header.to_le_bytes());
    out.extend_from_slice(&count_header.to_le_bytes());
    for chunk in chunks {
        if chunk.len() != dim {
            return Vec::new(); // ragged input — refuse rather than misalign
        }
        out.extend(chunk.iter().map(|&q| q.cast_unsigned()));
    }
    out
}

/// Reverse of `pack`. `None` on any malformed record (treated as absent).
fn unpack(bytes: &[u8]) -> Option<Vec<Vec<i8>>> {
    if bytes.len() < 4 {
        return None;
    }
    let dim = usize::from(u16::from_le_bytes(bytes[0..2].try_into().ok()?));
    let count = usize::from(u16::from_le_bytes(bytes[2..4].try_into().ok()?));
    if dim == 0 || bytes.len() != 4 + dim * count {
        return None;
    }
    let mut chunks = Vec::with_capacity(count);
    for i in 0..count {
        let start = 4 + i * dim;
        let chunk: Vec<i8> = bytes[start..start + dim].iter().map(|&b| b.cast_signed()).collect();
        chunks.push(chunk);
    }
    Some(chunks)
}

/// Whether an open error means the file itself is unusable (corrupt, or a
/// format too old to read), so setting it aside is right. Anything else —
/// the store open in another process, a passing I/O error — is not: the
/// file is fine, and throwing it away lost every entry.
pub(crate) fn is_corrupt(error: &redb::DatabaseError) -> bool {
    matches!(
        error,
        redb::DatabaseError::UpgradeRequired(_) | redb::DatabaseError::Storage(redb::StorageError::Corrupted(_))
    )
}

fn open_db(path: &Path) -> Result<Database, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Cannot create vector cache directory: {e}"))?;
    }
    let db = if path.exists() {
        match Database::open(path) {
            Ok(db) => db,
            Err(error) if is_corrupt(&error) => {
                eprintln!("vector_cache: cannot open existing store ({error}); quarantining and starting fresh.");
                let quarantined = path.with_extension("corrupt.redb");
                let _ = fs::rename(path, quarantined);
                Database::create(path).map_err(|e| format!("Cannot create vector cache: {e}"))?
            }
            Err(error) => return Err(format!("Vector cache is unavailable: {error}")),
        }
    } else {
        Database::create(path).map_err(|e| format!("Cannot create vector cache: {e}"))?
    };
    let write_txn = db.begin_write().map_err(|e| format!("Cannot initialize vector cache: {e}"))?;
    {
        let _ = write_txn.open_table(VECTOR_TABLE).map_err(|e| format!("Cannot initialize vector table: {e}"))?;
        for old in OLD_TABLES {
            let _ = write_txn.delete_table(old);
        }
    }
    write_txn.commit().map_err(|e| format!("Cannot commit vector cache init: {e}"))?;
    Ok(db)
}

/// Store (or overwrite) documents' chunk vectors in one transaction; they're
/// quantized to int8 here. A document stored without chunks is recorded as
/// done (no text to embed) so it isn't tried again until it changes. Errors
/// are logged, never bubbled — a failed write just leaves the documents for
/// the next pass.
pub fn store_many(db_path: &Path, docs: &[DocVectors]) {
    if docs.is_empty() {
        return;
    }
    let Ok(_guard) = VECTOR_CACHE_LOCK.lock() else {
        return;
    };
    let db = match open_db(db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("vector_cache: cannot open for store ({e})");
            return;
        }
    };
    if let Ok(write_txn) = db.begin_write() {
        if let Ok(mut table) = write_txn.open_table(VECTOR_TABLE) {
            for doc in docs {
                let quantized: Vec<Vec<i8>> =
                    doc.chunks.iter().filter(|c| !c.is_empty()).map(|c| quantize(c)).collect();
                let mut value = doc.modified_ms.to_le_bytes().to_vec();
                value.extend(pack(&quantized));
                let _ = table.insert(doc.path.as_str(), value.as_slice());
            }
        }
        let _ = write_txn.commit();
    }
}

/// Path → modification time of every stored document: what the background
/// embedder compares against the index to find missing and stale vectors.
pub fn stored_versions(db_path: &Path) -> std::collections::HashMap<String, u64> {
    let mut versions = std::collections::HashMap::new();
    let Ok(_guard) = VECTOR_CACHE_LOCK.lock() else {
        return versions;
    };
    if !db_path.exists() {
        return versions;
    }
    let Ok(db) = open_db(db_path) else {
        return versions;
    };
    if let Ok(read_txn) = db.begin_read() {
        if let Ok(table) = read_txn.open_table(VECTOR_TABLE) {
            if let Ok(iter) = table.iter() {
                for (key, value) in iter.flatten() {
                    if let Some(stamp) = value.value().get(..8).and_then(|bytes| bytes.try_into().ok()) {
                        versions.insert(key.value().to_string(), u64::from_le_bytes(stamp));
                    }
                }
            }
        }
    }
    versions
}

/// Drop the vectors of `paths` (files gone from the index, or no longer
/// documents that get embedded).
pub fn remove_many(db_path: &Path, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    let Ok(_guard) = VECTOR_CACHE_LOCK.lock() else {
        return;
    };
    let Ok(db) = open_db(db_path) else {
        return;
    };
    if let Ok(write_txn) = db.begin_write() {
        if let Ok(mut table) = write_txn.open_table(VECTOR_TABLE) {
            for path in paths {
                let _ = table.remove(path.as_str());
            }
        }
        let _ = write_txn.commit();
    }
}

/// Load every stored chunk set, using the mtime-guarded cache when the store
/// hasn't changed since the last load.
fn load_all_cached(db_path: &Path) -> Vec<ChunkSet> {
    let mtime = fs::metadata(db_path).and_then(|m| m.modified()).ok();
    if let (Some(mtime), Ok(cache)) = (mtime, DECODE_CACHE.lock()) {
        if let Some((cached_mtime, sets)) = cache.as_ref() {
            if *cached_mtime == mtime {
                return sets.clone();
            }
        }
    }
    let sets = read_all(db_path);
    if let (Some(mtime), Ok(mut cache)) = (mtime, DECODE_CACHE.lock()) {
        *cache = Some((mtime, sets.clone()));
    }
    sets
}

fn read_all(db_path: &Path) -> Vec<ChunkSet> {
    if !db_path.exists() {
        return Vec::new();
    }
    let Ok(db) = open_db(db_path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Ok(read_txn) = db.begin_read() {
        if let Ok(table) = read_txn.open_table(VECTOR_TABLE) {
            if let Ok(iter) = table.iter() {
                for entry in iter.flatten() {
                    let path = entry.0.value().to_string();
                    // After the modification time; documents without text
                    // have no chunks and are skipped here.
                    if let Some(chunks) = entry.1.value().get(8..).and_then(unpack) {
                        out.push((path, chunks));
                    }
                }
            }
        }
    }
    out
}

/// Cosine between an f32 query and an int8 chunk vector. Guards zero-norm and
/// length mismatch (→ 0). The int8 side is treated as f32; because cosine is
/// scale-invariant, the missing dequant scale doesn't matter.
fn cosine_f32_i8(query: &[f32], chunk: &[i8]) -> f32 {
    if query.len() != chunk.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut nq = 0.0f32;
    let mut nc = 0.0f32;
    for i in 0..query.len() {
        let c = f32::from(chunk[i]);
        dot += query[i] * c;
        nq += query[i] * query[i];
        nc += c * c;
    }
    if nq <= 0.0 || nc <= 0.0 {
        return 0.0;
    }
    (dot / (nq.sqrt() * nc.sqrt())).clamp(-1.0, 1.0)
}

/// The `k` documents whose BEST chunk is most similar to `query`, as
/// `(path, cosine)` pairs sorted most-similar first. The path is returned
/// exactly as stored (original case) so callers can look the document up by
/// its `path_exact` term. Only positive similarities are returned. Empty on
/// any error or empty store.
pub fn top_k(db_path: &Path, query: &[f32], k: usize) -> Vec<(String, f32)> {
    if query.is_empty() || k == 0 {
        return Vec::new();
    }
    let Ok(_guard) = VECTOR_CACHE_LOCK.lock() else {
        return Vec::new();
    };
    let sets = load_all_cached(db_path);
    let mut scored: Vec<(String, f32)> = sets
        .iter()
        .filter_map(|(path, chunks)| {
            let best = chunks.iter().map(|chunk| cosine_f32_i8(query, chunk)).fold(0.0f32, f32::max);
            (best > 0.0).then(|| (path.clone(), best))
        })
        .collect();
    scored.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrips_multichunk() {
        let a = quantize(&[1.0, 0.0, -0.5]);
        let b = quantize(&[0.0, 1.0, 0.25]);
        let packed = pack(&[a.clone(), b.clone()]);
        assert_eq!(unpack(&packed), Some(vec![a, b]));
        // Malformed inputs are rejected, not panicked on.
        assert_eq!(unpack(&[]), None);
        assert_eq!(unpack(&[3, 0, 1, 0]), None); // claims dim 3 / 1 chunk, no body
    }

    #[test]
    fn quantize_preserves_direction() {
        // Cosine on the quantized vector should closely match the f32 cosine.
        let a = [0.9f32, 0.1, -0.3, 0.05];
        let b = [0.85f32, 0.2, -0.25, 0.0];
        let qb = quantize(&b);
        let f32_cos = {
            let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
            let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
            dot / (na * nb)
        };
        let q_cos = cosine_f32_i8(&a, &qb);
        assert!((f32_cos - q_cos).abs() < 0.02, "quant cosine drift too high: {f32_cos} vs {q_cos}");
    }

    #[test]
    fn cosine_ranks_aligned_over_orthogonal() {
        let q = [1.0f32, 0.0, 0.0];
        assert!((cosine_f32_i8(&q, &quantize(&[2.0, 0.0, 0.0])) - 1.0).abs() < 1e-3);
        assert!(cosine_f32_i8(&q, &quantize(&[0.0, 1.0, 0.0])).abs() < 1e-3);
        assert!((cosine_f32_i8(&q, &quantize(&[-1.0, 0.0, 0.0])) + 1.0).abs() < 1e-3);
        // Zero-norm and mismatched length are safe (→ 0), never NaN/panic.
        assert_eq!(cosine_f32_i8(&q, &[0i8, 0, 0]), 0.0);
        assert_eq!(cosine_f32_i8(&q, &[1i8, 0]), 0.0);
    }
}
