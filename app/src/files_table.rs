//! The Files tab's results table without the drawing: which rows are
//! selected, which pages of rows are loaded, and how a header click changes
//! the order. Kept free of GPUI so it can be unit-tested.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use dash_search::entry::{NameRow, SortKey};

/// Rows fetched per request.
pub const PAGE: usize = 200;

/// Pages kept on each side of the one in view; farther ones are dropped so
/// scrolling through millions of rows doesn't keep them all.
const KEEP_PAGES: usize = 25;

/// Which rows are selected: sorted, disjoint, non-adjacent ranges (so
/// selecting all of a million rows is one range), plus the anchor a
/// Shift-click extends from and the focused row (the one previewed).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Selection {
    ranges: Vec<Range<usize>>,
    anchor: usize,
    focus: usize,
}

impl Selection {
    pub fn focus(&self) -> usize {
        self.focus
    }

    #[cfg(test)]
    pub fn ranges(&self) -> &[Range<usize>] {
        &self.ranges
    }

    pub fn count(&self) -> usize {
        self.ranges.iter().map(|r| r.len()).sum()
    }

    pub fn contains(&self, ix: usize) -> bool {
        self.ranges.iter().any(|r| r.contains(&ix))
    }

    /// Every selected row, in order.
    pub fn indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.ranges.iter().flat_map(|r| r.clone())
    }

    /// A plain click (or arrow key): just this row.
    pub fn select_only(&mut self, ix: usize) {
        self.ranges = vec![ix..ix + 1];
        self.anchor = ix;
        self.focus = ix;
    }

    /// Ctrl+click: flip this row, keep the rest.
    pub fn toggle(&mut self, ix: usize) {
        if self.contains(ix) {
            self.remove(ix);
        } else {
            self.insert(ix..ix + 1);
        }
        self.anchor = ix;
        self.focus = ix;
    }

    /// Shift+click / Shift+arrow: the rows from the anchor to here, only.
    pub fn extend_to(&mut self, ix: usize) {
        self.ranges = vec![self.anchor.min(ix)..self.anchor.max(ix) + 1];
        self.focus = ix;
    }

    /// Ctrl+Shift+click: add the rows from the anchor to here.
    pub fn add_range_to(&mut self, ix: usize) {
        self.insert(self.anchor.min(ix)..self.anchor.max(ix) + 1);
        self.focus = ix;
    }

    /// A click with its modifiers.
    pub fn click(&mut self, ix: usize, ctrl: bool, shift: bool) {
        match (ctrl, shift) {
            (false, false) => self.select_only(ix),
            (true, false) => self.toggle(ix),
            (false, true) => self.extend_to(ix),
            (true, true) => self.add_range_to(ix),
        }
    }

    /// Moves the focus by `delta` rows (clamped); `extend` keeps the anchor.
    pub fn move_by(&mut self, delta: isize, extend: bool, total: usize) {
        if total == 0 {
            return;
        }
        let next = (self.focus as isize + delta).clamp(0, total as isize - 1) as usize;
        if extend {
            self.extend_to(next);
        } else {
            self.select_only(next);
        }
    }

    pub fn select_all(&mut self, total: usize) {
        self.ranges = if total == 0 { Vec::new() } else { vec![0..total] };
    }

    /// Back to the first row (a new query or order).
    pub fn reset(&mut self, total: usize) {
        if total == 0 {
            *self = Selection::default();
        } else {
            self.select_only(0);
        }
    }

    /// After rows went away: the focus stays near where it was.
    pub fn clamp_to(&mut self, ix: usize, total: usize) {
        if total == 0 {
            *self = Selection::default();
        } else {
            self.select_only(ix.min(total - 1));
        }
    }

    fn insert(&mut self, new: Range<usize>) {
        let mut merged = new;
        let mut out = Vec::with_capacity(self.ranges.len() + 1);
        for r in self.ranges.drain(..) {
            if r.end < merged.start || r.start > merged.end {
                out.push(r);
            } else {
                merged = merged.start.min(r.start)..merged.end.max(r.end);
            }
        }
        out.push(merged);
        out.sort_by_key(|r| r.start);
        self.ranges = out;
    }

    fn remove(&mut self, ix: usize) {
        let mut out = Vec::with_capacity(self.ranges.len() + 1);
        for r in self.ranges.drain(..) {
            if !r.contains(&ix) {
                out.push(r);
                continue;
            }
            if r.start < ix {
                out.push(r.start..ix);
            }
            if ix + 1 < r.end {
                out.push(ix + 1..r.end);
            }
        }
        self.ranges = out;
    }
}

