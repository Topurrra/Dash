//! Natural-language queries: dates, sizes, terms, Tantivy queries.

use super::*;

/// Apply an explicit `operator:value` query operator to the plan being built.
/// `ext:` / `extension:` / `filetype:` take a literal (comma-separated)
/// extension list; `type:` / `kind:` take a category word, a folder/file
/// intent, or a literal extension. Returns `true` when the token was a
/// recognised operator and has been fully consumed.
pub(super) fn apply_field_operator(
    operator: &str,
    raw_value: &str,
    extension_filters: &mut Vec<String>,
    entry_type_filter: &mut Option<String>,
) -> bool {
    if raw_value.trim().is_empty() {
        return false;
    }
    match operator {
        "ext" | "extension" | "filetype" => {
            for part in raw_value.split(',') {
                let part = part.trim().trim_start_matches('.');
                if part.is_empty() {
                    continue;
                }
                for ext in normalize_extension_token(part) {
                    push_unique(extension_filters, ext);
                }
            }
            true
        }
        "type" | "kind" => {
            let value = raw_value.trim();
            if is_folder_intent_token(value) {
                *entry_type_filter = Some(ENTRY_TYPE_FOLDER.to_string());
            } else if is_file_intent_token(value) {
                *entry_type_filter = Some(ENTRY_TYPE_FILE.to_string());
            } else if let Some(category) = token_category_extensions(value) {
                for ext in category {
                    push_unique(extension_filters, ext);
                }
            } else {
                for ext in normalize_extension_token(value) {
                    push_unique(extension_filters, ext);
                }
            }
            true
        }
        _ => false,
    }
}

