use super::*;

/// A child that never answers is killed at the per-file timeout, and one
/// that exits is reported as died — neither blocks the caller.
#[cfg(windows)]
#[test]
fn watched_child_times_out_and_detects_death() {
    use std::os::windows::process::CommandExt;
    let spawn = |script: &str| {
        let mut child = Command::new("cmd")
            .args(["/c", script])
            .creation_flags(0x0800_0000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("cmd runs");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        WatchedChild::new(child, stdin, stdout)
    };
    // Silent for 30 s (ping's own output goes to nul, so only cmd
    // holds our pipe and the kill closes it).
    let mut hung = spawn("ping -n 30 127.0.0.1 >nul");
    let started = std::time::Instant::now();
    let answer = hung.extract(Path::new(r"C:\x.pdf"), std::time::Duration::from_millis(500));
    assert!(matches!(answer, ExtractAnswer::TimedOut));
    assert!(started.elapsed() < std::time::Duration::from_secs(10), "took {:?}", started.elapsed());
    drop(hung);

    let mut dead = spawn("exit 0");
    let answer = dead.extract(Path::new(r"C:\x.pdf"), std::time::Duration::from_secs(20));
    assert!(matches!(answer, ExtractAnswer::Died));
}

#[test]
fn reconcile_spares_entries_under_offline_roots() {
    let present = std::env::temp_dir().to_string_lossy().into_owned();
    let missing = r"Q:\dash-test-unplugged-drive\Data\".to_string();
    let offline = offline_roots(&[present, missing]);
    assert_eq!(offline, vec![r"q:\dash-test-unplugged-drive\data".to_string()]);
    let root = &offline[0];
    assert!(is_under_root(r"q:\dash-test-unplugged-drive\data", root));
    assert!(is_under_root(r"q:\dash-test-unplugged-drive\data\a\b.txt", root));
    assert!(!is_under_root(r"q:\dash-test-unplugged-drive\database.txt", root));
    assert!(is_under_root(r"c:\x", "c:"));
    assert!(!is_confirmed_gone(&std::env::temp_dir()));
    assert!(is_confirmed_gone(&std::env::temp_dir().join("dash-test-surely-missing-file")));
}

#[test]
fn extractor_timeout_covers_ocr() {
    use super::super::text_extract::OcrChildEnv;
    assert_eq!(OcrChildEnv::default().extract_timeout().as_secs(), 60);
    let ocr = OcrChildEnv::from_options(true, &[], 20, "eng", 0, 0.0, 30);
    assert_eq!(ocr.extract_timeout().as_secs(), 60 + 20 * 30);
}

#[test]
fn sandboxed_extraction_reads_plain_text_and_skips_unknown() {
    let dir = std::env::temp_dir().join(format!("dash-sandbox-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let txt = dir.join("note.txt");
    std::fs::write(&txt, "hello preview").unwrap();
    assert_eq!(extract_text_sandboxed(&txt, 1024).as_deref(), Some("hello preview"));
    let bin = dir.join("tool.exe");
    std::fs::write(&bin, b"MZ\0\0").unwrap();
    assert_eq!(extract_text_sandboxed(&bin, 1024), None);
    let _ = std::fs::remove_dir_all(&dir);
}

fn day_ms(days: u64) -> u64 {
    days * 24 * 60 * 60 * 1000
}

/// The name index finds what ignore files exclude; the content index doesn't.
#[test]
fn name_walker_ignores_ignore_files_content_walker_honours_them() {
    let root = std::env::temp_dir().join(format!("dash-walker-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(".ignore"), "secret.txt\n").unwrap();
    fs::write(root.join("secret.txt"), "x").unwrap();
    fs::write(root.join("plain.txt"), "x").unwrap();
    let options: FileSearchIndexOptions =
        serde_json::from_value(serde_json::json!({ "roots": [], "includeHidden": false, "indexContent": true }))
            .unwrap();
    let names = |honour: bool| -> Vec<String> {
        index_walker(&root.display().to_string(), &options, honour)
            .build()
            .flatten()
            .filter_map(|e| e.path().file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect()
    };
    let for_names = names(false);
    let for_content = names(true);
    assert!(for_names.contains(&"secret.txt".to_string()), "{for_names:?}");
    assert!(for_names.contains(&"plain.txt".to_string()));
    assert!(!for_content.contains(&"secret.txt".to_string()), "{for_content:?}");
    assert!(for_content.contains(&"plain.txt".to_string()));
    let _ = fs::remove_dir_all(&root);
}

/// Excluded folders are not entered at all, not just filtered file by file.
#[test]
fn walker_does_not_enter_excluded_folders() {
    let root = std::env::temp_dir().join(format!("dash-excluded-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    fs::write(root.join("node_modules/pkg/index.js"), "x").unwrap();
    fs::write(root.join("keep.txt"), "x").unwrap();
    let options: FileSearchIndexOptions = serde_json::from_value(
        serde_json::json!({ "roots": [], "includeHidden": false, "indexContent": true, "excludeFolders": ["node_modules"] }),
    )
    .unwrap();
    let seen: Vec<String> = index_walker(&root.display().to_string(), &options, false)
        .build()
        .flatten()
        .filter_map(|e| e.path().file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    assert!(seen.contains(&"keep.txt".to_string()), "{seen:?}");
    assert!(!seen.iter().any(|name| name == "node_modules" || name == "pkg" || name == "index.js"), "{seen:?}");
    let _ = fs::remove_dir_all(&root);
}

/// Saved progress survives cleanup; a build continues only progress made
/// with its own settings, and other leftovers are deleted.
#[test]
fn interrupted_builds_are_kept_and_matched() {
    let state = std::env::temp_dir().join(format!("dash-resume-{}", std::process::id()));
    let _ = fs::remove_dir_all(&state);
    let build = |stamp: u128, marker: Option<&str>| {
        let dir = staging_search_index_dir_for_state(&state, stamp);
        fs::create_dir_all(&dir).unwrap();
        if let Some(key) = marker {
            fs::write(dir.join(RESUME_MARKER_FILE), key).unwrap();
        }
        dir
    };
    let unsaved = build(1, None);
    let other = build(2, Some("other settings"));
    let mine = build(3, Some("my settings"));

    remove_staging_index_dirs(&state, STAGING_SEARCH_INDEX_PREFIX);
    assert!(!unsaved.exists(), "nothing saved: swept as before");
    assert!(other.exists() && mine.exists(), "saved progress is kept");

    assert_eq!(take_resumable_staging(&state, "my settings"), Some(mine.clone()));
    assert!(!other.exists(), "progress made with other settings is deleted");
    assert_eq!(take_resumable_staging(&state, "new settings"), None);
    assert!(!mine.exists());
    let _ = fs::remove_dir_all(&state);
}

/// Only one content watcher can hold an index's watcher lock.
#[cfg(windows)]
#[test]
fn second_watcher_is_refused() {
    let state = std::env::temp_dir().join(format!("dash-watch-lock-{}", std::process::id()));
    fs::create_dir_all(&state).unwrap();
    let first = claim_watcher_lock(&state);
    assert!(first.is_some());
    assert!(claim_watcher_lock(&state).is_none(), "held by the first");
    drop(first);
    assert!(claim_watcher_lock(&state).is_some(), "free again once released");
    let _ = fs::remove_dir_all(&state);
}

/// Removing a folder removes everything under it and nothing next to it.
#[test]
fn folder_delete_takes_its_contents_only() {
    let dir = std::env::temp_dir().join(format!("dash-cascade-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let schema = build_search_schema();
    let index = Index::open_or_create(MmapDirectory::open(&dir).unwrap(), schema.clone()).unwrap();
    let fields = extract_fields(&schema).unwrap();
    let mut writer: IndexWriter = index.writer_with_num_threads(1, 20_000_000).unwrap();
    for path in [r"C:\a", r"C:\a\x.txt", r"C:\a\b\y.txt", r"C:\ab\z.txt", r"C:\a b.txt"] {
        let meta = ContentDocMeta {
            file_path: path,
            file_name: "x",
            extension: "txt",
            entry_type: ENTRY_TYPE_FILE,
            size: 1,
            modified_ms: 1,
            created_ms: 1,
        };
        add_content_document(&writer, fields, &meta, "").unwrap();
    }
    writer.commit().unwrap();
    delete_path_and_children(&writer, fields.path_exact, Path::new(r"C:\a"));
    writer.commit().unwrap();
    writer.wait_merging_threads().unwrap();
    let mut left: Vec<String> = load_existing_index_for_reuse(&dir).unwrap().map.into_keys().collect();
    left.sort();
    assert_eq!(left, vec![r"c:\a b.txt".to_string(), r"c:\ab\z.txt".to_string()]);
    let _ = fs::remove_dir_all(&dir);
}

/// The reuse snapshot reads paths, sizes and dates without the stored text,
/// skips deleted documents, and reuses empty text only when told to.
#[test]
fn previous_index_is_read_for_reuse() {
    let dir = std::env::temp_dir().join(format!("dash-reuse-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let schema = build_search_schema();
    let index = Index::open_or_create(MmapDirectory::open(&dir).unwrap(), schema.clone()).unwrap();
    let fields = extract_fields(&schema).unwrap();
    let mut writer: IndexWriter = index.writer_with_num_threads(1, 20_000_000).unwrap();
    let doc = |path: &'static str, size: u64, modified_ms: u64| ContentDocMeta {
        file_path: path,
        file_name: "x",
        extension: "txt",
        entry_type: ENTRY_TYPE_FILE,
        size,
        modified_ms,
        created_ms: modified_ms,
    };
    add_content_document(&writer, fields, &doc(r"C:\Docs\Notes.txt", 11, 1_000), "hello world").unwrap();
    add_content_document(&writer, fields, &doc(r"C:\Docs\scan.pdf", 500, 2_000), "").unwrap();
    add_content_document(&writer, fields, &doc(r"C:\Docs\gone.txt", 3, 3_000), "bye").unwrap();
    writer.commit().unwrap();
    writer.delete_term(tantivy::Term::from_field_text(fields.path_exact, r"C:\Docs\gone.txt"));
    writer.commit().unwrap();
    writer.wait_merging_threads().unwrap();

    let mut cache = load_existing_index_for_reuse(&dir).unwrap();
    assert_eq!(cache.map.len(), 2, "the deleted document is left out");
    let notes = &cache.map[r"c:\docs\notes.txt"];
    assert_eq!((notes.size, notes.modified_ms), (11, 1_000));
    assert_eq!(reuse_cached_content(&cache, r"c:\docs\notes.txt", 1_000, 11).as_deref(), Some("hello world"));
    assert_eq!(reuse_cached_content(&cache, r"c:\docs\notes.txt", 1_001, 11), None, "changed file");
    assert_eq!(reuse_cached_content(&cache, r"c:\docs\scan.pdf", 2_000, 500), None, "empty text retried by default");
    cache.keep_empty = true;
    assert_eq!(reuse_cached_content(&cache, r"c:\docs\scan.pdf", 2_000, 500).as_deref(), Some(""));
    drop(cache);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn relative_date_phrases_resolve_to_rolling_windows() {
    // "today" — a 24h window.
    let today = build_natural_query_plan("report today").date_filter.expect("today → date filter");
    assert_eq!(today.end_ms.unwrap() - today.start_ms.unwrap(), day_ms(1));

    // "last week" — previously unparsed (only "this week" existed).
    let last_week = build_natural_query_plan("invoices from last week").date_filter.expect("last week → date filter");
    assert_eq!(last_week.end_ms.unwrap() - last_week.start_ms.unwrap(), day_ms(7));

    // "this month" — previously unparsed (only "last month" existed).
    let this_month = build_natural_query_plan("photos this month").date_filter.expect("this month → date filter");
    assert_eq!(this_month.end_ms.unwrap() - this_month.start_ms.unwrap(), day_ms(31));

    // "past year".
    let past_year = build_natural_query_plan("notes past year").date_filter.expect("past year → date filter");
    assert_eq!(past_year.end_ms.unwrap() - past_year.start_ms.unwrap(), day_ms(365));
}

#[test]
fn numeric_relative_date_windows_resolve() {
    let week = build_natural_query_plan("logs from last 7 days").date_filter.expect("last 7 days → date filter");
    assert_eq!(week.end_ms.unwrap() - week.start_ms.unwrap(), day_ms(7));

    let quarter = build_natural_query_plan("reports past 3 months").date_filter.expect("past 3 months → date filter");
    assert_eq!(quarter.end_ms.unwrap() - quarter.start_ms.unwrap(), day_ms(93));
}

#[test]
fn year_phrases_resolve_to_date_filters() {
    // "during"/"year" + a year still resolve to an exact-year date filter.
    assert!(build_natural_query_plan("taxes during 2023").date_filter.is_some());
    // "from"/"in" no longer mean a date — they fall through to a filename
    // keyword, so "movies from 2025" == "movies 2025".
    assert!(build_natural_query_plan("movies from 2025").date_filter.is_none());
    assert!(build_natural_query_plan("movies in 2025").date_filter.is_none());
    assert!(build_natural_query_plan("files before 2020").date_filter.is_some());
    assert!(build_natural_query_plan("docs between 2019 and 2021").date_filter.is_some());

    // "created" sets the date-field intent to creation time.
    let created = build_natural_query_plan("created today").date_filter.expect("created today → date filter");
    assert!(matches!(created.field, DateFieldIntent::Created));
}

#[test]
fn natural_language_date_field_phrases_resolve() {
    // "<category> created/createdat/created at <year>" → date filter on the
    // Created field, across the bare, one-word, and "at"-connector forms.
    let created_phrases = ["movies created 2025", "movies created at 2025", "movies createdat 2025"];
    for &query in &created_phrases {
        let f = build_natural_query_plan(query).date_filter.unwrap_or_else(|| panic!("{query} → date filter"));
        assert!(matches!(f.field, DateFieldIntent::Created), "{query} should map to Created");
    }

    let modified_phrases = ["movies modified 2025", "movies modified at 2025", "movies modifiedat 2025"];
    for &query in &modified_phrases {
        let f = build_natural_query_plan(query).date_filter.unwrap_or_else(|| panic!("{query} → date filter"));
        assert!(matches!(f.field, DateFieldIntent::Modified), "{query} should map to Modified");
    }

    // A bare year is the filename release-year default — NOT a date filter.
    assert!(
        build_natural_query_plan("movies 2025").date_filter.is_none(),
        "bare year must stay a filename keyword, not a date filter"
    );
}

#[test]
fn bare_category_keyword_searches_by_format_only() {
    // "movies" with nothing else → movie extensions, no name keyword. The
    // query is extension-only, so the filename index returns every
    // movie-format file (build_result_item enforces the per-hit filter).
    let plan = build_natural_query_plan("movies");
    assert!(plan.query_keywords.is_empty(), "a bare category must not add a name keyword");
    assert_eq!(plan.query_text, "");
    assert!(plan.extension_filters.iter().any(|e| e == "mp4"));
    assert!(plan.extension_filters.iter().any(|e| e == "mkv"));
    assert!(!plan.extension_filters.iter().any(|e| e == "webm"), "movies excludes webm");

    let query = build_natural_query_string(&plan.query_text, &plan.query_keywords, &plan.extension_filters)
        .expect("a bare category must still produce an extension-only query");
    assert!(query.contains("extension:mp4"));
    assert!(!query.contains("extension:webm"));

    // "video" is the broad bucket — webm and the rest are included.
    let video = build_natural_query_plan("video");
    assert!(video.extension_filters.iter().any(|e| e == "webm"));
    assert!(video.extension_filters.iter().any(|e| e == "mp4"));
}

#[test]
fn size_phrases_resolve_to_size_filters() {
    let mb = 1024 * 1024;
    let gb = 1024 * mb;

    match build_natural_query_plan("large files").size_filter {
        Some(SizeFilter::Range { min_bytes, max_bytes, .. }) => {
            assert_eq!(min_bytes, Some(100 * mb));
            assert_eq!(max_bytes, None);
        }
        other => panic!("large → unexpected {other:?}"),
    }

    match build_natural_query_plan("huge videos").size_filter {
        Some(SizeFilter::Range { min_bytes, .. }) => assert_eq!(min_bytes, Some(gb)),
        other => panic!("huge → unexpected {other:?}"),
    }

    match build_natural_query_plan("bigger than 50mb").size_filter {
        Some(SizeFilter::Range { min_bytes, .. }) => assert_eq!(min_bytes, Some(50 * mb)),
        other => panic!("bigger than 50mb → unexpected {other:?}"),
    }

    match build_natural_query_plan("smaller than 2gb").size_filter {
        Some(SizeFilter::Range { max_bytes, .. }) => assert_eq!(max_bytes, Some(2 * gb)),
        other => panic!("smaller than 2gb → unexpected {other:?}"),
    }

    assert!(matches!(build_natural_query_plan("empty folders").size_filter, Some(SizeFilter::Empty)));
}

#[test]
fn extensions_resolve_from_dots_and_categories() {
    // Dot-prefixed extension.
    assert!(build_natural_query_plan(".pdf reports").extension_filters.contains(&"pdf".to_string()));

    // A category word expands to a family of extensions.
    let images = build_natural_query_plan("vacation images").extension_filters;
    assert!(images.contains(&"png".to_string()));
    assert!(images.contains(&"jpg".to_string()));
}

#[test]
fn field_operators_resolve_explicitly() {
    // `ext:` is a literal extension — not the doc category family.
    let plan = build_natural_query_plan("report ext:docx");
    assert_eq!(plan.extension_filters, vec!["docx".to_string()]);
    assert!(plan.query_keywords.contains(&"report".to_string()));

    // Comma-separated extension list.
    let multi = build_natural_query_plan("ext:jpg,png").extension_filters;
    assert!(multi.contains(&"jpg".to_string()));
    assert!(multi.contains(&"png".to_string()));

    // `type:` takes a category word.
    assert!(build_natural_query_plan("type:image").extension_filters.contains(&"png".to_string()));

    // `kind:folder` → entry-type intent.
    assert_eq!(build_natural_query_plan("kind:folder").entry_type_filter.as_deref(), Some(ENTRY_TYPE_FOLDER));

    // A non-operator colon is dropped, not mangled into a field query.
    assert!(build_natural_query_plan("build:debug").query_keywords.contains(&"builddebug".to_string()));
}

#[test]
fn entry_type_intent_is_detected() {
    assert_eq!(build_natural_query_plan("screenshots folder").entry_type_filter.as_deref(), Some(ENTRY_TYPE_FOLDER));
    assert_eq!(build_natural_query_plan("documents directory").entry_type_filter.as_deref(), Some(ENTRY_TYPE_FOLDER));
}

#[test]
fn quoted_phrases_are_extracted() {
    let plan = build_natural_query_plan("\"annual report\" budget");
    assert_eq!(plan.exact_phrases, vec!["annual report".to_string()]);
    assert!(plan.query_keywords.contains(&"budget".to_string()));
}

#[test]
fn stopwords_are_dropped_and_synonyms_added() {
    let plan = build_natural_query_plan("show me the invoice");
    assert!(!plan.query_keywords.contains(&"the".to_string()));
    assert!(!plan.query_keywords.contains(&"show".to_string()));
    assert!(plan.query_keywords.contains(&"invoice".to_string()));
    // Synonym expansion via related_natural_terms.
    assert!(plan.query_keywords.contains(&"receipt".to_string()));
}

#[test]
fn combined_query_extracts_every_intent() {
    // The plan's own example: "big pdfs from last week".
    let plan = build_natural_query_plan("big pdfs from last week");
    assert!(plan.extension_filters.contains(&"pdf".to_string()));
    assert!(matches!(plan.size_filter, Some(SizeFilter::Range { min_bytes: Some(_), .. })));
    let date = plan.date_filter.expect("last week → date filter");
    assert_eq!(date.end_ms.unwrap() - date.start_ms.unwrap(), day_ms(7));
}

#[test]
fn decompounding_splits_long_alphanumeric_keywords() {
    let decompound = build_decompound_query_string(&["spiderman".to_string()])
        .expect("a long keyword should produce decompound clauses");
    assert!(decompound.contains("spider"));
    assert!(decompound.contains("man"));
    // Too short to split.
    assert!(build_decompound_query_string(&["cat".to_string()]).is_none());
}

#[test]
fn pure_parser_helpers_behave() {
    assert_eq!(parse_year_token("2023"), Some(2023));
    assert_eq!(parse_year_token("23"), None);
    assert_eq!(parse_year_token("1850"), None); // before the 1970 floor

    assert_eq!(period_to_days("week"), Some(7));
    assert_eq!(period_to_days("months"), Some(31));
    assert_eq!(period_to_days("fortnight"), None);

    assert!(is_natural_stopword("the"));
    assert!(!is_natural_stopword("report"));

    assert_eq!(split_size_token("100mb", None), Some((100.0, "mb", 1)));
    assert_eq!(split_size_token("100", Some("mb")), Some((100.0, "mb", 2)));
    assert_eq!(split_size_token("mb", None), None);
}
