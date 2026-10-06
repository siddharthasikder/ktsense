//! The `symbols` command: resolve a name to declarations and present them for an agent.
//!
//! The engine's `find --json` answers with a bare location per match: name, file, line, column,
//! and nothing else. The card's `fqn  kind  file:line  signature` row needs three of those columns
//! the engine does not supply, so each location is enriched here from the file it points at, the
//! same skeleton the `outline` command already builds. Enrichment is syntactic, so it degrades
//! honestly: a location that no declaration in the file matches (a Java symbol, a line the parser
//! and the engine disagree on) keeps the engine's bare name rather than inventing a kind or a
//! signature.
//!
//! Ambiguity is the whole point of the command. When a name resolves to several declarations the
//! rows are all printed and the process exits [`Exit::Ambiguous`], unless `--pick <fqn>` selects
//! one. Resolution itself lives in `ktsense-lsp`; this module owns only the enrichment, the
//! filtering, and the presentation, so `trace` (KT-18) reuses the resolver without inheriting any
//! of this policy.

use std::path::Path;

use ktsense_core::{
    contained_declarations, match_pick, render_skeleton, shortest_unique_suffix, DeclKind,
    Declaration, FileSkeleton, PickMatch, RenderOptions, SymbolMatch,
};
use ktsense_lsp::SymbolCandidate;
use serde::Serialize;

use crate::{neutralize, normalized_path, CommandError, CommandOutcome, Exit, Format, KindFilter};

/// One resolved declaration ready to render: the engine location enriched with what the file
/// skeleton knows about the declaration at that point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ResolvedSymbol {
    pub(crate) fqn: String,
    pub(crate) kind: String,
    pub(crate) file: String,
    pub(crate) line: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) signature: String,
    /// True for a declaration that lives inside a function or property body, which the file
    /// skeleton drops. Rendered as a `(local)` marker and, being defaulted-false and skipped when
    /// false, absent from the JSON of every top-level or member declaration.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) local: bool,
}

impl ResolvedSymbol {
    fn matches_kind(&self, filter: KindFilter) -> bool {
        self.kind == filter.label()
    }
}

/// The rendered answer and the exit status it ends with, kept together so the caller never infers
/// an exit from the text. An ambiguity carries a `--pick` hint for stderr, so a caller reading only
/// stdout is not the sole audience for how to resolve the name.
pub(crate) struct SymbolsOutcome {
    text: String,
    exit: Exit,
    stderr: Option<String>,
}

impl SymbolsOutcome {
    fn with_stderr(mut self, stderr: String) -> Self {
        self.stderr = Some(stderr);
        self
    }
}

impl From<SymbolsOutcome> for CommandOutcome {
    fn from(outcome: SymbolsOutcome) -> Self {
        CommandOutcome {
            text: outcome.text,
            exit: outcome.exit,
            stderr: outcome.stderr,
        }
    }
}

/// Enriches, filters, picks, and renders engine candidates into the command's answer. Pure over
/// its inputs except for reading each candidate's file, which enrichment needs and which is the
/// same read `outline` performs.
pub(crate) fn present_symbols(
    root: &Path,
    query: &str,
    candidates: Vec<SymbolCandidate>,
    kind: Option<KindFilter>,
    limit: Option<usize>,
    filter: PickFilter,
    format: Format,
) -> Result<SymbolsOutcome, CommandError> {
    let mut resolved: Vec<ResolvedSymbol> = enrich_sorted(root, candidates)
        .into_iter()
        .map(|(_, resolved)| resolved)
        .collect();
    if let Some(kind) = kind {
        resolved.retain(|symbol| symbol.matches_kind(kind));
    }

    match filter {
        PickFilter::Explicit(pick) => pick_present(query, resolved, pick, format, || {
            Err(CommandError::pick_missed(pick))
        }),
        PickFilter::Dotted(pick) => pick_present(query, resolved, pick, format, || {
            Err(CommandError::no_symbol(query))
        }),
        PickFilter::None => match resolved.len() {
            0 => Err(CommandError::no_symbol(query)),
            1 => render(query, &resolved, None, Exit::Success, format),
            _ => {
                let total = resolved.len();
                let hint = ambiguity_hint(query, &resolved);
                let shown = limit.unwrap_or(total).min(total);
                let hidden = total - shown;
                resolved.truncate(shown);
                render(
                    query,
                    &resolved,
                    hidden_note(hidden),
                    Exit::Ambiguous,
                    format,
                )
                .map(|outcome| outcome.with_stderr(hint))
            }
        },
    }
}

