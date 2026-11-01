//! Runs in any app that is a `dash_core::Host`.
//! Text extraction for content search indexing.
//!
//! Dispatches by extension to the right extractor and returns plain text
//! suitable for Tantivy tokenization. Every extractor honors `max_bytes` as
//! a hard upper bound so a 500-page PDF doesn't blow up the index.
//!
//! Supported formats (beyond plain text): PDF (PDFium primary — lazy-loading +
//! accurate Unicode; lopdf per-page fallback), DOCX / PPTX /
//! ODT / ODP / ODS (ZIP archives — `quick-xml` streamed straight off the
//! decompressing entry), XLSX (`quick-xml` streamed over the shared-string
//! table), and RTF (a control-word parser over a size-capped read). Except
//! PDF, every extractor streams and stops at `max_bytes`, so working memory
//! does not scale with file size. DOC / XLS / PPT (pre-2007 binary formats)
//! are not supported.

use std::cell::RefCell;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use pdfium_render::prelude::*;

/// Per-format byte cap. Code is short so 64 KB covers anything sane;
/// PDFs and rich documents get more headroom because they're often long
/// and the extra disk usage is justified by better search coverage.
///
/// The caller's `user_max_bytes` argument acts as a global ceiling — we use
/// `min(tier_limit, user_max_bytes)` so users can throttle everything down
/// to save disk space, but raising it past the tier's natural limit has no
/// effect (the tier caps win).
fn tier_bytes_for_path(path: &Path) -> usize {
    let ext = path.extension().and_then(|v| v.to_str()).unwrap_or_default().to_lowercase();

    let kb: usize = match ext.as_str() {
        // Code files — rare to be over a few KB, 64 KB is generous.
        "rs" | "py" | "js" | "ts" | "jsx" | "tsx" | "go" | "java" | "kt" | "c" | "cpp" | "h" | "hpp" | "swift"
        | "php" | "rb" | "sh" | "ps1" | "svelte" | "html" | "css" => 64,

        // Config / small structured files.
        "ini" | "cfg" | "conf" | "env" | "toml" | "yaml" | "yml" => 64,

        // Plain text, logs, structured data. Larger because logs and JSON
        // dumps can be sizable but still mostly fit in a quarter MB. `.ki` is
        // Note files (Markdown + YAML frontmatter).
        "txt" | "md" | "markdown" | "ki" | "log" | "json" | "xml" | "sql" | "csv" | "tsv" => 256,

        // Rich-text documents — bumped from the old global default so the
        // body, footers, comments, and speaker notes all comfortably fit.
        "docx" | "odt" | "rtf" | "pptx" | "odp" => 512,

        // Spreadsheets — same tier as documents; sheets with lots of cell
        // data benefit from the extra room.
        "xlsx" | "ods" => 512,

        // PDFs — often long technical documents and reports. 1 MB of
        // extracted text covers ~500-800 pages of typical text.
        "pdf" => 1024,

        // Fallback for any extension that's in the supported list but
        // wasn't categorized above.
        _ => 256,
    };

    kb * 1024
}

/// On-disk size ceiling for the heavy (non-plain-text) formats. A file larger
/// than its ceiling skips content extraction and is indexed by name + metadata
/// only.
///
/// Since task #12 the DOCX/PPTX/ODF, XLSX, and RTF extractors stream — they
/// read incrementally and stop at `max_bytes`, so their working memory no
/// longer scales with file size and their ceiling is now just a generous
/// sanity backstop. PDF is the exception: `lopdf::Document::load` builds the
/// whole object graph up front with no streaming-load API, so its ceiling
/// stays tight and load-bearing.
///
/// Returns `None` for plain-text/code formats (which stream in fixed chunks)
/// and for unsupported extensions — `Some` doubles as the "supported heavy
/// format" signal for `extract_text_for_indexing`.
fn heavy_format_input_ceiling(ext: &str) -> Option<u64> {
    const MB: u64 = 1024 * 1024;
    // Sanity backstop for the streaming formats — a "document" past this size
    // is almost certainly not one, and memory does not depend on it anyway.
    const STREAMED: u64 = 4096 * MB;
    match ext {
        // lopdf builds the whole object graph in memory — load-bearing guard.
        "pdf" => Some(64 * MB),
        // Streaming extractors (task #12): bounded regardless of file size.
        "docx" | "pptx" | "odt" | "odp" | "xlsx" | "ods" | "rtf" => Some(STREAMED),
        _ => None,
    }
}

/// Caps how many heavy-format extractions run concurrently. The file-search
/// indexer walks with one thread per CPU core, and each can call into a heavy
/// extractor at once; without this gate, transient parser memory stacks across
/// all of them. The size guard bounds any *single* parse; this bounds the
/// *multiplier*. Plain-text extraction is never gated — it streams cheaply.
const MAX_CONCURRENT_HEAVY_EXTRACTIONS: usize = 3;

struct HeavyExtractionGate {
    active: Mutex<usize>,
    slot_freed: Condvar,
}

static HEAVY_EXTRACTION_GATE: HeavyExtractionGate =
    HeavyExtractionGate { active: Mutex::new(0), slot_freed: Condvar::new() };

/// RAII permit for a heavy extraction slot. `acquire` blocks until a slot is
/// free; the slot is released on drop — including during panic unwinding, so a
/// parser panic (the PDF extractor catches them) can't leak a slot.
struct HeavyExtractionPermit;

