//! Lazy persistent cache for shell icons used by the overlay quick-search.
//!
//! Cache key strategy (bounded growth):
//!   - kind = "app"    → hash(full path)            unique per app (~100-500)
//!   - kind = "folder" → "_folder"                  one icon for all folders
//!   - kind = "file"   → "_ext_<extension>"         one icon per extension (~50-100)
//!   - kind = None     → hash(full path)            safe fallback (unique)
//!
//! For non-app icons we use the Windows shell flag `SHGFI_USEFILEATTRIBUTES`,
//! which makes `SHGetFileInfoW` treat the path purely as a string with the
//! attributes we pass — no disk inspection. That gives us the *generic* icon
//! for any extension or directory, exactly what Explorer shows by default.
//!
//! Failures are remembered in an in-memory set for the session so we don't
//! repeatedly hammer the shell for the same broken path.
//!
//! Used by the overlay's results.
//!
//! macOS (`launcher_icons/macos.rs`) asks `NSWorkspace`; Linux
//! (`launcher_icons/linux.rs`) looks in the freedesktop icon themes.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use std::sync::Arc;

use dash_core::Host;

// Cache directory version is bumped whenever icon-extraction semantics change,
// so existing on-disk icons get re-extracted on first request rather than
// serving up a stale image. v2: pre-resolve .lnk shortcuts via parselnk so
// the shell-stamped shortcut-arrow overlay no longer appears on app icons.
// v3 (2026-07-27): packaged (MSIX/Store) apps now resolve to their real .exe and
// use its embedded icon, and the exe-less UWP fallback tile is cropped to its
// mark. Builds before this cached the raw padded tile, which renders as a speck
// — without the bump those stale PNGs would survive and the fix would look
// like it did nothing.
const ICON_CACHE_DIR: &str = "launcher_icons_v3";
// Icon size is implicitly 32x32 — we ask Windows for SHGFI_LARGEICON which
// returns the system's "large icon" size, 32px on standard DPI displays.

/// Paths we already tried and failed to extract. Avoids hammering the shell
/// for the same broken path across many overlay queries within a session.
static FAILED_KEYS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn icon_cache_dir(app: &dyn Host) -> Result<PathBuf, String> {
    let dir = app.data_dir().map_err(|e| format!("Cannot resolve app data directory: {e}"))?.join(ICON_CACHE_DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("Cannot create icon cache directory: {e}"))?;
    Ok(dir)
}