pub(super) fn build_natural_query_plan(input: &str) -> NaturalQueryPlan {
    let (input_without_phrases, exact_phrases) = extract_quoted_phrases(input);
    let tokens = tokenize_search_input(&input_without_phrases);
    let mut query_words = Vec::new();
    let mut inferred_extension_filters = Vec::new();
    let mut explicit_extension_filters = Vec::new();
    let mut entry_type_filter: Option<String> = None;
    let mut date_filter: Option<DateFilter> = None;
    let mut size_filter: Option<SizeFilter> = None;
    let mut date_field_hint = DateFieldIntent::Modified;
    let mut previous_token: Option<String> = None;
    let mut index = 0usize;

    while index < tokens.len() {
        let bare = tokens[index].trim_start_matches('.').to_string();
        let had_dot_prefix = tokens[index].starts_with('.');
        if bare.is_empty() {
            index += 1;
            continue;
        }

        // Explicit field operators — `ext:docx`, `type:image`, `kind:folder` —
        // resolved before the keyword/filter heuristics so an operator wins.
        if let Some((operator, raw_value)) = bare.split_once(':') {
            if apply_field_operator(operator, raw_value, &mut explicit_extension_filters, &mut entry_type_filter) {
                previous_token = Some(bare.clone());
                index += 1;
                continue;
            }
        }
        // A colon / comma that was not part of a recognised operator is dropped
        // so the token still matches as an ordinary keyword (`build:debug` →
        // `builddebug`) — `,` only survives tokenizing for operator value lists.
        let bare = if bare.contains(':') || bare.contains(',') { bare.replace([':', ','], "") } else { bare };
        if bare.is_empty() {
            index += 1;
            continue;
        }

        if let Some(field) = date_field_intent(&bare) {
            date_field_hint = field;
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if is_folder_intent_token(&bare) {
            entry_type_filter = Some(ENTRY_TYPE_FOLDER.to_string());
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if is_file_intent_token(&bare) {
            entry_type_filter = Some(ENTRY_TYPE_FILE.to_string());
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if bare == "empty" {
            size_filter = Some(SizeFilter::Empty);
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if let Some((filter, consumed)) = parse_size_filter(&tokens, index) {
            size_filter = Some(filter);
            previous_token = Some(tokens[index].clone());
            index += consumed;
            continue;
        }

        if let Some((filter, consumed)) = parse_date_filter(&tokens, index, date_field_hint) {
            date_filter = Some(filter);
            previous_token = Some(tokens[index].clone());
            index += consumed;
            continue;
        }

        if let Some(year) = parse_year_token(&bare) {
            if previous_token.as_deref().map(is_year_filter_hint).unwrap_or(false) {
                date_filter = exact_year_filter(date_field_hint, year);
            } else {
                push_expanded_query_word(&mut query_words, bare.clone());
            }
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if let Some(exts) = token_category_extensions(&bare) {
            for ext in exts {
                push_unique(&mut inferred_extension_filters, ext);
            }
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if had_dot_prefix || is_common_extension_token(&bare) {
            for ext in normalize_extension_token(&bare) {
                push_unique(&mut explicit_extension_filters, ext);
            }
            previous_token = Some(bare);
            index += 1;
            continue;
        }

        if is_natural_stopword(&bare) {
            // A connector like "at"/"on" between a date word and the year
            // ("created at 2025", "modified on 2025") must NOT break the
            // year-hint chain — keep the preceding hint so the year binds to it.
            let bridges_year_hint = matches!(bare.as_str(), "at" | "on")
                && previous_token.as_deref().map(is_year_filter_hint).unwrap_or(false);
            if !bridges_year_hint {
                previous_token = Some(bare);
            }
            index += 1;
            continue;
        }
        push_expanded_query_word(&mut query_words, bare.clone());
        previous_token = Some(bare);
        index += 1;
    }

    for phrase in &exact_phrases {
        for token in tokenize_search_input(phrase) {
            let bare = token.trim_start_matches('.').replace([':', ','], "");
            if bare.is_empty() || is_natural_stopword(&bare) {
                continue;
            }
            push_expanded_query_word(&mut query_words, bare);
        }
    }

    let extension_filters =
        if explicit_extension_filters.is_empty() { inferred_extension_filters } else { explicit_extension_filters };

    NaturalQueryPlan {
        query_text: query_words.join(" "),
        query_keywords: query_words,
        extension_filters,
        entry_type_filter,
        date_filter,
        size_filter,
        exact_phrases,
    }
}

pub(super) fn should_run_aggressive_file_fallback(
    natural_plan: &NaturalQueryPlan,
    query_keywords: &[String],
    extension_filters: &[String],
    path_filter: Option<&str>,
) -> bool {
    if path_filter.is_some()
        || natural_plan.entry_type_filter.is_some()
        || natural_plan.size_filter.is_some()
        || !extension_filters.is_empty()
        || !natural_plan.exact_phrases.is_empty()
    {
        return true;
    }

    // Only widen the net when the entire query is a single short keyword.
    // A short word inside a multi-keyword query is meaningful — don't over-fetch.
    query_keywords.len() == 1 && query_keywords.first().map(|k| k.len() >= 3 && k.len() <= 5).unwrap_or(false)
}

pub(super) fn extract_quoted_phrases(input: &str) -> (String, Vec<String>) {
    let mut remaining = String::with_capacity(input.len());
    let mut phrases = Vec::new();
    let mut current_phrase = String::new();
    let mut in_quote = false;

    for ch in input.chars() {
        if ch == '"' || ch == '\'' {
            if in_quote {
                let phrase = current_phrase.trim();
                if !phrase.is_empty() {
                    phrases.push(phrase.to_string());
                }
                current_phrase.clear();
                in_quote = false;
            } else {
                in_quote = true;
            }
            remaining.push(' ');
            continue;
        }

        if in_quote {
            current_phrase.push(ch);
        } else {
            remaining.push(ch);
        }
    }

    if in_quote && !current_phrase.trim().is_empty() {
        remaining.push(' ');
        remaining.push_str(&current_phrase);
    }

    (remaining, phrases)
}

pub(super) fn tokenize_search_input(input: &str) -> Vec<String> {
    input
        .split_whitespace()
        .filter_map(|token| {
            let cleaned = token
                .chars()
                .filter(|ch| ch.is_alphanumeric() || *ch == '.' || *ch == '_' || *ch == '-' || *ch == ':' || *ch == ',')
                .collect::<String>()
                .to_lowercase();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            }
        })
        .collect()
}

pub(super) fn push_expanded_query_word(query_words: &mut Vec<String>, word: String) {
    push_unique(query_words, word.clone());
    for related in related_natural_terms(&word) {
        push_unique(query_words, related.to_string());
    }
}

pub(super) fn related_natural_terms(word: &str) -> &'static [&'static str] {
    match word {
        // Visual / animation
        "animation" | "animations" | "animated" => &["anime", "cartoon", "cartoons"],
        "anime" => &["animation", "animated", "cartoon", "cartoons"],
        "cartoon" | "cartoons" => &["animation", "animated", "anime"],
        "screenshot" | "screenshots" => &["screenshot", "screenshots", "capture", "screen"],
        "capture" | "captures" => &["screenshot", "screenshots", "capture"],

        // HR / career
        "resume" | "cv" | "curriculum" => &["resume", "cv", "curriculum", "vitae"],
        "vitae" => &["resume", "cv", "curriculum"],
        "portfolio" | "portfolios" | "showcase" => &["portfolio", "portfolios", "showcase", "gallery"],

        // Finance / billing
        "invoice" | "invoices" => &["invoice", "invoices", "bill", "receipt", "billing"],
        "bill" | "bills" | "receipt" | "receipts" | "billing" => &["invoice", "invoices", "bill", "receipt"],
        "budget" | "budgets" => &["budget", "budgets", "finance", "financial", "spending", "expenditure"],
        "finance" | "financial" | "spending" | "expenditure" => {
            &["budget", "budgets", "finance", "financial", "spending"]
        }
        "expense" | "expenses" | "cost" | "costs" => &["expense", "expenses", "cost", "bill", "receipt"],
        "payment" | "payments" | "transaction" | "transactions" => &["payment", "payments", "transaction", "transfer"],
        "transfer" | "transfers" => &["payment", "transfer", "transaction"],
        "quote" | "quotes" | "quotation" | "quotations" | "estimate" | "estimates" => {
            &["quote", "quotation", "estimate", "proposal"]
        }
        "order" | "orders" | "purchase" | "purchases" => &["order", "orders", "purchase", "purchases", "transaction"],
        "salary" | "salaries" | "payroll" | "paycheck" | "paychecks" | "wage" | "wages" => {
            &["salary", "salaries", "payroll", "paycheck", "wages"]
        }
        "tax" | "taxes" | "taxation" => &["tax", "taxes", "taxable", "filing"],
        "filing" | "filings" => &["filing", "tax", "taxes", "form", "forms"],
        "insurance" | "policy" | "policies" | "coverage" => &["insurance", "policy", "policies", "coverage", "claim"],
        "claim" | "claims" => &["claim", "claims", "insurance", "policy"],
        "statement" | "statements" => &["statement", "statements", "balance", "bank"],
        "bank" | "banking" => &["bank", "banking", "statement", "account"],

        // Legal / formal
        "contract" | "contracts" => &["contract", "contracts", "agreement", "agreements", "nda"],
        "agreement" | "agreements" | "nda" => &["contract", "agreement", "agreements", "nda"],
        "license" | "licence" | "licenses" | "licences" | "permit" | "permits" => {
            &["license", "licence", "permit", "certificate"]
        }
        "certificate" | "certificates" | "certification" | "cert" | "certs" | "credential" | "credentials" => {
            &["certificate", "cert", "credential", "diploma", "license"]
        }
        "diploma" | "diplomas" | "degree" | "degrees" => &["diploma", "degree", "certificate", "cert"],
        "legal" | "lawsuit" | "litigation" => &["legal", "contract", "agreement", "filing"],

        // Documents / writing
        "report" | "reports" => &["report", "reports", "summary", "summaries", "analysis"],
        "summary" | "summaries" | "analysis" | "analyses" => {
            &["report", "summary", "summaries", "analysis", "overview"]
        }
        "overview" => &["summary", "overview", "report"],
        "note" | "notes" => &["note", "notes", "memo", "memos", "jot"],
        "memo" | "memos" | "jot" | "jots" => &["note", "notes", "memo", "memos"],
        "draft" | "drafts" | "wip" => &["draft", "drafts", "wip", "revision", "revisions"],
        "revision" | "revisions" => &["draft", "drafts", "revision", "revisions", "version"],
        "template" | "templates" | "boilerplate" => &["template", "templates", "boilerplate", "skeleton"],
        "proposal" | "proposals" | "pitch" | "pitches" => &["proposal", "proposals", "pitch", "pitches", "rfp"],
        "rfp" => &["rfp", "proposal", "proposals"],
        "manual" | "manuals" | "handbook" | "handbooks" => {
            &["manual", "manuals", "guide", "guides", "handbook", "instructions"]
        }
        "guide" | "guides" | "instructions" | "howto" => {
            &["guide", "guides", "manual", "manuals", "tutorial", "instructions"]
        }
        "tutorial" | "tutorials" => &["tutorial", "tutorials", "guide", "guides", "example", "howto"],
        "example" | "examples" | "sample" | "samples" | "demo" | "demos" => {
            &["example", "examples", "sample", "samples", "demo"]
        }
        "readme" | "documentation" | "docs" | "doc" => &["readme", "documentation", "docs", "guide", "manual"],
        "changelog" | "history" | "changes" => &["changelog", "history", "release", "releases", "version"],
        "release" | "releases" | "version" | "versions" => &["release", "releases", "changelog", "version", "patch"],

        // Planning / project
        "plan" | "plans" | "planning" => &["plan", "plans", "schedule", "roadmap", "strategy"],
        "roadmap" | "roadmaps" | "strategy" | "strategies" => &["roadmap", "plan", "plans", "strategy", "milestone"],
        "milestone" | "milestones" => &["milestone", "milestones", "roadmap", "plan"],
        "schedule" | "schedules" | "calendar" => &["schedule", "schedules", "calendar", "agenda", "plan"],
        "agenda" => &["agenda", "schedule", "meeting", "minutes"],
        "minutes" => &["minutes", "agenda", "meeting", "notes"],
        "meeting" | "meetings" | "appointment" | "appointments" => {
            &["meeting", "meetings", "appointment", "agenda", "minutes"]
        }
        "checklist" | "checklists" | "todo" | "todos" | "task" | "tasks" => &["checklist", "todo", "todos", "tasks"],

        // Development / technical
        "patch" | "patches" | "hotfix" | "hotfixes" => &["patch", "patches", "fix", "hotfix", "update"],
        "fix" | "fixes" | "bugfix" | "bugfixes" => &["fix", "fixes", "patch", "patches", "bugfix"],
        "test" | "tests" | "testing" => &["test", "tests", "spec", "specs"],
        "spec" | "specs" | "specification" | "specifications" => &["spec", "specs", "test", "tests", "specification"],
        "error" | "errors" | "exception" | "exceptions" => &["error", "errors", "exception", "crash", "log"],
        "crash" | "crashes" | "dump" | "dumps" => &["crash", "crashes", "error", "dump", "log"],
        "log" | "logs" => &["log", "logs", "trace", "traces"],
        "trace" | "traces" => &["trace", "traces", "log", "logs", "debug"],
        "debug" | "debugging" => &["debug", "trace", "log", "error"],
        "config" | "configuration" | "settings" | "preferences" => {
            &["config", "configuration", "settings", "conf", "preferences"]
        }
        "conf" | "env" => &["conf", "config", "configuration", "settings"],
        "backup" | "backups" | "bak" => &["backup", "backups", "bak", "archive"],
        "script" | "scripts" | "automation" => &["script", "scripts", "automation", "macro"],
        "macro" | "macros" => &["macro", "macros", "script", "scripts", "automation"],

        // Messaging / communication
        "message" | "messages" | "chat" | "chats" | "conversation" | "conversations" => {
            &["message", "messages", "chat", "conversation", "thread"]
        }
        "thread" | "threads" => &["thread", "threads", "message", "conversation"],
        "email" | "emails" | "mail" | "mails" => &["email", "emails", "mail", "message", "correspondence"],
        "correspondence" => &["correspondence", "email", "letter", "letters"],
        "letter" | "letters" => &["letter", "letters", "correspondence", "email"],

        // Diagrams / visuals
        "diagram" | "diagrams" | "flowchart" | "flowcharts" | "wireframe" | "wireframes" => {
            &["diagram", "diagrams", "flowchart", "chart", "wireframe"]
        }
        "chart" | "charts" => &["chart", "charts", "graph", "graphs", "diagram"],
        "graph" | "graphs" => &["graph", "graphs", "chart", "charts", "plot"],
        "plot" | "plots" => &["plot", "plots", "graph", "chart"],

        // Presentation / slides (keyword synonym even if also mapped as extension category)
        "presentation" | "presentations" | "slideshow" => &["presentation", "presentations", "slides", "slideshow"],
        "slides" => &["slides", "slideshow", "presentation", "presentations"],

        // Health / personal
        "medical" | "health" | "healthcare" => &["medical", "health", "healthcare", "record", "records"],
        "record" | "records" => &["record", "records", "medical", "history"],
        "prescription" | "prescriptions" | "medication" | "medications" => {
            &["prescription", "medication", "medical", "health"]
        }

        _ => &[],
    }
}

pub(super) fn date_field_intent(token: &str) -> Option<DateFieldIntent> {
    match token {
        "created" | "creation" | "made" | "createdat" | "createdon" => Some(DateFieldIntent::Created),
        "modified" | "updated" | "changed" | "edited" | "modifiedat" | "modifiedon" | "updatedat" => {
            Some(DateFieldIntent::Modified)
        }
        _ => None,
    }
}

pub(super) fn is_folder_intent_token(token: &str) -> bool {
    matches!(token, "folder" | "folders" | "directory" | "directories" | "dir" | "dirs")
}

pub(super) fn is_file_intent_token(token: &str) -> bool {
    matches!(token, "file" | "files")
}

pub(super) fn parse_date_filter(
    tokens: &[String],
    index: usize,
    field: DateFieldIntent,
) -> Option<(DateFilter, usize)> {
    let token = tokens.get(index)?.as_str();

    match token {
        "today" => {
            let now = current_unix_ms();
            Some((
                DateFilter {
                    field,
                    start_ms: Some(now.saturating_sub(24 * 60 * 60 * 1000)),
                    end_ms: Some(now),
                    label: format!("{} today", date_field_label(field)),
                },
                1,
            ))
        }
        "yesterday" => {
            let now = current_unix_ms();
            Some((
                DateFilter {
                    field,
                    start_ms: Some(now.saturating_sub(48 * 60 * 60 * 1000)),
                    end_ms: Some(now.saturating_sub(24 * 60 * 60 * 1000)),
                    label: format!("{} yesterday", date_field_label(field)),
                },
                1,
            ))
        }
        "recent" | "recently" => {
            let now = current_unix_ms();
            Some((
                DateFilter {
                    field,
                    start_ms: Some(now.saturating_sub(30 * 24 * 60 * 60 * 1000)),
                    end_ms: Some(now),
                    label: format!("recently {}", date_field_label(field).to_lowercase()),
                },
                1,
            ))
        }
        "this" | "last" | "past" => {
            let next = tokens.get(index + 1).map(|value| value.as_str())?;
            // Numeric window — "last 7 days", "past 3 months".
            if let Ok(count) = next.parse::<u64>() {
                let unit = tokens.get(index + 2)?;
                let span_days = count.saturating_mul(period_to_days(unit)?).max(1);
                return Some((
                    rolling_days_filter(
                        field,
                        span_days,
                        format!("{} in the {token} {count} {unit}", date_field_label(field)),
                    ),
                    3,
                ));
            }
            // Word window — "this week", "last month", "past year".
            let span_days = period_to_days(next)?;
            Some((rolling_days_filter(field, span_days, format!("{} {token} {next}", date_field_label(field))), 2))
        }
        "between" => {
            let first_year = tokens.get(index + 1).and_then(|value| parse_year_token(value))?;
            let second_index =
                if tokens.get(index + 2).map(|value| value.as_str()) == Some("and") { index + 3 } else { index + 2 };
            let second_year = tokens.get(second_index).and_then(|value| parse_year_token(value))?;
            let start_year = first_year.min(second_year);
            let end_year = first_year.max(second_year);
            Some((year_range_filter(field, start_year, end_year)?, second_index.saturating_sub(index) + 1))
        }
        "before" => {
            let year = tokens.get(index + 1).and_then(|value| parse_year_token(value))?;
            Some((before_year_filter(field, year)?, 2))
        }
        "after" => {
            let year = tokens.get(index + 1).and_then(|value| parse_year_token(value))?;
            Some((after_year_filter(field, year)?, 2))
        }
        "since" => {
            let year = tokens.get(index + 1).and_then(|value| parse_year_token(value))?;
            Some((since_year_filter(field, year)?, 2))
        }
        // `from`/`in` are intentionally NOT here — they read as the release
        // year in the *name* ("movies from 2025" == "movies 2025"), not a date.
        // Date filtering by year is explicit: `during`/`year`, or `created`/
        // `modified` + a year.
        "during" | "year" => {
            let year = tokens.get(index + 1).and_then(|value| parse_year_token(value))?;
            Some((exact_year_filter(field, year)?, 2))
        }
        _ => None,
    }
}

/// Days in a single relative period word — `day`, `week`, `month`, `year`
/// (singular or plural). The windows are rolling approximations, matching the
/// existing `recent` / `this week` behaviour rather than calendar boundaries.
pub(super) fn period_to_days(word: &str) -> Option<u64> {
    match word {
        "day" | "days" => Some(1),
        "week" | "weeks" => Some(7),
        "month" | "months" => Some(31),
        "year" | "years" => Some(365),
        _ => None,
    }
}

/// A rolling `[now - days, now]` date filter — the shared shape behind
/// `today`, `recently`, `this week`, `last 30 days`, and similar phrases.
pub(super) fn rolling_days_filter(field: DateFieldIntent, days: u64, label: String) -> DateFilter {
    let now = current_unix_ms();
    let span_ms = days.saturating_mul(24 * 60 * 60 * 1000);
    DateFilter { field, start_ms: Some(now.saturating_sub(span_ms)), end_ms: Some(now), label }
}

pub(super) fn parse_size_filter(tokens: &[String], index: usize) -> Option<(SizeFilter, usize)> {
    let token = tokens.get(index)?.as_str();
    match token {
        "large" | "big" => Some((
            SizeFilter::Range { min_bytes: Some(100 * 1024 * 1024), max_bytes: None, label: "large files".to_string() },
            1,
        )),
        "huge" | "massive" => Some((
            SizeFilter::Range { min_bytes: Some(1024 * 1024 * 1024), max_bytes: None, label: "huge files".to_string() },
            1,
        )),
        "small" | "tiny" => Some((
            SizeFilter::Range { min_bytes: None, max_bytes: Some(10 * 1024 * 1024), label: "small files".to_string() },
            1,
        )),
        "bigger" | "larger" | "over" | "above" => parse_size_after(tokens, index + 1).map(|(bytes, consumed)| {
            (
                SizeFilter::Range {
                    min_bytes: Some(bytes),
                    max_bytes: None,
                    label: format!("bigger than {}", format_size_label(bytes)),
                },
                consumed + 1,
            )
        }),
        "smaller" | "under" | "below" => parse_size_after(tokens, index + 1).map(|(bytes, consumed)| {
            (
                SizeFilter::Range {
                    min_bytes: None,
                    max_bytes: Some(bytes),
                    label: format!("smaller than {}", format_size_label(bytes)),
                },
                consumed + 1,
            )
        }),
        "more" if tokens.get(index + 1).map(|value| value.as_str()) == Some("than") => {
            parse_size_after(tokens, index + 2).map(|(bytes, consumed)| {
                (
                    SizeFilter::Range {
                        min_bytes: Some(bytes),
                        max_bytes: None,
                        label: format!("bigger than {}", format_size_label(bytes)),
                    },
                    consumed + 2,
                )
            })
        }
        "less" if tokens.get(index + 1).map(|value| value.as_str()) == Some("than") => {
            parse_size_after(tokens, index + 2).map(|(bytes, consumed)| {
                (
                    SizeFilter::Range {
                        min_bytes: None,
                        max_bytes: Some(bytes),
                        label: format!("smaller than {}", format_size_label(bytes)),
                    },
                    consumed + 2,
                )
            })
        }
        "at" if tokens.get(index + 1).map(|value| value.as_str()) == Some("least") => {
            parse_size_after(tokens, index + 2).map(|(bytes, consumed)| {
                (
                    SizeFilter::Range {
                        min_bytes: Some(bytes),
                        max_bytes: None,
                        label: format!("at least {}", format_size_label(bytes)),
                    },
                    consumed + 2,
                )
            })
        }
        "at" if tokens.get(index + 1).map(|value| value.as_str()) == Some("most") => {
            parse_size_after(tokens, index + 2).map(|(bytes, consumed)| {
                (
                    SizeFilter::Range {
                        min_bytes: None,
                        max_bytes: Some(bytes),
                        label: format!("at most {}", format_size_label(bytes)),
                    },
                    consumed + 2,
                )
            })
        }
        _ => None,
    }
}

pub(super) fn parse_size_after(tokens: &[String], mut index: usize) -> Option<(u64, usize)> {
    let original_index = index;
    if tokens.get(index).map(|value| value.as_str()) == Some("than") {
        index += 1;
    }
    let (bytes, consumed) = parse_size_expression(tokens, index)?;
    Some((bytes, index.saturating_sub(original_index) + consumed))
}

pub(super) fn parse_size_expression(tokens: &[String], index: usize) -> Option<(u64, usize)> {
    let token = tokens.get(index)?;
    let (number, unit, consumed) = split_size_token(token, tokens.get(index + 1).map(String::as_str))?;
    let multiplier = match unit {
        "b" | "byte" | "bytes" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "`as` saturates: an oversized value becomes u64::MAX and a negative or NaN one 0"
    )]
    let bytes = (number * multiplier).round() as u64;
    Some((bytes, consumed))
}

pub(super) fn split_size_token<'a>(token: &'a str, next: Option<&'a str>) -> Option<(f64, &'a str, usize)> {
    let mut number = String::new();
    let mut unit_start = token.len();
    for (index, ch) in token.char_indices() {
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
        } else {
            unit_start = index;
            break;
        }
    }
    if number.is_empty() {
        return None;
    }
    let value = number.parse::<f64>().ok()?;
    if value <= 0.0 {
        return None;
    }
    let unit = token[unit_start..].trim();
    if !unit.is_empty() {
        return Some((value, unit, 1));
    }
    let unit = next?;
    Some((value, unit, 2))
}

pub(super) fn exact_year_filter(field: DateFieldIntent, year: i32) -> Option<DateFilter> {
    year_range_filter(field, year, year)
}

pub(super) fn before_year_filter(field: DateFieldIntent, year: i32) -> Option<DateFilter> {
    Some(DateFilter {
        field,
        start_ms: None,
        end_ms: year_start_ms(year).map(|value| value.saturating_sub(1)),
        label: format!("{} before {year}", date_field_label(field)),
    })
}

pub(super) fn after_year_filter(field: DateFieldIntent, year: i32) -> Option<DateFilter> {
    Some(DateFilter {
        field,
        start_ms: year_start_ms(year.saturating_add(1)),
        end_ms: None,
        label: format!("{} after {year}", date_field_label(field)),
    })
}

pub(super) fn since_year_filter(field: DateFieldIntent, year: i32) -> Option<DateFilter> {
    Some(DateFilter {
        field,
        start_ms: year_start_ms(year),
        end_ms: None,
        label: format!("{} since {year}", date_field_label(field)),
    })
}

pub(super) fn year_range_filter(field: DateFieldIntent, start_year: i32, end_year: i32) -> Option<DateFilter> {
    Some(DateFilter {
        field,
        start_ms: year_start_ms(start_year),
        end_ms: year_start_ms(end_year.saturating_add(1)).map(|value| value.saturating_sub(1)),
        label: if start_year == end_year {
            format!("{} in {start_year}", date_field_label(field))
        } else {
            format!("{} between {start_year} and {end_year}", date_field_label(field))
        },
    })
}

pub(super) fn date_field_label(field: DateFieldIntent) -> &'static str {
    match field {
        DateFieldIntent::Modified => "Modified",
        DateFieldIntent::Created => "Created",
    }
}

