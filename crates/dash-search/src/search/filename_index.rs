//! The filename index: its documents, reconcile, and watching.

use super::*;

/// Delete a single document from the filename index by its exact path.
/// Used by the notify watcher (all platforms) when a file disappears on
/// disk — the watcher fires a remove event, we drop the doc, the index
/// stays consistent without a full rebuild. Originally lived inside the
/// MFT block but is engine-agnostic; preserved here as the walker still
/// needs it.
pub(super) fn delete_filename_doc(writer: &IndexWriter, fields: &FilenameIndexFields, path: &str) {
    let _ = writer.delete_term(tantivy::Term::from_field_text(fields.path_exact, path));
}

/// Build the filename index. After the bulk build the worker stays alive
/// as the notify watcher (step 9e), keeping the index live without a
/// rescan.
pub(super) fn build_filename_index_best_effort(
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
pub(super) fn upsert_filename_document(
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
        path.extension().and_then(|value| value.to_str()).unwrap_or_default().to_lowercase()
    } else {
        String::new()
    };
    // `.ki` notes are found + shown by their human title (frontmatter / first
    // heading), not the on-disk slug like `untitled-3.ki` the user never sees.
    let file_name = if is_file && is_ki_path(path) {
        note_display_title(path)
    } else {
        path.file_name().and_then(|value| value.to_str()).unwrap_or_default().to_string()
    };
    let parent_path = path.parent().map(|parent| parent.to_string_lossy().to_string()).unwrap_or_default();
    let entry_type = if is_dir { ENTRY_TYPE_FOLDER } else { ENTRY_TYPE_FILE };
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX));
    let created_ms = metadata
        .created()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(modified_ms, |value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX));

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
    writer.add_document(document).map(|_| ()).map_err(|error| format!("Cannot index filename update: {error}"))
}

/// Fold one notify event into filename-index edits. A removed (or now-excluded)
/// path drops its doc; a created/modified path is re-`upsert`ed. Returns the
/// (updated, deleted) doc counts. The filename twin of `apply_watch_event_to_writer`.
pub(super) fn apply_filename_watch_event(
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
                // A folder takes everything under it along.
                delete_path_and_children(writer, fields.path_exact, path);
                deleted += 1;
            }
        }
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any | EventKind::Other => {
            for path in &event.paths {
                match path.try_exists() {
                    Ok(true) => {}
                    Ok(false) => {
                        // Gone, or renamed away: with whatever was under it.
                        delete_path_and_children(writer, fields.path_exact, path);
                        deleted += 1;
                        continue;
                    }
                    Err(_) => continue,
                }
                if !path.is_file() && !path.is_dir() {
                    continue;
                }
                if !should_include_path(path, options) {
                    delete_path_and_children(writer, fields.path_exact, path);
                    deleted += 1;
                    continue;
                }
                if upsert_filename_document(path, writer, fields).is_ok() {
                    updated += 1;
                }
                // A folder that appeared (created, moved or renamed in)
                // arrives as one event: its contents are new names too.
                if path.is_dir() {
                    for entry in index_walker(&path.to_string_lossy(), options, false).build().flatten().skip(1) {
                        if should_include_path(entry.path(), options)
                            && upsert_filename_document(entry.path(), writer, fields).is_ok()
                        {
                            updated += 1;
                        }
                    }
                }
            }
        }
        EventKind::Access(_) => {}
    }
    (updated, deleted)
}

/// Apply one debounced batch of notify events to the active filename index.
/// Opens a single short-lived writer for the whole batch — the filename twin of
/// `apply_watch_batch_to_index`.
pub(super) fn apply_filename_watch_batch(
    active_dir: &Path,
    options: &FileSearchIndexOptions,
    pending: Vec<notify::Result<Event>>,
) -> Result<(u64, u64), String> {
    let events: Vec<Event> = pending
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|event| !matches!(event.kind, EventKind::Access(_)))
        .collect();
    if events.is_empty() {
        return Ok((0, 0));
    }

    let directory =
        MmapDirectory::open(active_dir).map_err(|error| format!("Cannot open filename index for watching: {error}"))?;
    let index = Index::open(directory).map_err(|error| format!("Cannot open filename index for watching: {error}"))?;
    let schema = index.schema();
    let fields = extract_filename_index_fields(&schema)?;
    let mut writer = index
        .writer_with_num_threads(1, WRITER_MEMORY_BUDGET_BYTES_IDLE)
        .map_err(|error| format!("Cannot open filename watcher writer: {error}"))?;

    let mut updated = 0_u64;
    let mut deleted = 0_u64;
    for event in &events {
        let (event_updated, event_deleted) = apply_filename_watch_event(event, &writer, &fields, options);
        updated += event_updated;
        deleted += event_deleted;
    }

    if updated == 0 && deleted == 0 {
        return Ok((0, 0));
    }
    writer.commit().map_err(|error| format!("Cannot commit filename watcher update: {error}"))?;
    writer.wait_merging_threads().map_err(|error| format!("Cannot finish filename watcher merge: {error}"))?;
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
/// (see `FileSearchStatus`). `standard_filters` must come first: it also turns
/// hidden-skipping on, which used to override the setting.
///
/// Excluded and system folders are never entered: rejecting only their
/// files still walked every one of them (`node_modules`, `AppData`, ...).
pub(super) fn index_walker(root: &str, options: &FileSearchIndexOptions, honour_ignore_files: bool) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder.standard_filters(honour_ignore_files);
    builder.hidden(!options.include_hidden);
    builder.follow_links(false);
    let exclude_folders = options.exclude_folders.clone();
    builder.filter_entry(move |entry| {
        entry.depth() == 0
            || !entry.file_type().is_some_and(|kind| kind.is_dir())
            || !(is_system_protected_path(entry.path()) || has_excluded_folder(entry.path(), &exclude_folders))
    });
    builder
}

