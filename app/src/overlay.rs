//! The overlay: a search box over a results list, in two modes — Search
//! (Files / Inside tabs once something is typed) and Clipboard (history,
//! text and images, GIFs animated; Enter pastes into the app you came from).
//! Summoned by hotkey or the tray; Esc or clicking elsewhere hides it, and
//! it keeps its state for the next time.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dash_clipboard::{ClipboardEntry, EntryKind};
use gpui::{
    actions, div, img, prelude::*, px, size, uniform_list, App, Bounds, ClipboardItem, Context, Entity, FocusHandle,
    Focusable, Global, KeyBinding, MouseButton, ObjectFit, Point, RenderImage, ScrollHandle, ScrollStrategy, SharedString, Subscription,
    UniformListScrollHandle, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
    WindowOptions,
};

use crate::input::{InputEvent, TextInput};
use crate::media::{self, Player};
use crate::settings::{self, Settings};
use crate::preview::{self, human_size, Body, Preview};
use crate::theme;

mod table;

actions!(
    dash,
    [SelectNext, SelectPrev, PageDown, PageUp, OpenSelected, RevealSelected, CopyPath, Dismiss, NextTab, SearchMode, ClipboardMode, TogglePin, DeleteEntry, ToggleWrap, PlayPause, Back10, Forward10, Mute, VolumeUp, VolumeDown, OpenSettings, SelectAllRows, ExtendNext, ExtendPrev, RenameSelected, DeleteSelected, DeleteForGood, MoveSelected]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("Overlay");
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, ctx),
        KeyBinding::new("up", SelectPrev, ctx),
        KeyBinding::new("pagedown", PageDown, ctx),
        KeyBinding::new("pageup", PageUp, ctx),
        KeyBinding::new("enter", OpenSelected, ctx),
        KeyBinding::new("ctrl-enter", RevealSelected, ctx),
        KeyBinding::new("ctrl-shift-c", CopyPath, ctx),
        KeyBinding::new("escape", Dismiss, ctx),
        KeyBinding::new("tab", NextTab, ctx),
        KeyBinding::new("ctrl-1", SearchMode, ctx),
        KeyBinding::new("ctrl-2", ClipboardMode, ctx),
        KeyBinding::new("ctrl-,", OpenSettings, ctx),
        // Alt+letter: pin, delete.
        KeyBinding::new("alt-p", TogglePin, ctx),
        KeyBinding::new("alt-d", DeleteEntry, ctx),
        KeyBinding::new("alt-w", ToggleWrap, ctx),
        // Player (YouTube-like letters).
        KeyBinding::new("alt-k", PlayPause, ctx),
        KeyBinding::new("alt-j", Back10, ctx),
        KeyBinding::new("alt-l", Forward10, ctx),
        KeyBinding::new("alt-m", Mute, ctx),
        KeyBinding::new("alt-up", VolumeUp, ctx),
        KeyBinding::new("alt-down", VolumeDown, ctx),
        // Files table. The search box takes Ctrl+A and Delete while it has
        // text (it passes them on when empty; see `TextInput`).
        KeyBinding::new("ctrl-a", SelectAllRows, ctx),
        KeyBinding::new("shift-down", ExtendNext, ctx),
        KeyBinding::new("shift-up", ExtendPrev, ctx),
        KeyBinding::new("f2", RenameSelected, ctx),
        KeyBinding::new("delete", DeleteSelected, ctx),
        KeyBinding::new("shift-delete", DeleteForGood, ctx),
        KeyBinding::new("ctrl-m", MoveSelected, ctx),
    ]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Search,
    Clipboard,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Files,
    Inside,
}

#[derive(Clone)]
struct Row {
    name: SharedString,
    path: SharedString,
    meta: SharedString,
    /// Inside: the matching passage.
    snippet: Option<SharedString>,
    folder: bool,
}

/// One clipboard history entry, ready to draw.
#[derive(Clone)]
struct Clip {
    id: u64,
    /// Text entries: the text on one line (shortened).
    text: SharedString,
    /// Image entries: what to show (the GIF itself so it moves, else the thumbnail).
    image: Option<PathBuf>,
    meta: SharedString,
    pinned: bool,
    full_text: SharedString,
    full_image: Option<PathBuf>,
    /// The app it was copied from, for its icon.
    app_path: Option<String>,
}

