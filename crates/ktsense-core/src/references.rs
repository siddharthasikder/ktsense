//! Compresses a symbol's reference sites into a dense, agent-readable answer.
//!
//! `kmp-lsp` 0.26.0 has no call-hierarchy, so the callers a `trace` reports are derived here:
//! every LSP reference is grouped by file, tagged with the declaration that encloses it, and the
//! list is capped. This module is the only source of caller data in the product.
//!
//! Like the rest of `core` this is pure. A [`Location`] is the crate's own reference site, a file
//! path and a 1-based line; the `ktsense-lsp` adapter converts `lsp_types` into it at the boundary
//! so nothing here depends on `lsp-types`, a parser, or the filesystem.
//!
//! The rules, stated once so they can be argued with:
//!
//! 1. References are grouped by file, groups ordered by path, references ordered by line so the
//!    output is deterministic for golden tests.
//! 2. Each reference is tagged with its enclosing declaration: the innermost declaration in the
//!    file skeleton whose span contains the line. A declaration records only a start line, so the
//!    span end is recovered from the next sibling's start (the last sibling inherits its parent's
//!    bound, the last top-level declaration extends to end of file). This assumes siblings are
//!    stored in ascending source-line order, which the `ktsense-syntax` extractor guarantees by
//!    walking the tree in source order; a violation is caught by a debug assertion rather than
//!    silently mislabelling an enclosure. This is exact for every reference site, which is always a
//!    symbol occurrence inside a declaration; only bare closing-brace or blank lines, which
//!    references never occupy, would be mislabelled.
//! 3. References in the file header - below the first declaration, where Kotlin requires imports to
//!    sit - are noise for a caller question and are dropped by default.
//! 4. Each group is capped at `limit`; the omitted count travels as data so the renderer can print
//!    `... N more`. Capping per group keeps one hot file from drowning out the others.

use crate::skeleton::{DeclKind, Declaration, FileSkeleton};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single reference site: a file path and a 1-based line, optionally a column.
///
/// The column is carried for a future precise renderer; enclosure and grouping are line-based, so
/// it never changes which declaration a reference is attributed to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub path: String,
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

impl Location {
    pub fn new(path: impl Into<String>, line: u32) -> Self {
        Self {
            path: path.into(),
            line,
            column: None,
        }
    }

    pub fn at_column(mut self, column: u32) -> Self {
        self.column = Some(column);
        self
    }
}

/// What to include when grouping. The default is the cheapest useful answer: no cap, imports hidden.
#[derive(Debug, Clone, Copy, Default)]
pub struct GroupingOptions {
    pub limit: Option<usize>,
    pub include_imports: bool,
}

impl GroupingOptions {
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn including_imports(mut self) -> Self {
        self.include_imports = true;
        self
    }
}

/// The declaration a reference sits inside, so a caller answer can name the function or class.
///
/// `qualified_name` is the dotted chain of enclosing declaration names within the file, for example
/// `CheckoutService.placeOrder`; prepending the file's package yields the fully qualified caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnclosingDeclaration {
    pub kind: DeclKind,
    pub qualified_name: String,
    pub line: u32,
}

impl EnclosingDeclaration {
    fn from_chain(chain: &[&Declaration]) -> Self {
        let innermost = chain.last().expect("an enclosing chain is never empty");
        let qualified_name = chain
            .iter()
            .map(|declaration| declaration.name.as_str())
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>()
            .join(".");
        Self {
            kind: innermost.kind,
            qualified_name,
            line: innermost.line,
        }
    }
}

/// One reference site kept in the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enclosing: Option<EnclosingDeclaration>,
}

/// Every reference in one file, capped, with the count the cap dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceGroup {
    pub path: String,
    pub references: Vec<Reference>,
    /// How many references the cap dropped from this group, the `N` behind `... N more`.
    pub omitted: usize,
}

/// Groups reference sites by file, attaches each one's enclosing declaration, drops header
/// references unless asked to keep them, and caps each group.
///
/// Precondition: within every skeleton, a declaration's `children` are stored in ascending
/// source-line order (as the `ktsense-syntax` extractor produces them). Enclosure spans are
/// derived from that order; a violation is caught by a debug assertion rather than silently
/// attributing a reference to the wrong declaration.
pub fn group_references(
    locations: &[Location],
    skeletons: &[FileSkeleton],
    options: GroupingOptions,
) -> Vec<ReferenceGroup> {
    let skeleton_by_path: BTreeMap<&str, &FileSkeleton> = skeletons
        .iter()
        .map(|skeleton| (skeleton.path.as_str(), skeleton))
        .collect();

    let mut sites_by_path: BTreeMap<&str, Vec<&Location>> = BTreeMap::new();
    for location in locations {
        sites_by_path
            .entry(location.path.as_str())
            .or_default()
            .push(location);
    }

    sites_by_path
        .into_iter()
        .map(|(path, sites)| {
            let (references, omitted) =
                capped_references(&sites, skeleton_by_path.get(path).copied(), options);
            ReferenceGroup {
                path: path.to_string(),
                references,
                omitted,
            }
        })
        .collect()
}