impl HeavyExtractionPermit {
    fn acquire() -> Self {
        let mut active = HEAVY_EXTRACTION_GATE.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while *active >= MAX_CONCURRENT_HEAVY_EXTRACTIONS {
            active = HEAVY_EXTRACTION_GATE.slot_freed.wait(active).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *active += 1;
        HeavyExtractionPermit
    }
}

impl Drop for HeavyExtractionPermit {
    fn drop(&mut self) {
        {
            let mut active = HEAVY_EXTRACTION_GATE.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *active = active.saturating_sub(1);
        }
        HEAVY_EXTRACTION_GATE.slot_freed.notify_one();
    }
}

/// Single entry point — returns Some(text) for any supported format, None
/// for unsupported extensions, encrypted files, or extraction failures.
///
/// `user_max_bytes` is the global ceiling from settings. Each format has
/// its own tier default; the effective per-file limit is the smaller of
/// the two so the user's cap always wins.
pub fn extract_text_for_indexing(path: &Path, user_max_bytes: usize) -> Option<String> {
    let ext = path.extension().and_then(|v| v.to_str()).unwrap_or_default().to_lowercase();

    // Apply per-format tier limit capped by the user's global ceiling.
    let max_bytes = tier_bytes_for_path(path).min(user_max_bytes);

    // Plain-text & code formats — `extract_plain_text` reads raw bytes in
    // fixed chunks up to `max_bytes` with a binary-content guard, so it is
    // already memory-safe for any file size and needs no input guard or gate.
    if is_plain_text_extension(&ext) {
        return extract_plain_text(path, max_bytes);
    }

    // Wave 8B / task #88 (2026-05-28): OCR-on-Index for images. Same
    // opt-in + folder-scope rules as the PDF fallback in
    // `ocr_fallback_for_pdf`. Returns None immediately when OCR is
    // disabled (one env-var read, ~ns) so an opt-out user pays
    // basically nothing for images flowing through the pool.
    if is_ocr_image_extension(&ext) {
        return extract_image_text_via_ocr(path, max_bytes);
    }

    // Heavy formats load the whole file into memory. Anything without a
    // ceiling here is an unsupported extension → no content.
    let ceiling = heavy_format_input_ceiling(&ext)?;

    // Input-size guard — skip content extraction for files over the per-format
    // ceiling. They stay indexed by name + metadata; only full-text is lost.
    match fs::metadata(path) {
        Ok(meta) if meta.len() > ceiling => return None,
        Ok(_) => {}
        Err(_) => return None,
    }

    // Concurrency gate — bound how many heavy parses run at once so transient
    // parser memory cannot stack across all of the indexer's walker threads.
    let _permit = HeavyExtractionPermit::acquire();
    match ext.as_str() {
        "pdf" => extract_pdf_text(path, max_bytes),
        "docx" => extract_docx_text(path, max_bytes),
        "xlsx" => extract_xlsx_text(path, max_bytes),
        "pptx" => extract_pptx_text(path, max_bytes),

        // OpenDocument formats — same ZIP + XML structure as OOXML.
        "odt" | "ods" | "odp" => extract_odf_text(path, max_bytes),

        "rtf" => extract_rtf_text(path, max_bytes),

        _ => None,
    }
}

/// Returns true if we have an extractor for this path's extension. Used by
/// the indexer to decide whether to even attempt content extraction.
/// Wave 8.1 (2026-05-28): walker-side OCR filtering replaced the old
/// monolithic `is_supported_extension(path)` predicate with the two
/// focused helpers below (`is_text_or_doc_extension` and
/// `is_ocr_image_extension`). The walker now combines them with the
/// FileSearchIndexOptions OCR config in `should_index_for_content`,
/// so this module no longer owns a single-step "is this indexable?"
/// check — that question only makes sense WITH the OCR config in
/// scope, and the walker has it.
///
/// Wave 8.1 (2026-05-28): split for walker-side OCR filtering. Text /
/// code / document formats are ALWAYS indexable when content indexing
/// is enabled — they don't depend on OCR config. The walker uses this
/// as the "definitely include" leg of `should_index_for_content` so it
/// can avoid sending images to the extractor pool when OCR is off or
/// the path isn't in a configured OCR folder.
pub fn is_text_or_doc_extension(ext: &str) -> bool {
    matches!(
        ext,
        "txt"
            | "md"
            | "markdown"
            | "ki"
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
            | "css"
            | "pdf"
            | "docx"
            | "xlsx"
            | "pptx"
            | "odt"
            | "ods"
            | "odp"
            | "rtf"
    )
}

/// Wave 8B / task #88 (2026-05-28): image extensions eligible for the
/// OCR-on-Index fallback. Tesseract reads PNG/JPG/JPEG/TIF/TIFF/BMP
/// natively; WEBP is converted to a temp PNG first via the `image`
/// crate (tesseract doesn't read webp). HEIC/HEIF deferred — needs
/// libheif-rs and that crate brings a native C library dependency
/// the bundler would have to ship; revisit when there's user demand.
///
/// Public so the walker's `should_index_for_content` can short-circuit
/// images BEFORE submitting them to the extractor pool — saves the
/// IPC round-trip when OCR is off or the path isn't in scope.
pub fn is_ocr_image_extension(ext: &str) -> bool {
    matches!(ext, "png" | "jpg" | "jpeg" | "tif" | "tiff" | "bmp" | "webp")
}

// ─── Plain-text extractor ──────────────────────────────────────────────────

fn extract_plain_text(path: &Path, max_bytes: usize) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut limited = Vec::with_capacity(max_bytes.min(128 * 1024));
    let mut chunk = vec![0u8; 32 * 1024];
    let mut total = 0usize;

    loop {
        let read = file.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(total);
        if remaining == 0 {
            break;
        }
        let take = read.min(remaining);
        limited.extend_from_slice(&chunk[..take]);
        total += take;
        if total >= max_bytes {
            break;
        }
    }

    if limited.is_empty() || looks_binary(&limited) {
        return None;
    }
    Some(String::from_utf8_lossy(&limited).to_string())
}

/// Heuristic: too many control characters or any NUL means we read into a
/// binary file with a text-like extension (rare but defensive).
fn looks_binary(bytes: &[u8]) -> bool {
    let sample_len = bytes.len().min(4096);
    if sample_len == 0 {
        return true;
    }
    let mut control = 0usize;
    for byte in bytes.iter().take(sample_len) {
        if *byte == 0 {
            return true;
        }
        if *byte < 9 || (*byte > 13 && *byte < 32) {
            control += 1;
        }
    }
    control as f32 / sample_len as f32 > 0.15
}