pub struct Overlay {
    input: Entity<TextInput>,
    focus: FocusHandle,
    mode: Mode,
    tab: Tab,
    /// The Files tab's table (rows paged in from the name search).
    table: table::Table,
    inside: Vec<Row>,
    clips: Vec<Clip>,
    inside_total: usize,
    searching: bool,
    error: Option<String>,
    selected: usize,
    generation: u64,
    status: SharedString,
    scroll: UniformListScrollHandle,
    preview: Option<Preview>,
    /// Clipboard: the selected entry's pane, by entry id.
    clip_preview: Option<(u64, Body)>,
    preview_generation: u64,
    preview_scroll: ScrollHandle,
    wrap: bool,
    /// The video or music in the pane, by path.
    player: Option<(String, Player)>,
    settings: Entity<Settings>,
    /// The frame last drawn, so the one before can be freed.
    drawn_frame: Option<Arc<RenderImage>>,
    /// Player volume (0–1) and mute, kept from file to file.
    volume: f32,
    muted: bool,
    /// Shell icons by `icon_key` (`None`: loading or none).
    icons: HashMap<String, Option<PathBuf>>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for Overlay {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Overlay {
    fn new(cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Search files…", cx));
        let subscription = cx.subscribe(&input, |this: &mut Self, _input, event: &InputEvent, cx| match event {
            InputEvent::Changed => this.query_changed(cx),
        });
        Overlay {
            input,
            focus: cx.focus_handle(),
            mode: Mode::Search,
            tab: Tab::Files,
            table: table::Table::default(),
            inside: Vec::new(),
            clips: Vec::new(),
            inside_total: 0,
            searching: false,
            error: None,
            selected: 0,
            generation: 0,
            status: "".into(),
            scroll: UniformListScrollHandle::new(),
            preview: None,
            clip_preview: None,
            preview_generation: 0,
            preview_scroll: ScrollHandle::new(),
            wrap: false,
            player: None,
            settings: cx.new(Settings::new),
            drawn_frame: None,
            volume: 1.0,
            muted: false,
            icons: HashMap::new(),
            _subscriptions: vec![subscription],
        }
    }

    fn query(&self, cx: &App) -> String {
        self.input.read(cx).text().trim().to_string()
    }

    fn rows(&self) -> &[Row] {
        match (self.mode, self.tab) {
            (Mode::Search, Tab::Inside) => &self.inside,
            // The Files tab has its own table.
            (Mode::Search, Tab::Files) | (Mode::Clipboard | Mode::Settings, _) => &[],
        }
    }

    fn count(&self) -> usize {
        match self.mode {
            Mode::Search if self.tab == Tab::Files => self.table.total,
            Mode::Search => self.rows().len(),
            Mode::Clipboard => self.clips.len(),
            Mode::Settings => 0,
        }
    }

    /// Re-reads the history and filters it by the query (pinned first).
    fn load_clips(&mut self, cx: &mut Context<Self>) {
        let needle = self.query(cx).to_lowercase();
        let mut entries: Vec<ClipboardEntry> = dash_clipboard::get_clipboard_history()
            .into_iter()
            .filter(|e| {
                needle.is_empty()
                    || e.text.to_lowercase().contains(&needle)
                    || e.pin_label.as_deref().is_some_and(|l| l.to_lowercase().contains(&needle))
                    || e.source_app.as_deref().is_some_and(|a| a.to_lowercase().contains(&needle))
            })
            .collect();
        entries.sort_by_key(|e| !e.is_pinned);
        self.clips = entries.into_iter().map(clip).collect();
        let apps: Vec<(String, bool)> = self.clips.iter().filter_map(|c| c.app_path.clone()).map(|p| (p, false)).collect();
        self.request_icons(apps, cx);
        self.selected = self.selected.min(self.clips.len().saturating_sub(1));
        self.clip_preview = None;
        self.update_clip_preview();
    }

    fn selected_clip(&self) -> Option<u64> {
        (self.mode == Mode::Clipboard).then(|| self.clips.get(self.selected).map(|c| c.id)).flatten()
    }

    /// Called each time the overlay is shown.
    fn shown(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = mode;
        self.refresh_status();
        let on_settings = mode == Mode::Settings;
        self.settings.update(cx, |settings, cx| {
            settings.visible = on_settings;
            if on_settings {
                settings.refresh(cx);
            } else {
                // Left Settings mid-recording: give the global hotkeys back.
                settings.stop_recording(cx);
            }
        });
        match mode {
            Mode::Clipboard => self.load_clips(cx),
            Mode::Search => {
                // Fresh rows each time (files change while it's hidden).
                self.reload_table(true, cx);
                self.update_preview(cx);
            }
            Mode::Settings => {
                window.focus(&self.focus);
                cx.notify();
                return;
            }
        }
        self.input.update(cx, |input, cx| {
            input.set_placeholder(
                if mode == Mode::Search { "Search files…" } else { "Search clipboard history…" },
                cx,
            );
            input.select_everything(cx);
        });
        window.focus(&self.input.focus_handle(cx));
        cx.notify();
    }

    fn refresh_status(&mut self) {
        let host = crate::host::host();
        self.status = match dash_search::search::get_file_search_status(host) {
            Ok(s) if s.filename_indexing || s.indexing => {
                format!("Indexing… {} names, {} documents so far", s.filename_indexed_files, s.indexed_files).into()
            }
            Ok(s) => format!("{} names · {} documents indexed", s.filename_indexed_files, s.indexed_files).into(),
            Err(e) => e.into(),
        };
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        if self.mode == Mode::Settings {
            return;
        }
        let query = self.query(cx);
        self.selected = 0;
        if self.mode == Mode::Clipboard {
            self.load_clips(cx);
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
            cx.notify();
            return;
        }
        let names = self.table_query(&query);
        let table_generation = self.table_generation();
        if query.is_empty() {
            // Nothing to look for inside files; the Files tab lists everything.
            self.tab = Tab::Files;
            self.inside.clear();
            self.inside_total = 0;
        }
        self.searching = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            // Wait for a pause in typing.
            cx.background_executor().timer(Duration::from_millis(110)).await;
            if this.update(cx, |this, _| this.generation != generation).unwrap_or(true) {
                return;
            }
            let host = crate::host::host();
            let rt = crate::host::runtime();
            let files = rt.spawn(dash_search::names::search_names(host.clone(), names));
            let inside = (!query.is_empty()).then(|| {
                rt.spawn(dash_search::search::search_file_contents(
                    host,
                    dash_search::search::FileSearchQueryOptions {
                        query: query.clone(),
                        limit: Some(80),
                        offset: None,
                        extension_filter: None,
                        path_filter: None,
                        natural_language: Some(true),
                    },
                ))
            });
            let files = files.await.map_err(|e| e.to_string()).and_then(|r| r);
            let inside = match inside {
                Some(task) => Some(task.await.map_err(|e| e.to_string()).and_then(|r| r)),
                None => None,
            };
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.searching = false;
                match inside {
                    Some(Ok(r)) => {
                        this.inside_total = r.total_hits;
                        this.inside = r.results.into_iter().map(inside_row).collect();
                    }
                    Some(Err(_)) | None => {
                        this.inside.clear();
                        this.inside_total = 0;
                    }
                }
                this.selected = 0;
                let wanted: Vec<(String, bool)> = this.inside.iter().map(|r| (r.path.to_string(), r.folder)).collect();
                this.request_icons(wanted, cx);
                if this.table_generation() == table_generation {
                    // Sets the rows, scrolls to the top, updates the preview, redraws.
                    this.apply_first_page(files, false, cx);
                } else {
                    this.update_preview(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Loads the pane for the selected row (Search) or entry (Clipboard).
    fn update_preview(&mut self, cx: &mut Context<Self>) {
        if self.mode == Mode::Clipboard {
            self.update_clip_preview();
            self.preview_scroll.set_offset(Point::default());
            return;
        }
        let Some(path) = self.selected_path() else {
            self.preview = None;
            self.player = None;
            return;
        };
        if self.player.as_ref().is_some_and(|(p, _)| *p != path) {
            self.player = None;
        }
        if self.preview.as_ref().is_some_and(|p| p.path == path) {
            return;
        }
        self.preview_generation += 1;
        let generation = self.preview_generation;
        let query = if self.tab == Tab::Inside { self.query(cx) } else { String::new() };
        cx.spawn(async move |this, cx| {
            // Skip rows passed over while holding an arrow key.
            cx.background_executor().timer(Duration::from_millis(60)).await;
            if this.update(cx, |this, _| this.preview_generation != generation).unwrap_or(true) {
                return;
            }
            let loaded = crate::host::runtime().spawn_blocking(move || preview::load(path, &query)).await;
            let _ = this.update(cx, |this, cx| {
                if this.preview_generation != generation {
                    return;
                }
                if let Ok(p) = loaded {
                    if let Body::Media(path) = &p.body {
                        this.start_player(path.display().to_string(), cx);
                    }
                    match &p.body {
                        Body::Text { hit: Some(line), .. } => this.preview_scroll.scroll_to_item(line.saturating_sub(3)),
                        _ => this.preview_scroll.set_offset(Point::default()),
                    }
                    this.preview = Some(p);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn update_clip_preview(&mut self) {
        let Some(clip) = self.clips.get(self.selected) else {
            self.clip_preview = None;
            return;
        };
        if self.clip_preview.as_ref().is_some_and(|(id, _)| *id == clip.id) {
            return;
        }
        let body = match &clip.full_image {
            Some(path) => Body::Image(path.clone()),
            None => preview::text_only(&clip.full_text),
        };
        self.clip_preview = Some((clip.id, body));
    }

    fn on_toggle_wrap(&mut self, _: &ToggleWrap, _: &mut Window, cx: &mut Context<Self>) {
        self.wrap = !self.wrap;
        cx.notify();
    }

    /// Loads shell icons for these paths in the background (cached on disk).
    fn request_icons(&mut self, paths: Vec<(String, bool)>, cx: &mut Context<Self>) {
        let mut wanted = Vec::new();
        for (path, folder) in paths {
            let (key, kind) = icon_key(&path, folder);
            if !self.icons.contains_key(&key) {
                self.icons.insert(key.clone(), None);
                wanted.push((key, path, kind));
            }
        }
        if wanted.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let loaded = crate::host::runtime()
                .spawn_blocking(move || {
                    let host = crate::host::host();
                    wanted
                        .into_iter()
                        .map(|(key, path, kind)| {
                            let icon = dash_search::launcher_icons::ensure_launcher_icon(host.clone(), path, Some(kind.into()))
                                .ok()
                                .flatten()
                                .map(PathBuf::from);
                            (key, icon)
                        })
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                this.icons.extend(loaded);
                cx.notify();
            });
        })
        .detach();
    }

    fn icon(&self, path: &str, folder: bool) -> Option<PathBuf> {
        self.icons.get(&icon_key(path, folder).0).cloned().flatten()
    }

    fn select(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.files_tab() {
            self.table_move(delta, false, cx);
            return;
        }
        let count = self.count();
        if count == 0 {
            return;
        }
        let next = (self.selected as isize + delta).clamp(0, count as isize - 1) as usize;
        self.selected = next;
        self.scroll.scroll_to_item(next, ScrollStrategy::Center);
        self.update_preview(cx);
        cx.notify();
    }

    fn selected_path(&self) -> Option<String> {
        if self.files_tab() {
            return self.table.focus_path();
        }
        self.rows().get(self.selected).map(|r| r.path.to_string())
    }

    fn on_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select(1, cx);
    }
    fn on_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.select(-1, cx);
    }
    fn on_page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select(8, cx);
    }
    fn on_page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select(-8, cx);
    }

    fn on_open(&mut self, _: &OpenSelected, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.selected_clip() {
            // Hide first so the paste lands in the app we came from.
            self.hide(window, cx);
            if let Err(e) = dash_clipboard::paste_clipboard_entry(id, Some(true)) {
                eprintln!("dash: paste failed: {e}");
            }
            return;
        }
        if self.files_tab() {
            if !self.table_enter(window, cx) {
                self.open_files(window, cx);
            }
            return;
        }
        if let Some(path) = self.selected_path() {
            match dash_search::search::open_search_result_path(crate::host::host(), path) {
                Ok(()) => self.hide(window, cx),
                Err(e) => {
                    self.error = Some(e);
                    cx.notify();
                }
            }
        }
    }

    fn on_reveal(&mut self, _: &RevealSelected, window: &mut Window, cx: &mut Context<Self>) {
        // Clipboard: copy only, no paste.
        if let Some(id) = self.selected_clip() {
            self.hide(window, cx);
            if let Err(e) = dash_clipboard::copy_clipboard_entry_to_clipboard(id) {
                eprintln!("dash: copy failed: {e}");
            }
            return;
        }
        if self.files_tab() {
            self.reveal_files(window, cx);
            return;
        }
        if let Some(path) = self.selected_path() {
            reveal(&path);
            self.hide(window, cx);
        }
    }

    fn on_copy_path(&mut self, _: &CopyPath, _: &mut Window, cx: &mut Context<Self>) {
        if self.files_tab() {
            self.copy_file_paths(cx);
            return;
        }
        if let Some(path) = self.selected_path() {
            cx.write_to_clipboard(ClipboardItem::new_string(path));
        }
    }

    fn on_toggle_pin(&mut self, _: &TogglePin, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(clip) = self.clips.get(self.selected).filter(|_| self.mode == Mode::Clipboard) {
            let _ = dash_clipboard::pin_clipboard_entry(clip.id, !clip.pinned);
            self.load_clips(cx);
            cx.notify();
        }
    }

    fn on_delete_entry(&mut self, _: &DeleteEntry, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.selected_clip() {
            let _ = dash_clipboard::delete_clipboard_entry(id);
            self.load_clips(cx);
            cx.notify();
        }
    }

    fn on_dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        // Esc first closes the table's menu, rename box or delete question.
        if self.files_tab() && self.table_escape(window, cx) {
            return;
        }
        self.hide(window, cx);
    }

    /// Hides the overlay. A video pauses; music keeps playing.
    fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pause_video();
        // A hotkey being recorded in Settings: give the global hotkeys back.
        self.settings.update(cx, |settings, cx| settings.stop_recording(cx));
        // A delete question or rename left open mustn't run on the next Enter.
        self.table_forget_pending();
        window.remove_window();
    }

    fn pause_video(&mut self) {
        if let Some((_, player)) = &self.player {
            if player.is_video() && !player.paused() {
                player.toggle();
            }
        }
    }

    fn player(&self) -> Option<&Player> {
        self.player.as_ref().map(|(_, p)| p).filter(|_| self.mode == Mode::Search)
    }

    fn on_play_pause(&mut self, _: &PlayPause, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.player() {
            p.toggle();
            cx.notify();
        }
    }
    fn on_back(&mut self, _: &Back10, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.player() {
            p.skip(-10);
            cx.notify();
        }
    }
    fn on_forward(&mut self, _: &Forward10, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.player() {
            p.skip(10);
            cx.notify();
        }
    }
    fn on_mute(&mut self, _: &Mute, _: &mut Window, cx: &mut Context<Self>) {
        self.muted = !self.muted;
        self.apply_volume(cx);
    }
    fn on_volume_up(&mut self, _: &VolumeUp, _: &mut Window, cx: &mut Context<Self>) {
        self.set_volume(self.volume + 0.1, cx);
    }
    fn on_volume_down(&mut self, _: &VolumeDown, _: &mut Window, cx: &mut Context<Self>) {
        self.set_volume(self.volume - 0.1, cx);
    }

    fn set_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        self.volume = (volume * 10.).round().clamp(0., 10.) / 10.;
        self.muted = false;
        self.apply_volume(cx);
    }

    fn apply_volume(&mut self, cx: &mut Context<Self>) {
        if let Some((_, p)) = &self.player {
            p.set_volume(if self.muted { 0. } else { self.volume });
        }
        cx.notify();
    }

    /// Starts (paused) the player for a media preview, and redraws while it runs.
    fn start_player(&mut self, path: String, cx: &mut Context<Self>) {
        if self.player.as_ref().is_some_and(|(p, _)| *p == path) {
            return;
        }
        self.player = Some((path.clone(), Player::open(&path)));
        self.apply_volume(cx);
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(33)).await;
            let alive = this
                .update(cx, |this, cx| {
                    let Some((p, player)) = &this.player else { return false };
                    if *p != path {
                        return false;
                    }
                    let new_frame = player.frame().map(|f| f.id) != this.drawn_frame.as_ref().map(|f| f.id);
                    if !player.paused() || new_frame {
                        cx.notify();
                    }
                    true
                })
                .unwrap_or(false);
            if !alive {
                break;
            }
        })
        .detach();
    }

    fn on_next_tab(&mut self, _: &NextTab, _: &mut Window, cx: &mut Context<Self>) {
        if self.mode == Mode::Search && !self.query(cx).is_empty() {
            self.tab = if self.tab == Tab::Files { Tab::Inside } else { Tab::Files };
            self.selected = 0;
            self.update_preview(cx);
            cx.notify();
        }
    }

    fn set_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.shown(mode, window, cx);
            self.query_changed(cx);
        }
    }

    fn on_search_mode(&mut self, _: &SearchMode, window: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(Mode::Search, window, cx);
    }
    fn on_clipboard_mode(&mut self, _: &ClipboardMode, window: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(Mode::Clipboard, window, cx);
    }
    fn on_open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(Mode::Settings, window, cx);
    }

    fn render_mode_chip(&self, mode: Mode, label: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.mode == mode;
        div()
            .id(label)
            .px_3()
            .py_1()
            .rounded_md()
            .text_sm()
            .cursor_pointer()
            .when(active, |d| d.bg(theme::accent_soft()).text_color(theme::text()))
            .when(!active, |d| d.text_color(theme::muted()).hover(|s| s.text_color(theme::text_2())))
            .on_click(cx.listener(move |this, _, window, cx| this.set_mode(mode, window, cx)))
            .child(label)
    }

    fn render_tab(&self, tab: Tab, label: &'static str, count: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.tab == tab;
        div()
            .id(label)
            .flex()
            .gap_1()
            .px_3()
            .py_1()
            .text_sm()
            .cursor_pointer()
            .border_b_2()
            .when(active, |d| d.border_color(theme::accent()).text_color(theme::text()))
            .when(!active, |d| d.border_color(gpui::transparent_black()).text_color(theme::muted()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tab = tab;
                this.selected = 0;
                this.update_preview(cx);
                cx.notify();
            }))
            .child(label)
            .child(div().text_color(theme::muted()).child(count.to_string()))
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let query = self.query(cx);
        let note = |text: String| {
            div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .px_6()
                .text_sm()
                .text_color(theme::muted())
                .child(text)
                .into_any_element()
        };
        if self.mode == Mode::Clipboard {
            if self.clips.is_empty() {
                return note(if query.is_empty() { "Nothing copied yet.".into() } else { "Nothing found.".into() });
            }
            return self.render_clips(cx);
        }
        if let Some(error) = &self.error {
            return note(error.clone());
        }
        if self.tab == Tab::Files {
            if self.table.total == 0 {
                return note(match (self.searching, query.is_empty()) {
                    (true, _) => "Searching…".into(),
                    (false, true) => "Type to search file names. Tab switches to what's inside files.".into(),
                    (false, false) => "Nothing found.".into(),
                });
            }
            return self.render_table(cx);
        }
        let count = self.rows().len();
        if count == 0 {
            return note(if self.searching { "Searching…".into() } else { "Nothing found.".into() });
        }
        let selected = self.selected;
        uniform_list(
            "results",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                let rows = this.rows();
                range
                    .filter_map(|ix| rows.get(ix).cloned().map(|row| (ix, row)))
                    .map(|(ix, row)| {
                        let active = ix == selected;
                        div()
                            .id(ix)
                            .flex()
                            .gap_3()
                            .items_center()
                            .h(px(if row.snippet.is_some() { 58. } else { 44. }))
                            .px_3()
                            .mx_2()
                            .rounded_md()
                            .cursor_pointer()
                            .when(active, |d| d.bg(theme::panel_2()))
                            .hover(|s| s.bg(theme::panel()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                    this.selected = ix;
                                    this.update_preview(cx);
                                    cx.notify();
                                    if event.click_count >= 2 {
                                        this.on_open(&OpenSelected, window, cx);
                                    }
                                }),
                            )
                            .child(match this.icon(&row.path, row.folder) {
                                Some(icon) => div()
                                    .flex_none()
                                    .w(px(30.))
                                    .h(px(30.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(img(icon).w(px(26.)).h(px(26.)))
                                    .into_any_element(),
                                None => div()
                                    .flex_none()
                                    .w(px(30.))
                                    .h(px(30.))
                                    .rounded_md()
                                    .bg(theme::panel_2())
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_xs()
                                    .text_color(theme::text_2())
                                    .child(ext_badge(&row.path, row.folder))
                                    .into_any_element(),
                            })
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .child(div().text_sm().text_color(theme::text()).truncate().child(row.name.clone()))
                                    .child(div().text_xs().text_color(theme::muted()).truncate().child(row.path.clone()))
                                    .when_some(row.snippet.clone(), |d, s| {
                                        d.child(div().text_xs().text_color(theme::text_2()).truncate().child(s))
                                    }),
                            )
                            .child(div().flex_none().text_xs().text_color(theme::muted()).child(row.meta.clone()))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.scroll.clone())
        .flex_1()
        .into_any_element()
    }

    fn render_clips(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected = self.selected;
        uniform_list(
            "clips",
            self.clips.len(),
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .filter_map(|ix| this.clips.get(ix).cloned().map(|clip| (ix, clip)))
                    .map(|(ix, clip)| {
                        let active = ix == selected;
                        div()
                            .id(ix)
                            .flex()
                            .gap_3()
                            .items_center()
                            .h(px(64.))
                            .px_3()
                            .mx_2()
                            .rounded_md()
                            .cursor_pointer()
                            .when(active, |d| d.bg(theme::panel_2()))
                            .hover(|s| s.bg(theme::panel()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                    this.selected = ix;
                                    this.update_preview(cx);
                                    cx.notify();
                                    if event.click_count >= 2 {
                                        this.on_open(&OpenSelected, window, cx);
                                    }
                                }),
                            )
                            .child(match clip.image.clone() {
                                Some(path) => div()
                                    .flex_none()
                                    .w(px(84.))
                                    .h(px(52.))
                                    .rounded_md()
                                    .overflow_hidden()
                                    .bg(theme::panel_2())
                                    .child(
                                        img(path)
                                            // An id keeps a GIF's frame between redraws.
                                            .id(SharedString::from(format!("clip-img-{}", clip.id)))
                                            .size_full()
                                            .object_fit(ObjectFit::Contain),
                                    )
                                    .into_any_element(),
                                None => div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .text_color(theme::text())
                                    .line_clamp(2)
                                    .child(clip.text.clone())
                                    .into_any_element(),
                            })
                            .when(clip.image.is_some(), |d| {
                                d.child(div().flex_1().min_w_0().text_sm().text_color(theme::text_2()).truncate().child(clip.text.clone()))
                            })
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_none()
                                    .items_end()
                                    .gap_1()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .when(clip.pinned, |d| d.child(div().text_color(theme::accent()).child("Pinned")))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .when_some(clip.app_path.as_deref().and_then(|p| this.icon(p, false)), |d, icon| {
                                                d.child(img(icon).w(px(14.)).h(px(14.)))
                                            })
                                            .child(clip.meta.clone()),
                                    ),
                            )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.scroll.clone())
        .flex_1()
        .into_any_element()
    }
}

