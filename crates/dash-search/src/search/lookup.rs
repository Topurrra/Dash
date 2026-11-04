//! Running searches: files, contents, the filename index.

use super::*;

pub(crate) fn search_local_files_inner(
    app: &std::sync::Arc<dyn dash_core::Host>,
    options: &FileSearchQueryOptions,
) -> Result<FileSearchQueryResult, String> {
    // Load the content index if available — file search merges content hits
    // when it exists. A failure here is NOT fatal: the most common cause is a
    // content-index rebuild in progress, and file search runs off the separate,
    // already-built filename index. Degrade to that instead of failing the
    // whole query with the content index's "rebuilding" message.
    let _ = ensure_engine_loaded(app);

    let query_text = options.query.trim().to_string();
    if query_text.is_empty() {
        return Err("Enter a search query".to_string());
    }

    let started_at = SystemTime::now();
    // High-precision timer for per-phase debug logging.
    let t_start = std::time::Instant::now();
    let limit = options.limit.unwrap_or(30).clamp(1, 200);
    let offset = options.offset.unwrap_or(0).min(20_000);
    let manual_extension_filters = parse_extension_filters(options.extension_filter.as_deref());
    let path_filter =
        options.path_filter.as_ref().map(|value| value.trim().to_lowercase()).filter(|value| !value.is_empty());

    // The filename index is searchable on its own — load it so file search
    // works even when no content index has been built (the common case for a
    // user who only set up the File Search tab).
    ensure_filename_engine_loaded(app);

    // Take what the query needs (all cheap clones) and release the engine
    // lock, so a long query doesn't block other searches or the indexer.
    let (searcher, index, fields) = {
        let guard = SEARCH_ENGINE.lock().map_err(|_| "Search engine lock failed".to_string())?;
        let Some(engine) = guard.as_ref() else {
            // No content index — serve file/name search from the standalone
            // filename index alone. Content search legitimately still requires the
            // content index and reports "not ready" through its own command.
            drop(guard);
            return search_local_files_via_filename_index(app, options, started_at);
        };
        // Reader auto-reloads via OnCommitWithDelay policy (background thread). The
        // previous explicit `reload_search_reader_if_due` was blocking the search
        // thread for 2-4 seconds after each watcher commit while it verified segments.
        // Cost of removal: newly-indexed files take ~5s to appear in search instead
        // of being instantly visible — acceptable trade for never blocking queries.
        (engine.reader.searcher(), engine.index.clone(), engine.fields)
    };
    let t_engine_ready = t_start.elapsed();

    // File search queries names and paths only — never the `content` field.
    // Including `content` made file search scale with indexed *full text*: on a
    // 67k-file content index a query like "Asura" took ~865 ms instead of the
    // tens of ms it costs against names alone. In-file text is the Content
    // Search tab's job, served by its own `search_file_contents` command.
    let mut parser = QueryParser::for_index(&index, vec![fields.file_name, fields.path]);

    let natural_language = options.natural_language.unwrap_or(true);
    if natural_language {
        // Favor file name and path matching in natural-language mode.
        //
        // Parser-level fuzzy (`set_field_fuzzy`) was previously enabled here but it
        // expanded *every* term in *every* query into an edit-distance-1 Levenshtein
        // automaton. For short prefixes like "fi", "fo", or "as", this expanded into
        // thousands of terms each becoming a sub-query, causing 70ms–11s query times.
        //
        // Typo tolerance is preserved by the dedicated Pass 3 (FuzzyTermQuery) which
        // only fires when exact matching returns zero results — much more efficient.
        parser.set_field_boost(fields.file_name, 2.3);
        parser.set_field_boost(fields.path, 1.35);
    } else {
        parser.set_conjunction_by_default();
    }

    let natural_plan =
        if natural_language { build_natural_query_plan(&query_text) } else { NaturalQueryPlan::default() };

    let natural_extension_filters = natural_plan.extension_filters.clone();
    let effective_extension_filters = if manual_extension_filters.is_empty() {
        natural_extension_filters.clone()
    } else if natural_extension_filters.is_empty() {
        manual_extension_filters.clone()
    } else {
        manual_extension_filters
            .iter()
            .filter(|value| natural_extension_filters.iter().any(|allowed| allowed == *value))
            .cloned()
            .collect::<Vec<_>>()
    };

    if !manual_extension_filters.is_empty()
        && !natural_extension_filters.is_empty()
        && effective_extension_filters.is_empty()
    {
        let took_ms = started_at.elapsed().map_or(0, |d| d.as_millis());
        return Ok(FileSearchQueryResult {
            query: query_text,
            total_hits: 0,
            returned: 0,
            took_ms,
            results: Vec::new(),
        });
    }

    let query_keywords =
        if natural_language { natural_plan.query_keywords.clone() } else { extract_query_keywords(&query_text) };
    let normalized_rank_query =
        normalize_launcher_text(if natural_language { &natural_plan.query_text } else { &query_text });

    let query = if natural_language {
        if let Some(natural_query) = build_natural_query_string(
            &natural_plan.query_text,
            &natural_plan.query_keywords,
            &effective_extension_filters,
        ) {
            let (parsed, _warnings) = parser.parse_query_lenient(&natural_query);
            parsed
        } else {
            Box::new(AllQuery)
        }
    } else {
        let strict_query = if effective_extension_filters.is_empty() {
            query_text.clone()
        } else {
            build_query_with_extension_filters(&query_text, &effective_extension_filters)
        };
        parser.parse_query(&strict_query).map_err(|error| format!("Invalid query: {error}"))?
    };
    let t_query_built = t_start.elapsed();

    // Name/path/metadata field view shared by `build_result_item` across the
    // combined index and (step 7c-ii) the filename index.
    let result_fields = fields.result_doc_fields();
    // Frecency snapshot — loaded once here so `build_result_item` can fuse it
    // into every result's score (Search #13). Loading it before the passes is
    // what lets the unified scorer see frecency, instead of it being bolted on
    // afterwards in a separate post-pass loop.
    let frecency = crate::frecency::snapshot(app);

    let fetch_results = |query: &dyn Query,
                         aggressive_fallback: bool,
                         use_fuzzy: bool|
     -> Result<(usize, Vec<FileSearchResultItem>), String> {
        let total_hits = searcher.search(query, &Count).map_err(|error| format!("Search failed: {error}"))?;

        let mut matched = Vec::new();
        let mut effective_total_hits = total_hits;
        let needs_post_filter = path_filter.is_some()
            || natural_plan.entry_type_filter.is_some()
            || natural_plan.date_filter.is_some()
            || natural_plan.size_filter.is_some();
        let needs_result_filter =
            needs_post_filter || aggressive_fallback || (natural_language && !query_keywords.is_empty());
        if needs_result_filter {
            let target_count = offset + limit;
            // Bound how many index docs the post-filtering scan walks. The scan
            // runs in descending score order, so the cap keeps the best-scored
            // candidates and only drops deeply-buried ones. Both caps exist
            // because the scan costs ~40 us/doc: a broad query (e.g. "a folder"
            // — the type filter passes few docs, so the scan never reaches
            // `target_count` and would otherwise run to the ceiling) took ~11 s
            // at the old 250k ceiling. 30k bounds the worst case to ~1 s; the
            // last-ditch aggressive AllQuery pass is capped tighter, at 8k.
            let multiplier = if aggressive_fallback { 20 } else { 8 };
            let cap = if aggressive_fallback { 8_000 } else { 4_000 };
            let page_size = target_count.saturating_mul(multiplier).clamp(300, cap);
            let max_scan = if aggressive_fallback {
                total_hits.min(8_000).max(target_count.saturating_mul(20))
            } else {
                total_hits.min(30_000).max(target_count)
            };
            let mut scan_offset = 0usize;

            while matched.len() < target_count && scan_offset < max_scan {
                let start = scan_offset;
                let end = (start + page_size).min(max_scan);
                let requested = end.saturating_sub(start);
                let top_docs = searcher
                    .search(query, &TopDocs::for_doc_range(start..end).order_by_score())
                    .map_err(|error| format!("Search failed: {error}"))?;

                if top_docs.is_empty() {
                    break;
                }

                let fetched = top_docs.len();
                scan_offset = start + fetched;

                for (score, address) in top_docs {
                    if let Some(item) = build_result_item(
                        &searcher,
                        &result_fields,
                        address,
                        score,
                        &query_keywords,
                        &normalized_rank_query,
                        natural_language,
                        path_filter.as_deref(),
                        &natural_plan,
                        &effective_extension_filters,
                        use_fuzzy,
                        &frecency,
                    )? {
                        matched.push(item);
                    }
                }

                if fetched < requested {
                    break;
                }
            }

            if needs_result_filter {
                effective_total_hits =
                    if matched.len() < target_count || scan_offset >= max_scan || scan_offset >= total_hits {
                        matched.len()
                    } else {
                        matched.len().saturating_add(page_size).min(total_hits)
                    };
            }
        } else {
            let fetch_limit = if aggressive_fallback {
                (offset + limit).saturating_mul(40).clamp(limit * 5, 20_000)
            } else {
                (offset + limit).saturating_mul(2).clamp(limit, 5_000)
            };
            let top_docs = searcher
                .search(query, &TopDocs::with_limit(fetch_limit).order_by_score())
                .map_err(|error| format!("Search failed: {error}"))?;

            for (score, address) in top_docs {
                if let Some(item) = build_result_item(
                    &searcher,
                    &result_fields,
                    address,
                    score,
                    &query_keywords,
                    &normalized_rank_query,
                    natural_language,
                    path_filter.as_deref(),
                    &natural_plan,
                    &effective_extension_filters,
                    use_fuzzy,
                    &frecency,
                )? {
                    matched.push(item);
                }
            }
        }

        Ok((effective_total_hits, matched))
    };

    // Pass 0: native Tantivy — date/size/entry-type filters expressed as RangeQuery/TermQuery
    // so Tantivy skips disqualified segments via FAST fields before we ever see the doc.
    // Only fires when at least one structured filter is present; falls through on 0 results.
    let mut pass0_fired = false;
    let mut pass1_fired = false;
    let mut pass1_5_fired = false;
    let mut pass2_fired = false;
    let mut pass3_fired = false;
    let (mut total_hits, mut matched) = {
        let mut native_result = None;
        if natural_language {
            if let Some(native_query) =
                build_native_tantivy_query(&query_keywords, &effective_extension_filters, &natural_plan, &fields)
            {
                pass0_fired = true;
                let (t, m) = fetch_results(native_query.as_ref(), false, false)?;
                if !m.is_empty() {
                    native_result = Some((t, m));
                }
            }
        }
        if let Some(r) = native_result {
            r
        } else {
            pass1_fired = true;
            fetch_results(query.as_ref(), false, false)?
        }
    };
    let t_pass_main = t_start.elapsed();

    // Pass 1.5: prefix range query. When exact term matching (Pass 1) returns nothing,
    // try matching every indexed term that *starts with* each keyword. The term
    // dictionary is FST-backed, so this is essentially a free lookup — single-digit
    // millisecond worst case, vs Pass 2's 200ms AllQuery scan.
    //
    // This catches the common "as-you-type" pattern: user typed "spi" but the index
    // contains "spider" as a token. Pass 1's exact match misses it; Pass 1.5's
    // [spi, spj) range hits it instantly.
    if matched.is_empty() && natural_language && !query_keywords.is_empty() && natural_plan.date_filter.is_none() {
        if let Some(prefix_query) = build_prefix_tantivy_query(&query_keywords, fields.file_name, fields.path) {
            pass1_5_fired = true;
            let (prefix_total, prefix_matches) = fetch_results(prefix_query.as_ref(), false, false)?;
            if !prefix_matches.is_empty() {
                total_hits = prefix_total;
                matched = prefix_matches;
            }
        }
    }
    let t_pass_prefix = t_start.elapsed();

    // Option B query routing (Phase 3, step 7c): merge whole-disk name/path
    // matches from the standalone filename index, deduped by path. This runs
    // BEFORE the expensive content-index fallbacks below: the filename index
    // usually already holds what file search is after, so folding it in here
    // means Pass 2 (an up-to-8k-doc AllQuery scan) and the Pass 3 fuzzy walk
    // fire only when nothing matched anywhere — not fruitlessly on every query
    // whose answer is a non-document file (the content index holds readable
    // documents only, so e.g. a video named "Asura" is never in it). When the
    // filename index is absent this is skipped, identical to the pre-7c path.
    if let Ok(filename_guard) = FILENAME_ENGINE.lock() {
        if let Some(filename_handle) = filename_guard.as_ref() {
            let filename_fetch_limit = (offset + limit).saturating_mul(8).clamp(limit, 8_000);
            if let Ok(filename_matches) = search_filename_index(
                filename_handle,
                natural_language,
                &natural_plan,
                &query_text,
                &query_keywords,
                &normalized_rank_query,
                &effective_extension_filters,
                path_filter.as_deref(),
                filename_fetch_limit,
                &frecency,
            ) {
                let seen: std::collections::HashSet<String> =
                    matched.iter().map(|item| item.path.to_lowercase()).collect();
                for item in filename_matches {
                    if !seen.contains(&item.path.to_lowercase()) {
                        matched.push(item);
                    }
                }
                total_hits = total_hits.max(matched.len());
            }
            // A filename-index query error is non-fatal — the content-index
            // results already in `matched` still stand.
        }
    }

    if matched.is_empty()
        && natural_language
        && !query_keywords.is_empty()
        && natural_plan.date_filter.is_none()
        && should_run_aggressive_file_fallback(
            &natural_plan,
            &query_keywords,
            &effective_extension_filters,
            path_filter.as_deref(),
        )
    {
        pass2_fired = true;
        let all_docs_query = AllQuery;
        let (fallback_total_hits, fallback_matches) = fetch_results(&all_docs_query, true, false)?;
        if !fallback_matches.is_empty() {
            total_hits = fallback_total_hits;
            matched = fallback_matches;
        }
    }
    let t_pass_fallback = t_start.elapsed();

    // Fuzzy pass: when exact + aggressive both returned nothing, use FuzzyTermQuery (edit-distance 1).
    // This is the Rust API path — much more reliable than the string ~1 syntax.
    if matched.is_empty()
        && natural_language
        && !query_keywords.is_empty()
        && natural_plan.date_filter.is_none()
        && natural_plan.exact_phrases.is_empty()
    {
        if let Some(fuzzy_query) = build_fuzzy_tantivy_query(&query_keywords, fields.file_name, fields.path) {
            pass3_fired = true;
            let (fuzzy_total, fuzzy_matches) = fetch_results(fuzzy_query.as_ref(), false, true)?;
            if !fuzzy_matches.is_empty() {
                total_hits = fuzzy_total;
                matched = fuzzy_matches;
            }
        }
    }
    let t_pass_fuzzy = t_start.elapsed();

    matched.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file_name.cmp(&b.file_name))
            .then_with(|| a.path.cmp(&b.path))
    });

    let results = matched.into_iter().skip(offset).take(limit).collect::<Vec<_>>();
    let took_ms = started_at.elapsed().map_or(0, |d| d.as_millis());
    let t_total = t_start.elapsed();

    // Per-phase debug timing — printed to stderr, visible in `cargo tauri dev` console.
    // Format chosen so each line is grep-able. Remove or gate behind env var when done measuring.
    #[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "debug timings in ms; rounding is fine")]
    let us = |d: std::time::Duration| (d.as_micros() as f64) / 1000.0;
    let pass_main_ms = us(t_pass_main) - us(t_query_built);
    let pass_prefix_ms = us(t_pass_prefix) - us(t_pass_main);
    let pass_fallback_ms = us(t_pass_fallback) - us(t_pass_prefix);
    let pass_fuzzy_ms = us(t_pass_fuzzy) - us(t_pass_fallback);
    let post_ms = us(t_total) - us(t_pass_fuzzy);
    eprintln!(
        "[search] q={:?} nl={} hits={} returned={} | engine_ready={:.2}ms plan+parse={:.2}ms main={:.2}ms (p0={} p1={}) prefix={:.2}ms (p1.5={}) fallback={:.2}ms (p2={}) fuzzy={:.2}ms (p3={}) post={:.2}ms TOTAL={:.2}ms",
        query_text,
        natural_language,
        total_hits,
        results.len(),
        us(t_engine_ready),
        us(t_query_built) - us(t_engine_ready),
        pass_main_ms,
        pass0_fired,
        pass1_fired,
        pass_prefix_ms,
        pass1_5_fired,
        pass_fallback_ms,
        pass2_fired,
        pass_fuzzy_ms,
        pass3_fired,
        post_ms,
        us(t_total),
    );

    Ok(FileSearchQueryResult { query: query_text, total_hits, returned: results.len(), took_ms, results })
}

