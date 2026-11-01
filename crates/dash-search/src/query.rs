//! The search box's query language (F2), in the style of Everything and
//! FSearch. Plain words work as before: each must appear somewhere in the
//! name, ignoring case.
//!
//! | Syntax | Meaning |
//! |---|---|
//! | `rep pdf` | every word appears in the name (any case) |
//! | `"annual report"` | a phrase is one word |
//! | `*.pdf`, `IMG_????.jpg` | wildcards match the whole name |
//! | `a\b`, `docs/x` | a word with a path separator matches the full path |
//! | `a b`, `a AND b` | both |
//! | `a OR b`, `a \| b` | either |
//! | `NOT a`, `!a`, `-a` | not |
//! | `( … )` | grouping; NOT binds tighter than AND, AND tighter than OR |
//! | `regex:` `case:` `exact:` `path:` `nopath:` | on a word (`case:Foo`) or alone, for the rest of the group |
//! | `file:` `folder:` `empty:` | alone: a filter; on a word: the word and the filter |
//! | `size:>10mb`, `size:1mb..5mb`, `size:..5mb` | size (b, kb, mb, gb, tb; 1024-based); folders never match |
//! | `size:10mb` | sizes that show as 10 MB: from 10 MB up to (not including) 11 MB |
//! | `dm:` / `dc:` | date modified / created: `2025-03-01`, `2025-03`, `2025`, `today`, `yesterday`, `thisweek`, `lastweek`, `thismonth`, `lastmonth`, `thisyear`, `lastyear`, `last5days`, `lastthreeweeks`, `last2months`, `last1year`; with `>`, `>=`, `<`, `<=` or `a..b` |
//! | `ext:pdf;docx` | extension (no dot); `ext:` alone means files without one |
//! | `parent:C:\Users` | the folder an entry is directly in |
//! | `depth:3`, `depth:2..4`, `depth:>3` | separators below the root |
//! | `childcount:5`, `childcount:>2` | folders with that many entries directly inside |
//! | `@name`, `filter:name` | a saved filter (`parse_with_filters`) |
//!
//! Dates use local time; weeks start on Monday. `lastweek` is the previous
//! calendar week, while `last1week` is the past seven days up to now.
//!
//! Typing in progress is forgiven where it's harmless: a dangling `OR` or
//! `NOT`, an empty `()` and an unclosed quote are ignored. Unbalanced
//! parentheses and bad sizes, dates and patterns are errors.
//!
//! `@name` that isn't a saved filter (and every `@name` in plain `parse`) is
//! ordinary text, so names like `@2x` stay searchable; `filter:name` that
//! doesn't exist is an error.
//!
//! Matching allocates nothing: the query side is folded to lower case and
//! patterns and date boundaries are worked out when parsing.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use time::{Date, Duration, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use crate::entry::{Entry, Matcher};

/// A parsed query.
#[derive(Debug, Clone)]
pub struct Query {
    root: Node,
}

impl Default for Query {
    fn default() -> Self {
        Query { root: Node::True }
    }
}

impl Query {
    /// Nothing typed: matches everything.
    pub fn is_empty(&self) -> bool {
        matches!(self.root, Node::True)
    }

    /// Only folders can match (`folder:` applies to the whole query), so a
    /// caller can skip files.
    pub fn wants_folders_only(&self) -> bool {
        match &self.root {
            Node::IsDir(true) => true,
            Node::And(parts) => parts.iter().any(|p| matches!(p, Node::IsDir(true))),
            _ => false,
        }
    }
}

/// Parses what's typed; `Err` explains what's wrong (shown to the user).
/// `@name` is plain text here; use `parse_with_filters` for saved filters.
pub fn parse(text: &str) -> Result<Query, String> {
    parse_inner(text, Clock::local(), None)
}

/// Like `parse`, with dates worked out relative to `now` (and its offset as
/// local time). For tests and for callers that already know the time.
pub fn parse_at(text: &str, now: OffsetDateTime) -> Result<Query, String> {
    parse_inner(text, Clock { now, follow_local: false }, None)
}

/// Like `parse`, expanding `@name` and `filter:name` from the filters saved
/// under `dir` (see `saved_filters`).
pub fn parse_with_filters(text: &str, dir: &Path) -> Result<Query, String> {
    let filters = saved_filters(dir);
    parse_inner(text, Clock::local(), Some(&filters))
}

impl Matcher for Query {
    fn matches(&self, entry: &Entry) -> bool {
        self.root.matches(entry)
    }
}

/// Longest query accepted, in characters.
const MAX_QUERY_CHARS: usize = 4096;
/// Deepest nesting of `(` and NOT accepted. Parsing, matching and dropping a
/// query all recurse this deep, so the limit keeps the stack safe.
const MAX_NESTING: usize = 64;