/// The path is known not to exist (an error checking — offline share,
/// unplugged drive, access denied — is not "gone").
pub(super) fn is_confirmed_gone(path: &Path) -> bool {
    matches!(path.try_exists(), Ok(false))
}

/// Roots that can't be confirmed present right now (unplugged drive, offline
/// share), lower-cased without a trailing separator. Reconcile leaves what is
/// indexed under them alone instead of treating all of it as deleted.
pub(super) fn offline_roots(roots: &[String]) -> Vec<String> {
    roots
        .iter()
        .filter(|root| !matches!(Path::new(String::as_str(root)).try_exists(), Ok(true)))
        .map(|root| root.trim_end_matches(['\\', '/']).to_lowercase())
        .collect()
}

/// Whether lower-cased `path_lower` is `root_lower` (from `offline_roots`) or
/// inside it.
pub(super) fn is_under_root(path_lower: &str, root_lower: &str) -> bool {
    path_lower.strip_prefix(root_lower).is_some_and(|rest| rest.is_empty() || rest.starts_with(['\\', '/']))
}

pub(super) fn reconcile_filename_index(
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
    let Ok(reader): Result<IndexReader, _> = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()
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
            let is_file = file_type.is_some_and(|value| value.is_file());
            let is_dir = file_type.is_some_and(|value| value.is_dir());
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
                        .map_or(0, |value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX));
                    disk_modified != *modified_ms || meta.len() != *size
                }
            };
            if needs_update {
                events.push(Ok(Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path.to_path_buf())));
            }
        }
    }

    // Docs whose file is gone from disk → deletions the watcher missed. Delete
    // by the case-preserved stored path so the exact `path_exact` term matches.
    for (path_lower, (real_path, _, _)) in &indexed {
        if seen.contains(path_lower) || offline.iter().any(|root| is_under_root(path_lower, root)) {
            continue;
        }
        events.push(Ok(Event::new(EventKind::Remove(RemoveKind::Any)).add_path(PathBuf::from(real_path))));
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
pub(super) fn watch_filename_index(
    state_dir: &Path,
    options: &FileSearchIndexOptions,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    // Minimum gap between batch applies — events accumulate in the channel
    // during the wait so each apply opens the index once for many changes.
    const MIN_APPLY_INTERVAL_MS: u128 = 5_000;
    // Periodic full reconciliation, plus an immediate one when the loop gap
    // implies the process was suspended (sleep/hibernate dropped events).
    const RECONCILE_INTERVAL_MS: u128 = 6 * 60 * 60 * 1_000;
    const SLEEP_GAP_THRESHOLD_MS: u128 = 2 * 60 * 1_000;
    // After a failed batch or lost events, a reconciliation this soon heals
    // what was missed (not at once: a persistent error shouldn't spin).
    const RECONCILE_SOON_MS: u128 = 60 * 1_000;
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
        // Kept: "rescan" notices (Windows' event buffer overflowed, so changes
        // were lost) arrive as `Other` events.
        if let Ok(ref event) = result {
            use notify::event::ModifyKind;
            match &event.kind {
                _ if event.need_rescan() => {}
                EventKind::Access(_)
                | EventKind::Modify(ModifyKind::Metadata(_) | ModifyKind::Other)
                | EventKind::Any
                | EventKind::Other => return,
                _ => {}
            }
        }
        let _ = tx.send(result);
    })
    .map_err(|error| format!("Cannot start filename index watcher: {error}"))?;

    let mut watched_roots = 0_u64;
    for root in &watch_roots {
        if watcher.watch(Path::new(root), RecursiveMode::Recursive).is_ok() {
            watched_roots += 1;
        }
    }
    if watched_roots == 0 {
        return Ok(());
    }

    let strategy = WatcherStrategy { name: "filename watcher", debounce_ms: 2_000, max_batch: 1_024 };
    let mut last_apply_ms: u128 = 0;
    let mut last_reconcile_ms: u128 = unix_now_ms();
    let reconcile_soon = |now_ms: u128| now_ms.saturating_sub(RECONCILE_INTERVAL_MS - RECONCILE_SOON_MS);
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
        if resumed_from_suspend || now_ms.saturating_sub(last_reconcile_ms) >= RECONCILE_INTERVAL_MS {
            last_reconcile_ms = now_ms;
            match reconcile_filename_index(&active_dir, options) {
                Ok((reconciled_updates, reconciled_deletes)) => {
                    updated_total += reconciled_updates;
                    deleted_total += reconciled_deletes;
                }
                Err(_) => last_reconcile_ms = reconcile_soon(now_ms),
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
                    format!("Filename index live — {updated_total} updated, {deleted_total} removed"),
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

        // A watcher error or a rescan notice means events were lost.
        if pending.iter().any(|event| event.as_ref().map_or(true, Event::need_rescan)) {
            last_reconcile_ms = last_reconcile_ms.min(reconcile_soon(unix_now_ms()));
        }

        // A failed apply (the index busy, a file locked, ...) used to stop
        // watching until the next rebuild, hours away. Now a reconciliation
        // soon picks up what the batch would have changed.
        let applied = apply_filename_watch_batch(&active_dir, options, pending);
        last_apply_ms = unix_now_ms();
        let Ok((updated_batch, deleted_batch)) = applied else {
            last_reconcile_ms = last_reconcile_ms.min(reconcile_soon(last_apply_ms));
            continue;
        };
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