/// Build a fallback query string that decompounds joined keywords. A keyword
/// like "spiderman" is split at every interior point into two parts of at
/// least 3 characters, and each split becomes an `(a AND b)` clause — so the
/// query can match a file indexed as the split tokens "spider" + "man" (from
/// "spider-man"). Clauses for different keywords are AND-ed. Returns `None`
/// when no keyword is long enough to split usefully.
pub(super) fn build_decompound_query_string(keywords: &[String]) -> Option<String> {
    const MIN_PART: usize = 3;

    let mut clauses: Vec<String> = Vec::new();
    let mut any_decompounded = false;

    for keyword in keywords {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            continue;
        }
        let chars: Vec<char> = keyword.chars().collect();
        let decompoundable = chars.len() >= MIN_PART * 2 && chars.iter().all(|c| c.is_alphanumeric());
        if !decompoundable {
            clauses.push(keyword.to_string());
            continue;
        }
        let mut splits: Vec<String> = Vec::new();
        for cut in MIN_PART..=(chars.len() - MIN_PART) {
            let head: String = chars[..cut].iter().collect();
            let tail: String = chars[cut..].iter().collect();
            splits.push(format!("({head} AND {tail})"));
        }
        any_decompounded = true;
        clauses.push(format!("({})", splits.join(" OR ")));
    }

    if !any_decompounded || clauses.is_empty() {
        return None;
    }
    Some(clauses.join(" AND "))
}