fn parse_inner(text: &str, clock: Clock, filters: Option<&[(String, String)]>) -> Result<Query, String> {
    if text.chars().count() > MAX_QUERY_CHARS {
        return Err(format!("the search is too long (over {MAX_QUERY_CHARS} characters)"));
    }
    let mut parser = Parser { toks: tokenize(text), pos: 0, clock: &clock, filters, expanding: Vec::new(), depth: 0 };
    Ok(Query { root: parser.parse_all()? })
}

// ---------------------------------------------------------------- saved filters

#[derive(Serialize, Deserialize)]
struct SavedFilter {
    name: String,
    query: String,
}

fn filters_file(dir: &Path) -> PathBuf {
    dir.join("preferences").join("dash-filters.json")
}

/// Saved filters as `(name, query)`, in the order they were saved. A missing
/// or unreadable file means none.
pub fn saved_filters(dir: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(filters_file(dir)) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<SavedFilter>>(&text)
        .map(|list| list.into_iter().map(|f| (f.name, f.query)).collect())
        .unwrap_or_default()
}

/// Saves (or replaces, ignoring case) the filter `name`. The query must parse,
/// and may use other saved filters but not itself.
pub fn save_filter(dir: &Path, name: &str, query: &str) -> Result<(), String> {
    let name = name.trim().trim_start_matches('@');
    if name.is_empty() {
        return Err("the filter needs a name".into());
    }
    if !name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err("a filter name can only use letters, digits, - _ and .".into());
    }
    let mut list = saved_filters(dir);
    let key = name.to_lowercase();
    match list.iter_mut().find(|(n, _)| n.to_lowercase() == key) {
        Some(existing) => *existing = (name.to_string(), query.to_string()),
        None => list.push((name.to_string(), query.to_string())),
    }
    let clock = Clock::local();
    let mut check =
        Parser { toks: tokenize(query), pos: 0, clock: &clock, filters: Some(&list), expanding: vec![key], depth: 0 };
    check.parse_all()?;
    write_filters(dir, &list)
}

/// Removes the saved filter `name` (ignoring case).
pub fn delete_filter(dir: &Path, name: &str) -> Result<(), String> {
    let name = name.trim().trim_start_matches('@');
    let key = name.to_lowercase();
    let mut list = saved_filters(dir);
    let before = list.len();
    list.retain(|(n, _)| n.to_lowercase() != key);
    if list.len() == before {
        return Err(format!("there's no saved filter called {name}"));
    }
    write_filters(dir, &list)
}

fn write_filters(dir: &Path, list: &[(String, String)]) -> Result<(), String> {
    let file = filters_file(dir);
    let saved: Vec<SavedFilter> = list.iter().map(|(n, q)| SavedFilter { name: n.clone(), query: q.clone() }).collect();
    let json = serde_json::to_string_pretty(&saved).map_err(|e| format!("couldn't save filters: {e}"))?;
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder).map_err(|e| format!("couldn't save filters: {e}"))?;
    }
    // Write beside it, then swap in, so a crash never leaves half a file.
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("couldn't save filters: {e}"))?;
    std::fs::rename(&tmp, &file).map_err(|e| format!("couldn't save filters: {e}"))
}

// ---------------------------------------------------------------- matching

#[derive(Debug, Clone)]
enum Node {
    True,
    Text(TextTerm),
    IsDir(bool),
    Size(Span),
    Modified(Span),
    Created(Span),
    /// Lower-case extensions; empty means "no extension".
    Ext(Vec<Needle>),
    /// Folded, `/` turned into `\`, trailing separators removed.
    Parent(String),
    Depth(Span),
    ChildCount(Span),
    Empty,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
}

impl Node {
    fn matches(&self, e: &Entry) -> bool {
        match self {
            Node::True => true,
            Node::Text(t) => t.matches(e),
            Node::IsDir(dir) => e.is_dir == *dir,
            Node::Size(s) => !e.is_dir && s.contains(e.size),
            Node::Modified(s) => s.contains(e.modified_ms),
            Node::Created(s) => s.contains(e.created_ms),
            Node::Ext(list) => {
                if e.is_dir {
                    return false;
                }
                let ext = ext_of(e.name);
                if list.is_empty() {
                    ext.is_empty()
                } else {
                    list.iter().any(|n| n.equals(ext))
                }
            }
            Node::Parent(want) => path_eq(e.parent, want),
            Node::Depth(s) => s.contains(e.depth as u64),
            Node::ChildCount(s) => e.is_dir && e.child_count.is_some_and(|c| s.contains(c as u64)),
            Node::Empty => {
                if e.is_dir {
                    e.child_count == Some(0)
                } else {
                    e.size == 0
                }
            }
            Node::And(parts) => parts.iter().all(|p| p.matches(e)),
            Node::Or(parts) => parts.iter().any(|p| p.matches(e)),
            Node::Not(inner) => !inner.matches(e),
        }
    }
}

