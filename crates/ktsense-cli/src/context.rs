//! The `context` command: one symbol's declaration, the outline of its file, its callers and its
//! implementors, trimmed to a token budget in that order.
//!
//! The engine work is exactly a depth-1 `trace`, so this command drives the trace resolvers rather
//! than opening its own: [`crate::trace::resolve`] in process and [`crate::trace::resolve_warm`] on a
//! daemon's warm session. Callers come from `references` plus the enclosing declaration of each site,
//! which `ktsense-core` derives in one place because `kmp-lsp` 0.26.0 advertises no
//! `callHierarchyProvider`. Packing is [`ktsense_core::build_context`], which spends the budget
//! through the crate's one budgeted emitter.
//!
//! The answer carries the same `index: partial|complete` marker a trace does. A bundle built while
//! the index was still building lists fewer callers than exist, and a reader who acts on it as
//! though it were complete is being misled.
//!
//! `context` is a routed wire command (KT-89). Like `trace`, a live daemon answers it on its own warm
//! session and the client falls back to the in-process path when no daemon is live, when
//! `KTSENSE_NO_DAEMON=1` is set, or when the transport fails. Both paths reach the same [`finish`],
//! which packs the bundle with `ktsense-core`, so a routed answer equals the in-process one by
//! construction. What differs between them is only how the trace it builds on is resolved: the
//! in-process path opens a fresh engine session and resolves through a command-mode `find`, while the
//! routed path resolves and traces on the daemon's warm session exactly as a routed `trace` does.

use std::path::Path;

use ktsense_core::{
    build_context, render_context_markdown, AppliedContextFilter, ByteRatioEstimator, ContextInput,
    LineMatcher, SiteFilter, SourceMatch as CoreSourceMatch, SymbolContext,
};
use ktsense_daemon::WarmEngine;
use regex::Regex;

use crate::trace::{self, IndexWaitPolicy, TraceRequest, Traced};
use crate::{CommandError, CommandOutcome, Format};

/// A compiled `--match` request owned by the context request. The regex lives here, in the CLI,
/// because `ktsense-core` holds no regex engine; core is lent a [`LineMatcher`] over it, the same
/// way it is lent a token estimator.
pub(crate) struct SourceMatch {
    matcher: RegexMatcher,
    around: usize,
}

struct RegexMatcher(Regex);

impl LineMatcher for RegexMatcher {
    fn matches(&self, line: &str) -> bool {
        self.0.is_match(line)
    }
}

impl SourceMatch {
    /// Compiles the `--match` pattern, reporting an invalid regex as a failed input rather than a
    /// panic. `around` is the number of context lines to keep on each side of a hit.
    pub(crate) fn compile(pattern: &str, around: usize) -> Result<Self, CommandError> {
        let matcher = RegexMatcher(Regex::new(pattern).map_err(CommandError::bad_match_pattern)?);
        Ok(Self { matcher, around })
    }

    fn as_core(&self) -> CoreSourceMatch<'_> {
        CoreSourceMatch {
            matcher: &self.matcher,
            around: self.around,
        }
    }
}

/// How many levels of callers a bundle carries. Direct callers are what a reader needs to know who
/// depends on the symbol; deeper levels are `trace --depth`'s job and would spend the budget on
/// breadth the card does not ask for.
const DIRECT_CALLERS_ONLY: usize = 1;

pub(crate) struct ContextRequest<'a> {
    pub root: &'a Path,
    pub symbol: &'a str,
    pub pick: Option<&'a str>,
    pub budget: usize,
    pub sections: ktsense_core::ContextSections,
    pub source_match: Option<SourceMatch>,
    pub format: Format,
    /// The `--path`/`--tests` filter narrowing the callers, annotated and text-reference sections
    /// (KT-127). Passed to the trace it is built on so the callers are filtered there.
    pub filter: Option<SiteFilter>,
}