/// How a follow-up narrows engine candidates to one declaration. The three cases differ only when
/// the filter matches no candidate: an explicit `--pick` miss is the caller's mistake and errors,
/// while a dotted `Type.member` query that matches nothing is a name the workspace does not declare
/// and takes the not-found answer, as if the lookup had found nothing (KT-100). The lookup itself
/// uses only the query's last segment; the whole dotted query is the filter.
pub(crate) enum PickFilter<'a> {
    None,
    Dotted(&'a str),
    Explicit(&'a str),
}

impl<'a> PickFilter<'a> {
    /// The filter for `query` given an optional explicit `--pick`: the explicit value wins over the
    /// dotted filter, else a dotted query filters by its whole self, else nothing filters.
    pub(crate) fn for_query(query: &'a str, pick: Option<&'a str>) -> Self {
        match pick {
            Some(pick) => PickFilter::Explicit(pick),
            None if query.contains('.') => PickFilter::Dotted(query),
            None => PickFilter::None,
        }
    }
}

/// What resolving a name to exactly one declaration produced: the one, or the answer to print when
/// there is not exactly one. Shared by `trace`, so the ambiguity contract (list every candidate,
/// exit 3 unless `--pick`) is stated once, here.
pub(crate) enum Selection {
    One(SymbolCandidate, ResolvedSymbol),
    Ambiguous(SymbolsOutcome),
    /// The filter matched no candidate, or nothing resolved at all: a name the workspace does not
    /// declare, which `trace` and `context` answer by listing its text references rather than
    /// failing (KT-94/KT-100).
    NotFound,
}

/// Narrows engine candidates to the single declaration a follow-up command should act on.
pub(crate) fn select(
    root: &Path,
    query: &str,
    candidates: Vec<SymbolCandidate>,
    filter: PickFilter,
    format: Format,
) -> Result<Selection, CommandError> {
    let enriched: Vec<(SymbolCandidate, ResolvedSymbol)> = enrich_sorted(root, candidates);
    match filter {
        PickFilter::Explicit(pick) => select_by_pick(query, enriched, pick, format, || {
            Err(CommandError::pick_missed(pick))
        }),
        PickFilter::Dotted(pick) => {
            select_by_pick(query, enriched, pick, format, || Ok(Selection::NotFound))
        }
        PickFilter::None => match enriched.len() {
            0 => Ok(Selection::NotFound),
            1 => {
                let mut enriched = enriched;
                let (candidate, resolved) = enriched.remove(0);
                Ok(Selection::One(candidate, resolved))
            }
            _ => {
                let resolved: Vec<ResolvedSymbol> =
                    enriched.into_iter().map(|(_, resolved)| resolved).collect();
                render_ambiguous_selection(query, &resolved, format).map(Selection::Ambiguous)
            }
        },
    }
}

/// Applies a pick value to the enriched candidates, selecting the one it matches or listing the
/// several it matches. `on_miss` decides what matching nothing means, which is the only thing that
/// differs between an explicit `--pick` (the caller's error) and a dotted query (not found).
fn select_by_pick(
    query: &str,
    mut enriched: Vec<(SymbolCandidate, ResolvedSymbol)>,
    pick: &str,
    format: Format,
    on_miss: impl FnOnce() -> Result<Selection, CommandError>,
) -> Result<Selection, CommandError> {
    match match_pick(pick, &fqns_of(&enriched)) {
        PickMatch::Selected(index) => {
            let (candidate, resolved) = enriched.swap_remove(index);
            Ok(Selection::One(candidate, resolved))
        }
        PickMatch::Ambiguous(indices) => {
            let subset: Vec<ResolvedSymbol> = retain_indices(enriched, &indices)
                .into_iter()
                .map(|(_, resolved)| resolved)
                .collect();
            render_ambiguous_selection(query, &subset, format).map(Selection::Ambiguous)
        }
        PickMatch::Missed => on_miss(),
    }
}

fn pick_present(
    query: &str,
    resolved: Vec<ResolvedSymbol>,
    pick: &str,
    format: Format,
    on_miss: impl FnOnce() -> Result<SymbolsOutcome, CommandError>,
) -> Result<SymbolsOutcome, CommandError> {
    let fqns: Vec<&str> = resolved.iter().map(|symbol| symbol.fqn.as_str()).collect();
    match match_pick(pick, &fqns) {
        PickMatch::Selected(index) => {
            let chosen = resolved
                .into_iter()
                .nth(index)
                .expect("index within candidates");
            render(query, &[chosen], None, Exit::Success, format)
        }
        PickMatch::Ambiguous(indices) => {
            let subset = retain_indices(resolved, &indices);
            render_ambiguous_selection(query, &subset, format)
        }
        PickMatch::Missed => on_miss(),
    }
}

/// The fully-qualified names of enriched candidates, in their enriched order, for matching a
/// `--pick` value without borrowing the richer candidate values it indexes into.
fn fqns_of(enriched: &[(SymbolCandidate, ResolvedSymbol)]) -> Vec<&str> {
    enriched
        .iter()
        .map(|(_, resolved)| resolved.fqn.as_str())
        .collect()
}

/// Keeps the items at `indices`, in their original order, dropping the rest. The indices come from
/// [`match_pick`] over the same list, so they are ascending and in range.
fn retain_indices<T>(items: Vec<T>, indices: &[usize]) -> Vec<T> {
    items
        .into_iter()
        .enumerate()
        .filter(|(index, _)| indices.contains(index))
        .map(|(_, item)| item)
        .collect()
}

/// The default cap on a `--contains` listing: a partial-name search can match a great many
/// declarations, so a bound keeps the answer readable when the caller gives no `--limit`.
const DEFAULT_CONTAINS_LIMIT: usize = 50;

/// Lists declarations whose simple name contains `query`, ranked by [`contained_declarations`]. The
/// candidates come from the workspace's own syntax skeletons, not the engine, so the answer states
/// `source: syntax index` and never depends on the index being warm. A listing is the whole answer,
/// so this exits successfully however many it found; only a query nothing contains is a failure.
pub(crate) fn present_contained(
    root: &Path,
    query: &str,
    kind: Option<KindFilter>,
    limit: Option<usize>,
    format: Format,
) -> Result<SymbolsOutcome, CommandError> {
    let mut ranked = contained_declarations(query, gather_workspace_declarations(root)?);
    if let Some(kind) = kind {
        ranked.retain(|found| kind_label(found.kind) == kind.label());
    }
    if ranked.is_empty() {
        return Err(CommandError::no_symbol(query));
    }

    let total = ranked.len();
    let shown = limit.unwrap_or(DEFAULT_CONTAINS_LIMIT).min(total);
    ranked.truncate(shown);
    let resolved: Vec<ResolvedSymbol> = ranked.into_iter().map(resolved_from_match).collect();

    let text = match format {
        Format::Md => contains_markdown(query, &resolved, total - shown),
        Format::Json => contains_json(&resolved, total - shown)?,
        Format::Dot => return Err(CommandError::unsupported_format("symbols")),
    };
    Ok(SymbolsOutcome {
        text,
        exit: Exit::Success,
        stderr: None,
    })
}

fn resolved_from_match(found: SymbolMatch) -> ResolvedSymbol {
    ResolvedSymbol {
        fqn: found.qualified_name,
        kind: kind_label(found.kind).to_string(),
        file: found.path,
        line: found.line,
        signature: found.signature,
        local: false,
    }
}

/// Every named declaration in the workspace's Kotlin sources, as [`SymbolMatch`] values ready to
/// rank. It walks the same tree `map` does, so build, target and bin copies are skipped; an
/// unreadable or unparseable file is dropped rather than aborting the search.
fn gather_workspace_declarations(root: &Path) -> Result<Vec<SymbolMatch>, CommandError> {
    let mut matches = Vec::new();
    for path in crate::collect_kotlin_files(root)? {
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let display = normalized_path(root, &path);
        let Ok(skeleton) = ktsense_syntax::extract(display.clone(), &source) else {
            continue;
        };
        let mut ancestors = Vec::new();
        gather_declarations(
            skeleton.package.as_deref(),
            &display,
            &skeleton.declarations,
            &mut ancestors,
            &mut matches,
        );
    }
    Ok(matches)
}

fn gather_declarations(
    package: Option<&str>,
    path: &str,
    declarations: &[Declaration],
    ancestors: &mut Vec<String>,
    out: &mut Vec<SymbolMatch>,
) {
    for declaration in declarations {
        if !declaration.name.is_empty() {
            out.push(SymbolMatch {
                qualified_name: fqn(package, ancestors, &declaration.name),
                simple_name: declaration.name.clone(),
                kind: declaration.kind,
                path: path.to_string(),
                line: declaration.line,
                signature: signature_of(path, declaration),
            });
        }
        ancestors.push(declaration.name.clone());
        gather_declarations(package, path, &declaration.children, ancestors, out);
        ancestors.pop();
    }
}

fn contains_markdown(query: &str, resolved: &[ResolvedSymbol], hidden: usize) -> String {
    let mut out = format!("## Symbols: {}\n", neutralize(query));
    out.push_str("source: syntax index\n");
    out.push('\n');
    out.push_str(&crate::fenced_block(&rows(resolved)));
    if hidden > 0 {
        out.push_str(&format!("\n... {hidden} more matches not shown\n"));
    }
    out
}

fn contains_json(resolved: &[ResolvedSymbol], hidden: usize) -> Result<String, CommandError> {
    #[derive(serde::Serialize)]
    struct ContainsAnswer<'a> {
        source: &'static str,
        matches: &'a [ResolvedSymbol],
        more_matches: usize,
    }
    crate::as_json(&ContainsAnswer {
        source: "syntax index",
        matches: resolved,
        more_matches: hidden,
    })
}

