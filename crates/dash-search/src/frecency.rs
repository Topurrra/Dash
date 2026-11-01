//! Runs in any app that is a `dash_core::Host`.
//! Frecency ranking — the "frequency × recency" combination that makes the
//! overlay learn what you actually use.
//!
//! What's tracked: each time the user activates a result (launches an app,
//! opens a file/folder, opens a tool), we increment a launch counter
//! and timestamp it. On every subsequent search, we boost result scores using
//! both signals so often-used + recently-used things float to the top.
//!
//! Privacy: all data lives in a local redb-backed JSON blob in the app data
//! directory (`<app_data>/preferences/frecency.json` via `local_db`). No
//! network, no telemetry — same storage substrate the rest of the app uses
//! for preferences and profile data.
//!
//! Scoring formula:
//!   recency  = exp(-age_days × ln(2) / HALF_LIFE_DAYS)
//!   boost    = ln(launch_count + 1) × recency × MULTIPLIER
//!
//! With HALF_LIFE_DAYS=7 and MULTIPLIER=2.0, the boost ranges from about
//! 1.4 (single launch today) to ~9 (100 launches today), decaying smoothly
//! over a week and effectively to zero past a month.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use super::local_db;

/// Follower: run this call on the owner instead (see `dash_core::link`).
macro_rules! on_owner {
    ($op:literal, $args:tt) => {
        if dash_core::link::is_follower() {
            if let Some(result) = dash_core::link::forward($op, serde_json::json!($args)) {
                return result;
            }
        }
    };
    ($op:literal, $args:tt, or_default) => {
        if dash_core::link::is_follower() {
            if let Some(result) = dash_core::link::forward($op, serde_json::json!($args)) {
                return result.unwrap_or_default();
            }
        }
    };
}

const FRECENCY_FILE: &str = "frecency.json";
const PREFERENCES_DIR: &str = "preferences";

