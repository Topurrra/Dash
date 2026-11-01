//! Writing documents: watch events, notes, schemas and fields.

use super::*;

pub(super) fn index_writer_threads() -> usize {
    let available = thread::available_parallelism().map(|value| value.get()).unwrap_or(2);
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
pub(super) fn index_walker_threads(performance_mode: &str, on_battery: bool) -> usize {
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
pub(super) fn index_walker_threads_with_ocr(performance_mode: &str, on_battery: bool, ocr_enabled: bool) -> usize {
    let available = thread::available_parallelism().map(|value| value.get()).unwrap_or(INDEX_WALKER_THREADS_FALLBACK);
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

pub(super) fn apply_watch_event_to_writer(
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

pub(super) fn upsert_path_document(
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
        path.extension().and_then(|value| value.to_str()).unwrap_or_default().to_lowercase()
    } else {
        String::new()
    };
    // `.ki` notes index + display by their human title, matching the filename
    // index so a note reads the same in "Inside" and "Files" results.
    let file_name = if is_file && is_ki_path(path) {
        note_display_title(path)
    } else {
        path.file_name().and_then(|value| value.to_str()).unwrap_or_default().to_string()
    };
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let created_ms = metadata
        .created()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(modified_ms);
    let content =
        if is_file { read_text_for_index(path, max_content_bytes).unwrap_or_default() } else { String::new() };
    let entry_type = if is_dir { ENTRY_TYPE_FOLDER } else { ENTRY_TYPE_FILE };

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
    writer.add_document(document).map_err(|error| format!("Cannot index path update: {error}"))?;

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
    changed +=
        update_notes_content_index(&active_search_index_dir_for_state(&state_dir), &upserts, &deletes).unwrap_or(0);
    changed +=
        update_notes_filename_index(&active_filename_index_dir_for_state(&state_dir), &upserts, &deletes).unwrap_or(0);
    Ok(changed)
}

/// True if `path` has the `.ki` note extension (case-insensitive). Cheap —
/// touches the path only, never the filesystem.
pub(super) fn is_ki_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("ki")) == Some(true)
}

/// True if `path` is an existing `.ki` file (notes only — never index arbitrary
/// paths handed to the command).
pub(super) fn is_note_file(path: &str) -> bool {
    let p = Path::new(path);
    p.is_file() && is_ki_path(p)
}

/// Best-effort human title for a `.ki` note: frontmatter `title:`, else the
/// first `# ` heading, else the file stem. Used so filename/palette search
/// matches (and displays) the title the user gave the note rather than its
/// on-disk slug like `untitled-3.ki`, which they never think in. Reads the
/// file (notes are tiny); callers gate on `is_ki_path` so this never runs in
/// the full-walk hot path for non-note files.
pub(super) fn note_display_title(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("untitled").to_string();
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
                    let value = value.trim().trim_matches('"').trim_matches('\'').trim();
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

pub(super) fn update_notes_content_index(
    active_dir: &Path,
    upserts: &[String],
    deletes: &[String],
) -> Result<u64, String> {
    if !active_dir.join("meta.json").exists() {
        return Ok(0); // content index not built yet
    }
    let directory =
        MmapDirectory::open(active_dir).map_err(|error| format!("Cannot open content index for notes: {error}"))?;
    let index = Index::open(directory).map_err(|error| format!("Cannot open content index: {error}"))?;
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
        if is_note_file(path) && upsert_path_document(Path::new(path), &mut writer, &fields, max_content_bytes).is_ok()
        {
            changed += 1;
        }
    }
    if changed == 0 {
        return Ok(0);
    }
    writer.commit().map_err(|error| format!("Cannot commit notes content index: {error}"))?;
    let _ = writer.wait_merging_threads();
    Ok(changed)
}

pub(super) fn update_notes_filename_index(
    active_dir: &Path,
    upserts: &[String],
    deletes: &[String],
) -> Result<u64, String> {
    if !active_dir.join("meta.json").exists() {
        return Ok(0); // filename index not built yet
    }
    let directory =
        MmapDirectory::open(active_dir).map_err(|error| format!("Cannot open filename index for notes: {error}"))?;
    let index = Index::open(directory).map_err(|error| format!("Cannot open filename index: {error}"))?;
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
    writer.commit().map_err(|error| format!("Cannot commit notes filename index: {error}"))?;
    let _ = writer.wait_merging_threads();
    Ok(changed)
}

pub(super) fn build_search_schema() -> Schema {
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

pub(super) fn extract_fields(schema: &Schema) -> Result<SearchFields, String> {
    Ok(SearchFields {
        path: schema.get_field("path").map_err(|_| "Search schema field 'path' missing".to_string())?,
        path_exact: schema
            .get_field("path_exact")
            .map_err(|_| "Search schema field 'path_exact' missing".to_string())?,
        file_name: schema.get_field("file_name").map_err(|_| "Search schema field 'file_name' missing".to_string())?,
        entry_type: schema.get_field("entry_type").ok(),
        extension: schema.get_field("extension").map_err(|_| "Search schema field 'extension' missing".to_string())?,
        content: schema.get_field("content").map_err(|_| "Search schema field 'content' missing".to_string())?,
        sensitive_kinds: schema.get_field("sensitive_kinds").ok(),
        size: schema.get_field("size").map_err(|_| "Search schema field 'size' missing".to_string())?,
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
pub(super) fn build_filename_index_schema() -> Schema {
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
pub(super) struct FilenameIndexFields {
    pub(super) path: Field,
    pub(super) path_exact: Field,
    pub(super) file_name: Field,
    pub(super) parent_path: Field,
    pub(super) extension: Field,
    pub(super) entry_type: Field,
    pub(super) size: Field,
    pub(super) modified_ms: Field,
    pub(super) created_ms: Field,
}

pub(super) fn extract_filename_index_fields(schema: &Schema) -> Result<FilenameIndexFields, String> {
    let field =
        |name: &str| schema.get_field(name).map_err(|_| format!("Filename index schema field '{name}' missing"));
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
