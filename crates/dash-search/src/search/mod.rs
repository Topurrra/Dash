//! Runs in any app that is a `dash_core::Host`.
use super::local_db;
use ignore::WalkBuilder;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tantivy::collector::{Count, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::query::AllQuery;
use tantivy::query::{BooleanQuery, FuzzyTermQuery, Occur, Query, QueryParser, RangeQuery, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, SchemaBuilder, Value, FAST, STORED, STRING, TEXT};
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term};
use walkdir::WalkDir;

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

const SEARCH_INDEX_DIR: &str = "file-search-index";
const ACTIVE_SEARCH_INDEX_DIR: &str = "index";
const PREVIOUS_SEARCH_INDEX_DIR: &str = "index-previous";
const STAGING_SEARCH_INDEX_PREFIX: &str = "index-build-";
// Quarantined copy of a corrupt content index, preserved for diagnostics.
const CORRUPT_SEARCH_INDEX_PREFIX: &str = "index-corrupt-";
// ── Filename index (Phase 2, step 6) ────────────────────────────────────────
// A separate tantivy index holding only names + paths + metadata (no file
// content), so filename search is never blocked by content indexing. It lives
// in the same `file-search-index/` state directory under distinct names so the
// two indexes have fully independent lifecycles. The `#[allow(dead_code)]`
// markers on the filename-index scaffolding come off as later 6x steps wire it in.
#[allow(dead_code, reason = "filename-index scaffolding, wired in step by step")]
const FILENAME_INDEX_ACTIVE_DIR: &str = "filename";
#[allow(dead_code, reason = "filename-index scaffolding, wired in step by step")]
const FILENAME_INDEX_PREVIOUS_DIR: &str = "filename-previous";
#[allow(dead_code, reason = "filename-index scaffolding, wired in step by step")]
const FILENAME_STAGING_INDEX_PREFIX: &str = "filename-build-";
// Quarantined copy of a corrupt filename index, preserved for diagnostics.
const FILENAME_CORRUPT_INDEX_PREFIX: &str = "filename-corrupt-";
#[allow(dead_code, reason = "filename-index scaffolding, wired in step by step")]
const FILENAME_SCHEMA_VERSION_FILE: &str = "filename_schema_version.json";
#[allow(dead_code, reason = "filename-index scaffolding, wired in step by step")]
const FILENAME_SCHEMA_VERSION: u32 = 1;
const SEARCH_CONFIG_FILE: &str = "search-config.json";
const INDEX_WORKER_OPTIONS_FILE: &str = "index-worker-options.json";
const INDEX_WORKER_STATUS_FILE: &str = "index-worker-status.json";
const INDEX_WORKER_CANCEL_FILE: &str = "index-worker-cancel";
const WATCHER_WORKER_OPTIONS_FILE: &str = "index-watcher-options.json";
const WATCHER_WORKER_STATUS_FILE: &str = "index-watcher-status.json";
const WATCHER_WORKER_CANCEL_FILE: &str = "index-watcher-cancel";
// Standalone filename indexer (Phase 3, step 7b) — its own worker IPC files so
// it runs fully independent of the content index worker.
const FILENAME_WORKER_OPTIONS_FILE: &str = "filename-index-options.json";
const FILENAME_WORKER_STATUS_FILE: &str = "filename-index-status.json";
const FILENAME_WORKER_CANCEL_FILE: &str = "filename-index-cancel";
const INDEX_WORKER_ARG: &str = "--dash-index-worker";
const MAX_INDEXED_FILES: usize = 5_000_000;
const WRITER_MEMORY_BUDGET_BYTES_IDLE: usize = 32 * 1024 * 1024;
/// The least a full build's writer gets (see `index_writer_sizing`).
const WRITER_MEMORY_BUDGET_BYTES_INDEXING: usize = 96 * 1024 * 1024;
const WRITER_MEMORY_PER_THREAD_BYTES: usize = 64 * 1024 * 1024;
// Global ceiling on per-file content indexing. Each format has its own
// tier limit in `text_extract::tier_bytes_for_path`; the effective limit
// per file is min(tier, this). Default of 1024 KB lets every tier hit
// its natural ceiling — code files only use ~64 KB, PDFs use the full 1 MB.
const DEFAULT_MAX_CONTENT_KB: u32 = 1024;
const MIN_COMMIT_EVERY: usize = 300;
const DEFAULT_COMMIT_EVERY: usize = 25_000;
const ENTRY_TYPE_FILE: &str = "file";
const ENTRY_TYPE_FOLDER: &str = "folder";
const INDEX_WRITER_THREADS_MAX: usize = 6;
// Default fallback if available_parallelism() can't be queried. Real value
// comes from `index_walker_threads()` which scales with CPU cores.
const INDEX_WALKER_THREADS_FALLBACK: usize = 4;
const INDEX_PROGRESS_EVERY_FILES: u64 = 1000;
const INDEX_PROGRESS_MIN_INTERVAL_MS: u128 = 700;
const INDEX_THROTTLE_EVERY_FILES: u64 = 2_500;
const INDEX_THROTTLE_SLEEP_MS_BALANCED: u64 = 6;
const INDEX_THROTTLE_SLEEP_MS_QUIET: u64 = 18;
const LARGE_INDEX_WATCHER_THRESHOLD: u64 = 250_000;
const HUGE_INDEX_WATCHER_THRESHOLD: u64 = 1_000_000;
const SEARCH_READER_RELOAD_MIN_INTERVAL_MS: u128 = 2_500;
const INDEX_PROGRESS_EVENT: &str = "file-search-index-progress";
const INDEX_REBUILD_SCHEDULER_POLL_SECS: u64 = 60;
/// How long the standalone filename index may go between full rebuilds before
/// the scheduler refreshes it. Filename indexing is a cheap metadata-only walk,
/// so a safety-net rebuild a few times a day costs little; live updates
/// (task 9) will later keep it current between these full rebuilds.
const FILENAME_INDEX_REFRESH_INTERVAL_MS: u128 = 6 * 60 * 60 * 1000;
const WATCHER_SETTINGS_VERSION: u32 = 1;
/// Schema version for the Tantivy file-search index. Bump this whenever
/// `build_search_schema` changes — a field added, removed, or retyped, or a
/// tokenizer change. On startup the on-disk `schema_version.json` sentinel is
/// compared against this value; a mismatch wipes the stale index so it
/// rebuilds cleanly against the current schema.
const SEARCH_SCHEMA_VERSION: u32 = 1;
const SCHEMA_VERSION_FILE: &str = "schema_version.json";
const MAX_LAUNCH_TARGETS: usize = 12_000;
const MAX_LAUNCH_SCAN_DEPTH: usize = 6;
const DEFAULT_EXCLUDE_FOLDERS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".cache",
    ".gradle",
    ".idea",
    ".mypy_cache",
    ".next",
    ".pytest_cache",
    ".ruff_cache",
    ".svelte-kit",
    ".venv",
    ".vscode",
    "$recycle.bin",
    "$windows.~bt",
    "$windows.~ws",
    "__pycache__",
    // Wave 6 (2026-05-28): block the whole AppData/Local + AppData/LocalLow
    // trees. They contain ~50-300k files of pure app-internal cache
    // (browser profiles, Spotify cache, Discord/Teams installs, etc.) with
    // zero user-authored content. AppData/Roaming stays in scope — it
    // holds smaller, more user-relevant app configs (.npmrc, IDE settings,
    // dev tool prefs).
    "appdata/local",
    "appdata/locallow",
    "appdata/local/crashdumps",
    "appdata/local/temp",
    "bin",
    "build",
    "config.msi",
    "dist",
    "file-search-index",
    "inetpub/logs",
    "microsoft/windows/wer",
    "node_modules",
    "obj",
    "programdata/microsoft/windows/wer",
    "programdata/package cache",
    "recovery",
    "system volume information",
    "target",
    "temp",
    "tmp",
    "users/*/appdata/local/temp",
    "venv",
    "windows/debug",
    "windows/logs",
    "windows/panther",
    "windows/prefetch",
    "windows/softwaredistribution/download",
    "windows/temp",
    "windows/winsxs/temp",
];
/// Folders added with `exclude_always` (an app's private data).
static ALWAYS_EXCLUDED: LazyLock<Mutex<Vec<String>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Never index `folder` (a name like `my-app-cache`, or a full path), even
/// if the user's settings don't list it. For an app embedding the engines.
pub fn exclude_always(folder: &str) {
    let token = folder.trim().replace('\\', "/").to_lowercase();
    let mut list = ALWAYS_EXCLUDED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !token.is_empty() && !list.contains(&token) {
        list.push(token);
    }
}

