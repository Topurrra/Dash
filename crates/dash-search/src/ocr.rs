//! Runs in any app that is a `dash_core::Host`.
//! OCR via Tesseract — shell-out (CLI), not linked libtesseract.
//!
//! We invoke the system `tesseract` binary the same way the Vosk downloader and
//! the Local-AI path invoke external tools (curl): no heavy build-time C dep, no
//! bundled engine to maintain, and the user controls the engine version + which
//! language packs (incl. Georgian `kat`) are installed. Fully on-device /
//! air-gapped — Tesseract never touches the network.
//!
//! This is the OCR *core* (7a): locate tesseract, report availability +
//! languages, and OCR a single image. PDF OCR (render-then-OCR), the content-
//! index fallback, and PDF→Word for scanned docs build on `ocr_image_file`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

// CREATE_NO_WINDOW — keep tesseract's console from flashing on screen, matching
// every other external-process call in the app (voice.rs, quick_actions.rs, …).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ── Cancel registry (Wave 2.5b, 2026-05-27) ───────────────────────────
//
// OCR's cancel shape is different from the walk-loop pattern used by
// hash / crypto / live-grep: Tesseract is a single subprocess; the
// natural cancel is "kill it." We track every in-flight child by
// operation_id so `cancel_ocr_operation` can find + kill the right
// one. Cancellation is also recorded in CANCELLED_OCR so a kill that
// races with the wait can be distinguished from a genuine
// non-zero exit.
//
// An operation is registered (key present, `None`) for the whole call,
// and holds `Some(child)` while tesseract runs — including while its
// pipes are read, so a cancel or timeout can always kill it. Locks are
// taken OCR_CHILDREN first, then CANCELLED_OCR.

static OCR_CHILDREN: LazyLock<Mutex<HashMap<String, Option<Child>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static CANCELLED_OCR: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Cancels a running OCR operation (kills its tesseract). An id that isn't
/// running is ignored, so nothing is left behind in the cancel set.
/// # Errors
/// When a cancel registry lock fails.
pub fn cancel_ocr_operation(operation_id: String) -> Result<(), String> {
    let mut map = OCR_CHILDREN.lock().map_err(|_| "Cancel registry lock failed".to_string())?;
    let Some(slot) = map.get_mut(&operation_id) else {
        return Ok(());
    };
    CANCELLED_OCR.lock().map_err(|_| "Cancel registry lock failed".to_string())?.insert(operation_id);
    // Best-effort kill; the owner sees the closed pipes, reaps the
    // child and returns "Cancelled".
    if let Some(child) = slot.as_mut() {
        let _ = child.kill();
    }
    Ok(())
}

fn is_ocr_cancelled(op_id: Option<&str>) -> bool {
    op_id.and_then(|id| CANCELLED_OCR.lock().ok().map(|s| s.contains(id))).unwrap_or(false)
}

fn clear_ocr_cancel(op_id: Option<&str>) {
    if let Some(id) = op_id {
        if let Ok(mut map) = OCR_CHILDREN.lock() {
            map.remove(id);
        }
        if let Ok(mut set) = CANCELLED_OCR.lock() {
            set.remove(id);
        }
    }
}

/// Registers an operation id for the length of one OCR call and clears it
/// (and any cancel mark) on every way out.
struct OcrRegistration(Option<String>);

impl OcrRegistration {
    fn new(op_id: Option<&str>) -> Self {
        if let Some(id) = op_id {
            if let Ok(mut map) = OCR_CHILDREN.lock() {
                map.insert(id.to_string(), None);
            }
        }
        OcrRegistration(op_id.map(str::to_string))
    }
}

impl Drop for OcrRegistration {
    fn drop(&mut self) {
        clear_ocr_cancel(self.0.as_deref());
    }
}

