//! What gets indexed (exclusions), preview extraction, drives.

use super::*;

pub(super) fn should_include_path(path: &Path, options: &FileSearchIndexOptions) -> bool {
    // Wave 6 (2026-05-28): hard-block on the four Windows system trees
    // (Windows / Program Files / Program Files (x86) / ProgramData). These
    // contain zero user-authored content and were previously eating
    // ~200k+ index entries per drive for no user benefit. Applied here so
    // `WalkDir::filter_entry` skips descending into the entire subtree.
    if is_system_protected_path(path) {
        return false;
    }
    if !options.include_hidden && is_hidden(path) {
        return false;
    }
    if has_excluded_folder(path, &options.exclude_folders) {
        return false;
    }
    if path.is_file() && has_excluded_extension(path, &options.exclude_extensions) {
        return false;
    }
    true
}

pub(super) fn is_hidden(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str().to_str().map(|value| value.starts_with('.')).unwrap_or(false))
}

pub(super) fn normalize_exclude_folders(values: &[String]) -> Vec<String> {
    let mut normalized = normalize_unique_tokens(values, false);
    let mut seen: HashSet<String> = normalized.iter().cloned().collect();
    for token in required_exclude_folders() {
        if seen.insert(token.clone()) {
            normalized.push(token);
        }
    }
    normalized
}

pub(super) fn normalize_exclude_extensions(values: &[String]) -> Vec<String> {
    normalize_unique_tokens(values, true)
}

pub(super) fn normalize_unique_tokens(values: &[String], trim_dot_prefix: bool) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for value in values {
        let mut token = value.trim().trim_matches('"').trim_matches('\'').to_lowercase();
        token = token.replace('\\', "/");
        if trim_dot_prefix {
            token = token.trim_start_matches('.').to_string();
        }
        token = token.trim_matches('/').trim().to_string();
        if token.is_empty() || seen.contains(&token) {
            continue;
        }
        seen.insert(token.clone());
        normalized.push(token);
    }
    normalized
}

pub(super) fn has_excluded_folder(path: &Path, exclude_folders: &[String]) -> bool {
    if exclude_folders.is_empty() {
        return false;
    }

    let normalized_path = normalize_path_for_exclusion(path);
    if exclude_folders
        .iter()
        .any(|excluded| is_path_like_exclusion(excluded) && exclusion_pattern_matches_path(&normalized_path, excluded))
    {
        return true;
    }

    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .map(|value| {
                let normalized = value.to_lowercase();
                exclude_folders.iter().any(|excluded| excluded == &normalized)
            })
            .unwrap_or(false)
    })
}

pub(super) fn normalize_path_for_exclusion(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase().trim_end_matches('/').to_string()
}

pub(super) fn is_path_like_exclusion(excluded: &str) -> bool {
    excluded.contains('/') || excluded.contains(':') || excluded.contains('*')
}

pub(super) fn exclusion_pattern_matches_path(path: &str, excluded: &str) -> bool {
    if excluded.contains('*') {
        let mut remainder = path;
        for part in excluded.split('*').filter(|part| !part.is_empty()) {
            let Some(index) = remainder.find(part) else {
                return false;
            };
            remainder = &remainder[index + part.len()..];
        }
        return true;
    }

    path == excluded
        || path.starts_with(&format!("{excluded}/"))
        || path.contains(&format!("/{excluded}/"))
        || path.ends_with(&format!("/{excluded}"))
}

pub(super) fn has_excluded_extension(path: &Path, exclude_extensions: &[String]) -> bool {
    if exclude_extensions.is_empty() {
        return false;
    }

    let token = path_extension_or_dotfile_token(path);
    !token.is_empty() && exclude_extensions.iter().any(|excluded| excluded == &token)
}

pub(super) fn path_extension_or_dotfile_token(path: &Path) -> String {
    if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
        return extension.trim_start_matches('.').to_lowercase();
    }

    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return String::new();
    };
    let normalized = file_name.to_lowercase();
    if normalized.starts_with('.') && normalized.matches('.').count() == 1 {
        return normalized.trim_start_matches('.').to_string();
    }

    String::new()
}

/// Read indexable text from a file. Dispatches to format-aware extractors
/// (PDF, DOCX, XLSX, PPTX) in addition to plain text — see the
/// `commands::text_extract` module for per-format details.
pub(super) fn read_text_for_index(path: &Path, max_bytes: usize) -> Option<String> {
    crate::text_extract::extract_text_for_indexing(path, max_bytes)
}

/// Extract up to `max_bytes` of readable text from a file for the command
/// palette's preview pane. Reuses the content-index extractor, so it returns
/// text for plain-text / code / Markdown / `.ki` AND rich formats (PDF, DOCX,
/// XLSX, …). Returns `None` for folders, missing files, unsupported types, or
/// binaries — the UI then shows metadata only. Local + read-only; the asset
/// protocol handles images/audio/video directly, so those never reach here.
/// Rich formats are parsed in the sandboxed extractor child (see
/// `extract_text_sandboxed`).
pub fn read_file_preview(path: String, max_bytes: Option<usize>) -> Option<String> {
    let cap = max_bytes.unwrap_or(64 * 1024).clamp(1024, 512 * 1024);
    let p = Path::new(&path);
    if !p.is_file() {
        return None;
    }
    extract_text_sandboxed(p, cap)
}

