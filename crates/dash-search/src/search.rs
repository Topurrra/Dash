//! Runs in any app that is a `dash_core::Host`.
use super::local_db;
use ignore::WalkBuilder;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
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
use std::ops::Bound;
use tantivy::{
    doc, Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term,
};
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
#[allow(dead_code)]
const FILENAME_INDEX_ACTIVE_DIR: &str = "filename";
#[allow(dead_code)]
const FILENAME_INDEX_PREVIOUS_DIR: &str = "filename-previous";
#[allow(dead_code)]
const FILENAME_STAGING_INDEX_PREFIX: &str = "filename-build-";
// Quarantined copy of a corrupt filename index, preserved for diagnostics.
const FILENAME_CORRUPT_INDEX_PREFIX: &str = "filename-corrupt-";
#[allow(dead_code)]
const FILENAME_SCHEMA_VERSION_FILE: &str = "filename_schema_version.json";
#[allow(dead_code)]
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
const WRITER_MEMORY_BUDGET_BYTES_INDEXING: usize = 96 * 1024 * 1024;
// Global ceiling on per-file content indexing. Each format has its own
// tier limit in `text_extract::tier_bytes_for_path`; the effective limit
// per file is min(tier, this). Default of 1024 KB lets every tier hit
// its natural ceiling — code files only use ~64 KB, PDFs use the full 1 MB.
const DEFAULT_MAX_CONTENT_KB: u32 = 1024;
const MIN_COMMIT_EVERY: usize = 300;
const DEFAULT_COMMIT_EVERY: usize = 25_000;
const ENTRY_TYPE_FILE: &str = "file";
const ENTRY_TYPE_FOLDER: &str = "folder";
const INDEX_WRITER_THREADS_MAX: usize = 2;
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
    let mut list = ALWAYS_EXCLUDED.lock().unwrap_or_else(|p| p.into_inner());
    if !token.is_empty() && !list.contains(&token) {
        list.push(token);
    }
}

/// Folders every index skips: the engines' own data (an index mustn't
/// index itself) and those added with `exclude_always`.
fn required_exclude_folders() -> Vec<String> {
    let own = dash_core::shared::shared_data_dir().to_string_lossy().replace('\\', "/").to_lowercase();
    let mut list = vec![own.trim_end_matches('/').to_string()];
    list.extend(ALWAYS_EXCLUDED.lock().unwrap_or_else(|p| p.into_inner()).iter().cloned());
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
static FILENAME_ENGINE: LazyLock<Mutex<Option<FilenameIndexHandle>>> =
    LazyLock::new(|| Mutex::new(None));
static SEARCH_STATUS: LazyLock<Mutex<FileSearchStatus>> =
    LazyLock::new(|| Mutex::new(FileSearchStatus::default()));
static LAUNCH_TARGET_CACHE: LazyLock<Mutex<Option<LaunchTargetCache>>> =
    LazyLock::new(|| Mutex::new(None));
/// Process-global folder watcher that invalidates `LAUNCH_TARGET_CACHE` when an
/// app is installed / uninstalled / renamed under a launcher root, so a freshly
/// installed app appears in the palette without an app restart. Started lazily,
/// once (see `ensure_launch_target_watcher`).
static LAUNCH_TARGET_WATCHER: LazyLock<Mutex<Option<RecommendedWatcher>>> =
    LazyLock::new(|| Mutex::new(None));
static SEARCH_INDEX_WORKER: LazyLock<Mutex<Option<SearchIndexWorkerProcess>>> =
    LazyLock::new(|| Mutex::new(None));
static SEARCH_WATCHER_WORKER: LazyLock<Mutex<Option<SearchIndexWorkerProcess>>> =
    LazyLock::new(|| Mutex::new(None));
static FILENAME_INDEX_WORKER: LazyLock<Mutex<Option<SearchIndexWorkerProcess>>> =
    LazyLock::new(|| Mutex::new(None));
static LAST_SEARCH_READER_RELOAD_MS: LazyLock<Mutex<u128>> = LazyLock::new(|| Mutex::new(0));
/// In-memory timestamp (ms) of the last filename rebuild this scheduler
/// triggered. Resets on restart — the launch check uses the on-disk sentinel
/// for cross-restart staleness — and stops a failed build from being retried
/// every poll.
static FILENAME_INDEX_LAST_REBUILD_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[derive(Clone)]
struct SearchIndexWorkerProcess {
    child: Arc<Mutex<Child>>,
    status_path: PathBuf,
    cancel_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchIndexOptions {
    /// Sources for the content index (the Content Search tab).
    pub roots: Vec<String>,
    /// Sources for the filename index (the File Search tab) — separate from
    /// `roots` so filename search can be broad/whole-disk while content search
    /// stays folder-scoped. Migrated from `roots` on first load when empty.
    #[serde(default)]
    pub filename_roots: Vec<String>,
    pub include_hidden: bool,
    // Wave 7.7 (2026-05-28): `follow_symlinks` removed — see the comment
    // on FileSearchStatus.include_hidden below for the full rationale.
    // Kept the field in serde for backward compat (old saved configs
    // just get the value ignored on deserialize).
    pub index_content: bool,
    pub max_content_kb: Option<u32>,
    pub commit_every: Option<usize>,
    #[serde(default = "default_index_performance_mode")]
    pub performance_mode: String,
    #[serde(default = "default_watcher_enabled")]
    pub watcher_enabled: bool,
    #[serde(default)]
    pub watcher_paused: bool,
    #[serde(default)]
    pub watcher_settings_version: u32,
    #[serde(default = "default_exclude_folders")]
    pub exclude_folders: Vec<String>,
    #[serde(default = "default_exclude_extensions")]
    pub exclude_extensions: Vec<String>,
    // Wave 6 (2026-05-28): removed `mft_fast_index` and `allow_root_drive_watcher`
    // along with the MFT/USN engine. Saved configs that still carry these
    // fields just deserialize-ignore them (serde drops unknown fields by
    // default on Options). System paths (`C:\Windows`, `Program Files`,
    // `Program Files (x86)`, `ProgramData`) are now always hard-blocked
    // from both walker and watcher — no opt-in flag needed.
    //
    // Wave 8 / task #88 (2026-05-28): OCR-on-Index for scan-only PDFs.
    // When enabled AND a PDF in one of `ocr_on_index_folders` has no text
    // layer (both pdfium-render and lopdf yield nothing), the extractor
    // subprocess falls back to rendering pages via PDFium + Tesseract OCR
    // so scanned documents become full-text searchable. Languages locked
    // at eng+rus+kat (all bundled in `tessdata-runtime/`). Default OFF —
    // tesseract per-page is slow and most users don't need it.
    #[serde(default)]
    pub ocr_on_index_enabled: bool,
    #[serde(default)]
    pub ocr_on_index_folders: Vec<String>,
    #[serde(default = "default_ocr_max_pages")]
    pub ocr_max_pages_per_file: u32,
    #[serde(default = "default_ocr_langs")]
    pub ocr_langs: String,
    // Wave 8.2 (2026-05-28): smart-skip heuristic knobs. Each value is
    // 0-or-positive; 0 disables the corresponding check.
    #[serde(default = "default_ocr_min_image_dim")]
    pub ocr_min_image_dim: u32,
    #[serde(default = "default_ocr_max_aspect_ratio")]
    pub ocr_max_aspect_ratio: f32,
    #[serde(default = "default_ocr_timeout_secs")]
    pub ocr_per_file_timeout_secs: u32,
    // Indexing Phase 2 / Task 4.3 (2026-06-18): user opt-in for content
    // indexing ("search inside files"). When false the content worker no-ops;
    // the filename index still builds in its own separate worker. Default true;
    // the frontend defaults it OFF on first run on Lite (low-RAM) profiles
    // where a content index can't converge.
    #[serde(default = "default_content_indexing_enabled")]
    pub content_indexing_enabled: bool,
    // Semantic search: opt-in embedding-based retrieval. When true AND the
    // `semantic` feature is compiled in AND the bundled model is present,
    // the content indexer stores a vector per doc and content queries add a
    // semantic candidate pass. Default OFF — it costs a bundled model,
    // per-doc embedding at index time, and a brute-force cosine scan per
    // query. Requires content indexing.
    #[serde(default)]
    pub semantic_search_enabled: bool,
}

fn default_content_indexing_enabled() -> bool {
    true
}

fn default_ocr_max_pages() -> u32 {
    20
}

fn default_ocr_langs() -> String {
    "eng+rus+kat".to_string()
}

fn default_ocr_min_image_dim() -> u32 {
    400
}

fn default_ocr_max_aspect_ratio() -> f32 {
    5.0
}

fn default_ocr_timeout_secs() -> u32 {
    30
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchRebuildSchedule {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_rebuild_interval_hours")]
    pub interval_hours: u32,
    #[serde(default)]
    pub last_scheduled_rebuild_at_ms: Option<u128>,
}

impl Default for FileSearchRebuildSchedule {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_hours: default_rebuild_interval_hours(),
            last_scheduled_rebuild_at_ms: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchQueryOptions {
    pub query: String,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub extension_filter: Option<String>,
    pub path_filter: Option<String>,
    pub natural_language: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentSearchResultItem {
    pub path: String,
    pub file_name: String,
    pub extension: String,
    pub size: u64,
    pub modified_ms: u64,
    pub score: f32,
    /// Primary highlighted snippet — HTML with `<b>` around matched terms.
    pub snippet: String,
    /// Additional highlighted snippets from elsewhere in the document, up to
    /// a small cap. Empty when the file has only one matching passage.
    pub snippets: Vec<String>,
    /// Total occurrences of any query keyword in the content. Used as a
    /// secondary ranking signal and shown to the user as a confidence cue.
    pub match_count: u32,
    pub matched_keywords: Vec<String>,
    /// Sensitive-content kinds detected at index time. See FileSearchResultItem.
    pub sensitive_kinds: Vec<String>,
    /// Semantic (embedding cosine) similarity to the query, 0..1. > 0 means the
    /// meaning-search pass rated this document related; 0 means it surfaced on
    /// keywords alone (or semantic search is off). Shown as a "meaning match" cue.
    pub semantic_score: f32,
    /// True when this result was surfaced ONLY by semantic search — it shares no
    /// literal query keyword and would not appear without the meaning pass.
    pub semantic_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentSearchQueryResult {
    pub query: String,
    pub total_hits: usize,
    pub returned: usize,
    pub took_ms: u128,
    pub results: Vec<ContentSearchResultItem>,
    /// True when the semantic (meaning) pass actually ran for this query — the
    /// toggle is on, the model is loaded, and the query embedded. Lets the UI
    /// show "semantic on" even when a query happens to yield no meaning-only
    /// hits, so the user can tell it's active vs. silently unavailable.
    pub semantic_active: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchTargetSearchOptions {
    pub query: String,
    pub limit: Option<usize>,
    /// Palette Appearance Wave G (2026-05-27): "browse all installed
    /// apps" mode. When `true` AND `query` is empty, returns up to
    /// `limit` records straight from the launch-target cache (no
    /// scoring) so the palette's "Apps" chip can show every installed
    /// app at-a-glance without forcing the user to type something.
    #[serde(default)]
    pub browse_all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchTargetItem {
    pub id: String,
    pub name: String,
    pub path: String,
    pub kind: String,
    pub source: String,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchTargetSearchResult {
    pub query: String,
    pub total_hits: usize,
    pub returned: usize,
    pub took_ms: u128,
    pub cache_built_at_ms: Option<u128>,
    pub results: Vec<LaunchTargetItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchResultItem {
    pub path: String,
    pub file_name: String,
    pub entry_type: String,
    pub extension: String,
    pub size: u64,
    pub modified_ms: u64,
    pub score: f32,
    pub matched_keywords: Vec<String>,
    pub match_reason: String,
    /// Sensitive-content findings detected at index time (e.g. "aws_access_key",
    /// "private_key_pem"). Empty when nothing was detected or content wasn't
    /// indexed for this file. Always contains "sensitive" as the first entry
    /// when anything else is present.
    pub sensitive_kinds: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchQueryResult {
    pub query: String,
    pub total_hits: usize,
    pub returned: usize,
    pub took_ms: u128,
    pub results: Vec<FileSearchResultItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchBuildResult {
    pub success: bool,
    pub canceled: bool,
    pub background: bool,
    pub indexed_files: u64,
    pub scanned_entries: u64,
    pub skipped_files: u64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchProgressEvent {
    pub stage: String,
    /// Which index this event reports — "filename" or "content" — so the UI
    /// can notify the right index when a build `finished`.
    pub index_kind: String,
    pub indexed_files: u64,
    pub scanned_entries: u64,
    pub skipped_files: u64,
    pub current_path: String,
    pub finished: bool,
    pub canceled: bool,
    /// Whether the build succeeded — meaningful only on a `finished` event, and
    /// distinct from `canceled` so the UI can tell success / failure / cancel apart.
    pub success: bool,
    pub message: String,
    /// Indexing Phase 2 / Task 4.1: a promoted *partial* index (a low-memory
    /// pool death or user cancel committed what was indexed so far). The build
    /// is resumable, not complete — the UI shows it distinctly from a full
    /// success. Only meaningful on a `finished` content event.
    pub partial: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchDiagnostics {
    pub index_worker: String,
    pub watcher_worker: String,
    pub watcher_strategy: String,
    pub performance_mode: String,
    pub last_worker_message: Option<String>,
    pub last_status_at_ms: Option<u128>,
}

impl Default for FileSearchDiagnostics {
    fn default() -> Self {
        Self {
            index_worker: "stopped".to_string(),
            watcher_worker: "stopped".to_string(),
            watcher_strategy: "idle".to_string(),
            performance_mode: default_index_performance_mode(),
            last_worker_message: None,
            last_status_at_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchStatus {
    pub initialized: bool,
    pub indexing: bool,
    pub watching: bool,
    pub roots: Vec<String>,
    /// Filename-index sources (the File Search tab) — distinct from `roots`.
    pub filename_roots: Vec<String>,
    pub include_hidden: bool,
    // Wave 7.7 (2026-05-28): `follow_symlinks` was removed. On Windows,
    // "follow symlinks" includes following NTFS reparse points / junctions
    // (pnpm content-addressable store, OneDrive placeholders, Windows
    // backward-compat junctions like Application Data → Local). Following
    // them inflated the index ~3× (the same files visible via multiple
    // paths) and triggered cloud-sync downloads. Walker is now permanently
    // `follow_links(false)`. Users who need a symlinked target indexed
    // can add the TARGET folder directly as a root.
    pub index_content: bool,
    pub max_content_kb: u32,
    pub commit_every: usize,
    pub watcher_enabled: bool,
    pub watcher_paused: bool,
    pub exclude_folders: Vec<String>,
    pub exclude_extensions: Vec<String>,
    // Wave 6 (2026-05-28): removed `mft_fast_index` / `allow_root_drive_watcher`
    // / `elevated` along with the MFT engine. Frontend types mirror.
    /// Latest message from the standalone filename indexer. `None` until that
    /// worker has run at least once.
    pub filename_index_message: Option<String>,
    /// Document count of the filename index and when it was last built/updated.
    /// Shown on the File Search tab the way `indexed_files` / `last_indexed_at_ms`
    /// are shown for the content index, so both engines report progress alike.
    pub filename_indexed_files: u64,
    pub filename_last_indexed_at_ms: Option<u128>,
    /// On-disk size, in bytes, of the active filename index directory — shown
    /// on the File Search Index settings page so disk usage is never a mystery.
    pub filename_index_bytes: u64,
    /// A filename-index build (My engine or MFT) is in progress. Distinct from
    /// `indexing` (the content build) so the File Search tab shows live build
    /// progress without blocking content search, and vice versa.
    pub filename_indexing: bool,
    /// The filename index is being live-watched right now (the walker
    /// notify-watch loop or the MFT USN tailer) — the File Search tab's own
    /// watcher state, distinct from `watching` (the content-index watcher).
    pub filename_watching: bool,
    pub rebuild_schedule: FileSearchRebuildSchedule,
    pub indexed_files: u64,
    /// Live "entries scanned" / "files skipped" counters for the in-flight
    /// build, mirrored from the worker status file so `get_file_search_status`
    /// is authoritative for the progress panel. A view that mounts mid-build
    /// (e.g. the first-run auto-index, which starts before any progress listener
    /// exists) reads real numbers here instead of 0/0/0, even when it missed the
    /// transient `file-search-index-progress` events.
    pub scanned_entries: u64,
    pub skipped_files: u64,
    pub updated_files: u64,
    pub deleted_files: u64,
    pub last_indexed_at_ms: Option<u128>,
    /// On-disk size, in bytes, of the active content index directory.
    pub content_index_bytes: u64,
    pub last_error: Option<String>,
    pub diagnostics: FileSearchDiagnostics,
}

impl Default for FileSearchStatus {
    fn default() -> Self {
        Self {
            initialized: false,
            indexing: false,
            watching: false,
            roots: Vec::new(),
            filename_roots: Vec::new(),
            include_hidden: false,
            index_content: true,
            max_content_kb: DEFAULT_MAX_CONTENT_KB,
            commit_every: DEFAULT_COMMIT_EVERY,
            watcher_enabled: true,
            watcher_paused: false,
            exclude_folders: default_exclude_folders(),
            exclude_extensions: default_exclude_extensions(),
            filename_index_message: None,
            filename_indexed_files: 0,
            filename_last_indexed_at_ms: None,
            filename_index_bytes: 0,
            filename_indexing: false,
            filename_watching: false,
            rebuild_schedule: FileSearchRebuildSchedule::default(),
            indexed_files: 0,
            scanned_entries: 0,
            skipped_files: 0,
            updated_files: 0,
            deleted_files: 0,
            last_indexed_at_ms: None,
            content_index_bytes: 0,
            last_error: None,
            diagnostics: FileSearchDiagnostics::default(),
        }
    }
}

#[derive(Clone, Copy)]
struct SearchFields {
    path: Field,
    path_exact: Field,
    file_name: Field,
    entry_type: Option<Field>,
    extension: Field,
    content: Field,
    /// Multi-valued tag field for findings from the `sensitive_scan` module.
    /// Optional so indexes built before this field existed still load cleanly.
    sensitive_kinds: Option<Field>,
    size: Field,
    modified_ms: Field,
    created_ms: Option<Field>,
}

/// The name/path/metadata field subset that `build_result_item` reads — never
/// `content`. Both the combined index (`SearchFields`) and the standalone
/// filename index (`FilenameIndexFields`) project into this, so one ranking
/// path serves both (Phase 3, step 7c — Option B query routing).
#[derive(Clone, Copy)]
struct ResultDocFields {
    path: Field,
    file_name: Field,
    extension: Field,
    size: Field,
    modified_ms: Field,
    entry_type: Option<Field>,
    created_ms: Option<Field>,
    sensitive_kinds: Option<Field>,
}

impl SearchFields {
    fn result_doc_fields(&self) -> ResultDocFields {
        ResultDocFields {
            path: self.path,
            file_name: self.file_name,
            extension: self.extension,
            size: self.size,
            modified_ms: self.modified_ms,
            entry_type: self.entry_type,
            created_ms: self.created_ms,
            sensitive_kinds: self.sensitive_kinds,
        }
    }
}

impl FilenameIndexFields {
    /// The filename index has no content scan, so `sensitive_kinds` is `None`;
    /// its `entry_type` / `created_ms` columns are always present, hence `Some`.
    fn result_doc_fields(&self) -> ResultDocFields {
        ResultDocFields {
            path: self.path,
            file_name: self.file_name,
            extension: self.extension,
            size: self.size,
            modified_ms: self.modified_ms,
            entry_type: Some(self.entry_type),
            created_ms: Some(self.created_ms),
            sensitive_kinds: None,
        }
    }
}

/// An opened filename index — the Phase 2 names-only index. Held as a unit so
/// it is all-or-nothing: either every handle is present or the whole thing is
/// `None`. Queried by `search_filename_index` (step 7c — Option B routing).
struct FilenameIndexHandle {
    index: Index,
    reader: IndexReader,
    fields: FilenameIndexFields,
}

struct SearchEngine {
    index: Index,
    reader: IndexReader,
    writer: Option<IndexWriter>,
    watcher: Option<RecommendedWatcher>,
    fields: SearchFields,
    options: FileSearchIndexOptions,
}

#[derive(Debug, Clone, Default)]
struct NaturalQueryPlan {
    query_text: String,
    query_keywords: Vec<String>,
    extension_filters: Vec<String>,
    entry_type_filter: Option<String>,
    date_filter: Option<DateFilter>,
    size_filter: Option<SizeFilter>,
    exact_phrases: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum DateFieldIntent {
    Modified,
    Created,
}

#[derive(Debug, Clone)]
struct DateFilter {
    field: DateFieldIntent,
    start_ms: Option<u64>,
    end_ms: Option<u64>,
    label: String,
}

#[derive(Debug, Clone)]
enum SizeFilter {
    Empty,
    Range {
        min_bytes: Option<u64>,
        max_bytes: Option<u64>,
        label: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredSearchConfig {
    options: FileSearchIndexOptions,
    indexed_files: u64,
    last_indexed_at_ms: Option<u128>,
    #[serde(default)]
    rebuild_schedule: FileSearchRebuildSchedule,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexWorkerStatusFile {
    running: bool,
    finished: bool,
    success: bool,
    canceled: bool,
    indexed_files: u64,
    scanned_entries: u64,
    skipped_files: u64,
    message: String,
    updated_at_ms: u128,
    error: Option<String>,
    // Indexing Phase 2 / Task 4.1 (2026-06-18): the run was interrupted (a
    // low-memory pool death or a user cancel) but its partial index was
    // committed and promoted to active — it is a valid, RESUMABLE index, not a
    // failure. `success=true, partial=true`. Distinct from a full success
    // (`success=true, partial=false`) and from a failure (`success=false`).
    #[serde(default)]
    partial: bool,
}

impl IndexWorkerStatusFile {
    fn progress(
        indexed_files: u64,
        scanned_entries: u64,
        skipped_files: u64,
        message: String,
    ) -> Self {
        Self {
            running: true,
            finished: false,
            success: false,
            canceled: false,
            indexed_files,
            scanned_entries,
            skipped_files,
            message,
            updated_at_ms: unix_now_ms(),
            error: None,
            partial: false,
        }
    }

    fn finished(
        success: bool,
        canceled: bool,
        indexed_files: u64,
        scanned_entries: u64,
        skipped_files: u64,
        message: String,
        error: Option<String>,
    ) -> Self {
        Self {
            running: false,
            finished: true,
            success,
            canceled,
            indexed_files,
            scanned_entries,
            skipped_files,
            message,
            updated_at_ms: unix_now_ms(),
            error,
            partial: false,
        }
    }

    /// Mark a finished status as a promoted *partial* index (Task 4.1).
    fn as_partial(mut self) -> Self {
        self.partial = true;
        self
    }
}

#[derive(Debug, Clone)]
struct LaunchTargetRecord {
    id: String,
    name: String,
    path: String,
    kind: String,
    source: String,
    normalized_name: String,
    normalized_path: String,
}

#[derive(Debug, Clone)]
struct LaunchTargetCache {
    built_at_ms: u128,
    records: Vec<LaunchTargetRecord>,
}

fn default_exclude_folders() -> Vec<String> {
    DEFAULT_EXCLUDE_FOLDERS
        .iter()
        .map(|value| (*value).to_string())
        .collect()
}

fn default_exclude_extensions() -> Vec<String> {
    DEFAULT_EXCLUDE_EXTENSIONS
        .iter()
        .map(|value| (*value).to_string())
        .collect()
}

fn default_index_performance_mode() -> String {
    "balanced".to_string()
}

fn default_watcher_enabled() -> bool {
    true
}

fn normalize_index_performance_mode(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "fast" => "fast".to_string(),
        "quiet" => "quiet".to_string(),
        _ => "balanced".to_string(),
    }
}

/// Whether the machine is currently running on battery power. Used to make
/// the bulk indexer gentler (fewer threads, a CPU yield) so a background
/// build doesn't drain a laptop. Treats "unknown" as AC so a desktop with no
/// battery is never throttled.
#[cfg(windows)]
fn is_on_battery() -> bool {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: GetSystemPowerStatus fills the caller-owned struct in place.
    let ok = unsafe { GetSystemPowerStatus(&mut status) }.is_ok();
    // ACLineStatus: 0 = on battery, 1 = on AC, 255 = unknown.
    ok && status.ACLineStatus == 0
}

#[cfg(not(windows))]
fn is_on_battery() -> bool {
    false
}

/// How much RAM (in bytes) is currently available (free + reclaimable) on the
/// machine. Used to scale the extractor pool so we never commit more child
/// processes than the hardware can sustain.
///
/// Reads `MEMORYSTATUSEX.ullAvailPhys` via `GlobalMemoryStatusEx` — the same
/// Win32 structure used by Task Manager's "Available" column. On non-Windows or
/// on API failure we return a conservative 4 GB so the pool keeps its current
/// default behaviour.
#[cfg(windows)]
fn available_ram_bytes() -> u64 {
    use std::mem::size_of;
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut ms = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `GlobalMemoryStatusEx` writes into a caller-owned struct.
    if unsafe { GlobalMemoryStatusEx(&mut ms) }.is_ok() {
        ms.ullAvailPhys
    } else {
        4 * 1024 * 1024 * 1024 // conservative fallback
    }
}

#[cfg(not(windows))]
fn available_ram_bytes() -> u64 {
    4 * 1024 * 1024 * 1024
}

/// Clamp the CPU-derived extractor child count and memory cap to what the
/// hardware can actually sustain. Called at content-index-build time so a
/// low-RAM machine never spawns N × 512 MB children and immediately OOMs.
///
/// Returns `(child_count, memory_cap_bytes_per_child)`.
///
/// Profile tiers (keyed on *available* RAM, not total):
/// - **Lite** (< 1.5 GB free): 1 child, 256 MB cap — keeps one extractor
///   alive while leaving headroom for the OS, Tantivy writer, and UI.
/// - **Standard** (1.5–3 GB free): 2 children, 384 MB cap.
/// - **Max** (> 3 GB free): no clamping beyond the CPU-derived count and
///   the existing 512 MB cap (current behaviour).
///
/// Note: these thresholds are *available* (free) RAM, not total installed RAM.
/// A 4 GB machine with 3 GB in use reads as "Lite." Optimizations ship
/// default-off on real hardware (Golden Rule #2), so the caps are conservative.
fn clamp_pool_for_available_ram(cpu_child_count: usize) -> (usize, usize) {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    let available = available_ram_bytes();
    if available < (3 * GB) / 2 {
        // Lite: < 1.5 GB free
        (1, 256 * MB as usize)
    } else if available < 3 * GB {
        // Standard: 1.5–3 GB free
        (cpu_child_count.min(2), 384 * MB as usize)
    } else {
        // Max: > 3 GB free — keep CPU-derived count and existing cap
        (cpu_child_count, EXTRACTOR_CHILD_MEMORY_CAP_BYTES)
    }
}

// Wave 6 (2026-05-28): the MFT/USN fast-path was removed wholesale —
// see UxAudit.md TOP 5 wave 6. The deterministic walker is fast enough
// (~20s for 500K files on consumer SSDs / mixed HDDs) that the MFT
// path's marginal speed win didn't justify its admin requirement,
// Win32 unsafe surface, or Mac-port portability cost. The original
// MFT/USN code lives in `search.rs.backup-pre-mft-removal` if anyone
// needs it back; the active codebase is walker-only.

/// Delete a single document from the filename index by its exact path.
/// Used by the notify watcher (all platforms) when a file disappears on
/// disk — the watcher fires a remove event, we drop the doc, the index
/// stays consistent without a full rebuild. Originally lived inside the
/// MFT block but is engine-agnostic; preserved here as the walker still
/// needs it.
fn delete_filename_doc(writer: &IndexWriter, fields: &FilenameIndexFields, path: &str) {
    let _ = writer.delete_term(tantivy::Term::from_field_text(fields.path_exact, path));
}

/// Build the filename index. After the bulk build the worker stays alive
/// as the notify watcher (step 9e), keeping the index live without a
/// rescan.
fn build_filename_index_best_effort(
    state_dir: &Path,
    staging_dir: &Path,
    options: &FileSearchIndexOptions,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    build_filename_index_in_worker(state_dir, staging_dir, options, status_path, cancel_path)?;
    watch_filename_index(state_dir, options, status_path, cancel_path)
}

/// Re-index one path into the filename index: drop any stale doc for the exact
/// path, then add a fresh 9-field doc with current metadata. The notify-watcher
/// twin of the content index's `upsert_path_document` — no content extraction,
/// so it is cheap enough to run per filesystem event.
fn upsert_filename_document(
    path: &Path,
    writer: &IndexWriter,
    fields: &FilenameIndexFields,
) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| format!("Cannot stat path: {error}"))?;
    let is_file = metadata.is_file();
    let is_dir = metadata.is_dir();
    if !is_file && !is_dir {
        return Ok(());
    }

    let file_path = path.to_string_lossy().to_string();
    let extension = if is_file {
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_lowercase()
    } else {
        String::new()
    };
    // `.ki` notes are found + shown by their human title (frontmatter / first
    // heading), not the on-disk slug like `untitled-3.ki` the user never sees.
    let file_name = if is_file && is_ki_path(path) {
        note_display_title(path)
    } else {
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string()
    };
    let parent_path = path
        .parent()
        .map(|parent| parent.to_string_lossy().to_string())
        .unwrap_or_default();
    let entry_type = if is_dir {
        ENTRY_TYPE_FOLDER
    } else {
        ENTRY_TYPE_FILE
    };
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0);
    let created_ms = metadata
        .created()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_millis() as u64)
        .unwrap_or(modified_ms);

    delete_filename_doc(writer, fields, &file_path);
    let document = doc!(
        fields.path => file_path.clone(),
        fields.path_exact => file_path,
        fields.file_name => file_name,
        fields.parent_path => parent_path,
        fields.extension => extension,
        fields.entry_type => entry_type,
        fields.size => metadata.len(),
        fields.modified_ms => modified_ms,
        fields.created_ms => created_ms,
    );
    writer
        .add_document(document)
        .map(|_| ())
        .map_err(|error| format!("Cannot index filename update: {error}"))
}

/// Fold one notify event into filename-index edits. A removed (or now-excluded)
/// path drops its doc; a created/modified path is re-`upsert`ed. Returns the
/// (updated, deleted) doc counts. The filename twin of `apply_watch_event_to_writer`.
fn apply_filename_watch_event(
    event: &Event,
    writer: &IndexWriter,
    fields: &FilenameIndexFields,
    options: &FileSearchIndexOptions,
) -> (u64, u64) {
    let mut updated = 0_u64;
    let mut deleted = 0_u64;
    match &event.kind {
        EventKind::Remove(_) => {
            for path in &event.paths {
                // A folder rename can race a Remove; only delete what is truly
                // gone (not what merely can't be checked, e.g. offline).
                if !is_confirmed_gone(path) {
                    continue;
                }
                delete_filename_doc(writer, fields, &path.to_string_lossy());
                deleted += 1;
            }
        }
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any | EventKind::Other => {
            for path in &event.paths {
                match path.try_exists() {
                    Ok(true) => {}
                    Ok(false) => {
                        delete_filename_doc(writer, fields, &path.to_string_lossy());
                        deleted += 1;
                        continue;
                    }
                    Err(_) => continue,
                }
                if !path.is_file() && !path.is_dir() {
                    continue;
                }
                if !should_include_path(path, options) {
                    delete_filename_doc(writer, fields, &path.to_string_lossy());
                    deleted += 1;
                    continue;
                }
                if upsert_filename_document(path, writer, fields).is_ok() {
                    updated += 1;
                }
            }
        }
        _ => {}
    }
    (updated, deleted)
}

/// Apply one debounced batch of notify events to the active filename index.
/// Opens a single short-lived writer for the whole batch — the filename twin of
/// `apply_watch_batch_to_index`.
fn apply_filename_watch_batch(
    active_dir: &Path,
    options: &FileSearchIndexOptions,
    pending: Vec<notify::Result<Event>>,
) -> Result<(u64, u64), String> {
    let events: Vec<Event> = pending
        .into_iter()
        .filter_map(|result| result.ok())
        .filter(|event| !matches!(event.kind, EventKind::Access(_)))
        .collect();
    if events.is_empty() {
        return Ok((0, 0));
    }

    let directory = MmapDirectory::open(active_dir)
        .map_err(|error| format!("Cannot open filename index for watching: {error}"))?;
    let index = Index::open(directory)
        .map_err(|error| format!("Cannot open filename index for watching: {error}"))?;
    let schema = index.schema();
    let fields = extract_filename_index_fields(&schema)?;
    let mut writer = index
        .writer_with_num_threads(1, WRITER_MEMORY_BUDGET_BYTES_IDLE)
        .map_err(|error| format!("Cannot open filename watcher writer: {error}"))?;

    let mut updated = 0_u64;
    let mut deleted = 0_u64;
    for event in &events {
        let (event_updated, event_deleted) =
            apply_filename_watch_event(event, &writer, &fields, options);
        updated += event_updated;
        deleted += event_deleted;
    }

    if updated == 0 && deleted == 0 {
        return Ok((0, 0));
    }
    writer
        .commit()
        .map_err(|error| format!("Cannot commit filename watcher update: {error}"))?;
    writer
        .wait_merging_threads()
        .map_err(|error| format!("Cannot finish filename watcher merge: {error}"))?;
    Ok((updated, deleted))
}

/// Heal the filename index against the live filesystem: walk every configured
/// root, re-`upsert` files whose mtime/size drifted (or that are missing), and
/// delete docs whose path is gone from disk. This is what catches events the
/// notify watcher cannot — those dropped while the machine slept, and the
/// uncascaded descendants of a renamed folder (notify reports the folder, not
/// its children). The filename twin of `reconcile_watched_index`.
/// A walker over `root` for indexing. `honour_ignore_files`: skip what
/// `.gitignore` / `.ignore` files exclude — right for the content index
/// (no point reading build output), wrong for the name index, which should
/// find every name. Hidden entries follow the setting. Never follows links
/// (see FileSearchStatus). `standard_filters` must come first: it also turns
/// hidden-skipping on, which used to override the setting.
fn index_walker(root: &str, options: &FileSearchIndexOptions, honour_ignore_files: bool) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder.standard_filters(honour_ignore_files);
    builder.hidden(!options.include_hidden);
    builder.follow_links(false);
    builder
}

/// The path is known not to exist (an error checking — offline share,
/// unplugged drive, access denied — is not "gone").
fn is_confirmed_gone(path: &Path) -> bool {
    matches!(path.try_exists(), Ok(false))
}

/// Roots that can't be confirmed present right now (unplugged drive, offline
/// share), lower-cased without a trailing separator. Reconcile leaves what is
/// indexed under them alone instead of treating all of it as deleted.
fn offline_roots(roots: &[String]) -> Vec<String> {
    roots
        .iter()
        .filter(|root| !matches!(Path::new(String::as_str(root)).try_exists(), Ok(true)))
        .map(|root| root.trim_end_matches(['\\', '/']).to_lowercase())
        .collect()
}

/// Whether lower-cased `path_lower` is `root_lower` (from `offline_roots`) or
/// inside it.
fn is_under_root(path_lower: &str, root_lower: &str) -> bool {
    path_lower
        .strip_prefix(root_lower)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['\\', '/']))
}

fn reconcile_filename_index(
    active_dir: &Path,
    options: &FileSearchIndexOptions,
) -> Result<(u64, u64), String> {
    use notify::event::{ModifyKind, RemoveKind};

    let Ok(directory) = MmapDirectory::open(active_dir) else {
        return Ok((0, 0));
    };
    let Ok(index) = Index::open(directory) else {
        return Ok((0, 0));
    };
    let schema = index.schema();
    let Ok(fields) = extract_filename_index_fields(&schema) else {
        return Ok((0, 0));
    };
    let Ok(reader): Result<IndexReader, _> = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
    else {
        return Ok((0, 0));
    };
    let searcher = reader.searcher();

    // Snapshot every alive doc: lowercase path → (case-preserved path, mtime, size).
    // Lowercase keys because Windows paths are case-insensitive; the real path is
    // kept so deletions delete the exact stored term.
    let mut indexed: HashMap<String, (String, u64, u64)> = HashMap::with_capacity(8192);
    for segment_reader in searcher.segment_readers() {
        let Ok(store_reader) = segment_reader.get_store_reader(0) else {
            continue;
        };
        let alive_bitset = segment_reader.alive_bitset();
        for doc_id in 0..segment_reader.max_doc() {
            if let Some(bitset) = alive_bitset {
                if !bitset.is_alive(doc_id) {
                    continue;
                }
            }
            let Ok(doc): Result<TantivyDocument, _> = store_reader.get(doc_id) else {
                continue;
            };
            let Some(path) = doc_text(&doc, fields.path) else {
                continue;
            };
            let modified_ms = doc_u64(&doc, fields.modified_ms).unwrap_or(0);
            let size = doc_u64(&doc, fields.size).unwrap_or(0);
            indexed.insert(path.to_lowercase(), (path, modified_ms, size));
        }
    }

    let mut seen: HashSet<String> = HashSet::with_capacity(indexed.len());
    let mut events: Vec<notify::Result<Event>> = Vec::new();
    let offline = offline_roots(&options.roots);

    for root in &options.roots {
        if offline.contains(&root.trim_end_matches(['\\', '/']).to_lowercase()) {
            continue;
        }
        let builder = index_walker(root, options, false);
        for entry in builder.build().flatten() {
            let path = entry.path();
            if !should_include_path(path, options) {
                continue;
            }
            let file_type = entry.file_type();
            let is_file = file_type.map(|value| value.is_file()).unwrap_or(false);
            let is_dir = file_type.map(|value| value.is_dir()).unwrap_or(false);
            if !is_file && !is_dir {
                continue;
            }
            let path_lower = path.to_string_lossy().to_lowercase();
            seen.insert(path_lower.clone());
            // Folders need no mtime check (a folder's mtime is noisy and the
            // doc carries no content); a file is re-indexed when mtime or size
            // drifted from the stored doc, or when it is absent entirely.
            let needs_update = match indexed.get(&path_lower) {
                None => true,
                Some(_) if is_dir => false,
                Some((_, modified_ms, size)) => {
                    let Ok(meta) = fs::metadata(path) else {
                        continue;
                    };
                    let disk_modified = meta
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .map(|value| value.as_millis() as u64)
                        .unwrap_or(0);
                    disk_modified != *modified_ms || meta.len() != *size
                }
            };
            if needs_update {
                events.push(Ok(
                    Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path.to_path_buf())
                ));
            }
        }
    }

    // Docs whose file is gone from disk → deletions the watcher missed. Delete
    // by the case-preserved stored path so the exact `path_exact` term matches.
    for (path_lower, (real_path, _, _)) in &indexed {
        if seen.contains(path_lower) || offline.iter().any(|root| is_under_root(path_lower, root)) {
            continue;
        }
        events.push(Ok(
            Event::new(EventKind::Remove(RemoveKind::Any)).add_path(PathBuf::from(real_path))
        ));
    }

    if events.is_empty() {
        return Ok((0, 0));
    }
    // Release the read-side mmap before the apply opens a writer.
    drop(searcher);
    drop(reader);
    drop(index);
    apply_filename_watch_batch(active_dir, options, events)
}

/// Keep the walker-built filename index live (Phase 3, step 9e) — the notify
/// fallback for volumes the USN tailer cannot serve (non-NTFS, or NTFS without
/// elevation). Runs in-process after the walker build, so the filename worker
/// stays alive as the watcher exactly as the MFT path stays alive as the USN
/// tailer; the scheduled rebuild is suppressed because the watcher re-stamps
/// the schema sentinel, and a periodic reconcile heals what notify drops
/// (sleep-time events, uncascaded folder-rename descendants).
///
/// Returns `Ok(())` immediately — letting the worker exit and the scheduled
/// rebuild remain the backstop — when live watching is disabled or no root is a
/// watchable explicit folder (drive roots are not notify-watched).
fn watch_filename_index(
    state_dir: &Path,
    options: &FileSearchIndexOptions,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    if cancel_path.exists() || !options.watcher_enabled {
        return Ok(());
    }
    let watch_roots = watchable_index_roots(options);
    if watch_roots.is_empty() {
        // Write a diagnostic so the UI status line explains why watching is
        // not active, rather than leaving "Rebuild to activate" with no context.
        let _ = write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::progress(
                0,
                0,
                0,
                "Live watching is off: every source is a whole drive or a system folder. \
                Add specific folders to keep them up to date as files change."
                    .to_string(),
            ),
        );
        return Ok(());
    }
    let active_dir = active_filename_index_dir_for_state(state_dir);
    if !active_dir.exists() {
        return Ok(());
    }

    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        // Drop noise events at the source — reads and metadata-only changes
        // never alter a name or path, so they never change the filename index.
        if let Ok(ref event) = result {
            use notify::event::ModifyKind;
            match &event.kind {
                EventKind::Access(_) => return,
                EventKind::Modify(ModifyKind::Metadata(_)) => return,
                EventKind::Modify(ModifyKind::Other) => return,
                EventKind::Any | EventKind::Other => return,
                _ => {}
            }
        }
        let _ = tx.send(result);
    })
    .map_err(|error| format!("Cannot start filename index watcher: {error}"))?;

    let mut watched_roots = 0_u64;
    for root in &watch_roots {
        if watcher
            .watch(Path::new(root), RecursiveMode::Recursive)
            .is_ok()
        {
            watched_roots += 1;
        }
    }
    if watched_roots == 0 {
        return Ok(());
    }

    let strategy = WatcherStrategy {
        name: "filename watcher",
        debounce_ms: 2_000,
        max_batch: 1_024,
    };
    // Minimum gap between batch applies — events accumulate in the channel
    // during the wait so each apply opens the index once for many changes.
    const MIN_APPLY_INTERVAL_MS: u128 = 5_000;
    // Periodic full reconciliation, plus an immediate one when the loop gap
    // implies the process was suspended (sleep/hibernate dropped events).
    const RECONCILE_INTERVAL_MS: u128 = 6 * 60 * 60 * 1_000;
    const SLEEP_GAP_THRESHOLD_MS: u128 = 2 * 60 * 1_000;
    let mut last_apply_ms: u128 = 0;
    let mut last_reconcile_ms: u128 = unix_now_ms();
    let mut last_loop_iter_ms: u128 = unix_now_ms();
    let mut updated_total = 0_u64;
    let mut deleted_total = 0_u64;

    // No status is written until the first applied change: the bulk build's
    // "Indexed N filenames" success status is left in place so the parent
    // monitor can re-open the engine's filename handle before it is overwritten.
    loop {
        if cancel_path.exists() {
            return Ok(());
        }

        let now_ms = unix_now_ms();
        let loop_gap_ms = now_ms.saturating_sub(last_loop_iter_ms);
        last_loop_iter_ms = now_ms;
        let resumed_from_suspend = loop_gap_ms > SLEEP_GAP_THRESHOLD_MS;
        if resumed_from_suspend
            || now_ms.saturating_sub(last_reconcile_ms) >= RECONCILE_INTERVAL_MS
        {
            last_reconcile_ms = now_ms;
            if let Ok((reconciled_updates, reconciled_deletes)) =
                reconcile_filename_index(&active_dir, options)
            {
                updated_total += reconciled_updates;
                deleted_total += reconciled_deletes;
            }
            // Re-stamp the sentinel so the scheduler keeps treating this
            // watcher-owned index as current (mirrors the USN tailer).
            let _ = write_filename_schema_version_sentinel(state_dir);
            let _ = write_index_worker_status(
                status_path,
                &IndexWorkerStatusFile::progress(
                    updated_total,
                    updated_total,
                    deleted_total,
                    format!(
                        "Filename index live — {updated_total} updated, {deleted_total} removed"
                    ),
                ),
            );
            last_apply_ms = unix_now_ms();
            continue;
        }

        // Rate limit: enforce a minimum gap between applies. Events queue in
        // the channel during this sleep and are then processed together.
        if last_apply_ms > 0 {
            let elapsed = unix_now_ms().saturating_sub(last_apply_ms);
            if elapsed < MIN_APPLY_INTERVAL_MS {
                thread::sleep(Duration::from_millis(500));
                continue;
            }
        }

        let mut pending = Vec::new();
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(value) => pending.push(value),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            // The channel only disconnects if the watcher is dropped — which
            // does not happen here. Treat it as a clean stop: the index is
            // built and the scheduled rebuild remains the backstop.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        collect_watcher_events(&rx, &mut pending, strategy, cancel_path);

        // A failed apply is not a build failure — the index is already built
        // and valid. Stop watching cleanly and let the scheduled rebuild
        // resume as the backstop rather than marking the whole build failed.
        let (updated_batch, deleted_batch) =
            match apply_filename_watch_batch(&active_dir, options, pending) {
                Ok(counts) => counts,
                Err(_) => return Ok(()),
            };
        last_apply_ms = unix_now_ms();
        if updated_batch == 0 && deleted_batch == 0 {
            continue;
        }

        updated_total += updated_batch;
        deleted_total += deleted_batch;
        // Keep the schema sentinel fresh so the scheduler does not consider the
        // live index stale and try to rebuild over the watcher.
        let _ = write_filename_schema_version_sentinel(state_dir);
        let _ = write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::progress(
                updated_total,
                updated_total,
                deleted_total,
                format!("Filename index live — {updated_total} updated, {deleted_total} removed"),
            ),
        );
    }
}

/// Per-file throttle sleep for the bulk indexer. `fast` runs flat-out;
/// `balanced`/`quiet` periodically yield the CPU so the rest of the system
/// stays responsive. On battery even `fast` is given a yield so a background
/// index doesn't drain a laptop.
fn index_throttle_sleep_ms(performance_mode: &str, on_battery: bool) -> u64 {
    let base = match normalize_index_performance_mode(performance_mode).as_str() {
        "fast" => 0,
        "quiet" => INDEX_THROTTLE_SLEEP_MS_QUIET,
        _ => INDEX_THROTTLE_SLEEP_MS_BALANCED,
    };
    if on_battery {
        base.max(INDEX_THROTTLE_SLEEP_MS_BALANCED)
    } else {
        base
    }
}

#[allow(dead_code)]
fn reload_search_reader_if_due(reader: &IndexReader) {
    // No longer called on the hot search path because OnCommitWithDelay
    // auto-reloads in a background thread. Kept available for explicit
    // post-rebuild scenarios where immediate visibility is required.
    let now_ms = unix_now_ms();
    let Ok(mut last_reload_ms) = LAST_SEARCH_READER_RELOAD_MS.lock() else {
        return;
    };
    if now_ms.saturating_sub(*last_reload_ms) < SEARCH_READER_RELOAD_MIN_INTERVAL_MS {
        return;
    }
    if reader.reload().is_ok() {
        *last_reload_ms = now_ms;
    }
}

#[derive(Clone, Copy)]
struct WatcherStrategy {
    name: &'static str,
    debounce_ms: u64,
    max_batch: usize,
}

fn watcher_strategy_for_index_size(indexed_files: u64) -> WatcherStrategy {
    if indexed_files >= HUGE_INDEX_WATCHER_THRESHOLD {
        WatcherStrategy {
            name: "conservative batched watcher",
            debounce_ms: 5_000,
            max_batch: 2_048,
        }
    } else if indexed_files >= LARGE_INDEX_WATCHER_THRESHOLD {
        WatcherStrategy {
            name: "large-index batched watcher",
            debounce_ms: 2_000,
            max_batch: 1_024,
        }
    } else {
        WatcherStrategy {
            name: "responsive watcher",
            debounce_ms: 2_000,
            max_batch: 256,
        }
    }
}

fn watchable_index_roots(options: &FileSearchIndexOptions) -> Vec<String> {
    options
        .roots
        .iter()
        .filter(|root| {
            let path = Path::new(root);
            path.exists()
                && should_include_path(path, options)
                && !is_system_protected_path(path)
        })
        .cloned()
        .collect()
}

/// Wave 6 (2026-05-28): hard-block on Windows system folders.
///
/// Replaces the old `is_broad_or_system_watch_root` (which only blocked these
/// at the watcher root level when the opt-in checkbox was off). Now the gate
/// is unconditional AND applies to both the walker (via `should_include_path`)
/// and the watcher root selection — so a user adding `C:\` as a root gets
/// everything indexed EXCEPT these four system trees.
///
/// Why these four specifically (see UxAudit.md Wave 6 rationale):
///   - `C:\Windows` — OS binaries, kernel files, no user content
///   - `C:\Program Files` — installed-app binaries (apps themselves are
///     surfaced via the launcher cache, not file-search)
///   - `C:\Program Files (x86)` — same, for 32-bit installs
///   - `C:\ProgramData` — machine-wide shared app data + caches
///
/// `C:\Users` deliberately STAYS in scope — it's where all real user content
/// lives. Plain drive roots (`C:\`) are also allowed now (we hard-block
/// the system trees below them instead).
///
/// Matching is "is OR is under": `C:\Windows\System32\foo.dll` matches, so
/// `WalkDir`'s `filter_entry` will skip descending into these trees during
/// indexing. The notify watcher's root filter uses the same predicate.
fn is_system_protected_path(path: &Path) -> bool {
    let normalized = normalize_path_for_exclusion(path);
    if normalized.is_empty() {
        return true;
    }

    #[cfg(target_os = "windows")]
    {
        let trimmed = normalized.trim_end_matches('/');
        let system_drive = windows_system_drive_token();
        let protected_roots = [
            format!("{system_drive}/windows"),
            format!("{system_drive}/program files"),
            format!("{system_drive}/program files (x86)"),
            format!("{system_drive}/programdata"),
        ];
        for blocked in protected_roots.iter() {
            // Match exact OR any path under that root.
            if trimmed == blocked || trimmed.starts_with(&format!("{blocked}/")) {
                return true;
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        // macOS / Linux equivalents — kept conservative.
        if normalized == "/"
            || normalized == "/tmp"
            || normalized == "/var"
            || normalized.starts_with("/System/")
            || normalized.starts_with("/private/")
        {
            return true;
        }
    }

    false
}

#[cfg(target_os = "windows")]
fn windows_system_drive_token() -> String {
    env::var("SystemDrive")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "C:".to_string())
        .replace('\\', "/")
        .to_lowercase()
        .trim_end_matches('/')
        .to_string()
}

fn default_rebuild_interval_hours() -> u32 {
    24
}

fn normalize_rebuild_schedule(
    mut schedule: FileSearchRebuildSchedule,
) -> FileSearchRebuildSchedule {
    schedule.interval_hours = schedule.interval_hours.clamp(1, 720);
    schedule
}

fn unix_now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0)
}

pub fn cancel_file_search_index_build() -> Result<(), String> {
    on_owner!("cancel_file_search_index_build", {});
    SEARCH_BUILD_CANCELLED.store(true, Ordering::Relaxed);
    // Cancel whichever build is running — the content worker and the filename
    // worker are separate processes, and the File Search tab's Cancel button
    // must reach the filename build too. Killing the filename worker mid-build
    // is safe: it has not swapped its staging index in yet, and the cancel only
    // shows while a build is in progress (not while the tailer/watcher runs).
    for slot in [&SEARCH_INDEX_WORKER, &FILENAME_INDEX_WORKER] {
        if let Ok(guard) = slot.lock() {
            if let Some(worker) = guard.as_ref() {
                let _ = fs::write(&worker.cancel_path, b"cancel");
                if let Ok(mut child) = worker.child.lock() {
                    // Kill the worker AND its extractor children. Terminating
                    // only the worker (Child::kill → TerminateProcess) can leave
                    // its extractor child processes running — they keep burning
                    // CPU, so "Cancel" looks like it did nothing. taskkill /T
                    // takes out the whole tree.
                    kill_process_tree(child.id());
                    let _ = child.kill();
                }
            }
        }
    }
    Ok(())
}

/// Kill a process and its ENTIRE descendant tree on Windows (`taskkill /F /T`).
/// The index workers spawn out-of-process extractor children; `Child::kill()`
/// (TerminateProcess) only takes out the one process, so descendants survive.
/// `taskkill /F /T` terminates the whole tree.
#[cfg(windows)]
fn kill_process_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(windows))]
fn kill_process_tree(_pid: u32) {}

/// Terminate EVERY running index/extractor worker process tree — the content
/// build, the filename build, and the live watcher. Called on app exit so
/// quitting never leaves orphaned indexer + extractor processes running (and
/// indexing) in the background.
pub fn stop_all_index_workers() {
    SEARCH_BUILD_CANCELLED.store(true, Ordering::Relaxed);
    for slot in [
        &SEARCH_INDEX_WORKER,
        &FILENAME_INDEX_WORKER,
        &SEARCH_WATCHER_WORKER,
    ] {
        if let Ok(mut guard) = slot.lock() {
            if let Some(worker) = guard.as_ref() {
                let _ = fs::write(&worker.cancel_path, b"cancel");
                if let Ok(mut child) = worker.child.lock() {
                    kill_process_tree(child.id());
                    let _ = child.kill();
                }
            }
            *guard = None;
        }
    }
}

pub fn stop_file_search_index_watcher(app: std::sync::Arc<dyn dash_core::Host>) -> Result<(), String> {
    on_owner!("stop_file_search_index_watcher", {});
    let state_dir = search_index_dir(&app)?;
    stop_isolated_watch_worker_for_state(&state_dir);
    if let Some(mut config) = read_search_config(&app)? {
        config.options.watcher_paused = true;
        write_search_config(&app, &config)?;
    }
    let mut guard = SEARCH_ENGINE
        .lock()
        .map_err(|_| "Search engine lock failed".to_string())?;
    if let Some(engine) = guard.as_mut() {
        engine.watcher = None;
        engine.writer = None;
        engine.options.watcher_paused = true;
    }

    update_status(|status| {
        status.watcher_paused = true;
        status.watching = false;
        status.diagnostics.watcher_worker = "stopped".to_string();
        status.diagnostics.watcher_strategy = "paused manually".to_string();
        status.diagnostics.last_worker_message = Some("Index watcher stopped".to_string());
        status.diagnostics.last_status_at_ms = Some(unix_now_ms());
    });
    Ok(())
}

pub fn get_file_search_status(app: std::sync::Arc<dyn dash_core::Host>) -> Result<FileSearchStatus, String> {
    on_owner!("get_file_search_status", {});
    // Wave 6 (2026-05-28): `status.elevated` populated removed with MFT.
    hydrate_status_from_index_worker_files(&app);
    hydrate_status_from_filename_worker_files(&app);
    hydrate_worker_process_diagnostics();
    if let Ok(state_dir) = search_index_dir(&app) {
        let content_bytes = directory_size_bytes(&active_search_index_dir_for_state(&state_dir));
        let filename_bytes =
            directory_size_bytes(&active_filename_index_dir_for_state(&state_dir));
        update_status(|status| {
            status.content_index_bytes = content_bytes;
            status.filename_index_bytes = filename_bytes;
        });
    }
    if let Ok(status) = SEARCH_STATUS.lock() {
        if status.indexing {
            return Ok(status.clone());
        }
    }

    hydrate_status_from_saved_config(&app);
    hydrate_status_from_watcher_worker_files(&app);
    if let Err(error) = ensure_engine_loaded(&app) {
        update_status(|status| {
            status.last_error = Some(error);
        });
    }
    // Authoritative content count from the live index. Covers the warm path too
    // (engine already loaded → ensure_engine_loaded returns early without
    // recomputing), so a lost/0 persisted count never shows 0 while a populated
    // index is open. Guard is dropped before update_status to avoid lock nesting.
    let live_indexed_files = SEARCH_ENGINE
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|engine| engine.reader.searcher().num_docs()));
    if let Some(live) = live_indexed_files {
        if live > 0 {
            update_status(|status| status.indexed_files = live);
        }
    }
    let status = SEARCH_STATUS
        .lock()
        .map_err(|_| "Search status lock failed".to_string())?;
    Ok(status.clone())
}

// Wave 6 (2026-05-28): an elevated relaunch command was the UAC-restart
// shim used to enable the MFT fast-path; removed wholesale with the MFT
// engine. The walker never needs admin, so no elevation path remains.

fn hydrate_worker_process_diagnostics() {
    let index_running = worker_process_running(&SEARCH_INDEX_WORKER);
    let watcher_running = worker_process_running(&SEARCH_WATCHER_WORKER);
    let filename_worker_running = worker_process_running(&FILENAME_INDEX_WORKER);
    update_status(|status| {
        if status.indexing || index_running {
            status.diagnostics.index_worker = "running".to_string();
        } else if status.diagnostics.index_worker == "starting" {
            status.diagnostics.index_worker = "stopped".to_string();
        }

        if !status.watcher_enabled || status.watcher_paused {
            status.watching = false;
            status.diagnostics.watcher_worker = "stopped".to_string();
        } else if watcher_running {
            status.watching = true;
            status.diagnostics.watcher_worker = "running".to_string();
        } else if status.diagnostics.watcher_worker == "starting" {
            status.diagnostics.watcher_worker = "stopped".to_string();
        }

        // The filename index is live-watched when its worker process is still
        // alive but past the bulk-build phase — i.e. it has entered the walker
        // notify-watch loop or the MFT USN tailer.
        status.filename_watching = filename_worker_running && !status.filename_indexing;
    });
}

fn worker_process_running(worker_slot: &LazyLock<Mutex<Option<SearchIndexWorkerProcess>>>) -> bool {
    let Ok(guard) = worker_slot.lock() else {
        return false;
    };
    let Some(worker) = guard.as_ref() else {
        return false;
    };
    worker
        .child
        .lock()
        .map(|mut child| child.try_wait().ok().flatten().is_none())
        .unwrap_or(false)
}

/// The saved index options (`None` before the first save).
pub fn get_file_search_index_options(
    app: std::sync::Arc<dyn dash_core::Host>,
) -> Result<Option<FileSearchIndexOptions>, String> {
    on_owner!("get_file_search_index_options", {});
    Ok(read_search_config(&app)?.map(|config| config.options))
}

pub fn save_file_search_index_options(
    app: std::sync::Arc<dyn dash_core::Host>,
    mut options: FileSearchIndexOptions,
) -> Result<(), String> {
    on_owner!("save_file_search_index_options", { "options": options });
    options.max_content_kb = Some(options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB));
    options.commit_every = Some(normalize_commit_every(options.commit_every));
    options.performance_mode = normalize_index_performance_mode(&options.performance_mode);
    if !options.watcher_enabled {
        options.watcher_paused = false;
    }
    options.watcher_settings_version = WATCHER_SETTINGS_VERSION;
    options.exclude_folders = normalize_exclude_folders(&options.exclude_folders);
    options.exclude_extensions = normalize_exclude_extensions(&options.exclude_extensions);

    let saved_config = read_search_config(&app)?;
    let search_coverage_changed = saved_config
        .as_ref()
        .map(|config| search_coverage_options_changed(&config.options, &options))
        .unwrap_or(false);
    let watcher_enabled_changed = saved_config
        .as_ref()
        .map(|config| config.options.watcher_enabled != options.watcher_enabled)
        .unwrap_or(false);
    let watcher_pause_changed = saved_config
        .as_ref()
        .map(|config| config.options.watcher_paused != options.watcher_paused)
        .unwrap_or(false);
    if search_coverage_changed || !options.watcher_enabled {
        let state_dir = search_index_dir(&app)?;
        stop_isolated_watch_worker_for_state(&state_dir);
    }
    let (indexed_files, last_indexed_at_ms) = saved_config
        .as_ref()
        .map(|config| (config.indexed_files, config.last_indexed_at_ms))
        .unwrap_or_else(|| {
            SEARCH_STATUS
                .lock()
                .map(|status| (status.indexed_files, status.last_indexed_at_ms))
                .unwrap_or((0, None))
        });

    write_search_config(
        &app,
        &StoredSearchConfig {
            options: options.clone(),
            indexed_files,
            last_indexed_at_ms,
            rebuild_schedule: saved_config
                .as_ref()
                .map(|config| config.rebuild_schedule.clone())
                .unwrap_or_default(),
        },
    )?;

    {
        let mut guard = SEARCH_ENGINE
            .lock()
            .map_err(|_| "Search engine lock failed".to_string())?;
        if let Some(engine) = guard.as_mut() {
            engine.options.roots = options.roots.clone();
            engine.options.include_hidden = options.include_hidden;
            engine.options.index_content = options.index_content;
            engine.options.max_content_kb = options.max_content_kb;
            engine.options.commit_every = options.commit_every;
            engine.options.performance_mode = options.performance_mode.clone();
            engine.options.watcher_enabled = options.watcher_enabled;
            engine.options.watcher_paused = options.watcher_paused;
            engine.options.exclude_folders = options.exclude_folders.clone();
            engine.options.exclude_extensions = options.exclude_extensions.clone();
        }
    }

    update_status(|status| {
        status.roots = options.roots.clone();
        status.filename_roots = options.filename_roots.clone();
        status.include_hidden = options.include_hidden;
        status.index_content = options.index_content;
        status.max_content_kb = options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB);
        status.commit_every = normalize_commit_every(options.commit_every);
        status.watcher_enabled = options.watcher_enabled;
        status.watcher_paused = options.watcher_paused;
        status.exclude_folders = options.exclude_folders.clone();
        status.exclude_extensions = options.exclude_extensions.clone();
        status.diagnostics.performance_mode = options.performance_mode.clone();
        if !options.watcher_enabled {
            status.watching = false;
            status.diagnostics.watcher_worker = "stopped".to_string();
            status.diagnostics.watcher_strategy = "disabled manually".to_string();
            status.diagnostics.last_worker_message =
                Some("Watcher disabled in search settings".to_string());
            status.diagnostics.last_status_at_ms = Some(unix_now_ms());
        } else if options.watcher_paused {
            status.watching = false;
            status.diagnostics.watcher_worker = "stopped".to_string();
            status.diagnostics.watcher_strategy = "paused manually".to_string();
            status.diagnostics.last_worker_message =
                Some("Watcher paused until the next rebuild or settings change".to_string());
            status.diagnostics.last_status_at_ms = Some(unix_now_ms());
        } else if search_coverage_changed {
            status.watching = false;
            status.diagnostics.watcher_worker = "stopped".to_string();
            status.diagnostics.watcher_strategy = "settings changed - rebuild to apply".to_string();
            status.diagnostics.last_worker_message =
                Some("Watcher stopped because search settings changed".to_string());
            status.diagnostics.last_status_at_ms = Some(unix_now_ms());
        } else if watcher_enabled_changed || watcher_pause_changed {
            // The watcher was just (re-)enabled or un-paused. Without this branch
            // the stale "disabled"/"paused" diagnostic from the previous toggle
            // lingers, so the UI keeps showing "disabled" after the user turns
            // the watcher back on. When a content index exists,
            // `start_isolated_watch_worker` below replaces this with the live
            // watcher status; for a filename-only setup the notify watcher picks
            // the setting up on the next index build.
            status.diagnostics.watcher_strategy = "enabled".to_string();
            status.diagnostics.last_worker_message =
                Some("Live watching enabled — activates on the next index build".to_string());
            status.diagnostics.last_status_at_ms = Some(unix_now_ms());
        }
    });

    if options.watcher_enabled
        && !options.watcher_paused
        && (watcher_enabled_changed || watcher_pause_changed)
        && !search_coverage_changed
    {
        let _ = start_isolated_watch_worker(&app, options.clone(), indexed_files);
    }

    Ok(())
}

pub fn save_file_search_rebuild_schedule(
    app: std::sync::Arc<dyn dash_core::Host>,
    schedule: FileSearchRebuildSchedule,
) -> Result<FileSearchRebuildSchedule, String> {
    on_owner!("save_file_search_rebuild_schedule", { "schedule": schedule });
    let schedule = normalize_rebuild_schedule(schedule);
    let mut config = read_search_config(&app)?.unwrap_or_else(|| {
        let status = SEARCH_STATUS
            .lock()
            .ok()
            .map(|value| value.clone())
            .unwrap_or_default();
        StoredSearchConfig {
            options: status_to_index_options(&status),
            indexed_files: status.indexed_files,
            last_indexed_at_ms: status.last_indexed_at_ms,
            rebuild_schedule: FileSearchRebuildSchedule::default(),
        }
    });
    config.rebuild_schedule = schedule.clone();
    write_search_config(&app, &config)?;

    update_status(|status| {
        status.rebuild_schedule = schedule.clone();
    });

    Ok(schedule)
}

pub fn refresh_launch_target_cache() -> Result<(), String> {
    let mut guard = LAUNCH_TARGET_CACHE
        .lock()
        .map_err(|_| "Launch target cache lock failed".to_string())?;
    *guard = None;
    Ok(())
}

pub async fn search_launch_targets(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: LaunchTargetSearchOptions,
) -> Result<LaunchTargetSearchResult, String> {
    let query = options.query.trim().to_string();
    // Wave G (2026-05-27): the "browse all apps" path needs more than
    // 50 to be useful — most systems have 80-200 installed targets and
    // capping at 50 would silently drop the rest. Lift the upper bound
    // for browse_all, keep the search path tight.
    let max_limit = if options.browse_all { 500 } else { 50 };
    let default_limit = if options.browse_all { 300 } else { 12 };
    let limit = options.limit.unwrap_or(default_limit).clamp(1, max_limit);
    let started_at = SystemTime::now();

    if query.is_empty() {
        // Wave G (2026-05-27): empty + browse_all → return the cache
        // straight from disk (no scoring). The frontend uses this for
        // the "Apps" chip's empty-state list. We still respect the
        // limit so the palette can't be flooded if a user happens to
        // have 5,000 launch targets.
        if options.browse_all {
            let cache = tokio::task::spawn_blocking(ensure_launch_target_cache)
                .await
                .map_err(|error| format!("Launch target worker failed: {error}"))??;
            let frecency = super::frecency::snapshot(&app);
            let total = cache.records.len();
            // Sort by frecency boost so frequently-launched apps lead,
            // then by name for a stable order on ties. Apps the user
            // has launched recently jump to the top — exactly what
            // they'd expect from "show me all my apps".
            let mut sorted: Vec<&LaunchTargetRecord> = cache.records.iter().collect();
            sorted.sort_by(|a, b| {
                let ab = frecency.boost("app", &a.path);
                let bb = frecency.boost("app", &b.path);
                bb.partial_cmp(&ab)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            let results: Vec<LaunchTargetItem> = sorted
                .into_iter()
                .take(limit)
                .map(|record| LaunchTargetItem {
                    id: record.id.clone(),
                    name: record.name.clone(),
                    path: record.path.clone(),
                    kind: record.kind.clone(),
                    source: record.source.clone(),
                    score: frecency.boost("app", &record.path),
                })
                .collect();
            return Ok(LaunchTargetSearchResult {
                query,
                total_hits: total,
                returned: results.len(),
                took_ms: started_at.elapsed().map(|d| d.as_millis()).unwrap_or(0),
                cache_built_at_ms: Some(cache.built_at_ms),
                results,
            });
        }
        return Ok(LaunchTargetSearchResult {
            query,
            total_hits: 0,
            returned: 0,
            took_ms: started_at.elapsed().map(|d| d.as_millis()).unwrap_or(0),
            cache_built_at_ms: None,
            results: Vec::new(),
        });
    }

    let query_normalized = normalize_launcher_text(&query);
    let query_tokens = query_normalized
        .split_whitespace()
        .filter(|token| token.len() >= 2 && !is_natural_stopword(token))
        .map(|token| token.to_string())
        .collect::<Vec<_>>();

    let cache = tokio::task::spawn_blocking(ensure_launch_target_cache)
        .await
        .map_err(|error| format!("Launch target worker failed: {error}"))??;

    // Load frecency snapshot once — boost is applied per-result below.
    let frecency = super::frecency::snapshot(&app);

    let mut ranked = Vec::<LaunchTargetItem>::new();
    for record in &cache.records {
        if let Some(mut score) = score_launch_target(record, &query_normalized, &query_tokens) {
            // Frecency boost: often-launched + recently-launched apps float to the top.
            score += frecency.boost("app", &record.path);
            ranked.push(LaunchTargetItem {
                id: record.id.clone(),
                name: record.name.clone(),
                path: record.path.clone(),
                kind: record.kind.clone(),
                source: record.source.clone(),
                score,
            });
        }
    }

    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });

    let total_hits = ranked.len();
    let results = ranked.into_iter().take(limit).collect::<Vec<_>>();
    let took_ms = started_at.elapsed().map(|d| d.as_millis()).unwrap_or(0);

    Ok(LaunchTargetSearchResult {
        query,
        total_hits,
        returned: results.len(),
        took_ms,
        cache_built_at_ms: Some(cache.built_at_ms),
        results,
    })
}

pub async fn launch_cached_target(app: std::sync::Arc<dyn dash_core::Host>, path: String) -> Result<(), String> {
    let requested = path.trim();
    if requested.is_empty() {
        return Err("Launch path is empty".to_string());
    }

    // Packaged (MSIX / Store) apps: the "path" is a `shell:AppsFolder\<AUMID>`
    // handle, not a file, so the filesystem checks below don't apply. The
    // trusted-cache check still does — it is the actual security boundary here,
    // and skipping it would turn this command into "launch anything".
    if crate::packaged_apps::is_packaged_launch_path(requested) {
        return launch_packaged_target(app, requested).await;
    }

    let requested_path = PathBuf::from(requested);
    if !requested_path.exists() {
        return Err(format!("Launch target not found: {requested}"));
    }

    let requested_normalized = normalize_launch_path_for_match(&requested_path);

    let cache = tokio::task::spawn_blocking(ensure_launch_target_cache)
        .await
        .map_err(|error| format!("Launch target worker failed: {error}"))??;

    let is_trusted = cache.records.iter().any(|record| {
        normalize_launch_path_for_match(Path::new(&record.path)) == requested_normalized
    });
    if !is_trusted {
        return Err("Launch blocked: target is not in trusted launcher cache".to_string());
    }

    open::that_detached(&requested_path)
        .map_err(|error| format!("Failed to launch target: {error}"))?;

    // Record the activation so future searches surface this app sooner.
    // Best-effort — if frecency recording fails (lock contention, disk full),
    // we still consider the launch successful.
    let _ = super::frecency::record_frecency_launch(
        app,
        "app".to_string(),
        requested.to_string(),
    );
    Ok(())
}

/// Launch a packaged (MSIX / Store) app by its `shell:AppsFolder\<AUMID>` handle.
///
/// Split out of [`launch_cached_target`] because a packaged app has no file to
/// stat: the `exists()` / canonicalize path-validation there is meaningless for
/// an AUMID. The **trusted-cache check is preserved verbatim** — it is the real
/// security boundary, ensuring we only ever launch something our own scan found.
///
/// Activation goes through Explorer rather than `open::that_detached`, because
/// `shell:` URIs are a shell namespace concept: handing the string to Explorer
/// is the documented, COM-free way to activate an AUMID.
async fn launch_packaged_target(app: std::sync::Arc<dyn dash_core::Host>, requested: &str) -> Result<(), String> {
    let cache = tokio::task::spawn_blocking(ensure_launch_target_cache)
        .await
        .map_err(|error| format!("Launch target worker failed: {error}"))??;

    // Exact match on the stored handle. AUMIDs are opaque and case-insensitive
    // in practice, so compare lowercased — same treatment the cache key gets.
    let requested_key = requested.to_ascii_lowercase();
    let is_trusted = cache
        .records
        .iter()
        .any(|record| record.path.to_ascii_lowercase() == requested_key);
    if !is_trusted {
        return Err("Launch blocked: target is not in trusted launcher cache".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // Detached + no console window, matching how the rest of the app spawns
        // helpers. Explorer returns immediately; the app is activated by the shell.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("explorer.exe")
            .arg(requested)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|error| format!("Failed to launch packaged app: {error}"))?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        return Err("Packaged apps are Windows-only".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        let _ = super::frecency::record_frecency_launch(
            app,
            "app".to_string(),
            requested.to_string(),
        );
        Ok(())
    }
}

pub fn open_search_result_path(app: std::sync::Arc<dyn dash_core::Host>, path: String) -> Result<(), String> {
    let requested = path.trim();
    if requested.is_empty() {
        return Err("Search result path is empty".to_string());
    }

    // Validate the path before handing it to the OS shell launcher. We
    // canonicalize first (rejects `..` traversal and resolves symlinks)
    // then refuse system locations — without this, a compromised frontend
    // could invoke us to "open" a `.bat` in `C:\Windows\Temp` and silently
    // get arbitrary command execution under the user's account.
    let target_path = dash_core::safe_path::validate_user_path(requested)?;

    let metadata = fs::metadata(&target_path)
        .map_err(|error| format!("Cannot inspect search result: {error}"))?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err("Only files and folders can be opened from search results".to_string());
    }

    open::that_detached(&target_path)
        .map_err(|error| format!("Failed to open search result: {error}"))?;

    // Record the activation with the appropriate kind so search ranking learns
    // which files and folders the user actually returns to.
    let kind = if metadata.is_dir() { "folder" } else { "file" };
    let _ = super::frecency::record_frecency_launch(
        app,
        kind.to_string(),
        requested.to_string(),
    );
    Ok(())
}

pub async fn start_file_search_index(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchIndexOptions,
) -> Result<FileSearchBuildResult, String> {
    on_owner!("start_file_search_index", { "options": options });
    let app_for_worker = app.clone();
    tokio::task::spawn_blocking(move || {
        // Also refresh the standalone filename index (it uses the MFT
        // fast-path when opted-in + elevated). Best-effort and independent —
        // its outcome does not affect the content-index build result the user
        // sees, and post-7d the two indexers no longer touch each other.
        let _ = start_isolated_filename_index_worker(&app_for_worker, options.clone(), true);
        start_isolated_index_worker(app_for_worker, options)
    })
    .await
    .map_err(|error| format!("Search index launcher failed: {error}"))?
}

/// Build the content index (the Content Search tab) — triggers only the
/// content indexer, not the filename indexer.
pub async fn start_content_search_index(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchIndexOptions,
) -> Result<FileSearchBuildResult, String> {
    on_owner!("start_content_search_index", { "options": options });
    let app_for_worker = app.clone();
    tokio::task::spawn_blocking(move || start_isolated_index_worker(app_for_worker, options))
        .await
        .map_err(|error| format!("Search index launcher failed: {error}"))?
}

/// Build the filename index (the File Search tab) via the deterministic
/// folder walker. Triggers only the filename indexer.
pub async fn start_filename_search_index(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchIndexOptions,
) -> Result<bool, String> {
    on_owner!("start_filename_search_index", { "options": options });
    let app_for_worker = app.clone();
    tokio::task::spawn_blocking(move || {
        start_isolated_filename_index_worker(&app_for_worker, options, true)
    })
    .await
    .map_err(|error| format!("Filename index launcher failed: {error}"))?
}

pub async fn search_local_files(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchQueryOptions,
) -> Result<FileSearchQueryResult, String> {
    on_owner!("search_local_files", { "options": options });
    tokio::task::spawn_blocking(move || search_local_files_inner(app, options))
        .await
        .map_err(|error| format!("Search worker failed: {error}"))?
}

pub async fn search_file_contents(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchQueryOptions,
) -> Result<ContentSearchQueryResult, String> {
    on_owner!("search_file_contents", { "options": options });
    tokio::task::spawn_blocking(move || search_file_contents_inner(app, options))
        .await
        .map_err(|error| format!("Content search worker failed: {error}"))?
}

pub(crate) fn search_local_files_inner(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchQueryOptions,
) -> Result<FileSearchQueryResult, String> {
    // Load the content index if available — file search merges content hits
    // when it exists. A failure here is NOT fatal: the most common cause is a
    // content-index rebuild in progress, and file search runs off the separate,
    // already-built filename index. Degrade to that instead of failing the
    // whole query with the content index's "rebuilding" message.
    let _ = ensure_engine_loaded(&app);

    let query_text = options.query.trim().to_string();
    if query_text.is_empty() {
        return Err("Enter a search query".to_string());
    }

    let started_at = SystemTime::now();
    // High-precision timer for per-phase debug logging.
    let t_start = std::time::Instant::now();
    let limit = options.limit.unwrap_or(30).clamp(1, 200);
    let offset = options.offset.unwrap_or(0).min(20_000);
    let manual_extension_filters = parse_extension_filters(options.extension_filter.as_deref());
    let path_filter = options
        .path_filter
        .as_ref()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty());

    // The filename index is searchable on its own — load it so file search
    // works even when no content index has been built (the common case for a
    // user who only set up the File Search tab).
    ensure_filename_engine_loaded(&app);

    // Take what the query needs (all cheap clones) and release the engine
    // lock, so a long query doesn't block other searches or the indexer.
    let (searcher, index, fields) = {
        let guard = SEARCH_ENGINE
            .lock()
            .map_err(|_| "Search engine lock failed".to_string())?;
        let Some(engine) = guard.as_ref() else {
            // No content index — serve file/name search from the standalone
            // filename index alone. Content search legitimately still requires the
            // content index and reports "not ready" through its own command.
            drop(guard);
            return search_local_files_via_filename_index(&app, &options, started_at);
        };
        // Reader auto-reloads via OnCommitWithDelay policy (background thread). The
        // previous explicit `reload_search_reader_if_due` was blocking the search
        // thread for 2-4 seconds after each watcher commit while it verified segments.
        // Cost of removal: newly-indexed files take ~5s to appear in search instead
        // of being instantly visible — acceptable trade for never blocking queries.
        (engine.reader.searcher(), engine.index.clone(), engine.fields)
    };
    let t_engine_ready = t_start.elapsed();

    // File search queries names and paths only — never the `content` field.
    // Including `content` made file search scale with indexed *full text*: on a
    // 67k-file content index a query like "Asura" took ~865 ms instead of the
    // tens of ms it costs against names alone. In-file text is the Content
    // Search tab's job, served by its own `search_file_contents` command.
    let mut parser = QueryParser::for_index(
        &index,
        vec![fields.file_name, fields.path],
    );

    let natural_language = options.natural_language.unwrap_or(true);
    if natural_language {
        // Favor file name and path matching in natural-language mode.
        //
        // Parser-level fuzzy (`set_field_fuzzy`) was previously enabled here but it
        // expanded *every* term in *every* query into an edit-distance-1 Levenshtein
        // automaton. For short prefixes like "fi", "fo", or "as", this expanded into
        // thousands of terms each becoming a sub-query, causing 70ms–11s query times.
        //
        // Typo tolerance is preserved by the dedicated Pass 3 (FuzzyTermQuery) which
        // only fires when exact matching returns zero results — much more efficient.
        parser.set_field_boost(fields.file_name, 2.3);
        parser.set_field_boost(fields.path, 1.35);
    } else {
        parser.set_conjunction_by_default();
    }

    let natural_plan = if natural_language {
        build_natural_query_plan(&query_text)
    } else {
        NaturalQueryPlan::default()
    };

    let natural_extension_filters = natural_plan.extension_filters.clone();
    let effective_extension_filters = if manual_extension_filters.is_empty() {
        natural_extension_filters.clone()
    } else if natural_extension_filters.is_empty() {
        manual_extension_filters.clone()
    } else {
        manual_extension_filters
            .iter()
            .filter(|value| {
                natural_extension_filters
                    .iter()
                    .any(|allowed| allowed == *value)
            })
            .cloned()
            .collect::<Vec<_>>()
    };

    if !manual_extension_filters.is_empty()
        && !natural_extension_filters.is_empty()
        && effective_extension_filters.is_empty()
    {
        let took_ms = started_at.elapsed().map(|d| d.as_millis()).unwrap_or(0);
        return Ok(FileSearchQueryResult {
            query: query_text,
            total_hits: 0,
            returned: 0,
            took_ms,
            results: Vec::new(),
        });
    }

    let query_keywords = if natural_language {
        natural_plan.query_keywords.clone()
    } else {
        extract_query_keywords(&query_text)
    };
    let normalized_rank_query = normalize_launcher_text(if natural_language {
        &natural_plan.query_text
    } else {
        &query_text
    });

    let query = if natural_language {
        if let Some(natural_query) = build_natural_query_string(
            &natural_plan.query_text,
            &natural_plan.query_keywords,
            &effective_extension_filters,
        ) {
            let (parsed, _warnings) = parser.parse_query_lenient(&natural_query);
            parsed
        } else {
            Box::new(AllQuery)
        }
    } else {
        let strict_query = if effective_extension_filters.is_empty() {
            query_text.clone()
        } else {
            build_query_with_extension_filters(&query_text, &effective_extension_filters)
        };
        parser
            .parse_query(&strict_query)
            .map_err(|error| format!("Invalid query: {error}"))?
    };
    let t_query_built = t_start.elapsed();

    // Name/path/metadata field view shared by `build_result_item` across the
    // combined index and (step 7c-ii) the filename index.
    let result_fields = fields.result_doc_fields();
    // Frecency snapshot — loaded once here so `build_result_item` can fuse it
    // into every result's score (Search #13). Loading it before the passes is
    // what lets the unified scorer see frecency, instead of it being bolted on
    // afterwards in a separate post-pass loop.
    let frecency = super::frecency::snapshot(&app);

    let fetch_results = |query: &dyn Query,
                         aggressive_fallback: bool,
                         use_fuzzy: bool|
     -> Result<(usize, Vec<FileSearchResultItem>), String> {
        let total_hits = searcher
            .search(query, &Count)
            .map_err(|error| format!("Search failed: {error}"))?;

        let mut matched = Vec::new();
        let mut effective_total_hits = total_hits;
        let needs_post_filter = path_filter.is_some()
            || natural_plan.entry_type_filter.is_some()
            || natural_plan.date_filter.is_some()
            || natural_plan.size_filter.is_some();
        let needs_result_filter = needs_post_filter
            || aggressive_fallback
            || (natural_language && !query_keywords.is_empty());
        if needs_result_filter {
            let target_count = offset + limit;
            // Bound how many index docs the post-filtering scan walks. The scan
            // runs in descending score order, so the cap keeps the best-scored
            // candidates and only drops deeply-buried ones. Both caps exist
            // because the scan costs ~40 us/doc: a broad query (e.g. "a folder"
            // — the type filter passes few docs, so the scan never reaches
            // `target_count` and would otherwise run to the ceiling) took ~11 s
            // at the old 250k ceiling. 30k bounds the worst case to ~1 s; the
            // last-ditch aggressive AllQuery pass is capped tighter, at 8k.
            let multiplier = if aggressive_fallback { 20 } else { 8 };
            let cap = if aggressive_fallback { 8_000 } else { 4_000 };
            let page_size = target_count.saturating_mul(multiplier).clamp(300, cap);
            let max_scan = if aggressive_fallback {
                total_hits.min(8_000).max(target_count.saturating_mul(20))
            } else {
                total_hits.min(30_000).max(target_count)
            };
            let mut scan_offset = 0usize;

            while matched.len() < target_count && scan_offset < max_scan {
                let start = scan_offset;
                let end = (start + page_size).min(max_scan);
                let requested = end.saturating_sub(start);
                let top_docs = searcher
                    .search(query, &TopDocs::for_doc_range(start..end).order_by_score())
                    .map_err(|error| format!("Search failed: {error}"))?;

                if top_docs.is_empty() {
                    break;
                }

                let fetched = top_docs.len();
                scan_offset = start + fetched;

                for (score, address) in top_docs {
                    if let Some(item) = build_result_item(
                        &searcher,
                        &result_fields,
                        address,
                        score,
                        &query_keywords,
                        &normalized_rank_query,
                        natural_language,
                        path_filter.as_deref(),
                        &natural_plan,
                        &effective_extension_filters,
                        use_fuzzy,
                        &frecency,
                    )? {
                        matched.push(item);
                    }
                }

                if fetched < requested {
                    break;
                }
            }

            if needs_result_filter {
                effective_total_hits = if matched.len() < target_count
                    || scan_offset >= max_scan
                    || scan_offset >= total_hits
                {
                    matched.len()
                } else {
                    matched.len().saturating_add(page_size).min(total_hits)
                };
            }
        } else {
            let fetch_limit = if aggressive_fallback {
                (offset + limit).saturating_mul(40).clamp(limit * 5, 20_000)
            } else {
                (offset + limit).saturating_mul(2).clamp(limit, 5_000)
            };
            let top_docs = searcher
                .search(query, &TopDocs::with_limit(fetch_limit).order_by_score())
                .map_err(|error| format!("Search failed: {error}"))?;

            for (score, address) in top_docs {
                if let Some(item) = build_result_item(
                    &searcher,
                    &result_fields,
                    address,
                    score,
                    &query_keywords,
                    &normalized_rank_query,
                    natural_language,
                    path_filter.as_deref(),
                    &natural_plan,
                    &effective_extension_filters,
                    use_fuzzy,
                    &frecency,
                )? {
                    matched.push(item);
                }
            }
        }

        Ok((effective_total_hits, matched))
    };

    // Pass 0: native Tantivy — date/size/entry-type filters expressed as RangeQuery/TermQuery
    // so Tantivy skips disqualified segments via FAST fields before we ever see the doc.
    // Only fires when at least one structured filter is present; falls through on 0 results.
    let mut pass0_fired = false;
    let mut pass1_fired = false;
    let mut pass1_5_fired = false;
    let mut pass2_fired = false;
    let mut pass3_fired = false;
    let (mut total_hits, mut matched) = {
        let mut native_result = None;
        if natural_language {
            if let Some(native_query) = build_native_tantivy_query(
                &query_keywords,
                &effective_extension_filters,
                &natural_plan,
                &fields,
            ) {
                pass0_fired = true;
                let (t, m) = fetch_results(native_query.as_ref(), false, false)?;
                if !m.is_empty() {
                    native_result = Some((t, m));
                }
            }
        }
        match native_result {
            Some(r) => r,
            None => {
                pass1_fired = true;
                fetch_results(query.as_ref(), false, false)?
            }
        }
    };
    let t_pass_main = t_start.elapsed();

    // Pass 1.5: prefix range query. When exact term matching (Pass 1) returns nothing,
    // try matching every indexed term that *starts with* each keyword. The term
    // dictionary is FST-backed, so this is essentially a free lookup — single-digit
    // millisecond worst case, vs Pass 2's 200ms AllQuery scan.
    //
    // This catches the common "as-you-type" pattern: user typed "spi" but the index
    // contains "spider" as a token. Pass 1's exact match misses it; Pass 1.5's
    // [spi, spj) range hits it instantly.
    if matched.is_empty()
        && natural_language
        && !query_keywords.is_empty()
        && natural_plan.date_filter.is_none()
    {
        if let Some(prefix_query) = build_prefix_tantivy_query(
            &query_keywords,
            fields.file_name,
            fields.path,
        ) {
            pass1_5_fired = true;
            let (prefix_total, prefix_matches) = fetch_results(prefix_query.as_ref(), false, false)?;
            if !prefix_matches.is_empty() {
                total_hits = prefix_total;
                matched = prefix_matches;
            }
        }
    }
    let t_pass_prefix = t_start.elapsed();

    // Option B query routing (Phase 3, step 7c): merge whole-disk name/path
    // matches from the standalone filename index, deduped by path. This runs
    // BEFORE the expensive content-index fallbacks below: the filename index
    // usually already holds what file search is after, so folding it in here
    // means Pass 2 (an up-to-8k-doc AllQuery scan) and the Pass 3 fuzzy walk
    // fire only when nothing matched anywhere — not fruitlessly on every query
    // whose answer is a non-document file (the content index holds readable
    // documents only, so e.g. a video named "Asura" is never in it). When the
    // filename index is absent this is skipped, identical to the pre-7c path.
    if let Ok(filename_guard) = FILENAME_ENGINE.lock() {
        if let Some(filename_handle) = filename_guard.as_ref() {
            let filename_fetch_limit = (offset + limit).saturating_mul(8).clamp(limit, 8_000);
            if let Ok(filename_matches) = search_filename_index(
                filename_handle,
                natural_language,
                &natural_plan,
                &query_text,
                &query_keywords,
                &normalized_rank_query,
                &effective_extension_filters,
                path_filter.as_deref(),
                filename_fetch_limit,
                &frecency,
            ) {
                let seen: std::collections::HashSet<String> =
                    matched.iter().map(|item| item.path.to_lowercase()).collect();
                for item in filename_matches {
                    if !seen.contains(&item.path.to_lowercase()) {
                        matched.push(item);
                    }
                }
                total_hits = total_hits.max(matched.len());
            }
            // A filename-index query error is non-fatal — the content-index
            // results already in `matched` still stand.
        }
    }

    if matched.is_empty()
        && natural_language
        && !query_keywords.is_empty()
        && natural_plan.date_filter.is_none()
        && should_run_aggressive_file_fallback(
            &natural_plan,
            &query_keywords,
            &effective_extension_filters,
            path_filter.as_deref(),
        )
    {
        pass2_fired = true;
        let all_docs_query = AllQuery;
        let (fallback_total_hits, fallback_matches) = fetch_results(&all_docs_query, true, false)?;
        if !fallback_matches.is_empty() {
            total_hits = fallback_total_hits;
            matched = fallback_matches;
        }
    }
    let t_pass_fallback = t_start.elapsed();

    // Fuzzy pass: when exact + aggressive both returned nothing, use FuzzyTermQuery (edit-distance 1).
    // This is the Rust API path — much more reliable than the string ~1 syntax.
    if matched.is_empty()
        && natural_language
        && !query_keywords.is_empty()
        && natural_plan.date_filter.is_none()
        && natural_plan.exact_phrases.is_empty()
    {
        if let Some(fuzzy_query) = build_fuzzy_tantivy_query(
            &query_keywords,
            fields.file_name,
            fields.path,
        ) {
            pass3_fired = true;
            let (fuzzy_total, fuzzy_matches) = fetch_results(fuzzy_query.as_ref(), false, true)?;
            if !fuzzy_matches.is_empty() {
                total_hits = fuzzy_total;
                matched = fuzzy_matches;
            }
        }
    }
    let t_pass_fuzzy = t_start.elapsed();

    matched.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file_name.cmp(&b.file_name))
            .then_with(|| a.path.cmp(&b.path))
    });

    let results = matched
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let took_ms = started_at.elapsed().map(|d| d.as_millis()).unwrap_or(0);
    let t_total = t_start.elapsed();

    // Per-phase debug timing — printed to stderr, visible in `cargo tauri dev` console.
    // Format chosen so each line is grep-able. Remove or gate behind env var when done measuring.
    let us = |d: std::time::Duration| (d.as_micros() as f64) / 1000.0;
    let pass_main_ms = us(t_pass_main) - us(t_query_built);
    let pass_prefix_ms = us(t_pass_prefix) - us(t_pass_main);
    let pass_fallback_ms = us(t_pass_fallback) - us(t_pass_prefix);
    let pass_fuzzy_ms = us(t_pass_fuzzy) - us(t_pass_fallback);
    let post_ms = us(t_total) - us(t_pass_fuzzy);
    eprintln!(
        "[search] q={:?} nl={} hits={} returned={} | engine_ready={:.2}ms plan+parse={:.2}ms main={:.2}ms (p0={} p1={}) prefix={:.2}ms (p1.5={}) fallback={:.2}ms (p2={}) fuzzy={:.2}ms (p3={}) post={:.2}ms TOTAL={:.2}ms",
        query_text,
        natural_language,
        total_hits,
        results.len(),
        us(t_engine_ready),
        us(t_query_built) - us(t_engine_ready),
        pass_main_ms,
        pass0_fired,
        pass1_fired,
        pass_prefix_ms,
        pass1_5_fired,
        pass_fallback_ms,
        pass2_fired,
        pass_fuzzy_ms,
        pass3_fired,
        post_ms,
        us(t_total),
    );

    Ok(FileSearchQueryResult {
        query: query_text,
        total_hits,
        returned: results.len(),
        took_ms,
        results,
    })
}

/// Build a fallback query string that decompounds joined keywords. A keyword
/// like "spiderman" is split at every interior point into two parts of at
/// least 3 characters, and each split becomes an `(a AND b)` clause — so the
/// query can match a file indexed as the split tokens "spider" + "man" (from
/// "spider-man"). Clauses for different keywords are AND-ed. Returns `None`
/// when no keyword is long enough to split usefully.
fn build_decompound_query_string(keywords: &[String]) -> Option<String> {
    const MIN_PART: usize = 3;

    let mut clauses: Vec<String> = Vec::new();
    let mut any_decompounded = false;

    for keyword in keywords {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            continue;
        }
        let chars: Vec<char> = keyword.chars().collect();
        let decompoundable =
            chars.len() >= MIN_PART * 2 && chars.iter().all(|c| c.is_alphanumeric());
        if !decompoundable {
            clauses.push(keyword.to_string());
            continue;
        }
        let mut splits: Vec<String> = Vec::new();
        for cut in MIN_PART..=(chars.len() - MIN_PART) {
            let head: String = chars[..cut].iter().collect();
            let tail: String = chars[cut..].iter().collect();
            splits.push(format!("({head} AND {tail})"));
        }
        any_decompounded = true;
        clauses.push(format!("({})", splits.join(" OR ")));
    }

    if !any_decompounded || clauses.is_empty() {
        return None;
    }
    Some(clauses.join(" AND "))
}

/// The whole-disk name/path half of Option B query routing (Phase 3, step 7c).
/// Searches the standalone filename index and returns ranked result items; the
/// combined index keeps serving the content half. Reuses the shared query
/// builders and `build_result_item`. v1 uses a straight top-N fetch per pass
/// (no cap-scan) — the combined index carries the rigorous filtered passes, the
/// filename index adds whole-disk breadth.
#[allow(clippy::too_many_arguments)]
fn search_filename_index(
    handle: &FilenameIndexHandle,
    natural_language: bool,
    natural_plan: &NaturalQueryPlan,
    query_text: &str,
    query_keywords: &[String],
    normalized_rank_query: &str,
    effective_extension_filters: &[String],
    path_filter: Option<&str>,
    fetch_limit: usize,
    frecency: &super::frecency::FrecencySnapshot,
) -> Result<Vec<FileSearchResultItem>, String> {
    let searcher = handle.reader.searcher();
    let result_fields = handle.fields.result_doc_fields();

    let mut parser =
        QueryParser::for_index(&handle.index, vec![handle.fields.file_name, handle.fields.path]);
    if natural_language {
        parser.set_field_boost(handle.fields.file_name, 2.3);
        parser.set_field_boost(handle.fields.path, 1.35);
    } else {
        parser.set_conjunction_by_default();
    }

    let main_query: Box<dyn Query> = if natural_language {
        match build_natural_query_string(
            &natural_plan.query_text,
            &natural_plan.query_keywords,
            effective_extension_filters,
        ) {
            Some(natural_query) => parser.parse_query_lenient(&natural_query).0,
            None => Box::new(AllQuery),
        }
    } else {
        let strict_query = if effective_extension_filters.is_empty() {
            query_text.to_string()
        } else {
            build_query_with_extension_filters(query_text, effective_extension_filters)
        };
        match parser.parse_query(&strict_query) {
            Ok(parsed) => parsed,
            Err(_) => return Ok(Vec::new()),
        }
    };

    let run = |query: &dyn Query, use_fuzzy: bool| -> Result<Vec<FileSearchResultItem>, String> {
        let top_docs = searcher
            .search(query, &TopDocs::with_limit(fetch_limit).order_by_score())
            .map_err(|error| format!("Filename index search failed: {error}"))?;
        let mut items = Vec::new();
        for (score, address) in top_docs {
            if let Some(item) = build_result_item(
                &searcher,
                &result_fields,
                address,
                score,
                query_keywords,
                normalized_rank_query,
                natural_language,
                path_filter,
                natural_plan,
                effective_extension_filters,
                use_fuzzy,
                frecency,
            )? {
                items.push(item);
            }
        }
        Ok(items)
    };

    let mut matched = run(main_query.as_ref(), false)?;

    // Prefix fallback (as-you-type): match indexed terms that start with each
    // keyword when the exact-term query found nothing.
    if matched.is_empty()
        && natural_language
        && !query_keywords.is_empty()
        && natural_plan.date_filter.is_none()
    {
        if let Some(prefix_query) =
            build_prefix_tantivy_query(query_keywords, handle.fields.file_name, handle.fields.path)
        {
            matched = run(prefix_query.as_ref(), false)?;
        }
    }

    // Fuzzy fallback (edit-distance 1) when exact + prefix both found nothing.
    if matched.is_empty()
        && natural_language
        && !query_keywords.is_empty()
        && natural_plan.date_filter.is_none()
        && natural_plan.exact_phrases.is_empty()
    {
        if let Some(fuzzy_query) =
            build_fuzzy_tantivy_query(query_keywords, handle.fields.file_name, handle.fields.path)
        {
            matched = run(fuzzy_query.as_ref(), true)?;
        }
    }

    // Decompound fallback — a query keyword may be a joined compound
    // ("spiderman") whose on-disk target is indexed as split tokens ("spider"
    // + "man", from "spider-man"). When every pass above came up empty, split
    // each long keyword and require all of its parts.
    if matched.is_empty()
        && natural_language
        && !query_keywords.is_empty()
        && natural_plan.date_filter.is_none()
        && natural_plan.exact_phrases.is_empty()
    {
        if let Some(decompound) = build_decompound_query_string(query_keywords) {
            let (parsed, _warnings) = parser.parse_query_lenient(&decompound);
            matched = run(parsed.as_ref(), false)?;
        }
    }

    Ok(matched)
}

/// Lazily open and cache the standalone filename index in `FILENAME_ENGINE`.
/// A no-op once loaded; the filename worker monitor refreshes the handle after
/// every rebuild, and the `OnCommitWithDelay` reader picks up the tailer's /
/// watcher's live commits on its own.
fn ensure_filename_engine_loaded(app: &std::sync::Arc<dyn dash_core::Host>) {
    {
        let Ok(guard) = FILENAME_ENGINE.lock() else {
            return;
        };
        if guard.is_some() {
            return;
        }
    }
    let Ok(state_dir) = search_index_dir(app) else {
        return;
    };
    let handle = try_open_filename_index(&state_dir);
    if handle.is_none() {
        return;
    }
    if let Ok(mut guard) = FILENAME_ENGINE.lock() {
        if guard.is_none() {
            *guard = handle;
        }
    }
}

/// Live document count of the cached filename index — 0 when it is not loaded.
/// Surfaced on the File Search tab as the filename index's "indexed files".
fn filename_index_num_docs() -> u64 {
    let Ok(guard) = FILENAME_ENGINE.lock() else {
        return 0;
    };
    let Some(handle) = guard.as_ref() else {
        return 0;
    };
    handle.reader.searcher().num_docs()
}

/// Merge manually-typed and naturally-inferred extension filters. `None` means
/// the two non-empty sets are disjoint — a query that can match nothing, which
/// the caller answers with an empty result set.
fn resolve_extension_filters(manual: &[String], natural: &[String]) -> Option<Vec<String>> {
    if manual.is_empty() {
        return Some(natural.to_vec());
    }
    if natural.is_empty() {
        return Some(manual.to_vec());
    }
    let intersection: Vec<String> = manual
        .iter()
        .filter(|value| natural.iter().any(|allowed| allowed == *value))
        .cloned()
        .collect();
    if intersection.is_empty() {
        None
    } else {
        Some(intersection)
    }
}

/// File/name search served from the standalone filename index alone — used when
/// no content index exists. The File Search tab and the Content Search tab
/// build independent indexes, so a user who only set up filename search must
/// still get results here instead of "Search index is not ready."
fn search_local_files_via_filename_index(
    app: &std::sync::Arc<dyn dash_core::Host>,
    options: &FileSearchQueryOptions,
    started_at: SystemTime,
) -> Result<FileSearchQueryResult, String> {
    let query_text = options.query.trim().to_string();
    let limit = options.limit.unwrap_or(30).clamp(1, 200);
    let offset = options.offset.unwrap_or(0).min(20_000);
    let manual_extension_filters = parse_extension_filters(options.extension_filter.as_deref());
    let path_filter = options
        .path_filter
        .as_ref()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty());
    let natural_language = options.natural_language.unwrap_or(true);
    let natural_plan = if natural_language {
        build_natural_query_plan(&query_text)
    } else {
        NaturalQueryPlan::default()
    };

    let effective_extension_filters =
        match resolve_extension_filters(&manual_extension_filters, &natural_plan.extension_filters) {
            Some(filters) => filters,
            None => {
                return Ok(FileSearchQueryResult {
                    query: query_text,
                    total_hits: 0,
                    returned: 0,
                    took_ms: started_at.elapsed().map(|d| d.as_millis()).unwrap_or(0),
                    results: Vec::new(),
                });
            }
        };

    let query_keywords = if natural_language {
        natural_plan.query_keywords.clone()
    } else {
        extract_query_keywords(&query_text)
    };
    let normalized_rank_query = normalize_launcher_text(if natural_language {
        &natural_plan.query_text
    } else {
        &query_text
    });

    // Frecency snapshot — loaded once so `search_filename_index` can fuse it
    // into every result's score (Search #13).
    let frecency = super::frecency::snapshot(app);
    let mut matched = {
        let guard = FILENAME_ENGINE
            .lock()
            .map_err(|_| "Filename engine lock failed".to_string())?;
        let handle = guard
            .as_ref()
            .ok_or_else(|| "Search index is not ready yet. Build it first.".to_string())?;
        let fetch_limit = (offset + limit).saturating_mul(8).clamp(limit, 8_000);
        search_filename_index(
            handle,
            natural_language,
            &natural_plan,
            &query_text,
            &query_keywords,
            &normalized_rank_query,
            &effective_extension_filters,
            path_filter.as_deref(),
            fetch_limit,
            &frecency,
        )?
    };

    matched.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file_name.cmp(&b.file_name))
            .then_with(|| a.path.cmp(&b.path))
    });

    let total_hits = matched.len();
    let results = matched
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let took_ms = started_at.elapsed().map(|d| d.as_millis()).unwrap_or(0);
    Ok(FileSearchQueryResult {
        query: query_text,
        total_hits,
        returned: results.len(),
        took_ms,
        results,
    })
}

fn search_file_contents_inner(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchQueryOptions,
) -> Result<ContentSearchQueryResult, String> {
    ensure_engine_loaded(&app)?;

    let query_text = options.query.trim().to_string();
    if query_text.is_empty() {
        return Err("Enter text to search inside indexed files".to_string());
    }

    let started_at = SystemTime::now();
    let limit = options.limit.unwrap_or(30).clamp(1, 100);
    let offset = options.offset.unwrap_or(0).min(20_000);
    let extension_filters = parse_extension_filters(options.extension_filter.as_deref());
    let path_filter = options
        .path_filter
        .as_ref()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty());

    // Take what the query needs (all cheap clones) and release the engine
    // lock before embedding, scoring and snippet work.
    let (searcher, index, fields, index_content, max_content_kb, semantic_search_enabled) = {
        let guard = SEARCH_ENGINE
            .lock()
            .map_err(|_| "Search engine lock failed".to_string())?;
        let engine = guard
            .as_ref()
            .ok_or_else(|| "Search index is not ready yet. Build it first.".to_string())?;
        // Reader auto-reloads via OnCommitWithDelay; no explicit reload needed.
        (
            engine.reader.searcher(),
            engine.index.clone(),
            engine.fields,
            engine.options.index_content,
            engine.options.max_content_kb,
            engine.options.semantic_search_enabled,
        )
    };

    if !index_content {
        return Err(
            "Content indexing is disabled. Enable content indexing and rebuild the search index."
                .to_string(),
        );
    }

    let max_content_bytes = (max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB) as usize)
        .saturating_mul(1024)
        .clamp(8 * 1024, 2 * 1024 * 1024);

    let natural_language = options.natural_language.unwrap_or(true);

    // Build a natural-language plan: extracts date/size/extension filters from
    // queries like "tax docs from 2023" or "invoices > 1mb" so content search
    // gets the same structured intent layer as file search.
    let natural_plan = if natural_language {
        build_natural_query_plan(&query_text)
    } else {
        NaturalQueryPlan::default()
    };

    // Combine manual and NL-derived extension filters.
    let natural_extension_filters = natural_plan.extension_filters.clone();
    let effective_extension_filters = if extension_filters.is_empty() {
        natural_extension_filters.clone()
    } else if natural_extension_filters.is_empty() {
        extension_filters.clone()
    } else {
        extension_filters
            .iter()
            .filter(|v| natural_extension_filters.iter().any(|a| a == *v))
            .cloned()
            .collect::<Vec<_>>()
    };
    if !extension_filters.is_empty()
        && !natural_extension_filters.is_empty()
        && effective_extension_filters.is_empty()
    {
        let took_ms = started_at.elapsed().map(|d| d.as_millis()).unwrap_or(0);
        return Ok(ContentSearchQueryResult {
            query: query_text,
            total_hits: 0,
            returned: 0,
            took_ms,
            results: Vec::new(),
            // This early-out (contradictory ext filters) is before the semantic
            // pass; nothing ran, so it's inactive for this query.
            semantic_active: false,
        });
    }

    // Keywords drive snippet generation and match counting; cleaned for highlighting.
    let query_keywords = if natural_language {
        natural_plan.query_keywords.clone()
    } else {
        extract_query_keywords(&query_text)
    };

    // Build the base content query. parse_query_lenient already supports phrases
    // ("quarterly earnings") and boolean operators (AND/OR/NOT, +/-).
    let content_query_text = if natural_language {
        natural_plan.query_text.clone()
    } else {
        query_text.clone()
    };
    let mut parser = QueryParser::for_index(&index, vec![fields.content]);
    if !natural_language {
        parser.set_conjunction_by_default();
    }
    let base_query: Box<dyn Query> = if natural_language {
        let (parsed, _warnings) = parser.parse_query_lenient(&content_query_text);
        parsed
    } else {
        parser
            .parse_query(&content_query_text)
            .map_err(|error| format!("Invalid content query: {error}"))?
    };

    // Frecency snapshot — loaded once so `build_content_result_item` can fuse
    // open-history into each result's score (Search #13).
    let frecency = super::frecency::snapshot(&app);

    // Semantic search (beta): embed the query and find the stored vectors closest
    // in meaning. `semantic_scores` (path_lower → cosine) re-ranks keyword hits;
    // `semantic_candidates` ((path, cosine), original case) are the top matches to
    // also surface as pure-semantic results below. Both empty — so content ranking
    // is byte-identical to before — unless the toggle is on AND the embedder is
    // compiled in (`semantic` feature) and its model loaded. `embed` returns
    // `None` in builds without the feature, keeping this a no-op there.
    // Deep pagination: fetch enough semantic candidates to cover the requested
    // page (offset + limit), plus a small buffer, capped so a very deep page
    // can't trigger an unbounded cosine scan. Because semantic-only hits are
    // then merged into the same ranked pool as keyword hits before pagination,
    // meaning-only results page through like everything else — not just page 1.
    // ponytail: capped at 1000; a page past that many pure-semantic hits stops
    // deepening — raise the cap or stream if a corpus ever needs it.
    let semantic_top_k = (offset + limit + 20).min(1000);
    let mut semantic_scores: HashMap<String, f32> = HashMap::new();
    let mut semantic_active = false;
    let semantic_candidates: Vec<(String, f32)> = if semantic_search_enabled
        && super::embedding::is_available()
    {
        match super::embedding::embed(&query_text).zip(search_index_dir(&app).ok()) {
            Some((qvec, state_dir)) => {
                // The meaning pass genuinely ran (toggle on, model loaded, query
                // embedded) — record it so the UI can show "semantic on" even when
                // this query yields no meaning-only hits.
                semantic_active = true;
                let vec_path = super::vector_cache::cache_path_for_dir(&state_dir);
                let hits = super::vector_cache::top_k(&vec_path, &qvec, semantic_top_k);
                for (path, score) in &hits {
                    semantic_scores.insert(path.to_lowercase(), *score);
                }
                hits
            }
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    // fetch_results: runs a query, walks results with post-filtering, hydrates
    // each into a ContentSearchResultItem. Returns (effective_total, items).
    let fetch_results = |query: &dyn Query|
     -> Result<(usize, Vec<ContentSearchResultItem>), String> {
        let total_hits = searcher
            .search(query, &Count)
            .map_err(|error| format!("Content search failed: {error}"))?;
        let needs_post_filter = path_filter.is_some() || !effective_extension_filters.is_empty();
        let mut matched: Vec<ContentSearchResultItem> = Vec::new();
        let mut scanned = 0usize;
        let target_count = offset + limit;
        let page_size = target_count.saturating_mul(6).clamp(200, 3_000);
        let max_scan = if needs_post_filter {
            total_hits.min(20_000).max(target_count)
        } else {
            total_hits.min(target_count + 200).max(target_count)
        };

        while matched.len() < target_count && scanned < max_scan {
            let start = scanned;
            let end = (start + page_size).min(max_scan);
            let requested = end.saturating_sub(start);
            let top_docs = searcher
                .search(query, &TopDocs::for_doc_range(start..end).order_by_score())
                .map_err(|error| format!("Content search failed: {error}"))?;
            if top_docs.is_empty() {
                break;
            }
            let fetched = top_docs.len();
            scanned = start + fetched;
            for (score, address) in top_docs {
                if let Some(item) = build_content_result_item(
                    &searcher,
                    &fields,
                    address,
                    score,
                    &query_keywords,
                    path_filter.as_deref(),
                    &effective_extension_filters,
                    max_content_bytes,
                    &frecency,
                    &semantic_scores,
                    false, // keyword pass — not a pure-semantic hit
                )? {
                    matched.push(item);
                }
            }
            if fetched < requested {
                break;
            }
        }
        let effective_total = if needs_post_filter {
            if matched.len() < target_count || scanned >= total_hits {
                matched.len()
            } else {
                matched.len().saturating_add(page_size).min(total_hits)
            }
        } else {
            total_hits
        };
        Ok((effective_total, matched))
    };

    // ───── Multi-pass content search ─────

    // Pass 1: lenient base query (handles phrases + boolean ops natively).
    let (mut total_hits, mut matched) = fetch_results(base_query.as_ref())?;

    // Pass 1.5: prefix-range query for partial token matches. Hits when the
    // user typed a partial word ("invest") and content has the full form
    // ("investment"). FST-backed, single-digit ms cost.
    if matched.is_empty() && natural_language && !query_keywords.is_empty() {
        if let Some(prefix_query) =
            build_content_prefix_query(&query_keywords, fields.content)
        {
            let (prefix_total, prefix_matches) = fetch_results(prefix_query.as_ref())?;
            if !prefix_matches.is_empty() {
                total_hits = prefix_total;
                matched = prefix_matches;
            }
        }
    }

    // Pass 3: fuzzy edit-distance-1 for typo tolerance on longer tokens.
    if matched.is_empty()
        && natural_language
        && !query_keywords.is_empty()
        && natural_plan.exact_phrases.is_empty()
    {
        if let Some(fuzzy_query) =
            build_content_fuzzy_query(&query_keywords, fields.content)
        {
            let (fuzzy_total, fuzzy_matches) = fetch_results(fuzzy_query.as_ref())?;
            if !fuzzy_matches.is_empty() {
                total_hits = fuzzy_total;
                matched = fuzzy_matches;
            }
        }
    }

    // Semantic-only candidates: docs the vector pass rated similar that NONE of
    // the keyword passes surfaced (they share no literal query terms — the whole
    // point of semantic search). Fetch each by exact path and fuse it in with its
    // semantic score (BM25 = 0), so it ranks below real keyword hits but is still
    // found. Bounded by semantic_top_k → at most K extra single-doc lookups.
    // ponytail: these augment the current page only; they aren't deep-paginated.
    if !semantic_candidates.is_empty() {
        let existing: std::collections::HashSet<String> =
            matched.iter().map(|item| item.path.to_lowercase()).collect();
        for (path, _score) in &semantic_candidates {
            let path_lower = path.to_lowercase();
            if existing.contains(&path_lower) {
                continue;
            }
            if let Some(filter) = &path_filter {
                if !path_lower.contains(filter.as_str()) {
                    continue;
                }
            }
            let Some(address) = content_doc_address_for_path(&searcher, &fields, path) else {
                continue; // vector store had a path the index no longer holds (deleted) — skip
            };
            if let Ok(Some(item)) = build_content_result_item(
                &searcher,
                &fields,
                address,
                0.0, // pure semantic hit — no BM25 relevance
                &query_keywords,
                path_filter.as_deref(),
                &effective_extension_filters,
                max_content_bytes,
                &frecency,
                &semantic_scores,
                true, // pure-semantic candidate — surfaced by meaning only
            ) {
                matched.push(item);
                total_hits += 1;
            }
        }
    }

    matched.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file_name.cmp(&b.file_name))
            .then_with(|| a.path.cmp(&b.path))
    });

    let results = matched
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let took_ms = started_at.elapsed().map(|d| d.as_millis()).unwrap_or(0);

    Ok(ContentSearchQueryResult {
        query: query_text,
        total_hits,
        returned: results.len(),
        took_ms,
        results,
        semantic_active,
    })
}

/// Open the active file-search index and build its reader. Returns an `Err`
/// (rather than panicking) if the on-disk index is unreadable — `Index::open`,
/// `extract_fields`, or the reader build all fail on a corrupted index. The
/// caller turns that error into a quarantine + rebuild prompt.
fn try_open_active_index(
    index_dir: &Path,
) -> Result<(Index, SearchFields, IndexReader), String> {
    let directory = MmapDirectory::open(index_dir)
        .map_err(|error| format!("Cannot open search index directory: {error}"))?;
    let index =
        Index::open(directory).map_err(|error| format!("Cannot open search index: {error}"))?;
    let schema = index.schema();
    let fields = extract_fields(&schema)?;
    let reader: IndexReader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()
        .map_err(|error| format!("Cannot create search reader: {error}"))?;
    reader
        .reload()
        .map_err(|error| format!("Cannot load search reader: {error}"))?;
    Ok((index, fields, reader))
}

/// Move a corrupt active index out of the way so the next rebuild starts
/// clean. Tries a rename to a timestamped `<prefix>*` sibling first (preserves
/// it for diagnostics); if that fails — e.g. a stale mmap still holds the
/// directory — deletes it outright. Returns whether the corrupt index is no
/// longer in the active slot. `corrupt_prefix` distinguishes the content
/// index (`index-corrupt-`) from the filename index (`filename-corrupt-`).
fn quarantine_corrupt_index(index_dir: &Path, corrupt_prefix: &str) -> bool {
    if let Some(parent) = index_dir.parent() {
        let quarantine = parent.join(format!("{corrupt_prefix}{}", unix_now_ms()));
        if fs::rename(index_dir, &quarantine).is_ok() {
            return true;
        }
    }
    fs::remove_dir_all(index_dir).is_ok()
}

/// Open the filename index from its active slot. `Err` means the directory
/// exists but is unreadable — i.e. on-disk corruption — which the caller
/// turns into a quarantine. `try_open_active_index` is the content-index twin.
fn open_active_filename_index(dir: &Path) -> Result<FilenameIndexHandle, String> {
    let directory = MmapDirectory::open(dir)
        .map_err(|error| format!("Cannot open filename index directory: {error}"))?;
    let index =
        Index::open(directory).map_err(|error| format!("Cannot open filename index: {error}"))?;
    let schema = index.schema();
    let fields = extract_filename_index_fields(&schema)?;
    let reader: IndexReader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()
        .map_err(|error| format!("Cannot create filename index reader: {error}"))?;
    reader
        .reload()
        .map_err(|error| format!("Cannot load filename index reader: {error}"))?;
    Ok(FilenameIndexHandle {
        index,
        reader,
        fields,
    })
}

/// Open the filename index if it exists and is readable. Returns `None` when
/// the index has not been built yet (installs predating step 6b). A corrupt
/// filename index is quarantined to a `filename-corrupt-*` sibling and `None`
/// is returned — search degrades gracefully to the content index until the
/// next rebuild recreates it, so no user-facing error is raised.
fn try_open_filename_index(state_dir: &Path) -> Option<FilenameIndexHandle> {
    let dir = active_filename_index_dir_for_state(state_dir);
    if !dir.exists() {
        return None;
    }
    match open_active_filename_index(&dir) {
        Ok(handle) => Some(handle),
        Err(_) => {
            quarantine_corrupt_index(&dir, FILENAME_CORRUPT_INDEX_PREFIX);
            None
        }
    }
}

fn ensure_engine_loaded(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<(), String> {
    if SEARCH_STATUS
        .lock()
        .map(|status| status.indexing)
        .unwrap_or(false)
    {
        return Err("Search index is rebuilding in an isolated worker. Search will be available when it finishes.".to_string());
    }

    {
        let guard = SEARCH_ENGINE
            .lock()
            .map_err(|_| "Search engine lock failed".to_string())?;
        if guard.is_some() {
            return Ok(());
        }
    }

    // Recover an orphaned index. A content build swaps the new index into the
    // active slot BEFORE it writes search-config.json, so a restart (or a
    // DB-corruption reset) in that window leaves a valid on-disk index with no
    // config. Without this, `ensure_engine_loaded` would abandon a perfectly
    // good index and the app would re-index from scratch every launch (the
    // filename index doesn't gate on a config file, which is why it survives).
    // Fall back to the last build's options (index-worker-options.json) so the
    // index still loads; the live doc count below corrects indexed_files and we
    // persist a real config so it's not orphaned again next time.
    let (config, recovered_orphan) = match read_search_config(app)? {
        Some(config) => (config, false),
        None => {
            let state_dir = search_index_dir(app)?;
            let Some(options) = fs::read(state_dir.join(INDEX_WORKER_OPTIONS_FILE))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<FileSearchIndexOptions>(&bytes).ok())
                .and_then(|options| normalize_index_options(options).ok())
            else {
                // No config AND no recoverable options — nothing safe to load.
                return Ok(());
            };
            (
                StoredSearchConfig {
                    options,
                    indexed_files: 0,
                    last_indexed_at_ms: None,
                    rebuild_schedule: FileSearchRebuildSchedule::default(),
                },
                true,
            )
        }
    };

    recover_search_index_dirs(app)?;
    let index_dir = active_search_index_dir(app)?;
    if !index_dir.exists() {
        if config.indexed_files > 0 {
            update_status(|status| {
                status.last_error = Some(
                    "Search index storage was upgraded. Rebuild the file index once.".to_string(),
                );
            });
        }
        return Ok(());
    }

    // Open the active index. A failure here is on-disk corruption — schema
    // mismatches were already resolved at startup by `enforce_search_schema_version`.
    // Quarantine the corrupt index and prompt a one-time rebuild rather than
    // leaving search permanently broken.
    let (index, fields, reader) = match try_open_active_index(&index_dir) {
        Ok(value) => value,
        Err(error) => {
            let isolated = quarantine_corrupt_index(&index_dir, CORRUPT_SEARCH_INDEX_PREFIX);
            update_status(|status| {
                status.last_error = Some(if isolated {
                    "The search index was corrupted and has been removed. Rebuild the file index once."
                        .to_string()
                } else {
                    format!("The search index is corrupted and could not be opened: {error}")
                });
            });
            return Ok(());
        }
    };

    // Authoritative count straight from the opened index — one document per
    // indexed file. This survives a lost or 0 persisted count (the metadata can
    // vanish; the index can't), so the UI never shows 0 while a populated index
    // is loaded, and the watcher gets the right size class.
    let live_indexed_files = reader.searcher().num_docs();
    let indexed_files = if live_indexed_files > 0 {
        live_indexed_files
    } else {
        config.indexed_files
    };

    let watcher_active =
        match start_isolated_watch_worker(app, config.options.clone(), indexed_files) {
            Ok(value) => value,
            Err(error) => {
                update_status(|status| {
                    status.last_error = Some(error);
                });
                false
            }
        };

    // The standalone filename index is cached separately in `FILENAME_ENGINE`
    // (loaded lazily by `ensure_filename_engine_loaded`) so file search works
    // whether or not this content index exists.

    {
        let mut guard = SEARCH_ENGINE
            .lock()
            .map_err(|_| "Search engine lock failed".to_string())?;
        if guard.is_none() {
            *guard = Some(SearchEngine {
                index,
                reader,
                writer: None,
                watcher: None,
                fields,
                options: config.options.clone(),
            });
        }
    }

    update_status(|status| {
        status.initialized = true;
        status.indexing = false;
        status.watching = watcher_active;
        status.roots = config.options.roots.clone();
        status.filename_roots = config.options.filename_roots.clone();
        status.include_hidden = config.options.include_hidden;
        status.index_content = config.options.index_content;
        status.max_content_kb = config
            .options
            .max_content_kb
            .unwrap_or(DEFAULT_MAX_CONTENT_KB);
        status.commit_every = normalize_commit_every(config.options.commit_every);
        status.watcher_enabled = config.options.watcher_enabled;
        status.watcher_paused = config.options.watcher_paused;
        status.exclude_folders = config.options.exclude_folders.clone();
        status.exclude_extensions = config.options.exclude_extensions.clone();
        status.rebuild_schedule = config.rebuild_schedule.clone();
        status.indexed_files = indexed_files;
        status.last_indexed_at_ms = config.last_indexed_at_ms;
        status.last_error = None;
        status.diagnostics.performance_mode =
            normalize_index_performance_mode(&config.options.performance_mode);
        status.diagnostics.index_worker = "stopped".to_string();
        if !watcher_active {
            status.diagnostics.watcher_worker = "stopped".to_string();
            if !status.diagnostics.watcher_strategy.contains("disabled") {
                status.diagnostics.watcher_strategy = "not active".to_string();
            }
        }
    });

    // We recovered an index that had no config (the orphan case above). Write a
    // proper config now — with the real live count — so the next launch finds it
    // directly and never re-indexes a perfectly good index again.
    if recovered_orphan {
        let _ = write_search_config(
            app,
            &StoredSearchConfig {
                options: config.options.clone(),
                indexed_files,
                last_indexed_at_ms: config.last_indexed_at_ms,
                rebuild_schedule: config.rebuild_schedule.clone(),
            },
        );
    }

    Ok(())
}

/// Open the search index readers in the background at app launch so the first
/// query — typically from the global search overlay (Ctrl+Alt+S) — does not
/// pay the cold-open cost: index recovery, `MmapDirectory::open`, and building
/// the reader. Both engines are warmed; a not-yet-built index is a cheap
/// no-op. Runs on its own thread so app startup is never blocked.
pub fn prewarm_search_engines(app: std::sync::Arc<dyn dash_core::Host>) {
    if dash_core::link::is_follower() {
        dash_core::link::when_owner(move || prewarm_search_engines(app));
        return;
    }
    thread::spawn(move || {
        // The content engine first — it also recovers the index dirs and
        // starts the content watcher; the filename engine is a pure reader
        // open. A failure on either is non-fatal: the lazy path still runs on
        // the first real query.
        let _ = ensure_engine_loaded(&app);
        ensure_filename_engine_loaded(&app);
    });
}

pub fn start_file_search_scheduler(app: std::sync::Arc<dyn dash_core::Host>) {
    if dash_core::link::is_follower() {
        dash_core::link::when_owner(move || start_file_search_scheduler(app));
        return;
    }
    if SEARCH_SCHEDULER_STARTED.swap(true, Ordering::Relaxed) {
        return;
    }

    // One-time startup guard: drop the file-search index if it was built
    // against an older Tantivy schema so it rebuilds cleanly. Best-effort —
    // a failure here just leaves the existing index in place.
    let _ = enforce_search_schema_version(&app);

    // Launch trigger: refresh the standalone filename index now if it is
    // missing or stale, so filename search is available without waiting for a
    // full content rebuild.
    let _ = run_scheduled_filename_index_rebuild(&app);

    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(INDEX_REBUILD_SCHEDULER_POLL_SECS));
        if let Err(error) = run_scheduled_file_search_rebuild(&app) {
            update_status(|status| {
                status.last_error = Some(error);
            });
        }
        // Periodic trigger: keep the standalone filename index fresh.
        let _ = run_scheduled_filename_index_rebuild(&app);
    });
}

fn run_scheduled_file_search_rebuild(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<(), String> {
    let Some(mut config) = read_search_config(app)? else {
        return Ok(());
    };
    let schedule = normalize_rebuild_schedule(config.rebuild_schedule.clone());
    if !schedule.enabled || config.options.roots.is_empty() {
        return Ok(());
    }
    if SEARCH_STATUS
        .lock()
        .map(|status| status.indexing)
        .unwrap_or(false)
    {
        return Ok(());
    }

    let now_ms = unix_now_ms();
    let interval_ms = (schedule.interval_hours as u128).saturating_mul(60 * 60 * 1000);
    let reference_ms = schedule
        .last_scheduled_rebuild_at_ms
        .or(config.last_indexed_at_ms)
        .unwrap_or(0);
    if reference_ms > 0 && now_ms.saturating_sub(reference_ms) < interval_ms {
        return Ok(());
    }

    config.rebuild_schedule.last_scheduled_rebuild_at_ms = Some(now_ms);
    write_search_config(app, &config)?;
    update_status(|status| {
        status.rebuild_schedule = config.rebuild_schedule.clone();
    });

    let _ = start_isolated_index_worker(app.clone(), config.options.clone())?;
    Ok(())
}

/// Wall-clock ms of the last successful filename-index build, read from the
/// schema sentinel's modification time. The sentinel is rewritten at the end
/// of every successful filename build, so its mtime is a sound "last built"
/// proxy. `None` when no build has completed or the mtime is unreadable.
fn filename_index_last_built_ms(state_dir: &Path) -> Option<u128> {
    let path = filename_schema_version_path_for_state(state_dir);
    let modified = fs::metadata(&path).ok()?.modified().ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|value| value.as_millis())
}

/// Scheduler tick for the standalone filename indexer (Phase 3, step 7b). Runs
/// the metadata-only filename build when the index is missing or older than
/// `FILENAME_INDEX_REFRESH_INTERVAL_MS`. Called once at launch and once per
/// scheduler poll; `start_isolated_filename_index_worker` itself skips when a
/// content build is running or a filename build already is.
fn run_scheduled_filename_index_rebuild(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<(), String> {
    let Some(config) = read_search_config(app)? else {
        return Ok(());
    };
    if config.options.roots.is_empty() {
        return Ok(());
    }

    let state_dir = search_index_dir(app)?;
    let now_ms = unix_now_ms();

    // Cross-restart staleness: a missing index, or one whose last build is
    // older than the refresh interval, is stale.
    let active_dir = active_filename_index_dir_for_state(&state_dir);
    let stale = !active_dir.exists()
        || filename_index_last_built_ms(&state_dir)
            .map(|built| now_ms.saturating_sub(built) >= FILENAME_INDEX_REFRESH_INTERVAL_MS)
            .unwrap_or(true);
    if !stale {
        return Ok(());
    }

    // In-session anti-thrash: don't re-trigger — or retry a failed build —
    // more than once per refresh interval.
    let last_attempt = FILENAME_INDEX_LAST_REBUILD_MS.load(Ordering::Relaxed);
    if last_attempt > 0
        && now_ms.saturating_sub(last_attempt as u128) < FILENAME_INDEX_REFRESH_INTERVAL_MS
    {
        return Ok(());
    }

    FILENAME_INDEX_LAST_REBUILD_MS.store(now_ms as u64, Ordering::Relaxed);
    let _ = start_isolated_filename_index_worker(app, config.options.clone(), false)?;
    Ok(())
}

fn status_to_index_options(status: &FileSearchStatus) -> FileSearchIndexOptions {
    FileSearchIndexOptions {
        roots: status.roots.clone(),
        filename_roots: status.filename_roots.clone(),
        include_hidden: status.include_hidden,
        index_content: status.index_content,
        max_content_kb: Some(status.max_content_kb),
        commit_every: Some(status.commit_every),
        performance_mode: normalize_index_performance_mode(&status.diagnostics.performance_mode),
        watcher_enabled: status.watcher_enabled,
        watcher_paused: status.watcher_paused,
        watcher_settings_version: WATCHER_SETTINGS_VERSION,
        exclude_folders: status.exclude_folders.clone(),
        exclude_extensions: status.exclude_extensions.clone(),
        // Wave 8 / task #88 (2026-05-28): this builder runs only on the
        // corrupt-stored-config fallback path (`StoredSearchConfig`
        // unwrap_or_default branch). Defaulting OCR off is the safer
        // choice — if we can't trust the saved config, we shouldn't
        // accidentally auto-enable OCR scanning. The user reconfigures
        // from the UI on next open.
        ocr_on_index_enabled: false,
        ocr_on_index_folders: Vec::new(),
        ocr_max_pages_per_file: default_ocr_max_pages(),
        ocr_langs: default_ocr_langs(),
        ocr_min_image_dim: default_ocr_min_image_dim(),
        ocr_max_aspect_ratio: default_ocr_max_aspect_ratio(),
        ocr_per_file_timeout_secs: default_ocr_timeout_secs(),
        content_indexing_enabled: default_content_indexing_enabled(),
        // Same safe-default reasoning as OCR above: on a corrupt-config
        // fallback, leave the semantic beta off; the user re-enables from UI.
        semantic_search_enabled: false,
    }
}

fn normalize_commit_every(value: Option<usize>) -> usize {
    match value {
        Some(raw) if raw >= MIN_COMMIT_EVERY => raw.min(250_000),
        _ => DEFAULT_COMMIT_EVERY,
    }
}

fn normalize_index_options(
    mut options: FileSearchIndexOptions,
) -> Result<FileSearchIndexOptions, String> {
    if options.roots.is_empty() {
        return Err("Choose at least one folder to index".to_string());
    }

    for root in &options.roots {
        let path = PathBuf::from(root);
        if !path.exists() {
            return Err(format!("Index source not found: {root}"));
        }
    }

    options.max_content_kb = Some(options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB));
    options.commit_every = Some(normalize_commit_every(options.commit_every));
    options.performance_mode = normalize_index_performance_mode(&options.performance_mode);
    if !options.watcher_enabled {
        options.watcher_paused = false;
    }
    options.watcher_settings_version = WATCHER_SETTINGS_VERSION;
    options.exclude_folders = normalize_exclude_folders(&options.exclude_folders);
    options.exclude_extensions = normalize_exclude_extensions(&options.exclude_extensions);
    Ok(options)
}

fn search_coverage_options_changed(
    previous: &FileSearchIndexOptions,
    next: &FileSearchIndexOptions,
) -> bool {
    previous.roots != next.roots
        || previous.include_hidden != next.include_hidden
        || previous.index_content != next.index_content
        || previous.max_content_kb != next.max_content_kb
        || previous.exclude_folders != next.exclude_folders
        || previous.exclude_extensions != next.exclude_extensions
}

fn start_isolated_index_worker(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchIndexOptions,
) -> Result<FileSearchBuildResult, String> {
    let mut options = normalize_index_options(options)?;
    // A fresh rebuild should resume live watching when the desired setting is enabled.
    options.watcher_paused = false;
    // The Content Search index always indexes text content — that is the
    // entire purpose of this index, and the opt-out toggle was removed from
    // the UI. Pin it on so an older saved config cannot leave the content
    // index built without any content.
    options.index_content = true;
    let state_dir = search_index_dir(&app)?;
    fs::create_dir_all(&state_dir)
        .map_err(|error| format!("Cannot create search index state directory: {error}"))?;
    recover_content_index_dirs_for_state(&state_dir)?;
    stop_isolated_watch_worker_for_state(&state_dir);

    {
        let mut worker_lock = SEARCH_INDEX_WORKER
            .lock()
            .map_err(|_| "Search worker lock failed".to_string())?;
        if let Some(worker) = worker_lock.as_ref() {
            let still_running = worker
                .child
                .lock()
                .map(|mut child| child.try_wait().ok().flatten().is_none())
                .unwrap_or(false);
            if still_running {
                return Err("Index build is already running".to_string());
            }
            *worker_lock = None;
        }
    }

    {
        let mut guard = SEARCH_ENGINE
            .lock()
            .map_err(|_| "Search engine lock failed".to_string())?;
        *guard = None;
    }

    let options_path = state_dir.join(INDEX_WORKER_OPTIONS_FILE);
    let status_path = state_dir.join(INDEX_WORKER_STATUS_FILE);
    let cancel_path = state_dir.join(INDEX_WORKER_CANCEL_FILE);
    let options_bytes = serde_json::to_vec_pretty(&options)
        .map_err(|error| format!("Cannot serialize index worker options: {error}"))?;
    fs::write(&options_path, options_bytes)
        .map_err(|error| format!("Cannot write index worker options: {error}"))?;
    request_stale_worker_stop(&status_path, &cancel_path);
    let _ = fs::remove_file(&cancel_path);
    let _ = fs::remove_file(&status_path);
    write_index_worker_status(
        &status_path,
        &IndexWorkerStatusFile::progress(0, 0, 0, "Starting isolated index worker".to_string()),
    )?;

    let mut command = Command::new(
        std::env::current_exe()
            .map_err(|error| format!("Cannot find this program's executable: {error}"))?,
    );
    command
        .arg(INDEX_WORKER_ARG)
        .arg("index")
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--options")
        .arg(&options_path)
        .arg("--status")
        .arg(&status_path)
        .arg("--cancel")
        .arg(&cancel_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_worker_process(
        &mut command,
        &options.performance_mode,
        WorkerProcessKind::Index,
    );

    let child = command
        .spawn()
        .map_err(|error| format!("Cannot start isolated index worker: {error}"))?;
    let worker = SearchIndexWorkerProcess {
        child: Arc::new(Mutex::new(child)),
        status_path: status_path.clone(),
        cancel_path: cancel_path.clone(),
    };

    {
        let mut worker_lock = SEARCH_INDEX_WORKER
            .lock()
            .map_err(|_| "Search worker lock failed".to_string())?;
        *worker_lock = Some(worker.clone());
    }

    update_status(|status| {
        status.indexing = true;
        status.initialized = false;
        status.watching = false;
        status.roots = options.roots.clone();
        status.include_hidden = options.include_hidden;
        status.index_content = options.index_content;
        status.max_content_kb = options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB);
        status.commit_every = normalize_commit_every(options.commit_every);
        status.watcher_enabled = options.watcher_enabled;
        status.watcher_paused = options.watcher_paused;
        status.exclude_folders = options.exclude_folders.clone();
        status.exclude_extensions = options.exclude_extensions.clone();
        // A fresh build restarts the live scan counters from zero so a stale
        // count from the previous build never flashes before the first poll.
        status.scanned_entries = 0;
        status.skipped_files = 0;
        status.last_error = None;
        status.diagnostics.index_worker = "starting".to_string();
        status.diagnostics.watcher_worker = "stopped".to_string();
        status.diagnostics.watcher_strategy = "stopped during rebuild".to_string();
        status.diagnostics.performance_mode = options.performance_mode.clone();
        status.diagnostics.last_worker_message =
            Some("Index build started in isolated worker".to_string());
        status.diagnostics.last_status_at_ms = Some(unix_now_ms());
    });
    emit_progress(
        &app,
        FileSearchProgressEvent {
            stage: "indexing".to_string(),
            index_kind: "content".to_string(),
            indexed_files: 0,
            scanned_entries: 0,
            skipped_files: 0,
            current_path: String::new(),
            finished: false,
            canceled: false,
            success: false,
            message: "Index build started in isolated worker".to_string(),
            partial: false,
        },
    );

    start_index_worker_monitor(app, worker, options);

    Ok(FileSearchBuildResult {
        success: true,
        canceled: false,
        background: true,
        indexed_files: 0,
        scanned_entries: 0,
        skipped_files: 0,
        message: "Index build started in isolated worker".to_string(),
    })
}

#[derive(Clone, Copy)]
enum WorkerProcessKind {
    Index,
    Watch,
    FilenameIndex,
}

#[cfg(target_os = "windows")]
fn configure_worker_process(
    command: &mut Command,
    performance_mode: &str,
    worker_kind: WorkerProcessKind,
) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const IDLE_PRIORITY_CLASS: u32 = 0x00000040;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x00004000;
    const NORMAL_PRIORITY_CLASS: u32 = 0x00000020;

    let mode = normalize_index_performance_mode(performance_mode);
    let priority = match (mode.as_str(), worker_kind) {
        ("fast", WorkerProcessKind::Index) => NORMAL_PRIORITY_CLASS,
        (_, WorkerProcessKind::Watch) => IDLE_PRIORITY_CLASS,
        ("quiet", _) => IDLE_PRIORITY_CLASS,
        _ => BELOW_NORMAL_PRIORITY_CLASS,
    };
    command.creation_flags(CREATE_NO_WINDOW | priority);
}

#[cfg(not(target_os = "windows"))]
fn configure_worker_process(
    _command: &mut Command,
    _performance_mode: &str,
    _worker_kind: WorkerProcessKind,
) {
}

/// A Windows Job Object with a hard per-process memory cap — used to sandbox
/// the out-of-process content extractor children (Phase 4, task 11). The OS
/// terminates any assigned process that exceeds the cap, so a pathological file
/// can only ever take down one extractor child, never the whole app. The
/// `KILL_ON_JOB_CLOSE` flag means dropping this handle also kills every child
/// still assigned to it — a parent crash cannot leave orphan extractors.
#[cfg(windows)]
struct MemoryCappedJob {
    handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl MemoryCappedJob {
    /// Create a job object capping every assigned process at `memory_limit_bytes`.
    fn new(memory_limit_bytes: usize) -> Result<Self, String> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        };

        // SAFETY: standard Win32 job-object creation; the returned handle is
        // owned by `self` and closed exactly once in `Drop`.
        let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|error| format!("Cannot create extractor job object: {error}"))?;

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        info.ProcessMemoryLimit = memory_limit_bytes;

        // SAFETY: `info` is a fully-initialized struct of the size declared for
        // the `JobObjectExtendedLimitInformation` class.
        let applied = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if let Err(error) = applied {
            // SAFETY: closing the handle just created, before discarding it.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(format!("Cannot set extractor job memory cap: {error}"));
        }

        Ok(Self { handle })
    }

    /// A `Copy`, non-owning handle to this job, for assigning children to it
    /// from the extractor slot threads (including respawned children). The job
    /// stays owned — and closed — by this `MemoryCappedJob`.
    fn assigner(&self) -> JobAssigner {
        JobAssigner(self.handle.0)
    }
}

#[cfg(windows)]
impl Drop for MemoryCappedJob {
    fn drop(&mut self) {
        // SAFETY: `self.handle` was created in `new` and is closed exactly once
        // here. With KILL_ON_JOB_CLOSE this also terminates any still-assigned
        // extractor child, so no orphan processes survive the parent.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// Non-Windows stand-in for `MemoryCappedJob`: there is no Job Object on other
/// platforms, so extractor children run un-capped (the in-child size guard from
/// task #1 stays the safety net). Keeps `ExtractorPool` cross-platform.
#[cfg(not(windows))]
struct MemoryCappedJob;

#[cfg(not(windows))]
impl MemoryCappedJob {
    fn new(_memory_limit_bytes: usize) -> Result<Self, String> {
        Ok(Self)
    }
    fn assigner(&self) -> JobAssigner {
        JobAssigner
    }
}

/// A `Copy`, non-owning handle to the pool's Job Object. Each extractor slot
/// thread holds one so it can place a respawned child under the same memory
/// cap. The Job Object is owned (and closed) by the pool's single
/// `MemoryCappedJob`; once that handle is closed `assign` simply fails, which a
/// slot treats as a respawn failure.
#[cfg(windows)]
#[derive(Clone, Copy)]
struct JobAssigner(isize);

#[cfg(windows)]
impl JobAssigner {
    /// Place a freshly-spawned child under the job's per-process memory cap.
    fn assign(&self, child: &std::process::Child) -> Result<(), String> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::AssignProcessToJobObject;

        // SAFETY: `child` is a live process just spawned; `self.0` is the job
        // handle the pool created. AssignProcessToJobObject validates both and
        // returns an error (never UB) if the job handle has been closed.
        let process = HANDLE(child.as_raw_handle() as isize);
        unsafe { AssignProcessToJobObject(HANDLE(self.0), process) }
            .map_err(|error| format!("Cannot assign extractor child to job object: {error}"))
    }
}

/// Non-Windows stand-in for `JobAssigner`: no Job Object, so `assign` is a
/// no-op and extractor children run un-capped (the in-child size guard from
/// task #1 stays the safety net).
#[cfg(not(windows))]
#[derive(Clone, Copy)]
struct JobAssigner;

#[cfg(not(windows))]
impl JobAssigner {
    fn assign(&self, _child: &std::process::Child) -> Result<(), String> {
        Ok(())
    }
}

/// Hard per-extractor-child memory cap. A child that exceeds this — a
/// pathological file parsing into a memory bomb — is killed by the OS, and that
/// file is then indexed by name only. Sized to fit normal PDF/Office parsing
/// while still bounding a small pool on a 4–8 GB machine.
const EXTRACTOR_CHILD_MEMORY_CAP_BYTES: usize = 512 * 1024 * 1024;
/// Bounded paths channel — backpressure: the walker blocks submitting paths
/// once the pool is this far ahead of the children.
const EXTRACTOR_POOL_PATH_BUFFER: usize = 64;
/// Bounded results channel — caps how much extracted text waits to be indexed.
const EXTRACTOR_POOL_RESULT_BUFFER: usize = 16;
/// Global cap on extractor-child respawns for one index build. A healthy build
/// needs zero; a pathological one (memory-bomb files, a broken extractor) is
/// bounded here so a systemic failure cannot spawn processes without end. Once
/// it is reached, a slot whose child dies just exits and the pool shrinks.
const EXTRACTOR_POOL_MAX_RESPAWNS: usize = 32;

/// One finished extraction handed back from a pool child.
struct ExtractResult {
    path: String,
    /// `Some` = extracted text. `None` = unsupported format, over the size
    /// guard, an extraction failure, or the child died mid-extraction — in
    /// every case the caller indexes the file by name only.
    text: Option<String>,
}

/// A pool of out-of-process content extractor children (Phase 4, task 11). The
/// content-index build's walker submits document file paths; the children
/// extract text in parallel under an OS-enforced per-process memory cap; the
/// results stream back for the build to index.
///
/// `new` returns the pool plus the two channel ends — clone the `SyncSender`
/// for each producer (walker thread) and drain the `Receiver` on the indexing
/// side. The result channel closes (ending the drain) once every path sender
/// is dropped and all in-flight extractions have completed. The pool value only
/// has to be kept alive for the duration: it owns the one Job Object, so
/// dropping it terminates every extractor child still running.
struct ExtractorPool {
    _job: MemoryCappedJob,
}

/// Read one result frame from a child's stdout — `[u32 path_len][path][u8 ok]
/// [u32 text_len][text]`, little-endian. `None` on EOF or a malformed/partial
/// frame, i.e. the child has exited (cleanly or OS-killed).
fn read_extract_frame(reader: &mut impl std::io::Read) -> Option<(String, Option<String>)> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).ok()?;
    let path_len = u32::from_le_bytes(len_buf) as usize;
    if path_len > 64 * 1024 {
        return None;
    }
    let mut path_bytes = vec![0u8; path_len];
    reader.read_exact(&mut path_bytes).ok()?;
    let path = String::from_utf8(path_bytes).ok()?;

    let mut ok_buf = [0u8; 1];
    reader.read_exact(&mut ok_buf).ok()?;

    reader.read_exact(&mut len_buf).ok()?;
    let text_len = u32::from_le_bytes(len_buf) as usize;
    if text_len > 64 * 1024 * 1024 {
        return None;
    }
    let mut text_bytes = vec![0u8; text_len];
    reader.read_exact(&mut text_bytes).ok()?;

    let text = if ok_buf[0] == 1 {
        Some(String::from_utf8_lossy(&text_bytes).into_owned())
    } else {
        None
    };
    Some((path, text))
}

/// Spawn one extractor child (`--dash-index-worker extract`) with piped
/// stdin/stdout, no console window, and a process priority matching the build's
/// performance mode (fast → normal, balanced → below-normal, quiet → idle) so a
/// fast build's extractor children are not needlessly throttled.
fn spawn_extractor_child(
    max_bytes: usize,
    performance_mode: &str,
    ocr_env: &super::text_extract::OcrChildEnv,
) -> Result<std::process::Child, String> {
    let mut command = Command::new(
        std::env::current_exe()
            .map_err(|error| format!("Cannot find this program's executable: {error}"))?,
    );
    command
        .arg(INDEX_WORKER_ARG)
        .arg("extract")
        .arg("--max-bytes")
        .arg(max_bytes.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Wave 8 / task #88 (2026-05-28): set DASH_OCR_* env vars on the
    // child Command — children inherit them and OcrIndexConfig::from_env()
    // in text_extract.rs reads them once on first PDF. No-op when OCR is
    // disabled (apply_to does nothing on an empty OcrChildEnv).
    ocr_env.apply_to(&mut command);
    configure_worker_process(&mut command, performance_mode, WorkerProcessKind::Index);
    command
        .spawn()
        .map_err(|error| format!("Cannot start extractor child: {error}"))
}

/// What one watched extraction ended with.
enum ExtractAnswer {
    /// The child answered (`text` `None`: no content for that file).
    Done(String, Option<String>),
    /// The child died mid-file (crash, memory cap); it has been reaped.
    Died,
    /// The child ran past the per-file timeout and was killed and reaped.
    TimedOut,
}

/// An extractor child whose answers are read on a helper thread, so the
/// caller can stop waiting after a per-file timeout and kill it (a parser
/// stuck in a loop would otherwise stall its caller forever). The wait
/// doesn't rely on the pipe closing: a grandchild (tesseract) can hold it
/// open after the child is killed.
struct WatchedChild {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    /// One result frame per request; disconnected once stdout ends.
    frames: std::sync::mpsc::Receiver<(String, Option<String>)>,
}

impl WatchedChild {
    fn new(child: Child, stdin: std::process::ChildStdin, stdout: std::process::ChildStdout) -> Self {
        let (frame_tx, frames) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut stdout = std::io::BufReader::new(stdout);
            while let Some(frame) = read_extract_frame(&mut stdout) {
                if frame_tx.send(frame).is_err() {
                    break;
                }
            }
        });
        WatchedChild { child, stdin: Some(stdin), frames }
    }

    /// Asks the child for one file's text (write the path to its stdin, take
    /// the result frame), waiting at most `timeout`. After `Died` or
    /// `TimedOut` the child is gone; the caller replaces it.
    fn extract(&mut self, path: &Path, timeout: std::time::Duration) -> ExtractAnswer {
        use std::io::Write;
        use std::sync::mpsc::RecvTimeoutError;
        let line = format!("{}\n", path.to_string_lossy());
        let sent = match self.stdin.as_mut() {
            Some(stdin) => stdin.write_all(line.as_bytes()).and_then(|()| stdin.flush()).is_ok(),
            None => false,
        };
        if !sent {
            // Can't reach it: make sure it's gone, then reap.
            let _ = self.child.kill();
            let _ = self.child.wait();
            return ExtractAnswer::Died;
        }
        match self.frames.recv_timeout(timeout) {
            Ok((answered_path, text)) => ExtractAnswer::Done(answered_path, text),
            Err(RecvTimeoutError::Disconnected) => {
                // Usually already exited; a garbled frame leaves it running.
                let _ = self.child.kill();
                let _ = self.child.wait();
                ExtractAnswer::Died
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                ExtractAnswer::TimedOut
            }
        }
    }
}

impl Drop for WatchedChild {
    /// Closes stdin (an idle child exits at EOF) and reaps the child.
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

/// Spawn one extractor child, place it under the pool's memory-capped job, and
/// take its stdin/stdout pipes. Used both for the pool's initial children and
/// for respawning a child that died.
fn spawn_assigned_extractor_child(
    max_bytes: usize,
    performance_mode: &str,
    assigner: JobAssigner,
    ocr_env: &super::text_extract::OcrChildEnv,
) -> Result<
    (
        std::process::Child,
        std::process::ChildStdin,
        std::process::ChildStdout,
    ),
    String,
> {
    let mut child = spawn_extractor_child(max_bytes, performance_mode, ocr_env)?;
    assigner.assign(&child)?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Extractor child has no stdin pipe".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Extractor child has no stdout pipe".to_string())?;
    Ok((child, stdin, stdout))
}

/// Body of one extractor slot thread. The slot owns its child end-to-end —
/// stdin, stdout, lifecycle — so recovering from a child death is entirely
/// local: it reports the lost file as name-only and respawns a replacement
/// under the same job, keeping the pool at full strength. It signals readiness
/// on `free_tx` and receives one path at a time on `work_rx`.
fn extractor_slot(
    index: usize,
    max_bytes: usize,
    performance_mode: String,
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    work_rx: std::sync::mpsc::Receiver<PathBuf>,
    result_tx: std::sync::mpsc::SyncSender<ExtractResult>,
    free_tx: std::sync::mpsc::Sender<usize>,
    assigner: JobAssigner,
    respawn_budget: Arc<std::sync::atomic::AtomicUsize>,
    should_stop: Arc<AtomicBool>,
    ocr_env: super::text_extract::OcrChildEnv,
) {
    // The child is reaped when `watched` is dropped or replaced.
    let mut watched = WatchedChild::new(child, stdin, stdout);
    let timeout = ocr_env.extract_timeout();

    // Announce readiness for the first path.
    if free_tx.send(index).is_err() {
        return;
    }

    while let Ok(path) = work_rx.recv() {
        let timed_out = match watched.extract(&path, timeout) {
            ExtractAnswer::Done(result_path, text) => {
                // The child answered — forward the result and ask for more work.
                let delivered = result_tx
                    .send(ExtractResult {
                        path: result_path,
                        text,
                    })
                    .is_ok();
                if !delivered || free_tx.send(index).is_err() {
                    return;
                }
                continue;
            }
            ExtractAnswer::Died => false,
            ExtractAnswer::TimedOut => true,
        };

        // The child died (or was killed for taking too long) and has been
        // reaped. If the build is aborting (the pool was dropped to hard-kill
        // children) exit quietly — no name-only result, no respawn.
        if should_stop.load(Ordering::Relaxed) {
            return;
        }

        // A live build: index the lost file by name only, then recover the
        // slot. The toxic file is never retried — the walk submits each path
        // exactly once and the replacement child resumes from the next
        // `work_rx` item — so one bad file can take down at most one child.
        let _ = result_tx.send(ExtractResult {
            path: path.to_string_lossy().into_owned(),
            text: None,
        });
        if timed_out {
            eprintln!(
                "dash-search: content extractor took over {}s on {} — indexed by name only",
                timeout.as_secs(),
                path.display()
            );
        } else {
            eprintln!(
                "dash-search: content extractor child stopped on {} — indexed by name only",
                path.display()
            );
        }

        // Respawn within the global budget so the pool stays at full strength.
        if respawn_budget
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(1)
            })
            .is_err()
        {
            eprintln!("dash-search: content extractor respawn budget spent — pool shrinking");
            return;
        }
        match spawn_assigned_extractor_child(max_bytes, &performance_mode, assigner, &ocr_env) {
            Ok((new_child, new_stdin, new_stdout)) => {
                watched = WatchedChild::new(new_child, new_stdin, new_stdout);
                if free_tx.send(index).is_err() {
                    return;
                }
            }
            Err(error) => {
                eprintln!(
                    "dash-search: content extractor child could not be respawned ({error}) — pool shrinking"
                );
                return;
            }
        }
    }

    // `work_rx` closed — the build is done. Dropping `watched` closes the
    // child's stdin (it exits at EOF) and reaps it so it does not linger.
}

impl ExtractorPool {
    /// Build a pool of `child_count` extractor children (clamped to 1..=8).
    /// Returns the pool, the path `SyncSender` (clone one per producer), and the
    /// result `Receiver` (drain on the indexing side). `should_stop` is the
    /// build's abort flag: when it is set, a slot whose child dies exits instead
    /// of respawning, because the build is tearing the pool down.
    ///
    /// Wave 8.5 (2026-05-28): when `cache_ctx` is `Some`, the dispatcher
    /// content-hashes OCR-eligible files (images + PDFs) before sending them
    /// to a slot and looks up `(hash, lang)` in the persistent OCR cache. On
    /// hit, the cached text bypasses the slot entirely — zero subprocess work
    /// for byte-identical duplicates across folders or rebuilds. On miss, the
    /// slot processes normally and a forwarder thread stores the result back
    /// to the cache. `None` (OCR off) keeps the pool's original behavior:
    /// no hashing, no redb writes, no overhead.
    fn new(
        child_count: usize,
        max_bytes: usize,
        // Per-child Job Object memory cap in bytes. Normally
        // `EXTRACTOR_CHILD_MEMORY_CAP_BYTES`, but `clamp_pool_for_available_ram`
        // reduces it on low-RAM machines.
        memory_cap_bytes: usize,
        performance_mode: &str,
        should_stop: Arc<AtomicBool>,
        ocr_env: super::text_extract::OcrChildEnv,
        cache_ctx: Option<super::ocr_cache::OcrCacheCtx>,
    ) -> Result<
        (
            ExtractorPool,
            std::sync::mpsc::SyncSender<PathBuf>,
            std::sync::mpsc::Receiver<ExtractResult>,
        ),
        String,
    > {
        use std::collections::HashMap;
        use std::sync::atomic::AtomicUsize;
        use std::sync::mpsc::{channel, sync_channel};

        let child_count = child_count.clamp(1, 8);

        // One Job Object for the whole pool. `JOB_OBJECT_LIMIT_PROCESS_MEMORY`
        // caps each assigned process individually; every child — original or
        // respawned — gets the same ceiling. `KILL_ON_JOB_CLOSE` terminates
        // all children when the pool drops. The cap is profile-aware:
        // `clamp_pool_for_available_ram` reduces it on low-RAM machines so we
        // never commit N × 512 MB on a 4 GB laptop.
        let job = MemoryCappedJob::new(memory_cap_bytes)?;
        let assigner = job.assigner();

        let (path_tx, path_rx) = sync_channel::<PathBuf>(EXTRACTOR_POOL_PATH_BUFFER);
        // Wave 8.5 split: slots send to `inner_result_tx`. The forwarder
        // thread (below) reads `inner_result_rx`, stores any cacheable text
        // back to redb, and forwards the result on `outer_result_tx` — which
        // is the receiver the caller sees. When `cache_ctx` is `None` the
        // forwarder is still in the loop but does nothing besides forward,
        // costing one extra channel hop per result (microseconds, dwarfed
        // by extraction time).
        let (inner_result_tx, inner_result_rx) =
            sync_channel::<ExtractResult>(EXTRACTOR_POOL_RESULT_BUFFER);
        let (outer_result_tx, outer_result_rx) =
            sync_channel::<ExtractResult>(EXTRACTOR_POOL_RESULT_BUFFER);
        let (free_tx, free_rx) = channel::<usize>();
        let respawn_budget = Arc::new(AtomicUsize::new(EXTRACTOR_POOL_MAX_RESPAWNS));

        // Wave 8.5: dispatcher records `(path_string, content_hash)` here
        // for every cache MISS it sent to a slot; the forwarder pops the
        // hash when the slot answers, so cache stores only happen for
        // misses we actually wanted to cache. Wrapped in a Mutex because
        // the dispatcher (writer) and forwarder (reader/remover) run on
        // separate threads.
        let pending_hashes: Arc<Mutex<HashMap<String, super::ocr_cache::ContentHash>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // One `work` channel per slot. The dispatcher routes each path to the
        // slot it just pulled from `free`; each slot thread fully owns a child.
        let mut work_txs = Vec::with_capacity(child_count);
        for index in 0..child_count {
            let (child, stdin, stdout) =
                spawn_assigned_extractor_child(max_bytes, performance_mode, assigner, &ocr_env)?;
            let (work_tx, work_rx) = channel::<PathBuf>();
            work_txs.push(work_tx);

            let inner_result_tx = inner_result_tx.clone();
            let free_tx = free_tx.clone();
            let respawn_budget = Arc::clone(&respawn_budget);
            let should_stop = Arc::clone(&should_stop);
            let performance_mode = performance_mode.to_string();
            // Wave 8 / task #88 (2026-05-28): each slot owns a clone of
            // the OCR env so respawns inside `extractor_slot` keep the
            // same OCR config the user opted into at build start.
            let ocr_env = ocr_env.clone();
            thread::spawn(move || {
                extractor_slot(
                    index,
                    max_bytes,
                    performance_mode,
                    child,
                    stdin,
                    stdout,
                    work_rx,
                    inner_result_tx,
                    free_tx,
                    assigner,
                    respawn_budget,
                    should_stop,
                    ocr_env,
                );
            });
        }

        // Wave 8.5 forwarder thread. Sits between slots/dispatcher and the
        // caller's result receiver. For each result on `inner_result_rx`:
        //  - Pop any pending hash for this path (so misses don't leak).
        //  - If we had a hash AND the slot returned text, store
        //    `(hash, lang) → text` in the persistent cache.
        //  - Forward the result to the caller unchanged.
        //
        // The thread exits when `inner_result_rx` closes — that happens once
        // every `inner_result_tx` clone has been dropped (slots' clones drop
        // when slots exit; the dispatcher's clone drops when the dispatcher
        // exits; the original is dropped immediately below). Dropping the
        // last `outer_result_tx` then closes the caller's receiver too.
        let dispatcher_inner_result_tx = inner_result_tx.clone();
        {
            let cache_ctx = cache_ctx.clone();
            let pending_hashes = Arc::clone(&pending_hashes);
            thread::spawn(move || {
                while let Ok(result) = inner_result_rx.recv() {
                    let hash = pending_hashes
                        .lock()
                        .ok()
                        .and_then(|mut p| p.remove(&result.path));
                    if let (Some(ctx), Some(hash), Some(text)) =
                        (cache_ctx.as_ref(), hash, result.text.as_deref())
                    {
                        // We store even empty text — that's a legitimate
                        // "extracted nothing useful" answer for a file we'd
                        // otherwise burn CPU on every rebuild.
                        super::ocr_cache::store(ctx, &hash, text);
                    }
                    if outer_result_tx.send(result).is_err() {
                        // Caller dropped the receiver — nothing to forward to.
                        return;
                    }
                }
                drop(outer_result_tx);
            });
        }
        drop(inner_result_tx);

        // Dispatcher thread: routes each submitted path to the next idle slot.
        // It is the single consumer of `path_rx`; demand-driven via `free`, so
        // a slot stuck on a slow file never holds up the others.
        //
        // Wave 8.5: when `cache_ctx` is set and the file is OCR-eligible,
        // BLAKE3-hash the bytes and look up `(hash, lang)` in the cache.
        //  - HIT  → emit a synthetic result on `dispatcher_inner_result_tx`
        //           so the forwarder sees a normal event stream. No pending
        //           entry → forwarder doesn't double-cache. The slot is
        //           never touched, saving the full extraction round-trip.
        //  - MISS → record the hash in `pending_hashes`; fall through to
        //           normal slot dispatch. The forwarder will store the
        //           slot's text under that hash when it arrives.
        let cache_ctx_for_dispatcher = cache_ctx.clone();
        let pending_hashes_for_dispatcher = Arc::clone(&pending_hashes);
        thread::spawn(move || {
            while let Ok(path) = path_rx.recv() {
                if let Some(ctx) = cache_ctx_for_dispatcher.as_ref() {
                    if super::ocr_cache::is_cache_eligible_path(&path) {
                        if let Ok(hash) = super::ocr_cache::hash_file(&path) {
                            if let Some(cached_text) = super::ocr_cache::lookup(ctx, &hash) {
                                let path_string = path.to_string_lossy().into_owned();
                                if dispatcher_inner_result_tx
                                    .send(ExtractResult {
                                        path: path_string,
                                        text: Some(cached_text),
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                                continue;
                            }
                            if let Ok(mut p) = pending_hashes_for_dispatcher.lock() {
                                p.insert(path.to_string_lossy().into_owned(), hash);
                            }
                        }
                    }
                }

                let Ok(slot) = free_rx.recv() else {
                    // No slot will ever free again — every slot thread has
                    // exited (respawn budget spent). Stop dispatching.
                    return;
                };
                // A slot pulled from `free` is alive and waiting, so this send
                // succeeds; if it somehow does not, the path is simply skipped
                // (still discoverable via the filename index).
                let _ = work_txs[slot].send(path);
            }
            // `path_rx` closed — the walk is done. Dropping every `work`
            // sender closes the live slots' `work_rx` so they finish and
            // exit. Dropping `dispatcher_inner_result_tx` (held by this
            // closure) plus the slots' clones is what eventually closes
            // `inner_result_rx` for the forwarder.
            drop(work_txs);
            drop(dispatcher_inner_result_tx);
        });

        Ok((ExtractorPool { _job: job }, path_tx, outer_result_rx))
    }
}

fn start_index_worker_monitor(
    app: std::sync::Arc<dyn dash_core::Host>,
    worker: SearchIndexWorkerProcess,
    options: FileSearchIndexOptions,
) {
    thread::spawn(move || {
        let mut last_indexed = 0u64;
        loop {
            if let Ok(Some(status)) = read_index_worker_status(&worker.status_path) {
                last_indexed = status.indexed_files;
                apply_index_worker_status(&app, &options, &status);
            }

            let exited = worker
                .child
                .lock()
                .map(|mut child| child.try_wait().ok().flatten())
                .unwrap_or(None);
            if exited.is_some() {
                let mut final_status = read_index_worker_status(&worker.status_path)
                    .ok()
                    .flatten()
                    .unwrap_or(IndexWorkerStatusFile {
                        running: false,
                        finished: true,
                        success: false,
                        canceled: false,
                        indexed_files: last_indexed,
                        scanned_entries: 0,
                        skipped_files: 0,
                        message: "Index worker exited without a final status".to_string(),
                        updated_at_ms: unix_now_ms(),
                        error: Some("Index worker exited without a final status".to_string()),
                        partial: false,
                    });
                if !final_status.finished {
                    let was_canceled = worker.cancel_path.exists();
                    let message = if was_canceled {
                        "Index build cancelled".to_string()
                    } else {
                        "Index worker exited before finishing".to_string()
                    };
                    final_status = IndexWorkerStatusFile::finished(
                        false,
                        was_canceled,
                        final_status.indexed_files,
                        final_status.scanned_entries,
                        final_status.skipped_files,
                        message.clone(),
                        if was_canceled { None } else { Some(message) },
                    );
                    let _ = write_index_worker_status(&worker.status_path, &final_status);
                }
                apply_index_worker_status(&app, &options, &final_status);
                if final_status.success {
                    // The worker can't write the settings database while this
                    // process holds it, so record the finished build here.
                    let rebuild_schedule = read_search_config(&app)
                        .ok()
                        .flatten()
                        .map(|config| config.rebuild_schedule)
                        .unwrap_or_default();
                    let _ = write_search_config(
                        &app,
                        &StoredSearchConfig {
                            options: options.clone(),
                            indexed_files: final_status.indexed_files,
                            last_indexed_at_ms: Some(final_status.updated_at_ms),
                            rebuild_schedule,
                        },
                    );
                    let _ = ensure_engine_loaded(&app);
                }
                if let Ok(mut lock) = SEARCH_INDEX_WORKER.lock() {
                    *lock = None;
                }
                let _ = fs::remove_file(&worker.cancel_path);
                break;
            }

            thread::sleep(Duration::from_millis(700));
        }
    });
}

/// Spawn the standalone filename indexer as an isolated subprocess (Phase 3,
/// step 7b). Returns `Ok(true)` when a worker was started, `Ok(false)` when the
/// run was skipped. Safe to run alongside a content build.
///
/// `force` decides what happens when a filename worker is already running —
/// which, post-9d, is the long-lived USN tailer. A user-triggered rebuild
/// passes `force = true`: the running tailer is stopped so the index can be
/// rebuilt for the current roots. The scheduler passes `force = false`: a
/// running tailer is left alone, since it already keeps the index live.
fn start_isolated_filename_index_worker(
    app: &std::sync::Arc<dyn dash_core::Host>,
    mut options: FileSearchIndexOptions,
    force: bool,
) -> Result<bool, String> {
    // The filename index indexes the File Search tab's sources
    // (`filename_roots`), stored separately from the content index's `roots`.
    options.roots = std::mem::take(&mut options.filename_roots);
    // The filename index ("My engine" / MFT) records names and paths only — its
    // schema has no content field, so it never extracts text. Pin the flag off
    // so the worker options file plainly reflects that.
    options.index_content = false;
    let options = normalize_index_options(options)?;
    if options.roots.is_empty() {
        return Ok(false);
    }

    let state_dir = search_index_dir(app)?;
    fs::create_dir_all(&state_dir)
        .map_err(|error| format!("Cannot create search index state directory: {error}"))?;

    // A filename worker may already be running — post-9d that is the long-lived
    // tailer. Without `force`, leave it be; with `force` (a user rebuild), stop
    // it so the index can be rebuilt for the current roots.
    {
        let mut worker_lock = FILENAME_INDEX_WORKER
            .lock()
            .map_err(|_| "Filename index worker lock failed".to_string())?;
        if let Some(worker) = worker_lock.as_ref() {
            let still_running = worker
                .child
                .lock()
                .map(|mut child| child.try_wait().ok().flatten().is_none())
                .unwrap_or(false);
            if still_running {
                if !force {
                    return Ok(false);
                }
                // Stop the running worker / tailer: drop the cancel file so a
                // tailer mid-loop exits cleanly, and kill the child outright
                // so the rebuild can proceed without waiting.
                let _ = fs::write(&worker.cancel_path, b"cancel");
                if let Ok(mut child) = worker.child.lock() {
                    let _ = child.kill();
                }
            }
            *worker_lock = None;
        }
    }

    // Release the cached filename-index mmap so the worker's directory-rename
    // swap can succeed on Windows (an open mmap blocks the rename). The monitor
    // re-opens the handle once the build's status reports success.
    if let Ok(mut guard) = FILENAME_ENGINE.lock() {
        *guard = None;
    }

    let options_path = state_dir.join(FILENAME_WORKER_OPTIONS_FILE);
    let status_path = state_dir.join(FILENAME_WORKER_STATUS_FILE);
    let cancel_path = state_dir.join(FILENAME_WORKER_CANCEL_FILE);
    let options_bytes = serde_json::to_vec_pretty(&options)
        .map_err(|error| format!("Cannot serialize filename worker options: {error}"))?;
    fs::write(&options_path, options_bytes)
        .map_err(|error| format!("Cannot write filename worker options: {error}"))?;
    request_stale_worker_stop(&status_path, &cancel_path);
    let _ = fs::remove_file(&cancel_path);
    let _ = fs::remove_file(&status_path);
    write_index_worker_status(
        &status_path,
        &IndexWorkerStatusFile::progress(0, 0, 0, "Starting filename indexer".to_string()),
    )?;

    let mut command = Command::new(
        std::env::current_exe()
            .map_err(|error| format!("Cannot find this program's executable: {error}"))?,
    );
    command
        .arg(INDEX_WORKER_ARG)
        .arg("filename-index")
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--options")
        .arg(&options_path)
        .arg("--status")
        .arg(&status_path)
        .arg("--cancel")
        .arg(&cancel_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_worker_process(
        &mut command,
        &options.performance_mode,
        WorkerProcessKind::FilenameIndex,
    );

    let child = command
        .spawn()
        .map_err(|error| format!("Cannot start filename indexer: {error}"))?;
    let worker = SearchIndexWorkerProcess {
        child: Arc::new(Mutex::new(child)),
        status_path,
        cancel_path,
    };

    {
        let mut worker_lock = FILENAME_INDEX_WORKER
            .lock()
            .map_err(|_| "Filename index worker lock failed".to_string())?;
        *worker_lock = Some(worker.clone());
    }

    // Mark the filename build in progress and raise the live "Indexing in
    // progress" panel immediately — before the monitor's first poll — so the
    // File Search tab reflects the build the same way the Content tab does.
    update_status(|status| {
        status.filename_indexing = true;
        status.filename_indexed_files = 0;
        // A fresh build restarts the live scan counters from zero.
        status.scanned_entries = 0;
        status.skipped_files = 0;
    });
    emit_progress(
        app,
        FileSearchProgressEvent {
            stage: "indexing".to_string(),
            index_kind: "filename".to_string(),
            indexed_files: 0,
            scanned_entries: 0,
            skipped_files: 0,
            current_path: String::new(),
            finished: false,
            canceled: false,
            success: false,
            message: "Filename index build started".to_string(),
            partial: false,
        },
    );
    start_filename_index_worker_monitor(app.clone(), worker, state_dir);
    Ok(true)
}

/// Mirror one filename-build status update into `SEARCH_STATUS` and emit a
/// `file-search-index-progress` event — the filename-build twin of
/// `apply_index_worker_status`, so the File Search tab shows the same live
/// "Indexing in progress" panel + counters the Content tab shows. Only called
/// while the *build* is in flight; once the worker becomes the USN tailer /
/// notify watcher its messages are surfaced as the secondary status line.
fn apply_filename_index_worker_status(app: &std::sync::Arc<dyn dash_core::Host>, status_file: &IndexWorkerStatusFile) {
    let finished = status_file.finished;
    let message = status_file
        .error
        .clone()
        .unwrap_or_else(|| status_file.message.clone());
    update_status(|status| {
        status.filename_indexing = !finished;
        status.filename_indexed_files = status_file.indexed_files;
        // Live scan counters — keep `get_file_search_status` in sync with the
        // progress event so a mid-build view shows real numbers, not 0/0/0.
        status.scanned_entries = status_file.scanned_entries;
        status.skipped_files = status_file.skipped_files;
        if finished && status_file.success {
            status.filename_last_indexed_at_ms = Some(status_file.updated_at_ms);
        }
        status.diagnostics.last_status_at_ms = Some(status_file.updated_at_ms);
    });
    emit_progress(
        app,
        FileSearchProgressEvent {
            stage: "indexing".to_string(),
            index_kind: "filename".to_string(),
            indexed_files: status_file.indexed_files,
            scanned_entries: status_file.scanned_entries,
            skipped_files: status_file.skipped_files,
            current_path: String::new(),
            finished,
            canceled: status_file.canceled,
            success: status_file.success,
            message,
            partial: false,
        },
    );
}

/// Watch the filename indexer subprocess. Relays the *build's* progress to the
/// UI (the same live panel the content build shows) until the build's status
/// reports `finished`; after that the worker lives on as the USN tailer / notify
/// watcher, so relaying stops. Also re-opens the cached filename handle once the
/// build succeeds and clears the worker slot on exit.
fn start_filename_index_worker_monitor(
    app: std::sync::Arc<dyn dash_core::Host>,
    worker: SearchIndexWorkerProcess,
    state_dir: PathBuf,
) {
    thread::spawn(move || {
        let reopen_filename_handle = || {
            if let Ok(mut guard) = FILENAME_ENGINE.lock() {
                *guard = try_open_filename_index(&state_dir);
            }
        };
        let mut handle_reopened = false;
        // Latches once the build's status reports `finished`. After that the
        // worker keeps running as the live updater — its further status writes
        // are not "a build in progress" and must not re-raise the UI panel.
        let mut build_finished = false;
        let mut last_emitted_ms: u128 = 0;
        loop {
            let status = read_index_worker_status(&worker.status_path)
                .ok()
                .flatten();

            if let Some(status_file) = &status {
                if !build_finished && status_file.updated_at_ms != last_emitted_ms {
                    last_emitted_ms = status_file.updated_at_ms;
                    apply_filename_index_worker_status(&app, status_file);
                    if status_file.finished {
                        build_finished = true;
                    }
                }
                if !handle_reopened && status_file.finished && status_file.success {
                    reopen_filename_handle();
                    handle_reopened = true;
                }
            }

            let exited = worker
                .child
                .lock()
                .map(|mut child| child.try_wait().ok().flatten())
                .unwrap_or(None);
            if exited.is_some() {
                // A build that never transitioned to a live updater (the walker
                // path with no watchable roots) reaches success only at exit.
                if !handle_reopened
                    && read_index_worker_status(&worker.status_path)
                        .ok()
                        .flatten()
                        .map(|status| status.finished && status.success)
                        .unwrap_or(false)
                {
                    reopen_filename_handle();
                }
                // If the worker died before its build reported `finished`
                // (killed, crashed, or cancelled), close out the UI so the
                // "Indexing in progress" panel does not stick on screen.
                if !build_finished {
                    let cancelled = worker.cancel_path.exists();
                    let message = if cancelled {
                        "Filename index build cancelled".to_string()
                    } else {
                        "Filename indexer exited before finishing".to_string()
                    };
                    update_status(|status| status.filename_indexing = false);
                    emit_progress(
                        &app,
                        FileSearchProgressEvent {
                            stage: "indexing".to_string(),
                            index_kind: "filename".to_string(),
                            indexed_files: 0,
                            scanned_entries: 0,
                            skipped_files: 0,
                            current_path: String::new(),
                            finished: true,
                            canceled: cancelled,
                            success: false,
                            message,
                            partial: false,
                        },
                    );
                }
                if let Ok(mut lock) = FILENAME_INDEX_WORKER.lock() {
                    *lock = None;
                }
                let _ = fs::remove_file(&worker.cancel_path);
                break;
            }
            thread::sleep(Duration::from_millis(700));
        }
    });
}

fn start_isolated_watch_worker(
    app: &std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchIndexOptions,
    indexed_files: u64,
) -> Result<bool, String> {
    if options.roots.is_empty() {
        return Ok(false);
    }
    if !options.watcher_enabled {
        update_status(|status| {
            status.watcher_enabled = false;
            status.watcher_paused = false;
            status.watching = false;
            status.diagnostics.watcher_worker = "stopped".to_string();
            status.diagnostics.watcher_strategy = "disabled manually".to_string();
            status.diagnostics.last_worker_message =
                Some("Live watcher is disabled in search settings".to_string());
            status.diagnostics.last_status_at_ms = Some(unix_now_ms());
        });
        return Ok(false);
    }
    if options.watcher_paused {
        update_status(|status| {
            status.watcher_enabled = true;
            status.watcher_paused = true;
            status.watching = false;
            status.diagnostics.watcher_worker = "stopped".to_string();
            status.diagnostics.watcher_strategy = "paused manually".to_string();
            status.diagnostics.last_worker_message =
                Some("Live watcher is paused until the next rebuild".to_string());
            status.diagnostics.last_status_at_ms = Some(unix_now_ms());
        });
        return Ok(false);
    }

    let state_dir = search_index_dir(app)?;
    let active_dir = active_search_index_dir_for_state(&state_dir);
    if !active_dir.exists() {
        return Ok(false);
    }

    {
        let mut worker_lock = SEARCH_WATCHER_WORKER
            .lock()
            .map_err(|_| "Search watcher worker lock failed".to_string())?;
        if let Some(worker) = worker_lock.as_ref() {
            let still_running = worker
                .child
                .lock()
                .map(|mut child| child.try_wait().ok().flatten().is_none())
                .unwrap_or(false);
            if still_running {
                return Ok(true);
            }
            *worker_lock = None;
        }
    }

    let mut options = normalize_index_options(options)?;
    let watchable_roots = watchable_index_roots(&options);
    if watchable_roots.is_empty() {
        update_status(|status| {
            status.watching = false;
            status.diagnostics.watcher_worker = "stopped".to_string();
            status.diagnostics.watcher_strategy =
                "disabled for drive roots/system roots".to_string();
            status.diagnostics.last_worker_message = Some(
                "Live watcher only works for explicit folders. Drive roots are not watched; use scheduled rebuilds or add specific folders.".to_string(),
            );
            status.diagnostics.last_status_at_ms = Some(unix_now_ms());
        });
        return Ok(false);
    }
    options.roots = watchable_roots;
    let options_path = state_dir.join(WATCHER_WORKER_OPTIONS_FILE);
    let status_path = state_dir.join(WATCHER_WORKER_STATUS_FILE);
    let cancel_path = state_dir.join(WATCHER_WORKER_CANCEL_FILE);
    let options_bytes = serde_json::to_vec_pretty(&options)
        .map_err(|error| format!("Cannot serialize watcher worker options: {error}"))?;
    fs::write(&options_path, options_bytes)
        .map_err(|error| format!("Cannot write watcher worker options: {error}"))?;
    request_stale_worker_stop(&status_path, &cancel_path);
    let _ = fs::remove_file(&cancel_path);
    let _ = fs::remove_file(&status_path);
    write_index_worker_status(
        &status_path,
        &IndexWorkerStatusFile::progress(
            0,
            indexed_files,
            0,
            "Starting isolated index watcher".to_string(),
        ),
    )?;

    let mut command = Command::new(
        std::env::current_exe()
            .map_err(|error| format!("Cannot find this program's executable: {error}"))?,
    );
    command
        .arg(INDEX_WORKER_ARG)
        .arg("watch")
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--options")
        .arg(&options_path)
        .arg("--status")
        .arg(&status_path)
        .arg("--cancel")
        .arg(&cancel_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_worker_process(
        &mut command,
        &options.performance_mode,
        WorkerProcessKind::Watch,
    );

    let child = command
        .spawn()
        .map_err(|error| format!("Cannot start isolated index watcher: {error}"))?;
    let worker = SearchIndexWorkerProcess {
        child: Arc::new(Mutex::new(child)),
        status_path: status_path.clone(),
        cancel_path: cancel_path.clone(),
    };

    {
        let mut worker_lock = SEARCH_WATCHER_WORKER
            .lock()
            .map_err(|_| "Search watcher worker lock failed".to_string())?;
        *worker_lock = Some(worker.clone());
    }

    let watcher_strategy = watcher_strategy_for_index_size(indexed_files)
        .name
        .to_string();
    update_status(|status| {
        status.watching = true;
        status.watcher_enabled = true;
        status.watcher_paused = false;
        status.diagnostics.watcher_worker = "starting".to_string();
        status.diagnostics.watcher_strategy = watcher_strategy;
        status.diagnostics.performance_mode = options.performance_mode.clone();
        status.diagnostics.last_worker_message =
            Some("Index watcher started in isolated worker".to_string());
        status.diagnostics.last_status_at_ms = Some(unix_now_ms());
    });
    emit_progress(
        app,
        FileSearchProgressEvent {
            stage: "watch".to_string(),
            index_kind: "content".to_string(),
            indexed_files: 0,
            scanned_entries: indexed_files,
            skipped_files: 0,
            current_path: String::new(),
            finished: false,
            canceled: false,
            success: false,
            message: "Index watcher started in isolated worker".to_string(),
            partial: false,
        },
    );

    start_watch_worker_monitor(app.clone(), worker, options);
    Ok(true)
}

fn stop_isolated_watch_worker_for_state(state_dir: &Path) {
    let _ = fs::write(state_dir.join(WATCHER_WORKER_CANCEL_FILE), b"cancel");
    let _ = write_index_worker_status(
        &state_dir.join(WATCHER_WORKER_STATUS_FILE),
        &IndexWorkerStatusFile::finished(
            false,
            true,
            0,
            0,
            0,
            "Index watcher stopped".to_string(),
            None,
        ),
    );
    if let Ok(mut guard) = SEARCH_WATCHER_WORKER.lock() {
        if let Some(worker) = guard.take() {
            let _ = fs::write(&worker.cancel_path, b"cancel");
            if let Ok(mut child) = worker.child.lock() {
                let _ = child.kill();
            }
        }
    }
}

fn start_watch_worker_monitor(
    app: std::sync::Arc<dyn dash_core::Host>,
    worker: SearchIndexWorkerProcess,
    options: FileSearchIndexOptions,
) {
    thread::spawn(move || {
        // Track the last emitted status timestamp so we don't fire Tauri events
        // when nothing has changed. The watcher only rewrites status_path when
        // a batch produced changes, so an unchanged timestamp means no work to report.
        let mut last_emitted_ms: u128 = 0;
        loop {
        if let Ok(Some(status)) = read_index_worker_status(&worker.status_path) {
            if status.updated_at_ms != last_emitted_ms {
                last_emitted_ms = status.updated_at_ms;
                apply_watch_worker_status(&app, &options, &status);
            }
        }

        let exited = worker
            .child
            .lock()
            .map(|mut child| child.try_wait().ok().flatten())
            .unwrap_or(None);
        if exited.is_some() {
            let mut final_status = read_index_worker_status(&worker.status_path)
                .ok()
                .flatten()
                .unwrap_or(IndexWorkerStatusFile::finished(
                    false,
                    false,
                    0,
                    0,
                    0,
                    "Index watcher exited without a final status".to_string(),
                    Some("Index watcher exited without a final status".to_string()),
                ));
            if !final_status.finished {
                let was_canceled = worker.cancel_path.exists();
                let message = if was_canceled {
                    "Index watcher stopped".to_string()
                } else {
                    "Index watcher exited before finishing".to_string()
                };
                final_status = IndexWorkerStatusFile::finished(
                    false,
                    was_canceled,
                    final_status.indexed_files,
                    final_status.scanned_entries,
                    final_status.skipped_files,
                    message.clone(),
                    if was_canceled { None } else { Some(message) },
                );
            }
            apply_watch_worker_status(&app, &options, &final_status);
            if let Ok(mut lock) = SEARCH_WATCHER_WORKER.lock() {
                *lock = None;
            }
            let _ = fs::remove_file(&worker.cancel_path);
            break;
        }

        // Poll every 3s — the watcher rate-limits to 5s minimum between applies,
        // so faster polling just wastes CPU on duplicate status file reads.
        thread::sleep(Duration::from_millis(3_000));
        }
    });
}

fn apply_watch_worker_status(
    app: &std::sync::Arc<dyn dash_core::Host>,
    options: &FileSearchIndexOptions,
    status_file: &IndexWorkerStatusFile,
) {
    let message = status_file
        .error
        .clone()
        .unwrap_or_else(|| status_file.message.clone());
    update_status(|status| {
        status.watching = status_file.running && !status_file.finished;
        status.watcher_enabled = options.watcher_enabled;
        status.watcher_paused = options.watcher_paused;
        status.updated_files = status_file.indexed_files;
        status.deleted_files = status_file.skipped_files;
        if status_file.finished && !status_file.canceled {
            status.last_error = status_file.error.clone();
        }
        status.diagnostics.watcher_worker = if status_file.running && !status_file.finished {
            "running".to_string()
        } else if status_file.canceled {
            "stopped".to_string()
        } else if status_file.success {
            "finished".to_string()
        } else {
            "stopped".to_string()
        };
        status.diagnostics.performance_mode =
            normalize_index_performance_mode(&options.performance_mode);
        status.diagnostics.last_worker_message = Some(message.clone());
        status.diagnostics.last_status_at_ms = Some(status_file.updated_at_ms);
    });

    emit_progress(
        app,
        FileSearchProgressEvent {
            stage: "watch".to_string(),
            index_kind: "content".to_string(),
            indexed_files: status_file.indexed_files,
            scanned_entries: status_file.scanned_entries,
            skipped_files: status_file.skipped_files,
            current_path: String::new(),
            finished: status_file.finished,
            canceled: status_file.canceled,
            success: false,
            message,
            partial: false,
        },
    );
}

fn apply_index_worker_status(
    app: &std::sync::Arc<dyn dash_core::Host>,
    options: &FileSearchIndexOptions,
    status_file: &IndexWorkerStatusFile,
) {
    let finished = status_file.finished;
    let message = status_file
        .error
        .clone()
        .unwrap_or_else(|| status_file.message.clone());

    // Compute the effective count BEFORE taking the write lock so we can use
    // the same value in both the status update and the emitted event.
    //
    // The count must never go backwards during an active build. The very first
    // status writes ("Starting…", "Preparing…", "Loaded cache") carry
    // indexed_files=0 and would drop both the stat card AND the progress panel
    // from e.g. 37000 → 0 before climbing back up. Rule: only advance the
    // counter when the new value is strictly higher, OR when the build is done
    // (finished=true) — so the final tally always lands, even if it's a
    // legitimate 0 on a canceled empty run.
    let prev_indexed = SEARCH_STATUS
        .lock()
        .map(|s| s.indexed_files)
        .unwrap_or(0);
    let effective_indexed = if finished || status_file.indexed_files > prev_indexed {
        status_file.indexed_files
    } else {
        prev_indexed
    };

    update_status(|status| {
        status.indexing = !finished;
        status.initialized = if finished {
            status_file.success
        } else {
            status.initialized
        };
        status.watching = false;
        status.roots = options.roots.clone();
        status.include_hidden = options.include_hidden;
        status.index_content = options.index_content;
        status.max_content_kb = options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB);
        status.commit_every = normalize_commit_every(options.commit_every);
        status.watcher_enabled = options.watcher_enabled;
        status.watcher_paused = options.watcher_paused;
        status.exclude_folders = options.exclude_folders.clone();
        status.exclude_extensions = options.exclude_extensions.clone();
        status.indexed_files = effective_indexed;
        // Live scan counters — mirrored so `get_file_search_status` reports the
        // same numbers the progress event carries (fixes 0/0/0 on a mid-build view).
        status.scanned_entries = status_file.scanned_entries;
        status.skipped_files = status_file.skipped_files;
        if finished && status_file.success {
            status.last_indexed_at_ms = Some(status_file.updated_at_ms);
            status.last_error = None;
        } else if finished && !status_file.canceled {
            status.last_error = Some(message.clone());
        }
        status.diagnostics.index_worker = if !finished {
            "running".to_string()
        } else if status_file.canceled {
            "stopped".to_string()
        } else if status_file.success {
            "finished".to_string()
        } else {
            "failed".to_string()
        };
        status.diagnostics.watcher_worker = "stopped".to_string();
        status.diagnostics.performance_mode =
            normalize_index_performance_mode(&options.performance_mode);
        status.diagnostics.last_worker_message = Some(message.clone());
        status.diagnostics.last_status_at_ms = Some(status_file.updated_at_ms);
    });

    emit_progress(
        app,
        FileSearchProgressEvent {
            stage: "indexing".to_string(),
            index_kind: "content".to_string(),
            indexed_files: effective_indexed,
            scanned_entries: status_file.scanned_entries,
            skipped_files: status_file.skipped_files,
            current_path: String::new(),
            finished,
            canceled: status_file.canceled,
            success: status_file.success,
            message,
            partial: status_file.partial,
        },
    );
}

pub fn maybe_run_index_worker_from_args() -> bool {
    let args: Vec<std::ffi::OsString> = env::args_os().collect();
    let Some(worker_arg_position) = args
        .iter()
        .position(|arg| arg.to_string_lossy() == INDEX_WORKER_ARG)
    else {
        return false;
    };

    let mode = args
        .get(worker_arg_position + 1)
        .and_then(|value| value.to_str())
        .unwrap_or_default();

    let worker_args = args
        .get(worker_arg_position + 2..)
        .map(|value| value.to_vec())
        .unwrap_or_default();

    let result = match mode {
        "index" | "watch" | "filename-index" => {
            let state_dir = worker_arg_path(&worker_args, "--state-dir");
            let options_path = worker_arg_path(&worker_args, "--options");
            let status_path = worker_arg_path(&worker_args, "--status");
            let cancel_path = worker_arg_path(&worker_args, "--cancel");
            match (state_dir, options_path, status_path, cancel_path) {
                (Ok(state_dir), Ok(options_path), Ok(status_path), Ok(cancel_path)) => {
                    match mode {
                        "index" => run_index_worker_index_mode(
                            &state_dir,
                            &options_path,
                            &status_path,
                            &cancel_path,
                        ),
                        "filename-index" => run_index_worker_filename_mode(
                            &state_dir,
                            &options_path,
                            &status_path,
                            &cancel_path,
                        ),
                        _ => run_index_worker_watch_mode(
                            &state_dir,
                            &options_path,
                            &status_path,
                            &cancel_path,
                        ),
                    }
                }
                (_, _, Ok(status_path), _) => {
                    let message = "Index worker was started with invalid arguments".to_string();
                    let _ = write_index_worker_status(
                        &status_path,
                        &IndexWorkerStatusFile::finished(
                            false,
                            false,
                            0,
                            0,
                            0,
                            message.clone(),
                            Some(message.clone()),
                        ),
                    );
                    Err(message)
                }
                _ => Err("Index worker was started with invalid arguments".to_string()),
            }
        }
        "extract" => run_extractor_worker(&worker_args),
        _ => Err(format!("Unknown index worker mode: {mode}")),
    };

    if let Err(error) = result {
        eprintln!("{error}");
    }

    true
}

fn worker_arg_path(args: &[std::ffi::OsString], key: &str) -> Result<PathBuf, String> {
    let Some(position) = args.iter().position(|arg| arg.to_string_lossy() == key) else {
        return Err(format!("Missing worker argument {key}"));
    };
    args.get(position + 1)
        .map(PathBuf::from)
        .ok_or_else(|| format!("Missing value for worker argument {key}"))
}

/// Subprocess entry point for a content extractor child (`--dash-index-worker
/// extract`) — the out-of-process half of the Phase 4 task-11 pool. It loops
/// reading one file path per line from stdin, extracts text via
/// `text_extract::extract_text_for_indexing`, and writes one length-prefixed
/// result frame per path to stdout. The parent ends the child by closing its
/// stdin (EOF). The child runs inside a `MemoryCappedJob` (task 11a), so a
/// pathological file that blows the memory cap gets this child OS-killed — the
/// parent then respawns it (task 11e) and indexes that file by name only.
///
/// Result frame, all integers little-endian:
///   `[u32 path_len][path UTF-8][u8 ok][u32 text_len][text UTF-8]`
/// `ok` = 1 when text was extracted; 0 for an unsupported format, an
/// over-the-size-guard file, or an extraction failure (then `text_len` = 0).
/// File paths cannot contain newlines, so the stdin side is safely
/// line-delimited; the text side is length-prefixed since it can be anything.
fn run_extractor_worker(worker_args: &[std::ffi::OsString]) -> Result<(), String> {
    use std::io::{BufRead, BufWriter, Write};

    let max_bytes = worker_args
        .iter()
        .position(|arg| arg.to_string_lossy() == "--max-bytes")
        .and_then(|index| worker_args.get(index + 1))
        .and_then(|value| value.to_string_lossy().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_CONTENT_KB as usize * 1024);

    let mut stdout = BufWriter::new(std::io::stdout().lock());

    for line in std::io::stdin().lock().lines() {
        // An unreadable line means stdin was closed — the parent is done with
        // this child, so exit cleanly.
        let Ok(raw) = line else {
            break;
        };
        let path_text = raw.trim();
        if path_text.is_empty() {
            continue;
        }

        let extracted =
            super::text_extract::extract_text_for_indexing(Path::new(path_text), max_bytes);
        let (ok, text) = match extracted {
            Some(text) => (1u8, text),
            None => (0u8, String::new()),
        };

        let path_bytes = path_text.as_bytes();
        let text_bytes = text.as_bytes();
        let frame = (|| -> std::io::Result<()> {
            stdout.write_all(&(path_bytes.len() as u32).to_le_bytes())?;
            stdout.write_all(path_bytes)?;
            stdout.write_all(&[ok])?;
            stdout.write_all(&(text_bytes.len() as u32).to_le_bytes())?;
            stdout.write_all(text_bytes)?;
            stdout.flush()
        })();
        // If the parent closed our stdout there is no one to report to — exit.
        if frame.is_err() {
            break;
        }
    }

    Ok(())
}

fn run_index_worker_index_mode(
    state_dir: &Path,
    options_path: &Path,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    let result =
        run_index_worker_index_mode_inner(state_dir, options_path, status_path, cancel_path);
    if let Err(error) = &result {
        let _ = write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::finished(
                false,
                false,
                0,
                0,
                0,
                error.clone(),
                Some(error.clone()),
            ),
        );
    }
    result
}

fn run_index_worker_index_mode_inner(
    state_dir: &Path,
    options_path: &Path,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    let options_bytes = fs::read(options_path)
        .map_err(|error| format!("Cannot read index worker options: {error}"))?;
    let options: FileSearchIndexOptions = serde_json::from_slice(&options_bytes)
        .map_err(|error| format!("Cannot parse index worker options: {error}"))?;
    let options = normalize_index_options(options)?;

    fs::create_dir_all(state_dir)
        .map_err(|error| format!("Cannot create search index state directory: {error}"))?;
    recover_content_index_dirs_for_state(state_dir)?;
    write_index_worker_status(
        status_path,
        &IndexWorkerStatusFile::progress(0, 0, 0, "Preparing search index".to_string()),
    )?;

    let staging_dir = staging_search_index_dir_for_state(state_dir, unix_now_ms());
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir)
            .map_err(|error| format!("Cannot reset staging search index: {error}"))?;
    }
    fs::create_dir_all(&staging_dir)
        .map_err(|error| format!("Cannot create staging search index: {error}"))?;

    let result = build_index_in_worker(state_dir, &staging_dir, &options, status_path, cancel_path);
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging_dir);
    }
    result
}

/// Subprocess entry point for the standalone filename indexer (`--worker
/// filename-index`). Twin of `run_index_worker_index_mode`: on any error it
/// writes a failed status file so the parent monitor sees the failure.
fn run_index_worker_filename_mode(
    state_dir: &Path,
    options_path: &Path,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    let result =
        run_index_worker_filename_mode_inner(state_dir, options_path, status_path, cancel_path);
    if let Err(error) = &result {
        let _ = write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::finished(
                false,
                false,
                0,
                0,
                0,
                error.clone(),
                Some(error.clone()),
            ),
        );
    }
    result
}

fn run_index_worker_filename_mode_inner(
    state_dir: &Path,
    options_path: &Path,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    let options_bytes = fs::read(options_path)
        .map_err(|error| format!("Cannot read filename index worker options: {error}"))?;
    let options: FileSearchIndexOptions = serde_json::from_slice(&options_bytes)
        .map_err(|error| format!("Cannot parse filename index worker options: {error}"))?;
    let options = normalize_index_options(options)?;

    fs::create_dir_all(state_dir)
        .map_err(|error| format!("Cannot create search index state directory: {error}"))?;
    recover_filename_index_dirs_for_state(state_dir)?;
    write_index_worker_status(
        status_path,
        &IndexWorkerStatusFile::progress(0, 0, 0, "Preparing filename index".to_string()),
    )?;

    let staging_dir = staging_filename_index_dir_for_state(state_dir, unix_now_ms());
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir)
            .map_err(|error| format!("Cannot reset staging filename index: {error}"))?;
    }
    fs::create_dir_all(&staging_dir)
        .map_err(|error| format!("Cannot create staging filename index: {error}"))?;

    let result = build_filename_index_best_effort(
        state_dir,
        &staging_dir,
        &options,
        status_path,
        cancel_path,
    );
    // The walker renames the staging dir into place; the MFT path leaves it
    // unused. Either way, drop any leftover.
    let _ = fs::remove_dir_all(&staging_dir);
    result
}

/// One entry in the "incremental rebuild" cache. Used to skip the expensive
/// content-extraction step when a file is unchanged since the last index.
struct ExistingDocLookup {
    modified_ms: u64,
    size: u64,
    address: tantivy::DocAddress,
}

/// Snapshot of the previous index used during a staging rebuild. The reader
/// is kept alive for the duration of the build so per-file content lookups
/// can resolve via doc_address without re-extracting from disk.
struct ExistingIndexCache {
    searcher: tantivy::Searcher,
    fields: SearchFields,
    /// Lowercase path → existing doc info. Case-insensitive because Windows
    /// paths are case-insensitive and the file walker may emit different casing.
    map: HashMap<String, ExistingDocLookup>,
}

/// Open the previous active index (if any) and build a path → doc lookup map
/// so the rebuild can reuse cached content for unchanged files.
///
/// Returns None if there's no previous index, the index is corrupt, or the
/// schema doesn't match — in any of those cases the caller falls back to
/// full content extraction for every file.
fn load_existing_index_for_reuse(active_dir: &Path) -> Option<ExistingIndexCache> {
    if !active_dir.exists() {
        return None;
    }
    let directory = MmapDirectory::open(active_dir).ok()?;
    let index = Index::open(directory).ok()?;
    let schema = index.schema();
    let fields = extract_fields(&schema).ok()?;
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .ok()?;
    let searcher: tantivy::Searcher = reader.searcher();

    // Walk every alive document by iterating segment readers directly. This
    // avoids the scoring overhead a Query+Collector path would add — for a
    // 270k-doc index, building this map should take a few seconds at most.
    let mut map: HashMap<String, ExistingDocLookup> = HashMap::with_capacity(8192);
    for (segment_ord, segment_reader) in searcher.segment_readers().iter().enumerate() {
        let Ok(store_reader) = segment_reader.get_store_reader(0) else { continue };
        let max_doc = segment_reader.max_doc();
        let alive_bitset = segment_reader.alive_bitset();
        for doc_id in 0..max_doc {
            if let Some(bitset) = alive_bitset {
                if !bitset.is_alive(doc_id) {
                    continue;
                }
            }
            let Ok(doc): Result<TantivyDocument, _> = store_reader.get(doc_id) else { continue };
            let Some(path) = doc_text(&doc, fields.path) else { continue };
            let modified_ms = doc_u64(&doc, fields.modified_ms).unwrap_or(0);
            let size = doc_u64(&doc, fields.size).unwrap_or(0);
            map.insert(
                path.to_lowercase(),
                ExistingDocLookup {
                    modified_ms,
                    size,
                    address: tantivy::DocAddress::new(segment_ord as u32, doc_id),
                },
            );
        }
    }

    Some(ExistingIndexCache {
        searcher,
        fields,
        map,
    })
}

/// Returns cached content if the previous index has this exact file unchanged
/// (matching mtime + size) AND that previous entry actually stored non-empty
/// content. Returning None means "no cache hit — please extract fresh."
fn reuse_cached_content(
    cache: &ExistingIndexCache,
    path_lower: &str,
    modified_ms: u64,
    size: u64,
) -> Option<String> {
    let entry = cache.map.get(path_lower)?;
    if entry.modified_ms != modified_ms || entry.size != size {
        return None;
    }
    let doc: TantivyDocument = cache.searcher.doc(entry.address).ok()?;
    let content = doc_text(&doc, cache.fields.content).unwrap_or_default();
    // Skip empty cached content — happens for old indexes built before content
    // was STORED, plus genuine "no extractable text" cases. Forcing a fresh
    // extract handles both: it's idempotent for empty files, and self-heals
    // old indexes on first rebuild.
    if content.is_empty() {
        return None;
    }
    Some(content)
}

/// The metadata columns of one content-index document — everything except the
/// extracted `content`. Built at the call site so the same metadata can pair
/// with cached text (reuse-cache hit) or freshly-extracted text (pool result).
struct ContentDocMeta<'a> {
    file_path: &'a str,
    file_name: &'a str,
    extension: &'a str,
    entry_type: &'a str,
    size: u64,
    modified_ms: u64,
    created_ms: u64,
}

/// Build one content-index document and add it to the writer. Shared by the
/// discovery walk (reuse-cache hits) and the extractor-pool indexer thread so
/// both paths produce byte-identical documents. `content` is empty for a file
/// indexed by name only. `add_document` is `&self`, so this is safe to call
/// from the walk's worker threads and the indexer thread concurrently.
fn add_content_document(
    writer: &IndexWriter,
    fields: SearchFields,
    meta: &ContentDocMeta,
    content: &str,
) -> Result<(), String> {
    // Sensitive-content scan: only when text was extracted. Detection is
    // local and only the finding *labels* are stored, never the secrets.
    let sensitive_kinds = if content.is_empty() {
        Vec::new()
    } else {
        super::sensitive_scan::scan_text(content)
    };
    let mut document = doc!(
        fields.path => meta.file_path.to_string(),
        fields.path_exact => meta.file_path.to_string(),
        fields.file_name => meta.file_name.to_string(),
        fields.extension => meta.extension.to_string(),
        fields.content => content.to_string(),
        fields.size => meta.size,
        fields.modified_ms => meta.modified_ms,
    );
    if let Some(created_ms_field) = fields.created_ms {
        document.add_u64(created_ms_field, meta.created_ms);
    }
    if let Some(entry_type_field) = fields.entry_type {
        document.add_text(entry_type_field, meta.entry_type);
    }
    if let Some(kinds_field) = fields.sensitive_kinds {
        for kind in &sensitive_kinds {
            document.add_text(kinds_field, *kind);
        }
    }
    writer
        .add_document(document)
        .map(|_| ())
        .map_err(|error| format!("Cannot add document to index: {error}"))
}

/// Emit a periodic progress status and apply the per-mode CPU-yield throttle.
/// Shared by the discovery walk and the indexer thread; the shared
/// `last_progress` mutex ensures only one status write happens per interval.
fn report_index_progress(
    status_path: &Path,
    new_indexed: u64,
    scanned: &std::sync::atomic::AtomicU64,
    skipped: &std::sync::atomic::AtomicU64,
    last_progress: &Mutex<u128>,
    throttle_ms: u64,
) {
    if new_indexed.is_multiple_of(INDEX_PROGRESS_EVERY_FILES) {
        let now = unix_now_ms();
        let mut should_emit = false;
        if let Ok(mut last) = last_progress.lock() {
            if now.saturating_sub(*last) >= INDEX_PROGRESS_MIN_INTERVAL_MS {
                *last = now;
                should_emit = true;
            }
        }
        if should_emit {
            let _ = write_index_worker_status(
                status_path,
                &IndexWorkerStatusFile::progress(
                    new_indexed,
                    scanned.load(Ordering::Relaxed),
                    skipped.load(Ordering::Relaxed),
                    format!("Indexed {new_indexed} entries"),
                ),
            );
        }
    }
    if throttle_ms > 0 && new_indexed > 0 && new_indexed.is_multiple_of(INDEX_THROTTLE_EVERY_FILES) {
        thread::sleep(Duration::from_millis(throttle_ms));
    }
}

/// One content-indexer thread: drain extracted text from the pool and add a
/// document per result. Several of these run in parallel — the per-document
/// work (sensitive scan + tantivy tokenisation in `add_document`) is the build's
/// real cost, so a single drainer is a hard throughput bottleneck. The shared
/// `result_rx` mutex is held only across `recv`, never across the heavy work
/// below, so the threads genuinely parallelise.
fn run_content_indexer(
    result_rx: &Mutex<std::sync::mpsc::Receiver<ExtractResult>>,
    writer: &IndexWriter,
    fields: SearchFields,
    indexed_files: &std::sync::atomic::AtomicU64,
    scanned: &std::sync::atomic::AtomicU64,
    skipped: &std::sync::atomic::AtomicU64,
    first_error: &Mutex<Option<String>>,
    last_progress: &Mutex<u128>,
    should_stop: &AtomicBool,
    status_path: &Path,
    throttle_ms: u64,
    // Semantic search (beta): path to the vector sidecar store, `Some` only when
    // the toggle is on AND the embedder is compiled in and loaded. `None` skips
    // all embedding work (the default), so this loop is unchanged when off.
    vector_db_path: Option<&Path>,
) {
    loop {
        // Hold the receiver lock only for `recv` itself — never across the
        // scan + `add_document` below; that is what lets the threads parallise.
        let received = {
            let Ok(rx) = result_rx.lock() else {
                return;
            };
            rx.recv()
        };
        let Ok(result) = received else {
            return; // the pool closed the result channel — extraction is done.
        };
        // The build is aborting (error / cancel) — keep draining so no pool
        // thread blocks on a full channel, but stop adding documents.
        if should_stop.load(Ordering::Relaxed) {
            continue;
        }
        let metadata = match fs::metadata(&result.path) {
            Ok(value) => value,
            Err(_) => {
                // The file vanished between discovery and extraction.
                skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let modified_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_millis() as u64)
            .unwrap_or(0);
        let created_ms = metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_millis() as u64)
            .unwrap_or(modified_ms);
        let entry = Path::new(&result.path);
        let file_name = entry
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string();
        let extension = entry
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_lowercase();
        // `text` is `None` for an unsupported format, an over-size-guard file,
        // an extraction failure, or a child that died — in every case the file
        // is indexed by name only (empty content).
        let content = result.text.unwrap_or_default();
        let meta = ContentDocMeta {
            file_path: &result.path,
            file_name: &file_name,
            extension: &extension,
            entry_type: ENTRY_TYPE_FILE,
            size: metadata.len(),
            modified_ms,
            created_ms,
        };
        if let Err(error) = add_content_document(writer, fields, &meta, &content) {
            if let Ok(mut guard) = first_error.lock() {
                if guard.is_none() {
                    *guard = Some(error);
                }
            }
            should_stop.store(true, Ordering::Relaxed);
            continue;
        }
        // Semantic search: chunk the extracted text into overlapping passages,
        // embed them in one batch, and store the (int8-quantized) chunk vectors
        // so the query-time cosine pass can find this doc by the meaning of its
        // best-matching passage — not one blurry whole-doc average. Best-effort:
        // a failed embed just drops this doc from semantic recall, never the
        // keyword index. Skipped for name-only (empty content) docs.
        if let Some(vec_path) = vector_db_path {
            if !content.is_empty() {
                let chunks = super::embedding::chunk_text(&content, 180, 30, 16);
                if let Some(vectors) = super::embedding::embed_batch(&chunks) {
                    if !vectors.is_empty() {
                        super::vector_cache::store(vec_path, &result.path, &vectors);
                    }
                }
            }
        }
        let new_indexed = indexed_files.fetch_add(1, Ordering::Relaxed) + 1;
        report_index_progress(
            status_path,
            new_indexed,
            scanned,
            skipped,
            last_progress,
            throttle_ms,
        );
    }
}

/// Cheap "did anything change?" pre-pass for the content rebuild. Walks the
/// roots collecting ONLY (path, mtime, size) for indexable text files — no text
/// extraction, no tokenisation, no commit, no swap — and compares against the
/// previous index snapshot. Returns `true` only when the discovered set is
/// byte-for-byte identical to the cache (same files, same mtime+size, same
/// count), meaning a full rebuild would produce an identical index and can be
/// skipped entirely.
///
/// Correctness: this relies on the exact same `(mtime, size)` equality the reuse
/// path (`reuse_cached_content`) already trusts, so it introduces no staleness
/// risk beyond the existing incremental rebuild — a file edited in place that
/// preserves both mtime and size is already treated as "unchanged" today. Any
/// addition, deletion, or `(mtime|size)` change returns `false` and the caller
/// proceeds with the normal rebuild. Read errors (unreadable dir entry or
/// metadata) also return `false`, so a rebuild is never skipped on uncertain
/// ground. A schema change can't reach here: `load_existing_index_for_reuse`
/// returns `None` on a schema mismatch, so the caller never calls this.
fn content_index_unchanged(
    options: &FileSearchIndexOptions,
    cache: &ExistingIndexCache,
    cancel_path: &Path,
) -> bool {
    // Deduplicate roots exactly like the rebuild walk so the discovered set
    // matches what a real build would index (a child path already covered by a
    // parent root must not be walked twice).
    let mut deduped_roots: Vec<&String> = Vec::new();
    'outer: for root in &options.roots {
        let root_path = Path::new(root);
        for existing in &deduped_roots {
            if root_path.starts_with(Path::new(existing)) {
                continue 'outer;
            }
        }
        deduped_roots.retain(|existing| !Path::new(existing).starts_with(root_path));
        deduped_roots.push(root);
    }

    let mut matched: u64 = 0;
    let mut scanned: u64 = 0;
    for root in deduped_roots {
        let builder = index_walker(root, options, true);
        // Single-threaded on purpose: metadata-only stat work is cheap, the walk
        // bails on the first change, and a sequential walk avoids sharing the
        // cache across threads. It is still orders of magnitude faster than the
        // full re-add + commit + merge + swap it lets us skip.
        for entry_result in builder.build() {
            scanned += 1;
            if scanned.is_multiple_of(512) && cancel_path.exists() {
                // A cancel during the pre-pass: don't claim "unchanged" — fall
                // through to the normal build so its cancel handling runs.
                return false;
            }
            let entry = match entry_result {
                Ok(value) => value,
                Err(_) => return false,
            };
            let path = entry.path();
            let is_file = entry
                .file_type()
                .map(|value| value.is_file())
                .unwrap_or(false);
            // Folders carry no content document; the content index holds text
            // files only, so only those participate in the comparison.
            if !is_file {
                continue;
            }
            if !should_include_path(path, options) {
                continue;
            }
            if !should_index_for_content(path, options) {
                continue;
            }
            let metadata = match fs::metadata(path) {
                Ok(value) => value,
                Err(_) => return false,
            };
            let size = metadata.len();
            let modified_ms = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_millis() as u64)
                .unwrap_or(0);
            let path_lower = path.to_string_lossy().to_lowercase();
            match cache.map.get(&path_lower) {
                Some(doc) if doc.modified_ms == modified_ms && doc.size == size => {
                    matched += 1;
                }
                // New file, or an mtime/size change — a rebuild is required.
                _ => return false,
            }
        }
    }

    // Every discovered text file matched the cache (no additions, no changes).
    // The count check catches deletions: if a file in the previous index is gone
    // from disk, `matched` falls short of the cache size and we must rebuild.
    matched == cache.map.len() as u64
}

fn build_index_in_worker(
    state_dir: &Path,
    staging_dir: &Path,
    options: &FileSearchIndexOptions,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    // ── Content indexing opt-in (Phase 2 / Task 4.3) ──────────────────────
    // When the user has turned "search inside files" off, the content worker
    // no-ops: the filename index (built by a separate worker) still gives full
    // name search. Default is on; the frontend defaults it OFF on first run on
    // low-RAM (Lite) profiles where a content index can't converge.
    if !options.content_indexing_enabled {
        let _ = fs::remove_dir_all(staging_dir);
        write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::finished(
                true,
                false,
                0,
                0,
                0,
                "Content search is off — filename search only.".to_string(),
                None,
            ),
        )?;
        return Ok(());
    }

    // ── Fast path: skip the rebuild entirely when nothing changed ─────────
    // Load the previous-index snapshot up front (also reused below for the
    // incremental content rebuild). If a cheap metadata-only walk shows the file
    // set is unchanged, a rebuild would produce an identical index — so write a
    // success status and return without re-adding a single document. This turns
    // a "reindex with no changes" from ~a minute (re-add + commit + merge + swap
    // of every doc) into the cost of one metadata walk.
    let active_dir = active_search_index_dir_for_state(state_dir);
    let existing_cache = load_existing_index_for_reuse(&active_dir);
    if let Some(cache) = &existing_cache {
        if content_index_unchanged(options, cache, cancel_path) {
            let doc_count = cache.map.len() as u64;
            let now_ms = unix_now_ms();
            let rebuild_schedule = read_search_config_for_state(state_dir)
                .ok()
                .flatten()
                .map(|config| config.rebuild_schedule)
                .unwrap_or_default();
            let _ = write_search_config_for_state(
                state_dir,
                &StoredSearchConfig {
                    options: options.clone(),
                    indexed_files: doc_count,
                    last_indexed_at_ms: Some(now_ms),
                    rebuild_schedule,
                },
            );
            // The caller created an empty staging dir for the (now-skipped)
            // rebuild; the normal path consumes it via the swap, so discard it
            // here to leave the same on-disk state.
            let _ = fs::remove_dir_all(staging_dir);
            write_index_worker_status(
                status_path,
                &IndexWorkerStatusFile::finished(
                    true,
                    false,
                    doc_count,
                    doc_count,
                    0,
                    format!(
                        "Index already up to date — {doc_count} files unchanged (rebuild skipped)"
                    ),
                    None,
                ),
            )?;
            return Ok(());
        }
    }

    let schema = build_search_schema();
    let directory = MmapDirectory::open(staging_dir)
        .map_err(|error| format!("Cannot open staging search index directory: {error}"))?;
    let index = Index::open_or_create(directory, schema.clone())
        .map_err(|error| format!("Cannot create staging search index: {error}"))?;
    let fields = extract_fields(&schema)?;
    // No `mut` — the writer is wrapped in Arc for the parallel walk and we
    // reclaim sole ownership afterward via Arc::try_unwrap for the final commit.
    let writer = index
        .writer_with_num_threads(index_writer_threads(), WRITER_MEMORY_BUDGET_BYTES_INDEXING)
        .map_err(|error| format!("Cannot create search writer: {error}"))?;

    // The filename index is built separately by the standalone filename
    // indexer (Phase 3, step 7d retired the in-walk dual-write) — this worker
    // builds only the content index.

    // Incremental indexing: the previous-index snapshot was loaded up front for
    // the fast-path check above; reuse it here so the rebuild can reuse already-
    // extracted content for files whose (mtime, size) haven't changed. For
    // typical "rebuild after light filesystem activity" workflows this skips
    // 95%+ of expensive PDF/Office extraction work.
    if let Some(cache) = &existing_cache {
        write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::progress(
                0,
                0,
                0,
                format!(
                    "Loaded previous index ({} files) — reusing extraction for unchanged files",
                    cache.map.len()
                ),
            ),
        )?;
    }

    let max_content_bytes = (options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB) as usize)
        .saturating_mul(1024)
        .max(8 * 1024);
    // commit_every is intentionally unused in the parallel build — see the
    // comment below where we walk in parallel. Tantivy auto-flushes when its
    // memory budget fills, so we rely on a single final commit instead.
    let _ = normalize_commit_every(options.commit_every);

    // Shared state for the parallel walker. Counters use atomics so workers
    // never block each other. Errors and progress timing use small mutexes
    // because they're touched rarely.
    let writer = Arc::new(writer);
    let existing_cache = Arc::new(existing_cache);
    let indexed_files_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let scanned_entries_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let skipped_files_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let reused_count_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let first_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let last_progress_at_ms_shared: Arc<Mutex<u128>> = Arc::new(Mutex::new(unix_now_ms()));
    let should_stop_walker = Arc::new(AtomicBool::new(false));

    // Deduplicate roots so that a child path (e.g. /home/user/docs) that is
    // already covered by a parent root (e.g. /home/user) doesn't get walked
    // twice and produce duplicate index entries.
    let mut deduped_roots: Vec<&String> = Vec::new();
    'outer: for root in &options.roots {
        let root_path = std::path::Path::new(root);
        for existing in &deduped_roots {
            if root_path.starts_with(std::path::Path::new(existing)) {
                continue 'outer;
            }
        }
        // Also drop any previously accepted roots that are children of this new one.
        deduped_roots.retain(|existing| !std::path::Path::new(existing).starts_with(root_path));
        deduped_roots.push(root);
    }

    // Build in parallel. The `ignore` crate's parallel walker spawns N worker
    // threads that each enumerate part of the tree AND run the per-file
    // closure on the same thread. Since PDF/Office text extraction is the
    // bottleneck and is purely CPU-bound, putting it on worker threads scales
    // linearly with cores — the indexer goes from CPU-of-one-thread-bound to
    // CPU-of-all-threads-bound.
    //
    // Periodic commits aren't used inside the parallel section because Tantivy's
    // `commit` needs `&mut self` (can't be called through Arc). The writer's
    // 96 MB internal budget auto-flushes segments as it fills, so memory stays
    // bounded. A single final commit happens after all workers finish.
    let performance_mode = options.performance_mode.clone();
    // On battery the indexer runs gentler (fewer threads + a CPU yield) so a
    // background build doesn't drain a laptop. Sampled once at build start.
    let on_battery = is_on_battery();
    let throttle_ms = index_throttle_sleep_ms(&performance_mode, on_battery);
    let options_for_filter = options.clone();

    // Out-of-process content extraction (Phase 4, task 11d). The discovery
    // walk below only finds document files; this pool of memory-capped
    // extractor children does the actual text extraction, and the indexer
    // thread owns every `add_document` for the content index.
    // Wave 8.3 (2026-05-28): when OCR is enabled the workload becomes
    // CPU-bound and benefits from more concurrent extractor children.
    // `index_walker_threads_with_ocr` raises the per-mode caps accordingly.
    let cpu_extractor_count = index_walker_threads_with_ocr(
        &performance_mode,
        on_battery,
        options.ocr_on_index_enabled,
    );
    // RAM-aware clamping (Phase 1 of the HardwareProfile design). On machines
    // with < 1.5 GB available RAM, running N × 512 MB children causes the OS
    // to kill them all under memory pressure, exhausting the respawn budget
    // and silently stopping indexing. `clamp_pool_for_available_ram` keeps the
    // count and per-child memory cap within what `GlobalMemoryStatusEx` says
    // the machine can sustain right now.
    let (extractor_child_count, extractor_memory_cap) =
        clamp_pool_for_available_ram(cpu_extractor_count);
    // Wave 8 / task #88 (2026-05-28): propagate OCR-on-Index opt-in to the
    // extractor children via env vars on each spawn Command. The struct is
    // empty (no env vars) when the user hasn't enabled OCR, so the spawn
    // path is unchanged for the default-off case.
    let ocr_env = super::text_extract::OcrChildEnv::from_options(
        options.ocr_on_index_enabled,
        &options.ocr_on_index_folders,
        options.ocr_max_pages_per_file,
        &options.ocr_langs,
        options.ocr_min_image_dim,
        options.ocr_max_aspect_ratio,
        options.ocr_per_file_timeout_secs,
    );
    // Wave 8.5 (2026-05-28): build the persistent OCR cache context once
    // per rebuild and hand it to the extractor pool. The cache lives at
    // `state_dir/ocr_cache.redb` (sibling of the search index) and is
    // keyed by `(blake3_hash, lang)` so identical bytes only ever get
    // OCR'd once per language profile. `None` when OCR is off keeps the
    // pre-Wave-8.5 behavior: no hashing, no redb writes, no overhead.
    let cache_ctx = if options.ocr_on_index_enabled {
        Some(super::ocr_cache::OcrCacheCtx::new(
            state_dir,
            &options.ocr_langs,
        ))
    } else {
        None
    };
    // Semantic search (beta, 2026-07-01): when the toggle is on AND the embedder
    // is compiled in (`semantic` feature) and its model loaded, index one vector
    // per document beside the search index. `None` — the default, or any build
    // without the feature — makes the indexer threads skip all embedding work.
    // Cleared up front so vectors for files deleted since the last build don't
    // linger as orphans (this is the full-rebuild path).
    let vector_db_path = if options.semantic_search_enabled && super::embedding::is_available() {
        let path = super::vector_cache::cache_path_for_dir(state_dir);
        super::vector_cache::clear(&path);
        Some(path)
    } else {
        None
    };
    let (pool, path_tx, result_rx) = ExtractorPool::new(
        extractor_child_count,
        max_content_bytes,
        extractor_memory_cap,
        &performance_mode,
        Arc::clone(&should_stop_walker),
        ocr_env,
        cache_ctx,
    )
    .map_err(|error| {
        let _ = fs::remove_dir_all(staging_dir);
        format!("Cannot start content extractor pool: {error}")
    })?;

    // Indexer threads — drain extracted text from the pool and add documents.
    // They MUST run concurrently with the walk (the pool's bounded channels
    // would deadlock a walk-everything-then-drain), and there are several so
    // the per-document work (sensitive scan + `add_document` tokenisation)
    // runs in parallel — a single drainer was a hard throughput bottleneck.
    let indexer_count = extractor_child_count.clamp(1, 8);
    let result_rx = Arc::new(Mutex::new(result_rx));
    let indexer_handles: Vec<_> = (0..indexer_count)
        .map(|_| {
            let result_rx = Arc::clone(&result_rx);
            let writer = Arc::clone(&writer);
            let indexed_files_atomic = Arc::clone(&indexed_files_atomic);
            let scanned_entries_atomic = Arc::clone(&scanned_entries_atomic);
            let skipped_files_atomic = Arc::clone(&skipped_files_atomic);
            let first_error = Arc::clone(&first_error);
            let last_progress = Arc::clone(&last_progress_at_ms_shared);
            let should_stop = Arc::clone(&should_stop_walker);
            let status_path = status_path.to_path_buf();
            let vector_db_path = vector_db_path.clone();
            thread::spawn(move || {
                run_content_indexer(
                    &result_rx,
                    &writer,
                    fields,
                    &indexed_files_atomic,
                    &scanned_entries_atomic,
                    &skipped_files_atomic,
                    &first_error,
                    &last_progress,
                    &should_stop,
                    &status_path,
                    throttle_ms,
                    vector_db_path.as_deref(),
                );
            })
        })
        .collect();

    // Tracks whether the extractor pool died abnormally mid-walk (all children
    // OS-killed + respawn budget exhausted). When this fires, the walk stops
    // via `should_stop_walker` but the cancel sentinel was never written, so
    // without this flag the code would fall through to `success=true` with a
    // misleading partial count. Shared across walker threads via Arc.
    let pool_died = Arc::new(AtomicBool::new(false));

    for root in deduped_roots {
        if should_stop_walker.load(Ordering::Relaxed) {
            break;
        }

        let mut builder = index_walker(root, options, true);
        // Per-mode parallelism. On an 8-core machine this gives 7 threads in
        // "fast", 8 (capped) in "balanced" (default), and just 2 in "quiet"
        // (1 on a 4-core). Each thread independently extracts content from its
        // files, so PDF/Office parsing — the dominant CPU cost — runs in
        // parallel across all assigned cores.
        builder.threads(index_walker_threads(&performance_mode, on_battery));

        let walker = builder.build_parallel();
        let status_path_owned = status_path.to_path_buf();
        let cancel_path_owned = cancel_path.to_path_buf();

        walker.run(|| {
            // Per-worker clones of the shared state. The outer closure is FnMut
            // and runs once per worker thread; cloning Arcs here gives each
            // worker its own handle without contention on the original.
            let writer = Arc::clone(&writer);
            let existing_cache = Arc::clone(&existing_cache);
            let indexed_files_atomic = Arc::clone(&indexed_files_atomic);
            let scanned_entries_atomic = Arc::clone(&scanned_entries_atomic);
            let skipped_files_atomic = Arc::clone(&skipped_files_atomic);
            let reused_count_atomic = Arc::clone(&reused_count_atomic);
            let first_error = Arc::clone(&first_error);
            let last_progress = Arc::clone(&last_progress_at_ms_shared);
            let should_stop = Arc::clone(&should_stop_walker);
            let pool_died = Arc::clone(&pool_died);
            let fields = fields;
            let options = options_for_filter.clone();
            let status_path = status_path_owned.clone();
            let cancel_path = cancel_path_owned.clone();
            let throttle_ms = throttle_ms;
            let path_tx = path_tx.clone();

            Box::new(move |entry_result| {
                if should_stop.load(Ordering::Relaxed) {
                    return ignore::WalkState::Quit;
                }

                let current_scanned = scanned_entries_atomic.fetch_add(1, Ordering::Relaxed) + 1;

                // Cancellation check: every ~500 scans, see if the cancel file
                // was dropped. Atomic + mod check is cheap enough to run often.
                if current_scanned.is_multiple_of(500) && cancel_path.exists() {
                    should_stop.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }

                if current_scanned as usize > MAX_INDEXED_FILES {
                    if let Ok(mut guard) = first_error.lock() {
                        if guard.is_none() {
                            *guard = Some(format!(
                                "Index scan exceeded {MAX_INDEXED_FILES} entries. Choose fewer folders."
                            ));
                        }
                    }
                    should_stop.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }

                let entry = match entry_result {
                    Ok(value) => value,
                    Err(_) => {
                        skipped_files_atomic.fetch_add(1, Ordering::Relaxed);
                        return ignore::WalkState::Continue;
                    }
                };

                let path = entry.path();
                let file_type = entry.file_type();
                let is_file = file_type.map(|value| value.is_file()).unwrap_or(false);
                let is_dir = file_type.map(|value| value.is_dir()).unwrap_or(false);
                if !is_file && !is_dir {
                    return ignore::WalkState::Continue;
                }
                if !should_include_path(path, &options) {
                    skipped_files_atomic.fetch_add(1, Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                }
                // The content index holds readable document files only — plain
                // text, code, PDF, Office, and similar. Folders and non-readable
                // files (movies, music, archives, executables, binaries) carry
                // no searchable text; they are skipped here and stay
                // discoverable through the filename index instead.
                //
                // Wave 8.1 (2026-05-28): images are included only when the
                // user opted into OCR-on-Index AND the path lives in a
                // configured OCR folder. `should_index_for_content` handles
                // that decision in one place.
                if !is_file || !should_index_for_content(path, &options) {
                    skipped_files_atomic.fetch_add(1, Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                }

                let metadata = match fs::metadata(path) {
                    Ok(value) => value,
                    Err(_) => {
                        skipped_files_atomic.fetch_add(1, Ordering::Relaxed);
                        return ignore::WalkState::Continue;
                    }
                };

                let file_path = path.to_string_lossy().to_string();
                let extension = if is_file {
                    path.extension()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default()
                        .to_lowercase()
                } else {
                    String::new()
                };
                let file_name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
                    .to_string();
                let entry_type = if is_dir {
                    ENTRY_TYPE_FOLDER
                } else {
                    ENTRY_TYPE_FILE
                };
                let size = metadata.len();
                let modified_ms = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|value| value.as_millis() as u64)
                    .unwrap_or(0);
                let created_ms = metadata
                    .created()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|value| value.as_millis() as u64)
                    .unwrap_or(modified_ms);

                // Reuse-cache check — if this document's (mtime, size) match
                // the previous index, its already-extracted content is reused
                // and it is indexed here. On a miss the path goes to the
                // out-of-process extractor pool; the walk never extracts text.
                let cached = existing_cache.as_ref().as_ref().and_then(|cache| {
                    reuse_cached_content(cache, &file_path.to_lowercase(), modified_ms, size)
                });
                match cached {
                    Some(content) => {
                        reused_count_atomic.fetch_add(1, Ordering::Relaxed);
                        let meta = ContentDocMeta {
                            file_path: &file_path,
                            file_name: &file_name,
                            extension: &extension,
                            entry_type,
                            size,
                            modified_ms,
                            created_ms,
                        };
                        if let Err(error) =
                            add_content_document(&writer, fields, &meta, &content)
                        {
                            if let Ok(mut guard) = first_error.lock() {
                                if guard.is_none() {
                                    *guard = Some(error);
                                }
                            }
                            should_stop.store(true, Ordering::Relaxed);
                            return ignore::WalkState::Quit;
                        }
                        let new_indexed =
                            indexed_files_atomic.fetch_add(1, Ordering::Relaxed) + 1;
                        report_index_progress(
                            &status_path,
                            new_indexed,
                            &scanned_entries_atomic,
                            &skipped_files_atomic,
                            &last_progress,
                            throttle_ms,
                        );
                    }
                    None => {
                        // Reuse-cache miss — hand the path to the extractor
                        // pool. `try_send` (not blocking `send`) so a walk
                        // thread never parks inside a full channel: when the
                        // pipeline is the bottleneck the walk must still be
                        // able to notice a cancellation request rather than
                        // stalling on the send for the whole build.
                        let mut pending = path.to_path_buf();
                        loop {
                            match path_tx.try_send(pending) {
                                Ok(()) => break,
                                Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                                    if should_stop.load(Ordering::Relaxed)
                                        || cancel_path.exists()
                                    {
                                        should_stop.store(true, Ordering::Relaxed);
                                        return ignore::WalkState::Quit;
                                    }
                                    pending = returned;
                                    thread::sleep(Duration::from_millis(20));
                                }
                                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                                    // The pool's receiver is gone — all extractor
                                    // children were killed (likely OOM) and the
                                    // respawn budget was exhausted. Record this
                                    // separately from a user cancel so the final
                                    // status write can emit an actionable error
                                    // instead of a misleading "success".
                                    pool_died.store(true, Ordering::Relaxed);
                                    should_stop.store(true, Ordering::Relaxed);
                                    return ignore::WalkState::Quit;
                                }
                            }
                        }
                    }
                }

                ignore::WalkState::Continue
            })
        });
    }

    // Discovery walk done. Dropping every path sender lets the pool drain:
    // the dispatcher finishes, children hit stdin EOF and exit, their reader
    // threads close the result channel, and the indexer threads' loops end.
    drop(path_tx);
    // The walk only *discovers* files — the extractor pool and indexer threads
    // do the slow work and outlive it, and nothing else polls the cancel
    // sentinel. Watch it here so a cancel pressed during extraction/indexing is
    // honoured within ~150 ms instead of waiting out the whole backlog.
    while !should_stop_walker.load(Ordering::Relaxed)
        && !indexer_handles.iter().all(|handle| handle.is_finished())
    {
        if cancel_path.exists() {
            should_stop_walker.store(true, Ordering::Relaxed);
        } else {
            thread::sleep(Duration::from_millis(150));
        }
    }
    // If the build is aborting (cancel, too-many-files, or a writer error)
    // the staging index is discarded anyway — hard-kill the extractor
    // children (KILL_ON_JOB_CLOSE) rather than wait out the in-flight backlog.
    if should_stop_walker.load(Ordering::Relaxed) {
        drop(pool);
    }
    // Join every indexer thread: on success they return once all extracted
    // text is indexed; on abort they return near-instantly. Afterwards every
    // `add_document` is complete, so the writer Arc has one strong reference.
    let mut indexer_panicked = false;
    for handle in indexer_handles {
        if handle.join().is_err() {
            indexer_panicked = true;
        }
    }
    if indexer_panicked {
        let _ = fs::remove_dir_all(staging_dir);
        return Err("Content indexer thread panicked".to_string());
    }

    // Pull final counter values from atomics into plain locals for the rest
    // of the function (commit, status messages, etc.).
    let indexed_files = indexed_files_atomic.load(Ordering::Relaxed);
    let scanned_entries = scanned_entries_atomic.load(Ordering::Relaxed);
    let skipped_files = skipped_files_atomic.load(Ordering::Relaxed);
    let reused_content_count = reused_count_atomic.load(Ordering::Relaxed);

    // ── Resumable interruption (Phase 2 / Task 4.1) ───────────────────────
    // `pool_died`: the extractor children were OS-killed (memory pressure) and
    // the respawn budget was exhausted (`should_stop_walker` is set but the
    // cancel sentinel was never written). `canceled`: the user wrote the cancel
    // sentinel. In BOTH cases every indexer thread has already drained and
    // exited (joined just above), so the staging index holds a complete,
    // committable set of whatever was indexed so far.
    //
    // Pre-Phase-2 this work was discarded — which forced a low-RAM machine to
    // restart from zero every run and never converge. Instead we now COMMIT and
    // PROMOTE the partial index (down at the shared commit/swap below). The next
    // run's skip-unchanged (path, mtime) fast path skips the already-indexed
    // files, so indexing resumes where it left off. The main worker process
    // survives a child-pool death, so it can still commit here.
    let pool_died = pool_died.load(Ordering::Relaxed);
    let canceled = cancel_path.exists();
    let resumable_interrupt = pool_died || canceled;

    // A genuine writer/walk error means the staging index may be inconsistent —
    // never promote it. Checked before the promote decision so a real error is
    // never masked as a recoverable partial.
    if let Ok(guard) = first_error.lock() {
        if let Some(err) = guard.as_ref() {
            let _ = fs::remove_dir_all(staging_dir);
            return Err(err.clone());
        }
    }

    // Regression guard: only promote a partial index when it is at least as
    // complete as the index it would replace. On a rebuild of an already-
    // complete index, an early interruption would otherwise swap a smaller
    // index over a full one — so there we keep the existing index intact and
    // discard the partial staging (the pre-Phase-2 behavior).
    let prior_active_count = match &*existing_cache {
        Some(cache) => cache.map.len() as u64,
        None => 0,
    };
    if resumable_interrupt && indexed_files < prior_active_count {
        let _ = fs::remove_dir_all(staging_dir);
        let message = if canceled {
            "Index build cancelled — existing index kept.".to_string()
        } else {
            "Content extraction stopped early (low memory) — existing index kept. \
             Filename search still works. Close other apps or reduce indexed folders, then re-run."
                .to_string()
        };
        write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::finished(
                !pool_died,
                canceled,
                prior_active_count,
                scanned_entries,
                skipped_files,
                message,
                None,
            ),
        )?;
        return Ok(());
    }

    // Reclaim sole ownership of the writer for the final commit, which requires
    // `&mut self`. All worker threads have exited at this point, so the Arc
    // should have exactly one strong reference.
    let mut writer = Arc::try_unwrap(writer)
        .map_err(|_| "Internal: search writer still held by a worker".to_string())?;

    write_index_worker_status(
        status_path,
        &IndexWorkerStatusFile::progress(
            indexed_files,
            scanned_entries,
            skipped_files,
            "Finalizing search index".to_string(),
        ),
    )?;
    writer
        .commit()
        .map_err(|error| format!("Cannot finalize search index: {error}"))?;
    writer
        .wait_merging_threads()
        .map_err(|error| format!("Cannot finish search index merges: {error}"))?;
    drop(index);

    // Release the previous-index mmap BEFORE the swap. On Windows, an open
    // memory map prevents the directory rename inside `swap_in_staging_search_index_for_state`.
    drop(existing_cache);

    swap_in_staging_search_index_for_state(state_dir, staging_dir)?;

    // Wave 8.5 (2026-05-28): run the persistent OCR cache GC pass now
    // that the new index is live. TTL pass drops entries last touched
    // more than 90 days ago; LRU pass trims oldest entries until total
    // size is under 200 MB. Errors are logged but never propagated —
    // the rebuild is already complete and shouldn't fail-stop on a
    // cache maintenance hiccup. Skipped when OCR was off (no cache
    // entries to consider).
    if options.ocr_on_index_enabled {
        let cache_path = super::ocr_cache::cache_path_for_dir(state_dir);
        match super::ocr_cache::gc(
            &cache_path,
            super::ocr_cache::DEFAULT_TTL_DAYS,
            super::ocr_cache::DEFAULT_MAX_BYTES,
        ) {
            Ok(stats) if stats.entries_before > 0 => {
                eprintln!(
                    "ocr_cache: GC kept {}/{} entries ({:.1} MB / {:.1} MB), dropped {} TTL + {} LRU",
                    stats.entries_after,
                    stats.entries_before,
                    stats.bytes_after as f64 / 1_048_576.0,
                    stats.bytes_before as f64 / 1_048_576.0,
                    stats.ttl_dropped,
                    stats.lru_dropped,
                );
            }
            Ok(_) => {}
            Err(e) => eprintln!("ocr_cache: GC failed (non-fatal): {e}"),
        }
    }

    // Stamp the freshly-built index with the current schema version so a
    // future schema change can detect and rebuild a stale index.
    let _ = write_schema_version_sentinel(state_dir);

    let now_ms = unix_now_ms();
    let rebuild_schedule = read_search_config_for_state(state_dir)
        .ok()
        .flatten()
        .map(|config| config.rebuild_schedule)
        .unwrap_or_default();
    // Best effort: the app that started this worker usually holds the
    // settings database open, and then records the build itself when it
    // sees this worker succeed (`start_index_worker_monitor`).
    let _ = write_search_config_for_state(
        state_dir,
        &StoredSearchConfig {
            options: options.clone(),
            indexed_files,
            last_indexed_at_ms: Some(now_ms),
            rebuild_schedule,
        },
    );

    // Final status: a full success, or a promoted *partial* index (Task 4.1).
    let status = if resumable_interrupt {
        let message = if canceled {
            format!(
                "Index build cancelled — {indexed_files} files indexed and kept. Re-run to continue."
            )
        } else {
            format!(
                "Content indexing paused (low memory) — {indexed_files} files indexed so far. \
                 Filename search works now; re-run to index the rest."
            )
        };
        IndexWorkerStatusFile::finished(
            true,
            canceled,
            indexed_files,
            scanned_entries,
            skipped_files,
            message,
            None,
        )
        .as_partial()
    } else {
        let final_message = if reused_content_count > 0 {
            format!(
                "Indexed {indexed_files} entries (reused content for {reused_content_count} unchanged files — incremental rebuild)"
            )
        } else {
            format!("Indexed {indexed_files} entries")
        };
        IndexWorkerStatusFile::finished(
            true,
            false,
            indexed_files,
            scanned_entries,
            skipped_files,
            final_message,
            None,
        )
    };
    write_index_worker_status(status_path, &status)?;
    Ok(())
}

/// Standalone metadata-only indexer for the filename index (Phase 3, step 7b).
/// Walks `options.roots` and writes one filename-only document per entry —
/// names, paths, and metadata, never file content. Skipping content extraction
/// makes it far lighter than `build_index_in_worker`, so it can run on its own
/// schedule independent of content indexing.
fn build_filename_index_in_worker(
    state_dir: &Path,
    staging_dir: &Path,
    options: &FileSearchIndexOptions,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    let schema = build_filename_index_schema();
    let directory = MmapDirectory::open(staging_dir)
        .map_err(|error| format!("Cannot open staging filename index directory: {error}"))?;
    let index = Index::open_or_create(directory, schema.clone())
        .map_err(|error| format!("Cannot create staging filename index: {error}"))?;
    let fields = extract_filename_index_fields(&schema)?;
    let writer = index
        .writer_with_num_threads(index_writer_threads(), WRITER_MEMORY_BUDGET_BYTES_INDEXING)
        .map_err(|error| format!("Cannot create filename index writer: {error}"))?;

    write_index_worker_status(
        status_path,
        &IndexWorkerStatusFile::progress(0, 0, 0, "Preparing filename index".to_string()),
    )?;

    // Shared state for the parallel walk — same pattern as `build_index_in_worker`
    // but with no content-extraction state (no reuse cache, no sensitive scan).
    let writer = Arc::new(writer);
    let indexed_files_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let scanned_entries_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let skipped_files_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let first_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let last_progress_at_ms_shared: Arc<Mutex<u128>> = Arc::new(Mutex::new(unix_now_ms()));
    let should_stop_walker = Arc::new(AtomicBool::new(false));

    // Deduplicate roots so a child path covered by a parent root isn't walked
    // twice and producing duplicate entries.
    let mut deduped_roots: Vec<&String> = Vec::new();
    'outer: for root in &options.roots {
        let root_path = std::path::Path::new(root);
        for existing in &deduped_roots {
            if root_path.starts_with(std::path::Path::new(existing)) {
                continue 'outer;
            }
        }
        deduped_roots.retain(|existing| !std::path::Path::new(existing).starts_with(root_path));
        deduped_roots.push(root);
    }

    let performance_mode = options.performance_mode.clone();
    let on_battery = is_on_battery();
    let throttle_ms = index_throttle_sleep_ms(&performance_mode, on_battery);
    let options_for_filter = options.clone();

    for root in deduped_roots {
        if should_stop_walker.load(Ordering::Relaxed) {
            break;
        }

        let mut builder = index_walker(root, options, false);
        builder.threads(index_walker_threads(&performance_mode, on_battery));

        let walker = builder.build_parallel();
        let status_path_owned = status_path.to_path_buf();
        let cancel_path_owned = cancel_path.to_path_buf();

        walker.run(|| {
            let writer = Arc::clone(&writer);
            let indexed_files_atomic = Arc::clone(&indexed_files_atomic);
            let scanned_entries_atomic = Arc::clone(&scanned_entries_atomic);
            let skipped_files_atomic = Arc::clone(&skipped_files_atomic);
            let first_error = Arc::clone(&first_error);
            let last_progress = Arc::clone(&last_progress_at_ms_shared);
            let should_stop = Arc::clone(&should_stop_walker);
            let fields = fields;
            let options = options_for_filter.clone();
            let status_path = status_path_owned.clone();
            let cancel_path = cancel_path_owned.clone();
            let throttle_ms = throttle_ms;

            Box::new(move |entry_result| {
                if should_stop.load(Ordering::Relaxed) {
                    return ignore::WalkState::Quit;
                }

                let current_scanned = scanned_entries_atomic.fetch_add(1, Ordering::Relaxed) + 1;

                if current_scanned.is_multiple_of(500) && cancel_path.exists() {
                    should_stop.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }

                if current_scanned as usize > MAX_INDEXED_FILES {
                    if let Ok(mut guard) = first_error.lock() {
                        if guard.is_none() {
                            *guard = Some(format!(
                                "Filename index scan exceeded {MAX_INDEXED_FILES} entries. Choose fewer folders."
                            ));
                        }
                    }
                    should_stop.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }

                let entry = match entry_result {
                    Ok(value) => value,
                    Err(_) => {
                        skipped_files_atomic.fetch_add(1, Ordering::Relaxed);
                        return ignore::WalkState::Continue;
                    }
                };

                let path = entry.path();
                let file_type = entry.file_type();
                let is_file = file_type.map(|value| value.is_file()).unwrap_or(false);
                let is_dir = file_type.map(|value| value.is_dir()).unwrap_or(false);
                if !is_file && !is_dir {
                    return ignore::WalkState::Continue;
                }
                if !should_include_path(path, &options) {
                    skipped_files_atomic.fetch_add(1, Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                }

                let metadata = match fs::metadata(path) {
                    Ok(value) => value,
                    Err(_) => {
                        skipped_files_atomic.fetch_add(1, Ordering::Relaxed);
                        return ignore::WalkState::Continue;
                    }
                };

                let file_path = path.to_string_lossy().to_string();
                let extension = if is_file {
                    path.extension()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default()
                        .to_lowercase()
                } else {
                    String::new()
                };
                let file_name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
                    .to_string();
                let entry_type = if is_dir {
                    ENTRY_TYPE_FOLDER
                } else {
                    ENTRY_TYPE_FILE
                };
                let size = metadata.len();
                let modified_ms = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|value| value.as_millis() as u64)
                    .unwrap_or(0);
                let created_ms = metadata
                    .created()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|value| value.as_millis() as u64)
                    .unwrap_or(modified_ms);
                let parent_path = path
                    .parent()
                    .map(|parent| parent.to_string_lossy().to_string())
                    .unwrap_or_default();

                let document = doc!(
                    fields.path => file_path.clone(),
                    fields.path_exact => file_path,
                    fields.file_name => file_name,
                    fields.parent_path => parent_path,
                    fields.extension => extension,
                    fields.entry_type => entry_type,
                    fields.size => size,
                    fields.modified_ms => modified_ms,
                    fields.created_ms => created_ms,
                );
                if let Err(error) = writer.add_document(document) {
                    if let Ok(mut guard) = first_error.lock() {
                        if guard.is_none() {
                            *guard =
                                Some(format!("Cannot add document to filename index: {error}"));
                        }
                    }
                    should_stop.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }
                let new_indexed = indexed_files_atomic.fetch_add(1, Ordering::Relaxed) + 1;

                if new_indexed.is_multiple_of(INDEX_PROGRESS_EVERY_FILES) {
                    let now = unix_now_ms();
                    let mut should_emit = false;
                    if let Ok(mut last) = last_progress.lock() {
                        if now.saturating_sub(*last) >= INDEX_PROGRESS_MIN_INTERVAL_MS {
                            *last = now;
                            should_emit = true;
                        }
                    }
                    if should_emit {
                        let scanned = scanned_entries_atomic.load(Ordering::Relaxed);
                        let skipped = skipped_files_atomic.load(Ordering::Relaxed);
                        let _ = write_index_worker_status(
                            &status_path,
                            &IndexWorkerStatusFile::progress(
                                new_indexed,
                                scanned,
                                skipped,
                                format!("Indexed {new_indexed} filenames"),
                            ),
                        );
                    }
                }

                if throttle_ms > 0
                    && new_indexed > 0
                    && new_indexed.is_multiple_of(INDEX_THROTTLE_EVERY_FILES)
                {
                    thread::sleep(Duration::from_millis(throttle_ms));
                }

                ignore::WalkState::Continue
            })
        });
    }

    let indexed_files = indexed_files_atomic.load(Ordering::Relaxed);
    let scanned_entries = scanned_entries_atomic.load(Ordering::Relaxed);
    let skipped_files = skipped_files_atomic.load(Ordering::Relaxed);

    if let Ok(guard) = first_error.lock() {
        if let Some(err) = guard.as_ref() {
            let _ = fs::remove_dir_all(staging_dir);
            return Err(err.clone());
        }
    }

    if cancel_path.exists() {
        let _ = fs::remove_dir_all(staging_dir);
        write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::finished(
                false,
                true,
                indexed_files,
                scanned_entries,
                skipped_files,
                "Filename index build cancelled".to_string(),
                None,
            ),
        )?;
        return Ok(());
    }

    // Reclaim sole ownership of the writer for the final commit (needs `&mut`).
    let mut writer = Arc::try_unwrap(writer)
        .map_err(|_| "Internal: filename index writer still held by a worker".to_string())?;

    write_index_worker_status(
        status_path,
        &IndexWorkerStatusFile::progress(
            indexed_files,
            scanned_entries,
            skipped_files,
            "Finalizing filename index".to_string(),
        ),
    )?;
    writer
        .commit()
        .map_err(|error| format!("Cannot finalize filename index: {error}"))?;
    writer
        .wait_merging_threads()
        .map_err(|error| format!("Cannot finish filename index merges: {error}"))?;
    drop(index);

    swap_in_staging_filename_index_for_state(state_dir, staging_dir)?;
    let _ = write_filename_schema_version_sentinel(state_dir);

    write_index_worker_status(
        status_path,
        &IndexWorkerStatusFile::finished(
            true,
            false,
            indexed_files,
            scanned_entries,
            skipped_files,
            format!("Indexed {indexed_files} filenames"),
            None,
        ),
    )?;
    Ok(())
}

fn run_index_worker_watch_mode(
    state_dir: &Path,
    options_path: &Path,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    let result =
        run_index_worker_watch_mode_inner(state_dir, options_path, status_path, cancel_path);
    if let Err(error) = &result {
        let _ = write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::finished(
                false,
                false,
                0,
                0,
                0,
                error.clone(),
                Some(error.clone()),
            ),
        );
    }
    result
}

fn run_index_worker_watch_mode_inner(
    state_dir: &Path,
    options_path: &Path,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    let options_bytes = fs::read(options_path)
        .map_err(|error| format!("Cannot read watcher worker options: {error}"))?;
    let options: FileSearchIndexOptions = serde_json::from_slice(&options_bytes)
        .map_err(|error| format!("Cannot parse watcher worker options: {error}"))?;
    let mut options = normalize_index_options(options)?;
    if !options.watcher_enabled {
        return Err("Live watcher is disabled in search settings".to_string());
    }
    options.roots = watchable_index_roots(&options);
    if options.roots.is_empty() {
        return Err(
            "Live watcher only works for explicit folders. Drive roots are not watched; use scheduled rebuilds or add specific folders."
                .to_string(),
        );
    }

    recover_content_index_dirs_for_state(state_dir)?;
    let active_dir = active_search_index_dir_for_state(state_dir);
    if !active_dir.exists() {
        return Err("Search index is not ready for watching".to_string());
    }

    let indexed_files = read_search_config_for_state(state_dir)
        .ok()
        .flatten()
        .map(|config| config.indexed_files)
        .unwrap_or(0);
    let strategy = watcher_strategy_for_index_size(indexed_files);
    let notify_roots = options.roots.clone();

    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        // Drop noise events at the source so they never wake the main loop.
        // Access events (file reads by Search Indexer/Defender) and metadata-only
        // changes never change index content — skip them entirely.
        if let Ok(ref event) = result {
            use notify::event::ModifyKind;
            match &event.kind {
                EventKind::Access(_) => return,
                EventKind::Modify(ModifyKind::Metadata(_)) => return,
                EventKind::Modify(ModifyKind::Other) => return,
                EventKind::Any | EventKind::Other => return,
                _ => {}
            }
        }
        let _ = tx.send(result);
    })
    .map_err(|error| format!("Cannot start isolated file watcher: {error}"))?;

    let mut watched_roots = 0_u64;
    for root in &notify_roots {
        let watch_path = Path::new(root);
        if !watch_path.exists() {
            continue;
        }
        if watcher.watch(watch_path, RecursiveMode::Recursive).is_ok() {
            watched_roots += 1;
        }
    }
    if watched_roots == 0 {
        return Err(
            "No explicit folder roots can be live watched. Drive roots are not watched; use scheduled rebuilds or add specific folders."
                .to_string(),
        );
    }

    // Minimum gap between batch applies. Events accumulate in the channel during
    // this wait and are processed together — fewer index opens, less CPU overall.
    const MIN_APPLY_INTERVAL_MS: u128 = 5_000;
    // Periodic full reconciliation, plus the loop-gap above which we assume the
    // process was suspended (sleep/hibernate) and reconcile immediately.
    const RECONCILE_INTERVAL_MS: u128 = 6 * 60 * 60 * 1_000;
    const SLEEP_GAP_THRESHOLD_MS: u128 = 2 * 60 * 1_000;
    let mut last_apply_ms: u128 = 0;
    let mut last_reconcile_ms: u128 = unix_now_ms();
    let mut last_loop_iter_ms: u128 = unix_now_ms();
    let mut updated_total = 0_u64;
    let mut deleted_total = 0_u64;
    write_index_worker_status(
        status_path,
        &IndexWorkerStatusFile::progress(
            updated_total,
            watched_roots,
            deleted_total,
            format!(
                "Watching {watched_roots} explicit folder root(s) with {}",
                strategy.name
            ),
        ),
    )?;

    loop {
        if cancel_path.exists() {
            write_index_worker_status(
                status_path,
                &IndexWorkerStatusFile::finished(
                    false,
                    true,
                    updated_total,
                    watched_roots,
                    deleted_total,
                    "Index watcher stopped".to_string(),
                    None,
                ),
            )?;
            return Ok(());
        }

        // Reconciliation: the live watcher can miss filesystem events while
        // this process is suspended (sleep/hibernate). Periodically — and
        // immediately after a detected suspension — walk the watched roots and
        // diff them against the index to heal any missed changes.
        let now_ms = unix_now_ms();
        let loop_gap_ms = now_ms.saturating_sub(last_loop_iter_ms);
        last_loop_iter_ms = now_ms;
        let resumed_from_suspend = loop_gap_ms > SLEEP_GAP_THRESHOLD_MS;
        if resumed_from_suspend
            || now_ms.saturating_sub(last_reconcile_ms) >= RECONCILE_INTERVAL_MS
        {
            last_reconcile_ms = now_ms;
            if let Ok((reconciled_updates, reconciled_deletes)) =
                reconcile_watched_index(&active_dir, &options)
            {
                if reconciled_updates > 0 || reconciled_deletes > 0 {
                    updated_total += reconciled_updates;
                    deleted_total += reconciled_deletes;
                    let _ = write_index_worker_status(
                        status_path,
                        &IndexWorkerStatusFile::progress(
                            updated_total,
                            watched_roots,
                            deleted_total,
                            format!(
                                "Reconciled after {}: +{reconciled_updates} / -{reconciled_deletes}",
                                if resumed_from_suspend { "resume" } else { "interval" }
                            ),
                        ),
                    );
                }
            }
            last_apply_ms = unix_now_ms();
            continue;
        }

        // Rate limiting: enforce a minimum gap between batch applies.
        // During this sleep events accumulate in the channel so the next
        // batch covers all of them at once — fewer index open/commit cycles.
        if last_apply_ms > 0 {
            let elapsed = unix_now_ms().saturating_sub(last_apply_ms);
            if elapsed < MIN_APPLY_INTERVAL_MS {
                thread::sleep(Duration::from_millis(500));
                continue;
            }
        }

        let mut pending = Vec::new();
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(value) => pending.push(value),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Index watcher event channel disconnected".to_string());
            }
        }
        collect_watcher_events(&rx, &mut pending, strategy, cancel_path);

        let (updated_batch, deleted_batch) =
            apply_watch_batch_to_index(&active_dir, &options, pending)?;

        // Always mark the apply time so the rate limiter kicks in even when
        // a batch produced no changes (e.g. all events were already filtered).
        // Without this, noise-only batches would loop continuously.
        last_apply_ms = unix_now_ms();

        if updated_batch == 0 && deleted_batch == 0 {
            continue;
        }

        updated_total += updated_batch;
        deleted_total += deleted_batch;
        write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::progress(
                updated_total,
                watched_roots,
                deleted_total,
                format!("{}: +{updated_batch} / -{deleted_batch}", strategy.name),
            ),
        )?;
    }
}

fn collect_watcher_events(
    rx: &std::sync::mpsc::Receiver<notify::Result<Event>>,
    pending: &mut Vec<notify::Result<Event>>,
    strategy: WatcherStrategy,
    cancel_path: &Path,
) {
    let started_at = unix_now_ms();
    while pending.len() < strategy.max_batch
        && unix_now_ms().saturating_sub(started_at) < strategy.debounce_ms as u128
        && !cancel_path.exists()
    {
        match rx.try_recv() {
            Ok(value) => pending.push(value),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
    }
}

/// Periodic reconciliation: walk the watched roots and diff them against the
/// current index, healing changes the live watcher missed (events dropped
/// while the process was suspended during sleep/hibernate). New or modified
/// files are re-indexed; index entries whose file is gone are removed. The
/// walk mirrors the bulk indexer's `ignore` filtering so the same files are
/// considered. Returns (updated, deleted) counts.
fn reconcile_watched_index(
    active_dir: &Path,
    options: &FileSearchIndexOptions,
) -> Result<(u64, u64), String> {
    use notify::event::{ModifyKind, RemoveKind};

    let Some(cache) = load_existing_index_for_reuse(active_dir) else {
        // No readable index to diff against — a scheduled rebuild will cover it.
        return Ok((0, 0));
    };

    let mut seen: HashSet<String> = HashSet::with_capacity(cache.map.len());
    let mut events: Vec<notify::Result<Event>> = Vec::new();
    let offline = offline_roots(&options.roots);

    for root in &options.roots {
        if offline.contains(&root.trim_end_matches(['\\', '/']).to_lowercase()) {
            continue;
        }
        let builder = index_walker(root, options, true);
        for entry in builder.build().flatten() {
            let path = entry.path();
            if !should_include_path(path, options) {
                continue;
            }
            let path_lower = path.to_string_lossy().to_lowercase();
            seen.insert(path_lower.clone());
            let is_file = entry
                .file_type()
                .map(|file_type| file_type.is_file())
                .unwrap_or(false);
            if !is_file {
                continue;
            }
            let Ok(meta) = fs::metadata(path) else {
                continue;
            };
            let modified_ms = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_millis() as u64)
                .unwrap_or(0);
            let unchanged = cache.map.get(&path_lower).is_some_and(|existing| {
                existing.modified_ms == modified_ms && existing.size == meta.len()
            });
            if !unchanged {
                events.push(Ok(Event::new(EventKind::Modify(ModifyKind::Any))
                    .add_path(path.to_path_buf())));
            }
        }
    }

    // Index entries whose file no longer exists on disk → deletions the live
    // watcher missed. Resolve the case-preserved path from the stored doc.
    for (path_lower, existing) in &cache.map {
        if seen.contains(path_lower) || offline.iter().any(|root| is_under_root(path_lower, root)) {
            continue;
        }
        let doc: TantivyDocument = match cache.searcher.doc(existing.address) {
            Ok(doc) => doc,
            Err(_) => continue,
        };
        if let Some(real_path) = doc_text(&doc, cache.fields.path) {
            events.push(Ok(Event::new(EventKind::Remove(RemoveKind::Any))
                .add_path(PathBuf::from(real_path))));
        }
    }

    if events.is_empty() {
        return Ok((0, 0));
    }
    // Release the read-side mmap before apply_watch_batch_to_index opens a writer.
    drop(cache);
    apply_watch_batch_to_index(active_dir, options, events)
}

fn apply_watch_batch_to_index(
    active_dir: &Path,
    options: &FileSearchIndexOptions,
    pending: Vec<notify::Result<Event>>,
) -> Result<(u64, u64), String> {
    // Filter out pure access events (file reads by Windows Search, Defender, etc.)
    // and any other events that can never result in an index change, before we pay
    // the cost of opening the index at all.
    let events: Vec<Event> = pending
        .into_iter()
        .filter_map(|r| r.ok())
        .filter(|e| !matches!(e.kind, EventKind::Access(_)))
        .collect();

    if events.is_empty() {
        return Ok((0, 0));
    }

    let directory = MmapDirectory::open(active_dir)
        .map_err(|error| format!("Cannot open active search index directory: {error}"))?;
    let index = Index::open(directory)
        .map_err(|error| format!("Cannot open active search index: {error}"))?;
    let schema = index.schema();
    let fields = extract_fields(&schema)?;
    // 1 writer thread — the watcher writes tiny amounts per batch.
    let mut writer = index
        .writer_with_num_threads(1, WRITER_MEMORY_BUDGET_BYTES_IDLE)
        .map_err(|error| format!("Cannot open watcher search writer: {error}"))?;
    let max_content_bytes = (options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB) as usize)
        .saturating_mul(1024)
        .max(8 * 1024);
    let mut updated_batch = 0_u64;
    let mut deleted_batch = 0_u64;

    for event in events {
        let (updated, deleted) =
            apply_watch_event_to_writer(&event, &mut writer, &fields, options, max_content_bytes)?;
        updated_batch += updated;
        deleted_batch += deleted;
    }

    if updated_batch == 0 && deleted_batch == 0 {
        return Ok((0, 0));
    }

    writer
        .commit()
        .map_err(|error| format!("Cannot commit isolated watcher update: {error}"))?;
    writer
        .wait_merging_threads()
        .map_err(|error| format!("Cannot finish isolated watcher merge: {error}"))?;
    Ok((updated_batch, deleted_batch))
}

fn index_writer_threads() -> usize {
    let available = thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(2);
    available.clamp(1, INDEX_WRITER_THREADS_MAX)
}

/// How many worker threads to use for the parallel bulk indexer. Each thread
/// runs file enumeration AND content extraction (PDF/Office parsing), so we
/// want close to as many threads as CPU cores — extraction is the dominant
/// CPU cost.
///
/// This count drives THREE things at once: the file-walk threads, the number
/// of out-of-process extractor children (each a separate process), and the
/// indexer threads — so it's the dominant lever on both CPU and RAM.
///
/// `fast` uses all cores but one (stays responsive); `balanced` (the default)
/// is a capped middle ground; `quiet` is deliberately minimal — 1–2 threads /
/// extractor processes — so a background build barely registers (it just takes
/// longer). On battery the result is halved again to limit power draw.
fn index_walker_threads(performance_mode: &str, on_battery: bool) -> usize {
    index_walker_threads_with_ocr(performance_mode, on_battery, false)
}

/// Wave 8.3 (2026-05-28): OCR-aware pool sizing. When the user has opted
/// into OCR-on-Index, the workload becomes CPU-bound (Tesseract per file)
/// rather than I/O-bound (PDF/DOCX parsing). Raise the per-mode caps so
/// extra cores get used for parallel OCR:
///
///   fast   : cpu - 1, clamp 2..=16  (unchanged — already optimal)
///   balanced + OCR : cpu - 1, clamp 2..=12  (was clamp 2..=8)
///   balanced + no OCR : unchanged  (clamp 2..=8)
///   quiet + OCR : cpu / 2, clamp 1..=4  (was clamp 1..=2 — let OCR
///                                        progress even in quiet mode)
///   quiet + no OCR : unchanged  (clamp 1..=2)
///
/// Battery halving still applies on top, so a laptop on battery in
/// balanced mode still gets ~6 instead of 12.
fn index_walker_threads_with_ocr(
    performance_mode: &str,
    on_battery: bool,
    ocr_enabled: bool,
) -> usize {
    let available = thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(INDEX_WALKER_THREADS_FALLBACK);
    let threads = match normalize_index_performance_mode(performance_mode).as_str() {
        "fast" => available.saturating_sub(1).clamp(2, 16),
        "quiet" => {
            if ocr_enabled {
                (available / 2).clamp(1, 4)
            } else {
                (available / 4).clamp(1, 2)
            }
        }
        _ => {
            if ocr_enabled {
                available.saturating_sub(1).clamp(2, 12)
            } else {
                available.clamp(2, 8)
            }
        }
    };
    if on_battery {
        (threads / 2).max(1)
    } else {
        threads
    }
}

fn apply_watch_event_to_writer(
    event: &Event,
    writer: &mut IndexWriter,
    fields: &SearchFields,
    options: &FileSearchIndexOptions,
    max_content_bytes: usize,
) -> Result<(u64, u64), String> {
    let mut updated = 0_u64;
    let mut deleted = 0_u64;

    match &event.kind {
        EventKind::Remove(_) => {
            for path in &event.paths {
                // Can't check (offline): keep it.
                if !path.is_file() && !is_confirmed_gone(path) {
                    continue;
                }
                let path_text = path.to_string_lossy().to_string();
                writer.delete_term(Term::from_field_text(fields.path_exact, &path_text));
                deleted += 1;
            }
        }
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any | EventKind::Other => {
            for path in &event.paths {
                match path.try_exists() {
                    Ok(true) => {}
                    Ok(false) => {
                        let path_text = path.to_string_lossy().to_string();
                        writer.delete_term(Term::from_field_text(fields.path_exact, &path_text));
                        deleted += 1;
                        continue;
                    }
                    Err(_) => continue,
                }

                if !path.is_file() && !path.is_dir() {
                    continue;
                }
                if !should_include_path(path, options) {
                    let path_text = path.to_string_lossy().to_string();
                    writer.delete_term(Term::from_field_text(fields.path_exact, &path_text));
                    deleted += 1;
                    continue;
                }
                // Content index = readable document files only (matching the
                // build walk). Folders and media/binary files are not indexed
                // here — they stay searchable by name in the filename index.
                // Wave 8.1 (2026-05-28): same OCR-aware gate as the build
                // walk — images only when OCR is enabled + path in scope.
                if !path.is_file() || !should_index_for_content(path, options) {
                    continue;
                }
                if upsert_path_document(path, writer, fields, max_content_bytes).is_ok() {
                    updated += 1;
                }
            }
        }
        _ => {}
    }

    Ok((updated, deleted))
}

fn upsert_path_document(
    path: &Path,
    writer: &mut IndexWriter,
    fields: &SearchFields,
    max_content_bytes: usize,
) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| format!("Cannot stat file: {error}"))?;
    let is_file = metadata.is_file();
    let is_dir = metadata.is_dir();
    if !is_file && !is_dir {
        return Ok(());
    }

    let file_path = path.to_string_lossy().to_string();
    let extension = if is_file {
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_lowercase()
    } else {
        String::new()
    };
    // `.ki` notes index + display by their human title, matching the filename
    // index so a note reads the same in "Inside" and "Files" results.
    let file_name = if is_file && is_ki_path(path) {
        note_display_title(path)
    } else {
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string()
    };
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0);
    let created_ms = metadata
        .created()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_millis() as u64)
        .unwrap_or(modified_ms);
    let content = if is_file {
        read_text_for_index(path, max_content_bytes).unwrap_or_default()
    } else {
        String::new()
    };
    let entry_type = if is_dir {
        ENTRY_TYPE_FOLDER
    } else {
        ENTRY_TYPE_FILE
    };

    writer.delete_term(Term::from_field_text(fields.path_exact, &file_path));
    let mut document = doc!(
        fields.path => file_path.clone(),
        fields.path_exact => file_path,
        fields.file_name => file_name,
        fields.extension => extension,
        fields.content => content,
        fields.size => metadata.len(),
        fields.modified_ms => modified_ms,
    );
    if let Some(created_ms_field) = fields.created_ms {
        document.add_u64(created_ms_field, created_ms);
    }
    if let Some(entry_type_field) = fields.entry_type {
        document.add_text(entry_type_field, entry_type);
    }
    writer
        .add_document(document)
        .map_err(|error| format!("Cannot index path update: {error}"))?;

    Ok(())
}

/// Incrementally update BOTH local indexes — CONTENT ("Inside" search) and
/// FILENAME ("Files" search + command palette) — for a set of note `.ki`
/// files, with no full rebuild. This makes a saved note instantly findable in
/// either search mode.
///
/// Each index is handled independently and best-effort, mirroring the watchers'
/// proven isolated-writer pattern: open a short-lived writer directly on the
/// active index directory (the engines hold no persistent writer during normal
/// operation), upsert/delete, commit. If an index isn't built yet — or a write
/// races a rebuild — that half silently no-ops. Note-saving never depends on
/// any of this, so a bug here cannot break Notes OR the search pillars. The
/// caller fire-and-forgets. `upserts` = note files to (re)index; `deletes` =
/// note paths whose docs to drop (on delete).
// `(async)` so the heavy Tantivy index commits (two short-lived writers +
// commit + wait_merging_threads) run on a worker thread, not the Tauri main/
// event-loop thread. The frontend fire-and-forgets this on a 2500ms debounce
// after a note save; on the main thread the commit would stall the UI the same
// way a sync `write_note` does. Note-saving never depends on this either way.
pub fn update_notes_index(
    app: std::sync::Arc<dyn dash_core::Host>,
    upserts: Vec<String>,
    deletes: Vec<String>,
) -> Result<u64, String> {
    on_owner!("update_notes_index", { "upserts": upserts, "deletes": deletes });
    let state_dir = search_index_dir(&app)?;
    let mut changed = 0_u64;
    changed += update_notes_content_index(
        &active_search_index_dir_for_state(&state_dir),
        &upserts,
        &deletes,
    )
    .unwrap_or(0);
    changed += update_notes_filename_index(
        &active_filename_index_dir_for_state(&state_dir),
        &upserts,
        &deletes,
    )
    .unwrap_or(0);
    Ok(changed)
}

/// True if `path` has the `.ki` note extension (case-insensitive). Cheap —
/// touches the path only, never the filesystem.
fn is_ki_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("ki"))
        == Some(true)
}

/// True if `path` is an existing `.ki` file (notes only — never index arbitrary
/// paths handed to the command).
fn is_note_file(path: &str) -> bool {
    let p = Path::new(path);
    p.is_file() && is_ki_path(p)
}

/// Best-effort human title for a `.ki` note: frontmatter `title:`, else the
/// first `# ` heading, else the file stem. Used so filename/palette search
/// matches (and displays) the title the user gave the note rather than its
/// on-disk slug like `untitled-3.ki`, which they never think in. Reads the
/// file (notes are tiny); callers gate on `is_ki_path` so this never runs in
/// the full-walk hot path for non-note files.
fn note_display_title(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled")
        .to_string();
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return stem,
    };
    let normalized = content.replace("\r\n", "\n");
    // Frontmatter `title:` (the form the Notes editor writes).
    if let Some(rest) = normalized.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            for line in rest[..end].lines() {
                if let Some(value) = line.trim().strip_prefix("title:") {
                    let value = value
                        .trim()
                        .trim_matches('"')
                        .trim_matches('\'')
                        .trim();
                    if !value.is_empty() {
                        return value.to_string();
                    }
                }
            }
        }
    }
    // First Markdown ATX heading, else the stem.
    for line in normalized.lines() {
        let trimmed = line.trim_start();
        if let Some(heading) = trimmed.strip_prefix("# ") {
            let heading = heading.trim();
            if !heading.is_empty() {
                return heading.to_string();
            }
        }
        if !trimmed.is_empty() && trimmed != "---" {
            break;
        }
    }
    stem
}

fn update_notes_content_index(
    active_dir: &Path,
    upserts: &[String],
    deletes: &[String],
) -> Result<u64, String> {
    if !active_dir.join("meta.json").exists() {
        return Ok(0); // content index not built yet
    }
    let directory = MmapDirectory::open(active_dir)
        .map_err(|error| format!("Cannot open content index for notes: {error}"))?;
    let index =
        Index::open(directory).map_err(|error| format!("Cannot open content index: {error}"))?;
    let schema = index.schema();
    let fields = extract_fields(&schema)?;
    let mut writer = index
        .writer_with_num_threads(1, WRITER_MEMORY_BUDGET_BYTES_IDLE)
        .map_err(|error| format!("Cannot open notes content writer: {error}"))?;
    // Notes are small Markdown — a generous fixed cap fully covers their text.
    let max_content_bytes: usize = 256 * 1024;
    let mut changed = 0_u64;
    for path in deletes {
        writer.delete_term(Term::from_field_text(fields.path_exact, path));
        changed += 1;
    }
    for path in upserts {
        if is_note_file(path)
            && upsert_path_document(Path::new(path), &mut writer, &fields, max_content_bytes).is_ok()
        {
            changed += 1;
        }
    }
    if changed == 0 {
        return Ok(0);
    }
    writer
        .commit()
        .map_err(|error| format!("Cannot commit notes content index: {error}"))?;
    let _ = writer.wait_merging_threads();
    Ok(changed)
}

fn update_notes_filename_index(
    active_dir: &Path,
    upserts: &[String],
    deletes: &[String],
) -> Result<u64, String> {
    if !active_dir.join("meta.json").exists() {
        return Ok(0); // filename index not built yet
    }
    let directory = MmapDirectory::open(active_dir)
        .map_err(|error| format!("Cannot open filename index for notes: {error}"))?;
    let index =
        Index::open(directory).map_err(|error| format!("Cannot open filename index: {error}"))?;
    let schema = index.schema();
    let fields = extract_filename_index_fields(&schema)?;
    let mut writer = index
        .writer_with_num_threads(1, WRITER_MEMORY_BUDGET_BYTES_IDLE)
        .map_err(|error| format!("Cannot open notes filename writer: {error}"))?;
    let mut changed = 0_u64;
    for path in deletes {
        delete_filename_doc(&writer, &fields, path);
        changed += 1;
    }
    for path in upserts {
        if is_note_file(path) && upsert_filename_document(Path::new(path), &writer, &fields).is_ok() {
            changed += 1;
        }
    }
    if changed == 0 {
        return Ok(0);
    }
    writer
        .commit()
        .map_err(|error| format!("Cannot commit notes filename index: {error}"))?;
    let _ = writer.wait_merging_threads();
    Ok(changed)
}

fn build_search_schema() -> Schema {
    // Any change to these fields (added/removed/retyped, tokenizer changes)
    // must bump `SEARCH_SCHEMA_VERSION` so stale on-disk indexes auto-rebuild.
    let mut builder = SchemaBuilder::new();
    builder.add_text_field("path", TEXT | STORED);
    builder.add_text_field("path_exact", STRING | STORED);
    builder.add_text_field("file_name", TEXT | STORED);
    builder.add_text_field("entry_type", STRING | STORED);
    builder.add_text_field("extension", STRING | STORED);
    // STORED lets the content search return snippets without re-reading the
    // source file on every result. Older indexes without STORED still work —
    // build_content_result_item falls back to a disk read when the doc has
    // no stored content text.
    builder.add_text_field("content", TEXT | STORED);
    // Multi-valued tag field for sensitive-content findings. Populated at
    // index time by the `sensitive_scan` module. STRING means each value is
    // matched literally; STORED so the UI can list which kinds were found.
    // Old indexes without this field continue to work — searches return no
    // matches for sensitive queries until the user rebuilds.
    builder.add_text_field("sensitive_kinds", STRING | STORED);
    builder.add_u64_field("size", FAST | STORED);
    builder.add_u64_field("modified_ms", FAST | STORED);
    builder.add_u64_field("created_ms", FAST | STORED);
    builder.build()
}

fn extract_fields(schema: &Schema) -> Result<SearchFields, String> {
    Ok(SearchFields {
        path: schema
            .get_field("path")
            .map_err(|_| "Search schema field 'path' missing".to_string())?,
        path_exact: schema
            .get_field("path_exact")
            .map_err(|_| "Search schema field 'path_exact' missing".to_string())?,
        file_name: schema
            .get_field("file_name")
            .map_err(|_| "Search schema field 'file_name' missing".to_string())?,
        entry_type: schema.get_field("entry_type").ok(),
        extension: schema
            .get_field("extension")
            .map_err(|_| "Search schema field 'extension' missing".to_string())?,
        content: schema
            .get_field("content")
            .map_err(|_| "Search schema field 'content' missing".to_string())?,
        sensitive_kinds: schema.get_field("sensitive_kinds").ok(),
        size: schema
            .get_field("size")
            .map_err(|_| "Search schema field 'size' missing".to_string())?,
        modified_ms: schema
            .get_field("modified_ms")
            .map_err(|_| "Search schema field 'modified_ms' missing".to_string())?,
        created_ms: schema.get_field("created_ms").ok(),
    })
}

/// Schema for the dedicated filename index — names, paths, and metadata only,
/// no file content. Kept separate from the content index so filename search is
/// never blocked by content indexing. Any field change here must bump
/// `FILENAME_SCHEMA_VERSION`. Wired in by Phase 2 steps 6b–6e.
#[allow(dead_code)]
fn build_filename_index_schema() -> Schema {
    let mut builder = SchemaBuilder::new();
    builder.add_text_field("path", TEXT | STORED);
    builder.add_text_field("path_exact", STRING | STORED);
    builder.add_text_field("file_name", TEXT | STORED);
    builder.add_text_field("parent_path", TEXT | STORED);
    builder.add_text_field("extension", STRING | STORED);
    builder.add_text_field("entry_type", STRING | STORED);
    builder.add_u64_field("size", FAST | STORED);
    builder.add_u64_field("modified_ms", FAST | STORED);
    builder.add_u64_field("created_ms", FAST | STORED);
    builder.build()
}

/// Field handles for the filename index. `Copy` so the parallel indexer's
/// per-worker closure can hold its own copy, like `SearchFields`.
#[derive(Clone, Copy)]
struct FilenameIndexFields {
    path: Field,
    path_exact: Field,
    file_name: Field,
    parent_path: Field,
    extension: Field,
    entry_type: Field,
    size: Field,
    modified_ms: Field,
    created_ms: Field,
}

fn extract_filename_index_fields(schema: &Schema) -> Result<FilenameIndexFields, String> {
    let field = |name: &str| {
        schema
            .get_field(name)
            .map_err(|_| format!("Filename index schema field '{name}' missing"))
    };
    Ok(FilenameIndexFields {
        path: field("path")?,
        path_exact: field("path_exact")?,
        file_name: field("file_name")?,
        parent_path: field("parent_path")?,
        extension: field("extension")?,
        entry_type: field("entry_type")?,
        size: field("size")?,
        modified_ms: field("modified_ms")?,
        created_ms: field("created_ms")?,
    })
}

/// Active / previous / staging directories for the filename index — siblings
/// of the content index's directories inside the same state directory.
#[allow(dead_code)]
fn active_filename_index_dir_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(FILENAME_INDEX_ACTIVE_DIR)
}

/// Quality Pass Wave 1 / Option B (2026-05-28): enumerate every FILE
/// in the active filename index as `lowercase_path → (real_path, size,
/// modified_ms)`. Used by DuplicateFinder to skip the metadata stat for
/// already-indexed files (10-100x speedup on indexed roots) — the
/// stored `size` IS the prefilter the duplicate finder builds from
/// scratch every run.
///
/// Returns `None` when no filename index exists yet, the index can't be
/// opened (corrupt / schema mismatch), or the schema-projection fails.
/// Caller should fall back to the walker-only path in that case.
///
/// Folders are filtered out via the `entry_type` field — DuplicateFinder
/// works on files only. Lowercase keys because Windows paths are
/// case-insensitive; the real (case-preserved) path is the value so
/// downstream code uses the canonical form.
pub fn enumerate_indexed_file_sizes(
    state_dir: &Path,
) -> Option<std::collections::HashMap<String, (String, u64, u64)>> {
    let dir = active_filename_index_dir_for_state(state_dir);
    if !dir.exists() {
        return None;
    }
    let index = Index::open_in_dir(&dir).ok()?;
    let schema = index.schema();
    let fields = extract_filename_index_fields(&schema).ok()?;
    let reader: IndexReader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .ok()?;
    let searcher = reader.searcher();

    let mut out: std::collections::HashMap<String, (String, u64, u64)> =
        std::collections::HashMap::with_capacity(8192);
    for segment_reader in searcher.segment_readers() {
        let Ok(store_reader) = segment_reader.get_store_reader(0) else {
            continue;
        };
        let alive_bitset = segment_reader.alive_bitset();
        for doc_id in 0..segment_reader.max_doc() {
            if let Some(bitset) = alive_bitset {
                if !bitset.is_alive(doc_id) {
                    continue;
                }
            }
            let Ok(doc): Result<TantivyDocument, _> = store_reader.get(doc_id) else {
                continue;
            };
            // Skip dirs — duplicate finder only works on files. The
            // entry_type field is a single string of "file" or "dir".
            if let Some(kind) = doc_text(&doc, fields.entry_type) {
                if kind != "file" {
                    continue;
                }
            }
            let Some(path) = doc_text(&doc, fields.path) else {
                continue;
            };
            let size = doc_u64(&doc, fields.size).unwrap_or(0);
            let modified_ms = doc_u64(&doc, fields.modified_ms).unwrap_or(0);
            out.insert(path.to_lowercase(), (path, size, modified_ms));
        }
    }
    Some(out)
}

#[allow(dead_code)]
fn previous_filename_index_dir_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(FILENAME_INDEX_PREVIOUS_DIR)
}

#[allow(dead_code)]
fn staging_filename_index_dir_for_state(state_dir: &Path, stamp_ms: u128) -> PathBuf {
    state_dir.join(format!("{FILENAME_STAGING_INDEX_PREFIX}{stamp_ms}"))
}

/// Schema-version sentinel for the filename index — mirrors the content
/// index's sentinel but in its own file, so the two indexes version
/// independently. Reuses `SchemaVersionSentinel`.
#[allow(dead_code)]
fn filename_schema_version_path_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(FILENAME_SCHEMA_VERSION_FILE)
}

#[allow(dead_code)]
fn write_filename_schema_version_sentinel(state_dir: &Path) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(&SchemaVersionSentinel {
        version: FILENAME_SCHEMA_VERSION,
    })
    .map_err(|error| format!("Cannot serialize filename schema sentinel: {error}"))?;
    fs::write(filename_schema_version_path_for_state(state_dir), bytes)
        .map_err(|error| format!("Cannot write filename schema sentinel: {error}"))
}

#[allow(dead_code)]
fn read_filename_schema_version_sentinel(state_dir: &Path) -> Option<u32> {
    let bytes = fs::read(filename_schema_version_path_for_state(state_dir)).ok()?;
    serde_json::from_slice::<SchemaVersionSentinel>(&bytes)
        .ok()
        .map(|sentinel| sentinel.version)
}

fn search_index_dir(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<PathBuf, String> {
    let base = app
        .data_dir()
        .map_err(|error| format!("Cannot resolve app data directory: {error}"))?;
    Ok(base.join(SEARCH_INDEX_DIR))
}

fn active_search_index_dir(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<PathBuf, String> {
    Ok(active_search_index_dir_for_state(&search_index_dir(app)?))
}

/// Total on-disk size, in bytes, of every file under `dir` (recursively). Used
/// to report how much disk each Tantivy index occupies. A missing or unreadable
/// directory — e.g. an index that was never built — reports 0.
fn directory_size_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            total = total.saturating_add(directory_size_bytes(&entry.path()));
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    total
}

fn active_search_index_dir_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(ACTIVE_SEARCH_INDEX_DIR)
}

fn previous_search_index_dir_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(PREVIOUS_SEARCH_INDEX_DIR)
}

fn staging_search_index_dir_for_state(state_dir: &Path, stamp_ms: u128) -> PathBuf {
    state_dir.join(format!("{STAGING_SEARCH_INDEX_PREFIX}{stamp_ms}"))
}

fn search_config_path_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(SEARCH_CONFIG_FILE)
}

fn write_index_worker_status(path: &Path, status: &IndexWorkerStatusFile) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(status)
        .map_err(|error| format!("Cannot serialize index worker status: {error}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create index worker status directory: {error}"))?;
    }
    let temp_path = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(INDEX_WORKER_STATUS_FILE)
    ));
    fs::write(&temp_path, bytes)
        .map_err(|error| format!("Cannot write index worker status: {error}"))?;
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&temp_path, path)
        .map_err(|error| format!("Cannot publish index worker status: {error}"))
}

fn read_index_worker_status(path: &Path) -> Result<Option<IndexWorkerStatusFile>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        fs::read(path).map_err(|error| format!("Cannot read index worker status: {error}"))?;
    match serde_json::from_slice::<IndexWorkerStatusFile>(&raw) {
        Ok(status) => Ok(Some(status)),
        Err(_) => Ok(None),
    }
}

fn request_stale_worker_stop(status_path: &Path, cancel_path: &Path) {
    let Ok(Some(status)) = read_index_worker_status(status_path) else {
        return;
    };
    if status.running && !status.finished {
        let _ = fs::write(cancel_path, b"cancel");
        thread::sleep(Duration::from_millis(500));
    }
}

/// Delete abandoned staging index directories — `<prefix>*` build dirs left
/// behind by an indexer run that crashed before it could swap or clean up.
/// Best-effort: a failed `read_dir` or `remove_dir_all` is ignored, since a
/// leftover staging dir is harmless and gets overwritten on the next build.
fn remove_staging_index_dirs(state_dir: &Path, prefix: &str) {
    let Ok(entries) = fs::read_dir(state_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_staging = path.is_dir()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix));
        if is_staging {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

fn recover_search_index_dirs(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<(), String> {
    recover_search_index_dirs_for_state(&search_index_dir(app)?)
}

/// Recover a half-finished content-index swap from a prior crash and sweep
/// abandoned content `index-build-*` staging dirs. Skips entirely while a
/// content worker is alive: that worker owns a live `index-build-*` staging
/// dir, and sweeping it would kill the worker's tantivy writer mid-build. In
/// the worker subprocess this guard is inert (the global slot is `None`), so
/// the worker still recovers at its own startup before creating its dir.
fn recover_content_index_dirs_for_state(state_dir: &Path) -> Result<(), String> {
    if worker_process_running(&SEARCH_INDEX_WORKER) {
        return Ok(());
    }
    if !state_dir.exists() {
        return Ok(());
    }
    let active_dir = active_search_index_dir_for_state(state_dir);
    let previous_dir = previous_search_index_dir_for_state(state_dir);
    if !active_dir.exists() && previous_dir.exists() {
        fs::rename(&previous_dir, &active_dir)
            .map_err(|error| format!("Cannot restore previous search index: {error}"))?;
    } else if active_dir.exists() && previous_dir.exists() {
        let _ = fs::remove_dir_all(&previous_dir);
    }
    remove_staging_index_dirs(state_dir, STAGING_SEARCH_INDEX_PREFIX);
    Ok(())
}

/// Filename-index twin of `recover_content_index_dirs_for_state`. Skips
/// entirely while a filename worker is alive: it owns a live
/// `filename-build-*` staging dir, and `ensure_engine_loaded` runs recovery
/// from the main process on every frontend status poll. Filename builds do not
/// set `SEARCH_STATUS.indexing`, so without this guard a poll mid-build would
/// sweep the worker's staging dir and kill its tantivy writer ("index writer
/// was killed"). Inert in the worker subprocess, where the slot is `None`.
fn recover_filename_index_dirs_for_state(state_dir: &Path) -> Result<(), String> {
    if worker_process_running(&FILENAME_INDEX_WORKER) {
        return Ok(());
    }
    if !state_dir.exists() {
        return Ok(());
    }
    let active_dir = active_filename_index_dir_for_state(state_dir);
    let previous_dir = previous_filename_index_dir_for_state(state_dir);
    if !active_dir.exists() && previous_dir.exists() {
        fs::rename(&previous_dir, &active_dir)
            .map_err(|error| format!("Cannot restore previous filename index: {error}"))?;
    } else if active_dir.exists() && previous_dir.exists() {
        let _ = fs::remove_dir_all(&previous_dir);
    }
    remove_staging_index_dirs(state_dir, FILENAME_STAGING_INDEX_PREFIX);
    Ok(())
}

/// Recover both search indexes — used by startup paths (`ensure_engine_loaded`,
/// `enforce_search_schema_version`) that run before any worker is spawned. The
/// per-index workers instead call only their own half so a content build and a
/// filename build can run without clobbering each other's staging dirs.
fn recover_search_index_dirs_for_state(state_dir: &Path) -> Result<(), String> {
    recover_content_index_dirs_for_state(state_dir)?;
    recover_filename_index_dirs_for_state(state_dir)?;
    Ok(())
}

/// Atomically swap a freshly-built staging index into its active slot:
/// active → previous (backup), staging → active, then drop the backup. A
/// failed final rename rolls the backup back. Generic over the directory
/// triple so the content and filename indexes both reuse it.
fn swap_staging_index_dir(
    staging_dir: &Path,
    active_dir: &Path,
    previous_dir: &Path,
) -> Result<(), String> {
    if !staging_dir.exists() {
        return Err("Staging search index was not created.".to_string());
    }

    if let Some(parent) = active_dir.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create search index parent directory: {error}"))?;
    }

    if previous_dir.exists() {
        fs::remove_dir_all(previous_dir)
            .map_err(|error| format!("Cannot remove previous search index backup: {error}"))?;
    }

    if active_dir.exists() {
        fs::rename(active_dir, previous_dir)
            .map_err(|error| format!("Cannot prepare previous search index backup: {error}"))?;
    }

    match fs::rename(staging_dir, active_dir) {
        Ok(()) => {
            if previous_dir.exists() {
                let _ = fs::remove_dir_all(previous_dir);
            }
            Ok(())
        }
        Err(error) => {
            if !active_dir.exists() && previous_dir.exists() {
                let _ = fs::rename(previous_dir, active_dir);
            }
            Err(format!("Cannot activate new search index: {error}"))
        }
    }
}

fn swap_in_staging_search_index_for_state(
    state_dir: &Path,
    staging_dir: &Path,
) -> Result<(), String> {
    swap_staging_index_dir(
        staging_dir,
        &active_search_index_dir_for_state(state_dir),
        &previous_search_index_dir_for_state(state_dir),
    )
}

fn swap_in_staging_filename_index_for_state(
    state_dir: &Path,
    staging_dir: &Path,
) -> Result<(), String> {
    swap_staging_index_dir(
        staging_dir,
        &active_filename_index_dir_for_state(state_dir),
        &previous_filename_index_dir_for_state(state_dir),
    )
}

#[derive(Serialize, Deserialize)]
struct SchemaVersionSentinel {
    version: u32,
}

fn schema_version_path_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(SCHEMA_VERSION_FILE)
}

/// Record the schema version the active index was built with. A missing
/// sentinel is read back as the baseline version, so a failed write only
/// risks one spurious rebuild after a future schema bump.
fn write_schema_version_sentinel(state_dir: &Path) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(&SchemaVersionSentinel {
        version: SEARCH_SCHEMA_VERSION,
    })
    .map_err(|error| format!("Cannot serialize schema version sentinel: {error}"))?;
    fs::write(schema_version_path_for_state(state_dir), bytes)
        .map_err(|error| format!("Cannot write schema version sentinel: {error}"))
}

/// Read the on-disk schema version. `None` when the sentinel is absent or
/// unreadable — callers treat that as the pre-sentinel baseline (version 1).
fn read_schema_version_sentinel(state_dir: &Path) -> Option<u32> {
    let bytes = fs::read(schema_version_path_for_state(state_dir)).ok()?;
    serde_json::from_slice::<SchemaVersionSentinel>(&bytes)
        .ok()
        .map(|sentinel| sentinel.version)
}

/// Startup guard: if either search index was built against an older Tantivy
/// schema, wipe it so the next rebuild recreates it cleanly. Without this, a
/// schema change in an update makes `Index::open` fail or field
/// extraction reject the index — leaving search silently broken. The content
/// and filename indexes version independently, so each is checked separately.
fn enforce_search_schema_version(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<(), String> {
    let state_dir = search_index_dir(app)?;
    if !state_dir.exists() {
        return Ok(());
    }
    // Resolve any half-finished swap from a prior crash first so the version
    // checks run against the genuinely-active indexes.
    recover_search_index_dirs_for_state(&state_dir)?;
    enforce_content_index_schema_version(&state_dir);
    enforce_filename_index_schema_version(&state_dir);
    Ok(())
}

/// Wipe the content index if it was built against an older Tantivy schema.
///
/// A missing sentinel is treated as schema version 1 (the version that
/// predates the sentinel), so existing installs are NOT force-rebuilt when
/// this guard first ships — their index is simply backfilled with a sentinel.
fn enforce_content_index_schema_version(state_dir: &Path) {
    let active_dir = active_search_index_dir_for_state(state_dir);
    if !active_dir.exists() {
        return;
    }

    let on_disk_version = read_schema_version_sentinel(state_dir).unwrap_or(1);
    if on_disk_version == SEARCH_SCHEMA_VERSION {
        // Backfill the sentinel for pre-sentinel installs so a future bump can
        // distinguish them from a genuinely current index.
        let _ = write_schema_version_sentinel(state_dir);
        return;
    }

    // Schema mismatch: the stored index cannot be trusted against the current
    // schema. Drop the active + previous + any staging copies and the stale
    // sentinel. The next scheduled (or manual) rebuild recreates the index,
    // and `open_existing` surfaces the "rebuild the file index once" status.
    let _ = fs::remove_dir_all(&active_dir);
    let _ = fs::remove_dir_all(previous_search_index_dir_for_state(state_dir));
    remove_staging_index_dirs(state_dir, STAGING_SEARCH_INDEX_PREFIX);
    let _ = fs::remove_file(schema_version_path_for_state(state_dir));
}

/// Filename-index twin of `enforce_content_index_schema_version`. Because the
/// two indexes version independently, a filename-schema bump wipes only the
/// filename index and leaves the content index untouched (and vice versa).
fn enforce_filename_index_schema_version(state_dir: &Path) {
    let active_dir = active_filename_index_dir_for_state(state_dir);
    if !active_dir.exists() {
        return;
    }

    let on_disk_version = read_filename_schema_version_sentinel(state_dir).unwrap_or(1);
    if on_disk_version == FILENAME_SCHEMA_VERSION {
        let _ = write_filename_schema_version_sentinel(state_dir);
        return;
    }

    let _ = fs::remove_dir_all(&active_dir);
    let _ = fs::remove_dir_all(previous_filename_index_dir_for_state(state_dir));
    remove_staging_index_dirs(state_dir, FILENAME_STAGING_INDEX_PREFIX);
    let _ = fs::remove_file(filename_schema_version_path_for_state(state_dir));
}

fn write_search_config(app: &std::sync::Arc<dyn dash_core::Host>, config: &StoredSearchConfig) -> Result<(), String> {
    write_search_config_for_state(&search_index_dir(app)?, config)
}

fn read_search_config(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<Option<StoredSearchConfig>, String> {
    read_search_config_for_state(&search_index_dir(app)?)
}

fn write_search_config_for_state(
    state_dir: &Path,
    config: &StoredSearchConfig,
) -> Result<(), String> {
    let db_path = local_db::database_path_for_dir(state_dir);
    local_db::write_json(&db_path, SEARCH_CONFIG_FILE, config)
}

fn read_search_config_for_state(state_dir: &Path) -> Result<Option<StoredSearchConfig>, String> {
    let db_path = local_db::database_path_for_dir(state_dir);
    if let Some(mut parsed) =
        local_db::read_json::<StoredSearchConfig>(&db_path, SEARCH_CONFIG_FILE)?
    {
        parsed.options.exclude_folders = normalize_exclude_folders(&parsed.options.exclude_folders);
        parsed.options.exclude_extensions =
            normalize_exclude_extensions(&parsed.options.exclude_extensions);
        parsed.options.performance_mode =
            normalize_index_performance_mode(&parsed.options.performance_mode);
        if parsed.options.watcher_settings_version == 0 && !parsed.options.watcher_enabled {
            parsed.options.watcher_enabled = true;
            parsed.options.watcher_paused = true;
        }
        if !parsed.options.watcher_enabled {
            parsed.options.watcher_paused = false;
        }
        parsed.options.watcher_settings_version = WATCHER_SETTINGS_VERSION;
        if parsed.options.filename_roots.is_empty() {
            parsed.options.filename_roots = parsed.options.roots.clone();
        }
        parsed.rebuild_schedule = normalize_rebuild_schedule(parsed.rebuild_schedule);
        return Ok(Some(parsed));
    }

    let path = search_config_path_for_state(state_dir);
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(&path).map_err(|error| format!("Cannot read search config: {error}"))?;
    let mut parsed: StoredSearchConfig = serde_json::from_str(&raw)
        .map_err(|error| format!("Cannot parse search config: {error}"))?;
    parsed.options.exclude_folders = normalize_exclude_folders(&parsed.options.exclude_folders);
    parsed.options.exclude_extensions =
        normalize_exclude_extensions(&parsed.options.exclude_extensions);
    parsed.options.performance_mode =
        normalize_index_performance_mode(&parsed.options.performance_mode);
    if parsed.options.watcher_settings_version == 0 && !parsed.options.watcher_enabled {
        parsed.options.watcher_enabled = true;
        parsed.options.watcher_paused = true;
    }
    if !parsed.options.watcher_enabled {
        parsed.options.watcher_paused = false;
    }
    parsed.options.watcher_settings_version = WATCHER_SETTINGS_VERSION;
    if parsed.options.filename_roots.is_empty() {
        parsed.options.filename_roots = parsed.options.roots.clone();
    }
    parsed.rebuild_schedule = normalize_rebuild_schedule(parsed.rebuild_schedule);
    local_db::write_json(&db_path, SEARCH_CONFIG_FILE, &parsed)?;
    Ok(Some(parsed))
}

fn hydrate_status_from_saved_config(app: &std::sync::Arc<dyn dash_core::Host>) {
    let has_live_engine = SEARCH_ENGINE
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false);
    if has_live_engine {
        return;
    }

    let Ok(Some(config)) = read_search_config(app) else {
        return;
    };

    update_status(|status| {
        if status.indexing || status.initialized {
            return;
        }
        status.roots = config.options.roots.clone();
        status.filename_roots = config.options.filename_roots.clone();
        status.include_hidden = config.options.include_hidden;
        status.index_content = config.options.index_content;
        status.max_content_kb = config
            .options
            .max_content_kb
            .unwrap_or(DEFAULT_MAX_CONTENT_KB);
        status.commit_every = normalize_commit_every(config.options.commit_every);
        status.watcher_enabled = config.options.watcher_enabled;
        status.watcher_paused = config.options.watcher_paused;
        status.exclude_folders = config.options.exclude_folders.clone();
        status.exclude_extensions = config.options.exclude_extensions.clone();
        status.rebuild_schedule = config.rebuild_schedule.clone();
        status.indexed_files = config.indexed_files;
        status.last_indexed_at_ms = config.last_indexed_at_ms;
        status.watching = false;
        status.last_error = None;
        status.diagnostics.performance_mode =
            normalize_index_performance_mode(&config.options.performance_mode);
    });
}

fn hydrate_status_from_index_worker_files(app: &std::sync::Arc<dyn dash_core::Host>) {
    let Ok(state_dir) = search_index_dir(app) else {
        return;
    };
    let status_path = state_dir.join(INDEX_WORKER_STATUS_FILE);
    let Ok(Some(status_file)) = read_index_worker_status(&status_path) else {
        return;
    };
    if !status_file.running {
        return;
    }
    // A worker that was killed (or crashed) mid-build never writes
    // "finished": with no live worker, "running" is stale, and believing it
    // would show "Indexing" forever.
    if !worker_process_running(&SEARCH_INDEX_WORKER) {
        update_status(|status| status.indexing = false);
        return;
    }

    let options_path = state_dir.join(INDEX_WORKER_OPTIONS_FILE);
    let options_from_worker = fs::read(&options_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<FileSearchIndexOptions>(&bytes).ok())
        .and_then(|options| normalize_index_options(options).ok());
    let options = options_from_worker.or_else(|| {
        read_search_config_for_state(&state_dir)
            .ok()
            .flatten()
            .map(|config| config.options)
    });
    let Some(options) = options else {
        return;
    };

    apply_index_worker_status(app, &options, &status_file);
}

fn hydrate_status_from_watcher_worker_files(app: &std::sync::Arc<dyn dash_core::Host>) {
    let Ok(state_dir) = search_index_dir(app) else {
        return;
    };
    let status_path = state_dir.join(WATCHER_WORKER_STATUS_FILE);
    let Ok(Some(status_file)) = read_index_worker_status(&status_path) else {
        return;
    };
    if !status_file.running || status_file.finished {
        return;
    }

    let options_path = state_dir.join(WATCHER_WORKER_OPTIONS_FILE);
    let options_from_worker = fs::read(&options_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<FileSearchIndexOptions>(&bytes).ok())
        .and_then(|options| normalize_index_options(options).ok());
    let options = options_from_worker.or_else(|| {
        read_search_config_for_state(&state_dir)
            .ok()
            .flatten()
            .map(|config| config.options)
    });
    let Some(options) = options else {
        return;
    };
    if !options.watcher_enabled {
        return;
    }

    apply_watch_worker_status(app, &options, &status_file);
}

/// Surface the standalone filename indexer's latest message into
/// `FileSearchStatus` so the UI can show whether MFT or the walker built the
/// filename index — that worker emits no progress events, so without this it
/// is entirely invisible to the user.
fn hydrate_status_from_filename_worker_files(app: &std::sync::Arc<dyn dash_core::Host>) {
    let Ok(state_dir) = search_index_dir(app) else {
        return;
    };
    // Filename index size + last-built time — surfaced so the File Search tab
    // can show a count and timestamp like the content tab. While a build is in
    // flight the worker monitor owns `filename_indexed_files` (it streams the
    // live build count) and the index is mid-swap, so skip this then. Otherwise
    // the count is the index's own `num_docs()` (accurate for the MFT and
    // walker engines alike, and it tracks live tailer/watcher edits) and the
    // time is the schema sentinel's mtime.
    let building = SEARCH_STATUS
        .lock()
        .map(|status| status.filename_indexing)
        .unwrap_or(false);
    if !building {
        ensure_filename_engine_loaded(app);
        let indexed = filename_index_num_docs();
        let built_at = filename_index_last_built_ms(&state_dir);
        update_status(|status| {
            status.filename_indexed_files = indexed;
            status.filename_last_indexed_at_ms = built_at;
        });
    }
    // Latest free-text message from the standalone filename worker, if it has
    // run this session — the secondary "live status" line under the count.
    let status_path = state_dir.join(FILENAME_WORKER_STATUS_FILE);
    if let Ok(Some(status_file)) = read_index_worker_status(&status_path) {
        update_status(|status| {
            status.filename_index_message = Some(status_file.message.clone());
        });
    }
}

fn should_include_path(path: &Path, options: &FileSearchIndexOptions) -> bool {
    // Wave 6 (2026-05-28): hard-block on the four Windows system trees
    // (Windows / Program Files / Program Files (x86) / ProgramData). These
    // contain zero user-authored content and were previously eating
    // ~200k+ index entries per drive for no user benefit. Applied here so
    // `WalkDir::filter_entry` skips descending into the entire subtree.
    if is_system_protected_path(path) {
        return false;
    }
    if !options.include_hidden && is_hidden(path) {
        return false;
    }
    if has_excluded_folder(path, &options.exclude_folders) {
        return false;
    }
    if path.is_file() && has_excluded_extension(path, &options.exclude_extensions) {
        return false;
    }
    true
}

fn is_hidden(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .map(|value| value.starts_with('.'))
            .unwrap_or(false)
    })
}

fn normalize_exclude_folders(values: &[String]) -> Vec<String> {
    let mut normalized = normalize_unique_tokens(values, false);
    let mut seen: HashSet<String> = normalized.iter().cloned().collect();
    for token in required_exclude_folders() {
        if seen.insert(token.clone()) {
            normalized.push(token);
        }
    }
    normalized
}

fn normalize_exclude_extensions(values: &[String]) -> Vec<String> {
    normalize_unique_tokens(values, true)
}

fn normalize_unique_tokens(values: &[String], trim_dot_prefix: bool) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for value in values {
        let mut token = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_lowercase();
        token = token.replace('\\', "/");
        if trim_dot_prefix {
            token = token.trim_start_matches('.').to_string();
        }
        token = token.trim_matches('/').trim().to_string();
        if token.is_empty() || seen.contains(&token) {
            continue;
        }
        seen.insert(token.clone());
        normalized.push(token);
    }
    normalized
}

fn has_excluded_folder(path: &Path, exclude_folders: &[String]) -> bool {
    if exclude_folders.is_empty() {
        return false;
    }

    let normalized_path = normalize_path_for_exclusion(path);
    if exclude_folders.iter().any(|excluded| {
        is_path_like_exclusion(excluded)
            && exclusion_pattern_matches_path(&normalized_path, excluded)
    }) {
        return true;
    }

    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .map(|value| {
                let normalized = value.to_lowercase();
                exclude_folders
                    .iter()
                    .any(|excluded| excluded == &normalized)
            })
            .unwrap_or(false)
    })
}

fn normalize_path_for_exclusion(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_lowercase()
        .trim_end_matches('/')
        .to_string()
}

fn is_path_like_exclusion(excluded: &str) -> bool {
    excluded.contains('/') || excluded.contains(':') || excluded.contains('*')
}

fn exclusion_pattern_matches_path(path: &str, excluded: &str) -> bool {
    if excluded.contains('*') {
        let mut remainder = path;
        for part in excluded.split('*').filter(|part| !part.is_empty()) {
            let Some(index) = remainder.find(part) else {
                return false;
            };
            remainder = &remainder[index + part.len()..];
        }
        return true;
    }

    path == excluded
        || path.starts_with(&format!("{excluded}/"))
        || path.contains(&format!("/{excluded}/"))
        || path.ends_with(&format!("/{excluded}"))
}

fn has_excluded_extension(path: &Path, exclude_extensions: &[String]) -> bool {
    if exclude_extensions.is_empty() {
        return false;
    }

    let token = path_extension_or_dotfile_token(path);
    !token.is_empty() && exclude_extensions.iter().any(|excluded| excluded == &token)
}

fn path_extension_or_dotfile_token(path: &Path) -> String {
    if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
        return extension.trim_start_matches('.').to_lowercase();
    }

    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return String::new();
    };
    let normalized = file_name.to_lowercase();
    if normalized.starts_with('.') && normalized.matches('.').count() == 1 {
        return normalized.trim_start_matches('.').to_string();
    }

    String::new()
}

/// Read indexable text from a file. Dispatches to format-aware extractors
/// (PDF, DOCX, XLSX, PPTX) in addition to plain text — see the
/// `commands::text_extract` module for per-format details.
fn read_text_for_index(path: &Path, max_bytes: usize) -> Option<String> {
    super::text_extract::extract_text_for_indexing(path, max_bytes)
}

/// Extract up to `max_bytes` of readable text from a file for the command
/// palette's preview pane. Reuses the content-index extractor, so it returns
/// text for plain-text / code / Markdown / `.ki` AND rich formats (PDF, DOCX,
/// XLSX, …). Returns `None` for folders, missing files, unsupported types, or
/// binaries — the UI then shows metadata only. Local + read-only; the asset
/// protocol handles images/audio/video directly, so those never reach here.
/// Rich formats are parsed in the sandboxed extractor child (see
/// `extract_text_sandboxed`).
pub fn read_file_preview(path: String, max_bytes: Option<usize>) -> Option<String> {
    let cap = max_bytes.unwrap_or(64 * 1024).clamp(1024, 512 * 1024);
    let p = Path::new(&path);
    if !p.is_file() {
        return None;
    }
    extract_text_sandboxed(p, cap)
}

/// How long a sandboxed preview extraction may take before its child is
/// killed. Shorter than the index build's per-file timeout: someone is
/// waiting on it, and a file that slow gets no preview.
const PREVIEW_EXTRACT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// The preview child extracts up to this much (the largest preview cap;
/// result snippets scan far less); callers trim to their own cap.
const PREVIEW_CHILD_MAX_BYTES: usize = 512 * 1024;

/// The extractor child previews use: started on first use, replaced after it
/// dies or times out. Dropping it closes the job, which kills the child.
struct PreviewExtractor {
    child: WatchedChild,
    _job: MemoryCappedJob,
}

static PREVIEW_EXTRACTOR: LazyLock<Mutex<Option<PreviewExtractor>>> = LazyLock::new(|| Mutex::new(None));

fn spawn_preview_extractor() -> Result<PreviewExtractor, String> {
    let job = MemoryCappedJob::new(EXTRACTOR_CHILD_MEMORY_CAP_BYTES)?;
    // No OCR settings: OCR stays off for previews. "fast" = normal priority,
    // since someone is waiting.
    let (child, stdin, stdout) = spawn_assigned_extractor_child(
        PREVIEW_CHILD_MAX_BYTES,
        "fast",
        job.assigner(),
        &super::text_extract::OcrChildEnv::default(),
    )?;
    Ok(PreviewExtractor { child: WatchedChild::new(child, stdin, stdout), _job: job })
}

/// Up to `max_bytes` of `path`'s text without parsing untrusted files in this
/// process: plain text is read here (bytes only, no parser); documents go to
/// the memory-capped extractor child with a timeout, so a parser crash, memory
/// bomb or endless loop costs that child, not the app.
fn extract_text_sandboxed(path: &Path, max_bytes: usize) -> Option<String> {
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if super::text_extract::is_plain_text_extension(&ext) {
        return super::text_extract::extract_text_for_indexing(path, max_bytes);
    }
    if !super::text_extract::is_text_or_doc_extension(&ext) {
        return None;
    }
    let mut slot = PREVIEW_EXTRACTOR.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_none() {
        *slot = spawn_preview_extractor().ok();
    }
    let extractor = slot.as_mut()?;
    match extractor.child.extract(path, PREVIEW_EXTRACT_TIMEOUT) {
        ExtractAnswer::Done(_, text) => {
            let mut text = text?;
            text.truncate(nearest_char_boundary_before(&text, max_bytes));
            Some(text)
        }
        ExtractAnswer::Died | ExtractAnswer::TimedOut => {
            *slot = None;
            None
        }
    }
}

/// Roots of every accessible logical drive (e.g. "C:\\", "D:\\"). Used by
/// first-run indexing to seed the FILENAME index with every drive the user
/// has. Dependency-free: probes drive letters A–Z and keeps the ones whose
/// root is a readable directory (so an empty optical/card reader is skipped).
pub fn list_logical_drives() -> Vec<String> {
    #[cfg(windows)]
    {
        let mut out = Vec::new();
        for letter in b'A'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            if Path::new(&root).is_dir() {
                out.push(root);
            }
        }
        out
    }
    #[cfg(not(windows))]
    {
        vec!["/".to_string()]
    }
}

/// Wave 8.1 (2026-05-28): walker-side content-index gate. Returns true when
/// the path should be sent through the content extractor pool — that is, the
/// extension is either a definite text / document format, OR it's an image
/// AND OCR-on-Index is enabled AND the path lives under one of the user's
/// configured OCR folders. The image case is gated HERE in the walker (not
/// later in the extractor child) so opt-out images never even cost an IPC
/// round-trip.
///
/// Folder matching mirrors `OcrIndexConfig::applies_to` in `text_extract.rs`:
/// case-insensitive prefix, with trailing separator tolerated.
fn should_index_for_content(path: &Path, options: &FileSearchIndexOptions) -> bool {
    let ext = path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if super::text_extract::is_text_or_doc_extension(&ext) {
        return true;
    }
    if options.ocr_on_index_enabled
        && super::text_extract::is_ocr_image_extension(&ext)
        && ocr_path_in_scope(path, &options.ocr_on_index_folders)
    {
        return true;
    }
    false
}

/// Case-insensitive prefix-match a path against the configured OCR folders.
/// Mirrors `OcrIndexConfig::applies_to` in `text_extract.rs` — kept in sync;
/// if you change one, change the other.
///
/// Wave 8.4.1 (2026-05-29): normalize ALL separators to forward slashes
/// before comparison. Tauri's folder dialog typically returns paths with
/// forward slashes (`C:/OCRTest`), but the walker yields OS-native paths
/// (`C:\OCRTest\img.png` on Windows). Without normalization, a folder
/// picked via the dialog never matched walker-yielded paths and images
/// in scope were silently skipped.
fn ocr_path_in_scope(path: &Path, folders: &[String]) -> bool {
    if folders.is_empty() {
        return false;
    }
    let path_str = path
        .to_string_lossy()
        .to_lowercase()
        .replace('\\', "/");
    folders.iter().any(|root| {
        let root_str = root
            .to_lowercase()
            .replace('\\', "/");
        let root_str = root_str.trim_end_matches('/');
        path_str == root_str || path_str.starts_with(&format!("{}/", root_str))
    })
}

/// Apply an explicit `operator:value` query operator to the plan being built.
/// `ext:` / `extension:` / `filetype:` take a literal (comma-separated)
/// extension list; `type:` / `kind:` take a category word, a folder/file
/// intent, or a literal extension. Returns `true` when the token was a
/// recognised operator and has been fully consumed.
fn apply_field_operator(
    operator: &str,
    raw_value: &str,
    extension_filters: &mut Vec<String>,
    entry_type_filter: &mut Option<String>,
) -> bool {
    if raw_value.trim().is_empty() {
        return false;
    }
    match operator {
        "ext" | "extension" | "filetype" => {
            for part in raw_value.split(',') {
                let part = part.trim().trim_start_matches('.');
                if part.is_empty() {
                    continue;
                }
                for ext in normalize_extension_token(part) {
                    push_unique(extension_filters, ext);
                }
            }
            true
        }
        "type" | "kind" => {
            let value = raw_value.trim();
            if is_folder_intent_token(value) {
                *entry_type_filter = Some(ENTRY_TYPE_FOLDER.to_string());
            } else if is_file_intent_token(value) {
                *entry_type_filter = Some(ENTRY_TYPE_FILE.to_string());
            } else if let Some(category) = token_category_extensions(value) {
                for ext in category {
                    push_unique(extension_filters, ext);
                }
            } else {
                for ext in normalize_extension_token(value) {
                    push_unique(extension_filters, ext);
                }
            }
            true
        }
        _ => false,
    }
}

fn build_natural_query_plan(input: &str) -> NaturalQueryPlan {
    let (input_without_phrases, exact_phrases) = extract_quoted_phrases(input);
    let tokens = tokenize_search_input(&input_without_phrases);
    let mut query_words = Vec::new();
    let mut inferred_extension_filters = Vec::new();
    let mut explicit_extension_filters = Vec::new();
    let mut entry_type_filter: Option<String> = None;
    let mut date_filter: Option<DateFilter> = None;
    let mut size_filter: Option<SizeFilter> = None;
    let mut date_field_hint = DateFieldIntent::Modified;
    let mut previous_token: Option<String> = None;
    let mut index = 0usize;

    while index < tokens.len() {
        let bare = tokens[index].trim_start_matches('.').to_string();
        let had_dot_prefix = tokens[index].starts_with('.');
        if bare.is_empty() {
            index += 1;
            continue;
        }

        // Explicit field operators — `ext:docx`, `type:image`, `kind:folder` —
        // resolved before the keyword/filter heuristics so an operator wins.
        if let Some((operator, raw_value)) = bare.split_once(':') {
            if apply_field_operator(
                operator,
                raw_value,
                &mut explicit_extension_filters,
                &mut entry_type_filter,
            ) {
                previous_token = Some(bare.clone());
                index += 1;
                continue;
            }
        }
        // A colon / comma that was not part of a recognised operator is dropped
        // so the token still matches as an ordinary keyword (`build:debug` →
        // `builddebug`) — `,` only survives tokenizing for operator value lists.
        let bare = if bare.contains(':') || bare.contains(',') {
            bare.replace(':', "").replace(',', "")
        } else {
            bare
        };
        if bare.is_empty() {
            index += 1;
            continue;
        }

        if let Some(field) = date_field_intent(&bare) {
            date_field_hint = field;
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if is_folder_intent_token(&bare) {
            entry_type_filter = Some(ENTRY_TYPE_FOLDER.to_string());
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if is_file_intent_token(&bare) {
            entry_type_filter = Some(ENTRY_TYPE_FILE.to_string());
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if bare == "empty" {
            size_filter = Some(SizeFilter::Empty);
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if let Some((filter, consumed)) = parse_size_filter(&tokens, index) {
            size_filter = Some(filter);
            previous_token = Some(tokens[index].clone());
            index += consumed;
            continue;
        }

        if let Some((filter, consumed)) = parse_date_filter(&tokens, index, date_field_hint) {
            date_filter = Some(filter);
            previous_token = Some(tokens[index].clone());
            index += consumed;
            continue;
        }

        if let Some(year) = parse_year_token(&bare) {
            if previous_token
                .as_deref()
                .map(is_year_filter_hint)
                .unwrap_or(false)
            {
                date_filter = exact_year_filter(date_field_hint, year);
            } else {
                push_expanded_query_word(&mut query_words, bare.clone());
            }
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if let Some(exts) = token_category_extensions(&bare) {
            for ext in exts {
                push_unique(&mut inferred_extension_filters, ext);
            }
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if had_dot_prefix || is_common_extension_token(&bare) {
            for ext in normalize_extension_token(&bare) {
                push_unique(&mut explicit_extension_filters, ext);
            }
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if is_natural_stopword(&bare) {
            // A connector like "at"/"on" between a date word and the year
            // ("created at 2025", "modified on 2025") must NOT break the
            // year-hint chain — keep the preceding hint so the year binds to it.
            let bridges_year_hint = matches!(bare.as_str(), "at" | "on")
                && previous_token
                    .as_deref()
                    .map(is_year_filter_hint)
                    .unwrap_or(false);
            if !bridges_year_hint {
                previous_token = Some(bare);
            }
            index += 1;
            continue;
        }
        push_expanded_query_word(&mut query_words, bare.clone());
        previous_token = Some(bare);
        index += 1;
    }

    for phrase in &exact_phrases {
        for token in tokenize_search_input(phrase) {
            let bare = token.trim_start_matches('.').replace(':', "").replace(',', "");
            if bare.is_empty() || is_natural_stopword(&bare) {
                continue;
            }
            push_expanded_query_word(&mut query_words, bare);
        }
    }

    let extension_filters = if explicit_extension_filters.is_empty() {
        inferred_extension_filters
    } else {
        explicit_extension_filters
    };

    NaturalQueryPlan {
        query_text: query_words.join(" "),
        query_keywords: query_words,
        extension_filters,
        entry_type_filter,
        date_filter,
        size_filter,
        exact_phrases,
    }
}

fn should_run_aggressive_file_fallback(
    natural_plan: &NaturalQueryPlan,
    query_keywords: &[String],
    extension_filters: &[String],
    path_filter: Option<&str>,
) -> bool {
    if path_filter.is_some()
        || natural_plan.entry_type_filter.is_some()
        || natural_plan.size_filter.is_some()
        || !extension_filters.is_empty()
        || !natural_plan.exact_phrases.is_empty()
    {
        return true;
    }

    // Only widen the net when the entire query is a single short keyword.
    // A short word inside a multi-keyword query is meaningful — don't over-fetch.
    query_keywords.len() == 1
        && query_keywords
            .first()
            .map(|k| k.len() >= 3 && k.len() <= 5)
            .unwrap_or(false)
}

fn extract_quoted_phrases(input: &str) -> (String, Vec<String>) {
    let mut remaining = String::with_capacity(input.len());
    let mut phrases = Vec::new();
    let mut current_phrase = String::new();
    let mut in_quote = false;

    for ch in input.chars() {
        if ch == '"' || ch == '\'' {
            if in_quote {
                let phrase = current_phrase.trim();
                if !phrase.is_empty() {
                    phrases.push(phrase.to_string());
                }
                current_phrase.clear();
                in_quote = false;
            } else {
                in_quote = true;
            }
            remaining.push(' ');
            continue;
        }

        if in_quote {
            current_phrase.push(ch);
        } else {
            remaining.push(ch);
        }
    }

    if in_quote && !current_phrase.trim().is_empty() {
        remaining.push(' ');
        remaining.push_str(&current_phrase);
    }

    (remaining, phrases)
}

fn tokenize_search_input(input: &str) -> Vec<String> {
    input
        .split_whitespace()
        .filter_map(|token| {
            let cleaned = token
                .chars()
                .filter(|ch| {
                    ch.is_alphanumeric()
                        || *ch == '.'
                        || *ch == '_'
                        || *ch == '-'
                        || *ch == ':'
                        || *ch == ','
                })
                .collect::<String>()
                .to_lowercase();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            }
        })
        .collect()
}

fn push_expanded_query_word(query_words: &mut Vec<String>, word: String) {
    push_unique(query_words, word.clone());
    for related in related_natural_terms(&word) {
        push_unique(query_words, related.to_string());
    }
}

fn related_natural_terms(word: &str) -> &'static [&'static str] {
    match word {
        // Visual / animation
        "animation" | "animations" | "animated" => &["anime", "cartoon", "cartoons"],
        "anime" => &["animation", "animated", "cartoon", "cartoons"],
        "cartoon" | "cartoons" => &["animation", "animated", "anime"],
        "screenshot" | "screenshots" => &["screenshot", "screenshots", "capture", "screen"],
        "capture" | "captures" => &["screenshot", "screenshots", "capture"],

        // HR / career
        "resume" | "cv" | "curriculum" => &["resume", "cv", "curriculum", "vitae"],
        "vitae" => &["resume", "cv", "curriculum"],
        "portfolio" | "portfolios" | "showcase" => &["portfolio", "portfolios", "showcase", "gallery"],

        // Finance / billing
        "invoice" | "invoices" => &["invoice", "invoices", "bill", "receipt", "billing"],
        "bill" | "bills" | "receipt" | "receipts" | "billing" => &["invoice", "invoices", "bill", "receipt"],
        "budget" | "budgets" => &["budget", "budgets", "finance", "financial", "spending", "expenditure"],
        "finance" | "financial" | "spending" | "expenditure" => &["budget", "budgets", "finance", "financial", "spending"],
        "expense" | "expenses" | "cost" | "costs" => &["expense", "expenses", "cost", "bill", "receipt"],
        "payment" | "payments" | "transaction" | "transactions" => &["payment", "payments", "transaction", "transfer"],
        "transfer" | "transfers" => &["payment", "transfer", "transaction"],
        "quote" | "quotes" | "quotation" | "quotations" | "estimate" | "estimates" => &["quote", "quotation", "estimate", "proposal"],
        "order" | "orders" | "purchase" | "purchases" => &["order", "orders", "purchase", "purchases", "transaction"],
        "salary" | "salaries" | "payroll" | "paycheck" | "paychecks" | "wage" | "wages" => &["salary", "salaries", "payroll", "paycheck", "wages"],
        "tax" | "taxes" | "taxation" => &["tax", "taxes", "taxable", "filing"],
        "filing" | "filings" => &["filing", "tax", "taxes", "form", "forms"],
        "insurance" | "policy" | "policies" | "coverage" => &["insurance", "policy", "policies", "coverage", "claim"],
        "claim" | "claims" => &["claim", "claims", "insurance", "policy"],
        "statement" | "statements" => &["statement", "statements", "balance", "bank"],
        "bank" | "banking" => &["bank", "banking", "statement", "account"],

        // Legal / formal
        "contract" | "contracts" => &["contract", "contracts", "agreement", "agreements", "nda"],
        "agreement" | "agreements" | "nda" => &["contract", "agreement", "agreements", "nda"],
        "license" | "licence" | "licenses" | "licences" | "permit" | "permits" => &["license", "licence", "permit", "certificate"],
        "certificate" | "certificates" | "certification" | "cert" | "certs" | "credential" | "credentials" => {
            &["certificate", "cert", "credential", "diploma", "license"]
        }
        "diploma" | "diplomas" | "degree" | "degrees" => &["diploma", "degree", "certificate", "cert"],
        "legal" | "lawsuit" | "litigation" => &["legal", "contract", "agreement", "filing"],

        // Documents / writing
        "report" | "reports" => &["report", "reports", "summary", "summaries", "analysis"],
        "summary" | "summaries" | "analysis" | "analyses" => &["report", "summary", "summaries", "analysis", "overview"],
        "overview" => &["summary", "overview", "report"],
        "note" | "notes" => &["note", "notes", "memo", "memos", "jot"],
        "memo" | "memos" | "jot" | "jots" => &["note", "notes", "memo", "memos"],
        "draft" | "drafts" | "wip" => &["draft", "drafts", "wip", "revision", "revisions"],
        "revision" | "revisions" => &["draft", "drafts", "revision", "revisions", "version"],
        "template" | "templates" | "boilerplate" => &["template", "templates", "boilerplate", "skeleton"],
        "proposal" | "proposals" | "pitch" | "pitches" => &["proposal", "proposals", "pitch", "pitches", "rfp"],
        "rfp" => &["rfp", "proposal", "proposals"],
        "manual" | "manuals" | "handbook" | "handbooks" => &["manual", "manuals", "guide", "guides", "handbook", "instructions"],
        "guide" | "guides" | "instructions" | "howto" => &["guide", "guides", "manual", "manuals", "tutorial", "instructions"],
        "tutorial" | "tutorials" => &["tutorial", "tutorials", "guide", "guides", "example", "howto"],
        "example" | "examples" | "sample" | "samples" | "demo" | "demos" => &["example", "examples", "sample", "samples", "demo"],
        "readme" | "documentation" | "docs" | "doc" => &["readme", "documentation", "docs", "guide", "manual"],
        "changelog" | "history" | "changes" => &["changelog", "history", "release", "releases", "version"],
        "release" | "releases" | "version" | "versions" => &["release", "releases", "changelog", "version", "patch"],

        // Planning / project
        "plan" | "plans" | "planning" => &["plan", "plans", "schedule", "roadmap", "strategy"],
        "roadmap" | "roadmaps" | "strategy" | "strategies" => &["roadmap", "plan", "plans", "strategy", "milestone"],
        "milestone" | "milestones" => &["milestone", "milestones", "roadmap", "plan"],
        "schedule" | "schedules" | "calendar" => &["schedule", "schedules", "calendar", "agenda", "plan"],
        "agenda" => &["agenda", "schedule", "meeting", "minutes"],
        "minutes" => &["minutes", "agenda", "meeting", "notes"],
        "meeting" | "meetings" | "appointment" | "appointments" => &["meeting", "meetings", "appointment", "agenda", "minutes"],
        "checklist" | "checklists" | "todo" | "todos" | "task" | "tasks" => &["checklist", "todo", "todos", "tasks"],

        // Development / technical
        "patch" | "patches" | "hotfix" | "hotfixes" => &["patch", "patches", "fix", "hotfix", "update"],
        "fix" | "fixes" | "bugfix" | "bugfixes" => &["fix", "fixes", "patch", "patches", "bugfix"],
        "test" | "tests" | "testing" => &["test", "tests", "spec", "specs"],
        "spec" | "specs" | "specification" | "specifications" => &["spec", "specs", "test", "tests", "specification"],
        "error" | "errors" | "exception" | "exceptions" => &["error", "errors", "exception", "crash", "log"],
        "crash" | "crashes" | "dump" | "dumps" => &["crash", "crashes", "error", "dump", "log"],
        "log" | "logs" => &["log", "logs", "trace", "traces"],
        "trace" | "traces" => &["trace", "traces", "log", "logs", "debug"],
        "debug" | "debugging" => &["debug", "trace", "log", "error"],
        "config" | "configuration" | "settings" | "preferences" => &["config", "configuration", "settings", "conf", "preferences"],
        "conf" | "env" => &["conf", "config", "configuration", "settings"],
        "backup" | "backups" | "bak" => &["backup", "backups", "bak", "archive"],
        "script" | "scripts" | "automation" => &["script", "scripts", "automation", "macro"],
        "macro" | "macros" => &["macro", "macros", "script", "scripts", "automation"],

        // Messaging / communication
        "message" | "messages" | "chat" | "chats" | "conversation" | "conversations" => {
            &["message", "messages", "chat", "conversation", "thread"]
        }
        "thread" | "threads" => &["thread", "threads", "message", "conversation"],
        "email" | "emails" | "mail" | "mails" => &["email", "emails", "mail", "message", "correspondence"],
        "correspondence" => &["correspondence", "email", "letter", "letters"],
        "letter" | "letters" => &["letter", "letters", "correspondence", "email"],

        // Diagrams / visuals
        "diagram" | "diagrams" | "flowchart" | "flowcharts" | "wireframe" | "wireframes" => {
            &["diagram", "diagrams", "flowchart", "chart", "wireframe"]
        }
        "chart" | "charts" => &["chart", "charts", "graph", "graphs", "diagram"],
        "graph" | "graphs" => &["graph", "graphs", "chart", "charts", "plot"],
        "plot" | "plots" => &["plot", "plots", "graph", "chart"],

        // Presentation / slides (keyword synonym even if also mapped as extension category)
        "presentation" | "presentations" | "slideshow" => &["presentation", "presentations", "slides", "slideshow"],
        "slides" => &["slides", "slideshow", "presentation", "presentations"],

        // Health / personal
        "medical" | "health" | "healthcare" => &["medical", "health", "healthcare", "record", "records"],
        "record" | "records" => &["record", "records", "medical", "history"],
        "prescription" | "prescriptions" | "medication" | "medications" => &["prescription", "medication", "medical", "health"],

        _ => &[],
    }
}

fn date_field_intent(token: &str) -> Option<DateFieldIntent> {
    match token {
        "created" | "creation" | "made" | "createdat" | "createdon" => {
            Some(DateFieldIntent::Created)
        }
        "modified" | "updated" | "changed" | "edited" | "modifiedat" | "modifiedon"
        | "updatedat" => Some(DateFieldIntent::Modified),
        _ => None,
    }
}

fn is_folder_intent_token(token: &str) -> bool {
    matches!(
        token,
        "folder" | "folders" | "directory" | "directories" | "dir" | "dirs"
    )
}

fn is_file_intent_token(token: &str) -> bool {
    matches!(token, "file" | "files")
}

fn parse_date_filter(
    tokens: &[String],
    index: usize,
    field: DateFieldIntent,
) -> Option<(DateFilter, usize)> {
    let token = tokens.get(index)?.as_str();

    match token {
        "today" => {
            let now = current_unix_ms();
            Some((
                DateFilter {
                    field,
                    start_ms: Some(now.saturating_sub(24 * 60 * 60 * 1000)),
                    end_ms: Some(now),
                    label: format!("{} today", date_field_label(field)),
                },
                1,
            ))
        }
        "yesterday" => {
            let now = current_unix_ms();
            Some((
                DateFilter {
                    field,
                    start_ms: Some(now.saturating_sub(48 * 60 * 60 * 1000)),
                    end_ms: Some(now.saturating_sub(24 * 60 * 60 * 1000)),
                    label: format!("{} yesterday", date_field_label(field)),
                },
                1,
            ))
        }
        "recent" | "recently" => {
            let now = current_unix_ms();
            Some((
                DateFilter {
                    field,
                    start_ms: Some(now.saturating_sub(30 * 24 * 60 * 60 * 1000)),
                    end_ms: Some(now),
                    label: format!("recently {}", date_field_label(field).to_lowercase()),
                },
                1,
            ))
        }
        "this" | "last" | "past" => {
            let next = tokens.get(index + 1).map(|value| value.as_str())?;
            // Numeric window — "last 7 days", "past 3 months".
            if let Ok(count) = next.parse::<u64>() {
                let unit = tokens.get(index + 2)?;
                let span_days = count.saturating_mul(period_to_days(unit)?).max(1);
                return Some((
                    rolling_days_filter(
                        field,
                        span_days,
                        format!("{} in the {token} {count} {unit}", date_field_label(field)),
                    ),
                    3,
                ));
            }
            // Word window — "this week", "last month", "past year".
            let span_days = period_to_days(next)?;
            Some((
                rolling_days_filter(
                    field,
                    span_days,
                    format!("{} {token} {next}", date_field_label(field)),
                ),
                2,
            ))
        }
        "between" => {
            let first_year = tokens
                .get(index + 1)
                .and_then(|value| parse_year_token(value))?;
            let second_index = if tokens.get(index + 2).map(|value| value.as_str()) == Some("and") {
                index + 3
            } else {
                index + 2
            };
            let second_year = tokens
                .get(second_index)
                .and_then(|value| parse_year_token(value))?;
            let start_year = first_year.min(second_year);
            let end_year = first_year.max(second_year);
            Some((
                year_range_filter(field, start_year, end_year)?,
                second_index.saturating_sub(index) + 1,
            ))
        }
        "before" => {
            let year = tokens
                .get(index + 1)
                .and_then(|value| parse_year_token(value))?;
            Some((before_year_filter(field, year)?, 2))
        }
        "after" => {
            let year = tokens
                .get(index + 1)
                .and_then(|value| parse_year_token(value))?;
            Some((after_year_filter(field, year)?, 2))
        }
        "since" => {
            let year = tokens
                .get(index + 1)
                .and_then(|value| parse_year_token(value))?;
            Some((since_year_filter(field, year)?, 2))
        }
        // `from`/`in` are intentionally NOT here — they read as the release
        // year in the *name* ("movies from 2025" == "movies 2025"), not a date.
        // Date filtering by year is explicit: `during`/`year`, or `created`/
        // `modified` + a year.
        "during" | "year" => {
            let year = tokens
                .get(index + 1)
                .and_then(|value| parse_year_token(value))?;
            Some((exact_year_filter(field, year)?, 2))
        }
        _ => None,
    }
}

/// Days in a single relative period word — `day`, `week`, `month`, `year`
/// (singular or plural). The windows are rolling approximations, matching the
/// existing `recent` / `this week` behaviour rather than calendar boundaries.
fn period_to_days(word: &str) -> Option<u64> {
    match word {
        "day" | "days" => Some(1),
        "week" | "weeks" => Some(7),
        "month" | "months" => Some(31),
        "year" | "years" => Some(365),
        _ => None,
    }
}

/// A rolling `[now - days, now]` date filter — the shared shape behind
/// `today`, `recently`, `this week`, `last 30 days`, and similar phrases.
fn rolling_days_filter(field: DateFieldIntent, days: u64, label: String) -> DateFilter {
    let now = current_unix_ms();
    let span_ms = days.saturating_mul(24 * 60 * 60 * 1000);
    DateFilter {
        field,
        start_ms: Some(now.saturating_sub(span_ms)),
        end_ms: Some(now),
        label,
    }
}

fn parse_size_filter(tokens: &[String], index: usize) -> Option<(SizeFilter, usize)> {
    let token = tokens.get(index)?.as_str();
    match token {
        "large" | "big" => Some((
            SizeFilter::Range {
                min_bytes: Some(100 * 1024 * 1024),
                max_bytes: None,
                label: "large files".to_string(),
            },
            1,
        )),
        "huge" | "massive" => Some((
            SizeFilter::Range {
                min_bytes: Some(1024 * 1024 * 1024),
                max_bytes: None,
                label: "huge files".to_string(),
            },
            1,
        )),
        "small" | "tiny" => Some((
            SizeFilter::Range {
                min_bytes: None,
                max_bytes: Some(10 * 1024 * 1024),
                label: "small files".to_string(),
            },
            1,
        )),
        "bigger" | "larger" | "over" | "above" => {
            parse_size_after(tokens, index + 1).map(|(bytes, consumed)| {
                (
                    SizeFilter::Range {
                        min_bytes: Some(bytes),
                        max_bytes: None,
                        label: format!("bigger than {}", format_size_label(bytes)),
                    },
                    consumed + 1,
                )
            })
        }
        "smaller" | "under" | "below" => {
            parse_size_after(tokens, index + 1).map(|(bytes, consumed)| {
                (
                    SizeFilter::Range {
                        min_bytes: None,
                        max_bytes: Some(bytes),
                        label: format!("smaller than {}", format_size_label(bytes)),
                    },
                    consumed + 1,
                )
            })
        }
        "more" if tokens.get(index + 1).map(|value| value.as_str()) == Some("than") => {
            parse_size_after(tokens, index + 2).map(|(bytes, consumed)| {
                (
                    SizeFilter::Range {
                        min_bytes: Some(bytes),
                        max_bytes: None,
                        label: format!("bigger than {}", format_size_label(bytes)),
                    },
                    consumed + 2,
                )
            })
        }
        "less" if tokens.get(index + 1).map(|value| value.as_str()) == Some("than") => {
            parse_size_after(tokens, index + 2).map(|(bytes, consumed)| {
                (
                    SizeFilter::Range {
                        min_bytes: None,
                        max_bytes: Some(bytes),
                        label: format!("smaller than {}", format_size_label(bytes)),
                    },
                    consumed + 2,
                )
            })
        }
        "at" if tokens.get(index + 1).map(|value| value.as_str()) == Some("least") => {
            parse_size_after(tokens, index + 2).map(|(bytes, consumed)| {
                (
                    SizeFilter::Range {
                        min_bytes: Some(bytes),
                        max_bytes: None,
                        label: format!("at least {}", format_size_label(bytes)),
                    },
                    consumed + 2,
                )
            })
        }
        "at" if tokens.get(index + 1).map(|value| value.as_str()) == Some("most") => {
            parse_size_after(tokens, index + 2).map(|(bytes, consumed)| {
                (
                    SizeFilter::Range {
                        min_bytes: None,
                        max_bytes: Some(bytes),
                        label: format!("at most {}", format_size_label(bytes)),
                    },
                    consumed + 2,
                )
            })
        }
        _ => None,
    }
}

fn parse_size_after(tokens: &[String], mut index: usize) -> Option<(u64, usize)> {
    let original_index = index;
    if tokens.get(index).map(|value| value.as_str()) == Some("than") {
        index += 1;
    }
    let (bytes, consumed) = parse_size_expression(tokens, index)?;
    Some((bytes, index.saturating_sub(original_index) + consumed))
}

fn parse_size_expression(tokens: &[String], index: usize) -> Option<(u64, usize)> {
    let token = tokens.get(index)?;
    let (number, unit, consumed) =
        split_size_token(token, tokens.get(index + 1).map(String::as_str))?;
    let multiplier = match unit {
        "b" | "byte" | "bytes" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(((number * multiplier).round() as u64, consumed))
}

fn split_size_token<'a>(token: &'a str, next: Option<&'a str>) -> Option<(f64, &'a str, usize)> {
    let mut number = String::new();
    let mut unit_start = token.len();
    for (index, ch) in token.char_indices() {
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
        } else {
            unit_start = index;
            break;
        }
    }
    if number.is_empty() {
        return None;
    }
    let value = number.parse::<f64>().ok()?;
    if value <= 0.0 {
        return None;
    }
    let unit = token[unit_start..].trim();
    if !unit.is_empty() {
        return Some((value, unit, 1));
    }
    let unit = next?;
    Some((value, unit, 2))
}

fn exact_year_filter(field: DateFieldIntent, year: i32) -> Option<DateFilter> {
    year_range_filter(field, year, year)
}

fn before_year_filter(field: DateFieldIntent, year: i32) -> Option<DateFilter> {
    Some(DateFilter {
        field,
        start_ms: None,
        end_ms: year_start_ms(year).map(|value| value.saturating_sub(1)),
        label: format!("{} before {year}", date_field_label(field)),
    })
}

fn after_year_filter(field: DateFieldIntent, year: i32) -> Option<DateFilter> {
    Some(DateFilter {
        field,
        start_ms: year_start_ms(year.saturating_add(1)),
        end_ms: None,
        label: format!("{} after {year}", date_field_label(field)),
    })
}

fn since_year_filter(field: DateFieldIntent, year: i32) -> Option<DateFilter> {
    Some(DateFilter {
        field,
        start_ms: year_start_ms(year),
        end_ms: None,
        label: format!("{} since {year}", date_field_label(field)),
    })
}

fn year_range_filter(field: DateFieldIntent, start_year: i32, end_year: i32) -> Option<DateFilter> {
    Some(DateFilter {
        field,
        start_ms: year_start_ms(start_year),
        end_ms: year_start_ms(end_year.saturating_add(1)).map(|value| value.saturating_sub(1)),
        label: if start_year == end_year {
            format!("{} in {start_year}", date_field_label(field))
        } else {
            format!(
                "{} between {start_year} and {end_year}",
                date_field_label(field)
            )
        },
    })
}

fn date_field_label(field: DateFieldIntent) -> &'static str {
    match field {
        DateFieldIntent::Modified => "Modified",
        DateFieldIntent::Created => "Created",
    }
}

fn year_start_ms(year: i32) -> Option<u64> {
    let date = time::Date::from_calendar_date(year, time::Month::January, 1).ok()?;
    let timestamp = date
        .with_time(time::Time::MIDNIGHT)
        .assume_utc()
        .unix_timestamp();
    if timestamp < 0 {
        None
    } else {
        Some(timestamp as u64 * 1000)
    }
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0)
}

fn parse_extension_filters(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };

    let mut filters = Vec::new();
    for token in raw.split(|ch: char| ch == ',' || ch == ';' || ch == '|' || ch.is_whitespace()) {
        let cleaned = token.trim().trim_start_matches('.').to_lowercase();
        if cleaned.is_empty() {
            continue;
        }
        if let Some(expanded) = token_extensions(&cleaned) {
            for ext in expanded {
                push_unique(&mut filters, ext);
            }
        }
    }
    filters
}

fn build_natural_query_string(
    query_text: &str,
    query_keywords: &[String],
    extension_filters: &[String],
) -> Option<String> {
    let mut clauses = Vec::new();

    if let Some(keyword_clause) = build_natural_keyword_clause(query_text, query_keywords) {
        clauses.push(format!("({keyword_clause})"));
    }

    if !extension_filters.is_empty() {
        let ext_clause = extension_filters
            .iter()
            .map(|ext| format!("extension:{ext}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        clauses.push(format!("({ext_clause})"));
    }

    if clauses.is_empty() {
        None
    } else {
        Some(clauses.join(" AND "))
    }
}

fn build_natural_keyword_clause(query_text: &str, query_keywords: &[String]) -> Option<String> {
    let mut keyword_groups = Vec::new();
    for keyword in query_keywords {
        let token = keyword.trim();
        if token.is_empty() {
            continue;
        }
        keyword_groups.push(token.to_string());
    }

    if !keyword_groups.is_empty() {
        return Some(keyword_groups.join(" "));
    }

    let query_text = query_text.trim();
    if query_text.is_empty() {
        None
    } else {
        Some(query_text.to_string())
    }
}

fn build_native_tantivy_query(
    query_keywords: &[String],
    extension_filters: &[String],
    natural_plan: &NaturalQueryPlan,
    fields: &SearchFields,
) -> Option<Box<dyn Query>> {
    // Only worth running when at least one filter can be pushed into Tantivy natively.
    // Pure keyword queries are already handled well by Pass 1.
    let has_date = natural_plan.date_filter.is_some();
    let has_size = matches!(natural_plan.size_filter, Some(SizeFilter::Range { .. }));
    let has_entry_type = natural_plan.entry_type_filter.is_some();
    if !has_date && !has_size && !has_entry_type {
        return None;
    }

    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    // Keywords: OR across file_name and path, combined as Must so the doc must contain
    // at least one keyword somewhere.
    if !query_keywords.is_empty() {
        let mut kw: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for keyword in query_keywords {
            let fn_term = Term::from_field_text(fields.file_name, keyword.as_str());
            kw.push((Occur::Should, Box::new(TermQuery::new(fn_term, IndexRecordOption::Basic))));
            let path_term = Term::from_field_text(fields.path, keyword.as_str());
            kw.push((Occur::Should, Box::new(TermQuery::new(path_term, IndexRecordOption::Basic))));
        }
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(kw))));
    }

    // Extension filter: Tantivy TermQuery on the STRING extension field.
    if !extension_filters.is_empty() {
        let mut ext: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for e in extension_filters {
            let term = Term::from_field_text(fields.extension, e.as_str());
            ext.push((Occur::Should, Box::new(TermQuery::new(term, IndexRecordOption::Basic))));
        }
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(ext))));
    }

    // Date range: native RangeQuery on the FAST u64 field.
    // Tantivy can skip entire segments using the column-oriented FAST reader.
    if let Some(date_filter) = &natural_plan.date_filter {
        let ms_field = match date_filter.field {
            DateFieldIntent::Modified => fields.modified_ms,
            DateFieldIntent::Created => fields.created_ms?,
        };
        let lower = date_filter
            .start_ms
            .map(|ms| Bound::Included(Term::from_field_u64(ms_field, ms)))
            .unwrap_or(Bound::Unbounded);
        let upper = date_filter
            .end_ms
            .map(|ms| Bound::Included(Term::from_field_u64(ms_field, ms)))
            .unwrap_or(Bound::Unbounded);
        let range = RangeQuery::new(lower, upper);
        clauses.push((Occur::Must, Box::new(range)));
    }

    // Size range: native RangeQuery on the FAST u64 size field.
    // SizeFilter::Empty is skipped here — empty-folder detection needs a filesystem check
    // that can't be expressed as a Tantivy query, so the post-filter handles it.
    if let Some(SizeFilter::Range { min_bytes, max_bytes, .. }) = &natural_plan.size_filter {
        let lower = min_bytes
            .map(|b| Bound::Included(Term::from_field_u64(fields.size, b)))
            .unwrap_or(Bound::Unbounded);
        let upper = max_bytes
            .map(|b| Bound::Included(Term::from_field_u64(fields.size, b)))
            .unwrap_or(Bound::Unbounded);
        let range = RangeQuery::new(lower, upper);
        clauses.push((Occur::Must, Box::new(range)));
    }

    // Entry type filter: TermQuery on the STRING entry_type field.
    if let Some(entry_type) = &natural_plan.entry_type_filter {
        if let Some(et_field) = fields.entry_type {
            let term = Term::from_field_text(et_field, entry_type.as_str());
            clauses.push((Occur::Must, Box::new(TermQuery::new(term, IndexRecordOption::Basic))));
        }
    }

    if clauses.is_empty() {
        None
    } else {
        Some(Box::new(BooleanQuery::new(clauses)))
    }
}

fn build_fuzzy_tantivy_query(
    query_keywords: &[String],
    file_name_field: Field,
    path_field: Field,
) -> Option<Box<dyn Query>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    for keyword in query_keywords {
        // Skip tokens too short for fuzzy — edit-distance-1 on 1-3 char tokens causes noise.
        if keyword.len() < 4 {
            continue;
        }
        // Build a FuzzyTermQuery on both the file_name and path fields so the
        // filename "Asura.mkv" is found when the user types "assura" or "asurr".
        for &field in &[file_name_field, path_field] {
            let term = Term::from_field_text(field, keyword.as_str());
            let fuzzy = FuzzyTermQuery::new(term, 1, true);
            clauses.push((Occur::Should, Box::new(fuzzy)));
        }
    }

    if clauses.is_empty() {
        None
    } else {
        Some(Box::new(BooleanQuery::new(clauses)))
    }
}

/// Build a prefix range query for the keywords. For each keyword `K`, this finds
/// every term in the file_name and path fields that starts with `K` by doing a
/// lexicographic range query over the term dictionary `[K, K_next)`.
///
/// This is what catches "asu" → "asura", "spi" → "spider", "xmen" → "xmen_..."
/// in single-digit milliseconds, replacing the 200ms AllQuery+scan fallback.
///
/// Tantivy's term dictionary is FST-backed, so range queries on string prefixes
/// are effectively free — they jump directly to the start of the range and stream.
fn build_prefix_tantivy_query(
    query_keywords: &[String],
    file_name_field: Field,
    path_field: Field,
) -> Option<Box<dyn Query>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    for keyword in query_keywords {
        // Single-character prefixes match too much; skip them.
        if keyword.len() < 2 {
            continue;
        }
        let lower = keyword.to_lowercase();
        // Build the lexicographic upper bound. For "spi" we want "spj" so that
        // every term >= "spi" and < "spj" is in range — i.e. every term starting with "spi".
        let upper = match next_lex_prefix(&lower) {
            Some(s) => s,
            None => continue,
        };

        for &field in &[file_name_field, path_field] {
            let lower_term = Term::from_field_text(field, &lower);
            let upper_term = Term::from_field_text(field, &upper);
            let range = RangeQuery::new(
                Bound::Included(lower_term),
                Bound::Excluded(upper_term),
            );
            clauses.push((Occur::Should, Box::new(range)));
        }
    }

    if clauses.is_empty() {
        None
    } else {
        Some(Box::new(BooleanQuery::new(clauses)))
    }
}

/// Return the next lexicographic string after `s`, used as the exclusive upper
/// bound of a prefix range. For "spi" returns "spj". Returns None if every byte
/// is already at maximum value (extremely unlikely for indexed text).
fn next_lex_prefix(s: &str) -> Option<String> {
    let mut bytes = s.as_bytes().to_vec();
    for i in (0..bytes.len()).rev() {
        if bytes[i] < 0xFF {
            bytes[i] = bytes[i].saturating_add(1);
            // Truncate at the incremented byte — anything after is irrelevant.
            bytes.truncate(i + 1);
            return String::from_utf8(bytes).ok();
        }
    }
    None
}

fn build_query_with_extension_filters(query_text: &str, extension_filters: &[String]) -> String {
    if extension_filters.is_empty() {
        return query_text.to_string();
    }

    let ext_clause = extension_filters
        .iter()
        .map(|ext| format!("extension:{ext}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    format!("({query_text}) AND ({ext_clause})")
}

fn extract_query_keywords(input: &str) -> Vec<String> {
    let mut keywords = Vec::new();
    for token in input.split_whitespace() {
        let cleaned = token
            .chars()
            .filter(|ch| ch.is_alphanumeric() || *ch == '.' || *ch == '_' || *ch == '-')
            .collect::<String>()
            .to_lowercase();
        if cleaned.is_empty() {
            continue;
        }
        let bare = cleaned.trim_start_matches('.');
        if bare.is_empty() || is_natural_stopword(bare) {
            continue;
        }
        push_unique(&mut keywords, bare.to_string());
    }
    keywords
}

fn edit_distance_within_1(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let la = a.len();
    let lb = b.len();
    if la.abs_diff(lb) > 1 {
        return false;
    }
    if a == b {
        return true;
    }
    let mut prev: Vec<usize> = (0..=lb).collect();
    let mut curr = vec![0usize; lb + 1];
    for i in 1..=la {
        curr[0] = i;
        for j in 1..=lb {
            curr[j] = if a[i - 1] == b[j - 1] {
                prev[j - 1]
            } else {
                1 + prev[j - 1].min(prev[j]).min(curr[j - 1])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[lb] <= 1
}

fn build_result_item(
    searcher: &Searcher,
    fields: &ResultDocFields,
    address: tantivy::DocAddress,
    score: f32,
    query_keywords: &[String],
    normalized_rank_query: &str,
    natural_language: bool,
    path_filter: Option<&str>,
    natural_plan: &NaturalQueryPlan,
    extension_filters: &[String],
    use_fuzzy: bool,
    frecency: &super::frecency::FrecencySnapshot,
) -> Result<Option<FileSearchResultItem>, String> {
    let retrieved = searcher
        .doc::<TantivyDocument>(address)
        .map_err(|error| format!("Cannot load search document: {error}"))?;

    let path = doc_text(&retrieved, fields.path).unwrap_or_default();
    if path.is_empty() {
        return Ok(None);
    }

    let entry_type = fields
        .entry_type
        .and_then(|field| doc_text(&retrieved, field))
        .unwrap_or_else(|| ENTRY_TYPE_FILE.to_string());
    let is_folder = entry_type == ENTRY_TYPE_FOLDER;
    if let Some(expected_entry_type) = natural_plan.entry_type_filter.as_deref() {
        if entry_type != expected_entry_type {
            return Ok(None);
        }
    }

    let extension = doc_text(&retrieved, fields.extension).unwrap_or_default();
    if !extension_filters.is_empty() && !extension_filters.iter().any(|value| value == &extension) {
        return Ok(None);
    }

    let modified_ms = doc_u64(&retrieved, fields.modified_ms).unwrap_or(0);
    let created_ms = fields
        .created_ms
        .and_then(|field| doc_u64(&retrieved, field))
        .unwrap_or(modified_ms);
    if let Some(date_filter) = natural_plan.date_filter.as_ref() {
        let date_value = match date_filter.field {
            DateFieldIntent::Modified => modified_ms,
            DateFieldIntent::Created => created_ms,
        };
        if !date_filter_matches(date_value, date_filter) {
            return Ok(None);
        }
    }

    let size = doc_u64(&retrieved, fields.size).unwrap_or(0);
    if let Some(size_filter) = natural_plan.size_filter.as_ref() {
        match size_filter {
            SizeFilter::Empty => {
                let is_empty = if is_folder {
                    is_empty_folder_path(Path::new(&path))
                } else {
                    size == 0
                };
                if !is_empty {
                    return Ok(None);
                }
            }
            SizeFilter::Range {
                min_bytes,
                max_bytes,
                ..
            } => {
                if is_folder {
                    return Ok(None);
                }
                if let Some(minimum) = min_bytes {
                    if size < *minimum {
                        return Ok(None);
                    }
                }
                if let Some(maximum) = max_bytes {
                    if size > *maximum {
                        return Ok(None);
                    }
                }
            }
        }
    }

    let path_lower = path.to_lowercase();
    if let Some(filter) = path_filter {
        if !path_lower.contains(filter) {
            return Ok(None);
        }
    }

    let file_name = doc_text(&retrieved, fields.file_name).unwrap_or_default();
    let file_name_lower = file_name.to_lowercase();
    let extension_lower = extension.to_lowercase();
    let normalized_file_name = normalize_launcher_text(&file_name_lower);
    let normalized_path = normalize_launcher_text(&path_lower);
    // Separator-collapsed forms (alphanumerics only) so a joined query keyword
    // like "spiderman" can match a hyphenated/spaced name like "spider-man" —
    // the receiving end of the decompound fallback in `search_filename_index`.
    let collapse =
        |text: &str| -> String { text.chars().filter(|c| c.is_alphanumeric()).collect() };
    let file_name_collapsed = collapse(&file_name_lower);
    let path_collapsed = collapse(&path_lower);
    let rank_query = normalized_rank_query.trim();
    let rank_query_collapsed = collapse(rank_query);
    let has_full_query_in_name = !rank_query.is_empty()
        && (normalized_file_name.contains(rank_query)
            || (!rank_query_collapsed.is_empty()
                && file_name_collapsed.contains(&rank_query_collapsed)));
    let has_full_query_in_path = !rank_query.is_empty()
        && (normalized_path.contains(rank_query)
            || (!rank_query_collapsed.is_empty()
                && path_collapsed.contains(&rank_query_collapsed)));
    let mut phrase_hits = 0usize;
    let mut phrase_path_hits = 0usize;
    for phrase in &natural_plan.exact_phrases {
        let normalized_phrase = normalize_launcher_text(phrase);
        if normalized_phrase.is_empty() {
            continue;
        }
        if normalized_file_name.contains(&normalized_phrase) {
            phrase_hits += 1;
        } else if normalized_path.contains(&normalized_phrase) {
            phrase_path_hits += 1;
        }
    }

    let mut exact_keyword_hits = 0usize;
    let mut prefix_keyword_hits = 0usize;
    let mut lexical_keyword_hits = 0usize;
    let mut fuzzy_keyword_hits = 0usize;
    let mut matched_keywords = Vec::new();
    for keyword in query_keywords {
        if keyword.is_empty() {
            continue;
        }

        let exact_token_match = normalized_text_has_exact_token(&normalized_file_name, keyword)
            || normalized_text_has_exact_token(&normalized_path, keyword)
            || extension_lower == *keyword;
        let prefix_token_match = !exact_token_match
            && keyword.len() >= 3
            && (normalized_text_has_prefix_token(&normalized_file_name, keyword)
                || normalized_text_has_prefix_token(&normalized_path, keyword));
        let contains_match = path_lower.contains(keyword)
            || file_name_lower.contains(keyword)
            || file_name_collapsed.contains(keyword)
            || path_collapsed.contains(keyword);
        let has_lexical_signal = contains_match || exact_token_match || prefix_token_match;

        if exact_token_match {
            exact_keyword_hits += 1;
        } else if prefix_token_match {
            prefix_keyword_hits += 1;
        }
        if has_lexical_signal {
            lexical_keyword_hits += 1;
            push_unique(&mut matched_keywords, keyword.clone());
        } else if use_fuzzy && keyword.len() >= 4 {
            // Fuzzy pass: check edit-distance-1 against each word in the filename and path.
            let mut fuzzy_hit = false;
            for word in normalized_file_name
                .split_whitespace()
                .chain(normalized_path.split_whitespace())
            {
                if word.len() >= 3 && edit_distance_within_1(keyword, word) {
                    fuzzy_hit = true;
                    break;
                }
            }
            if fuzzy_hit {
                fuzzy_keyword_hits += 1;
                push_unique(&mut matched_keywords, keyword.clone());
            }
        }
    }

    let effective_keyword_hits = lexical_keyword_hits + fuzzy_keyword_hits;

    if natural_language
        && !query_keywords.is_empty()
        && effective_keyword_hits == 0
        && !has_full_query_in_name
        && !has_full_query_in_path
        && phrase_hits == 0
        && phrase_path_hits == 0
    {
        // Drop results with no lexical or fuzzy signal.
        return Ok(None);
    }

    // For queries with 3+ keywords require at least half to match lexically or by fuzzy.
    // Single and two-keyword queries keep the existing pass-any-one behaviour.
    if natural_language && query_keywords.len() >= 3 {
        let required = (query_keywords.len() + 1) / 2; // ceil(N/2)
        if effective_keyword_hits < required && phrase_hits == 0 && phrase_path_hits == 0 {
            return Ok(None);
        }
    }

    if !extension_filters.is_empty() && !extension_lower.is_empty() {
        push_unique(&mut matched_keywords, extension_lower.clone());
    }
    if let Some(date_filter) = natural_plan.date_filter.as_ref() {
        push_unique(&mut matched_keywords, date_filter.label.clone());
    }
    if let Some(size_filter) = natural_plan.size_filter.as_ref() {
        push_unique(&mut matched_keywords, size_filter_label(size_filter));
    }
    for phrase in &natural_plan.exact_phrases {
        let normalized_phrase = normalize_launcher_text(phrase);
        if !normalized_phrase.is_empty()
            && (normalized_file_name.contains(&normalized_phrase)
                || normalized_path.contains(&normalized_phrase))
        {
            push_unique(&mut matched_keywords, phrase.clone());
        }
    }

    let mut reason_parts = Vec::new();
    if !matched_keywords.is_empty() {
        reason_parts.push(format!("Matched: {}", matched_keywords.join(", ")));
    }
    if is_folder {
        reason_parts.push("Folder result".to_string());
    }
    if let Some(expected_entry_type) = natural_plan.entry_type_filter.as_deref() {
        if expected_entry_type == ENTRY_TYPE_FOLDER {
            reason_parts.push("Folder filter".to_string());
        } else {
            reason_parts.push("File filter".to_string());
        }
    }
    if !extension_filters.is_empty() && !extension_lower.is_empty() {
        reason_parts.push(format!("Extension .{extension_lower}"));
    }
    if let Some(date_filter) = natural_plan.date_filter.as_ref() {
        reason_parts.push(date_filter.label.clone());
    }
    if let Some(size_filter) = natural_plan.size_filter.as_ref() {
        reason_parts.push(size_filter_label(size_filter));
    }
    if path_filter.is_some() {
        reason_parts.push("Path filter matched".to_string());
    }
    if reason_parts.is_empty() {
        reason_parts.push("Matched indexed relevance/content".to_string());
    }

    // Fuse the per-result signals into one score (Search task #13). The
    // lexical tallies gathered above grade name/path match strength; BM25,
    // frecency, and file recency are folded in by `rank::fuse`, so the whole
    // score now comes from one tunable place (`rank::WEIGHTS`) rather than the
    // pile of additive magic numbers this block used to be.
    let mut filter_bonus = 0.0f32;
    if natural_plan.entry_type_filter.as_deref() == Some(ENTRY_TYPE_FOLDER) && is_folder {
        filter_bonus += super::rank::FOLDER_FILTER_BONUS;
        if has_full_query_in_name || exact_keyword_hits > 0 {
            filter_bonus += super::rank::FOLDER_FILTER_NAME_BONUS;
        }
    }
    if natural_plan.entry_type_filter.as_deref() == Some(ENTRY_TYPE_FILE) && !is_folder {
        filter_bonus += super::rank::FILE_FILTER_BONUS;
    }
    // Extension-priority: when the query carries a multi-extension filter (a
    // category like "movies"/"video", or an explicit `ext:` list), rank by the
    // extension's position in that list. The lists are ordered most-likely-first
    // (mp4 > mkv > … > ts), so true movies sort above a stray `.ts` TypeScript
    // file. No effect for a single-extension filter.
    if let Some(position) = extension_filters
        .iter()
        .position(|value| value == &extension_lower)
    {
        filter_bonus += super::rank::ext_priority_bonus(position, extension_filters.len());
    }
    let lexical = super::rank::lexical_quality(&super::rank::LexicalHits {
        full_query_in_name: has_full_query_in_name,
        full_query_in_path: has_full_query_in_path,
        exact_keyword_hits,
        prefix_keyword_hits,
        fuzzy_keyword_hits,
        phrase_name_hits: phrase_hits,
        phrase_path_hits,
        total_keywords: query_keywords.iter().filter(|word| !word.is_empty()).count(),
    });
    let adjusted_score = super::rank::fuse(
        &super::rank::RankSignals {
            bm25: score,
            lexical,
            frecency_boost: frecency.boost(
                if is_folder { ENTRY_TYPE_FOLDER } else { ENTRY_TYPE_FILE },
                &path,
            ),
            modified_ms,
            filter_bonus,
            // Filename search has no content vector — semantic is a content
            // feature only.
            semantic: 0.0,
        },
        frecency.now_ms,
    );

    // Sensitive-content findings stored on this doc. Multi-valued STRING field
    // means we collect every value via repeated lookups. Old indexes (built
    // before the field existed) return an empty list, which is correct.
    let sensitive_kinds = collect_doc_text_values(&retrieved, fields.sensitive_kinds);

    Ok(Some(FileSearchResultItem {
        path,
        file_name,
        entry_type,
        extension,
        size,
        modified_ms,
        score: adjusted_score,
        matched_keywords,
        match_reason: reason_parts.join(" | "),
        sensitive_kinds,
    }))
}

/// A file the search index flagged as containing sensitive content.
pub struct SensitiveFile {
    pub path: String,
    pub file_name: String,
    /// Specific finding kinds (e.g. "aws_access_key", "us_ssn"); the universal
    /// "sensitive" tag is stripped — callers want the specifics.
    pub kinds: Vec<String>,
}

/// Result of the unencrypted-secrets scan: the flagged files + a total count
/// (which may exceed the returned slice) and an index-availability flag.
pub struct SensitiveFilesReport {
    pub files: Vec<SensitiveFile>,
    pub total: usize,
    /// True when no content index is built (or it predates the sensitive
    /// field) — the caller should prompt to build/rebuild it rather than
    /// report a misleading "all clear".
    pub index_unavailable: bool,
}

/// Collect files the index already flagged as containing sensitive content
/// (secrets / PII), by term-querying the `sensitive_kinds` tag written at
/// index time by `sensitive_scan`. Reuses existing index data — NO new
/// content scan — and is a purely LOCAL read (nothing leaves the machine).
/// Powers the Privacy Audit's unencrypted-secrets card.
pub fn collect_sensitive_files(
    app: &std::sync::Arc<dyn dash_core::Host>,
    limit: usize,
) -> Result<SensitiveFilesReport, String> {
    // Best-effort load; a content index may simply not exist yet.
    let _ = ensure_engine_loaded(app);
    let guard = SEARCH_ENGINE
        .lock()
        .map_err(|_| "Search engine lock failed".to_string())?;
    let Some(engine) = guard.as_ref() else {
        return Ok(SensitiveFilesReport {
            files: Vec::new(),
            total: 0,
            index_unavailable: true,
        });
    };
    let Some(kinds_field) = engine.fields.sensitive_kinds else {
        // Index predates the field — a rebuild is needed to populate it.
        return Ok(SensitiveFilesReport {
            files: Vec::new(),
            total: 0,
            index_unavailable: true,
        });
    };

    let searcher = engine.reader.searcher();
    let query = TermQuery::new(
        Term::from_field_text(kinds_field, "sensitive"),
        IndexRecordOption::Basic,
    );
    let capped = limit.clamp(1, 1000);

    let total = searcher
        .search(&query, &Count)
        .map_err(|error| format!("Sensitive scan failed: {error}"))?;
    let hits = searcher
        .search(&query, &TopDocs::with_limit(capped).order_by_score())
        .map_err(|error| format!("Sensitive scan failed: {error}"))?;

    let fields = engine.fields.result_doc_fields();
    let mut files = Vec::with_capacity(hits.len());
    for (_score, address) in hits {
        let Ok(doc) = searcher.doc::<TantivyDocument>(address) else {
            continue;
        };
        let path = doc_text(&doc, fields.path).unwrap_or_default();
        if path.is_empty() {
            continue;
        }
        let file_name = doc_text(&doc, fields.file_name).unwrap_or_default();
        let mut kinds = collect_doc_text_values(&doc, fields.sensitive_kinds);
        kinds.retain(|kind| kind != "sensitive"); // drop universal tag; keep specifics
        files.push(SensitiveFile {
            path,
            file_name,
            kinds,
        });
    }

    // Live-verify each index hit against the current file content.
    //
    // The Tantivy index records what a file contained AT INDEXING TIME. If
    // the user later edits a file to remove a secret (or deletes it) but the
    // watcher hasn't re-indexed it yet, the index entry is stale and the
    // Privacy Audit would show a "no longer contains detectable secrets" file
    // as still flagged. Fix: for every candidate, read up to 64 KB of the
    // live file and re-run the scanner. Files that no longer exist or that
    // contain no findings are excluded before we return.
    //
    // Cost: one small file read per sensitive hit (cap = 500 entries, typical
    // .env/config files are a few KB). Runs only on user-initiated audit
    // clicks — the overhead is negligible vs. the correctness gain.
    files.retain(|f| {
        use std::io::Read as _;
        let p = std::path::Path::new(&f.path);
        if !p.exists() {
            return false; // deleted since last index → exclude
        }
        let Ok(mut fh) = std::fs::File::open(p) else {
            return true; // can't open → conservative: keep the finding
        };
        let mut buf = vec![0u8; 64 * 1024];
        let n = fh.read(&mut buf).unwrap_or(0);
        buf.truncate(n);
        let content = String::from_utf8_lossy(&buf);
        !super::sensitive_scan::scan_text_detailed(&content, false).is_empty()
    });

    Ok(SensitiveFilesReport {
        files,
        total,
        index_unavailable: false,
    })
}

/// Resolve a content-index document to its address by exact path, via a term
/// query on the `path_exact` (STRING) field. Used by the semantic pass to
/// hydrate a pure-vector candidate the keyword passes never returned. `None`
/// when the path isn't in the index (e.g. deleted since it was embedded).
fn content_doc_address_for_path(
    searcher: &Searcher,
    fields: &SearchFields,
    path: &str,
) -> Option<tantivy::DocAddress> {
    let term = tantivy::Term::from_field_text(fields.path_exact, path);
    let query =
        tantivy::query::TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
    let top = searcher
        .search(&query, &TopDocs::with_limit(1).order_by_score())
        .ok()?;
    top.into_iter().next().map(|(_score, address)| address)
}

fn build_content_result_item(
    searcher: &Searcher,
    fields: &SearchFields,
    address: tantivy::DocAddress,
    score: f32,
    query_keywords: &[String],
    path_filter: Option<&str>,
    extension_filters: &[String],
    max_content_bytes: usize,
    frecency: &super::frecency::FrecencySnapshot,
    // Semantic (embedding cosine) score per lower-cased path, from the query's
    // vector pass. Empty when the semantic beta is off → every lookup is 0.
    semantic_scores: &HashMap<String, f32>,
    // True when this call is hydrating a pure-semantic candidate (no keyword
    // match); false on the keyword passes. Flows to `semantic_only` on the item.
    semantic_only: bool,
) -> Result<Option<ContentSearchResultItem>, String> {
    let retrieved = searcher
        .doc::<TantivyDocument>(address)
        .map_err(|error| format!("Cannot load content search document: {error}"))?;

    let path = doc_text(&retrieved, fields.path).unwrap_or_default();
    if path.is_empty() {
        return Ok(None);
    }

    let entry_type = fields
        .entry_type
        .and_then(|field| doc_text(&retrieved, field))
        .unwrap_or_else(|| ENTRY_TYPE_FILE.to_string());
    if entry_type != ENTRY_TYPE_FILE {
        return Ok(None);
    }

    let path_lower = path.to_lowercase();
    if let Some(filter) = path_filter {
        if !path_lower.contains(filter) {
            return Ok(None);
        }
    }
    let semantic_score = semantic_scores.get(&path_lower).copied().unwrap_or(0.0);

    let extension = doc_text(&retrieved, fields.extension).unwrap_or_default();
    if !extension_filters.is_empty() && !extension_filters.iter().any(|value| value == &extension) {
        return Ok(None);
    }

    let file_name = doc_text(&retrieved, fields.file_name).unwrap_or_else(|| {
        Path::new(&path)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(&path)
            .to_string()
    });

    // Prefer stored content (new indexes) — instant, no disk reads. Fall back
    // to a disk read for old indexes that pre-date the STORED schema change.
    // This keeps existing users working while new indexes get the fast path.
    // The disk read goes through the sandboxed extractor (untrusted file).
    let content = doc_text(&retrieved, fields.content)
        .filter(|s| !s.is_empty())
        .or_else(|| extract_text_sandboxed(Path::new(&path), max_content_bytes))
        .unwrap_or_default();

    // Snippet building, match counting, and matched-keyword detection each clean
    // + lowercase + scan the text — and on a large document (multi-hundred-KB
    // books) doing that several times per result, across every matched file, is
    // the dominant content-search cost (seconds on a common term like "უფალი"
    // across big Georgian books). Cap what these per-result text passes scan:
    // the hit itself already came from the FULL-text index (so no result is
    // dropped), highlightable matches almost always sit in the document head,
    // and the depth/coverage ranking signals are log-scaled + clamped, so the
    // ranking impact of a capped count is negligible. ~5-10× faster on big docs.
    const SNIPPET_SCAN_CAP_BYTES: usize = 96 * 1024;
    let scan_text: &str = if content.len() > SNIPPET_SCAN_CAP_BYTES {
        &content[..nearest_char_boundary_before(&content, SNIPPET_SCAN_CAP_BYTES)]
    } else {
        content.as_str()
    };

    // Extract multi-snippet (up to 3 non-overlapping windows). First becomes
    // the primary `snippet`, the rest go into `snippets` so the frontend can
    // optionally reveal them.
    let mut all_snippets = extract_highlighted_snippets(scan_text, query_keywords, 3, 110);
    let primary_snippet = if all_snippets.is_empty() {
        // Fallback: legacy snippet builder produces a plain-text excerpt with
        // surrounding context but no highlighting. HTML-escape so {@html} is safe.
        let raw = build_content_snippet(scan_text, query_keywords);
        html_escape(&raw)
    } else {
        all_snippets.remove(0)
    };

    let match_count = count_keyword_matches(scan_text, query_keywords);
    let matched_keywords = matched_content_keywords(scan_text, query_keywords);
    let sensitive_kinds = collect_doc_text_values(&retrieved, fields.sensitive_kinds);

    // Unified scoring (Search #13). Content relevance is BM25 over the indexed
    // text; `lexical` here is keyword *coverage* — the fraction of query
    // keywords present in the file — lifted by in-document frequency so a file
    // that mentions every query word many times outranks a passing mention.
    // Frecency + file recency fuse in via the same `rank` weights file search
    // uses; content results have no entry-type filter, so `filter_bonus` is 0.
    let modified_ms = doc_u64(&retrieved, fields.modified_ms).unwrap_or(0);
    let total_keywords = query_keywords.iter().filter(|word| !word.is_empty()).count();
    let coverage = if total_keywords > 0 {
        matched_keywords.len() as f32 / total_keywords as f32
    } else {
        0.0
    };
    // 0..1 in-document frequency factor — logarithmic so 200 mentions does not
    // dwarf a precise match in a file with 3.
    let depth = (((match_count as f32) + 1.0).ln() / 6.0).clamp(0.0, 1.0);
    let lexical = (coverage * (0.5 + 0.5 * depth)).clamp(0.0, 1.0);
    let fused_score = super::rank::fuse(
        &super::rank::RankSignals {
            bm25: score,
            lexical,
            frecency_boost: frecency.boost("file", &path),
            modified_ms,
            filter_bonus: 0.0,
            // Semantic re-rank score, attached by the caller when the semantic
            // beta is on (0 here keeps content ranking unchanged when it's off).
            semantic: semantic_score,
        },
        frecency.now_ms,
    );

    Ok(Some(ContentSearchResultItem {
        path,
        file_name,
        extension,
        size: doc_u64(&retrieved, fields.size).unwrap_or(0),
        modified_ms,
        score: fused_score,
        snippet: primary_snippet,
        snippets: all_snippets,
        match_count,
        matched_keywords,
        sensitive_kinds,
        semantic_score,
        semantic_only,
    }))
}

fn matched_content_keywords(content: &str, query_keywords: &[String]) -> Vec<String> {
    if content.is_empty() || query_keywords.is_empty() {
        return Vec::new();
    }
    let lower = content.to_lowercase();
    let mut matched = Vec::new();
    for keyword in query_keywords {
        if keyword.len() >= 2 && lower.contains(keyword) {
            push_unique(&mut matched, keyword.clone());
        }
    }
    matched
}

fn build_content_snippet(content: &str, query_keywords: &[String]) -> String {
    let clean_content = collapse_preview_whitespace(content);
    if clean_content.is_empty() {
        return "Indexed content matched, but preview text is not available.".to_string();
    }

    let ascii_lower = if clean_content.is_ascii() {
        clean_content.to_ascii_lowercase()
    } else {
        String::new()
    };
    let match_index = if ascii_lower.is_empty() {
        None
    } else {
        query_keywords
            .iter()
            .filter(|keyword| keyword.len() >= 2)
            .filter_map(|keyword| {
                ascii_lower
                    .find(keyword)
                    .map(|index| (index, keyword.len()))
            })
            .min_by_key(|(index, _)| *index)
    };

    let (start, end) = if let Some((index, len)) = match_index {
        let raw_start = index.saturating_sub(120);
        let raw_end = (index + len + 220).min(clean_content.len());
        (
            nearest_char_boundary_before(&clean_content, raw_start),
            nearest_char_boundary_after(&clean_content, raw_end),
        )
    } else {
        (
            0,
            nearest_char_boundary_after(&clean_content, clean_content.len().min(360)),
        )
    };

    let mut snippet = clean_content[start..end].trim().to_string();
    if start > 0 {
        snippet = format!("...{snippet}");
    }
    if end < clean_content.len() {
        snippet.push_str("...");
    }
    snippet
}

fn collapse_preview_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// HTML-escape a snippet of plain text so it's safe to inject with `{@html}`.
/// Only quotes/angle brackets matter — Svelte's @html doesn't itself sanitize.
fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Wrap every case-insensitive occurrence of any keyword with `<b>…</b>` after
/// HTML-escaping. Handles overlap by walking left-to-right with the longest
/// matching keyword preferred at each position.
fn highlight_text(text: &str, keywords: &[String]) -> String {
    if keywords.is_empty() || text.is_empty() {
        return html_escape(text);
    }
    // Pre-lowercase keywords (already lowercased by extract_query_keywords but
    // defensive) and sort longest-first so "investment" wins over "invest" when
    // matching from the same position.
    let mut kws: Vec<String> = keywords
        .iter()
        .filter(|k| k.len() >= 2)
        .map(|k| k.to_lowercase())
        .collect();
    kws.sort_by(|a, b| b.len().cmp(&a.len()));
    if kws.is_empty() {
        return html_escape(text);
    }

    let lower = text.to_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len() + 32);
    let mut i = 0usize;
    while i < text.len() {
        let mut matched: Option<usize> = None;
        for kw in &kws {
            if lower.as_bytes().len() < i + kw.len() {
                continue;
            }
            if &lower.as_bytes()[i..i + kw.len()] == kw.as_bytes() {
                matched = Some(kw.len());
                break;
            }
        }
        if let Some(len) = matched {
            // Ensure we don't slice mid-character.
            if text.is_char_boundary(i) && text.is_char_boundary(i + len) {
                out.push_str("<b>");
                out.push_str(&html_escape(&text[i..i + len]));
                out.push_str("</b>");
                i += len;
                continue;
            }
        }
        // Walk one character (not byte) forward, escaping as we go.
        let ch_end = {
            let mut j = i + 1;
            while j < bytes.len() && !text.is_char_boundary(j) {
                j += 1;
            }
            j
        };
        out.push_str(&html_escape(&text[i..ch_end]));
        i = ch_end;
    }
    out
}

/// Find every occurrence of every keyword and return a count. Used as a
/// secondary ranking signal (more mentions → more relevant) and displayed
/// to the user as a confidence cue ("47 matches").
fn count_keyword_matches(content: &str, keywords: &[String]) -> u32 {
    if content.is_empty() || keywords.is_empty() {
        return 0;
    }
    let lower = content.to_lowercase();
    let mut count: u32 = 0;
    for kw in keywords {
        if kw.len() < 2 {
            continue;
        }
        let kw_lower = kw.to_lowercase();
        let mut start = 0usize;
        while let Some(pos) = lower[start..].find(&kw_lower) {
            count = count.saturating_add(1);
            start += pos + kw_lower.len();
            if start >= lower.len() {
                break;
            }
        }
    }
    count
}

/// Extract up to `max_snippets` highlighted passages from `content` centered
/// on keyword matches. Snippets are non-overlapping and ordered by position so
/// they read naturally to the user. The window is sized so each snippet
/// carries useful surrounding context without becoming a wall of text.
fn extract_highlighted_snippets(
    content: &str,
    keywords: &[String],
    max_snippets: usize,
    window: usize,
) -> Vec<String> {
    let cleaned = collapse_preview_whitespace(content);
    if cleaned.is_empty() || keywords.is_empty() {
        return Vec::new();
    }
    let lower = cleaned.to_lowercase();

    // Collect every match position (deduplicated, sorted).
    let mut positions: Vec<(usize, usize)> = Vec::new();
    for kw in keywords {
        if kw.len() < 2 {
            continue;
        }
        let kw_lower = kw.to_lowercase();
        let mut start = 0usize;
        while let Some(pos) = lower[start..].find(&kw_lower) {
            let abs = start + pos;
            positions.push((abs, kw_lower.len()));
            start = abs + kw_lower.len();
        }
    }
    if positions.is_empty() {
        return Vec::new();
    }
    positions.sort_by_key(|(p, _)| *p);
    positions.dedup();

    let mut snippets: Vec<String> = Vec::new();
    let mut last_end = 0usize;
    for (pos, len) in positions {
        if pos < last_end {
            continue; // overlap with the previous window
        }
        let raw_start = pos.saturating_sub(window);
        let raw_end = (pos + len + window).min(cleaned.len());
        let start = nearest_char_boundary_before(&cleaned, raw_start);
        let end = nearest_char_boundary_after(&cleaned, raw_end);
        let mut snippet = highlight_text(cleaned[start..end].trim(), keywords);
        if start > 0 {
            snippet = format!("…{snippet}");
        }
        if end < cleaned.len() {
            snippet.push('…');
        }
        snippets.push(snippet);
        last_end = end;
        if snippets.len() >= max_snippets {
            break;
        }
    }
    snippets
}

/// Build a prefix-range query for the content field. Mirrors what main file
/// search does with file_name/path — when the parser's exact term match
/// returns nothing for "invest", a range query [invest, invesu) catches
/// "investment", "investing", "investor" via the FST term dictionary.
fn build_content_prefix_query(query_keywords: &[String], content_field: Field) -> Option<Box<dyn Query>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for keyword in query_keywords {
        if keyword.len() < 2 {
            continue;
        }
        let lower = keyword.to_lowercase();
        let upper = match next_lex_prefix(&lower) {
            Some(s) => s,
            None => continue,
        };
        let lower_term = Term::from_field_text(content_field, &lower);
        let upper_term = Term::from_field_text(content_field, &upper);
        let range = RangeQuery::new(
            Bound::Included(lower_term),
            Bound::Excluded(upper_term),
        );
        clauses.push((Occur::Should, Box::new(range)));
    }
    if clauses.is_empty() {
        None
    } else {
        Some(Box::new(BooleanQuery::new(clauses)))
    }
}

/// FuzzyTermQuery on the content field for edit-distance-1 typo tolerance.
/// Only applies to tokens long enough that one edit doesn't hit every word.
fn build_content_fuzzy_query(query_keywords: &[String], content_field: Field) -> Option<Box<dyn Query>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for keyword in query_keywords {
        if keyword.len() < 4 {
            continue;
        }
        let term = Term::from_field_text(content_field, keyword.as_str());
        let fuzzy = FuzzyTermQuery::new(term, 1, true);
        clauses.push((Occur::Should, Box::new(fuzzy)));
    }
    if clauses.is_empty() {
        None
    } else {
        Some(Box::new(BooleanQuery::new(clauses)))
    }
}

fn nearest_char_boundary_before(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn nearest_char_boundary_after(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn date_filter_matches(value_ms: u64, filter: &DateFilter) -> bool {
    if value_ms == 0 {
        return false;
    }
    if let Some(start_ms) = filter.start_ms {
        if value_ms < start_ms {
            return false;
        }
    }
    if let Some(end_ms) = filter.end_ms {
        if value_ms > end_ms {
            return false;
        }
    }
    true
}

fn size_filter_label(filter: &SizeFilter) -> String {
    match filter {
        SizeFilter::Empty => "empty".to_string(),
        SizeFilter::Range { label, .. } => label.clone(),
    }
}

fn format_size_label(bytes: u64) -> String {
    if bytes >= 1024_u64.pow(4) {
        format!("{:.1} TB", bytes as f64 / 1024_u64.pow(4) as f64)
    } else if bytes >= 1024_u64.pow(3) {
        format!("{:.1} GB", bytes as f64 / 1024_u64.pow(3) as f64)
    } else if bytes >= 1024_u64.pow(2) {
        format!("{:.1} MB", bytes as f64 / 1024_u64.pow(2) as f64)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn is_empty_folder_path(path: &Path) -> bool {
    fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

fn parse_year_token(token: &str) -> Option<i32> {
    if token.len() != 4 || !token.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let year = token.parse::<i32>().ok()?;
    if (1970..=2100).contains(&year) {
        Some(year)
    } else {
        None
    }
}

fn token_extensions(token: &str) -> Option<Vec<String>> {
    if let Some(exts) = token_category_extensions(token) {
        return Some(exts);
    }
    if looks_like_extension(token) {
        return Some(normalize_extension_token(token));
    }
    None
}

fn token_category_extensions(token: &str) -> Option<Vec<String>> {
    match token {
        "pdf" | "pdfs" => Some(vec!["pdf".to_string()]),
        "doc" | "docx" | "document" | "documents" | "word" => Some(vec![
            "doc".to_string(),
            "docx".to_string(),
            "odt".to_string(),
            "rtf".to_string(),
            "txt".to_string(),
            "md".to_string(),
        ]),
        // "movies"/"films" → the container formats actual movie files use.
        // Deliberately EXCLUDES web/streaming formats (webm, ogv, flv) that
        // movies aren't distributed in — use "videos" for the everything bucket.
        "movie" | "movies" | "film" | "films" => Some(vec![
            "mp4".to_string(),
            "mkv".to_string(),
            "avi".to_string(),
            "mov".to_string(),
            "m4v".to_string(),
            "wmv".to_string(),
            "ts".to_string(),
            "m2ts".to_string(),
        ]),
        // "videos" → the BROAD bucket: every video format, including the movie
        // containers above PLUS web/streaming/mobile/legacy formats.
        "video" | "videos" => Some(vec![
            "mp4".to_string(),
            "mkv".to_string(),
            "avi".to_string(),
            "mov".to_string(),
            "wmv".to_string(),
            "m4v".to_string(),
            "webm".to_string(),
            "flv".to_string(),
            "mpg".to_string(),
            "mpeg".to_string(),
            "3gp".to_string(),
            "ogv".to_string(),
            "ts".to_string(),
            "m2ts".to_string(),
            "mts".to_string(),
            "vob".to_string(),
            "asf".to_string(),
            "f4v".to_string(),
            "divx".to_string(),
        ]),
        "music" | "audio" | "song" | "songs" => Some(vec![
            "mp3".to_string(),
            "wav".to_string(),
            "flac".to_string(),
            "m4a".to_string(),
            "ogg".to_string(),
            "aac".to_string(),
            "opus".to_string(),
            "wma".to_string(),
            "aiff".to_string(),
            "aif".to_string(),
            "m4b".to_string(),
            "mid".to_string(),
            "midi".to_string(),
        ]),
        "image" | "images" | "photo" | "photos" | "picture" | "pictures" => Some(vec![
            "jpg".to_string(),
            "jpeg".to_string(),
            "png".to_string(),
            "webp".to_string(),
            "bmp".to_string(),
            "gif".to_string(),
            "tiff".to_string(),
            "heic".to_string(),
            "avif".to_string(),
            "ico".to_string(),
            "jfif".to_string(),
            "jxl".to_string(),
        ]),
        "archive" | "archives" => Some(vec![
            "zip".to_string(),
            "7z".to_string(),
            "rar".to_string(),
            "tar".to_string(),
            "gz".to_string(),
            "xz".to_string(),
            "bz2".to_string(),
            "zst".to_string(),
            "tgz".to_string(),
            "cab".to_string(),
        ]),
        "spreadsheet" | "spreadsheets" | "excel" => Some(vec![
            "xlsx".to_string(),
            "xls".to_string(),
            "csv".to_string(),
            "ods".to_string(),
            "xlsm".to_string(),
            "xlsb".to_string(),
            "tsv".to_string(),
        ]),
        "ebook" | "ebooks" | "book" | "books" => Some(vec![
            "epub".to_string(),
            "mobi".to_string(),
            "azw".to_string(),
            "azw3".to_string(),
            "pdf".to_string(),
            "cbz".to_string(),
            "cbr".to_string(),
            "djvu".to_string(),
            "fb2".to_string(),
        ]),
        "presentation" | "presentations" | "slideshow" | "slides" | "powerpoint" => Some(vec![
            "pptx".to_string(),
            "ppt".to_string(),
            "odp".to_string(),
            "key".to_string(),
        ]),
        "code" | "source" | "sourcecode" => Some(vec![
            "rs".to_string(),
            "py".to_string(),
            "js".to_string(),
            "ts".to_string(),
            "go".to_string(),
            "java".to_string(),
            "cpp".to_string(),
            "c".to_string(),
            "h".to_string(),
            "rb".to_string(),
            "php".to_string(),
            "swift".to_string(),
            "kt".to_string(),
            "cs".to_string(),
            "sh".to_string(),
            "html".to_string(),
            "css".to_string(),
            "scss".to_string(),
            "json".to_string(),
            "yaml".to_string(),
            "toml".to_string(),
            "xml".to_string(),
            "jsx".to_string(),
            "tsx".to_string(),
            "vue".to_string(),
            "svelte".to_string(),
            "lua".to_string(),
            "dart".to_string(),
            "sql".to_string(),
            "pl".to_string(),
            "r".to_string(),
        ]),
        "font" | "fonts" | "typeface" | "typefaces" => Some(vec![
            "ttf".to_string(),
            "otf".to_string(),
            "woff".to_string(),
            "woff2".to_string(),
            "eot".to_string(),
        ]),
        "database" | "db" | "sql" => Some(vec![
            "sql".to_string(),
            "db".to_string(),
            "sqlite".to_string(),
            "sqlite3".to_string(),
            "mdb".to_string(),
            "accdb".to_string(),
        ]),
        "email" | "emails" | "mail" | "eml" => Some(vec![
            "eml".to_string(),
            "msg".to_string(),
            "pst".to_string(),
            "mbox".to_string(),
        ]),
        "subtitle" | "subtitles" | "caption" | "captions" | "srt" => Some(vec![
            "srt".to_string(),
            "vtt".to_string(),
            "ass".to_string(),
            "ssa".to_string(),
            "sub".to_string(),
        ]),
        "raw" | "rawphoto" | "rawimage" => Some(vec![
            "raw".to_string(),
            "cr2".to_string(),
            "cr3".to_string(),
            "nef".to_string(),
            "arw".to_string(),
            "dng".to_string(),
            "orf".to_string(),
            "rw2".to_string(),
        ]),
        "executable" | "installer" | "setup" | "app" => Some(vec![
            "exe".to_string(),
            "msi".to_string(),
            "dmg".to_string(),
            "pkg".to_string(),
            "deb".to_string(),
            "rpm".to_string(),
            "appimage".to_string(),
            "bat".to_string(),
            "cmd".to_string(),
            "ps1".to_string(),
            "apk".to_string(),
            "jar".to_string(),
        ]),
        "calendar" | "ics" | "vcal" => Some(vec![
            "ics".to_string(),
            "vcf".to_string(),
        ]),
        "vector" | "illustration" | "illustrator" => Some(vec![
            "svg".to_string(),
            "ai".to_string(),
            "eps".to_string(),
            "cdr".to_string(),
        ]),
        "3d" | "model" | "mesh" => Some(vec![
            "obj".to_string(),
            "fbx".to_string(),
            "stl".to_string(),
            "gltf".to_string(),
            "glb".to_string(),
            "blend".to_string(),
            "dae".to_string(),
            "3ds".to_string(),
            "ply".to_string(),
            "usd".to_string(),
            "usdz".to_string(),
        ]),
        "disk" | "iso" | "diskimage" => Some(vec![
            "iso".to_string(),
            "img".to_string(),
            "vmdk".to_string(),
            "vhd".to_string(),
            "vhdx".to_string(),
        ]),
        _ => None,
    }
}

fn normalize_extension_token(token: &str) -> Vec<String> {
    match token {
        "jpeg" => vec!["jpg".to_string(), "jpeg".to_string()],
        "htm" => vec!["htm".to_string(), "html".to_string()],
        "yml" => vec!["yml".to_string(), "yaml".to_string()],
        _ => vec![token.to_string()],
    }
}

fn looks_like_extension(token: &str) -> bool {
    token.len() >= 2
        && token.len() <= 8
        && token.chars().all(|ch| ch.is_ascii_alphanumeric())
        && !is_natural_stopword(token)
}

fn is_common_extension_token(token: &str) -> bool {
    if !looks_like_extension(token) {
        return false;
    }

    matches!(
        token,
        "txt"
            | "md"
            | "markdown"
            | "csv"
            | "tsv"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "xml"
            | "log"
            | "ini"
            | "cfg"
            | "conf"
            | "env"
            | "sql"
            | "js"
            | "ts"
            | "jsx"
            | "tsx"
            | "rs"
            | "py"
            | "go"
            | "java"
            | "kt"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "swift"
            | "php"
            | "rb"
            | "sh"
            | "ps1"
            | "svelte"
            | "html"
            | "htm"
            | "css"
            | "pdf"
            | "doc"
            | "docx"
            | "odt"
            | "rtf"
            | "xls"
            | "xlsx"
            | "ppt"
            | "pptx"
            | "mp4"
            | "mkv"
            | "avi"
            | "mov"
            | "wmv"
            | "webm"
            | "m4v"
            | "mp3"
            | "wav"
            | "flac"
            | "m4a"
            | "ogg"
            | "aac"
            | "jpg"
            | "jpeg"
            | "png"
            | "webp"
            | "bmp"
            | "gif"
            | "tiff"
            | "heic"
            | "zip"
            | "7z"
            | "rar"
            | "tar"
            | "gz"
            | "xz"
    )
}

fn is_year_filter_hint(token: &str) -> bool {
    matches!(
        token,
        // `from`/`in` deliberately excluded — a bare year is a filename match.
        "during"
            | "year"
            | "date"
            | "created"
            | "creation"
            | "made"
            | "createdat"
            | "createdon"
            | "modified"
            | "updated"
            | "changed"
            | "edited"
            | "modifiedat"
            | "modifiedon"
            | "updatedat"
    )
}

fn is_natural_stopword(token: &str) -> bool {
    matches!(
        token,
        "a" | "an"
            | "the"
            | "please"
            | "show"
            | "find"
            | "search"
            | "for"
            | "about"
            | "with"
            | "without"
            | "files"
            | "file"
            | "me"
            | "my"
            | "in"
            | "on"
            | "at"
            | "to"
            | "of"
            | "from"
            | "by"
            | "or"
            | "and"
            | "all"
            | "just"
            | "only"
            | "folder"
            | "folders"
            | "directory"
            | "directories"
            | "dir"
            | "format"
            | "formats"
            | "type"
            | "types"
            | "kind"
            | "kinds"
    )
}

/// Arm the launch-target folder watcher once (idempotent, process-global).
///
/// `LAUNCH_TARGET_CACHE` is otherwise built once and never refreshed for the
/// process lifetime, so a freshly-installed app — whose installer drops a
/// `.lnk` into Start Menu\Programs — wouldn't appear in the palette until the
/// next app restart. Watching the handful of shallow launcher roots (Start
/// Menu user/system + Desktop user/public) and clearing the cache on any
/// add/remove/rename makes a new app show up on the next search. Cheap on
/// Windows: `ReadDirectoryChangesW` watches each root's whole subtree with a
/// single handle. Best-effort: if the watcher can't start, the cache simply
/// keeps its build-once behavior — no worse than before.
fn ensure_launch_target_watcher() {
    let Ok(mut slot) = LAUNCH_TARGET_WATCHER.lock() else {
        return;
    };
    if slot.is_some() {
        return; // already watching
    }

    let mut watcher = match notify::recommended_watcher(|res: notify::Result<Event>| {
        let Ok(event) = res else { return };
        // An app install / uninstall / rename surfaces as a create / remove /
        // rename under a launcher root. Invalidate the cache; the next
        // `search_launch_targets` rebuilds it lazily (off the UI thread).
        if matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_)
        ) {
            if let Ok(mut cache) = LAUNCH_TARGET_CACHE.lock() {
                *cache = None;
            }
        }
    }) {
        Ok(watcher) => watcher,
        Err(_) => return,
    };

    let mut watching = false;
    for (root, _source) in launcher_candidate_roots() {
        if root.exists() && watcher.watch(&root, RecursiveMode::Recursive).is_ok() {
            watching = true;
        }
    }

    // Only retain the watcher if it's actually watching at least one root;
    // otherwise drop it so a later build can retry (e.g. a root that didn't
    // exist at first launch).
    if watching {
        *slot = Some(watcher);
    }
}

fn ensure_launch_target_cache() -> Result<LaunchTargetCache, String> {
    {
        let guard = LAUNCH_TARGET_CACHE
            .lock()
            .map_err(|_| "Launch target cache lock failed".to_string())?;
        if let Some(cache) = guard.as_ref() {
            return Ok(cache.clone());
        }
    }

    let cache = build_launch_target_cache();
    // First build also arms the folder watcher, so later app installs/removals
    // invalidate the cache automatically (idempotent — only starts once).
    ensure_launch_target_watcher();
    let mut guard = LAUNCH_TARGET_CACHE
        .lock()
        .map_err(|_| "Launch target cache lock failed".to_string())?;
    if guard.is_none() {
        *guard = Some(cache.clone());
    }
    Ok(guard.as_ref().cloned().unwrap_or(cache))
}

fn build_launch_target_cache() -> LaunchTargetCache {
    let mut records = Vec::new();
    let mut seen_paths = HashSet::new();

    for (root, source) in launcher_candidate_roots() {
        if !root.exists() {
            continue;
        }

        let iter = WalkDir::new(&root)
            .follow_links(false)
            .max_depth(MAX_LAUNCH_SCAN_DEPTH)
            .into_iter();

        for entry in iter.filter_map(|item| item.ok()) {
            if records.len() >= MAX_LAUNCH_TARGETS {
                break;
            }

            let path = entry.path();
            let file_type = entry.file_type();

            #[cfg(target_os = "macos")]
            if file_type.is_dir()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("app"))
                    .unwrap_or(false)
            {
                add_launch_target_record(&mut records, &mut seen_paths, path, source, "bundle");
                continue;
            }

            if !file_type.is_file() || !is_launch_target_file(path) {
                continue;
            }

            let kind = if path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("lnk") || ext.eq_ignore_ascii_case("desktop"))
                .unwrap_or(false)
            {
                "shortcut"
            } else {
                "application"
            };
            add_launch_target_record(&mut records, &mut seen_paths, path, source, kind);
        }
    }

    for (name, path) in builtin_launcher_targets() {
        if records.len() >= MAX_LAUNCH_TARGETS {
            break;
        }
        add_launch_target_record_with_name(
            &mut records,
            &mut seen_paths,
            &path,
            "system-builtin",
            "application",
            Some(name),
        );
    }

    // Packaged (MSIX / Microsoft Store) apps. These have no `.lnk` and no
    // reachable exe, so the walk above cannot see them at all — without this,
    // Settings, Terminal, ChatGPT, Claude and every other Store app are simply
    // absent from the launcher. See `commands::packaged_apps`.
    for app in crate::packaged_apps::enumerate_packaged_apps() {
        if records.len() >= MAX_LAUNCH_TARGETS {
            break;
        }
        let launch_path = crate::packaged_apps::packaged_launch_path(&app.aumid);
        add_packaged_launch_target_record(&mut records, &mut seen_paths, &app.name, &launch_path);
    }

    let built_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);

    LaunchTargetCache {
        built_at_ms,
        records,
    }
}

fn launcher_candidate_roots() -> Vec<(PathBuf, &'static str)> {
    let mut roots: Vec<(PathBuf, &'static str)> = Vec::new();

    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = env::var("APPDATA") {
            roots.push((
                PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs"),
                "start-menu-user",
            ));
        }
        if let Ok(program_data) = env::var("PROGRAMDATA") {
            roots.push((
                PathBuf::from(program_data).join(r"Microsoft\Windows\Start Menu\Programs"),
                "start-menu-system",
            ));
        }
        if let Ok(user_profile) = env::var("USERPROFILE") {
            roots.push((PathBuf::from(user_profile).join("Desktop"), "desktop-user"));
        }
        if let Ok(public_profile) = env::var("PUBLIC") {
            roots.push((
                PathBuf::from(public_profile).join("Desktop"),
                "desktop-public",
            ));
        }
    }

    #[cfg(target_os = "macos")]
    {
        roots.push((PathBuf::from("/Applications"), "applications-system"));
        if let Ok(home) = env::var("HOME") {
            roots.push((
                PathBuf::from(home).join("Applications"),
                "applications-user",
            ));
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        roots.push((
            PathBuf::from("/usr/share/applications"),
            "applications-system",
        ));
        roots.push((
            PathBuf::from("/usr/local/share/applications"),
            "applications-local",
        ));
        if let Ok(home) = env::var("HOME") {
            roots.push((
                PathBuf::from(home).join(".local/share/applications"),
                "applications-user",
            ));
        }
    }

    roots
}

fn builtin_launcher_targets() -> Vec<(String, PathBuf)> {
    let mut targets = Vec::new();

    #[cfg(target_os = "windows")]
    {
        let mut push_if_exists = |name: &str, path: &str| {
            let resolved = PathBuf::from(path);
            if resolved.exists() {
                targets.push((name.to_string(), resolved));
            }
        };

        push_if_exists("Snipping Tool", r"C:\Windows\System32\SnippingTool.exe");
        push_if_exists("Notepad", r"C:\Windows\System32\notepad.exe");
        push_if_exists("Calculator", r"C:\Windows\System32\calc.exe");
        push_if_exists("Paint", r"C:\Windows\System32\mspaint.exe");
    }

    targets
}

fn is_launch_target_file(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    let extension = extension.to_ascii_lowercase();

    #[cfg(target_os = "windows")]
    {
        return matches!(extension.as_str(), "lnk" | "exe" | "url" | "msc");
    }

    #[cfg(target_os = "macos")]
    {
        return matches!(extension.as_str(), "app");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return matches!(extension.as_str(), "desktop" | "appimage" | "sh");
    }

    #[allow(unreachable_code)]
    false
}

fn add_launch_target_record(
    records: &mut Vec<LaunchTargetRecord>,
    seen_paths: &mut HashSet<String>,
    path: &Path,
    source: &str,
    kind: &str,
) {
    add_launch_target_record_with_name(records, seen_paths, path, source, kind, None);
}

fn add_launch_target_record_with_name(
    records: &mut Vec<LaunchTargetRecord>,
    seen_paths: &mut HashSet<String>,
    path: &Path,
    source: &str,
    kind: &str,
    override_name: Option<String>,
) {
    if !path.exists() {
        return;
    }

    let path_text = path.to_string_lossy().to_string();
    let path_key = path_text.to_ascii_lowercase();
    if !seen_paths.insert(path_key.clone()) {
        return;
    }

    let name = override_name.unwrap_or_else(|| {
        path.file_stem()
            .or_else(|| path.file_name())
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string()
    });
    let name = name.trim().to_string();
    if name.is_empty() {
        return;
    }

    records.push(LaunchTargetRecord {
        id: format!("launch://{path_key}"),
        name: name.clone(),
        path: path_text.clone(),
        kind: kind.to_string(),
        source: source.to_string(),
        normalized_name: normalize_launcher_text(&name),
        normalized_path: normalize_launcher_text(&path_text),
    });
}

/// Insert a packaged (MSIX / Store) app record.
///
/// Deliberately separate from [`add_launch_target_record_with_name`]: that one
/// gates on `path.exists()`, which is correct for files and always false for a
/// packaged app — its "path" is the synthetic `shell:AppsFolder\<AUMID>` handle,
/// not a file. The name always comes from the shell, so there is no file-stem
/// fallback either. Everything downstream (dedup key, id, normalization) matches
/// the file path exactly, so packaged apps rank and dedupe like any other target.
fn add_packaged_launch_target_record(
    records: &mut Vec<LaunchTargetRecord>,
    seen_paths: &mut HashSet<String>,
    name: &str,
    launch_path: &str,
) {
    let path_key = launch_path.to_ascii_lowercase();
    if !seen_paths.insert(path_key.clone()) {
        return;
    }
    let name = name.trim().to_string();
    if name.is_empty() {
        return;
    }

    records.push(LaunchTargetRecord {
        id: format!("launch://{path_key}"),
        name: name.clone(),
        path: launch_path.to_string(),
        kind: "packaged".to_string(),
        source: "apps-folder".to_string(),
        normalized_name: normalize_launcher_text(&name),
        // Normalize the AUMID too: it carries the package family name, so a
        // query like "sticky notes" or "microsoft" can still reach the app even
        // when the display name alone wouldn't match.
        normalized_path: normalize_launcher_text(launch_path),
    });
}

fn normalize_launcher_text(input: &str) -> String {
    let mut normalized = String::with_capacity(input.len());
    let mut previous_space = false;

    for ch in input.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else if ch == '+' {
            ' '
        } else {
            ' '
        };

        if mapped == ' ' {
            if !previous_space {
                normalized.push(' ');
                previous_space = true;
            }
        } else {
            normalized.push(mapped);
            previous_space = false;
        }
    }

    normalized.trim().to_string()
}

fn normalize_launch_path_for_match(path: &Path) -> String {
    let raw = path.to_string_lossy().to_string();
    #[cfg(target_os = "windows")]
    {
        raw.to_ascii_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        raw
    }
}

fn score_launch_target(
    record: &LaunchTargetRecord,
    query_normalized: &str,
    query_tokens: &[String],
) -> Option<f32> {
    if query_normalized.is_empty() {
        return None;
    }

    let mut matched_tokens = 0usize;
    let mut score = 0.0_f32;

    if record.normalized_name == query_normalized {
        score += 500.0;
    } else if record.normalized_name.starts_with(query_normalized) {
        score += 300.0;
    } else if record.normalized_name.contains(query_normalized) {
        score += 180.0;
    } else if record.normalized_path.contains(query_normalized) {
        score += 90.0;
    }

    for token in query_tokens {
        if token.is_empty() {
            continue;
        }
        if normalized_text_has_exact_token(&record.normalized_name, token) {
            matched_tokens += 1;
            score += 72.0;
        } else if token.len() >= 3
            && normalized_text_has_prefix_token(&record.normalized_name, token)
        {
            matched_tokens += 1;
            score += 44.0;
        } else if normalized_text_has_exact_token(&record.normalized_path, token) {
            matched_tokens += 1;
            score += 26.0;
        } else if token.len() >= 3
            && normalized_text_has_prefix_token(&record.normalized_path, token)
        {
            matched_tokens += 1;
            score += 14.0;
        }
    }

    if query_tokens.len() >= 2 && matched_tokens < 2 && score < 180.0 {
        return None;
    }
    if matched_tokens == 0 && score < 80.0 {
        return None;
    }

    let token_bonus = if query_tokens.is_empty() {
        0.0
    } else {
        (matched_tokens as f32 / query_tokens.len() as f32) * 40.0
    };

    let kind_bonus = match record.kind.as_str() {
        "application" => 12.0,
        "shortcut" => 8.0,
        _ => 0.0,
    };

    Some(score + token_bonus + kind_bonus)
}

fn normalized_text_has_exact_token(text: &str, token: &str) -> bool {
    text.split_whitespace().any(|word| word == token)
}

fn normalized_text_has_prefix_token(text: &str, token: &str) -> bool {
    text.split_whitespace().any(|word| word.starts_with(token))
}

fn update_status(update: impl FnOnce(&mut FileSearchStatus)) {
    if let Ok(mut status) = SEARCH_STATUS.lock() {
        update(&mut status);
    }
}

fn emit_progress(app: &std::sync::Arc<dyn dash_core::Host>, payload: FileSearchProgressEvent) {
    app.emit(INDEX_PROGRESS_EVENT, serde_json::to_value(&payload).unwrap_or_default());
}

// ---- Read access for the in-memory name table (`names.rs`, F1) ----
// Self-contained: nothing else in this file calls these.

/// One live filename-index document, borrowed for `visit_filename_index_docs`
/// (the name is the path's last component, so `file_name` isn't read).
pub(crate) struct FilenameDoc<'a> {
    pub path: &'a str,
    pub is_dir: bool,
    pub size: u64,
    pub modified_ms: u64,
    pub created_ms: u64,
}

/// Changes whenever the searcher's contents change: hashes each segment's id
/// and delete opstamp, which every commit, merge or rebuild alters.
fn filename_searcher_fingerprint(searcher: &Searcher) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for segment in searcher.segment_readers() {
        segment.segment_id().hash(&mut hasher);
        segment.delete_opstamp().hash(&mut hasher);
    }
    hasher.finish()
}

/// Fingerprint of the active filename index (opening it if needed); `None`
/// when there is none, or while a rebuild has released it.
pub(crate) fn filename_index_fingerprint(app: &std::sync::Arc<dyn dash_core::Host>) -> Option<u64> {
    ensure_filename_engine_loaded(app);
    let guard = FILENAME_ENGINE.lock().ok()?;
    Some(filename_searcher_fingerprint(&guard.as_ref()?.reader.searcher()))
}

/// Calls `visit` for every live document of the active filename index and
/// returns the fingerprint of what was read. `None` when there is no index, or
/// when a rebuild released the handle mid-read (the open searcher would block
/// its directory swap on Windows, so the read stops early).
pub(crate) fn visit_filename_index_docs(
    app: &std::sync::Arc<dyn dash_core::Host>,
    visit: &mut dyn FnMut(FilenameDoc),
) -> Option<u64> {
    ensure_filename_engine_loaded(app);
    let (searcher, fields) = {
        let guard = FILENAME_ENGINE.lock().ok()?;
        let handle = guard.as_ref()?;
        (handle.reader.searcher(), handle.fields)
    };
    let still_open = || FILENAME_ENGINE.lock().map(|guard| guard.is_some()).unwrap_or(false);
    visit_filename_searcher(&searcher, fields, &still_open, visit)
}

/// Same as `visit_filename_index_docs` for an index directory, opened
/// read-only (real-data timing test).
#[cfg(test)]
pub(crate) fn visit_filename_index_at(dir: &Path, visit: &mut dyn FnMut(FilenameDoc)) -> Option<u64> {
    let index = Index::open_in_dir(dir).ok()?;
    let fields = extract_filename_index_fields(&index.schema()).ok()?;
    let reader: IndexReader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into().ok()?;
    visit_filename_searcher(&reader.searcher(), fields, &|| true, visit)
}

fn visit_filename_searcher(
    searcher: &Searcher,
    fields: FilenameIndexFields,
    still_open: &dyn Fn() -> bool,
    visit: &mut dyn FnMut(FilenameDoc),
) -> Option<u64> {
    let mut seen: u64 = 0;
    for segment_reader in searcher.segment_readers() {
        let store_reader = segment_reader.get_store_reader(64).ok()?;
        for doc in store_reader.iter::<TantivyDocument>(segment_reader.alive_bitset()) {
            seen += 1;
            if seen % 65_536 == 0 && !still_open() {
                return None;
            }
            let Ok(doc) = doc else { continue };
            let text = |field: Field| doc.get_first(field).and_then(|value| value.as_str());
            let number = |field: Field| doc.get_first(field).and_then(|value| value.as_u64()).unwrap_or(0);
            let Some(path) = text(fields.path) else { continue };
            visit(FilenameDoc {
                path,
                is_dir: text(fields.entry_type) == Some(ENTRY_TYPE_FOLDER),
                size: number(fields.size),
                modified_ms: number(fields.modified_ms),
                created_ms: number(fields.created_ms),
            });
        }
    }
    Some(filename_searcher_fingerprint(searcher))
}

fn doc_text(document: &TantivyDocument, field: Field) -> Option<String> {
    document
        .get_first(field)
        .and_then(|value| value.as_str().map(|text| text.to_string()))
}

fn doc_u64(document: &TantivyDocument, field: Field) -> Option<u64> {
    document.get_first(field).and_then(|value| value.as_u64())
}

/// Collect every text value stored under `field` as a `Vec<String>`. Used for
/// multi-valued tag fields like `sensitive_kinds`. Returns an empty vec when
/// the field is absent (Option::None) so old indexes simply yield no findings.
fn collect_doc_text_values(document: &TantivyDocument, field: Option<Field>) -> Vec<String> {
    let Some(field) = field else { return Vec::new() };
    document
        .get_all(field)
        .filter_map(|value| value.as_str().map(|s| s.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child that never answers is killed at the per-file timeout, and one
    /// that exits is reported as died — neither blocks the caller.
    #[cfg(windows)]
    #[test]
    fn watched_child_times_out_and_detects_death() {
        use std::os::windows::process::CommandExt;
        let spawn = |script: &str| {
            let mut child = Command::new("cmd")
                .args(["/c", script])
                .creation_flags(0x0800_0000)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("cmd runs");
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            WatchedChild::new(child, stdin, stdout)
        };
        // Silent for 30 s (ping's own output goes to nul, so only cmd
        // holds our pipe and the kill closes it).
        let mut hung = spawn("ping -n 30 127.0.0.1 >nul");
        let started = std::time::Instant::now();
        let answer = hung.extract(Path::new(r"C:\x.pdf"), std::time::Duration::from_millis(500));
        assert!(matches!(answer, ExtractAnswer::TimedOut));
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "took {:?}", started.elapsed());
        drop(hung);

        let mut dead = spawn("exit 0");
        let answer = dead.extract(Path::new(r"C:\x.pdf"), std::time::Duration::from_secs(20));
        assert!(matches!(answer, ExtractAnswer::Died));
    }

    #[test]
    fn reconcile_spares_entries_under_offline_roots() {
        let present = std::env::temp_dir().to_string_lossy().into_owned();
        let missing = r"Q:\dash-test-unplugged-drive\Data\".to_string();
        let offline = offline_roots(&[present, missing]);
        assert_eq!(offline, vec![r"q:\dash-test-unplugged-drive\data".to_string()]);
        let root = &offline[0];
        assert!(is_under_root(r"q:\dash-test-unplugged-drive\data", root));
        assert!(is_under_root(r"q:\dash-test-unplugged-drive\data\a\b.txt", root));
        assert!(!is_under_root(r"q:\dash-test-unplugged-drive\database.txt", root));
        assert!(is_under_root(r"c:\x", "c:"));
        assert!(!is_confirmed_gone(&std::env::temp_dir()));
        assert!(is_confirmed_gone(&std::env::temp_dir().join("dash-test-surely-missing-file")));
    }

    #[test]
    fn extractor_timeout_covers_ocr() {
        use super::super::text_extract::OcrChildEnv;
        assert_eq!(OcrChildEnv::default().extract_timeout().as_secs(), 60);
        let ocr = OcrChildEnv::from_options(true, &[], 20, "eng", 0, 0.0, 30);
        assert_eq!(ocr.extract_timeout().as_secs(), 60 + 20 * 30);
    }

    #[test]
    fn sandboxed_extraction_reads_plain_text_and_skips_unknown() {
        let dir = std::env::temp_dir().join(format!("dash-sandbox-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let txt = dir.join("note.txt");
        std::fs::write(&txt, "hello preview").unwrap();
        assert_eq!(extract_text_sandboxed(&txt, 1024).as_deref(), Some("hello preview"));
        let bin = dir.join("tool.exe");
        std::fs::write(&bin, b"MZ\0\0").unwrap();
        assert_eq!(extract_text_sandboxed(&bin, 1024), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn day_ms(days: u64) -> u64 {
        days * 24 * 60 * 60 * 1000
    }

    /// The name index finds what ignore files exclude; the content index doesn't.
    #[test]
    fn name_walker_ignores_ignore_files_content_walker_honours_them() {
        let root = std::env::temp_dir().join(format!("dash-walker-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".ignore"), "secret.txt\n").unwrap();
        fs::write(root.join("secret.txt"), "x").unwrap();
        fs::write(root.join("plain.txt"), "x").unwrap();
        let options: FileSearchIndexOptions =
            serde_json::from_value(serde_json::json!({ "roots": [], "includeHidden": false, "indexContent": true }))
                .unwrap();
        let names = |honour: bool| -> Vec<String> {
            index_walker(&root.display().to_string(), &options, honour)
                .build()
                .flatten()
                .filter_map(|e| e.path().file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect()
        };
        let for_names = names(false);
        let for_content = names(true);
        assert!(for_names.contains(&"secret.txt".to_string()), "{for_names:?}");
        assert!(for_names.contains(&"plain.txt".to_string()));
        assert!(!for_content.contains(&"secret.txt".to_string()), "{for_content:?}");
        assert!(for_content.contains(&"plain.txt".to_string()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn relative_date_phrases_resolve_to_rolling_windows() {
        // "today" — a 24h window.
        let today = build_natural_query_plan("report today")
            .date_filter
            .expect("today → date filter");
        assert_eq!(today.end_ms.unwrap() - today.start_ms.unwrap(), day_ms(1));

        // "last week" — previously unparsed (only "this week" existed).
        let last_week = build_natural_query_plan("invoices from last week")
            .date_filter
            .expect("last week → date filter");
        assert_eq!(
            last_week.end_ms.unwrap() - last_week.start_ms.unwrap(),
            day_ms(7)
        );

        // "this month" — previously unparsed (only "last month" existed).
        let this_month = build_natural_query_plan("photos this month")
            .date_filter
            .expect("this month → date filter");
        assert_eq!(
            this_month.end_ms.unwrap() - this_month.start_ms.unwrap(),
            day_ms(31)
        );

        // "past year".
        let past_year = build_natural_query_plan("notes past year")
            .date_filter
            .expect("past year → date filter");
        assert_eq!(
            past_year.end_ms.unwrap() - past_year.start_ms.unwrap(),
            day_ms(365)
        );
    }

    #[test]
    fn numeric_relative_date_windows_resolve() {
        let week = build_natural_query_plan("logs from last 7 days")
            .date_filter
            .expect("last 7 days → date filter");
        assert_eq!(week.end_ms.unwrap() - week.start_ms.unwrap(), day_ms(7));

        let quarter = build_natural_query_plan("reports past 3 months")
            .date_filter
            .expect("past 3 months → date filter");
        assert_eq!(
            quarter.end_ms.unwrap() - quarter.start_ms.unwrap(),
            day_ms(93)
        );
    }

    #[test]
    fn year_phrases_resolve_to_date_filters() {
        // "during"/"year" + a year still resolve to an exact-year date filter.
        assert!(build_natural_query_plan("taxes during 2023")
            .date_filter
            .is_some());
        // "from"/"in" no longer mean a date — they fall through to a filename
        // keyword, so "movies from 2025" == "movies 2025".
        assert!(build_natural_query_plan("movies from 2025")
            .date_filter
            .is_none());
        assert!(build_natural_query_plan("movies in 2025")
            .date_filter
            .is_none());
        assert!(build_natural_query_plan("files before 2020")
            .date_filter
            .is_some());
        assert!(build_natural_query_plan("docs between 2019 and 2021")
            .date_filter
            .is_some());

        // "created" sets the date-field intent to creation time.
        let created = build_natural_query_plan("created today")
            .date_filter
            .expect("created today → date filter");
        assert!(matches!(created.field, DateFieldIntent::Created));
    }

    #[test]
    fn natural_language_date_field_phrases_resolve() {
        // "<category> created/createdat/created at <year>" → date filter on the
        // Created field, across the bare, one-word, and "at"-connector forms.
        let created_phrases = [
            "movies created 2025",
            "movies created at 2025",
            "movies createdat 2025",
        ];
        for &query in created_phrases.iter() {
            let f = build_natural_query_plan(query)
                .date_filter
                .unwrap_or_else(|| panic!("{} → date filter", query));
            assert!(
                matches!(f.field, DateFieldIntent::Created),
                "{} should map to Created",
                query
            );
        }

        let modified_phrases = [
            "movies modified 2025",
            "movies modified at 2025",
            "movies modifiedat 2025",
        ];
        for &query in modified_phrases.iter() {
            let f = build_natural_query_plan(query)
                .date_filter
                .unwrap_or_else(|| panic!("{} → date filter", query));
            assert!(
                matches!(f.field, DateFieldIntent::Modified),
                "{} should map to Modified",
                query
            );
        }

        // A bare year is the filename release-year default — NOT a date filter.
        assert!(
            build_natural_query_plan("movies 2025").date_filter.is_none(),
            "bare year must stay a filename keyword, not a date filter"
        );
    }

    #[test]
    fn bare_category_keyword_searches_by_format_only() {
        // "movies" with nothing else → movie extensions, no name keyword. The
        // query is extension-only, so the filename index returns every
        // movie-format file (build_result_item enforces the per-hit filter).
        let plan = build_natural_query_plan("movies");
        assert!(
            plan.query_keywords.is_empty(),
            "a bare category must not add a name keyword"
        );
        assert!(plan.query_text.is_empty());
        assert!(plan.extension_filters.iter().any(|e| e == "mp4"));
        assert!(plan.extension_filters.iter().any(|e| e == "mkv"));
        assert!(
            !plan.extension_filters.iter().any(|e| e == "webm"),
            "movies excludes webm"
        );

        let query = build_natural_query_string(
            &plan.query_text,
            &plan.query_keywords,
            &plan.extension_filters,
        )
        .expect("a bare category must still produce an extension-only query");
        assert!(query.contains("extension:mp4"));
        assert!(!query.contains("extension:webm"));

        // "video" is the broad bucket — webm and the rest are included.
        let video = build_natural_query_plan("video");
        assert!(video.extension_filters.iter().any(|e| e == "webm"));
        assert!(video.extension_filters.iter().any(|e| e == "mp4"));
    }

    #[test]
    fn size_phrases_resolve_to_size_filters() {
        let mb = 1024 * 1024;
        let gb = 1024 * mb;

        match build_natural_query_plan("large files").size_filter {
            Some(SizeFilter::Range {
                min_bytes,
                max_bytes,
                ..
            }) => {
                assert_eq!(min_bytes, Some(100 * mb));
                assert_eq!(max_bytes, None);
            }
            other => panic!("large → unexpected {other:?}"),
        }

        match build_natural_query_plan("huge videos").size_filter {
            Some(SizeFilter::Range { min_bytes, .. }) => assert_eq!(min_bytes, Some(gb)),
            other => panic!("huge → unexpected {other:?}"),
        }

        match build_natural_query_plan("bigger than 50mb").size_filter {
            Some(SizeFilter::Range { min_bytes, .. }) => assert_eq!(min_bytes, Some(50 * mb)),
            other => panic!("bigger than 50mb → unexpected {other:?}"),
        }

        match build_natural_query_plan("smaller than 2gb").size_filter {
            Some(SizeFilter::Range { max_bytes, .. }) => assert_eq!(max_bytes, Some(2 * gb)),
            other => panic!("smaller than 2gb → unexpected {other:?}"),
        }

        assert!(matches!(
            build_natural_query_plan("empty folders").size_filter,
            Some(SizeFilter::Empty)
        ));
    }

    #[test]
    fn extensions_resolve_from_dots_and_categories() {
        // Dot-prefixed extension.
        assert!(build_natural_query_plan(".pdf reports")
            .extension_filters
            .contains(&"pdf".to_string()));

        // A category word expands to a family of extensions.
        let images = build_natural_query_plan("vacation images").extension_filters;
        assert!(images.contains(&"png".to_string()));
        assert!(images.contains(&"jpg".to_string()));
    }

    #[test]
    fn field_operators_resolve_explicitly() {
        // `ext:` is a literal extension — not the doc category family.
        let plan = build_natural_query_plan("report ext:docx");
        assert_eq!(plan.extension_filters, vec!["docx".to_string()]);
        assert!(plan.query_keywords.contains(&"report".to_string()));

        // Comma-separated extension list.
        let multi = build_natural_query_plan("ext:jpg,png").extension_filters;
        assert!(multi.contains(&"jpg".to_string()));
        assert!(multi.contains(&"png".to_string()));

        // `type:` takes a category word.
        assert!(build_natural_query_plan("type:image")
            .extension_filters
            .contains(&"png".to_string()));

        // `kind:folder` → entry-type intent.
        assert_eq!(
            build_natural_query_plan("kind:folder")
                .entry_type_filter
                .as_deref(),
            Some(ENTRY_TYPE_FOLDER)
        );

        // A non-operator colon is dropped, not mangled into a field query.
        assert!(build_natural_query_plan("build:debug")
            .query_keywords
            .contains(&"builddebug".to_string()));
    }

    #[test]
    fn entry_type_intent_is_detected() {
        assert_eq!(
            build_natural_query_plan("screenshots folder")
                .entry_type_filter
                .as_deref(),
            Some(ENTRY_TYPE_FOLDER)
        );
        assert_eq!(
            build_natural_query_plan("documents directory")
                .entry_type_filter
                .as_deref(),
            Some(ENTRY_TYPE_FOLDER)
        );
    }

    #[test]
    fn quoted_phrases_are_extracted() {
        let plan = build_natural_query_plan("\"annual report\" budget");
        assert_eq!(plan.exact_phrases, vec!["annual report".to_string()]);
        assert!(plan.query_keywords.contains(&"budget".to_string()));
    }

    #[test]
    fn stopwords_are_dropped_and_synonyms_added() {
        let plan = build_natural_query_plan("show me the invoice");
        assert!(!plan.query_keywords.contains(&"the".to_string()));
        assert!(!plan.query_keywords.contains(&"show".to_string()));
        assert!(plan.query_keywords.contains(&"invoice".to_string()));
        // Synonym expansion via related_natural_terms.
        assert!(plan.query_keywords.contains(&"receipt".to_string()));
    }

    #[test]
    fn combined_query_extracts_every_intent() {
        // The plan's own example: "big pdfs from last week".
        let plan = build_natural_query_plan("big pdfs from last week");
        assert!(plan.extension_filters.contains(&"pdf".to_string()));
        assert!(matches!(
            plan.size_filter,
            Some(SizeFilter::Range {
                min_bytes: Some(_),
                ..
            })
        ));
        let date = plan.date_filter.expect("last week → date filter");
        assert_eq!(date.end_ms.unwrap() - date.start_ms.unwrap(), day_ms(7));
    }

    #[test]
    fn decompounding_splits_long_alphanumeric_keywords() {
        let decompound = build_decompound_query_string(&["spiderman".to_string()])
            .expect("a long keyword should produce decompound clauses");
        assert!(decompound.contains("spider"));
        assert!(decompound.contains("man"));
        // Too short to split.
        assert!(build_decompound_query_string(&["cat".to_string()]).is_none());
    }

    #[test]
    fn pure_parser_helpers_behave() {
        assert_eq!(parse_year_token("2023"), Some(2023));
        assert_eq!(parse_year_token("23"), None);
        assert_eq!(parse_year_token("1850"), None); // before the 1970 floor

        assert_eq!(period_to_days("week"), Some(7));
        assert_eq!(period_to_days("months"), Some(31));
        assert_eq!(period_to_days("fortnight"), None);

        assert!(is_natural_stopword("the"));
        assert!(!is_natural_stopword("report"));

        assert_eq!(split_size_token("100mb", None), Some((100.0, "mb", 1)));
        assert_eq!(split_size_token("100", Some("mb")), Some((100.0, "mb", 2)));
        assert_eq!(split_size_token("mb", None), None);
    }
}