impl Overlay {
    fn render_pane(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let pane = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .border_l_1()
            .border_color(theme::border())
            .bg(theme::panel());
        let (name, details, body): (Option<SharedString>, Vec<(&'static str, SharedString)>, Option<&Body>) = match self.mode {
            Mode::Search => match &self.preview {
                Some(p) if self.selected_path().as_deref() == Some(p.path.as_str()) => {
                    (Some(p.name.clone()), p.details.clone(), Some(&p.body))
                }
                _ => (None, Vec::new(), None),
            },
            Mode::Clipboard => match (&self.clip_preview, self.clips.get(self.selected)) {
                (Some((id, body)), Some(clip)) if *id == clip.id => {
                    (None, vec![("From", clip.meta.clone())], Some(body))
                }
                _ => (None, Vec::new(), None),
            },
            Mode::Settings => (None, Vec::new(), None),
        };
        let Some(body) = body else {
            return pane.into_any_element();
        };
        let text = match body {
            Body::Text { full, .. } => Some(full.clone()),
            _ => None,
        };
        let wrap = self.wrap;
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_2()
                .py_0p5()
                .rounded_md()
                .text_xs()
                .text_color(theme::text_2())
                .bg(theme::panel_2())
                .cursor_pointer()
                .hover(|s| s.text_color(theme::text()))
                .child(label)
        };
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(36.))
            .flex_none()
            .border_b_1()
            .border_color(theme::border())
            .child(div().flex_1().min_w_0().text_sm().truncate().child(name.unwrap_or_else(|| "Preview".into())))
            .when(text.is_some(), |d| {
                d.child(
                    button("wrap", if wrap { "No wrap" } else { "Wrap" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.wrap = !this.wrap;
                            cx.notify();
                        })),
                )
            })
            .when_some(text, |d, text| {
                d.child(button("copy-text", "Copy text").on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()))))
            });
        let content = match body {
            Body::Image(path) => div()
                .flex()
                .flex_1()
                .min_h_0()
                .p_3()
                .child(img(path.clone()).id("pane-img").size_full().object_fit(ObjectFit::Contain))
                .into_any_element(),
            Body::Media(_) => self.render_player(cx),
            Body::Note(note) => div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .px_6()
                .text_sm()
                .text_color(theme::muted())
                .child(note.clone())
                .into_any_element(),
            Body::Text { lines, code, hit, truncated, .. } => {
                let (code, hit) = (*code, *hit);
                div()
                    .id("pane-text")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .py_2()
                    .overflow_scroll()
                    .track_scroll(&self.preview_scroll)
                    .text_xs()
                    .line_height(px(18.))
                    .when(code, |d| d.font_family("Cascadia Mono"))
                    .children(lines.iter().enumerate().map(|(ix, line)| {
                        div()
                            .flex()
                            .flex_none()
                            .px_2()
                            .when(Some(ix) == hit, |d| d.bg(theme::accent_soft()))
                            .when(code, |d| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .w(px(44.))
                                        .pr_3()
                                        .text_right()
                                        .text_color(theme::muted())
                                        .child((ix + 1).to_string()),
                                )
                            })
                            .child(
                                div()
                                    .text_color(theme::text_2())
                                    .when(wrap, |d| d.flex_1().min_w_0())
                                    .when(!wrap, |d| d.whitespace_nowrap())
                                    .child(if line.is_empty() { SharedString::from(" ") } else { line.clone() }),
                            )
                    }))
                    .when(*truncated, |d| {
                        d.child(div().px_3().py_2().text_color(theme::muted()).child("… the rest isn't shown here."))
                    })
                    .into_any_element()
            }
        };
        pane.child(header)
            .child(content)
            .when(!details.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .gap_0p5()
                        .px_3()
                        .py_2()
                        .border_t_1()
                        .border_color(theme::border())
                        .text_xs()
                        .children(details.into_iter().map(|(label, value)| {
                            div()
                                .flex()
                                .gap_2()
                                .child(div().w(px(64.)).flex_none().text_color(theme::muted()).child(label))
                                .child(div().flex_1().min_w_0().truncate().text_color(theme::text_2()).child(value))
                        })),
                )
            })
            .into_any_element()
    }
}