/// The whole-disk name/path half of Option B query routing (Phase 3, step 7c).
/// Searches the standalone filename index and returns ranked result items; the
/// combined index keeps serving the content half. Reuses the shared query
/// builders and `build_result_item`. v1 uses a straight top-N fetch per pass
/// (no cap-scan) — the combined index carries the rigorous filtered passes, the
/// filename index adds whole-disk breadth.
#[expect(clippy::too_many_arguments, reason = "each argument is a separate part of one search request")]
pub(super) fn search_filename_index(
    handle: &FilenameIndexHandle,
    natural_language: bool,
    natural_plan: &NaturalQueryPlan,
    query_text: &str,
    query_keywords: &[String],
    normalized_rank_query: &str,
    effective_extension_filters: &[String],
    path_filter: Option<&str>,
    fetch_limit: usize,
    frecency: &crate::frecency::FrecencySnapshot,
) -> Result<Vec<FileSearchResultItem>, String> {
    let searcher = handle.reader.searcher();
    let result_fields = handle.fields.result_doc_fields();

    let mut parser = QueryParser::for_index(&handle.index, vec![handle.fields.file_name, handle.fields.path]);
    if natural_language {
        parser.set_field_boost(handle.fields.file_name, 2.3);
        parser.set_field_boost(handle.fields.path, 1.35);
    } else {
        parser.set_conjunction_by_default();
    }

    let main_query: Box<dyn Query> = if natural_language {
        match build_natural_query_string(
            &natural_plan.query_text,
            &natural_plan.query_keywords,
            effective_extension_filters,
        ) {
            Some(natural_query) => parser.parse_query_lenient(&natural_query).0,
            None => Box::new(AllQuery),
        }
    } else {
        let strict_query = if effective_extension_filters.is_empty() {
            query_text.to_string()
        } else {
            build_query_with_extension_filters(query_text, effective_extension_filters)
        };
        match parser.parse_query(&strict_query) {
            Ok(parsed) => parsed,
            Err(_) => return Ok(Vec::new()),
        }
    };

    let run = |query: &dyn Query, use_fuzzy: bool| -> Result<Vec<FileSearchResultItem>, String> {
        let top_docs = searcher
            .search(query, &TopDocs::with_limit(fetch_limit).order_by_score())
            .map_err(|error| format!("Filename index search failed: {error}"))?;
        let mut items = Vec::new();
        for (score, address) in top_docs {
            if let Some(item) = build_result_item(
                &searcher,
                &result_fields,
                address,
                score,
                query_keywords,
                normalized_rank_query,
                natural_language,
                path_filter,
                natural_plan,
                effective_extension_filters,
                use_fuzzy,
                frecency,
            )? {
                items.push(item);
            }
        }
        Ok(items)
    };

    let mut matched = run(main_query.as_ref(), false)?;

    // Prefix fallback (as-you-type): match indexed terms that start with each
    // keyword when the exact-term query found nothing.
    if matched.is_empty() && natural_language && !query_keywords.is_empty() && natural_plan.date_filter.is_none() {
        if let Some(prefix_query) =
            build_prefix_tantivy_query(query_keywords, handle.fields.file_name, handle.fields.path)
        {
            matched = run(prefix_query.as_ref(), false)?;
        }
    }

    // Fuzzy fallback (edit-distance 1) when exact + prefix both found nothing.
    if matched.is_empty()
        && natural_language
        && !query_keywords.is_empty()
        && natural_plan.date_filter.is_none()
        && natural_plan.exact_phrases.is_empty()
    {
        if let Some(fuzzy_query) =
            build_fuzzy_tantivy_query(query_keywords, handle.fields.file_name, handle.fields.path)
        {
            matched = run(fuzzy_query.as_ref(), true)?;
        }
    }

    // Decompound fallback — a query keyword may be a joined compound
    // ("spiderman") whose on-disk target is indexed as split tokens ("spider"
    // + "man", from "spider-man"). When every pass above came up empty, split
    // each long keyword and require all of its parts.
    if matched.is_empty()
        && natural_language
        && !query_keywords.is_empty()
        && natural_plan.date_filter.is_none()
        && natural_plan.exact_phrases.is_empty()
    {
        if let Some(decompound) = build_decompound_query_string(query_keywords) {
            let (parsed, _warnings) = parser.parse_query_lenient(&decompound);
            matched = run(parsed.as_ref(), false)?;
        }
    }

    Ok(matched)
}