fn hidden_note(hidden: usize) -> Option<usize> {
    (hidden > 0).then_some(hidden)
}

fn render(
    query: &str,
    resolved: &[ResolvedSymbol],
    hidden: Option<usize>,
    exit: Exit,
    format: Format,
) -> Result<SymbolsOutcome, CommandError> {
    let text = match format {
        Format::Md => symbols_markdown(query, resolved, hidden),
        Format::Json => symbols_json(resolved)?,
        Format::Dot => return Err(CommandError::unsupported_format("symbols")),
    };
    Ok(SymbolsOutcome {
        text,
        exit,
        stderr: None,
    })
}

/// The answer for a name that resolved to several declarations on the `trace`/`context` path: the
/// candidate list under an `## Ambiguous:` heading, ending with the `--pick` hint that same line
/// carries on stderr, so an agent reading the list is told outright it is not the answer and how to
/// get one. The `symbols` command keeps its plain `## Symbols:` listing and carries the hint on
/// stderr alone, because a listing is what it was asked for.
fn render_ambiguous_selection(
    query: &str,
    resolved: &[ResolvedSymbol],
    format: Format,
) -> Result<SymbolsOutcome, CommandError> {
    let hint = ambiguity_hint(query, resolved);
    let text = match format {
        Format::Md => ambiguous_markdown(query, resolved, &hint),
        Format::Json => symbols_json(resolved)?,
        Format::Dot => return Err(CommandError::unsupported_format("symbols")),
    };
    Ok(SymbolsOutcome {
        text,
        exit: Exit::Ambiguous,
        stderr: Some(hint),
    })
}

