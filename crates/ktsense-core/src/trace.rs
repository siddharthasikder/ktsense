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
    declaration_starting_at, enclosing_declaration, group_references, is_test_source,
    GroupingOptions, Location, ReferenceGroup, SiteKind,
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
    /// Whether this caller lives in a test source, by [`is_test_source`]. `trace` lists production
    /// callers before test callers and `context` orders them the same way (KT-91).
    #[serde(default)]
    pub test: bool,
}

/// The callers found at one distance from the symbol: depth 1 is the declarations that refer to
/// it directly, depth 2 the declarations that refer to those, and so on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerLevel {
    pub depth: usize,
    pub callers: Vec<RelatedDeclaration>,
}

/// A reference site left out of the callers and the per-file usage list because it names the symbol
/// without using it: a comment, KDoc link, string literal, or the name of another declaration with
/// the same simple name. Counted rather than dropped, so the Usages line can say what it omitted and
/// why, and JSON can carry the sites with their kind (KT-83).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedSite {
    pub path: String,
    pub line: u32,
    pub kind: SiteKind,
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
    /// Sites a whole-word engine search reported that name the symbol without using it, kept as
    /// data so the answer accounts for every site (KT-83).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_sites: Vec<ExcludedSite>,
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
    let implementation_sites =
        implementations_other_than(&input.implementation_sites, &input.definition_site);
    let implementors = if skeletons.cannot_be_subtyped(&input.definition_site) {
        Vec::new()
    } else {
        related_declarations(&implementation_sites, &skeletons)
    };

    let mut set_aside = vec![input.definition_site.clone()];
    set_aside.extend(implementation_sites.iter().cloned());
    let direct = callers_of(&input.reference_sites, &set_aside, input.skeletons);

    let (usage_sites, mut excluded_sites) =
        partition_by_kind(&input.reference_sites, &input.definition_site);
    excluded_sites.sort_by(|a, b| (&a.path, a.line, a.kind).cmp(&(&b.path, b.line, b.kind)));
    let usages = group_references(&usage_sites, input.skeletons, input.options);
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
        excluded_sites,
    }
}

/// Splits reference sites into those the usage list keeps and those it leaves out. A `Code` site is
/// kept, and so is the queried definition's own name site whatever its kind, because that site
/// stays listed as it always was; every other non-`Code` site is a counted exclusion (KT-83).
fn partition_by_kind(
    references: &[Location],
    definition: &Location,
) -> (Vec<Location>, Vec<ExcludedSite>) {
    let mut kept = Vec::new();
    let mut excluded = Vec::new();
    for site in references {
        if site.kind.is_code() || same_site(site, definition) {
            kept.push(site.clone());
        } else {
            excluded.push(ExcludedSite {
                path: site.path.clone(),
                line: site.line,
                kind: site.kind,
            });
        }
    }
    (kept, excluded)
}

/// The declarations enclosing the reference sites in `references`, minus any site listed in
/// `set_aside`, one entry per declaration with its site count. Only `Code` sites produce callers;
/// a comment, KDoc, string or same-named declaration names the symbol without calling it (KT-83).
/// This is the whole of caller derivation, reused for each deeper level.
pub fn callers_of(
    references: &[Location],
    set_aside: &[Location],
    skeletons: &[FileSkeleton],
) -> Vec<RelatedDeclaration> {
    let index = SkeletonIndex::new(skeletons);
    let sites: Vec<Location> = references
        .iter()
        .filter(|site| site.kind.is_code())
        .filter(|site| !set_aside.iter().any(|excluded| same_site(excluded, site)))
        .cloned()
        .collect();
    related_declarations(&sites, &index)
}

fn same_site(a: &Location, b: &Location) -> bool {
    a.path == b.path && a.line == b.line
}