/// Lazily open and cache the standalone filename index in `FILENAME_ENGINE`.
/// A no-op once loaded; the filename worker monitor refreshes the handle after
/// every rebuild, and the `OnCommitWithDelay` reader picks up the tailer's /
/// watcher's live commits on its own.
pub(super) fn ensure_filename_engine_loaded(app: &std::sync::Arc<dyn dash_core::Host>) {
    if FILENAME_BUILD_RUNNING.load(Ordering::Relaxed) {
        return;
    }
    {
        let Ok(guard) = FILENAME_ENGINE.lock() else {
            return;
        };
        if guard.is_some() {
            return;
        }
    }
    let Ok(state_dir) = search_index_dir(app) else {
        return;
    };
    let handle = try_open_filename_index(&state_dir);
    if handle.is_none() {
        return;
    }
    if let Ok(mut guard) = FILENAME_ENGINE.lock() {
        if guard.is_none() {
            *guard = handle;
        }
    }
}

/// Live document count of the cached filename index — 0 when it is not loaded.
/// Surfaced on the File Search tab as the filename index's "indexed files".
pub(super) fn filename_index_num_docs() -> u64 {
    let Ok(guard) = FILENAME_ENGINE.lock() else {
        return 0;
    };
    let Some(handle) = guard.as_ref() else {
        return 0;
    };
    handle.reader.searcher().num_docs()
}

