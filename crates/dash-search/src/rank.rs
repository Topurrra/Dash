//! Runs in any app that is a `dash_core::Host`.
//! Unified search-result scorer (Search task #13).
//!
//! File search and content search both rank results by fusing four signals
//! into one number:
//!
//!   * **BM25** — Tantivy's textual relevance score for the hit.
//!   * **lexical quality** — how the query matched the *name / path*: an exact
//!     quoted phrase beats a full-query substring beats exact tokens beats a
//!     prefix beats a fuzzy (edit-distance) match.
//!   * **frecency** — how often and how recently the user opens this exact
//!     item (`frecency::compute_boost`).
//!   * **recency** — how recently the file itself was modified.
//!
//! Each signal is normalised to a comparable 0..1 range, then combined with
//! the named weights in [`WEIGHTS`]. Ranking is therefore tuned in exactly one
//! place — not via magic numbers scattered through `search.rs`.
//!
//! Design decisions (2026-05-17):
//!   1. Preserve today's *textual* ordering — phrase > full-query > exact >
//!      prefix > fuzzy. `lexical_quality` grades in that order.
//!   2. Frecency carries a real weight: a file the user opens constantly may
//!      outrank a slightly better textual match.
//!   3. File recency is a genuine but secondary nudge — a file modified today
//!      ranks above an equally-matching stale one; it never filters.
//!
//! Everything here is pure (no I/O, no globals) so it is trivially testable.

/// Half-life, in days, of the file-recency decay: a file modified this long
/// ago contributes half the recency signal of one modified just now.
const RECENCY_HALF_LIFE_DAYS: f64 = 30.0;

/// Saturation constant for BM25 → 0..1 (`x / (x + k)`). Tantivy BM25 scores for
/// these indexes typically land in the low tens; at `x == k` the normalised
/// signal is 0.5.
const BM25_SATURATION: f32 = 12.0;

/// Saturation constant for the frecency boost → 0..1. `frecency::compute_boost`
/// returns roughly 0..14 (heavy daily use); 6.0 puts "opened a handful of times
/// recently" near the middle of the curve.
const FRECENCY_SATURATION: f32 = 6.0;

/// Fusion weights — the single surface for tuning ranking. Only the *relative*
/// sizes matter. Calibrated 2026-05-17 (13d); retune here if ranking drifts.
pub struct RankWeights {
    /// Weight on normalised BM25 textual relevance.
    pub bm25: f32,
    /// Weight on lexical match quality (name / path match strength).
    pub lexical: f32,
    /// Weight on frecency — deliberately large enough that a heavily-used file
    /// can overcome a modest lexical-quality deficit (decision 2).
    pub frecency: f32,
    /// Weight on file recency — kept small so recency only nudges, never
    /// dominates (decision 3).
    pub recency: f32,
    /// Weight on semantic (embedding cosine) relevance. Kept well below
    /// `lexical`/`bm25` so an exact text match always outranks a merely
    /// conceptual one — semantic only surfaces meaning-related content the
    /// keyword signals miss, and orders it *below* the literal hits. 0 for
    /// every result until the semantic-search beta is enabled (default off).
    pub semantic: f32,
}

/// The active ranking weights — calibrated 2026-05-17 (13d) and confirmed by
/// runtime testing. This `const` is the one place to retune ranking.
pub const WEIGHTS: RankWeights = RankWeights {
    bm25: 1.0,
    lexical: 3.0,
    frecency: 2.2,
    recency: 0.6,
    // Below bm25 so a literal keyword hit always wins; large enough that a
    // strong conceptual match (cosine ~0.7+) still lifts a doc the keywords
    // rate near-zero into the visible tail. Retune here if semantic drifts.
    semantic: 0.8,
};

/// Structural bonus (in final-score units — added straight through `fuse`,
/// outside the four normalised signals) for a result that matches the entry
/// type the query explicitly asked for. Calibrated 2026-05-17 (13d).
pub const FOLDER_FILTER_BONUS: f32 = 1.0;
/// Extra bump when a folder the query filtered for also matches it by name.
pub const FOLDER_FILTER_NAME_BONUS: f32 = 0.8;
/// Bonus for a file-filtered result that is in fact a file.
pub const FILE_FILTER_BONUS: f32 = 0.6;

/// Maximum extension-priority bonus (final-score units), awarded to the FIRST
/// extension of a multi-extension filter. Category lists are ordered
/// most-likely-first (e.g. movies: mp4 > mkv > … > ts), so this makes a real
/// movie outrank a stray `.ts` (usually a TypeScript file) on an otherwise
/// comparable hit. Kept below `WEIGHTS.lexical` so a strong keyword/name match
/// still wins — this only orders results the text signals rate equally.
pub const EXT_PRIORITY_BONUS: f32 = 1.5;

