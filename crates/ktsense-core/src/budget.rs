//! Emits as many items as fit within a token budget and reports the estimate it consumed.
//!
//! The cost of an item is measured by the crate's [`TokenEstimator`], never by a second estimator
//! invented here, so a front-end that swaps in a real tokenizer changes both what it renders and
//! what it budgets in one place.
//!
//! Items are emitted in the order given, which is a priority order for every caller: the ranked
//! output of [`crate::page_rank`], the most-referenced files first, and so on. So the emitter fills
//! a *prefix* of that order and stops at the first item that would not fit, rather than skipping a
//! large item to squeeze in a smaller, lower-priority one behind it. The two invariants a caller
//! relies on: the reported estimate never exceeds the budget, and it equals the summed estimate of
//! exactly the items emitted.

use crate::TokenEstimator;

/// The outcome of a budgeted emission: the items that fit, and the estimate they consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetedEmission<T> {
    pub items: Vec<T>,
    pub tokens_used: usize,
}

/// Emits the longest prefix of `items` whose combined estimate stays within `budget`.
///
/// Stops at the first item that would push the running estimate past the budget. A single item
/// larger than the whole budget is therefore never emitted, a zero budget emits only zero-cost
/// leading items, and the reported estimate is always at most the budget.
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
    let mut tokens_used = 0usize;

    for item in items {
        let cost = estimator.estimate(item.as_ref());
        match tokens_used.checked_add(cost) {
            Some(total) if total <= budget => {
                tokens_used = total;
                emitted.push(item);
            }
            _ => break,
        }
    }

    BudgetedEmission {
        items: emitted,
        tokens_used,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ByteRatioEstimator;

    /// The three degenerate shapes the acceptance criteria name, checked as one table: a zero
    /// budget emits nothing payable, an item larger than the budget is refused whole, and zero-cost
    /// items ride along for free. Each case records what was emitted and the estimate reported.
    #[test]
    fn degenerate_budgets_never_overrun_and_report_what_they_emit() {
        let estimator = ByteRatioEstimator;
        let oversized = "x".repeat(400);

        let zero_budget = emit_within_budget(vec!["fun a()".to_string()], 0, &estimator);
        let single_too_big = emit_within_budget(vec![oversized.clone()], 1, &estimator);
        let zero_cost_items = emit_within_budget(vec![String::new(), String::new()], 0, &estimator);

        let observed = (
            (zero_budget.items, zero_budget.tokens_used),
            (single_too_big.items, single_too_big.tokens_used),
            (zero_cost_items.items.len(), zero_cost_items.tokens_used),
        );

        assert_eq!(observed, ((Vec::new(), 0), (Vec::new(), 0), (2, 0),));
    }
}