fn capped_references(
    sites: &[&Location],
    skeleton: Option<&FileSkeleton>,
    options: GroupingOptions,
) -> (Vec<Reference>, usize) {
    let header_boundary = skeleton.and_then(first_declaration_line);
    let mut references: Vec<Reference> = sites
        .iter()
        .filter(|location| options.include_imports || !in_header(header_boundary, location.line))
        .map(|location| Reference {
            line: location.line,
            column: location.column,
            enclosing: skeleton.and_then(|skeleton| enclosing_declaration(skeleton, location.line)),
        })
        .collect();
    references.sort_by_key(|reference| (reference.line, reference.column));

    match options.limit {
        Some(limit) if references.len() > limit => {
            let omitted = references.len() - limit;
            references.truncate(limit);
            (references, omitted)
        }
        _ => (references, 0),
    }
}

/// The line of the first declaration, which every import sits above in a valid Kotlin file.
fn first_declaration_line(skeleton: &FileSkeleton) -> Option<u32> {
    skeleton
        .declarations
        .iter()
        .map(|declaration| declaration.line)
        .min()
}

fn in_header(boundary: Option<u32>, line: u32) -> bool {
    boundary.is_some_and(|boundary| line < boundary)
}

pub(crate) fn enclosing_declaration(
    skeleton: &FileSkeleton,
    line: u32,
) -> Option<EnclosingDeclaration> {
    let chain = enclosing_chain(&skeleton.declarations, line, None);
    (!chain.is_empty()).then(|| EnclosingDeclaration::from_chain(&chain))
}

/// The declarations from outermost to innermost whose spans contain `line`, empty when none do.
fn enclosing_chain(
    siblings: &[Declaration],
    line: u32,
    parent_end: Option<u32>,
) -> Vec<&Declaration> {
    for (index, declaration) in siblings.iter().enumerate() {
        let span_end = span_end(siblings, index, parent_end);
        if declaration.line <= line && span_end.is_none_or(|end| line <= end) {
            let mut chain = vec![declaration];
            chain.extend(enclosing_chain(&declaration.children, line, span_end));
            return chain;
        }
    }
    Vec::new()
}

