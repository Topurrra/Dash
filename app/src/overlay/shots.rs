//! `dash --shots <folder>`: draws each view of the overlay into a PNG, for
//! checking the look without anyone at the keyboard. The window opens
//! off-screen and never takes focus; each view is set up in code (no key
//! presses) and copied from the window itself, so nothing else on the
//! screen is touched or captured. Runs beside a normal Dash (as a follower).

use super::*;

type Scene = fn(&mut Overlay, &mut Window, &mut Context<Overlay>);

fn type_query(view: &mut Overlay, text: &str, cx: &mut Context<Overlay>) {
    view.input.update(cx, |input, cx| input.set_text(text, cx));
    view.query_changed(cx);
}

/// The Files tab, for `text`.
fn files(view: &mut Overlay, text: &str, cx: &mut Context<Overlay>) {
    view.tab = Tab::Files;
    type_query(view, text, cx);
}

const SCENES: &[(&str, Scene)] = &[
    ("1-welcome", |v, w, cx| {
        // Fixed starting views, whatever is saved (nothing here is saved).
        (v.search_view, v.clip_view, v.show_pane) = (SearchView::Table, ClipView::List, true);
        v.welcome = true;
        v.set_mode(Mode::Search, w, cx);
        type_query(v, "", cx);
        v.watch_welcome_progress(cx);
    }),
    ("1a-update-offered", |_, _, cx| crate::update::show_sample(Some("0.2.0"), cx)),
    ("1b-search-empty", |v, _, cx| {
        crate::update::show_sample(None, cx);
        // Not `finish_welcome`: that would save "seen" in the real preferences.
        v.welcome = false;
        cx.notify();
    }),
    ("2-files", |v, _, cx| type_query(v, "invoice", cx)),
    ("3-files-text", |v, _, cx| v.table_move(1, false, cx)),
    ("4-inside", |v, _, cx| {
        v.tab = Tab::Inside;
        v.selected = 0;
        v.update_preview(cx);
        cx.notify();
    }),
    ("5-music", |v, _, cx| files(v, "ext:mp3", cx)),
    ("6-video", |v, _, cx| files(v, "ext:mp4", cx)),
    ("7-image", |v, _, cx| files(v, "ext:png", cx)),
    ("8-clipboard", |v, w, cx| {
        v.set_mode(Mode::Clipboard, w, cx);
        type_query(v, "", cx);
    }),
    ("9-settings", |v, w, cx| {
        v.set_mode(Mode::Settings, w, cx);
        v.settings.update(cx, |s, cx| s.show_section(0, false, cx));
    }),
    ("9b-settings-bottom", |v, _, cx| v.settings.update(cx, |s, cx| s.show_section(0, true, cx))),
    ("10-settings-clipboard", |v, _, cx| v.settings.update(cx, |s, cx| s.show_section(1, false, cx))),
    ("11-settings-general", |v, _, cx| v.settings.update(cx, |s, cx| s.show_section(2, false, cx))),
    // The preview pane off (not saved: the real preference stays).
    ("12-pane-off", |v, w, cx| {
        v.set_mode(Mode::Search, w, cx);
        v.show_pane = false;
        files(v, "invoice", cx);
    }),
    ("13-pane-back", |v, _, cx| {
        v.show_pane = true;
        v.update_preview(cx);
        cx.notify();
    }),
    ("14-files-list", |v, _, cx| {
        v.search_view = SearchView::List;
        files(v, "invoice", cx);
    }),
    ("15-files-compact", |v, _, cx| {
        v.search_view = SearchView::Compact;
        files(v, "ext:mp4", cx);
    }),
    ("16-files-grid", |v, _, cx| {
        v.search_view = SearchView::Grid;
        files(v, "ext:jpg", cx);
    }),
    ("17-files-grid-wide", |v, _, cx| {
        v.show_pane = false;
        v.table_move(9, false, cx);
    }),
    ("18-clip-grid", |v, w, cx| {
        (v.search_view, v.show_pane, v.clip_view) = (SearchView::Table, true, ClipView::Grid);
        v.set_mode(Mode::Clipboard, w, cx);
        type_query(v, "", cx);
    }),
    ("19-clip-grid-wide", |v, _, cx| {
        v.show_pane = false;
        cx.notify();
    }),
    ("20-clip-compact", |v, _, cx| {
        (v.show_pane, v.clip_view) = (true, ClipView::Compact);
        v.update_preview(cx);
        cx.notify();
    }),
    ("21-before-drag", |v, w, cx| {
        v.clip_view = ClipView::List;
        v.set_mode(Mode::Search, w, cx);
        files(v, "ext:jpg", cx);
    }),
    // Drags the list's scrollbar thumb down with mouse messages sent to this
    // window only (the real pointer doesn't move).
    ("22-scrollbar-dragged", |_, w, _| drag(w, (655, 142), (655, 420))),
    ("23-clipboard-list", |v, w, cx| {
        v.clip_view = ClipView::List;
        v.set_mode(Mode::Clipboard, w, cx);
        type_query(v, "", cx);
    }),
    ("24-clip-menu", |_, w, _| right_click(w, (200, 160))),
    // A text in the pane (this repo's README as a clipboard entry, so it
    // works with no index), then a drag across it.
    ("25-text-shown", |v, _, cx| {
        v.clip_menu = None;
        let text = include_str!("../../../README.md");
        v.clips = vec![Clip {
            id: u64::MAX,
            text: "README.md".into(),
            image: None,
            meta: "Dash · today".into(),
            pinned: false,
            full_text: text.into(),
            full_image: None,
            app_path: None,
        }];
        v.selected = 0;
        v.clip_preview = None;
        v.update_clip_preview();
        cx.notify();
    }),
    ("26-text-selected", |_, w, _| drag(w, (520, 113), (760, 205))),
];

