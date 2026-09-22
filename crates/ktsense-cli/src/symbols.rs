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

use ktsense_core::{render_skeleton, DeclKind, Declaration, FileSkeleton, RenderOptions};
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
}

impl ResolvedSymbol {
    fn matches_kind(&self, filter: KindFilter) -> bool {
        self.kind == filter.label()
    }
}

/// The rendered answer and the exit status it ends with, kept together so the caller never infers
/// an exit from the text.
pub(crate) struct SymbolsOutcome {
    text: String,
    exit: Exit,
}

impl From<SymbolsOutcome> for CommandOutcome {
    fn from(outcome: SymbolsOutcome) -> Self {
        CommandOutcome {
            text: outcome.text,
            exit: outcome.exit,
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
    pick: Option<&str>,
    format: Format,
) -> Result<SymbolsOutcome, CommandError> {
    let mut resolved: Vec<ResolvedSymbol> = candidates
        .iter()
        .map(|candidate| enrich(root, candidate))
        .collect();
    if let Some(kind) = kind {
        resolved.retain(|symbol| symbol.matches_kind(kind));
    }

    if let Some(pick) = pick {
        return pick_one(query, resolved, pick, format);
    }
    match resolved.len() {
        0 => Err(CommandError::no_symbol(query)),
        1 => render(query, &resolved, None, Exit::Success, format),
        _ => {
            let total = resolved.len();
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
        }
    }
}

/// What resolving a name to exactly one declaration produced: the one, or the answer to print when
/// there is not exactly one. Shared by `trace`, so the ambiguity contract (list every candidate,
/// exit 3 unless `--pick`) is stated once, here.
pub(crate) enum Selection {
    One(SymbolCandidate, ResolvedSymbol),
    Ambiguous(SymbolsOutcome),
}

/// Narrows engine candidates to the single declaration a follow-up command should act on.
pub(crate) fn select(
    root: &Path,
    query: &str,
    candidates: Vec<SymbolCandidate>,
    pick: Option<&str>,
    format: Format,
) -> Result<Selection, CommandError> {
    let mut enriched: Vec<(SymbolCandidate, ResolvedSymbol)> = candidates
        .into_iter()
        .map(|candidate| {
            let resolved = enrich(root, &candidate);
            (candidate, resolved)
        })
        .collect();
    if let Some(pick) = pick {
        return enriched
            .into_iter()
            .find(|(_, resolved)| resolved.fqn == pick)
            .map(|(candidate, resolved)| Selection::One(candidate, resolved))
            .ok_or_else(|| CommandError::pick_missed(pick));
    }
    match enriched.len() {
        0 => Err(CommandError::no_symbol(query)),
        1 => {
            let (candidate, resolved) = enriched.remove(0);
            Ok(Selection::One(candidate, resolved))
        }
        _ => {
            let resolved: Vec<ResolvedSymbol> =
                enriched.into_iter().map(|(_, resolved)| resolved).collect();
            render(query, &resolved, None, Exit::Ambiguous, format).map(Selection::Ambiguous)
        }
    }
}

fn pick_one(
    query: &str,
    resolved: Vec<ResolvedSymbol>,
    pick: &str,
    format: Format,
) -> Result<SymbolsOutcome, CommandError> {
    match resolved.into_iter().find(|symbol| symbol.fqn == pick) {
        Some(chosen) => render(query, &[chosen], None, Exit::Success, format),
        None => Err(CommandError::pick_missed(pick)),
    }
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
    Ok(SymbolsOutcome { text, exit })
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
        },
        None => bare(),
    }
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
    let rows: Vec<String> = resolved
        .iter()
        .map(|symbol| {
            format!(
                "{}  {}  {}:{}  {}",
                symbol.fqn, symbol.kind, symbol.file, symbol.line, symbol.signature
            )
        })
        .collect();
    out.push('\n');
    out.push_str(&crate::fenced_block(&rows));
    if let Some(hidden) = hidden {
        out.push_str(&format!("\n... {hidden} more (use --limit)\n"));
    }
    out
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
        let outcome = present_symbols(
            &fixtures().join("multi-module"),
            "save",
            candidates,
            kind,
            limit,
            pick,
            Format::Md,
        );
        match outcome {
            Ok(SymbolsOutcome { text, exit }) => (exit.code(), text),
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
}