pub(super) fn year_start_ms(year: i32) -> Option<u64> {
    let date = time::Date::from_calendar_date(year, time::Month::January, 1).ok()?;
    let timestamp = date.with_time(time::Time::MIDNIGHT).assume_utc().unix_timestamp();
    // Before 1970 has no u64 value.
    u64::try_from(timestamp).ok().map(|seconds| seconds * 1000)
}

pub(super) fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

pub(super) fn parse_extension_filters(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };

    let mut filters = Vec::new();
    for token in raw.split(|ch: char| ch == ',' || ch == ';' || ch == '|' || ch.is_whitespace()) {
        let cleaned = token.trim().trim_start_matches('.').to_lowercase();
        if cleaned.is_empty() {
            continue;
        }
        if let Some(expanded) = token_extensions(&cleaned) {
            for ext in expanded {
                push_unique(&mut filters, ext);
            }
        }
    }
    filters
}

pub(super) fn build_natural_query_string(
    query_text: &str,
    query_keywords: &[String],
    extension_filters: &[String],
) -> Option<String> {
    let mut clauses = Vec::new();

    if let Some(keyword_clause) = build_natural_keyword_clause(query_text, query_keywords) {
        clauses.push(format!("({keyword_clause})"));
    }

    if !extension_filters.is_empty() {
        let ext_clause =
            extension_filters.iter().map(|ext| format!("extension:{ext}")).collect::<Vec<_>>().join(" OR ");
        clauses.push(format!("({ext_clause})"));
    }

    if clauses.is_empty() {
        None
    } else {
        Some(clauses.join(" AND "))
    }
}

