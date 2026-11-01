//! What keeps Dash around between summons: global hotkeys and the tray icon
//! (Search, Clipboard, Settings, Quit). Defaults are Ctrl+Alt+F and Ctrl+Alt+C;
//! they can be changed
//! in Settings and are saved in `preferences/dash-hotkeys.json`. Closing the
//! overlay only hides it; "Quit Dash" ends the app (and its indexing and
//! clipboard capture).

use std::path::PathBuf;
use std::time::Duration;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use gpui::{App, Global};
use serde_json::json;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::overlay::{summon, Mode};

/// The two global hotkeys.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Search,
    Clipboard,
}

struct Shell {
    /// `None` when the system wouldn't give Dash global hotkeys (the tray
    /// still works).
    hotkeys: Option<GlobalHotKeyManager>,
    search: HotKey,
    clipboard: HotKey,
    /// Unregistered while Settings records a new combination.
    suspended: bool,
    search_item: MenuItem,
    clipboard_item: MenuItem,
    /// `None` when there's no tray to put an icon in (the hotkeys still work).
    tray: Option<TrayIcon>,
}

impl Global for Shell {}

impl Shell {
    fn get(&self, which: Which) -> HotKey {
        match which {
            Which::Search => self.search,
            Which::Clipboard => self.clipboard,
        }
    }

    /// Tray menu and tooltip show the current hotkeys.
    fn relabel(&self) {
        let (search, clipboard) = (label(self.search), label(self.clipboard));
        self.search_item.set_text(format!("Search    {search}"));
        self.clipboard_item.set_text(format!("Clipboard    {clipboard}"));
        if let Some(tray) = &self.tray {
            let _ = tray.set_tooltip(Some(format!("Dash — {search} search, {clipboard} clipboard")));
        }
    }
}

fn saved_path() -> PathBuf {
    dash_core::shared::shared_data_dir().join("preferences").join("dash-hotkeys.json")
}

fn load_saved() -> (Option<HotKey>, Option<HotKey>) {
    let Ok(bytes) = std::fs::read(saved_path()) else { return (None, None) };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return (None, None) };
    let parse = |key: &str| value.get(key).and_then(|v| v.as_str()).and_then(|s| s.parse::<HotKey>().ok());
    (parse("search"), parse("clipboard"))
}

fn save(search: HotKey, clipboard: HotKey) {
    let path = saved_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, json!({ "search": search.to_string(), "clipboard": clipboard.to_string() }).to_string());
}

/// "Ctrl+Alt+F".
pub fn label(hotkey: HotKey) -> String {
    let mut parts = Vec::new();
    for (flag, name) in [
        (Modifiers::CONTROL, "Ctrl"),
        (Modifiers::ALT, "Alt"),
        (Modifiers::SHIFT, "Shift"),
        (Modifiers::META, "Win"),
    ] {
        if hotkey.mods.contains(flag) {
            parts.push(name.to_string());
        }
    }
    let key = hotkey.key.to_string();
    let key = key
        .strip_prefix("Key")
        .or_else(|| key.strip_prefix("Digit"))
        .or_else(|| key.strip_prefix("Arrow"))
        .unwrap_or(&key)
        .to_string();
    parts.push(key);
    parts.join("+")
}

/// The hotkey's current combination, for Settings.
pub fn current(which: Which, cx: &App) -> String {
    label(cx.global::<Shell>().get(which))
}

/// Turns the global hotkeys off (true) or back on while a new one is recorded.
pub fn suspend(on: bool, cx: &mut App) {
    let shell = cx.global_mut::<Shell>();
    if shell.suspended == on {
        return;
    }
    shell.suspended = on;
    let keys = [shell.search, shell.clipboard];
    let Some(hotkeys) = &shell.hotkeys else { return };
    for key in keys {
        let _ = if on { hotkeys.unregister(key) } else { hotkeys.register(key) };
    }
}

/// Makes `hotkey` the one for `which`, if the system lets Dash have it.
/// Call while suspended (Settings records with the hotkeys off).
pub fn change(which: Which, hotkey: HotKey, cx: &mut App) -> Result<(), String> {
    let shell = cx.global_mut::<Shell>();
    let other = match which {
        Which::Search => shell.clipboard,
        Which::Clipboard => shell.search,
    };
    if hotkey == other {
        return Err(format!("{} is already Dash's other hotkey.", label(hotkey)));
    }
    let hotkeys = shell.hotkeys.as_ref().ok_or("Global hotkeys aren't available on this system.")?;
    // Registering now tells whether another app has it.
    hotkeys
        .register(hotkey)
        .map_err(|_| format!("{} is taken by another app. Try a different combination.", label(hotkey)))?;
    let _ = hotkeys.unregister(hotkey);
    match which {
        Which::Search => shell.search = hotkey,
        Which::Clipboard => shell.clipboard = hotkey,
    }
    save(shell.search, shell.clipboard);
    shell.relabel();
    Ok(())
}

/// A hotkey from a key pressed in the overlay (GPUI's names), if it can be one:
/// it needs Ctrl, Alt or Win (Shift alone would block normal typing).
pub fn from_keystroke(keystroke: &gpui::Keystroke) -> Result<HotKey, String> {
    let m = &keystroke.modifiers;
    if !(m.control || m.alt || m.platform) {
        return Err("Use Ctrl, Alt or Win with a key.".into());
    }
    let key = match keystroke.key.as_str() {
        "up" | "down" | "left" | "right" => format!("Arrow{}", &keystroke.key),
        "\\" => "Backslash".into(),
        k => k.to_string(),
    };
    let mut text = String::new();
    for (on, name) in [(m.control, "control+"), (m.alt, "alt+"), (m.shift, "shift+"), (m.platform, "meta+")] {
        if on {
            text.push_str(name);
        }
    }
    text.push_str(&key);
    text.parse::<HotKey>().map_err(|_| format!("{} can't be a hotkey.", keystroke.key))
}

