//! Sensitive content detection.
//!
//! Scans extracted text for credentials, API keys, private keys, database
//! connection strings, and PII patterns. Runs at index time so users can
//! later find "files I should worry about" via a search filter (`kind:sensitive`)
//! or visual badge on results, and so the Privacy Audit can report
//! unencrypted secrets sitting on disk.
//!
//! Privacy: all detection runs locally during indexing. The matched *kinds*
//! are stored in the search index (so we can surface them in UI), but the
//! matched *contents themselves* are never extracted — only the boolean
//! "this file contains a credential of kind X". Users can find sensitive
//! files without anyone (including the app) seeing what the secrets are.
//!
//! ## Phase 6.5-1 (2026-05-27): tiers, spans, fancy-regex
//!
//! The kinds-only API was promoted to a span-aware `Finding` shape so the
//! PrivacyBlur primitive can wrap exact match positions in blurred spans.
//! Three explicit tiers replace the implicit "high-confidence vs. validated"
//! split:
//!
//!   • **HIGH** — prefixed / structured tokens (AWS `AKIA…`, GitHub `ghp_…`,
//!     Stripe `sk_live_…`, PEM, PGP, Ethereum, Bitcoin WIF, JWT, npm, Slack,
//!     OpenAI), plus Luhn-validated credit cards and DB URLs with embedded
//!     passwords. All structurally unambiguous → default to BLUR ON.
//!   • **MEDIUM** — validated noisy shapes: generic `key = value`
//!     assignments, `Bearer …` headers, structurally-valid US SSNs. Default
//!     to BLUR OFF in clipboard / audit; still surfaced to the Secret Leak
//!     Scanner tool.
//!   • **LOW** (off by default everywhere) — generic PII shapes: email,
//!     phone, IPv4. Only surfaces when a tool / setting explicitly opts in.
//!
//! ## Engine choice per tier
//!
//! HIGH stays on `regex::RegexSet` — one O(n) pass for "does *anything*
//! match?", then a per-pattern `Regex::find_iter` extracts spans only for
//! the rows that hit. RegexSet's parallelism is the whole point.
//!
//! MEDIUM moves to **`fancy-regex`** so we can embed negative-lookahead
//! placeholder rejection inside the pattern: `(?!process\.env|getenv|YOUR|
//! EXAMPLE|\$|%)` rejects env refs and common placeholders BEFORE
//! producing a match. The Rust validator (`is_plausible_secret_value`)
//! still handles the parts regex can't — entropy gating for unquoted
//! values, all-same-char mask detection, sub-substring placeholder words
//! that would make the regex unreadable.
//!
//! LOW uses plain `regex` (no lookahead needed; just structural shapes).
//!
//! Credit card / SSN keep their Rust validators (Luhn math; SSN
//! never-assigned-range table — neither expressible as regex).

use fancy_regex::Regex as FancyRegex;
use regex::{Regex, RegexSet};
use serde::Serialize;
use std::sync::LazyLock;

// ─── Tier enum + Finding shape ────────────────────────────────────────

/// Confidence tier. Drives the default blur policy and surface visibility
/// (see module docs for the full rationale).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    High,
    Medium,
    Low,
}

/// One sensitive-content finding with its byte-range span in the scanned
/// text. The span lets the UI wrap `text[start..end]` in a blurred span
/// (click-to-reveal); the kind drives any tier-aware filtering.
///
/// Spans always point at the SECRET ITSELF, not at the surrounding
/// context — for a `password = "hunter2"` match we report `[start..end]`
/// of `hunter2`, not of the whole assignment. That keeps the blur tight
/// (only the secret hides, the keyword stays readable so the user knows
/// what they're looking at).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub kind: &'static str,
    pub tier: Tier,
    pub start: usize,
    pub end: usize,
}

// ─── HIGH-tier patterns ───────────────────────────────────────────────
//
// Prefixed / structured tokens. Each row is `(label, regex, tier)`. Tier
// is always HIGH here by construction — this table only holds shapes
// whose structure alone gives a strong signal. Noisier shapes live in
// the fancy-regex passes below.

const PATTERNS: &[(&str, &str, Tier)] = &[
    // ─── Cloud & SaaS API keys ─────────────────────────────────────────
    // AWS access key — `AKIA…` (long-term) or `ASIA…` (session).
    ("aws_access_key", r"\b(AKIA|ASIA)[A-Z0-9]{16}\b", Tier::High),
    // GitHub tokens — classic (`ghp_/gho_/ghu_/ghs_/ghr_`).
    ("github_token", r"\bgh[opusr]_[A-Za-z0-9]{36,}", Tier::High),
    // GitHub fine-grained PAT.
    ("github_token", r"\bgithub_pat_[A-Za-z0-9_]{30,}", Tier::High),
    // Slack tokens have a strict prefix.
    ("slack_token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}", Tier::High),
    // Slack incoming-webhook URL — leaks a post-to-channel capability.
    (
        "slack_webhook",
        r"https://hooks\.slack\.com/services/T[A-Za-z0-9]+/B[A-Za-z0-9]+/[A-Za-z0-9]+",
        Tier::High,
    ),
    // Stripe live/test keys.
    (
        "stripe_secret_key",
        r"\b(sk|rk)_(live|test)_[A-Za-z0-9]{24,}",
        Tier::High,
    ),
    // Google API key.
    ("google_api_key", r"\bAIza[0-9A-Za-z_-]{35}\b", Tier::High),
    // Twilio account SID.
    ("twilio_account_sid", r"\bAC[a-f0-9]{32}\b", Tier::High),
    // SendGrid API key.
    (
        "sendgrid_api_key",
        r"\bSG\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{40,}",
        Tier::High,
    ),
    // JWT tokens — three base64url chunks separated by dots, header `eyJ`.
    (
        "jwt_token",
        r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b",
        Tier::High,
    ),
    // ─── Private keys ──────────────────────────────────────────────────
    (
        "private_key_pem",
        r"-----BEGIN ([A-Z]+ )*PRIVATE KEY( BLOCK)?-----",
        Tier::High,
    ),
    (
        "pgp_private_key",
        r"-----BEGIN PGP PRIVATE KEY BLOCK-----",
        Tier::High,
    ),
    // ─── Connection strings ───────────────────────────────────────────
    (
        "database_url_with_password",
        r"\b(postgres|postgresql|mysql|mongodb|mongodb\+srv|redis|amqp)://[^\s:/]+:[^\s@]+@[^\s/]+",
        Tier::High,
    ),
    // ─── Crypto keys ──────────────────────────────────────────────────
    ("ethereum_private_key", r"\b0x[a-fA-F0-9]{64}\b", Tier::High),
    (
        "bitcoin_wif_private_key",
        r"\b[5KL][1-9A-HJ-NP-Za-km-z]{50,51}\b",
        Tier::High,
    ),
    // ─── More cloud / package-registry tokens ─────────────────────────
    // Anthropic BEFORE OpenAI — the `sk-ant-` prefix is more specific, and a
    // small post-filter in `scan_text_detailed` drops the openai duplicate that
    // the (lookahead-free) `regex` crate would otherwise also emit for it.
    ("anthropic_api_key", r"\bsk-ant-[A-Za-z0-9_\-]{20,}", Tier::High),
    (
        "openai_api_key",
        r"\bsk-(?:proj-)?[A-Za-z0-9_\-]{20,}",
        Tier::High,
    ),
    ("npm_token", r"\bnpm_[A-Za-z0-9]{36,}\b", Tier::High),
    // ─── Crypto wallet addresses ──────────────────────────────────────
    // Deterministic prefix/length → near-zero false positives. The
    // ethereum *private* key (64 hex) is matched above; these are the
    // *public* wallet addresses.
    // Bitcoin P2PKH (1…) / P2SH (3…) — Base58Check, 25-34 chars total.
    ("bitcoin_address", r"\b[13][1-9A-HJ-NP-Za-km-z]{24,33}\b", Tier::High),
    // Bitcoin bech32 mainnet (bc1…) — SegWit v0/v1.
    ("bitcoin_bech32", r"\bbc1[ac-hj-np-z02-9]{39,59}\b", Tier::High),
    // Ethereum wallet address — exactly 40 hex chars (private key is 64).
    ("ethereum_address", r"\b0x[a-fA-F0-9]{40}\b", Tier::High),
    // Solana base58 public key — always exactly 44 Base58 chars.
    ("solana_address", r"\b[1-9A-HJ-NP-Za-km-z]{44}\b", Tier::High),
];