/// The line that tells the caller how to turn an ambiguous name into an answer: how many
/// declarations carry it and the shortest dot-boundary suffix of the first after the deterministic
/// sort that names it alone, so a `--pick` reproduces without the caller copying a whole package
/// path from the list.
fn ambiguity_hint(query: &str, resolved: &[ResolvedSymbol]) -> String {
    let fqns: Vec<&str> = resolved.iter().map(|symbol| symbol.fqn.as_str()).collect();
    let suffix = shortest_unique_suffix(&resolved[0].fqn, &fqns);
    let count = resolved.len();
    neutralize(&format!(
        "ambiguous: {count} declarations named {query}; rerun with --pick {suffix}"
    ))
    .into_owned()
}

/// Enriches every engine candidate and orders the result deterministically, so a set of
/// declarations renders identically however the engine ordered its answer. The engine's `find`
/// order is not stable between runs (KT-81); ordering by fully-qualified name, then file, line and
/// column, is. This is the one place both `symbols` and the `trace`/`context` ambiguity listing
/// order candidates, so every consumer sees the same rows in the same order.
fn enrich_sorted(
    root: &Path,
    candidates: Vec<SymbolCandidate>,
) -> Vec<(SymbolCandidate, ResolvedSymbol)> {
    let mut enriched: Vec<(SymbolCandidate, ResolvedSymbol)> = candidates
        .into_iter()
        .map(|candidate| {
            let resolved = enrich(root, &candidate);
            (candidate, resolved)
        })
        .collect();
    enriched.sort_by(|(left_candidate, left), (right_candidate, right)| {
        (&left.fqn, &left.file, left.line, left_candidate.col).cmp(&(
            &right.fqn,
            &right.file,
            right.line,
            right_candidate.col,
        ))
    });
    enriched
}