/// Waits for a registered operation's child, keeping it killable: the child
/// stays in `OCR_CHILDREN` while stdout is read to EOF (stderr on a helper
/// thread). A kill closes the pipes, so the reads end; then the child is
/// taken back out and reaped.
fn wait_registered(id: &str, mut child: Child) -> std::io::Result<std::process::Output> {
    use std::io::Read;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    {
        let mut map = OCR_CHILDREN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // A cancel that came in before the child was stored had nothing
        // to kill: do it now.
        if CANCELLED_OCR.lock().is_ok_and(|s| s.contains(id)) {
            let _ = child.kill();
        }
        map.insert(id.to_string(), Some(child));
    }
    let stderr_reader = stderr.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    });
    let mut out = Vec::new();
    let read = match stdout {
        Some(mut pipe) => pipe.read_to_end(&mut out).map(|_| ()),
        None => Ok(()),
    };
    let child =
        OCR_CHILDREN.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(id).and_then(Option::take);
    let status = match child {
        Some(mut child) => {
            if read.is_err() {
                let _ = child.kill();
            }
            child.wait()
        }
        None => Err(std::io::Error::other("OCR child went missing")),
    };
    let err = stderr_reader.and_then(|t| t.join().ok()).unwrap_or_default();
    read?;
    Ok(std::process::Output { status: status?, stdout: out, stderr: err })
}

/// Build a `Command` for tesseract with the no-console-window flag applied on
/// Windows. Centralized so every call site is consistent.
fn tesseract_command(program: &str) -> Command {
    #[cfg_attr(not(windows), expect(unused_mut, reason = "the creation flags are Windows-only"))]
    let mut command = Command::new(program);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// Does `program` run as a working tesseract (`--version` exits 0)?
fn tesseract_runs(program: &str) -> bool {
    tesseract_command(program).arg("--version").output().is_ok_and(|output| output.status.success())
}

/// Resolve the tesseract executable. Wave 8.4 (2026-05-29) order:
///   1. Bundled `tesseract.exe` next to our own exe — the v1 production
///      path, populated from `src-tauri/tesseract-runtime/` and copied by
///      `tauri.conf.json bundle.resources`. Users never need to install
///      Tesseract separately when this is present.
///   2. System PATH `tesseract` — devs / power-users who already installed it.
///   3. Common Windows install locations — fallback for the
///      `tesseract` PATH lookup failing on freshly-launched processes
///      that don't see the latest installer's PATH update.
///
/// `None` means OCR is unavailable. UI shows the install card.
fn resolve_tesseract() -> Option<String> {
    // Wave 8.4: bundled tesseract.exe alongside the app's executable.
    // tauri.conf.json's "tesseract-runtime/**/*.exe": "." rule places it
    // there for installer builds; in dev mode the same files land next to
    // the debug build's exe in development.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bundled = dir.join(if cfg!(windows) { "tesseract.exe" } else { "tesseract" });
            if bundled.is_file() && tesseract_runs(&bundled.to_string_lossy()) {
                return bundled.to_str().map(str::to_string);
            }
        }
    }
    if tesseract_runs("tesseract") {
        return Some("tesseract".to_string());
    }
    #[cfg(windows)]
    {
        const CANDIDATES: [&str; 3] = [
            r"C:\Program Files\Tesseract-OCR\tesseract.exe",
            r"C:\Program Files (x86)\Tesseract-OCR\tesseract.exe",
            r"C:\Program Files\tesseract.exe",
        ];
        for candidate in CANDIDATES {
            if Path::new(candidate).exists() {
                return Some(candidate.to_string());
            }
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            for sub in [r"Tesseract-OCR\tesseract.exe", r"Programs\Tesseract-OCR\tesseract.exe"] {
                let path = Path::new(&local).join(sub);
                if path.exists() {
                    return path.to_str().map(str::to_string);
                }
            }
        }
    }
    None
}

/// The bundled tessdata directory: `<exe_dir>/tessdata`, holding our best
/// `kat`/`eng` models (shipped via tauri.conf.json resources; copied to
/// `target/debug/tessdata` in dev). `None` when it isn't present.
fn bundled_tessdata_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.join("tessdata");
    dir.is_dir().then_some(dir)
}