/// Stable filename derived from the path. Used only when caching per-path
/// (i.e. apps with unique icons). The default hasher is fine here — collision
/// resistance isn't security-critical, just enough to keep filenames distinct.
fn hash_path(path: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    path.to_lowercase().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Sanitize an extension for use as a filename component. Keeps it ASCII
/// alphanumeric and bounded — defense against weird/long extensions.
fn sanitize_extension(ext: &str) -> String {
    ext.chars().filter(char::is_ascii_alphanumeric).take(8).collect::<String>().to_lowercase()
}

/// Decide cache filename + classify how to extract.
fn cache_classification(path: &str, kind: Option<&str>) -> Classification {
    let lower = path.to_lowercase();
    let ext =
        std::path::Path::new(&lower).extension().and_then(|e| e.to_str()).map(sanitize_extension).unwrap_or_default();

    // Packaged (MSIX / Store) apps are checked FIRST: the frontend sends them
    // with kind="app" like any other launch target, but their "path" is a
    // `shell:AppsFolder\<AUMID>` handle that no filesystem-based extractor can
    // resolve. Unique cache key per app, same as a real exe.
    if crate::packaged_apps::is_packaged_launch_path(path) {
        return Classification { cache_key: hash_path(&lower), mode: ExtractMode::Packaged };
    }

    match kind {
        Some("folder") => Classification { cache_key: "_folder".to_string(), mode: ExtractMode::Folder },
        Some("file") if !ext.is_empty() => {
            Classification { cache_key: format!("_ext_{ext}"), mode: ExtractMode::Extension(ext) }
        }
        Some("file") => {
            Classification { cache_key: "_file_noext".to_string(), mode: ExtractMode::Extension("dat".to_string()) }
        }
        // "app" or unknown — treat as unique per path.
        _ => Classification { cache_key: hash_path(&lower), mode: ExtractMode::RealPath },
    }
}

struct Classification {
    cache_key: String,
    mode: ExtractMode,
}

enum ExtractMode {
    /// Extract from the real path on disk (apps).
    RealPath,
    /// Generic folder icon — `SHGFI_USEFILEATTRIBUTES` + `FILE_ATTRIBUTE_DIRECTORY`.
    Folder,
    /// Generic icon for an extension — `SHGFI_USEFILEATTRIBUTES` + `FILE_ATTRIBUTE_NORMAL`
    /// with a dummy path like "x.docx".
    Extension(String),
    /// Packaged (MSIX / Store) app — no file to stat, so the icon comes from
    /// the shell item itself via `IShellItemImageFactory`.
    Packaged,
}

/// Returns the absolute path to a cached PNG icon, extracting and saving it on
/// first request. Returns Ok(None) when the source yields no extractable icon.
///
/// `kind` should be one of "app" | "folder" | "file" | None. When None we treat
/// the path as a unique app (safe fallback). Frontend passes the actual kind
/// based on result type so we cache efficiently.
///
/// # Errors
/// When the icon cache folder can't be found or the icon can't be written.
// `(async)` so each extraction runs on a Tauri worker thread, NOT the main/UI
// thread. The palette fans out dozens of icon requests on first (cold-cache)
// open; as a sync command these serialized on the main thread and froze the UI
// while SHGetFileInfoW did its work. Off-thread, the bounded blocking pool
// resolves them in the background and icons fade in. (COM is initialized per
// worker thread inside extract_icon_png, since SHGetFileInfoW requires it.)
#[expect(clippy::needless_pass_by_value, reason = "public API shared with other apps")]
pub fn ensure_launcher_icon(app: Arc<dyn Host>, path: String, kind: Option<String>) -> Result<Option<String>, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let class = cache_classification(trimmed, kind.as_deref());
    let cache_dir = icon_cache_dir(app.as_ref())?;
    let icon_path = cache_dir.join(format!("{}.png", class.cache_key));

    // Fast path — already extracted to disk (this is the hot case once warm).
    if icon_path.exists() {
        return Ok(Some(icon_path.to_string_lossy().to_string()));
    }

    // Fast path — we already tried and failed for this key in this session.
    {
        if let Ok(failed) = FAILED_KEYS.lock() {
            if failed.contains(&class.cache_key) {
                return Ok(None);
            }
        }
    }

    let bytes = match extract_icon_png(trimmed, &class.mode) {
        Some(b) if !b.is_empty() => b,
        _ => {
            if let Ok(mut failed) = FAILED_KEYS.lock() {
                failed.insert(class.cache_key);
            }
            return Ok(None);
        }
    };

    if let Err(e) = write_atomic(&icon_path, &bytes) {
        // Don't poison the failure cache for write errors — they may be transient.
        return Err(format!("Cannot write launcher icon: {e}"));
    }

    Ok(Some(icon_path.to_string_lossy().to_string()))
}

/// Writes beside `path`, then renames into place, so a crash or a parallel
/// request for the same icon never leaves a half-written PNG that the
/// `exists()` fast path would then serve forever.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!("{}-{}.tmp", std::process::id(), NEXT_TMP.fetch_add(1, Ordering::Relaxed)));
    let result = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos::extract_icon_png;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux::extract_icon_png;

/// Icons are made at this size on macOS / Linux (sharp at 2x in the
/// overlay's rows).
#[cfg(not(windows))]
const ICON_PX: u32 = 64;

/// `image` as a PNG, scaled to `ICON_PX` if it's another size.
#[cfg(not(windows))]
fn png_of(image: &image::RgbaImage) -> Option<Vec<u8>> {
    let scaled;
    let image = if image.dimensions() == (ICON_PX, ICON_PX) {
        image
    } else {
        scaled = image::imageops::resize(image, ICON_PX, ICON_PX, image::imageops::FilterType::Lanczos3);
        &scaled
    };
    let mut png = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    Some(png)
}

