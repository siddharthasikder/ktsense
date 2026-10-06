//! The path and test-source filter `trace` and `context` apply to their callers, usages, text
//! references and annotated sites (KT-127).
//!
//! The filter is the one `grep` already takes (KT-102, KT-122): a repeatable workspace-relative
//! path prefix matched with `starts_with`, OR-combined, and the KT-91 `--tests`/`--no-tests` split.
//! It never touches the definition or the implementors; it narrows the site lists a reader scans and
//! the budget a `context` spends. Like the rest of `core` it is pure over the path string, so one
//! rule decides what a filtered answer keeps whether the trace was answered in process or on a warm
//! daemon.

use serde::{Deserialize, Serialize};

use crate::references::is_test_source;

/// Which source sets a filtered answer covers. The KT-91 split, mirroring `grep --tests`/`--no-tests`
/// so a filtered `trace` and a `grep` scope the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestScope {
    All,
    Tests,
    Production,
}

impl TestScope {
    /// Reads the two mutually exclusive CLI flags into one scope. clap refuses both at once, so a
    /// true-true pair cannot reach here.
    pub fn from_flags(tests: bool, no_tests: bool) -> Self {
        match (tests, no_tests) {
            (true, _) => TestScope::Tests,
            (_, true) => TestScope::Production,
            _ => TestScope::All,
        }
    }

    /// Whether every source set is admitted, the default and the condition under which the scope is
    /// left out of the serialized filter.
    pub fn is_all(&self) -> bool {
        matches!(self, TestScope::All)
    }

    fn admits(self, is_test: bool) -> bool {
        match self {
            TestScope::All => true,
            TestScope::Tests => is_test,
            TestScope::Production => !is_test,
        }
    }
}

/// A path-and-test filter applied to the site lists of a `trace` or `context`. `path` is a set of
/// workspace-relative prefixes OR-combined with `starts_with`, empty when only a test scope was
/// asked for; `tests` is the source-set scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteFilter {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    #[serde(default = "all_scope", skip_serializing_if = "TestScope::is_all")]
    pub tests: TestScope,
}

fn all_scope() -> TestScope {
    TestScope::All
}

impl SiteFilter {
    /// A filter from the CLI flags, or `None` when neither a path prefix nor a test scope was given,
    /// which is the state under which output stays byte-identical to an unfiltered answer.
    pub fn new(path: Vec<String>, tests: TestScope) -> Option<Self> {
        (!path.is_empty() || !tests.is_all()).then_some(Self { path, tests })
    }

    /// Whether a site at `path`, whose test-ness is `is_test`, survives the filter: its path starts
    /// with one of the prefixes (or no prefix was given) and its source set is admitted.
    pub fn matches(&self, path: &str, is_test: bool) -> bool {
        self.matches_path_prefix(path) && self.tests.admits(is_test)
    }

    /// Whether a site at `path` survives, deriving its test-ness from the path by the KT-91 rule.
    /// Used where only a path is in hand, such as a usage group or a text-reference file.
    pub fn matches_path(&self, path: &str) -> bool {
        self.matches(path, is_test_source(path))
    }

    fn matches_path_prefix(&self, path: &str) -> bool {
        self.path.is_empty()
            || self
                .path
                .iter()
                .any(|prefix| path.starts_with(prefix.as_str()))
    }

    /// The filter as a heading phrase, so a count reads `3 under src/main; 31 others`: the path
    /// prefixes as `under a, b`, the test scope as `in tests` or `in production`, joined when both
    /// are present. Never empty, because a filter exists only when at least one half was asked for.
    pub fn describe(&self) -> String {
        let path = (!self.path.is_empty()).then(|| format!("under {}", self.path.join(", ")));
        let tests = match self.tests {
            TestScope::All => None,
            TestScope::Tests => Some("in tests".to_string()),
            TestScope::Production => Some("in production".to_string()),
        };
        match (path, tests) {
            (Some(path), Some(tests)) => format!("{path} {tests}"),
            (Some(path), None) => path,
            (None, Some(tests)) => tests,
            (None, None) => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_none_only_when_no_half_was_asked_for() {
        let observed = (
            SiteFilter::new(Vec::new(), TestScope::All).is_none(),
            SiteFilter::new(vec!["app/".to_string()], TestScope::All).is_some(),
            SiteFilter::new(Vec::new(), TestScope::Tests).is_some(),
            SiteFilter::new(vec!["app/".to_string()], TestScope::Production).is_some(),
        );

        assert_eq!(observed, (true, true, true, true));
    }

    /// A path prefix is OR-combined and matched with `starts_with`, and the test scope admits the
    /// matching source set, both halves ANDed. Checked as one table over a production path, a test
    /// path, and a path outside every prefix, under each kind of filter.
    #[test]
    fn matches_ors_the_prefixes_and_ands_the_test_scope() {
        let prod = "app/src/main/kotlin/A.kt";
        let test = "app/src/test/kotlin/ATest.kt";
        let elsewhere = "lib/src/main/kotlin/B.kt";
        let path_only = SiteFilter::new(vec!["app/".to_string()], TestScope::All).unwrap();
        let path_or =
            SiteFilter::new(vec!["app/".to_string(), "lib/".to_string()], TestScope::All).unwrap();
        let no_tests = SiteFilter::new(vec!["app/".to_string()], TestScope::Production).unwrap();
        let only_tests = SiteFilter::new(Vec::new(), TestScope::Tests).unwrap();

        let observed = (
            path_only.matches_path(prod),
            path_only.matches_path(elsewhere),
            path_or.matches_path(elsewhere),
            no_tests.matches_path(test),
            no_tests.matches_path(prod),
            only_tests.matches_path(test),
            only_tests.matches_path(prod),
        );

        assert_eq!(observed, (true, false, true, false, true, true, false));
    }

    #[test]
    fn describe_names_the_path_the_scope_or_both() {
        let path = SiteFilter::new(vec!["src/main".to_string()], TestScope::All).unwrap();
        let paths =
            SiteFilter::new(vec!["app/".to_string(), "lib/".to_string()], TestScope::All).unwrap();
        let tests = SiteFilter::new(Vec::new(), TestScope::Tests).unwrap();
        let production = SiteFilter::new(Vec::new(), TestScope::Production).unwrap();
        let both = SiteFilter::new(vec!["app/".to_string()], TestScope::Tests).unwrap();

        let observed = (
            path.describe(),
            paths.describe(),
            tests.describe(),
            production.describe(),
            both.describe(),
        );

        assert_eq!(
            observed,
            (
                "under src/main".to_string(),
                "under app/, lib/".to_string(),
                "in tests".to_string(),
                "in production".to_string(),
                "under app/ in tests".to_string(),
            )
        );
    }
}