impl Overlay {
    /// Ten steps; click one to set the volume (Alt+Up / Alt+Down too).
    fn render_volume(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let level = if self.muted { 0 } else { (self.volume * 10.).round() as usize };
        div()
            .id("volume")
            .flex()
            .flex_none()
            .items_end()
            .gap(px(2.))
            .h(px(18.))
            .children((1..=10usize).map(move |step| {
                div()
                    .id(("vol", step))
                    .w(px(5.))
                    .h(px(4. + step as f32 * 1.4))
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(if step <= level { theme::accent() } else { theme::border() })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.set_volume(step as f32 / 10., cx)),
                    )
            }))
            .child(div().ml_1().w(px(32.)).text_xs().text_color(theme::muted()).child(format!("{}%", level * 10)))
    }

    fn render_player(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(player) = self.player() else {
            return div().flex_1().into_any_element();
        };
        let (position, duration, paused) = (player.position(), player.duration(), player.paused());
        let picture = if let Some(error) = player.error() {
            div().text_sm().text_color(theme::muted()).child(format!("Can't play this: {error}")).into_any_element()
        } else if player.is_video() {
            match self.drawn_frame.clone() {
                Some(frame) => img(frame).size_full().object_fit(ObjectFit::Contain).into_any_element(),
                None => div().text_sm().text_color(theme::muted()).child("Loading…").into_any_element(),
            }
        } else {
            let music = self.preview.as_ref().and_then(|p| p.music.as_ref());
            let name = self.preview.as_ref().map(|p| p.name.clone()).unwrap_or_default();
            let cover = match music.and_then(|m| m.cover.clone()) {
                Some(cover) => div()
                    .size(px(180.))
                    .flex_none()
                    .rounded_lg()
                    .overflow_hidden()
                    .child(img(cover).size_full().object_fit(ObjectFit::Cover))
                    .into_any_element(),
                None => div()
                    .size(px(180.))
                    .flex_none()
                    .rounded_lg()
                    .bg(theme::panel_2())
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(64.))
                    .text_color(theme::muted())
                    .child("♪")
                    .into_any_element(),
            };
            div()
                .flex()
                .items_center()
                .gap_5()
                .px_6()
                .max_w_full()
                .child(cover)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_lg()
                                .text_color(theme::text())
                                .line_clamp(2)
                                .child(music.and_then(|m| m.title.clone()).unwrap_or(name)),
                        )
                        .when_some(music.and_then(|m| m.artist.clone()), |d, a| {
                            d.child(div().text_sm().text_color(theme::text_2()).truncate().child(a))
                        })
                        .when_some(music.and_then(|m| m.album.clone()), |d, a| {
                            d.child(div().text_sm().text_color(theme::muted()).truncate().child(a))
                        }),
                )
                .into_any_element()
        };
        let button = |id: &'static str, label: String| {
            div()
                .id(id)
                .px_2()
                .py_1()
                .rounded_md()
                .text_sm()
                .text_color(theme::text())
                .bg(theme::panel_2())
                .cursor_pointer()
                .hover(|s| s.bg(theme::border()))
                .child(label)
        };
        // A row of thin cells: clicking one jumps there.
        const CELLS: usize = 80;
        let done = if duration > 0 { (position as f64 / duration as f64 * CELLS as f64) as usize } else { 0 };
        let bar = div().flex().flex_1().h(px(14.)).items_center().children((0..CELLS).map(|i| {
            div()
                .id(("seek", i))
                .flex_1()
                .h(px(14.))
                .flex()
                .items_center()
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        if let Some(p) = this.player() {
                            p.seek(media::hns_from_fraction(i as f32 / CELLS as f32, p.duration()));
                            cx.notify();
                        }
                    }),
                )
                .child(div().w_full().h(px(4.)).bg(if i < done { theme::accent() } else { theme::border() }))
        }));
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .items_center()
                    .justify_center()
                    .when(player.is_video(), |d| d.bg(gpui::black()))
                    .child(picture),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h(px(40.))
                    .flex_none()
                    .child(button("play", if paused { "▶".into() } else { "❚❚".into() }).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(p) = this.player() {
                            p.toggle();
                            cx.notify();
                        }
                    })))
                    .child(button("back", "−10".into()).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(p) = this.player() {
                            p.skip(-10);
                            cx.notify();
                        }
                    })))
                    .child(button("fwd", "+10".into()).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(p) = this.player() {
                            p.skip(10);
                            cx.notify();
                        }
                    })))
                    .child(div().text_xs().text_color(theme::text_2()).child(media::clock(position)))
                    .child(bar)
                    .child(div().text_xs().text_color(theme::muted()).child(media::clock(duration)))
                    .child(
                        button("mute", if self.muted { "Muted".into() } else { "Sound".into() }).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.muted = !this.muted;
                                this.apply_volume(cx);
                            },
                        )),
                    )
                    .child(self.render_volume(cx)),
            )
            .into_any_element()
    }
}