/// Resolve a Windows .lnk shortcut to its target path so we extract the
/// icon from the real executable rather than the shortcut file.
///
/// Why this matters: when `SHGetFileInfoW` is given a `.lnk` path, Windows
/// returns the target's icon *with the shell shortcut-arrow overlay baked
/// in*. That made every Start-Menu-launched app in our overlay results
/// render with a small arrow badge — visual noise that didn't match the
/// non-shortcut icons (e.g. items found by indexing the real exe).
///
/// We feed the resolved `.exe` (or whatever the link points at) into the
/// same extraction pipeline and get a clean icon back.
///
/// Returns `None` when:
///   - the path isn't a `.lnk` (caller falls back to the original path)
///   - parselnk can't decode the file (corrupt / unusual shortcut)
///   - no usable target is recorded (URL / namespace shortcuts)
///   - the resolved target doesn't actually exist on disk
///
/// Pure-Rust + filesystem-only — no shell COM, no network, no spawned
/// processes. parselnk reads the binary header + `LinkInfo` / `StringData`
/// blocks and we pick the first viable path candidate.
#[cfg(windows)]
pub fn resolve_lnk_target(path: &str) -> Option<String> {
    resolve_lnk_full(path).map(|(target, _args)| target)
}

/// Resolve a `.lnk` to `(target_path, command_line_arguments)`.
///
/// Same parse as [`resolve_lnk_target`] (which delegates here) — split out so
/// callers that need the *arguments* don't re-read and re-decode the file. The
/// arguments matter for telling "this shortcut IS the app" apart from "this
/// shortcut opens a document/URL/folder THROUGH an app": a Start-Menu entry
/// pointing at `explorer.exe "C:\...\Windows Kits\10\"` resolves to Explorer
/// but is not Explorer. See `processes::target_is_app_itself`.
#[cfg(windows)]
pub fn resolve_lnk_full(path: &str) -> Option<(String, Option<String>)> {
    if !path.to_ascii_lowercase().ends_with(".lnk") {
        return None;
    }
    let lnk_path = std::path::Path::new(path);
    let lnk = parselnk::Lnk::try_from(lnk_path).ok()?;
    let args = lnk.string_data.command_line_arguments.clone();

    // Preferred: LinkInfo.local_base_path — the fully-qualified path on a
    // local volume. This is what Start-Menu shortcuts to installed apps
    // (e.g. "...\Steam\steamapps\common\DOOM\doom.exe") typically carry.
    // parselnk 0.1: `link_info` is `LinkInfo` (not Option) but its inner
    // `local_base_path` field IS optional — a shortcut to a URL or shell
    // namespace will have no local base.
    if let Some(base) = &lnk.link_info.local_base_path {
        let trimmed = base.trim();
        if !trimmed.is_empty() && std::path::Path::new(trimmed).exists() {
            return Some((trimmed.to_string(), args));
        }
    }

    // Fallback: StringData.relative_path — a path expressed relative to
    // the .lnk's own directory. Used by portable-app shortcuts that ship
    // alongside the executable. Join against the shortcut's parent dir
    // and canonicalize lightly via `exists()` so we don't return ghosts.
    if let Some(rel) = &lnk.string_data.relative_path {
        let parent = lnk_path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let joined = parent.join(rel);
        if joined.exists() {
            return joined.to_str().map(|s| (s.to_string(), args));
        }
    }

    None
}