/// Final-score bonus for a result whose extension sits at `position` in a
/// `count`-long extension filter (0 = first / highest priority). Linear from
/// [`EXT_PRIORITY_BONUS`] (first) down toward `EXT_PRIORITY_BONUS / count`
/// (last). Returns 0 for a single-extension or empty filter — nothing to order.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "an extension filter has a handful of entries, exact in f32"
)]
pub fn ext_priority_bonus(position: usize, count: usize) -> f32 {
    if count <= 1 || position >= count {
        return 0.0;
    }
    EXT_PRIORITY_BONUS * (count - position) as f32 / count as f32
}

/// The raw lexical-match tallies for one result — produced by
/// `build_result_item` while it inspects the file name and path, consumed by
/// [`lexical_quality`].
pub struct LexicalHits {
    /// The whole query string appears as a substring of the file name.
    pub full_query_in_name: bool,
    /// The whole query string appears as a substring of the path.
    pub full_query_in_path: bool,
    /// Query keywords that matched an exact token in the name / path.
    pub exact_keyword_hits: usize,
    /// Query keywords that matched only as a token *prefix*.
    pub prefix_keyword_hits: usize,
    /// Query keywords that matched only fuzzily (edit distance 1).
    pub fuzzy_keyword_hits: usize,
    /// Quoted exact phrases found in the file name.
    pub phrase_name_hits: usize,
    /// Quoted exact phrases found in the path.
    pub phrase_path_hits: usize,
    /// Total query keywords — the denominator for keyword-coverage fractions.
    pub total_keywords: usize,
}

/// Collapse the lexical tallies into a single 0..1 match-quality score,
/// grading in the order decision 1 fixes: an exact phrase in the name is the
/// strongest signal, a fuzzy-only match the weakest. The component weights were
/// calibrated 2026-05-17 (13d).
#[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "keyword counts are tiny, exact in f32")]
pub fn lexical_quality(hits: &LexicalHits) -> f32 {
    let mut quality = 0.0f32;

    // Quoted exact phrase — the strongest textual signal.
    if hits.phrase_name_hits > 0 {
        quality += 0.55;
    } else if hits.phrase_path_hits > 0 {
        quality += 0.30;
    }

    // The whole query as a contiguous substring.
    if hits.full_query_in_name {
        quality += 0.30;
    } else if hits.full_query_in_path {
        quality += 0.15;
    }

    // Per-keyword coverage — the fraction of query keywords matched, graded by
    // match strength (exact > prefix > fuzzy).
    if hits.total_keywords > 0 {
        let keywords = hits.total_keywords as f32;
        let exact = hits.exact_keyword_hits as f32 / keywords;
        let prefix = hits.prefix_keyword_hits as f32 / keywords;
        let fuzzy = hits.fuzzy_keyword_hits as f32 / keywords;
        quality += exact * 0.35 + prefix * 0.18 + fuzzy * 0.08;
    }

    quality.clamp(0.0, 1.0)
}

/// 0..1 file-recency signal from a modified timestamp (ms since the Unix
/// epoch). Exponential decay: modified now → ~1.0, modified
/// `RECENCY_HALF_LIFE_DAYS` ago → 0.5, fading smoothly toward 0. A `0`
/// timestamp means "unknown" and scores 0.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "an age in ms only rounds in f64 (far finer than the days it becomes); the decay is in 0..=1, so f64 to f32 only drops precision"
)]
pub fn recency_score(modified_ms: u64, now_ms: u128) -> f32 {
    if modified_ms == 0 {
        return 0.0;
    }
    let age_ms = now_ms.saturating_sub(u128::from(modified_ms)) as f64;
    let age_days = age_ms / (1000.0 * 60.0 * 60.0 * 24.0);
    (-age_days * std::f64::consts::LN_2 / RECENCY_HALF_LIFE_DAYS).exp() as f32
}

/// Squash an unbounded, non-negative signal into 0..1 via `x / (x + k)`. The
/// curve is 0 at 0, 0.5 at `k`, and approaches 1 asymptotically — so a very
/// large raw value can never dominate the weighted sum the way a raw additive
/// score would.
fn saturate(value: f32, k: f32) -> f32 {
    if value <= 0.0 {
        0.0
    } else {
        value / (value + k)
    }
}

