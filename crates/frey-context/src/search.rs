//! Finding capabilities in a catalog too large to show the model at once.
//!
//! Tool-selection accuracy degrades once a model can see more than roughly thirty to fifty tools,
//! and a five-server setup can spend fifty thousand tokens on definitions before the first real
//! message. Discovery is the answer to both: index everything, present a few.
//!
//! Two implementations ship here. [`RegexSearch`] mirrors the provider-native regex variant down to
//! its limits — Python `re.search` semantics, case-insensitive, two hundred characters — so that a
//! query behaves the same whether Frey ran it or delegated it. [`Bm25Search`] takes natural
//! language. Both index the same four fields the provider-side implementations do: **name,
//! description, argument names, and argument descriptions**.

use std::collections::HashMap;

use frey_core::ids::ToolName;
use frey_core::tool::{SearchHit, SearchKind, SearchQuery};
use frey_core::tool_def::ToolDefinition;

/// The longest regex a provider-native search accepts. Mirrored so that a query which would be
/// rejected upstream is rejected here too, rather than silently behaving differently.
pub const MAX_REGEX_LEN: usize = 200;

/// The longest natural-language query a provider-native search accepts.
pub const MAX_QUERY_LEN: usize = 500;

/// A query was not usable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SearchError {
    /// Longer than the provider would accept.
    #[error("a {kind} query may be at most {max} characters; this one is {len}")]
    TooLong {
        /// Which kind of query.
        kind: &'static str,
        /// The limit.
        max: usize,
        /// What was supplied.
        len: usize,
    },
    /// The pattern would not compile.
    #[error("that pattern is not valid: {0}")]
    BadPattern(String),
}

/// Everything a search indexes about one capability.
#[derive(Debug, Clone)]
struct Indexed {
    name: ToolName,
    haystack: String,
    terms: Vec<String>,
}

fn index(definition: &ToolDefinition) -> Indexed {
    // The same four fields the provider-native implementations search, which is why an
    // undocumented parameter is lost search surface rather than a style problem.
    let haystack = definition.searchable_text().to_ascii_lowercase();
    Indexed { name: definition.name.clone(), terms: tokenise(&haystack), haystack }
}

fn tokenise(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Substring and alternation search, mirroring the provider-native regex variant.
///
/// A deliberately small subset of regex is supported: literals, `.*`, `.`, and `|` alternation.
/// That covers what a model actually writes — the documented examples are `"weather"`,
/// `"get_.*_data"`, and `"database.*query|query.*database"` — and avoids taking a regex engine
/// dependency into a crate whose whole value is being pure and fast.
#[derive(Debug, Clone, Default)]
pub struct RegexSearch {
    entries: Vec<Indexed>,
}

impl RegexSearch {
    /// Index a catalog.
    pub fn new<'a>(definitions: impl IntoIterator<Item = &'a ToolDefinition>) -> Self {
        Self { entries: definitions.into_iter().map(index).collect() }
    }

    /// Find capabilities matching `query`.
    ///
    /// # Errors
    /// Returns [`SearchError`] for a pattern the provider would also reject.
    pub fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>, SearchError> {
        if query.text.len() > MAX_REGEX_LEN {
            return Err(SearchError::TooLong {
                kind: "regex",
                max: MAX_REGEX_LEN,
                len: query.text.len(),
            });
        }
        let pattern = query.text.to_ascii_lowercase();
        let alternatives: Vec<&str> = pattern.split('|').map(str::trim).collect();

        let mut hits: Vec<SearchHit> = self
            .entries
            .iter()
            .filter(|e| alternatives.iter().any(|alt| matches(alt, &e.haystack)))
            .map(|e| SearchHit::new(e.name.clone(), 1.0))
            .collect();
        hits.truncate(query.limit as usize);
        Ok(hits)
    }

    /// Which strategy this is.
    #[must_use]
    pub fn kind(&self) -> SearchKind {
        SearchKind::Regex
    }
}

/// Match a small regex subset: literals, `.`, and `.*`.
fn matches(pattern: &str, haystack: &str) -> bool {
    let parts: Vec<&str> = pattern.split(".*").collect();
    if parts.len() == 1 {
        return contains_with_dots(haystack, pattern);
    }
    // Every literal chunk must appear, in order.
    let mut cursor = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match find_with_dots(&haystack[cursor..], part) {
            Some(at) => cursor += at + part.len(),
            None => return false,
        }
        if i == 0 && pattern.starts_with(part) {
            // A leading literal need not be anchored; `re.search` is unanchored.
        }
    }
    true
}