/// Exponential half-life for the recency component. After 7 days, the boost
/// from "frequency" is halved; after 14 days, quartered, etc.
const HALF_LIFE_DAYS: f64 = 7.0;
/// Multiplier applied to the (freq × recency) product to scale the boost into
/// the same magnitude as Tantivy/BM25 scores. Tuned to influence ranking
/// meaningfully without overwhelming a precise textual match.
const BOOST_MULTIPLIER: f64 = 2.0;
/// Cap on how many entries we keep on disk — prevents unbounded growth from a
/// user who searches and opens thousands of files over months.
const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrecencyEntry {
    pub launch_count: u32,
    pub last_launched_ms: u128,
    /// The original path as the user activated it (preserves case). The HashMap
    /// key is lowercased for matching, but for display ("Recent Files" rows)
    /// we want the real path. Defaulted for backwards compat with older data.
    #[serde(default)]
    pub original_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrecencyData {
    pub version: u32,
    pub entries: HashMap<String, FrecencyEntry>,
}

impl Default for FrecencyData {
    fn default() -> Self {
        Self { version: 1, entries: HashMap::new() }
    }
}

/// In-memory mirror of the on-disk data. Loaded lazily on first access; every
/// write keeps both the memory copy and the disk copy in sync.
static FRECENCY_CACHE: LazyLock<Mutex<Option<FrecencyData>>> = LazyLock::new(|| Mutex::new(None));

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn frecency_paths(app: &dyn dash_core::Host) -> Result<(PathBuf, PathBuf, String), String> {
    let dir = app.data_dir().map_err(|e| format!("Cannot resolve app data directory: {e}"))?.join(PREFERENCES_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create preferences directory: {e}"))?;
    let json_path = dir.join(FRECENCY_FILE);
    let db_path = local_db::database_path_for_dir(&dir);
    Ok((json_path, db_path, FRECENCY_FILE.to_string()))
}

/// Stable lookup key. Kind segregates app vs file vs folder vs tool so they
/// don't collide when paths overlap (e.g. a folder and an .exe at the same
/// canonical path location).
fn make_key(kind: &str, path: &str) -> String {
    format!("{}:{}", kind, path.to_lowercase())
}

/// Load (and lazily initialize) the cache, returning a clone for read-only use.
fn load(app: &dyn dash_core::Host) -> FrecencyData {
    {
        let cache = FRECENCY_CACHE.lock();
        if let Ok(guard) = cache {
            if let Some(data) = guard.as_ref() {
                return data.clone();
            }
        }
    }
    let data = read_stored(app);
    if let Ok(mut guard) = FRECENCY_CACHE.lock() {
        *guard = Some(data.clone());
    }
    data
}

/// The data as stored on disk (default when missing or unreadable).
fn read_stored(app: &dyn dash_core::Host) -> FrecencyData {
    match frecency_paths(app) {
        Ok((_json_path, db_path, key)) => {
            local_db::read_json_or_default(&db_path, &key, FrecencyData::default()).unwrap_or_default()
        }
        Err(_) => FrecencyData::default(),
    }
}

fn persist(app: &dyn dash_core::Host, data: &FrecencyData) -> Result<(), String> {
    let (_json_path, db_path, key) = frecency_paths(app)?;
    let bytes = serde_json::to_vec(data).map_err(|e| format!("Cannot serialize frecency data: {e}"))?;
    local_db::write_json_bytes(&db_path, &key, &bytes)
}

/// Record a single activation. Called from the same place we launch/open
/// the underlying entry. Persists synchronously so a crash before the next
/// boot doesn't lose the data — writes are cheap (~1ms via redb).
pub fn record_frecency_launch(
    app: std::sync::Arc<dyn dash_core::Host>,
    kind: String,
    path: String,
) -> Result<(), String> {
    on_owner!("record_frecency_launch", { "kind": kind, "path": path });
    if path.trim().is_empty() {
        return Ok(());
    }

    let key = make_key(&kind, &path);
    let now = now_ms();

    // One lock across read-modify-persist, so two launches at once can't
    // drop each other's update.
    let mut guard = FRECENCY_CACHE.lock().map_err(|_| "Frecency cache lock failed".to_string())?;
    let mut data = match guard.as_ref() {
        Some(data) => data.clone(),
        None => read_stored(app.as_ref()),
    };
    let entry = data.entries.entry(key).or_insert(FrecencyEntry {
        launch_count: 0,
        last_launched_ms: 0,
        original_path: String::new(),
    });
    entry.launch_count = entry.launch_count.saturating_add(1);
    entry.last_launched_ms = now;
    // Always update — if path casing changed (e.g. user switched drives) we
    // want the most recent display form.
    entry.original_path = path.clone();

    // Bound the table size — drop the oldest-by-last-launch entries when over cap.
    if data.entries.len() > MAX_ENTRIES {
        let mut sorted: Vec<(String, u128)> =
            data.entries.iter().map(|(k, v)| (k.clone(), v.last_launched_ms)).collect();
        sorted.sort_by_key(|&(_, ms)| ms);
        let to_drop = data.entries.len() - MAX_ENTRIES;
        for (k, _) in sorted.into_iter().take(to_drop) {
            data.entries.remove(&k);
        }
    }

    persist(app.as_ref(), &data)?;
    *guard = Some(data);
    Ok(())
}

/// Compute the boost an individual entry contributes to a base score.
/// Public so search.rs can apply it when ranking results.
#[expect(
    clippy::cast_possible_truncation,
    reason = "ln(count + 1) * a 0..=1 decay * 2 stays under 100, so f64 to f32 only drops precision"
)]
pub fn compute_boost(entry: &FrecencyEntry, now: u128) -> f32 {
    let age_ms = now.saturating_sub(entry.last_launched_ms) as f64;
    let age_days = age_ms / (1000.0 * 60.0 * 60.0 * 24.0);
    let recency = (-age_days * std::f64::consts::LN_2 / HALF_LIFE_DAYS).exp();
    let freq = ((entry.launch_count as f64) + 1.0).ln();
    (freq * recency * BOOST_MULTIPLIER) as f32
}