/// Rough cost of testing a node, so cheap filters run first inside an AND.
fn cost(n: &Node) -> u8 {
    match n {
        Node::Text(t) => {
            if matches!(t.how, TextMatch::Pattern(_)) {
                3
            } else {
                2
            }
        }
        Node::Ext(_) | Node::Parent(_) => 1,
        Node::And(_) | Node::Or(_) | Node::Not(_) => 4,
        _ => 0,
    }
}

/// AND of `nodes`, flattened, without `True`, cheapest first.
fn and(nodes: Vec<Node>) -> Node {
    let mut out = Vec::new();
    for n in nodes {
        match n {
            Node::True => {}
            Node::And(inner) => out.extend(inner),
            other => out.push(other),
        }
    }
    out.sort_by_key(cost);
    match out.len() {
        0 => Node::True,
        1 => out.remove(0),
        _ => Node::And(out),
    }
}

/// OR of `nodes`, flattened; anything OR everything is everything.
fn or(nodes: Vec<Node>) -> Node {
    let mut out = Vec::new();
    for n in nodes {
        match n {
            Node::True => return Node::True,
            Node::Or(inner) => out.extend(inner),
            other => out.push(other),
        }
    }
    match out.len() {
        0 => Node::True,
        1 => out.remove(0),
        _ => Node::Or(out),
    }
}

/// An inclusive range of numbers (`lo > hi` matches nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    lo: u64,
    hi: u64,
}

impl Span {
    fn contains(&self, v: u64) -> bool {
        self.lo <= v && v <= self.hi
    }
}

#[derive(Debug, Clone)]
struct TextTerm {
    /// Match the full path instead of the name.
    on_path: bool,
    how: TextMatch,
}

#[derive(Debug, Clone)]
enum TextMatch {
    Contains(Needle),
    Exact(Needle),
    StartsWith(Needle),
    EndsWith(Needle),
    Pattern(Regex),
}

impl TextTerm {
    fn matches(&self, e: &Entry) -> bool {
        let hay = if self.on_path { e.path } else { e.name };
        match &self.how {
            TextMatch::Contains(n) => n.contained_in(hay),
            TextMatch::Exact(n) => n.equals(hay),
            TextMatch::StartsWith(n) => n.starts(hay),
            TextMatch::EndsWith(n) => n.ends(hay),
            TextMatch::Pattern(re) => re.is_match(hay),
        }
    }
}

/// Text to look for. Unless `case`, it's stored folded (simple lower case)
/// so only the haystack side is folded while matching.
#[derive(Debug, Clone)]
struct Needle {
    text: String,
    ascii: bool,
    case: bool,
}

impl Needle {
    fn new(s: &str, case: bool) -> Self {
        let text: String = if case { s.to_string() } else { s.chars().map(fold).collect() };
        let ascii = text.is_ascii();
        Needle { text, ascii, case }
    }

    fn contained_in(&self, hay: &str) -> bool {
        if self.case {
            return hay.contains(self.text.as_str());
        }
        contains_folded(hay, &self.text, self.ascii)
    }

    fn equals(&self, hay: &str) -> bool {
        if self.case {
            return hay == self.text;
        }
        if hay.is_ascii() && self.ascii {
            return hay.eq_ignore_ascii_case(&self.text);
        }
        chars_eq(hay.chars().map(fold), self.text.chars())
    }

    fn starts(&self, hay: &str) -> bool {
        if self.case {
            return hay.starts_with(self.text.as_str());
        }
        if hay.is_ascii() && self.ascii {
            return hay.len() >= self.text.len()
                && hay.as_bytes()[..self.text.len()].eq_ignore_ascii_case(self.text.as_bytes());
        }
        starts_folded(hay, &self.text)
    }

    fn ends(&self, hay: &str) -> bool {
        if self.case {
            return hay.ends_with(self.text.as_str());
        }
        if hay.is_ascii() && self.ascii {
            let n = self.text.len();
            return hay.len() >= n && hay.as_bytes()[hay.len() - n..].eq_ignore_ascii_case(self.text.as_bytes());
        }
        let mut h = hay.chars().rev();
        self.text.chars().rev().all(|n| h.next().map(fold) == Some(n))
    }
}

/// Simple case folding of one character: its lower case when that is a single
/// character (Georgian Mtavruli → Mkhedruli, Cyrillic, Greek, …), else itself.
fn fold(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

/// Whether folded `needle` appears in `hay`, ignoring case, without allocating.
fn contains_folded(hay: &str, needle: &str, needle_ascii: bool) -> bool {
    if needle.is_empty() {
        return true;
    }
    if hay.is_ascii() {
        // ASCII folds to ASCII, so a non-ASCII needle can't be in there.
        return needle_ascii && ascii_contains(hay.as_bytes(), needle.as_bytes());
    }
    hay.char_indices().any(|(i, _)| starts_folded(&hay[i..], needle))
}

/// `needle` is lower-case ASCII.
fn ascii_contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.len() > hay.len() {
        return false;
    }
    let first = needle[0];
    (0..=hay.len() - needle.len()).any(|i| {
        hay[i].to_ascii_lowercase() == first
            && hay[i + 1..i + needle.len()].iter().zip(&needle[1..]).all(|(a, b)| a.to_ascii_lowercase() == *b)
    })
}