/// HIGH-tier RegexSet — O(n) "does anything match?" first-pass. Spans
/// are NOT extracted here; this is the fast filter.
static REGEX_SET: LazyLock<RegexSet> =
    LazyLock::new(|| RegexSet::new(PATTERNS.iter().map(|(_, p, _)| *p)).expect("invalid pattern"));

/// HIGH-tier patterns compiled individually for span extraction. Same
/// indexing as PATTERNS, so a RegexSet hit at index N → INDIVIDUAL[N].
static INDIVIDUAL_REGEXES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    PATTERNS
        .iter()
        .map(|(_, p, _)| Regex::new(p).expect("invalid pattern"))
        .collect()
});

// ─── MEDIUM-tier: fancy-regex with inline placeholder rejection ───────

const SECRET_KEYWORDS: &str = r"(?:api[_-]?key|secret[_-]?key|access[_-]?key|access[_-]?token|auth[_-]?key|auth[_-]?token|bearer[_-]?token|refresh[_-]?token|private[_-]?key|encryption[_-]?key|client[_-]?secret|passphrase|password|passwd)";

/// Generic `key = value` detector with inline rejection of env refs +
/// common placeholder PREFIXES. The Rust validator handles the rest
/// (entropy gate for unquoted values, sub-substring placeholders,
/// all-same-char masks). Captures:
///   group 1 = opening quote (empty if unquoted)
///   group 2 = the value itself  ← we report SPAN over this
///
/// The negative lookahead after `=` rejects:
///   - `$VAR` / `%VAR%` env-var references
///   - `process.env.X` / `os.environ` / `getenv(...)` / `config.X`
///   - `{{template_var}}` templating
///   - common placeholder prefixes: YOUR_, EXAMPLE_, PLACEHOLDER, REDACTED,
///     CHANGEME, TODO, FIXME (in any case)
static CRED_FANCY: LazyLock<FancyRegex> = LazyLock::new(|| {
    let pattern = format!(
        "(?i){SECRET_KEYWORDS}{}",
        // `["']?\s*[:=]\s*` — optional quote then separator.
        // The big `(?!...)` is the inline rejection.
        // `(["']?)` captures the opening quote (group 1).
        // `([^\s"'<>{}]{6,})` captures the value (group 2) — at least
        //   6 chars, no whitespace / quotes / structural chars.
        r#"["']?\s*[:=]\s*(?!\$|%|process\.env|os\.environ|import\.meta\.env|getenv|config\.|settings\.|\{\{|your[_\- ]|example[_\- ]|placeholder|redacted|changeme|changethis|fixme|todo[_\- ])(["']?)([^\s"'<>{}]{6,})"#
    );
    FancyRegex::new(&pattern).expect("invalid credential regex")
});

/// `Bearer <token>` detector with inline placeholder-prefix rejection
/// (YOUR / EXAMPLE / etc. as the literal start of the token). Token is
/// group 1; that's the span we report. The Rust validator still gates
/// on length + sub-substring placeholders for the rest.
static BEARER_FANCY: LazyLock<FancyRegex> = LazyLock::new(|| {
    FancyRegex::new(
        r"(?i)\bbearer\s+(?!\$|your|example|placeholder|redacted|todo|test_|dummy|sample|xxx)([A-Za-z0-9_\-\.]{20,})"
    )
    .expect("invalid bearer regex")
});

/// US SSN candidate — area/group/serial captured for structural validation.
static SSN_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d{3})-(\d{2})-(\d{4})\b").expect("invalid ssn regex"));

/// Credit-card candidate finder. Matches 13–19 digit runs (with optional
/// spaces/dashes); each candidate is Luhn-validated downstream.
static CREDIT_CARD_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:\d[ -]?){12,18}\d\b").expect("invalid credit card regex")
});

// ─── LOW-tier (off by default) ─────────────────────────────────────────
//
// Generic PII shapes. NOT surfaced in clipboard / audit unless the user
// opts in — emails / phones / IPs are too common in normal documents to
// flag every time. The Secret Leak Scanner tool's Privacy preset opts in.

const LOW_PATTERNS: &[(&str, &str)] = &[
    (
        "email",
        r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b",
    ),
    // Phone — international or national-format. Loose by design (low tier).
    (
        "phone",
        r"\b(?:\+?\d{1,3}[ \-\.]?)?(?:\(\d{1,4}\)|\d{1,4})[ \-\.]?\d{3,4}[ \-\.]?\d{4}\b",
    ),
    // IPv4 — four dotted octets. We don't structurally validate ranges
    // (RFC 1918 / link-local / loopback) — too low tier to bother.
    ("ipv4", r"\b(?:\d{1,3}\.){3}\d{1,3}\b"),
];

static LOW_REGEXES: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    LOW_PATTERNS
        .iter()
        .map(|(label, p)| (*label, Regex::new(p).expect("invalid low pattern")))
        .collect()
});

// ─── Ported PII catalog (always-on MEDIUM) ─────────────────────────────
//
// Ported additively from an earlier tiered detector. These
// are ALWAYS-ON (run regardless of `include_low`) because each is either
// structurally distinctive, checksum-validated, or label-anchored — so
// the false-positive rate is low enough to surface them by default,
// alongside SSN / credit-card. Noisier bare-digit / generic-PII shapes
// live in the `include_low` gate further down.

// IBAN — two-letter country + two check digits + 11–30 alphanumerics. The
// 2-letter/2-digit prefix makes this specific enough to avoid most false
// positives; mod-97 validation is future hardening.
static IBAN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Z]{2}\d{2}[A-Z0-9]{11,30}\b").expect("invalid iban regex"));

// Italian Codice Fiscale: 6 letters + 2 digits + letter + 2 digits + letter
// + 3 digits + letter (16 chars). Distinctive letter/digit alternation.
static ITALIAN_CF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Z]{6}\d{2}[A-Z]\d{2}[A-Z]\d{3}[A-Z]\b").expect("invalid italian cf regex")
});

// UK National Insurance — 2 prefix letters + 6 digits + suffix A–D.
static UK_NINO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[ABCEGHJ-PRSTW-Z][ABCEGHJ-NPRSTW-Z]\s?\d{2}\s?\d{2}\s?\d{2}\s?[A-D]\b")
        .expect("invalid uk nino regex")
});

// France NIR (social security) — sex(1/2)+YY+MM(01-12)+dept(2)+commune(3)+order(3)[+key(2)].
static FRANCE_NIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[12]\s?\d{2}\s?(?:0[1-9]|1[0-2])\s?\d{2}\s?\d{3}\s?\d{3}(?:\s?\d{2})?\b")
        .expect("invalid france nir regex")
});

// VIN — 17 chars, no I/O/Q. Pass requires a mix of letters+digits (real VINs).
static VIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-HJ-NPR-Z0-9]{17}\b").expect("invalid vin regex"));

// Spain DNI (8 digits + letter) / NIE ([XYZ] + 7 digits + letter); check-letter validated.
static SPAIN_DNI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([XYZ]?)(\d{7,8})-?([A-Z])\b").expect("invalid dni regex"));

// US ITIN — SSN-shaped but 9xx area (the numbers the SSN validator rejects).
static US_ITIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b9\d{2}-(?:7\d|8[0-8]|9[0-2]|9[4-9])-\d{4}\b").expect("invalid itin regex")
});

// Contextual (label → value). Capture group 1 is the value we flag.
static PASSPORT_CTX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bpassport\s*(?:no\.?|number|num|#)?\s*[:#]?\s*([A-Z0-9]{6,9})\b")
        .expect("invalid passport regex")
});
static DL_CTX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:driver'?s?\s*licen[sc]e|driving\s*licen[sc]e)\s*(?:no\.?|number|num|#)?\s*[:#]?\s*([A-Z0-9-]{5,16})\b").expect("invalid dl regex")
});
static ACCT_CTX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:bank\s*)?acc(?:oun)?t\.?\s*(?:no\.?|number|num|#)?\s*[:#]?\s*(\d{6,17})\b",
    )
    .expect("invalid acct regex")
});
static MRN_CTX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:MRN|medical\s*record(?:\s*(?:no\.?|number|num|#))?)\s*[:#]?\s*([A-Z0-9-]{5,12})\b").expect("invalid mrn regex")
});
static DOB_CTX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:DOB|date\s*of\s*birth|born)\s*(?:on)?\s*[:#]?\s*(\d{1,4}[/.\-]\d{1,2}[/.\-]\d{1,4})\b").expect("invalid dob regex")
});
static ROUTING_CTX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:routing|aba)\s*(?:no\.?|number|num|#|transit)?\s*[:#]?\s*(\d{9})\b")
        .expect("invalid routing regex")
});

