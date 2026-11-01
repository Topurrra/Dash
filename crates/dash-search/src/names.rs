//! Name search: an in-memory table of every indexed file and folder name
//! (Everything / FSearch style), read from the filename index and scanned in
//! parallel on every keystroke. Each column has a cached sort order, so a
//! sorted page is a walk over that order keeping the ids that matched.
//!
//! Memory layout (see `Table`): one arena for all names, the distinct parent
//! folders as a tree (each stores its parent folder and only its own segment),
//! and a 40-byte `Item` per entry. Full paths aren't stored; a scan builds each
//! one into a per-thread buffer (reusing the folder prefix while consecutive
//! entries share it) so `Entry::path` stays a plain `&str`.
//!
//! Freshness: the table is built on a background thread on first use and
//! rebuilt when the filename index's contents change (segment fingerprint),
//! at most once every few seconds; the old table answers until the new one is
//! ready. With no table yet, the old filename search answers (≤200 hits).

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use dash_core::Host;
use rayon::prelude::*;

use crate::entry::{Entry, Matcher, NameQueryOptions, NameRow, NameSearchResult, SortKey};

/// Runs a name search (on the owner when this app follows; see `dash_core::link`).
/// The scan, a first sort and the follower round trip are seconds of blocking
/// work at worst, so they run on the blocking pool.
pub async fn search_names(app: Arc<dyn Host>, options: NameQueryOptions) -> Result<NameSearchResult, String> {
    tokio::task::spawn_blocking(move || search_names_blocking(app, options))
        .await
        .map_err(|error| format!("Name search worker failed: {error}"))?
}

fn search_names_blocking(app: Arc<dyn Host>, options: NameQueryOptions) -> Result<NameSearchResult, String> {
    if dash_core::link::is_follower() {
        if let Some(result) = dash_core::link::forward("search_names", serde_json::json!({ "options": options })) {
            return result;
        }
    }
    let started = Instant::now();
    let query = match crate::query::parse(&options.query) {
        Ok(q) => q,
        Err(e) => return Ok(NameSearchResult { error: Some(e), ..Default::default() }),
    };
    refresh_table(&app);
    let table = STATE.table.read().ok().and_then(|guard| guard.clone());
    let Some(table) = table else {
        return fallback_search(app, &query, &options, started);
    };
    let hide = hidden_bits(&table);
    let (total, ids) = table.search(&query, query.is_empty(), &options, hide.as_deref());
    let rows = ids.into_iter().map(|id| table.row(id)).collect();
    Ok(NameSearchResult { total, rows, took_ms: started.elapsed().as_millis() as u64, error: None })
}

// ---- The table ----

/// A distinct parent folder, as a tree: its prefix (path with the trailing
/// separator, `C:\Users\`) is its parent's prefix + its own segment (`Users\`),
/// so a full path is prefix + name and no long path string is stored.
#[derive(Clone, Copy)]
struct Folder {
    /// `NO_FOLDER` for a top-level prefix (`C:\`, `/`).
    parent: u32,
    seg_start: u32,
    seg_len: u32,
    /// Length of the parent as `Entry::parent` shows it (separator trimmed,
    /// except for roots like `C:\` and `/`).
    parent_len: u32,
    depth: u16,
}

const NO_FOLDER: u32 = u32::MAX;

/// Per-thread path buffer; remembers which folder's prefix it holds, since
/// consecutive entries usually share one.
struct PathScratch {
    buf: String,
    folder: u32,
    prefix_len: usize,
}

impl PathScratch {
    fn new() -> Self {
        PathScratch { buf: String::with_capacity(256), folder: NO_FOLDER, prefix_len: 0 }
    }
}

/// One file or folder (40 bytes).
#[derive(Clone, Copy)]
struct Item {
    name_start: u32,
    name_len: u16,
    is_dir: bool,
    folder: u32,
    /// Entries directly inside (folders only).
    child_count: u32,
    size: u64,
    modified_ms: u64,
    created_ms: u64,
}

/// Cached sort orders, by `order_slot`.
const ORDER_NAME: usize = 0;
const ORDER_PATH: usize = 1;
const ORDER_SIZE: usize = 2;
const ORDER_MODIFIED: usize = 3;
const ORDER_TYPE: usize = 4;
const ORDER_COUNT: usize = 5;

/// Every indexed name, plus lazily built sort orders. `TableBuilder` keeps
/// the item and folder counts within `u32`, so ids and positions cast to
/// `u32` below never wrap.
pub(crate) struct Table {
    names: String,
    /// Folder segments (`Folder::seg_start`).
    segments: String,
    folders: Vec<Folder>,
    items: Vec<Item>,
    /// The filename index contents this was read from.
    fingerprint: u64,
    /// Ids sorted ascending by each column (`ORDER_*`).
    orders: [OnceLock<Vec<u32>>; ORDER_COUNT],
    /// Each id's position in the name order (tie-break key for other sorts).
    name_rank: OnceLock<Vec<u32>>,
}

/// Builds a `Table` from rows (the filename index, or plain rows in tests).
///
/// Ids and arena offsets are `u32`: once a row would overflow one, the
/// builder stops taking rows (`push` returns false) instead of wrapping.
pub(crate) struct TableBuilder {
    names: String,
    segments: String,
    folders: Vec<Folder>,
    /// Full prefix → folder id (build time only).
    folder_ids: HashMap<Box<str>, u32>,
    items: Vec<Item>,
    /// Consecutive rows usually share a folder; skips the hash lookup.
    last_folder: Option<u32>,
    last_prefix: String,
    /// Largest id count / arena length allowed (`u32::MAX`; small in tests).
    limit: u32,
    /// The limit was reached; further rows are dropped.
    full: bool,
}

impl Default for TableBuilder {
    fn default() -> Self {
        TableBuilder {
            names: String::new(),
            segments: String::new(),
            folders: Vec::new(),
            folder_ids: HashMap::new(),
            items: Vec::new(),
            last_folder: None,
            last_prefix: String::new(),
            limit: u32::MAX,
            full: false,
        }
    }
}

fn is_sep(c: char) -> bool {
    c == '\\' || c == '/'
}

/// Splits a path into (prefix with trailing separator, name). A trailing
/// separator isn't a split point, so `C:\` stays whole as a name.
fn split_path(path: &str) -> (&str, &str) {
    let trimmed = path.trim_end_matches(is_sep);
    if trimmed.is_empty() {
        return ("", path);
    }
    match trimmed.rfind(is_sep) {
        Some(at) => (&path[..at + 1], &path[at + 1..]),
        None => ("", path),
    }
}