pub(super) fn build_natural_keyword_clause(query_text: &str, query_keywords: &[String]) -> Option<String> {
    let mut keyword_groups = Vec::new();
    for keyword in query_keywords {
        let token = keyword.trim();
        if token.is_empty() {
            continue;
        }
        keyword_groups.push(token.to_string());
    }

    if !keyword_groups.is_empty() {
        return Some(keyword_groups.join(" "));
    }

    let query_text = query_text.trim();
    if query_text.is_empty() {
        None
    } else {
        Some(query_text.to_string())
    }
}

pub(super) fn build_native_tantivy_query(
    query_keywords: &[String],
    extension_filters: &[String],
    natural_plan: &NaturalQueryPlan,
    fields: &SearchFields,
) -> Option<Box<dyn Query>> {
    // Only worth running when at least one filter can be pushed into Tantivy natively.
    // Pure keyword queries are already handled well by Pass 1.
    let has_date = natural_plan.date_filter.is_some();
    let has_size = matches!(natural_plan.size_filter, Some(SizeFilter::Range { .. }));
    let has_entry_type = natural_plan.entry_type_filter.is_some();
    if !has_date && !has_size && !has_entry_type {
        return None;
    }

    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    // Keywords: OR across file_name and path, combined as Must so the doc must contain
    // at least one keyword somewhere.
    if !query_keywords.is_empty() {
        let mut kw: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for keyword in query_keywords {
            let fn_term = Term::from_field_text(fields.file_name, keyword.as_str());
            kw.push((Occur::Should, Box::new(TermQuery::new(fn_term, IndexRecordOption::Basic))));
            let path_term = Term::from_field_text(fields.path, keyword.as_str());
            kw.push((Occur::Should, Box::new(TermQuery::new(path_term, IndexRecordOption::Basic))));
        }
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(kw))));
    }

    // Extension filter: Tantivy TermQuery on the STRING extension field.
    if !extension_filters.is_empty() {
        let mut ext: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for e in extension_filters {
            let term = Term::from_field_text(fields.extension, e.as_str());
            ext.push((Occur::Should, Box::new(TermQuery::new(term, IndexRecordOption::Basic))));
        }
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(ext))));
    }

    // Date range: native RangeQuery on the FAST u64 field.
    // Tantivy can skip entire segments using the column-oriented FAST reader.
    if let Some(date_filter) = &natural_plan.date_filter {
        let ms_field = match date_filter.field {
            DateFieldIntent::Modified => fields.modified_ms,
            DateFieldIntent::Created => fields.created_ms?,
        };
        let lower = date_filter
            .start_ms
            .map(|ms| Bound::Included(Term::from_field_u64(ms_field, ms)))
            .unwrap_or(Bound::Unbounded);
        let upper = date_filter
            .end_ms
            .map(|ms| Bound::Included(Term::from_field_u64(ms_field, ms)))
            .unwrap_or(Bound::Unbounded);
        let range = RangeQuery::new(lower, upper);
        clauses.push((Occur::Must, Box::new(range)));
    }

    // Size range: native RangeQuery on the FAST u64 size field.
    // SizeFilter::Empty is skipped here — empty-folder detection needs a filesystem check
    // that can't be expressed as a Tantivy query, so the post-filter handles it.
    if let Some(SizeFilter::Range { min_bytes, max_bytes, .. }) = &natural_plan.size_filter {
        let lower =
            min_bytes.map(|b| Bound::Included(Term::from_field_u64(fields.size, b))).unwrap_or(Bound::Unbounded);
        let upper =
            max_bytes.map(|b| Bound::Included(Term::from_field_u64(fields.size, b))).unwrap_or(Bound::Unbounded);
        let range = RangeQuery::new(lower, upper);
        clauses.push((Occur::Must, Box::new(range)));
    }

    // Entry type filter: TermQuery on the STRING entry_type field.
    if let Some(entry_type) = &natural_plan.entry_type_filter {
        if let Some(et_field) = fields.entry_type {
            let term = Term::from_field_text(et_field, entry_type.as_str());
            clauses.push((Occur::Must, Box::new(TermQuery::new(term, IndexRecordOption::Basic))));
        }
    }

    if clauses.is_empty() {
        None
    } else {
        Some(Box::new(BooleanQuery::new(clauses)))
    }
}

