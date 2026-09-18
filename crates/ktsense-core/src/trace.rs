//! Assembles a `trace` answer, purely, from engine locations and file skeletons.
//!
//! `kmp-lsp` 0.26.0 has no call hierarchy, so callers are the declarations that enclose each
//! reference site, as [`crate::references`] attributes them; the declaration site itself and the
//! sites of implementing declarations are set aside first, because an `override` is a reference to
//! the symbol without being a call of it. Every answer carries an [`IndexCompleteness`], because a
//! reference list read off a still-building index is a lower bound, and the reader must be told.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::references::{
    enclosing_declaration, group_references, GroupingOptions, Location, ReferenceGroup,
};
use crate::skeleton::{DeclKind, FileSkeleton};

/// Whether the engine had finished indexing when the answer was collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexCompleteness {
    /// The index was still building, or never reported finishing: sites may be missing.
    Partial,
    /// The index reported finishing before the requests were issued.
    Complete,
}

impl IndexCompleteness {
    pub fn label(self) -> &'static str {
        match self {
            IndexCompleteness::Partial => "partial",
            IndexCompleteness::Complete => "complete",
        }
    }
}

/// The traced symbol's own declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Definition {
    pub qualified_name: String,
    pub path: String,
    pub line: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signature: String,
}

/// A declaration related to the traced symbol: one that implements it, or one whose body refers
/// to it. `line` is where the declaration starts when the skeleton resolved it, otherwise the
/// reference site itself, and `qualified_name` is absent in that same fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelatedDeclaration {
    pub path: String,
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<DeclKind>,
    /// How many reference sites inside this declaration refer to the symbol.
    pub sites: usize,
}

/// The callers found at one distance from the symbol: depth 1 is the declarations that refer to
/// it directly, depth 2 the declarations that refer to those, and so on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerLevel {
    pub depth: usize,
    pub callers: Vec<RelatedDeclaration>,
}

/// The complete `trace` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceReport {
    pub symbol: String,
    pub index: IndexCompleteness,
    pub definition: Definition,
    pub implementors: Vec<RelatedDeclaration>,
    pub callers: Vec<CallerLevel>,
    /// Every reference site grouped by file, the declaration site included.
    pub usages: Vec<ReferenceGroup>,
    /// Reference sites in total, before any per-file cap.
    pub sites: usize,
}

/// Everything `build_trace` needs, gathered by the caller from the engine and the parser.
pub struct TraceInput<'a> {
    pub definition: Definition,
    pub index: IndexCompleteness,
    pub definition_site: Location,
    pub implementation_sites: Vec<Location>,
    pub reference_sites: Vec<Location>,
    pub skeletons: &'a [FileSkeleton],
    pub options: GroupingOptions,
}

impl TraceReport {
    /// Attaches the callers found at the next depth. Levels are appended in order, so `depth` is
    /// one more than the deepest level already present.
    pub fn with_deeper_callers(mut self, callers: Vec<RelatedDeclaration>) -> Self {
        let depth = self.callers.len() + 1;
        self.callers.push(CallerLevel { depth, callers });
        self
    }

    /// The direct callers, from which a deeper level is requested.
    pub fn direct_callers(&self) -> &[RelatedDeclaration] {
        self.callers
            .first()
            .map(|level| level.callers.as_slice())
            .unwrap_or_default()
    }
}

/// Builds the answer: implementors and direct callers attributed to their enclosing declarations,
/// and every reference site grouped by file.
pub fn build_trace(input: TraceInput<'_>) -> TraceReport {
    let skeletons = SkeletonIndex::new(input.skeletons);
    let implementors = related_declarations(&input.implementation_sites, &skeletons);

    let mut set_aside = vec![input.definition_site.clone()];
    set_aside.extend(input.implementation_sites.iter().cloned());
    let direct = callers_of(&input.reference_sites, &set_aside, input.skeletons);

    let usages = group_references(&input.reference_sites, input.skeletons, input.options);
    TraceReport {
        symbol: input.definition.qualified_name.clone(),
        index: input.index,
        definition: input.definition,
        implementors,
        callers: vec![CallerLevel {
            depth: 1,
            callers: direct,
        }],
        sites: input.reference_sites.len(),
        usages,
    }
}

/// The declarations enclosing the reference sites in `references`, minus any site listed in
/// `set_aside`, one entry per declaration with its site count. This is the whole of caller
/// derivation, reused for each deeper level.
pub fn callers_of(
    references: &[Location],
    set_aside: &[Location],
    skeletons: &[FileSkeleton],
) -> Vec<RelatedDeclaration> {
    let index = SkeletonIndex::new(skeletons);
    let sites: Vec<Location> = references
        .iter()
        .filter(|site| !set_aside.iter().any(|excluded| same_site(excluded, site)))
        .cloned()
        .collect();
    related_declarations(&sites, &index)
}

fn same_site(a: &Location, b: &Location) -> bool {
    a.path == b.path && a.line == b.line
}

struct SkeletonIndex<'a> {
    by_path: BTreeMap<&'a str, &'a FileSkeleton>,
}

impl<'a> SkeletonIndex<'a> {
    fn new(skeletons: &'a [FileSkeleton]) -> Self {
        Self {
            by_path: skeletons
                .iter()
                .map(|skeleton| (skeleton.path.as_str(), skeleton))
                .collect(),
        }
    }

    /// The declaration enclosing `site`, fully qualified with its file's package, when the file's
    /// skeleton is known and a declaration spans the line.
    fn resolve(&self, site: &Location) -> Option<(String, DeclKind, u32)> {
        let skeleton = self.by_path.get(site.path.as_str())?;
        let enclosing = enclosing_declaration(skeleton, site.line)?;
        let qualified = match &skeleton.package {
            Some(package) if !enclosing.qualified_name.is_empty() => {
                format!("{package}.{}", enclosing.qualified_name)
            }
            Some(package) => package.clone(),
            None => enclosing.qualified_name,
        };
        Some((qualified, enclosing.kind, enclosing.line))
    }
}