impl Render for Overlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = self.player.as_ref().and_then(|(_, p)| p.frame());
        if frame.as_ref().map(|f| f.id) != self.drawn_frame.as_ref().map(|f| f.id) {
            if let Some(old) = std::mem::replace(&mut self.drawn_frame, frame) {
                let _ = window.drop_image(old);
            }
        }
        let has_query = !self.query(cx).is_empty();
        div()
            .key_context("Overlay")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_next))
            .on_action(cx.listener(Self::on_prev))
            .on_action(cx.listener(Self::on_page_down))
            .on_action(cx.listener(Self::on_page_up))
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(Self::on_reveal))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_dismiss))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_search_mode))
            .on_action(cx.listener(Self::on_clipboard_mode))
            .on_action(cx.listener(Self::on_toggle_pin))
            .on_action(cx.listener(Self::on_delete_entry))
            .on_action(cx.listener(Self::on_toggle_wrap))
            .on_action(cx.listener(Self::on_play_pause))
            .on_action(cx.listener(Self::on_back))
            .on_action(cx.listener(Self::on_forward))
            .on_action(cx.listener(Self::on_mute))
            .on_action(cx.listener(Self::on_volume_up))
            .on_action(cx.listener(Self::on_volume_down))
            .on_action(cx.listener(Self::on_open_settings))
            .on_action(cx.listener(Self::on_select_all_rows))
            .on_action(cx.listener(Self::on_extend_next))
            .on_action(cx.listener(Self::on_extend_prev))
            .on_action(cx.listener(Self::on_rename))
            .on_action(cx.listener(Self::on_delete_selected))
            .on_action(cx.listener(Self::on_delete_for_good))
            .on_action(cx.listener(Self::on_move_selected))
            .on_key_down(cx.listener(Self::on_list_key_down))
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::bg())
            .border_1()
            .border_color(theme::border())
            .text_color(theme::text())
            .font_family("Segoe UI")
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .pt_3()
                    .child(self.render_mode_chip(Mode::Search, "Search", cx))
                    .child(self.render_mode_chip(Mode::Clipboard, "Clipboard", cx))
                    .child(div().flex_1())
                    .child(self.render_mode_chip(Mode::Settings, "Settings", cx)),
            )
            .when(self.mode != Mode::Settings, |d| d.child(
                div()
                    .flex()
                    .items_center()
                    .mx_3()
                    .mt_2()
                    .px_3()
                    .h(px(42.))
                    .rounded_lg()
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .text_size(px(16.))
                    .line_height(px(22.))
                    .child(self.input.clone()),
            ))
            .when(self.mode == Mode::Search && has_query, |d| {
                d.child(
                    div()
                        .flex()
                        .gap_2()
                        .px_3()
                        .mt_2()
                        .border_b_1()
                        .border_color(theme::border())
                        .items_center()
                        .child(self.render_tab(Tab::Files, "Files", self.table.total, cx))
                        .child(self.render_tab(Tab::Inside, "Inside", self.inside_total, cx))
                        .child(div().flex_1())
                        .when(self.tab == Tab::Files, |d| d.child(self.render_best_match(cx))),
                )
            })
            .when(self.mode == Mode::Settings, |d| {
                d.child(div().flex().flex_1().min_h_0().mt_2().border_t_1().border_color(theme::border()).child(self.settings.clone()))
            })
            .when(self.mode != Mode::Settings, |d| d.child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    // The Files table has more columns, so more room.
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w(px(if self.files_tab() { 660. } else { 470. }))
                            .flex_none()
                            .min_h_0()
                            .pt_2()
                            .child(self.render_body(cx)),
                    )
                    .child(self.render_pane(cx)),
            ))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .px_3()
                    .h(px(30.))
                    .border_t_1()
                    .border_color(theme::border())
                    .text_xs()
                    .text_color(theme::muted())
                    .gap_4()
                    .child(if self.files_tab() && self.table_footer_busy() {
                        self.render_table_footer(cx)
                    } else if self.mode == Mode::Search && self.player().is_some() {
                        "↵ Open   Alt+K Play/Pause   Alt+J/L −10/+10 s   Alt+↑/↓ Volume   Alt+M Mute   Esc Close".into_any_element()
                    } else if self.files_tab() {
                        self.render_table_footer(cx)
                    } else if self.mode == Mode::Search {
                        "↵ Open   Ctrl+↵ Show in folder   Ctrl+Shift+C Copy path   Tab Files/Inside   Alt+W Wrap   Esc Close".into_any_element()
                    } else if self.mode == Mode::Clipboard {
                        "↵ Paste   Ctrl+↵ Copy   Alt+P Pin   Alt+D Delete   Esc Close".into_any_element()
                    } else {
                        "Ctrl+1 Search   Ctrl+2 Clipboard   Esc Close".into_any_element()
                    })
                    // Files: "12 of 4,512 · 3 selected · 4 ms" beside the index status.
                    .when(self.files_tab(), |d| {
                        let summary = self.table.summary();
                        let text = if summary.is_empty() { self.status.to_string() } else { format!("{summary}   {}", self.status) };
                        d.child(div().flex_none().child(text))
                    })
                    .when(self.mode == Mode::Search && !self.files_tab(), |d| d.child(self.status.clone()))
                    .when(self.mode == Mode::Clipboard, |d| d.child(format!("{} items", self.clips.len()))),
            )
    }
}

