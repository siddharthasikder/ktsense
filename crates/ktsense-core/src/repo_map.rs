//! Token-budgeted repository map.
//!
//! The map answers "what is this repository, in as few tokens as you can spare". Files are ordered
//! by how central their package is in the import graph, then by how often their declarations are
//! referenced; declarations within a file are ordered by their own reference count, and signatures
//! are emitted until the budget is spent.
//!
//! Two different signals, deliberately not conflated:
//!
//! 1. **The import graph gives file-level centrality.** A package everything depends on outranks a
//!    leaf, which is a question about architecture and is exactly what imports answer.
//! 2. **Reference counts rank declarations.** Imports alone cannot: files in one package never
//!    import each other, so a declaration used mainly by its neighbours is credited nothing. On a
//!    largely single-package repository such as kotlinx.coroutines that is most of the real usage,
//!    which is why the import proxy put an alphabetically early file at the top instead of a central
//!    one (KT-22a).
//!
//! A [`ReferenceCounts`] is keyed by a declaration's *simple name*, because that is what an
//! occurrence in source spells; two declarations sharing a simple name across packages therefore
//! share a count. Import counts stay as the tiebreak, since they are fully qualified and so
//! discriminate exactly where reference counts cannot. Both limits are stated in the rendered
//! footer rather than implying a type-checked reference count neither can compute.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::budget::emit_within_budget;
use crate::imports::{build_import_graph, DepLevel};
use crate::rank::{page_rank, Graph, PageRankOptions};
use crate::render::{render_skeleton, RenderOptions};
use crate::skeleton::{Declaration, FileSkeleton};
use crate::TokenEstimator;

/// How many times each declaration name is referenced across the corpus.
///
/// Keyed by simple name, not by fully-qualified name: an occurrence in source is `OrderRepository`,
/// never `shop.order.OrderRepository`, and resolving one to the other is the engine's job, not a
/// syntactic pass's. A caller builds this by counting identifier occurrences; what counts as one is
/// the caller's decision and is documented where it is built.
///
/// A distinct type rather than a bare map because the other ranking signal, importer counts, is a
/// `BTreeMap<String, usize>` too, keyed by fully-qualified name. Two same-shaped maps with different
/// keys are transposable in a call, and silently so.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferenceCounts {
    by_name: BTreeMap<String, usize>,
}

impl ReferenceCounts {
    pub fn from_counts(by_name: BTreeMap<String, usize>) -> Self {
        Self { by_name }
    }

    pub fn for_name(&self, name: &str) -> usize {
        self.by_name.get(name).copied().unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Distinct names carrying at least one reference, which a caller reports as evidence that the
    /// count was actually gathered rather than silently defaulted.
    pub fn distinct_names(&self) -> usize {
        self.by_name.len()
    }
}

/// One file in the map, with the signatures that fit the budget in emission order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MappedFile {
    pub path: String,
    pub declarations: Vec<String>,
}

/// A budgeted map of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoMap {
    /// Files that contributed at least one signature, most central first.
    pub files: Vec<MappedFile>,
    /// The budget requested by the caller.
    pub budget: usize,
    /// Conservative upper bound on the tokens the emitted content occupies. Per-item ceilings are
    /// superadditive, so this over-counts relative to the concatenated text; over-reporting is the
    /// safe direction for a limit and is the figure the packing gate itself enforced.
    pub token_upper_bound: usize,
    /// Files that had declarations to show but did not fit.
    pub files_omitted: usize,
}

/// Everything the map is built from. A parameter object rather than four arguments, and it keeps the
/// two ranking signals named at the call site instead of positional.
pub struct RepoMapInput<'a> {
    pub files: &'a [FileSkeleton],
    pub references: &'a ReferenceCounts,
    pub budget: usize,
}

/// Builds a budgeted map from parsed skeletons and the corpus reference counts.
///
/// Ordering is total and deterministic: package rank descending, then the file's own reference
/// count, then its importer count, then path ascending, so the same repository maps identically on
/// every machine regardless of traversal order.
pub fn build_repo_map<E: TokenEstimator>(input: RepoMapInput<'_>, estimator: &E) -> RepoMap {
    let files = input.files;
    let package_scores = package_scores(files);
    let importers = importer_counts(files);
    let ranking = Ranking {
        references: input.references,
        importers: &importers,
    };

    let mut ordered: Vec<&FileSkeleton> = files.iter().collect();
    ordered.sort_by(|left, right| {
        let left_score = score_of(&package_scores, left);
        let right_score = score_of(&package_scores, right);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| ranking.file_weight(right).cmp(&ranking.file_weight(left)))
            .then_with(|| left.path.cmp(&right.path))
    });

    let mut units = Vec::new();
    let mut candidate_files = 0usize;
    for file in ordered {
        let signatures = signatures_in_reference_order(file, &ranking);
        if signatures.is_empty() {
            continue;
        }
        candidate_files += 1;
        units.push(Unit::header(&file.path));
        for signature in signatures {
            units.push(Unit::declaration(candidate_files - 1, signature));
        }
    }

    let emission = emit_within_budget(units, input.budget, estimator);
    let files_shown = regroup(&emission.items);
    RepoMap {
        files_omitted: candidate_files.saturating_sub(files_shown.len()),
        files: files_shown,
        budget: input.budget,
        token_upper_bound: emission.token_upper_bound,
    }
}