// ─── Ported PII catalog (include_low-gated) ────────────────────────────
//
// Noisier shapes — bare digit runs / generic PII. Gated behind
// `include_low` next to email/phone/ipv4 so the default index path
// doesn't flood every document.

// MAC address — six hex pairs, `:`/`-` separated.
static MAC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:[0-9A-Fa-f]{2}[:-]){5}[0-9A-Fa-f]{2}\b").expect("invalid mac regex")
});

// Date — ISO `YYYY-MM-DD` or `M/D/YYYY`. Inherently noisy (any date matches).
static DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b\d{4}-\d{2}-\d{2}\b|\b\d{1,2}/\d{1,2}/\d{2,4}\b").expect("invalid date regex")
});

// Georgian national ID ("personal number") — bare 11-digit run. No public
// checksum; surfaced as a suggestion only (hence include_low-gated).
static GEORGIAN_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{11}\b").expect("invalid georgian id regex"));

// Local Georgian mobile: 9 digits starting with 5, often grouped 3-2-2-2.
// International +995 numbers are caught by the generic phone detector.
static GEORGIAN_MOBILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b5\d{2}[\s.\-]?\d{2}[\s.\-]?\d{2}[\s.\-]?\d{2}\b")
        .expect("invalid georgian mobile regex")
});

// India Aadhaar: 12-digit UID, first digit 2-9, contiguous or space/dash-grouped.
static INDIA_AADHAAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[2-9]\d{3}[ -]?\d{4}[ -]?\d{4}\b").expect("invalid aadhaar regex"));

// US EIN — employer ID, `NN-NNNNNNN` (distinctive dash position).
static US_EIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{2}-\d{7}\b").expect("invalid ein regex"));

// Dutch BSN — 8 or 9 digit run, modulo-11 (elfproef) validated downstream.
static DUTCH_BSN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{8,9}\b").expect("invalid bsn regex"));

// ─── Scan API ──────────────────────────────────────────────────────────

/// Span-aware scan returning every finding with its byte range. Used by
/// the Secret Leak Scanner UI and the PrivacyBlur primitive.
///
/// `include_low` opts in to LOW-tier patterns (email/phone/IP); the
/// default-OFF behavior matches the "no PII flood on every document"
/// stance — only tools that explicitly want PII surface it.
///
/// Findings are sorted by start position. Overlapping spans are NOT
/// deduplicated — multiple detectors can legitimately point at the same
/// region, and the UI chooses how to render (highest tier wins typically).
pub fn scan_text_detailed(text: &str, include_low: bool) -> Vec<Finding> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut findings: Vec<Finding> = Vec::new();

    // 1. HIGH tier — RegexSet pre-filter, then per-pattern find_iter for
    //    spans. When nothing matches (common case for innocent text)
    //    we only pay the one RegexSet pass.
    let hits = REGEX_SET.matches(text);
    for idx in hits {
        let (label, _, tier) = PATTERNS[idx];
        for m in INDIVIDUAL_REGEXES[idx].find_iter(text) {
            findings.push(Finding {
                kind: label,
                tier,
                start: m.start(),
                end: m.end(),
            });
        }
    }

    // 2. HIGH — credit cards (Luhn + IIN + placeholder checks; not fully
    //    expressible as regex). Three gates in sequence:
    //      (a) Luhn mod-10 checksum
    //      (b) IIN structural validation — MII must be a bank-card range and
    //          length must match the brand derived from the prefix
    //      (c) Placeholder rejection — fewer than 3 distinct digit values means
    //          a test/example card (e.g. "4111 1111 1111 1111"), not a real one
    for m in CREDIT_CARD_REGEX.find_iter(text) {
        let digits: String = m.as_str().chars().filter(|c| c.is_ascii_digit()).collect();
        if !(13..=19).contains(&digits.len()) {
            continue;
        }
        if !luhn_valid(&digits) {
            continue;
        }
        if !is_valid_card_iin(&digits) || is_monotonic_placeholder(&digits) {
            continue;
        }
        findings.push(Finding {
            kind: "credit_card",
            tier: Tier::High,
            start: m.start(),
            end: m.end(),
        });
    }

    // 3. MEDIUM — generic key=value (fancy-regex inline rejection +
    //    Rust validator for the rest).
    for caps_result in CRED_FANCY.captures_iter(text) {
        let Ok(caps) = caps_result else { continue };
        let quoted = caps.get(1).is_some_and(|m| !m.as_str().is_empty());
        let Some(value_match) = caps.get(2) else { continue };
        if is_plausible_secret_value(value_match.as_str(), quoted) {
            findings.push(Finding {
                kind: "generic_credential_assignment",
                tier: Tier::Medium,
                start: value_match.start(),
                end: value_match.end(),
            });
        }
    }

    // 4. MEDIUM — Bearer token.
    for caps_result in BEARER_FANCY.captures_iter(text) {
        let Ok(caps) = caps_result else { continue };
        let Some(token_match) = caps.get(1) else { continue };
        if is_plausible_secret_value(token_match.as_str(), false) {
            findings.push(Finding {
                kind: "bearer_token",
                tier: Tier::Medium,
                start: token_match.start(),
                end: token_match.end(),
            });
        }
    }

    // 5. MEDIUM — US SSN (structural-validity gate).
    for caps in SSN_REGEX.captures_iter(text) {
        let area = &caps[1];
        let group = &caps[2];
        let serial = &caps[3];
        if area == "000" || area == "666" || area.starts_with('9') {
            continue;
        }
        if group == "00" || serial == "0000" {
            continue;
        }
        let m = caps.get(0).expect("ssn match exists");
        findings.push(Finding {
            kind: "us_ssn",
            tier: Tier::Medium,
            start: m.start(),
            end: m.end(),
        });
    }

    // 5b. MEDIUM — always-on ported PII catalog. Each detector is
    //     structurally distinctive, checksum-validated, or label-anchored,
    //     so it stays on regardless of `include_low`.
    push_full_matches(&mut findings, &IBAN, text, "iban");
    push_full_matches(&mut findings, &ITALIAN_CF, text, "italian_cf");
    push_full_matches(&mut findings, &UK_NINO, text, "uk_nino");
    push_full_matches(&mut findings, &FRANCE_NIR, text, "france_nir");
    push_full_matches(&mut findings, &US_ITIN, text, "us_itin");

    // VIN — require a letter+digit mix (drops 17-char all-digit / all-letter runs).
    for m in VIN.find_iter(text) {
        let s = &text[m.start()..m.end()];
        if s.bytes().any(|b| b.is_ascii_alphabetic()) && s.bytes().any(|b| b.is_ascii_digit()) {
            findings.push(Finding {
                kind: "vin",
                tier: Tier::Medium,
                start: m.start(),
                end: m.end(),
            });
        }
    }

    // Spain DNI (8 digits) / NIE ([XYZ]+7 digits) — check-letter validated.
    for caps in SPAIN_DNI.captures_iter(text) {
        let whole = caps.get(0).expect("group 0 always present");
        let prefix = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let digits = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let letter = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let len_ok =
            (prefix.is_empty() && digits.len() == 8) || (!prefix.is_empty() && digits.len() == 7);
        if len_ok && is_valid_dni(prefix, digits, letter) {
            findings.push(Finding {
                kind: "spain_dni",
                tier: Tier::Medium,
                start: whole.start(),
                end: whole.end(),
            });
        }
    }

    // Contextual (label → value). Span = capture group 1 (the value).
    push_captured(&mut findings, &PASSPORT_CTX, text, "passport");
    push_captured(&mut findings, &DL_CTX, text, "drivers_license");
    push_captured(&mut findings, &ACCT_CTX, text, "bank_account");
    push_captured(&mut findings, &MRN_CTX, text, "medical_record_number");
    push_captured(&mut findings, &DOB_CTX, text, "dob");

    // Routing — contextual + ABA checksum.
    for caps in ROUTING_CTX.captures_iter(text) {
        if let Some(v) = caps.get(1) {
            if is_valid_aba(v.as_str()) {
                findings.push(Finding {
                    kind: "routing_number",
                    tier: Tier::Medium,
                    start: v.start(),
                    end: v.end(),
                });
            }
        }
    }

    // 6. LOW tier — only when caller explicitly opts in.
    if include_low {
        for (label, regex) in LOW_REGEXES.iter() {
            for m in regex.find_iter(text) {
                findings.push(Finding {
                    kind: *label,
                    tier: Tier::Low,
                    start: m.start(),
                    end: m.end(),
                });
            }
        }

        // Ported include_low-gated catalog — noisy bare-digit / generic PII.
        push_low_matches(&mut findings, &MAC, text, "mac_address");
        push_low_matches(&mut findings, &DATE, text, "date");
        push_low_matches(&mut findings, &GEORGIAN_ID, text, "georgian_id");
        push_low_matches(&mut findings, &INDIA_AADHAAR, text, "india_aadhaar");
        push_low_matches(&mut findings, &US_EIN, text, "us_ein");
        // Local Georgian mobiles (no +995 prefix) — emitted as "phone".
        push_low_matches(&mut findings, &GEORGIAN_MOBILE, text, "phone");
        // Dutch BSN — 8-9 digits, modulo-11 validated.
        for m in DUTCH_BSN.find_iter(text) {
            if is_valid_bsn(&text[m.start()..m.end()]) {
                findings.push(Finding {
                    kind: "dutch_bsn",
                    tier: Tier::Low,
                    start: m.start(),
                    end: m.end(),
                });
            }
        }
    }

    // Targeted dedup: an `sk-ant-…` key matches BOTH the anthropic pattern
    // AND the broader openai pattern (the `regex` crate has no lookahead).
    // Drop any openai finding fully covered by an anthropic finding. This is
    // intentionally narrow — the module's general "overlapping spans are NOT
    // deduplicated" contract is preserved for every other detector.
    let anthropic_spans: Vec<(usize, usize)> = findings
        .iter()
        .filter(|f| f.kind == "anthropic_api_key")
        .map(|f| (f.start, f.end))
        .collect();
    if !anthropic_spans.is_empty() {
        findings.retain(|f| {
            f.kind != "openai_api_key"
                || !anthropic_spans
                    .iter()
                    .any(|&(s, e)| s <= f.start && e >= f.end)
        });
    }

    findings.sort_by_key(|f| (f.start, f.end));
    findings
}

