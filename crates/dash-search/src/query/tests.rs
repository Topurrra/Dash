use super::*;

/// An owned entry for tests.
#[derive(Clone)]
struct T {
    name: String,
    path: String,
    parent: String,
    is_dir: bool,
    size: u64,
    modified_ms: u64,
    created_ms: u64,
    depth: u16,
    child_count: Option<u32>,
}

/// A file at a Windows path, 1 KB, modified "now".
fn f(path: &str) -> T {
    let (parent, name) = path.rsplit_once(['\\', '/']).unwrap();
    T {
        name: name.to_string(),
        path: path.to_string(),
        parent: parent.to_string(),
        is_dir: false,
        size: 1024,
        modified_ms: ms(2025, 6, 18, 14, 0),
        created_ms: ms(2025, 6, 18, 14, 0),
        depth: u16::try_from(path.matches(['\\', '/']).count()).unwrap(),
        child_count: None,
    }
}

/// A folder with `children` entries inside.
fn d(path: &str, children: Option<u32>) -> T {
    T { is_dir: true, size: 0, child_count: children, ..f(path) }
}

impl T {
    fn e(&self) -> Entry<'_> {
        Entry {
            name: &self.name,
            path: &self.path,
            parent: &self.parent,
            is_dir: self.is_dir,
            size: self.size,
            modified_ms: self.modified_ms,
            created_ms: self.created_ms,
            depth: self.depth,
            child_count: self.child_count,
        }
    }
    fn size(mut self, s: u64) -> T {
        self.size = s;
        self
    }
    fn modified(mut self, m: u64) -> T {
        self.modified_ms = m;
        self
    }
    fn created(mut self, c: u64) -> T {
        self.created_ms = c;
        self
    }
}

const KB: u64 = 1024;
const MB: u64 = 1024 * 1024;
const GB: u64 = 1024 * 1024 * 1024;

fn offset() -> UtcOffset {
    UtcOffset::from_hms(4, 0, 0).unwrap()
}

/// Wednesday 2025-06-18 14:30 in UTC+4.
fn now() -> OffsetDateTime {
    at(2025, 6, 18, 14, 30)
}

fn at(y: i32, m: u8, day: u8, h: u8, mi: u8) -> OffsetDateTime {
    Date::from_calendar_date(y, Month::try_from(m).unwrap(), day)
        .unwrap()
        .with_hms(h, mi, 0)
        .unwrap()
        .assume_offset(offset())
}

/// Unix ms of a local (UTC+4) time.
fn ms(y: i32, m: u8, day: u8, h: u8, mi: u8) -> u64 {
    u64::try_from(at(y, m, day, h, mi).unix_timestamp_nanos() / 1_000_000).unwrap()
}

fn q(text: &str) -> Query {
    parse_at(text, now()).unwrap_or_else(|e| panic!("{text:?} didn't parse: {e}"))
}

fn hit(text: &str, t: &T) -> bool {
    q(text).matches(&t.e())
}

fn err(text: &str) -> String {
    match parse_at(text, now()) {
        Ok(_) => panic!("{text:?} should not parse"),
        Err(e) => e,
    }
}

fn report() -> T {
    f(r"C:\Users\me\Documents\Q3 Report.pdf")
}

// ---------------------------------------------------------------- plain words

#[test]
fn empty_query_matches_everything() {
    for text in ["", "   ", "case:", "()", "NOT", "OR"] {
        let query = q(text);
        assert!(query.is_empty(), "{text:?}");
        assert!(query.matches(&report().e()));
    }
    assert!(!q("x").is_empty());
}

#[test]
fn plain_words_are_substrings_of_the_name() {
    let r = report();
    assert!(hit("rep pdf", &r));
    assert!(hit("REP", &r));
    assert!(hit("q3", &r));
    assert!(hit("t.p", &r));
    assert!(!hit("rep doc", &r));
    // The folder part of the path doesn't count for plain words.
    assert!(!hit("documents", &r));
}

#[test]
fn quoted_phrases_are_one_word() {
    let a = f(r"C:\x\Annual Report 2024.pdf");
    let b = f(r"C:\x\report annual.txt");
    assert!(hit("\"annual report\"", &a));
    assert!(!hit("\"annual report\"", &b));
    assert!(hit("annual report", &b));
    // Unclosed quote runs to the end.
    assert!(hit("\"annual rep", &a));
    // Quoted keywords are plain text.
    let or = f(r"C:\x\OR notes.txt");
    assert!(hit("\"OR\"", &or));
    assert!(!hit("\"OR\"", &f(r"C:\x\data.bin")));
}

#[test]
fn keywords_are_upper_case_only() {
    let t = f(r"C:\x\for real.txt");
    assert!(hit("or", &t));
    assert!(!hit("or zzz", &t));
}

#[test]
fn unknown_prefix_is_plain_text() {
    let t = f(r"C:\x\foo:bar.txt");
    assert!(hit("foo:bar", &t));
    assert!(!hit("foo:baz", &t));
}

