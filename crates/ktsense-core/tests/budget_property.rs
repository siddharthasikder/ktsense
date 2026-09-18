//! The acceptance property for the budgeted emitter: over randomly generated budgets and item
//! sizes it must never report a bound above the budget, that bound must equal the summed per-item
//! estimate and never fall below the estimate of the emitted items concatenated (the direction that
//! makes it a conservative upper bound), and those items must be a leading prefix of the input.
//!
//! There is no property-testing crate here on purpose: a dependency would land in the shared lock
//! file and cut against this crate's serde-only rule. Instead a splitmix64 generator drives a few
//! thousand cases from fixed seeds, so a failure is fully reproducible from its seed, and the named
//! degenerate cases - zero budget, a single item larger than the whole budget, and zero-size items
//! - are fed in explicitly ahead of the random ones so they are always exercised.

use ktsense_core::{emit_within_budget, ByteRatioEstimator, TokenEstimator};

const RANDOM_CASES: u64 = 4096;
const MAX_BUDGET: u64 = 200;
const MAX_ITEMS: u64 = 25;
const MAX_ITEM_BYTES: u64 = 400;

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Case {
    budget: usize,
    sizes: Vec<usize>,
}

/// What broke, and the case that broke it. Equality against `None` is the single assertion.
#[derive(Debug, PartialEq, Eq)]
struct Violation {
    case: Case,
    reported_bound: usize,
    reason: &'static str,
}

fn check(case: &Case) -> Option<Violation> {
    let texts: Vec<String> = case.sizes.iter().map(|&bytes| "x".repeat(bytes)).collect();
    let emission = emit_within_budget(texts.clone(), case.budget, &ByteRatioEstimator);

    let recomputed: usize = emission
        .items
        .iter()
        .map(|text| ByteRatioEstimator.estimate(text))
        .sum();
    let concat_estimate = ByteRatioEstimator.estimate(&emission.items.concat());
    let is_prefix = emission.items == texts[..emission.items.len()];

    let reason = if emission.token_upper_bound > case.budget {
        Some("reported bound exceeded the budget")
    } else if emission.token_upper_bound != recomputed {
        Some("reported bound did not match the summed per-item estimate")
    } else if emission.token_upper_bound < concat_estimate {
        Some("reported bound fell below the estimate of the concatenated output")
    } else if !is_prefix {
        Some("emitted items were not a leading prefix of the input")
    } else {
        None
    };

    reason.map(|reason| Violation {
        case: Case {
            budget: case.budget,
            sizes: case.sizes.clone(),
        },
        reported_bound: emission.token_upper_bound,
        reason,
    })
}

fn random_case(seed: u64) -> Case {
    let mut rng = SplitMix64(seed.wrapping_mul(0x2545_F491_4F6C_DD1D).wrapping_add(1));
    let budget = rng.below(MAX_BUDGET + 1) as usize;
    let item_count = rng.below(MAX_ITEMS + 1);
    let sizes = (0..item_count)
        .map(|_| rng.below(MAX_ITEM_BYTES + 1) as usize)
        .collect();
    Case { budget, sizes }
}

#[test]
fn budgeted_emission_never_overruns_over_random_and_degenerate_cases() {
    let degenerate = [
        Case {
            budget: 0,
            sizes: vec![5, 0, 3],
        },
        Case {
            budget: 2,
            sizes: vec![400],
        },
        Case {
            budget: 10,
            sizes: vec![0, 0, 0],
        },
        Case {
            budget: 0,
            sizes: Vec::new(),
        },
    ];

    let first_violation = degenerate
        .into_iter()
        .chain((0..RANDOM_CASES).map(random_case))
        .find_map(|case| check(&case));

    assert_eq!(first_violation, None);
}