/// Read at most `cap` bytes from the start of a file. Used by extractors whose
/// parser produces text in input order and stops early — reading a bounded
/// prefix keeps memory flat regardless of the file's true size.
fn read_capped(path: &Path, cap: usize) -> Option<Vec<u8>> {
    let file = fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(cap as u64).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Wave 8 / task #88 (2026-05-28): builder for the env vars that propagate
/// the OCR-on-Index config to extractor children. Used by
/// `search.rs::spawn_extractor_child` to call `.env()` on the spawn Command
/// — children inherit them and `OcrIndexConfig::from_env()` below reads
/// them once into a thread_local on first use. Empty when OCR is disabled,
/// so spawn sites can unconditionally call `apply_to` without paying for
/// the env setup when the user hasn't opted in.
#[derive(Debug, Clone, Default)]
pub struct OcrChildEnv {
    vars: Vec<(String, String)>,
    /// Worst case OCR may add to one file (0 with OCR off).
    ocr_secs: u64,
}

impl OcrChildEnv {
    pub fn from_options(
        enabled: bool,
        folders: &[String],
        max_pages: u32,
        langs: &str,
        // Wave 8.2 (2026-05-28): smart-skip heuristic knobs.
        min_image_dim: u32,
        max_aspect_ratio: f32,
        per_file_timeout_secs: u32,
    ) -> Self {
        let mut vars = Vec::new();
        if enabled {
            vars.push(("DASH_OCR_ENABLED".to_string(), "1".to_string()));
            // Newline-separated paths — folders can legally contain commas
            // (Windows allows them in file names), so newline is the safe
            // delimiter. Paths cannot legally contain newlines on Windows
            // or Unix file systems.
            vars.push(("DASH_OCR_FOLDERS".to_string(), folders.join("\n")));
            vars.push(("DASH_OCR_MAX_PAGES".to_string(), max_pages.to_string()));
            vars.push(("DASH_OCR_LANGS".to_string(), langs.to_string()));
            vars.push(("DASH_OCR_MIN_DIM".to_string(), min_image_dim.to_string()));
            vars.push(("DASH_OCR_MAX_ASPECT".to_string(), max_aspect_ratio.to_string()));
            vars.push(("DASH_OCR_TIMEOUT_SECS".to_string(), per_file_timeout_secs.to_string()));
        }
        // Pages may be OCR'd one after another, each up to its own timeout
        // (an untimed OCR is budgeted 5 minutes a page).
        let per_page = if per_file_timeout_secs == 0 { 300 } else { u64::from(per_file_timeout_secs) };
        let ocr_secs = if enabled { u64::from(max_pages.max(1)) * per_page } else { 0 };
        Self { vars, ocr_secs }
    }

    pub fn apply_to(&self, command: &mut std::process::Command) {
        for (k, v) in &self.vars {
            command.env(k, v);
        }
    }

    /// How long an extractor child may spend on one file before it is
    /// killed: 60 s for parsing, plus the OCR budget when OCR is on.
    pub fn extract_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(60 + self.ocr_secs)
    }
}

/// Plain-text and code formats: read as bytes, no parser involved.
pub fn is_plain_text_extension(ext: &str) -> bool {
    matches!(
        ext,
        "txt"
            | "md"
            | "markdown"
            | "ki"
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
            | "css"
    )
}

// ─── OCR-on-Index config (Wave 8 / task #88, 2026-05-28) ──────────────────
//
// Scan-only PDFs (image-only, no text layer) become searchable when the
// user opts in. Config is propagated to the extractor SUBPROCESS via env
// vars set on the spawn `Command` by `search.rs::spawn_extractor_child`,
// because env vars are inherited and don't require threading new CLI
// args through every spawn site. Read once into a thread_local on first
// use to avoid hammering env-lookup per file.
//
//   DASH_OCR_ENABLED      = "1" or absent
//   DASH_OCR_FOLDERS      = newline-separated canonical paths
//   DASH_OCR_MAX_PAGES    = u32, defaults to 20
//   DASH_OCR_LANGS        = e.g. "eng+rus+kat", defaults to "eng"

#[derive(Debug, Clone)]
struct OcrIndexConfig {
    enabled: bool,
    folders: Vec<PathBuf>,
    max_pages: u32,
    langs: String,
    /// Wave 8.2 (2026-05-28): smart skip heuristics. Wider/taller images
    /// than this on either dimension is required; smaller is skipped.
    /// Default 400 px — skips thumbnails, button glyphs, icons. 0 = no
    /// minimum (skip the check).
    min_image_dim: u32,
    /// Maximum aspect ratio (longer side / shorter side). Banners,
    /// sliders, header strips often hit 10:1; default 5.0 skips those
    /// without losing real document scans. 0 = no maximum.
    max_aspect_ratio: f32,
    /// Per-file Tesseract timeout in seconds. Bounds the worst case on
    /// a complex/busy image where Tesseract can spin for minutes. The
    /// file is marked failed (returns None) so subsequent rebuilds
    /// skip it via the existing skip-unchanged fast-path. 0 = no
    /// timeout (existing behavior).
    per_file_timeout_secs: u32,
}

impl OcrIndexConfig {
    fn from_env() -> Self {
        let enabled = std::env::var("DASH_OCR_ENABLED").ok().as_deref() == Some("1");
        let folders = std::env::var("DASH_OCR_FOLDERS")
            .ok()
            .unwrap_or_default()
            .lines()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        let max_pages = std::env::var("DASH_OCR_MAX_PAGES")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(20)
            .clamp(1, 200);
        let langs =
            std::env::var("DASH_OCR_LANGS").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "eng".to_string());
        // Wave 8.2: smart-skip heuristics. Defaults chosen to skip
        // obvious-junk (thumbnails / banners / icons) without losing
        // real document scans.
        let min_image_dim =
            std::env::var("DASH_OCR_MIN_DIM").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(400);
        let max_aspect_ratio =
            std::env::var("DASH_OCR_MAX_ASPECT").ok().and_then(|v| v.trim().parse::<f32>().ok()).unwrap_or(5.0);
        let per_file_timeout_secs =
            std::env::var("DASH_OCR_TIMEOUT_SECS").ok().and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(30);
        Self { enabled, folders, max_pages, langs, min_image_dim, max_aspect_ratio, per_file_timeout_secs }
    }

    /// True when OCR is enabled AND the path lives inside (or equals) at
    /// least one configured folder. Case-insensitive comparison; separators
    /// normalized to forward slashes on BOTH sides so a folder picked via
    /// Tauri's dialog (often returns `C:/OCRTest`) still matches a path the
    /// walker yields with backslashes (`C:\OCRTest\img.png`).
    ///
    /// Wave 8.4.1 (2026-05-29) fix: the previous version only checked one
    /// separator at a time and silently failed when the dialog and walker
    /// disagreed on separator style — the symptom was images in scope
    /// getting skipped while PDFs (which don't need this check) went
    /// through fine.
    fn applies_to(&self, path: &Path) -> bool {
        if !self.enabled || self.folders.is_empty() {
            return false;
        }
        let path_str = path.to_string_lossy().to_lowercase().replace('\\', "/");
        self.folders.iter().any(|root| {
            let root_str = root.to_string_lossy().to_lowercase().replace('\\', "/");
            let root_str = root_str.trim_end_matches('/');
            path_str == root_str || path_str.starts_with(&format!("{}/", root_str))
        })
    }
}

thread_local! {
    static OCR_INDEX_CONFIG: RefCell<Option<OcrIndexConfig>> = const { RefCell::new(None) };
}

fn with_ocr_config<R>(f: impl FnOnce(&OcrIndexConfig) -> R) -> R {
    OCR_INDEX_CONFIG.with(|slot| {
        let mut guard = slot.borrow_mut();
        if guard.is_none() {
            *guard = Some(OcrIndexConfig::from_env());
        }
        f(guard.as_ref().expect("OcrIndexConfig initialized above"))
    })
}

