//! The Files tab: a table over the name search (`dash_search::names`) —
//! Name / Path / Size / Modified / Type, sorted by a header click, every
//! match reachable (pages of rows fetched as they come into view), several
//! rows selectable, and file actions on the selection: open, show in
//! folder, copy paths, rename (F2), delete (Delete: Recycle Bin; Shift+Delete:
//! for good — both ask first), move (Ctrl+M) and dragging out to other apps.
//!
//! Keys that also edit text (Ctrl+A, Delete) act on rows when the list has
//! focus (after clicking a row) or when the search box is empty; typing in
//! the list goes back to the search box.

use super::*;

use dash_search::entry::{NameQueryOptions, NameRow, NameSearchResult, SortKey};
use gpui::{anchored, deferred, KeyDownEvent, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, PathPromptOptions};

use crate::file_ops;
use crate::files_table::{self as ft, PageCache, Selection, PAGE};

/// Opening more at once is more likely a slip than a wish.
const MAX_OPEN: usize = 20;
/// The most rows a file action takes at once.
const MAX_ACT: usize = 5000;
/// Table rows; Compact view's are shorter.
const ROW_HEIGHT: f32 = 30.;
const COMPACT_ROW_HEIGHT: f32 = 23.;
/// List view: name over its folder.
const LIST_ROW_HEIGHT: f32 = 48.;
/// How far the mouse moves with the button down before a drag starts.
const DRAG_START: f32 = 5.;
/// The index's watcher needs a moment to see a change before a refresh.
const REFRESH_AFTER: Duration = Duration::from_millis(1500);

/// The row being renamed, in an inline box.
pub(super) struct Rename {
    ix: usize,
    path: String,
    input: Entity<TextInput>,
}

/// A delete waiting for a second Delete (or Enter).
pub(super) enum Confirm {
    Recycle(Vec<(usize, NameRow)>),
    Forever(Vec<(usize, NameRow)>),
}

/// The left button went down on a row: where (a drag starts from here), and
/// whether releasing it without a drag selects just that row (a plain click
/// on a row of a bigger selection keeps the selection so it can be dragged).
pub(super) struct Press {
    ix: usize,
    at: Point<Pixels>,
    collapse: bool,
}

#[derive(Clone, Copy)]
enum MenuItem {
    Open,
    Reveal,
    CopyPath,
    Rename,
    Move,
    Recycle,
    Forever,
}

/// The table's state: loaded rows, order, selection and what's in progress.
#[derive(Default)]
pub(super) struct Table {
    cache: PageCache,
    pub(super) total: usize,
    took_ms: u64,
    sort: SortKey,
    descending: bool,
    sel: Selection,
    /// Bumped when the query or order changes, so older pages are dropped.
    generation: u64,
    /// The query the loaded rows answer.
    query: String,
    rename: Option<Rename>,
    confirm: Option<Confirm>,
    /// Footer message: what an action did, or why it couldn't.
    notice: Option<SharedString>,
    press: Option<Press>,
    /// The right-click menu, where it opened.
    menu: Option<Point<Pixels>>,
}

impl Table {
    fn options(&self, page: usize) -> NameQueryOptions {
        NameQueryOptions {
            query: self.query.clone(),
            sort: self.sort,
            descending: self.descending,
            offset: page * PAGE,
            limit: PAGE,
        }
    }

    pub(super) fn focus_ix(&self) -> usize {
        self.sel.focus()
    }

    pub(super) fn focus_path(&self) -> Option<String> {
        self.cache.row(self.sel.focus()).map(|r| r.path.clone())
    }

    /// Footer: "4,512 items · 3 selected · 4 ms".
    pub(super) fn summary(&self) -> String {
        if self.total == 0 {
            return String::new();
        }
        let mut text = plural(self.total);
        if self.sel.count() > 1 {
            text.push_str(&format!(" · {} selected", thousands(self.sel.count())));
        }
        text.push_str(&format!(" · {} ms", self.took_ms));
        text
    }
}

pub(super) use crate::preview::thousands;

/// One page of a name search, on the engine runtime.
async fn fetch(options: NameQueryOptions) -> Result<NameSearchResult, String> {
    let host = crate::host::host();
    crate::host::runtime()
        .spawn(dash_search::names::search_names(host, options))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r)
}

pub(super) fn plural(n: usize) -> String {
    if n == 1 {
        "1 item".into()
    } else {
        format!("{} items", thousands(n))
    }
}

/// Footer after a delete of `total` items, `gone` of which are gone now.
/// Without certainty that all went to the Recycle Bin (the shell deletes
/// for good, after asking, what the bin can't take), it doesn't claim so.
fn delete_notice(forever: bool, result: &Result<(), String>, gone: usize, total: usize) -> String {
    let left = total.saturating_sub(gone);
    if gone == 0 {
        return match result {
            Err(e) => format!("Couldn't delete: {e}"),
            Ok(()) => "Nothing was deleted.".into(),
        };
    }
    if left > 0 {
        return format!("Deleted {}; {} not deleted.", plural(gone), plural(left));
    }
    if forever {
        format!("Deleted {}.", plural(gone))
    } else {
        format!("Deleted {} (to the Recycle Bin where possible).", plural(gone))
    }
}

impl Overlay {
    pub(super) fn files_tab(&self) -> bool {
        self.mode == Mode::Search && self.tab == Tab::Files
    }

    /// A new query: rows load after the typing pause (`query_changed`).
    pub(super) fn table_query(&mut self, query: &str) -> NameQueryOptions {
        self.table.generation += 1;
        self.table.query = query.to_string();
        self.table.rename = None;
        self.table.confirm = None;
        self.table.notice = None;
        self.table.menu = None;
        self.table.options(0)
    }

    pub(super) fn table_generation(&self) -> u64 {
        self.table.generation
    }