/// Collapses reference sites onto the declarations enclosing them, counting sites per declaration.
/// Sites whose declaration cannot be resolved stay as individual entries so nothing is dropped.
fn related_declarations(
    sites: &[Location],
    skeletons: &SkeletonIndex<'_>,
) -> Vec<RelatedDeclaration> {
    let mut by_declaration: BTreeMap<(String, u32, Option<String>), RelatedDeclaration> =
        BTreeMap::new();
    for site in sites {
        let entry = match skeletons.resolve(site) {
            Some((qualified_name, kind, line)) => RelatedDeclaration {
                path: site.path.clone(),
                line,
                qualified_name: Some(qualified_name),
                kind: Some(kind),
                sites: 0,
            },
            None => RelatedDeclaration {
                path: site.path.clone(),
                line: site.line,
                qualified_name: None,
                kind: None,
                sites: 0,
            },
        };
        let key = (entry.path.clone(), entry.line, entry.qualified_name.clone());
        by_declaration.entry(key).or_insert(entry).sites += 1;
    }
    by_declaration.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton::Declaration;

    fn shop() -> Vec<FileSkeleton> {
        vec![
            FileSkeleton::new("core/OrderRepository.kt")
                .in_package("shop.order")
                .with_declarations(vec![Declaration::interface("OrderRepository", 3)
                    .containing(vec![Declaration::function("save", 4)])]),
            FileSkeleton::new("db/JdbcOrderRepository.kt")
                .in_package("shop.db")
                .with_declarations(vec![Declaration::class("JdbcOrderRepository", 5)
                    .containing(vec![Declaration::function("save", 8)])]),
            FileSkeleton::new("app/CheckoutService.kt")
                .in_package("shop.app.checkout")
                .with_declarations(vec![Declaration::class("CheckoutService", 8).containing(
                    vec![
                        Declaration::function("placeOrder", 12),
                        Declaration::function("retry", 20),
                    ],
                )]),
        ]
    }

    fn input(skeletons: &[FileSkeleton]) -> TraceInput<'_> {
        TraceInput {
            definition: Definition {
                qualified_name: "shop.order.OrderRepository.save".to_string(),
                path: "core/OrderRepository.kt".to_string(),
                line: 4,
                signature: "fun save(order: Order): OrderId".to_string(),
            },
            index: IndexCompleteness::Complete,
            definition_site: Location::new("core/OrderRepository.kt", 4),
            implementation_sites: vec![Location::new("db/JdbcOrderRepository.kt", 8)],
            reference_sites: vec![
                Location::new("core/OrderRepository.kt", 4),
                Location::new("db/JdbcOrderRepository.kt", 8),
                Location::new("app/CheckoutService.kt", 13),
                Location::new("app/CheckoutService.kt", 14),
                Location::new("app/CheckoutService.kt", 21),
                Location::new("lib/Unparsed.kt", 9),
            ],
            skeletons,
            options: GroupingOptions::default(),
        }
    }

    fn related(
        path: &str,
        line: u32,
        name: Option<&str>,
        kind: Option<DeclKind>,
        sites: usize,
    ) -> RelatedDeclaration {
        RelatedDeclaration {
            path: path.to_string(),
            line,
            qualified_name: name.map(str::to_string),
            kind,
            sites,
        }
    }

    #[test]
    fn callers_are_enclosing_declarations_minus_the_definition_and_its_overrides() {
        let skeletons = shop();
        let report = build_trace(input(&skeletons));

        let observed = (
            report.implementors.clone(),
            report.callers.clone(),
            report.sites,
            report.usages.len(),
            report.index,
        );
        let expected = (
            vec![related(
                "db/JdbcOrderRepository.kt",
                8,
                Some("shop.db.JdbcOrderRepository.save"),
                Some(DeclKind::Function),
                1,
            )],
            vec![CallerLevel {
                depth: 1,
                callers: vec![
                    related(
                        "app/CheckoutService.kt",
                        12,
                        Some("shop.app.checkout.CheckoutService.placeOrder"),
                        Some(DeclKind::Function),
                        2,
                    ),
                    related(
                        "app/CheckoutService.kt",
                        20,
                        Some("shop.app.checkout.CheckoutService.retry"),
                        Some(DeclKind::Function),
                        1,
                    ),
                    related("lib/Unparsed.kt", 9, None, None, 1),
                ],
            }],
            6,
            4,
            IndexCompleteness::Complete,
        );
        assert_eq!(
            (observed.0, observed.1, observed.2, observed.3, observed.4),
            expected
        );
    }

    #[test]
    fn deeper_levels_append_in_order_and_serialize_the_index_marker_lowercase() {
        let skeletons = shop();
        let report = build_trace(input(&skeletons)).with_deeper_callers(vec![related(
            "app/Main.kt",
            3,
            Some("shop.app.main"),
            Some(DeclKind::Function),
            1,
        )]);

        let json = serde_json::to_value(&report).expect("serializes");
        let observed = (
            report
                .callers
                .iter()
                .map(|level| level.depth)
                .collect::<Vec<_>>(),
            report.direct_callers().len(),
            json["index"].clone(),
            json["callers"][1]["callers"][0]["qualified_name"].clone(),
        );
        assert_eq!(
            observed,
            (
                vec![1, 2],
                3,
                serde_json::json!("complete"),
                serde_json::json!("shop.app.main")
            )
        );
    }
}