/// Merge manually-typed and naturally-inferred extension filters. `None` means
/// the two non-empty sets are disjoint — a query that can match nothing, which
/// the caller answers with an empty result set.
pub(super) fn resolve_extension_filters(manual: &[String], natural: &[String]) -> Option<Vec<String>> {
    if manual.is_empty() {
        return Some(natural.to_vec());
    }
    if natural.is_empty() {
        return Some(manual.to_vec());
    }
    let intersection: Vec<String> =
        manual.iter().filter(|value| natural.iter().any(|allowed| allowed == *value)).cloned().collect();
    if intersection.is_empty() {
        None
    } else {
        Some(intersection)
    }
}

/// File/name search served from the standalone filename index alone — used when
/// no content index exists. The File Search tab and the Content Search tab
/// build independent indexes, so a user who only set up filename search must
/// still get results here instead of "Search index is not ready."
pub(super) fn search_local_files_via_filename_index(
    app: &std::sync::Arc<dyn dash_core::Host>,
    options: &FileSearchQueryOptions,
    started_at: SystemTime,
) -> Result<FileSearchQueryResult, String> {
    let query_text = options.query.trim().to_string();
    let limit = options.limit.unwrap_or(30).clamp(1, 200);
    let offset = options.offset.unwrap_or(0).min(20_000);
    let manual_extension_filters = parse_extension_filters(options.extension_filter.as_deref());
    let path_filter =
        options.path_filter.as_ref().map(|value| value.trim().to_lowercase()).filter(|value| !value.is_empty());
    let natural_language = options.natural_language.unwrap_or(true);
    let natural_plan =
        if natural_language { build_natural_query_plan(&query_text) } else { NaturalQueryPlan::default() };

    let Some(effective_extension_filters) =
        resolve_extension_filters(&manual_extension_filters, &natural_plan.extension_filters)
    else {
        return Ok(FileSearchQueryResult {
            query: query_text,
            total_hits: 0,
            returned: 0,
            took_ms: started_at.elapsed().map_or(0, |d| d.as_millis()),
            results: Vec::new(),
        });
    };

    let query_keywords =
        if natural_language { natural_plan.query_keywords.clone() } else { extract_query_keywords(&query_text) };
    let normalized_rank_query =
        normalize_launcher_text(if natural_language { &natural_plan.query_text } else { &query_text });

    // Frecency snapshot — loaded once so `search_filename_index` can fuse it
    // into every result's score (Search #13).
    let frecency = crate::frecency::snapshot(app);
    let mut matched = {
        let guard = FILENAME_ENGINE.lock().map_err(|_| "Filename engine lock failed".to_string())?;
        let handle = guard.as_ref().ok_or_else(|| "Search index is not ready yet. Build it first.".to_string())?;
        let fetch_limit = (offset + limit).saturating_mul(8).clamp(limit, 8_000);
        search_filename_index(
            handle,
            natural_language,
            &natural_plan,
            &query_text,
            &query_keywords,
            &normalized_rank_query,
            &effective_extension_filters,
            path_filter.as_deref(),
            fetch_limit,
            &frecency,
        )?
    };

    matched.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file_name.cmp(&b.file_name))
            .then_with(|| a.path.cmp(&b.path))
    });

    let total_hits = matched.len();
    let results = matched.into_iter().skip(offset).take(limit).collect::<Vec<_>>();
    let took_ms = started_at.elapsed().map_or(0, |d| d.as_millis());
    Ok(FileSearchQueryResult { query: query_text, total_hits, returned: results.len(), took_ms, results })
}

