//! Index folders, status and config files, recovery and swaps.

use super::*;

/// Active / previous / staging directories for the filename index — siblings
/// of the content index's directories inside the same state directory.
#[allow(dead_code)]
pub(super) fn active_filename_index_dir_for_state(state_dir: &Path) -> PathBuf {
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
pub fn enumerate_indexed_file_sizes(state_dir: &Path) -> Option<std::collections::HashMap<String, (String, u64, u64)>> {
    let dir = active_filename_index_dir_for_state(state_dir);
    if !dir.exists() {
        return None;
    }
    let index = Index::open_in_dir(&dir).ok()?;
    let schema = index.schema();
    let fields = extract_filename_index_fields(&schema).ok()?;
    let reader: IndexReader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into().ok()?;
    let searcher = reader.searcher();

    let mut out: std::collections::HashMap<String, (String, u64, u64)> = std::collections::HashMap::with_capacity(8192);
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
pub(super) fn previous_filename_index_dir_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(FILENAME_INDEX_PREVIOUS_DIR)
}

#[allow(dead_code)]
pub(super) fn staging_filename_index_dir_for_state(state_dir: &Path, stamp_ms: u128) -> PathBuf {
    state_dir.join(format!("{FILENAME_STAGING_INDEX_PREFIX}{stamp_ms}"))
}

/// Schema-version sentinel for the filename index — mirrors the content
/// index's sentinel but in its own file, so the two indexes version
/// independently. Reuses `SchemaVersionSentinel`.
#[allow(dead_code)]
pub(super) fn filename_schema_version_path_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(FILENAME_SCHEMA_VERSION_FILE)
}

#[allow(dead_code)]
pub(super) fn write_filename_schema_version_sentinel(state_dir: &Path) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(&SchemaVersionSentinel { version: FILENAME_SCHEMA_VERSION })
        .map_err(|error| format!("Cannot serialize filename schema sentinel: {error}"))?;
    fs::write(filename_schema_version_path_for_state(state_dir), bytes)
        .map_err(|error| format!("Cannot write filename schema sentinel: {error}"))
}

#[allow(dead_code)]
pub(super) fn read_filename_schema_version_sentinel(state_dir: &Path) -> Option<u32> {
    let bytes = fs::read(filename_schema_version_path_for_state(state_dir)).ok()?;
    serde_json::from_slice::<SchemaVersionSentinel>(&bytes).ok().map(|sentinel| sentinel.version)
}

pub(super) fn search_index_dir(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<PathBuf, String> {
    let base = app.data_dir().map_err(|error| format!("Cannot resolve app data directory: {error}"))?;
    Ok(base.join(SEARCH_INDEX_DIR))
}

pub(super) fn active_search_index_dir(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<PathBuf, String> {
    Ok(active_search_index_dir_for_state(&search_index_dir(app)?))
}

/// Total on-disk size, in bytes, of every file under `dir` (recursively). Used
/// to report how much disk each Tantivy index occupies. A missing or unreadable
/// directory — e.g. an index that was never built — reports 0.
pub(super) fn directory_size_bytes(dir: &Path) -> u64 {
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

pub(super) fn active_search_index_dir_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(ACTIVE_SEARCH_INDEX_DIR)
}

pub(super) fn previous_search_index_dir_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(PREVIOUS_SEARCH_INDEX_DIR)
}

pub(super) fn staging_search_index_dir_for_state(state_dir: &Path, stamp_ms: u128) -> PathBuf {
    state_dir.join(format!("{STAGING_SEARCH_INDEX_PREFIX}{stamp_ms}"))
}

pub(super) fn search_config_path_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(SEARCH_CONFIG_FILE)
}

pub(super) fn write_index_worker_status(path: &Path, status: &IndexWorkerStatusFile) -> Result<(), String> {
    let bytes =
        serde_json::to_vec_pretty(status).map_err(|error| format!("Cannot serialize index worker status: {error}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("Cannot create index worker status directory: {error}"))?;
    }
    let temp_path = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().and_then(|value| value.to_str()).unwrap_or(INDEX_WORKER_STATUS_FILE)
    ));
    fs::write(&temp_path, bytes).map_err(|error| format!("Cannot write index worker status: {error}"))?;
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&temp_path, path).map_err(|error| format!("Cannot publish index worker status: {error}"))
}

