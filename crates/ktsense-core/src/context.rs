//! Assembles a budgeted context bundle for one symbol, purely.
//!
//! The bundle answers "tell me what I need to know about this symbol in at most N tokens". The
//! priority order is fixed and stated once, here: the declaration itself, then its own source body,
//! then the outline of the file it lives in, then its callers, then its implementors.
//! [`crate::emit_within_budget`] fills a prefix of that order, so a budget too small for the outline
//! spends nothing on callers either, and the whole bundle is gated by the one budget mechanism the
//! crate already has.
//!
//! Three rules follow from that, stated so they can be argued with:
//!
//! 1. Every unit is self-contained. An outline unit is one whole top-level declaration with
//!    balanced braces, never a line of one, so a bundle the budget cut short is still readable
//!    Kotlin rather than a half-open block.
//! 2. A section the budget could not reach still appears, reporting what it dropped. An absent
//!    `Callers` section would read as "this symbol has no callers", which is a different answer.
//! 3. A unit is measured as the exact line the renderer will emit, including its list bullet, so
//!    the reported bound is never below the text it pays for. The bundle nonetheless carries
//!    callers and implementors as values rather than rendered lines, because `--format json` is
//!    consumed by a tool and a markdown bullet is not data.
//!
//! Callers and implementors arrive already derived by [`crate::trace`], because `kmp-lsp` 0.26.0
//! advertises no call hierarchy and that derivation has exactly one home.

use serde::{Deserialize, Serialize};

use crate::annotated::AnnotatedDeclaration;
use crate::budget::emit_within_budget;
use crate::references::declaration_span_at;
use crate::render::{context_caller_line, related_line, render_skeleton, RenderOptions};
use crate::skeleton::{Declaration, FileSkeleton};
use crate::text::neutralize;
use crate::trace::{Definition, IndexCompleteness, RelatedDeclaration};
use crate::TokenEstimator;

/// Which optional sections a bundle carries, so a caller can ask for only what it needs rather than
/// paying the budget for the whole bundle. The declaration line is not a section: it is always
/// present, because a bundle without the thing it describes is not an answer. Everything else is a
/// section a caller can switch off.
///
/// [`Self::all`] is the default and reproduces the pre-filter bundle exactly, so an unfiltered
/// `context` is byte-identical to the one that shipped before `--only`. When a section is off, its
/// units are never offered to the budget, so the budget spends on the sections that are on alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSections {
    pub source: bool,
    pub outline: bool,
    pub callers: bool,
    pub implementors: bool,
}

impl ContextSections {
    /// Every section on: the default, and the shape that reproduces the pre-filter bundle.
    pub const fn all() -> Self {
        Self {
            source: true,
            outline: true,
            callers: true,
            implementors: true,
        }
    }

    /// Whether every section is on, which is both the default and the condition under which the
    /// filter is left out of the serialized bundle so an unfiltered answer is byte-identical.
    pub fn is_all(&self) -> bool {
        *self == Self::all()
    }
}

impl Default for ContextSections {
    fn default() -> Self {
        Self::all()
    }
}

/// One part of the bundle: the items that fit, and how many the budget dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextSection<T> {
    pub items: Vec<T>,
    pub omitted: usize,
}

impl<T> ContextSection<T> {
    /// Everything the section had to offer, whether or not it fit.
    pub fn available(&self) -> usize {
        self.items.len() + self.omitted
    }

    /// Whether the section carries nothing at all: no items and nothing dropped. The annotated
    /// section is left out of the JSON in this case, so a non-annotation bundle serializes exactly
    /// as it did before this section existed (KT-109).
    pub fn is_absent(&self) -> bool {
        self.items.is_empty() && self.omitted == 0
    }
}

/// The queried declaration's own source lines that the budget afforded, the absolute 1-based range
/// they were sliced from, and how many lines the budget dropped. The lines are neutralized, so the
/// budget measures and the renderer emits the same safe text. `omitted_lines` are the lines from
/// `start_line + lines.len()` through `end_line` that did not fit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceSection {
    pub lines: Vec<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub omitted_lines: usize,
}

/// One source line kept by `--match`: its absolute 1-based number, its neutralized text, and
/// whether a run of lines was skipped just before it, which the renderer shows as a `...` gap. The
/// gap flag is intrinsic to the kept lines' numbers and is decided before the budget trims the
/// tail, so a budget-truncated match keeps the same gaps the full one would.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MatchedLine {
    pub number: u32,
    pub text: String,
    pub gap_before: bool,
}

/// The `--match` view of a declaration's body: only the lines matching the pattern plus their
/// `--around` context, each numbered, with `gap_before` marking where lines were dropped between
/// runs. `start_line` and `end_line` are the declaration's full span, so a reader can see how much
/// of it the matched lines cover; `omitted_lines` are matched or context lines the budget could not
/// afford, dropped from the tail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MatchedSource {
    pub lines: Vec<MatchedLine>,
    pub start_line: u32,
    pub end_line: u32,
    pub omitted_lines: usize,
}

/// Decides whether a source line is kept by `--match`. A trait rather than a concrete regex because
/// `ktsense-core` holds no parser or regex engine: the CLI compiles the pattern and passes a matcher
/// in, exactly as it passes a [`crate::TokenEstimator`], so the budgeting and the filtering both stay
/// testable from hand-built values.
pub trait LineMatcher {
    fn matches(&self, line: &str) -> bool;
}