/// Wave 8 / task #88 (2026-05-28): OCR fallback for scan-only PDFs.
///
/// Called when `pdfium_extract_pdf_text` returned no text from a PDF the
/// pdfium-render layer could open — the file IS a PDF, it just has no
/// text layer (scanned receipts, ID photos saved as PDF, screenshots
/// printed-to-PDF). When OCR-on-Index is enabled and the path is in a
/// configured folder, this renders pages via pdfium and runs each one
/// through Tesseract. Strictly capped: max pages per file, single
/// HeavyExtractionPermit held across the whole loop so concurrent walker
/// threads can't pile up tesseract processes.
///
/// Returns the OCR'd text on success, None when OCR is disabled, the
/// path isn't in scope, or every page failed. Per-page failures don't
/// abort the loop — partial OCR is better than nothing.
fn ocr_fallback_for_pdf(path: &Path, max_bytes: usize) -> Option<String> {
    use rayon::prelude::*;
    let config = with_ocr_config(|c| c.clone());
    if !config.applies_to(path) {
        return None;
    }
    // 200 DPI is the sweet spot for Tesseract — empirically yields ~95%
    // accuracy on receipts/contracts while keeping the bitmap under
    // ~5 MB for typical letter-sized pages.
    const OCR_RENDER_DPI: u32 = 200;
    let temp_dir = std::env::temp_dir();
    let pid = std::process::id();

    // Wave 8.3 (2026-05-28): two-phase render + OCR. The render phase
    // stays serial because pdfium-render isn't thread-safe (each call
    // would need its own thread_local PDFium handle, opening the doc
    // multiple times); render is fast (~50-100ms/page at 200 DPI) so
    // serial is fine. The OCR phase is parallelised via rayon —
    // Tesseract is CPU-bound per page, so a 20-page scan goes from
    // ~40s serial to ~5-10s on an 8-core machine. The rayon pool is
    // the global one (`cpu_count` threads), but since the extractor
    // child is single-threaded for stdin/stdout the parent's pool of
    // N children means at most N PDFs in this path at once — bounded
    // total Tesseract concurrency.

    // Phase 1: render pages to temp PNGs serially. Stop at the
    // configured max_pages OR when we hit the document's end (the
    // first None signals "past end").
    struct RenderedPage {
        index: u32,
        temp_path: PathBuf,
    }
    let mut rendered: Vec<RenderedPage> = Vec::with_capacity(config.max_pages as usize);
    for page_index in 0..config.max_pages {
        // pdfium page indexes are u16; there is no page past that to render.
        let Ok(pdfium_index) = u16::try_from(page_index) else {
            break;
        };
        let Some(png_bytes) = pdfium_render_page_to_png(path, pdfium_index, OCR_RENDER_DPI) else {
            break;
        };
        let temp_path =
            temp_dir.join(format!("dash-ocr-{pid}-{}-{}.png", std::ptr::addr_of!(config) as usize, page_index));
        if fs::write(&temp_path, &png_bytes).is_err() {
            continue;
        }
        rendered.push(RenderedPage { index: page_index, temp_path });
    }

    // Phase 2: OCR all rendered pages in parallel. Wrap each result
    // in a TempPngFile RAII guard so the temp PNG is removed whether
    // OCR succeeded, failed, or panicked. par_iter preserves input
    // order in the output Vec, so the final text is page-ordered.
    let mut page_texts: Vec<(u32, Option<String>)> = rendered
        .par_iter()
        .map(|rp| {
            let _cleanup = TempPngFile(rp.temp_path.clone());
            let result = ocr_with_timeout(&rp.temp_path, &config.langs, config.per_file_timeout_secs);
            (rp.index, result.ok())
        })
        .collect();

    // Phase 3: assemble page texts in page-order. Truncate as soon as
    // we hit max_bytes — wastes the OCR work past that point but the
    // index size cap is what the user actually cares about.
    page_texts.sort_by_key(|(idx, _)| *idx);
    let mut text = String::with_capacity(8 * 1024);
    for (_, page_text) in page_texts {
        if text.len() >= max_bytes {
            break;
        }
        if let Some(t) = page_text {
            let trimmed = t.trim();
            if !trimmed.is_empty() {
                text.push_str(trimmed);
                text.push('\n');
            }
        }
    }

    if text.is_empty() {
        return None;
    }
    truncate_safely(&mut text, max_bytes);
    Some(text)
}

/// RAII guard that deletes a temp file (typically a rendered PDF page or
/// a WEBP→PNG conversion) when dropped. Owns the path so callers don't
/// have to juggle borrow lifetimes around panic-prone code (Tesseract).
struct TempPngFile(PathBuf);
impl Drop for TempPngFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Wave 8B / task #88 (2026-05-28): OCR an image file directly.
///
/// Called from `extract_text_for_indexing` when the file is one of the
/// image extensions in `is_ocr_image_extension`. Returns None when OCR
/// is disabled or the path isn't in a configured folder — that's the
/// "opt-out fast path" — so a user who hasn't enabled OCR pays only
/// for one env-var read per image (the thread_local caches it after
/// the first read).
///
/// Format handling:
///   PNG / JPG / JPEG / TIF / TIFF / BMP → pass directly to Tesseract.
///     Tesseract reads these natively, no conversion needed.
///   WEBP → load via `image` crate, save to temp PNG, OCR the temp PNG,
///     RAII-clean. Tesseract doesn't read WEBP, so the conversion is
///     mandatory; the image crate has the `webp` feature on already.
///   HEIC / HEIF → not in this list (see `is_ocr_image_extension`).
///     Adding HEIC needs `libheif-rs` which pulls a native C library
///     the bundler would have to ship — deferred until there's user
///     demand.
///
/// Skips images smaller than 64×64 — those are favicons / button
/// glyphs / spacer GIFs masquerading as PNG, never have meaningful
/// text, and OCR'ing them wastes 1-2 seconds per file on a large
/// Downloads folder.
fn extract_image_text_via_ocr(path: &Path, max_bytes: usize) -> Option<String> {
    let config = with_ocr_config(|c| c.clone());
    if !config.applies_to(path) {
        return None;
    }
    let ext = path.extension().and_then(|v| v.to_str()).unwrap_or_default().to_lowercase();

    // Cheap file-size floor — a real-content scan is almost never under
    // 8 KB. Saves opening every tiny PNG on disk just to discover it's
    // a 16×16 icon.
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() < 8 * 1024 {
            return None;
        }
    }

    // Wave 8.2 (2026-05-28): smart-skip heuristics. Read the image header
    // (NOT the pixel data — fast even on a 50 MB photo) to get
    // dimensions, then apply the user's min-dimension + max-aspect-ratio
    // gates BEFORE we spend any of Tesseract's time on the file.
    if config.min_image_dim > 0 || config.max_aspect_ratio > 0.0 {
        if let Some((w, h)) = read_image_dimensions(path) {
            if config.min_image_dim > 0 && (w < config.min_image_dim || h < config.min_image_dim) {
                return None;
            }
            if config.max_aspect_ratio > 0.0 {
                let long = w.max(h) as f32;
                let short = w.min(h).max(1) as f32;
                if (long / short) > config.max_aspect_ratio {
                    return None;
                }
            }
        }
        // Unreadable header (corrupt / unsupported variant) → let
        // Tesseract have a try at it. Most likely we'll error out
        // there too, but no use rejecting purely on a header read
        // failure.
    }

    let ocr_target: PathBuf;
    let _cleanup;

    match ext.as_str() {
        "webp" => {
            // Convert WEBP → temp PNG (Tesseract doesn't read WEBP). Panic-
            // guarded — a malformed WEBP must never crash the extractor
            // subprocess.
            let png_bytes = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let dyn_image = image::open(path).ok()?;
                let mut buf: Vec<u8> = Vec::new();
                use std::io::Cursor;
                dyn_image.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png).ok()?;
                Some(buf)
            })) {
                Ok(Some(b)) => b,
                _ => return None,
            };
            let temp_dir = std::env::temp_dir();
            let pid = std::process::id();
            let temp_path = temp_dir.join(format!("dash-ocr-img-{pid}-{}.png", std::ptr::addr_of!(config) as usize,));
            if fs::write(&temp_path, &png_bytes).is_err() {
                return None;
            }
            // Clone the path: one copy for the OCR target, one held by
            // the RAII guard so the file is removed even on a Tesseract
            // panic.
            _cleanup = Some(TempPngFile(temp_path.clone()));
            ocr_target = temp_path;
        }
        // Tesseract reads these directly.
        "png" | "jpg" | "jpeg" | "tif" | "tiff" | "bmp" => {
            _cleanup = None;
            ocr_target = path.to_path_buf();
        }
        _ => return None,
    }

    let text = match ocr_with_timeout(&ocr_target, &config.langs, config.per_file_timeout_secs) {
        Ok(t) => t,
        Err(_) => return None,
    };
    drop(_cleanup);
    let mut text = text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    truncate_safely(&mut text, max_bytes);
    Some(text)
}