/// The per-result inputs to [`fuse`]. `build_result_item` fills this in.
pub struct RankSignals {
    /// Raw Tantivy BM25 score for the hit.
    pub bm25: f32,
    /// Lexical match quality, already 0..1 (from [`lexical_quality`]).
    pub lexical: f32,
    /// Raw frecency boost for this path (`frecency::compute_boost`).
    pub frecency_boost: f32,
    /// File modified time, ms since the Unix epoch; 0 = unknown.
    pub modified_ms: u64,
    /// Structural bonus, already in final-score units — the entry-type-filter
    /// match bumps. Added straight through, outside the four normalised signals.
    pub filter_bonus: f32,
    /// Semantic (embedding cosine) similarity to the query, already 0..1.
    /// 0 when the semantic beta is off, the query wasn't embedded, or this
    /// result has no stored vector — in which case it contributes nothing.
    pub semantic: f32,
}

/// Fuse the per-result signals into one final ranking score; higher is better.
/// The four signals are each normalised to 0..1 and multiplied by their weight
/// in [`WEIGHTS`]; the structural `filter_bonus` is added straight on top.
pub fn fuse(signals: &RankSignals, now_ms: u128) -> f32 {
    let bm25 = saturate(signals.bm25, BM25_SATURATION);
    let lexical = signals.lexical.clamp(0.0, 1.0);
    let frecency = saturate(signals.frecency_boost, FRECENCY_SATURATION);
    let recency = recency_score(signals.modified_ms, now_ms);
    let semantic = signals.semantic.clamp(0.0, 1.0);

    WEIGHTS.bm25 * bm25
        + WEIGHTS.lexical * lexical
        + WEIGHTS.frecency * frecency
        + WEIGHTS.recency * recency
        + WEIGHTS.semantic * semantic
        + signals.filter_bonus
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed "now" so the time-dependent signals are deterministic.
    const NOW_MS: u128 = 1_700_000_000_000;

    fn signals(bm25: f32, lexical: f32, frecency_boost: f32, modified_ms: u64) -> RankSignals {
        RankSignals { bm25, lexical, frecency_boost, modified_ms, filter_bonus: 0.0, semantic: 0.0 }
    }

    fn hits() -> LexicalHits {
        LexicalHits {
            full_query_in_name: false,
            full_query_in_path: false,
            exact_keyword_hits: 0,
            prefix_keyword_hits: 0,
            fuzzy_keyword_hits: 0,
            phrase_name_hits: 0,
            phrase_path_hits: 0,
            total_keywords: 1,
        }
    }

    #[test]
    fn saturate_curve_is_bounded_and_centred_on_k() {
        assert_eq!(saturate(0.0, 12.0), 0.0);
        assert_eq!(saturate(-5.0, 12.0), 0.0); // negative input clamps to 0
        assert!((saturate(12.0, 12.0) - 0.5).abs() < 1e-6); // x == k → 0.5
        let big = saturate(10_000.0, 12.0);
        assert!(big > 0.99 && big < 1.0); // approaches but never reaches 1
    }

    #[test]
    fn recency_score_decays_by_half_life() {
        // A zero timestamp means "unknown" → no recency signal.
        assert_eq!(recency_score(0, NOW_MS), 0.0);
        // Modified just now → ~1.0.
        assert!(recency_score(u64::try_from(NOW_MS).unwrap(), NOW_MS) > 0.99);
        // Modified one half-life (30 days) ago → ~0.5.
        let thirty_days_ms = 30 * 24 * 60 * 60 * 1000u64;
        let half_life_ago = u64::try_from(NOW_MS).unwrap() - thirty_days_ms;
        assert!((recency_score(half_life_ago, NOW_MS) - 0.5).abs() < 0.02);
    }

    #[test]
    fn lexical_quality_grades_phrase_over_fuzzy() {
        // Realistic stacked hits: a phrase match also covers the full query and
        // the exact keywords; a fuzzy match covers neither.
        let mut phrase = hits();
        phrase.phrase_name_hits = 1;
        phrase.full_query_in_name = true;
        phrase.exact_keyword_hits = 1;

        let mut full_query = hits();
        full_query.full_query_in_name = true;
        full_query.exact_keyword_hits = 1;

        let mut exact = hits();
        exact.exact_keyword_hits = 1;

        let mut prefix = hits();
        prefix.prefix_keyword_hits = 1;

        let mut fuzzy = hits();
        fuzzy.fuzzy_keyword_hits = 1;

        let graded = [
            lexical_quality(&phrase),
            lexical_quality(&full_query),
            lexical_quality(&exact),
            lexical_quality(&prefix),
            lexical_quality(&fuzzy),
        ];
        // Strictly descending: phrase > full-query > exact > prefix > fuzzy.
        for pair in graded.windows(2) {
            assert!(pair[0] > pair[1], "expected {} > {}", pair[0], pair[1]);
        }
        for quality in graded {
            assert!((0.0..=1.0).contains(&quality));
        }
    }

    // ── Ranking fixtures: labeled scenario → expected ordering ───────────────

    #[test]
    fn fixture_strong_text_match_outranks_weak() {
        let strong = signals(5.0, 1.0, 0.0, 0);
        let weak = signals(5.0, 0.08, 0.0, 0);
        assert!(fuse(&strong, NOW_MS) > fuse(&weak, NOW_MS));
    }

    #[test]
    fn fixture_frecency_overcomes_modest_lexical_deficit() {
        // A file the user opens constantly whose name matches a touch worse...
        let frequent = signals(5.0, 0.5, 14.0, 0);
        // ...should beat a slightly better name match the user never opens.
        let never_opened = signals(5.0, 0.7, 0.0, 0);
        assert!(fuse(&frequent, NOW_MS) > fuse(&never_opened, NOW_MS), "a heavily-used file should win on frecency");
    }

    #[test]
    fn fixture_recency_nudges_but_never_dominates() {
        // Same signals, different mtime → the fresher file wins.
        let fresh = signals(5.0, 0.5, 0.0, u64::try_from(NOW_MS).unwrap());
        let stale = signals(5.0, 0.5, 0.0, 0);
        assert!(fuse(&fresh, NOW_MS) > fuse(&stale, NOW_MS));

        // But recency must not overturn a real lexical gap.
        let recent_weak = signals(5.0, 0.2, 0.0, u64::try_from(NOW_MS).unwrap());
        let stale_strong = signals(5.0, 0.8, 0.0, 0);
        assert!(fuse(&stale_strong, NOW_MS) > fuse(&recent_weak, NOW_MS), "recency must stay a secondary nudge");
    }

    #[test]
    fn fixture_filter_bonus_adds_straight_through() {
        let plain = signals(5.0, 0.5, 1.0, 0);
        let bonused = RankSignals { filter_bonus: FOLDER_FILTER_BONUS, ..signals(5.0, 0.5, 1.0, 0) };
        let diff = fuse(&bonused, NOW_MS) - fuse(&plain, NOW_MS);
        assert!((diff - FOLDER_FILTER_BONUS).abs() < 1e-4);
    }

    #[test]
    fn weights_keep_lexical_dominant() {
        // Checked at compile time: the weights are constants.
        const {
            assert!(WEIGHTS.lexical > WEIGHTS.bm25);
            assert!(WEIGHTS.lexical > WEIGHTS.frecency);
            assert!(WEIGHTS.frecency > WEIGHTS.recency);
            // Semantic must never outweigh a literal keyword hit: an exact text
            // match always beats a merely conceptual one.
            assert!(WEIGHTS.lexical > WEIGHTS.semantic);
            assert!(WEIGHTS.bm25 >= WEIGHTS.semantic);
        }
    }

    #[test]
    fn semantic_lifts_but_stays_below_a_real_lexical_hit() {
        // Same weak text signals, one with a strong semantic match → it wins.
        let related = RankSignals { semantic: 0.9, ..signals(0.5, 0.1, 0.0, 0) };
        let unrelated = signals(0.5, 0.1, 0.0, 0);
        assert!(fuse(&related, NOW_MS) > fuse(&unrelated, NOW_MS));

        // But a pure-semantic hit must not overturn a strong literal match.
        let semantic_only = RankSignals { semantic: 1.0, ..signals(0.0, 0.0, 0.0, 0) };
        let lexical_hit = signals(5.0, 0.9, 0.0, 0);
        assert!(
            fuse(&lexical_hit, NOW_MS) > fuse(&semantic_only, NOW_MS),
            "an exact text match must outrank a pure semantic match"
        );
    }

    #[test]
    fn ext_priority_orders_earlier_extensions_higher() {
        // movies = [mp4, mkv, avi, mov, m4v, wmv, ts, m2ts] → 8 entries.
        let mp4 = ext_priority_bonus(0, 8);
        let mkv = ext_priority_bonus(1, 8);
        let ts = ext_priority_bonus(6, 8);
        assert!(mp4 > mkv && mkv > ts, "earlier extensions must rank higher");
        // The mp4→ts gap must beat the recency nudge so a real movie outranks a
        // merely-fresher stray .ts.
        assert!(mp4 - ts > WEIGHTS.recency);
        // Nothing to order with a single-extension (or empty) filter.
        assert_eq!(ext_priority_bonus(0, 1), 0.0);
        assert_eq!(ext_priority_bonus(0, 0), 0.0);
        // Stays below the lexical ceiling so keyword relevance still wins.
        assert!(mp4 < WEIGHTS.lexical);
    }
}