/// Windows implementation. Three modes branch on what we ask the shell for:
///   - `RealPath`          → no USEFILEATTRIBUTES; the real app's embedded icon
///   - Folder            → USEFILEATTRIBUTES + `FILE_ATTRIBUTE_DIRECTORY`
///   - Extension(ext)    → USEFILEATTRIBUTES + `FILE_ATTRIBUTE_NORMAL` + dummy path
///
/// Pipeline:
///   path -> `SHGetFileInfoW` -> HICON
///        -> `GetIconInfo` -> ICONINFO (hbmColor / hbmMask)
///        -> `GetObjectW` on hbmColor -> BITMAP (width, height)
///        -> `GetDIBits` -> BGRA pixel buffer
///        -> swap BGRA->RGBA, fix all-zero alpha as opaque
///        -> encode as PNG via the `image` crate
///        -> `DestroyIcon` for cleanup
///
/// `.lnk` resolution: in `RealPath` mode, shortcuts are pre-resolved to their
/// target path (see `resolve_lnk_target`) so the returned icon doesn't carry
/// the shell shortcut-arrow overlay.
#[cfg(windows)]
fn extract_icon_png(path: &str, mode: &ExtractMode) -> Option<Vec<u8>> {
    use image::{ImageBuffer, Rgba};
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
    };
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

    // SHGetFileInfoW requires COM on the calling thread. Now that
    // `ensure_launcher_icon` is `(async)` (runs on a worker thread, off the UI
    // thread), initialize COM once per worker thread before any shell call.
    // STA matches the historical main-thread apartment; a prior MTA init by
    // another command returns RPC_E_CHANGED_MODE, which is fine for SHGetFileInfoW.
    thread_local! {
        // SAFETY: CoInitializeEx is always safe to call. We never CoUninitialize —
        // the blocking pool reuses worker threads for the process lifetime, so one
        // init per thread is correct and effectively leak-free.
        static COM_INIT: () = unsafe { let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); };
    }
    COM_INIT.with(|()| {});

    // Packaged apps: prefer the real exe's embedded icon, which fills its canvas
    // like every classic app's does. Only fall back to the shell's tile image if
    // the exe can't be resolved — the tile is a Start-menu asset with safe-area
    // padding, so it renders as a small mark in a 32px row.
    if matches!(mode, ExtractMode::Packaged) {
        if let Some(exe) = crate::packaged_apps::resolve_packaged_exe(path.trim_start_matches("shell:AppsFolder\\")) {
            if let Some(png) = extract_icon_png(&exe.to_string_lossy(), &ExtractMode::RealPath) {
                return Some(png);
            }
        }
        return extract_packaged_icon_png(path);
    }

    // For .lnk shortcuts, swap to the resolved target before talking to the
    // shell. Folder / Extension modes use synthetic paths anyway, so the
    // resolution only runs in RealPath mode.
    let resolved_owned = match mode {
        ExtractMode::RealPath => resolve_lnk_target(path),
        _ => None,
    };
    let effective_path: &str = resolved_owned.as_deref().unwrap_or(path);

    // Build the input path string and the attribute/flag combination based on mode.
    let (query_string, attrs, flags) = match mode {
        ExtractMode::RealPath => {
            (effective_path.to_string(), FILE_FLAGS_AND_ATTRIBUTES(0), SHGFI_ICON | SHGFI_LARGEICON)
        }
        ExtractMode::Folder => (
            // Dummy non-empty path — flag below tells Windows to ignore disk state.
            "folder".to_string(),
            FILE_ATTRIBUTE_DIRECTORY,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
        ),
        ExtractMode::Extension(ext) => (
            // Synthetic "x.<ext>" — shell uses just the extension association.
            format!("x.{ext}"),
            FILE_ATTRIBUTE_NORMAL,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
        ),
        // Handled by the early return above — SHGetFileInfoW can't resolve an
        // AUMID, so a packaged app must never reach this match.
        ExtractMode::Packaged => return None,
    };

    let wide: Vec<u16> = query_string.encode_utf16().chain(std::iter::once(0)).collect();

    // We DestroyIcon the resulting hIcon before returning regardless of
    // conversion success.
    let mut info = SHFILEINFOW::default();
    // SAFETY: `wide` is NUL-terminated and outlives the call; SHGetFileInfoW
    // writes into `info`, whose size we pass.
    let result = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            attrs,
            Some(&raw mut info),
            u32::try_from(std::mem::size_of::<SHFILEINFOW>()).expect("SHFILEINFOW is under 1 KB"),
            flags,
        )
    };
    if result == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let icon: HICON = info.hIcon;

    let pixels = extract_icon_pixels(icon);

    // SAFETY: icon was obtained from SHGetFileInfoW which transfers ownership.
    unsafe {
        let _ = DestroyIcon(icon);
    }

    let (rgba, width, height) = pixels?;
    let buf: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_raw(width, height, rgba)?;
    let mut png = Vec::with_capacity(4096);
    let mut cursor = std::io::Cursor::new(&mut png);
    buf.write_to(&mut cursor, image::ImageFormat::Png).ok()?;
    Some(png)
}

