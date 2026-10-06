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
    build_context, render_context_markdown, ByteRatioEstimator, ContextInput, SymbolContext,
};
use ktsense_daemon::WarmEngine;

use crate::trace::{self, IndexWaitPolicy, TraceRequest, Traced};
use crate::{CommandError, CommandOutcome, Format};

/// How many levels of callers a bundle carries. Direct callers are what a reader needs to know who
/// depends on the symbol; deeper levels are `trace --depth`'s job and would spend the budget on
/// breadth the card does not ask for.
const DIRECT_CALLERS_ONLY: usize = 1;

pub(crate) struct ContextRequest<'a> {
    pub root: &'a Path,
    pub symbol: &'a str,
    pub pick: Option<&'a str>,
    pub budget: usize,
    pub format: Format,
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
    }
}

/// Packs a resolved trace into the budgeted bundle, or returns the ambiguous candidate listing
/// unchanged. The file outline and the declaration's own source are read here from plain files, so
/// both the fresh and warm paths compose the bundle from the same data.
fn finish(request: &ContextRequest<'_>, traced: Traced) -> Result<CommandOutcome, CommandError> {
    let report = match traced {
        Traced::Ambiguous(outcome) => return Ok(outcome),
        Traced::NotFound => {
            return crate::text_refs::not_found_outcome(
                request.root,
                request.symbol,
                None,
                request.format,
                "context",
            )
        }
        Traced::Resolved(report) => report,
    };

    let file = trace::skeleton_at(request.root, &report.definition.path);
    let source = trace::source_lines_at(request.root, &report.definition.path);
    let bundle = build_context(
        ContextInput {
            definition: report.definition.clone(),
            index: report.index,
            file: file.as_ref(),
            source: source.as_deref(),
            callers: report.direct_callers(),
            implementors: &report.implementors,
            budget: request.budget,
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
