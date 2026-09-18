//! Emits as many items as fit within a token budget and reports the estimate it consumed.
//!
//! The cost of an item is measured by the crate's [`TokenEstimator`], never by a second estimator
//! invented here, so a front-end that swaps in a real tokenizer changes both what it renders and
//! what it budgets in one place.
//!
//! Items are emitted in the order given, which is a priority order for every caller: the ranked
//! output of [`crate::page_rank`], the most-referenced files first, and so on. So the emitter fills
//! a *prefix* of that order and stops at the first item that would not fit, rather than skipping a
//! large item to squeeze in a smaller, lower-priority one behind it.
//!
//! The reported figure is a *conservative upper bound*, not the estimate of the emitted text. The
//! budget is gated on the running sum of per-item estimates, and the estimator is a ceiling of byte
//! length; ceilings are superadditive, so that sum can exceed the estimate of the same items
//! concatenated (two 1-byte items sum to 2 tokens where `"xx"` estimates 1). Reporting the summed
//! bound rather than the concatenation estimate keeps the number consistent with what actually
//! gated the hard limit: it is the figure a caller must subtract from a shared budget to stay
//! safe, since over-reporting wastes a sliver of context whereas under-reporting could breach the
//! limit. The two invariants a caller relies on: the reported bound never exceeds the budget, and
//! it is never below the estimate of the concatenated emitted text.

use crate::TokenEstimator;

/// The outcome of a budgeted emission: the items that fit, and the token bound they consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetedEmission<T> {
    pub items: Vec<T>,
    /// A conservative upper bound on the token estimate of the emitted items: the sum of their
    /// per-item estimates. Never exceeds the budget, and never below the estimate of the same
    /// items concatenated, because the per-item ceiling estimator is superadditive.
    pub token_upper_bound: usize,
}

/// Emits the longest prefix of `items` whose combined estimate stays within `budget`.
///
/// Stops at the first item that would push the running estimate past the budget. A single item
/// larger than the whole budget is therefore never emitted, a zero budget emits only zero-cost
/// leading items, and the reported [`BudgetedEmission::token_upper_bound`] is always at most the
/// budget.
pub fn emit_within_budget<T, E>(
    items: impl IntoIterator<Item = T>,
    budget: usize,
    estimator: &E,
) -> BudgetedEmission<T>
where
    T: AsRef<str>,
    E: TokenEstimator,
{
    let mut emitted = Vec::new();
    let mut token_upper_bound = 0usize;

    for item in items {
        let cost = estimator.estimate(item.as_ref());
        match token_upper_bound.checked_add(cost) {
            Some(total) if total <= budget => {
                token_upper_bound = total;
                emitted.push(item);
            }
            _ => break,
        }
    }

    BudgetedEmission {
        items: emitted,
        token_upper_bound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ByteRatioEstimator;

    /// The three degenerate shapes the acceptance criteria name, checked as one table: a zero
    /// budget emits nothing payable, an item larger than the budget is refused whole, and zero-cost
    /// items ride along for free. Each case records what was emitted and the bound reported.
    #[test]
    fn degenerate_budgets_never_overrun_and_report_what_they_emit() {
        let estimator = ByteRatioEstimator;
        let oversized = "x".repeat(400);

        let zero_budget = emit_within_budget(vec!["fun a()".to_string()], 0, &estimator);
        let single_too_big = emit_within_budget(vec![oversized.clone()], 1, &estimator);
        let zero_cost_items = emit_within_budget(vec![String::new(), String::new()], 0, &estimator);

        let observed = (
            (zero_budget.items, zero_budget.token_upper_bound),
            (single_too_big.items, single_too_big.token_upper_bound),
            (
                zero_cost_items.items.len(),
                zero_cost_items.token_upper_bound,
            ),
        );

        assert_eq!(observed, ((Vec::new(), 0), (Vec::new(), 0), (2, 0),));
    }

    /// Pins the over-count that names the reported figure an upper bound rather than the estimate
    /// of the emitted text: two 1-byte items sum to 2 tokens, while the same bytes concatenated
    /// (`"xx"`) estimate 1. The reported bound is the summed 2, strictly above the concatenation.
    #[test]
    fn token_upper_bound_can_exceed_the_estimate_of_the_concatenation() {
        let emission = emit_within_budget(
            vec!["x".to_string(), "x".to_string()],
            10,
            &ByteRatioEstimator,
        );
        let concatenated = emission.items.concat();

        let observed = (
            emission.items.len(),
            emission.token_upper_bound,
            ByteRatioEstimator.estimate(&concatenated),
        );

        assert_eq!(observed, (2, 2, 1));
    }
}
