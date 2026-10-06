//! Pure domain layer for ktsense.
//!
//! This crate holds the model, the compressors, the ranking and the budgeting logic. It must never
//! depend on the filesystem, a process, a runtime, or a parser: adapters convert the outside world
//! into these types, and the CLI and MCP front-ends render them. Keeping the rule makes every
//! algorithm here testable from hand-built values.

#![forbid(unsafe_code)]

pub mod annotated;
pub mod budget;
pub mod context;
pub mod imports;
pub mod java_text;
pub mod pick;
pub mod rank;
pub mod references;
pub mod render;
pub mod repo_map;
pub mod scc;
pub mod skeleton;
pub mod symbol_search;
pub mod text;
pub mod text_refs;
pub mod text_search;
pub mod trace;

pub use annotated::{
    build_annotated, AnnotatedDeclaration, AnnotatedDeclarations, AnnotatedGroup, AnnotatedMember,
    ANNOTATION_PRECISION,
};
pub use budget::{emit_within_budget, BudgetedEmission};
pub use context::{
    build_context, ContextInput, ContextSection, ContextSections, ForeignReference, LineMatcher,
    MatchedLine, MatchedSource, SourceMatch, SourceSection, SymbolContext,
};
pub use imports::{build_import_graph, DepEdge, DepLevel, ExternalImport, ImportGraph};
pub use java_text::classify_java_sites;
pub use pick::{last_segment, match_pick, shortest_unique_suffix, PickMatch};
pub use rank::{page_rank, Graph, PageRankOptions, RankedNode};
pub use references::{
    declaration_starting_at, fully_qualified_enclosing, group_references, is_test_source,
    EnclosingDeclaration, GroupingOptions, Location, QualifiedEnclosing, Reference, ReferenceGroup,
    SiteKind,
};
pub use render::{
    render_annotated_markdown, render_context_markdown, render_deps_dot, render_deps_markdown,
    render_map_markdown, render_markdown, render_member_summary, render_skeleton,
    render_text_references_markdown, render_text_references_titled, render_text_search_markdown,
    render_trace_markdown, RenderOptions,
};
pub use repo_map::{
    build_repo_map, FocusMatches, FocusSpec, MappedFile, NameMatcher, OmittedDirectory,
    ReferenceCounts, RepoMap, RepoMapInput,
};
pub use scc::{cycles, strongly_connected_components};
pub use skeleton::{
    DeclKind, Declaration, FileSkeleton, Modifier, NamedProperty, Parameter, ParameterProperty,
    Visibility, MAX_NESTING_DEPTH,
};
pub use symbol_search::{contained_declarations, SymbolMatch};
pub use text::{fence_for, neutralize, normalize_signature_layout, MIN_FENCE_BACKTICKS};
pub use text_refs::{
    build_text_references, build_text_references_attributed, TextReferenceGroup, TextReferenceSite,
    TextReferences, TEXT_MATCH_PRECISION,
};
pub use text_search::{
    build_text_search, TextSearch, TextSearchDeclaration, TextSearchFile, TextSearchHit,
    TextSearchLine,
};
pub use trace::{
    build_trace, callers_of, CallerLevel, Definition, ExcludedSite, IndexCompleteness,
    RelatedDeclaration, TraceInput, TraceReport,
};

/// Estimates the token cost of rendered output so budgeted commands can stop in time.
///
/// The default implementation is a byte-ratio approximation rather than a real tokenizer, so a
/// front-end that wants exactness can supply its own without changing any caller.
pub trait TokenEstimator {
    fn estimate(&self, text: &str) -> usize;
}

/// Byte-ratio estimator tuned for Kotlin-like source text.
#[derive(Debug, Clone, Copy, Default)]
pub struct ByteRatioEstimator;

const BYTES_PER_TOKEN: f64 = 3.6;

impl TokenEstimator for ByteRatioEstimator {
    fn estimate(&self, text: &str) -> usize {
        (text.len() as f64 / BYTES_PER_TOKEN).ceil() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_costs_nothing() {
        assert_eq!(ByteRatioEstimator.estimate(""), 0);
    }

    #[test]
    fn estimate_rounds_up_so_a_budget_is_never_understated() {
        assert_eq!(ByteRatioEstimator.estimate("fun main()"), 3);
    }
}