    /// Loads the first page again (same query): after a change of order,
    /// on showing the overlay, and after files changed.
    pub(super) fn reload_table(&mut self, keep_focus: bool, cx: &mut Context<Self>) {
        self.table.generation += 1;
        let generation = self.table.generation;
        let options = self.table.options(0);
        cx.spawn(async move |this, cx| {
            let result = fetch(options).await;
            let _ = this.update(cx, |this, cx| {
                if this.table.generation == generation {
                    this.apply_first_page(result, keep_focus, cx);
                }
            });
        })
        .detach();
    }

    /// The first page of a search arrived: it replaces the table.
    pub(super) fn apply_first_page(&mut self, result: Result<NameSearchResult, String>, keep_focus: bool, cx: &mut Context<Self>) {
        let focus = self.table.sel.focus();
        self.table.cache.clear();
        match result {
            Ok(r) => {
                self.error = r.error;
                self.table.total = r.total;
                self.table.took_ms = r.took_ms;
                let icons: Vec<(String, bool)> = r.rows.iter().map(|row| (row.path.clone(), row.is_dir)).collect();
                self.table.cache.insert(0, r.rows);
                self.request_icons(icons, cx);
            }
            Err(e) => {
                self.error = Some(e);
                self.table.total = 0;
            }
        }
        if keep_focus {
            self.table.sel.clamp_to(focus, self.table.total);
        } else {
            self.table.sel.reset(self.table.total);
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        if self.mode == Mode::Search {
            self.update_preview(cx);
        }
        cx.notify();
    }

    /// Fetches the pages rows in view need (called while drawing them).
    fn ensure_rows(&mut self, range: std::ops::Range<usize>, cx: &mut Context<Self>) {
        self.table.cache.evict_far(range.start);
        for page in self.table.cache.need(range, self.table.total) {
            let generation = self.table.generation;
            let options = self.table.options(page);
            cx.spawn(async move |this, cx| {
                let result = fetch(options).await;
                let _ = this.update(cx, |this, cx| {
                    if this.table.generation != generation {
                        return;
                    }
                    match result {
                        Ok(r) => {
                            let icons: Vec<(String, bool)> = r.rows.iter().map(|row| (row.path.clone(), row.is_dir)).collect();
                            this.table.cache.insert(page, r.rows);
                            this.request_icons(icons, cx);
                            if this.files_tab() && this.table.sel.focus() / PAGE == page {
                                this.update_preview(cx);
                            }
                        }
                        Err(e) => {
                            this.table.cache.failed(page);
                            this.table.notice = Some(e.into());
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// After the selection or focus moved.
    fn selection_changed(&mut self, cx: &mut Context<Self>) {
        self.table.confirm = None;
        self.table.notice = None;
        self.table.menu = None;
        self.scroll_to_index(self.table.sel.focus(), cx);
        self.update_preview(cx);
        cx.notify();
    }

    /// Arrow keys (and Page Up / Down); `extend` with Shift.
    pub(super) fn table_move(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        // Up / Down don't leave the rename box (Enter or Esc does).
        if self.table.rename.is_some() {
            return;
        }
        self.table.sel.move_by(delta, extend, self.table.total);
        self.selection_changed(cx);
    }

    fn set_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        let (sort, descending) = ft::toggle_sort(self.table.sort, self.table.descending, key);
        self.table.sort = sort;
        self.table.descending = descending;
        self.reload_table(false, cx);
        cx.notify();
    }

    /// Runs `then` on the selected rows, fetching any not loaded yet.
    fn with_selection(&mut self, cx: &mut Context<Self>, then: impl FnOnce(&mut Self, Vec<(usize, NameRow)>, &mut Context<Self>) + 'static) {
        let count = self.table.sel.count();
        if count == 0 {
            return;
        }
        if count > MAX_ACT {
            self.table.notice = Some(format!("{} selected; select at most {} at a time.", thousands(count), thousands(MAX_ACT)).into());
            cx.notify();
            return;
        }
        if let Some(rows) = self.table.cache.rows_at(self.table.sel.indices()) {
            then(self, rows, cx);
            return;
        }
        // Rows were just taken out: fetching by position now could pick up
        // other files than the ones shown. Wait for the refresh.
        if self.table.cache.stale() {
            self.table.notice = Some("Refreshing…".into());
            cx.notify();
            return;
        }
        let missing: Vec<usize> = self.table.sel.indices().filter(|ix| self.table.cache.row(*ix).is_none()).collect();
        let pages = ft::pages_of(missing.into_iter());
        let generation = self.table.generation;
        let options: Vec<(usize, NameQueryOptions)> = pages.into_iter().map(|p| (p, self.table.options(p))).collect();
        cx.spawn(async move |this, cx| {
            let mut fetched = Vec::new();
            for (page, options) in options {
                fetched.push((page, fetch(options).await));
            }
            let _ = this.update(cx, |this, cx| {
                if this.table.generation != generation {
                    return;
                }
                for (page, result) in fetched {
                    if let Ok(r) = result {
                        this.table.cache.insert(page, r.rows);
                    }
                }
                match this.table.cache.rows_at(this.table.sel.indices()) {
                    Some(rows) => then(this, rows, cx),
                    None => {
                        this.table.notice = Some("Couldn't load all the selected rows.".into());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    // ── Mouse ──────────────────────────────────────────────────────────

    fn row_mouse_down(&mut self, ix: usize, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Clicks inside the rename box are the box's.
        if self.table.rename.as_ref().is_some_and(|r| r.ix == ix) {
            return;
        }
        window.focus(&self.focus);
        let m = event.modifiers;
        let collapse = !m.control && !m.shift && self.table.sel.contains(ix) && self.table.sel.count() > 1;
        if !collapse {
            self.table.sel.click(ix, m.control, m.shift);
        }
        self.table.press = Some(Press { ix, at: event.position, collapse });
        self.table.rename = None;
        self.table.confirm = None;
        self.table.notice = None;
        self.table.menu = None;
        self.update_preview(cx);
        cx.notify();
        if event.click_count >= 2 && !m.control && !m.shift {
            self.table.press = None;
            self.on_open(&OpenSelected, window, cx);
        }
    }

    fn row_right_click(&mut self, ix: usize, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        if !self.table.sel.contains(ix) {
            self.table.sel.select_only(ix);
            self.update_preview(cx);
        }
        self.table.rename = None;
        self.table.confirm = None;
        self.table.menu = Some(event.position);
        cx.notify();
    }

    fn table_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(press) = self.table.press.take() {
            if press.collapse {
                self.table.sel.select_only(press.ix);
                self.update_preview(cx);
                cx.notify();
            }
        }
    }

    fn table_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(press) = &self.table.press else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            self.table.press = None;
            return;
        }
        let (dx, dy) = (f32::from(event.position.x - press.at.x), f32::from(event.position.y - press.at.y));
        if dx * dx + dy * dy < DRAG_START * DRAG_START {
            return;
        }
        self.table.press = None;
        self.start_drag(cx);
    }

    /// Drags the selected files out (Windows: the shell's drag loop). It
    /// runs from a task, outside this event, because it pumps messages
    /// until the drop.
    fn start_drag(&mut self, cx: &mut Context<Self>) {
        let count = self.table.sel.count();
        if count == 0 || count > MAX_ACT {
            return;
        }
        let Some(rows) = self.table.cache.rows_at(self.table.sel.indices()) else { return };
        let paths: Vec<String> = rows.into_iter().map(|(_, r)| r.path).collect();
        cx.spawn(async move |this, cx| {
            // Keep the overlay up while the pointer is over another window.
            settings::PICKING.store(true, std::sync::atomic::Ordering::SeqCst);
            let result = file_ops::drag_out(&paths);
            settings::PICKING.store(false, std::sync::atomic::Ordering::SeqCst);
            let _ = this.update(cx, |this, cx| {
                match result {
                    // A move into Explorer takes the files away: catch up.
                    Ok(()) => this.schedule_refresh(cx),
                    Err(e) => this.table.notice = Some(e.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ── Keys ───────────────────────────────────────────────────────────

    /// Typing while the list has focus goes to the search box.
    pub(super) fn on_list_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Search || !self.focus.is_focused(window) {
            return;
        }
        let m = event.keystroke.modifiers;
        if m.control || m.alt || m.platform {
            return;
        }
        let Some(typed) = event.keystroke.key_char.clone().filter(|c| !c.is_empty() && !c.chars().any(char::is_control)) else {
            return;
        };
        let text = format!("{}{}", self.input.read(cx).text(), typed);
        self.input.update(cx, |input, cx| input.set_text(&text, cx));
        window.focus(&self.input.focus_handle(cx));
        self.query_changed(cx);
        cx.stop_propagation();
    }

    /// Whether a table action may run now (Files tab, nothing being renamed).
    fn table_ready(&self) -> bool {
        self.files_tab() && self.table.rename.is_none()
    }

    pub(super) fn on_select_all_rows(&mut self, _: &SelectAllRows, _: &mut Window, cx: &mut Context<Self>) {
        if !self.table_ready() {
            return;
        }
        self.table.sel.select_all(self.table.total);
        self.table.confirm = None;
        cx.notify();
    }

    pub(super) fn on_extend_next(&mut self, _: &ExtendNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.table_ready() {
            self.table_move(1, true, cx);
        }
    }

    pub(super) fn on_extend_prev(&mut self, _: &ExtendPrev, _: &mut Window, cx: &mut Context<Self>) {
        if self.table_ready() {
            self.table_move(-1, true, cx);
        }
    }

    /// Drops a delete question, rename box or menu left open (the overlay
    /// is hiding: Enter after it comes back mustn't run them).
    pub(super) fn table_forget_pending(&mut self) {
        self.table.confirm = None;
        self.table.rename = None;
        self.table.menu = None;
    }

    /// Takes the player away when it has one of `targets` (or a file inside
    /// one) open, so renaming, moving or deleting it isn't refused as "in
    /// use". The caller closes it off the UI thread before the file
    /// operation; the pane loads again afterwards.
    fn release_player<'a>(&mut self, targets: impl IntoIterator<Item = &'a str>) -> Option<Player> {
        let (playing, _) = self.player.as_ref()?;
        if !targets.into_iter().any(|t| ft::is_within(playing, t)) {
            return None;
        }
        self.preview = None;
        self.player.take().map(|(_, player)| player)
    }

    /// Enter / Esc while renaming or confirming. Returns whether it was used.
    pub(super) fn table_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.table.rename.is_some() {
            self.apply_rename(window, cx);
            return true;
        }
        if let Some(confirm) = self.table.confirm.take() {
            self.run_delete(confirm, cx);
            return true;
        }
        false
    }

    pub(super) fn table_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.table.menu.take().is_some() || self.table.confirm.take().is_some() {
            cx.notify();
            return true;
        }
        if self.table.rename.take().is_some() {
            window.focus(&self.focus);
            cx.notify();
            return true;
        }
        false
    }

    /// Enter: opens the selected rows (at most `MAX_OPEN`).
    pub(super) fn open_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.table.sel.count();
        if count > MAX_OPEN {
            self.table.notice = Some(format!("{} selected; open at most {MAX_OPEN} at once.", thousands(count)).into());
            cx.notify();
            return;
        }
        let Some(rows) = self.table.cache.rows_at(self.table.sel.indices()) else { return };
        let mut failed = None;
        for (_, row) in rows {
            if let Err(e) = dash_search::search::open_search_result_path(crate::host::host(), row.path) {
                failed = Some(e);
            }
        }
        match failed {
            None => self.hide(window, cx),
            Some(e) => {
                self.error = None;
                self.table.notice = Some(e.into());
                cx.notify();
            }
        }
    }

    /// Ctrl+Enter: each item's folder opens with it selected.
    pub(super) fn reveal_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rows) = self.table.cache.rows_at(self.table.sel.indices().take(MAX_ACT)) else { return };
        let paths: Vec<String> = rows.into_iter().map(|(_, r)| r.path).collect();
        if paths.is_empty() {
            return;
        }
        #[cfg(windows)]
        let result = if paths.len() == 1 {
            reveal(&paths[0]);
            Ok(())
        } else {
            file_ops::reveal_many(&paths)
        };
        #[cfg(not(windows))]
        let result: Result<(), String> = {
            for (_, items) in file_ops::group_by_folder(&paths).into_iter().take(10) {
                reveal(&items[0]);
            }
            Ok(())
        };
        match result {
            Ok(()) => self.hide(window, cx),
            Err(e) => {
                self.table.notice = Some(e.into());
                cx.notify();
            }
        }
    }

    /// Ctrl+Shift+C: the selected paths, one per line.
    pub(super) fn copy_file_paths(&mut self, cx: &mut Context<Self>) {
        self.with_selection(cx, |this, rows, cx| {
            let text = rows.iter().map(|(_, r)| r.path.as_str()).collect::<Vec<_>>().join("\r\n");
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            this.table.notice = Some(if rows.len() == 1 { "Path copied.".into() } else { format!("{} paths copied.", rows.len()).into() });
            cx.notify();
        });
    }

    // ── Rename ─────────────────────────────────────────────────────────

    pub(super) fn on_rename(&mut self, _: &RenameSelected, window: &mut Window, cx: &mut Context<Self>) {
        if !self.table_ready() {
            return;
        }
        let ix = self.table.sel.focus();
        let Some(row) = self.table.cache.row(ix).cloned() else { return };
        let input = cx.new(|cx| {
            let mut input = TextInput::new("New name", cx);
            input.set_text(&row.name, cx);
            input.select_everything(cx);
            input
        });
        window.focus(&input.focus_handle(cx));
        self.table.sel.select_only(ix);
        self.table.confirm = None;
        self.table.menu = None;
        self.table.notice = None;
        self.table.rename = Some(Rename { ix, path: row.path, input });
        cx.notify();
    }

    fn apply_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Rename { ix, path, input }) = self.table.rename.take() else { return };
        let new_name = input.read(cx).text().to_string();
        window.focus(&self.focus);
        let player = self.release_player([path.as_str()]);
        cx.spawn(async move |this, cx| {
            let (from, name) = (path.clone(), new_name.clone());
            // Off the UI thread: closing a player waits for its threads.
            let result = crate::host::runtime()
                .spawn_blocking(move || {
                    if let Some(player) = player {
                        player.close();
                    }
                    file_ops::rename_in_place(std::path::Path::new(&from), &name)
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(new_path) => {
                        let new_path = new_path.display().to_string();
                        if let Some(row) = this.table.cache.row_mut(ix) {
                            if row.path == path {
                                row.name = new_name.trim().to_string();
                                row.extension = if row.is_dir {
                                    String::new()
                                } else {
                                    std::path::Path::new(&row.name)
                                        .extension()
                                        .map(|e| e.to_string_lossy().to_lowercase())
                                        .unwrap_or_default()
                                };
                                row.path = new_path.clone();
                            }
                        }
                        let is_dir = this.table.cache.row(ix).is_some_and(|r| r.is_dir);
                        this.request_icons(vec![(new_path, is_dir)], cx);
                        this.schedule_refresh(cx);
                    }
                    Err(e) => this.table.notice = Some(format!("Couldn't rename: {e}").into()),
                }
                this.update_preview(cx);
                cx.notify();
            });
        })
        .detach();
    }

    // ── Delete ─────────────────────────────────────────────────────────

    pub(super) fn on_delete_selected(&mut self, _: &DeleteSelected, _: &mut Window, cx: &mut Context<Self>) {
        self.ask_delete(false, cx);
    }

    pub(super) fn on_delete_for_good(&mut self, _: &DeleteForGood, _: &mut Window, cx: &mut Context<Self>) {
        self.ask_delete(true, cx);
    }

    /// First press asks (footer); the same key again does it.
    fn ask_delete(&mut self, forever: bool, cx: &mut Context<Self>) {
        if !self.table_ready() {
            return;
        }
        match self.table.confirm.take() {
            Some(Confirm::Recycle(rows)) if !forever => return self.run_delete(Confirm::Recycle(rows), cx),
            Some(Confirm::Forever(rows)) if forever => return self.run_delete(Confirm::Forever(rows), cx),
            _ => {}
        }
        self.table.menu = None;
        self.with_selection(cx, move |this, rows, cx| {
            this.table.notice = None;
            this.table.confirm = Some(if forever { Confirm::Forever(rows) } else { Confirm::Recycle(rows) });
            cx.notify();
        });
    }

    fn run_delete(&mut self, confirm: Confirm, cx: &mut Context<Self>) {
        let (forever, rows) = match confirm {
            Confirm::Recycle(rows) => (false, rows),
            Confirm::Forever(rows) => (true, rows),
        };
        self.table.notice = Some(if forever { "Deleting…".into() } else { "Moving to the Recycle Bin…".into() });
        cx.notify();
        // Rows are patched locally only if the table is still the one the
        // positions came from.
        let generation = self.table.generation;
        let player = self.release_player(rows.iter().map(|(_, r)| r.path.as_str()));
        cx.spawn(async move |this, cx| {
            let paths: Vec<String> = rows.iter().map(|(_, r)| r.path.clone()).collect();
            let result = crate::host::runtime()
                .spawn_blocking(move || {
                    if let Some(player) = player {
                        player.close();
                    }
                    if forever {
                        let mut first_error = None;
                        for p in &paths {
                            if let Err(e) = file_ops::delete_permanently(std::path::Path::new(p)) {
                                first_error.get_or_insert(e);
                            }
                        }
                        first_error.map_or(Ok(()), Err)
                    } else {
                        // The shell may ask before deleting something for
                        // good: keep the overlay up meanwhile.
                        settings::PICKING.store(true, std::sync::atomic::Ordering::SeqCst);
                        let result = file_ops::recycle(&paths);
                        settings::PICKING.store(false, std::sync::atomic::Ordering::SeqCst);
                        result
                    }
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r);
            if !forever {
                // Back from the shell's question (if any).
                let window = cx.update(|cx| cx.try_global::<DashWindow>().and_then(|d| d.window)).ok().flatten();
                if let Some(handle) = window {
                    let _ = handle.update(cx, |_, window, _| window.activate_window());
                }
            }
            let gone: Vec<(usize, String)> = rows
                .iter()
                .filter(|(_, r)| std::fs::symlink_metadata(&r.path).is_err())
                .map(|(ix, r)| (*ix, r.path.clone()))
                .collect();
            let _ = this.update(cx, |this, cx| {
                this.table.notice = Some(delete_notice(forever, &result, gone.len(), rows.len()).into());
                if this.table.generation == generation {
                    this.rows_removed(&gone, cx);
                } else {
                    this.schedule_refresh(cx);
                }
                this.update_preview(cx);
                cx.notify();
            });
        })
        .detach();
    }

    // ── Move ───────────────────────────────────────────────────────────

    pub(super) fn on_move_selected(&mut self, _: &MoveSelected, window: &mut Window, cx: &mut Context<Self>) {
        if !self.table_ready() {
            return;
        }
        self.table.menu = None;
        self.table.confirm = None;
        let handle = window.window_handle();
        self.with_selection(cx, move |_, rows, cx| {
            settings::PICKING.store(true, std::sync::atomic::Ordering::SeqCst);
            let picked = cx.prompt_for_paths(PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some("Move here".into()),
            });
            cx.spawn(async move |this, cx| {
                let folder = picked.await.ok().and_then(|r| r.ok()).flatten().and_then(|p| p.into_iter().next());
                let _ = handle.update(cx, |_, window, _| window.activate_window());
                settings::PICKING.store(false, std::sync::atomic::Ordering::SeqCst);
                let Some(folder) = folder else { return };
                let shown = folder.display().to_string();
                let moving = rows.clone();
                // Rows are patched locally only if the table is still the
                // one the positions came from.
                let Ok((generation, player)) = this.update(cx, |this, _| {
                    (this.table.generation, this.release_player(rows.iter().map(|(_, r)| r.path.as_str())))
                }) else {
                    return;
                };
                let errors = crate::host::runtime()
                    .spawn_blocking(move || {
                        if let Some(player) = player {
                            player.close();
                        }
                        let mut errors = Vec::new();
                        for (_, row) in &moving {
                            if let Err(e) = file_ops::move_into(std::path::Path::new(&row.path), &folder) {
                                errors.push(format!("{}: {e}", row.name));
                            }
                        }
                        errors
                    })
                    .await
                    .unwrap_or_else(|e| vec![e.to_string()]);
                let gone: Vec<(usize, String)> = rows
                    .iter()
                    .filter(|(_, r)| std::fs::symlink_metadata(&r.path).is_err())
                    .map(|(ix, r)| (*ix, r.path.clone()))
                    .collect();
                let _ = this.update(cx, |this, cx| {
                    this.table.notice = Some(match errors.first() {
                        None => format!("Moved {} to {shown}.", plural(gone.len())).into(),
                        Some(e) => format!("Moved {}; couldn't move {}", plural(gone.len()), e).into(),
                    });
                    if this.table.generation == generation {
                        this.rows_removed(&gone, cx);
                    } else {
                        this.schedule_refresh(cx);
                    }
                    this.update_preview(cx);
                    cx.notify();
                });
            })
            .detach();
        });
    }

    /// Rows that are gone (deleted, moved away) leave the table at once;
    /// a refresh follows once the index has seen it. `gone` is (position,
    /// path); a row is taken out only if that position still shows that path.
    fn rows_removed(&mut self, gone: &[(usize, String)], cx: &mut Context<Self>) {
        let gone: Vec<usize> = gone
            .iter()
            .filter(|(ix, path)| self.table.cache.row(*ix).is_some_and(|r| r.path == *path))
            .map(|(ix, _)| *ix)
            .collect();
        if !gone.is_empty() {
            // Pages in flight were asked for at the old positions: drop them.
            // Until the next first page nothing is fetched by position
            // (`PageCache::stale`).
            self.table.generation += 1;
            let first = gone.iter().copied().min().unwrap_or(0);
            self.table.cache.remove_rows(&gone);
            self.table.total = self.table.total.saturating_sub(gone.len());
            self.table.sel.clamp_to(first, self.table.total);
            self.update_preview(cx);
        }
        self.schedule_refresh(cx);
        cx.notify();
    }

    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        let generation = self.table.generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_AFTER).await;
            let _ = this.update(cx, |this, cx| {
                // Not if the query or order changed meanwhile (that reloads anyway).
                if this.table.generation == generation {
                    this.reload_table(true, cx);
                }
            });
        })
        .detach();
    }

    fn run_menu(&mut self, item: MenuItem, window: &mut Window, cx: &mut Context<Self>) {
        self.table.menu = None;
        match item {
            MenuItem::Open => self.on_open(&OpenSelected, window, cx),
            MenuItem::Reveal => self.on_reveal(&RevealSelected, window, cx),
            MenuItem::CopyPath => self.copy_file_paths(cx),
            MenuItem::Rename => self.on_rename(&RenameSelected, window, cx),
            MenuItem::Move => self.on_move_selected(&MoveSelected, window, cx),
            MenuItem::Recycle => self.ask_delete(false, cx),
            MenuItem::Forever => self.ask_delete(true, cx),
        }
        cx.notify();
    }

    // ── Drawing ────────────────────────────────────────────────────────

    pub(super) fn render_table(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let view = self.search_view;
        let header = match view {
            SearchView::Table | SearchView::Compact => self.render_header(cx).into_any_element(),
            SearchView::List | SearchView::Grid => self.render_sort_bar(cx),
        };
        let menu = self.table.menu.map(|at| self.render_menu(at, cx));
        let focus = self.table.sel.focus();
        let total = self.table.total;
        let list = match view {
            SearchView::Grid => {
                let cols = self.grid_cols();
                uniform_list(
                    "files-grid",
                    total.div_ceil(cols),
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        this.ensure_rows(range.start * cols..(range.end * cols).min(total), cx);
                        range.map(|r| this.render_tile_row(r, cols, focus, cx)).collect::<Vec<_>>()
                    }),
                )
            }
            _ => uniform_list(
                "files-table",
                total,
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    this.ensure_rows(range.clone(), cx);
                    range
                        .map(|ix| match view {
                            SearchView::List => this.render_list_row(ix, ix == focus, cx),
                            _ => this.render_row(ix, ix == focus, cx),
                        })
                        .collect::<Vec<_>>()
                }),
            ),
        }
        .track_scroll(self.scroll.clone())
        .flex_1()
        .px_2()
        .into_any_element();
        let list = self.with_list_scrollbar(list);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .on_mouse_move(cx.listener(Self::table_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::table_mouse_up))
            .child(header)
            .child(list)
            .children(menu)
            .into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (sort, descending) = (self.table.sort, self.table.descending);
        let column = |key: SortKey, label: &'static str| {
            let sorted = sort == key;
            div()
                .id(label)
                .flex()
                .items_center()
                .gap_0p5()
                .min_w_0()
                .cursor_pointer()
                .when(sorted, |d| d.text_color(theme::text_2()))
                .hover(|s| s.text_color(theme::text()))
                .on_click(cx.listener(move |this, _, _, cx| this.set_sort(key, cx)))
                .child(div().one_line().child(label))
                .when(sorted, |d| d.child(icon(if descending { Icon::Down } else { Icon::Up }, 12., theme::text_2())))
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_3()
            .h(px(if self.search_view == SearchView::Compact { 24. } else { 28. }))
            .px_3()
            .mx_2()
            .text_xs()
            .text_color(theme::muted())
            .child(column(SortKey::Name, "Name").flex_1().pl(px(26.)))
            .child(column(SortKey::Path, "Folder").flex_1())
            .child(column(SortKey::Size, "Size").w(px(60.)).flex_none().justify_end())
            .child(column(SortKey::Modified, "Modified").w(px(116.)).flex_none())
            .child(column(SortKey::Type, "Type").w(px(48.)).flex_none())
    }

    fn render_row(&mut self, ix: usize, focused: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let compact = self.search_view == SearchView::Compact;
        let row_height = if compact { COMPACT_ROW_HEIGHT } else { ROW_HEIGHT };
        let Some(row) = self.table.cache.row(ix).cloned() else {
            return div()
                .id(ix)
                .flex()
                .items_center()
                .w_full()
                .h(px(row_height))
                .pl(px(42.))
                .text_xs()
                .text_color(theme::muted())
                .child("…")
                .into_any_element();
        };
        let selected = self.table.sel.contains(ix);
        let renaming = self.table.rename.as_ref().filter(|r| r.ix == ix).map(|r| r.input.clone());
        let icon = match self.icon(&row.path, row.is_dir) {
            Some(icon) => div()
                .flex_none()
                .w(px(18.))
                .h(px(18.))
                .flex()
                .items_center()
                .justify_center()
                .child(img(icon).w(px(18.)).h(px(18.)))
                .into_any_element(),
            None => div()
                .flex_none()
                .w(px(18.))
                .h(px(18.))
                .rounded_sm()
                .bg(theme::panel_2())
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(7.))
                .text_color(theme::text_2())
                .child(ext_badge(&row.path, row.is_dir))
                .into_any_element(),
        };
        let name = match renaming {
            Some(input) => rename_box(input, true),
            None => div().flex_1().min_w_0().one_line().text_color(theme::text()).child(row.name.clone()).into_any_element(),
        };
        let cell = || div().flex_none().text_xs().text_color(theme::muted()).one_line();
        // With several selected, a faint ring marks the focused (previewed) one.
        let ring: gpui::Hsla =
            if focused && self.table.sel.count() > 1 { theme::border().into() } else { gpui::transparent_black() };
        div()
            .id(ix)
            .flex()
            .items_center()
            .gap_3()
            .w_full()
            .h(px(row_height))
            .px_3()
            .rounded_md()
            .when(compact, |d| d.text_xs())
            .when(!compact, |d| d.text_sm())
            .cursor_pointer()
            .border_1()
            .border_color(ring)
            .when(selected, |d| d.bg(theme::selected()))
            .when(!selected, |d| d.hover(|s| s.bg(theme::hover())))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| this.row_mouse_down(ix, event, window, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| this.row_right_click(ix, event, window, cx)),
            )
            .child(div().flex().flex_1().min_w_0().items_center().gap_2().child(icon).child(name))
            .child(div().flex_1().min_w_0().one_line().text_xs().text_color(theme::muted()).child(ft::short_folder(ft::folder_of(&row.path), 30)))
            .child(cell().w(px(60.)).text_right().child(if row.is_dir { String::new() } else { human_size(row.size) }))
            .child(cell().w(px(116.)).child(ft::modified_text(row.modified_ms)))
            .child(cell().w(px(48.)).child(ft::type_text(&row)))
            .into_any_element()
    }

    /// The file's shell icon at `size`, or its extension in a box until it loads.
    fn file_icon(&self, row: &NameRow, size: f32) -> gpui::AnyElement {
        match self.icon(&row.path, row.is_dir) {
            Some(icon) => img(icon).size(px(size)).flex_none().into_any_element(),
            None => div()
                .flex_none()
                .size(px(size))
                .rounded_sm()
                .bg(theme::panel_2())
                .flex()
                .items_center()
                .justify_center()
                .text_size(px((size * 0.38).max(7.)))
                .text_color(theme::text_2())
                .child(ext_badge(&row.path, row.is_dir))
                .into_any_element(),
        }
    }

    /// Mouse handlers every view's item shares (select, open, drag, menu).
    fn item_mouse(&self, item: gpui::Stateful<gpui::Div>, ix: usize, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        item.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| this.row_mouse_down(ix, event, window, cx)),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| this.row_right_click(ix, event, window, cx)),
        )
    }

    /// List view: icon, name over the full folder, size and date.
    fn render_list_row(&mut self, ix: usize, focused: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(row) = self.table.cache.row(ix).cloned() else {
            return div().id(ix).w_full().h(px(LIST_ROW_HEIGHT)).into_any_element();
        };
        let selected = self.table.sel.contains(ix);
        let renaming = self.table.rename.as_ref().filter(|r| r.ix == ix).map(|r| r.input.clone());
        let ring: gpui::Hsla =
            if focused && self.table.sel.count() > 1 { theme::border().into() } else { gpui::transparent_black() };
        let meta = if row.is_dir {
            ft::modified_text(row.modified_ms)
        } else {
            format!("{} · {}", human_size(row.size), ft::modified_text(row.modified_ms))
        };
        let item = div()
            .id(ix)
            .flex()
            .items_center()
            .gap_3()
            .w_full()
            .h(px(LIST_ROW_HEIGHT))
            .px_3()
            .rounded_lg()
            .cursor_pointer()
            .border_1()
            .border_color(ring)
            .when(selected, |d| d.bg(theme::selected()))
            .when(!selected, |d| d.hover(|s| s.bg(theme::hover())))
            .child(self.file_icon(&row, 28.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(match renaming {
                        Some(input) => rename_box(input, false),
                        None => div().text_sm().text_color(theme::text()).one_line().child(row.name.clone()).into_any_element(),
                    })
                    .child(div().text_xs().text_color(theme::muted()).one_line().child(ft::folder_of(&row.path).to_string())),
            )
            .child(div().flex_none().text_xs().text_color(theme::muted()).child(meta));
        self.item_mouse(item, ix, cx).into_any_element()
    }

    /// Grid view: one row of tiles (`cols` of them), pictures as thumbnails.
    fn render_tile_row(&mut self, r: usize, cols: usize, focus: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let total = self.table.total;
        let tiles: Vec<gpui::AnyElement> = (r * cols..((r + 1) * cols).min(total))
            .map(|ix| {
                let Some(row) = self.table.cache.row(ix).cloned() else {
                    return div().id(ix).w(px(views::TILE_W)).h(px(views::TILE_H)).into_any_element();
                };
                let selected = self.table.sel.contains(ix);
                let focused = ix == focus && self.table.sel.count() > 1;
                let renaming = self.table.rename.as_ref().filter(|r| r.ix == ix).map(|r| r.input.clone());
                let picture = match self.thumb(&row.path, cx) {
                    Some(thumb) => div()
                        .size(px(84.))
                        .rounded_md()
                        .overflow_hidden()
                        .bg(theme::panel_2())
                        .child(img(thumb).size_full().object_fit(ObjectFit::Cover))
                        .into_any_element(),
                    None => div().size(px(84.)).flex().items_center().justify_center().child(self.file_icon(&row, 52.)).into_any_element(),
                };
                let item = div()
                    .id(ix)
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1p5()
                    .w(px(views::TILE_W))
                    .h(px(views::TILE_H))
                    .pt_2()
                    .px_1p5()
                    .rounded_lg()
                    .cursor_pointer()
                    .border_1()
                    .border_color(if focused { theme::border() } else { gpui::transparent_black().into() })
                    .when(selected, |d| d.bg(theme::selected()))
                    .when(!selected, |d| d.hover(|s| s.bg(theme::hover())))
                    .child(picture)
                    .child(match renaming {
                        Some(input) => rename_box(input, false),
                        None => div()
                            .w_full()
                            .text_xs()
                            .line_height(px(15.))
                            .text_center()
                            .text_color(theme::text())
                            .line_clamp(2)
                            // Two short lines' worth, cut in the middle.
                            .child(ft::short_name(&row.name, 26))
                            .into_any_element(),
                    });
                self.item_mouse(item, ix, cx).into_any_element()
            })
            .collect();
        div()
            .id(("tile-row", r))
            .flex()
            .gap(px(views::GRID_GAP))
            .h(px(views::TILE_H + views::GRID_GAP))
            .children(tiles)
            .into_any_element()
    }

    /// List and Grid have no column headers: the sort order picks from here.
    fn render_sort_bar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (sort, descending) = (self.table.sort, self.table.descending);
        let keys = [
            (SortKey::Relevance, "Best match"),
            (SortKey::Name, "Name"),
            (SortKey::Path, "Folder"),
            (SortKey::Size, "Size"),
            (SortKey::Modified, "Modified"),
            (SortKey::Type, "Type"),
        ];
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .h(px(32.))
            .px_4()
            .text_xs()
            .text_color(theme::muted())
            .child(div().mr_1().child("Sort"))
            .children(keys.into_iter().map(|(key, label)| {
                let active = sort == key;
                div()
                    .id(label)
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .h(px(22.))
                    .px_2()
                    .rounded_md()
                    .cursor_pointer()
                    .when(active, |d| d.bg(theme::selected()).text_color(theme::text()))
                    .when(!active, |d| d.hover(|s| s.bg(theme::hover()).text_color(theme::text_2())))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_sort(key, cx)))
                    .child(label)
                    .when(active && key != SortKey::Relevance, |d| {
                        d.child(icon(if descending { Icon::Down } else { Icon::Up }, 11., theme::text_2()))
                    })
            }))
            .into_any_element()
    }

    fn render_menu(&self, at: Point<Pixels>, cx: &mut Context<Self>) -> gpui::AnyElement {
        let many = self.table.sel.count() > 1;
        let items: [(MenuItem, &'static str, &'static str); 7] = [
            (MenuItem::Open, "Open", "Enter"),
            (MenuItem::Reveal, "Show in folder", "Ctrl+Enter"),
            (MenuItem::CopyPath, if many { "Copy paths" } else { "Copy path" }, "Ctrl+Shift+C"),
            (MenuItem::Rename, "Rename", "F2"),
            (MenuItem::Move, "Move to…", "Ctrl+M"),
            (MenuItem::Recycle, "Delete", "Del"),
            (MenuItem::Forever, "Delete for good", "Shift+Del"),
        ];
        deferred(
            anchored().position(at).snap_to_window().child(
                div()
                    .id("row-menu")
                    .occlude()
                    .flex()
                    .flex_col()
                    .py_1()
                    .min_w(px(210.))
                    .rounded_lg()
                    .bg(theme::menu())
                    .border_1()
                    .border_color(theme::border())
                    .shadow_md()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.table.menu = None;
                        cx.notify();
                    }))
                    .children(items.into_iter().filter(|(item, ..)| !(many && matches!(item, MenuItem::Rename))).map(
                        |(item, label, keys)| {
                            div()
                                .id(label)
                                .flex()
                                .justify_between()
                                .gap_4()
                                .px_3()
                                .py_1()
                                .text_sm()
                                .text_color(theme::text())
                                .cursor_pointer()
                                .hover(|s| s.bg(theme::border()))
                                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| this.run_menu(item, window, cx)))
                                .child(label)
                                .child(div().text_xs().text_color(theme::muted()).child(keys))
                        },
                    )),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// "Sorted by best match" / a way back to it, beside the tabs.
    pub(super) fn render_best_match(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.table.sort == SortKey::Relevance;
        div()
            .id("best-match")
            .px_2()
            .py_1()
            .text_xs()
            .when(active, |d| d.text_color(theme::muted()).child("Sorted by best match"))
            .when(!active, |d| {
                d.text_color(theme::text_2())
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme::text()))
                    .on_click(cx.listener(|this, _, _, cx| this.set_sort(SortKey::Relevance, cx)))
                    .child("Best match")
            })
    }

    /// Whether the footer has something of the table's to show (ahead of
    /// the player's keys).
    pub(super) fn table_footer_busy(&self) -> bool {
        self.table.confirm.is_some() || self.table.notice.is_some() || self.table.rename.is_some()
    }

    /// The footer's left side on the Files tab: a delete to confirm, what
    /// the last action did, or the keys.
    pub(super) fn render_table_footer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some(confirm) = &self.table.confirm {
            let (question, key) = match confirm {
                Confirm::Recycle(rows) => (format!("Move {} to the Recycle Bin?", plural(rows.len())), "Del"),
                Confirm::Forever(rows) => (format!("Delete {} for good? This can't be undone.", plural(rows.len())), "Shift+Del"),
            };
            let button = |id: &'static str, label: String| {
                div()
                    .id(id)
                    .px_2()
                    .rounded_md()
                    .bg(theme::panel_2())
                    .text_color(theme::text())
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::border()))
                    .child(label)
            };
            return div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap_2()
                .text_color(theme::text())
                .child(question)
                .child(button("confirm-delete", format!("Delete ({key} / ↵)")).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(confirm) = this.table.confirm.take() {
                        this.run_delete(confirm, cx);
                    }
                })))
                .child(button("cancel-delete", "Cancel (Esc)".into()).on_click(cx.listener(|this, _, _, cx| {
                    this.table.confirm = None;
                    cx.notify();
                })))
                .into_any_element();
        }
        if let Some(notice) = &self.table.notice {
            return div().flex_1().min_w_0().text_color(theme::text_2()).one_line().child(notice.clone()).into_any_element();
        }
        // Renaming: the keys are on the right.
        div().flex_1().into_any_element()
    }

    pub(super) fn table_renaming(&self) -> bool {
        self.table.rename.is_some()
    }

    pub(super) fn table_confirming(&self) -> bool {
        self.table.confirm.is_some()
    }
}