/// Enriches one engine location from the declaration at that point in its file. A file that cannot
/// be read or parsed, or that holds no declaration matching the location, yields the bare name so
/// the row is still honest rather than absent.
fn enrich(root: &Path, candidate: &SymbolCandidate) -> ResolvedSymbol {
    let display = normalized_path(root, Path::new(&candidate.file));
    let bare = || ResolvedSymbol {
        fqn: candidate.name.clone(),
        kind: "symbol".to_string(),
        file: display.clone(),
        line: candidate.line,
        signature: String::new(),
        local: false,
    };
    let Ok(source) = std::fs::read_to_string(&candidate.file) else {
        return bare();
    };
    let Ok(skeleton) = ktsense_syntax::extract(display.clone(), &source) else {
        return bare();
    };
    match locate(&skeleton, &candidate.name, candidate.line) {
        Some(found) => ResolvedSymbol {
            fqn: fqn(
                skeleton.package.as_deref(),
                &found.ancestors,
                &found.declaration.name,
            ),
            kind: kind_label(found.declaration.kind).to_string(),
            signature: signature_of(&display, found.declaration),
            file: display,
            line: candidate.line,
            local: false,
        },
        None => local_declaration(&display, &source, candidate).unwrap_or_else(bare),
    }
}

/// Enriches a location the file skeleton has no declaration for by looking inside function and
/// property bodies, which the skeleton drops. A hit qualifies the name through its enclosing
/// declarations and marks it local, so a function-local class carries its kind, package and chain
/// rather than the bare name the engine gave. A miss returns `None` so the caller stays bare.
fn local_declaration(
    display: &str,
    source: &str,
    candidate: &SymbolCandidate,
) -> Option<ResolvedSymbol> {
    let local = ktsense_syntax::locate_local(source, &candidate.name, candidate.line)?;
    Some(ResolvedSymbol {
        fqn: fqn(
            local.package.as_deref(),
            &local.ancestors,
            &local.declaration.name,
        ),
        kind: kind_label(local.declaration.kind).to_string(),
        signature: signature_of(display, &local.declaration),
        file: display.to_string(),
        line: candidate.line,
        local: true,
    })
}

/// A declaration found in a skeleton together with the names of the containers enclosing it, so a
/// fully-qualified name can be composed from the outside in.
struct Located<'a> {
    declaration: &'a Declaration,
    ancestors: Vec<String>,
}

/// Finds the declaration a location points at, preferring an exact line match and falling back to
/// the first same-named declaration when the engine and the parser disagree on the line by a hair.
fn locate<'a>(skeleton: &'a FileSkeleton, name: &str, line: u32) -> Option<Located<'a>> {
    let mut fallback: Option<Located<'a>> = None;
    let mut ancestors = Vec::new();
    if walk(
        &skeleton.declarations,
        name,
        line,
        &mut ancestors,
        &mut fallback,
    ) {
        // `walk` returns true only after storing the exact match in `fallback`, so the exact match
        // is whatever `fallback` now holds.
    }
    fallback
}

fn walk<'a>(
    declarations: &'a [Declaration],
    name: &str,
    line: u32,
    ancestors: &mut Vec<String>,
    best: &mut Option<Located<'a>>,
) -> bool {
    for declaration in declarations {
        if declaration.name == name {
            let candidate = Located {
                declaration,
                ancestors: ancestors.clone(),
            };
            if declaration.line == line {
                *best = Some(candidate);
                return true;
            }
            best.get_or_insert(candidate);
        }
        ancestors.push(declaration.name.clone());
        let found = walk(&declaration.children, name, line, ancestors, best);
        ancestors.pop();
        if found {
            return true;
        }
    }
    false
}

