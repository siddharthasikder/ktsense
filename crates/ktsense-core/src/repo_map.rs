//! Token-budgeted repository map.
//!
//! The map answers "what is this repository, in as few tokens as you can spare". Files are ordered
//! by how central their package is in the import graph, declarations within a file by how many other
//! files import them by name, and signatures are emitted until the budget is spent.
//!
//! Centrality is derived from imports alone, which is the only cross-file evidence a syntactic pass
//! has: bodies are elided by the compressor, so a call site inside a function is invisible here. A
//! wildcard import names no declaration, so it contributes to its package's rank but never to an
//! individual declaration's. Both limits are deliberate, and the output says so rather than implying
//! a reference count it cannot compute.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::budget::emit_within_budget;
use crate::imports::{build_import_graph, DepLevel};
use crate::rank::{page_rank, Graph, PageRankOptions};
use crate::render::{render_skeleton, RenderOptions};
use crate::skeleton::{Declaration, FileSkeleton};
use crate::TokenEstimator;

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

/// Builds a budgeted map from parsed skeletons.
///
/// Ordering is total and deterministic: package rank descending, then path ascending, so the same
/// repository maps identically on every machine regardless of traversal order.
pub fn build_repo_map<E: TokenEstimator>(
    files: &[FileSkeleton],
    budget: usize,
    estimator: &E,
) -> RepoMap {
    let package_scores = package_scores(files);
    let importers = importer_counts(files);

    let mut ordered: Vec<&FileSkeleton> = files.iter().collect();
    ordered.sort_by(|left, right| {
        let left_score = score_of(&package_scores, left);
        let right_score = score_of(&package_scores, right);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| file_importers(right, &importers).cmp(&file_importers(left, &importers)))
            .then_with(|| left.path.cmp(&right.path))
    });

    let mut units = Vec::new();
    let mut candidate_files = 0usize;
    for file in ordered {
        let signatures = signatures_in_reference_order(file, &importers);
        if signatures.is_empty() {
            continue;
        }
        candidate_files += 1;
        units.push(Unit::header(&file.path));
        for signature in signatures {
            units.push(Unit::declaration(candidate_files - 1, signature));
        }
    }

    let emission = emit_within_budget(units, budget, estimator);
    let files_shown = regroup(&emission.items);
    RepoMap {
        files_omitted: candidate_files.saturating_sub(files_shown.len()),
        files: files_shown,
        budget,
        token_upper_bound: emission.token_upper_bound,
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

/// How many times this file's own declarations are imported by name across the corpus.
///
/// Package rank alone cannot order files inside one package, and alphabetical order there is
/// arbitrary: on kotlinx.coroutines it put `AbstractCoroutine.kt` ahead of the file declaring
/// `launch` and `async`. Summing per-declaration importers makes the file order earned rather than
/// incidental.
fn file_importers(file: &FileSkeleton, importers: &BTreeMap<String, usize>) -> usize {
    file.declarations
        .iter()
        .map(|declaration| {
            let key = match file.package.as_deref() {
                Some(package) => format!("{package}.{}", declaration.name),
                None => declaration.name.clone(),
            };
            importers.get(&key).copied().unwrap_or(0)
        })
        .sum()
}

/// The file's top-level declarations rendered one signature per line, most imported first, ties
/// broken by source order so the result is stable.
fn signatures_in_reference_order(
    file: &FileSkeleton,
    importers: &BTreeMap<String, usize>,
) -> Vec<String> {
    let mut ranked: Vec<(usize, u32, &Declaration)> = file
        .declarations
        .iter()
        .map(|declaration| {
            let key = match file.package.as_deref() {
                Some(package) => format!("{package}.{}", declaration.name),
                None => declaration.name.clone(),
            };
            (
                importers.get(&key).copied().unwrap_or(0),
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
    fn the_most_imported_package_leads_and_its_most_imported_declaration_leads_within_it() {
        let map = build_repo_map(&corpus(), 10_000, &ByteRatioEstimator);

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
                    ("cli/Cli.kt", vec!["class Main"]),
                    ("web/Web.kt", vec!["class Server"]),
                ],
                0,
                true
            )
        );
    }

    #[test]
    fn a_budget_that_fits_one_file_partially_reports_the_files_it_dropped() {
        let generous = build_repo_map(&corpus(), 10_000, &ByteRatioEstimator);
        let first_file_cost = ByteRatioEstimator.estimate("## core/Core.kt")
            + ByteRatioEstimator.estimate("class Engine");

        let tight = build_repo_map(&corpus(), first_file_cost, &ByteRatioEstimator);

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
        let map = build_repo_map(&corpus(), 0, &ByteRatioEstimator);

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

        let map = build_repo_map(&files, 10_000, &ByteRatioEstimator);

        // Both core declarations have zero explicit importers, so source order decides, proving the
        // wildcard did not silently credit either one.
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

        let map = build_repo_map(&files, 10_000, &ByteRatioEstimator);

        assert_eq!(
            map.files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            vec!["core/Core.kt"]
        );
    }
}