pub(super) fn search_file_contents_inner(
    app: &std::sync::Arc<dyn dash_core::Host>,
    options: &FileSearchQueryOptions,
) -> Result<ContentSearchQueryResult, String> {
    ensure_engine_loaded(app)?;

    let query_text = options.query.trim().to_string();
    if query_text.is_empty() {
        return Err("Enter text to search inside indexed files".to_string());
    }

    let started_at = SystemTime::now();
    let limit = options.limit.unwrap_or(30).clamp(1, 100);
    let offset = options.offset.unwrap_or(0).min(20_000);
    let extension_filters = parse_extension_filters(options.extension_filter.as_deref());
    let path_filter =
        options.path_filter.as_ref().map(|value| value.trim().to_lowercase()).filter(|value| !value.is_empty());

    // Take what the query needs (all cheap clones) and release the engine
    // lock before embedding, scoring and snippet work.
    let (searcher, index, fields, index_content, max_content_kb, semantic_search_enabled) = {
        let guard = SEARCH_ENGINE.lock().map_err(|_| "Search engine lock failed".to_string())?;
        let engine = guard.as_ref().ok_or_else(|| "Search index is not ready yet. Build it first.".to_string())?;
        // Reader auto-reloads via OnCommitWithDelay; no explicit reload needed.
        (
            engine.reader.searcher(),
            engine.index.clone(),
            engine.fields,
            engine.options.index_content,
            engine.options.max_content_kb,
            engine.options.semantic_search_enabled,
        )
    };

    if !index_content {
        return Err("Content indexing is disabled. Enable content indexing and rebuild the search index.".to_string());
    }

    let max_content_bytes = usize::try_from(max_content_kb.unwrap_or(DEFAULT_MAX_CONTENT_KB))
        .unwrap_or(usize::MAX)
        .saturating_mul(1024)
        .clamp(8 * 1024, 2 * 1024 * 1024);

    let natural_language = options.natural_language.unwrap_or(true);

    // Build a natural-language plan: extracts date/size/extension filters from
    // queries like "tax docs from 2023" or "invoices > 1mb" so content search
    // gets the same structured intent layer as file search.
    let natural_plan =
        if natural_language { build_natural_query_plan(&query_text) } else { NaturalQueryPlan::default() };

    // Combine manual and NL-derived extension filters.
    let natural_extension_filters = natural_plan.extension_filters.clone();
    let effective_extension_filters = if extension_filters.is_empty() {
        natural_extension_filters.clone()
    } else if natural_extension_filters.is_empty() {
        extension_filters.clone()
    } else {
        extension_filters
            .iter()
            .filter(|v| natural_extension_filters.iter().any(|a| a == *v))
            .cloned()
            .collect::<Vec<_>>()
    };
    if !extension_filters.is_empty() && !natural_extension_filters.is_empty() && effective_extension_filters.is_empty()
    {
        let took_ms = started_at.elapsed().map_or(0, |d| d.as_millis());
        return Ok(ContentSearchQueryResult {
            query: query_text,
            total_hits: 0,
            returned: 0,
            took_ms,
            results: Vec::new(),
            // This early-out (contradictory ext filters) is before the semantic
            // pass; nothing ran, so it's inactive for this query.
            semantic_active: false,
        });
    }

    // Keywords drive snippet generation and match counting; cleaned for highlighting.
    let query_keywords =
        if natural_language { natural_plan.query_keywords.clone() } else { extract_query_keywords(&query_text) };

    // Build the base content query. parse_query_lenient already supports phrases
    // ("quarterly earnings") and boolean operators (AND/OR/NOT, +/-).
    let content_query_text = if natural_language { natural_plan.query_text.clone() } else { query_text.clone() };
    let mut parser = QueryParser::for_index(&index, vec![fields.content]);
    if !natural_language {
        parser.set_conjunction_by_default();
    }
    let base_query: Box<dyn Query> = if natural_language {
        let (parsed, _warnings) = parser.parse_query_lenient(&content_query_text);
        parsed
    } else {
        parser.parse_query(&content_query_text).map_err(|error| format!("Invalid content query: {error}"))?
    };

    // Frecency snapshot — loaded once so `build_content_result_item` can fuse
    // open-history into each result's score (Search #13).
    let frecency = crate::frecency::snapshot(app);

    // Semantic search (beta): embed the query and find the stored vectors closest
    // in meaning. `semantic_scores` (path_lower → cosine) re-ranks keyword hits;
    // `semantic_candidates` ((path, cosine), original case) are the top matches to
    // also surface as pure-semantic results below. Both empty — so content ranking
    // is byte-identical to before — unless the toggle is on AND the embedder is
    // compiled in (`semantic` feature) and its model loaded. `embed` returns
    // `None` in builds without the feature, keeping this a no-op there.
    // Deep pagination: fetch enough semantic candidates to cover the requested
    // page (offset + limit), plus a small buffer, capped so a very deep page
    // can't trigger an unbounded cosine scan. Because semantic-only hits are
    // then merged into the same ranked pool as keyword hits before pagination,
    // meaning-only results page through like everything else — not just page 1.
    // ponytail: capped at 1000; a page past that many pure-semantic hits stops
    // deepening — raise the cap or stream if a corpus ever needs it.
    let semantic_top_k = (offset + limit + 20).min(1000);
    let mut semantic_scores: HashMap<String, f32> = HashMap::new();
    let mut semantic_active = false;
    let semantic_candidates: Vec<(String, f32)> = if semantic_search_enabled && crate::embedding::is_available() {
        match crate::embedding::embed(&query_text).zip(search_index_dir(app).ok()) {
            Some((qvec, state_dir)) => {
                // The meaning pass genuinely ran (toggle on, model loaded, query
                // embedded) — record it so the UI can show "semantic on" even when
                // this query yields no meaning-only hits.
                semantic_active = true;
                let vec_path = crate::vector_cache::cache_path_for_dir(&state_dir);
                let hits = crate::vector_cache::top_k(&vec_path, &qvec, semantic_top_k);
                for (path, score) in &hits {
                    semantic_scores.insert(path.to_lowercase(), *score);
                }
                hits
            }
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    // fetch_results: runs a query, walks results with post-filtering, hydrates
    // each into a ContentSearchResultItem. Returns (effective_total, items).
    let fetch_results = |query: &dyn Query| -> Result<(usize, Vec<ContentSearchResultItem>), String> {
        let total_hits = searcher.search(query, &Count).map_err(|error| format!("Content search failed: {error}"))?;
        let needs_post_filter = path_filter.is_some() || !effective_extension_filters.is_empty();
        let mut matched: Vec<ContentSearchResultItem> = Vec::new();
        let mut scanned = 0usize;
        let target_count = offset + limit;
        let page_size = target_count.saturating_mul(6).clamp(200, 3_000);
        let max_scan = if needs_post_filter {
            total_hits.min(20_000).max(target_count)
        } else {
            total_hits.min(target_count + 200).max(target_count)
        };

        while matched.len() < target_count && scanned < max_scan {
            let start = scanned;
            let end = (start + page_size).min(max_scan);
            let requested = end.saturating_sub(start);
            let top_docs = searcher
                .search(query, &TopDocs::for_doc_range(start..end).order_by_score())
                .map_err(|error| format!("Content search failed: {error}"))?;
            if top_docs.is_empty() {
                break;
            }
            let fetched = top_docs.len();
            scanned = start + fetched;
            for (score, address) in top_docs {
                if let Some(item) = build_content_result_item(
                    &searcher,
                    &fields,
                    address,
                    score,
                    &query_keywords,
                    path_filter.as_deref(),
                    &effective_extension_filters,
                    max_content_bytes,
                    &frecency,
                    &semantic_scores,
                    false, // keyword pass — not a pure-semantic hit
                )? {
                    matched.push(item);
                }
            }
            if fetched < requested {
                break;
            }
        }
        let effective_total = if needs_post_filter {
            if matched.len() < target_count || scanned >= total_hits {
                matched.len()
            } else {
                matched.len().saturating_add(page_size).min(total_hits)
            }
        } else {
            total_hits
        };
        Ok((effective_total, matched))
    };

    // ───── Multi-pass content search ─────

    // Pass 1: lenient base query (handles phrases + boolean ops natively).
    let (mut total_hits, mut matched) = fetch_results(base_query.as_ref())?;

    // Pass 1.5: prefix-range query for partial token matches. Hits when the
    // user typed a partial word ("invest") and content has the full form
    // ("investment"). FST-backed, single-digit ms cost.
    if matched.is_empty() && natural_language && !query_keywords.is_empty() {
        if let Some(prefix_query) = build_content_prefix_query(&query_keywords, fields.content) {
            let (prefix_total, prefix_matches) = fetch_results(prefix_query.as_ref())?;
            if !prefix_matches.is_empty() {
                total_hits = prefix_total;
                matched = prefix_matches;
            }
        }
    }

    // Pass 3: fuzzy edit-distance-1 for typo tolerance on longer tokens.
    if matched.is_empty() && natural_language && !query_keywords.is_empty() && natural_plan.exact_phrases.is_empty() {
        if let Some(fuzzy_query) = build_content_fuzzy_query(&query_keywords, fields.content) {
            let (fuzzy_total, fuzzy_matches) = fetch_results(fuzzy_query.as_ref())?;
            if !fuzzy_matches.is_empty() {
                total_hits = fuzzy_total;
                matched = fuzzy_matches;
            }
        }
    }

    // Semantic-only candidates: docs the vector pass rated similar that NONE of
    // the keyword passes surfaced (they share no literal query terms — the whole
    // point of semantic search). Fetch each by exact path and fuse it in with its
    // semantic score (BM25 = 0), so it ranks below real keyword hits but is still
    // found. Bounded by semantic_top_k → at most K extra single-doc lookups.
    // ponytail: these augment the current page only; they aren't deep-paginated.
    if !semantic_candidates.is_empty() {
        let existing: std::collections::HashSet<String> = matched.iter().map(|item| item.path.to_lowercase()).collect();
        for (path, _score) in &semantic_candidates {
            let path_lower = path.to_lowercase();
            if existing.contains(&path_lower) {
                continue;
            }
            if let Some(filter) = &path_filter {
                if !path_lower.contains(filter.as_str()) {
                    continue;
                }
            }
            let Some(address) = content_doc_address_for_path(&searcher, &fields, path) else {
                continue; // vector store had a path the index no longer holds (deleted) — skip
            };
            if let Ok(Some(item)) = build_content_result_item(
                &searcher,
                &fields,
                address,
                0.0, // pure semantic hit — no BM25 relevance
                &query_keywords,
                path_filter.as_deref(),
                &effective_extension_filters,
                max_content_bytes,
                &frecency,
                &semantic_scores,
                true, // pure-semantic candidate — surfaced by meaning only
            ) {
                matched.push(item);
                total_hits += 1;
            }
        }
    }

    matched.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file_name.cmp(&b.file_name))
            .then_with(|| a.path.cmp(&b.path))
    });

    let results = matched.into_iter().skip(offset).take(limit).collect::<Vec<_>>();
    let took_ms = started_at.elapsed().map_or(0, |d| d.as_millis());

    Ok(ContentSearchQueryResult {
        query: query_text,
        total_hits,
        returned: results.len(),
        took_ms,
        results,
        semantic_active,
    })
}