/// Folders every index skips: the engines' own data (an index mustn't
/// index itself) and those added with `exclude_always`.
fn required_exclude_folders() -> Vec<String> {
    let own = dash_core::shared::shared_data_dir().to_string_lossy().replace('\\', "/").to_lowercase();
    let mut list = vec![own.trim_end_matches('/').to_string()];
    list.extend(ALWAYS_EXCLUDED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().cloned());
    list
}
const DEFAULT_EXCLUDE_EXTENSIONS: &[&str] = &[
    "a",
    "bin",
    "cache",
    "class",
    "crdownload",
    "dll",
    "dmp",
    "ds_store",
    "dylib",
    "gitignore",
    "ilk",
    "lib",
    "o",
    "obj",
    "part",
    "pdb",
    "pyc",
    "pyd",
    "pyo",
    "so",
    "sys",
    "temp",
    "tmp",
];

static SEARCH_BUILD_CANCELLED: AtomicBool = AtomicBool::new(false);
static SEARCH_SCHEDULER_STARTED: AtomicBool = AtomicBool::new(false);
static SEARCH_ENGINE: LazyLock<Mutex<Option<SearchEngine>>> = LazyLock::new(|| Mutex::new(None));
/// The standalone filename index, cached independently of `SEARCH_ENGINE`. The
/// filename index (File Search tab) and the content index (Content Search tab)
/// are built independently, so file/name search must work even when no content
/// index exists — keeping this handle out of `SearchEngine` is what allows that.
static FILENAME_ENGINE: LazyLock<Mutex<Option<FilenameIndexHandle>>> = LazyLock::new(|| Mutex::new(None));
/// A filename build is running: `FILENAME_ENGINE` stays closed until it
/// finishes. A search used to reopen it lazily, and the open memory map then
/// blocked the build's final folder rename on Windows, throwing the finished
/// build away. Name search keeps working from the in-memory name table.
static FILENAME_BUILD_RUNNING: AtomicBool = AtomicBool::new(false);
static SEARCH_STATUS: LazyLock<Mutex<FileSearchStatus>> = LazyLock::new(|| Mutex::new(FileSearchStatus::default()));
static LAUNCH_TARGET_CACHE: LazyLock<Mutex<Option<LaunchTargetCache>>> = LazyLock::new(|| Mutex::new(None));
/// Process-global folder watcher that invalidates `LAUNCH_TARGET_CACHE` when an
/// app is installed / uninstalled / renamed under a launcher root, so a freshly
/// installed app appears in the palette without an app restart. Started lazily,
/// once (see `ensure_launch_target_watcher`).
static LAUNCH_TARGET_WATCHER: LazyLock<Mutex<Option<RecommendedWatcher>>> = LazyLock::new(|| Mutex::new(None));
static SEARCH_INDEX_WORKER: LazyLock<Mutex<Option<SearchIndexWorkerProcess>>> = LazyLock::new(|| Mutex::new(None));
static SEARCH_WATCHER_WORKER: LazyLock<Mutex<Option<SearchIndexWorkerProcess>>> = LazyLock::new(|| Mutex::new(None));
static FILENAME_INDEX_WORKER: LazyLock<Mutex<Option<SearchIndexWorkerProcess>>> = LazyLock::new(|| Mutex::new(None));
static LAST_SEARCH_READER_RELOAD_MS: LazyLock<Mutex<u128>> = LazyLock::new(|| Mutex::new(0));
/// In-memory timestamp (ms) of the last filename rebuild this scheduler
/// triggered. Resets on restart — the launch check uses the on-disk sentinel
/// for cross-restart staleness — and stops a failed build from being retried
/// every poll.
static FILENAME_INDEX_LAST_REBUILD_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// The code, by what it does (each part sees the others through `super`).
mod commands;
mod documents;
mod engine;
mod filename_index;
mod filters;
mod indexing;
mod launch;
mod lookup;
mod name_table;
mod natural;
mod results;
mod semantic;
mod storage;
mod types;
mod workers;

pub use commands::*;
pub use documents::*;
pub use engine::*;
use filename_index::*;
pub use filters::*;
use indexing::*;
use launch::*;
pub(crate) use lookup::*;
pub(crate) use name_table::*;
use natural::*;
pub use results::*;
use semantic::*;
pub use storage::*;
pub use types::*;
pub use workers::*;

#[cfg(test)]
mod tests;