/// The two cross-file signals a declaration is ranked by, so every comparison reads them the same
/// way and no call site can pass them in the wrong order.
struct Ranking<'a> {
    references: &'a ReferenceCounts,
    importers: &'a BTreeMap<String, usize>,
}

impl Ranking<'_> {
    /// How much a declaration is used: its reference count, then how many files import it by name.
    /// The second term only ever breaks a tie in the first, and is fully qualified, so it separates
    /// two same-named declarations that the reference count necessarily pools.
    fn weight_of(&self, file: &FileSkeleton, declaration: &Declaration) -> (usize, usize) {
        (
            self.references.for_name(&declaration.name),
            self.importers
                .get(&qualified_name(file, declaration))
                .copied()
                .unwrap_or(0),
        )
    }

    /// How much a file is used: its declarations' weights summed term by term.
    ///
    /// Package rank cannot order files inside one package, and alphabetical order there is
    /// arbitrary: on kotlinx.coroutines it put `AbstractCoroutine.kt` ahead of the file declaring
    /// `launch` and `async`. Summing reference counts makes the file order earned rather than
    /// incidental, and unlike summed importer counts it is not uniformly zero in a single-package
    /// repository.
    fn file_weight(&self, file: &FileSkeleton) -> (usize, usize) {
        file.declarations
            .iter()
            .map(|declaration| self.weight_of(file, declaration))
            .fold((0, 0), |(references, importers), (next, also)| {
                (references + next, importers + also)
            })
    }
}

fn qualified_name(file: &FileSkeleton, declaration: &Declaration) -> String {
    match file.package.as_deref() {
        Some(package) => format!("{package}.{}", declaration.name),
        None => declaration.name.clone(),
    }
}

/// PageRank over the package-level import graph, so a package everything depends on outranks a leaf.
fn package_scores(files: &[FileSkeleton]) -> BTreeMap<String, f64> {
    let graph = build_import_graph(files, DepLevel::Package);
    let edges = graph
        .edges
        .iter()
        .map(|edge| (edge.from.clone(), edge.to.clone()));
    let ranked = page_rank(&Graph::from_edges(edges), &PageRankOptions::default());
    ranked
        .into_iter()
        .map(|node| (node.node, node.score))
        .collect()
}

/// How many files import each fully-qualified declaration name explicitly. A wildcard import names
/// no declaration, so it is deliberately absent here and is felt only through the package rank.
fn importer_counts(files: &[FileSkeleton]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for file in files {
        for import in &file.imports {
            if import.ends_with(".*") {
                continue;
            }
            *counts.entry(import.clone()).or_insert(0usize) += 1;
        }
    }
    counts
}

fn score_of(scores: &BTreeMap<String, f64>, file: &FileSkeleton) -> f64 {
    file.package
        .as_deref()
        .and_then(|package| scores.get(package).copied())
        .unwrap_or(0.0)
}

/// The file's top-level declarations rendered one signature per line, most referenced first, ties
/// broken by source order so the result is stable.
fn signatures_in_reference_order(file: &FileSkeleton, ranking: &Ranking<'_>) -> Vec<String> {
    let mut ranked: Vec<((usize, usize), u32, &Declaration)> = file
        .declarations
        .iter()
        .map(|declaration| {
            (
                ranking.weight_of(file, declaration),
                declaration.line,
                declaration,
            )
        })
        .collect();
    ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    ranked
        .into_iter()
        .filter_map(|(_, _, declaration)| signature_of(file, declaration))
        .collect()
}