pub(super) fn build_fuzzy_tantivy_query(
    query_keywords: &[String],
    file_name_field: Field,
    path_field: Field,
) -> Option<Box<dyn Query>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    for keyword in query_keywords {
        // Skip tokens too short for fuzzy — edit-distance-1 on 1-3 char tokens causes noise.
        if keyword.len() < 4 {
            continue;
        }
        // Build a FuzzyTermQuery on both the file_name and path fields so the
        // filename "Asura.mkv" is found when the user types "assura" or "asurr".
        for &field in &[file_name_field, path_field] {
            let term = Term::from_field_text(field, keyword.as_str());
            let fuzzy = FuzzyTermQuery::new(term, 1, true);
            clauses.push((Occur::Should, Box::new(fuzzy)));
        }
    }

    if clauses.is_empty() {
        None
    } else {
        Some(Box::new(BooleanQuery::new(clauses)))
    }
}

/// Build a prefix range query for the keywords. For each keyword `K`, this finds
/// every term in the file_name and path fields that starts with `K` by doing a
/// lexicographic range query over the term dictionary `[K, K_next)`.
///
/// This is what catches "asu" → "asura", "spi" → "spider", "xmen" → "xmen_..."
/// in single-digit milliseconds, replacing the 200ms AllQuery+scan fallback.
///
/// Tantivy's term dictionary is FST-backed, so range queries on string prefixes
/// are effectively free — they jump directly to the start of the range and stream.
pub(super) fn build_prefix_tantivy_query(
    query_keywords: &[String],
    file_name_field: Field,
    path_field: Field,
) -> Option<Box<dyn Query>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    for keyword in query_keywords {
        // Single-character prefixes match too much; skip them.
        if keyword.len() < 2 {
            continue;
        }
        let lower = keyword.to_lowercase();
        // Build the lexicographic upper bound. For "spi" we want "spj" so that
        // every term >= "spi" and < "spj" is in range — i.e. every term starting with "spi".
        let upper = match next_lex_prefix(&lower) {
            Some(s) => s,
            None => continue,
        };

        for &field in &[file_name_field, path_field] {
            let lower_term = Term::from_field_text(field, &lower);
            let upper_term = Term::from_field_text(field, &upper);
            let range = RangeQuery::new(Bound::Included(lower_term), Bound::Excluded(upper_term));
            clauses.push((Occur::Should, Box::new(range)));
        }
    }

    if clauses.is_empty() {
        None
    } else {
        Some(Box::new(BooleanQuery::new(clauses)))
    }
}

