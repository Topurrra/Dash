//! Result items, snippets, highlighting, extension words.

use super::*;

#[expect(
    clippy::too_many_arguments,
    reason = "per-hit inputs from the search loop; a struct would only be built to be taken apart here"
)]
pub(super) fn build_result_item(
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
    frecency: &crate::frecency::FrecencySnapshot,
) -> Result<Option<FileSearchResultItem>, String> {
    let retrieved =
        searcher.doc::<TantivyDocument>(address).map_err(|error| format!("Cannot load search document: {error}"))?;

    let path = doc_text(&retrieved, fields.path).unwrap_or_default();
    if path.is_empty() {
        return Ok(None);
    }

    let entry_type =
        fields.entry_type.and_then(|field| doc_text(&retrieved, field)).unwrap_or_else(|| ENTRY_TYPE_FILE.to_string());
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
    let created_ms = fields.created_ms.and_then(|field| doc_u64(&retrieved, field)).unwrap_or(modified_ms);
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
                let is_empty = if is_folder { is_empty_folder_path(Path::new(&path)) } else { size == 0 };
                if !is_empty {
                    return Ok(None);
                }
            }
            SizeFilter::Range { min_bytes, max_bytes, .. } => {
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
    let collapse = |text: &str| -> String { text.chars().filter(|c| c.is_alphanumeric()).collect() };
    let file_name_collapsed = collapse(&file_name_lower);
    let path_collapsed = collapse(&path_lower);
    let rank_query = normalized_rank_query.trim();
    let rank_query_collapsed = collapse(rank_query);
    let has_full_query_in_name = !rank_query.is_empty()
        && (normalized_file_name.contains(rank_query)
            || (!rank_query_collapsed.is_empty() && file_name_collapsed.contains(&rank_query_collapsed)));
    let has_full_query_in_path = !rank_query.is_empty()
        && (normalized_path.contains(rank_query)
            || (!rank_query_collapsed.is_empty() && path_collapsed.contains(&rank_query_collapsed)));
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
            for word in normalized_file_name.split_whitespace().chain(normalized_path.split_whitespace()) {
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
        let required = query_keywords.len().div_ceil(2); // ceil(N/2)
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
            && (normalized_file_name.contains(&normalized_phrase) || normalized_path.contains(&normalized_phrase))
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
        filter_bonus += crate::rank::FOLDER_FILTER_BONUS;
        if has_full_query_in_name || exact_keyword_hits > 0 {
            filter_bonus += crate::rank::FOLDER_FILTER_NAME_BONUS;
        }
    }
    if natural_plan.entry_type_filter.as_deref() == Some(ENTRY_TYPE_FILE) && !is_folder {
        filter_bonus += crate::rank::FILE_FILTER_BONUS;
    }
    // Extension-priority: when the query carries a multi-extension filter (a
    // category like "movies"/"video", or an explicit `ext:` list), rank by the
    // extension's position in that list. The lists are ordered most-likely-first
    // (mp4 > mkv > … > ts), so true movies sort above a stray `.ts` TypeScript
    // file. No effect for a single-extension filter.
    if let Some(position) = extension_filters.iter().position(|value| value == &extension_lower) {
        filter_bonus += crate::rank::ext_priority_bonus(position, extension_filters.len());
    }
    let lexical = crate::rank::lexical_quality(&crate::rank::LexicalHits {
        full_query_in_name: has_full_query_in_name,
        full_query_in_path: has_full_query_in_path,
        exact_keyword_hits,
        prefix_keyword_hits,
        fuzzy_keyword_hits,
        phrase_name_hits: phrase_hits,
        phrase_path_hits,
        total_keywords: query_keywords.iter().filter(|word| !word.is_empty()).count(),
    });
    let adjusted_score = crate::rank::fuse(
        &crate::rank::RankSignals {
            bm25: score,
            lexical,
            frecency_boost: frecency.boost(if is_folder { ENTRY_TYPE_FOLDER } else { ENTRY_TYPE_FILE }, &path),
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
    let guard = SEARCH_ENGINE.lock().map_err(|_| "Search engine lock failed".to_string())?;
    let Some(engine) = guard.as_ref() else {
        return Ok(SensitiveFilesReport { files: Vec::new(), total: 0, index_unavailable: true });
    };
    let Some(kinds_field) = engine.fields.sensitive_kinds else {
        // Index predates the field — a rebuild is needed to populate it.
        return Ok(SensitiveFilesReport { files: Vec::new(), total: 0, index_unavailable: true });
    };

    let searcher = engine.reader.searcher();
    let query = TermQuery::new(Term::from_field_text(kinds_field, "sensitive"), IndexRecordOption::Basic);
    let capped = limit.clamp(1, 1000);

    let total = searcher.search(&query, &Count).map_err(|error| format!("Sensitive scan failed: {error}"))?;
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
        files.push(SensitiveFile { path, file_name, kinds });
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
        !crate::sensitive_scan::scan_text_detailed(&content, false).is_empty()
    });

    Ok(SensitiveFilesReport { files, total, index_unavailable: false })
}

