//! Runs in any app that is a `dash_core::Host`.
//! Live grep — ripgrep-engine content search for ad-hoc / un-indexed
//! folders. Wave 3.3 (2026-05-27).
//!
//! Complements the Tantivy content index (which needs pre-building):
//! when the user adds a new folder and immediately searches, the index
//! hasn't seen it yet and content search would return nothing. The
//! live-grep path scans the folder right now — zero setup, ~1 GB/s on
//! a modern SSD — and streams matches as they're found.
//!
//! Engine: `grep-searcher` + `grep-regex` (BurntSushi crates that
//! power ripgrep). Walk: `ignore::WalkBuilder` (already a KIL dep —
//! respects `.gitignore`, `.ignore`, hidden files by default).
//!
//! Streaming model: each hit is emitted as a `live-grep-hit-<opId>`
//! Tauri event so the frontend renders incrementally instead of waiting
//! for the whole walk. The function still returns a summary at the end
//! (total matches, files scanned, cancelled flag, truncated flag) so
//! the caller can show a final status line.
//!
//! Cancellation: standard Wave 2.5 pattern — `CANCELLED_LIVE_GREP`
//! HashSet keyed by operation ID; checked at each walker entry. The
//! `cancel_live_grep` Tauri command sets the flag.

use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, sinks::UTF8};
use ignore::WalkBuilder;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

// ── Cancel registry ────────────────────────────────────────────────────

static CANCELLED_LIVE_GREP: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

pub fn cancel_live_grep(operation_id: String) -> Result<(), String> {
    CANCELLED_LIVE_GREP
        .lock()
        .map_err(|_| "Cancel registry lock failed".to_string())?
        .insert(operation_id);
    Ok(())
}

fn is_live_grep_cancelled(op_id: &str) -> bool {
    CANCELLED_LIVE_GREP
        .lock()
        .ok()
        .map(|s| s.contains(op_id))
        .unwrap_or(false)
}

fn clear_live_grep_cancel(op_id: &str) {
    if let Ok(mut s) = CANCELLED_LIVE_GREP.lock() {
        s.remove(op_id);
    }
}

// ── Options + result shapes ───────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveGrepOptions {
    /// Absolute paths of one or more folders to scan. Each is walked
    /// recursively respecting `.gitignore` etc. Multi-folder support
    /// is the default mode now (Wave 3.3.1, 2026-05-27) — the palette's
    /// auto-fallback runs against the user's configured indexed roots,
    /// which is typically Documents + Downloads + Desktop or a custom
    /// list. Empty list → zero-result summary, not an error (matches
    /// the "no configured folders" path gracefully).
    pub folders: Vec<String>,
    /// Search query. Regex by default; pass `literal: true` to escape
    /// regex metacharacters and search for the string verbatim.
    pub query: String,
    /// Unique ID for this query so the frontend can cancel it via
    /// `cancel_live_grep` and route the streamed-hit events.
    pub operation_id: String,
    /// Case-sensitive match. Defaults to false (case-insensitive) since
    /// that's what users expect from a casual content search.
    #[serde(default)]
    pub case_sensitive: bool,
    /// Treat the query as a literal string (escape regex
    /// metacharacters). Defaults to false (regex mode). Most palette
    /// users want literal — the frontend defaults to literal=true.
    #[serde(default)]
    pub literal: bool,
    /// Hard cap on total matches emitted across ALL folders. Defaults
    /// to 500 to keep the UI responsive — a "find every match in
    /// node_modules" query could otherwise emit millions of events.
    #[serde(default = "default_max_matches")]
    pub max_matches: usize,
    /// Skip files larger than this byte count. Defaults to 50 MB.
    /// Larger files are almost always binaries or generated artifacts;
    /// scanning them eats time without typically finding meaningful
    /// hits.
    #[serde(default = "default_max_file_size")]
    pub max_file_size: u64,
}

fn default_max_matches() -> usize {
    500
}
/// 100 MB. Raised from the original 50 MB (2026-05-27, v1 production
/// stance): people genuinely have ~80 MB code-bundles, lecture-note
/// PDFs, and chat-log exports they want to grep. Stops well before
/// log-file / database-dump territory where grep wouldn't return
/// useful hits anyway.
fn default_max_file_size() -> u64 {
    100 * 1024 * 1024
}

/// A single match streamed via the `live-grep-hit-<opId>` Tauri event.
/// The frontend collects these into the results panel as they arrive.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LiveGrepHit {
    pub path: String,
    pub line_number: u64,
    /// The full line of text containing the match, with the trailing
    /// newline stripped. May be empty for matches in zero-length lines.
    pub line: String,
    /// Byte offsets of the matched substring within `line` — the
    /// frontend uses these to highlight the matched span. May be (0, 0)
    /// for line-anchored patterns that match against the empty position
    /// at the start of the line.
    pub match_start: usize,
    pub match_end: usize,
}

/// Final summary returned by `live_grep_in_folder` after the walk
/// completes (or is cancelled / truncated).
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LiveGrepSummary {
    pub matches: usize,
    pub files_scanned: usize,
    pub cancelled: bool,
    /// True if we hit `max_matches` and stopped early. Distinct from
    /// `cancelled` (which is user-initiated) — `truncated` means "there
    /// might be more, try a narrower query."
    pub truncated: bool,
}

// ── The command ────────────────────────────────────────────────────────