fn starts_folded(hay: &str, needle: &str) -> bool {
    let mut h = hay.chars();
    needle.chars().all(|n| h.next().map(fold) == Some(n))
}

fn chars_eq(mut a: impl Iterator<Item = char>, mut b: impl Iterator<Item = char>) -> bool {
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x == y => {}
            _ => return false,
        }
    }
}

/// Extension as `Entry::extension` sees it, but borrowed and not lower-cased.
fn ext_of(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => "",
    }
}

fn norm_sep(c: char) -> char {
    if c == '/' {
        '\\'
    } else {
        c
    }
}

/// Folder paths equal, ignoring case, `/` vs `\` and trailing separators.
fn path_eq(path: &str, want: &str) -> bool {
    let path = path.trim_end_matches(['\\', '/']);
    chars_eq(path.chars().map(|c| norm_sep(fold(c))), want.chars())
}

// ---------------------------------------------------------------- tokens

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// `quote_at`: where (in `text`) the first quoted part began; prefixes
    /// and keywords are only recognised before it.
    Word {
        text: String,
        quote_at: Option<usize>,
    },
    Open,
    Close,
    Or,
    And,
    Not,
}

const MODIFIERS: [&str; 8] = ["case", "regex", "exact", "path", "nopath", "file", "folder", "empty"];

fn tokenize(text: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut quote_at: Option<usize> = None;
    let mut in_quote = false;
    // Inside a `regex:` word: `|` is part of it and `( )` nest.
    let mut regex_depth: Option<usize> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quote {
            if c == '"' {
                in_quote = false;
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '"' => {
                in_quote = true;
                if quote_at.is_none() {
                    quote_at = Some(cur.len());
                }
            }
            c if c.is_whitespace() => flush(&mut toks, &mut cur, &mut quote_at, &mut regex_depth),
            '(' | ')' | '|' if regex_depth.is_some() => {
                let depth = regex_depth.unwrap_or(0);
                if c == ')' && depth == 0 {
                    flush(&mut toks, &mut cur, &mut quote_at, &mut regex_depth);
                    toks.push(Tok::Close);
                } else {
                    if c == '(' {
                        regex_depth = Some(depth + 1);
                    } else if c == ')' {
                        regex_depth = Some(depth - 1);
                    }
                    cur.push(c);
                }
            }
            '(' | ')' | '|' => {
                flush(&mut toks, &mut cur, &mut quote_at, &mut regex_depth);
                toks.push(match c {
                    '(' => Tok::Open,
                    ')' => Tok::Close,
                    _ => Tok::Or,
                });
            }
            '!' if cur.is_empty() && quote_at.is_none() => toks.push(Tok::Not),
            '-' if cur.is_empty() && quote_at.is_none() && chars.peek().is_some_and(|n| !n.is_whitespace()) => {
                toks.push(Tok::Not)
            }
            _ => {
                cur.push(c);
                if c == ':' && regex_depth.is_none() && opens_regex(&cur) {
                    regex_depth = Some(0);
                }
            }
        }
    }
    flush(&mut toks, &mut cur, &mut quote_at, &mut regex_depth);
    toks
}

/// `cur` (ending in `:`) is modifiers ending in `regex:`.
fn opens_regex(cur: &str) -> bool {
    let body = &cur[..cur.len() - 1];
    let mut parts = body.rsplit(':');
    parts.next().is_some_and(|last| last.eq_ignore_ascii_case("regex"))
        && parts.all(|p| MODIFIERS.iter().any(|m| p.eq_ignore_ascii_case(m)))
}

fn flush(toks: &mut Vec<Tok>, cur: &mut String, quote_at: &mut Option<usize>, regex_depth: &mut Option<usize>) {
    *regex_depth = None;
    if cur.is_empty() && quote_at.is_none() {
        return;
    }
    let text = std::mem::take(cur);
    let quote = quote_at.take();
    if quote.is_none() {
        match text.as_str() {
            "OR" => return toks.push(Tok::Or),
            "AND" => return toks.push(Tok::And),
            "NOT" => return toks.push(Tok::Not),
            _ => {}
        }
    }
    toks.push(Tok::Word { text, quote_at: quote });
}

// ---------------------------------------------------------------- parsing