/// One declaration's header, rendered by the same writer `outline` uses so a signature reads
/// identically in both, already neutralized. Children are dropped so only the header survives.
fn signature_of(file: &FileSkeleton, declaration: &Declaration) -> Option<String> {
    let lone = FileSkeleton {
        path: file.path.clone(),
        package: file.package.clone(),
        imports: Vec::new(),
        declarations: vec![Declaration {
            children: Vec::new(),
            ..declaration.clone()
        }],
        truncated: false,
    };
    let rendered = render_skeleton(&lone, &RenderOptions::default());
    let line = rendered.lines().next()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// An emission unit: either a file's heading or one signature under it. Budgeting at unit level
/// rather than file level is what lets the last file be partially included instead of dropped.
struct Unit {
    file_index: Option<usize>,
    text: String,
}

impl Unit {
    fn header(path: &str) -> Self {
        Self {
            file_index: None,
            text: format!("## {path}"),
        }
    }

    fn declaration(file_index: usize, text: String) -> Self {
        Self {
            file_index: Some(file_index),
            text,
        }
    }

    fn is_header(&self) -> bool {
        self.file_index.is_none()
    }
}

impl AsRef<str> for Unit {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

/// Rebuilds files from the emitted units, discarding a trailing heading whose signatures all fell
/// outside the budget: a file named with nothing under it tells a reader nothing.
fn regroup(units: &[Unit]) -> Vec<MappedFile> {
    let mut files: Vec<MappedFile> = Vec::new();
    for unit in units {
        if unit.is_header() {
            files.push(MappedFile {
                path: unit.text.trim_start_matches("## ").to_string(),
                declarations: Vec::new(),
            });
        } else if let Some(current) = files.last_mut() {
            current.declarations.push(unit.text.clone());
        }
    }
    files.retain(|file| !file.declarations.is_empty());
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ByteRatioEstimator;

    fn file(path: &str, package: &str, imports: &[&str], names: &[(&str, u32)]) -> FileSkeleton {
        FileSkeleton {
            path: path.to_string(),
            package: Some(package.to_string()),
            imports: imports.iter().map(|i| i.to_string()).collect(),
            declarations: names
                .iter()
                .map(|(name, line)| Declaration::class(*name, *line))
                .collect(),
            truncated: false,
        }
    }

    fn counts(pairs: &[(&str, usize)]) -> ReferenceCounts {
        ReferenceCounts::from_counts(
            pairs
                .iter()
                .map(|(name, count)| (name.to_string(), *count))
                .collect(),
        )
    }

    /// The map as built before KT-22a: ranked on imports alone, with no reference evidence at all.
    /// Several tests use it to prove a behaviour that does not depend on the new signal.
    fn map_without_references(files: &[FileSkeleton], budget: usize) -> RepoMap {
        build_repo_map(
            RepoMapInput {
                files,
                references: &ReferenceCounts::default(),
                budget,
            },
            &ByteRatioEstimator,
        )
    }

    /// Two leaf packages both import `core`, so `core` outranks them and its file leads the map.
    fn corpus() -> Vec<FileSkeleton> {
        vec![
            file(
                "core/Core.kt",
                "app.core",
                &[],
                &[("Engine", 1), ("Helper", 2)],
            ),
            file(
                "web/Web.kt",
                "app.web",
                &["app.core.Engine"],
                &[("Server", 1)],
            ),
            file(
                "cli/Cli.kt",
                "app.cli",
                &["app.core.Engine"],
                &[("Main", 1)],
            ),
        ]
    }

    #[test]
    fn the_most_imported_package_leads_and_its_most_referenced_declaration_leads_within_it() {
        let map = build_repo_map(
            RepoMapInput {
                files: &corpus(),
                references: &counts(&[("Engine", 7), ("Helper", 2), ("Server", 1), ("Main", 0)]),
                budget: 10_000,
            },
            &ByteRatioEstimator,
        );

        let observed: Vec<(&str, Vec<&str>)> = map
            .files
            .iter()
            .map(|file| {
                (
                    file.path.as_str(),
                    file.declarations.iter().map(String::as_str).collect(),
                )
            })
            .collect();

        assert_eq!(
            (observed, map.files_omitted, map.token_upper_bound <= 10_000),
            (
                vec![
                    ("core/Core.kt", vec!["class Engine", "class Helper"]),
                    ("web/Web.kt", vec!["class Server"]),
                    ("cli/Cli.kt", vec!["class Main"]),
                ],
                0,
                true
            )
        );
    }

    /// The whole point of KT-22a. Every file sits in one package, so no file imports another and
    /// every importer count is zero: the old ranking had nothing left but the alphabet, which put
    /// `Abstract.kt` first. Reference counts see the usage the imports cannot, and the order
    /// inverts. Both rankings are computed here so the diff between them is the assertion.
    #[test]
    fn inside_one_package_reference_counts_order_what_imports_cannot_see() {
        let single_package = vec![
            file("k/Abstract.kt", "k.core", &[], &[("AbstractThing", 1)]),
            file(
                "k/Builders.kt",
                "k.core",
                &[],
                &[("launch", 1), ("async", 2)],
            ),
        ];
        let paths = |map: &RepoMap| -> Vec<String> {
            map.files.iter().map(|file| file.path.clone()).collect()
        };

        let on_imports_alone = map_without_references(&single_package, 10_000);
        let on_references = build_repo_map(
            RepoMapInput {
                files: &single_package,
                references: &counts(&[("launch", 900), ("async", 400), ("AbstractThing", 12)]),
                budget: 10_000,
            },
            &ByteRatioEstimator,
        );

        assert_eq!(
            (
                paths(&on_imports_alone),
                paths(&on_references),
                on_references
                    .files
                    .first()
                    .map(|file| file.declarations.clone()),
            ),
            (
                vec!["k/Abstract.kt".to_string(), "k/Builders.kt".to_string()],
                vec!["k/Builders.kt".to_string(), "k/Abstract.kt".to_string()],
                Some(vec!["class launch".to_string(), "class async".to_string()]),
            )
        );
    }

    /// Reference counts are keyed by simple name, so two declarations sharing one share its count.
    /// The fully-qualified importer count is what separates them, and it may only break a tie.
    /// Isolated within a single file, where package rank cannot interfere: `Alpha` and `Beta` are
    /// referenced equally often, `Beta` is the one imported by name, and it leads despite `Alpha`
    /// coming first in source.
    #[test]
    fn an_equal_reference_count_is_separated_by_the_fully_qualified_importer_count() {
        let files = vec![
            file(
                "core/Pair.kt",
                "app.core",
                &[],
                &[("Alpha", 1), ("Beta", 2)],
            ),
            file("web/Use.kt", "app.web", &["app.core.Beta"], &[("User", 1)]),
        ];

        let map = build_repo_map(
            RepoMapInput {
                files: &files,
                references: &counts(&[("Alpha", 5), ("Beta", 5), ("User", 1)]),
                budget: 10_000,
            },
            &ByteRatioEstimator,
        );

        let ranked_by_source_order_alone = map
            .files
            .iter()
            .find(|mapped| mapped.path == "core/Pair.kt")
            .map(|mapped| mapped.declarations.clone());
        assert_eq!(
            ranked_by_source_order_alone,
            Some(vec!["class Beta".to_string(), "class Alpha".to_string()])
        );
    }

    #[test]
    fn a_budget_that_fits_one_file_partially_reports_the_files_it_dropped() {
        let generous = map_without_references(&corpus(), 10_000);
        let first_file_cost = ByteRatioEstimator.estimate("## core/Core.kt")
            + ByteRatioEstimator.estimate("class Engine");

        let tight = map_without_references(&corpus(), first_file_cost);

        assert_eq!(
            (
                tight.files.len(),
                tight.files.first().map(|f| f.declarations.len()),
                tight.files_omitted,
                tight.token_upper_bound <= first_file_cost,
                generous.files.len(),
            ),
            (1, Some(1), 2, true, 3)
        );
    }

    #[test]
    fn a_zero_budget_emits_nothing_and_admits_what_it_dropped() {
        let map = map_without_references(&corpus(), 0);

        assert_eq!(
            (map.files.len(), map.files_omitted, map.token_upper_bound),
            (0, 3, 0)
        );
    }

    #[test]
    fn a_wildcard_import_ranks_the_package_but_never_one_declaration() {
        let files = vec![
            file(
                "core/Core.kt",
                "app.core",
                &[],
                &[("Engine", 1), ("Zebra", 2)],
            ),
            file("web/Web.kt", "app.web", &["app.core.*"], &[("Server", 1)]),
        ];

        let map = map_without_references(&files, 10_000);

        // Both core declarations have zero explicit importers and no references were supplied, so
        // source order decides, proving the wildcard did not silently credit either one.
        assert_eq!(
            map.files.first().map(|file| (
                file.path.as_str(),
                file.declarations
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            )),
            Some(("core/Core.kt", vec!["class Engine", "class Zebra"]))
        );
    }

    #[test]
    fn a_file_without_declarations_never_becomes_an_empty_heading() {
        let files = vec![
            file("core/Core.kt", "app.core", &[], &[("Engine", 1)]),
            file("core/Empty.kt", "app.core", &[], &[]),
        ];

        let map = map_without_references(&files, 10_000);

        assert_eq!(
            map.files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            vec!["core/Core.kt"]
        );
    }
}
