//! Dash's colours, light or dark as chosen in Settings (by default as
//! Windows, or the system, is set). The
//! window is acrylic (blurred) on Windows, so the surfaces are slightly
//! see-through; the rest are layers of white (dark) or black (light) over
//! them.

use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{rgba, Rgba, Styled, WindowAppearance};

/// One line, clipped where it doesn't fit. Not GPUI 0.2.2's "…": its
/// truncation shortens the text's style runs in place and cuts again from
/// those on the next measurement, so the byte ranges drift and a
/// multi-byte character (Cyrillic, Georgian…) gets split, which panics.
/// Never use `truncate()` / `text_ellipsis()` in Dash for that reason.
pub trait OneLine: Styled + Sized {
    fn one_line(self) -> Self {
        self.overflow_hidden().whitespace_nowrap()
    }
}

impl<E: Styled> OneLine for E {}

/// The interface font: Inter (SIL OFL 1.1, `assets/fonts`), built into the
/// binary so it needs no download or installed font.
pub const FONT: &str = "Inter";

pub fn load_fonts(cx: &mut gpui::App) {
    let fonts = vec![
        std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/Inter-Regular.ttf").as_slice()),
        std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/Inter-Medium.ttf").as_slice()),
        std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/Inter-SemiBold.ttf").as_slice()),
    ];
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        eprintln!("dash: can't load Inter, using the system font: {e}");
    }
}

static LIGHT: AtomicBool = AtomicBool::new(false);

/// Light, dark, or as the system is set (Settings › General › Appearance).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    System,
    Light,
    Dark,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::System, Mode::Light, Mode::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Mode::System => "System",
            Mode::Light => "Light",
            Mode::Dark => "Dark",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Mode::System => "system",
            Mode::Light => "light",
            Mode::Dark => "dark",
        }
    }
}

fn mode_path() -> std::path::PathBuf {
    dash_core::shared::shared_data_dir().join("preferences").join("dash-theme.json")
}

static MODE: std::sync::Mutex<Option<Mode>> = std::sync::Mutex::new(None);

/// The chosen mode (read from disk the first time).
pub fn mode() -> Mode {
    let mut slot = MODE.lock().unwrap_or_else(|p| p.into_inner());
    *slot.get_or_insert_with(|| {
        let saved = std::fs::read(mode_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|v| v.get("mode").and_then(|m| m.as_str()).map(str::to_string));
        Mode::ALL.into_iter().find(|m| Some(m.key()) == saved.as_deref()).unwrap_or(Mode::System)
    })
}

/// Chooses `mode` (saved) and applies it with the system's `appearance`.
pub fn set_mode(mode: Mode, appearance: WindowAppearance) {
    *MODE.lock().unwrap_or_else(|p| p.into_inner()) = Some(mode);
    let path = mode_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, serde_json::json!({ "mode": mode.key() }).to_string());
    follow(appearance);
}

/// Applies the chosen mode; with System, the system's light / dark setting.
/// Returns whether the colours changed.
pub fn follow(appearance: WindowAppearance) -> bool {
    let light = match mode() {
        Mode::Light => true,
        Mode::Dark => false,
        Mode::System => matches!(appearance, WindowAppearance::Light | WindowAppearance::VibrantLight),
    };
    LIGHT.swap(light, Ordering::Relaxed) != light
}

pub fn is_light() -> bool {
    LIGHT.load(Ordering::Relaxed)
}

/// `dash --shots` draws both themes, whatever the system is set to.
pub fn set_light(light: bool) {
    LIGHT.store(light, Ordering::Relaxed);
}

fn pick(dark: u32, light: u32) -> Rgba {
    rgba(if LIGHT.load(Ordering::Relaxed) { light } else { dark })
}

/// The window itself.
pub fn bg() -> Rgba {
    pick(0x131316f0, 0xf6f6f8f2)
}
/// The preview pane and other side surfaces.
pub fn panel() -> Rgba {
    pick(0xffffff07, 0x00000006)
}
/// Buttons, chips, key caps.
pub fn panel_2() -> Rgba {
    pick(0xffffff10, 0x0000000d)
}
/// A row under the mouse.
pub fn hover() -> Rgba {
    pick(0xffffff0a, 0x00000009)
}
/// The selected row.
pub fn selected() -> Rgba {
    pick(0xffffff17, 0x00000012)
}
/// The scrollbar's thumb (brighter under the mouse: `muted`).
pub fn scrollbar() -> Rgba {
    pick(0xffffff30, 0x00000030)
}
/// Menus over the content (solid: nothing shows through).
pub fn menu() -> Rgba {
    pick(0x232328ff, 0xffffffff)
}
pub fn border() -> Rgba {
    pick(0xffffff14, 0x00000017)
}
pub fn text() -> Rgba {
    pick(0xececf0ff, 0x1b1b20ff)
}
pub fn text_2() -> Rgba {
    pick(0xb2b2bbff, 0x494952ff)
}
pub fn muted() -> Rgba {
    pick(0x7c7c87ff, 0x7e7e88ff)
}
pub fn accent() -> Rgba {
    pick(0x5b8defff, 0x2f6fe0ff)
}
/// "All good" (index up to date).
pub fn success() -> Rgba {
    pick(0x3fb950ff, 0x1a7f37ff)
}
/// Something that can't be undone.
pub fn danger() -> Rgba {
    pick(0xf85149ff, 0xcf222eff)
}
/// Selected text (the preview pane).
pub fn selection() -> Rgba {
    pick(0x5b8def55, 0x2f6fe040)
}
pub fn accent_soft() -> Rgba {
    pick(0x5b8def33, 0x2f6fe024)
}
/// The window's outline, drawn by Windows (`0x00BBGGRR`).
pub fn window_border() -> u32 {
    if LIGHT.load(Ordering::Relaxed) {
        0x00cfcfcf
    } else {
        0x00403a3a
    }
}