pub async fn live_grep_in_folders(
    app: std::sync::Arc<dyn dash_core::Host>,
    options: LiveGrepOptions,
) -> Result<LiveGrepSummary, String> {
    tokio::task::spawn_blocking(move || {
        let op_id = options.operation_id.clone();
        clear_live_grep_cancel(&op_id);

        // Validate / build the matcher. Empty query short-circuits to
        // an empty result rather than matching everything.
        let trimmed = options.query.trim();
        if trimmed.is_empty() || options.folders.is_empty() {
            return Ok(LiveGrepSummary {
                matches: 0,
                files_scanned: 0,
                cancelled: false,
                truncated: false,
            });
        }
        let pattern = if options.literal {
            regex_escape(trimmed)
        } else {
            trimmed.to_string()
        };
        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(!options.case_sensitive)
            // Multi-line is OFF: each hit is a single-line match. The
            // grep-searcher line-iterator drives the loop one line at a
            // time so cross-line patterns won't fire — fine for typical
            // palette queries ("find this phrase").
            .build(&pattern)
            .map_err(|e| format!("Invalid regex: {e}"))?;

        let event_name = format!("live-grep-hit-{}", op_id);
        // Wave 3.3.2 (v1 production polish, 2026-05-27): walk each
        // configured folder in PARALLEL via rayon. With 3 indexed
        // folders on a modern SSD this turns ~750ms wall-clock-before-
        // first-hit into ~250ms. Shared state across the workers:
        //   - total_matches: AtomicUsize — capped via fetch_add CAS.
        //   - truncated: AtomicBool — set when any worker sees the cap.
        //   - files_scanned: AtomicUsize — purely informational, relaxed.
        // Cancellation flows through the existing CANCELLED_LIVE_GREP
        // registry, checked at every walker entry by every worker.
        // The cap is enforced atomically: a worker that would push
        // total_matches past max_matches reverts its add and stops
        // emitting.
        let total_matches = Arc::new(AtomicUsize::new(0));
        let files_scanned = Arc::new(AtomicUsize::new(0));
        let truncated = Arc::new(AtomicBool::new(false));
        let max_matches = options.max_matches;
        let max_file_size = options.max_file_size;
        let op_id_arc = Arc::new(op_id.clone());
        let matcher_arc = Arc::new(matcher);

        // Skip non-existent / non-folder entries up front so the
        // parallel iter doesn't fan out workers that immediately exit.
        let valid_folders: Vec<PathBuf> = options
            .folders
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .collect();

        valid_folders.par_iter().for_each(|folder| {
            let walker = WalkBuilder::new(folder).build();
            let mut searcher = Searcher::new();
            for entry_result in walker {
                if is_live_grep_cancelled(&op_id_arc) {
                    return;
                }
                if total_matches.load(Ordering::Relaxed) >= max_matches {
                    truncated.store(true, Ordering::Relaxed);
                    return;
                }
                let Ok(entry) = entry_result else { continue };
                let Some(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_file() {
                    continue;
                }
                let path = entry.path();
                if let Ok(metadata) = path.metadata() {
                    if metadata.len() > max_file_size {
                        continue;
                    }
                }

                files_scanned.fetch_add(1, Ordering::Relaxed);
                let path_str = path.to_string_lossy().to_string();
                let app_handle = app.clone();
                let event_name_local = event_name.clone();
                let matcher_for_sink = matcher_arc.clone();
                let total_ref = total_matches.clone();
                let truncated_ref = truncated.clone();

                let _ = searcher.search_path(
                    matcher_arc.as_ref(),
                    path,
                    UTF8(|line_number, line| {
                        // Atomic cap enforcement: try to claim a slot.
                        // fetch_add returns the PREVIOUS value, so if
                        // it was already at/over max, our increment
                        // overshot — back it out and stop.
                        let prev = total_ref.fetch_add(1, Ordering::Relaxed);
                        if prev >= max_matches {
                            total_ref.fetch_sub(1, Ordering::Relaxed);
                            truncated_ref.store(true, Ordering::Relaxed);
                            return Ok(false);
                        }
                        let trimmed_line = line.trim_end_matches(['\n', '\r']);
                        let line_bytes = trimmed_line.as_bytes();
                        let (m_start, m_end) = match matcher_for_sink.find(line_bytes) {
                            Ok(Some(m)) => (m.start(), m.end()),
                            _ => (0, 0),
                        };

                        let hit = LiveGrepHit {
                            path: path_str.clone(),
                            line_number,
                            line: trimmed_line.to_string(),
                            match_start: m_start,
                            match_end: m_end,
                        };
                        app_handle.emit(&event_name_local, serde_json::to_value(&hit).unwrap_or_default());
                        Ok(true)
                    }),
                );
            }
        });

        let cancelled = is_live_grep_cancelled(&op_id);
        clear_live_grep_cancel(&op_id);
        Ok(LiveGrepSummary {
            matches: total_matches.load(Ordering::Relaxed),
            files_scanned: files_scanned.load(Ordering::Relaxed),
            cancelled,
            truncated: truncated.load(Ordering::Relaxed),
        })
    })
    .await
    .map_err(|e| format!("Live grep worker failed: {e}"))?
}

/// Escape regex metacharacters for "literal mode" queries. The
/// `regex-syntax` crate has `escape`, but we don't currently depend on
/// it directly (only transitively); doing the small subset by hand
/// keeps the dependency surface tight. Mirrors `regex::escape`.
fn regex_escape(s: &str) -> String {
    const META: &[char] = &[
        '\\', '.', '+', '*', '?', '(', ')', '|', '[', ']', '{', '}', '^', '$', '#', '&', '-', '~',
    ];
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if META.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