// ---------------------------------------------------------------- wildcards

#[test]
fn wildcards_match_the_whole_name() {
    let pdf = f(r"C:\x\a.pdf");
    let bak = f(r"C:\x\a.pdf.bak");
    assert!(hit("*.pdf", &pdf));
    assert!(hit("*.PDF", &pdf));
    assert!(!hit("*.pdf", &bak));
    assert!(hit("*.pdf*", &bak));
    assert!(hit("a.*", &bak));
    assert!(!hit("b*", &pdf));
    assert!(hit("*", &pdf));
    assert!(hit("**", &pdf));
}

#[test]
fn question_mark_is_one_character() {
    let ok = f(r"C:\x\IMG_1234.jpg");
    let long = f(r"C:\x\IMG_12345.jpg");
    assert!(hit("IMG_????.jpg", &ok));
    assert!(hit("img_????.JPG", &ok));
    assert!(!hit("IMG_????.jpg", &long));
    assert!(hit("IMG_*.jpg", &long));
    assert!(hit("?.txt", &f(r"C:\x\ა.txt")));
    assert!(!hit("?.txt", &f(r"C:\x\აა.txt")));
}

#[test]
fn wildcards_in_the_middle_and_regex_characters() {
    let t = f(r"C:\x\a+b [1].txt");
    assert!(hit("a+b*.txt", &t));
    assert!(!hit("a+b*.txt", &f(r"C:\x\aab.txt")));
    assert!(hit("*[1]*", &t));
    assert!(hit("a*b*t", &t));
    assert!(!hit("a*z*t", &t));
    assert!(hit("case:a*.txt", &t));
    assert!(!hit("case:A*.txt", &t));
    assert!(!hit("case:*.TXT", &t));
}

#[test]
fn wildcards_with_unicode() {
    let t = f(r"C:\x\Отчёт за год.docx");
    assert!(hit("отчёт*", &t));
    assert!(hit("ОТЧЁТ*.DOCX", &t));
    assert!(hit("*ГОД*", &t));
    assert!(hit("о?чёт*", &t));
}

// ---------------------------------------------------------------- unicode

#[test]
fn unicode_case_insensitive() {
    // Georgian: Mkhedruli and Mtavruli (upper case) are one letter.
    let mk = f(r"C:\x\საქაღალდე.txt");
    let mt = f(r"C:\x\ᲡᲐᲥᲐᲦᲐᲚᲓᲔ.txt");
    assert!(hit("საქა", &mk));
    assert!(hit("ᲡᲐᲥᲐ", &mk));
    assert!(hit("საქა", &mt));
    assert!(hit("exact:საქაღალდე.TXT", &mt));
    // Cyrillic.
    let ru = f(r"C:\x\Отчёт.pdf");
    assert!(hit("отчёт", &ru));
    assert!(hit("ОТЧЁТ pdf", &ru));
    assert!(!hit("отчет", &ru));
    // Greek.
    assert!(hit("αθήνα", &f(r"C:\x\ΑΘΉΝΑ.jpg")));
    // Mixed ASCII needle, non-ASCII name.
    assert!(hit("REPORT", &f(r"C:\x\report — ✓.txt")));
    assert!(!hit("ыы", &f(r"C:\x\plain.txt")));
}

#[test]
fn fold_helpers() {
    let n = Needle::new("Straße", false);
    assert!(n.contained_in("GROSSE STRAẞE 5"));
    assert!(n.contained_in("die straße"));
    assert!(n.starts("STRAßE.txt"));
    assert!(n.ends("x straße"));
    assert!(!n.ends("straße x"));
    assert!(contains_folded("abc", "", true));
    assert!(!contains_folded("ab", "abc", true));
    assert!(contains_folded("xxABC", "abc", true));
    // Kelvin sign folds to ASCII k.
    assert!(contains_folded("\u{212A}m", "km", true));
}

// ---------------------------------------------------------------- modifiers

#[test]
fn case_modifier() {
    let up = f(r"C:\x\Report.pdf");
    let low = f(r"C:\x\report.pdf");
    assert!(hit("case:Report", &up));
    assert!(!hit("case:Report", &low));
    // Prefix applies to its word only.
    assert!(hit("case:Report PDF", &up));
    // Alone: the rest of the query.
    assert!(!hit("case: Report PDF", &up));
    assert!(hit("case: Report pdf", &up));
    // Groups keep their own modifiers.
    assert!(hit("(case: Report) PDF", &up));
}

#[test]
fn exact_modifier() {
    let t = f(r"C:\x\Report.PDF");
    assert!(hit("exact:report.pdf", &t));
    assert!(!hit("exact:report", &t));
    assert!(!hit("exact:report.pdf", &f(r"C:\x\report.pdf.bak")));
    assert!(!hit("case:exact:report.pdf", &t));
    assert!(hit("exact:case:Report.PDF", &t));
    assert!(hit("exact: report.pdf", &t));
}