/// The rows loaded so far, by row number, and the pages being fetched.
#[derive(Debug, Default)]
pub struct PageCache {
    rows: HashMap<usize, NameRow>,
    pending: HashSet<usize>,
    /// Rows were taken out locally, so the index's positions no longer
    /// match ours until the next first page (`clear`): nothing may be
    /// fetched by position meanwhile.
    stale: bool,
}

impl PageCache {
    pub fn row(&self, ix: usize) -> Option<&NameRow> {
        self.rows.get(&ix)
    }

    pub fn row_mut(&mut self, ix: usize) -> Option<&mut NameRow> {
        self.rows.get_mut(&ix)
    }

    /// Empty, ready for a first page (positions are the index's again).
    pub fn clear(&mut self) {
        self.rows.clear();
        self.pending.clear();
        self.stale = false;
    }

    /// Whether positions may disagree with the index (see `stale`).
    pub fn stale(&self) -> bool {
        self.stale
    }

    /// Pages that rows in `range` need and nobody is fetching yet; they're
    /// marked as being fetched. None while positions are stale.
    pub fn need(&mut self, range: Range<usize>, total: usize) -> Vec<usize> {
        if self.stale {
            return Vec::new();
        }
        let mut pages = Vec::new();
        for ix in range.start..range.end.min(total) {
            if self.rows.contains_key(&ix) {
                continue;
            }
            let page = ix / PAGE;
            if self.pending.insert(page) {
                pages.push(page);
            }
        }
        pages
    }

    /// A fetched page arrived.
    pub fn insert(&mut self, page: usize, rows: Vec<NameRow>) {
        self.pending.remove(&page);
        let start = page * PAGE;
        for (i, row) in rows.into_iter().enumerate() {
            self.rows.insert(start + i, row);
        }
    }

    /// A fetch failed: let a later draw ask again.
    pub fn failed(&mut self, page: usize) {
        self.pending.remove(&page);
    }

    /// Drops rows far from `center` (a row number) to bound memory.
    pub fn evict_far(&mut self, center: usize) {
        if self.rows.len() <= KEEP_PAGES * 2 * PAGE {
            return;
        }
        let page = center / PAGE;
        let low = page.saturating_sub(KEEP_PAGES) * PAGE;
        let high = (page + KEEP_PAGES + 1) * PAGE;
        self.rows.retain(|ix, _| *ix >= low && *ix < high);
    }

    /// Takes these rows out (deleted or moved away) and closes the gaps, so
    /// the table updates before the index catches up. Positions are stale
    /// from then until the next `clear`.
    pub fn remove_rows(&mut self, removed: &[usize]) {
        if removed.is_empty() {
            return;
        }
        self.stale = true;
        let mut removed = removed.to_vec();
        removed.sort_unstable();
        removed.dedup();
        let rows = std::mem::take(&mut self.rows);
        for (ix, row) in rows {
            if removed.binary_search(&ix).is_ok() {
                continue;
            }
            let before = removed.partition_point(|r| *r < ix);
            self.rows.insert(ix - before, row);
        }
        // Pages in flight were asked for at the old positions.
        self.pending.clear();
    }

    /// Rows at these positions, or `None` while any isn't loaded.
    pub fn rows_at(&self, indices: impl Iterator<Item = usize>) -> Option<Vec<(usize, NameRow)>> {
        indices.map(|ix| self.rows.get(&ix).map(|r| (ix, r.clone()))).collect()
    }
}

