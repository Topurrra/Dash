//! Apps and shortcuts to launch: their cache and scoring.

use super::*;

/// Arm the launch-target folder watcher once (idempotent, process-global).
///
/// `LAUNCH_TARGET_CACHE` is otherwise built once and never refreshed for the
/// process lifetime, so a freshly-installed app — whose installer drops a
/// `.lnk` into Start Menu\Programs — wouldn't appear in the palette until the
/// next app restart. Watching the handful of shallow launcher roots (Start
/// Menu user/system + Desktop user/public) and clearing the cache on any
/// add/remove/rename makes a new app show up on the next search. Cheap on
/// Windows: `ReadDirectoryChangesW` watches each root's whole subtree with a
/// single handle. Best-effort: if the watcher can't start, the cache simply
/// keeps its build-once behavior — no worse than before.
pub(super) fn ensure_launch_target_watcher() {
    let Ok(mut slot) = LAUNCH_TARGET_WATCHER.lock() else {
        return;
    };
    if slot.is_some() {
        return; // already watching
    }

    let mut watcher = match notify::recommended_watcher(|res: notify::Result<Event>| {
        let Ok(event) = res else { return };
        // An app install / uninstall / rename surfaces as a create / remove /
        // rename under a launcher root. Invalidate the cache; the next
        // `search_launch_targets` rebuilds it lazily (off the UI thread).
        if matches!(event.kind, EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_)) {
            if let Ok(mut cache) = LAUNCH_TARGET_CACHE.lock() {
                *cache = None;
            }
        }
    }) {
        Ok(watcher) => watcher,
        Err(_) => return,
    };

    let mut watching = false;
    for (root, _source) in launcher_candidate_roots() {
        if root.exists() && watcher.watch(&root, RecursiveMode::Recursive).is_ok() {
            watching = true;
        }
    }

    // Only retain the watcher if it's actually watching at least one root;
    // otherwise drop it so a later build can retry (e.g. a root that didn't
    // exist at first launch).
    if watching {
        *slot = Some(watcher);
    }
}

pub(super) fn ensure_launch_target_cache() -> Result<LaunchTargetCache, String> {
    {
        let guard = LAUNCH_TARGET_CACHE.lock().map_err(|_| "Launch target cache lock failed".to_string())?;
        if let Some(cache) = guard.as_ref() {
            return Ok(cache.clone());
        }
    }

    let cache = build_launch_target_cache();
    // First build also arms the folder watcher, so later app installs/removals
    // invalidate the cache automatically (idempotent — only starts once).
    ensure_launch_target_watcher();
    let mut guard = LAUNCH_TARGET_CACHE.lock().map_err(|_| "Launch target cache lock failed".to_string())?;
    if guard.is_none() {
        *guard = Some(cache.clone());
    }
    Ok(guard.as_ref().cloned().unwrap_or(cache))
}

pub(super) fn build_launch_target_cache() -> LaunchTargetCache {
    let mut records = Vec::new();
    let mut seen_paths = HashSet::new();

    for (root, source) in launcher_candidate_roots() {
        if !root.exists() {
            continue;
        }

        let iter = WalkDir::new(&root).follow_links(false).max_depth(MAX_LAUNCH_SCAN_DEPTH).into_iter();

        for entry in iter.filter_map(|item| item.ok()) {
            if records.len() >= MAX_LAUNCH_TARGETS {
                break;
            }

            let path = entry.path();
            let file_type = entry.file_type();

            #[cfg(target_os = "macos")]
            if file_type.is_dir()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("app"))
                    .unwrap_or(false)
            {
                add_launch_target_record(&mut records, &mut seen_paths, path, source, "bundle");
                continue;
            }

            if !file_type.is_file() || !is_launch_target_file(path) {
                continue;
            }

            let kind = if path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("lnk") || ext.eq_ignore_ascii_case("desktop"))
                .unwrap_or(false)
            {
                "shortcut"
            } else {
                "application"
            };
            add_launch_target_record(&mut records, &mut seen_paths, path, source, kind);
        }
    }

    for (name, path) in builtin_launcher_targets() {
        if records.len() >= MAX_LAUNCH_TARGETS {
            break;
        }
        add_launch_target_record_with_name(
            &mut records,
            &mut seen_paths,
            &path,
            "system-builtin",
            "application",
            Some(name),
        );
    }

    // Packaged (MSIX / Microsoft Store) apps. These have no `.lnk` and no
    // reachable exe, so the walk above cannot see them at all — without this,
    // Settings, Terminal, ChatGPT, Claude and every other Store app are simply
    // absent from the launcher. See `commands::packaged_apps`.
    for app in crate::packaged_apps::enumerate_packaged_apps() {
        if records.len() >= MAX_LAUNCH_TARGETS {
            break;
        }
        let launch_path = crate::packaged_apps::packaged_launch_path(&app.aumid);
        add_packaged_launch_target_record(&mut records, &mut seen_paths, &app.name, &launch_path);
    }

    let built_at_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|value| value.as_millis()).unwrap_or(0);

    LaunchTargetCache { built_at_ms, records }
}

