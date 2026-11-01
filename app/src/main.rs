//! Dash — search and clipboard history in a hotkey overlay (GPUI). Shares
//! its engines (`dash-search`, `dash-clipboard`) with other apps built on them.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod file_ops;
mod files_table;
mod host;
mod icons;
mod input;
mod logo;
mod media;
mod overlay;
mod preview;
mod scrollbar;
mod settings;
mod shell;
mod theme;
mod update;

use gpui::{App, Application};

fn main() {
    // The index workers are this same program started with a worker flag.
    if dash_search::search::maybe_run_index_worker_from_args() {
        return;
    }
    // `--shots <folder>`: pictures of each view, then quit (overlay/shots.rs).
    let args: Vec<String> = std::env::args().collect();
    let shots = args
        .iter()
        .position(|a| a == "--shots")
        .map(|i| args.get(i + 1).cloned().unwrap_or_else(|| "dash-shots".into()));
    // Restarted after losing ownership: the old process is still exiting.
    if args.iter().any(|a| a == host::RESTARTED) {
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    // The engines' async parts run on Tokio; enter it for the whole app.
    let _runtime = host::runtime().enter();
    // Run the engines, or follow another app that already does.
    host::claim_engines();
    // Pictures only: no scheduled builds or watchers, even if this run ends
    // up owning the engines (searches still load the index when asked).
    if shots.is_none() {
        host::start_search_engines();
    }
    if let Err(e) = dash_clipboard::start_clipboard_listener(host::host()) {
        eprintln!("dash: clipboard capture unavailable: {e}");
    }

    Application::new().with_assets(icons::Assets).run(|cx: &mut App| {
        theme::load_fonts(cx);
        input::bind_keys(cx);
        overlay::init(cx);
        if let Some(dir) = shots {
            overlay::shots(dir.into(), cx);
            return;
        }
        shell::init(cx);
        update::start_checks(cx);
        // Started at sign-in: wait in the tray.
        if !std::env::args().any(|a| a == "--background") {
            overlay::summon(overlay::Mode::Search, cx);
        }
    });
}