/// The order after clicking a column header: the same column flips the
/// direction; a new one starts ascending, except Size and Modified (biggest
/// and newest first, like Explorer). "Best match" has no direction.
pub fn toggle_sort(current: SortKey, descending: bool, clicked: SortKey) -> (SortKey, bool) {
    if clicked == SortKey::Relevance {
        return (SortKey::Relevance, false);
    }
    if current == clicked {
        return (clicked, !descending);
    }
    (clicked, matches!(clicked, SortKey::Size | SortKey::Modified))
}

/// Pages that hold these rows (for fetching a selection before acting on it).
pub fn pages_of(indices: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut pages: Vec<usize> = indices.map(|ix| ix / PAGE).collect();
    pages.dedup();
    pages
}

/// The folder a row is in.
pub fn folder_of(path: &str) -> &str {
    match path.rfind(['\\', '/']) {
        Some(0) => &path[..1],
        // A drive's root keeps its separator (`C:\`, not `C:`).
        Some(i) if path[..i].ends_with(':') => &path[..=i],
        Some(i) => &path[..i],
        None => "",
    }
}

/// A file name of at most `max` characters, shortened in the middle so its
/// end (extension included) stays: `(2003) Двойной Фор…-poster.jpg`.
pub fn short_name(name: &str, max: usize) -> String {
    let count = name.chars().count();
    if count <= max {
        return name.to_string();
    }
    let tail = (max / 3).max(1);
    let head = max.saturating_sub(tail + 1);
    let start: String = name.chars().take(head).collect();
    let end: String = name.chars().skip(count - tail).collect();
    format!("{start}…{end}")
}

/// A folder path of at most `max` characters, shortened in the middle so
/// the drive and the last folders stay: `C:\…\templates\examples-main`.
pub fn short_folder(folder: &str, max: usize) -> String {
    if folder.chars().count() <= max {
        return folder.to_string();
    }
    let sep = if folder.contains('\\') { '\\' } else { '/' };
    let mut parts = folder.split(sep);
    let head = parts.next().unwrap_or_default();
    let rest: Vec<&str> = parts.filter(|p| !p.is_empty()).collect();
    let mut len = head.chars().count() + 2;
    let mut tail = Vec::new();
    for part in rest.iter().rev() {
        let add = part.chars().count() + 1;
        if len + add > max && !tail.is_empty() {
            break;
        }
        len += add;
        tail.push(*part);
    }
    tail.reverse();
    let mut tail = tail.join(&sep.to_string());
    // One folder name longer than the room: keep its end.
    let room = max.saturating_sub(head.chars().count() + 3).max(1);
    if tail.chars().count() > room {
        tail = tail.chars().skip(tail.chars().count() - room).collect();
    }
    format!("{head}{sep}…{sep}{tail}")
}

/// Whether `path` is `target` or inside it (a file in a folder being moved).
pub fn is_within(path: &str, target: &str) -> bool {
    match path.strip_prefix(target) {
        Some(rest) => rest.is_empty() || rest.starts_with(['\\', '/']) || target.ends_with(['\\', '/']),
        None => false,
    }
}

/// The Modified column: local date and time.
pub fn modified_text(ms: u64) -> String {
    if ms == 0 {
        return String::new();
    }
    match chrono::DateTime::from_timestamp_millis(ms as i64) {
        Some(t) => t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string(),
        None => String::new(),
    }
}

