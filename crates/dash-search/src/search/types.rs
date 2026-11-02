//! The public data types (options, status, results) and their defaults.

use super::*;

#[derive(Clone)]
pub(super) struct SearchIndexWorkerProcess {
    pub(super) child: Arc<Mutex<Child>>,
    pub(super) status_path: PathBuf,
    pub(super) cancel_path: PathBuf,
}

#[expect(clippy::struct_excessive_bools, reason = "serialized settings and status: each bool is an independent flag")]
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

pub(super) fn default_content_indexing_enabled() -> bool {
    true
}

pub(super) fn default_ocr_max_pages() -> u32 {
    20
}

pub(super) fn default_ocr_langs() -> String {
    "eng+rus+kat".to_string()
}

pub(super) fn default_ocr_min_image_dim() -> u32 {
    400
}

pub(super) fn default_ocr_max_aspect_ratio() -> f32 {
    5.0
}

pub(super) fn default_ocr_timeout_secs() -> u32 {
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
        Self { enabled: false, interval_hours: default_rebuild_interval_hours(), last_scheduled_rebuild_at_ms: None }
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
    /// Sensitive-content kinds detected at index time. See `FileSearchResultItem`.
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
    /// Sensitive-content findings detected at index time (e.g. "`aws_access_key`",
    /// "`private_key_pem`"). Empty when nothing was detected or content wasn't
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

#[expect(clippy::struct_excessive_bools, reason = "serialized settings and status: each bool is an independent flag")]
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

#[expect(clippy::struct_excessive_bools, reason = "serialized settings and status: each bool is an independent flag")]
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
pub(super) struct SearchFields {
    pub(super) path: Field,
    pub(super) path_exact: Field,
    pub(super) file_name: Field,
    pub(super) entry_type: Option<Field>,
    pub(super) extension: Field,
    pub(super) content: Field,
    /// Multi-valued tag field for findings from the `sensitive_scan` module.
    /// Optional so indexes built before this field existed still load cleanly.
    pub(super) sensitive_kinds: Option<Field>,
    pub(super) size: Field,
    pub(super) modified_ms: Field,
    pub(super) created_ms: Option<Field>,
}

/// The name/path/metadata field subset that `build_result_item` reads — never
/// `content`. Both the combined index (`SearchFields`) and the standalone
/// filename index (`FilenameIndexFields`) project into this, so one ranking
/// path serves both (Phase 3, step 7c — Option B query routing).
#[derive(Clone, Copy)]
pub(super) struct ResultDocFields {
    pub(super) path: Field,
    pub(super) file_name: Field,
    pub(super) extension: Field,
    pub(super) size: Field,
    pub(super) modified_ms: Field,
    pub(super) entry_type: Option<Field>,
    pub(super) created_ms: Option<Field>,
    pub(super) sensitive_kinds: Option<Field>,
}

impl SearchFields {
    pub(super) fn result_doc_fields(&self) -> ResultDocFields {
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
    pub(super) fn result_doc_fields(&self) -> ResultDocFields {
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
pub(super) struct FilenameIndexHandle {
    pub(super) index: Index,
    pub(super) reader: IndexReader,
    pub(super) fields: FilenameIndexFields,
}

pub(super) struct SearchEngine {
    pub(super) index: Index,
    pub(super) reader: IndexReader,
    pub(super) writer: Option<IndexWriter>,
    pub(super) watcher: Option<RecommendedWatcher>,
    pub(super) fields: SearchFields,
    pub(super) options: FileSearchIndexOptions,
}

#[derive(Debug, Clone, Default)]
pub(super) struct NaturalQueryPlan {
    pub(super) query_text: String,
    pub(super) query_keywords: Vec<String>,
    pub(super) extension_filters: Vec<String>,
    pub(super) entry_type_filter: Option<String>,
    pub(super) date_filter: Option<DateFilter>,
    pub(super) size_filter: Option<SizeFilter>,
    pub(super) exact_phrases: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum DateFieldIntent {
    Modified,
    Created,
}

#[derive(Debug, Clone)]
pub(super) struct DateFilter {
    pub(super) field: DateFieldIntent,
    pub(super) start_ms: Option<u64>,
    pub(super) end_ms: Option<u64>,
    pub(super) label: String,
}

#[derive(Debug, Clone)]
pub(super) enum SizeFilter {
    Empty,
    Range { min_bytes: Option<u64>, max_bytes: Option<u64>, label: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StoredSearchConfig {
    pub(super) options: FileSearchIndexOptions,
    pub(super) indexed_files: u64,
    pub(super) last_indexed_at_ms: Option<u128>,
    #[serde(default)]
    pub(super) rebuild_schedule: FileSearchRebuildSchedule,
}

#[expect(clippy::struct_excessive_bools, reason = "serialized settings and status: each bool is an independent flag")]
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct IndexWorkerStatusFile {
    pub(super) running: bool,
    pub(super) finished: bool,
    pub(super) success: bool,
    pub(super) canceled: bool,
    pub(super) indexed_files: u64,
    pub(super) scanned_entries: u64,
    pub(super) skipped_files: u64,
    pub(super) message: String,
    pub(super) updated_at_ms: u128,
    pub(super) error: Option<String>,
    // Indexing Phase 2 / Task 4.1 (2026-06-18): the run was interrupted (a
    // low-memory pool death or a user cancel) but its partial index was
    // committed and promoted to active — it is a valid, RESUMABLE index, not a
    // failure. `success=true, partial=true`. Distinct from a full success
    // (`success=true, partial=false`) and from a failure (`success=false`).
    #[serde(default)]
    pub(super) partial: bool,
}

impl IndexWorkerStatusFile {
    pub(super) fn progress(indexed_files: u64, scanned_entries: u64, skipped_files: u64, message: String) -> Self {
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

    pub(super) fn finished(
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
    pub(super) fn into_partial(mut self) -> Self {
        self.partial = true;
        self
    }
}

#[derive(Debug, Clone)]
pub(super) struct LaunchTargetRecord {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) path: String,
    pub(super) kind: String,
    pub(super) source: String,
    pub(super) normalized_name: String,
    pub(super) normalized_path: String,
}

#[derive(Debug, Clone)]
pub(super) struct LaunchTargetCache {
    pub(super) built_at_ms: u128,
    pub(super) records: Vec<LaunchTargetRecord>,
}

pub(super) fn default_exclude_folders() -> Vec<String> {
    DEFAULT_EXCLUDE_FOLDERS.iter().map(|value| (*value).to_string()).collect()
}

pub(super) fn default_exclude_extensions() -> Vec<String> {
    DEFAULT_EXCLUDE_EXTENSIONS.iter().map(|value| (*value).to_string()).collect()
}

pub(super) fn default_index_performance_mode() -> String {
    "balanced".to_string()
}

pub(super) fn default_watcher_enabled() -> bool {
    true
}

pub(super) fn normalize_index_performance_mode(value: &str) -> String {
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
pub(super) fn is_on_battery() -> bool {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: GetSystemPowerStatus fills the caller-owned struct in place.
    let ok = unsafe { GetSystemPowerStatus(&raw mut status) }.is_ok();
    // ACLineStatus: 0 = on battery, 1 = on AC, 255 = unknown.
    ok && status.ACLineStatus == 0
}

#[cfg(not(windows))]
pub(super) fn is_on_battery() -> bool {
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
pub(super) fn available_ram_bytes() -> u64 {
    use std::mem::size_of;
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut ms = MEMORYSTATUSEX {
        dwLength: u32::try_from(size_of::<MEMORYSTATUSEX>()).expect("MEMORYSTATUSEX is 64 bytes"),
        ..Default::default()
    };
    // SAFETY: `GlobalMemoryStatusEx` writes into a caller-owned struct.
    if unsafe { GlobalMemoryStatusEx(&raw mut ms) }.is_ok() {
        ms.ullAvailPhys
    } else {
        4 * 1024 * 1024 * 1024 // conservative fallback
    }
}

#[cfg(not(windows))]
pub(super) fn available_ram_bytes() -> u64 {
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
pub(super) fn clamp_pool_for_available_ram(cpu_child_count: usize) -> (usize, usize) {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    let available = available_ram_bytes();
    if available < (3 * GB) / 2 {
        // Lite: < 1.5 GB free
        (1, 256 * 1024 * 1024)
    } else if available < 3 * GB {
        // Standard: 1.5–3 GB free
        (cpu_child_count.min(2), 384 * 1024 * 1024)
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