#[test]
fn regex_modifier() {
    let r = report();
    assert!(hit(r"regex:^q\d", &r));
    assert!(hit(r"regex:report\.pdf$", &r));
    assert!(!hit(r"case:regex:^q\d", &r));
    assert!(hit(r"regex:case:^Q\d", &r));
    // `|` and parentheses stay inside the pattern.
    assert!(hit("regex:zzz|report", &r));
    assert!(hit("regex:^(q3|q4) ", &r));
    assert!(hit("(regex:^q3) pdf", &r));
    assert!(!hit("(regex:^q4) | zzz", &r));
    // Alone: the rest of the query.
    assert!(hit(r"regex: ^q \.pdf$", &r));
    // Quoted with spaces.
    assert!(hit("regex:\"q3 rep\"", &r));
}

#[test]
fn path_modifier() {
    let r = report();
    assert!(hit("path:documents", &r));
    assert!(hit("path:users pdf", &r));
    assert!(!hit("path:zzz", &r));
    // Alone, then back to the name.
    assert!(hit("path: users nopath: report", &r));
    assert!(!hit("path: users nopath: documents", &r));
    // A separator in the word means the path.
    assert!(hit(r"documents\q3", &r));
    assert!(hit("Documents/Q3", &f("C:/Users/me/Documents/Q3 Report.pdf")));
    assert!(!hit(r"nopath:documents\q3", &r));
    assert!(hit(r"path:*\documents\*.pdf", &r));
    assert!(hit(r"*\documents\*.pdf", &r));
    assert!(!hit(r"*\music\*.pdf", &r));
    assert!(hit("path:\"me\\documents\\q3 report\"", &r));
}

#[test]
fn file_and_folder() {
    let file = report();
    let dir = d(r"C:\Users\me\Reports", Some(3));
    assert!(hit("file:", &file));
    assert!(!hit("file:", &dir));
    assert!(hit("folder:", &dir));
    assert!(!hit("folder:", &file));
    assert!(hit("folder:rep", &dir));
    assert!(!hit("folder:rep", &file));
    assert!(hit("file:rep", &file));
    assert!(hit("folder: rep", &dir));
    assert!(hit("!folder:", &file));
}

#[test]
fn wants_folders_only() {
    assert!(q("folder:").wants_folders_only());
    assert!(q("folder: proj").wants_folders_only());
    assert!(q("folder:proj size:>1").wants_folders_only());
    assert!(!q("file:").wants_folders_only());
    assert!(!q("proj").wants_folders_only());
    assert!(!q("folder: | x").wants_folders_only());
    assert!(!q("!folder:").wants_folders_only());
}

#[test]
fn prefixes_stack() {
    let big = report().size(2 * MB);
    let small = report();
    assert!(hit("file:size:>1mb", &big));
    assert!(!hit("file:size:>1mb", &small));
    assert!(!hit("folder:size:>1mb", &big));
    assert!(hit("case:path:Documents", &big));
    assert!(!hit("case:path:documents", &big));
}

// ---------------------------------------------------------------- size

#[test]
fn size_comparisons() {
    let t = |s| report().size(s);
    assert!(hit("size:>10mb", &t(10 * MB + 1)));
    assert!(!hit("size:>10mb", &t(10 * MB)));
    assert!(hit("size:>=10mb", &t(10 * MB)));
    assert!(hit("size:<500kb", &t(500 * KB - 1)));
    assert!(!hit("size:<500kb", &t(500 * KB)));
    assert!(hit("size:<=500kb", &t(500 * KB)));
    assert!(!hit("size:<=500kb", &t(500 * KB + 1)));
    assert!(hit("size:>1.5mb", &t(2 * MB)));
    assert!(!hit("size:>1.5mb", &t(MB)));
    assert!(hit("size:>1GB", &t(2 * GB)));
    assert!(!hit("size:<0", &t(0)));
}

#[test]
fn size_ranges() {
    let t = |s| report().size(s);
    assert!(hit("size:1mb..5mb", &t(MB)));
    assert!(hit("size:1mb..5mb", &t(5 * MB)));
    assert!(!hit("size:1mb..5mb", &t(5 * MB + 1)));
    assert!(!hit("size:1mb..5mb", &t(MB - 1)));
    assert!(hit("size:..5mb", &t(0)));
    assert!(!hit("size:..5mb", &t(6 * MB)));
    assert!(hit("size:1gb..", &t(3 * GB)));
    assert!(!hit("size:1gb..", &t(GB - 1)));
}

#[test]
fn size_alone() {
    let t = |s| report().size(s);
    // With a unit: everything that shows as that many units.
    assert!(hit("size:10mb", &t(10 * MB)));
    assert!(hit("size:10mb", &t(10 * MB + MB / 2)));
    assert!(!hit("size:10mb", &t(11 * MB)));
    assert!(!hit("size:10mb", &t(10 * MB - 1)));
    // Plain bytes: exact.
    assert!(hit("size:500", &t(500)));
    assert!(!hit("size:500", &t(501)));
    assert!(hit("size:=2kb", &t(2 * KB + 5)));
    // Folders have no size.
    assert!(!hit("size:<1mb", &d(r"C:\x\dir", Some(1))));
}