/// A right-click at `at` (client pixels), sent to this window only.
#[cfg(windows)]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "like MAKELPARAM: each client coordinate goes into 16 bits of the LPARAM \
              (an isize, which holds any u16 on Windows)"
)]
fn right_click(window: &Window, at: (i32, i32)) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_RBUTTONUP};
    let Some(hwnd) = window_handle(window) else { return };
    let pos = LPARAM(((at.1 as u16 as isize) << 16) | (at.0 as u16 as isize));
    // SAFETY: posting to this live window; the message carries plain values.
    let _ = unsafe { PostMessageW(hwnd, WM_MOUSEMOVE, WPARAM(0), pos) };
    // SAFETY: posting to this live window; the message carries plain values.
    let _ = unsafe { PostMessageW(hwnd, WM_RBUTTONDOWN, WPARAM(2), pos) };
    // SAFETY: posting to this live window; the message carries plain values.
    let _ = unsafe { PostMessageW(hwnd, WM_RBUTTONUP, WPARAM(0), pos) };
}

#[cfg(not(windows))]
fn right_click(_: &Window, _: (i32, i32)) {}

/// Presses at `from`, moves in steps to `to`, releases (client pixels).
#[cfg(windows)]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "like MAKELPARAM: each client coordinate goes into 16 bits of the LPARAM \
              (an isize, which holds any u16 on Windows)"
)]
fn drag(window: &Window, from: (i32, i32), to: (i32, i32)) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE};
    const LEFT_DOWN: WPARAM = WPARAM(1);
    let Some(hwnd) = window_handle(window) else { return };
    let at = |x: i32, y: i32| LPARAM(((y as u16 as isize) << 16) | (x as u16 as isize));
    // SAFETY: posting to this live window; the message carries plain values.
    let _ = unsafe { PostMessageW(hwnd, WM_MOUSEMOVE, WPARAM(0), at(from.0, from.1)) };
    // SAFETY: posting to this live window; the message carries plain values.
    let _ = unsafe { PostMessageW(hwnd, WM_LBUTTONDOWN, LEFT_DOWN, at(from.0, from.1)) };
    for step in 1..=12 {
        let y = from.1 + (to.1 - from.1) * step / 12;
        let x = from.0 + (to.0 - from.0) * step / 12;
        // SAFETY: posting to this live window; the message carries plain values.
        let _ = unsafe { PostMessageW(hwnd, WM_MOUSEMOVE, LEFT_DOWN, at(x, y)) };
    }
    // SAFETY: posting to this live window; the message carries plain values.
    let _ = unsafe { PostMessageW(hwnd, WM_LBUTTONUP, WPARAM(0), at(to.0, to.1)) };
}