#[cfg(windows)]
fn extract_icon_pixels(icon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<(Vec<u8>, u32, u32)> {
    // The GDI pixel read moved to `hbitmap_to_rgba` (shared with the packaged
    // path), so this function only unpacks the icon's bitmaps and frees them.
    use windows::Win32::Graphics::Gdi::DeleteObject;
    use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};

    let mut icon_info = ICONINFO::default();
    // SAFETY: `icon` is a live HICON the caller owns; GetIconInfo fills
    // `icon_info` and hands us its two bitmaps, deleted below.
    if unsafe { GetIconInfo(icon, &raw mut icon_info) }.is_err() {
        return None;
    }
    let hbm_color = icon_info.hbmColor;
    let hbm_mask = icon_info.hbmMask;

    let result = hbitmap_to_rgba(hbm_color, false);

    // SAFETY: GetIconInfo gave us this bitmap; it's deleted once, here.
    let _ = unsafe { DeleteObject(hbm_color) };
    // SAFETY: as above, for the mask bitmap.
    let _ = unsafe { DeleteObject(hbm_mask) };

    result
}

/// Read a GDI bitmap's pixels as top-down RGBA.
///
/// Shared by the HICON path (classic apps, via `GetIconInfo`) and the packaged
/// path (Store apps, via `IShellItemImageFactory::GetImage`) — the GDI half of
/// both is identical, only the handle's origin differs. **Does not free `hbm`**;
/// the caller owns the handle and deletes it.
///
/// `unpremultiply` un-does alpha premultiplication. The shell hands back
/// premultiplied BGRA for packaged-app images; written straight into a PNG
/// (which expects straight alpha) that darkens antialiased edges into a grey
/// fringe. Classic HICONs pass `false` — that path predates this and its
/// behavior is deliberately unchanged.
#[cfg(windows)]
fn hbitmap_to_rgba(hbm: windows::Win32::Graphics::Gdi::HBITMAP, unpremultiply: bool) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };

    // The GDI reads below are paired (DC acquired then released); we never
    // take ownership of `hbm`.
    let mut bitmap = BITMAP::default();
    let bitmap_size = i32::try_from(std::mem::size_of::<BITMAP>()).expect("BITMAP is a few dozen bytes");
    // SAFETY: `hbm` is a live bitmap the caller owns; GetObjectW writes at
    // most `bitmap_size` bytes into `bitmap`.
    let got = unsafe { GetObjectW(hbm, bitmap_size, Some((&raw mut bitmap).cast())) };
    if got == 0 {
        return None;
    }

    // A negative size would turn into a huge buffer below; skip the icon.
    let width = u32::try_from(bitmap.bmWidth).ok()?;
    let height = u32::try_from(bitmap.bmHeight).ok()?;
    if width == 0 || height == 0 {
        return None;
    }

    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize =
        u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).expect("BITMAPINFOHEADER is 40 bytes");
    // Both came from non-negative i32s above, so neither wraps.
    bmi.bmiHeader.biWidth = width.cast_signed();
    // Negative biHeight to get top-down rows — matches PNG's row order.
    bmi.bmiHeader.biHeight = -height.cast_signed();
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;

    let row_bytes = usize::try_from(width).ok()? * 4;
    let mut pixels = vec![0u8; row_bytes * usize::try_from(height).ok()?];

    // SAFETY: the screen DC; released just below.
    let hdc = unsafe { GetDC(HWND(0)) };
    // SAFETY: `pixels` holds `height` rows of `width` 32-bit pixels, the
    // format `bmi` asks for; `hdc` and `hbm` are live.
    let lines =
        unsafe { GetDIBits(hdc, hbm, 0, height, Some(pixels.as_mut_ptr().cast()), &raw mut bmi, DIB_RGB_COLORS) };
    // SAFETY: `hdc` came from GetDC above and is released once.
    unsafe { ReleaseDC(HWND(0), hdc) };

    if lines == 0 {
        return None;
    }

    // GDI returns BGRA; PNG wants RGBA. Swap in place.
    // Some legacy 24bpp icons surface with zero alpha; if every alpha byte
    // is 0 we treat as fully opaque rather than rendering an invisible PNG.
    let mut any_alpha_set = false;
    for chunk in pixels.as_chunks_mut::<4>().0 {
        chunk.swap(0, 2);
        if chunk[3] != 0 {
            any_alpha_set = true;
        }
    }
    if !any_alpha_set {
        for chunk in pixels.as_chunks_mut::<4>().0 {
            chunk[3] = 0xFF;
        }
    } else if unpremultiply {
        for chunk in pixels.as_chunks_mut::<4>().0 {
            let a = u32::from(chunk[3]);
            if a > 0 && a < 255 {
                for channel in &mut chunk[..3] {
                    *channel = u8::try_from(((u32::from(*channel) * 255 + a / 2) / a).min(255)).unwrap_or(u8::MAX);
                }
            }
        }
    }

    Some((pixels, width, height))
}

