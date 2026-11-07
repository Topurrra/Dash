//! Semantic search vectors, made in the background.
//!
//! Only prose documents are embedded (`is_semantic_document`): finding by
//! meaning is for notes, documents and PDFs, and code, logs and data files
//! number in the hundreds of thousands on a developer's machine — days of
//! CPU for the embedding model. Builds don't embed at all; the content
//! watcher, which runs at idle priority, works through the documents that
//! have no vector or one made from an older version of the file, a little
//! at a time between file changes. Progress is the vector store itself, so
//! a restart picks up where it left off, and results grow as it goes.

use super::*;
use crate::vector_cache::DocVectors;

/// Prose documents: the files semantic search covers.
pub(super) fn is_semantic_document(path: &str) -> bool {
    let extension = Path::new(path).extension().and_then(|value| value.to_str()).unwrap_or_default().to_lowercase();
    matches!(extension.as_str(), "pdf" | "docx" | "odt" | "rtf" | "pptx" | "odp" | "md" | "markdown" | "txt" | "ki")
}

/// The documents still to embed, newest last (taken first), with the index
/// snapshot to read their text from.
pub(super) struct SemanticBacklog {
    searcher: tantivy::Searcher,
    fields: SearchFields,
    todo: Vec<(String, u64, tantivy::DocAddress)>,
}

impl SemanticBacklog {
    pub(super) fn remaining(&self) -> usize {
        self.todo.len()
    }
}

/// Compares the index with the vector store: documents without a current
/// vector go on the backlog, and vectors of files no longer indexed (or no
/// longer embedded) are dropped. `None` without a readable index.
pub(super) fn semantic_backlog(active_dir: &Path, vectors: &Path) -> Option<SemanticBacklog> {
    use tantivy::DocSet as _;
    let index = Index::open(MmapDirectory::open(active_dir).ok()?).ok()?;
    let fields = extract_fields(&index.schema()).ok()?;
    let reader =
        index.reader_builder().reload_policy(ReloadPolicy::Manual).doc_store_cache_num_blocks(4).try_into().ok()?;
    let searcher: tantivy::Searcher = reader.searcher();
    let mut stored = crate::vector_cache::stored_versions(vectors);
    let mut todo = Vec::new();
    for (segment_ord, segment) in (0u32..).zip(searcher.segment_readers()) {
        let Ok(paths) = segment.inverted_index(fields.path_exact) else { continue };
        let Ok(modified) = segment.fast_fields().u64("modified_ms") else { continue };
        let alive = segment.alive_bitset();
        let Ok(mut terms) = paths.terms().stream() else { continue };
        while terms.advance() {
            let Ok(path) = std::str::from_utf8(terms.key()) else { continue };
            if !is_semantic_document(path) {
                continue;
            }
            let Ok(mut docs) =
                paths.read_postings_from_terminfo(terms.value(), tantivy::schema::IndexRecordOption::Basic)
            else {
                continue;
            };
            let mut doc_id = docs.doc();
            while doc_id != tantivy::TERMINATED {
                if alive.is_none_or(|bits| bits.is_alive(doc_id)) {
                    let modified_ms = modified.first(doc_id).unwrap_or(0);
                    if stored.remove(path) != Some(modified_ms) {
                        todo.push((path.to_string(), modified_ms, tantivy::DocAddress::new(segment_ord, doc_id)));
                    }
                }
                doc_id = docs.advance();
            }
        }
    }
    // What's left in `stored` matched no indexed document.
    crate::vector_cache::remove_many(vectors, &stored.into_keys().collect::<Vec<_>>());
    // Most recently changed last, so they're embedded first.
    todo.sort_unstable_by_key(|(_, modified_ms, _)| *modified_ms);
    Some(SemanticBacklog { searcher, fields, todo })
}