/// Modifiers in force for a word.
#[derive(Debug, Clone, Copy, Default)]
struct Mods {
    case: bool,
    regex: bool,
    exact: bool,
    /// `Some(true)` path, `Some(false)` name; `None`: path when the word has
    /// a separator.
    path: Option<bool>,
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    clock: &'a Clock,
    filters: Option<&'a [(String, String)]>,
    /// Saved filters being expanded (lower case), to catch loops.
    expanding: Vec<String>,
    /// Current nesting of `(` and NOT, saved filters included.
    depth: usize,
}

const UNMATCHED_OPEN: &str = "a ( has no matching )";
const UNMATCHED_CLOSE: &str = "a ) has no matching (";

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    /// One level deeper; `Err` past `MAX_NESTING`.
    fn enter(&mut self) -> Result<(), String> {
        if self.depth >= MAX_NESTING {
            return Err(format!("the search nests ( or NOT more than {MAX_NESTING} deep"));
        }
        self.depth += 1;
        Ok(())
    }

    fn parse_all(&mut self) -> Result<Node, String> {
        let mut mods = Mods::default();
        let node = self.parse_or(&mut mods)?;
        if self.peek().is_some() {
            return Err(UNMATCHED_CLOSE.into());
        }
        Ok(node.unwrap_or(Node::True))
    }

    fn parse_or(&mut self, mods: &mut Mods) -> Result<Option<Node>, String> {
        let mut alts = Vec::new();
        loop {
            if let Some(n) = self.parse_and(mods)? {
                alts.push(n);
            }
            if self.peek() == Some(&Tok::Or) {
                self.pos += 1;
            } else {
                break;
            }
        }
        Ok(if alts.is_empty() { None } else { Some(or(alts)) })
    }

    fn parse_and(&mut self, mods: &mut Mods) -> Result<Option<Node>, String> {
        let mut parts = Vec::new();
        loop {
            match self.peek() {
                None | Some(Tok::Close) | Some(Tok::Or) => break,
                Some(Tok::And) => self.pos += 1,
                _ => {
                    if let Some(n) = self.parse_unary(mods)? {
                        parts.push(n);
                    }
                }
            }
        }
        Ok(if parts.is_empty() { None } else { Some(and(parts)) })
    }

    fn parse_unary(&mut self, mods: &mut Mods) -> Result<Option<Node>, String> {
        let Some(tok) = self.peek().cloned() else {
            return Ok(None);
        };
        self.pos += 1;
        match tok {
            Tok::Not => match self.peek() {
                None | Some(Tok::Close) | Some(Tok::Or) => Ok(None),
                _ => {
                    self.enter()?;
                    let node = self.parse_unary(mods)?.map(|n| Node::Not(Box::new(n)));
                    self.depth -= 1;
                    Ok(node)
                }
            },
            Tok::Open => {
                // A group keeps its own modifiers.
                self.enter()?;
                let mut inner = *mods;
                let node = self.parse_or(&mut inner)?;
                self.depth -= 1;
                if self.peek() != Some(&Tok::Close) {
                    return Err(UNMATCHED_OPEN.into());
                }
                self.pos += 1;
                Ok(node)
            }
            Tok::Word { text, quote_at } => self.term(&text, quote_at, mods),
            Tok::And | Tok::Or | Tok::Close => Ok(None),
        }
    }

    /// One word: prefixes, filters, saved filters and the text itself. A word
    /// that is only modifiers (`case:`) switches them on for the rest of the
    /// group and returns `None`.
    fn term(&mut self, word: &str, quote_at: Option<usize>, mods: &mut Mods) -> Result<Option<Node>, String> {
        if quote_at != Some(0) && word.len() > 1 && word.starts_with('@') && self.find_filter(&word[1..]).is_some() {
            return self.expand(&word[1..]);
        }
        let unquoted = quote_at.unwrap_or(word.len());
        let mut local = *mods;
        let mut extra = Vec::new();
        let mut start = 0;
        let mut took_value = false;
        while let Some(colon) = word[start..].find(':') {
            if start + colon >= unquoted {
                break;
            }
            let key = word[start..start + colon].to_ascii_lowercase();
            let value_start = start + colon + 1;
            match key.as_str() {
                "case" => local.case = true,
                "regex" => local.regex = true,
                "exact" => local.exact = true,
                "path" => local.path = Some(true),
                "nopath" => local.path = Some(false),
                "file" => extra.push(Node::IsDir(false)),
                "folder" => extra.push(Node::IsDir(true)),
                "empty" => extra.push(Node::Empty),
                "filter" | "size" | "dm" | "dc" | "ext" | "parent" | "depth" | "childcount" => {
                    let value = &word[value_start..];
                    if key == "filter" {
                        if let Some(n) = self.expand(value)? {
                            extra.push(n);
                        }
                    } else {
                        extra.push(self.filter(&key, value)?);
                    }
                    took_value = true;
                    start = word.len();
                    break;
                }
                _ => break,
            }
            start = value_start;
        }
        let text = &word[start..];
        if text.is_empty() {
            if !took_value {
                *mods = local;
            }
            return Ok(if extra.is_empty() { None } else { Some(and(extra)) });
        }
        extra.push(text_node(text, local)?);
        Ok(Some(and(extra)))
    }

    fn filter(&self, key: &str, value: &str) -> Result<Node, String> {
        match key {
            "size" => Ok(Node::Size(parse_span(value, "size:", &size_atom)?)),
            "dm" | "dc" => {
                let label = if key == "dm" { "dm:" } else { "dc:" };
                let atom = |t: &str, _bare: bool| self.clock.date_atom(t).ok_or_else(|| date_help(label));
                let span = parse_span(value, label, &atom)?;
                Ok(if key == "dm" { Node::Modified(span) } else { Node::Created(span) })
            }
            "ext" => Ok(Node::Ext(
                value
                    .split(';')
                    .map(|e| e.trim().trim_start_matches('.'))
                    .filter(|e| !e.is_empty())
                    .map(|e| Needle::new(e, false))
                    .collect(),
            )),
            "parent" => {
                let value = value.trim();
                if value.is_empty() {
                    return Err("parent: needs a folder path, like parent:C:\\Users".into());
                }
                let folded: String = value.chars().map(|c| norm_sep(fold(c))).collect();
                Ok(Node::Parent(folded.trim_end_matches('\\').to_string()))
            }
            "depth" => Ok(Node::Depth(parse_span(value, "depth:", &|t, _| int_atom(t, "depth:"))?)),
            _ => Ok(Node::ChildCount(parse_span(value, "childcount:", &|t, _| int_atom(t, "childcount:"))?)),
        }
    }

    fn find_filter(&self, name: &str) -> Option<&'a str> {
        let key = name.to_lowercase();
        let filters: &'a [(String, String)] = self.filters?;
        filters.iter().find(|(n, _)| n.to_lowercase() == key).map(|(_, q)| q.as_str())
    }

    /// The saved filter `name`, parsed as its own group.
    fn expand(&mut self, name: &str) -> Result<Option<Node>, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("filter: needs the name of a saved filter".into());
        }
        let Some(text) = self.find_filter(name) else {
            return Err(format!("there's no saved filter called {name}"));
        };
        let key = name.to_lowercase();
        if self.expanding.contains(&key) {
            return Err(format!("the saved filter @{name} uses itself"));
        }
        let mut expanding = self.expanding.clone();
        expanding.push(key);
        // Starts at our depth, so filters nested in filters stay within the limit.
        let mut inner = Parser {
            toks: tokenize(text),
            pos: 0,
            clock: self.clock,
            filters: self.filters,
            expanding,
            depth: self.depth,
        };
        let node = inner.parse_all().map_err(|e| {
            if e.contains("saved filter") {
                e
            } else {
                format!("in the saved filter @{name}: {e}")
            }
        })?;
        Ok(Some(node))
    }
}