/// A `--match` request: the matcher and how many context lines to keep around each hit.
pub struct SourceMatch<'a> {
    pub matcher: &'a dyn LineMatcher,
    pub around: usize,
}

/// A budgeted context bundle for one symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolContext {
    pub symbol: String,
    /// Which sections the caller asked for. Left out of the JSON when every section is on, so an
    /// unfiltered bundle serializes exactly as it did before `--only` existed; present only when a
    /// filter was applied, which is the signal a consumer reads to tell a filtered answer apart.
    #[serde(skip_serializing_if = "ContextSections::is_all")]
    pub sections: ContextSections,
    /// Whether the engine had finished indexing: a bundle built on a partial index lists fewer
    /// callers than exist, and the reader must be told before acting on it.
    pub index: IndexCompleteness,
    pub definition: Definition,
    /// The traced declaration's own signature, as one line, or nothing when the budget or the
    /// resolver left it without one.
    pub declaration: ContextSection<String>,
    /// The traced declaration's own body, from its declaration line through the end of its span,
    /// absent when the caller passed no source or the span could not be located.
    pub source: Option<SourceSection>,
    /// The `--match` view of the body: only matching lines plus their context, numbered. Present
    /// instead of [`Self::source`] when the caller passed a `--match` pattern, and left out of the
    /// JSON otherwise so an unfiltered bundle serializes exactly as before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_source: Option<MatchedSource>,
    /// The outline of the declaration's file, one balanced block per top-level declaration.
    pub file_outline: ContextSection<String>,
    pub callers: ContextSection<RelatedDeclaration>,
    /// The declarations this symbol is written on as an annotation, present only when it resolved to
    /// an annotation class and at least one declaration carries it (KT-109). Left out of the JSON
    /// when absent, so a non-annotation bundle serializes exactly as before.
    #[serde(skip_serializing_if = "ContextSection::is_absent")]
    pub annotated: ContextSection<AnnotatedDeclaration>,
    pub implementors: ContextSection<RelatedDeclaration>,
    pub budget: usize,
    /// Conservative upper bound on the tokens the emitted content occupies, as
    /// [`crate::BudgetedEmission::token_upper_bound`] defines it: never above `budget`.
    pub token_upper_bound: usize,
}

impl SymbolContext {
    /// Units the budget dropped across every section.
    pub fn omitted(&self) -> usize {
        self.declaration.omitted
            + self
                .source
                .as_ref()
                .map_or(0, |source| source.omitted_lines)
            + self
                .matched_source
                .as_ref()
                .map_or(0, |matched| matched.omitted_lines)
            + self.file_outline.omitted
            + self.callers.omitted
            + self.annotated.omitted
            + self.implementors.omitted
    }
}

/// Everything `build_context` needs, gathered by the caller from a `trace` answer and the parser.
pub struct ContextInput<'a> {
    pub definition: Definition,
    pub index: IndexCompleteness,
    /// Which sections to carry. [`ContextSections::all`] reproduces the pre-filter bundle.
    pub sections: ContextSections,
    /// Skeleton of the file the declaration lives in, absent when it could not be read or parsed.
    pub file: Option<&'a FileSkeleton>,
    /// The lines of the file the declaration lives in, 1-based (`source[0]` is line 1), from which
    /// the declaration's own body is sliced. The caller reads the file; core never does. Absent
    /// when it could not be read.
    pub source: Option<&'a [String]>,
    /// When present, the body is filtered to lines the matcher keeps plus their context, and the
    /// bundle carries a [`MatchedSource`] instead of a [`SourceSection`].
    pub source_match: Option<SourceMatch<'a>>,
    pub callers: &'a [RelatedDeclaration],
    /// The declarations the symbol is written on as an annotation, when it resolved to an annotation
    /// class; empty otherwise. Offered to the budget after callers and before implementors (KT-109).
    pub annotated: &'a [AnnotatedDeclaration],
    pub implementors: &'a [RelatedDeclaration],
    pub budget: usize,
}

/// Builds the bundle, emitting as much of the priority order as the budget affords.
pub fn build_context<E: TokenEstimator>(input: ContextInput<'_>, estimator: &E) -> SymbolContext {
    let body = input.sections.source.then(|| source_body(&input)).flatten();
    let offered = units_in_priority_order(&input, body.as_ref());
    let available = Available::of(&offered);

    let emission = emit_within_budget(offered, input.budget, estimator);
    let kept = Kept::of(emission.items);

    let (source, matched_source) = split_source(body, kept.source);

    SymbolContext {
        symbol: input.definition.qualified_name.clone(),
        sections: input.sections,
        index: input.index,
        declaration: section(kept.declaration, available.declaration),
        source,
        matched_source,
        file_outline: section(kept.file_outline, available.file_outline),
        callers: section(kept.callers, available.callers),
        annotated: section(kept.annotated, available.annotated),
        implementors: section(kept.implementors, available.implementors),
        definition: input.definition,
        budget: input.budget,
        token_upper_bound: emission.token_upper_bound,
    }
}

