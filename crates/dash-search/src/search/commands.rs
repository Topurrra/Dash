//! The public commands: status, options, start / stop, open.

use super::*;

/// Per-file throttle sleep for the bulk indexer. `fast` runs flat-out;
/// `balanced`/`quiet` periodically yield the CPU so the rest of the system
/// stays responsive. On battery even `fast` is given a yield so a background
/// index doesn't drain a laptop.
pub(super) fn index_throttle_sleep_ms(performance_mode: &str, on_battery: bool) -> u64 {
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

#[allow(dead_code, reason = "kept for explicit post-rebuild reloads; nothing calls it today")]
pub(super) fn reload_search_reader_if_due(reader: &IndexReader) {
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
pub(super) struct WatcherStrategy {
    pub(super) name: &'static str,
    pub(super) debounce_ms: u64,
    pub(super) max_batch: usize,
}

pub(super) fn watcher_strategy_for_index_size(indexed_files: u64) -> WatcherStrategy {
    if indexed_files >= HUGE_INDEX_WATCHER_THRESHOLD {
        WatcherStrategy { name: "conservative batched watcher", debounce_ms: 5_000, max_batch: 2_048 }
    } else if indexed_files >= LARGE_INDEX_WATCHER_THRESHOLD {
        WatcherStrategy { name: "large-index batched watcher", debounce_ms: 2_000, max_batch: 1_024 }
    } else {
        WatcherStrategy { name: "responsive watcher", debounce_ms: 2_000, max_batch: 256 }
    }
}

pub(super) fn watchable_index_roots(options: &FileSearchIndexOptions) -> Vec<String> {
    options
        .roots
        .iter()
        .filter(|root| {
            let path = Path::new(root);
            path.exists() && should_include_path(path, options) && !is_system_protected_path(path)
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
pub(super) fn is_system_protected_path(path: &Path) -> bool {
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
        for blocked in &protected_roots {
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
pub(super) fn windows_system_drive_token() -> String {
    env::var("SystemDrive")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "C:".to_string())
        .replace('\\', "/")
        .to_lowercase()
        .trim_end_matches('/')
        .to_string()
}

pub(super) fn default_rebuild_interval_hours() -> u32 {
    24
}

pub(super) fn normalize_rebuild_schedule(mut schedule: FileSearchRebuildSchedule) -> FileSearchRebuildSchedule {
    schedule.interval_hours = schedule.interval_hours.clamp(1, 720);
    schedule
}

pub(super) fn unix_now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |value| value.as_millis())
}

/// # Errors
/// Only on a follower, when the owner's call fails.
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
/// (`TerminateProcess`) only takes out the one process, so descendants survive.
/// `taskkill /F /T` terminates the whole tree.
#[cfg(windows)]
pub(super) fn kill_process_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(windows))]
pub(super) fn kill_process_tree(_pid: u32) {}

/// Terminate EVERY running index/extractor worker process tree — the content
/// build, the filename build, and the live watcher. Called on app exit so
/// quitting never leaves orphaned indexer + extractor processes running (and
/// indexing) in the background.
pub fn stop_all_index_workers() {
    SEARCH_BUILD_CANCELLED.store(true, Ordering::Relaxed);
    for slot in [&SEARCH_INDEX_WORKER, &FILENAME_INDEX_WORKER, &SEARCH_WATCHER_WORKER] {
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

/// # Errors
/// When the index folder or saved config can't be read or written, or the
/// engine lock fails.
/// On a follower, also when the owner's call fails.
#[expect(clippy::needless_pass_by_value, reason = "public API shared with other apps")]
pub fn stop_file_search_index_watcher(app: std::sync::Arc<dyn dash_core::Host>) -> Result<(), String> {
    on_owner!("stop_file_search_index_watcher", {});
    let state_dir = search_index_dir(&app)?;
    stop_isolated_watch_worker_for_state(&state_dir);
    if let Some(mut config) = read_search_config(&app)? {
        config.options.watcher_paused = true;
        write_search_config(&app, &config)?;
    }
    let mut guard = SEARCH_ENGINE.lock().map_err(|_| "Search engine lock failed".to_string())?;
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

/// # Errors
/// When the status lock fails.
/// On a follower, also when the owner's call fails.
#[expect(clippy::needless_pass_by_value, reason = "public API shared with other apps")]
pub fn get_file_search_status(app: std::sync::Arc<dyn dash_core::Host>) -> Result<FileSearchStatus, String> {
    on_owner!("get_file_search_status", {});
    // Wave 6 (2026-05-28): `status.elevated` populated removed with MFT.
    hydrate_status_from_index_worker_files(&app);
    hydrate_status_from_filename_worker_files(&app);
    hydrate_worker_process_diagnostics();
    if let Ok(state_dir) = search_index_dir(&app) {
        let content_bytes = directory_size_bytes(&active_search_index_dir_for_state(&state_dir));
        let filename_bytes = directory_size_bytes(&active_filename_index_dir_for_state(&state_dir));
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
    let live_indexed_files =
        SEARCH_ENGINE.lock().ok().and_then(|guard| guard.as_ref().map(|engine| engine.reader.searcher().num_docs()));
    if let Some(live) = live_indexed_files {
        if live > 0 {
            update_status(|status| status.indexed_files = live);
        }
    }
    let status = SEARCH_STATUS.lock().map_err(|_| "Search status lock failed".to_string())?;
    Ok(status.clone())
}

// Wave 6 (2026-05-28): an elevated relaunch command was the UAC-restart
// shim used to enable the MFT fast-path; removed wholesale with the MFT
// engine. The walker never needs admin, so no elevation path remains.

pub(super) fn hydrate_worker_process_diagnostics() {
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

pub(super) fn worker_process_running(worker_slot: &LazyLock<Mutex<Option<SearchIndexWorkerProcess>>>) -> bool {
    let Ok(guard) = worker_slot.lock() else {
        return false;
    };
    let Some(worker) = guard.as_ref() else {
        return false;
    };
    worker.child.lock().is_ok_and(|mut child| child.try_wait().ok().flatten().is_none())
}

/// The saved index options (`None` before the first save).
///
/// # Errors
/// When the saved config can't be read.
/// On a follower, also when the owner's call fails.
#[expect(clippy::needless_pass_by_value, reason = "public API shared with other apps")]
pub fn get_file_search_index_options(
    app: std::sync::Arc<dyn dash_core::Host>,
) -> Result<Option<FileSearchIndexOptions>, String> {
    on_owner!("get_file_search_index_options", {});
    Ok(read_search_config(&app)?.map(|config| config.options))
}

/// # Errors
/// When the saved config can't be read or written, the index folder can't
/// be found, or the engine lock fails.
/// On a follower, also when the owner's call fails.
#[expect(clippy::needless_pass_by_value, reason = "public API shared with other apps")]
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
    let search_coverage_changed =
        saved_config.as_ref().is_some_and(|config| search_coverage_options_changed(&config.options, &options));
    let watcher_enabled_changed =
        saved_config.as_ref().is_some_and(|config| config.options.watcher_enabled != options.watcher_enabled);
    let watcher_pause_changed =
        saved_config.as_ref().is_some_and(|config| config.options.watcher_paused != options.watcher_paused);
    if search_coverage_changed || !options.watcher_enabled {
        let state_dir = search_index_dir(&app)?;
        stop_isolated_watch_worker_for_state(&state_dir);
    }
    let (indexed_files, last_indexed_at_ms) = saved_config.as_ref().map_or_else(
        || SEARCH_STATUS.lock().map_or((0, None), |status| (status.indexed_files, status.last_indexed_at_ms)),
        |config| (config.indexed_files, config.last_indexed_at_ms),
    );

    write_search_config(
        &app,
        &StoredSearchConfig {
            options: options.clone(),
            indexed_files,
            last_indexed_at_ms,
            rebuild_schedule: saved_config.as_ref().map(|config| config.rebuild_schedule.clone()).unwrap_or_default(),
        },
    )?;

    {
        let mut guard = SEARCH_ENGINE.lock().map_err(|_| "Search engine lock failed".to_string())?;
        if let Some(engine) = guard.as_mut() {
            engine.options.roots.clone_from(&options.roots);
            engine.options.include_hidden = options.include_hidden;
            engine.options.index_content = options.index_content;
            engine.options.max_content_kb = options.max_content_kb;
            engine.options.commit_every = options.commit_every;
            engine.options.performance_mode.clone_from(&options.performance_mode);
            engine.options.watcher_enabled = options.watcher_enabled;
            engine.options.watcher_paused = options.watcher_paused;
            engine.options.exclude_folders.clone_from(&options.exclude_folders);
            engine.options.exclude_extensions.clone_from(&options.exclude_extensions);
        }
    }

    update_status(|status| {
        status.roots.clone_from(&options.roots);
        status.filename_roots.clone_from(&options.filename_roots);
        status.include_hidden = options.include_hidden;
        status.index_content = options.index_content;
        status.max_content_kb = options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB);
        status.commit_every = normalize_commit_every(options.commit_every);
        status.watcher_enabled = options.watcher_enabled;
        status.watcher_paused = options.watcher_paused;
        status.exclude_folders.clone_from(&options.exclude_folders);
        status.exclude_extensions.clone_from(&options.exclude_extensions);
        status.diagnostics.performance_mode.clone_from(&options.performance_mode);
        if !options.watcher_enabled {
            status.watching = false;
            status.diagnostics.watcher_worker = "stopped".to_string();
            status.diagnostics.watcher_strategy = "disabled manually".to_string();
            status.diagnostics.last_worker_message = Some("Watcher disabled in search settings".to_string());
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

/// # Errors
/// When the saved config can't be read or written.
/// On a follower, also when the owner's call fails.
#[expect(clippy::needless_pass_by_value, reason = "public API shared with other apps")]
pub fn save_file_search_rebuild_schedule(
    app: std::sync::Arc<dyn dash_core::Host>,
    schedule: FileSearchRebuildSchedule,
) -> Result<FileSearchRebuildSchedule, String> {
    on_owner!("save_file_search_rebuild_schedule", { "schedule": schedule });
    let schedule = normalize_rebuild_schedule(schedule);
    let mut config = read_search_config(&app)?.unwrap_or_else(|| {
        let status = SEARCH_STATUS.lock().ok().map(|value| value.clone()).unwrap_or_default();
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

/// # Errors
/// When the cache lock fails.
pub fn refresh_launch_target_cache() -> Result<(), String> {
    let mut guard = LAUNCH_TARGET_CACHE.lock().map_err(|_| "Launch target cache lock failed".to_string())?;
    *guard = None;
    Ok(())
}

/// # Errors
/// When the launch target cache can't be built or loaded.
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
            let frecency = crate::frecency::snapshot(&app);
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
                took_ms: started_at.elapsed().map_or(0, |d| d.as_millis()),
                cache_built_at_ms: Some(cache.built_at_ms),
                results,
            });
        }
        return Ok(LaunchTargetSearchResult {
            query,
            total_hits: 0,
            returned: 0,
            took_ms: started_at.elapsed().map_or(0, |d| d.as_millis()),
            cache_built_at_ms: None,
            results: Vec::new(),
        });
    }

    let query_normalized = normalize_launcher_text(&query);
    let query_tokens = query_normalized
        .split_whitespace()
        .filter(|token| token.len() >= 2 && !is_natural_stopword(token))
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();

    let cache = tokio::task::spawn_blocking(ensure_launch_target_cache)
        .await
        .map_err(|error| format!("Launch target worker failed: {error}"))??;

    // Load frecency snapshot once — boost is applied per-result below.
    let frecency = crate::frecency::snapshot(&app);

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
        b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.name.cmp(&b.name))
    });

    let total_hits = ranked.len();
    let results = ranked.into_iter().take(limit).collect::<Vec<_>>();
    let took_ms = started_at.elapsed().map_or(0, |d| d.as_millis());

    Ok(LaunchTargetSearchResult {
        query,
        total_hits,
        returned: results.len(),
        took_ms,
        cache_built_at_ms: Some(cache.built_at_ms),
        results,
    })
}

/// # Errors
/// When the path is empty or missing, isn't in the trusted launcher cache,
/// the cache can't be loaded, or the launch fails.
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

    let is_trusted = cache
        .records
        .iter()
        .any(|record| normalize_launch_path_for_match(Path::new(&record.path)) == requested_normalized);
    if !is_trusted {
        return Err("Launch blocked: target is not in trusted launcher cache".to_string());
    }

    open::that_detached(&requested_path).map_err(|error| format!("Failed to launch target: {error}"))?;

    // Record the activation so future searches surface this app sooner.
    // Best-effort — if frecency recording fails (lock contention, disk full),
    // we still consider the launch successful.
    let _ = crate::frecency::record_frecency_launch(app, "app".to_string(), requested.to_string());
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
pub(super) async fn launch_packaged_target(
    app: std::sync::Arc<dyn dash_core::Host>,
    requested: &str,
) -> Result<(), String> {
    let cache = tokio::task::spawn_blocking(ensure_launch_target_cache)
        .await
        .map_err(|error| format!("Launch target worker failed: {error}"))??;

    // Exact match on the stored handle. AUMIDs are opaque and case-insensitive
    // in practice, so compare lowercased — same treatment the cache key gets.
    let requested_key = requested.to_ascii_lowercase();
    let is_trusted = cache.records.iter().any(|record| record.path.to_ascii_lowercase() == requested_key);
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
        let _ = crate::frecency::record_frecency_launch(app, "app".to_string(), requested.to_string());
        Ok(())
    }
}

/// # Errors
/// When the path is empty, fails validation (system locations), isn't a
/// file or folder, or the OS can't open it.
#[expect(clippy::needless_pass_by_value, reason = "public API shared with other apps")]
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

    let metadata = fs::metadata(&target_path).map_err(|error| format!("Cannot inspect search result: {error}"))?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err("Only files and folders can be opened from search results".to_string());
    }

    // The app gets a normal path, not the canonical `\\?\D:\…` form.
    open::that_detached(dash_core::safe_path::without_verbatim_prefix(&target_path))
        .map_err(|error| format!("Failed to open search result: {error}"))?;

    // Record the activation with the appropriate kind so search ranking learns
    // which files and folders the user actually returns to.
    let kind = if metadata.is_dir() { "folder" } else { "file" };
    let _ = crate::frecency::record_frecency_launch(app, kind.to_string(), requested.to_string());
    Ok(())
}

/// # Errors
/// When the launcher task fails or the content index worker can't start.
/// On a follower, also when the owner's call fails.
pub async fn start_file_search_index(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchIndexOptions,
) -> Result<FileSearchBuildResult, String> {
    on_owner!("start_file_search_index", { "options": options });
    let app_for_worker = Arc::clone(&app);
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
///
/// # Errors
/// When the launcher task fails or the content index worker can't start.
/// On a follower, also when the owner's call fails.
pub async fn start_content_search_index(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchIndexOptions,
) -> Result<FileSearchBuildResult, String> {
    on_owner!("start_content_search_index", { "options": options });
    let app_for_worker = Arc::clone(&app);
    tokio::task::spawn_blocking(move || start_isolated_index_worker(app_for_worker, options))
        .await
        .map_err(|error| format!("Search index launcher failed: {error}"))?
}

/// Build the filename index (the File Search tab) via the deterministic
/// folder walker. Triggers only the filename indexer.
///
/// # Errors
/// When the launcher task fails or the filename index worker can't start.
/// On a follower, also when the owner's call fails.
pub async fn start_filename_search_index(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchIndexOptions,
) -> Result<bool, String> {
    on_owner!("start_filename_search_index", { "options": options });
    let app_for_worker = Arc::clone(&app);
    tokio::task::spawn_blocking(move || start_isolated_filename_index_worker(&app_for_worker, options, true))
        .await
        .map_err(|error| format!("Filename index launcher failed: {error}"))?
}

/// # Errors
/// When the search worker fails or the search itself does.
/// On a follower, also when the owner's call fails.
pub async fn search_local_files(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchQueryOptions,
) -> Result<FileSearchQueryResult, String> {
    on_owner!("search_local_files", { "options": options });
    tokio::task::spawn_blocking(move || search_local_files_inner(&app, &options))
        .await
        .map_err(|error| format!("Search worker failed: {error}"))?
}

/// # Errors
/// When the search worker fails or the search itself does.
/// On a follower, also when the owner's call fails.
pub async fn search_file_contents(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: FileSearchQueryOptions,
) -> Result<ContentSearchQueryResult, String> {
    on_owner!("search_file_contents", { "options": options });
    tokio::task::spawn_blocking(move || search_file_contents_inner(&app, &options))
        .await
        .map_err(|error| format!("Content search worker failed: {error}"))?
}