/// Open the active file-search index and build its reader. Returns an `Err`
/// (rather than panicking) if the on-disk index is unreadable — `Index::open`,
/// `extract_fields`, or the reader build all fail on a corrupted index. The
/// caller turns that error into a quarantine + rebuild prompt.
pub(super) fn try_open_active_index(index_dir: &Path) -> Result<(Index, SearchFields, IndexReader), String> {
    let directory =
        MmapDirectory::open(index_dir).map_err(|error| format!("Cannot open search index directory: {error}"))?;
    let index = Index::open(directory).map_err(|error| format!("Cannot open search index: {error}"))?;
    let schema = index.schema();
    let fields = extract_fields(&schema)?;
    let reader: IndexReader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()
        .map_err(|error| format!("Cannot create search reader: {error}"))?;
    reader.reload().map_err(|error| format!("Cannot load search reader: {error}"))?;
    Ok((index, fields, reader))
}

/// Move a corrupt active index out of the way so the next rebuild starts
/// clean. Tries a rename to a timestamped `<prefix>*` sibling first (preserves
/// it for diagnostics); if that fails — e.g. a stale mmap still holds the
/// directory — deletes it outright. Returns whether the corrupt index is no
/// longer in the active slot. `corrupt_prefix` distinguishes the content
/// index (`index-corrupt-`) from the filename index (`filename-corrupt-`).
pub(super) fn quarantine_corrupt_index(index_dir: &Path, corrupt_prefix: &str) -> bool {
    if let Some(parent) = index_dir.parent() {
        let quarantine = parent.join(format!("{corrupt_prefix}{}", unix_now_ms()));
        if fs::rename(index_dir, &quarantine).is_ok() {
            return true;
        }
    }
    fs::remove_dir_all(index_dir).is_ok()
}