/// A small round icon drawn in code (no image files to ship).
fn icon() -> tray_icon::Icon {
    const N: u32 = 32;
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let d = (dx * dx + dy * dy).sqrt();
            let inside = d <= 14.5;
            let ring = (9.0..=12.0).contains(&d) || (dx.abs() <= 1.5 && dy.abs() <= 6.0 && dx > -1.6);
            let (r, g, b, a) = match (inside, ring) {
                (true, true) => (0xff, 0xff, 0xff, 0xff),
                (true, false) => (0x4f, 0x8c, 0xff, 0xff),
                _ => (0, 0, 0, 0),
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }
    tray_icon::Icon::from_rgba(rgba, N, N).expect("icon")
}

pub fn init(cx: &mut App) {
    let defaults = (
        HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyF),
        HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyC),
    );
    let saved = load_saved();
    let (search, clipboard) = (saved.0.unwrap_or(defaults.0), saved.1.unwrap_or(defaults.1));
    // Without either piece Dash still runs (tray or hotkeys, whichever works).
    let hotkeys = match GlobalHotKeyManager::new() {
        Ok(hotkeys) => Some(hotkeys),
        Err(e) => {
            eprintln!("dash: no global hotkeys: {e}");
            None
        }
    };
    for key in [search, clipboard] {
        if let Some(Err(e)) = hotkeys.as_ref().map(|h| h.register(key)) {
            eprintln!("dash: hotkey {} unavailable: {e}", label(key));
        }
    }

    let search_item = MenuItem::new("Search", true, None);
    let clipboard_item = MenuItem::new("Clipboard", true, None);
    let settings_item = MenuItem::new("Settings", true, None);
    let quit_item = MenuItem::new("Quit Dash", true, None);
    let menu = Menu::new();
    let _ = menu.append_items(&[&search_item, &clipboard_item, &settings_item, &PredefinedMenuItem::separator(), &quit_item]);
    let tray = TrayIconBuilder::new()
        .with_icon(icon())
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
        .map_err(|e| eprintln!("dash: no tray icon: {e}"))
        .ok();

    let (search_id, clipboard_id, quit_id) = (search_item.id().clone(), clipboard_item.id().clone(), quit_item.id().clone());
    let settings_id = settings_item.id().clone();
    let shell = Shell { hotkeys, search, clipboard, suspended: false, search_item, clipboard_item, tray };
    shell.relabel();
    cx.set_global(shell);

    // Their events arrive on channels; check them often (cheap).
    cx.spawn(async move |cx| loop {
        cx.background_executor().timer(Duration::from_millis(50)).await;
        let Ok((search_key_id, clipboard_key_id)) =
            cx.update(|cx| (cx.global::<Shell>().search.id(), cx.global::<Shell>().clipboard.id()))
        else {
            break;
        };
        let mut wanted: Option<Option<Mode>> = None;
        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if event.state == HotKeyState::Pressed {
                if event.id == search_key_id {
                    wanted = Some(Some(Mode::Search));
                } else if event.id == clipboard_key_id {
                    wanted = Some(Some(Mode::Clipboard));
                }
            }
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == search_id {
                wanted = Some(Some(Mode::Search));
            } else if event.id == clipboard_id {
                wanted = Some(Some(Mode::Clipboard));
            } else if event.id == settings_id {
                wanted = Some(Some(Mode::Settings));
            } else if event.id == quit_id {
                wanted = Some(None);
            }
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: tray_icon::MouseButtonState::Up, .. } = event {
                wanted = Some(Some(Mode::Search));
            }
        }
        let quit = match wanted {
            Some(Some(mode)) => {
                let _ = cx.update(|cx| summon(mode, cx));
                false
            }
            Some(None) => true,
            None => false,
        };
        if quit {
            let _ = cx.update(|cx| {
                dash_search::search::stop_all_index_workers();
                cx.quit();
            });
            break;
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keystroke(key: &str, control: bool, alt: bool, shift: bool) -> gpui::Keystroke {
        gpui::Keystroke {
            key: key.into(),
            key_char: None,
            modifiers: gpui::Modifiers { control, alt, shift, ..Default::default() },
        }
    }

    #[test]
    fn pressed_keys_become_hotkeys_and_labels() {
        let hk = from_keystroke(&keystroke("g", true, true, false)).unwrap();
        assert_eq!(label(hk), "Ctrl+Alt+G");
        assert_eq!(hk.to_string().parse::<HotKey>().unwrap(), hk, "saves and loads back");
        assert_eq!(label(from_keystroke(&keystroke("space", false, true, true)).unwrap()), "Alt+Shift+Space");
        assert_eq!(label(from_keystroke(&keystroke("up", true, false, false)).unwrap()), "Ctrl+Up");
        assert_eq!(label(from_keystroke(&keystroke("5", true, true, false)).unwrap()), "Ctrl+Alt+5");
        assert_eq!(label(from_keystroke(&keystroke("f9", true, false, false)).unwrap()), "Ctrl+F9");
        assert!(from_keystroke(&keystroke("g", false, false, true)).is_err(), "Shift alone isn't enough");
        assert!(from_keystroke(&keystroke("g", false, false, false)).is_err());
    }
}