/// Extract a packaged (MSIX / Store) app's real tile icon.
///
/// Classic apps go through `SHGetFileInfoW`, which needs a filesystem path —
/// packaged apps have none, so it fails and they fall back to a generic glyph.
/// The shell exposes their artwork through `IShellItemImageFactory` on the same
/// `shell:AppsFolder\<AUMID>` item the launcher already stores, so we ask for
/// that instead.
///
/// Requested at 64px (larger than the 32px `SHGFI_LARGEICON` classic path) with
/// `BIGGERSIZEOK`: rows render at 20 CSS px, so oversampling keeps Store icons
/// crisp on `HiDPI` rather than upscaling a small tile.
#[cfg(windows)]
fn extract_packaged_icon_png(path: &str) -> Option<Vec<u8>> {
    use image::{ImageBuffer, Rgba};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::DeleteObject;
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
    };

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    // Read-only shell COM. The HBITMAP from GetImage is owned by us and
    // deleted below in every path.
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let factory: IShellItemImageFactory = unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }.ok()?;
    // 256px, not 64: this path only runs for tiles that will then be CROPPED
    // to their safe area (see `trim_tile_safe_area`). A 64px tile whose mark
    // is 25% wide crops to ~17px, which upscales into mush at a 20px row.
    // Asking for 256 leaves ~68px after the crop — sharp at any row size.
    // SAFETY: `factory` is a live COM object; the bitmap it returns is ours.
    let hbm = unsafe { factory.GetImage(SIZE { cx: 256, cy: 256 }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK) }.ok()?;

    let pixels = hbitmap_to_rgba(hbm, true);
    // SAFETY: GetImage gave us this bitmap; it's deleted once, here.
    let _ = unsafe { DeleteObject(hbm) };

    let (rgba, width, height) = pixels?;
    let buf: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_raw(width, height, rgba)?;
    let buf = trim_tile_safe_area(buf);

    let mut png = Vec::with_capacity(8192);
    let mut cursor = std::io::Cursor::new(&mut png);
    buf.write_to(&mut cursor, image::ImageFormat::Png).ok()?;
    Some(png)
}

/// Crop a Store tile's safe-area padding so the mark fills the icon.
///
/// Only reached for packaged apps with **no classic exe** (true UWP components
/// like Settings and Windows Backup) — everything else uses the exe's icon,
/// which already fills its canvas. Measured on a real machine: those tiles are
/// 64x64 with the mark occupying ~25% of the width, so at a 32px row the mark
/// renders about 8px and reads as a speck.
///
/// Conservative by construction:
///   * Does nothing unless the mark is genuinely small (<60% of width), so a
///     full-bleed tile is never touched.
///   * Keeps a small margin so the crop doesn't shave antialiased edges.
///   * Crops to a SQUARE centred on the mark, preserving aspect and the tile's
///     visual centre rather than stretching it.
#[cfg(windows)]
fn trim_tile_safe_area(
    img: image::ImageBuffer<image::Rgba<u8>, Vec<u8>>,
) -> image::ImageBuffer<image::Rgba<u8>, Vec<u8>> {
    const FILL_THRESHOLD: u32 = 60; // percent of width
    const MARGIN_PCT: u32 = 10; // of the cropped side

    let (w, h) = (img.width(), img.height());
    let Some((x0, y0, x1, y1)) = glyph_bounds(&img) else {
        return img;
    };
    let gw = x1 - x0 + 1;
    let gh = y1 - y0 + 1;
    if w == 0 || (gw * 100) / w >= FILL_THRESHOLD {
        return img; // already fills the canvas — leave it alone
    }

    // Square side around the mark, plus margin, clamped to the image.
    let side = gw.max(gh);
    let side = (side + (side * MARGIN_PCT) / 100).min(w.min(h));
    let cx = u32::midpoint(x0, x1);
    let cy = u32::midpoint(y0, y1);
    let x = cx.saturating_sub(side / 2).min(w.saturating_sub(side));
    let y = cy.saturating_sub(side / 2).min(h.saturating_sub(side));

    image::imageops::crop_imm(&img, x, y, side, side).to_image()
}