#[cfg(not(windows))]
fn drag(_: &Window, _: (i32, i32), _: (i32, i32)) {}

/// Time for searches, previews and icons to arrive before each picture.
const SETTLE: Duration = Duration::from_millis(1800);

pub fn run(dir: PathBuf, cx: &mut App) {
    // No console in a release build: a crash is written beside the pictures.
    let panic_file = dir.join("panic.txt");
    let _ = std::fs::create_dir_all(&dir);
    std::panic::set_hook(Box::new(move |info| {
        let _ = std::fs::write(&panic_file, format!("{info}\n{}", std::backtrace::Backtrace::force_capture()));
    }));
    let overlay = cx.new(Overlay::new);
    cx.global_mut::<DashWindow>().overlay = Some(overlay.clone());
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(Point::default(), size(px(1120.), px(620.))))),
        titlebar: None,
        focus: false,
        // Shown below, off-screen and without taking focus.
        show: false,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        app_id: Some("Dash".into()),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        overlay.update(cx, |view, cx| view.shown(Mode::Search, window, cx));
        overlay.clone()
    });
    let handle = match opened {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("dash: can't open the window: {e}");
            cx.quit();
            return;
        }
    };
    let _ = std::fs::create_dir_all(&dir);
    cx.spawn(async move |cx| {
        // Outside any update: the size change has to reach GPUI.
        if let Ok(window) = handle.update(cx, |_, window, _| (window_handle(window), window.scale_factor())) {
            show_offscreen(window);
        }
        // Every view dark, then again light.
        for (light, (name, scene)) in [false, true].into_iter().flat_map(|light| SCENES.iter().map(move |s| (light, s)))
        {
            theme::set_light(light);
            let _ = handle.update(cx, scene);
            cx.background_executor().timer(SETTLE).await;
            let path = dir.join(format!("{}{name}.png", if light { "light-" } else { "" }));
            let saved = handle.update(cx, |_, window, _| capture(window, &path)).unwrap_or_else(|e| Err(e.to_string()));
            match saved {
                Ok(()) => println!("{}", path.display()),
                Err(e) => eprintln!("dash: {name}: {e}"),
            }
        }
        let _ = cx.update(|cx| cx.quit());
    })
    .detach();
}

#[cfg(windows)]
fn window_handle(window: &Window) -> Option<windows::Win32::Foundation::HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(windows::Win32::Foundation::HWND(handle.hwnd.get())),
        _ => None,
    }
}

