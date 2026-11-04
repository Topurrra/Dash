//! Index builds inside the worker processes.

use super::*;

pub(super) fn run_index_worker_index_mode(
    state_dir: &Path,
    options_path: &Path,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    let result = run_index_worker_index_mode_inner(state_dir, options_path, status_path, cancel_path);
    if let Err(error) = &result {
        let _ = write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::finished(false, false, 0, 0, 0, error.clone(), Some(error.clone())),
        );
    }
    result
}

pub(super) fn run_index_worker_index_mode_inner(
    state_dir: &Path,
    options_path: &Path,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    let options_bytes = fs::read(options_path).map_err(|error| format!("Cannot read index worker options: {error}"))?;
    let options: FileSearchIndexOptions = serde_json::from_slice(&options_bytes)
        .map_err(|error| format!("Cannot parse index worker options: {error}"))?;
    let options = normalize_index_options(options)?;

    fs::create_dir_all(state_dir).map_err(|error| format!("Cannot create search index state directory: {error}"))?;
    recover_content_index_dirs_for_state(state_dir)?;
    write_index_worker_status(
        status_path,
        &IndexWorkerStatusFile::progress(0, 0, 0, "Preparing search index".to_string()),
    )?;

    // An interrupted build with the same settings is continued.
    let staging_dir = if let Some(resumable) = take_resumable_staging(state_dir, &resume_key(&options)) {
        resumable
    } else {
        let staging_dir = staging_search_index_dir_for_state(state_dir, unix_now_ms());
        if staging_dir.exists() {
            fs::remove_dir_all(&staging_dir).map_err(|error| format!("Cannot reset staging search index: {error}"))?;
        }
        fs::create_dir_all(&staging_dir).map_err(|error| format!("Cannot create staging search index: {error}"))?;
        staging_dir
    };

    let result = build_index_in_worker(state_dir, &staging_dir, &options, status_path, cancel_path, false);
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging_dir);
    }
    result
}

/// Subprocess entry point for the scheduled content refresh (`update`):
/// updates the live index in place (`build_index_in_worker` with
/// `in_place`) when it was fully built with today's extraction settings and
/// version; otherwise — no index yet, OCR or the text limit changed, a new
/// Dash version — a full build as in `index` mode.
pub(super) fn run_index_worker_update_mode(
    state_dir: &Path,
    options_path: &Path,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    let options_bytes = fs::read(options_path).map_err(|error| format!("Cannot read index worker options: {error}"))?;
    let options: FileSearchIndexOptions = serde_json::from_slice(&options_bytes)
        .map_err(|error| format!("Cannot parse index worker options: {error}"))?;
    let options = normalize_index_options(options)?;
    recover_content_index_dirs_for_state(state_dir)?;
    let active_dir = active_search_index_dir_for_state(state_dir);
    let same_extraction = fs::read_to_string(state_dir.join(EXTRACTION_SIGNATURE_FILE))
        .is_ok_and(|saved| saved == extraction_signature(&options));
    if !active_dir.exists() || !same_extraction {
        return run_index_worker_index_mode(state_dir, options_path, status_path, cancel_path);
    }
    write_index_worker_status(
        status_path,
        &IndexWorkerStatusFile::progress(0, 0, 0, "Updating search index".to_string()),
    )?;
    let result = build_index_in_worker(state_dir, &active_dir, &options, status_path, cancel_path, true);
    if let Err(error) = &result {
        let _ = write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::finished(false, false, 0, 0, 0, error.clone(), Some(error.clone())),
        );
    }
    result
}

/// Subprocess entry point for the standalone filename indexer (`--worker
/// filename-index`). Twin of `run_index_worker_index_mode`: on any error it
/// writes a failed status file so the parent monitor sees the failure.
pub(super) fn run_index_worker_filename_mode(
    state_dir: &Path,
    options_path: &Path,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    let result = run_index_worker_filename_mode_inner(state_dir, options_path, status_path, cancel_path);
    if let Err(error) = &result {
        let _ = write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::finished(false, false, 0, 0, 0, error.clone(), Some(error.clone())),
        );
    }
    result
}

pub(super) fn run_index_worker_filename_mode_inner(
    state_dir: &Path,
    options_path: &Path,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    let options_bytes =
        fs::read(options_path).map_err(|error| format!("Cannot read filename index worker options: {error}"))?;
    let options: FileSearchIndexOptions = serde_json::from_slice(&options_bytes)
        .map_err(|error| format!("Cannot parse filename index worker options: {error}"))?;
    let options = normalize_index_options(options)?;

    fs::create_dir_all(state_dir).map_err(|error| format!("Cannot create search index state directory: {error}"))?;
    recover_filename_index_dirs_for_state(state_dir)?;
    write_index_worker_status(
        status_path,
        &IndexWorkerStatusFile::progress(0, 0, 0, "Preparing filename index".to_string()),
    )?;

    let staging_dir = staging_filename_index_dir_for_state(state_dir, unix_now_ms());
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir).map_err(|error| format!("Cannot reset staging filename index: {error}"))?;
    }
    fs::create_dir_all(&staging_dir).map_err(|error| format!("Cannot create staging filename index: {error}"))?;

    let result = build_filename_index_best_effort(state_dir, &staging_dir, &options, status_path, cancel_path);
    // The walker renames the staging dir into place; the MFT path leaves it
    // unused. Either way, drop any leftover.
    let _ = fs::remove_dir_all(&staging_dir);
    result
}

/// One entry in the "incremental rebuild" cache. Used to skip the expensive
/// content-extraction step when a file is unchanged since the last index.
pub(super) struct ExistingDocLookup {
    pub(super) modified_ms: u64,
    pub(super) size: u64,
    pub(super) address: tantivy::DocAddress,
}

/// Snapshot of the previous index used during a staging rebuild. The reader
/// is kept alive for the duration of the build so per-file content lookups
/// can resolve via `doc_address` without re-extracting from disk.
pub(super) struct ExistingIndexCache {
    pub(super) searcher: tantivy::Searcher,
    pub(super) fields: SearchFields,
    /// Lowercase path → existing doc info. Case-insensitive because Windows
    /// paths are case-insensitive and the file walker may emit different casing.
    pub(super) map: HashMap<String, ExistingDocLookup>,
    /// Reuse documents that have no text too. Set when the previous build
    /// extracted the same way (`extraction_signature`): an unchanged file
    /// that had no text then (a scanned PDF without OCR, an encrypted file,
    /// one that crashed or timed out an extractor) has none now, and
    /// extracting it again on every rebuild only costs time.
    pub(super) keep_empty: bool,
}

