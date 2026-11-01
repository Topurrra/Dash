//! Dash — search and clipboard history in a hotkey overlay (GPUI). Shares
//! its engines (`dash-search`, `dash-clipboard`) with other apps built on them.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod file_ops;
mod files_table;
mod host;
mod input;
mod media;
mod overlay;
mod preview;
mod settings;
mod shell;
mod theme;

use gpui::{App, Application};

fn main() {
    // The index workers are this same program started with a worker flag.
    if dash_search::search::maybe_run_index_worker_from_args() {
        return;
    }
    // The engines' async parts run on Tokio; enter it for the whole app.
    let _runtime = host::runtime().enter();
    // Run the engines, or follow another app that already does.
    host::claim_engines();
    host::start_search_engines();
    if let Err(e) = dash_clipboard::start_clipboard_listener(host::host()) {
        eprintln!("dash: clipboard capture unavailable: {e}");
    }

    Application::new().run(|cx: &mut App| {
        input::bind_keys(cx);
        overlay::init(cx);
        shell::init(cx);
        // Started at sign-in: wait in the tray.
        if !std::env::args().any(|a| a == "--background") {
            overlay::summon(overlay::Mode::Search, cx);
        }
    });
}
