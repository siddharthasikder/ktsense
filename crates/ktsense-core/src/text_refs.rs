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

use crate::references::{fully_qualified_enclosing, Location, SiteKind};
use crate::skeleton::FileSkeleton;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The precision every text-reference answer carries, so it cannot be read as a resolved usage list.
pub const TEXT_MATCH_PRECISION: &str = "text match";

/// The precision as the default when deserializing, since it is a fixed constant rather than data a
/// consumer round-trips.
fn text_match_precision() -> &'static str {
    TEXT_MATCH_PRECISION
}

/// One place a name appears as text: a 1-based line and the syntax node the match falls in. Code and
/// prose are both listed; the kind is what lets a reader tell a use from a mention. `enclosing` names
/// the declaration the site sits inside when the scan attributed it (KT-112's Kotlin text references
/// reuse the KT-102 attribution; KT-114's Java text references use the pure Java enclosing scan); it
/// is absent for a scan that does not attribute, such as the KT-94 undeclared-name listing. `text`
/// is the trimmed source line a Java site renders beside its line number (KT-114), so the listing
/// reads like a grep hit; it is absent for the line-only listings, which serialize exactly as before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextReferenceSite {
    pub line: u32,
    #[serde(skip_serializing_if = "SiteKind::is_code")]
    pub kind: SiteKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enclosing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Every text match in one file, ordered by line and capped, with the count the cap dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextReferenceGroup {
    pub path: String,
    pub sites: Vec<TextReferenceSite>,
    /// How many sites the per-file cap dropped from this group, the `N` behind `... N more`.
    pub omitted: usize,
}

/// Where a name the workspace does not declare appears as text: the total, the comment or string
/// share counted apart, and the sites grouped by file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextReferences {
    pub symbol: String,
    /// Always [`TEXT_MATCH_PRECISION`]: these are text matches, not resolved references.
    #[serde(skip_deserializing, default = "text_match_precision")]
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
/// Sites carry no enclosing declaration or source text; the KT-94 undeclared-name listing uses this.
pub fn build_text_references(
    symbol: &str,
    sites: &[Location],
    limit: Option<usize>,
) -> TextReferences {
    build_grouped(symbol, sites, limit, |_, _| (None, None))
}

/// Like [`build_text_references`], but attributes each site to the declaration enclosing it, so a
/// Kotlin hit against a Java-declared symbol is listed with its enclosing FQN (KT-112), reusing the
/// KT-102 attribution [`fully_qualified_enclosing`]. A file with no skeleton (one that did not
/// parse) still lists its sites, unattributed, because a text match does not depend on the file
/// parsing.
pub fn build_text_references_attributed(
    symbol: &str,
    sites: &[Location],
    skeletons: &[FileSkeleton],
    limit: Option<usize>,
) -> TextReferences {
    let skeleton_by_path: BTreeMap<&str, &FileSkeleton> = skeletons
        .iter()
        .map(|skeleton| (skeleton.path.as_str(), skeleton))
        .collect();
    build_grouped(symbol, sites, limit, |path, line| {
        let enclosing = skeleton_by_path
            .get(path)
            .and_then(|skeleton| fully_qualified_enclosing(skeleton, line))
            .map(|enclosing| enclosing.fqn);
        (enclosing, None)
    })
}

/// Like [`build_text_references`], but `attribute` supplies both the declaration enclosing each site
/// and the trimmed source line to render beside it, so a Java text reference reads like a KT-102 grep
/// hit (KT-114). The enclosing comes from the pure Java enclosing scan the adapter runs per file; the
/// text is the file's source line. Returning `(None, None)` leaves a site a bare line, so the
/// adapter chooses per site.
pub fn build_text_references_with_text(
    symbol: &str,
    sites: &[Location],
    limit: Option<usize>,
    attribute: impl Fn(&str, u32) -> (Option<String>, Option<String>),
) -> TextReferences {
    build_grouped(symbol, sites, limit, attribute)
}

