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
    ("1-search-empty", |v, w, cx| {
        // Fixed starting views, whatever is saved (nothing here is saved).
        (v.search_view, v.clip_view, v.show_pane) = (SearchView::Table, ClipView::List, true);
        v.set_mode(Mode::Search, w, cx);
        type_query(v, "", cx);
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
];

/// Presses at `from`, moves in steps to `to`, releases (client pixels).
#[cfg(windows)]
fn drag(window: &Window, from: (i32, i32), to: (i32, i32)) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE};
    let Some(hwnd) = window_handle(window) else { return };
    let at = |x: i32, y: i32| LPARAM(((y as u16 as isize) << 16) | (x as u16 as isize));
    const LEFT_DOWN: WPARAM = WPARAM(1);
    // SAFETY: posting to this live window; the messages carry plain values.
    unsafe {
        let _ = PostMessageW(hwnd, WM_MOUSEMOVE, WPARAM(0), at(from.0, from.1));
        let _ = PostMessageW(hwnd, WM_LBUTTONDOWN, LEFT_DOWN, at(from.0, from.1));
        for step in 1..=12 {
            let y = from.1 + (to.1 - from.1) * step / 12;
            let x = from.0 + (to.0 - from.0) * step / 12;
            let _ = PostMessageW(hwnd, WM_MOUSEMOVE, LEFT_DOWN, at(x, y));
        }
        let _ = PostMessageW(hwnd, WM_LBUTTONUP, WPARAM(0), at(to.0, to.1));
    }
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
        for (light, (name, scene)) in [false, true].into_iter().flat_map(|light| SCENES.iter().map(move |s| (light, s))) {
            theme::set_light(light);
            let _ = handle.update(cx, |view, window, cx| scene(view, window, cx));
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
        GetClientRect, GetWindowRect, SetWindowPos, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    };
    let Some(hwnd) = hwnd else { return };
    let (w, h) = ((1120. * scale) as i32, (620. * scale) as i32);
    let (mut outer, mut client) = (RECT::default(), RECT::default());
    // SAFETY: `hwnd` is this live window's. Shown first, then sized: GPUI
    // only follows a size change while the window is visible. The window is
    // bigger than its content by GPUI's border.
    unsafe {
        let _ = SetWindowPos(hwnd, HWND_BOTTOM, -20_000, 0, 0, 0, SWP_NOACTIVATE | SWP_NOSIZE | SWP_SHOWWINDOW);
        let _ = GetWindowRect(hwnd, &mut outer);
        let _ = GetClientRect(hwnd, &mut client);
        let (extra_w, extra_h) = ((outer.right - outer.left) - client.right, (outer.bottom - outer.top) - client.bottom);
        let _ = SetWindowPos(hwnd, None, 0, 0, w + extra_w, h + extra_h, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOZORDER);
    }
}

/// The window's own pixels (DWM draws it for us even off-screen) as a PNG.
#[cfg(windows)]
fn capture(window: &Window, path: &std::path::Path) -> Result<(), String> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
    use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

    let hwnd = window_handle(window).ok_or("no window handle")?;
    let mut rect = RECT::default();
    // SAFETY: all handles are created and released here; `pixels` is as
    // large as the bitmap GetDIBits copies into it.
    let pixels = unsafe {
        GetClientRect(hwnd, &mut rect).map_err(|e| e.to_string())?;
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(dc, bitmap);
        // PW_CLIENTONLY | PW_RENDERFULLCONTENT: the content only, DirectX /
        // composition included.
        let printed = PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(3)).as_bool();
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(dc, bitmap, 0, h as u32, Some(pixels.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        if !printed || lines == 0 {
            return Err("Windows wouldn't copy the window".into());
        }
        (w as u32, h as u32, pixels)
    };
    let (w, h, mut bgra) = pixels;
    for px in bgra.chunks_exact_mut(4) {
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