/// Wave 8.2 (2026-05-28): read just the image header to get
/// dimensions. `ImageReader::with_guessed_format` + `into_dimensions`
/// parses ONLY the header bytes — fast (~1ms) even on a multi-MB
/// photo, vs. ~100ms for a full decode. Returns None when the format
/// is unknown or the file is corrupt; the caller falls back to letting
/// Tesseract try.
fn read_image_dimensions(path: &Path) -> Option<(u32, u32)> {
    let reader = image::ImageReader::open(path).ok()?;
    let reader = reader.with_guessed_format().ok()?;
    reader.into_dimensions().ok()
}

/// Wave 8.2 (2026-05-28): wrap an OCR call with a per-file timeout.
///
/// Re-uses the existing OCR cancellation infrastructure: generates a
/// unique operation_id, hands it to `ocr_image_file` so the spawned
/// Tesseract Child is stashed in OCR_CHILDREN, and spawns a watchdog
/// thread that calls `cancel_ocr_operation(op_id)` after the timeout
/// elapses. `cancel_ocr_operation` kills the child if it's still
/// running; the OCR call then returns "Cancelled" which we re-map to
/// a clear timeout error.
///
/// `timeout_secs == 0` means "no timeout" — falls through to the
/// existing untimed path with operation_id = None.
///
/// The watchdog waits on a channel that is closed when OCR returns, so
/// it exits as soon as the work is done instead of sleeping the full
/// timeout.
fn ocr_with_timeout(image_path: &Path, langs: &str, timeout_secs: u32) -> Result<String, String> {
    if timeout_secs == 0 {
        return super::ocr::ocr_image_file(image_path, langs, false, None, &None);
    }
    // A counter, not the clock: parallel PDF pages can start in the same
    // nanosecond and must not share an id.
    static NEXT_OCR_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let op_id =
        format!("ocr-{}-{}", std::process::id(), NEXT_OCR_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let op_id_for_watchdog = op_id.clone();
    let timeout = std::time::Duration::from_secs(timeout_secs as u64);
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = done_rx.recv_timeout(timeout) {
            let _ = super::ocr::cancel_ocr_operation(op_id_for_watchdog);
        }
    });
    let result = super::ocr::ocr_image_file(image_path, langs, false, None, &Some(op_id));
    // Wakes the watchdog (disconnected) so it exits now.
    drop(done_tx);
    match result {
        Ok(text) => Ok(text),
        Err(e) if e == "Cancelled" => Err(format!("OCR exceeded {timeout_secs}s timeout")),
        Err(other) => Err(other),
    }
}

// ─── PDF extractor (PDFium primary, lopdf fallback) ────────────────────────

/// Extract a PDF's text for indexing. PDFium runs first — it lazy-loads (only
/// the pages it reads, so memory stays bounded even on a 250 MB book) and reads
/// correct Unicode where lopdf silently returns nothing (e.g. Georgian CID/Type0
/// fonts — verified on a real corpus). If PDFium is unavailable (no `pdfium.dll`)
/// or yields nothing, we fall back to the original lopdf path, so there is no
/// regression on files PDFium can't open.
fn extract_pdf_text(path: &Path, max_bytes: usize) -> Option<String> {
    if let Some(text) = pdfium_extract_pdf_text(path, max_bytes) {
        return Some(text);
    }
    if let Some(text) = lopdf_extract_pdf_text(path, max_bytes) {
        return Some(text);
    }
    // Wave 8 / task #88 (2026-05-28): both text-layer extractors returned
    // nothing. That's the scan-only-PDF signal (image-only file, no text).
    // If the user opted into OCR-on-Index AND this file is in a configured
    // folder, fall back to rendering pages + Tesseract OCR. The
    // `applies_to` check inside `ocr_fallback_for_pdf` short-circuits when
    // disabled, so the cost when OCR is off is exactly one thread_local
    // read (~ns).
    ocr_fallback_for_pdf(path, max_bytes)
}

/// Process-wide PDFium handle, bound once per thread and reused. The extractor
/// worker is single-threaded (it reads one path at a time off stdin), and a
/// `thread_local` sidesteps PDFium's non-thread-safety without any locking. The
/// three states avoid re-attempting a failed bind (e.g. missing DLL) per file.
enum PdfiumSlot {
    Untried,
    Unavailable,
    Ready(Pdfium),
}

thread_local! {
    static PDFIUM: RefCell<PdfiumSlot> = const { RefCell::new(PdfiumSlot::Untried) };
}