#[test]
fn size_errors() {
    assert_eq!(err("size:abc"), SIZE_HELP);
    assert_eq!(err("size:"), SIZE_HELP);
    assert_eq!(err("size:>"), SIZE_HELP);
    assert_eq!(err("size:.."), SIZE_HELP);
    assert!(err("size:10xb").contains("isn't a unit"));
    assert!(err("size:5mb..1mb").contains("after its end"));
    assert!(err("size:99999999999tb").contains("too big"));
}

// ---------------------------------------------------------------- dates

fn modified(y: i32, m: u8, day: u8, h: u8, mi: u8) -> T {
    report().modified(ms(y, m, day, h, mi))
}

#[test]
fn today_and_yesterday() {
    assert!(hit("dm:today", &modified(2025, 6, 18, 0, 0)));
    assert!(hit("dm:today", &modified(2025, 6, 18, 23, 59)));
    assert!(!hit("dm:today", &modified(2025, 6, 17, 23, 59)));
    assert!(!hit("dm:today", &modified(2025, 6, 19, 0, 0)));
    assert!(hit("dm:yesterday", &modified(2025, 6, 17, 0, 0)));
    assert!(hit("dm:yesterday", &modified(2025, 6, 17, 23, 59)));
    assert!(!hit("dm:yesterday", &modified(2025, 6, 18, 0, 0)));
    assert!(hit("DM:TODAY", &modified(2025, 6, 18, 9, 0)));
}

#[test]
fn weeks_months_years() {
    // Weeks start on Monday 2025-06-16.
    assert!(!hit("dm:thisweek", &modified(2025, 6, 15, 23, 59)));
    assert!(hit("dm:thisweek", &modified(2025, 6, 16, 0, 0)));
    assert!(hit("dm:thisweek", &modified(2025, 6, 22, 23, 0)));
    assert!(!hit("dm:thisweek", &modified(2025, 6, 23, 0, 0)));
    assert!(hit("dm:lastweek", &modified(2025, 6, 9, 0, 0)));
    assert!(hit("dm:lastweek", &modified(2025, 6, 15, 23, 59)));
    assert!(!hit("dm:lastweek", &modified(2025, 6, 8, 23, 59)));
    assert!(!hit("dm:lastweek", &modified(2025, 6, 16, 0, 0)));
    assert!(hit("dm:thismonth", &modified(2025, 6, 1, 0, 0)));
    assert!(hit("dm:thismonth", &modified(2025, 6, 30, 23, 59)));
    assert!(!hit("dm:thismonth", &modified(2025, 7, 1, 0, 0)));
    assert!(hit("dm:lastmonth", &modified(2025, 5, 31, 12, 0)));
    assert!(!hit("dm:lastmonth", &modified(2025, 4, 30, 12, 0)));
    assert!(hit("dm:thisyear", &modified(2025, 1, 1, 0, 0)));
    assert!(!hit("dm:thisyear", &modified(2024, 12, 31, 23, 59)));
    assert!(hit("dm:lastyear", &modified(2024, 12, 31, 23, 59)));
    assert!(!hit("dm:lastyear", &modified(2025, 1, 1, 0, 0)));
}

#[test]
fn month_and_year_rollover() {
    let jan = at(2025, 1, 10, 9, 0);
    let dec = report().modified(ms(2024, 12, 5, 0, 0));
    assert!(parse_at("dm:lastmonth", jan).unwrap().matches(&dec.e()));
    assert!(!parse_at("dm:thismonth", jan).unwrap().matches(&dec.e()));
    // Sunday belongs to the week that started the Monday before.
    let sunday = at(2025, 6, 22, 9, 0);
    let monday = report().modified(ms(2025, 6, 16, 1, 0));
    assert!(parse_at("dm:thisweek", sunday).unwrap().matches(&monday.e()));
}

#[test]
fn last_n_units() {
    // From that long before now (14:30) to the end of today.
    for text in ["dm:last5days", "dm:lastfivedays", "dm:past5days"] {
        assert!(hit(text, &modified(2025, 6, 13, 14, 31)), "{text}");
        assert!(!hit(text, &modified(2025, 6, 13, 14, 29)), "{text}");
        assert!(hit(text, &modified(2025, 6, 18, 23, 0)), "{text}");
        assert!(!hit(text, &modified(2025, 6, 19, 0, 0)), "{text}");
    }
    for text in ["dm:last3weeks", "dm:lastthreeweeks", "dm:last3week"] {
        assert!(hit(text, &modified(2025, 5, 28, 14, 31)), "{text}");
        assert!(!hit(text, &modified(2025, 5, 28, 14, 29)), "{text}");
    }
    assert!(hit("dm:last2months", &modified(2025, 4, 18, 14, 31)));
    assert!(!hit("dm:last2months", &modified(2025, 4, 18, 14, 29)));
    assert!(hit("dm:last1year", &modified(2024, 6, 18, 14, 31)));
    assert!(!hit("dm:last1year", &modified(2024, 6, 18, 14, 29)));
    assert!(hit("dm:lasttwelvemonths", &modified(2024, 6, 18, 14, 31)));
}