/// What decides a file's extracted text besides the file itself: Dash's
/// version (its extractors) and the extraction settings. Saved after each
/// successful content build (`EXTRACTION_SIGNATURE_FILE`).
pub(super) fn extraction_signature(options: &FileSearchIndexOptions) -> String {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "maxContentKb": options.max_content_kb,
        "ocr": options.ocr_on_index_enabled,
        "ocrFolders": options.ocr_on_index_folders,
        "ocrMaxPages": options.ocr_max_pages_per_file,
        "ocrLangs": options.ocr_langs,
        "ocrMinImageDim": options.ocr_min_image_dim,
        "ocrMaxAspectRatio": options.ocr_max_aspect_ratio,
        "ocrTimeoutSecs": options.ocr_per_file_timeout_secs,
    })
    .to_string()
}

/// The signature the last successful content build used.
pub(super) const EXTRACTION_SIGNATURE_FILE: &str = "content-extraction.json";

/// What an interrupted content build must match to be continued: the same
/// extraction and the same options (roots, exclusions, ...).
pub(super) fn resume_key(options: &FileSearchIndexOptions) -> String {
    format!("{}{}", extraction_signature(options), serde_json::to_string(options).unwrap_or_default())
}

/// How often a content build saves its progress (commits its staging index).
const CHECKPOINT_INTERVAL: Duration = Duration::from_mins(3);

/// Open the previous active index (if any) and build a path → doc lookup map
/// so the rebuild can reuse cached content for unchanged files.
///
/// Returns None if there's no previous index, the index is corrupt, or the
/// schema doesn't match — in any of those cases the caller falls back to
/// full content extraction for every file.
pub(super) fn load_existing_index_for_reuse(active_dir: &Path) -> Option<ExistingIndexCache> {
    use tantivy::DocSet as _;
    if !active_dir.exists() {
        return None;
    }
    let directory = MmapDirectory::open(active_dir).ok()?;
    let index = Index::open(directory).ok()?;
    let schema = index.schema();
    let fields = extract_fields(&schema).ok()?;
    // A small doc-store cache: each reused file's text is read once, roughly
    // in index order, and blocks hold whole documents (up to 1 MB of text) —
    // tantivy's default of 100 blocks per segment held gigabytes during a
    // rebuild.
    let reader =
        index.reader_builder().reload_policy(ReloadPolicy::Manual).doc_store_cache_num_blocks(8).try_into().ok()?;
    let searcher: tantivy::Searcher = reader.searcher();

    // Paths come from `path_exact`'s term dictionary (one raw term per
    // document) and size / mtime from their fast-field columns, so nothing
    // is read from the document store: it holds each file's extracted text
    // (up to 1 MB), which used to be decompressed for every document just to
    // learn its path. Text is read later, only for files that are reused.
    let mut map: HashMap<String, ExistingDocLookup> = HashMap::with_capacity(8192);
    for (segment_ord, segment_reader) in (0u32..).zip(searcher.segment_readers()) {
        let Ok(paths) = segment_reader.inverted_index(fields.path_exact) else { continue };
        let fast = segment_reader.fast_fields();
        let (Ok(modified), Ok(sizes)) = (fast.u64("modified_ms"), fast.u64("size")) else { continue };
        let alive = segment_reader.alive_bitset();
        let Ok(mut terms) = paths.terms().stream() else { continue };
        while terms.advance() {
            let Ok(path) = std::str::from_utf8(terms.key()) else { continue };
            let Ok(mut docs) =
                paths.read_postings_from_terminfo(terms.value(), tantivy::schema::IndexRecordOption::Basic)
            else {
                continue;
            };
            let mut doc_id = docs.doc();
            while doc_id != tantivy::TERMINATED {
                if alive.is_none_or(|bits| bits.is_alive(doc_id)) {
                    map.insert(
                        path.to_lowercase(),
                        ExistingDocLookup {
                            modified_ms: modified.first(doc_id).unwrap_or(0),
                            size: sizes.first(doc_id).unwrap_or(0),
                            address: tantivy::DocAddress::new(segment_ord, doc_id),
                        },
                    );
                }
                doc_id = docs.advance();
            }
        }
    }

    Some(ExistingIndexCache { searcher, fields, map, keep_empty: false })
}

/// Returns cached content if the previous index has this exact file unchanged
/// (matching mtime + size) AND that previous entry actually stored non-empty
/// content. Returning None means "no cache hit — please extract fresh."
pub(super) fn reuse_cached_content(
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
    // Empty content is extracted again unless the last build extracted the
    // same way (`keep_empty`): changed settings or a new version may now
    // find text there (e.g. OCR just turned on).
    if content.is_empty() && !cache.keep_empty {
        return None;
    }
    Some(content)
}

/// The metadata columns of one content-index document — everything except the
/// extracted `content`. Built at the call site so the same metadata can pair
/// with cached text (reuse-cache hit) or freshly-extracted text (pool result).
pub(super) struct ContentDocMeta<'a> {
    pub(super) file_path: &'a str,
    pub(super) file_name: &'a str,
    pub(super) extension: &'a str,
    pub(super) entry_type: &'a str,
    pub(super) size: u64,
    pub(super) modified_ms: u64,
    pub(super) created_ms: u64,
}

/// Build one content-index document and add it to the writer. Shared by the
/// discovery walk (reuse-cache hits) and the extractor-pool indexer thread so
/// both paths produce byte-identical documents. `content` is empty for a file
/// indexed by name only. `add_document` is `&self`, so this is safe to call
/// from the walk's worker threads and the indexer thread concurrently.
pub(super) fn add_content_document(
    writer: &IndexWriter,
    fields: SearchFields,
    meta: &ContentDocMeta,
    content: &str,
) -> Result<(), String> {
    // Sensitive-content scan: only when text was extracted. Detection is
    // local and only the finding *labels* are stored, never the secrets.
    let sensitive_kinds = if content.is_empty() { Vec::new() } else { crate::sensitive_scan::scan_text(content) };
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
    writer.add_document(document).map(|_| ()).map_err(|error| format!("Cannot add document to index: {error}"))
}

