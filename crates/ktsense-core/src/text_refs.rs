//! The evidence left when a name resolves to no declaration: every place it appears as text in the
//! workspace's Kotlin sources, grouped by file and capped, kept apart from a resolved trace so it
//! can never be read as one (KT-94).
//!
//! A `trace` or `context` whose name the workspace does not declare has no usages to report, but a
//! whole-word text scan still finds where the name is written: a library method such as `putMetric`,
//! an annotation declared in a dependency, a generated class. Those sites are the only evidence
//! there is, so they are listed rather than swallowed by a bare exit 1.
//!
//! Like the rest of `core` this is pure. The sites arrive already classified by the syntax node they
//! fall in (KT-83): the adapter scans the files and hands over [`Location`] values, and this module
//! only groups, caps and counts them. Comment, KDoc and string mentions are kept, because prose is
//! the only evidence there is when nothing declares the name, but counted apart from code text so a
//! reader is never misled about what the match is.

use crate::references::{Location, SiteKind};
use serde::Serialize;
use std::collections::BTreeMap;

/// The precision every text-reference answer carries, so it cannot be read as a resolved usage list.
pub const TEXT_MATCH_PRECISION: &str = "text match";

/// One place a name appears as text: a 1-based line and the syntax node the match falls in. Code and
/// prose are both listed; the kind is what lets a reader tell a use from a mention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextReferenceSite {
    pub line: u32,
    #[serde(skip_serializing_if = "SiteKind::is_code")]
    pub kind: SiteKind,
}

/// Every text match in one file, ordered by line and capped, with the count the cap dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextReferenceGroup {
    pub path: String,
    pub sites: Vec<TextReferenceSite>,
    /// How many sites the per-file cap dropped from this group, the `N` behind `... N more`.
    pub omitted: usize,
}

/// Where a name the workspace does not declare appears as text: the total, the comment or string
/// share counted apart, and the sites grouped by file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextReferences {
    pub symbol: String,
    /// Always [`TEXT_MATCH_PRECISION`]: these are text matches, not resolved references.
    pub precision: &'static str,
    /// Every site found, before the per-file cap.
    pub total_sites: usize,
    /// How many files carry at least one site.
    pub file_count: usize,
    /// How many of `total_sites` fall in a comment, KDoc or string rather than in code (KT-83).
    pub text_mention_sites: usize,
    pub groups: Vec<TextReferenceGroup>,
}

/// Groups classified text-match sites by file, orders each file's sites by line, caps each file at
/// `limit`, and counts the comment or string share apart from code. The counts are taken over every
/// site found, before the cap, so the heading states what exists and each group says what it hid.
pub fn build_text_references(
    symbol: &str,
    sites: &[Location],
    limit: Option<usize>,
) -> TextReferences {
    let total_sites = sites.len();
    let text_mention_sites = sites
        .iter()
        .filter(|site| site.kind.is_text_mention())
        .count();

    let mut by_path: BTreeMap<&str, Vec<&Location>> = BTreeMap::new();
    for site in sites {
        by_path.entry(site.path.as_str()).or_default().push(site);
    }

    let groups: Vec<TextReferenceGroup> = by_path
        .into_iter()
        .map(|(path, mut file_sites)| {
            file_sites.sort_by_key(|site| (site.line, site.column));
            let omitted = match limit {
                Some(limit) if file_sites.len() > limit => {
                    let dropped = file_sites.len() - limit;
                    file_sites.truncate(limit);
                    dropped
                }
                _ => 0,
            };
            TextReferenceGroup {
                path: path.to_string(),
                sites: file_sites
                    .into_iter()
                    .map(|site| TextReferenceSite {
                        line: site.line,
                        kind: site.kind,
                    })
                    .collect(),
                omitted,
            }
        })
        .collect();

    TextReferences {
        symbol: symbol.to_string(),
        precision: TEXT_MATCH_PRECISION,
        total_sites,
        file_count: groups.len(),
        text_mention_sites,
        groups,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three things the model must get right at once: sites grouped by file and ordered by line,
    /// a per-file cap that reports what it dropped, and comment or string mentions counted apart from
    /// code over every site before the cap. Checked as one composed value over a corpus carrying all
    /// of them.
    #[test]
    fn groups_by_file_caps_per_file_and_counts_mentions_apart_from_code() {
        let sites = vec![
            Location::new("a/A.kt", 10).with_kind(SiteKind::Comment),
            Location::new("a/A.kt", 3),
            Location::new("a/A.kt", 5),
            Location::new("b/B.kt", 7).with_kind(SiteKind::String),
        ];

        let refs = build_text_references("putMetric", &sites, Some(2));

        assert_eq!(
            refs,
            TextReferences {
                symbol: "putMetric".to_string(),
                precision: "text match",
                total_sites: 4,
                file_count: 2,
                text_mention_sites: 2,
                groups: vec![
                    TextReferenceGroup {
                        path: "a/A.kt".to_string(),
                        sites: vec![
                            TextReferenceSite {
                                line: 3,
                                kind: SiteKind::Code,
                            },
                            TextReferenceSite {
                                line: 5,
                                kind: SiteKind::Code,
                            },
                        ],
                        omitted: 1,
                    },
                    TextReferenceGroup {
                        path: "b/B.kt".to_string(),
                        sites: vec![TextReferenceSite {
                            line: 7,
                            kind: SiteKind::String,
                        }],
                        omitted: 0,
                    },
                ],
            }
        );
    }
}
