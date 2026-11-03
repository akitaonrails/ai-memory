//! FTS5 `MATCH` query preparation for user/agent-supplied search text.
//!
//! FTS5 treats `column:term` as a column-qualified search. Natural-language
//! queries that contain bare colons (`pick: handoff`, `memory: bootstrap`) make
//! SQLite error with `no such column: pick` because only `title` and `body`
//! exist on the FTS tables. Unknown bare column syntax is neutralised without
//! discarding deliberate FTS operators such as `OR`.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

/// Built-in English stopwords excluded from bare-query OR-joins when the
/// operator has not configured `[search.fts] stopwords` at all (issue #953).
/// Deliberately small and boring: high-document-frequency function words
/// that carry no retrieval signal but dominate BM25 through term frequency.
/// This is exactly the list `is_stopword` matched against before the
/// filter became configurable, so an install that never touches
/// `[search.fts]` sees byte-identical behaviour.
pub const DEFAULT_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can", "could", "did", "do",
    "does", "for", "from", "had", "has", "have", "how", "i", "if", "in", "is", "it", "its", "me",
    "my", "of", "on", "or", "our", "she", "should", "so", "that", "the", "their", "them", "they",
    "this", "to", "was", "we", "were", "what", "when", "where", "which", "who", "why", "will",
    "with", "would", "you", "your",
];

/// Normalized stopword set used to filter bare natural-language FTS queries
/// before the OR-join (see [`prepare_fts5_query`]).
///
/// Construction folds every entry to lowercase with [`str::to_lowercase`]
/// (full Unicode case folding, not ASCII-only) so callers never have to
/// pre-normalize a configured list, and comparison at match time folds the
/// query token the same way. This matters specifically for the non-English
/// installs this feature targets: the FTS content index itself is
/// `unicode61 remove_diacritics 2`, so a page body indexes "É"/"é"/"e" as
/// the same posting — an ASCII-only fold would make this filter weaker than
/// the index it feeds, missing a sentence-initial "É" or an all-caps "NÃO"
/// entirely. Folding does NOT strip diacritics, though: "à" and "a" stay
/// distinct tokens here (unlike in the content index), because that would
/// silently change the built-in English default (no accented entries) and
/// because a stopword list should say precisely which spelling to drop.
/// Concretely: what matters for this filter is how a query is actually
/// TYPED, not how wiki content is spelled — content matches through the
/// diacritic-folded index regardless, but a bare query's raw characters are
/// all this filter ever sees. A configured list for an accented language
/// should include both the accented and unaccented forms a user or agent
/// might type (e.g. Portuguese `"e"` and `"é"`, `"nao"` and `"não"`).
///
/// Also note the filter runs on `raw.split_whitespace()` tokens, before any
/// punctuation handling — an entry never matches a token with attached
/// punctuation (`"de,"`, `"que?"`), the same limitation the built-in English
/// list has always had (`"the,"` was never filtered either).
/// The bag-of-words fallback for syntax that does not parse strips quotes
/// and parens first, so `(the` is filtered there, while a word the user put
/// in quotes is always kept.
///
/// Cheap to clone (an `Arc`-shared set) so it can be threaded through a
/// `'static` closure (e.g. `ReaderPool::with_conn`) without re-hashing the
/// list on every search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtsStopwords(Arc<HashSet<String>>);

impl FtsStopwords {
    /// Build from an explicit list of words, already validated by the
    /// caller (`ai-memory-cli`'s `Config::load` bounds list size and entry
    /// length for `search.fts.stopwords`, issue #953). Entries are folded
    /// to lowercase here (see the struct doc for the exact fold) so this is
    /// the single place normalization happens.
    #[must_use]
    pub fn new<I, S>(words: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self(Arc::new(
            words
                .into_iter()
                .map(|w| w.as_ref().to_lowercase())
                .collect(),
        ))
    }

    /// No filtering at all: every token survives the OR-join. This is what
    /// an explicit `stopwords = []` in `config.toml` selects.
    #[must_use]
    pub fn none() -> Self {
        Self(Arc::new(HashSet::new()))
    }

    fn contains(&self, lowercased_token: &str) -> bool {
        self.0.contains(lowercased_token)
    }
}

impl Default for FtsStopwords {
    /// The built-in English list — an absent `[search.fts] stopwords` key
    /// resolves to this, so behaviour is unchanged for every existing
    /// install.
    fn default() -> Self {
        Self::new(DEFAULT_STOPWORDS.iter().copied())
    }
}

