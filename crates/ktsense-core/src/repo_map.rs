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

/// A directory whose files were dropped for budget, and how many of them, so a reader learns where
/// the omitted code lives rather than only how much of it there was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OmittedDirectory {
    /// Root-relative parent directory of the omitted files, or `.` for a file at the root.
    pub path: String,
    pub files: usize,
}

/// A budgeted map of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoMap {
    /// Files that contributed at least one signature, most central first.
    pub files: Vec<MappedFile>,
    /// The budget requested by the caller.
    pub budget: usize,
    /// Conservative upper bound on the tokens the emitted content occupies, now including the
    /// reserved omission summary. Per-item ceilings are superadditive, so this over-counts relative
    /// to the concatenated text; over-reporting is the safe direction for a limit and is the figure
    /// the packing gate itself enforced.
    pub token_upper_bound: usize,
    /// Files that had declarations to show but did not fit.
    pub files_omitted: usize,
    /// The directories those omitted files live in, most files first then path order. The full
    /// grouping regardless of how many the Markdown summary had room to name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omitted_directories: Vec<OmittedDirectory>,
    /// How many leading entries of [`Self::omitted_directories`] the Markdown "Omitted:" line names
    /// in full before summarizing the rest as `and N more directories`. A rendering detail sized to
    /// the reserved budget, not data, so it stays out of the JSON.
    #[serde(skip)]
    pub omitted_directories_shown: usize,
    /// Source files the scan read but did not map because they declare nothing public.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub files_without_public_declarations: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
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
    let mut candidate_paths: Vec<String> = Vec::new();
    let mut files_without_public_declarations = 0usize;
    for file in ordered {
        let signatures = signatures_in_reference_order(file, &ranking);
        if signatures.is_empty() {
            files_without_public_declarations += 1;
            continue;
        }
        let index = candidate_paths.len();
        candidate_paths.push(file.path.clone());
        units.push(Unit::header(&file.path));
        for signature in signatures {
            units.push(Unit::declaration(index, signature));
        }
    }

    // Emitting once at the full budget shows which files would be dropped when nothing is reserved.
    // Only then, and only when files were actually dropped, is room held back for the summary, so a
    // map whose files all fit reserves nothing and is chosen exactly as before.
    let full = emit_within_budget(units.clone(), input.budget, estimator);
    let dropped_without_reserve = omitted_from(&candidate_paths, &full.items);
    let reserve = summary_reserve(&dropped_without_reserve, input.budget, estimator);

    let emission = if reserve == 0 {
        full
    } else {
        emit_within_budget(units, input.budget - reserve, estimator)
    };
    let files_shown = regroup(&emission.items);
    let omitted = omitted_from(&candidate_paths, &emission.items);
    let omitted_directories = group_omitted(&omitted);

    // The summary is rendered to fit the room the files left, so the reported bound, which now
    // covers it, still never exceeds the budget. A reservation larger than the summary needs only
    // leaves the summary fully named; a smaller one truncates it to the largest groups.
    let room_for_summary = input.budget - emission.token_upper_bound;
    let omitted_directories_shown =
        fit_directories(&omitted_directories, room_for_summary, estimator);
    let summary_cost = omitted_directories_line(&omitted_directories, omitted_directories_shown)
        .map(|line| estimator.estimate(&line))
        .unwrap_or(0);

    RepoMap {
        files_omitted: omitted.len(),
        files: files_shown,
        budget: input.budget,
        token_upper_bound: emission.token_upper_bound + summary_cost,
        omitted_directories,
        omitted_directories_shown,
        files_without_public_declarations,
    }
}