/// The text part of a word, compiled for the modifiers in force.
fn text_node(text: &str, m: Mods) -> Result<Node, String> {
    let on_path = m.path.unwrap_or(!m.regex && text.contains(['\\', '/']));
    let how = if m.regex {
        let pattern = if m.case { text.to_string() } else { format!("(?i){text}") };
        TextMatch::Pattern(Regex::new(&pattern).map_err(|_| format!("regex: \"{text}\" isn't a valid pattern"))?)
    } else if text.contains(['*', '?']) {
        wildcard(text, m.case)?
    } else if m.exact {
        TextMatch::Exact(Needle::new(text, m.case))
    } else {
        TextMatch::Contains(Needle::new(text, m.case))
    };
    Ok(Node::Text(TextTerm { on_path, how }))
}

/// A wildcard word matching the whole name. `*x`, `x*` and `*x*` become
/// plain comparisons; anything else a regex.
fn wildcard(text: &str, case: bool) -> Result<TextMatch, String> {
    if !text.contains('?') {
        let inner = text.trim_matches('*');
        if !inner.contains('*') {
            let n = Needle::new(inner, case);
            return Ok(match (text.starts_with('*'), text.ends_with('*')) {
                (true, true) => TextMatch::Contains(n),
                (true, false) => TextMatch::EndsWith(n),
                (false, true) => TextMatch::StartsWith(n),
                (false, false) => TextMatch::Exact(n),
            });
        }
    }
    let mut pattern = String::from(if case { "(?s)^" } else { "(?si)^" });
    let mut buf = [0u8; 4];
    for c in text.chars() {
        match c {
            '*' => pattern.push_str(".*"),
            '?' => pattern.push('.'),
            _ => pattern.push_str(&regex::escape(c.encode_utf8(&mut buf))),
        }
    }
    pattern.push('$');
    Regex::new(&pattern).map(TextMatch::Pattern).map_err(|_| format!("\"{text}\" is too complicated a pattern"))
}