pub(super) fn launcher_candidate_roots() -> Vec<(PathBuf, &'static str)> {
    let mut roots: Vec<(PathBuf, &'static str)> = Vec::new();

    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = env::var("APPDATA") {
            roots.push((PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs"), "start-menu-user"));
        }
        if let Ok(program_data) = env::var("PROGRAMDATA") {
            roots.push((
                PathBuf::from(program_data).join(r"Microsoft\Windows\Start Menu\Programs"),
                "start-menu-system",
            ));
        }
        if let Ok(user_profile) = env::var("USERPROFILE") {
            roots.push((PathBuf::from(user_profile).join("Desktop"), "desktop-user"));
        }
        if let Ok(public_profile) = env::var("PUBLIC") {
            roots.push((PathBuf::from(public_profile).join("Desktop"), "desktop-public"));
        }
    }

    #[cfg(target_os = "macos")]
    {
        roots.push((PathBuf::from("/Applications"), "applications-system"));
        if let Ok(home) = env::var("HOME") {
            roots.push((PathBuf::from(home).join("Applications"), "applications-user"));
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        roots.push((PathBuf::from("/usr/share/applications"), "applications-system"));
        roots.push((PathBuf::from("/usr/local/share/applications"), "applications-local"));
        if let Ok(home) = env::var("HOME") {
            roots.push((PathBuf::from(home).join(".local/share/applications"), "applications-user"));
        }
    }

    roots
}

pub(super) fn builtin_launcher_targets() -> Vec<(String, PathBuf)> {
    let mut targets = Vec::new();

    #[cfg(target_os = "windows")]
    {
        let mut push_if_exists = |name: &str, path: &str| {
            let resolved = PathBuf::from(path);
            if resolved.exists() {
                targets.push((name.to_string(), resolved));
            }
        };

        push_if_exists("Snipping Tool", r"C:\Windows\System32\SnippingTool.exe");
        push_if_exists("Notepad", r"C:\Windows\System32\notepad.exe");
        push_if_exists("Calculator", r"C:\Windows\System32\calc.exe");
        push_if_exists("Paint", r"C:\Windows\System32\mspaint.exe");
    }

    targets
}

pub(super) fn is_launch_target_file(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    let extension = extension.to_ascii_lowercase();

    #[cfg(target_os = "windows")]
    {
        return matches!(extension.as_str(), "lnk" | "exe" | "url" | "msc");
    }

    #[cfg(target_os = "macos")]
    {
        return matches!(extension.as_str(), "app");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return matches!(extension.as_str(), "desktop" | "appimage" | "sh");
    }

    #[allow(unreachable_code)]
    false
}

pub(super) fn add_launch_target_record(
    records: &mut Vec<LaunchTargetRecord>,
    seen_paths: &mut HashSet<String>,
    path: &Path,
    source: &str,
    kind: &str,
) {
    add_launch_target_record_with_name(records, seen_paths, path, source, kind, None);
}

pub(super) fn add_launch_target_record_with_name(
    records: &mut Vec<LaunchTargetRecord>,
    seen_paths: &mut HashSet<String>,
    path: &Path,
    source: &str,
    kind: &str,
    override_name: Option<String>,
) {
    if !path.exists() {
        return;
    }

    let path_text = path.to_string_lossy().to_string();
    let path_key = path_text.to_ascii_lowercase();
    if !seen_paths.insert(path_key.clone()) {
        return;
    }

    let name = override_name.unwrap_or_else(|| {
        path.file_stem().or_else(|| path.file_name()).and_then(|value| value.to_str()).unwrap_or_default().to_string()
    });
    let name = name.trim().to_string();
    if name.is_empty() {
        return;
    }

    records.push(LaunchTargetRecord {
        id: format!("launch://{path_key}"),
        name: name.clone(),
        path: path_text.clone(),
        kind: kind.to_string(),
        source: source.to_string(),
        normalized_name: normalize_launcher_text(&name),
        normalized_path: normalize_launcher_text(&path_text),
    });
}

/// Insert a packaged (MSIX / Store) app record.
///
/// Deliberately separate from [`add_launch_target_record_with_name`]: that one
/// gates on `path.exists()`, which is correct for files and always false for a
/// packaged app — its "path" is the synthetic `shell:AppsFolder\<AUMID>` handle,
/// not a file. The name always comes from the shell, so there is no file-stem
/// fallback either. Everything downstream (dedup key, id, normalization) matches
/// the file path exactly, so packaged apps rank and dedupe like any other target.
pub(super) fn add_packaged_launch_target_record(
    records: &mut Vec<LaunchTargetRecord>,
    seen_paths: &mut HashSet<String>,
    name: &str,
    launch_path: &str,
) {
    let path_key = launch_path.to_ascii_lowercase();
    if !seen_paths.insert(path_key.clone()) {
        return;
    }
    let name = name.trim().to_string();
    if name.is_empty() {
        return;
    }

    records.push(LaunchTargetRecord {
        id: format!("launch://{path_key}"),
        name: name.clone(),
        path: launch_path.to_string(),
        kind: "packaged".to_string(),
        source: "apps-folder".to_string(),
        normalized_name: normalize_launcher_text(&name),
        // Normalize the AUMID too: it carries the package family name, so a
        // query like "sticky notes" or "microsoft" can still reach the app even
        // when the display name alone wouldn't match.
        normalized_path: normalize_launcher_text(launch_path),
    });
}

pub(super) fn normalize_launcher_text(input: &str) -> String {
    let mut normalized = String::with_capacity(input.len());
    let mut previous_space = false;

    for ch in input.chars() {
        let mapped = if ch.is_ascii_alphanumeric() { ch.to_ascii_lowercase() } else { ' ' };

        if mapped == ' ' {
            if !previous_space {
                normalized.push(' ');
                previous_space = true;
            }
        } else {
            normalized.push(mapped);
            previous_space = false;
        }
    }

    normalized.trim().to_string()
}

pub(super) fn normalize_launch_path_for_match(path: &Path) -> String {
    let raw = path.to_string_lossy().to_string();
    #[cfg(target_os = "windows")]
    {
        raw.to_ascii_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        raw
    }
}

pub(super) fn score_launch_target(
    record: &LaunchTargetRecord,
    query_normalized: &str,
    query_tokens: &[String],
) -> Option<f32> {
    if query_normalized.is_empty() {
        return None;
    }

    let mut matched_tokens = 0usize;
    let mut score = 0.0_f32;

    if record.normalized_name == query_normalized {
        score += 500.0;
    } else if record.normalized_name.starts_with(query_normalized) {
        score += 300.0;
    } else if record.normalized_name.contains(query_normalized) {
        score += 180.0;
    } else if record.normalized_path.contains(query_normalized) {
        score += 90.0;
    }

    for token in query_tokens {
        if token.is_empty() {
            continue;
        }
        if normalized_text_has_exact_token(&record.normalized_name, token) {
            matched_tokens += 1;
            score += 72.0;
        } else if token.len() >= 3 && normalized_text_has_prefix_token(&record.normalized_name, token) {
            matched_tokens += 1;
            score += 44.0;
        } else if normalized_text_has_exact_token(&record.normalized_path, token) {
            matched_tokens += 1;
            score += 26.0;
        } else if token.len() >= 3 && normalized_text_has_prefix_token(&record.normalized_path, token) {
            matched_tokens += 1;
            score += 14.0;
        }
    }

    if query_tokens.len() >= 2 && matched_tokens < 2 && score < 180.0 {
        return None;
    }
    if matched_tokens == 0 && score < 80.0 {
        return None;
    }

    let token_bonus =
        if query_tokens.is_empty() { 0.0 } else { (matched_tokens as f32 / query_tokens.len() as f32) * 40.0 };

    let kind_bonus = match record.kind.as_str() {
        "application" => 12.0,
        "shortcut" => 8.0,
        _ => 0.0,
    };

    Some(score + token_bonus + kind_bonus)
}

pub(super) fn normalized_text_has_exact_token(text: &str, token: &str) -> bool {
    text.split_whitespace().any(|word| word == token)
}

pub(super) fn normalized_text_has_prefix_token(text: &str, token: &str) -> bool {
    text.split_whitespace().any(|word| word.starts_with(token))
}
