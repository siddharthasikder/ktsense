//! The `grep` answer: regex text hits grouped by file and by the declaration that encloses each,
//! stated as a text match so it is never read as a resolved reference (KT-102).
//!
//! `grep` is a text search, not a resolution: a hit is a line that matched a pattern, wherever it
//! sits. The value an agent cannot get from raw `rg` is the attribution, so each hit carries the
//! fully qualified name of the declaration it falls inside, whether its file is production or test
//! (the KT-91 source-set rule), and whether the match is code, a comment or a string (the KT-83
//! classification). All three come from the pure crate: the adapter scans the files and hands over
//! already classified hits, and this module groups, caps and counts them without a parser or a
//! filesystem.
//!
//! The comment and string share is counted apart from code for the same reason it is in a trace:
//! a mention in prose is evidence of a different weight than a use in code, and a reader is never
//! told a comment is a use.

use crate::references::{fully_qualified_enclosing, is_test_source, SiteKind};
use crate::skeleton::FileSkeleton;
use crate::text_refs::TEXT_MATCH_PRECISION;
use serde::Serialize;
use std::collections::BTreeMap;

/// One classified hit the adapter found: the file path (workspace-relative, `/`-separated), the
/// 1-based line, the whole source line as read, and the syntax node the match falls in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSearchHit {
    pub path: String,
    pub line: u32,
    pub source_line: String,
    pub kind: SiteKind,
}

/// One matched line kept in the answer: its 1-based line, the node kind, and the source text the
/// renderer trims. The kind is serialized only when it is not code, matching the trace sites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextSearchLine {
    pub line: u32,
    #[serde(skip_serializing_if = "SiteKind::is_code")]
    pub kind: SiteKind,
    pub source_line: String,
}

/// The hits in one file that share an enclosing declaration, under that declaration's FQN. `fqn`
/// is absent for a hit in the file header, above the first declaration (an import or a package
/// line), which belongs to no declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextSearchDeclaration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fqn: Option<String>,
    pub hits: Vec<TextSearchLine>,
}

/// Every hit in one file, grouped by enclosing declaration in source order, with the file's
/// production-or-test label and the count the per-file cap dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextSearchFile {
    pub path: String,
    pub test: bool,
    pub declarations: Vec<TextSearchDeclaration>,
    /// How many hits the per-file cap dropped from this file, the `N` behind `... N more`.
    pub omitted: usize,
}

/// The whole `grep` answer: the pattern, the text-match precision, the totals before any cap, and
/// the files in path order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextSearch {
    pub pattern: String,
    /// Always [`TEXT_MATCH_PRECISION`]: these are text matches, not resolved references.
    pub precision: &'static str,
    /// Every hit found, before the per-file cap.
    pub total_hits: usize,
    /// How many files carry at least one hit.
    pub file_count: usize,
    /// How many of `total_hits` fall in a comment, KDoc or string rather than in code (KT-83).
    pub text_mention_hits: usize,
    pub files: Vec<TextSearchFile>,
}

/// Groups classified hits by file (path order), orders each file's hits by line, caps each file at
/// `limit`, and groups the surviving hits by the declaration enclosing each. The totals are taken
/// over every hit before the cap, so the heading states what exists and each file says what it hid.
///
/// `skeletons` names the enclosing declaration; a file with no skeleton (one that did not parse)
/// still lists its hits, grouped under the file header, because a text match does not depend on the
/// file parsing.
pub fn build_text_search(
    pattern: &str,
    hits: &[TextSearchHit],
    skeletons: &[FileSkeleton],
    limit: Option<usize>,
) -> TextSearch {
    let total_hits = hits.len();
    let text_mention_hits = hits.iter().filter(|hit| hit.kind.is_text_mention()).count();

    let skeleton_by_path: BTreeMap<&str, &FileSkeleton> = skeletons
        .iter()
        .map(|skeleton| (skeleton.path.as_str(), skeleton))
        .collect();

    let mut by_path: BTreeMap<&str, Vec<&TextSearchHit>> = BTreeMap::new();
    for hit in hits {
        by_path.entry(hit.path.as_str()).or_default().push(hit);
    }

    let files: Vec<TextSearchFile> = by_path
        .into_iter()
        .map(|(path, mut file_hits)| {
            file_hits.sort_by_key(|hit| hit.line);
            let omitted = match limit {
                Some(limit) if file_hits.len() > limit => {
                    let dropped = file_hits.len() - limit;
                    file_hits.truncate(limit);
                    dropped
                }
                _ => 0,
            };
            TextSearchFile {
                path: path.to_string(),
                test: is_test_source(path),
                declarations: group_by_declaration(&file_hits, skeleton_by_path.get(path).copied()),
                omitted,
            }
        })
        .collect();

    TextSearch {
        pattern: pattern.to_string(),
        precision: TEXT_MATCH_PRECISION,
        total_hits,
        file_count: files.len(),
        text_mention_hits,
        files,
    }
}