/// Push every full-match span of `re` as a MEDIUM finding labelled `kind`.
fn push_full_matches(out: &mut Vec<Finding>, re: &Regex, text: &str, kind: &'static str) {
    for m in re.find_iter(text) {
        out.push(Finding {
            kind,
            tier: Tier::Medium,
            start: m.start(),
            end: m.end(),
        });
    }
}

/// Push every full-match span of `re` as a LOW finding labelled `kind`.
fn push_low_matches(out: &mut Vec<Finding>, re: &Regex, text: &str, kind: &'static str) {
    for m in re.find_iter(text) {
        out.push(Finding {
            kind,
            tier: Tier::Low,
            start: m.start(),
            end: m.end(),
        });
    }
}

/// Push capture-group-1 (the value after a label) of every match as a
/// MEDIUM finding labelled `kind`.
fn push_captured(out: &mut Vec<Finding>, re: &Regex, text: &str, kind: &'static str) {
    for caps in re.captures_iter(text) {
        if let Some(v) = caps.get(1) {
            out.push(Finding {
                kind,
                tier: Tier::Medium,
                start: v.start(),
                end: v.end(),
            });
        }
    }
}

/// Legacy kinds-only scan, preserved for the index path and any caller
/// that doesn't need spans. Returns a deduped sorted list of kind
/// labels with `"sensitive"` prepended when anything matched.
///
/// LOW tier is intentionally off here — the index path doesn't want
/// every document that mentions an email tagged `kind:sensitive`.
/// Callers that want PII labels use `scan_text_detailed(text, true)`
/// directly.
pub fn scan_text(text: &str) -> Vec<&'static str> {
    let findings = scan_text_detailed(text, false);
    if findings.is_empty() {
        return Vec::new();
    }
    let mut kinds: Vec<&'static str> = findings.into_iter().map(|f| f.kind).collect();
    kinds.sort();
    kinds.dedup();
    // Universal tag so `kind:sensitive` finds anything regardless of the
    // specific category.
    kinds.insert(0, "sensitive");
    kinds
}

/// File preview with sensitive findings — Phase 6.5-5 (2026-05-27,
/// revised to return EXCERPTS only).
///
/// We don't return the whole file: a 5000-line config with 3 secret
/// lines would force the user to scroll for the actual hits. Instead
/// the file is scanned, then trimmed to just the lines that triggered
/// (each window includes ±2 lines of context for readability).
/// Adjacent / overlapping windows are merged so the user sees a small
/// number of focused excerpts.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilePreview {
    /// One excerpt per merged hit window. Empty when nothing matched.
    pub excerpts: Vec<PreviewExcerpt>,
    /// Total findings in the file (sum of all excerpts' finding counts).
    pub total_findings: usize,
    /// True if the file was longer than `PREVIEW_MAX_BYTES` and the
    /// scan only saw the head. Surface this so the user knows to open
    /// the file directly for the rest.
    pub truncated: bool,
    pub error: Option<String>,
}

/// One excerpt — a contiguous slice of the file containing one or more
/// findings, plus ±N lines of context. Spans inside `findings` are
/// REWRITTEN to be relative to `content` (the excerpt slice), so the
/// PrivacyBlur primitive on the frontend Just Works against this
/// `text` + `findings` pair.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewExcerpt {
    /// 1-indexed source-file line number where this excerpt begins.
    /// Drives the "Line 42" header in the UI.
    pub line_start: usize,
    /// Excerpt text — typically a few lines.
    pub content: String,
    /// Findings inside this excerpt, with byte-spans rewritten to be
    /// relative to `content`.
    pub findings: Vec<Finding>,
}

/// 64 KB preview cap — fits the typical `.env` (few KB), a moderate
/// log excerpt, or the first chunk of a giant file.
pub const PREVIEW_MAX_BYTES: usize = 64 * 1024;

/// Default lines of context around each hit. 2 lines on each side is
/// enough to anchor an `API_KEY=…` in a `.env` or a credential
/// assignment in a Python config, without dragging in unrelated noise.
pub const EXCERPT_CONTEXT_LINES: usize = 2;

/// Build excerpts: for each finding, take ±`context_lines` of context,
/// merge overlapping windows, and rewrite span offsets to be relative
/// to the excerpt slice. Empty input → empty output (no excerpt at all
/// when there's nothing to highlight).
pub fn extract_excerpts(
    content: &str,
    findings: &[Finding],
    context_lines: usize,
) -> Vec<PreviewExcerpt> {
    if findings.is_empty() || content.is_empty() {
        return Vec::new();
    }

    // Line-start byte offsets. `line_starts[i]` is the byte index where
    // line `i` (0-based) begins. Always has at least one element (0).
    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' && i + 1 < content.len() {
            line_starts.push(i + 1);
        }
    }
    let total_lines = line_starts.len();

    // Byte offset → 0-based line index. Binary-search the line-start
    // table; the line is the largest start ≤ byte_pos.
    let line_at = |byte_pos: usize| -> usize {
        match line_starts.binary_search(&byte_pos) {
            Ok(idx) => idx,
            Err(idx) => idx.saturating_sub(1),
        }
    };

    // For each finding, compute its [first_line, last_line] window
    // INCLUSIVE on both ends, with ±context_lines applied.
    let mut windows: Vec<(usize, usize)> = findings
        .iter()
        .map(|f| {
            let start_line = line_at(f.start);
            // end-1 because spans are half-open and a span ending right at
            // a newline would otherwise point at the next line.
            let last_byte = f.end.saturating_sub(1).max(f.start);
            let end_line = line_at(last_byte);
            let w_start = start_line.saturating_sub(context_lines);
            let w_end = (end_line + context_lines).min(total_lines - 1);
            (w_start, w_end)
        })
        .collect();
    windows.sort_by_key(|w| w.0);

    // Merge overlapping or adjacent windows (gap ≤ 1 line collapses).
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for w in windows {
        if let Some(last) = merged.last_mut() {
            if w.0 <= last.1 + 1 {
                last.1 = last.1.max(w.1);
                continue;
            }
        }
        merged.push(w);
    }

    // Build one excerpt per merged window.
    let mut excerpts: Vec<PreviewExcerpt> = Vec::with_capacity(merged.len());
    for (start_line, end_line) in merged {
        let byte_start = line_starts[start_line];
        // Window end byte: start of next line minus the newline, or
        // end-of-content if this is the last line.
        let byte_end = if end_line + 1 < total_lines {
            // line_starts[end_line + 1] is the byte AFTER the \n that
            // closes end_line. Subtract 1 to drop that trailing \n —
            // it would render as a blank tail line otherwise.
            line_starts[end_line + 1].saturating_sub(1)
        } else {
            // Last line — clamp at content end.
            content.len()
        };
        let byte_end = byte_end.min(content.len());

        // Slice the excerpt text. Use char_indices to avoid splitting in
        // the middle of a multi-byte UTF-8 sequence (e.g. an emoji that
        // happens to straddle a line boundary).
        let excerpt_text = content
            .get(byte_start..byte_end)
            .unwrap_or("")
            .to_string();

        // Rewrite each in-window finding's span to be relative to the
        // excerpt slice. Findings whose span falls outside the window
        // (shouldn't happen given how windows are built, but defensive)
        // are dropped.
        let excerpt_findings: Vec<Finding> = findings
            .iter()
            .filter(|f| f.start >= byte_start && f.end <= byte_end)
            .map(|f| Finding {
                kind: f.kind,
                tier: f.tier,
                start: f.start - byte_start,
                end: f.end - byte_start,
            })
            .collect();

        excerpts.push(PreviewExcerpt {
            // 1-indexed for the UI ("Line 42" reads more naturally
            // than "Line 41" to non-developers).
            line_start: start_line + 1,
            content: excerpt_text,
            findings: excerpt_findings,
        });
    }

    excerpts
}