/// Bind to `pdfium.dll`. Tries the copy beside the current executable first
/// (bundled there via tauri.conf.json `bundle.resources`, like the Vosk DLLs —
/// works in both the main process and the same-exe extractor worker), then the
/// OS library search path. Returns None when no PDFium library is present.
fn bind_pdfium() -> Option<Pdfium> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let bindings = exe_dir
        .and_then(|dir| Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&dir)).ok())
        .or_else(|| Pdfium::bind_to_system_library().ok())?;
    Some(Pdfium::new(bindings))
}

/// PDFium extraction. Returns None on any failure (no DLL, can't open — e.g.
/// encrypted, or genuinely no text) so the caller can fall back to lopdf.
fn pdfium_extract_pdf_text(path: &Path, max_bytes: usize) -> Option<String> {
    PDFIUM.with(|slot| {
        {
            let mut state = slot.borrow_mut();
            if matches!(*state, PdfiumSlot::Untried) {
                *state = match bind_pdfium() {
                    Some(pdfium) => PdfiumSlot::Ready(pdfium),
                    None => PdfiumSlot::Unavailable,
                };
            }
        }
        let state = slot.borrow();
        let PdfiumSlot::Ready(pdfium) = &*state else {
            return None; // no PDFium library — caller falls back to lopdf
        };
        // Guard against a panic inside the FFI wrapper on a malformed file —
        // never abort the indexer; just fall back to lopdf.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let document = pdfium.load_pdf_from_file(path, None).ok()?;
            let mut text = String::with_capacity(8 * 1024);
            // `pages().iter()` loads each page on demand and drops (closes) it at
            // the end of the iteration, so peak memory is ~one page, not the
            // whole document. Stop once we hit the cap.
            for page in document.pages().iter() {
                if text.len() >= max_bytes {
                    break;
                }
                if let Ok(page_text) = page.text() {
                    let chunk = page_text.all();
                    if !chunk.is_empty() {
                        text.push_str(&chunk);
                        text.push('\n');
                    }
                }
            }
            if text.is_empty() {
                return None;
            }
            truncate_safely(&mut text, max_bytes);
            Some(text)
        }))
        .ok()
        .flatten()
    })
}

/// Wave 8 / task #88 (2026-05-28): render a single PDF page to PNG bytes via
/// the SAME thread_local PDFium handle used by text extraction. No std::sync::Arc<dyn dash_core::Host>,
/// no WinRT — works inside the extractor subprocess where the legacy
/// `render_windows_pdf_page_bytes()` in `commands/pdf.rs` could not (it needs
/// the Tauri main-thread WinRT apartment).
///
/// Used by the OCR-on-Index fallback: when `pdfium_extract_pdf_text` returns
/// no text from a PDF (image-only / scan-only file) AND the user opted in,
/// the indexer renders pages here and pipes them through Tesseract via
/// `ocr::ocr_image_file` so scanned receipts / contracts / IDs become
/// searchable just like real-text PDFs.
///
/// `dpi` is clamped to a sane [72, 400] range — anything above ~300 inflates
/// memory linearly without measurably improving OCR accuracy. Returns None on
/// any failure (no PDFium library, can't open document, render error, encode
/// error) so the caller can skip the page and continue indexing.
fn pdfium_render_page_to_png(path: &Path, page_index: u16, dpi: u32) -> Option<Vec<u8>> {
    use std::io::Cursor;
    let clamped_dpi = dpi.clamp(72, 400);
    PDFIUM.with(|slot| {
        {
            let mut state = slot.borrow_mut();
            if matches!(*state, PdfiumSlot::Untried) {
                *state = match bind_pdfium() {
                    Some(pdfium) => PdfiumSlot::Ready(pdfium),
                    None => PdfiumSlot::Unavailable,
                };
            }
        }
        let state = slot.borrow();
        let PdfiumSlot::Ready(pdfium) = &*state else {
            return None;
        };
        // Panic-guard the FFI wrapper exactly like the text-extract path —
        // a malformed PDF must never abort the indexer subprocess; just skip
        // the page and the file gets indexed by name + metadata only.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let document = pdfium.load_pdf_from_file(path, None).ok()?;
            let pages = document.pages();
            let page = pages.get(page_index).ok()?;
            // PdfPoints are in PDF points (1pt = 1/72 inch). Scale to the
            // requested DPI before allocating the bitmap.
            let scale = clamped_dpi as f32 / 72.0;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "rounded and at least 1; `as` saturates instead of wrapping for an absurd page size"
            )]
            let width_px = (page.width().value * scale).round().max(1.0) as i32;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "rounded and at least 1; `as` saturates instead of wrapping for an absurd page size"
            )]
            let height_px = (page.height().value * scale).round().max(1.0) as i32;
            let bitmap = page.render(width_px, height_px, None).ok()?;
            let dyn_image = bitmap.as_image();
            let mut buf: Vec<u8> = Vec::new();
            dyn_image.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png).ok()?;
            Some(buf)
        }))
        .ok()
        .flatten()
    })
}

fn lopdf_extract_pdf_text(path: &Path, max_bytes: usize) -> Option<String> {
    use lopdf::Document;

    // PDFs can be slow or fail to parse — catch panics from lopdf's internals
    // so a single malformed file doesn't abort the indexer.
    let doc = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Document::load(path))).ok()?.ok()?;

    let mut text = String::with_capacity(8 * 1024);
    let pages = doc.get_pages();
    if pages.is_empty() {
        return None;
    }

    // Iterate pages in order. Stop early once we hit the byte cap so a 500-page
    // report doesn't waste seconds extracting text we'll never index.
    let mut page_nums: Vec<u32> = pages.keys().copied().collect();
    page_nums.sort();

    for page_num in page_nums {
        if text.len() >= max_bytes {
            break;
        }
        let extracted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| doc.extract_text(&[page_num])))
            .ok()
            .and_then(|r| r.ok());

        if let Some(page_text) = extracted {
            if !page_text.is_empty() {
                text.push_str(&page_text);
                text.push('\n');
            }
        }
    }

    if text.is_empty() {
        return None;
    }
    truncate_safely(&mut text, max_bytes);
    Some(text)
}

// ─── DOCX extractor (zip + XML strip) ──────────────────────────────────────

fn extract_docx_text(path: &Path, max_bytes: usize) -> Option<String> {
    // DOCX is a ZIP with the body in word/document.xml. Headers/footers/footnotes
    // live in adjacent files — index them too so the search covers everything
    // a user would visually see in the document.
    let candidates = &[
        "word/document.xml",
        "word/header1.xml",
        "word/header2.xml",
        "word/header3.xml",
        "word/footer1.xml",
        "word/footer2.xml",
        "word/footer3.xml",
        "word/footnotes.xml",
        "word/endnotes.xml",
        "word/comments.xml",
    ];
    extract_from_zip_xml_files(path, candidates, max_bytes)
}