/// Sanitize free-text for use in `WHERE pages_fts MATCH ?`.
///
/// Returns an empty string when `raw` is empty/whitespace-only; callers
/// should skip the SQL query in that case.
///
/// Bare multi-word queries are joined with **`OR`**, not the FTS5 default
/// (`AND`). A natural-language query like "cross project search strategy"
/// otherwise requires every word to co-occur in one page — near-zero recall
/// for anything but single keywords. With `OR` + bm25 ranking (callers
/// `ORDER BY rank`), the best-matching pages still surface first. When the
/// caller supplies explicit FTS5 syntax (`OR` / `AND` / `NOT` / `NEAR` /
/// quoted phrases / parens) we preserve it verbatim instead.
///
/// `stopwords` is the operator-configured set (default: [`FtsStopwords::default`],
/// the built-in English list; `FtsStopwords::none()` disables filtering).
///
/// The input is bounded first: a NUL byte is folded to a space, and anything
/// past the 4 KiB cap is dropped, because FTS5's parser is superlinear in the
/// expression it is handed (see `bounded_query_text`).
#[must_use]
pub fn prepare_fts5_query(raw: &str, stopwords: &FtsStopwords) -> String {
    let bounded = bounded_query_text(raw);
    let raw = bounded.as_ref();
    let explicit_syntax = raw.contains('"')
        || raw.contains('(')
        || raw.contains(')')
        || raw
            .split_whitespace()
            .any(|t| matches!(t, "OR" | "AND" | "NOT" | "NEAR"));
    let tokens: Vec<String> = raw
        .split_whitespace()
        // Bare natural-language queries drop stopwords before the
        // OR-join: with no tokenizer-level stopword list, "the/of/a"
        // match nearly every page and BM25 term frequency lets a page
        // with five "the"s outrank the page whose CONTENT matches (seen
        // live: a release-procedure page beaten for a deploy question).
        // Explicit syntax that parses and quoted phrases are untouched, and
        // a query that is ONLY stopwords keeps them all — returning the
        // user's literal terms beats returning nothing. The active list is
        // operator-configured (`[search.fts] stopwords`, issue #953): the
        // built-in English list by default, a custom list for non-English
        // installs, or none at all. Comparison folds Unicode case (matching
        // `FtsStopwords`'s own fold) but never strips diacritics.
        .filter(|t| explicit_syntax || !stopwords.contains(&t.to_lowercase()))
        .flat_map(prepare_fts5_token)
        .collect();
    let tokens = if tokens.is_empty() && !explicit_syntax {
        raw.split_whitespace()
            .flat_map(prepare_fts5_token)
            .collect()
    } else {
        tokens
    };
    if tokens.is_empty() {
        return String::new();
    }
    let separator = if explicit_syntax { " " } else { " OR " };
    let candidate = tokens.join(separator);
    // Natural language trips the explicit-syntax path constantly — `(MoMA)`,
    // `'quoted phrases'`, a sentence that happens to contain OR — and a
    // preserved-verbatim mix can be grammatically invalid FTS5 (a quoted
    // OR-group adjacent to a bare term has no implicit AND, for one). The
    // MATCH must never error on user text, so validate the candidate against
    // a real FTS5 parser and degrade to the always-valid quoted bag of words
    // when it does not parse. Deliberate, well-formed operator queries pass
    // validation and are preserved.
    if fts5_query_parses(&candidate) {
        return candidate;
    }
    // Re-tokenize with every operator neutralised: strip quotes and parens,
    // then quote what remains. The result is a bare OR-join, so it drops
    // stopwords like the bare path above, keeping the words the user put in
    // quotes, and keeping every word when nothing else is left.
    let segments: Vec<&str> = raw.split('"').collect();
    // An unclosed quote leaves an even number of segments; the text after
    // it was never quoted, so it is filtered like the rest.
    let unclosed = segments.len().is_multiple_of(2);
    let last = segments.len() - 1;
    let words: Vec<(&str, bool)> = segments
        .into_iter()
        .enumerate()
        .flat_map(|(index, segment)| {
            let quoted = index % 2 == 1 && !(unclosed && index == last);
            segment
                .split(['(', ')'])
                .flat_map(str::split_whitespace)
                .map(move |word| (word, quoted))
        })
        .collect();
    let content: Vec<&str> = words
        .iter()
        .filter(|(word, quoted)| *quoted || !stopwords.contains(&word.to_lowercase()))
        .map(|(word, _)| *word)
        .collect();
    let kept = if content.is_empty() {
        words.iter().map(|(word, _)| *word).collect()
    } else {
        content
    };
    kept.into_iter()
        .map(quote_fts5_token)
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Terms stored in the auxiliary CJK index.
///
/// Each maximal Han/Kana/Hangul run contributes its scalar values and its
/// overlapping bigrams. Splitting only those runs keeps punctuation and Latin
/// text out of the derived index, while the bigrams retain enough adjacency
/// to avoid the very broad matches a character-only index would produce.
#[must_use]
pub(crate) fn cjk_index_terms(raw: &str) -> String {
    let mut terms = Vec::new();
    let mut run = Vec::new();
    for ch in raw.chars() {
        if is_cjk(ch) {
            run.push(ch);
        } else {
            append_cjk_terms(&mut terms, &mut run);
        }
    }
    append_cjk_terms(&mut terms, &mut run);
    terms.join(" ")
}

/// A MATCH expression for the CJK portion of a bounded user query.
///
/// `None` is the important common case: callers then execute the existing SQL
/// byte-for-byte. A multi-scalar run becomes an exact phrase of overlapping
/// bigrams; a single scalar uses the unigram fallback.
///
/// `None` also covers a query carrying `AND` / `NOT` / `NEAR`. This stream is
/// auxiliary: callers UNION it into the primary stream, so a run-based
/// OR-join of a `東京 NOT 大阪` query fuses the excluded term's own pages back
/// into the result set — the opposite of what the operator asked for, while
/// the primary stream honours it verbatim. Expanding each operand and
/// rebuilding the expression is the real fix; declining the stream meanwhile
/// restores the pre-#1433 answer instead of an inverted one. `OR` stays,
/// because the expander reproduces it faithfully.
///
/// A leading `title:` / `body:` qualifier is carried onto the run's phrase.
/// This stream is auxiliary — callers UNION it into the primary stream — so a
/// qualifier dropped here searches every column of `pages_cjk_fts`, including
/// `path_search`, and the union widens a query the user deliberately narrowed.
/// The two names are the ones [`prepare_fts5_token`] accepts; any other bare
/// column is left alone, because the primary stream neutralises it too.
#[must_use]
pub(crate) fn prepare_cjk_fts5_query(raw: &str) -> Option<String> {
    let bounded = bounded_query_text(raw);
    if bounded
        .split_whitespace()
        .any(|t| matches!(t, "AND" | "NOT" | "NEAR"))
    {
        return None;
    }
    let mut phrases = Vec::new();
    for token in bounded.split_whitespace() {
        // Grouping must not turn `(title:term)` into an unqualified match.
        // Like prepare_fts5_token, peel opening parentheses before the column:
        // https://www.sqlite.org/fts5.html#fts5_column_filters
        let token = token.trim_start_matches('(');
        let (column, text) = match token.split_once(':') {
            Some((column @ ("title" | "body"), text)) => (Some(column), text),
            _ => (None, token),
        };
        let mut run = Vec::new();
        for ch in text.chars() {
            if is_cjk(ch) {
                run.push(ch);
            } else {
                append_cjk_query(&mut phrases, &mut run, column);
            }
        }
        append_cjk_query(&mut phrases, &mut run, column);
    }
    (!phrases.is_empty()).then(|| phrases.join(" OR "))
}

fn append_cjk_terms(terms: &mut Vec<String>, run: &mut Vec<char>) {
    if run.is_empty() {
        return;
    }
    terms.extend(run.iter().map(char::to_string));
    terms.extend(run.windows(2).map(|pair| pair.iter().collect::<String>()));
    run.clear();
}

fn append_cjk_query(phrases: &mut Vec<String>, run: &mut Vec<char>, column: Option<&str>) {
    if run.is_empty() {
        return;
    }
    let prefix = column.map_or_else(String::new, |column| format!("{column}:"));
    if run.len() == 1 {
        phrases.push(format!("{prefix}\"{}\"", run[0]));
    } else {
        let bigrams = run
            .windows(2)
            .map(|pair| pair.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join(" ");
        phrases.push(format!("{prefix}\"{bigrams}\""));
    }
    run.clear();
}

fn is_cjk(ch: char) -> bool {
    ch.is_alphanumeric()
        && matches!(
        ch as u32,
        0x1100..=0x11ff
            | 0x2e80..=0x2fff
            // CJK Symbols and Punctuation holds a handful of LETTERS among its
            // punctuation: the iteration marks 々 〆 〻 and the kana repeat
            // marks. 々 is how 日々, 時々, 人々, 我々 and 様々 are written, so
            // excluding the block split those words into single scalars on both
            // the index and the query side, leaving the character-only match
            // the bigrams exist to prevent. The `is_alphanumeric` guard above
            // keeps the block's actual punctuation (、 。 「) out.
            | 0x3005..=0x3006
            | 0x3031..=0x3035
            | 0x303b
            | 0x3040..=0x30ff
            | 0x3130..=0x318f
            | 0x31f0..=0x31ff
            | 0x3400..=0x4dbf
            | 0x4e00..=0x9fff
            | 0xa960..=0xa97f
            | 0xac00..=0xd7af
            | 0xd7b0..=0xd7ff
            | 0xf900..=0xfaff
            | 0xff66..=0xff9f
            | 0x20000..=0x323af
        )
}

/// Upper bound (bytes) on the untrusted search text this module tokenises.
///
/// FTS5's query parser is superlinear in the expression it is handed:
/// measured here, 10k distinct terms spent ~60 ms inside
/// [`prepare_fts5_query`], 40k ~1.9 s and 100k ~17.5 s — before the real
/// `MATCH` ran. Search text is a phrase, not a document, so the tail of
/// anything longer is dropped instead of parsed.
const MAX_FTS_QUERY_BYTES: usize = 4_096;

/// Bound untrusted search text before anything parses it.
///
/// A NUL cannot appear in indexed text, but it used to reach the FTS5 parser,
/// which reads a quoted phrase as a C string: `"\0"` fails as an unterminated
/// string, so `memory_query` surfaced `sqlite: unterminated string` as an
/// internal error instead of a result. Folding NUL to a space keeps the rest
/// of the token searchable. Past [`MAX_FTS_QUERY_BYTES`] the text is cut on a
/// UTF-8 boundary; a cut that lands inside a phrase or paren leaves invalid
/// FTS5 syntax, which [`fts5_query_parses`] already degrades to the quoted
/// bag of words. Borrows when neither applies, so the common query path
/// allocates nothing.
fn bounded_query_text(raw: &str) -> Cow<'_, str> {
    let bounded = &raw[..raw.floor_char_boundary(MAX_FTS_QUERY_BYTES)];
    if !bounded.contains('\0') {
        return Cow::Borrowed(bounded);
    }
    Cow::Owned(bounded.replace('\0', " "))
}

/// Whether FTS5's own parser accepts `query`. Uses a throwaway in-memory
/// table (microseconds; search-path frequency, not ingest-path) so the
/// judgement is the engine's, not a re-implementation of its grammar.
fn fts5_query_parses(query: &str) -> bool {
    thread_local! {
        // One probe connection per thread: opening a fresh in-memory
        // database per search query was measurable overhead on the
        // read-pool hot path (post-audit nit).
        static PROBE: Option<rusqlite::Connection> = {
            let conn = rusqlite::Connection::open_in_memory().ok();
            if let Some(c) = &conn
                && c.execute_batch("CREATE VIRTUAL TABLE fts_probe USING fts5(title, body)")
                    .is_err()
            {
                None
            } else {
                conn
            }
        };
    }
    PROBE.with(|probe| {
        probe.as_ref().is_some_and(|conn| {
            conn.query_row(
                "SELECT count(*) FROM fts_probe WHERE fts_probe MATCH ?1",
                [query],
                |row| row.get::<_, i64>(0),
            )
            .is_ok()
        })
    })
}

fn prepare_fts5_token(token: &str) -> Vec<String> {
    // Group parens hug the token (`(title:handoff`, `body:deploy)`), so peel
    // them before the column checks below, which look at the token's start.
    let open = token.len() - token.trim_start_matches('(').len();
    let close = token.len() - token.trim_end_matches(')').len();
    if (open > 0 || close > 0) && open + close < token.len() {
        let mut parts = prepare_fts5_token(&token[open..token.len() - close]);
        if let Some(first) = parts.first_mut() {
            first.insert_str(0, &token[..open]);
        }
        if let Some(last) = parts.last_mut() {
            last.push_str(&token[token.len() - close..]);
        }
        return parts;
    }

    if has_unknown_bare_column(token) {
        return token
            .replace(':', " ")
            .split_whitespace()
            .map(quote_fts5_token)
            .collect();
    }

    // A known column filter keeps its column when its term needs quoting:
    // `title:ai-memory` becomes `title:("ai-memory" OR "ai memory")`, not a
    // quoted phrase that also searches for the literal word `title`.
    // A second colon in the term (`title:10:30`) needs that same quoting:
    // left bare, FTS5 reads the inner `10` as a column name and rejects the
    // whole expression, so the query degrades to the bag of words — which
    // drops the filter and searches every column for the word `title`.
    if !token.contains('"')
        && let Some((column, term)) = token.split_once(':')
        && matches!(column, "title" | "body")
        && !term.is_empty()
        && (should_quote_fts5_token(term) || term.contains(':'))
    {
        return vec![format!("{column}:{}", quote_fts5_token(term))];
    }

    if should_quote_fts5_token(token) {
        vec![quote_fts5_token(token)]
    } else {
        vec![token.to_string()]
    }
}

fn has_unknown_bare_column(token: &str) -> bool {
    token.contains(':')
        && !token.contains('"')
        && !token.starts_with("title:")
        && !token.starts_with("body:")
}

fn should_quote_fts5_token(token: &str) -> bool {
    if token.starts_with('"') && token.ends_with('"') {
        return false;
    }
    // Quote any token carrying ASCII punctuation so FTS5 treats it as a literal
    // phrase instead of erroring on its query grammar — e.g. a filename like
    // `current.md` otherwise yields `fts5: syntax error near "."`. A trailing
    // `*` (the FTS5 prefix operator) is allowed through bare; accented letters
    // and digits are unicode (not ASCII punctuation) so recall keeps accents.
    let core = token.strip_suffix('*').unwrap_or(token);
    // `:` is column syntax (handled by `has_unknown_bare_column`, or preserved
    // for known `title:`/`body:` columns) — it must not trigger quoting here.
    core.chars().any(|c| c.is_ascii_punctuation() && c != ':')
}

fn quote_fts5_token(token: &str) -> String {
    // FTS5 escapes `"` by doubling it. A token carrying a literal quote is an
    // explicit-phrase fragment — keep the simple escaped form (don't expand it).
    if token.contains('"') {
        return format!("\"{}\"", token.replace('"', "\"\""));
    }
    // Otherwise emit BOTH the whole token and a punctuation-stripped sub-token
    // phrase, OR'd, because the content tokenizer and the path index disagree
    // on punctuation:
    //   tokenize = "unicode61 remove_diacritics 2 tokenchars '_-'"
    // keeps `_ -` INSIDE tokens (so a body mention of `ai-memory` indexes as
    // the single token `ai-memory`), while `ops::path_search_text` pre-expands
    // `/ . - _` to spaces in the path index (so a path `ui-refresh-…` indexes
    // the sub-tokens `ui`, `refresh`, …). `/` and `.` are separators either way.
    // Neither form alone matches both: `"ai-memory"` matches the content token
    // but not the split path index; `"ai memory"` matches the path but not the
    // content token. OR-ing the two makes a search for `ai-memory` / `ui-refresh`
    // hit whichever surface indexed it. (Quoting both also neutralises the
    // punctuation that would otherwise be FTS5 query grammar — the original
    // `current.md` → `syntax error` bug.) With no punctuation the two coincide
    // and we emit a single phrase.
    //
    // A trailing `*` is the FTS5 prefix operator. Inside a quoted string it is
    // only a separator, so it moves outside each closing quote instead:
    // `ai-mem*` becomes `("ai-mem"* OR "ai mem"*)`, which FTS5 reads as a
    // prefix match on the phrase's last token.
    let (token, prefix) = match token.strip_suffix('*') {
        Some(core) if core.chars().any(char::is_alphanumeric) => (core, "*"),
        _ => (token, ""),
    };
    let split = token
        .chars()
        .map(|c| if c.is_ascii_punctuation() { ' ' } else { c })
        .collect::<String>();
    let split = split.split_whitespace().collect::<Vec<_>>().join(" ");
    if split.is_empty() || split == token {
        format!("\"{token}\"{prefix}")
    } else {
        format!("(\"{token}\"{prefix} OR \"{split}\"{prefix})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pre-#953 test called `prepare_fts5_query(raw)` against the
    /// built-in English list; this keeps that call shape so the existing
    /// test bodies below are unchanged (default list reproduces current
    /// behaviour exactly).
    fn prepare_fts5_query(raw: &str) -> String {
        super::prepare_fts5_query(raw, &FtsStopwords::default())
    }

    #[test]
    fn cjk_index_terms_are_bounded_to_cjk_runs() {
        assert_eq!(
            cjk_index_terms("pay 支付服务 now"),
            "支 付 服 务 支付 付服 服务"
        );
        assert_eq!(
            cjk_index_terms("日本語・テスト"),
            "日 本 語 日本 本語 テ ス ト テス スト"
        );
        assert_eq!(cjk_index_terms("plain latin"), "");
        // The iteration mark is a letter in CJK Symbols and Punctuation and
        // must continue a run, or 日々 is indexed as two one-char runs and is
        // unfindable as a unit.
        assert_eq!(
            cjk_index_terms("日々の作業"),
            "日 々 の 作 業 日々 々の の作 作業"
        );
        // Control: the punctuation sharing that block still breaks a run.
        assert_eq!(cjk_index_terms("日、本"), "日 本");
    }

    #[test]
    fn cjk_queries_require_adjacent_bigrams() {
        assert_eq!(
            prepare_cjk_fts5_query("支付服务 status"),
            Some("\"支付 付服 服务\"".to_string())
        );
        assert_eq!(
            prepare_cjk_fts5_query("支 / 韓"),
            Some("\"支\" OR \"韓\"".to_string())
        );
        assert_eq!(prepare_cjk_fts5_query("latin only"), None);
    }

    #[test]
    fn cjk_stream_declines_a_query_the_expander_would_invert() {
        assert_eq!(prepare_cjk_fts5_query("東京 NOT 大阪"), None);
        assert_eq!(prepare_cjk_fts5_query("東京 AND 大阪"), None);
        assert_eq!(prepare_cjk_fts5_query("東京 NEAR 大阪"), None);
        // Control: `OR` is the one operator the run expander reproduces, so
        // the auxiliary stream still runs for it — and a lowercase `not` is
        // not FTS5 syntax, so it must not suppress the stream either.
        let both = Some("\"東京\" OR \"大阪\"".to_string());
        assert_eq!(prepare_cjk_fts5_query("東京 OR 大阪"), both);
        assert_eq!(prepare_cjk_fts5_query("東京 not 大阪"), both);
        assert_parses(&prepare_cjk_fts5_query("東京 OR 大阪").unwrap());
    }

    #[test]
    fn cjk_queries_keep_a_known_column_filter() {
        let title = prepare_cjk_fts5_query("title:支付服务").unwrap();
        assert_eq!(title, "title:\"支付 付服 服务\"");
        assert_parses(&title);
        for query in ["(title:支付服务)", "((title:支付服务))"] {
            let grouped = prepare_cjk_fts5_query(query).unwrap();
            assert_eq!(grouped, title, "query={query}");
            assert_parses(&grouped);
        }
        let body = prepare_cjk_fts5_query("body:支付服务").unwrap();
        assert_eq!(body, "body:\"支付 付服 服务\"");
        assert_parses(&body);
        for query in ["(body:支付服务)", "((body:支付服务))"] {
            let grouped = prepare_cjk_fts5_query(query).unwrap();
            assert_eq!(grouped, body, "query={query}");
            assert_parses(&grouped);
        }
        let alternatives = prepare_cjk_fts5_query("((title:支付服务) OR (body:支付服务))").unwrap();
        assert_eq!(alternatives, format!("{title} OR {body}"));
        assert_parses(&alternatives);
        let single = prepare_cjk_fts5_query("title:支").unwrap();
        assert_eq!(single, "title:\"支\"");
        assert_parses(&single);
        // Control: the bare query is untouched, and a column FTS5 does not
        // know stays unqualified here exactly as the primary stream
        // neutralises it, rather than being emitted as invalid syntax.
        assert_eq!(
            prepare_cjk_fts5_query("支付服务"),
            Some("\"支付 付服 服务\"".to_string())
        );
        let unknown = prepare_cjk_fts5_query("author:支付服务").unwrap();
        assert_eq!(unknown, "\"支付 付服 服务\"");
        assert_parses(&unknown);
    }

    #[test]
    fn cjk_queries_keep_the_japanese_iteration_mark() {
        let query = prepare_cjk_fts5_query("日々").unwrap();
        assert_eq!(query, "\"日々\"");
        assert_parses(&query);
        // Control: the same block's punctuation still separates two runs,
        // so this is a widened letter range and not a widened block.
        assert_eq!(
            prepare_cjk_fts5_query("日、本"),
            Some("\"日\" OR \"本\"".to_string())
        );
    }

    #[test]
    fn cjk_query_input_is_bounded_before_expansion() {
        let query = prepare_cjk_fts5_query(&"日".repeat(10_000)).unwrap();
        assert!(
            query.len() < 12_500,
            "expanded query was {} bytes",
            query.len()
        );
        assert_parses(&query);
    }

    /// The engine, not this module, is the judge of validity: every
    /// prepared query must MATCH without error on a real FTS5 table.
    fn assert_parses(prepared: &str) {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(title, body)")
            .unwrap();
        let res = conn.query_row(
            "SELECT count(*) FROM t WHERE t MATCH ?1",
            [prepared],
            |row| row.get::<_, i64>(0),
        );
        assert!(res.is_ok(), "FTS5 rejected {prepared:?}: {res:?}");
    }

    /// Regression (found by the LongMemEval harness): a natural-language
    /// question containing parens and apostrophe-quotes tripped the
    /// explicit-syntax path and produced invalid FTS5 — `fts5: syntax
    /// error near "OR"` — failing the whole memory_query call.
    #[test]
    fn natural_language_with_parens_and_quotes_never_errors() {
        let raw = "How many days passed between my visit to the Museum of \
                   Modern Art (MoMA) and the 'Ancient Civilizations' exhibit \
                   at the Metropolitan Museum of Art?";
        let prepared = prepare_fts5_query(raw);
        assert_parses(&prepared);
        // and the degraded form still carries the searchable words
        assert!(prepared.contains("MoMA"));
        assert!(prepared.contains("Civilizations"));
    }

    /// A NUL byte cannot appear in indexed text, but it used to reach the
    /// FTS5 query parser: a token that starts with one is quoted (`"\0x"`),
    /// and the parser reads the phrase as end-of-input — `sqlite:
    /// unterminated string` came back from `memory_query` as an internal
    /// error. This module's contract is that MATCH never errors on user text.
    #[test]
    fn nul_bytes_never_reach_the_match_expression() {
        for raw in ["\0", "\0x", "x\0", "\0 \0", "(\0)", "a\0b", "title:\0x"] {
            let prepared = prepare_fts5_query(raw);
            assert!(!prepared.contains('\0'), "NUL survived into {prepared:?}");
            if !prepared.is_empty() {
                assert_parses(&prepared);
            }
        }
    }

    /// The FTS5 query parser is superlinear in the number of terms: 100k
    /// distinct terms spent ~17 s inside `prepare_fts5_query` alone, before
    /// the real MATCH ran. Search text is a phrase, not a document, so the
    /// tail of an oversized query must be dropped before the parser sees it.
    #[test]
    fn oversized_query_text_is_bounded_before_parsing() {
        let raw = (0..100_000)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let prepared = prepare_fts5_query(&raw);
        assert!(
            prepared.len() <= 32 * 1024,
            "prepared expression is not bounded: {} bytes",
            prepared.len()
        );
        assert_parses(&prepared);
    }

    /// The bound must not touch an ordinary query: this is the exact
    /// expression every search has produced since the OR-join landed.
    #[test]
    fn ordinary_query_text_is_untouched_by_the_bound() {
        assert_eq!(
            prepare_fts5_query("cross project search strategy"),
            "cross OR project OR search OR strategy"
        );
    }

    /// 4 KiB lands mid-character for a 3-byte code point (4096 % 3 != 0); the
    /// cut has to move back to a UTF-8 boundary instead of panicking.
    #[test]
    fn the_bound_cuts_on_a_character_boundary() {
        let raw = "日".repeat(2_000);
        assert!(raw.len() > MAX_FTS_QUERY_BYTES);
        let prepared = prepare_fts5_query(&raw);
        assert_parses(&prepared);
    }

    /// Truncation can land inside a quoted phrase; whatever the cut leaves,
    /// the prepared expression must still parse and MATCH without error.
    #[test]
    fn truncated_explicit_syntax_stays_parseable() {
        let raw = format!("\"unclosed {}", "w ".repeat(3_000));
        let prepared = prepare_fts5_query(&raw);
        assert_parses(&prepared);
    }

    #[test]
    fn hostile_operator_soup_never_errors() {
        for raw in [
            "((",
            "\"unclosed phrase",
            "NEAR(",
            "OR",
            "a OR",
            "OR b",
            "NOT",
            ") misplaced (",
            "col:val:col ( OR \" NEAR",
            "\"\"\"",
        ] {
            let prepared = prepare_fts5_query(raw);
            if !prepared.is_empty() {
                assert_parses(&prepared);
            }
        }
    }

    #[test]
    fn deliberate_well_formed_syntax_is_preserved() {
        let prepared = prepare_fts5_query("title:handoff OR body:deploy");
        assert_parses(&prepared);
        assert_eq!(prepared, "title:handoff OR body:deploy");
        // Quoted phrases were re-tokenized into escaped-quote form long
        // before the validation fallback existed; the contract here is
        // "still valid FTS5 with AND preserved", not byte identity.
        let phrase = prepare_fts5_query("\"exact phrase\" AND deploy");
        assert_parses(&phrase);
        assert!(phrase.contains(" AND deploy"), "{phrase}");
    }

    #[test]
    fn colon_is_not_column_syntax() {
        // Bare multi-word → OR-joined (no explicit operator present).
        // `ai-memory` expands to BOTH the whole token (matches content) and
        // the sub-token phrase (matches the split path index) — see
        // `quote_fts5_token`.
        let q = prepare_fts5_query("pick: handoff ai-memory");
        assert_eq!(q, "\"pick\" OR handoff OR (\"ai-memory\" OR \"ai memory\")");
    }

    #[test]
    fn bare_multi_word_is_or_joined() {
        // The recall fix: every word no longer has to co-occur.
        assert_eq!(
            prepare_fts5_query("cross project search strategy"),
            "cross OR project OR search OR strategy"
        );
    }

    #[test]
    fn portuguese_accented_terms_or_join_and_keep_accents() {
        // PT natural-language query: tokens preserved (accents intact),
        // joined with OR so a page matching any term is found.
        assert_eq!(
            prepare_fts5_query("descrição testes commits"),
            "descrição OR testes OR commits"
        );
    }

    #[test]
    fn single_word_has_no_or() {
        assert_eq!(prepare_fts5_query("handoff"), "handoff");
    }

    /// Regression: a filename like `current.md` used to pass through bare and
    /// FTS5 errored with `syntax error near "."`. Quoting it as a phrase both
    /// avoids the error and matches `architecture-current.md` (the tokens
    /// `current` + `md` are adjacent in the indexed path).
    #[test]
    fn dotted_filename_token_is_quoted() {
        // Whole token OR sub-token phrase. The split form (`current md`)
        // matches the tokenised path; the whole form covers content tokens.
        assert_eq!(
            prepare_fts5_query("current.md"),
            "(\"current.md\" OR \"current md\")"
        );
        assert_eq!(
            prepare_fts5_query("00-index.md"),
            "(\"00-index.md\" OR \"00 index md\")"
        );
        assert_eq!(
            prepare_fts5_query("a/b/c.md"),
            "(\"a/b/c.md\" OR \"a b c md\")"
        );
    }

    /// Regression for the live-found bug: searching `ui-refresh` returned
    /// nothing even though `follow-ups/ui-refresh-scroll-restoration.md`
    /// exists. The old quoting produced `"ui-refresh"`, which FTS5 does NOT
    /// match against the indexed `ui refresh`; the sub-token phrase
    /// `"ui refresh"` does. See the real-FTS5 test in `ops.rs`.
    #[test]
    fn hyphenated_token_quotes_as_subtoken_phrase() {
        assert_eq!(
            prepare_fts5_query("ui-refresh"),
            "(\"ui-refresh\" OR \"ui refresh\")"
        );
        assert_eq!(
            prepare_fts5_query("scroll-restoration"),
            "(\"scroll-restoration\" OR \"scroll restoration\")"
        );
    }

    /// The FTS5 prefix operator (`term*`) must survive — a trailing `*` is not
    /// quoted away.
    #[test]
    fn prefix_star_token_stays_bare() {
        assert_eq!(prepare_fts5_query("curr*"), "curr*");
    }

    /// A punctuated token is quoted, and inside an FTS5 string `*` is only a
    /// separator. The prefix operator has to follow each closing quote, or
    /// `ai-mem*` silently becomes an exact search for `ai-mem`.
    #[test]
    fn prefix_star_survives_quoting_of_punctuated_tokens() {
        assert_eq!(prepare_fts5_query("ai-mem*"), r#"("ai-mem"* OR "ai mem"*)"#);
        assert_eq!(
            prepare_fts5_query("current.m*"),
            r#"("current.m"* OR "current m"*)"#
        );
        // Unknown column syntax is split, and the prefix stays on its term.
        assert_eq!(prepare_fts5_query("pick:hand*"), r#""pick" OR "hand"*"#);
        // A token of punctuation alone has no term to prefix-match.
        assert_eq!(quote_fts5_token("-*"), r#""-*""#);
        for raw in ["ai-mem*", "current.m*", "pick:hand*", "a-* b", "-*", "x.*"] {
            let prepared = prepare_fts5_query(raw);
            if !prepared.is_empty() {
                assert_parses(&prepared);
            }
        }
    }

    #[test]
    fn empty_yields_empty() {
        assert_eq!(prepare_fts5_query("   "), "");
    }

    #[test]
    fn quote_emits_whole_and_subtoken_phrase() {
        // Punctuated identifier → both forms OR'd.
        assert_eq!(
            quote_fts5_token("ai-memory"),
            r#"("ai-memory" OR "ai memory")"#
        );
        // A literal-quote fragment keeps the simple escaped form (no expansion).
        assert_eq!(quote_fts5_token(r#"say "hello""#), r#""say ""hello""""#);
        // No punctuation → single phrase.
        assert_eq!(quote_fts5_token("handoff"), r#""handoff""#);
    }

    #[test]
    fn boolean_operators_are_preserved() {
        assert_eq!(prepare_fts5_query("quick OR slow"), "quick OR slow");
    }

    /// AND is the FTS5 default but operators can be explicit — when the
    /// caller writes one, the OR-join must NOT mangle it into
    /// `foo OR AND OR bar`. Same for NOT and NEAR. (The escape hatch from
    /// the broad-recall default is what makes the OR-join safe to land.)
    #[test]
    fn explicit_and_operator_is_preserved() {
        assert_eq!(prepare_fts5_query("foo AND bar"), "foo AND bar");
    }

    #[test]
    fn explicit_not_operator_is_preserved() {
        assert_eq!(prepare_fts5_query("foo NOT bar"), "foo NOT bar");
    }

    #[test]
    fn explicit_near_operator_is_preserved() {
        assert_eq!(prepare_fts5_query("foo NEAR bar"), "foo NEAR bar");
    }

    /// A query containing a quoted phrase is treated as explicit FTS5
    /// syntax — `"exact phrase" baz` must not become
    /// `"exact" OR "phrase" OR baz` (which destroys the phrase semantics).
    /// The exact assertion is "space-joined, not OR-joined"; what the
    /// individual tokens look like after `prepare_fts5_token` is a
    /// separate concern (and unchanged from pre-#58 behaviour).
    #[test]
    fn quoted_phrase_query_is_not_or_joined() {
        let q = prepare_fts5_query("\"exact phrase\" baz");
        assert!(
            !q.contains(" OR "),
            "explicit quoted-phrase query must not get OR-joined; got {q}"
        );
    }

    /// Same escape-hatch logic for parenthesised sub-expressions —
    /// `(foo OR bar) AND baz` must survive unmangled.
    #[test]
    fn parenthesised_query_is_not_or_joined() {
        let q = prepare_fts5_query("(foo OR bar) AND baz");
        assert!(
            !q.contains("OR (foo"),
            "parens detection must skip OR-join entirely; got {q}"
        );
        assert!(
            q.contains("AND"),
            "explicit AND inside parens query must survive; got {q}"
        );
    }

    #[test]
    fn known_columns_are_preserved() {
        assert_eq!(prepare_fts5_query("title:handoff"), "title:handoff");
    }

    /// A known column filter whose term carries punctuation keeps the filter
    /// on the quoted term, instead of quoting `title:` into the phrase and
    /// searching every column for the word "title".
    #[test]
    fn known_column_filter_survives_quoting_of_its_term() {
        assert_eq!(
            prepare_fts5_query("title:ai-memory"),
            r#"title:("ai-memory" OR "ai memory")"#
        );
        assert_eq!(
            prepare_fts5_query("body:current.md OR title:handoff"),
            r#"body:("current.md" OR "current md") OR title:handoff"#
        );
        for raw in ["title:ai-memory", "body:a.b c", "title:-", "title:a:b"] {
            let prepared = prepare_fts5_query(raw);
            if !prepared.is_empty() {
                assert_parses(&prepared);
            }
        }
    }

    /// A second colon inside a known column's term must not defeat the
    /// filter. FTS5 reads the inner `10` as a column name and rejects the
    /// expression, so the whole query degrades to the quoted bag of words:
    /// that drops the column filter and searches every column for the
    /// literal word "title", the outcome
    /// `known_column_filter_survives_quoting_of_its_term` exists to prevent.
    #[test]
    fn known_column_filter_survives_a_second_colon_in_its_term() {
        assert_eq!(
            prepare_fts5_query("title:10:30"),
            r#"title:("10:30" OR "10 30")"#
        );
        assert_parses(&prepare_fts5_query("title:10:30"));
    }

    /// A column filter inside a group keeps its column: the paren attached
    /// to `title:` must not turn it into an unknown column whose name is
    /// then searched as the literal word "title".
    #[test]
    fn grouped_known_column_filter_keeps_its_column() {
        assert_eq!(prepare_fts5_query("(title:handoff)"), "(title:handoff)");
        assert_eq!(
            prepare_fts5_query("(title:handoff OR body:deploy)"),
            "(title:handoff OR body:deploy)"
        );
        assert_eq!(
            prepare_fts5_query("(title:ai-memory) AND deploy"),
            r#"(title:("ai-memory" OR "ai memory")) AND deploy"#
        );
        for raw in [
            "(title:handoff)",
            "((title:a)",
            "(pick: handoff)",
            "(",
            "(title:)",
        ] {
            let prepared = prepare_fts5_query(raw);
            if !prepared.is_empty() {
                assert_parses(&prepared);
            }
        }
    }

    // --- issue #953: configurable stopword list -----------------------

    /// A custom (Portuguese) list filters the words on that list, but never
    /// touches the English words that are not on it — a Portuguese config
    /// does not accidentally strip English content out of a mixed-language
    /// corpus.
    #[test]
    fn custom_list_filters_configured_words_not_english_ones() {
        let pt = FtsStopwords::new([
            "o", "a", "os", "as", "de", "da", "do", "em", "uma", "um", "com", "para", "que", "se",
            "no", "na",
        ]);
        assert_eq!(
            super::prepare_fts5_query("o teste de integração", &pt),
            "teste OR integração"
        );
        // English stopwords are untouched by the pt list.
        assert_eq!(
            super::prepare_fts5_query("the deploy of the release", &pt),
            "the OR deploy OR of OR the OR release"
        );
    }

    /// `stopwords = []` (`FtsStopwords::none()`) disables filtering
    /// entirely: every bare token, English or not, survives the OR-join.
    #[test]
    fn empty_list_disables_filtering() {
        let none = FtsStopwords::none();
        assert_eq!(
            super::prepare_fts5_query("the of and search", &none),
            "the OR of OR and OR search"
        );
    }

    /// The only-stopwords escape hatch (returning the user's literal terms
    /// beats returning nothing) holds for a custom list too, not just the
    /// built-in English one.
    #[test]
    fn only_configured_stopwords_query_keeps_its_terms() {
        let pt = FtsStopwords::new(["o", "a", "de"]);
        assert_eq!(super::prepare_fts5_query("o a de", &pt), "o OR a OR de");
    }

    /// Explicit FTS5 syntax bypasses the configured filter exactly as it
    /// bypasses the default one.
    #[test]
    fn explicit_syntax_bypasses_custom_list_too() {
        let pt = FtsStopwords::new(["o", "de"]);
        assert_eq!(super::prepare_fts5_query("o AND de", &pt), "o AND de");
    }

    /// Text that trips the explicit-syntax path but does not parse degrades
    /// to the quoted bag of words. That bag is a bare OR-join, so it drops
    /// stopwords the way the bare path does, folding case: `What is the
    /// cause of the stuck deploy` must not OR-in `What`/`is`/`the`/`of` just
    /// because the sentence ends in `(prod)`.
    #[test]
    fn unparseable_syntax_fallback_drops_stopwords() {
        let prepared = prepare_fts5_query("What is the cause of the stuck deploy (prod)");
        assert_parses(&prepared);
        assert_eq!(prepared, "\"cause\" OR \"stuck\" OR \"deploy\" OR \"prod\"");
    }

    /// The fallback filters through the configured list, not the built-in
    /// English one, and `FtsStopwords::none()` keeps every word.
    #[test]
    fn unparseable_syntax_fallback_uses_configured_list() {
        let pt = FtsStopwords::new(["o", "de"]);
        assert_eq!(
            super::prepare_fts5_query("the cause de o deploy (prod)", &pt),
            "\"the\" OR \"cause\" OR \"deploy\" OR \"prod\""
        );
        assert_eq!(
            super::prepare_fts5_query("the cause of the deploy (prod)", &FtsStopwords::none()),
            "\"the\" OR \"cause\" OR \"of\" OR \"the\" OR \"deploy\" OR \"prod\""
        );
    }

    /// A word the user quoted is kept in the fallback even when it is a
    /// stopword, as a quoted phrase is on the explicit-syntax path; the same
    /// word outside quotes is still dropped.
    #[test]
    fn unparseable_syntax_fallback_keeps_quoted_stopwords() {
        let prepared = prepare_fts5_query("\"the\" deploy (prod of the");
        assert_parses(&prepared);
        assert_eq!(prepared, "\"the\" OR \"deploy\" OR \"prod\"");
    }

    /// An unclosed quote does not protect the words after it: they were never
    /// quoted, so their stopwords are dropped like any others.
    #[test]
    fn unparseable_syntax_fallback_filters_after_an_unclosed_quote() {
        let prepared = prepare_fts5_query("the 12\" (rack) of the server");
        assert_parses(&prepared);
        assert_eq!(prepared, "\"12\" OR \"rack\" OR \"server\"");
    }

    /// A fallback query made only of stopwords keeps them all, as the bare
    /// path does: the user's literal words beat an empty search.
    #[test]
    fn unparseable_syntax_fallback_of_only_stopwords_keeps_its_terms() {
        let prepared = prepare_fts5_query("what is (the)");
        assert_parses(&prepared);
        assert_eq!(prepared, "\"what\" OR \"is\" OR \"the\"");
    }

    /// Worked example from the issue / docs: a Portuguese natural-language
    /// question, filtered against a Portuguese stopword list, keeps only the
    /// content words (OR-joined) and preserves their accents — the
    /// stopword comparison is ASCII-lowercase, never diacritic-stripping.
    #[test]
    fn portuguese_stopword_list_worked_example() {
        let pt = FtsStopwords::new([
            "o", "a", "os", "as", "de", "da", "do", "em", "uma", "um", "com", "para", "que", "se",
            "no", "na", "e", "é",
        ]);
        assert_eq!(
            super::prepare_fts5_query("o que fazer quando o teste de integração falha no CI", &pt),
            "fazer OR quando OR teste OR integração OR falha OR CI"
        );
    }

    /// Stopword comparison folds full Unicode case (`str::to_lowercase`, not
    /// ASCII-only) but never strips diacritics. A capitalized or all-caps
    /// accented query token still matches a lowercase configured entry
    /// (sentence-initial "É", all-caps "NÃO"), because the content FTS index
    /// itself is `unicode61 remove_diacritics 2` and this filter would
    /// otherwise be weaker than the index for exactly the languages this
    /// feature targets. But an accent is never folded away: "e" and "é"
    /// stay distinct tokens, so a list must spell out every form a user
    /// might actually type.
    #[test]
    fn stopword_comparison_is_unicode_case_insensitive_but_keeps_diacritics() {
        let pt = FtsStopwords::new(["é", "não"]);
        // Sentence-initial capital + accent still matches the lowercase entry.
        assert_eq!(super::prepare_fts5_query("É teste", &pt), "teste");
        // An all-caps accented entry in the query also folds and matches.
        assert_eq!(super::prepare_fts5_query("NÃO teste", &pt), "teste");
        // "e" (no accent) is a different token entirely and is not filtered
        // by an "é" entry — diacritics are never stripped, only case is folded.
        assert_eq!(super::prepare_fts5_query("e teste", &pt), "e OR teste");
    }
}