fn age(modified_ms: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let days = now.saturating_sub(modified_ms) / 86_400_000;
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        d if d < 60 => format!("{d} days ago"),
        d if d < 730 => format!("{} months ago", d / 30),
        d => format!("{} years ago", d / 365),
    }
}

/// Which cached icon a path uses: apps, shortcuts and icon files have their
/// own; folders share one; other files share one per extension.
fn icon_key(path: &str, folder: bool) -> (String, &'static str) {
    if folder {
        return ("folder".into(), "folder");
    }
    let ext = std::path::Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "exe" | "lnk" | "ico" | "url" | "appref-ms" => (path.to_lowercase(), "app"),
        _ => (format!("ext:{ext}"), "file"),
    }
}

/// Up to four letters of the extension, for rows without a shell icon yet.
fn ext_badge(path: &str, folder: bool) -> String {
    if folder {
        return "DIR".into();
    }
    let ext = std::path::Path::new(path).extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
    ext.chars().take(4).collect()
}

fn inside_row(r: dash_search::search::ContentSearchResultItem) -> Row {
    Row {
        name: r.file_name.into(),
        path: r.path.into(),
        meta: format!("{} · {}", human_size(r.size), age(r.modified_ms)).into(),
        snippet: Some(strip_tags(&r.snippet).into()),
        folder: false,
    }
}