/// Whether the bundled tessdata dir has a `.traineddata` for every language in a
/// `+`-joined Tesseract lang string. This lets us prefer our best models for the
/// languages we ship (Georgian, English) while falling back to the system
/// tessdata for anything else — so all-language indexing still resolves.
fn bundled_tessdata_covers(dir: &Path, langs: &str) -> bool {
    langs
        .split('+')
        .map(str::trim)
        .filter(|lang| !lang.is_empty())
        .all(|lang| dir.join(format!("{lang}.traineddata")).is_file())
}

/// Whether a working Tesseract was found (drives "OCR available" UI + the
/// content-index fallback's decision to even attempt OCR).
pub fn ocr_available() -> bool {
    resolve_tesseract().is_some()
}

/// The language codes the installed Tesseract has (e.g. `["eng","kat",…]`),
/// so the UI can offer only what's actually installed. Empty if not found.
pub fn ocr_languages() -> Vec<String> {
    let Some(exe) = resolve_tesseract() else {
        return Vec::new();
    };
    let Ok(output) = tesseract_command(&exe).arg("--list-langs").output() else {
        return Vec::new();
    };
    // First stdout line is a header ("List of available languages…"); the rest
    // are language codes, one per line.
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .skip(1)
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty() && !line.contains(char::is_whitespace))
        .collect()
}

/// Linear percentile contrast-stretch on a grayscale image: map the 2nd
/// percentile to 0 and the 98th to 255 (tails clamped), so faint scans gain
/// contrast WITHOUT the stroke loss of hard binarization — mid-tone strokes keep
/// their anti-aliasing. Returns whether it changed anything (false on a flat
/// histogram). Stroke-safe, unlike the Otsu threshold we removed.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the targets are fractions of a pixel count (far below 2^52, so exact in f64), and a stretched pixel (lo < v < hi) rounds within 0..=255"
)]
fn normalize_contrast(gray: &mut image::GrayImage) -> bool {
    let mut histogram = [0u32; 256];
    for pixel in gray.pixels() {
        histogram[usize::from(pixel[0])] += 1;
    }
    let total: u64 = u64::from(gray.width()) * u64::from(gray.height());
    if total == 0 {
        return false;
    }
    let lo_target = (total as f64 * 0.02) as u64;
    let hi_target = (total as f64 * 0.98) as u64;
    let mut acc = 0u64;
    let mut lo = 0u8;
    for (i, &count) in (0u8..=255).zip(&histogram) {
        acc += u64::from(count);
        if acc >= lo_target {
            lo = i;
            break;
        }
    }
    acc = 0;
    let mut hi = 255u8;
    for (i, &count) in (0u8..=255).zip(&histogram) {
        acc += u64::from(count);
        if acc >= hi_target {
            hi = i;
            break;
        }
    }
    if hi <= lo {
        return false;
    }
    let range = f32::from(hi - lo);
    for pixel in gray.pixels_mut() {
        let v = pixel[0];
        pixel[0] = if v <= lo {
            0
        } else if v >= hi {
            255
        } else {
            ((f32::from(v - lo) / range) * 255.0).round() as u8
        };
    }
    true
}