/// Embeds documents from the backlog until `until`, storing them in one
/// write. Returns how many were done, or `None` when the model can't run
/// (then nothing more can be done this session).
pub(super) fn embed_some(backlog: &mut SemanticBacklog, vectors: &Path, until: std::time::Instant) -> Option<usize> {
    let mut done = Vec::new();
    while std::time::Instant::now() < until {
        let Some((path, modified_ms, address)) = backlog.todo.pop() else { break };
        let text = backlog
            .searcher
            .doc::<TantivyDocument>(address)
            .ok()
            .and_then(|doc| doc_text(&doc, backlog.fields.content))
            .unwrap_or_default();
        let chunks = crate::embedding::chunk_text(&text, 180, 30, 4);
        let chunks = if chunks.is_empty() { Vec::new() } else { crate::embedding::embed_batch(&chunks)? };
        done.push(DocVectors { path, modified_ms, chunks });
    }
    crate::vector_cache::store_many(vectors, &done);
    Some(done.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_documents_are_embedded_code_is_not() {
        for path in [r"C:\n\plan.md", r"C:\n\Report.PDF", r"C:\n\letter.docx", r"C:\n\todo.txt", r"C:\n\x.ki"] {
            assert!(is_semantic_document(path), "{path}");
        }
        for path in [r"C:\src\main.rs", r"C:\src\app.ts", r"C:\logs\run.log", r"C:\data\x.csv", r"C:\n\no-ext"] {
            assert!(!is_semantic_document(path), "{path}");
        }
    }

    /// Missing and stale vectors are queued (newest taken first); vectors of
    /// files gone from the index, or that aren't documents, are dropped.
    #[test]
    fn backlog_finds_missing_and_stale_and_prunes_the_rest() {
        let dir = std::env::temp_dir().join(format!("dash-semantic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let index_dir = dir.join("index");
        fs::create_dir_all(&index_dir).unwrap();
        let schema = build_search_schema();
        let index = Index::open_or_create(MmapDirectory::open(&index_dir).unwrap(), schema.clone()).unwrap();
        let fields = extract_fields(&schema).unwrap();
        let mut writer: IndexWriter = index.writer_with_num_threads(1, 20_000_000).unwrap();
        for (path, modified_ms) in [(r"C:\n\a.md", 10), (r"C:\n\b.md", 20), (r"C:\src\c.rs", 30), (r"C:\n\d.pdf", 40)] {
            let meta = ContentDocMeta {
                file_path: path,
                file_name: "x",
                extension: "x",
                entry_type: ENTRY_TYPE_FILE,
                size: 1,
                modified_ms,
                created_ms: modified_ms,
            };
            add_content_document(&writer, fields, &meta, "some text").unwrap();
        }
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();

        let vectors = dir.join("vectors.redb");
        let stored =
            |path: &str, modified_ms| DocVectors { path: path.into(), modified_ms, chunks: vec![vec![1.0, 0.0]] };
        crate::vector_cache::store_many(
            &vectors,
            &[
                stored(r"C:\n\a.md", 10),
                stored(r"C:\n\b.md", 5),
                stored(r"C:\n\gone.md", 1),
                stored(r"C:\src\c.rs", 30),
            ],
        );

        let backlog = semantic_backlog(&index_dir, &vectors).unwrap();
        let queued: Vec<&str> = backlog.todo.iter().map(|(path, _, _)| path.as_str()).collect();
        assert_eq!(queued, vec![r"C:\n\b.md", r"C:\n\d.pdf"], "stale and missing, newest last");
        let mut left: Vec<String> = crate::vector_cache::stored_versions(&vectors).into_keys().collect();
        left.sort();
        assert_eq!(left, vec![r"C:\n\a.md".to_string(), r"C:\n\b.md".to_string()], "gone and code dropped");
        drop(backlog);
        let _ = fs::remove_dir_all(&dir);
    }

    /// With the real model: backlog documents get vectors and are found by
    /// meaning. Run with `DASH_EMBEDDING_DIR=<embedding-runtime> cargo test
    /// --features dash-search/semantic -p dash-search --lib real_embedding --
    /// --ignored`.
    #[test]
    #[ignore = "needs the embedding model"]
    fn real_embedding_fills_the_backlog() {
        crate::embedding::set_model_dir(std::env::var("DASH_EMBEDDING_DIR").expect("DASH_EMBEDDING_DIR").into());
        assert!(crate::embedding::is_available(), "model loads");
        let dir = std::env::temp_dir().join(format!("dash-semantic-real-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let index_dir = dir.join("index");
        fs::create_dir_all(&index_dir).unwrap();
        let schema = build_search_schema();
        let index = Index::open_or_create(MmapDirectory::open(&index_dir).unwrap(), schema.clone()).unwrap();
        let fields = extract_fields(&schema).unwrap();
        let mut writer: IndexWriter = index.writer_with_num_threads(1, 20_000_000).unwrap();
        let docs = [
            (r"C:\n\trip.md", "Packing list for the mountain hiking holiday: boots, tent, sleeping bag."),
            (r"C:\n\taxes.md", "Quarterly VAT return and invoices for the accountant."),
        ];
        for (path, text) in docs {
            let meta = ContentDocMeta {
                file_path: path,
                file_name: "x",
                extension: "md",
                entry_type: ENTRY_TYPE_FILE,
                size: 1,
                modified_ms: 1,
                created_ms: 1,
            };
            add_content_document(&writer, fields, &meta, text).unwrap();
        }
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();

        let vectors = dir.join("vectors.redb");
        let mut backlog = semantic_backlog(&index_dir, &vectors).unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
        assert_eq!(embed_some(&mut backlog, &vectors, until), Some(2));
        let query = crate::embedding::embed("outdoor camping trip").unwrap();
        let best = crate::vector_cache::top_k(&vectors, &query, 1);
        assert_eq!(best.first().map(|(path, _)| path.as_str()), Some(r"C:\n\trip.md"), "{best:?}");
        drop(backlog);
        let _ = fs::remove_dir_all(&dir);
    }
}