#[test]
fn add_months_clamps_the_day() {
    let d = |y, m: u8, day| Date::from_calendar_date(y, Month::try_from(m).unwrap(), day).unwrap();
    assert_eq!(add_months(d(2025, 3, 31), -1), Some(d(2025, 2, 28)));
    assert_eq!(add_months(d(2024, 3, 31), -1), Some(d(2024, 2, 29)));
    assert_eq!(add_months(d(2025, 1, 15), -1), Some(d(2024, 12, 15)));
    assert_eq!(add_months(d(2025, 12, 1), 1), Some(d(2026, 1, 1)));
    assert_eq!(add_months(d(2025, 6, 18), -24), Some(d(2023, 6, 18)));
}

#[test]
fn absolute_dates() {
    assert!(hit("dm:2025-03-01", &modified(2025, 3, 1, 0, 0)));
    assert!(hit("dm:2025-03-01", &modified(2025, 3, 1, 23, 59)));
    assert!(!hit("dm:2025-03-01", &modified(2025, 3, 2, 0, 0)));
    assert!(hit("dm:2025/3/1", &modified(2025, 3, 1, 12, 0)));
    assert!(hit("dm:2025.03.01", &modified(2025, 3, 1, 12, 0)));
    assert!(hit("dm:2025-03", &modified(2025, 3, 31, 23, 59)));
    assert!(!hit("dm:2025-03", &modified(2025, 4, 1, 0, 0)));
    assert!(hit("dm:2025", &modified(2025, 12, 31, 23, 59)));
    assert!(!hit("dm:2025", &modified(2024, 12, 31, 23, 59)));
}

#[test]
fn date_comparisons_and_ranges() {
    assert!(hit("dm:>2025-03", &modified(2025, 4, 1, 0, 0)));
    assert!(!hit("dm:>2025-03", &modified(2025, 3, 31, 23, 59)));
    assert!(hit("dm:>=2025-03", &modified(2025, 3, 1, 0, 0)));
    assert!(!hit("dm:>=2025-03", &modified(2025, 2, 28, 23, 59)));
    assert!(hit("dm:<2025-03", &modified(2025, 2, 28, 23, 59)));
    assert!(!hit("dm:<2025-03", &modified(2025, 3, 1, 0, 0)));
    assert!(hit("dm:<=2025-03", &modified(2025, 3, 31, 23, 59)));
    assert!(!hit("dm:<=2025-03", &modified(2025, 4, 1, 0, 0)));
    // A range covers all of both ends.
    assert!(hit("dm:2025-03-01..2025-03-05", &modified(2025, 3, 5, 23, 0)));
    assert!(!hit("dm:2025-03-01..2025-03-05", &modified(2025, 3, 6, 0, 0)));
    assert!(!hit("dm:2025-03-01..2025-03-05", &modified(2025, 2, 28, 23, 0)));
    assert!(hit("dm:..2025-03", &modified(2000, 1, 1, 0, 0)));
    assert!(hit("dm:2025-06..", &modified(2025, 6, 18, 0, 0)));
    // Words in ranges: lastfiveweeks starts 2025-05-14 14:30.
    let r = "dm:lastfiveweeks..yesterday";
    assert!(hit(r, &modified(2025, 5, 14, 15, 0)));
    assert!(!hit(r, &modified(2025, 5, 14, 14, 0)));
    assert!(hit(r, &modified(2025, 6, 17, 23, 59)));
    assert!(!hit(r, &modified(2025, 6, 18, 1, 0)));
    assert!(hit("dm:>yesterday", &modified(2025, 6, 18, 0, 0)));
    assert!(!hit("dm:>yesterday", &modified(2025, 6, 17, 23, 0)));
}

#[test]
fn date_created() {
    let t = report().created(ms(2024, 2, 1, 10, 0)).modified(ms(2025, 6, 18, 10, 0));
    assert!(hit("dc:2024-02", &t));
    assert!(!hit("dm:2024-02", &t));
    assert!(hit("dc:lastyear dm:today", &t));
}

#[test]
fn date_errors() {
    assert_eq!(err("dm:banana"), date_help("dm:"));
    assert_eq!(err("dc:"), date_help("dc:"));
    assert_eq!(err("dm:2025-13"), date_help("dm:"));
    assert_eq!(err("dm:2025-02-30"), date_help("dm:"));
    assert_eq!(err("dm:25-01-01"), date_help("dm:"));
    assert_eq!(err("dm:last0days"), date_help("dm:"));
    assert_eq!(err("dm:last5fortnights"), date_help("dm:"));
    assert!(err("dm:2025-03-05..2025-03-01").contains("after its end"));
}

