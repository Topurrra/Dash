//! Loading the engines, the scheduler, index options.

use super::*;

pub(super) fn ensure_engine_loaded(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<(), String> {
    if SEARCH_STATUS.lock().is_ok_and(|status| status.indexing) {
        return Err(
            "Search index is rebuilding in an isolated worker. Search will be available when it finishes.".to_string()
        );
    }

    {
        let guard = SEARCH_ENGINE.lock().map_err(|_| "Search engine lock failed".to_string())?;
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
    let (config, recovered_orphan) = if let Some(config) = read_search_config(app)? {
        (config, false)
    } else {
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
    };

    recover_search_index_dirs(app)?;
    let index_dir = active_search_index_dir(app)?;
    if !index_dir.exists() {
        if config.indexed_files > 0 {
            update_status(|status| {
                status.last_error = Some("Search index storage was upgraded. Rebuild the file index once.".to_string());
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
                    "The search index was corrupted and has been removed. Rebuild the file index once.".to_string()
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
    let indexed_files = if live_indexed_files > 0 { live_indexed_files } else { config.indexed_files };

    let watcher_active = match start_isolated_watch_worker(app, config.options.clone(), indexed_files) {
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
        let mut guard = SEARCH_ENGINE.lock().map_err(|_| "Search engine lock failed".to_string())?;
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
        status.roots.clone_from(&config.options.roots);
        status.filename_roots.clone_from(&config.options.filename_roots);
        status.include_hidden = config.options.include_hidden;
        status.index_content = config.options.index_content;
        status.max_content_kb = config.options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB);
        status.commit_every = normalize_commit_every(config.options.commit_every);
        status.watcher_enabled = config.options.watcher_enabled;
        status.watcher_paused = config.options.watcher_paused;
        status.exclude_folders.clone_from(&config.options.exclude_folders);
        status.exclude_extensions.clone_from(&config.options.exclude_extensions);
        status.rebuild_schedule = config.rebuild_schedule.clone();
        status.indexed_files = indexed_files;
        status.last_indexed_at_ms = config.last_indexed_at_ms;
        status.last_error = None;
        status.diagnostics.performance_mode = normalize_index_performance_mode(&config.options.performance_mode);
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
        // Stepped down (another process owns the engines): its schedules run.
        if dash_core::link::is_follower() {
            continue;
        }
        if let Err(error) = run_scheduled_file_search_rebuild(&app) {
            update_status(|status| {
                status.last_error = Some(error);
            });
        }
        // Periodic trigger: keep the standalone filename index fresh.
        let _ = run_scheduled_filename_index_rebuild(&app);
        restart_stopped_watcher(&app);
    });
}

/// A watcher worker that died (killed, or an error it couldn't get past)
/// left the index frozen until the next start: start it again. At most
/// once per scheduler tick, so one that keeps failing can't spin.
pub(super) fn restart_stopped_watcher(app: &std::sync::Arc<dyn dash_core::Host>) {
    if worker_process_running(&SEARCH_WATCHER_WORKER) || SEARCH_STATUS.lock().map_or(true, |s| s.indexing) {
        return;
    }
    let Ok(Some(config)) = read_search_config(app) else { return };
    if !config.options.watcher_enabled || config.options.watcher_paused {
        return;
    }
    let indexed_files = SEARCH_STATUS.lock().map_or(0, |s| s.indexed_files).max(config.indexed_files);
    if let Err(error) = start_isolated_watch_worker(app, config.options, indexed_files) {
        update_status(|status| status.last_error = Some(error));
    }
}

pub(super) fn run_scheduled_file_search_rebuild(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<(), String> {
    let Some(mut config) = read_search_config(app)? else {
        return Ok(());
    };
    let schedule = normalize_rebuild_schedule(config.rebuild_schedule.clone());
    if !schedule.enabled || config.options.roots.is_empty() {
        return Ok(());
    }
    if SEARCH_STATUS.lock().is_ok_and(|status| status.indexing) {
        return Ok(());
    }

    let now_ms = unix_now_ms();
    let interval_ms = u128::from(schedule.interval_hours).saturating_mul(60 * 60 * 1000);
    let reference_ms = schedule.last_scheduled_rebuild_at_ms.or(config.last_indexed_at_ms).unwrap_or(0);
    if reference_ms > 0 && now_ms.saturating_sub(reference_ms) < interval_ms {
        return Ok(());
    }

    config.rebuild_schedule.last_scheduled_rebuild_at_ms = Some(now_ms);
    write_search_config(app, &config)?;
    update_status(|status| {
        status.rebuild_schedule = config.rebuild_schedule.clone();
    });

    let _ = start_isolated_index_worker(Arc::clone(app), config.options.clone(), false)?;
    Ok(())
}

/// Wall-clock ms of the last successful filename-index build, read from the
/// schema sentinel's modification time. The sentinel is rewritten at the end
/// of every successful filename build, so its mtime is a sound "last built"
/// proxy. `None` when no build has completed or the mtime is unreadable.
pub(super) fn filename_index_last_built_ms(state_dir: &Path) -> Option<u128> {
    let path = filename_schema_version_path_for_state(state_dir);
    let modified = fs::metadata(&path).ok()?.modified().ok()?;
    modified.duration_since(UNIX_EPOCH).ok().map(|value| value.as_millis())
}

/// Scheduler tick for the standalone filename indexer (Phase 3, step 7b). Runs
/// the metadata-only filename build when the index is missing or older than
/// `FILENAME_INDEX_REFRESH_INTERVAL_MS`. Called once at launch and once per
/// scheduler poll; `start_isolated_filename_index_worker` itself skips when a
/// content build is running or a filename build already is.
pub(super) fn run_scheduled_filename_index_rebuild(app: &std::sync::Arc<dyn dash_core::Host>) -> Result<(), String> {
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
            .is_none_or(|built| now_ms.saturating_sub(built) >= FILENAME_INDEX_REFRESH_INTERVAL_MS);
    if !stale {
        return Ok(());
    }

    // In-session anti-thrash: don't re-trigger — or retry a failed build —
    // more than once per refresh interval.
    let last_attempt = FILENAME_INDEX_LAST_REBUILD_MS.load(Ordering::Relaxed);
    if last_attempt > 0 && now_ms.saturating_sub(u128::from(last_attempt)) < FILENAME_INDEX_REFRESH_INTERVAL_MS {
        return Ok(());
    }

    FILENAME_INDEX_LAST_REBUILD_MS.store(u64::try_from(now_ms).unwrap_or(u64::MAX), Ordering::Relaxed);
    let _ = start_isolated_filename_index_worker(app, config.options.clone(), false)?;
    Ok(())
}

pub(super) fn status_to_index_options(status: &FileSearchStatus) -> FileSearchIndexOptions {
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

pub(super) fn normalize_commit_every(value: Option<usize>) -> usize {
    match value {
        Some(raw) if raw >= MIN_COMMIT_EVERY => raw.min(250_000),
        _ => DEFAULT_COMMIT_EVERY,
    }
}

pub(super) fn normalize_index_options(mut options: FileSearchIndexOptions) -> Result<FileSearchIndexOptions, String> {
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

pub(super) fn search_coverage_options_changed(
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