/// The Type column: "Folder", or the extension in capitals.
pub fn type_text(row: &NameRow) -> String {
    if row.is_dir {
        return "Folder".into();
    }
    row.extension.to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_keep_their_end() {
        assert_eq!(short_name("photo.jpg", 30), "photo.jpg");
        let short = short_name("(2003) Двойной Форсаж-movie-poster.jpg", 30);
        assert_eq!(short.chars().count(), 30);
        assert!(short.starts_with("(2003) Двойной") && short.ends_with("poster.jpg"));
    }

    #[test]
    fn long_folders_keep_drive_and_end() {
        assert_eq!(short_folder("C:\\Users\\me", 30), "C:\\Users\\me");
        assert_eq!(
            short_folder("C:\\Users\\user\\Projects\\OMNIS-X\\backup\\templates\\examples-main", 30),
            "C:\\…\\templates\\examples-main"
        );
        assert_eq!(short_folder("/home/me/a/very/deep/tree/of/folders", 20), "/…/tree/of/folders");
        // A single long folder name: its end.
        let short = short_folder("D:\\x\\an-extremely-long-folder-name-indeed", 20);
        assert_eq!(short.chars().count(), 20);
        assert!(short.starts_with("D:\\…\\") && short.ends_with("indeed"));
    }

    fn row(name: &str) -> NameRow {
        NameRow {
            path: format!("C:\\d\\{name}"),
            name: name.into(),
            is_dir: false,
            size: 1,
            modified_ms: 0,
            created_ms: 0,
            extension: "txt".into(),
        }
    }

    #[test]
    fn click_selects_only_that_row() {
        let mut s = Selection::default();
        s.select_all(10);
        s.click(4, false, false);
        assert_eq!(s.ranges(), &[4..5]);
        assert_eq!(s.focus(), 4);
        assert_eq!(s.count(), 1);
    }

    #[test]
    fn ctrl_click_toggles_and_merges() {
        let mut s = Selection::default();
        s.click(2, false, false);
        s.click(4, true, false);
        assert_eq!(s.ranges(), &[2..3, 4..5]);
        s.click(3, true, false);
        assert_eq!(s.ranges(), &[2..5]);
        s.click(3, true, false);
        assert_eq!(s.ranges(), &[2..3, 4..5]);
        s.click(2, true, false);
        s.click(4, true, false);
        assert!(s.ranges().is_empty());
        assert_eq!(s.count(), 0);
    }

    #[test]
    fn shift_click_extends_from_anchor_either_way() {
        let mut s = Selection::default();
        s.click(5, false, false);
        s.click(8, false, true);
        assert_eq!(s.ranges(), &[5..9]);
        assert_eq!(s.focus(), 8);
        // The anchor stays at 5.
        s.click(2, false, true);
        assert_eq!(s.ranges(), &[2..6]);
    }

    #[test]
    fn ctrl_shift_click_adds_a_range() {
        let mut s = Selection::default();
        s.click(1, false, false);
        s.click(10, true, false);
        s.click(12, true, true);
        assert_eq!(s.ranges(), &[1..2, 10..13]);
        assert_eq!(s.indices().collect::<Vec<_>>(), vec![1, 10, 11, 12]);
    }

    #[test]
    fn arrows_move_and_shift_arrows_extend() {
        let mut s = Selection::default();
        s.reset(5);
        s.move_by(1, false, 5);
        s.move_by(1, true, 5);
        s.move_by(1, true, 5);
        assert_eq!(s.ranges(), &[1..4]);
        s.move_by(-3, true, 5);
        assert_eq!(s.ranges(), &[0..2]);
        s.move_by(100, false, 5);
        assert_eq!(s.ranges(), &[4..5]);
        s.move_by(1, false, 0);
        assert_eq!(s.focus(), 4);
    }

    #[test]
    fn select_all_is_one_range() {
        let mut s = Selection::default();
        s.select_all(1_000_000);
        assert_eq!(s.ranges(), &[0..1_000_000]);
        assert_eq!(s.count(), 1_000_000);
        assert!(s.contains(999_999));
        s.select_all(0);
        assert_eq!(s.count(), 0);
    }

    #[test]
    fn clamp_after_removal() {
        let mut s = Selection::default();
        s.select_all(10);
        s.clamp_to(9, 5);
        assert_eq!(s.ranges(), &[4..5]);
        s.clamp_to(3, 0);
        assert_eq!(s, Selection::default());
    }

    #[test]
    fn cache_asks_for_each_missing_page_once() {
        let mut c = PageCache::default();
        assert_eq!(c.need(0..30, 1000), vec![0]);
        assert!(c.need(0..30, 1000).is_empty());
        assert_eq!(c.need(190..210, 1000), vec![1]);
        c.insert(0, (0..PAGE).map(|i| row(&i.to_string())).collect());
        assert_eq!(c.row(5).unwrap().name, "5");
        assert!(c.need(0..30, 1000).is_empty());
        // Past the end: nothing to ask for.
        assert!(c.need(1000..1100, 1000).is_empty());
        c.failed(1);
        assert_eq!(c.need(200..201, 1000), vec![1]);
        c.clear();
        assert!(c.row(5).is_none());
    }

    #[test]
    fn cache_removes_rows_and_closes_gaps() {
        let mut c = PageCache::default();
        c.insert(0, (0..6).map(|i| row(&i.to_string())).collect());
        c.remove_rows(&[4, 1, 1]);
        let names: Vec<_> = (0..4).map(|i| c.row(i).unwrap().name.clone()).collect();
        assert_eq!(names, vec!["0", "2", "3", "5"]);
        assert!(c.row(4).is_none());
    }

    #[test]
    fn after_removal_nothing_is_fetched_by_position_until_cleared() {
        let mut c = PageCache::default();
        c.insert(0, (0..6).map(|i| row(&i.to_string())).collect());
        assert!(!c.stale());
        c.remove_rows(&[]);
        assert!(!c.stale(), "nothing removed: positions still good");
        c.remove_rows(&[2]);
        assert!(c.stale());
        // Rows past the loaded ones would be asked for at shifted offsets.
        assert!(c.need(0..1000, 1000).is_empty());
        // Loaded rows still answer.
        assert_eq!(c.rows_at([0, 2].into_iter()).unwrap()[1].1.name, "3");
        // The next first page makes positions the index's again.
        c.clear();
        assert!(!c.stale());
        assert_eq!(c.need(0..10, 1000), vec![0]);
    }

    #[test]
    fn cache_evicts_far_pages() {
        let mut c = PageCache::default();
        for p in 0..60 {
            c.insert(p, (0..PAGE).map(|_| row("x")).collect());
        }
        c.evict_far(0);
        assert!(c.row(0).is_some());
        assert!(c.row(25 * PAGE).is_some());
        assert!(c.row(26 * PAGE).is_none());
    }

    #[test]
    fn rows_at_needs_every_row() {
        let mut c = PageCache::default();
        c.insert(0, (0..3).map(|i| row(&i.to_string())).collect());
        assert_eq!(c.rows_at([0, 2].into_iter()).unwrap().len(), 2);
        assert!(c.rows_at([0, 5].into_iter()).is_none());
        assert_eq!(pages_of([1, 2, 250, 260, 900].into_iter()), vec![0, 1, 4]);
    }

    #[test]
    fn header_clicks_toggle_direction() {
        use SortKey::*;
        assert_eq!(toggle_sort(Relevance, false, Name), (Name, false));
        assert_eq!(toggle_sort(Name, false, Name), (Name, true));
        assert_eq!(toggle_sort(Name, true, Name), (Name, false));
        assert_eq!(toggle_sort(Name, true, Size), (Size, true));
        assert_eq!(toggle_sort(Size, true, Size), (Size, false));
        assert_eq!(toggle_sort(Path, false, Modified), (Modified, true));
        assert_eq!(toggle_sort(Type, true, Relevance), (Relevance, false));
    }

    #[test]
    fn within_a_target() {
        assert!(is_within("C:\\a\\b.mp4", "C:\\a\\b.mp4"));
        assert!(is_within("C:\\a\\b.mp4", "C:\\a"));
        assert!(is_within("C:\\a\\b.mp4", "C:\\"));
        assert!(!is_within("C:\\ab\\b.mp4", "C:\\a"));
        assert!(!is_within("C:\\a", "C:\\a\\b.mp4"));
    }

    #[test]
    fn column_text() {
        assert_eq!(folder_of("C:\\a\\b.txt"), "C:\\a");
        assert_eq!(folder_of("/x"), "/");
        assert_eq!(folder_of("C:\\x.txt"), "C:\\");
        assert_eq!(folder_of("name"), "");
        let mut r = row("a.txt");
        assert_eq!(type_text(&r), "TXT");
        r.is_dir = true;
        assert_eq!(type_text(&r), "Folder");
        assert_eq!(modified_text(0), "");
        assert_eq!(modified_text(1_700_000_000_000).len(), 16);
    }
}