/// Far left of every screen, behind other windows, not activated.
#[cfg(windows)]
fn show_offscreen((hwnd, scale): (Option<windows::Win32::Foundation::HWND>, f32)) {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetWindowRect, SetWindowPos, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        SWP_SHOWWINDOW,
    };
    let Some(hwnd) = hwnd else { return };
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "a window size in pixels at the display scale, far inside i32"
    )]
    let (w, h) = ((1120. * scale) as i32, (620. * scale) as i32);
    let (mut outer, mut client) = (RECT::default(), RECT::default());
    // Shown first, then sized: GPUI only follows a size change while the
    // window is visible. The window is bigger than its content by GPUI's
    // border.
    // SAFETY: `hwnd` is this live window's; plain values only.
    let _ = unsafe { SetWindowPos(hwnd, HWND_BOTTOM, -20_000, 0, 0, 0, SWP_NOACTIVATE | SWP_NOSIZE | SWP_SHOWWINDOW) };
    // SAFETY: `hwnd` is this live window's; `outer` is a local out-param.
    let _ = unsafe { GetWindowRect(hwnd, &raw mut outer) };
    // SAFETY: `hwnd` is this live window's; `client` is a local out-param.
    let _ = unsafe { GetClientRect(hwnd, &raw mut client) };
    let (extra_w, extra_h) = ((outer.right - outer.left) - client.right, (outer.bottom - outer.top) - client.bottom);
    // SAFETY: `hwnd` is this live window's; plain values only.
    let _ =
        unsafe { SetWindowPos(hwnd, None, 0, 0, w + extra_w, h + extra_h, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOZORDER) };
}

/// The window's own pixels (DWM draws it for us even off-screen) as a PNG.
#[cfg(windows)]
fn capture(window: &Window, path: &std::path::Path) -> Result<(), String> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
    use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

    let hwnd = window_handle(window).ok_or("no window handle")?;
    let mut rect = RECT::default();
    // All handles are created and released here.
    // SAFETY: `hwnd` is this live window's; `rect` is a local out-param.
    unsafe { GetClientRect(hwnd, &raw mut rect) }.map_err(|e| e.to_string())?;
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    let (Ok(width), Ok(height)) = (u32::try_from(w), u32::try_from(h)) else {
        return Err("The window has no size".into());
    };
    // SAFETY: the screen DC, released below.
    let screen = unsafe { GetDC(None) };
    // SAFETY: `screen` is live; the DC is deleted below.
    let dc = unsafe { CreateCompatibleDC(screen) };
    // SAFETY: `screen` is live; the bitmap is deleted below.
    let bitmap = unsafe { CreateCompatibleBitmap(screen, w, h) };
    // SAFETY: `dc` and `bitmap` are ours and live; `old` is put back below.
    let old = unsafe { SelectObject(dc, bitmap) };
    // PW_CLIENTONLY | PW_RENDERFULLCONTENT: the content only, DirectX /
    // composition included.
    // SAFETY: `hwnd` is this live window's and `dc` is ours.
    let printed = unsafe { PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(3)) }.as_bool();
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).expect("BITMAPINFOHEADER is 40 bytes"),
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // u32 always fits in usize on Windows.
    let mut pixels = vec![0u8; usize::try_from(width).unwrap_or(0) * usize::try_from(height).unwrap_or(0) * 4];
    // SAFETY: `pixels` is as large as the 32-bit, `height`-row bitmap
    // GetDIBits copies into it; `info` is a local describing that layout.
    let lines =
        unsafe { GetDIBits(dc, bitmap, 0, height, Some(pixels.as_mut_ptr().cast()), &raw mut info, DIB_RGB_COLORS) };
    // SAFETY: puts back the object `dc` had.
    unsafe { SelectObject(dc, old) };
    // SAFETY: `bitmap` is ours and no longer selected.
    let _ = unsafe { DeleteObject(bitmap) };
    // SAFETY: `dc` is ours.
    let _ = unsafe { DeleteDC(dc) };
    // SAFETY: the DC GetDC gave us above.
    unsafe { ReleaseDC(None, screen) };
    if !printed || lines == 0 {
        return Err("Windows wouldn't copy the window".into());
    }
    let pixels = (width, height, pixels);
    let (w, h, mut bgra) = pixels;
    for px in bgra.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
        px[3] = 255;
    }
    image::RgbaImage::from_raw(w, h, bgra).ok_or("bad size")?.save(path).map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn window_handle(_: &Window) {}

#[cfg(not(windows))]
fn show_offscreen(_: ((), f32)) {}

#[cfg(not(windows))]
fn capture(_: &Window, _: &std::path::Path) -> Result<(), String> {
    Err("screenshots are Windows-only for now".into())
}