/// kmp-lsp 0.26.0 answers `textDocument/implementation` on a class with no subtypes by returning
/// the class's own declaration. A declaration never implements itself, so that site is dropped.
fn implementations_other_than(sites: &[Location], definition: &Location) -> Vec<Location> {
    sites
        .iter()
        .filter(|site| !same_site(site, definition))
        .cloned()
        .collect()
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

    /// kmp-lsp 0.26.0 answers `textDocument/implementation` on a final class with name matches
    /// such as `FooTest`, so a class Kotlin makes final is answered from its own modifiers instead.
    /// An unresolved or partially parsed definition is not judged, because a parse error can drop
    /// the very `open` that makes a class subtypable, and keeps the engine's answer.
    fn cannot_be_subtyped(&self, definition: &Location) -> bool {
        self.by_path
            .get(definition.path.as_str())
            .filter(|skeleton| !skeleton.partial)
            .and_then(|skeleton| declaration_starting_at(skeleton, definition.line))
            .is_some_and(|declaration| !declaration.can_be_subtyped())
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
/// Sites whose declaration cannot be resolved stay as individual entries so nothing is dropped. The
/// result is ordered production declarations first, then test declarations, each in path order, so
/// `trace` and `context` spend the reader's attention and the budget on production callers first
/// (KT-91).
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
                test: is_test_source(&site.path),
            },
            None => RelatedDeclaration {
                path: site.path.clone(),
                line: site.line,
                qualified_name: None,
                kind: None,
                sites: 0,
                test: is_test_source(&site.path),
            },
        };
        let key = (entry.path.clone(), entry.line, entry.qualified_name.clone());
        by_declaration.entry(key).or_insert(entry).sites += 1;
    }
    let mut declarations: Vec<RelatedDeclaration> = by_declaration.into_values().collect();
    declarations.sort_by(|a, b| a.test.cmp(&b.test));
    declarations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton::{Declaration, Modifier};

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
            test: crate::references::is_test_source(path),
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
    fn a_definition_echoed_back_as_its_own_implementation_is_not_an_implementor() {
        let skeletons = vec![
            FileSkeleton::new("app/AuditTrail.kt")
                .in_package("shop.app.reporting")
                .with_declarations(vec![Declaration::class("AuditTrail", 3)]),
            FileSkeleton::new("app/CheckoutService.kt")
                .in_package("shop.app.checkout")
                .with_declarations(vec![Declaration::class("CheckoutService", 8)]),
        ];
        let echoed = TraceInput {
            definition: Definition {
                qualified_name: "shop.app.reporting.AuditTrail".to_string(),
                path: "app/AuditTrail.kt".to_string(),
                line: 3,
                signature: "class AuditTrail".to_string(),
            },
            index: IndexCompleteness::Complete,
            definition_site: Location::new("app/AuditTrail.kt", 3),
            implementation_sites: vec![Location::new("app/AuditTrail.kt", 3).at_column(7)],
            reference_sites: vec![
                Location::new("app/AuditTrail.kt", 3),
                Location::new("app/CheckoutService.kt", 10),
            ],
            skeletons: &skeletons,
            options: GroupingOptions::default(),
        };

        let report = build_trace(echoed);

        assert_eq!(
            (report.implementors, report.callers[0].callers.clone()),
            (
                vec![],
                vec![related(
                    "app/CheckoutService.kt",
                    8,
                    Some("shop.app.checkout.CheckoutService"),
                    Some(DeclKind::Class),
                    1,
                )]
            )
        );
    }

    fn partially_parsed(mut skeletons: Vec<FileSkeleton>) -> Vec<FileSkeleton> {
        skeletons[0] = skeletons[0].clone().marked_partial();
        skeletons
    }

    #[test]
    fn a_class_that_cannot_be_subtyped_has_no_implementors_whatever_the_engine_says() {
        let class_named = |modifiers: Vec<Modifier>| {
            vec![
                FileSkeleton::new("app/Factory.kt")
                    .in_package("shop.app")
                    .with_declarations(vec![
                        Declaration::class("Factory", 3).with_modifiers(modifiers)
                    ]),
                FileSkeleton::new("test/FactoryTest.kt")
                    .in_package("shop.app")
                    .with_declarations(vec![Declaration::class("FactoryTest", 5)]),
            ]
        };
        let implementors_of = |skeletons: &[FileSkeleton]| {
            build_trace(TraceInput {
                definition: Definition {
                    qualified_name: "shop.app.Factory".to_string(),
                    path: "app/Factory.kt".to_string(),
                    line: 3,
                    signature: "class Factory".to_string(),
                },
                index: IndexCompleteness::Complete,
                definition_site: Location::new("app/Factory.kt", 3),
                implementation_sites: vec![Location::new("test/FactoryTest.kt", 5)],
                reference_sites: vec![],
                skeletons,
                options: GroupingOptions::default(),
            })
            .implementors
            .len()
        };

        let observed = (
            implementors_of(&class_named(vec![])),
            implementors_of(&class_named(vec![Modifier::Data])),
            implementors_of(&class_named(vec![Modifier::Open])),
            implementors_of(&class_named(vec![Modifier::Abstract])),
            implementors_of(&class_named(vec![Modifier::Sealed])),
            implementors_of(&class_named(vec![Modifier::Enum])),
            implementors_of(&class_named(vec![Modifier::Expect])),
            implementors_of(&partially_parsed(class_named(vec![]))),
        );
        assert_eq!(observed, (0, 0, 1, 1, 1, 1, 1, 1));
    }

    /// Only `Code` sites become callers and usage rows; a comment, KDoc, string and same-named
    /// declaration name the symbol without using it, so they leave the callers and the listing but
    /// are counted in `excluded_sites` with their kind. The definition's own name site stays listed.
    #[test]
    fn text_and_declaration_name_sites_are_excluded_from_callers_and_usages_but_counted() {
        let skeletons = vec![FileSkeleton::new("app/Widget.kt")
            .in_package("app")
            .with_declarations(vec![
                Declaration::class("Widget", 3),
                Declaration::function("use", 7),
            ])];
        let report = build_trace(TraceInput {
            definition: Definition {
                qualified_name: "app.Widget".to_string(),
                path: "app/Widget.kt".to_string(),
                line: 3,
                signature: "class Widget".to_string(),
            },
            index: IndexCompleteness::Complete,
            definition_site: Location::new("app/Widget.kt", 3),
            implementation_sites: vec![],
            reference_sites: vec![
                Location::new("app/Widget.kt", 3),
                Location::new("app/Widget.kt", 8),
                Location::new("app/Widget.kt", 5).with_kind(SiteKind::Comment),
                Location::new("app/Widget.kt", 6).with_kind(SiteKind::Kdoc),
                Location::new("app/Widget.kt", 9).with_kind(SiteKind::String),
                Location::new("app/Widget.kt", 12).with_kind(SiteKind::DeclarationName),
            ],
            skeletons: &skeletons,
            options: GroupingOptions::default(),
        });

        let listed_lines: Vec<u32> = report
            .usages
            .iter()
            .flat_map(|group| group.references.iter().map(|reference| reference.line))
            .collect();
        let excluded: Vec<(u32, SiteKind)> = report
            .excluded_sites
            .iter()
            .map(|site| (site.line, site.kind))
            .collect();
        let observed = (
            report.callers[0]
                .callers
                .iter()
                .map(|caller| caller.qualified_name.clone())
                .collect::<Vec<_>>(),
            report.sites,
            listed_lines,
            excluded,
        );
        assert_eq!(
            observed,
            (
                vec![Some("app.use".to_string())],
                6,
                vec![3, 8],
                vec![
                    (5, SiteKind::Comment),
                    (6, SiteKind::Kdoc),
                    (9, SiteKind::String),
                    (12, SiteKind::DeclarationName),
                ],
            )
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