// ─── PPTX extractor (zip + every slide) ────────────────────────────────────

fn extract_pptx_text(path: &Path, max_bytes: usize) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    // Enumerate slide XML paths first (we can't hold a ZipFile borrow across
    // iterations of the archive).
    let mut slide_paths: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index(i) {
            let name = entry.name();
            if name.starts_with("ppt/slides/slide") && name.ends_with(".xml") {
                slide_paths.push(name.to_string());
            }
            // Also pick up slide notes if present.
            if name.starts_with("ppt/notesSlides/notesSlide") && name.ends_with(".xml") {
                slide_paths.push(name.to_string());
            }
        }
    }
    // Sort so slide order is deterministic across runs.
    slide_paths.sort();

    let mut text = String::with_capacity(8 * 1024);
    for path_name in slide_paths {
        if text.len() >= max_bytes {
            break;
        }
        let Ok(entry) = archive.by_name(&path_name) else { continue };
        strip_xml_reader_to_text(std::io::BufReader::new(entry), &mut text, max_bytes);
        text.push('\n');
    }

    if text.is_empty() {
        return None;
    }
    truncate_safely(&mut text, max_bytes);
    Some(text)
}

// ─── ODT / ODS / ODP extractor (OpenDocument zip + content.xml) ────────────

fn extract_odf_text(path: &Path, max_bytes: usize) -> Option<String> {
    // ODT (text), ODS (spreadsheet), and ODP (presentation) all share the
    // same structure: a ZIP with the body in content.xml and headers/footers
    // plus named-style content in styles.xml. content.xml carries the bulk
    // of what a user would search for.
    extract_from_zip_xml_files(path, &["content.xml", "styles.xml"], max_bytes)
}

// ─── RTF extractor ─────────────────────────────────────────────────────────

