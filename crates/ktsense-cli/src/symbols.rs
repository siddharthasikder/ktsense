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

use std::collections::HashSet;
use std::path::Path;

use ktsense_core::{
    contained_declarations, java_declarations, java_package, match_pick, render_member_summary,
    render_skeleton, shortest_unique_suffix, DeclKind, Declaration, FileSkeleton, NamedProperty,
    PickMatch, RenderOptions, SymbolMatch,
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
    /// The entries of a single enum-class match, in declaration order (KT-103). Populated only for
    /// a one-match answer and skipped when empty, so a multi-candidate listing and every non-enum
    /// declaration keep byte-identical JSON.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) entries: Vec<String>,
    /// The primary-constructor properties of a single data-class match, with their types (KT-103).
    /// Populated only for a one-match answer and skipped when empty, so a multi-candidate listing
    /// and every non-data declaration keep byte-identical JSON.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) properties: Vec<NamedProperty>,
    /// True for a declaration read from a generated-source directory under `build` (KT-104).
    /// Rendered as a `(generated)` marker and, being defaulted-false and skipped when false, absent
    /// from the JSON of every ordinary declaration so that answer stays byte-identical.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) generated: bool,
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
        PickFilter::Explicit(pick) => pick_present(root, query, resolved, pick, format, || {
            Err(CommandError::pick_missed(pick))
        }),
        PickFilter::Dotted(pick) => pick_present(root, query, resolved, pick, format, || {
            Err(CommandError::no_symbol(query))
        }),
        PickFilter::None => match resolved.len() {
            0 => Err(CommandError::no_symbol(query)),
            1 => {
                attach_members(root, &mut resolved[0]);
                render(query, &resolved, None, Exit::Success, format)
            }
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
    root: &Path,
    query: &str,
    resolved: Vec<ResolvedSymbol>,
    pick: &str,
    format: Format,
    on_miss: impl FnOnce() -> Result<SymbolsOutcome, CommandError>,
) -> Result<SymbolsOutcome, CommandError> {
    let fqns: Vec<&str> = resolved.iter().map(|symbol| symbol.fqn.as_str()).collect();
    match match_pick(pick, &fqns) {
        PickMatch::Selected(index) => {
            let mut chosen = resolved
                .into_iter()
                .nth(index)
                .expect("index within candidates");
            attach_members(root, &mut chosen);
            render(query, &[chosen], None, Exit::Success, format)
        }
        PickMatch::Ambiguous(indices) => {
            let subset = retain_indices(resolved, &indices);
            render_ambiguous_selection(query, &subset, format)
        }
        PickMatch::Missed => on_miss(),
    }
}

/// Fills in a single match's enum entries or data-class properties by re-reading its file and
/// locating the declaration, so a one-match answer lists the members its signature line omits
/// (KT-103). A declaration that is neither, or a file that cannot be read, located or parsed, leaves
/// the members empty and the row unchanged. Only ever called for a single result, so a
/// multi-candidate listing never gains members and its output stays byte-identical.
fn attach_members(root: &Path, resolved: &mut ResolvedSymbol) {
    let Ok(source) = std::fs::read_to_string(root.join(&resolved.file)) else {
        return;
    };
    let Ok(skeleton) = ktsense_syntax::extract(resolved.file.clone(), &source) else {
        return;
    };
    let name = resolved.fqn.rsplit('.').next().unwrap_or(&resolved.fqn);
    if let Some(found) = locate(&skeleton, name, resolved.line) {
        resolved.entries = found.declaration.enum_entries();
        resolved.properties = found.declaration.data_class_properties();
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
///
/// The index includes generated sources under `build/generated` (KT-104), and a declaration from
/// one is labelled `generated`.
pub(crate) fn present_contained(
    root: &Path,
    query: &str,
    kind: Option<KindFilter>,
    limit: Option<usize>,
    format: Format,
) -> Result<SymbolsOutcome, CommandError> {
    let index = gather_syntax_index(root)?;
    let mut ranked = contained_declarations(query, index.matches);
    if let Some(kind) = kind {
        ranked.retain(|found| kind_label(found.kind) == kind.label());
    }
    if ranked.is_empty() {
        return Err(CommandError::no_symbol_scoped(query, root));
    }

    let total = ranked.len();
    let shown = limit.unwrap_or(DEFAULT_CONTAINS_LIMIT).min(total);
    ranked.truncate(shown);
    let resolved: Vec<ResolvedSymbol> = ranked
        .into_iter()
        .map(|found| resolved_from_match(found, &index.generated))
        .collect();

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

/// The exact-name fallback for a `symbols <name>` the engine answered with nothing. The engine
/// (kmp-lsp `find`) does not see generated sources under `build`, so this resolves the name from the
/// workspace's own syntax index, which does (KT-104). The answer states `source: syntax index` and
/// labels any generated declaration `generated`; a name nothing declares, even in generated sources,
/// is the scoped not-found answer, which says whether generated code was searched or is absent.
pub(crate) fn present_exact_from_syntax_index(
    root: &Path,
    query: &str,
    kind: Option<KindFilter>,
    limit: Option<usize>,
    format: Format,
) -> Result<SymbolsOutcome, CommandError> {
    let index = gather_syntax_index(root)?;
    let mut exact: Vec<SymbolMatch> = index
        .matches
        .into_iter()
        .filter(|found| found.simple_name == query)
        .collect();
    exact.sort_by(|left, right| {
        (&left.qualified_name, &left.path, left.line).cmp(&(
            &right.qualified_name,
            &right.path,
            right.line,
        ))
    });
    if let Some(kind) = kind {
        exact.retain(|found| kind_label(found.kind) == kind.label());
    }
    if exact.is_empty() {
        return Err(CommandError::no_symbol_scoped(query, root));
    }

    let total = exact.len();
    let shown = limit.unwrap_or(DEFAULT_CONTAINS_LIMIT).min(total);
    exact.truncate(shown);
    let resolved: Vec<ResolvedSymbol> = exact
        .into_iter()
        .map(|found| resolved_from_match(found, &index.generated))
        .collect();

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

fn resolved_from_match(found: SymbolMatch, generated: &HashSet<String>) -> ResolvedSymbol {
    let is_generated = generated.contains(&found.path);
    ResolvedSymbol {
        fqn: found.qualified_name,
        kind: kind_label(found.kind).to_string(),
        file: found.path,
        line: found.line,
        signature: found.signature,
        local: false,
        entries: Vec::new(),
        properties: Vec::new(),
        generated: is_generated,
    }
}

/// The workspace's syntax index: every named declaration in its Kotlin sources as a [`SymbolMatch`],
/// plus the display paths that came from a generated-source directory so a row can be labelled. It
/// walks the same production tree `map` does (so `build`, `target` and `bin` are skipped), then adds
/// the narrow generated opt-in walk on top (KT-104). An unreadable or unparseable file is dropped
/// rather than aborting the search.
struct SyntaxIndex {
    matches: Vec<SymbolMatch>,
    generated: HashSet<String>,
}

fn gather_syntax_index(root: &Path) -> Result<SyntaxIndex, CommandError> {
    let mut matches = Vec::new();
    let mut generated = HashSet::new();
    for path in crate::collect_kotlin_files(root)? {
        collect_file_declarations(root, &path, &mut matches);
    }
    for path in crate::collect_generated_kotlin_files(root) {
        let before = matches.len();
        collect_file_declarations(root, &path, &mut matches);
        if matches.len() > before {
            generated.insert(normalized_path(root, &path));
        }
    }
    Ok(SyntaxIndex { matches, generated })
}

/// Appends every named declaration of one file to `out`. A file that cannot be read or parsed
/// contributes nothing, so one broken file never denies the rest of the index.
fn collect_file_declarations(root: &Path, path: &Path, out: &mut Vec<SymbolMatch>) {
    let Ok(source) = std::fs::read_to_string(path) else {
        return;
    };
    let display = normalized_path(root, path);
    let Ok(skeleton) = ktsense_syntax::extract(display.clone(), &source) else {
        return;
    };
    let mut ancestors = Vec::new();
    gather_declarations(
        skeleton.package.as_deref(),
        &display,
        &skeleton.declarations,
        &mut ancestors,
        out,
    );
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
        for property in declaration.constructor_properties() {
            out.push(SymbolMatch {
                qualified_name: fqn(package, ancestors, &property.name),
                simple_name: property.name.clone(),
                kind: property.kind,
                path: path.to_string(),
                line: property.line,
                signature: signature_of(path, &property),
            });
        }
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
    fold_java_constructors(&mut enriched);
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

/// Drops a Java constructor candidate when a Java type candidate of the same simple name sits in the
/// same file, so a Java type is never ambiguous with its own constructor (KT-117). A constructor
/// shares its class's simple name, so the engine returns both the type and the constructor for a
/// type query; folding the constructor into the type leaves one row. Kotlin constructors are
/// untouched, so a Kotlin query stays byte-identical.
fn fold_java_constructors(enriched: &mut Vec<(SymbolCandidate, ResolvedSymbol)>) {
    let java_types: HashSet<(String, String)> = enriched
        .iter()
        .filter(|(_, resolved)| {
            resolved.file.ends_with(".java") && is_java_type_kind(&resolved.kind)
        })
        .map(|(_, resolved)| (resolved.file.clone(), simple_name_of(&resolved.fqn)))
        .collect();
    enriched.retain(|(_, resolved)| {
        !(resolved.file.ends_with(".java")
            && resolved.kind == "constructor"
            && java_types.contains(&(resolved.file.clone(), simple_name_of(&resolved.fqn))))
    });
}

fn is_java_type_kind(kind: &str) -> bool {
    matches!(
        kind,
        "class" | "interface" | "enum" | "record" | "@interface"
    )
}

fn simple_name_of(fqn: &str) -> String {
    fqn.rsplit('.').next().unwrap_or(fqn).to_string()
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
        entries: Vec::new(),
        properties: Vec::new(),
        generated: false,
    };
    let Ok(source) = std::fs::read_to_string(&candidate.file) else {
        return bare();
    };
    if candidate.file.ends_with(".java") {
        return enrich_java(&display, &source, candidate).unwrap_or_else(bare);
    }
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
            entries: Vec::new(),
            properties: Vec::new(),
            generated: false,
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
        entries: Vec::new(),
        properties: Vec::new(),
        generated: false,
    })
}

/// Enriches a `.java` engine location from the pure Java declaration scan (KT-117), which the Kotlin
/// parser cannot read. The declaration at the engine's name and line gives the real kind, the
/// package-qualified name through its enclosing types, and the folded signature line, so a Java row
/// is no longer a bare `symbol` with no package. A location no declaration matches returns `None` so
/// the caller stays bare, exactly as a Kotlin miss does.
fn enrich_java(display: &str, source: &str, candidate: &SymbolCandidate) -> Option<ResolvedSymbol> {
    let declarations = java_declarations(source);
    let found = declarations
        .iter()
        .find(|declaration| {
            declaration.name == candidate.name && declaration.line == candidate.line
        })
        .or_else(|| {
            declarations
                .iter()
                .find(|declaration| declaration.name == candidate.name)
        })?;
    Some(ResolvedSymbol {
        fqn: fqn(
            java_package(source).as_deref(),
            &found.enclosing,
            &found.name,
        ),
        kind: found.kind.label().to_string(),
        signature: found.signature.clone(),
        file: display.to_string(),
        line: candidate.line,
        local: false,
        entries: Vec::new(),
        properties: Vec::new(),
        generated: false,
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
    let mut lines = rows(resolved);
    if let [single] = resolved {
        if let Some(members) = render_member_summary(&single.entries, &single.properties) {
            lines.push(members);
        }
    }
    out.push_str(&crate::fenced_block(&lines));
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
            format!(
                "{}  {}  {}:{}  {}",
                symbol.fqn,
                kind_with_markers(symbol),
                symbol.file,
                symbol.line,
                symbol.signature
            )
        })
        .collect()
}

/// The kind column with its markers: `(local)` for a declaration inside a body and `(generated)`
/// for one read from a generated-source directory. Both can hold at once in principle, so each is
/// appended independently rather than chosen between.
fn kind_with_markers(symbol: &ResolvedSymbol) -> String {
    let mut kind = symbol.kind.clone();
    if symbol.local {
        kind.push_str(" (local)");
    }
    if symbol.generated {
        kind.push_str(" (generated)");
    }
    kind
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

    fn present_one(source: &str, name: &str, line: u32) -> (SymbolsOutcome, SymbolsOutcome) {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join(format!("{name}.kt"));
        std::fs::write(&file, source).expect("write fixture");
        let candidate = SymbolCandidate {
            name: name.to_string(),
            file: file.to_string_lossy().into_owned(),
            line,
            col: 12,
        };
        let present = |format| {
            present_symbols(
                dir.path(),
                name,
                vec![candidate.clone()],
                None,
                None,
                PickFilter::for_query(name, None),
                format,
            )
            .expect("single match")
        };
        (present(Format::Md), present(Format::Json))
    }

    #[test]
    fn a_single_enum_match_lists_its_entries_in_declaration_order() {
        let (md, json) = present_one(
            "package demo\n\nenum class Role {\n    ADMIN,\n    EDITOR,\n    GUEST,\n}\n",
            "Role",
            3,
        );

        let observed = (
            md.exit.code(),
            md.text.contains("entries: ADMIN, EDITOR, GUEST"),
            json.text.contains("\"entries\": [")
                && json.text.contains("\"ADMIN\"")
                && json.text.contains("\"GUEST\""),
            json.text.contains("properties"),
        );
        assert_eq!(
            observed,
            (0, true, true, false),
            "md:\n{}\njson:\n{}",
            md.text,
            json.text
        );
    }

    #[test]
    fn a_single_data_class_match_lists_its_primary_constructor_properties_with_types() {
        let (md, json) = present_one(
            "package demo\n\ndata class Order(val id: OrderId, val total: Money)\n",
            "Order",
            3,
        );

        let observed = (
            md.exit.code(),
            md.text.contains("properties: id: OrderId, total: Money"),
            json.text.contains("\"properties\": [")
                && json.text.contains("\"name\": \"id\"")
                && json.text.contains("\"type\": \"OrderId\""),
            json.text.contains("entries"),
        );
        assert_eq!(
            observed,
            (0, true, true, false),
            "md:\n{}\njson:\n{}",
            md.text,
            json.text
        );
    }

    #[test]
    fn several_matches_never_list_members_even_when_they_are_data_classes() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (package, file) in [("alpha", "Alpha.kt"), ("beta", "Beta.kt")] {
            std::fs::write(
                dir.path().join(file),
                format!("package {package}\n\ndata class Record(val id: Long)\n"),
            )
            .expect("write fixture");
        }
        let candidate_in = |file: &str| SymbolCandidate {
            name: "Record".to_string(),
            file: dir.path().join(file).to_string_lossy().into_owned(),
            line: 3,
            col: 12,
        };
        let present = |format| {
            present_symbols(
                dir.path(),
                "Record",
                vec![candidate_in("Alpha.kt"), candidate_in("Beta.kt")],
                None,
                None,
                PickFilter::for_query("Record", None),
                format,
            )
            .expect("ambiguous listing")
        };
        let md = present(Format::Md);
        let json = present(Format::Json);

        let observed = (
            md.exit.code(),
            md.text.contains("properties:"),
            json.text.contains("properties"),
        );
        assert_eq!(
            observed,
            (3, false, false),
            "md:\n{}\njson:\n{}",
            md.text,
            json.text
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
    fn an_extension_resolves_from_the_syntax_index_under_its_simple_name_with_a_receiver_free_fqn()
    {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Probe.kt"),
            "package com.example.probe\n\ninternal fun String?.blankToNull(): String? = this\n",
        )
        .expect("write fixture");

        let outcome =
            present_exact_from_syntax_index(dir.path(), "blankToNull", None, None, Format::Md)
                .expect("exact fallback finds the extension");

        let row = outcome
            .text
            .lines()
            .find(|line| line.contains("Probe.kt:"))
            .map(str::to_string);
        assert_eq!(
            (outcome.exit.code(), row),
            (
                0,
                Some(
                    "com.example.probe.blankToNull  fun  Probe.kt:3  internal fun String?.blankToNull(): String?"
                        .to_string()
                ),
            )
        );
    }

    #[test]
    fn a_java_class_candidate_gets_its_kind_qualified_name_signature_and_folds_its_constructor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("UpdateDocumentBase.java");
        std::fs::write(
            &file,
            "package com.example.billing;\n\npublic abstract class UpdateDocumentBase extends Activity {\n    protected UpdateDocumentBase() {\n    }\n}\n",
        )
        .expect("write fixture");
        let at = |line: u32| SymbolCandidate {
            name: "UpdateDocumentBase".to_string(),
            file: file.to_string_lossy().into_owned(),
            line,
            col: 1,
        };

        let outcome = present_symbols(
            dir.path(),
            "UpdateDocumentBase",
            vec![at(3), at(4)],
            None,
            None,
            PickFilter::for_query("UpdateDocumentBase", None),
            Format::Md,
        )
        .expect("single folded match");

        let rows: Vec<&str> = outcome
            .text
            .lines()
            .filter(|line| line.contains(".java:"))
            .collect();
        let observed = (outcome.exit.code(), rows.len(), rows.first().copied());
        assert_eq!(
            observed,
            (
                0,
                1,
                Some(
                    "com.example.billing.UpdateDocumentBase  class  UpdateDocumentBase.java:3  public abstract class UpdateDocumentBase extends Activity"
                )
            )
        );
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