/// The last line a declaration spans, recovered without a stored span: the next sibling's start
/// minus one, or the parent's bound for the last sibling, or unbounded at the end of the file.
fn span_end(siblings: &[Declaration], index: usize, parent_end: Option<u32>) -> Option<u32> {
    match siblings.get(index + 1) {
        Some(next) => {
            debug_assert!(
                next.line >= siblings[index].line,
                "sibling declarations must be stored in ascending source-line order"
            );
            Some(next.line.saturating_sub(1))
        }
        None => parent_end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton::Declaration;

    fn checkout_service() -> FileSkeleton {
        FileSkeleton::new("app/svc/CheckoutService.kt")
            .in_package("app.svc")
            .with_imports(vec!["shop.order.OrderRepository".to_string()])
            .with_declarations(vec![Declaration::class("CheckoutService", 3).containing(
                vec![
                    Declaration::function("placeOrder", 4).returning("OrderId"),
                    Declaration::function("cancel", 9),
                ],
            )])
    }

    fn enclosing(kind: DeclKind, qualified_name: &str, line: u32) -> Option<EnclosingDeclaration> {
        Some(EnclosingDeclaration {
            kind,
            qualified_name: qualified_name.to_string(),
            line,
        })
    }

    #[test]
    fn groups_by_file_orders_deterministically_and_tags_the_innermost_enclosing_declaration() {
        let locations = vec![
            Location::new("app/svc/CheckoutService.kt", 10),
            Location::new("app/svc/CheckoutService.kt", 6),
            Location::new("app/Main.kt", 2),
        ];

        let grouped = group_references(
            &locations,
            &[checkout_service()],
            GroupingOptions::default().including_imports(),
        );

        assert_eq!(
            grouped,
            vec![
                ReferenceGroup {
                    path: "app/Main.kt".to_string(),
                    references: vec![Reference {
                        line: 2,
                        column: None,
                        enclosing: None,
                    }],
                    omitted: 0,
                },
                ReferenceGroup {
                    path: "app/svc/CheckoutService.kt".to_string(),
                    references: vec![
                        Reference {
                            line: 6,
                            column: None,
                            enclosing: enclosing(
                                DeclKind::Function,
                                "CheckoutService.placeOrder",
                                4
                            ),
                        },
                        Reference {
                            line: 10,
                            column: None,
                            enclosing: enclosing(DeclKind::Function, "CheckoutService.cancel", 9),
                        },
                    ],
                    omitted: 0,
                },
            ]
        );
    }

    #[test]
    fn header_references_are_excluded_by_default_and_kept_only_when_asked_for() {
        let locations = vec![
            Location::new("app/svc/CheckoutService.kt", 1),
            Location::new("app/svc/CheckoutService.kt", 6),
        ];
        let skeletons = [checkout_service()];

        let default_lines = group_references(&locations, &skeletons, GroupingOptions::default())
            .into_iter()
            .flat_map(|group| group.references)
            .map(|reference| (reference.line, reference.enclosing))
            .collect::<Vec<_>>();
        let with_imports = group_references(
            &locations,
            &skeletons,
            GroupingOptions::default().including_imports(),
        )
        .into_iter()
        .flat_map(|group| group.references)
        .map(|reference| (reference.line, reference.enclosing))
        .collect::<Vec<_>>();

        assert_eq!(
            (default_lines, with_imports),
            (
                vec![(
                    6,
                    enclosing(DeclKind::Function, "CheckoutService.placeOrder", 4)
                )],
                vec![
                    (1, None),
                    (
                        6,
                        enclosing(DeclKind::Function, "CheckoutService.placeOrder", 4)
                    ),
                ],
            )
        );
    }

    #[test]
    fn a_group_past_the_limit_is_truncated_and_reports_the_dropped_count() {
        let locations = (6..=10)
            .map(|line| Location::new("app/svc/CheckoutService.kt", line))
            .collect::<Vec<_>>();
        let skeletons = [checkout_service()];

        let summarize = |options| {
            let group = group_references(&locations, &skeletons, options)
                .into_iter()
                .next()
                .expect("one file yields one group");
            (
                group
                    .references
                    .iter()
                    .map(|reference| reference.line)
                    .collect::<Vec<_>>(),
                group.omitted,
            )
        };

        assert_eq!(
            (
                summarize(GroupingOptions::default().with_limit(3)),
                summarize(GroupingOptions::default()),
            ),
            ((vec![6, 7, 8], 2), (vec![6, 7, 8, 9, 10], 0))
        );
    }

    fn nested_spans() -> FileSkeleton {
        FileSkeleton::new("app/B.kt").with_declarations(vec![
            Declaration::class("A", 3).containing(vec![
                Declaration::function("m1", 4),
                Declaration::function("m2", 8),
            ]),
            Declaration::function("top", 12),
        ])
    }

    /// The comparison boundaries that could each drift by one: a reference on a declaration's own
    /// start line (`A` at 3, `m1` at 4, `top` at 12), on a sibling's start line (`8`, the first
    /// line past `m1`'s span so it belongs to `m2` not `m1`), and on the last line of a span (`7`
    /// closes `m1`, `11` closes both `m2` and `A`). Checked as one attribution table.
    #[test]
    fn boundary_reference_lines_are_attributed_to_the_enclosing_declaration() {
        let locations = [3u32, 4, 7, 8, 11, 12]
            .map(|line| Location::new("app/B.kt", line))
            .to_vec();

        let attributions: Vec<(u32, Option<String>)> =
            group_references(&locations, &[nested_spans()], GroupingOptions::default())
                .into_iter()
                .flat_map(|group| group.references)
                .map(|reference| {
                    (
                        reference.line,
                        reference.enclosing.map(|enc| enc.qualified_name),
                    )
                })
                .collect();

        assert_eq!(
            attributions,
            vec![
                (3, Some("A".to_string())),
                (4, Some("A.m1".to_string())),
                (7, Some("A.m1".to_string())),
                (8, Some("A.m2".to_string())),
                (11, Some("A.m2".to_string())),
                (12, Some("top".to_string())),
            ]
        );
    }

    /// Siblings out of source-line order violate the precondition, which must be caught rather than
    /// silently producing a wrong enclosure. Debug-only: the guard is a `debug_assert!` so release
    /// builds degrade instead of aborting on adversarial input.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "ascending source-line order")]
    fn out_of_order_siblings_are_rejected_rather_than_mislabelled() {
        let skeleton = FileSkeleton::new("app/B.kt").with_declarations(vec![
            Declaration::function("later", 10),
            Declaration::function("earlier", 4),
        ]);

        let _ = group_references(
            &[Location::new("app/B.kt", 11)],
            &[skeleton],
            GroupingOptions::default(),
        );
    }
}
