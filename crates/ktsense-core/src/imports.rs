//! The import dependency graph: which packages (or files) import which, what stays inside the
//! workspace, and where the import cycles are.
//!
//! Pure. It is built from [`FileSkeleton`] values the syntax adapter already produced, so the
//! resolution rules here are tested from hand-built skeletons with no filesystem in reach.
//!
//! Resolution is syntactic and honest about it. An import resolves inside the workspace only when a
//! workspace file in the named package declares the imported name at the top level; a wildcard
//! resolves on the package alone. Everything else is external. No type inference and no classpath:
//! this reports the import statements a reader could see, not a compiler's resolved references.
//!
//! Only files that declare a `package` take part. A build script or a default-package file has no
//! package-qualified identity to place in the graph, so it is left out rather than guessed at.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::rank::Graph;
use crate::scc::cycles;
use crate::skeleton::FileSkeleton;

/// Whether the graph connects packages or individual files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepLevel {
    File,
    Package,
}

impl DepLevel {
    /// The word for one node at this level, used in rendered output and pluralized by the caller.
    pub fn noun(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Package => "package",
        }
    }
}

/// One import that resolves inside the workspace, as a directed edge between two graph nodes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DepEdge {
    pub from: String,
    pub to: String,
}

/// One import that does not resolve inside the workspace, kept with the node that wrote it so a
/// reader can see which package or file reaches outside.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExternalImport {
    pub source: String,
    pub import: String,
}

/// The resolved import graph at one level, with its cycles already computed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportGraph {
    pub level: DepLevel,
    pub nodes: Vec<String>,
    pub edges: Vec<DepEdge>,
    pub external: Vec<ExternalImport>,
    pub cycles: Vec<Vec<String>>,
}

/// Builds the import graph for a set of file skeletons at the requested level.
pub fn build_import_graph(files: &[FileSkeleton], level: DepLevel) -> ImportGraph {
    let index = WorkspaceIndex::build(files);
    let mut nodes: BTreeSet<String> = BTreeSet::new();
    let mut edges: BTreeSet<(String, String)> = BTreeSet::new();
    let mut external: BTreeSet<(String, String)> = BTreeSet::new();

    for file in files {
        let Some(package) = file.package.as_deref() else {
            continue;
        };
        let source = match level {
            DepLevel::Package => package.to_string(),
            DepLevel::File => file.path.clone(),
        };
        nodes.insert(source.clone());
        for import in &file.imports {
            match index.resolve(import, level) {
                Resolution::Internal(targets) => {
                    for target in targets {
                        if target != source {
                            edges.insert((source.clone(), target));
                        }
                    }
                }
                Resolution::External => {
                    external.insert((source.clone(), import.clone()));
                }
            }
        }
    }

    let edges: Vec<DepEdge> = edges
        .into_iter()
        .map(|(from, to)| DepEdge { from, to })
        .collect();
    let graph = Graph::from_edges(
        edges
            .iter()
            .map(|edge| (edge.from.clone(), edge.to.clone())),
    );

    ImportGraph {
        level,
        nodes: nodes.into_iter().collect(),
        edges,
        external: external
            .into_iter()
            .map(|(source, import)| ExternalImport { source, import })
            .collect(),
        cycles: cycles(&graph),
    }
}

enum Resolution {
    Internal(Vec<String>),
    External,
}

/// What the workspace declares, indexed so an import can be resolved without touching a file again.
struct WorkspaceIndex {
    packages: BTreeSet<String>,
    names_by_package: BTreeMap<String, BTreeSet<String>>,
    files_by_package: BTreeMap<String, Vec<String>>,
    files_by_symbol: BTreeMap<(String, String), Vec<String>>,
}

impl WorkspaceIndex {
    fn build(files: &[FileSkeleton]) -> Self {
        let mut index = Self {
            packages: BTreeSet::new(),
            names_by_package: BTreeMap::new(),
            files_by_package: BTreeMap::new(),
            files_by_symbol: BTreeMap::new(),
        };
        for file in files {
            let Some(package) = file.package.as_deref() else {
                continue;
            };
            index.packages.insert(package.to_string());
            index
                .files_by_package
                .entry(package.to_string())
                .or_default()
                .push(file.path.clone());
            for declaration in &file.declarations {
                index
                    .names_by_package
                    .entry(package.to_string())
                    .or_default()
                    .insert(declaration.name.clone());
                index
                    .files_by_symbol
                    .entry((package.to_string(), declaration.name.clone()))
                    .or_default()
                    .push(file.path.clone());
            }
        }
        for paths in index.files_by_package.values_mut() {
            paths.sort();
            paths.dedup();
        }
        for paths in index.files_by_symbol.values_mut() {
            paths.sort();
            paths.dedup();
        }
        index
    }

    fn resolve(&self, import: &str, level: DepLevel) -> Resolution {
        let Some((package, name)) = import.rsplit_once('.') else {
            return Resolution::External;
        };
        let wildcard = name == "*";
        let internal = if wildcard {
            self.packages.contains(package)
        } else {
            self.names_by_package
                .get(package)
                .is_some_and(|names| names.contains(name))
        };
        if !internal {
            return Resolution::External;
        }
        match level {
            DepLevel::Package => Resolution::Internal(vec![package.to_string()]),
            DepLevel::File => Resolution::Internal(self.target_files(package, name, wildcard)),
        }
    }

    fn target_files(&self, package: &str, name: &str, wildcard: bool) -> Vec<String> {
        if wildcard {
            self.files_by_package
                .get(package)
                .cloned()
                .unwrap_or_default()
        } else {
            self.files_by_symbol
                .get(&(package.to_string(), name.to_string()))
                .cloned()
                .unwrap_or_default()
        }
    }
}