fn clip(e: ClipboardEntry) -> Clip {
    let when = age(e.captured_at_ms.max(0) as u64);
    let meta = match e.source_app.as_deref() {
        Some(app) => format!("{} · {when}", app.trim_end_matches(".exe")),
        None => when,
    };
    let (text, image) = match e.kind {
        EntryKind::Image => {
            let format = e.image_format.clone().unwrap_or_else(|| "image".into());
            let dims = match (e.image_width, e.image_height) {
                (Some(w), Some(h)) => format!(" · {w}×{h}"),
                _ => String::new(),
            };
            let size = e.image_size_bytes.map(|b| format!(" · {}", human_size(b))).unwrap_or_default();
            let shown = if format == "gif" { e.image_path.clone() } else { e.thumbnail_path.clone().or(e.image_path.clone()) };
            (format!("{}{dims}{size}", format.to_uppercase()), shown)
        }
        EntryKind::Text => {
            let one_line: String = e.text.split_whitespace().collect::<Vec<_>>().join(" ");
            let short: String = one_line.chars().take(300).collect();
            (e.pin_label.clone().map(|l| format!("{l} — {short}")).unwrap_or(short), None)
        }
    };
    Clip { id: e.id, text: text.into(), image, meta: meta.into(), pinned: e.is_pinned, full_text: e.text.into(), full_image: e.image_path, app_path: e.source_app_path }
}