fn fqn(package: Option<&str>, ancestors: &[String], name: &str) -> String {
    package
        .into_iter()
        .map(str::to_string)
        .chain(ancestors.iter().filter(|part| !part.is_empty()).cloned())
        .chain(std::iter::once(name.to_string()))
        .collect::<Vec<_>>()
        .join(".")
}

/// The one-line signature of a declaration: its header rendered with bodies and children elided.
/// Rendering a single childless clone reuses the outline renderer, so a symbol's signature reads
/// exactly as it does in `outline`, already neutralized by that renderer.
fn signature_of(path: &str, declaration: &Declaration) -> String {
    let mut header = declaration.clone();
    header.children = Vec::new();
    let skeleton = FileSkeleton::new(path).with_declarations(vec![header]);
    render_skeleton(&skeleton, &RenderOptions::default().with_private())
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn kind_label(kind: DeclKind) -> &'static str {
    match kind {
        DeclKind::Class => "class",
        DeclKind::Interface => "interface",
        DeclKind::Object => "object",
        DeclKind::Function => "fun",
        DeclKind::Val => "val",
        DeclKind::Var => "var",
        DeclKind::TypeAlias => "typealias",
        DeclKind::EnumEntry => "enum-entry",
        DeclKind::Constructor => "constructor",
    }
}

fn symbols_markdown(query: &str, resolved: &[ResolvedSymbol], hidden: Option<usize>) -> String {
    let mut out = format!("## Symbols: {}\n", neutralize(query));
    out.push('\n');
    out.push_str(&crate::fenced_block(&rows(resolved)));
    if let Some(hidden) = hidden {
        out.push_str(&format!("\n... {hidden} more (use --limit)\n"));
    }
    out
}

/// The candidate listing under the `## Ambiguous:` heading the `trace`/`context` path uses, ending
/// with the `--pick` hint so the last line of the block is the same instruction that reaches stderr.
fn ambiguous_markdown(query: &str, resolved: &[ResolvedSymbol], hint: &str) -> String {
    let mut out = format!(
        "## Ambiguous: {} ({} candidates)\n",
        neutralize(query),
        resolved.len()
    );
    out.push('\n');
    out.push_str(&crate::fenced_block(&rows(resolved)));
    out.push_str(&format!("\n{hint}\n"));
    out
}

fn rows(resolved: &[ResolvedSymbol]) -> Vec<String> {
    resolved
        .iter()
        .map(|symbol| {
            let kind = if symbol.local {
                format!("{} (local)", symbol.kind)
            } else {
                symbol.kind.clone()
            };
            format!(
                "{}  {}  {}:{}  {}",
                symbol.fqn, kind, symbol.file, symbol.line, symbol.signature
            )
        })
        .collect()
}