/// Splits the surviving source candidates into the one of the two body views the request asked for:
/// a `--match` request yields a numbered [`MatchedSource`], an ordinary one the whole-body
/// [`SourceSection`]. The lines the budget dropped from the tail are reported either way, counted
/// against the full set of candidates the body offered.
fn split_source(
    body: Option<SourceBody>,
    kept: Vec<SourceCandidate>,
) -> (Option<SourceSection>, Option<MatchedSource>) {
    let Some(body) = body else {
        return (None, None);
    };
    let omitted_lines = body.candidates.len() - kept.len();
    if body.matched {
        let lines = kept
            .into_iter()
            .filter_map(|candidate| candidate.matched)
            .collect();
        (
            None,
            Some(MatchedSource {
                lines,
                start_line: body.start_line,
                end_line: body.end_line,
                omitted_lines,
            }),
        )
    } else {
        let lines = kept.into_iter().map(|candidate| candidate.raw).collect();
        (
            Some(SourceSection {
                lines,
                start_line: body.start_line,
                end_line: body.end_line,
                omitted_lines,
            }),
            None,
        )
    }
}

/// The queried declaration's body as emission candidates and the absolute range they cover, or
/// `None` when the caller passed no source, the file did not parse, or no declaration starts on the
/// definition line. The end of an unbounded span (a declaration running to the end of the file) is
/// resolved against the source length here, the one place both the span and the file's lines are in
/// hand. Neutralizing happens here so the budget's measure and the renderer's output are one text.
///
/// `matched` records whether `--match` filtered the candidates: a filtered body carries numbered
/// lines with gap markers, an unfiltered one the whole body unchanged.
struct SourceBody {
    candidates: Vec<SourceCandidate>,
    start_line: u32,
    end_line: u32,
    matched: bool,
}

/// One offered body line: the exact text the budget measures and the renderer emits, the raw
/// neutralized line the whole-body view keeps, and, in `--match` mode, the numbered line the matched
/// view keeps. `rendered` already carries a leading `...` for a line that opens a new run, so a
/// budget-trimmed match emits the same gaps the full one would.
#[derive(Clone)]
struct SourceCandidate {
    rendered: String,
    raw: String,
    matched: Option<MatchedLine>,
}

fn source_body(input: &ContextInput<'_>) -> Option<SourceBody> {
    let source = input.source?;
    let file = input.file?;
    let span = declaration_span_at(file, input.definition.line)?;
    let total = source.len() as u32;
    let end_line = span.end.unwrap_or(total).min(total);
    if span.start == 0 || span.start > end_line {
        return None;
    }
    let raw_lines: Vec<String> = source[(span.start - 1) as usize..end_line as usize]
        .iter()
        .map(|line| neutralize(line).into_owned())
        .collect();
    Some(match &input.source_match {
        Some(spec) => matched_body(raw_lines, span.start, end_line, spec),
        None => whole_body(raw_lines, span.start, end_line),
    })
}

/// The whole body, one candidate per line, rendered exactly as the line reads so an unfiltered
/// `context` is byte-identical to the one that shipped before `--match`.
fn whole_body(raw_lines: Vec<String>, start_line: u32, end_line: u32) -> SourceBody {
    let candidates = raw_lines
        .into_iter()
        .map(|raw| SourceCandidate {
            rendered: raw.clone(),
            raw,
            matched: None,
        })
        .collect();
    SourceBody {
        candidates,
        start_line,
        end_line,
        matched: false,
    }
}

/// The `--match` body: only the lines the matcher keeps plus their context, numbered, with a `...`
/// opening each run that follows dropped lines.
fn matched_body(
    raw_lines: Vec<String>,
    start_line: u32,
    end_line: u32,
    spec: &SourceMatch<'_>,
) -> SourceBody {
    let mut candidates = Vec::new();
    let mut previous: Option<usize> = None;
    for index in kept_indices(&raw_lines, spec) {
        let gap_before = match previous {
            None => index > 0,
            Some(prev) => index > prev + 1,
        };
        previous = Some(index);
        let number = start_line + index as u32;
        let text = raw_lines[index].clone();
        let rendered = if gap_before {
            format!("...\n{number}: {text}")
        } else {
            format!("{number}: {text}")
        };
        candidates.push(SourceCandidate {
            rendered,
            raw: text.clone(),
            matched: Some(MatchedLine {
                number,
                text,
                gap_before,
            }),
        });
    }
    SourceBody {
        candidates,
        start_line,
        end_line,
        matched: true,
    }
}

/// The body indices `--match` keeps: every line the matcher accepts, plus `around` lines on each
/// side, merged into a sorted set so overlapping windows never duplicate a line.
fn kept_indices(lines: &[String], spec: &SourceMatch<'_>) -> Vec<usize> {
    let mut keep = std::collections::BTreeSet::new();
    for (index, line) in lines.iter().enumerate() {
        if spec.matcher.matches(line) {
            let low = index.saturating_sub(spec.around);
            let high = (index + spec.around).min(lines.len() - 1);
            for neighbour in low..=high {
                keep.insert(neighbour);
            }
        }
    }
    keep.into_iter().collect()
}

/// What a unit contributes once it has survived the budget. Variant order is the priority order.
enum Payload {
    Declaration(String),
    SourceLine(SourceCandidate),
    FileOutline(String),
    Caller(RelatedDeclaration),
    Annotated(AnnotatedDeclaration),
    Implementor(RelatedDeclaration),
}

/// One emission unit: the exact text the renderer will emit for it, and the value it regroups into.
/// The text is what the budget measures; the value is what the bundle carries.
struct Unit {
    text: String,
    payload: Payload,
}