impl TableBuilder {
    /// Adds a row; false (and every later row dropped) once the table is full.
    pub(crate) fn push(&mut self, path: &str, is_dir: bool, size: u64, modified_ms: u64, created_ms: u64) -> bool {
        if self.full {
            return false;
        }
        let (prefix, name) = split_path(path);
        let name = &name[..floor_char_boundary(name, u16::MAX as usize)];
        // The new id is `items.len()`; ids run 0..len, so len + 1 must fit too.
        let fits = self.fit(self.items.len() + 1).is_some() && self.fit(self.names.len() + name.len()).is_some();
        let (Some(name_start), Some(folder), true) = (self.fit(self.names.len()), self.folder_id(prefix), fits) else {
            self.full = true;
            return false;
        };
        self.names.push_str(name);
        self.items.push(Item {
            name_start,
            name_len: name.len() as u16,
            is_dir,
            folder,
            child_count: 0,
            size: if is_dir { 0 } else { size },
            modified_ms,
            created_ms,
        });
        true
    }

    /// `n` as a `u32` when it is within the limit.
    fn fit(&self, n: usize) -> Option<u32> {
        u32::try_from(n).ok().filter(|&v| v <= self.limit)
    }

    fn folder_id(&mut self, prefix: &str) -> Option<u32> {
        if let Some(last) = self.last_folder {
            if self.last_prefix == prefix {
                return Some(last);
            }
        }
        let id = self.intern_folder(prefix)?;
        self.last_folder = Some(id);
        self.last_prefix.clear();
        self.last_prefix.push_str(prefix);
        Some(id)
    }

    /// The folder for `prefix`, adding it (and its missing ancestors);
    /// `None` when a folder id or segment offset would overflow.
    fn intern_folder(&mut self, prefix: &str) -> Option<u32> {
        if let Some(&id) = self.folder_ids.get(prefix) {
            return Some(id);
        }
        let (outer, segment) = split_path(prefix);
        let parent = if outer.is_empty() { NO_FOLDER } else { self.intern_folder(outer)? };
        let trimmed = prefix.strip_suffix(is_sep).unwrap_or(prefix);
        let parent_len = if trimmed.is_empty() || trimmed.ends_with(':') { prefix.len() } else { trimmed.len() };
        // `folders.len() + 1` fitting keeps every id below `NO_FOLDER`.
        self.fit(self.folders.len() + 1)?;
        self.fit(self.segments.len() + segment.len())?;
        let id = self.fit(self.folders.len())?;
        self.folders.push(Folder {
            parent,
            seg_start: self.fit(self.segments.len())?,
            seg_len: self.fit(segment.len())?,
            parent_len: self.fit(parent_len)?,
            depth: prefix.chars().filter(|&c| is_sep(c)).count().min(u16::MAX as usize) as u16,
        });
        self.segments.push_str(segment);
        self.folder_ids.insert(prefix.into(), id);
        Some(id)
    }

    /// Counts folder children and builds the name order (needed by relevance
    /// and as every other order's tie-break).
    pub(crate) fn finish(mut self, fingerprint: u64) -> Table {
        let folder_ids = std::mem::take(&mut self.folder_ids);
        self.names.shrink_to_fit();
        self.segments.shrink_to_fit();
        self.folders.shrink_to_fit();
        self.items.shrink_to_fit();
        let mut table = Table {
            names: self.names,
            segments: self.segments,
            folders: self.folders,
            items: self.items,
            fingerprint,
            orders: Default::default(),
            name_rank: OnceLock::new(),
        };
        let mut children = vec![0u32; table.folders.len()];
        for item in &table.items {
            children[item.folder as usize] += 1;
        }
        // A folder's children are the entries whose prefix is its path + separator.
        let mut key = String::new();
        for i in 0..table.items.len() {
            let item = table.items[i];
            if !item.is_dir {
                continue;
            }
            key.clear();
            table.push_prefix(item.folder, &mut key);
            let windows_style = key.contains('\\') || !key.contains('/');
            key.push_str(table.name(i));
            if !key.ends_with(is_sep) {
                key.push(if windows_style { '\\' } else { '/' });
            }
            if let Some(&child_folder) = folder_ids.get(key.as_str()) {
                table.items[i].child_count = children[child_folder as usize];
            }
        }
        drop(folder_ids);
        table.order(ORDER_NAME);
        table
    }
}

/// Largest index ≤ `max` on a char boundary of `s`.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if s.len() <= max {
        return s.len();
    }
    let mut at = max;
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Case-insensitive compare (ASCII fast path).
fn cmp_ci(a: &str, b: &str) -> Ordering {
    if a.is_ascii() && b.is_ascii() {
        a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
    } else {
        a.chars().flat_map(char::to_lowercase).cmp(b.chars().flat_map(char::to_lowercase))
    }
}

/// Extension without the dot, as `Entry::extension` finds it (not lower-cased).
fn extension_of(name: &str, is_dir: bool) -> &str {
    if is_dir {
        return "";
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => "",
    }
}