/// Emit a periodic progress status and apply the per-mode CPU-yield throttle.
/// Shared by the discovery walk and the indexer thread; the shared
/// `last_progress` mutex ensures only one status write happens per interval.
pub(super) fn report_index_progress(
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
#[expect(
    clippy::too_many_arguments,
    reason = "thread body: each argument is a separate shared counter or handle from the build"
)]
pub(super) fn run_content_indexer(
    result_rx: &Mutex<std::sync::mpsc::Receiver<ExtractResult>>,
    writer: &std::sync::RwLock<IndexWriter>,
    fields: SearchFields,
    indexed_files: &std::sync::atomic::AtomicU64,
    scanned: &std::sync::atomic::AtomicU64,
    skipped: &std::sync::atomic::AtomicU64,
    first_error: &Mutex<Option<String>>,
    last_progress: &Mutex<u128>,
    should_stop: &AtomicBool,
    status_path: &Path,
    throttle_ms: u64,
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
        let Ok(metadata) = fs::metadata(&result.path) else {
            // The file vanished between discovery and extraction.
            skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
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
        let entry = Path::new(&result.path);
        let file_name = entry.file_name().and_then(|value| value.to_str()).unwrap_or_default().to_string();
        let extension = entry.extension().and_then(|value| value.to_str()).unwrap_or_default().to_lowercase();
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
        let added = writer
            .read()
            .map_err(|_| "Search writer lock was poisoned".to_string())
            .and_then(|writer| add_content_document(&writer, fields, &meta, &content));
        if let Err(error) = added {
            if let Ok(mut guard) = first_error.lock() {
                if guard.is_none() {
                    *guard = Some(error);
                }
            }
            should_stop.store(true, Ordering::Relaxed);
            continue;
        }
        // Semantic vectors aren't made here: the watcher embeds documents in
        // the background (`search::semantic`), so builds stay fast.
        let new_indexed = indexed_files.fetch_add(1, Ordering::Relaxed) + 1;
        report_index_progress(status_path, new_indexed, scanned, skipped, last_progress, throttle_ms);
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
pub(super) fn content_index_unchanged(
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
            let Ok(entry) = entry_result else {
                return false;
            };
            let path = entry.path();
            let is_file = entry.file_type().is_some_and(|value| value.is_file());
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
            let Ok(metadata) = fs::metadata(path) else {
                return false;
            };
            let size = metadata.len();
            let modified_ms = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX));
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
    matched == u64::try_from(cache.map.len()).unwrap_or(u64::MAX)
}