// ─── Validators (the parts regex can't express) ────────────────────────

/// Decide whether a captured value looks like a REAL secret rather than
/// a placeholder, a mask, or — for UNQUOTED values — ordinary prose.
/// This is the main false-positive filter for the generic key=value and
/// bearer detectors. The app is for everyone, not just developers,
/// so it must not cry wolf on normal documents.
///
/// fancy-regex already rejects env-var refs and common placeholder
/// PREFIXES (process.env, $VAR, YOUR_, EXAMPLE_, …) before we get here.
/// This validator handles what the regex can't or shouldn't:
///   - sub-substring placeholder words (`my_example_token`)
///   - exact-word matches (`password = changeme`)
///   - all-same-char masks (`***`, `xxxxxx`)
///   - entropy gate for unquoted prose
fn is_plausible_secret_value(raw: &str, quoted: bool) -> bool {
    let value = raw.trim_end_matches(|c| c == ',' || c == ';' || c == '.');
    if value.len() < 6 {
        return false;
    }
    let lower = value.to_ascii_lowercase();

    // Defense-in-depth: regex already rejects the `$`/`%` prefix +
    // function call shapes, but a paranoid second check costs nothing.
    if value.starts_with('$') || value.starts_with('%') {
        return false;
    }
    if value.contains('(') {
        return false;
    }
    const REF_MARKERS: &[&str] = &[
        "process.env",
        "os.environ",
        "import.meta.env",
        "getenv",
        "config.",
        "settings.",
        "{{",
        "}}",
    ];
    if REF_MARKERS.iter().any(|m| lower.contains(m)) {
        return false;
    }

    // Placeholder / mask substrings — safe words that never appear in
    // a real high-entropy secret.
    const PLACEHOLDER_SUBSTR: &[&str] = &[
        "your",
        "example",
        "placeholder",
        "redacted",
        "changethis",
        "xxxx",
        "....",
        "dummy",
        "sample",
    ];
    if PLACEHOLDER_SUBSTR.iter().any(|m| lower.contains(m)) {
        return false;
    }

    // Exact common placeholders.
    const PLACEHOLDER_EXACT: &[&str] = &[
        "password",
        "passwd",
        "changeme",
        "secret",
        "mysecret",
        "none",
        "null",
        "true",
        "false",
        "undefined",
        "todo",
        "fixme",
        "test",
        "testing",
        "enabled",
        "disabled",
    ];
    if PLACEHOLDER_EXACT.contains(&lower.as_str()) {
        return false;
    }

    // A single repeated character (`*`, `x`, `0`, …) is a mask.
    let first = value.chars().next().expect("value has length >= 6");
    if value.chars().all(|c| c == first) {
        return false;
    }

    // Entropy gate for UNQUOTED values. A quoted literal (`key="value"`)
    // is an explicit assignment we trust once placeholders are filtered;
    // an unquoted bare word after a secret keyword is more likely prose
    // ("credential: Associate"), so it must actually look secret-shaped.
    if !quoted && !looks_secretish(value) {
        return false;
    }

    true
}

/// Heuristic "this looks like a secret, not a word": carries a digit, is
/// long enough to be a token, or contains a base64 / path character.
/// Deliberately lenient — only gates UNQUOTED values, where the
/// alternative is prose.
fn looks_secretish(value: &str) -> bool {
    value.chars().any(|c| c.is_ascii_digit())
        || value.len() >= 20
        || value.contains('/')
        || value.contains('+')
}

/// Returns `true` when the extracted digit string has a structurally plausible
/// bank-card IIN (Issuer Identification Number). Called **after** Luhn so it
/// only runs on the small set of candidates that already passed the checksum.
///
/// Two things are validated:
///   1. The Major Industry Identifier (first digit) must be a bank-card MII.
///      MII 0, 1, 2, 7, 8, 9 are assigned to other industries (airlines,
///      petroleum, healthcare, national bodies) — never to bank cards.
///   2. The IIN prefix + card length must match a known payment network:
///      - MII 4 (Visa): exactly 13 or 16 digits.
///      - MII 5 (Mastercard): prefix 51–55, exactly 16 digits.
///      - MII 3 (Amex/JCB/Diners): checked by second digit and length.
///      - MII 6 (Discover/Maestro/UnionPay): broad 6xxx range, 12–19 digits.
fn is_valid_card_iin(digits: &str) -> bool {
    let b = digits.as_bytes();
    let len = digits.len();
    if b.is_empty() {
        return false;
    }
    match b[0] - b'0' {
        4 => len == 13 || len == 16,
        5 => len == 16 && b.len() >= 2 && matches!(b[1] - b'0', 1..=5),
        3 => {
            if b.len() < 2 {
                return false;
            }
            match b[1] - b'0' {
                4 | 7 => len == 15, // Amex: 34xx / 37xx, always 15 digits
                5 => {
                    // JCB: prefix 3528–3589, always 16 digits
                    if len != 16 || b.len() < 4 {
                        return false;
                    }
                    let p: u16 = (b[0] - b'0') as u16 * 1000
                        + (b[1] - b'0') as u16 * 100
                        + (b[2] - b'0') as u16 * 10
                        + (b[3] - b'0') as u16;
                    (3528..=3589).contains(&p)
                }
                0 => {
                    // Diners Club International: 300–305, always 14 digits
                    b.len() >= 3 && (b[2] - b'0') <= 5 && len == 14
                }
                6 | 8 => len == 14, // Diners Club: 36xx / 38xx, always 14 digits
                _ => false,
            }
        }
        6 => (12..=19).contains(&len), // Discover / Maestro / UnionPay — wide range
        _ => false, // MII 0,1,2,7,8,9 are never bank-card ranges
    }
}

/// Returns `true` when the digit string looks like a test or placeholder value
/// rather than a real card number. The signal is low entropy: fewer than 3
/// distinct digit characters. Real card numbers have at least 4–5 distinct
/// digits; repeated-body patterns like `4111 1111 1111 1111` (Visa test card,
/// distinct digits = {4,1}) or `1111 1111 1111 1111` typically carry only 1–2
/// unique values and are never live credentials.
fn is_monotonic_placeholder(digits: &str) -> bool {
    let mut seen = [false; 10];
    let mut count = 0usize;
    for b in digits.bytes() {
        let d = (b - b'0') as usize;
        if !seen[d] {
            seen[d] = true;
            count += 1;
            if count >= 3 {
                return false; // already 3 distinct → not a placeholder
            }
        }
    }
    true // fewer than 3 distinct digits
}

/// Standard Luhn (mod-10) checksum used by credit cards and many other
/// numeric IDs. Walking right-to-left, every other digit is doubled;
/// two-digit values fold via 9-subtraction. Valid if `sum % 10 == 0`.
fn luhn_valid(digits: &str) -> bool {
    let mut sum: u32 = 0;
    let mut alternate = false;
    for ch in digits.chars().rev() {
        let mut n = match ch.to_digit(10) {
            Some(d) => d,
            None => return false,
        };
        if alternate {
            n *= 2;
            if n > 9 {
                n -= 9;
            }
        }
        sum += n;
        alternate = !alternate;
    }
    sum > 0 && sum.is_multiple_of(10)
}