impl Table {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.items.len()
    }

    fn name(&self, id: usize) -> &str {
        let item = &self.items[id];
        &self.names[item.name_start as usize..item.name_start as usize + item.name_len as usize]
    }

    /// Appends a folder's prefix (`C:\Users\`) by walking up its parents.
    fn push_prefix(&self, folder: u32, buf: &mut String) {
        let f = &self.folders[folder as usize];
        if f.parent != NO_FOLDER {
            self.push_prefix(f.parent, buf);
        }
        let start = f.seg_start as usize;
        buf.push_str(&self.segments[start..start + f.seg_len as usize]);
    }

    /// A folder's path as `Entry::parent` shows it.
    fn parent_string(&self, folder: u32) -> String {
        let mut buf = String::new();
        self.push_prefix(folder, &mut buf);
        buf.truncate(self.folders[folder as usize].parent_len as usize);
        buf
    }

    /// The entry for `id`, its path built into `scratch`.
    fn entry<'a>(&'a self, id: usize, scratch: &'a mut PathScratch) -> Entry<'a> {
        let item = &self.items[id];
        let folder = &self.folders[item.folder as usize];
        if scratch.folder == item.folder {
            scratch.buf.truncate(scratch.prefix_len);
        } else {
            scratch.buf.clear();
            self.push_prefix(item.folder, &mut scratch.buf);
            scratch.folder = item.folder;
            scratch.prefix_len = scratch.buf.len();
        }
        scratch.buf.push_str(self.name(id));
        let buf: &'a String = &scratch.buf;
        Entry {
            name: self.name(id),
            path: buf.as_str(),
            parent: &buf[..folder.parent_len as usize],
            is_dir: item.is_dir,
            size: item.size,
            modified_ms: item.modified_ms,
            created_ms: item.created_ms,
            depth: folder.depth,
            child_count: if item.is_dir { Some(item.child_count) } else { None },
        }
    }

    pub(crate) fn row(&self, id: u32) -> NameRow {
        let id = id as usize;
        let item = &self.items[id];
        let name = self.name(id);
        let mut path = String::new();
        self.push_prefix(item.folder, &mut path);
        path.push_str(name);
        NameRow {
            path,
            name: name.to_string(),
            is_dir: item.is_dir,
            size: item.size,
            modified_ms: item.modified_ms,
            created_ms: item.created_ms,
            extension: extension_of(name, item.is_dir).to_lowercase(),
        }
    }

    /// Heap bytes held (arenas, items, and whichever orders are built).
    #[cfg(test)]
    fn heap_bytes(&self) -> usize {
        let orders: usize = self.orders.iter().filter_map(|o| o.get()).map(|o| o.capacity() * 4).sum();
        let rank = self.name_rank.get().map(|r| r.capacity() * 4).unwrap_or(0);
        self.names.capacity()
            + self.segments.capacity()
            + self.folders.capacity() * std::mem::size_of::<Folder>()
            + self.items.capacity() * std::mem::size_of::<Item>()
            + orders
            + rank
    }

    fn name_rank(&self) -> &[u32] {
        self.name_rank.get_or_init(|| {
            let order = self.order(ORDER_NAME);
            let mut rank = vec![0u32; order.len()];
            for (position, &id) in order.iter().enumerate() {
                rank[id as usize] = position as u32;
            }
            rank
        })
    }

    /// Ids sorted ascending by an `ORDER_*` column (built once, then cached).
    fn order(&self, slot: usize) -> &[u32] {
        self.orders[slot].get_or_init(|| self.build_order(slot))
    }

    fn build_order(&self, slot: usize) -> Vec<u32> {
        let mut ids: Vec<u32> = (0..self.items.len() as u32).collect();
        if slot == ORDER_NAME {
            ids.par_sort_unstable_by(|&a, &b| cmp_ci(self.name(a as usize), self.name(b as usize)).then(a.cmp(&b)));
            return ids;
        }
        let rank = self.name_rank();
        let items = &self.items;
        match slot {
            ORDER_PATH => {
                // Folder, then name (like Everything's Path column).
                // Parent strings exist only while this order is built.
                let parents: Vec<String> = (0..self.folders.len() as u32).into_par_iter().map(|f| self.parent_string(f)).collect();
                let mut folders: Vec<u32> = (0..self.folders.len() as u32).collect();
                folders.par_sort_unstable_by(|&a, &b| {
                    cmp_ci(&parents[a as usize], &parents[b as usize]).then(a.cmp(&b))
                });
                drop(parents);
                let mut folder_rank = vec![0u32; folders.len()];
                for (position, &f) in folders.iter().enumerate() {
                    folder_rank[f as usize] = position as u32;
                }
                ids.par_sort_unstable_by_key(|&id| {
                    ((folder_rank[items[id as usize].folder as usize] as u64) << 32) | rank[id as usize] as u64
                });
            }
            ORDER_SIZE => ids.par_sort_unstable_by_key(|&id| (items[id as usize].size, rank[id as usize])),
            ORDER_MODIFIED => ids.par_sort_unstable_by_key(|&id| (items[id as usize].modified_ms, rank[id as usize])),
            _ => {
                // Type: folders first, then extension, then name.
                ids.par_sort_unstable_by(|&a, &b| {
                    let (ia, ib) = (&items[a as usize], &items[b as usize]);
                    ib.is_dir
                        .cmp(&ia.is_dir)
                        .then_with(|| {
                            cmp_ci(extension_of(self.name(a as usize), ia.is_dir), extension_of(self.name(b as usize), ib.is_dir))
                        })
                        .then(rank[a as usize].cmp(&rank[b as usize]))
                });
            }
        }
        ids
    }

    /// Builds every order not built yet.
    #[cfg(test)]
    fn warm_orders(&self) {
        for slot in 0..ORDER_COUNT {
            self.order(slot);
        }
    }

    /// Bitset of matching ids and how many matched (parallel scan).
    fn match_bits(&self, matcher: &dyn Matcher) -> (Vec<u64>, usize) {
        const WORDS_PER_CHUNK: usize = 64;
        let n = self.items.len();
        let mut bits = vec![0u64; n.div_ceil(64)];
        let total = bits
            .par_chunks_mut(WORDS_PER_CHUNK)
            .enumerate()
            .map_init(PathScratch::new, |buf, (chunk, words)| {
                let mut count = 0;
                for (w, word) in words.iter_mut().enumerate() {
                    let base = (chunk * WORDS_PER_CHUNK + w) * 64;
                    for bit in 0..64.min(n - base) {
                        if matcher.matches(&self.entry(base + bit, buf)) {
                            *word |= 1 << bit;
                            count += 1;
                        }
                    }
                }
                count
            })
            .sum();
        (bits, total)
    }

    /// Runs a search: the match count and one page of ids, in order.
    /// `match_all` skips the scan (empty query); `options.query` is also read
    /// for relevance ranking.
    /// `hide`: entries left out (see `forget_paths`).
    pub(crate) fn search(&self, matcher: &dyn Matcher, match_all: bool, options: &NameQueryOptions, hide: Option<&[u64]>) -> (usize, Vec<u32>) {
        let (bits, total) = if match_all { (None, self.items.len()) } else {
            let (bits, total) = self.match_bits(matcher);
            (Some(bits), total)
        };
        let (bits, total) = match hide {
            None => (bits, total),
            Some(hide) => {
                let mut bits = bits.unwrap_or_else(|| all_bits(self.items.len()));
                let mut total = total;
                for (word, hidden) in bits.iter_mut().zip(hide) {
                    total -= (*word & hidden).count_ones() as usize;
                    *word &= !hidden;
                }
                (Some(bits), total)
            }
        };
        let slot = match options.sort {
            SortKey::Relevance => {
                let needles = relevance_needles(&options.query);
                if needles.is_empty() || bits.is_none() {
                    ORDER_NAME
                } else {
                    let ids = self.relevance_page(bits.as_deref().unwrap_or(&[]), total, &needles, options);
                    return (total, ids);
                }
            }
            SortKey::Name => ORDER_NAME,
            SortKey::Path => ORDER_PATH,
            SortKey::Size => ORDER_SIZE,
            SortKey::Modified => ORDER_MODIFIED,
            SortKey::Type => ORDER_TYPE,
        };
        let order = self.order(slot);
        (total, page_of_order(order, bits.as_deref(), options.descending, options.offset, options.limit))
    }

    /// Relevance: tier (exact > starts with > word start > contains > other),
    /// then shorter name, then folders before files, then name. Packed into a
    /// u64 per match so the sort is a plain integer sort.
    fn relevance_page(&self, bits: &[u64], total: usize, needles: &[String], options: &NameQueryOptions) -> Vec<u32> {
        let rank = self.name_rank();
        let mut ids = Vec::with_capacity(total);
        for (w, &word) in bits.iter().enumerate() {
            let mut word = word;
            while word != 0 {
                ids.push((w * 64) as u32 + word.trailing_zeros());
                word &= word - 1;
            }
        }
        let mut keys: Vec<u64> = ids
            .par_iter()
            .map_init(String::new, |lower, &id| {
                let name = self.name(id as usize);
                lower.clear();
                lower.extend(name.chars().flat_map(char::to_lowercase));
                let tier = needles.iter().map(|n| match_tier(lower, n)).min().unwrap_or(4) as u64;
                let item = &self.items[id as usize];
                (tier << 60) | ((item.name_len.min(0xFFF) as u64) << 48) | ((!item.is_dir as u64) << 32) | rank[id as usize] as u64
            })
            .collect();
        keys.par_sort_unstable();
        let order = self.order(ORDER_NAME);
        let pick = |key: &u64| order[(*key & 0xFFFF_FFFF) as usize];
        if options.descending {
            keys.iter().rev().skip(options.offset).take(options.limit).map(pick).collect()
        } else {
            keys.iter().skip(options.offset).take(options.limit).map(pick).collect()
        }
    }
}