/// Builds the content index into `staging_dir` and swaps it in — or, with
/// `in_place`, updates the live index itself (`staging_dir` is then the
/// active dir): unchanged files are left alone, changed ones replaced, and
/// entries whose file is gone removed. No copy, no swap; this is what the
/// scheduled refresh does (`run_index_worker_update_mode`).
pub(super) fn build_index_in_worker(
    state_dir: &Path,
    staging_dir: &Path,
    options: &FileSearchIndexOptions,
    status_path: &Path,
    cancel_path: &Path,
    in_place: bool,
) -> Result<(), String> {
    // A build that fails or stops early drops its staging copy; in place,
    // that dir is the live index, which must never be deleted.
    let discard_staging = || {
        if !in_place {
            let _ = fs::remove_dir_all(staging_dir);
        }
    };
    // ── Content indexing opt-in (Phase 2 / Task 4.3) ──────────────────────
    // When the user has turned "search inside files" off, the content worker
    // no-ops: the filename index (built by a separate worker) still gives full
    // name search. Default is on; the frontend defaults it OFF on first run on
    // low-RAM (Lite) profiles where a content index can't converge.
    if !options.content_indexing_enabled {
        discard_staging();
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
    let existing_cache = load_existing_index_for_reuse(&active_dir).map(|mut cache| {
        cache.keep_empty = fs::read_to_string(state_dir.join(EXTRACTION_SIGNATURE_FILE))
            .is_ok_and(|saved| saved == extraction_signature(options));
        cache
    });
    // Only when extraction is also as before (`keep_empty`: same settings and
    // version): unchanged files with OCR just turned on still need a build.
    if let Some(cache) = existing_cache.as_ref().filter(|cache| !in_place && cache.keep_empty) {
        if content_index_unchanged(options, cache, cancel_path) {
            let doc_count = u64::try_from(cache.map.len()).unwrap_or(u64::MAX);
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
            discard_staging();
            write_index_worker_status(
                status_path,
                &IndexWorkerStatusFile::finished(
                    true,
                    false,
                    doc_count,
                    doc_count,
                    0,
                    format!("Index already up to date — {doc_count} files unchanged (rebuild skipped)"),
                    None,
                ),
            )?;
            return Ok(());
        }
    }

    // What an interrupted build already saved in this staging dir (see
    // `take_resumable_staging`): those files, unchanged since, are skipped.
    let resumed = (!in_place && staging_dir.join(RESUME_MARKER_FILE).exists())
        .then(|| load_existing_index_for_reuse(staging_dir))
        .flatten();

    let schema = build_search_schema();
    let directory = MmapDirectory::open(staging_dir)
        .map_err(|error| format!("Cannot open staging search index directory: {error}"))?;
    let index = Index::open_or_create(directory, schema.clone())
        .map_err(|error| format!("Cannot create staging search index: {error}"))?;
    let fields = extract_fields(&schema)?;
    // No `mut` — the writer is shared (Arc + RwLock) by the parallel walk, the
    // indexer threads and the checkpoint thread; we reclaim sole ownership
    // afterward via Arc::try_unwrap for the final commit.
    let (writer_threads, writer_budget) = index_writer_sizing();
    // In place, the watcher that was just told to stop may still hold the
    // live index's writer lock for a moment: wait for it (up to ~10 s).
    let mut attempts = 0;
    let writer = loop {
        match index.writer_with_num_threads(writer_threads, writer_budget) {
            Err(tantivy::TantivyError::LockFailure(..)) if in_place && attempts < 20 => {
                attempts += 1;
                thread::sleep(Duration::from_millis(500));
            }
            result => break result.map_err(|error| format!("Cannot create search writer: {error}"))?,
        }
    };

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
                format!("Loaded previous index ({} files) — reusing extraction for unchanged files", cache.map.len()),
            ),
        )?;
    }

    let max_content_bytes = usize::try_from(options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB))
        .unwrap_or(usize::MAX)
        .saturating_mul(1024)
        .max(8 * 1024);
    // commit_every is intentionally unused in the parallel build — see the
    // comment below where we walk in parallel. Tantivy auto-flushes when its
    // memory budget fills, so we rely on a single final commit instead.
    let _ = normalize_commit_every(options.commit_every);

    // Shared state for the parallel walker. Counters use atomics so workers
    // never block each other. Errors and progress timing use small mutexes
    // because they're touched rarely.
    // Documents are added under the read lock (any number at once); the
    // checkpoint thread takes the write lock to commit.
    let writer = Arc::new(std::sync::RwLock::new(writer));
    let existing_cache = Arc::new(existing_cache);
    let resumed = Arc::new(resumed);
    // In place: the indexed files the walk met (lower-cased), so the ones it
    // didn't — deleted, moved, now excluded — can be removed afterwards.
    let seen: Option<Arc<Mutex<HashSet<String>>>> = in_place.then(|| Arc::new(Mutex::new(HashSet::new())));
    let unchanged_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));
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
    let cpu_extractor_count =
        index_walker_threads_with_ocr(&performance_mode, on_battery, options.ocr_on_index_enabled);
    // RAM-aware clamping (Phase 1 of the HardwareProfile design). On machines
    // with < 1.5 GB available RAM, running N × 512 MB children causes the OS
    // to kill them all under memory pressure, exhausting the respawn budget
    // and silently stopping indexing. `clamp_pool_for_available_ram` keeps the
    // count and per-child memory cap within what `GlobalMemoryStatusEx` says
    // the machine can sustain right now.
    let (extractor_child_count, extractor_memory_cap) = clamp_pool_for_available_ram(cpu_extractor_count);
    // Wave 8 / task #88 (2026-05-28): propagate OCR-on-Index opt-in to the
    // extractor children via env vars on each spawn Command. The struct is
    // empty (no env vars) when the user hasn't enabled OCR, so the spawn
    // path is unchanged for the default-off case.
    let ocr_env = crate::text_extract::OcrChildEnv::from_options(
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
        Some(crate::ocr_cache::OcrCacheCtx::new(state_dir, &options.ocr_langs))
    } else {
        None
    };
    let (pool, path_tx, result_rx) = ExtractorPool::new(
        extractor_child_count,
        max_content_bytes,
        extractor_memory_cap,
        &performance_mode,
        &should_stop_walker,
        &ocr_env,
        cache_ctx.as_ref(),
    )
    .map_err(|error| {
        discard_staging();
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
                );
            })
        })
        .collect();

    // Checkpoints: every `CHECKPOINT_INTERVAL` the staging index is committed
    // and marked resumable, so a quit, crash or ownership change loses at
    // most a few minutes of work instead of the whole build.
    let checkpoint_done = Arc::new(AtomicBool::new(false));
    let checkpoint = {
        let writer = Arc::clone(&writer);
        let done = Arc::clone(&checkpoint_done);
        let should_stop = Arc::clone(&should_stop_walker);
        let first_error = Arc::clone(&first_error);
        let marker = staging_dir.join(RESUME_MARKER_FILE);
        let key = resume_key(options);
        thread::spawn(move || {
            let mut last = std::time::Instant::now();
            while !done.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(500));
                if last.elapsed() < CHECKPOINT_INTERVAL || should_stop.load(Ordering::Relaxed) {
                    continue;
                }
                last = std::time::Instant::now();
                let committed =
                    writer.write().map_err(|_| "Search writer lock was poisoned".to_string()).and_then(|mut writer| {
                        writer.commit().map_err(|error| format!("Cannot save search index progress: {error}"))
                    });
                match committed {
                    Ok(_) => {
                        if !in_place {
                            let _ = fs::write(&marker, &key);
                        }
                    }
                    Err(error) => {
                        if let Ok(mut guard) = first_error.lock() {
                            guard.get_or_insert(error);
                        }
                        should_stop.store(true, Ordering::Relaxed);
                    }
                }
            }
        })
    };

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
            let resumed = Arc::clone(&resumed);
            let seen = seen.clone();
            let unchanged_atomic = Arc::clone(&unchanged_atomic);
            let indexed_files_atomic = Arc::clone(&indexed_files_atomic);
            let scanned_entries_atomic = Arc::clone(&scanned_entries_atomic);
            let skipped_files_atomic = Arc::clone(&skipped_files_atomic);
            let reused_count_atomic = Arc::clone(&reused_count_atomic);
            let first_error = Arc::clone(&first_error);
            let last_progress = Arc::clone(&last_progress_at_ms_shared);
            let should_stop = Arc::clone(&should_stop_walker);
            let pool_died = Arc::clone(&pool_died);
            let options = options_for_filter.clone();
            let status_path = status_path_owned.clone();
            let cancel_path = cancel_path_owned.clone();
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

                if current_scanned > u64::try_from(MAX_INDEXED_FILES).unwrap_or(u64::MAX) {
                    if let Ok(mut guard) = first_error.lock() {
                        if guard.is_none() {
                            *guard =
                                Some(format!("Index scan exceeded {MAX_INDEXED_FILES} entries. Choose fewer folders."));
                        }
                    }
                    should_stop.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }

                let Ok(entry) = entry_result else {
                    skipped_files_atomic.fetch_add(1, Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                };

                let path = entry.path();
                let file_type = entry.file_type();
                let is_file = file_type.is_some_and(|value| value.is_file());
                let is_dir = file_type.is_some_and(|value| value.is_dir());
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

                let Ok(metadata) = fs::metadata(path) else {
                    skipped_files_atomic.fetch_add(1, Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                };

                let file_path = path.to_string_lossy().to_string();
                let extension = if is_file {
                    path.extension().and_then(|value| value.to_str()).unwrap_or_default().to_lowercase()
                } else {
                    String::new()
                };
                let file_name = path.file_name().and_then(|value| value.to_str()).unwrap_or_default().to_string();
                let entry_type = if is_dir { ENTRY_TYPE_FOLDER } else { ENTRY_TYPE_FILE };
                let size = metadata.len();
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

                // Updating in place: an unchanged file is already in the index
                // and is left alone; a changed one's old document goes.
                if let Some(seen) = &seen {
                    let key = file_path.to_lowercase();
                    let current = existing_cache.as_ref().as_ref().and_then(|cache| cache.map.get(&key));
                    if let Some(current) = current {
                        let unchanged = current.modified_ms == modified_ms && current.size == size;
                        if let Ok(mut seen) = seen.lock() {
                            seen.insert(key);
                        }
                        if unchanged {
                            unchanged_atomic.fetch_add(1, Ordering::Relaxed);
                            let new_indexed = indexed_files_atomic.fetch_add(1, Ordering::Relaxed) + 1;
                            report_index_progress(
                                &status_path,
                                new_indexed,
                                &scanned_entries_atomic,
                                &skipped_files_atomic,
                                &last_progress,
                                throttle_ms,
                            );
                            return ignore::WalkState::Continue;
                        }
                        if let Ok(writer) = writer.read() {
                            writer.delete_term(Term::from_field_text(fields.path_exact, &file_path));
                        }
                    }
                }

                // Resuming: the interrupted build already indexed this file.
                // Unchanged since, it is done; changed, its old document goes.
                if let Some(saved) =
                    resumed.as_ref().as_ref().and_then(|saved| saved.map.get(&file_path.to_lowercase()))
                {
                    if saved.modified_ms == modified_ms && saved.size == size {
                        let new_indexed = indexed_files_atomic.fetch_add(1, Ordering::Relaxed) + 1;
                        report_index_progress(
                            &status_path,
                            new_indexed,
                            &scanned_entries_atomic,
                            &skipped_files_atomic,
                            &last_progress,
                            throttle_ms,
                        );
                        return ignore::WalkState::Continue;
                    }
                    if let Ok(writer) = writer.read() {
                        writer.delete_term(Term::from_field_text(fields.path_exact, &file_path));
                    }
                }

                // Reuse-cache check — if this document's (mtime, size) match
                // the previous index, its already-extracted content is reused
                // and it is indexed here. On a miss, plain text and code — a
                // bounded, guarded file read — are read here, and everything
                // else goes to the out-of-process extractor pool.
                let cached = existing_cache
                    .as_ref()
                    .as_ref()
                    .and_then(|cache| reuse_cached_content(cache, &file_path.to_lowercase(), modified_ms, size));
                let reused = cached.is_some();
                let read_here =
                    (cached.is_none() && crate::text_extract::is_plain_text_extension(&extension)).then(|| {
                        crate::text_extract::extract_text_for_indexing(path, max_content_bytes).unwrap_or_default()
                    });
                if let Some(content) = cached.or(read_here) {
                    if reused {
                        reused_count_atomic.fetch_add(1, Ordering::Relaxed);
                    }
                    let meta = ContentDocMeta {
                        file_path: &file_path,
                        file_name: &file_name,
                        extension: &extension,
                        entry_type,
                        size,
                        modified_ms,
                        created_ms,
                    };
                    let added = writer
                        .read()
                        .map_err(|_| "Search writer lock was poisoned".to_string())
                        .and_then(|writer| add_content_document(&writer, fields, &meta, &content));
                    if let Err(error) = added {
                        if let Ok(mut guard) = first_error.lock() {
                            if guard.is_none() {
                                *guard = Some(error);
                            }
                        }
                        should_stop.store(true, Ordering::Relaxed);
                        return ignore::WalkState::Quit;
                    }
                    let new_indexed = indexed_files_atomic.fetch_add(1, Ordering::Relaxed) + 1;
                    report_index_progress(
                        &status_path,
                        new_indexed,
                        &scanned_entries_atomic,
                        &skipped_files_atomic,
                        &last_progress,
                        throttle_ms,
                    );
                } else {
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
                                if should_stop.load(Ordering::Relaxed) || cancel_path.exists() {
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
        && !indexer_handles.iter().all(std::thread::JoinHandle::is_finished)
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
        #[cfg_attr(not(windows), expect(clippy::drop_non_drop, reason = "only the Windows pool owns a Job Object"))]
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
    // No more checkpoints: what follows commits, discards or swaps the
    // staging index itself.
    checkpoint_done.store(true, Ordering::Relaxed);
    if checkpoint.join().is_err() {
        indexer_panicked = true;
    }
    if indexer_panicked {
        discard_staging();
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
            discard_staging();
            return Err(err.clone());
        }
    }

    // Regression guard: only promote a partial index when it is at least as
    // complete as the index it would replace. On a rebuild of an already-
    // complete index, an early interruption would otherwise swap a smaller
    // index over a full one — so there we keep the existing index intact and
    // discard the partial staging (the pre-Phase-2 behavior).
    let prior_active_count = match &*existing_cache {
        Some(cache) => u64::try_from(cache.map.len()).unwrap_or(u64::MAX),
        None => 0,
    };
    if !in_place && resumable_interrupt && indexed_files < prior_active_count {
        discard_staging();
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
        .map_err(|_| "Internal: search writer still held by a worker".to_string())?
        .into_inner()
        .map_err(|_| "Search writer lock was poisoned".to_string())?;

    // In place, after a complete walk: entries the walk didn't meet are gone
    // from disk, moved, or now excluded. Unplugged drives keep theirs.
    let mut removed: u64 = 0;
    if let (true, Some(cache), Some(seen)) = (!resumable_interrupt, existing_cache.as_ref(), &seen) {
        let seen = seen.lock().map_err(|_| "Seen-files lock was poisoned".to_string())?;
        let offline = offline_roots(&options.roots);
        for (key, entry) in &cache.map {
            if seen.contains(key) || offline.iter().any(|root| is_under_root(key, root)) {
                continue;
            }
            let Ok(stored) = cache.searcher.doc::<TantivyDocument>(entry.address) else { continue };
            if let Some(path) = doc_text(&stored, cache.fields.path) {
                writer.delete_term(Term::from_field_text(fields.path_exact, &path));
                removed += 1;
            }
        }
    }
    let unchanged = unchanged_atomic.load(Ordering::Relaxed);

    write_index_worker_status(
        status_path,
        &IndexWorkerStatusFile::progress(
            indexed_files,
            scanned_entries,
            skipped_files,
            "Finalizing search index".to_string(),
        ),
    )?;
    writer.commit().map_err(|error| format!("Cannot finalize search index: {error}"))?;
    writer.wait_merging_threads().map_err(|error| format!("Cannot finish search index merges: {error}"))?;
    drop(index);

    // Release the previous-index and resumed-progress mmaps BEFORE the swap.
    // On Windows, an open memory map prevents the directory rename inside
    // `swap_in_staging_search_index_for_state`. The finished index is no
    // longer something to resume.
    drop(existing_cache);
    drop(resumed);
    if !in_place {
        let _ = fs::remove_file(staging_dir.join(RESUME_MARKER_FILE));
        swap_in_staging_search_index_for_state(state_dir, staging_dir)?;
    }

    // Only a complete build vouches for its documents without text: in an
    // interrupted one, files are also empty because the pool stopped early.
    let signature_path = state_dir.join(EXTRACTION_SIGNATURE_FILE);
    if resumable_interrupt {
        let _ = fs::remove_file(&signature_path);
    } else {
        let _ = fs::write(&signature_path, extraction_signature(options));
    }

    // Wave 8.5 (2026-05-28): run the persistent OCR cache GC pass now
    // that the new index is live. TTL pass drops entries last touched
    // more than 90 days ago; LRU pass trims oldest entries until total
    // size is under 200 MB. Errors are logged but never propagated —
    // the rebuild is already complete and shouldn't fail-stop on a
    // cache maintenance hiccup. Skipped when OCR was off (no cache
    // entries to consider).
    if options.ocr_on_index_enabled {
        let cache_path = crate::ocr_cache::cache_path_for_dir(state_dir);
        match crate::ocr_cache::gc(&cache_path, crate::ocr_cache::DEFAULT_TTL_DAYS, crate::ocr_cache::DEFAULT_MAX_BYTES)
        {
            Ok(stats) if stats.entries_before > 0 => {
                #[expect(
                    clippy::as_conversions,
                    clippy::cast_precision_loss,
                    reason = "log line in MB; approximate is fine"
                )]
                let (after_mb, before_mb) =
                    (stats.bytes_after as f64 / 1_048_576.0, stats.bytes_before as f64 / 1_048_576.0);
                eprintln!(
                    "ocr_cache: GC kept {}/{} entries ({:.1} MB / {:.1} MB), dropped {} TTL + {} LRU",
                    stats.entries_after,
                    stats.entries_before,
                    after_mb,
                    before_mb,
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
            format!("Index build cancelled — {indexed_files} files indexed and kept. Re-run to continue.")
        } else {
            format!(
                "Content indexing paused (low memory) — {indexed_files} files indexed so far. \
                 Filename search works now; re-run to index the rest."
            )
        };
        IndexWorkerStatusFile::finished(true, canceled, indexed_files, scanned_entries, skipped_files, message, None)
            .into_partial()
    } else {
        let final_message = if in_place {
            format!(
                "Search index up to date — {indexed_files} files, {} new or changed, {removed} removed",
                indexed_files.saturating_sub(unchanged)
            )
        } else if reused_content_count > 0 {
            format!(
                "Indexed {indexed_files} entries (reused content for {reused_content_count} unchanged files — incremental rebuild)"
            )
        } else {
            format!("Indexed {indexed_files} entries")
        };
        IndexWorkerStatusFile::finished(true, false, indexed_files, scanned_entries, skipped_files, final_message, None)
    };
    write_index_worker_status(status_path, &status)?;
    Ok(())
}

/// Standalone metadata-only indexer for the filename index (Phase 3, step 7b).
/// Walks `options.roots` and writes one filename-only document per entry —
/// names, paths, and metadata, never file content. Skipping content extraction
/// makes it far lighter than `build_index_in_worker`, so it can run on its own
/// schedule independent of content indexing.
pub(super) fn build_filename_index_in_worker(
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
    let (writer_threads, writer_budget) = index_writer_sizing();
    let writer = index
        .writer_with_num_threads(writer_threads, writer_budget)
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
            let options = options_for_filter.clone();
            let status_path = status_path_owned.clone();
            let cancel_path = cancel_path_owned.clone();

            Box::new(move |entry_result| {
                if should_stop.load(Ordering::Relaxed) {
                    return ignore::WalkState::Quit;
                }

                let current_scanned = scanned_entries_atomic.fetch_add(1, Ordering::Relaxed) + 1;

                if current_scanned.is_multiple_of(500) && cancel_path.exists() {
                    should_stop.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }

                if current_scanned > u64::try_from(MAX_INDEXED_FILES).unwrap_or(u64::MAX) {
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

                let Ok(entry) = entry_result else {
                    skipped_files_atomic.fetch_add(1, Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                };

                let path = entry.path();
                let file_type = entry.file_type();
                let is_file = file_type.is_some_and(|value| value.is_file());
                let is_dir = file_type.is_some_and(|value| value.is_dir());
                if !is_file && !is_dir {
                    return ignore::WalkState::Continue;
                }
                if !should_include_path(path, &options) {
                    skipped_files_atomic.fetch_add(1, Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                }

                let Ok(metadata) = fs::metadata(path) else {
                    skipped_files_atomic.fetch_add(1, Ordering::Relaxed);
                    return ignore::WalkState::Continue;
                };

                let file_path = path.to_string_lossy().to_string();
                let extension = if is_file {
                    path.extension().and_then(|value| value.to_str()).unwrap_or_default().to_lowercase()
                } else {
                    String::new()
                };
                let file_name = path.file_name().and_then(|value| value.to_str()).unwrap_or_default().to_string();
                let entry_type = if is_dir { ENTRY_TYPE_FOLDER } else { ENTRY_TYPE_FILE };
                let size = metadata.len();
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
                let parent_path = path.parent().map(|parent| parent.to_string_lossy().to_string()).unwrap_or_default();

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
                            *guard = Some(format!("Cannot add document to filename index: {error}"));
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

                if throttle_ms > 0 && new_indexed > 0 && new_indexed.is_multiple_of(INDEX_THROTTLE_EVERY_FILES) {
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
    let mut writer =
        Arc::try_unwrap(writer).map_err(|_| "Internal: filename index writer still held by a worker".to_string())?;

    write_index_worker_status(
        status_path,
        &IndexWorkerStatusFile::progress(
            indexed_files,
            scanned_entries,
            skipped_files,
            "Finalizing filename index".to_string(),
        ),
    )?;
    writer.commit().map_err(|error| format!("Cannot finalize filename index: {error}"))?;
    writer.wait_merging_threads().map_err(|error| format!("Cannot finish filename index merges: {error}"))?;
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

pub(super) fn run_index_worker_watch_mode(
    state_dir: &Path,
    options_path: &Path,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    let result = run_index_worker_watch_mode_inner(state_dir, options_path, status_path, cancel_path);
    if let Err(error) = &result {
        let _ = write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::finished(false, false, 0, 0, 0, error.clone(), Some(error.clone())),
        );
    }
    result
}

pub(super) fn run_index_worker_watch_mode_inner(
    state_dir: &Path,
    options_path: &Path,
    status_path: &Path,
    cancel_path: &Path,
) -> Result<(), String> {
    // Minimum gap between batch applies. Events accumulate in the channel during
    // this wait and are processed together — fewer index opens, less CPU overall.
    const MIN_APPLY_INTERVAL_MS: u128 = 5_000;
    // Periodic full reconciliation, plus the loop-gap above which we assume the
    // process was suspended (sleep/hibernate) and reconcile immediately.
    const RECONCILE_INTERVAL_MS: u128 = 6 * 60 * 60 * 1_000;
    const SLEEP_GAP_THRESHOLD_MS: u128 = 2 * 60 * 1_000;
    // After a failed batch or lost events, a reconciliation this soon heals
    // what was missed (not at once: a persistent error shouldn't spin).
    const RECONCILE_SOON_MS: u128 = 60 * 1_000;
    // One watcher per index, whatever started it: a second one leaves now
    // instead of fighting the first over the index writer.
    let Some(_only_watcher) = claim_watcher_lock(state_dir) else {
        return Ok(());
    };
    let options_bytes =
        fs::read(options_path).map_err(|error| format!("Cannot read watcher worker options: {error}"))?;
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

    let indexed_files = read_search_config_for_state(state_dir).ok().flatten().map_or(0, |config| config.indexed_files);
    let strategy = watcher_strategy_for_index_size(indexed_files);
    let notify_roots = options.roots.clone();

    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        // Drop noise events at the source so they never wake the main loop.
        // Access events (file reads by Search Indexer/Defender) and metadata-only
        // changes never change index content — skip them entirely.
        // Kept: "rescan" notices (Windows' event buffer overflowed, so
        // changes were lost) arrive as `Other` events.
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

    let mut last_apply_ms: u128 = 0;
    let reconcile_soon = |now_ms: u128| now_ms.saturating_sub(RECONCILE_INTERVAL_MS - RECONCILE_SOON_MS);
    // Changes made while Dash wasn't running are found by a reconciliation
    // soon after start (a minute in, not during launch) instead of at the
    // next 6-hour pass — unless the index was just written (a build that
    // just finished, a quick restart), when there is nothing to catch up.
    let index_just_written = fs::metadata(active_dir.join("meta.json"))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < Duration::from_mins(10));
    let mut last_reconcile_ms: u128 = if index_just_written { unix_now_ms() } else { reconcile_soon(unix_now_ms()) };
    let mut last_loop_iter_ms: u128 = unix_now_ms();
    let mut updated_total = 0_u64;
    let mut deleted_total = 0_u64;
    write_index_worker_status(
        status_path,
        &IndexWorkerStatusFile::progress(
            updated_total,
            watched_roots,
            deleted_total,
            format!("Watching {watched_roots} explicit folder root(s) with {}", strategy.name),
        ),
    )?;

    // Semantic search: documents without a current vector are embedded here
    // while no file events wait, a step at a time (`search::semantic`),
    // from two minutes after start. The backlog is read again after the
    // index changes.
    let mut semantic_on = options.semantic_search_enabled && crate::embedding::is_available();
    let vectors = crate::vector_cache::cache_path_for_dir(state_dir);
    let semantic_from = std::time::Instant::now() + Duration::from_mins(2);
    let mut backlog: Option<SemanticBacklog> = None;
    let mut backlog_stale = true;
    let mut backlog_read_at = std::time::Instant::now();
    let mut semantic_reported_at = std::time::Instant::now();

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
        if resumed_from_suspend || now_ms.saturating_sub(last_reconcile_ms) >= RECONCILE_INTERVAL_MS {
            last_reconcile_ms = now_ms;
            let reconciled = reconcile_watched_index(&active_dir, &options);
            if reconciled.is_err() {
                last_reconcile_ms = reconcile_soon(now_ms);
            }
            if let Ok((reconciled_updates, reconciled_deletes)) = reconciled {
                if reconciled_updates > 0 || reconciled_deletes > 0 {
                    backlog_stale = true;
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
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if semantic_on && std::time::Instant::now() >= semantic_from {
                    if backlog.is_none() || (backlog_stale && backlog_read_at.elapsed() >= Duration::from_mins(2)) {
                        backlog = semantic_backlog(&active_dir, &vectors);
                        backlog_stale = false;
                        backlog_read_at = std::time::Instant::now();
                    }
                    if let Some(work) = backlog.as_mut().filter(|work| work.remaining() > 0) {
                        let until = std::time::Instant::now() + Duration::from_millis(1_500);
                        semantic_on = embed_some(work, &vectors, until).is_some();
                        let left = work.remaining();
                        if left == 0 || semantic_reported_at.elapsed() >= Duration::from_secs(30) {
                            semantic_reported_at = std::time::Instant::now();
                            let _ = write_index_worker_status(
                                status_path,
                                &IndexWorkerStatusFile::progress(
                                    updated_total,
                                    watched_roots,
                                    deleted_total,
                                    format!("Semantic search: {left} documents left to embed"),
                                ),
                            );
                        }
                    }
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Index watcher event channel disconnected".to_string());
            }
        }
        collect_watcher_events(&rx, &mut pending, strategy, cancel_path);

        // A watcher error or a rescan notice means events were lost.
        if pending.iter().any(|event| event.as_ref().map_or(true, Event::need_rescan)) {
            last_reconcile_ms = last_reconcile_ms.min(reconcile_soon(unix_now_ms()));
        }

        // A failed batch (the index busy, a file locked, ...) no longer
        // ends the watcher: it's reported, and a reconciliation soon picks
        // up what the batch would have changed.
        let (updated_batch, deleted_batch) = match apply_watch_batch_to_index(&active_dir, &options, pending) {
            Ok(counts) => counts,
            Err(error) => {
                last_apply_ms = unix_now_ms();
                last_reconcile_ms = last_reconcile_ms.min(reconcile_soon(last_apply_ms));
                let _ = write_index_worker_status(
                    status_path,
                    &IndexWorkerStatusFile::progress(
                        updated_total,
                        watched_roots,
                        deleted_total,
                        format!("Watcher update failed, retrying soon: {error}"),
                    ),
                );
                continue;
            }
        };

        // Always mark the apply time so the rate limiter kicks in even when
        // a batch produced no changes (e.g. all events were already filtered).
        // Without this, noise-only batches would loop continuously.
        last_apply_ms = unix_now_ms();

        if updated_batch == 0 && deleted_batch == 0 {
            continue;
        }

        backlog_stale = true;
        updated_total += updated_batch;
        deleted_total += deleted_batch;
        let _ = write_index_worker_status(
            status_path,
            &IndexWorkerStatusFile::progress(
                updated_total,
                watched_roots,
                deleted_total,
                format!("{}: +{updated_batch} / -{deleted_batch}", strategy.name),
            ),
        );
    }
}

/// The content watcher's lock: `index-watcher.lock`, held open with no
/// sharing for the watcher's lifetime (Windows releases it if the process
/// dies). `None` when another watcher holds it.
pub(super) fn claim_watcher_lock(state_dir: &Path) -> Option<fs::File> {
    let mut open = fs::OpenOptions::new();
    open.create(true).truncate(false).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        open.share_mode(0);
    }
    open.open(state_dir.join("index-watcher.lock")).ok()
}

pub(super) fn collect_watcher_events(
    rx: &std::sync::mpsc::Receiver<notify::Result<Event>>,
    pending: &mut Vec<notify::Result<Event>>,
    strategy: WatcherStrategy,
    cancel_path: &Path,
) {
    let started_at = unix_now_ms();
    while pending.len() < strategy.max_batch
        && unix_now_ms().saturating_sub(started_at) < u128::from(strategy.debounce_ms)
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
pub(super) fn reconcile_watched_index(
    active_dir: &Path,
    options: &FileSearchIndexOptions,
) -> Result<(u64, u64), String> {
    use notify::event::{ModifyKind, RemoveKind};

    let Some(mut cache) = load_existing_index_for_reuse(active_dir) else {
        // No readable index to diff against — a scheduled rebuild will cover it.
        return Ok((0, 0));
    };

    // Entries found on disk are taken out of `cache.map` as the walk meets
    // them, so what is left at the end is what disappeared — no second set
    // of every path. Only files the content index holds are compared: other
    // files (programs, media, ...) used to be queued as "changed" by the
    // hundred thousand, only to be dropped when applied.
    let mut events: Vec<notify::Result<Event>> = Vec::new();
    let offline = offline_roots(&options.roots);

    for root in &options.roots {
        if offline.contains(&root.trim_end_matches(['\\', '/']).to_lowercase()) {
            continue;
        }
        let builder = index_walker(root, options, true);
        for entry in builder.build().flatten() {
            let path = entry.path();
            let is_file = entry.file_type().is_some_and(|file_type| file_type.is_file());
            if !is_file || !should_include_path(path, options) || !should_index_for_content(path, options) {
                continue;
            }
            let existing = cache.map.remove(&path.to_string_lossy().to_lowercase());
            let Ok(meta) = fs::metadata(path) else {
                continue;
            };
            let modified_ms = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX));
            let unchanged =
                existing.is_some_and(|existing| existing.modified_ms == modified_ms && existing.size == meta.len());
            if !unchanged {
                events.push(Ok(Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path.to_path_buf())));
            }
        }
    }

    // Index entries whose file no longer exists on disk → deletions the live
    // watcher missed. Resolve the case-preserved path from the stored doc.
    for (path_lower, existing) in &cache.map {
        if offline.iter().any(|root| is_under_root(path_lower, root)) {
            continue;
        }
        let doc: TantivyDocument = match cache.searcher.doc(existing.address) {
            Ok(doc) => doc,
            Err(_) => continue,
        };
        if let Some(real_path) = doc_text(&doc, cache.fields.path) {
            events.push(Ok(Event::new(EventKind::Remove(RemoveKind::Any)).add_path(PathBuf::from(real_path))));
        }
    }

    if events.is_empty() {
        return Ok((0, 0));
    }
    // Release the read-side mmap before apply_watch_batch_to_index opens a writer.
    drop(cache);
    apply_watch_batch_to_index(active_dir, options, events)
}

pub(super) fn apply_watch_batch_to_index(
    active_dir: &Path,
    options: &FileSearchIndexOptions,
    pending: Vec<notify::Result<Event>>,
) -> Result<(u64, u64), String> {
    // Filter out pure access events (file reads by Windows Search, Defender, etc.)
    // and any other events that can never result in an index change, before we pay
    // the cost of opening the index at all.
    let events: Vec<Event> = pending
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|e| !matches!(e.kind, EventKind::Access(_)))
        .collect();

    if events.is_empty() {
        return Ok((0, 0));
    }

    let directory = MmapDirectory::open(active_dir)
        .map_err(|error| format!("Cannot open active search index directory: {error}"))?;
    let index = Index::open(directory).map_err(|error| format!("Cannot open active search index: {error}"))?;
    let schema = index.schema();
    let fields = extract_fields(&schema)?;
    // 1 writer thread — the watcher writes tiny amounts per batch.
    let mut writer = index
        .writer_with_num_threads(1, WRITER_MEMORY_BUDGET_BYTES_IDLE)
        .map_err(|error| format!("Cannot open watcher search writer: {error}"))?;
    let max_content_bytes = usize::try_from(options.max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB))
        .unwrap_or(usize::MAX)
        .saturating_mul(1024)
        .max(8 * 1024);
    let mut updated_batch = 0_u64;
    let mut deleted_batch = 0_u64;
    let mut missing_text = false;

    for event in events {
        let (updated, deleted, missed) =
            apply_watch_event_to_writer(&event, &mut writer, &fields, options, max_content_bytes);
        updated_batch += updated;
        deleted_batch += deleted;
        missing_text |= missed;
    }

    if updated_batch == 0 && deleted_batch == 0 {
        return Ok((0, 0));
    }

    writer.commit().map_err(|error| format!("Cannot commit isolated watcher update: {error}"))?;
    writer.wait_merging_threads().map_err(|error| format!("Cannot finish isolated watcher merge: {error}"))?;
    // The index now holds text-less documents a full build would extract
    // (see `apply_watch_event_to_writer`): the next build retries them.
    if missing_text {
        if let Some(state_dir) = active_dir.parent() {
            let _ = fs::remove_file(state_dir.join(EXTRACTION_SIGNATURE_FILE));
        }
    }
    Ok((updated_batch, deleted_batch))
}