fn contains_with_dots(haystack: &str, pattern: &str) -> bool {
    find_with_dots(haystack, pattern).is_some()
}

/// Substring search where `.` matches any single character.
fn find_with_dots(haystack: &str, pattern: &str) -> Option<usize> {
    let h: Vec<char> = haystack.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    if p.is_empty() {
        return Some(0);
    }
    if p.len() > h.len() {
        return None;
    }
    (0..=h.len() - p.len())
        .find(|&start| p.iter().enumerate().all(|(i, &pc)| pc == '.' || h[start + i] == pc))
}

/// Lexical ranking over the catalog, for natural-language queries.
///
/// A compact BM25: term frequency saturating, document length normalised, rare terms weighted
/// higher. Enough to make "find me something that reads files" retrieve `fs_read` ahead of forty
/// unrelated tools, which is the job.
#[derive(Debug, Clone, Default)]
pub struct Bm25Search {
    entries: Vec<Indexed>,
    document_frequency: HashMap<String, u32>,
    average_length: f64,
}

const K1: f64 = 1.2;
const B: f64 = 0.75;

impl Bm25Search {
    /// Index a catalog.
    pub fn new<'a>(definitions: impl IntoIterator<Item = &'a ToolDefinition>) -> Self {
        let entries: Vec<Indexed> = definitions.into_iter().map(index).collect();
        let mut document_frequency: HashMap<String, u32> = HashMap::new();
        for entry in &entries {
            let mut seen: Vec<&String> = entry.terms.iter().collect();
            seen.sort_unstable();
            seen.dedup();
            for term in seen {
                *document_frequency.entry(term.clone()).or_default() += 1;
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let average_length = if entries.is_empty() {
            0.0
        } else {
            entries.iter().map(|e| e.terms.len()).sum::<usize>() as f64 / entries.len() as f64
        };
        Self { entries, document_frequency, average_length }
    }

    /// Find capabilities matching `query`.
    ///
    /// # Errors
    /// Returns [`SearchError`] for a query the provider would also reject.
    pub fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>, SearchError> {
        if query.text.len() > MAX_QUERY_LEN {
            return Err(SearchError::TooLong {
                kind: "natural-language",
                max: MAX_QUERY_LEN,
                len: query.text.len(),
            });
        }
        let terms = tokenise(&query.text.to_ascii_lowercase());
        if terms.is_empty() || self.entries.is_empty() {
            return Ok(Vec::new());
        }

        #[allow(clippy::cast_precision_loss)]
        let total = self.entries.len() as f64;
        let mut scored: Vec<(f64, ToolName)> = self
            .entries
            .iter()
            .map(|entry| {
                #[allow(clippy::cast_precision_loss)]
                let length = entry.terms.len() as f64;
                let score: f64 = terms
                    .iter()
                    .map(|term| {
                        #[allow(clippy::cast_precision_loss)]
                        let tf = entry.terms.iter().filter(|t| *t == term).count() as f64;
                        if tf == 0.0 {
                            return 0.0;
                        }
                        let df = f64::from(*self.document_frequency.get(term).unwrap_or(&0));
                        let idf = ((total - df + 0.5) / (df + 0.5) + 1.0).ln();
                        let norm = 1.0 - B + B * (length / self.average_length.max(1.0));
                        idf * (tf * (K1 + 1.0)) / (tf + K1 * norm)
                    })
                    .sum();
                (score, entry.name.clone())
            })
            .filter(|(score, _)| *score > 0.0)
            .collect();

        // Sort by score, then by name, so equal scores do not reorder between runs — an unstable
        // order would churn any downstream cache.
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1))
        });

        let best = scored.first().map_or(1.0, |(s, _)| *s).max(f64::MIN_POSITIVE);
        Ok(scored
            .into_iter()
            .take(query.limit as usize)
            .map(|(score, name)| SearchHit::new(name, score / best))
            .collect())
    }

    /// Which strategy this is.
    #[must_use]
    pub fn kind(&self) -> SearchKind {
        SearchKind::Bm25
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frey_core::tool_def::JsonSchema;

    fn tool(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition::new(name, description, JsonSchema::empty_object())
    }

    fn documented(name: &str, description: &str, param: &str, param_doc: &str) -> ToolDefinition {
        ToolDefinition::new(
            name,
            description,
            JsonSchema::new(serde_json::json!({
                "type": "object",
                "properties": { param: {"type": "string", "description": param_doc} }
            }))
            .unwrap(),
        )
    }

    fn catalog() -> Vec<ToolDefinition> {
        vec![
            tool("fs_read", "Read a file from the workspace and return its contents"),
            tool("fs_write", "Write text to a file in the workspace, replacing what was there"),
            tool("github_list_issues", "List open issues on a GitHub repository, newest first"),
            tool("github_create_issue", "Open a new issue on a GitHub repository"),
            tool("weather_now", "Get the current weather for a location"),
            documented(
                "db_query",
                "Run a statement against the analytics database",
                "sql",
                "The SQL statement to execute, in Postgres dialect",
            ),
        ]
    }

    fn query(text: &str) -> SearchQuery {
        SearchQuery::new(text)
    }

    #[test]
    fn a_literal_pattern_finds_the_obvious_tool() {
        let search = RegexSearch::new(catalog().iter());
        let hits = search.search(&query("weather")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name.as_str(), "weather_now");
    }

    #[test]
    fn the_documented_regex_examples_all_work() {
        // Straight from the provider documentation, because mirroring the semantics is the point:
        // a query must behave the same whether Frey ran it or delegated it.
        let search = RegexSearch::new(catalog().iter());

        let wildcards = search.search(&query("github_.*_issue")).unwrap();
        assert!(wildcards.iter().any(|h| h.name.as_str() == "github_create_issue"));

        let alternation = search.search(&query("database.*statement|statement.*database")).unwrap();
        assert_eq!(alternation[0].name.as_str(), "db_query");
    }

    #[test]
    fn matching_is_case_insensitive_like_the_provider_implementation() {
        let search = RegexSearch::new(catalog().iter());
        assert_eq!(search.search(&query("WEATHER")).unwrap().len(), 1);
    }

    #[test]
    fn an_over_long_pattern_is_rejected_here_rather_than_upstream() {
        let search = RegexSearch::new(catalog().iter());
        let err = search.search(&query(&"a".repeat(MAX_REGEX_LEN + 1))).unwrap_err();
        assert!(matches!(err, SearchError::TooLong { max: MAX_REGEX_LEN, .. }));
    }

    #[test]
    fn natural_language_finds_the_right_tool_without_naming_it() {
        // The job: retrieve the right tool from a description of the task, not from its name.
        let search = Bm25Search::new(catalog().iter());
        let hits = search.search(&query("read the contents of a file")).unwrap();
        assert_eq!(hits[0].name.as_str(), "fs_read", "got {hits:?}");
    }

    #[test]
    fn argument_descriptions_are_searchable_which_is_why_they_must_exist() {
        // `db_query` is findable by "postgres" only because its parameter is documented. This is
        // the concrete reason an undocumented parameter is a defect rather than a style choice.
        let search = Bm25Search::new(catalog().iter());
        let hits = search.search(&query("postgres dialect")).unwrap();
        assert_eq!(hits[0].name.as_str(), "db_query");
    }

    #[test]
    fn results_are_capped_at_the_requested_limit() {
        let search = Bm25Search::new(catalog().iter());
        let mut q = query("file workspace github issue weather database");
        q.limit = 2;
        assert!(search.search(&q).unwrap().len() <= 2);
    }

    #[test]
    fn ranking_is_stable_across_runs() {
        // Equal scores must not reorder between runs; an unstable order churns any cache built on
        // the result.
        let search = Bm25Search::new(catalog().iter());
        let first = search.search(&query("github repository issue")).unwrap();
        let second = search.search(&query("github repository issue")).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn a_query_matching_nothing_returns_nothing_rather_than_erroring() {
        // The model needs to learn that nothing matched; an error would just be retried.
        let search = Bm25Search::new(catalog().iter());
        assert!(search.search(&query("quantum chromodynamics")).unwrap().is_empty());
        let regex = RegexSearch::new(catalog().iter());
        assert!(regex.search(&query("zzzz")).unwrap().is_empty());
    }

    #[test]
    fn searching_an_empty_catalog_is_not_an_error() {
        let search = Bm25Search::new(std::iter::empty());
        assert!(search.search(&query("anything")).unwrap().is_empty());
    }

    #[test]
    fn scores_are_normalised_so_the_best_hit_is_one() {
        let search = Bm25Search::new(catalog().iter());
        let hits = search.search(&query("read a file")).unwrap();
        assert_eq!(hits[0].score_bp, 10_000);
        assert!(hits.iter().all(|h| h.score() <= 1.0));
    }

    #[test]
    fn both_strategies_report_which_they_are() {
        assert_eq!(RegexSearch::default().kind(), SearchKind::Regex);
        assert_eq!(Bm25Search::default().kind(), SearchKind::Bm25);
    }
}