/// Return the next lexicographic string after `s`, used as the exclusive upper
/// bound of a prefix range. For "spi" returns "spj". Returns None if every byte
/// is already at maximum value (extremely unlikely for indexed text).
pub(super) fn next_lex_prefix(s: &str) -> Option<String> {
    let mut bytes = s.as_bytes().to_vec();
    for i in (0..bytes.len()).rev() {
        if bytes[i] < 0xFF {
            bytes[i] = bytes[i].saturating_add(1);
            // Truncate at the incremented byte — anything after is irrelevant.
            bytes.truncate(i + 1);
            return String::from_utf8(bytes).ok();
        }
    }
    None
}

pub(super) fn build_query_with_extension_filters(query_text: &str, extension_filters: &[String]) -> String {
    if extension_filters.is_empty() {
        return query_text.to_string();
    }

    let ext_clause = extension_filters.iter().map(|ext| format!("extension:{ext}")).collect::<Vec<_>>().join(" OR ");
    format!("({query_text}) AND ({ext_clause})")
}

pub(super) fn extract_query_keywords(input: &str) -> Vec<String> {
    let mut keywords = Vec::new();
    for token in input.split_whitespace() {
        let cleaned = token
            .chars()
            .filter(|ch| ch.is_alphanumeric() || *ch == '.' || *ch == '_' || *ch == '-')
            .collect::<String>()
            .to_lowercase();
        if cleaned.is_empty() {
            continue;
        }
        let bare = cleaned.trim_start_matches('.');
        if bare.is_empty() || is_natural_stopword(bare) {
            continue;
        }
        push_unique(&mut keywords, bare.to_string());
    }
    keywords
}

pub(super) fn edit_distance_within_1(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let la = a.len();
    let lb = b.len();
    if la.abs_diff(lb) > 1 {
        return false;
    }
    if a == b {
        return true;
    }
    let mut prev: Vec<usize> = (0..=lb).collect();
    let mut curr = vec![0usize; lb + 1];
    for i in 1..=la {
        curr[0] = i;
        for j in 1..=lb {
            curr[j] = if a[i - 1] == b[j - 1] { prev[j - 1] } else { 1 + prev[j - 1].min(prev[j]).min(curr[j - 1]) };
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[lb] <= 1
}