/// Bounding box of the visible artwork as `(x0, y0, x1, y1)` inclusive, or
/// `None` when every pixel is transparent.
///
/// Alpha threshold rather than `!= 0`: Store logos often carry a faint
/// drop-shadow halo whose alpha is 1-2, which would defeat a strict test and
/// report the full canvas as "visible".
/// Bounding box of pixels that DIFFER from the image's border colour, as
/// `(x0, y0, x1, y1)` inclusive. `None` when the whole image is that colour.
///
/// This is the "where is the actual glyph" measurement. [`opaque_bounds`] can't
/// answer it: a Store tile whose background is an OPAQUE plate reports 100%
/// opaque while the mark itself occupies a fraction of the canvas. Sampling the
/// border colour and finding what differs from it locates the artwork inside its
/// safe area.
#[cfg(windows)]
fn glyph_bounds(img: &image::RgbaImage) -> Option<(u32, u32, u32, u32)> {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    // Border colour = the four corners, if they agree. Transparent counts as a
    // colour here (alpha 0), which covers the classic padded-transparent case.
    let corners = [img.get_pixel(0, 0), img.get_pixel(w - 1, 0), img.get_pixel(0, h - 1), img.get_pixel(w - 1, h - 1)];
    let bg = *corners[0];
    let uniform = corners.iter().all(|p| similar(p.0, bg.0));
    if !uniform {
        // No consistent border — treat the whole canvas as artwork.
        return Some((0, 0, w - 1, h - 1));
    }

    let (mut x0, mut y0) = (u32::MAX, u32::MAX);
    let (mut x1, mut y1) = (0u32, 0u32);
    let mut found = false;
    for (x, y, px) in img.enumerate_pixels() {
        if !similar(px.0, bg.0) {
            found = true;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    found.then_some((x0, y0, x1, y1))
}

/// Per-channel tolerance so JPEG-ish ringing or a subtle gradient in the plate
/// doesn't read as "content".
#[cfg(windows)]
fn similar(a: [u8; 4], b: [u8; 4]) -> bool {
    // Both effectively transparent → same, whatever the RGB garbage underneath.
    if a[3] < 8 && b[3] < 8 {
        return true;
    }
    a.iter().zip(b.iter()).all(|(p, q)| (i16::from(*p) - i16::from(*q)).abs() <= 12)
}

#[cfg(all(windows, test))]
fn opaque_bounds(img: &image::RgbaImage) -> Option<(u32, u32, u32, u32)> {
    const ALPHA_FLOOR: u8 = 8;
    let (mut x0, mut y0) = (u32::MAX, u32::MAX);
    let (mut x1, mut y1) = (0u32, 0u32);
    let mut found = false;
    for (x, y, px) in img.enumerate_pixels() {
        if px.0[3] > ALPHA_FLOOR {
            found = true;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    found.then_some((x0, y0, x1, y1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_write_is_atomic_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("dash-icon-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let icon = dir.join("key.png");
        write_atomic(&icon, b"first").unwrap();
        write_atomic(&icon, b"second").unwrap();
        assert_eq!(fs::read(&icon).unwrap(), b"second");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temp files left");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn packaged_paths_classify_to_the_packaged_extractor() {
        let c = cache_classification("shell:AppsFolder\\OpenAI.Codex_2p2nqsd0c76g0!App", Some("app"));
        assert!(matches!(c.mode, ExtractMode::Packaged));
        // Unique per app, so two Store apps can't share a cached icon.
        let other = cache_classification("shell:AppsFolder\\Claude_pzs8sxrjxfjjc!Claude", Some("app"));
        assert_ne!(c.cache_key, other.cache_key);
    }

    #[test]
    fn classic_paths_are_unaffected_by_the_packaged_branch() {
        assert!(matches!(cache_classification(r"C:\Windows\notepad.exe", Some("app")).mode, ExtractMode::RealPath));
        assert!(matches!(cache_classification("anything", Some("folder")).mode, ExtractMode::Folder));
        assert!(matches!(cache_classification("report.docx", Some("file")).mode, ExtractMode::Extension(_)));
    }

    /// Extracts real artwork for every packaged app installed on this machine.
    /// Ignored by default (depends what's installed); run with:
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture extracts_real_packaged_icons
    /// ```
    ///
    /// Validates the PNG magic bytes and decodes each image, so a silently
    /// mangled bitmap (wrong stride, zero size, all-transparent) fails here
    /// rather than showing up as a blank square in the palette.
    #[cfg(windows)]
    #[test]
    #[ignore = "extracts icons for the real installed Store apps; run with --ignored"]
    fn extracts_real_packaged_icons() {
        let apps = crate::packaged_apps::enumerate_packaged_apps();
        assert!(!apps.is_empty(), "no packaged apps to test against");

        let mut ok = 0usize;
        for app in &apps {
            let path = crate::packaged_apps::packaged_launch_path(&app.aumid);
            // Go through the real dispatcher, NOT `extract_packaged_icon_png`:
            // production prefers the app's exe icon and only falls back to the
            // shell tile. Calling the tile extractor directly measured a path
            // users never hit, and hid that the exe route was already working.
            match extract_icon_png(&path, &ExtractMode::Packaged) {
                Some(png) => {
                    assert_eq!(&png[..4], b"\x89PNG", "not a PNG for {}", app.name);
                    let img = image::load_from_memory(&png)
                        .unwrap_or_else(|e| panic!("undecodable PNG for {}: {e}", app.name));
                    assert!(img.width() >= 16 && img.height() >= 16, "tiny: {}", app.name);
                    let rgba = img.to_rgba8();
                    let opaque = rgba.pixels().any(|p| p.0[3] != 0);
                    assert!(opaque, "fully transparent icon for {}", app.name);
                    // How much of the canvas the artwork actually fills — Store
                    // logos ship padded, so this is the number that decides
                    // whether the icon looks tiny in a 20px row.
                    // `glyph_bounds` (not `opaque_bounds`) is the number that
                    // matters: a tile with an opaque plate is 100% "opaque" while
                    // its actual mark fills a fraction of the canvas.
                    let (x0, y0, x1, y1) = glyph_bounds(&rgba).unwrap_or((0, 0, img.width() - 1, img.height() - 1));
                    let _ = opaque_bounds(&rgba);
                    let fill = ((x1 - x0 + 1) * 100) / img.width();
                    let via =
                        if crate::packaged_apps::resolve_packaged_exe(&app.aumid).is_some() { "exe" } else { "tile" };
                    println!(
                        "  OK  {:<24} {}x{}  glyph {}x{} = {}% of width   [via {}]",
                        app.name,
                        img.width(),
                        img.height(),
                        x1 - x0 + 1,
                        y1 - y0 + 1,
                        fill,
                        via
                    );
                    ok += 1;
                }
                None => println!("  --  {:<28} (no image)", app.name),
            }
        }
        println!("extracted {ok}/{} packaged icons", apps.len());
        assert!(ok > 0, "every packaged icon extraction failed");
    }

    /// Regression guard for the `hbitmap_to_rgba` extraction: the classic
    /// HICON path was refactored to share that helper with the packaged path,
    /// so this proves ordinary exe icons still decode. Run with:
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture extracts_classic_exe_icon
    /// ```
    #[cfg(windows)]
    #[test]
    #[ignore = "extracts a real system exe icon; run with --ignored"]
    fn extracts_classic_exe_icon() {
        // explorer.exe, not notepad.exe: Windows 11 ships Notepad as a Store app
        // and `C:\Windows\System32\notepad.exe` no longer exists there.
        let png = extract_icon_png(r"C:\Windows\explorer.exe", &ExtractMode::RealPath)
            .expect("no icon extracted for explorer.exe");
        assert_eq!(&png[..4], b"\x89PNG");
        let img = image::load_from_memory(&png).expect("undecodable classic icon PNG");
        assert!(img.width() >= 16 && img.height() >= 16);
        assert!(img.to_rgba8().pixels().any(|p| p.0[3] != 0), "classic icon came out fully transparent");
        println!("classic icon OK: {}x{}", img.width(), img.height());
    }
}