pub(super) fn read_index_worker_status(path: &Path) -> Result<Option<IndexWorkerStatusFile>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read(path).map_err(|error| format!("Cannot read index worker status: {error}"))?;
    match serde_json::from_slice::<IndexWorkerStatusFile>(&raw) {
        Ok(status) => Ok(Some(status)),
        Err(_) => Ok(None),
    }
}

pub(super) fn request_stale_worker_stop(status_path: &Path, cancel_path: &Path) {
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
pub(super) fn remove_staging_index_dirs(state_dir: &Path, prefix: &str) {
    let Ok(entries) = fs::read_dir(state_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_staging = path.is_dir()
            && path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with(prefix));
        if is_staging {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

pub(super) fn recover_search_index_dirs(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<(), String> {
    recover_search_index_dirs_for_state(&search_index_dir(app)?)
}

/// Recover a half-finished content-index swap from a prior crash and sweep
/// abandoned content `index-build-*` staging dirs. Skips entirely while a
/// content worker is alive: that worker owns a live `index-build-*` staging
/// dir, and sweeping it would kill the worker's tantivy writer mid-build. In
/// the worker subprocess this guard is inert (the global slot is `None`), so
/// the worker still recovers at its own startup before creating its dir.
pub(super) fn recover_content_index_dirs_for_state(state_dir: &Path) -> Result<(), String> {
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
pub(super) fn recover_filename_index_dirs_for_state(state_dir: &Path) -> Result<(), String> {
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
pub(super) fn recover_search_index_dirs_for_state(state_dir: &Path) -> Result<(), String> {
    recover_content_index_dirs_for_state(state_dir)?;
    recover_filename_index_dirs_for_state(state_dir)?;
    Ok(())
}

/// Atomically swap a freshly-built staging index into its active slot:
/// active → previous (backup), staging → active, then drop the backup. A
/// failed final rename rolls the backup back. Generic over the directory
/// triple so the content and filename indexes both reuse it.
pub(super) fn swap_staging_index_dir(staging_dir: &Path, active_dir: &Path, previous_dir: &Path) -> Result<(), String> {
    if !staging_dir.exists() {
        return Err("Staging search index was not created.".to_string());
    }

    if let Some(parent) = active_dir.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("Cannot create search index parent directory: {error}"))?;
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

pub(super) fn swap_in_staging_search_index_for_state(state_dir: &Path, staging_dir: &Path) -> Result<(), String> {
    swap_staging_index_dir(
        staging_dir,
        &active_search_index_dir_for_state(state_dir),
        &previous_search_index_dir_for_state(state_dir),
    )
}

pub(super) fn swap_in_staging_filename_index_for_state(state_dir: &Path, staging_dir: &Path) -> Result<(), String> {
    swap_staging_index_dir(
        staging_dir,
        &active_filename_index_dir_for_state(state_dir),
        &previous_filename_index_dir_for_state(state_dir),
    )
}

#[derive(Serialize, Deserialize)]
pub(super) struct SchemaVersionSentinel {
    pub(super) version: u32,
}

pub(super) fn schema_version_path_for_state(state_dir: &Path) -> PathBuf {
    state_dir.join(SCHEMA_VERSION_FILE)
}

/// Record the schema version the active index was built with. A missing
/// sentinel is read back as the baseline version, so a failed write only
/// risks one spurious rebuild after a future schema bump.
pub(super) fn write_schema_version_sentinel(state_dir: &Path) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(&SchemaVersionSentinel { version: SEARCH_SCHEMA_VERSION })
        .map_err(|error| format!("Cannot serialize schema version sentinel: {error}"))?;
    fs::write(schema_version_path_for_state(state_dir), bytes)
        .map_err(|error| format!("Cannot write schema version sentinel: {error}"))
}

/// Read the on-disk schema version. `None` when the sentinel is absent or
/// unreadable — callers treat that as the pre-sentinel baseline (version 1).
pub(super) fn read_schema_version_sentinel(state_dir: &Path) -> Option<u32> {
    let bytes = fs::read(schema_version_path_for_state(state_dir)).ok()?;
    serde_json::from_slice::<SchemaVersionSentinel>(&bytes).ok().map(|sentinel| sentinel.version)
}

/// Startup guard: if either search index was built against an older Tantivy
/// schema, wipe it so the next rebuild recreates it cleanly. Without this, a
/// schema change in an update makes `Index::open` fail or field
/// extraction reject the index — leaving search silently broken. The content
/// and filename indexes version independently, so each is checked separately.
pub(super) fn enforce_search_schema_version(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<(), String> {
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
pub(super) fn enforce_content_index_schema_version(state_dir: &Path) {
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
pub(super) fn enforce_filename_index_schema_version(state_dir: &Path) {
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

pub(super) fn write_search_config(
    app: &std::sync::Arc<dyn dash_core::Host>,
    config: &StoredSearchConfig,
) -> Result<(), String> {
    write_search_config_for_state(&search_index_dir(app)?, config)
}

pub(super) fn read_search_config(
    app: &std::sync::Arc<dyn dash_core::Host>,
) -> Result<Option<StoredSearchConfig>, String> {
    read_search_config_for_state(&search_index_dir(app)?)
}

pub(super) fn write_search_config_for_state(state_dir: &Path, config: &StoredSearchConfig) -> Result<(), String> {
    let db_path = local_db::database_path_for_dir(state_dir);
    local_db::write_json(&db_path, SEARCH_CONFIG_FILE, config)
}

pub(super) fn read_search_config_for_state(state_dir: &Path) -> Result<Option<StoredSearchConfig>, String> {
    let db_path = local_db::database_path_for_dir(state_dir);
    if let Some(mut parsed) = local_db::read_json::<StoredSearchConfig>(&db_path, SEARCH_CONFIG_FILE)? {
        parsed.options.exclude_folders = normalize_exclude_folders(&parsed.options.exclude_folders);
        parsed.options.exclude_extensions = normalize_exclude_extensions(&parsed.options.exclude_extensions);
        parsed.options.performance_mode = normalize_index_performance_mode(&parsed.options.performance_mode);
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
    let raw = fs::read_to_string(&path).map_err(|error| format!("Cannot read search config: {error}"))?;
    let mut parsed: StoredSearchConfig =
        serde_json::from_str(&raw).map_err(|error| format!("Cannot parse search config: {error}"))?;
    parsed.options.exclude_folders = normalize_exclude_folders(&parsed.options.exclude_folders);
    parsed.options.exclude_extensions = normalize_exclude_extensions(&parsed.options.exclude_extensions);
    parsed.options.performance_mode = normalize_index_performance_mode(&parsed.options.performance_mode);
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

pub(super) fn hydrate_status_from_saved_config(app: &std::sync::Arc<dyn dash_core::Host>) {
    let has_live_engine = SEARCH_ENGINE.lock().map(|guard| guard.is_some()).unwrap_or(false);
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
        status.max_content_kb = config.options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB);
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
        status.diagnostics.performance_mode = normalize_index_performance_mode(&config.options.performance_mode);
    });
}

pub(super) fn hydrate_status_from_index_worker_files(app: &std::sync::Arc<dyn dash_core::Host>) {
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
    let options = options_from_worker
        .or_else(|| read_search_config_for_state(&state_dir).ok().flatten().map(|config| config.options));
    let Some(options) = options else {
        return;
    };

    apply_index_worker_status(app, &options, &status_file);
}

pub(super) fn hydrate_status_from_watcher_worker_files(app: &std::sync::Arc<dyn dash_core::Host>) {
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
    let options = options_from_worker
        .or_else(|| read_search_config_for_state(&state_dir).ok().flatten().map(|config| config.options));
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
pub(super) fn hydrate_status_from_filename_worker_files(app: &std::sync::Arc<dyn dash_core::Host>) {
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
    let building = SEARCH_STATUS.lock().map(|status| status.filename_indexing).unwrap_or(false);
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