/// A bitset with the first `n` bits set.
fn all_bits(n: usize) -> Vec<u64> {
    let mut bits = vec![u64::MAX; n.div_ceil(64)];
    if n % 64 != 0 {
        if let Some(last) = bits.last_mut() {
            *last = (1u64 << (n % 64)) - 1;
        }
    }
    bits
}

// ---- Removed by the app ----

/// Paths the app itself just deleted or moved away. The table only learns of
/// that once the watcher updated the filename index and a rebuild ran
/// (seconds later, or not at all with the watcher off), so until then name
/// search leaves them, and everything inside them, out.
#[derive(Default)]
struct Gone {
    /// Case-folded on Windows, no trailing separator.
    paths: std::collections::HashSet<String>,
    /// The table `bits` were worked out for (its address; 0: none yet).
    table: usize,
    bits: Vec<u64>,
}

static GONE: LazyLock<Mutex<Gone>> = LazyLock::new(Mutex::default);

/// Leaves `paths` (and what's inside them) out of name search until the
/// index no longer has them or they exist again (on the owner when this app
/// follows).
pub fn forget_paths(paths: Vec<String>) {
    if dash_core::link::is_follower() {
        let forwarded: Option<Result<(), String>> = dash_core::link::forward("forget_paths", serde_json::json!({ "paths": paths }));
        if forwarded.is_some() {
            return;
        }
    }
    let Ok(mut gone) = GONE.lock() else { return };
    gone.paths.extend(paths.iter().map(|p| gone_key(p)));
    gone.table = 0;
}

fn gone_key(path: &str) -> String {
    let trimmed = path.trim_end_matches(is_sep);
    // Keep a root's separator (`c:\`).
    let key = if trimmed.ends_with(':') || trimmed.is_empty() { path } else { trimmed };
    fold_case(key)
}

/// Windows paths ignore case; elsewhere they don't.
fn fold_case(path: &str) -> String {
    if cfg!(windows) { path.to_lowercase() } else { path.to_string() }
}

/// Matches entries that are, or are inside, a forgotten path; notes which
/// forgotten paths the table still has.
struct UnderGone<'a> {
    paths: &'a std::collections::HashSet<String>,
    found: Mutex<std::collections::HashSet<String>>,
}

impl Matcher for UnderGone<'_> {
    fn matches(&self, entry: &Entry) -> bool {
        let path = fold_case(entry.path);
        if self.paths.contains(&path) {
            if let Ok(mut found) = self.found.lock() {
                found.insert(path);
            }
            return true;
        }
        path.char_indices().filter(|&(_, c)| is_sep(c)).any(|(i, _)| self.paths.contains(&path[..i]))
    }
}

/// The entries of `table` to leave out, if any. Worked out again for each new
/// table, dropping paths that exist again or that the table no longer has.
fn hidden_bits(table: &Arc<Table>) -> Option<Vec<u64>> {
    let mut gone = GONE.lock().ok()?;
    if gone.paths.is_empty() {
        return None;
    }
    let key = Arc::as_ptr(table) as usize;
    if gone.table != key {
        gone.paths.retain(|p| !std::path::Path::new(p).exists());
        let matcher = UnderGone { paths: &gone.paths, found: Mutex::default() };
        let (bits, _) = table.match_bits(&matcher);
        let found = matcher.found.into_inner().unwrap_or_default();
        gone.paths.retain(|p| found.contains(p));
        gone.bits = bits;
        gone.table = key;
        if gone.paths.is_empty() {
            return None;
        }
    }
    Some(gone.bits.clone())
}

/// One page of `order`, keeping only ids set in `bits` (all when `None`).
fn page_of_order(order: &[u32], bits: Option<&[u64]>, descending: bool, offset: usize, limit: usize) -> Vec<u32> {
    let Some(bits) = bits else {
        let start = offset.min(order.len());
        let end = offset.saturating_add(limit).min(order.len());
        return if descending {
            order[order.len() - end..order.len() - start].iter().rev().copied().collect()
        } else {
            order[start..end].to_vec()
        };
    };
    let hit = |id: &&u32| bits[(**id / 64) as usize] & (1 << (**id % 64)) != 0;
    if descending {
        order.iter().rev().filter(hit).skip(offset).take(limit).copied().collect()
    } else {
        order.iter().filter(hit).skip(offset).take(limit).copied().collect()
    }
}