/// Open the filename index from its active slot. `Err` means the directory
/// exists but is unreadable — i.e. on-disk corruption — which the caller
/// turns into a quarantine. `try_open_active_index` is the content-index twin.
pub(super) fn open_active_filename_index(dir: &Path) -> Result<FilenameIndexHandle, String> {
    let directory =
        MmapDirectory::open(dir).map_err(|error| format!("Cannot open filename index directory: {error}"))?;
    let index = Index::open(directory).map_err(|error| format!("Cannot open filename index: {error}"))?;
    let schema = index.schema();
    let fields = extract_filename_index_fields(&schema)?;
    let reader: IndexReader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()
        .map_err(|error| format!("Cannot create filename index reader: {error}"))?;
    reader.reload().map_err(|error| format!("Cannot load filename index reader: {error}"))?;
    Ok(FilenameIndexHandle { index, reader, fields })
}

/// Open the filename index if it exists and is readable. Returns `None` when
/// the index has not been built yet (installs predating step 6b). A corrupt
/// filename index is quarantined to a `filename-corrupt-*` sibling and `None`
/// is returned — search degrades gracefully to the content index until the
/// next rebuild recreates it, so no user-facing error is raised.
pub(super) fn try_open_filename_index(state_dir: &Path) -> Option<FilenameIndexHandle> {
    let dir = active_filename_index_dir_for_state(state_dir);
    if !dir.exists() {
        return None;
    }
    if let Ok(handle) = open_active_filename_index(&dir) {
        Some(handle)
    } else {
        quarantine_corrupt_index(&dir, FILENAME_CORRUPT_INDEX_PREFIX);
        None
    }
}
