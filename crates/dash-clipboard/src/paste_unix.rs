//! Pasting into the app that was in front before the overlay, on macOS and
//! Linux. The overlay hides, that app gets focus back (macOS: activated by
//! its process id; Linux: the window manager returns focus), and Cmd+V /
//! Ctrl+V is sent through enigo (`CGEvent` on macOS, `XTest` on X11).
//!
//! macOS asks once for Accessibility permission. A Wayland session takes no
//! synthetic keys from apps, so there the text stays on the clipboard with a
//! note, as on Windows when no target window is known.

use std::time::Duration;

/// Time for the overlay to hide and the target app to take focus.
const FOCUS_SETTLE: Duration = Duration::from_millis(120);

/// The app in front just before the overlay opened (macOS process id).
#[cfg(target_os = "macos")]
static PREVIOUS_APP_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(target_os = "macos")]
pub(crate) fn remember_front_app() {
    use objc2_app_kit::NSWorkspace;
    use std::sync::atomic::Ordering;
    let pid = NSWorkspace::sharedWorkspace().frontmostApplication().map_or(0, |app| app.processIdentifier());
    // Dash itself in front (its settings window) is no target.
    let pid = if u32::try_from(pid).is_ok_and(|pid| pid == std::process::id()) { 0 } else { pid };
    PREVIOUS_APP_PID.store(pid, Ordering::SeqCst);
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn remember_front_app() {
    // The window manager hands focus back by itself when the overlay hides.
}

/// Whether keys can be sent here at all; `Err` holds what to tell the user.
fn can_send_keys() -> Result<(), &'static str> {
    if cfg!(target_os = "linux") && std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind == "wayland") {
        return Err("Wayland doesn't let apps paste for you — press Ctrl+V in your app.");
    }
    #[cfg(target_os = "macos")]
    if PREVIOUS_APP_PID.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        return Err("Text is on your clipboard — switch to your target app and press Cmd+V.");
    }
    Ok(())
}

/// Brings the remembered app back to the front.
#[cfg(target_os = "macos")]
fn refocus() {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    let pid = PREVIOUS_APP_PID.load(std::sync::atomic::Ordering::SeqCst);
    if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
        app.activateWithOptions(NSApplicationActivationOptions::empty());
    }
}

#[cfg(not(target_os = "macos"))]
fn refocus() {}

fn keyboard() -> Result<enigo::Enigo, String> {
    enigo::Enigo::new(&enigo::Settings::default()).map_err(|e| format!("keyboard: {e}"))
}

/// Cmd+V (macOS) / Ctrl+V into the app in front, after the overlay hid.
fn send_paste() -> Result<(), String> {
    use enigo::{Direction, Key, Keyboard};
    refocus();
    std::thread::sleep(FOCUS_SETTLE);
    let mut keys = keyboard()?;
    // macOS: V by key code (kVK_ANSI_V); looking a character up in the
    // keyboard layout must happen on the main thread there.
    let (modifier, v) =
        if cfg!(target_os = "macos") { (Key::Meta, Key::Other(9)) } else { (Key::Control, Key::Unicode('v')) };
    keys.key(modifier, Direction::Press).map_err(|e| e.to_string())?;
    let pressed = keys.key(v, Direction::Click).map_err(|e| e.to_string());
    // The modifier comes back up even when V failed.
    keys.key(modifier, Direction::Release).map_err(|e| e.to_string())?;
    pressed
}

/// Types `text` into the app in front, after the overlay hid.
fn send_text(text: &str) -> Result<(), String> {
    use enigo::Keyboard;
    refocus();
    std::thread::sleep(FOCUS_SETTLE);
    keyboard()?.text(text).map_err(|e| e.to_string())
}

const PASTE_BY_HAND: &str = if cfg!(target_os = "macos") {
    "Text is on your clipboard — press Cmd+V in your app."
} else {
    "Text is on your clipboard — press Ctrl+V in your app."
};

/// Pastes what's on the clipboard into the previous app, on a thread so the
/// overlay can hide first. `restore` goes back on the clipboard afterwards
/// (dictation keeps the user's own copy).
pub(crate) fn paste_soon(restore: Option<String>) -> Result<(), String> {
    if let Err(note) = can_send_keys() {
        crate::emit_paste_fallback(note);
        return Ok(());
    }
    std::thread::Builder::new()
        .name("dash-paste-injector".to_string())
        .spawn(move || {
            if let Err(error) = send_paste() {
                eprintln!("clipboard_history: paste failed ({error})");
                crate::emit_paste_fallback(PASTE_BY_HAND);
                return;
            }
            if let Some(prior) = restore {
                std::thread::sleep(Duration::from_millis(400));
                let _ = crate::write_clipboard_text(prior);
            }
        })
        .map(drop)
        .map_err(|e| format!("spawn paste injector: {e}"))
}

/// Types `text` into the previous app; if it can't, the text goes on the
/// clipboard (kept out of the history) so it's never lost.
pub(crate) fn type_soon(text: String) -> Result<(), String> {
    if let Err(note) = can_send_keys() {
        let _ = crate::write_clipboard_text(text);
        crate::emit_paste_fallback(note);
        return Ok(());
    }
    std::thread::Builder::new()
        .name("dash-typeout-injector".to_string())
        .spawn(move || {
            if let Err(error) = send_text(&text) {
                eprintln!("clipboard_history: typing failed ({error})");
                let _ = crate::write_clipboard_text(text);
                crate::emit_paste_fallback(PASTE_BY_HAND);
            }
        })
        .map(drop)
        .map_err(|e| format!("spawn typeout injector: {e}"))
}