// ---------------------------------------------------------------- other filters

#[test]
fn ext_filter() {
    let pdf = f(r"C:\x\a.PDF");
    let docx = f(r"C:\x\b.docx");
    let txt = f(r"C:\x\c.txt");
    let none = f(r"C:\x\README");
    let dot = f(r"C:\x\.gitignore");
    assert!(hit("ext:pdf", &pdf));
    assert!(hit("ext:pdf;docx", &docx));
    assert!(hit("ext:.pdf;", &pdf));
    assert!(!hit("ext:pdf;docx", &txt));
    assert!(!hit("ext:pdf", &d(r"C:\x\folder.pdf", Some(1))));
    assert!(!hit("ext:pd", &pdf));
    // Alone: files without an extension.
    assert!(hit("ext:", &none));
    assert!(hit("ext:", &dot));
    assert!(!hit("ext:", &txt));
    assert!(!hit("ext:", &d(r"C:\x\dir", Some(1))));
}

#[test]
fn parent_filter() {
    let direct = f(r"C:\Users\me\a.txt");
    let deeper = f(r"C:\Users\me\sub\a.txt");
    assert!(hit(r"parent:C:\Users\me", &direct));
    assert!(!hit(r"parent:C:\Users\me", &deeper));
    assert!(hit(r"parent:c:\users\ME\", &direct));
    assert!(hit("parent:C:/Users/me/", &direct));
    assert!(hit(r"parent:C:\", &f(r"C:\top.txt")));
    let spaced = f(r"C:\Program Files\app.exe");
    assert!(hit("parent:\"C:\\Program Files\"", &spaced));
    assert!(err("parent:").contains("folder path"));
}

#[test]
fn depth_filter() {
    let t = report(); // C:\Users\me\Documents\Q3 Report.pdf → 4
    assert!(hit("depth:4", &t));
    assert!(!hit("depth:3", &t));
    assert!(hit("depth:2..4", &t));
    assert!(!hit("depth:5..6", &t));
    assert!(hit("depth:>3", &t));
    assert!(!hit("depth:>4", &t));
    assert!(hit("depth:<=4", &t));
    assert!(err("depth:x").contains("depth: needs a number"));
    assert!(err("depth:4..2").contains("after its end"));
}

#[test]
fn empty_filter() {
    assert!(hit("empty:", &d(r"C:\x\dir", Some(0))));
    assert!(!hit("empty:", &d(r"C:\x\dir", Some(2))));
    assert!(!hit("empty:", &d(r"C:\x\dir", None)));
    assert!(hit("empty:", &f(r"C:\x\a.txt").size(0)));
    assert!(!hit("empty:", &f(r"C:\x\a.txt")));
    assert!(hit("folder:empty:", &d(r"C:\x\dir", Some(0))));
    assert!(!hit("folder:empty:", &f(r"C:\x\a.txt").size(0)));
    assert!(hit("empty:dir", &d(r"C:\x\dir", Some(0))));
    assert!(!hit("empty:zzz", &d(r"C:\x\dir", Some(0))));
}

#[test]
fn childcount_filter() {
    let five = d(r"C:\x\dir", Some(5));
    assert!(hit("childcount:5", &five));
    assert!(!hit("childcount:4", &five));
    assert!(hit("childcount:>2", &five));
    assert!(hit("childcount:1..5", &five));
    assert!(!hit("childcount:6..", &five));
    assert!(!hit("childcount:>2", &d(r"C:\x\dir", None)));
    assert!(!hit("childcount:..5", &f(r"C:\x\a.txt")));
    assert!(err("childcount:many").contains("childcount: needs a number"));
}

// ---------------------------------------------------------------- logic

#[test]
fn or_and_not() {
    let r = report();
    assert!(hit("zzz OR report", &r));
    assert!(hit("zzz | report", &r));
    assert!(hit("zzz|report", &r));
    assert!(!hit("zzz | yyy", &r));
    assert!(hit("report AND pdf", &r));
    assert!(!hit("report AND doc", &r));
    assert!(!hit("NOT report", &r));
    assert!(!hit("!report", &r));
    assert!(!hit("-report", &r));
    assert!(hit("-tmp", &r));
    assert!(hit("report -tmp", &r));
    assert!(!hit("report -pdf", &r));
    assert!(hit("NOT NOT report", &r));
    assert!(hit("!!report", &r));
    assert!(!hit("-(zzz | report)", &r));
    assert!(hit("!(zzz | yyy)", &r));
    // A lone dash is text.
    assert!(hit("a - b", &f(r"C:\x\a - b.txt")));
}

#[test]
fn precedence() {
    let r = report();
    // AND binds tighter than OR: (zzz report) | pdf.
    assert!(hit("zzz report | pdf", &r));
    // report | (zzz pdf).
    assert!(hit("report | zzz pdf", &r));
    // (zzz | report) pdf.
    assert!(hit("(zzz | report) pdf", &r));
    assert!(!hit("(zzz | report) doc", &r));
    // NOT binds tighter than AND: (NOT zzz) report.
    assert!(hit("NOT zzz report", &r));
    // NOT binds tighter than OR: (NOT report) | pdf.
    assert!(hit("NOT report | pdf", &r));
    assert!(!hit("NOT (report | pdf)", &r));
    assert!(hit("((report))", &r));
}

#[test]
fn forgiving_while_typing() {
    let r = report();
    assert!(hit("report OR", &r));
    assert!(hit("OR report", &r));
    assert!(hit("report |", &r));
    assert!(hit("report NOT", &r));
    assert!(hit("report ()", &r));
    assert!(hit("report || zzz", &r));
}

#[test]
fn unbalanced_parentheses() {
    assert_eq!(err("(report"), UNMATCHED_OPEN);
    assert_eq!(err("((report)"), UNMATCHED_OPEN);
    assert_eq!(err("report)"), UNMATCHED_CLOSE);
    assert_eq!(err(")"), UNMATCHED_CLOSE);
    assert_eq!(err("(a) b)"), UNMATCHED_CLOSE);
    assert_eq!(err("(regex:^a"), UNMATCHED_OPEN);
}

#[test]
fn bad_regex() {
    assert!(err("regex:[").contains("isn't a valid pattern"));
    assert!(err("regex:(").contains("isn't a valid pattern"));
    assert!(err("regex: \"a(\"").contains("isn't a valid pattern"));
}

#[test]
fn deep_nesting_and_long_input_are_errors_not_crashes() {
    // At the limit: fine.
    let ok = format!("{}report{}", "(".repeat(MAX_NESTING), ")".repeat(MAX_NESTING));
    assert!(q(&ok).matches(&report().e()));
    assert!(parse(&format!("{}report", "!".repeat(MAX_NESTING))).is_ok());
    // Past it, by ( or by NOT in any spelling: a friendly error.
    let deep = format!("{}a{}", "(".repeat(MAX_NESTING + 1), ")".repeat(MAX_NESTING + 1));
    assert!(err(&deep).contains("nests"));
    assert!(err(&format!("{}a", "!".repeat(MAX_NESTING + 1))).contains("nests"));
    assert!(err(&format!("{}a", "-".repeat(MAX_NESTING + 1))).contains("nests"));
    assert!(err(&format!("{}a", "NOT ".repeat(MAX_NESTING + 1))).contains("nests"));
    // Pasted thousands deep (used to overflow the stack).
    assert!(err(&"(".repeat(4000)).contains("nests"));
    assert!(err(&"!".repeat(4000)).contains("nests"));
    // Long input.
    assert!(parse(&"a".repeat(MAX_QUERY_CHARS)).is_ok());
    assert!(err(&"a".repeat(MAX_QUERY_CHARS + 1)).contains("too long"));
    assert!(err(&"(".repeat(100_000)).contains("too long"));
}

#[test]
fn nesting_counts_through_saved_filters() {
    let dir = temp_dir("nesting");
    let half = MAX_NESTING / 2 + 1;
    let wrapped = format!("{}a{}", "(".repeat(half), ")".repeat(half));
    std::fs::create_dir_all(dir.join("preferences")).unwrap();
    let json = serde_json::json!([{ "name": "deep", "query": wrapped }]).to_string();
    std::fs::write(filters_file(&dir), json).unwrap();
    assert!(parse_with_filters("@deep", &dir).is_ok());
    let e = parse_with_filters(&format!("{}@deep{}", "(".repeat(half), ")".repeat(half)), &dir).unwrap_err();
    assert!(e.contains("nests"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- saved filters

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dash-query-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn saved_filters_round_trip() {
    let dir = temp_dir("roundtrip");
    assert!(saved_filters(&dir).is_empty());
    save_filter(&dir, "docs", "ext:pdf;docx").unwrap();
    save_filter(&dir, "@big", "size:>1mb").unwrap();
    assert_eq!(
        saved_filters(&dir),
        vec![("docs".to_string(), "ext:pdf;docx".to_string()), ("big".to_string(), "size:>1mb".to_string())]
    );
    assert!(filters_file(&dir).exists());

    let small = report();
    let big = report().size(2 * MB);
    let txt = f(r"C:\x\report.txt").size(2 * MB);
    let pq = |t: &str| parse_with_filters(t, &dir).unwrap();
    assert!(pq("@docs report").matches(&small.e()));
    assert!(!pq("@docs report").matches(&txt.e()));
    assert!(pq("@DOCS @big").matches(&big.e()));
    assert!(!pq("@docs @big").matches(&small.e()));
    assert!(pq("filter:docs").matches(&small.e()));
    assert!(pq("-@big").matches(&small.e()));
    assert!(pq("@big | zzz").matches(&big.e()));

    // Replace, ignoring case.
    save_filter(&dir, "DOCS", "ext:txt").unwrap();
    assert_eq!(saved_filters(&dir)[0], ("DOCS".to_string(), "ext:txt".to_string()));
    assert!(pq("@docs").matches(&txt.e()));

    delete_filter(&dir, "docs").unwrap();
    assert_eq!(saved_filters(&dir).len(), 1);
    assert!(delete_filter(&dir, "docs").unwrap_err().contains("no saved filter"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saved_filters_unknown_and_without_dir() {
    let dir = temp_dir("unknown");
    save_filter(&dir, "docs", "ext:pdf").unwrap();
    // Unknown `@name` is text; unknown `filter:name` is an error.
    let at2x = f(r"C:\x\icon@2x.png");
    assert!(parse_with_filters("@2x", &dir).unwrap().matches(&at2x.e()));
    assert!(parse_with_filters("filter:nope", &dir).unwrap_err().contains("no saved filter called nope"));
    assert!(parse_with_filters("filter:", &dir).unwrap_err().contains("needs the name"));
    // `parse` has no saved filters: `@docs` is text.
    assert!(!q("@docs").matches(&report().e()));
    assert!(q("@docs").matches(&f(r"C:\x\@docs.txt").e()));
    assert!(err("filter:docs").contains("no saved filter"));
    // A quoted @ is text even when the filter exists.
    assert!(!parse_with_filters("\"@docs\"", &dir).unwrap().matches(&report().e()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saved_filters_refuse_loops_and_bad_input() {
    let dir = temp_dir("loops");
    assert!(save_filter(&dir, "me", "@me").unwrap_err().contains("uses itself"));
    save_filter(&dir, "a", "@b").unwrap(); // @b is text until b exists
    assert!(save_filter(&dir, "b", "@a").unwrap_err().contains("uses itself"));
    assert!(save_filter(&dir, "bad name", "x").unwrap_err().contains("filter name"));
    assert!(save_filter(&dir, "", "x").unwrap_err().contains("needs a name"));
    assert!(save_filter(&dir, "broken", "(x").is_err());
    assert!(save_filter(&dir, "badsize", "size:huge").unwrap_err().contains("size:"));

    // A loop written by hand is caught when parsing.
    std::fs::write(filters_file(&dir), r#"[{"name":"a","query":"@b"},{"name":"b","query":"x @a"}]"#).unwrap();
    assert!(parse_with_filters("@a", &dir).unwrap_err().contains("uses itself"));
    // Errors inside a filter say which one.
    std::fs::write(filters_file(&dir), r#"[{"name":"c","query":"size:zz"}]"#).unwrap();
    let e = parse_with_filters("@c", &dir).unwrap_err();
    assert!(e.contains("@c") && e.contains("size:"), "{e}");
    // A broken file means no filters.
    std::fs::write(filters_file(&dir), "not json").unwrap();
    assert!(saved_filters(&dir).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- tokens

#[test]
fn tokenizer() {
    let w = |s: &str| Tok::Word { text: s.to_string(), quote_at: None };
    assert_eq!(tokenize("a (b|c) -d"), vec![w("a"), Tok::Open, w("b"), Tok::Or, w("c"), Tok::Close, Tok::Not, w("d")]);
    assert_eq!(tokenize("x-y !z"), vec![w("x-y"), Tok::Not, w("z")]);
    assert_eq!(tokenize("path:\"a b\" c"), vec![Tok::Word { text: "path:a b".into(), quote_at: Some(5) }, w("c")]);
    assert_eq!(tokenize("case:regex:a|(b) )"), vec![w("case:regex:a|(b)"), Tok::Close]);
}

// ---------------------------------------------------------------- performance

#[test]
#[ignore]
fn perf_one_million_entries() {
    let entries: Vec<T> = (0..1_000_000u64)
        .map(|i| {
            let ext = ["pdf", "txt", "jpg", "docx", "rs"][(i % 5) as usize];
            let name =
                if i % 7 == 0 { format!("Отчёт {i}.{ext}") } else { format!("Q{} Report {i}.{ext}", i % 4) };
            f(&format!(r"C:\Users\me\Documents\folder{}\{name}", i % 100)).size(i * 37 % (20 * MB))
        })
        .collect();
    for text in [
        "rep pdf",
        "отчёт",
        "*.pdf",
        "IMG_????.jpg",
        "size:>1mb ext:pdf;docx",
        "regex:^q\\d",
        "path:folder4 | dm:today",
    ] {
        let query = q(text);
        let started = std::time::Instant::now();
        let n = entries.iter().filter(|t| query.matches(&t.e())).count();
        let took = started.elapsed();
        println!("{text:32} {n:8} matches in {took:?}");
        assert!(took < std::time::Duration::from_secs(5), "{text} took {took:?}");
    }
}
