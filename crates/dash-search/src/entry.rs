//! The contract between the query language (`query`, F2), the in-memory name
//! table (`names`, F1) and the apps' results tables (F3): one indexed file or
//! folder, something that decides whether it matches, and how name searches
//! are asked for and answered.

use serde::{Deserialize, Serialize};

/// One indexed file or folder as a query sees it. Borrowed so the name table
/// can test millions of them without copying.
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    /// File or folder name, e.g. `report.pdf`.
    pub name: &'a str,
    /// Full path, e.g. `C:\Users\me\Documents\report.pdf`.
    pub path: &'a str,
    /// The folder it's in (path without the name).
    pub parent: &'a str,
    pub is_dir: bool,
    /// Bytes; 0 for folders.
    pub size: u64,
    pub modified_ms: u64,
    pub created_ms: u64,
    /// Path separators below the drive / filesystem root (`C:\a\b.txt` → 2).
    pub depth: u16,
    /// Folders: how many entries are directly inside, when known.
    pub child_count: Option<u32>,
}

impl Entry<'_> {
    /// Lower-case extension without the dot (`""` for folders and none).
    pub fn extension(&self) -> String {
        if self.is_dir {
            return String::new();
        }
        match self.name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => ext.to_lowercase(),
            _ => String::new(),
        }
    }
}

/// Decides whether an entry matches (a parsed query, or a test double).
pub trait Matcher: Send + Sync {
    fn matches(&self, entry: &Entry) -> bool;
}

/// Column a name search is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SortKey {
    /// Best match first (the old ranking); falls back to name for an empty query.
    #[default]
    Relevance,
    Name,
    Path,
    Size,
    Modified,
    /// Extension, then name.
    Type,
}

/// A name search: what's typed (the query language, `query::parse`), the
/// order, and which page. An empty query lists everything.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NameQueryOptions {
    pub query: String,
    #[serde(default)]
    pub sort: SortKey,
    #[serde(default)]
    pub descending: bool,
    #[serde(default)]
    pub offset: usize,
    /// Rows to return (the caller pages through `total`); no fixed cap.
    pub limit: usize,
}

/// One result row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NameRow {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_ms: u64,
    pub created_ms: u64,
    pub extension: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NameSearchResult {
    /// Everything that matched (rows hold one page of it).
    pub total: usize,
    pub rows: Vec<NameRow>,
    pub took_ms: u64,
    /// The query didn't parse: why (rows are empty then).
    pub error: Option<String>,
}