/// Preprocess a scan for OCR and return a temp PNG (caller deletes it), or
/// `None` when there is nothing to do. Two stroke-safe steps (NO binarization —
/// a global threshold thinned/broke thin Georgian strokes and hurt recognition):
///   1. Upscale toward ~3200 px on the long side (more pixels per glyph is the
///      single biggest OCR-accuracy lever), capped at 4×, when not already large.
///   2. Gentle percentile contrast-stretch so text/background separate cleanly
///      while strokes keep their anti-aliasing.
///
/// Uses the already-bundled `image` crate.
fn preprocess_for_ocr(src: &Path) -> Result<Option<PathBuf>, String> {
    use image::imageops::FilterType;
    let decoded = image::open(src).map_err(|error| format!("Cannot open image: {error}"))?;
    let mut gray = decoded.to_luma8();
    let (w, h) = gray.dimensions();
    let longest = w.max(h);
    if longest == 0 {
        return Ok(None);
    }
    let mut changed = false;
    if longest < 2600 {
        #[expect(
            clippy::as_conversions,
            clippy::cast_precision_loss,
            reason = "longest is below 2600 here, exact in f32"
        )]
        let scale = (3200.0 / longest as f32).min(4.0);
        #[expect(
            clippy::as_conversions,
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "sides are below 2600 (exact in f32); scale keeps the long side at 3200 or less, and both sides at 1 or more"
        )]
        let nw = ((w as f32) * scale).round().max(1.0) as u32;
        #[expect(
            clippy::as_conversions,
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "sides are below 2600 (exact in f32); scale keeps the long side at 3200 or less, and both sides at 1 or more"
        )]
        let nh = ((h as f32) * scale).round().max(1.0) as u32;
        gray = image::imageops::resize(&gray, nw, nh, FilterType::Lanczos3);
        changed = true;
    }
    if normalize_contrast(&mut gray) {
        changed = true;
    }
    if !changed {
        return Ok(None);
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let temp = std::env::temp_dir().join(format!("kil-ocr-{stamp}.png"));
    gray.save(&temp).map_err(|error| format!("Cannot write preprocessed image: {error}"))?;
    Ok(Some(temp))
}

/// Run OCR on one image file → recognized text. `langs` is a '+'-joined
/// Tesseract language string (e.g. `"eng+kat"`); empty falls back to `"eng"`.
/// With `preprocess`, the image is enhanced first (see `preprocess_for_ocr`) —
/// best-effort: a preprocessing failure falls back to the original image.
/// Internal helper reused by the PDF-OCR and index-fallback paths.
///
/// `operation_id` (Wave 2.5b) enables cancellation: when supplied, the
/// spawned tesseract Child is stashed in `OCR_CHILDREN` so a parallel
/// `cancel_ocr_operation` call can kill it. Callers from non-cancellable
/// code paths (PDF→OCR batches, content-index fallback) pass `None`.
/// Wave 8.4 (2026-05-29): in-process libtesseract OCR via the leptess
/// crate. Skips the per-file subprocess spawn cost (~100-300ms) by
/// calling libtesseract directly. Same engine, same language packs,
/// same accuracy as the subprocess path — just faster for small files.
///
/// Feature-gated: enabled only when `ocr-leptess` is on (see Cargo.toml).
/// Without the feature, this module is empty and `ocr_image_file` uses
/// the subprocess path unconditionally.
///
/// Threading model: leptess instances are NOT thread-safe. Each thread
/// gets its own LepTess via thread_local, lazy-initialized on first use.
/// The instance is cached for the thread's lifetime; switching language
/// strings forces a re-init. In practice the index uses a fixed langs
/// string ("eng+rus+kat" default) so init happens once per worker thread.
#[cfg(feature = "ocr-leptess")]
mod leptess_ocr {
    use leptess::LepTess;
    use std::cell::RefCell;
    use std::path::Path;

    thread_local! {
        /// Per-thread cached LepTess. The tuple stores the langs string
        /// the instance was initialized with, so a langs change re-inits
        /// (rare — only when the user changes OCR language settings).
        static LEPTESS_SLOT: RefCell<Option<(String, LepTess)>> = RefCell::new(None);
    }