/// How much of the budget to hold back for the omission summary before files are chosen: nothing
/// when no file was dropped, otherwise the cost of naming every dropped directory, capped at a
/// quarter of the budget so the map's own content stays the primary payload. The cap is why a huge
/// omitted list truncates to `and N more directories` rather than crowding out every signature.
fn summary_reserve<E: TokenEstimator>(dropped: &[String], budget: usize, estimator: &E) -> usize {
    if dropped.is_empty() {
        return 0;
    }
    let directories = group_omitted(dropped);
    let full_line = omitted_directories_line(&directories, directories.len()).unwrap_or_default();
    estimator
        .estimate(&full_line)
        .min(budget / OMISSION_SUMMARY_BUDGET_FRACTION)
}

/// The largest prefix of `directories` whose rendered "Omitted:" line, including its `and N more
/// directories` tail, fits `room`. Zero when not even the first directory fits, which drops the
/// line entirely rather than overrunning.
fn fit_directories<E: TokenEstimator>(
    directories: &[OmittedDirectory],
    room: usize,
    estimator: &E,
) -> usize {
    (1..=directories.len())
        .rev()
        .find(|&shown| {
            omitted_directories_line(directories, shown)
                .is_some_and(|line| estimator.estimate(&line) <= room)
        })
        .unwrap_or(0)
}

/// Candidate paths that no shown file covers: a file whose header emitted but whose every signature
/// fell outside the budget is dropped by [`regroup`], so it counts as omitted here too.
fn omitted_from(candidate_paths: &[String], emitted: &[Unit]) -> Vec<String> {
    let shown: std::collections::BTreeSet<String> =
        regroup(emitted).into_iter().map(|file| file.path).collect();
    candidate_paths
        .iter()
        .filter(|path| !shown.contains(*path))
        .cloned()
        .collect()
}

/// Groups omitted file paths by their root-relative parent directory, most files first then path
/// order, so the busiest omitted directory is named first and ties are stable.
fn group_omitted(paths: &[String]) -> Vec<OmittedDirectory> {
    let mut by_directory: BTreeMap<String, usize> = BTreeMap::new();
    for path in paths {
        *by_directory.entry(parent_directory(path)).or_insert(0) += 1;
    }
    let mut directories: Vec<OmittedDirectory> = by_directory
        .into_iter()
        .map(|(path, files)| OmittedDirectory { path, files })
        .collect();
    directories.sort_by(|left, right| {
        right
            .files
            .cmp(&left.files)
            .then_with(|| left.path.cmp(&right.path))
    });
    directories
}

fn parent_directory(path: &str) -> String {
    match path.rfind('/') {
        Some(slash) => path[..slash].to_string(),
        None => ".".to_string(),
    }
}

/// The Markdown "Omitted:" line naming where the budget-dropped files live, or `None` when none were
/// dropped or the reserved room fit not even the first directory. The first `shown` directories are
/// named in full; a positive remainder is summarized so the line stays within its reserved budget.
pub(crate) fn omitted_directories_line(
    directories: &[OmittedDirectory],
    shown: usize,
) -> Option<String> {
    if directories.is_empty() || shown == 0 {
        return None;
    }
    let named = directories[..shown]
        .iter()
        .map(|directory| format!("{} ({})", directory.path, directory.files))
        .collect::<Vec<_>>()
        .join(", ");
    let mut line = format!("Omitted: {named}");
    let remaining = directories.len() - shown;
    if remaining > 0 {
        line.push_str(&format!(", and {remaining} more directories"));
    }
    Some(line)
}

/// The Markdown line stating how many read files declared nothing public, or `None` when every file
/// contributed a signature. Kept grammatical for the single-file case.
pub(crate) fn files_without_public_declarations_line(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 file declares nothing public and is not mapped.".to_string()),
        many => Some(format!(
            "{many} files declare nothing public and are not mapped."
        )),
    }
}

/// At most a quarter of the budget is held back for the omission summary; the map's signatures keep
/// the rest.
const OMISSION_SUMMARY_BUDGET_FRACTION: usize = 4;

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
        partial: false,
    };
    let rendered = render_skeleton(&lone, &RenderOptions::default());
    let line = rendered.lines().next()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// An emission unit: either a file's heading or one signature under it. Budgeting at unit level