/// Resolve a boost for a (kind, path) pair given a pre-loaded snapshot.
/// Returns 0.0 if the path has never been activated.
pub fn boost_for(data: &FrecencyData, kind: &str, path: &str, now: u128) -> f32 {
    let key = make_key(kind, path);
    data.entries.get(&key).map(|entry| compute_boost(entry, now)).unwrap_or(0.0)
}

/// Snapshot of the current data + a reference timestamp — passed to per-result
/// boost calls so we don't recompute "now" or re-read disk for every result.
pub struct FrecencySnapshot {
    pub data: FrecencyData,
    pub now_ms: u128,
}

impl FrecencySnapshot {
    pub fn boost(&self, kind: &str, path: &str) -> f32 {
        boost_for(&self.data, kind, path, self.now_ms)
    }
}

/// Helper used by search code — load once, boost many results.
pub fn snapshot(app: &dyn dash_core::Host) -> FrecencySnapshot {
    FrecencySnapshot { data: load(app), now_ms: now_ms() }
}

/// An item returned by `get_recent_items`. Sorted client-side, presented
/// directly in the overlay's "Recent" view when no query is active.
///
/// `size_bytes` and `modified_ms` are populated lazily — only for `file` /
/// `folder` kinds, and only AFTER the per-kind list has been truncated to
/// the top-N. This keeps the disk hit bounded (≤ 20 stat() calls per call,
/// in practice 6). Apps / tools don't get stat'd since the preview pane
/// doesn't show those fields for those kinds.
///
/// Both fields are optional: missing means we either couldn't read the file
/// (deleted, permission denied, network drive offline) OR we deliberately
/// skipped stat'ing (kind is app/tool). The frontend treats `None` as
/// "metadata unavailable" rather than "size is zero".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentItem {
    pub kind: String,
    pub path: String,
    pub display_name: String,
    pub launch_count: u32,
    pub last_launched_ms: u128,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<u128>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentItemsResult {
    pub apps: Vec<RecentItem>,
    pub files: Vec<RecentItem>,
    pub folders: Vec<RecentItem>,
    pub tools: Vec<RecentItem>,
}

/// Return the top-N entries per kind, ranked by frecency. Used by the overlay
/// to render a curated "Recent" view when no query is active. Fast — single
/// pass over the in-memory frecency map (< 5000 entries by design).
pub fn get_recent_items(app: std::sync::Arc<dyn dash_core::Host>, limit_per_kind: Option<usize>) -> RecentItemsResult {
    on_owner!("get_recent_items", { "limit_per_kind": limit_per_kind }, or_default);
    let limit = limit_per_kind.unwrap_or(4).clamp(1, 20);
    let data = load(&app);
    let now = now_ms();

    let mut by_kind: HashMap<String, Vec<RecentItem>> = HashMap::new();
    for (key, entry) in &data.entries {
        let Some((kind, lowercase_path)) = key.split_once(':') else {
            continue;
        };
        // Use the original-case path for display when available, falling back
        // to the lowercase key path for entries written before that field existed.
        let display_path =
            if entry.original_path.is_empty() { lowercase_path.to_string() } else { entry.original_path.clone() };
        let display_name = derive_display_name(kind, &display_path);
        let score = compute_boost(entry, now);
        by_kind.entry(kind.to_string()).or_default().push(RecentItem {
            kind: kind.to_string(),
            path: display_path,
            display_name,
            launch_count: entry.launch_count,
            last_launched_ms: entry.last_launched_ms,
            score,
            size_bytes: None,
            modified_ms: None,
        });
    }

    // Sort each bucket by score (frecency) descending, then truncate.
    for items in by_kind.values_mut() {
        items.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        items.truncate(limit);
    }

    // Wave I (2026-05-27): stat the on-disk file for the kept `file` /
    // `folder` entries so the preview pane can show real Size + Modified
    // values instead of falling back to "0 B" / "—". We stat AFTER the
    // truncate so we only hit the top-N visible items (≤ 20) — not the
    // whole frecency map (potentially thousands of entries). Errors are
    // swallowed because a missing/locked file is not a reason to fail
    // the whole recents query; the frontend already handles `None`
    // gracefully (shows "—" for that one field).
    for kind in ["file", "folder"] {
        if let Some(items) = by_kind.get_mut(kind) {
            items.retain_mut(|item| match std::fs::metadata(&item.path) {
                Ok(metadata) => {
                    item.size_bytes = Some(metadata.len());
                    if let Ok(modified) = metadata.modified() {
                        if let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) {
                            item.modified_ms = Some(duration.as_millis());
                        }
                    }
                    true
                }
                // A file/folder that's actually gone (deleted or moved) must not
                // linger in Recents — opening it would silently do nothing. We
                // only drop confirmed-missing paths; other errors (permission
                // denied, offline network drive) keep the entry, since the path
                // still exists and we just can't stat it right this moment.
                Err(err) => err.kind() != std::io::ErrorKind::NotFound,
            });
        }
    }

    RecentItemsResult {
        apps: by_kind.remove("app").unwrap_or_default(),
        files: by_kind.remove("file").unwrap_or_default(),
        folders: by_kind.remove("folder").unwrap_or_default(),
        tools: by_kind.remove("tool").unwrap_or_default(),
    }
}