/// Resolve a content-index document to its address by exact path, via a term
/// query on the `path_exact` (STRING) field. Used by the semantic pass to
/// hydrate a pure-vector candidate the keyword passes never returned. `None`
/// when the path isn't in the index (e.g. deleted since it was embedded).
pub(super) fn content_doc_address_for_path(
    searcher: &Searcher,
    fields: &SearchFields,
    path: &str,
) -> Option<tantivy::DocAddress> {
    let term = tantivy::Term::from_field_text(fields.path_exact, path);
    let query = tantivy::query::TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
    let top = searcher.search(&query, &TopDocs::with_limit(1).order_by_score()).ok()?;
    top.into_iter().next().map(|(_score, address)| address)
}

#[expect(
    clippy::too_many_arguments,
    reason = "per-hit inputs from the search loop; a struct would only be built to be taken apart here"
)]
pub(super) fn build_content_result_item(
    searcher: &Searcher,
    fields: &SearchFields,
    address: tantivy::DocAddress,
    score: f32,
    query_keywords: &[String],
    path_filter: Option<&str>,
    extension_filters: &[String],
    max_content_bytes: usize,
    frecency: &crate::frecency::FrecencySnapshot,
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

    let entry_type =
        fields.entry_type.and_then(|field| doc_text(&retrieved, field)).unwrap_or_else(|| ENTRY_TYPE_FILE.to_string());
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

    let file_name = doc_text(&retrieved, fields.file_name)
        .unwrap_or_else(|| Path::new(&path).file_name().and_then(|value| value.to_str()).unwrap_or(&path).to_string());

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
    let coverage = if total_keywords > 0 { matched_keywords.len() as f32 / total_keywords as f32 } else { 0.0 };
    // 0..1 in-document frequency factor — logarithmic so 200 mentions does not
    // dwarf a precise match in a file with 3.
    let depth = (((match_count as f32) + 1.0).ln() / 6.0).clamp(0.0, 1.0);
    let lexical = (coverage * (0.5 + 0.5 * depth)).clamp(0.0, 1.0);
    let fused_score = crate::rank::fuse(
        &crate::rank::RankSignals {
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

pub(super) fn matched_content_keywords(content: &str, query_keywords: &[String]) -> Vec<String> {
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

pub(super) fn build_content_snippet(content: &str, query_keywords: &[String]) -> String {
    let clean_content = collapse_preview_whitespace(content);
    if clean_content.is_empty() {
        return "Indexed content matched, but preview text is not available.".to_string();
    }

    let ascii_lower = if clean_content.is_ascii() { clean_content.to_ascii_lowercase() } else { String::new() };
    let match_index = if ascii_lower.is_empty() {
        None
    } else {
        query_keywords
            .iter()
            .filter(|keyword| keyword.len() >= 2)
            .filter_map(|keyword| ascii_lower.find(keyword).map(|index| (index, keyword.len())))
            .min_by_key(|(index, _)| *index)
    };

    let (start, end) = if let Some((index, len)) = match_index {
        let raw_start = index.saturating_sub(120);
        let raw_end = (index + len + 220).min(clean_content.len());
        (nearest_char_boundary_before(&clean_content, raw_start), nearest_char_boundary_after(&clean_content, raw_end))
    } else {
        (0, nearest_char_boundary_after(&clean_content, clean_content.len().min(360)))
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

pub(super) fn collapse_preview_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// HTML-escape a snippet of plain text so it's safe to inject with `{@html}`.
/// Only quotes/angle brackets matter — Svelte's @html doesn't itself sanitize.
pub(super) fn html_escape(value: &str) -> String {
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
pub(super) fn highlight_text(text: &str, keywords: &[String]) -> String {
    if keywords.is_empty() || text.is_empty() {
        return html_escape(text);
    }
    // Pre-lowercase keywords (already lowercased by extract_query_keywords but
    // defensive) and sort longest-first so "investment" wins over "invest" when
    // matching from the same position.
    let mut kws: Vec<String> = keywords.iter().filter(|k| k.len() >= 2).map(|k| k.to_lowercase()).collect();
    kws.sort_by_key(|k| std::cmp::Reverse(k.len()));
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
            if lower.len() < i + kw.len() {
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
pub(super) fn count_keyword_matches(content: &str, keywords: &[String]) -> u32 {
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
pub(super) fn extract_highlighted_snippets(
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
pub(super) fn build_content_prefix_query(query_keywords: &[String], content_field: Field) -> Option<Box<dyn Query>> {
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
        let range = RangeQuery::new(Bound::Included(lower_term), Bound::Excluded(upper_term));
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
pub(super) fn build_content_fuzzy_query(query_keywords: &[String], content_field: Field) -> Option<Box<dyn Query>> {
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

pub(super) fn nearest_char_boundary_before(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub(super) fn nearest_char_boundary_after(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

pub(super) fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

pub(super) fn date_filter_matches(value_ms: u64, filter: &DateFilter) -> bool {
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

pub(super) fn size_filter_label(filter: &SizeFilter) -> String {
    match filter {
        SizeFilter::Empty => "empty".to_string(),
        SizeFilter::Range { label, .. } => label.clone(),
    }
}

pub(super) fn format_size_label(bytes: u64) -> String {
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

pub(super) fn is_empty_folder_path(path: &Path) -> bool {
    fs::read_dir(path).map(|mut entries| entries.next().is_none()).unwrap_or(false)
}

pub(super) fn parse_year_token(token: &str) -> Option<i32> {
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

pub(super) fn token_extensions(token: &str) -> Option<Vec<String>> {
    if let Some(exts) = token_category_extensions(token) {
        return Some(exts);
    }
    if looks_like_extension(token) {
        return Some(normalize_extension_token(token));
    }
    None
}

pub(super) fn token_category_extensions(token: &str) -> Option<Vec<String>> {
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
        "presentation" | "presentations" | "slideshow" | "slides" | "powerpoint" => {
            Some(vec!["pptx".to_string(), "ppt".to_string(), "odp".to_string(), "key".to_string()])
        }
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
        "font" | "fonts" | "typeface" | "typefaces" => {
            Some(vec!["ttf".to_string(), "otf".to_string(), "woff".to_string(), "woff2".to_string(), "eot".to_string()])
        }
        "database" | "db" | "sql" => Some(vec![
            "sql".to_string(),
            "db".to_string(),
            "sqlite".to_string(),
            "sqlite3".to_string(),
            "mdb".to_string(),
            "accdb".to_string(),
        ]),
        "email" | "emails" | "mail" | "eml" => {
            Some(vec!["eml".to_string(), "msg".to_string(), "pst".to_string(), "mbox".to_string()])
        }
        "subtitle" | "subtitles" | "caption" | "captions" | "srt" => {
            Some(vec!["srt".to_string(), "vtt".to_string(), "ass".to_string(), "ssa".to_string(), "sub".to_string()])
        }
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
        "calendar" | "ics" | "vcal" => Some(vec!["ics".to_string(), "vcf".to_string()]),
        "vector" | "illustration" | "illustrator" => {
            Some(vec!["svg".to_string(), "ai".to_string(), "eps".to_string(), "cdr".to_string()])
        }
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
        "disk" | "iso" | "diskimage" => {
            Some(vec!["iso".to_string(), "img".to_string(), "vmdk".to_string(), "vhd".to_string(), "vhdx".to_string()])
        }
        _ => None,
    }
}

pub(super) fn normalize_extension_token(token: &str) -> Vec<String> {
    match token {
        "jpeg" => vec!["jpg".to_string(), "jpeg".to_string()],
        "htm" => vec!["htm".to_string(), "html".to_string()],
        "yml" => vec!["yml".to_string(), "yaml".to_string()],
        _ => vec![token.to_string()],
    }
}

pub(super) fn looks_like_extension(token: &str) -> bool {
    token.len() >= 2
        && token.len() <= 8
        && token.chars().all(|ch| ch.is_ascii_alphanumeric())
        && !is_natural_stopword(token)
}

pub(super) fn is_common_extension_token(token: &str) -> bool {
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

pub(super) fn is_year_filter_hint(token: &str) -> bool {
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

pub(super) fn is_natural_stopword(token: &str) -> bool {
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