/// Collapses one file's line-ordered hits onto the declarations enclosing them, keeping source
/// order: consecutive hits with the same enclosing FQN share a group, and a change of FQN opens the
/// next. Hits are already sorted by line and a declaration spans a contiguous line range, so hits
/// for one declaration are always consecutive.
fn group_by_declaration(
    hits: &[&TextSearchHit],
    skeleton: Option<&FileSkeleton>,
) -> Vec<TextSearchDeclaration> {
    let mut groups: Vec<TextSearchDeclaration> = Vec::new();
    for hit in hits {
        let fqn = skeleton
            .and_then(|skeleton| fully_qualified_enclosing(skeleton, hit.line))
            .map(|enclosing| enclosing.fqn);
        let line = TextSearchLine {
            line: hit.line,
            kind: hit.kind,
            source_line: hit.source_line.clone(),
        };
        match groups.last_mut() {
            Some(group) if group.fqn == fqn => group.hits.push(line),
            _ => groups.push(TextSearchDeclaration {
                fqn,
                hits: vec![line],
            }),
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton::Declaration;

    fn hit(path: &str, line: u32, source: &str, kind: SiteKind) -> TextSearchHit {
        TextSearchHit {
            path: path.to_string(),
            line,
            source_line: source.to_string(),
            kind,
        }
    }

    /// Everything the grouping must get right at once, over one file with a package and a nested
    /// declaration and one test file: files ordered by path, hits ordered by line and grouped by
    /// their enclosing FQN in source order, the production and test labels from the path rule, a
    /// per-file cap that reports what it dropped, and the comment share counted apart from code over
    /// every hit before the cap. Asserted as one composed value.
    #[test]
    fn groups_by_file_then_enclosing_declaration_caps_per_file_and_counts_mentions_apart() {
        let repository = FileSkeleton::new("core/src/main/kotlin/shop/order/Repo.kt")
            .in_package("shop.order")
            .with_declarations(vec![Declaration::class("OrderRepository", 3).containing(
                vec![
                    Declaration::function("save", 4),
                    Declaration::function("cancel", 9),
                ],
            )]);
        let hits = vec![
            hit(
                "app/src/test/kotlin/shop/app/RepoTest.kt",
                7,
                "repo.save(order)",
                SiteKind::Code,
            ),
            hit(
                "core/src/main/kotlin/shop/order/Repo.kt",
                5,
                "// save it",
                SiteKind::Comment,
            ),
            hit(
                "core/src/main/kotlin/shop/order/Repo.kt",
                4,
                "fun save()",
                SiteKind::Code,
            ),
            hit(
                "core/src/main/kotlin/shop/order/Repo.kt",
                10,
                "db.save()",
                SiteKind::Code,
            ),
        ];

        let search = build_text_search("save", &hits, &[repository], Some(2));

        assert_eq!(
            search,
            TextSearch {
                pattern: "save".to_string(),
                precision: "text match",
                total_hits: 4,
                file_count: 2,
                text_mention_hits: 1,
                files: vec![
                    TextSearchFile {
                        path: "app/src/test/kotlin/shop/app/RepoTest.kt".to_string(),
                        test: true,
                        declarations: vec![TextSearchDeclaration {
                            fqn: None,
                            hits: vec![TextSearchLine {
                                line: 7,
                                kind: SiteKind::Code,
                                source_line: "repo.save(order)".to_string(),
                            }],
                        }],
                        omitted: 0,
                    },
                    TextSearchFile {
                        path: "core/src/main/kotlin/shop/order/Repo.kt".to_string(),
                        test: false,
                        declarations: vec![TextSearchDeclaration {
                            fqn: Some("shop.order.OrderRepository.save".to_string()),
                            hits: vec![
                                TextSearchLine {
                                    line: 4,
                                    kind: SiteKind::Code,
                                    source_line: "fun save()".to_string(),
                                },
                                TextSearchLine {
                                    line: 5,
                                    kind: SiteKind::Comment,
                                    source_line: "// save it".to_string(),
                                },
                            ],
                        }],
                        omitted: 1,
                    },
                ],
            }
        );
    }
}