/// The plain words of what's typed, lower-cased, for relevance ranking: the
/// whole phrase and each word, minus filters (`ext:pdf`), exclusions and
/// operators. Empty means "rank by name".
fn relevance_needles(query: &str) -> Vec<String> {
    let words: Vec<String> = query
        .split_whitespace()
        .filter(|w| !w.contains(':') && !w.starts_with('-') && !w.starts_with('!') && *w != "|")
        .filter(|w| !matches!(*w, "OR" | "AND" | "NOT"))
        .map(|w| w.trim_matches('"').to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    let mut needles = Vec::with_capacity(words.len() + 1);
    if words.len() > 1 {
        needles.push(words.join(" "));
    }
    needles.extend(words);
    needles
}

/// 0 exact name or stem, 1 starts with, 2 starts a word, 3 contains, 4 none.
fn match_tier(name_lower: &str, needle: &str) -> u8 {
    if name_lower == needle {
        return 0;
    }
    if let Some((stem, _)) = name_lower.rsplit_once('.') {
        if stem == needle {
            return 0;
        }
    }
    if name_lower.starts_with(needle) {
        return 1;
    }
    let mut found = false;
    for (at, _) in name_lower.match_indices(needle) {
        found = true;
        let before = name_lower[..at].chars().next_back();
        if before.map(|c| !c.is_alphanumeric()).unwrap_or(true) {
            return 2;
        }
    }
    if found {
        3
    } else {
        4
    }
}

// ---- Freshness ----

/// Shortest gap between two builds; slow builds wait longer (see `rebuild_gap`).
const MIN_REBUILD_GAP: Duration = Duration::from_secs(5);

struct State {
    table: RwLock<Option<Arc<Table>>>,
    building: AtomicBool,
    /// When the last build started and how long it took.
    last_build: Mutex<Option<(Instant, Duration)>>,
}

static STATE: LazyLock<State> =
    LazyLock::new(|| State { table: RwLock::new(None), building: AtomicBool::new(false), last_build: Mutex::new(None) });

/// Builds are spaced by at least 4× the last build's time so a busy
/// watcher can't keep a core rebuilding.
fn rebuild_gap(last_took: Duration) -> Duration {
    MIN_REBUILD_GAP.max(last_took * 4)
}

/// Whether to start a build: the index exists and differs from the table,
/// none is running, and the last one started long enough ago.
fn should_rebuild(
    table_fingerprint: Option<u64>,
    index_fingerprint: Option<u64>,
    building: bool,
    last_build: Option<(Duration, Duration)>,
) -> bool {
    let Some(index_fingerprint) = index_fingerprint else {
        return false;
    };
    if building || table_fingerprint == Some(index_fingerprint) {
        return false;
    }
    match last_build {
        Some((since_start, took)) => since_start >= rebuild_gap(took),
        None => true,
    }
}

/// Starts a background build when `should_rebuild` says so.
fn refresh_table(app: &Arc<dyn Host>) {
    let index_fingerprint = crate::search::filename_index_fingerprint(app);
    let table_fingerprint = STATE.table.read().ok().and_then(|t| t.as_ref().map(|t| t.fingerprint));
    let last_build = STATE.last_build.lock().ok().and_then(|l| *l).map(|(at, took)| (at.elapsed(), took));
    if !should_rebuild(table_fingerprint, index_fingerprint, STATE.building.load(AtomicOrdering::Acquire), last_build) {
        return;
    }
    if STATE.building.swap(true, AtomicOrdering::AcqRel) {
        return;
    }
    let app = app.clone();
    // Clears `building` however the build ends, a panic included, so a
    // failed build can't leave name search on the fallback for good.
    let building = BuildingFlag;
    let spawned = std::thread::Builder::new().name("dash-name-table".into()).spawn(move || {
        let started = Instant::now();
        let mut builder = TableBuilder::default();
        let fingerprint = crate::search::visit_filename_index_docs(&app, &mut |doc| {
            builder.push(doc.path, doc.is_dir, doc.size, doc.modified_ms, doc.created_ms);
        });
        let table = fingerprint.map(|fp| Arc::new(builder.finish(fp)));
        if let Some(table) = &table {
            if let Ok(mut slot) = STATE.table.write() {
                *slot = Some(table.clone());
            }
        }
        if let Ok(mut last) = STATE.last_build.lock() {
            *last = Some((started, started.elapsed()));
        }
        drop(building);
        if let Some(table) = table {
            // Stop warming once a newer table replaced this one.
            for slot in 0..ORDER_COUNT {
                if !is_current(&table) {
                    break;
                }
                table.order(slot);
            }
        }
    });
    // A failed spawn drops the closure, and the flag with it.
    drop(spawned);
}

/// Owned by a table build; clears `State::building` when dropped.
struct BuildingFlag;

impl Drop for BuildingFlag {
    fn drop(&mut self) {
        STATE.building.store(false, AtomicOrdering::Release);
    }
}

/// Whether `table` is still the one searches use.
fn is_current(table: &Arc<Table>) -> bool {
    STATE.table.read().ok().and_then(|t| t.as_ref().map(|t| Arc::ptr_eq(t, table))).unwrap_or(false)
}

/// Before the table exists: the old filename search (at most 200 hits),
/// filtered by the query and sorted here.
/// Runs on the caller's (blocking) thread, so it calls the blocking search
/// directly; a follower already forwarded above.
fn fallback_search(
    app: Arc<dyn Host>,
    query: &crate::query::Query,
    options: &NameQueryOptions,
    started: Instant,
) -> Result<NameSearchResult, String> {
    if options.query.trim().is_empty() {
        return Ok(NameSearchResult::default());
    }
    let found = crate::search::search_local_files_inner(
        app,
        crate::search::FileSearchQueryOptions {
            query: options.query.clone(),
            limit: Some(200),
            offset: None,
            extension_filter: None,
            path_filter: None,
            natural_language: Some(true),
        },
    )?;
    let mut rows: Vec<NameRow> = found
        .results
        .into_iter()
        .map(|r| NameRow {
            is_dir: r.entry_type == "folder" || r.entry_type == "directory",
            name: r.file_name,
            path: r.path,
            size: r.size,
            modified_ms: r.modified_ms,
            created_ms: 0,
            extension: r.extension.to_lowercase(),
        })
        .filter(|row| {
            let parent = std::path::Path::new(&row.path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            query.matches(&Entry {
                name: &row.name,
                path: &row.path,
                parent: &parent,
                is_dir: row.is_dir,
                size: row.size,
                modified_ms: row.modified_ms,
                created_ms: row.created_ms,
                depth: row.path.matches(['\\', '/']).count().saturating_sub(1) as u16,
                child_count: None,
            })
        })
        .collect();
    sort_rows(&mut rows, options.sort, options.descending);
    let total = rows.len();
    let rows = rows.into_iter().skip(options.offset).take(options.limit).collect();
    Ok(NameSearchResult { total, rows, took_ms: started.elapsed().as_millis() as u64, error: None })
}

fn sort_rows(rows: &mut [NameRow], key: SortKey, descending: bool) {
    let lower = |s: &str| s.to_lowercase();
    match key {
        SortKey::Relevance => return,
        SortKey::Name => rows.sort_by_key(|r| lower(&r.name)),
        SortKey::Path => rows.sort_by_key(|r| lower(&r.path)),
        SortKey::Size => rows.sort_by_key(|r| r.size),
        SortKey::Modified => rows.sort_by_key(|r| r.modified_ms),
        SortKey::Type => rows.sort_by_key(|r| (r.extension.clone(), lower(&r.name))),
    }
    if descending {
        rows.reverse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Matches names containing the text (lower-case); a stand-in for
    /// `query::Query`.
    struct Contains(&'static str);
    impl Matcher for Contains {
        fn matches(&self, entry: &Entry) -> bool {
            entry.name.to_lowercase().contains(self.0)
        }
    }
    struct Everything;
    impl Matcher for Everything {
        fn matches(&self, _: &Entry) -> bool {
            true
        }
    }

    fn sample() -> Table {
        let mut b = TableBuilder::default();
        b.push(r"C:\", true, 0, 1, 1);
        b.push(r"C:\Users", true, 0, 2, 2);
        b.push(r"C:\Users\me", true, 0, 3, 3);
        b.push(r"C:\Users\me\report.pdf", false, 300, 30, 3);
        b.push(r"C:\Users\me\Report Final.docx", false, 100, 40, 4);
        b.push(r"C:\Users\me\notes.txt", false, 200, 10, 5);
        b.push(r"C:\Users\me\reports", true, 999, 20, 6);
        b.push(r"C:\Users\me\reports\q1 report.pdf", false, 50, 50, 7);
        b.finish(42)
    }

    fn opts(query: &str, sort: SortKey, descending: bool, offset: usize, limit: usize) -> NameQueryOptions {
        NameQueryOptions { query: query.into(), sort, descending, offset, limit }
    }

    fn names(table: &Table, ids: &[u32]) -> Vec<String> {
        ids.iter().map(|&id| table.row(id).name).collect()
    }

    fn find(table: &Table, name: &str) -> usize {
        (0..table.len()).find(|&i| table.name(i) == name).unwrap()
    }

    #[test]
    fn builds_entries_with_parent_depth_and_path() {
        let t = sample();
        let mut buf = PathScratch::new();
        let e = t.entry(find(&t, "report.pdf"), &mut buf);
        assert_eq!(e.path, r"C:\Users\me\report.pdf");
        assert_eq!(e.parent, r"C:\Users\me");
        assert_eq!(e.depth, 3);
        assert_eq!(e.child_count, None);
        assert_eq!(e.size, 300);
        let mut buf = PathScratch::new();
        let e = t.entry(find(&t, "Users"), &mut buf);
        assert_eq!((e.path, e.parent, e.depth), (r"C:\Users", r"C:\", 1));
        let mut buf = PathScratch::new();
        let e = t.entry(find(&t, r"C:\"), &mut buf);
        assert_eq!((e.path, e.parent, e.depth), (r"C:\", "", 0));
        assert_eq!(t.row(find(&t, "reports") as u32).size, 0, "folders have no size");
        assert_eq!(t.row(find(&t, "Report Final.docx") as u32).extension, "docx");
    }

    #[test]
    fn unix_paths_split_and_count() {
        let mut b = TableBuilder::default();
        b.push("/", true, 0, 0, 0);
        b.push("/home", true, 0, 0, 0);
        b.push("/home/a.txt", false, 1, 0, 0);
        b.push("/home/b.txt", false, 1, 0, 0);
        let t = b.finish(0);
        let mut buf = PathScratch::new();
        let e = t.entry(find(&t, "a.txt"), &mut buf);
        assert_eq!((e.path, e.parent, e.depth), ("/home/a.txt", "/home", 2));
        let mut buf = PathScratch::new();
        let e = t.entry(find(&t, "home"), &mut buf);
        assert_eq!((e.parent, e.child_count), ("/", Some(2)));
        let mut buf = PathScratch::new();
        assert_eq!(t.entry(find(&t, "/"), &mut buf).child_count, Some(1));
    }

    /// Scans see the right path for every entry, across folder changes.
    #[test]
    fn scan_paths_match_rows() {
        struct PathIsConsistent;
        impl Matcher for PathIsConsistent {
            fn matches(&self, e: &Entry) -> bool {
                e.path.ends_with(e.name) && e.path.starts_with(e.parent)
            }
        }
        let mut b = TableBuilder::default();
        for path in [r"C:\a\x.txt", r"C:\b\c\y.txt", r"C:\a\z.txt", r"D:\q", r"C:\b\c\d\w"] {
            b.push(path, false, 0, 0, 0);
        }
        let t = b.finish(0);
        assert_eq!(t.match_bits(&PathIsConsistent).1, 5);
        let mut rows: Vec<String> = (0..5).map(|i| t.row(i).path).collect();
        rows.sort();
        assert_eq!(rows, [r"C:\a\x.txt", r"C:\a\z.txt", r"C:\b\c\d\w", r"C:\b\c\y.txt", r"D:\q"]);
    }

    /// Deleted paths (and what's inside a deleted folder) leave the results
    /// and the count; paths the table doesn't have are noted as not found.
    #[test]
    fn forgotten_paths_are_left_out() {
        let mut b = TableBuilder::default();
        for (path, dir) in [(r"C:\a", true), (r"C:\a\x.txt", false), (r"C:\a\sub\y.txt", false), (r"C:\b.txt", false), (r"C:\ab.txt", false)] {
            b.push(path, dir, 0, 0, 0);
        }
        let t = b.finish(0);
        let paths: std::collections::HashSet<String> = [gone_key(r"C:\A\"), gone_key(r"C:\missing.txt")].into_iter().collect();
        let matcher = UnderGone { paths: &paths, found: Mutex::default() };
        let (hide, hidden) = t.match_bits(&matcher);
        // C:\a, both files inside it; not C:\ab.txt (same prefix, other name).
        assert_eq!(hidden, 3);
        assert_eq!(matcher.found.into_inner().unwrap().len(), 1);
        let (total, ids) = t.search(&Everything, true, &opts("", SortKey::Name, false, 0, 10), Some(&hide));
        assert_eq!(total, 2);
        assert_eq!(names(&t, &ids), ["ab.txt", "b.txt"]);
        let (total, _) = t.search(&Contains("txt"), false, &opts("txt", SortKey::Name, false, 0, 10), Some(&hide));
        assert_eq!(total, 2);
        assert_eq!(all_bits(70).iter().map(|w| w.count_ones()).sum::<u32>(), 70);
    }

    #[test]
    fn counts_children() {
        let t = sample();
        let count = |name| {
            let mut buf = PathScratch::new();
            t.entry(find(&t, name), &mut buf).child_count
        };
        assert_eq!(count(r"C:\"), Some(1));
        assert_eq!(count("Users"), Some(1));
        assert_eq!(count("me"), Some(4));
        assert_eq!(count("reports"), Some(1));
    }

    #[test]
    fn sorts_by_every_column() {
        let t = sample();
        let run = |sort, desc| names(&t, &t.search(&Contains("re"), false, &opts("", sort, desc, 0, 100), None).1);
        assert_eq!(run(SortKey::Name, false), ["q1 report.pdf", "Report Final.docx", "report.pdf", "reports"]);
        assert_eq!(run(SortKey::Name, true), ["reports", "report.pdf", "Report Final.docx", "q1 report.pdf"]);
        assert_eq!(run(SortKey::Size, false), ["reports", "q1 report.pdf", "Report Final.docx", "report.pdf"]);
        assert_eq!(run(SortKey::Modified, true), ["q1 report.pdf", "Report Final.docx", "report.pdf", "reports"]);
        assert_eq!(run(SortKey::Type, false), ["reports", "Report Final.docx", "q1 report.pdf", "report.pdf"]);
        // Path: folder (`C:\Users\me` < `C:\Users\me\reports`), then name.
        assert_eq!(run(SortKey::Path, false), ["Report Final.docx", "report.pdf", "reports", "q1 report.pdf"]);
    }

    #[test]
    fn pages_with_offset_and_limit() {
        let t = sample();
        let (total, ids) = t.search(&Contains("re"), false, &opts("re", SortKey::Name, false, 1, 2), None);
        assert_eq!(total, 4);
        assert_eq!(names(&t, &ids), ["Report Final.docx", "report.pdf"]);
        let (total, ids) = t.search(&Contains("re"), false, &opts("re", SortKey::Name, false, 3, 10), None);
        assert_eq!((total, names(&t, &ids)), (4, vec!["reports".to_string()]));
        let (_, ids) = t.search(&Contains("re"), false, &opts("re", SortKey::Name, false, 50, 10), None);
        assert!(ids.is_empty());
    }

    #[test]
    fn empty_query_lists_everything() {
        let t = sample();
        let (total, ids) = t.search(&Everything, true, &opts("", SortKey::Relevance, false, 0, 3), None);
        assert_eq!(total, 8);
        assert_eq!(names(&t, &ids), [r"C:\", "me", "notes.txt"]);
        let (_, ids) = t.search(&Everything, true, &opts("", SortKey::Name, true, 0, 2), None);
        assert_eq!(names(&t, &ids), ["Users", "reports"]);
        let (_, ids) = t.search(&Everything, true, &opts("", SortKey::Name, true, 7, 5), None);
        assert_eq!(names(&t, &ids), [r"C:\"]);
        // The real (placeholder or F2) parser treats "" as empty.
        assert!(crate::query::parse("").unwrap().is_empty());
    }

    #[test]
    fn relevance_ranks_exact_then_prefix_then_word_then_contains() {
        let mut b = TableBuilder::default();
        for path in [r"C:\a\misreport.txt", r"C:\a\q1 report.pdf", r"C:\a\reports", r"C:\a\report.pdf", r"C:\a\Report", r"C:\a\reporting.md"] {
            b.push(path, !path.contains('.'), 1, 0, 0);
        }
        let t = b.finish(0);
        let (total, ids) = t.search(&Contains("report"), false, &opts("report", SortKey::Relevance, false, 0, 10), None);
        assert_eq!(total, 6);
        assert_eq!(names(&t, &ids), ["Report", "report.pdf", "reports", "reporting.md", "q1 report.pdf", "misreport.txt"]);
        assert_eq!(match_tier("ext:pdf", "pdf"), 2);
        assert_eq!(relevance_needles("q1 report ext:pdf -old"), ["q1 report", "q1", "report"]);
    }

    /// At the id / arena limit the builder stops taking rows; what it took
    /// stays intact.
    #[test]
    fn stops_at_the_u32_limit_instead_of_wrapping() {
        let mut b = TableBuilder { limit: 40, ..TableBuilder::default() };
        let mut taken = 0;
        for i in 0..100 {
            if !b.push(&format!(r"C:\d\f{i:02}.txt"), false, 0, 0, 0) {
                break;
            }
            taken += 1;
        }
        assert!(taken > 0 && taken < 100, "took {taken}");
        assert!(!b.push(r"C:\d\x", false, 0, 0, 0), "full stays full");
        assert!(b.names.len() <= 40 && b.segments.len() <= 40);
        let t = b.finish(0);
        assert_eq!(t.len(), taken);
        let rows: Vec<String> = (0..taken as u32).map(|i| t.row(i).path).collect();
        assert_eq!(rows[0], r"C:\d\f00.txt");
        assert_eq!(rows[taken - 1], format!(r"C:\d\f{:02}.txt", taken - 1));

        // A folder segment past the limit stops it too.
        let mut b = TableBuilder { limit: 10, ..TableBuilder::default() };
        assert!(!b.push(r"C:\a-very-long-folder\x", false, 0, 0, 0));
        assert_eq!(b.finish(0).len(), 0);
    }

    /// `building` is cleared even when the build thread panics.
    #[test]
    fn building_flag_clears_on_panic() {
        STATE.building.store(true, AtomicOrdering::Release);
        let flag = BuildingFlag;
        let joined = std::thread::spawn(move || {
            let _flag = flag;
            panic!("build failed");
        })
        .join();
        assert!(joined.is_err());
        assert!(!STATE.building.load(AtomicOrdering::Acquire));
    }

    #[test]
    fn rebuild_decision() {
        let s = Duration::from_secs;
        assert!(should_rebuild(None, Some(1), false, None), "first build");
        assert!(!should_rebuild(None, None, false, None), "no index yet");
        assert!(!should_rebuild(Some(1), None, false, Some((s(60), s(1)))), "index released: keep the table");
        assert!(!should_rebuild(Some(1), Some(1), false, Some((s(60), s(1)))), "unchanged");
        assert!(!should_rebuild(Some(1), Some(2), true, Some((s(60), s(1)))), "already building");
        assert!(!should_rebuild(Some(1), Some(2), false, Some((s(2), s(1)))), "too soon");
        assert!(should_rebuild(Some(1), Some(2), false, Some((s(6), s(1)))), "changed, gap passed");
        assert!(!should_rebuild(Some(1), Some(2), false, Some((s(30), s(10)))), "slow build waits 4x");
    }

    /// 1M synthetic entries: build, scans, cached-order pages.
    /// `cargo test --release -p dash-search names -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_one_million() {
        let words = ["report", "photo", "invoice", "notes", "draft", "budget", "Résumé", "backup", "music", "video"];
        let exts = ["pdf", "jpg", "docx", "txt", "mp4", "rs", "png", "xlsx"];
        let started = Instant::now();
        let mut b = TableBuilder::default();
        let mut path = String::new();
        let mut n = 0u64;
        'outer: for top in 0..100 {
            for sub in 0..100 {
                let folder = format!(r"C:\Users\me\Projects\area {top}\{} {sub}", words[sub % words.len()]);
                b.push(&folder, true, 0, n, n);
                n += 1;
                for f in 0..99 {
                    path.clear();
                    path.push_str(&folder);
                    path.push('\\');
                    path.push_str(&format!("{} {} {f}.{}", words[(f * 7 + sub) % words.len()], n % 997, exts[f % exts.len()]));
                    b.push(&path, false, n * 37 % 100_000, n * 7919 % 1_000_000, n);
                    n += 1;
                    if n >= 1_000_000 {
                        break 'outer;
                    }
                }
            }
        }
        let table = b.finish(0);
        let entries = table.len();
        println!("build {} entries (incl. name order): {:?}", entries, started.elapsed());
        let base = table.heap_bytes();
        let t = Instant::now();
        table.warm_orders();
        println!("other orders: {:?}", t.elapsed());
        println!(
            "memory: {:.1} B/entry with name order+rank, {:.1} B/entry with all orders",
            base as f64 / entries as f64,
            table.heap_bytes() as f64 / entries as f64
        );
        for (label, query) in [("rare 'budget 42'", "budget 42"), ("common 'report'", "report"), ("'e' (most)", "e"), ("'zzz' (none)", "zzz")] {
            let parsed = crate::query::parse(query).unwrap();
            for sort in [SortKey::Relevance, SortKey::Name, SortKey::Size] {
                let t = Instant::now();
                let (total, ids) = table.search(&parsed, false, &opts(query, sort, false, 0, 100), None);
                println!("{label:>18} {sort:?}: {total} hits, {} rows, {:?}", ids.len(), t.elapsed());
            }
        }
        let t = Instant::now();
        let (total, _) = table.search(&Everything, true, &opts("", SortKey::Path, true, 500_000, 100), None);
        println!("empty query, Path desc page @500k: {total} total, {:?}", t.elapsed());
        /// Allocation-free ASCII case-insensitive substring test.
        struct AsciiContains(&'static [u8]);
        impl Matcher for AsciiContains {
            fn matches(&self, entry: &Entry) -> bool {
                entry.name.as_bytes().windows(self.0.len()).any(|w| w.eq_ignore_ascii_case(self.0))
            }
        }
        let t = Instant::now();
        let (bits, hits) = table.match_bits(&AsciiContains(b"report"));
        println!("scan only (alloc-free matcher): {hits} hits, {:?}", t.elapsed());
        let t = Instant::now();
        let ids = page_of_order(table.order(ORDER_MODIFIED), Some(&bits), true, 1000, 100);
        println!("sorted page fetch from cached order: {} rows, {:?}", ids.len(), t.elapsed());
    }

    /// Real data: set DASH_NAMES_REAL_INDEX to a COPY of the filename index
    /// (opened read-only, but don't point it at the live one).
    #[test]
    #[ignore]
    fn bench_real_index() {
        let Ok(dir) = std::env::var("DASH_NAMES_REAL_INDEX") else {
            println!("DASH_NAMES_REAL_INDEX not set; skipped");
            return;
        };
        let started = Instant::now();
        let mut b = TableBuilder::default();
        let fp = crate::search::visit_filename_index_at(std::path::Path::new(&dir), &mut |d| {
            b.push(d.path, d.is_dir, d.size, d.modified_ms, d.created_ms);
        });
        let read = started.elapsed();
        let table = b.finish(fp.expect("index opens"));
        println!("real: {} entries, {} folders; read {:?}, total build {:?}", table.len(), table.folders.len(), read, started.elapsed());
        let base = table.heap_bytes();
        let t = Instant::now();
        table.warm_orders();
        println!("other orders: {:?}", t.elapsed());
        println!(
            "memory: names {:.1} B/entry, folders {:.1} B/entry; total {:.1} B/entry (name order), {:.1} B/entry (all orders)",
            table.names.len() as f64 / table.len() as f64,
            (table.segments.len() + table.folders.len() * std::mem::size_of::<Folder>()) as f64 / table.len() as f64,
            base as f64 / table.len() as f64,
            table.heap_bytes() as f64 / table.len() as f64
        );
        for query in ["report", "e", "png", "zzzzq"] {
            let parsed = crate::query::parse(query).unwrap();
            for sort in [SortKey::Relevance, SortKey::Name, SortKey::Modified] {
                let t = Instant::now();
                let (total, ids) = table.search(&parsed, false, &opts(query, sort, true, 0, 100), None);
                println!("{query:>8} {sort:?}: {total} hits, {} rows, {:?}", ids.len(), t.elapsed());
            }
        }
    }
}