/// Snippets come as HTML with `<b>` around matches; plain text for now.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(if c == '\n' { ' ' } else { c }),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'")
}

fn reveal(path: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(format!("/select,{path}")).spawn();
    }
    // Finder selects the file; Linux file managers just open its folder.
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = std::path::Path::new(path).parent().map(open::that);
    }
}

// ── Showing and hiding ─────────────────────────────────────────────────

struct DashWindow {
    overlay: Option<Entity<Overlay>>,
    window: Option<WindowHandle<Overlay>>,
}

impl Global for DashWindow {}

pub fn init(cx: &mut App) {
    bind_keys(cx);
    cx.set_global(DashWindow { overlay: None, window: None });
    keep_alive(cx);
}

/// Never shown. GPUI on Windows quits the app when its last window
/// closes, and hiding the overlay closes its window — this one keeps Dash
/// (tray, hotkeys, engines) running until "Quit Dash".
struct Keeper;

impl Render for Keeper {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn keep_alive(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(Point::default(), size(px(1.), px(1.))))),
        titlebar: None,
        focus: false,
        show: false,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        app_id: Some("Dash".into()),
        ..Default::default()
    };
    if let Err(e) = cx.open_window(options, |_, cx| cx.new(|_| Keeper)) {
        eprintln!("dash: can't open its hidden window: {e}");
    }
}

/// Shows the overlay in `mode` (or hides it when it's already showing
/// that mode).
pub fn summon(mode: Mode, cx: &mut App) {
    let state = cx.global::<DashWindow>();
    let overlay = state.overlay.clone();
    if let Some(handle) = state.window {
        let current = handle.update(cx, |view, window, cx| {
            if view.mode == mode && window.is_window_active() {
                view.hide(window, cx);
            } else {
                view.shown(mode, window, cx);
                window.activate_window();
            }
        });
        if current.is_ok() {
            return;
        }
    }
    // The overlay isn't up, so the foreground window is where a paste should go.
    dash_clipboard::remember_foreground_window();
    let overlay = overlay.unwrap_or_else(|| {
        let entity = cx.new(Overlay::new);
        cx.global_mut::<DashWindow>().overlay = Some(entity.clone());
        entity
    });
    let window_size = size(px(1120.), px(620.));
    let mut bounds = Bounds::centered(None, window_size, cx);
    if let Some(display) = cx.primary_display() {
        let area = display.bounds();
        bounds.origin = Point::new(bounds.origin.x, area.origin.y + area.size.height * 0.12);
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Opaque,
        app_id: Some("Dash".into()),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        overlay.update(cx, |view, cx| {
            view.shown(mode, window, cx);
            // Clicking elsewhere hides it.
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() && !settings::PICKING.load(std::sync::atomic::Ordering::SeqCst) {
                    this.hide(window, cx);
                }
            })
            .detach();
        });
        overlay.clone()
    });
    if let Ok(handle) = opened {
        cx.global_mut::<DashWindow>().window = Some(handle);
        cx.activate(true);
        // A new window isn't always brought to the front (another app had it).
        let _ = handle.update(cx, |_, window, _| window.activate_window());
    }
}