/// The shared body of the builders: group by file, order and cap each file, count mentions apart
/// from code, and attribute each surviving site through `attribute`, which returns the enclosing
/// declaration and the source text, each `None` for a listing that does not carry it.
fn build_grouped(
    symbol: &str,
    sites: &[Location],
    limit: Option<usize>,
    attribute: impl Fn(&str, u32) -> (Option<String>, Option<String>),
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
                    .map(|site| {
                        let (enclosing, text) = attribute(path, site.line);
                        TextReferenceSite {
                            line: site.line,
                            kind: site.kind,
                            enclosing,
                            text,
                        }
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
                                enclosing: None,
                                text: None,
                            },
                            TextReferenceSite {
                                line: 5,
                                kind: SiteKind::Code,
                                enclosing: None,
                                text: None,
                            },
                        ],
                        omitted: 1,
                    },
                    TextReferenceGroup {
                        path: "b/B.kt".to_string(),
                        sites: vec![TextReferenceSite {
                            line: 7,
                            kind: SiteKind::String,
                            enclosing: None,
                            text: None,
                        }],
                        omitted: 0,
                    },
                ],
            }
        );
    }

    /// The attributed builder names each site's enclosing declaration through the KT-102 attribution
    /// (KT-112): a hit inside `OrderRepository.save` carries that FQN, a hit in a file with no
    /// skeleton stays unattributed, and the mention counting is unchanged. Checked as one composed
    /// value.
    #[test]
    fn the_attributed_builder_names_each_sites_enclosing_declaration() {
        use crate::skeleton::Declaration;

        let repository = FileSkeleton::new("core/Repo.kt")
            .in_package("shop.order")
            .with_declarations(vec![Declaration::class("OrderRepository", 3)
                .containing(vec![Declaration::function("save", 5)])]);
        let sites = vec![
            Location::new("core/Repo.kt", 6),
            Location::new("app/Main.kt", 2),
        ];

        let refs = build_text_references_attributed("save", &sites, &[repository], None);

        assert_eq!(
            refs,
            TextReferences {
                symbol: "save".to_string(),
                precision: "text match",
                total_sites: 2,
                file_count: 2,
                text_mention_sites: 0,
                groups: vec![
                    TextReferenceGroup {
                        path: "app/Main.kt".to_string(),
                        sites: vec![TextReferenceSite {
                            line: 2,
                            kind: SiteKind::Code,
                            enclosing: None,
                            text: None,
                        }],
                        omitted: 0,
                    },
                    TextReferenceGroup {
                        path: "core/Repo.kt".to_string(),
                        sites: vec![TextReferenceSite {
                            line: 6,
                            kind: SiteKind::Code,
                            enclosing: Some("shop.order.OrderRepository.save".to_string()),
                            text: None,
                        }],
                        omitted: 0,
                    },
                ],
            }
        );
    }

    /// The with-text builder (KT-114) carries both the enclosing declaration and the trimmed source
    /// line the closure supplies, so a Java site renders like a grep hit, while the mention counting
    /// is unchanged. Checked as one composed value over a code hit and a comment hit.
    #[test]
    fn the_with_text_builder_carries_enclosing_and_source_text_per_site() {
        let sites = vec![
            Location::new("j/Dao.java", 12),
            Location::new("j/Dao.java", 20).with_kind(SiteKind::Comment),
        ];
        let text = |_: &str, line: u32| match line {
            12 => (
                Some("Dao.save".to_string()),
                Some("db.save(row);".to_string()),
            ),
            _ => (
                Some("Dao.save".to_string()),
                Some("// save the row".to_string()),
            ),
        };

        let refs = build_text_references_with_text("save", &sites, None, text);

        assert_eq!(
            refs,
            TextReferences {
                symbol: "save".to_string(),
                precision: "text match",
                total_sites: 2,
                file_count: 1,
                text_mention_sites: 1,
                groups: vec![TextReferenceGroup {
                    path: "j/Dao.java".to_string(),
                    sites: vec![
                        TextReferenceSite {
                            line: 12,
                            kind: SiteKind::Code,
                            enclosing: Some("Dao.save".to_string()),
                            text: Some("db.save(row);".to_string()),
                        },
                        TextReferenceSite {
                            line: 20,
                            kind: SiteKind::Comment,
                            enclosing: Some("Dao.save".to_string()),
                            text: Some("// save the row".to_string()),
                        },
                    ],
                    omitted: 0,
                }],
            }
        );
    }
}