pub(crate) fn context(request: ContextRequest<'_>) -> Result<CommandOutcome, CommandError> {
    finish(&request, trace::resolve(&trace_request(&request))?)
}

/// The daemon-side `context`: resolves and traces on the daemon's own warm session, exactly as a
/// routed `trace` does, then packs the bundle through the same [`finish`] the in-process path runs,
/// so a routed answer equals the in-process one by construction.
pub(crate) async fn context_warm(
    engine: &WarmEngine,
    request: ContextRequest<'_>,
) -> Result<CommandOutcome, CommandError> {
    let traced = trace::resolve_warm(engine, &trace_request(&request)).await?;
    finish(&request, traced)
}

/// The depth-1 trace a `context` bundle is built on. Stated once so the in-process and warm paths
/// trace the same thing and cannot drift.
fn trace_request<'a>(request: &ContextRequest<'a>) -> TraceRequest<'a> {
    TraceRequest {
        root: request.root,
        symbol: request.symbol,
        pick: request.pick,
        depth: DIRECT_CALLERS_ONLY,
        limit: None,
        wait: IndexWaitPolicy::Capped,
        format: request.format,
        filter: request.filter.clone(),
    }
}

/// Packs a resolved trace into the budgeted bundle, or returns the ambiguous candidate listing
/// unchanged. The file outline and the declaration's own source are read here from plain files, so
/// both the fresh and warm paths compose the bundle from the same data.
fn finish(request: &ContextRequest<'_>, traced: Traced) -> Result<CommandOutcome, CommandError> {
    let filter = request.filter.as_ref();
    let report = match traced {
        Traced::Ambiguous(outcome) => return Ok(outcome),
        Traced::NotFound => {
            return crate::text_refs::not_found_outcome(
                request.root,
                ktsense_core::last_segment(request.symbol),
                None,
                request.format,
                "context",
                filter,
            )
        }
        Traced::Resolved(report) => *report,
    };

    let file = trace::skeleton_at(request.root, &report.definition.path);
    let source = trace::source_lines_at(request.root, &report.definition.path);
    let (annotated, annotated_omitted) =
        trace::annotation_class_uses(request.root, &report.definition, filter)?;
    let name = ktsense_core::last_segment(request.symbol);
    let (java_text_references, java_omitted) =
        crate::text_refs::java_foreign_references(request.root, name, filter)?;
    let (kotlin_text_references, kotlin_omitted) = if report.definition.path.ends_with(".java") {
        crate::text_refs::kotlin_foreign_references(request.root, name, filter)?
    } else {
        (Vec::new(), 0)
    };
    let callers_omitted = report
        .callers
        .first()
        .map_or(0, |level| level.production_omitted + level.test_omitted);
    let applied_filter = request.filter.clone().map(|filter| AppliedContextFilter {
        filter,
        callers_omitted,
        annotated_omitted,
        java_text_references_omitted: java_omitted,
        kotlin_text_references_omitted: kotlin_omitted,
    });
    let bundle = build_context(
        ContextInput {
            definition: report.definition.clone(),
            index: report.index,
            sections: request.sections,
            file: file.as_ref(),
            source: source.as_deref(),
            source_match: request.source_match.as_ref().map(SourceMatch::as_core),
            callers: report.direct_callers(),
            annotated: &annotated,
            java_text_references: &java_text_references,
            kotlin_text_references: &kotlin_text_references,
            implementors: &report.implementors,
            budget: request.budget,
            filter: applied_filter,
        },
        &ByteRatioEstimator,
    );
    present(&bundle, request.format).map(CommandOutcome::success)
}

fn present(bundle: &SymbolContext, format: Format) -> Result<String, CommandError> {
    match format {
        Format::Md => Ok(render_context_markdown(bundle)),
        Format::Json => crate::as_json(bundle),
        Format::Dot => Err(CommandError::unsupported_format("context")),
    }
}