/// How long a sandboxed preview extraction may take before its child is
/// killed. Shorter than the index build's per-file timeout: someone is
/// waiting on it, and a file that slow gets no preview.
pub(super) const PREVIEW_EXTRACT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// The preview child extracts up to this much (the largest preview cap;
/// result snippets scan far less); callers trim to their own cap.
pub(super) const PREVIEW_CHILD_MAX_BYTES: usize = 512 * 1024;

/// The extractor child previews use: started on first use, replaced after it
/// dies or times out. Dropping it closes the job, which kills the child.
pub(super) struct PreviewExtractor {
    pub(super) child: WatchedChild,
    pub(super) _job: MemoryCappedJob,
}

pub(super) static PREVIEW_EXTRACTOR: LazyLock<Mutex<Option<PreviewExtractor>>> = LazyLock::new(|| Mutex::new(None));

pub(super) fn spawn_preview_extractor() -> Result<PreviewExtractor, String> {
    let job = MemoryCappedJob::new(EXTRACTOR_CHILD_MEMORY_CAP_BYTES)?;
    // No OCR settings: OCR stays off for previews. "fast" = normal priority,
    // since someone is waiting.
    let (child, stdin, stdout) = spawn_assigned_extractor_child(
        PREVIEW_CHILD_MAX_BYTES,
        "fast",
        job.assigner(),
        &crate::text_extract::OcrChildEnv::default(),
    )?;
    Ok(PreviewExtractor { child: WatchedChild::new(child, stdin, stdout), _job: job })
}

/// Up to `max_bytes` of `path`'s text without parsing untrusted files in this
/// process: plain text is read here (bytes only, no parser); documents go to
/// the memory-capped extractor child with a timeout, so a parser crash, memory
/// bomb or endless loop costs that child, not the app.
pub(super) fn extract_text_sandboxed(path: &Path, max_bytes: usize) -> Option<String> {
    let ext = path.extension().and_then(|value| value.to_str()).unwrap_or_default().to_lowercase();
    if crate::text_extract::is_plain_text_extension(&ext) {
        return crate::text_extract::extract_text_for_indexing(path, max_bytes);
    }
    if !crate::text_extract::is_text_or_doc_extension(&ext) {
        return None;
    }
    let mut slot = PREVIEW_EXTRACTOR.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_none() {
        *slot = spawn_preview_extractor().ok();
    }
    let extractor = slot.as_mut()?;
    match extractor.child.extract(path, PREVIEW_EXTRACT_TIMEOUT) {
        ExtractAnswer::Done(_, text) => {
            let mut text = text?;
            text.truncate(nearest_char_boundary_before(&text, max_bytes));
            Some(text)
        }
        ExtractAnswer::Died | ExtractAnswer::TimedOut => {
            *slot = None;
            None
        }
    }
}

/// Roots of every accessible logical drive (e.g. "C:\\", "D:\\"). Used by
/// first-run indexing to seed the FILENAME index with every drive the user
/// has. Dependency-free: probes drive letters A–Z and keeps the ones whose
/// root is a readable directory (so an empty optical/card reader is skipped).
pub fn list_logical_drives() -> Vec<String> {
    #[cfg(windows)]
    {
        let mut out = Vec::new();
        for letter in b'A'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            if Path::new(&root).is_dir() {
                out.push(root);
            }
        }
        out
    }
    #[cfg(not(windows))]
    {
        vec!["/".to_string()]
    }
}

/// Wave 8.1 (2026-05-28): walker-side content-index gate. Returns true when
/// the path should be sent through the content extractor pool — that is, the
/// extension is either a definite text / document format, OR it's an image
/// AND OCR-on-Index is enabled AND the path lives under one of the user's
/// configured OCR folders. The image case is gated HERE in the walker (not
/// later in the extractor child) so opt-out images never even cost an IPC
/// round-trip.
///
/// Folder matching mirrors `OcrIndexConfig::applies_to` in `text_extract.rs`:
/// case-insensitive prefix, with trailing separator tolerated.
pub(super) fn should_index_for_content(path: &Path, options: &FileSearchIndexOptions) -> bool {
    let ext = path.extension().and_then(|v| v.to_str()).unwrap_or_default().to_lowercase();
    if crate::text_extract::is_text_or_doc_extension(&ext) {
        return true;
    }
    if options.ocr_on_index_enabled
        && crate::text_extract::is_ocr_image_extension(&ext)
        && ocr_path_in_scope(path, &options.ocr_on_index_folders)
    {
        return true;
    }
    false
}

/// Case-insensitive prefix-match a path against the configured OCR folders.
/// Mirrors `OcrIndexConfig::applies_to` in `text_extract.rs` — kept in sync;
/// if you change one, change the other.
///
/// Wave 8.4.1 (2026-05-29): normalize ALL separators to forward slashes
/// before comparison. Tauri's folder dialog typically returns paths with
/// forward slashes (`C:/OCRTest`), but the walker yields OS-native paths
/// (`C:\OCRTest\img.png` on Windows). Without normalization, a folder
/// picked via the dialog never matched walker-yielded paths and images
/// in scope were silently skipped.
pub(super) fn ocr_path_in_scope(path: &Path, folders: &[String]) -> bool {
    if folders.is_empty() {
        return false;
    }
    let path_str = path.to_string_lossy().to_lowercase().replace('\\', "/");
    folders.iter().any(|root| {
        let root_str = root.to_lowercase().replace('\\', "/");
        let root_str = root_str.trim_end_matches('/');
        path_str == root_str || path_str.starts_with(&format!("{}/", root_str))
    })
}