/// A value as a range: `x` (the atom's own span), `>x`, `>=x`, `<x`, `<=x`,
/// `=x`, `a..b`, `..b`, `a..`. The atom returns the span one value covers
/// (`bare` when it stands alone, e.g. `size:10mb` covers all of 10.x MB).
/// Turns one value into the span it covers; see `parse_span`.
type SpanAtom<'a> = dyn Fn(&str, bool) -> Result<(u64, u64), String> + 'a;

fn parse_span(raw: &str, label: &str, atom: &SpanAtom<'_>) -> Result<Span, String> {
    let raw = raw.trim();
    if let Some((a, b)) = raw.split_once("..") {
        let (a, b) = (a.trim(), b.trim());
        if a.is_empty() && b.is_empty() {
            return atom("", false).map(|_| Span { lo: 0, hi: u64::MAX });
        }
        let lo = if a.is_empty() { 0 } else { atom(a, false)?.0 };
        let hi = if b.is_empty() { u64::MAX } else { atom(b, false)?.1 };
        if lo > hi {
            return Err(format!("{label} the start of the range is after its end"));
        }
        return Ok(Span { lo, hi });
    }
    // Longer operators first so `>=` isn't read as `>`.
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = raw.strip_prefix(op) {
            let (start, end) = atom(rest.trim(), op == "=")?;
            return Ok(match op {
                ">=" => Span { lo: start, hi: u64::MAX },
                "<=" => Span { lo: 0, hi: end },
                ">" => match end.checked_add(1) {
                    Some(lo) => Span { lo, hi: u64::MAX },
                    None => Span { lo: 1, hi: 0 },
                },
                "<" => match start.checked_sub(1) {
                    Some(hi) => Span { lo: 0, hi },
                    None => Span { lo: 1, hi: 0 },
                },
                _ => Span { lo: start, hi: end },
            });
        }
    }
    let (lo, hi) = atom(raw, true)?;
    Ok(Span { lo, hi })
}

const SIZE_HELP: &str = "size: needs a number like 10mb, >1gb or 1mb..5mb";

/// Bytes in `10mb`, `1.5 GB`, `500`. Alone with a unit it covers everything
/// that shows as that many units; in comparisons it's the exact value.
fn size_atom(t: &str, bare: bool) -> Result<(u64, u64), String> {
    let t = t.trim().to_ascii_lowercase();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().map_err(|_| SIZE_HELP.to_string())?;
    let unit_bytes: u64 = match unit.trim() {
        "" | "b" | "byte" | "bytes" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        other => return Err(format!("size: \"{other}\" isn't a unit — use b, kb, mb or gb")),
    };
    let bytes = n * unit_bytes as f64;
    if !bytes.is_finite() || bytes >= u64::MAX as f64 {
        return Err("size: that number is too big".into());
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked finite and below u64::MAX above; never negative since `num` holds only digits and dots"
    )]
    let bytes = bytes as u64;
    if bare && unit_bytes > 1 {
        Ok((bytes, bytes.saturating_add(unit_bytes - 1)))
    } else {
        Ok((bytes, bytes))
    }
}

fn int_atom(t: &str, label: &str) -> Result<(u64, u64), String> {
    t.trim().parse::<u64>().map(|n| (n, n)).map_err(|_| format!("{label} needs a number like 3, >3 or 2..4"))
}

fn date_help(label: &str) -> String {
    format!("{label} needs a date like 2025-03-01, 2025-03 or 2025, or a word like today, lastweek or last5days")
}

// ---------------------------------------------------------------- dates

/// "Now" for date words, and how dates become milliseconds.
#[derive(Debug, Clone, Copy)]
struct Clock {
    /// In local time.
    now: OffsetDateTime,
    /// Look up the local offset of each date (daylight saving); otherwise use
    /// `now`'s offset throughout.
    follow_local: bool,
}

impl Clock {
    fn local() -> Clock {
        let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
        Clock { now, follow_local: true }
    }

    /// Unix milliseconds of a local date and time.
    fn ms(&self, date: Date, t: Time) -> i64 {
        let local = PrimitiveDateTime::new(date, t);
        let mut offset = self.now.offset();
        if self.follow_local {
            if let Ok(o) = UtcOffset::local_offset_at(local.assume_offset(offset)) {
                offset = o;
            }
        }
        i64::try_from(local.assume_offset(offset).unix_timestamp_nanos() / 1_000_000)
            .expect("`time` caps years at 999,999, far inside i64 milliseconds")
    }

    /// From the start of `start` to just before the start of `end`.
    fn days(&self, start: Date, end: Date) -> (u64, u64) {
        (clamp_ms(self.ms(start, Time::MIDNIGHT)), clamp_ms(self.ms(end, Time::MIDNIGHT) - 1))
    }