/// rather than file level is what lets the last file be partially included instead of dropped.
#[derive(Clone)]
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
            partial: false,
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
    fn a_reserved_summary_shrinks_the_file_room_and_names_the_dropped_directories() {
        let generous = map_without_references(&corpus(), 10_000);
        let tight = map_without_references(&corpus(), 15);

        assert_eq!(
            (
                tight.files.len(),
                tight.files.first().map(|file| file.declarations.clone()),
                tight.files_omitted,
                tight.omitted_directories.clone(),
                tight.token_upper_bound <= 15,
                generous.files.len(),
                generous.files_omitted,
            ),
            (
                1,
                Some(vec!["class Engine".to_string()]),
                2,
                vec![
                    OmittedDirectory {
                        path: "cli".to_string(),
                        files: 1,
                    },
                    OmittedDirectory {
                        path: "web".to_string(),
                        files: 1,
                    },
                ],
                true,
                3,
                0,
            )
        );
    }

    #[test]
    fn omitted_directories_group_by_parent_most_files_first_then_path_and_truncate() {
        let paths: Vec<String> = [
            "app/checkout/A.kt",
            "app/checkout/B.kt",
            "app/checkout/C.kt",
            "db/X.kt",
            "db/Y.kt",
            "app/reporting/R.kt",
            "app/reporting/S.kt",
            "Root.kt",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let groups = group_omitted(&paths);
        let full_line = omitted_directories_line(&groups, groups.len());
        let truncated = omitted_directories_line(&groups, 2);

        assert_eq!(
            (groups, full_line, truncated),
            (
                vec![
                    OmittedDirectory {
                        path: "app/checkout".to_string(),
                        files: 3,
                    },
                    OmittedDirectory {
                        path: "app/reporting".to_string(),
                        files: 2,
                    },
                    OmittedDirectory {
                        path: "db".to_string(),
                        files: 2,
                    },
                    OmittedDirectory {
                        path: ".".to_string(),
                        files: 1,
                    },
                ],
                Some("Omitted: app/checkout (3), app/reporting (2), db (2), . (1)".to_string()),
                Some(
                    "Omitted: app/checkout (3), app/reporting (2), and 2 more directories"
                        .to_string()
                ),
            )
        );
    }

    #[test]
    fn the_public_declaration_footer_is_grammatical_and_absent_at_zero() {
        assert_eq!(
            (
                files_without_public_declarations_line(0),
                files_without_public_declarations_line(1),
                files_without_public_declarations_line(39),
            ),
            (
                None,
                Some("1 file declares nothing public and is not mapped.".to_string()),
                Some("39 files declare nothing public and are not mapped.".to_string()),
            )
        );
    }

    #[test]
    fn files_declaring_nothing_public_are_counted_and_left_out_of_the_map() {
        let files = vec![
            file("core/Core.kt", "app.core", &[], &[("Engine", 1)]),
            file("core/Empty.kt", "app.core", &[], &[]),
            file("core/Config.kt", "app.core", &[], &[]),
        ];

        let map = map_without_references(&files, 10_000);

        assert_eq!(
            (
                map.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
                map.files_without_public_declarations,
                map.files_omitted,
                map.omitted_directories,
            ),
            (vec!["core/Core.kt".to_string()], 2, 0, Vec::new(),)
        );
    }

    #[test]
    fn the_reported_bound_including_the_summary_never_exceeds_any_budget() {
        let files = corpus();
        let references = counts(&[("Engine", 7), ("Helper", 2), ("Server", 1), ("Main", 0)]);

        let overrun = (0..=400usize).find(|&budget| {
            let map = build_repo_map(
                RepoMapInput {
                    files: &files,
                    references: &references,
                    budget,
                },
                &ByteRatioEstimator,
            );
            map.token_upper_bound > budget
        });

        assert_eq!(overrun, None);
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