/// US ABA routing checksum: 3(d1+d4+d7)+7(d2+d5+d8)+(d3+d6+d9) ≡ 0 (mod 10).
fn is_valid_aba(raw: &str) -> bool {
    let d: Vec<u32> = raw.chars().filter_map(|c| c.to_digit(10)).collect();
    if d.len() != 9 {
        return false;
    }
    let s = 3 * (d[0] + d[3] + d[6]) + 7 * (d[1] + d[4] + d[7]) + (d[2] + d[5] + d[8]);
    s != 0 && s.is_multiple_of(10)
}

/// Spain DNI/NIE check letter: LETTERS[number mod 23] (NIE maps X/Y/Z → 0/1/2).
fn is_valid_dni(prefix: &str, digits: &str, letter: &str) -> bool {
    const LETTERS: &[u8] = b"TRWAGMYFPDXBNJZSQVHLCKE";
    let mapped = match prefix.to_ascii_uppercase().as_str() {
        "" => digits.to_string(),
        "X" => format!("0{digits}"),
        "Y" => format!("1{digits}"),
        "Z" => format!("2{digits}"),
        _ => return false,
    };
    let n: u64 = match mapped.parse() {
        Ok(v) => v,
        Err(_) => return false,
    };
    letter.as_bytes().first().map(|b| b.to_ascii_uppercase()) == Some(LETTERS[(n % 23) as usize])
}