/// Extract a human-friendly display name from a path or tool id.
/// - app: filename without extension ("chrome", not "Chrome.exe")
/// - file: filename with extension ("report.pdf")
/// - folder: folder name (last path component)
/// - tool: tool id is already the display key, frontend resolves to the proper title
fn derive_display_name(kind: &str, path: &str) -> String {
    if kind == "tool" {
        return path.to_string();
    }
    let path_buf = std::path::Path::new(path);
    let stem_or_name = if kind == "app" {
        path_buf.file_stem().and_then(|s| s.to_str()).map(str::to_string)
    } else {
        path_buf.file_name().and_then(|s| s.to_str()).map(str::to_string)
    };
    stem_or_name.unwrap_or_else(|| path.to_string())
}

/// Return a map of `path → boost` for every entry of the given kind. Used by
/// the frontend to apply frecency to lists that are scored client-side (notably
/// the overlay's tool matches, which never touch the backend search).
///
/// The returned paths have their `kind:` prefix stripped — callers ask for one
/// kind at a time so the keys are unambiguous.
pub fn get_frecency_boosts(app: std::sync::Arc<dyn dash_core::Host>, kind: String) -> HashMap<String, f32> {
    on_owner!("get_frecency_boosts", { "kind": kind }, or_default);
    let data = load(&app);
    let now = now_ms();
    let prefix = format!("{}:", kind.to_lowercase());
    data.entries
        .iter()
        .filter_map(|(k, entry)| k.strip_prefix(&prefix).map(|path| (path.to_string(), compute_boost(entry, now))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct TempHost(PathBuf);
    impl dash_core::Host for TempHost {
        fn data_dir(&self) -> Result<PathBuf, String> {
            Ok(self.0.clone())
        }
        fn emit(&self, _event: &str, _payload: serde_json::Value) {}
    }

    /// Launches recorded at the same time all count (no lost updates).
    #[test]
    fn concurrent_launches_all_count() {
        let dir = std::env::temp_dir().join(format!("dash-frecency-test-{}", std::process::id()));
        let app: Arc<dyn dash_core::Host> = Arc::new(TempHost(dir.clone()));
        let count = || load(app.as_ref()).entries.get(&make_key("file", "c:/race.txt")).map_or(0, |e| e.launch_count);
        let before = count();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let app = app.clone();
                std::thread::spawn(move || {
                    for _ in 0..10 {
                        record_frecency_launch(app.clone(), "file".into(), "C:/race.txt".into()).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(count() - before, 80);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