/// The inline rename box, in any view (`grow`: fill a row's width).
fn rename_box(input: Entity<TextInput>, grow: bool) -> gpui::AnyElement {
    div()
        .flex()
        .flex_none()
        .when(grow, |d| d.flex_1())
        .w_full()
        .min_w_0()
        .h(px(22.))
        .items_center()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(theme::accent())
        .bg(theme::bg())
        .text_sm()
        .child(input)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{delete_notice, plural, thousands};

    #[test]
    fn delete_notice_says_what_happened() {
        let ok = Ok(());
        let stopped = Err("Some items weren't deleted".to_string());
        assert_eq!(delete_notice(true, &ok, 2, 2), "Deleted 2 items.");
        // Never a plain "moved to the Recycle Bin": the shell may have
        // deleted for good (after asking) what the bin couldn't take.
        assert!(!delete_notice(false, &ok, 2, 2).starts_with("Moved"));
        assert_eq!(delete_notice(false, &stopped, 1, 3), "Deleted 1 item; 2 items not deleted.");
        assert_eq!(delete_notice(false, &stopped, 0, 3), "Couldn't delete: Some items weren't deleted");
        assert_eq!(delete_notice(false, &ok, 0, 1), "Nothing was deleted.");
    }

    #[test]
    fn counts_read_well() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(plural(1), "1 item");
        assert_eq!(plural(2500), "2,500 items");
    }
}
