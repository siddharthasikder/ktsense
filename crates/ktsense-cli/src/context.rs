//! The `context` command: one symbol's declaration, the outline of its file, its callers and its
//! implementors, trimmed to a token budget in that order.
//!
//! The engine work is exactly a depth-1 `trace`, so this command drives [`crate::trace::resolve`]
//! rather than opening its own session: callers come from `references` plus the enclosing
//! declaration of each site, which `ktsense-core` derives in one place because `kmp-lsp` 0.26.0
//! advertises no `callHierarchyProvider`. Packing is [`ktsense_core::build_context`], which spends
//! the budget through the crate's one budgeted emitter.
//!
//! The answer carries the same `index: partial|complete` marker a trace does. A bundle built while
//! the index was still building lists fewer callers than exist, and a reader who acts on it as
//! though it were complete is being misled.
//!
//! `context` is not a routed wire command, and the trace it drives is the in-process resolver above:
//! a fresh engine session, and a command-mode `find` to resolve the name, even while a daemon holds a
//! warm session for the same root. `outline`, `deps`, `map` and `trace` do route, so the gap is that
//! `context` inherits the cold half of `trace` rather than that nothing routes at all. A known gap
//! rather than an oversight.

use std::path::Path;

use ktsense_core::{
    build_context, render_context_markdown, ByteRatioEstimator, ContextInput, SymbolContext,
};

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
    let traced = trace::resolve(&TraceRequest {
        root: request.root,
        symbol: request.symbol,
        pick: request.pick,
        depth: DIRECT_CALLERS_ONLY,
        limit: None,
        wait: IndexWaitPolicy::Capped,
        format: request.format,
    })?;
    let report = match traced {
        Traced::Ambiguous(outcome) => return Ok(outcome),
        Traced::Resolved(report) => report,
    };

    let file = trace::skeleton_at(request.root, &report.definition.path);
    let bundle = build_context(
        ContextInput {
            definition: report.definition.clone(),
            index: report.index,
            file: file.as_ref(),
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
        Format::Json => serde_json::to_string_pretty(bundle)
            .map(|json| format!("{json}\n"))
            .map_err(CommandError::serialization),
        Format::Dot => Err(CommandError::unsupported_format("context")),
    }
}