    /// Run OCR on a single image file using the cached LepTess instance.
    /// Returns the recognized UTF-8 text on success.
    pub fn ocr_image(image_path: &Path, langs: &str) -> Result<String, String> {
        LEPTESS_SLOT.with(|slot| {
            let mut guard = slot.borrow_mut();
            let needs_init = match guard.as_ref() {
                Some((cached_langs, _)) => cached_langs != langs,
                None => true,
            };
            if needs_init {
                let tessdata = super::bundled_tessdata_dir().map(|p| p.to_string_lossy().into_owned());
                let lt = LepTess::new(tessdata.as_deref(), langs)
                    .map_err(|e| format!("LepTess init failed (langs={langs}): {e}"))?;
                *guard = Some((langs.to_string(), lt));
            }
            let (_, lt) = guard.as_mut().expect("initialized just above");
            lt.set_image(image_path).map_err(|e| format!("LepTess set_image failed: {e}"))?;
            lt.set_source_resolution(300);
            lt.get_utf8_text().map_err(|e| format!("LepTess get_utf8_text failed: {e}"))
        })
    }
}

/// OCRs one image with Tesseract (in-process with `ocr-leptess` when it can).
///
/// # Errors
/// When Tesseract is missing, can't run or reports a failure, or the
/// operation was cancelled (`"Cancelled"`).
pub fn ocr_image_file(
    image_path: &Path,
    langs: &str,
    preprocess: bool,
    psm: Option<u32>,
    operation_id: &Option<String>,
) -> Result<String, String> {
    /// Deletes the preprocessing temp file when dropped.
    struct TempGuard(Option<PathBuf>);
    impl Drop for TempGuard {
        fn drop(&mut self) {
            if let Some(p) = self.0.take() {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    // Wave 8.4 (2026-05-29): in-process libtesseract fast path. Saves
    // ~100-300ms of per-file subprocess overhead. Only available when
    // the `ocr-leptess` feature is on AND `preprocess=false` AND `psm`
    // is None (the leptess path doesn't yet wire the preprocessing or
    // PSM knobs — adding them is straightforward but not done yet, so
    // callers that need either fall back to subprocess for now). The
    // operation_id (cancellation) is satisfied by Wave 8.2's
    // `ocr_with_timeout` watchdog wrapper, which uses the existing
    // subprocess cancellation infra; for in-process leptess we
    // currently can't kill mid-call, so the timeout watchdog from
    // 8.2 doesn't apply here — but leptess calls are fast enough that
    // a runaway is unlikely in practice. If it ever matters, leptess
    // exposes `set_cancel_func` which we can wire up.
    #[cfg(feature = "ocr-leptess")]
    {
        if !preprocess && psm.is_none() && operation_id.is_none() {
            return leptess_ocr::ocr_image(image_path, langs);
        }
    }
    // Registered from here on so a cancel during setup is recorded.
    let _registration = OcrRegistration::new(operation_id.as_deref());
    let exe = resolve_tesseract()
        .ok_or_else(|| "Tesseract OCR was not found. Install Tesseract (5.x) and ensure it is on PATH.".to_string())?;
    let langs = langs.trim();
    let langs = if langs.is_empty() { "eng" } else { langs };

    // Early cancel: user might cancel before we even spawn.
    if is_ocr_cancelled(operation_id.as_deref()) {
        return Err("Cancelled".to_string());
    }

    let prepared = if preprocess { preprocess_for_ocr(image_path).ok().flatten() } else { None };
    let ocr_target: &Path = prepared.as_deref().unwrap_or(image_path);

    // `tesseract <image> stdout -l <langs> --dpi 300 [--psm N]` → recognized
    // text to stdout. The --dpi hint avoids tesseract's low default when the
    // image carries no resolution metadata. An explicit --psm (e.g. 6 = a single
    // uniform block) overrides the auto page-segmentation that can split numbered
    // lists on upscaled pages.
    let mut command = tesseract_command(&exe);
    command.arg(ocr_target).arg("stdout").arg("-l").arg(langs).arg("--dpi").arg("300");
    // Prefer our bundled best models (kat/eng) when they cover every requested
    // language; otherwise leave Tesseract on its system tessdata so other
    // languages (used by all-language indexing) still resolve.
    if let Some(dir) = bundled_tessdata_dir() {
        if bundled_tessdata_covers(&dir, langs) {
            command.arg("--tessdata-dir").arg(&dir);
        }
    }
    if let Some(mode) = psm {
        command.arg("--psm").arg(mode.to_string());
    }
    // Capture stdout + stderr separately so we can return either the
    // recognized text or a clean error message. Spawn (not .output()) so
    // we can stash the Child handle for cancellation.
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let spawn_result = command.spawn();

    // Whatever happens below, clean up the temp file from preprocessing.
    // We do this via a guard struct so an early-return (kill / error)
    // doesn't leak the temp.
    let _temp_guard = TempGuard(prepared);

    let child = match spawn_result {
        Ok(c) => c,
        Err(error) => return Err(format!("Failed to run Tesseract: {error}")),
    };

    // With an operation_id the child stays killable while we wait (see
    // `wait_registered`); without one (internal callers that don't need
    // cancel) we just wait.
    let Some(id) = operation_id else {
        let output = child.wait_with_output().map_err(|error| format!("Tesseract wait failed: {error}"))?;
        return finish_tesseract_output(&output);
    };

    let wait_result = wait_registered(id, child);
    if is_ocr_cancelled(operation_id.as_deref()) {
        return Err("Cancelled".to_string());
    }
    let output = wait_result.map_err(|error| format!("Tesseract wait failed: {error}"))?;
    finish_tesseract_output(&output)
}

/// Convert a finished tesseract subprocess output into the text result
/// (or a clean error). Shared between the `op_id` and no-op_id branches
/// of `ocr_image_file`.
fn finish_tesseract_output(output: &std::process::Output) -> Result<String, String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Tesseract failed: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Tauri command: OCR an image file path → text. `langs` defaults to `"eng"`;
/// `preprocess` defaults to true (upscale low-res before recognition).
///
/// Async + `spawn_blocking`: OCR (and the image upscale) is CPU-heavy and
/// blocking, so it runs off the main thread — the UI stays responsive while
/// Tesseract works (mirrors how content search dispatches).
///
/// # Errors
/// When the worker fails or [`ocr_image_file`] does.
pub async fn ocr_image(
    path: String,
    langs: Option<String>,
    preprocess: Option<bool>,
    psm: Option<u32>,
    operation_id: Option<String>,
) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        ocr_image_file(
            Path::new(&path),
            langs.as_deref().unwrap_or("eng"),
            preprocess.unwrap_or(true),
            psm,
            &operation_id,
        )
    })
    .await
    .map_err(|error| format!("OCR worker failed: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_of_an_unknown_id_leaves_nothing_behind() {
        cancel_ocr_operation("ocr-test-not-running".into()).unwrap();
        assert!(!CANCELLED_OCR.lock().unwrap().contains("ocr-test-not-running"));
        assert!(!OCR_CHILDREN.lock().unwrap().contains_key("ocr-test-not-running"));
    }

    #[test]
    fn registration_records_a_cancel_and_clears_it() {
        let id = Some("ocr-test-registered".to_string());
        {
            let _registration = OcrRegistration::new(id.as_deref());
            cancel_ocr_operation("ocr-test-registered".into()).unwrap();
            assert!(is_ocr_cancelled(id.as_deref()));
        }
        assert!(!is_ocr_cancelled(id.as_deref()));
        assert!(!OCR_CHILDREN.lock().unwrap().contains_key("ocr-test-registered"));
    }

    /// A long-running child is killed by a cancel while its output is being
    /// read (the path a hung tesseract takes).
    #[cfg(windows)]
    #[test]
    fn cancel_kills_a_child_while_its_pipes_are_read() {
        let id = Some("ocr-test-ping".to_string());
        let _registration = OcrRegistration::new(id.as_deref());
        let child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("ping runs");
        let canceller = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(500));
            cancel_ocr_operation("ocr-test-ping".into()).unwrap();
        });
        let started = std::time::Instant::now();
        let output = wait_registered("ocr-test-ping", child).expect("reaped");
        canceller.join().unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "took {:?}", started.elapsed());
        assert!(!output.status.success());
        assert!(is_ocr_cancelled(id.as_deref()));
    }
}