fn symbols_json(resolved: &[ResolvedSymbol]) -> Result<String, CommandError> {
    crate::as_json(&resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
    }

    fn absolute(relative: &str) -> String {
        fixtures().join(relative).to_string_lossy().into_owned()
    }

    fn candidate(name: &str, relative: &str, line: u32, col: u32) -> SymbolCandidate {
        SymbolCandidate {
            name: name.to_string(),
            file: absolute(relative),
            line,
            col,
        }
    }

    fn save_candidates() -> Vec<SymbolCandidate> {
        vec![
            candidate(
                "save",
                "multi-module/core/src/main/kotlin/shop/order/OrderRepository.kt",
                4,
                5,
            ),
            candidate(
                "save",
                "multi-module/db/src/main/kotlin/shop/db/JdbcOrderRepository.kt",
                8,
                14,
            ),
            candidate(
                "save",
                "multi-module/db/src/main/kotlin/shop/db/InMemoryOrderRepository.kt",
                13,
                14,
            ),
        ]
    }

    fn present(
        candidates: Vec<SymbolCandidate>,
        kind: Option<KindFilter>,
        limit: Option<usize>,
        pick: Option<&str>,
    ) -> (u8, String) {
        present_query("save", candidates, kind, limit, pick)
    }

    fn present_query(
        query: &str,
        candidates: Vec<SymbolCandidate>,
        kind: Option<KindFilter>,
        limit: Option<usize>,
        pick: Option<&str>,
    ) -> (u8, String) {
        let outcome = present_symbols(
            &fixtures().join("multi-module"),
            query,
            candidates,
            kind,
            limit,
            PickFilter::for_query(query, pick),
            Format::Md,
        );
        match outcome {
            Ok(SymbolsOutcome { text, exit, .. }) => (exit.code(), text),
            Err(error) => (error.exit.code(), format!("{}\n", error.message)),
        }
    }

    #[test]
    fn a_unique_match_enriches_fqn_kind_and_signature_and_exits_zero() {
        let unique = vec![candidate(
            "OrderRepository",
            "multi-module/core/src/main/kotlin/shop/order/OrderRepository.kt",
            3,
            1,
        )];
        let (code, text) = present(unique, None, None, None);
        insta::assert_snapshot!("unique_match", format!("exit {code}\n{text}"));
    }

    #[test]
    fn several_exact_matches_list_all_and_exit_three() {
        let (code, text) = present(save_candidates(), None, None, None);
        insta::assert_snapshot!("ambiguous_exits_three", format!("exit {code}\n{text}"));
    }

    #[test]
    fn an_ambiguous_listing_keeps_its_symbols_heading_and_writes_the_pick_hint_to_stderr() {
        let outcome = present_symbols(
            &fixtures().join("multi-module"),
            "save",
            save_candidates(),
            None,
            None,
            PickFilter::for_query("save", None),
            Format::Md,
        )
        .expect("ambiguous listing");

        let observed = (
            outcome.exit.code(),
            outcome.text.lines().next().map(str::to_string),
            outcome.stderr,
        );
        assert_eq!(
            observed,
            (
                3,
                Some("## Symbols: save".to_string()),
                Some(
                    "ambiguous: 3 declarations named save; rerun with --pick InMemoryOrderRepository.save"
                        .to_string()
                ),
            )
        );
    }

    #[test]
    fn candidate_order_does_not_change_the_rendering() {
        let forward = present(save_candidates(), None, None, None);
        let mut reversed = save_candidates();
        reversed.reverse();
        let backward = present(reversed, None, None, None);

        assert_eq!(forward, backward);
    }

    #[test]
    fn pick_selects_one_of_several_and_exits_zero() {
        let (code, text) = present(
            save_candidates(),
            None,
            None,
            Some("shop.db.JdbcOrderRepository.save"),
        );
        insta::assert_snapshot!("pick_selects_one", format!("exit {code}\n{text}"));
    }

    #[test]
    fn a_pick_that_matches_no_candidate_fails() {
        let (code, text) = present(save_candidates(), None, None, Some("shop.nope.save"));
        insta::assert_snapshot!("pick_missed", format!("exit {code}\n{text}"));
    }

    #[test]
    fn a_unique_suffix_picks_the_same_declaration_as_the_full_fqn() {
        let by_suffix = present(
            save_candidates(),
            None,
            None,
            Some("JdbcOrderRepository.save"),
        );
        let by_fqn = present(
            save_candidates(),
            None,
            None,
            Some("shop.db.JdbcOrderRepository.save"),
        );
        assert_eq!(by_suffix, by_fqn);
    }

    #[test]
    fn a_shared_suffix_lists_the_matches_under_the_ambiguous_heading_and_exits_three() {
        let (code, text) = present(save_candidates(), None, None, Some("save"));
        let observed = (
            code,
            text.lines().next().map(str::to_string),
            text.lines().filter(|line| line.contains("  fun  ")).count(),
        );
        assert_eq!(
            observed,
            (3, Some("## Ambiguous: save (3 candidates)".to_string()), 3)
        );
    }

    #[test]
    fn a_mid_identifier_suffix_is_not_a_match_and_keeps_the_pick_missed_error() {
        let (code, text) = present(save_candidates(), None, None, Some("Repository.save"));
        assert_eq!(
            (code, text),
            (
                1,
                "ktsense: no candidate has the fully-qualified name Repository.save\n".to_string()
            )
        );
    }

    #[test]
    fn a_dotted_query_selects_the_one_candidate_its_whole_name_matches() {
        let (code, text) =
            present_query("OrderRepository.save", save_candidates(), None, None, None);

        let observed = (
            code,
            text.lines().next().map(str::to_string),
            text.contains("shop.order.OrderRepository.save  fun"),
            text.contains("JdbcOrderRepository.save"),
            text.contains("InMemoryOrderRepository.save"),
        );
        assert_eq!(
            observed,
            (
                0,
                Some("## Symbols: OrderRepository.save".to_string()),
                true,
                false,
                false,
            )
        );
    }

    #[test]
    fn a_dotted_query_whose_whole_name_matches_nothing_is_reported_not_found() {
        let (code, text) = present_query("Imaginary.save", save_candidates(), None, None, None);

        let observed = (
            code,
            text.contains("no declaration named Imaginary.save in this workspace"),
        );
        assert_eq!(observed, (1, true));
    }

    #[test]
    fn a_dotted_query_matching_several_candidates_lists_only_those_and_exits_three() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (package, file) in [("alpha", "Alpha.kt"), ("beta", "Beta.kt")] {
            std::fs::write(
                dir.path().join(file),
                format!("package {package}\n\nclass Repo {{\n    fun save() {{}}\n}}\n"),
            )
            .expect("write fixture");
        }
        let candidate_in = |file: &str| SymbolCandidate {
            name: "save".to_string(),
            file: dir.path().join(file).to_string_lossy().into_owned(),
            line: 4,
            col: 9,
        };

        let outcome = present_symbols(
            dir.path(),
            "Repo.save",
            vec![candidate_in("Alpha.kt"), candidate_in("Beta.kt")],
            None,
            None,
            PickFilter::for_query("Repo.save", None),
            Format::Md,
        )
        .expect("ambiguous listing");

        let observed = (
            outcome.exit.code(),
            outcome.text.lines().next().map(str::to_string),
            outcome.text.matches("  fun  ").count(),
        );
        assert_eq!(
            observed,
            (
                3,
                Some("## Ambiguous: Repo.save (2 candidates)".to_string()),
                2
            )
        );
    }

    #[test]
    fn a_kind_filter_narrows_before_ambiguity_is_judged() {
        let mixed = vec![
            candidate(
                "OrderRepository",
                "multi-module/core/src/main/kotlin/shop/order/OrderRepository.kt",
                3,
                1,
            ),
            candidate(
                "InMemoryOrderRepository",
                "multi-module/db/src/main/kotlin/shop/db/InMemoryOrderRepository.kt",
                9,
                7,
            ),
        ];
        let (code, text) = present(mixed, Some(KindFilter::Interface), None, None);
        insta::assert_snapshot!("kind_filter_interface", format!("exit {code}\n{text}"));
    }

    #[test]
    fn a_limit_caps_the_shown_rows_and_reports_the_remainder() {
        let (code, text) = present(save_candidates(), None, Some(1), None);
        insta::assert_snapshot!("limit_caps_rows", format!("exit {code}\n{text}"));
    }

    #[test]
    fn a_name_matching_nothing_exits_one() {
        let (code, text) = present(Vec::new(), None, None, None);
        insta::assert_snapshot!("no_match", format!("exit {code}\n{text}"));
    }

    #[test]
    fn a_function_local_declaration_is_qualified_marked_local_and_present_in_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("Local.kt");
        std::fs::write(
            &file,
            "package demo\n\nclass Outer {\n    fun make() {\n        class Local(val id: Int)\n    }\n}\n",
        )
        .expect("write fixture");
        let candidate = SymbolCandidate {
            name: "Local".to_string(),
            file: file.to_string_lossy().into_owned(),
            line: 5,
            col: 15,
        };

        let markdown = present_symbols(
            dir.path(),
            "Local",
            vec![candidate.clone()],
            None,
            None,
            PickFilter::for_query("Local", None),
            Format::Md,
        )
        .expect("markdown");
        let json = present_symbols(
            dir.path(),
            "Local",
            vec![candidate],
            None,
            None,
            PickFilter::for_query("Local", None),
            Format::Json,
        )
        .expect("json");

        insta::assert_snapshot!(
            "function_local_declaration",
            format!(
                "exit {}\n{}\n---json---\n{}",
                markdown.exit.code(),
                markdown.text,
                json.text
            )
        );
    }
}
