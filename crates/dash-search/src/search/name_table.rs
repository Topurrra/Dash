//! Read access for the in-memory name table (`names.rs`), status helpers.

use super::*;

pub(super) fn update_status(update: impl FnOnce(&mut FileSearchStatus)) {
    if let Ok(mut status) = SEARCH_STATUS.lock() {
        update(&mut status);
    }
}

pub(super) fn emit_progress(app: &std::sync::Arc<dyn dash_core::Host>, payload: &FileSearchProgressEvent) {
    app.emit(INDEX_PROGRESS_EVENT, serde_json::to_value(payload).unwrap_or_default());
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
pub(super) fn filename_searcher_fingerprint(searcher: &Searcher) -> u64 {
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
    let still_open = || FILENAME_ENGINE.lock().is_ok_and(|guard| guard.is_some());
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

pub(super) fn visit_filename_searcher(
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
            if seen.is_multiple_of(65_536) && !still_open() {
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

pub(super) fn doc_text(document: &TantivyDocument, field: Field) -> Option<String> {
    document.get_first(field).and_then(|value| value.as_str().map(std::string::ToString::to_string))
}

pub(super) fn doc_u64(document: &TantivyDocument, field: Field) -> Option<u64> {
    document.get_first(field).and_then(|value| value.as_u64())
}

/// Collect every text value stored under `field` as a `Vec<String>`. Used for
/// multi-valued tag fields like `sensitive_kinds`. Returns an empty vec when
/// the field is absent (`Option::None`) so old indexes simply yield no findings.
pub(super) fn collect_doc_text_values(document: &TantivyDocument, field: Option<Field>) -> Vec<String> {
    let Some(field) = field else { return Vec::new() };
    document.get_all(field).filter_map(|value| value.as_str().map(std::string::ToString::to_string)).collect()
}