    /// The span of milliseconds a date word or date covers.
    fn date_atom(&self, t: &str) -> Option<(u64, u64)> {
        let t = t.trim().to_ascii_lowercase();
        let today = self.now.date();
        let monday = today.checked_sub(Duration::days(today.weekday().number_days_from_monday() as i64))?;
        let first = Date::from_calendar_date(today.year(), today.month(), 1).ok()?;
        let jan1 = |y: i32| Date::from_calendar_date(y, Month::January, 1).ok();
        match t.as_str() {
            "today" => Some(self.days(today, today.next_day()?)),
            "yesterday" => Some(self.days(today.previous_day()?, today)),
            "thisweek" => Some(self.days(monday, monday.checked_add(Duration::days(7))?)),
            "lastweek" => Some(self.days(monday.checked_sub(Duration::days(7))?, monday)),
            "thismonth" => Some(self.days(first, add_months(first, 1)?)),
            "lastmonth" => Some(self.days(add_months(first, -1)?, first)),
            "thisyear" => Some(self.days(jan1(today.year())?, jan1(today.year() + 1)?)),
            "lastyear" => Some(self.days(jan1(today.year() - 1)?, jan1(today.year())?)),
            _ => self.last_n(&t).or_else(|| self.absolute(&t)),
        }
    }

    /// `last5days`, `lastthreeweeks`, `past2months`: from that long before
    /// now to the end of today.
    fn last_n(&self, t: &str) -> Option<(u64, u64)> {
        let rest = t.strip_prefix("last").or_else(|| t.strip_prefix("past"))?;
        let (n, unit) = split_count(rest)?;
        if n == 0 || n > 100_000 {
            return None;
        }
        let today = self.now.date();
        let start = match unit {
            "day" | "days" => instant_ms(self.now.checked_sub(Duration::days(n))?),
            "week" | "weeks" => instant_ms(self.now.checked_sub(Duration::weeks(n))?),
            "month" | "months" => self.ms(add_months(today, -n)?, self.now.time()),
            "year" | "years" => self.ms(add_months(today, -n.checked_mul(12)?)?, self.now.time()),
            _ => return None,
        };
        let end = self.ms(today.next_day()?, Time::MIDNIGHT) - 1;
        Some((clamp_ms(start), clamp_ms(end)))
    }

    /// `2025-03-01`, `2025-03`, `2025` (also with `/` or `.`).
    fn absolute(&self, t: &str) -> Option<(u64, u64)> {
        let parts: Vec<&str> = t.split(['-', '/', '.']).collect();
        if parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
            return None;
        }
        let year: i32 = parts[0].parse().ok()?;
        if parts[0].len() != 4 {
            return None;
        }
        match parts.len() {
            1 => {
                let start = Date::from_calendar_date(year, Month::January, 1).ok()?;
                Some(self.days(start, add_months(start, 12)?))
            }
            2 => {
                let month = Month::try_from(parts[1].parse::<u8>().ok()?).ok()?;
                let start = Date::from_calendar_date(year, month, 1).ok()?;
                Some(self.days(start, add_months(start, 1)?))
            }
            3 => {
                let month = Month::try_from(parts[1].parse::<u8>().ok()?).ok()?;
                let day = Date::from_calendar_date(year, month, parts[2].parse().ok()?).ok()?;
                Some(self.days(day, day.next_day()?))
            }
            _ => None,
        }
    }
}

fn instant_ms(t: OffsetDateTime) -> i64 {
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000)
        .expect("`time` caps years at 999,999, far inside i64 milliseconds")
}

fn clamp_ms(ms: i64) -> u64 {
    u64::try_from(ms).unwrap_or(0)
}

const NUMBER_WORDS: [(&str, i64); 12] = [
    ("one", 1),
    ("two", 2),
    ("three", 3),
    ("four", 4),
    ("five", 5),
    ("six", 6),
    ("seven", 7),
    ("eight", 8),
    ("nine", 9),
    ("ten", 10),
    ("eleven", 11),
    ("twelve", 12),
];

/// `5days` → (5, "days"); `threeweeks` → (3, "weeks").
fn split_count(s: &str) -> Option<(i64, &str)> {
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digits > 0 {
        return Some((s[..digits].parse().ok()?, &s[digits..]));
    }
    NUMBER_WORDS.iter().find(|(w, _)| s.starts_with(w)).map(|(w, n)| (*n, &s[w.len()..]))
}

/// `date` moved by `delta` months, the day clamped to the month's length.
fn add_months(date: Date, delta: i64) -> Option<Date> {
    let total = (date.year() as i64) * 12 + (date.month() as i64 - 1) + delta;
    let year: i32 = total.div_euclid(12).try_into().ok()?;
    let month = Month::try_from(u8::try_from(total.rem_euclid(12) + 1).ok()?).ok()?;
    let day = date.day().min(month_len(year, month));
    Date::from_calendar_date(year, month, day).ok()
}

fn month_len(year: i32, month: Month) -> u8 {
    match month {
        Month::February => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        Month::April | Month::June | Month::September | Month::November => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests;