/// Extract plain text from an RTF file. RTF is a flat text format with
/// control sequences interleaved into the content — `\b` for bold,
/// `\par` for paragraph break, `\'XX` for hex-encoded bytes, etc.
///
/// We deliberately skip well-known metadata destinations (fonttbl, colortbl,
/// stylesheet, info, pictures, embedded objects) and pass through visible
/// document text. The output is good enough for search indexing — not a
/// pixel-perfect document recreation.
fn extract_rtf_text(path: &Path, max_bytes: usize) -> Option<String> {
    // Read only a size-capped prefix, not the whole file: the parser below
    // stops at `max_bytes` of *output*, and RTF markup means the visible text
    // is a fraction of the file, so reading ~8x the output cap covers it while
    // bounding memory regardless of how large the file is. RTF is technically
    // ASCII with escapes for non-ASCII, but legacy files carry stray UTF-8, so
    // the bytes are still decoded lossily.
    let raw = read_capped(path, max_bytes.saturating_mul(8).max(64 * 1024))?;
    let rtf = String::from_utf8_lossy(&raw);

    let mut out = String::with_capacity(max_bytes.min(rtf.len()));
    let mut chars = rtf.chars().peekable();
    let mut group_depth: i32 = 0;
    // When set, we're inside a metadata/binary group and should drop every
    // character until the group closes at that depth.
    let mut skip_until_depth: Option<i32> = None;

    while let Some(c) = chars.next() {
        if out.len() >= max_bytes {
            break;
        }

        match c {
            '{' => {
                group_depth += 1;
            }
            '}' => {
                if let Some(stop) = skip_until_depth {
                    if group_depth <= stop {
                        skip_until_depth = None;
                    }
                }
                group_depth -= 1;
            }
            '\\' => {
                // Control sequence. Look ahead at the next char to decide kind.
                let Some(&next) = chars.peek() else { break };

                if next == '\\' || next == '{' || next == '}' {
                    // Literal escape — `\\`, `\{`, `\}` represent the chars themselves.
                    chars.next();
                    if skip_until_depth.is_none() {
                        out.push(next);
                    }
                    continue;
                }

                if next == '\'' {
                    // Hex byte escape: `\'XX` (2 hex digits, Windows-1252 by default).
                    chars.next(); // consume '
                    let h1 = chars.next();
                    let h2 = chars.next();
                    if let (Some(h1), Some(h2)) = (h1, h2) {
                        let hex: String = [h1, h2].iter().collect();
                        if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                            if skip_until_depth.is_none() {
                                // Map CP1252 → Unicode for the common high-byte range.
                                out.push(cp1252_to_char(byte));
                            }
                        }
                    }
                    continue;
                }

                if next == 'u' {
                    // Unicode escape: `\uNNNN[ ?]` where N can be negative
                    // (16-bit two's complement). A single-char fallback may follow.
                    chars.next();
                    let mut num = String::new();
                    if chars.peek() == Some(&'-') {
                        num.push('-');
                        chars.next();
                    }
                    while let Some(&p) = chars.peek() {
                        if p.is_ascii_digit() {
                            num.push(p);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if let Ok(code) = num.parse::<i32>() {
                        // RTF writes code points above 32767 as negative numbers.
                        let unicode = if code < 0 { code + 0x10000 } else { code };
                        if let Some(c) = u32::try_from(unicode).ok().and_then(char::from_u32) {
                            if skip_until_depth.is_none() {
                                out.push(c);
                            }
                        }
                    }
                    // Eat the optional space delimiter and the fallback char.
                    if chars.peek() == Some(&' ') {
                        chars.next();
                    }
                    if let Some(&p) = chars.peek() {
                        if p == '?' {
                            chars.next();
                        }
                    }
                    continue;
                }

                if next.is_alphabetic() {
                    // Control word: letters [+ optional signed numeric arg] [+ optional space].
                    let mut word = String::with_capacity(8);
                    while let Some(&p) = chars.peek() {
                        if p.is_alphabetic() {
                            word.push(p);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    // Optional signed integer argument.
                    if chars.peek() == Some(&'-') {
                        chars.next();
                    }
                    while let Some(&p) = chars.peek() {
                        if p.is_ascii_digit() {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    // Optional single space delimiter (NOT part of content).
                    if chars.peek() == Some(&' ') {
                        chars.next();
                    }

                    // Translate / handle specific keywords.
                    match word.as_str() {
                        // Soft separators — emit a space so words don't run together.
                        "par" | "line" | "tab" | "page" | "sect" | "cell" | "row" if skip_until_depth.is_none() => {
                            out.push(' ');
                        }
                        // Non-breaking space / hyphens.
                        "nbsp" | "~" if skip_until_depth.is_none() => {
                            out.push(' ');
                        }
                        // Metadata / binary destinations — skip the entire group.
                        "fonttbl" | "colortbl" | "stylesheet" | "info" | "themedata" | "datastore" | "listtable"
                        | "listoverridetable" | "rsidtbl" | "filetbl" | "background" | "pgptbl" | "pict" | "object"
                        | "shppict" | "nonshppict" | "shp" | "shpgrp" | "shpinst" | "panose" | "generator"
                        | "creatim" | "revtim" | "operator" | "title" | "subject" | "author" | "comment"
                        | "keywords" | "company" | "category" | "manager" | "doccomm" | "version" | "hyperlinkbase"
                        | "buptim" | "printim"
                            if skip_until_depth.is_none() =>
                        {
                            skip_until_depth = Some(group_depth);
                        }
                        _ => {} // ignore other control words (formatting hints, etc.)
                    }
                    continue;
                }

                // Anything else: skip the lone backslash char.
                chars.next();
            }
            // Line breaks in RTF source are not part of content — RTF uses \par.
            '\r' | '\n' => {}
            _ => {
                if skip_until_depth.is_none() {
                    out.push(c);
                }
            }
        }
    }

    // Collapse runs of whitespace so the indexer doesn't see endless spaces.
    let collapsed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let mut result = collapsed;
    truncate_safely(&mut result, max_bytes);
    Some(result)
}

/// Map a Windows-1252 byte to its Unicode equivalent. The 0x80-0x9F range
/// has special characters (smart quotes, en/em dashes, etc.) that don't
/// match Unicode directly; the rest of high-bytes are equal to Latin-1.
fn cp1252_to_char(byte: u8) -> char {
    match byte {
        0x80 => '€',
        0x82 => '‚',
        0x83 => 'ƒ',
        0x84 => '„',
        0x85 => '…',
        0x86 => '†',
        0x87 => '‡',
        0x88 => 'ˆ',
        0x89 => '‰',
        0x8A => 'Š',
        0x8B => '‹',
        0x8C => 'Œ',
        0x8E => 'Ž',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201C}',
        0x94 => '\u{201D}',
        0x95 => '•',
        0x96 => '–',
        0x97 => '—',
        0x98 => '˜',
        0x99 => '™',
        0x9A => 'š',
        0x9B => '›',
        0x9C => 'œ',
        0x9E => 'ž',
        0x9F => 'Ÿ',
        _ => byte as char,
    }
}

/// Shared helper for DOCX-like formats: try a list of candidate XML paths
/// inside a ZIP, concatenate stripped text from whichever exist.
fn extract_from_zip_xml_files(path: &Path, candidates: &[&str], max_bytes: usize) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let mut text = String::with_capacity(8 * 1024);

    for candidate in candidates {
        if text.len() >= max_bytes {
            break;
        }
        let Ok(entry) = archive.by_name(candidate) else { continue };
        // Stream `quick-xml` straight off the decompressing entry — it is only
        // decompressed as far as the parser reads, and the parser stops at
        // `max_bytes`, so a huge XML part never lands in memory whole.
        strip_xml_reader_to_text(std::io::BufReader::new(entry), &mut text, max_bytes);
        text.push('\n');
    }

    if text.is_empty() {
        return None;
    }
    truncate_safely(&mut text, max_bytes);
    Some(text)
}

// ─── XLSX extractor (streaming shared strings) ─────────────────────────────

fn extract_xlsx_text(path: &Path, max_bytes: usize) -> Option<String> {
    // XLSX is a ZIP. The bulk of a spreadsheet's searchable text is the shared
    // string table (`xl/sharedStrings.xml`) — sheet cells reference it by
    // index. Streaming that file with `quick-xml` keeps extraction cheap and
    // bounded, instead of `calamine` loading the whole workbook (every sheet,
    // every cell) into memory. Per-cell numbers and the uncommon inline string
    // are not indexed — the shared strings carry a spreadsheet's text.
    let file = fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let mut text = String::with_capacity(8 * 1024);

    // Sheet names (from workbook.xml) are useful search context; they live in
    // `<sheet name="...">` attributes rather than text nodes.
    if let Ok(entry) = archive.by_name("xl/workbook.xml") {
        append_xlsx_sheet_names(std::io::BufReader::new(entry), &mut text, max_bytes);
    }

    // The shared string table — every distinct string used in any cell.
    if let Ok(entry) = archive.by_name("xl/sharedStrings.xml") {
        strip_xml_reader_to_text(std::io::BufReader::new(entry), &mut text, max_bytes);
    }

    if text.is_empty() {
        return None;
    }
    truncate_safely(&mut text, max_bytes);
    Some(text)
}

/// Append the `<sheet name="...">` names from a streamed `xl/workbook.xml`.
/// The names sit in attributes, so `strip_xml_reader_to_text` (text nodes only)
/// would miss them — pull them directly.
fn append_xlsx_sheet_names(reader: impl std::io::BufRead, out: &mut String, max_bytes: usize) {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_reader(reader);
    let mut buf = Vec::new();
    loop {
        if out.len() >= max_bytes {
            break;
        }
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.local_name().as_ref() == b"sheet" => {
                if let Ok(Some(attr)) = e.try_get_attribute("name") {
                    // Sheet names are plain text (entities are vanishingly rare
                    // in them), so a direct UTF-8 decode of the raw value is
                    // enough for search indexing.
                    if let Ok(name) = std::str::from_utf8(attr.value.as_ref()) {
                        out.push_str(name);
                        out.push('\n');
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
}

// ─── XML → plain text ──────────────────────────────────────────────────────

/// Pull text content out of an XML stream, ignoring tags, attributes, and
/// processing instructions. Reads straight from `reader` — typically a
/// decompressing ZIP entry — via `quick-xml`'s pull parser and stops at
/// `max_bytes`, so a large `document.xml` is never fully decompressed into
/// memory: only the prefix the parser actually consumes is.
fn strip_xml_reader_to_text(reader: impl std::io::BufRead, out: &mut String, max_bytes: usize) {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_reader(reader);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();

    loop {
        if out.len() >= max_bytes {
            break;
        }
        match reader.read_event_into(&mut buf) {
            Ok(Event::Text(e)) => {
                if let Ok(text) = e.unescape() {
                    let s = text.as_ref();
                    if !s.trim().is_empty() {
                        out.push_str(s);
                        out.push(' ');
                    }
                }
            }
            Ok(Event::CData(e)) => {
                if let Ok(text) = std::str::from_utf8(e.as_ref()) {
                    if !text.trim().is_empty() {
                        out.push_str(text);
                        out.push(' ');
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
}

/// Truncate a String at a byte boundary that respects UTF-8 character edges.
/// Naive `.truncate(max_bytes)` would panic if it landed mid-codepoint.
fn truncate_safely(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut idx = max_bytes;
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    text.truncate(idx);
}