/// Dutch BSN modulo-11 validation (elfproef).
///
/// Weights: 9×d1 + 8×d2 + 7×d3 + 6×d4 + 5×d5 + 4×d6 + 3×d7 + 2×d8 − 1×d9.
/// Result must be divisible by 11 and non-zero. 8-digit BSNs are padded with
/// a leading zero to 9 digits before validation.
fn is_valid_bsn(raw: &str) -> bool {
    let digits: Vec<u32> = raw
        .chars()
        .filter(|c| c.is_ascii_digit())
        .filter_map(|c| c.to_digit(10))
        .collect();
    // Accept 8 or 9 digit runs; pad 8-digit to 9 for the formula.
    let d: Vec<u32> = match digits.len() {
        9 => digits,
        8 => {
            let mut v = vec![0u32];
            v.extend(digits);
            v
        }
        _ => return false,
    };
    if d[0] == 0 {
        return false; // leading zero only valid in the padded 8-digit case, handled above
    }
    let weights: [i64; 9] = [9, 8, 7, 6, 5, 4, 3, 2, -1];
    let sum: i64 = d
        .iter()
        .zip(weights.iter())
        .map(|(&di, &w)| w * di as i64)
        .sum();
    sum > 0 && sum % 11 == 0
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Legacy kinds-only API (unchanged behaviour for all callers) ──

    #[test]
    fn detects_aws_access_key() {
        let kinds = scan_text("AWS_KEY=AKIAIOSFODNN7EXAMPLE in the config.");
        assert!(kinds.contains(&"aws_access_key"));
        assert!(kinds.contains(&"sensitive"));
    }

    #[test]
    fn detects_github_token() {
        let kinds = scan_text("token: ghp_1234567890abcdefghijklmnopqrstuvwxyzAB");
        assert!(kinds.contains(&"github_token"));
    }

    #[test]
    fn detects_github_fine_grained_pat() {
        let kinds = scan_text("token=github_pat_11ABCDEFG0aBcDeFgHiJkL_mNoPqRsTuVwXyZ012345");
        assert!(kinds.contains(&"github_token"));
    }

    #[test]
    fn detects_slack_webhook() {
        // Built from parts so secret scanners don't flag the (fake) URL.
        let url = ["https://hooks.slack.com/services/", "T00000000/B00000000/", "XXXXXXXXXXXXXXXXXXXXXXXX"].concat();
        let kinds = scan_text(&format!("url = {url}"));
        assert!(kinds.contains(&"slack_webhook"));
    }

    #[test]
    fn detects_pem_private_key() {
        let kinds = scan_text("-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA...");
        assert!(kinds.contains(&"private_key_pem"));
    }

    #[test]
    fn detects_jwt() {
        let kinds = scan_text(
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        );
        assert!(kinds.contains(&"jwt_token"));
    }

    #[test]
    fn empty_text_no_findings() {
        assert!(scan_text("").is_empty());
        assert!(scan_text("Just some plain prose with nothing sensitive.").is_empty());
    }

    #[test]
    fn detects_valid_credit_card_via_luhn() {
        // 4532 0151 1283 0366 — Luhn-valid Visa test card with high digit
        // entropy (8 distinct digits); passes all three gates.
        let kinds = scan_text("Card number: 4532 0151 1283 0366 expires 12/26.");
        assert!(kinds.contains(&"credit_card"));
    }

    #[test]
    fn rejects_invalid_luhn_digit_run() {
        let kinds = scan_text("Order ID 1234567890123 in the system.");
        assert!(!kinds.contains(&"credit_card"));
    }

    // ── IIN + placeholder hardening (Phase 6.5-3) ──

    #[test]
    fn rejects_non_bank_card_mii() {
        // Helpers accept only MII 3–6; 0,1,2,7,8,9 are never bank-card ranges.
        assert!(!is_valid_card_iin("1111111111111111")); // MII 1 — airlines
        assert!(!is_valid_card_iin("7890123456789012")); // MII 7 — petroleum
        assert!(!is_valid_card_iin("9876543210987654")); // MII 9 — national bodies
        assert!(!is_valid_card_iin("0000000000000000")); // MII 0
    }

    #[test]
    fn rejects_wrong_length_for_brand() {
        // Visa must be 13 or 16; Amex must be 15; MC must be 16.
        assert!(!is_valid_card_iin("45320151128303")); // 14 digits, Visa → invalid
        assert!(!is_valid_card_iin("3782822463100050")); // 16 digits, Amex → invalid (must be 15)
        assert!(!is_valid_card_iin("51000000000001")); // 14 digits, MC → invalid
    }

    #[test]
    fn accepts_valid_brand_iin_and_length() {
        assert!(is_valid_card_iin("4532015112830366")); // Visa 16
        assert!(is_valid_card_iin("5500005555555559")); // Mastercard 55xx, 16
        assert!(is_valid_card_iin("378282246310005")); // Amex 37xx, 15
        assert!(is_valid_card_iin("6011111111111117")); // Discover 6011, 16
    }

    #[test]
    fn rejects_low_entropy_placeholder_digits() {
        assert!(is_monotonic_placeholder("1111111111111111")); // 1 distinct
        assert!(is_monotonic_placeholder("4111111111111111")); // {4,1} = 2 distinct
        assert!(is_monotonic_placeholder("4444444444444441")); // {4,1} = 2 distinct
        assert!(!is_monotonic_placeholder("4532015112830366")); // 8 distinct — real card
    }

    #[test]
    fn rejects_visa_test_card_placeholder() {
        // 4111 1111 1111 1111 passes Luhn and is a valid Visa IIN (4, 16 digits),
        // but has only 2 distinct digits ({4,1}) → correctly rejected as placeholder.
        let kinds = scan_text("card: 4111 1111 1111 1111");
        assert!(
            !kinds.contains(&"credit_card"),
            "Visa test-card placeholder must be rejected, got: {kinds:?}"
        );
    }

    #[test]
    fn first_label_is_sensitive_when_anything_matched() {
        let kinds = scan_text("token: ghp_1234567890abcdefghijklmnopqrstuvwxyzAB");
        assert_eq!(kinds.first(), Some(&"sensitive"));
    }

    // ── Generic credential assignment ──

    #[test]
    fn detects_user_reported_suffix_apikey() {
        let kinds = scan_text(r#"stemapikey="asdasdas""#);
        assert!(
            kinds.contains(&"generic_credential_assignment"),
            "should catch suffix-matched apikey assignment, got: {kinds:?}"
        );
    }

    #[test]
    fn detects_yaml_password() {
        let kinds = scan_text("database:\n  password: hunter2supersecret\n");
        assert!(kinds.contains(&"generic_credential_assignment"));
    }

    #[test]
    fn detects_quoted_json_credential() {
        let kinds = scan_text(r#"{"api_key": "1234567890abcdef"}"#);
        assert!(kinds.contains(&"generic_credential_assignment"));
    }

    #[test]
    fn detects_env_file_assignments() {
        let kinds = scan_text("DATABASE_PASSWORD=changeme123\nAPI_KEY=mysecretkey789");
        assert!(kinds.contains(&"generic_credential_assignment"));
    }

    #[test]
    fn detects_dotted_property_assignment() {
        let kinds = scan_text("config.client_secret = 'abcdef1234567890'");
        assert!(kinds.contains(&"generic_credential_assignment"));
    }

    #[test]
    fn detects_dash_separated_keyword() {
        let kinds = scan_text("api-key: super-secret-value-987654");
        assert!(kinds.contains(&"generic_credential_assignment"));
    }

    #[test]
    fn ignores_unrelated_assignments() {
        assert!(!scan_text("monkey=banana").contains(&"generic_credential_assignment"));
        assert!(!scan_text("count=42").contains(&"generic_credential_assignment"));
        assert!(!scan_text("x = 'hello world'").contains(&"generic_credential_assignment"));
    }

    #[test]
    fn ignores_short_or_empty_values() {
        assert!(!scan_text("password=null").contains(&"generic_credential_assignment"));
        assert!(!scan_text("password=true").contains(&"generic_credential_assignment"));
        assert!(!scan_text("password=").contains(&"generic_credential_assignment"));
        assert!(!scan_text("api_key: ''").contains(&"generic_credential_assignment"));
    }

    #[test]
    fn ignores_prose_without_assignment_shape() {
        assert!(!scan_text("Don't share your password.").contains(&"generic_credential_assignment"));
        assert!(!scan_text("rotate the api key periodically").contains(&"generic_credential_assignment"));
    }

    // ── Precision: reject placeholders / refs ──

    #[test]
    fn rejects_placeholder_credential_values() {
        assert!(
            !scan_text(r#"api_key = "YOUR_API_KEY_HERE""#)
                .contains(&"generic_credential_assignment")
        );
        assert!(!scan_text("password = changethis").contains(&"generic_credential_assignment"));
        assert!(
            !scan_text("secret_key: example_value_goes").contains(&"generic_credential_assignment")
        );
        assert!(!scan_text("password = ******").contains(&"generic_credential_assignment"));
    }

    #[test]
    fn rejects_env_var_reference_values() {
        assert!(
            !scan_text("password = process.env.DB_PASSWORD")
                .contains(&"generic_credential_assignment")
        );
        assert!(
            !scan_text("api_key = os.getenv('KEY')").contains(&"generic_credential_assignment")
        );
        assert!(
            !scan_text("password=$DB_PASS_VARIABLE").contains(&"generic_credential_assignment")
        );
        assert!(!scan_text("password=%DB_PASSWORD%").contains(&"generic_credential_assignment"));
    }

    #[test]
    fn still_detects_real_credential_values() {
        assert!(
            scan_text(r#"api_key="A1b2C3d4E5f6G7h8""#)
                .contains(&"generic_credential_assignment")
        );
        assert!(scan_text("password = hunter2supersecret")
            .contains(&"generic_credential_assignment"));
    }

    #[test]
    fn ignores_credential_word_in_prose() {
        let kinds = scan_text(
            "For Mendix developers building a formal OutSystems credential: Associate Reactive Developer — entry point.",
        );
        assert!(
            !kinds.contains(&"generic_credential_assignment"),
            "prose 'credential:' must not flag, got {kinds:?}"
        );
        assert!(
            !kinds.contains(&"sensitive"),
            "the document should not be flagged sensitive at all, got {kinds:?}"
        );
    }

    #[test]
    fn ignores_unquoted_prose_values() {
        assert!(!scan_text("password: Associate").contains(&"generic_credential_assignment"));
        assert!(!scan_text("api_key: available").contains(&"generic_credential_assignment"));
        assert!(
            !scan_text("Your password should be memorable but unique")
                .contains(&"generic_credential_assignment")
        );
    }

    #[test]
    fn accepts_quoted_literal_even_when_wordlike() {
        assert!(
            scan_text(r#"password = "supersafeword""#)
                .contains(&"generic_credential_assignment")
        );
    }

    #[test]
    fn detects_unquoted_secretish_value() {
        assert!(
            scan_text("API_KEY=abc123def456ghi").contains(&"generic_credential_assignment")
        );
    }

    // ── Named tokens ──

    #[test]
    fn detects_openai_api_key_classic_format() {
        let kinds = scan_text("OPENAI_KEY=sk-abcdefghij1234567890ABCDEFGHIJ");
        assert!(kinds.contains(&"openai_api_key"));
    }

    #[test]
    fn detects_openai_project_key_format() {
        let kinds = scan_text("api_key = 'sk-proj-abcdefghij1234567890_ABCDEFGHIJ-token'");
        assert!(kinds.contains(&"openai_api_key"));
    }

    #[test]
    fn detects_npm_publish_token() {
        let kinds = scan_text(
            "//registry.npmjs.org/:_authToken=npm_abcdefghij1234567890ABCDEFghij1234567890",
        );
        assert!(kinds.contains(&"npm_token"));
    }

    #[test]
    fn detects_bearer_token_in_auth_header() {
        let kinds = scan_text("Authorization: Bearer abcdefghij1234567890XYZqrstuvwAB12");
        assert!(kinds.contains(&"bearer_token"));
    }

    #[test]
    fn ignores_short_bearer_prose() {
        let kinds = scan_text("requires bearer auth header");
        assert!(!kinds.contains(&"bearer_token"));
    }

    #[test]
    fn rejects_placeholder_bearer_token() {
        let kinds = scan_text("Authorization: Bearer YOUR_ACCESS_TOKEN_GOES_HEREE");
        assert!(!kinds.contains(&"bearer_token"));
    }

    // ── SSN structural validation ──

    #[test]
    fn detects_structurally_valid_ssn() {
        let kinds = scan_text("SSN on file: 123-45-6789.");
        assert!(kinds.contains(&"us_ssn"));
    }

    #[test]
    fn rejects_structurally_invalid_ssn() {
        assert!(!scan_text("000-12-3456").contains(&"us_ssn"));
        assert!(!scan_text("666-12-3456").contains(&"us_ssn"));
        assert!(!scan_text("900-12-3456").contains(&"us_ssn"));
        assert!(!scan_text("123-00-4567").contains(&"us_ssn"));
        assert!(!scan_text("123-45-0000").contains(&"us_ssn"));
    }

    // ── Phase 6.5-1 NEW: spans + tiers ──

    #[test]
    fn finding_carries_correct_span_for_github_token() {
        let text = "prefix ghp_1234567890abcdefghijklmnopqrstuvwxyzAB suffix";
        let findings = scan_text_detailed(text, false);
        let f = findings
            .iter()
            .find(|f| f.kind == "github_token")
            .expect("token finding");
        assert_eq!(&text[f.start..f.end], "ghp_1234567890abcdefghijklmnopqrstuvwxyzAB");
        assert_eq!(f.tier, Tier::High);
    }

    #[test]
    fn finding_value_span_for_generic_credential() {
        // The span should cover only the VALUE, not the keyword.
        let text = r#"api_key = "1234567890abcdef""#;
        let findings = scan_text_detailed(text, false);
        let f = findings
            .iter()
            .find(|f| f.kind == "generic_credential_assignment")
            .expect("cred finding");
        assert_eq!(&text[f.start..f.end], "1234567890abcdef");
        assert_eq!(f.tier, Tier::Medium);
    }

    #[test]
    fn ssn_tier_is_medium() {
        let findings = scan_text_detailed("SSN: 123-45-6789", false);
        let f = findings.iter().find(|f| f.kind == "us_ssn").expect("ssn");
        assert_eq!(f.tier, Tier::Medium);
        assert_eq!(f.start, 5);
        assert_eq!(f.end, 16);
    }

    #[test]
    fn credit_card_tier_is_high() {
        // Use a real-looking Visa test card (8 distinct digits) that passes all
        // three gates: Luhn + IIN + placeholder check.
        let findings = scan_text_detailed("card: 4532 0151 1283 0366", false);
        let f = findings.iter().find(|f| f.kind == "credit_card").expect("cc");
        assert_eq!(f.tier, Tier::High);
    }

    #[test]
    fn low_tier_off_by_default() {
        let findings = scan_text_detailed("contact me at hello@example.com", false);
        // No HIGH/MEDIUM detector should fire on a bare email.
        assert!(findings.is_empty(), "got {findings:?}");
    }

    #[test]
    fn low_tier_email_when_opted_in() {
        let findings = scan_text_detailed("contact me at hello@example.com", true);
        let f = findings.iter().find(|f| f.kind == "email").expect("email");
        assert_eq!(f.tier, Tier::Low);
        assert_eq!(&"contact me at hello@example.com"[f.start..f.end], "hello@example.com");
    }

    #[test]
    fn low_tier_ipv4_when_opted_in() {
        let findings = scan_text_detailed("server is at 192.168.1.42 today", true);
        assert!(findings.iter().any(|f| f.kind == "ipv4" && f.tier == Tier::Low));
    }

    #[test]
    fn findings_sorted_by_start_position() {
        // Multiple HIGH detectors fire on a single line.
        let text = "AKIA1234567890ABCDEF and ghp_1234567890abcdefghijklmnopqrstuvwxyzAB and more";
        let findings = scan_text_detailed(text, false);
        let starts: Vec<usize> = findings.iter().map(|f| f.start).collect();
        let mut sorted = starts.clone();
        sorted.sort();
        assert_eq!(starts, sorted);
    }

    #[test]
    fn scan_text_kinds_only_still_dedupes_and_tags() {
        // Two AWS keys in one string → kinds list has one "aws_access_key"
        // entry (deduped) plus the universal "sensitive" tag at position 0.
        let kinds =
            scan_text("AKIA1234567890ABCDEF and another AKIA0987654321FEDCBA");
        assert_eq!(kinds.first(), Some(&"sensitive"));
        assert_eq!(kinds.iter().filter(|&&k| k == "aws_access_key").count(), 1);
    }

    // ── Ported: HIGH-tier crypto wallet addresses + Anthropic key ──

    #[test]
    fn detects_anthropic_key_not_openai() {
        // An `sk-ant-…` key matches both the new anthropic pattern and the
        // broader openai pattern; the post-filter drops the openai duplicate.
        let f = scan_text_detailed(
            "key=sk-ant-api03-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx end",
            false,
        );
        assert!(
            f.iter().any(|x| x.kind == "anthropic_api_key"),
            "expected anthropic_api_key, got: {f:?}"
        );
        assert!(
            !f.iter().any(|x| x.kind == "openai_api_key"),
            "anthropic key must not also be labeled openai: {f:?}"
        );
    }

    #[test]
    fn detects_bitcoin_p2pkh_address() {
        let f = scan_text("Send to 1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa please");
        assert!(f.contains(&"bitcoin_address"), "got: {f:?}");
    }

    #[test]
    fn detects_bitcoin_bech32_address() {
        let f = scan_text("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq and more");
        assert!(f.contains(&"bitcoin_bech32"), "got: {f:?}");
    }

    #[test]
    fn detects_ethereum_address_not_private_key() {
        // 40 hex chars → wallet address; the 64-char form is a private key.
        let f = scan_text("ETH wallet 0x71C7656EC7ab88b098defB751B7401B5f6d8976F end");
        assert!(f.contains(&"ethereum_address"), "got: {f:?}");
    }

    #[test]
    fn ethereum_private_key_not_confused_with_address() {
        let f = scan_text(
            "key 0x4c0883a69102937d6231471b5dbb6e538eba2ef67d8b6e7c7bae8e22cbf4bc01 end",
        );
        assert!(f.contains(&"ethereum_private_key"), "got: {f:?}");
        assert!(
            !f.contains(&"ethereum_address"),
            "64-char key must not also match as address: {f:?}"
        );
    }

    #[test]
    fn detects_solana_address() {
        let f = scan_text("pubkey 9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM end");
        assert!(f.contains(&"solana_address"), "got: {f:?}");
    }

    // ── Ported: always-on MEDIUM detectors (include_low = false) ──

    #[test]
    fn always_on_medium_detected_without_include_low() {
        let cases: &[(&str, &str)] = &[
            ("IBAN DE89370400440532013000 end", "iban"),
            ("GE IBAN ანგარიში GE29NB0000000101904917 .", "iban"),
            ("CF: RSSMRA85T10A562S al servizio", "italian_cf"),
            ("NINO AB123456C noted", "uk_nino"),
            ("NIR 1 84 12 76 451 089 record", "france_nir"),
            ("ITIN 912-78-1234 on file", "us_itin"),
            ("VIN 1HGCM82633A004352 here", "vin"),
            ("DNI 12345678Z provided", "spain_dni"),
            ("Passport No: X1234567 attached", "passport"),
            ("Driver's License: D1234567 shown", "drivers_license"),
            ("Bank account number: 1234567890", "bank_account"),
            ("MRN: AB12345 in chart", "medical_record_number"),
            ("DOB: 1990-05-12 listed", "dob"),
            ("Routing number: 021000021", "routing_number"),
        ];
        for (text, kind) in cases {
            let f = scan_text_detailed(text, false);
            assert!(
                f.iter().any(|x| x.kind == *kind),
                "expected {kind} (always-on) in {text:?} -> {f:?}"
            );
        }
    }

    #[test]
    fn always_on_medium_avoids_obvious_false_positives() {
        // Bare values without a label must NOT trip the contextual detectors.
        let f = scan_text_detailed("the value 1234567890 appears and X1234567 too", false);
        assert!(!f.iter().any(|x| x.kind == "passport"), "no bare passport: {f:?}");
        assert!(!f.iter().any(|x| x.kind == "bank_account"), "no bare account: {f:?}");
        // Invalid ABA checksum behind a routing label is rejected.
        let f2 = scan_text_detailed("Routing number: 123456789", false);
        assert!(
            !f2.iter().any(|x| x.kind == "routing_number"),
            "bad ABA rejected: {f2:?}"
        );
        // Spain DNI with the wrong check letter (Z is correct for 12345678, not A).
        let f3 = scan_text_detailed("DNI 12345678A here", false);
        assert!(
            !f3.iter().any(|x| x.kind == "spain_dni"),
            "bad DNI letter rejected: {f3:?}"
        );
    }

    #[test]
    fn vin_all_digit_run_rejected() {
        // 17 all-digit chars must NOT be flagged as a VIN (needs letter+digit mix).
        let f = scan_text_detailed("number 12345678901234567 here", false);
        assert!(!f.iter().any(|x| x.kind == "vin"), "all-digit not a VIN: {f:?}");
    }

    // ── Ported: include_low-gated detectors ──

    #[test]
    fn gated_detectors_detected_when_opted_in() {
        let cases: &[(&str, &str)] = &[
            ("MAC 00:1A:2B:3C:4D:5E seen", "mac_address"),
            ("meeting 1985-07-22 scheduled", "date"),
            ("piradi nomeri 01024085006 aris", "georgian_id"),
            ("Aadhaar: 2345 6789 0123 here", "india_aadhaar"),
            ("EIN 12-3456789 for the org", "us_ein"),
            ("BSN: 111222333 op aanvraag", "dutch_bsn"),
            ("nomeria 555 12 34 56 today", "phone"),
        ];
        for (text, kind) in cases {
            let f = scan_text_detailed(text, true);
            assert!(
                f.iter().any(|x| x.kind == *kind),
                "expected {kind} (gated) in {text:?} -> {f:?}"
            );
        }
    }

    #[test]
    fn gated_detectors_absent_when_not_opted_in() {
        // Each gated detector's trigger text must produce NO finding of its
        // kind when include_low = false.
        let cases: &[(&str, &str)] = &[
            ("MAC 00:1A:2B:3C:4D:5E seen", "mac_address"),
            ("meeting 1985-07-22 scheduled", "date"),
            ("piradi nomeri 01024085006 aris", "georgian_id"),
            ("Aadhaar: 2345 6789 0123 here", "india_aadhaar"),
            ("EIN 12-3456789 for the org", "us_ein"),
            ("BSN: 111222333 op aanvraag", "dutch_bsn"),
        ];
        for (text, kind) in cases {
            let f = scan_text_detailed(text, false);
            assert!(
                !f.iter().any(|x| x.kind == *kind),
                "{kind} must be gated off when include_low=false in {text:?} -> {f:?}"
            );
        }
    }

    #[test]
    fn dutch_bsn_invalid_rejected() {
        // 123456789 fails the modulo-11 check even with include_low on.
        let f = scan_text_detailed("nummer 123456789 wrong", true);
        assert!(
            !f.iter().any(|x| x.kind == "dutch_bsn"),
            "invalid BSN must be rejected: {f:?}"
        );
    }

    // ── Ported validators (unit-level) ──

    #[test]
    fn aba_validator_accepts_and_rejects() {
        assert!(is_valid_aba("021000021"));
        assert!(!is_valid_aba("123456789"));
    }

    #[test]
    fn dni_validator_accepts_and_rejects() {
        assert!(is_valid_dni("", "12345678", "Z"));
        assert!(!is_valid_dni("", "12345678", "A"));
        assert!(is_valid_dni("X", "1234567", "L"));
    }

    #[test]
    fn bsn_validator_accepts_and_rejects() {
        assert!(is_valid_bsn("111222333"));
        assert!(!is_valid_bsn("123456789"));
    }
}
