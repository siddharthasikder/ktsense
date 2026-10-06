//! Ranking a partial-name declaration search.
//!
//! `symbols --contains <query>` lists every declaration whose simple name contains the query, which
//! `find`'s exact-name lookup cannot answer. The filtering and ordering are the interesting part and
//! they are pure string work over hand-built values, so they live here rather than in the adapter
//! that gathers candidates from the syntax tree. Ordering is total and deterministic (KT-81): an
//! exact match ranks above a prefix match, a prefix match above any other containment, a shorter
//! name above a longer one, and remaining ties break on qualified name, then path, then line.

use crate::skeleton::DeclKind;

/// One declaration a partial-name search may return, reduced to what ranking and rendering need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolMatch {
    pub qualified_name: String,
    pub simple_name: String,
    pub kind: DeclKind,
    pub path: String,
    pub line: u32,
    pub signature: String,
}

/// Keeps the declarations whose simple name contains `query` (case-sensitive) and orders them so the
/// most likely intended match reads first. The order is exact match, then prefix match, then shorter
/// simple name, then qualified name, path and line, which is total, so the list is identical on every
/// run regardless of the order candidates were gathered in.
pub fn contained_declarations(query: &str, mut candidates: Vec<SymbolMatch>) -> Vec<SymbolMatch> {
    candidates.retain(|candidate| candidate.simple_name.contains(query));
    candidates.sort_by(|left, right| match_rank(query, left).cmp(&match_rank(query, right)));
    candidates
}

/// Where a match sits inside a query's containment: `0` when the simple name is the query exactly,
/// `1` when it begins with the query, `2` for any other containment. Named so the ordering reads as
/// intent rather than as magic numbers.
fn containment_tier(query: &str, simple_name: &str) -> u8 {
    if simple_name == query {
        0
    } else if simple_name.starts_with(query) {
        1
    } else {
        2
    }
}

fn match_rank<'a>(query: &str, candidate: &'a SymbolMatch) -> (u8, usize, &'a str, &'a str, u32) {
    (
        containment_tier(query, &candidate.simple_name),
        candidate.simple_name.len(),
        candidate.qualified_name.as_str(),
        candidate.path.as_str(),
        candidate.line,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(simple: &str, qualified: &str, path: &str, line: u32) -> SymbolMatch {
        SymbolMatch {
            qualified_name: qualified.to_string(),
            simple_name: simple.to_string(),
            kind: DeclKind::Class,
            path: path.to_string(),
            line,
            signature: format!("class {simple}"),
        }
    }

    #[test]
    fn containment_is_filtered_and_ordered_exact_then_prefix_then_length() {
        let candidates = vec![
            candidate("OrderRepository", "x.OrderRepository", "x.kt", 1),
            candidate("Widget", "w.Widget", "w.kt", 1),
            candidate("Repository", "a.Repository", "a.kt", 1),
            candidate(
                "InMemoryOrderRepository",
                "d.InMemoryOrderRepository",
                "d.kt",
                1,
            ),
            candidate("RepositoryFactory", "z.RepositoryFactory", "z.kt", 1),
            candidate("JdbcOrderRepository", "d.JdbcOrderRepository", "d.kt", 1),
        ];

        let ordered: Vec<String> = contained_declarations("Repository", candidates)
            .into_iter()
            .map(|found| found.qualified_name)
            .collect();

        assert_eq!(
            ordered,
            vec![
                "a.Repository".to_string(),
                "z.RepositoryFactory".to_string(),
                "x.OrderRepository".to_string(),
                "d.JdbcOrderRepository".to_string(),
                "d.InMemoryOrderRepository".to_string(),
            ]
        );
    }

    #[test]
    fn an_equal_rank_breaks_on_qualified_name_then_path_then_line() {
        let candidates = vec![
            candidate("FooHandler", "b.FooHandler", "b.kt", 9),
            candidate("BarHandler", "a.BarHandler", "a.kt", 3),
            candidate("BarHandler", "a.BarHandler", "a.kt", 1),
        ];

        let ordered: Vec<(String, u32)> = contained_declarations("Handler", candidates)
            .into_iter()
            .map(|found| (found.qualified_name, found.line))
            .collect();

        assert_eq!(
            ordered,
            vec![
                ("a.BarHandler".to_string(), 1),
                ("a.BarHandler".to_string(), 3),
                ("b.FooHandler".to_string(), 9),
            ]
        );
    }
}