impl AsRef<str> for Unit {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

fn units_in_priority_order(input: &ContextInput<'_>, source: Option<&SourceBody>) -> Vec<Unit> {
    let mut units = Vec::new();
    if !input.definition.signature.is_empty() {
        units.push(Unit {
            text: input.definition.signature.clone(),
            payload: Payload::Declaration(input.definition.signature.clone()),
        });
    }
    if let Some(body) = source {
        units.extend(body.candidates.iter().map(|candidate| Unit {
            text: candidate.rendered.clone(),
            payload: Payload::SourceLine(candidate.clone()),
        }));
    }
    if input.sections.outline {
        if let Some(file) = input.file {
            units.extend(outline_blocks(file).into_iter().map(|block| Unit {
                text: block.clone(),
                payload: Payload::FileOutline(block),
            }));
        }
    }
    if input.sections.callers {
        units.extend(input.callers.iter().map(|caller| Unit {
            text: context_caller_line(caller),
            payload: Payload::Caller(caller.clone()),
        }));
        units.extend(input.annotated.iter().map(|declaration| Unit {
            text: crate::render::annotated_context_line(declaration),
            payload: Payload::Annotated(declaration.clone()),
        }));
    }
    if input.sections.implementors {
        units.extend(input.implementors.iter().map(|implementor| Unit {
            text: related_line(implementor),
            payload: Payload::Implementor(implementor.clone()),
        }));
    }
    units
}

/// The file's outline split into one block per top-level declaration, each rendered by the same
/// writer `outline` uses so a block reads identically in both, already neutralized. Splitting at
/// top level rather than by line is what keeps a budget-truncated outline balanced.
fn outline_blocks(file: &FileSkeleton) -> Vec<String> {
    file.declarations
        .iter()
        .filter_map(|declaration| outline_block(file, declaration))
        .collect()
}

fn outline_block(file: &FileSkeleton, declaration: &Declaration) -> Option<String> {
    let lone = FileSkeleton {
        path: file.path.clone(),
        package: file.package.clone(),
        imports: Vec::new(),
        declarations: vec![declaration.clone()],
        truncated: false,
        partial: false,
    };
    let rendered = render_skeleton(&lone, &RenderOptions::default());
    (!rendered.is_empty()).then_some(rendered)
}

/// How many units each section offered, counted before the budget saw any of them.
struct Available {
    declaration: usize,
    source: usize,
    file_outline: usize,
    callers: usize,
    annotated: usize,
    implementors: usize,
}

impl Available {
    fn of(units: &[Unit]) -> Self {
        let mut counts = Self {
            declaration: 0,
            source: 0,
            file_outline: 0,
            callers: 0,
            annotated: 0,
            implementors: 0,
        };
        for unit in units {
            match unit.payload {
                Payload::Declaration(_) => counts.declaration += 1,
                Payload::SourceLine(_) => counts.source += 1,
                Payload::FileOutline(_) => counts.file_outline += 1,
                Payload::Caller(_) => counts.callers += 1,
                Payload::Annotated(_) => counts.annotated += 1,
                Payload::Implementor(_) => counts.implementors += 1,
            }
        }
        counts
    }
}

/// The surviving units regrouped into their sections, in emission order.
struct Kept {
    declaration: Vec<String>,
    source: Vec<SourceCandidate>,
    file_outline: Vec<String>,
    callers: Vec<RelatedDeclaration>,
    annotated: Vec<AnnotatedDeclaration>,
    implementors: Vec<RelatedDeclaration>,
}

impl Kept {
    fn of(units: Vec<Unit>) -> Self {
        let mut kept = Self {
            declaration: Vec::new(),
            source: Vec::new(),
            file_outline: Vec::new(),
            callers: Vec::new(),
            annotated: Vec::new(),
            implementors: Vec::new(),
        };
        for unit in units {
            match unit.payload {
                Payload::Declaration(signature) => kept.declaration.push(signature),
                Payload::SourceLine(candidate) => kept.source.push(candidate),
                Payload::FileOutline(block) => kept.file_outline.push(block),
                Payload::Caller(caller) => kept.callers.push(caller),
                Payload::Annotated(declaration) => kept.annotated.push(declaration),
                Payload::Implementor(implementor) => kept.implementors.push(implementor),
            }
        }
        kept
    }
}

fn section<T>(items: Vec<T>, available: usize) -> ContextSection<T> {
    ContextSection {
        omitted: available.saturating_sub(items.len()),
        items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_context_markdown;
    use crate::skeleton::{DeclKind, Parameter};
    use crate::ByteRatioEstimator;

    fn repository_file() -> FileSkeleton {
        FileSkeleton::new("core/OrderRepository.kt")
            .in_package("shop.order")
            .with_declarations(vec![
                Declaration::interface("OrderRepository", 3).containing(vec![
                    Declaration::function("save", 4)
                        .with_parameters(vec![Parameter::new("order", "Order")])
                        .returning("OrderId"),
                    Declaration::function("findById", 6)
                        .with_parameters(vec![Parameter::new("id", "OrderId")])
                        .returning("Order?"),
                ]),
                Declaration::new(DeclKind::TypeAlias, "Orders", 9).returning("List<Order>"),
            ])
    }

    fn definition() -> Definition {
        Definition {
            qualified_name: "shop.order.OrderRepository.save".to_string(),
            path: "core/OrderRepository.kt".to_string(),
            line: 4,
            signature: "fun save(order: Order): OrderId".to_string(),
        }
    }

    fn related(path: &str, line: u32, name: &str, sites: usize) -> RelatedDeclaration {
        RelatedDeclaration {
            path: path.to_string(),
            line,
            qualified_name: Some(name.to_string()),
            kind: Some(DeclKind::Function),
            sites,
            test: crate::references::is_test_source(path),
        }
    }

    fn callers() -> Vec<RelatedDeclaration> {
        vec![
            related(
                "app/CheckoutService.kt",
                12,
                "shop.app.CheckoutService.placeOrder",
                2,
            ),
            related(
                "app/OrderImporter.kt",
                8,
                "shop.app.OrderImporter.importAll",
                1,
            ),
        ]
    }

    fn implementors() -> Vec<RelatedDeclaration> {
        vec![related(
            "db/JdbcOrderRepository.kt",
            8,
            "shop.db.JdbcOrderRepository.save",
            1,
        )]
    }

    fn build(budget: usize) -> SymbolContext {
        let file = repository_file();
        build_context(
            ContextInput {
                definition: definition(),
                index: IndexCompleteness::Complete,
                sections: ContextSections::all(),
                file: Some(&file),
                source: None,
                source_match: None,
                callers: &callers(),
                annotated: &[],
                implementors: &implementors(),
                budget,
            },
            &ByteRatioEstimator,
        )
    }

    #[test]
    fn a_generous_budget_carries_every_section_with_balanced_outline_blocks() {
        let bundle = build(10_000);

        let observed = (
            bundle.declaration.items.clone(),
            bundle.file_outline.items.clone(),
            bundle
                .callers
                .items
                .iter()
                .map(|caller| caller.qualified_name.clone())
                .collect::<Vec<_>>(),
            bundle.implementors.items.len(),
            bundle.omitted(),
            bundle.token_upper_bound <= 10_000,
        );
        assert_eq!(
            observed,
            (
                vec!["fun save(order: Order): OrderId".to_string()],
                vec![
                    concat!(
                        "interface OrderRepository {\n",
                        "    fun save(order: Order): OrderId\n",
                        "    fun findById(id: OrderId): Order?\n",
                        "}"
                    )
                    .to_string(),
                    "typealias Orders = List<Order>".to_string(),
                ],
                vec![
                    Some("shop.app.CheckoutService.placeOrder".to_string()),
                    Some("shop.app.OrderImporter.importAll".to_string()),
                ],
                1,
                0,
                true
            )
        );
    }

    /// The whole point of the priority order: as the budget shrinks, sections are lost from the
    /// bottom up and the declaration is the last thing standing, while the reported bound never
    /// passes the budget at any size. Swept rather than sampled, so an off-by-one in the packing
    /// cannot hide between two chosen budgets.
    #[test]
    fn shrinking_the_budget_drops_sections_bottom_up_and_never_overruns() {
        let generous = build(10_000);
        let every_budget: Vec<(usize, usize, usize, usize, bool, bool)> = (0..=generous
            .token_upper_bound)
            .map(|budget| {
                let bundle = build(budget);
                let filled = (
                    bundle.declaration.items.len(),
                    bundle.file_outline.items.len(),
                    bundle.callers.items.len(),
                    bundle.implementors.items.len(),
                );
                (
                    filled.0,
                    filled.1,
                    filled.2,
                    filled.3,
                    bundle.token_upper_bound <= budget,
                    // A lower-priority section is never populated while a higher one is starved.
                    (filled.1 == 0 || filled.0 == 1)
                        && (filled.2 == 0 || filled.1 == 2)
                        && (filled.3 == 0 || filled.2 == 2),
                )
            })
            .collect();

        let overran = every_budget.iter().filter(|row| !row.4).count();
        let out_of_order = every_budget.iter().filter(|row| !row.5).count();
        let at_zero = every_budget.first().map(|row| (row.0, row.1, row.2, row.3));
        let at_full = every_budget.last().map(|row| (row.0, row.1, row.2, row.3));

        assert_eq!(
            (overran, out_of_order, at_zero, at_full),
            (0, 0, Some((0, 0, 0, 0)), Some((1, 2, 2, 1)))
        );
    }

    #[test]
    fn a_section_the_budget_could_not_reach_reports_its_loss_rather_than_vanishing() {
        let declaration_only =
            build(ByteRatioEstimator.estimate("fun save(order: Order): OrderId"));

        let observed = (
            declaration_only.declaration.items.len(),
            declaration_only.file_outline.available(),
            declaration_only.callers.available(),
            declaration_only.implementors.available(),
            declaration_only.omitted(),
        );
        assert_eq!(observed, (1, 2, 2, 1, 5));
    }

    #[test]
    fn an_unreadable_file_yields_no_outline_without_dropping_the_rest() {
        let bundle = build_context(
            ContextInput {
                definition: definition(),
                index: IndexCompleteness::Partial,
                sections: ContextSections::all(),
                file: None,
                source: None,
                source_match: None,
                callers: &callers(),
                annotated: &[],
                implementors: &implementors(),
                budget: 10_000,
            },
            &ByteRatioEstimator,
        );

        let observed = (
            bundle.file_outline.available(),
            bundle.callers.items.len(),
            bundle.implementors.items.len(),
            bundle.index,
        );
        assert_eq!(observed, (0, 2, 1, IndexCompleteness::Partial));
    }

    /// The bound must cover the bullet the renderer adds to a caller line, not just the name, or a
    /// bundle would emit more text than it reported. Pinned as the gap between the two.
    #[test]
    fn a_caller_unit_is_measured_as_the_line_that_will_be_emitted_bullet_included() {
        let caller = related(
            "app/CheckoutService.kt",
            12,
            "shop.app.CheckoutService.placeOrder",
            2,
        );
        let rendered = related_line(&caller);

        let observed = (
            rendered.starts_with("- "),
            ByteRatioEstimator.estimate(&rendered)
                >= ByteRatioEstimator.estimate(rendered.trim_start_matches("- ")),
        );
        assert_eq!(observed, (true, true));
    }

    fn greeter() -> (FileSkeleton, Vec<String>) {
        let file = FileSkeleton::new("app/Greeter.kt")
            .in_package("app")
            .with_declarations(vec![
                Declaration::function("greet", 1).returning("String"),
                Declaration::function("bye", 4).returning("String"),
            ]);
        let source = vec![
            "fun greet(): String {".to_string(),
            "    return \"hi\"".to_string(),
            "}".to_string(),
            "fun bye(): String = \"bye\"".to_string(),
        ];
        (file, source)
    }

    fn greet_definition() -> Definition {
        Definition {
            qualified_name: "app.greet".to_string(),
            path: "app/Greeter.kt".to_string(),
            line: 1,
            signature: "fun greet(): String".to_string(),
        }
    }

    /// A body that fits appears in full, and between the declaration signature and the file outline
    /// in the rendered order: `greet` spans lines 1 through 3 (the next declaration, `bye`, starts
    /// at 4), so its three lines are the whole body and nothing is dropped.
    #[test]
    fn a_body_that_fits_appears_in_full_between_the_declaration_and_the_file_outline() {
        let (file, source) = greeter();
        let bundle = build_context(
            ContextInput {
                definition: greet_definition(),
                index: IndexCompleteness::Complete,
                sections: ContextSections::all(),
                file: Some(&file),
                source: Some(&source),
                source_match: None,
                callers: &[],
                annotated: &[],
                implementors: &[],
                budget: 10_000,
            },
            &ByteRatioEstimator,
        );
        let rendered = render_context_markdown(&bundle);
        let ordered = rendered.find("## Declaration") < rendered.find("## Source")
            && rendered.find("## Source") < rendered.find("## File outline");

        assert_eq!(
            (bundle.source, bundle.token_upper_bound <= 10_000, ordered),
            (
                Some(SourceSection {
                    lines: vec![
                        "fun greet(): String {".to_string(),
                        "    return \"hi\"".to_string(),
                        "}".to_string(),
                    ],
                    start_line: 1,
                    end_line: 3,
                    omitted_lines: 0,
                }),
                true,
                true,
            )
        );
    }

    /// A body larger than the budget is cut on a line boundary, keeps the prefix that fits, names
    /// the omitted range by its absolute lines, and never carries the bundle past the budget. The
    /// budget is set to the declaration plus exactly three body lines, so the fourth is dropped.
    #[test]
    fn a_body_too_large_is_cut_on_a_line_boundary_and_names_the_omitted_range_within_budget() {
        let file = FileSkeleton::new("app/Big.kt")
            .in_package("app")
            .with_declarations(vec![Declaration::function("big", 1)]);
        let source: Vec<String> = (0..8).map(|n| format!("    line number {n:02}")).collect();
        let estimator = ByteRatioEstimator;
        let budget = estimator.estimate("fun big()") + estimator.estimate(&source[0]) * 3;
        let bundle = build_context(
            ContextInput {
                definition: Definition {
                    qualified_name: "app.big".to_string(),
                    path: "app/Big.kt".to_string(),
                    line: 1,
                    signature: "fun big()".to_string(),
                },
                index: IndexCompleteness::Complete,
                sections: ContextSections::all(),
                file: Some(&file),
                source: Some(&source),
                source_match: None,
                callers: &[],
                annotated: &[],
                implementors: &[],
                budget,
            },
            &estimator,
        );
        let rendered = render_context_markdown(&bundle);

        assert_eq!(
            (
                bundle.source.clone(),
                bundle.token_upper_bound <= budget,
                rendered.contains("5 more lines omitted; read app/Big.kt:4-8"),
            ),
            (
                Some(SourceSection {
                    lines: source[..3].to_vec(),
                    start_line: 1,
                    end_line: 8,
                    omitted_lines: 5,
                }),
                true,
                true,
            )
        );
    }

    /// With no source supplied the bundle carries no source section and the render shows no Source
    /// heading, so a caller that could not read the file is not told an empty body is the body.
    #[test]
    fn no_source_supplied_renders_no_source_section() {
        let (file, _) = greeter();
        let bundle = build_context(
            ContextInput {
                definition: greet_definition(),
                index: IndexCompleteness::Complete,
                sections: ContextSections::all(),
                file: Some(&file),
                source: None,
                source_match: None,
                callers: &[],
                annotated: &[],
                implementors: &[],
                budget: 10_000,
            },
            &ByteRatioEstimator,
        );
        let rendered = render_context_markdown(&bundle);

        assert_eq!(
            (bundle.source, rendered.contains("## Source")),
            (None, false)
        );
    }

    /// `--only source` keeps the declaration and the source body and spends the budget on them
    /// alone: the file outline, callers and implementors are off, so none is offered to the budget
    /// and none of their headings is rendered, while the declaration line stays because it is not a
    /// section. The sections filter is carried on the bundle so a JSON consumer can read it.
    #[test]
    fn only_source_keeps_the_declaration_and_source_and_renders_no_other_section() {
        let (file, source) = greeter();
        let bundle = build_context(
            ContextInput {
                definition: greet_definition(),
                index: IndexCompleteness::Complete,
                sections: ContextSections {
                    source: true,
                    outline: false,
                    callers: false,
                    implementors: false,
                },
                file: Some(&file),
                source: Some(&source),
                source_match: None,
                callers: &callers(),
                annotated: &[],
                implementors: &implementors(),
                budget: 10_000,
            },
            &ByteRatioEstimator,
        );
        let rendered = render_context_markdown(&bundle);

        let observed = (
            bundle.sections,
            bundle.source.is_some(),
            bundle.file_outline.available(),
            bundle.callers.available(),
            bundle.implementors.available(),
            rendered.contains("## Declaration"),
            rendered.contains("## Source"),
            rendered.contains("## File outline"),
            rendered.contains("## Callers"),
            rendered.contains("## Implementors"),
            bundle.token_upper_bound <= 10_000,
        );
        assert_eq!(
            observed,
            (
                ContextSections {
                    source: true,
                    outline: false,
                    callers: false,
                    implementors: false,
                },
                true,
                0,
                0,
                0,
                true,
                true,
                false,
                false,
                false,
                true,
            )
        );
    }

    /// A line matcher that keeps any line containing a fixed substring, so the match filtering is
    /// tested from a hand-built matcher with no regex engine in core.
    struct Contains(&'static str);

    impl LineMatcher for Contains {
        fn matches(&self, line: &str) -> bool {
            line.contains(self.0)
        }
    }

    fn multibranch() -> (FileSkeleton, Vec<String>) {
        let file = FileSkeleton::new("app/Guard.kt")
            .in_package("app")
            .with_declarations(vec![Declaration::function("check", 1).returning("String")]);
        let source = vec![
            "fun check(x: Int): String {".to_string(),
            "    log(x)".to_string(),
            "    if (x < 0) {".to_string(),
            "        return \"neg\"".to_string(),
            "    } else if (x == 0) {".to_string(),
            "        return \"zero\"".to_string(),
            "    }".to_string(),
            "    return \"pos\"".to_string(),
            "}".to_string(),
        ];
        (file, source)
    }

    fn check_definition() -> Definition {
        Definition {
            qualified_name: "app.check".to_string(),
            path: "app/Guard.kt".to_string(),
            line: 1,
            signature: "fun check(x: Int): String".to_string(),
        }
    }

    /// `--match return --around 0` on a multi-branch body keeps only the three `return` lines, each
    /// numbered, with a `...` opening every run because none are adjacent; the whole-body `source`
    /// is absent, the matched view is present, and the budget is not exceeded. The rendered Source
    /// carries the line numbers and the `...` gaps.
    #[test]
    fn match_keeps_only_the_matching_lines_numbered_with_gaps() {
        let (file, source) = multibranch();
        let matcher = Contains("return");
        let bundle = build_context(
            ContextInput {
                definition: check_definition(),
                index: IndexCompleteness::Complete,
                sections: ContextSections::all(),
                file: Some(&file),
                source: Some(&source),
                source_match: Some(SourceMatch {
                    matcher: &matcher,
                    around: 0,
                }),
                callers: &[],
                annotated: &[],
                implementors: &[],
                budget: 10_000,
            },
            &ByteRatioEstimator,
        );
        let rendered = render_context_markdown(&bundle);
        let matched: Vec<(u32, bool)> = bundle
            .matched_source
            .as_ref()
            .map(|matched| {
                matched
                    .lines
                    .iter()
                    .map(|line| (line.number, line.gap_before))
                    .collect()
            })
            .unwrap_or_default();

        assert_eq!(
            (
                bundle.source.is_some(),
                matched,
                bundle.matched_source.as_ref().map(|m| m.omitted_lines),
                rendered.contains("## Source"),
                rendered.contains("4:         return \"neg\""),
                rendered.contains("\n...\n"),
                bundle.token_upper_bound <= 10_000,
            ),
            (
                false,
                vec![(4, true), (6, true), (8, true)],
                Some(0),
                true,
                true,
                true,
                true,
            )
        );
    }

    /// `--match` composes with `--only source`: the file outline, callers and implementors are off,
    /// so the budget pays only for the declaration and the matched lines. `--around 1` widens each
    /// hit by one line, merging the two adjacent `return` branches into one run.
    #[test]
    fn match_composes_with_only_source_and_around_widens_the_window() {
        let (file, source) = multibranch();
        let matcher = Contains("return");
        let bundle = build_context(
            ContextInput {
                definition: check_definition(),
                index: IndexCompleteness::Complete,
                sections: ContextSections {
                    source: true,
                    outline: false,
                    callers: false,
                    implementors: false,
                },
                file: Some(&file),
                source: Some(&source),
                source_match: Some(SourceMatch {
                    matcher: &matcher,
                    around: 1,
                }),
                callers: &callers(),
                annotated: &[],
                implementors: &implementors(),
                budget: 10_000,
            },
            &ByteRatioEstimator,
        );
        let rendered = render_context_markdown(&bundle);
        let numbers: Vec<u32> = bundle
            .matched_source
            .as_ref()
            .map(|matched| matched.lines.iter().map(|line| line.number).collect())
            .unwrap_or_default();

        assert_eq!(
            (
                numbers,
                bundle.callers.available(),
                bundle.implementors.available(),
                bundle.file_outline.available(),
                rendered.contains("## Callers"),
                rendered.contains("## File outline"),
            ),
            (vec![3, 4, 5, 6, 7, 8, 9], 0, 0, 0, false, false)
        );
    }

    /// Renders the greeter bundle under one filter so a test can compare the chrome a focused and a
    /// full bundle carry. `matched` filters the body with `--match fun`, `sections` drops the other
    /// sections like `--only`, and `index` chooses the completeness marker.
    fn rendered(sections: ContextSections, index: IndexCompleteness, matched: bool) -> String {
        let (file, source) = greeter();
        let matcher = Contains("fun");
        let source_match = matched.then_some(SourceMatch {
            matcher: &matcher,
            around: 0,
        });
        let bundle = build_context(
            ContextInput {
                definition: greet_definition(),
                index,
                sections,
                file: Some(&file),
                source: Some(&source),
                source_match,
                callers: &[],
                annotated: &[],
                implementors: &[],
                budget: 10_000,
            },
            &ByteRatioEstimator,
        );
        render_context_markdown(&bundle)
    }

    /// A one-fact answer (`--only` or `--match`) drops the budget line, the `index: complete` line
    /// and the trailing resolution note, while the declaration fence stays and a partial index keeps
    /// its warning. A full, unfiltered bundle keeps all three. One table over the four renders.
    #[test]
    fn a_focused_bundle_drops_the_chrome_a_full_bundle_keeps() {
        let only = ContextSections {
            source: true,
            outline: false,
            callers: false,
            implementors: false,
        };
        let note = "Resolution is syntactic";
        let full = rendered(ContextSections::all(), IndexCompleteness::Complete, false);
        let filtered = rendered(only, IndexCompleteness::Complete, false);
        let matched = rendered(ContextSections::all(), IndexCompleteness::Complete, true);
        let partial = rendered(only, IndexCompleteness::Partial, false);

        let observed = (
            (
                full.contains("Budget "),
                full.contains("index: complete"),
                full.contains(note),
                full.contains("## Declaration"),
            ),
            (
                filtered.contains("Budget "),
                filtered.contains("index:"),
                filtered.contains(note),
                filtered.contains("## Declaration"),
            ),
            (
                matched.contains("Budget "),
                matched.contains("index:"),
                matched.contains(note),
            ),
            (
                partial.contains("index: partial"),
                partial.contains("Budget "),
                partial.contains(note),
            ),
        );

        assert_eq!(
            observed,
            (
                (true, true, true, true),
                (false, false, false, true),
                (false, false, false),
                (true, false, false),
            )
        );
    }

    fn annotated_sites() -> Vec<AnnotatedDeclaration> {
        vec![
            AnnotatedDeclaration::new("app/Reports.kt", 4, "app.SalesReport", DeclKind::Class),
            AnnotatedDeclaration::new("app/Reports.kt", 8, "app.AuditReport", DeclKind::Class),
        ]
    }

    /// When the symbol resolved to an annotation class, the bundle carries an `## Annotated` section
    /// after the callers and before the implementors, counts its declarations, and stays within the
    /// budget; a bundle given no annotation sites renders no such section (byte-identical default).
    #[test]
    fn the_annotated_section_sits_after_callers_within_budget_and_is_absent_when_empty() {
        let file = repository_file();
        let build_with = |sites: &[AnnotatedDeclaration]| {
            build_context(
                ContextInput {
                    definition: definition(),
                    index: IndexCompleteness::Complete,
                    sections: ContextSections::all(),
                    file: Some(&file),
                    source: None,
                    source_match: None,
                    callers: &callers(),
                    annotated: sites,
                    implementors: &implementors(),
                    budget: 10_000,
                },
                &ByteRatioEstimator,
            )
        };
        let with_sites = build_with(&annotated_sites());
        let without = build_with(&[]);
        let rendered = render_context_markdown(&with_sites);

        let observed = (
            with_sites.annotated.available(),
            rendered.find("## Callers") < rendered.find("## Annotated"),
            rendered.find("## Annotated") < rendered.find("## Implementors"),
            rendered.contains("- app.SalesReport  class  app/Reports.kt:4"),
            with_sites.token_upper_bound <= 10_000,
            without.annotated.is_absent(),
            render_context_markdown(&without).contains("## Annotated"),
        );
        assert_eq!(observed, (2, true, true, true, true, true, false));
    }

    /// An annotation's annotated declarations are its uses, so `--only` treats them as part of the
    /// callers section: `--only callers` keeps them and `--only outline` leaves them out, as it does
    /// every other section it was not asked for.
    #[test]
    fn only_shows_the_annotated_section_when_callers_are_asked_for() {
        let file = repository_file();
        let rendered_with = |sections: ContextSections| {
            render_context_markdown(&build_context(
                ContextInput {
                    definition: definition(),
                    index: IndexCompleteness::Complete,
                    sections,
                    file: Some(&file),
                    source: None,
                    source_match: None,
                    callers: &callers(),
                    annotated: &annotated_sites(),
                    implementors: &implementors(),
                    budget: 10_000,
                },
                &ByteRatioEstimator,
            ))
        };
        let only = |callers: bool, outline: bool| ContextSections {
            source: false,
            outline,
            callers,
            implementors: false,
        };

        let observed = (
            rendered_with(only(true, false)).contains("## Annotated"),
            rendered_with(only(false, true)).contains("## Annotated"),
        );

        assert_eq!(observed, (true, false));
    }
}
