//! Assembles a budgeted context bundle for one symbol, purely.
//!
//! The bundle answers "tell me what I need to know about this symbol in at most N tokens". The
//! priority order is fixed and stated once, here: the declaration itself, then the outline of the
//! file it lives in, then its callers, then its implementors. [`crate::emit_within_budget`] fills a
//! prefix of that order, so a budget too small for the outline spends nothing on callers either,
//! and the whole bundle is gated by the one budget mechanism the crate already has.
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

use serde::Serialize;

use crate::budget::emit_within_budget;
use crate::render::{related_line, render_skeleton, RenderOptions};
use crate::skeleton::{Declaration, FileSkeleton};
use crate::trace::{Definition, IndexCompleteness, RelatedDeclaration};
use crate::TokenEstimator;

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
}

/// A budgeted context bundle for one symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolContext {
    pub symbol: String,
    /// Whether the engine had finished indexing: a bundle built on a partial index lists fewer
    /// callers than exist, and the reader must be told before acting on it.
    pub index: IndexCompleteness,
    pub definition: Definition,
    /// The traced declaration's own signature, as one line, or nothing when the budget or the
    /// resolver left it without one.
    pub declaration: ContextSection<String>,
    /// The outline of the declaration's file, one balanced block per top-level declaration.
    pub file_outline: ContextSection<String>,
    pub callers: ContextSection<RelatedDeclaration>,
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
            + self.file_outline.omitted
            + self.callers.omitted
            + self.implementors.omitted
    }
}

/// Everything `build_context` needs, gathered by the caller from a `trace` answer and the parser.
pub struct ContextInput<'a> {
    pub definition: Definition,
    pub index: IndexCompleteness,
    /// Skeleton of the file the declaration lives in, absent when it could not be read or parsed.
    pub file: Option<&'a FileSkeleton>,
    pub callers: &'a [RelatedDeclaration],
    pub implementors: &'a [RelatedDeclaration],
    pub budget: usize,
}

/// Builds the bundle, emitting as much of the priority order as the budget affords.
pub fn build_context<E: TokenEstimator>(input: ContextInput<'_>, estimator: &E) -> SymbolContext {
    let offered = units_in_priority_order(&input);
    let available = Available::of(&offered);

    let emission = emit_within_budget(offered, input.budget, estimator);
    let kept = Kept::of(emission.items);

    SymbolContext {
        symbol: input.definition.qualified_name.clone(),
        index: input.index,
        declaration: section(kept.declaration, available.declaration),
        file_outline: section(kept.file_outline, available.file_outline),
        callers: section(kept.callers, available.callers),
        implementors: section(kept.implementors, available.implementors),
        definition: input.definition,
        budget: input.budget,
        token_upper_bound: emission.token_upper_bound,
    }
}

/// What a unit contributes once it has survived the budget. Variant order is the priority order.
enum Payload {
    Declaration(String),
    FileOutline(String),
    Caller(RelatedDeclaration),
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

fn units_in_priority_order(input: &ContextInput<'_>) -> Vec<Unit> {
    let mut units = Vec::new();
    if !input.definition.signature.is_empty() {
        units.push(Unit {
            text: input.definition.signature.clone(),
            payload: Payload::Declaration(input.definition.signature.clone()),
        });
    }
    if let Some(file) = input.file {
        units.extend(outline_blocks(file).into_iter().map(|block| Unit {
            text: block.clone(),
            payload: Payload::FileOutline(block),
        }));
    }
    units.extend(input.callers.iter().map(|caller| Unit {
        text: related_line(caller),
        payload: Payload::Caller(caller.clone()),
    }));
    units.extend(input.implementors.iter().map(|implementor| Unit {
        text: related_line(implementor),
        payload: Payload::Implementor(implementor.clone()),
    }));
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
    file_outline: usize,
    callers: usize,
    implementors: usize,
}

impl Available {
    fn of(units: &[Unit]) -> Self {
        let mut counts = Self {
            declaration: 0,
            file_outline: 0,
            callers: 0,
            implementors: 0,
        };
        for unit in units {
            match unit.payload {
                Payload::Declaration(_) => counts.declaration += 1,
                Payload::FileOutline(_) => counts.file_outline += 1,
                Payload::Caller(_) => counts.callers += 1,
                Payload::Implementor(_) => counts.implementors += 1,
            }
        }
        counts
    }
}

/// The surviving units regrouped into their sections, in emission order.
struct Kept {
    declaration: Vec<String>,
    file_outline: Vec<String>,
    callers: Vec<RelatedDeclaration>,
    implementors: Vec<RelatedDeclaration>,
}

impl Kept {
    fn of(units: Vec<Unit>) -> Self {
        let mut kept = Self {
            declaration: Vec::new(),
            file_outline: Vec::new(),
            callers: Vec::new(),
            implementors: Vec::new(),
        };
        for unit in units {
            match unit.payload {
                Payload::Declaration(signature) => kept.declaration.push(signature),
                Payload::FileOutline(block) => kept.file_outline.push(block),
                Payload::Caller(caller) => kept.callers.push(caller),
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
                file: Some(&file),
                callers: &callers(),
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
                file: None,
                callers: &callers(),
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
}
