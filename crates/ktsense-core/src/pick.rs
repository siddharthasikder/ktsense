//! Resolving a `--pick` value against a set of candidate fully-qualified names.
//!
//! A name that resolves to several declarations is disambiguated with `--pick`. The value may be a
//! full FQN or a dot-boundary suffix of one, such as `InventoryItemsRepository.createProduct` for
//! `com.example.data.InventoryItemsRepository.createProduct`. This is the pure rule both the CLI's
//! `symbols`/`trace`/`context` paths and the MCP `pick` argument share, kept here so the precedence
//! (exact FQN wins, then a unique dot-suffix, else the ambiguous subset) is stated once and tested
//! from hand-built values rather than from an engine session.
//!
//! Matching is on dot boundaries only: `Repository.save` selects `shop.OrderRepository.save` never,
//! because the character before the matched span is `y`, not a dot, so a suffix names whole trailing
//! segments and cannot cut an identifier in half.

/// What a `--pick` value resolved to against the candidate list it was matched against, as indices
/// into that list so the caller keeps ownership of its own richer candidate values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickMatch {
    /// Exactly one candidate matched: an exact FQN, or a dot-suffix unique to it.
    Selected(usize),
    /// The suffix matched several candidates; the caller lists exactly these and exits ambiguous.
    Ambiguous(Vec<usize>),
    /// No candidate matched the value as an FQN or a dot-suffix.
    Missed,
}

/// Resolves `pick` against `fqns`: an exact FQN match wins outright, otherwise a dot-boundary suffix
/// selects the one candidate it matches, lists the several it matches, or misses.
pub fn match_pick(pick: &str, fqns: &[&str]) -> PickMatch {
    if let Some(index) = fqns.iter().position(|fqn| *fqn == pick) {
        return PickMatch::Selected(index);
    }
    let matched: Vec<usize> = fqns
        .iter()
        .enumerate()
        .filter(|(_, fqn)| is_dot_suffix(fqn, pick))
        .map(|(index, _)| index)
        .collect();
    match matched.as_slice() {
        [] => PickMatch::Missed,
        [only] => PickMatch::Selected(*only),
        _ => PickMatch::Ambiguous(matched),
    }
}

/// The shortest dot-boundary suffix of `target` that no other candidate in `fqns` also carries, for
/// the ambiguity hint. It walks up from the last segment, so a name gets the least the caller must
/// type to reach it uniquely, and falls back to the whole FQN when even that is shared (a duplicate).
pub fn shortest_unique_suffix(target: &str, fqns: &[&str]) -> String {
    let segments: Vec<&str> = target.split('.').collect();
    for start in (0..segments.len()).rev() {
        let suffix = segments[start..].join(".");
        if fqns
            .iter()
            .filter(|fqn| is_dot_suffix(fqn, &suffix))
            .count()
            == 1
        {
            return suffix;
        }
    }
    target.to_string()
}

/// Whether `suffix` names whole trailing segments of `fqn`: the two are equal, or `fqn` ends with
/// `suffix` at a dot boundary so the segment `suffix` begins with is not sliced out of a longer one.
fn is_dot_suffix(fqn: &str, suffix: &str) -> bool {
    if fqn == suffix {
        return true;
    }
    match fqn.len().checked_sub(suffix.len()) {
        Some(boundary) if boundary > 0 => {
            fqn.ends_with(suffix) && fqn.as_bytes()[boundary - 1] == b'.'
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANDIDATES: [&str; 3] = [
        "shop.db.InMemoryOrderRepository.save",
        "shop.db.JdbcOrderRepository.save",
        "shop.order.OrderRepository.save",
    ];

    #[test]
    fn match_pick_follows_exact_then_unique_suffix_then_ambiguous_then_missed() {
        let cases = [
            (
                "an exact FQN wins",
                "shop.db.JdbcOrderRepository.save",
                PickMatch::Selected(1),
            ),
            (
                "a suffix unique to one candidate selects it",
                "InMemoryOrderRepository.save",
                PickMatch::Selected(0),
            ),
            (
                "a suffix several candidates share lists exactly those",
                "save",
                PickMatch::Ambiguous(vec![0, 1, 2]),
            ),
            (
                "a suffix must fall on a dot boundary, not mid-identifier",
                "Repository.save",
                PickMatch::Missed,
            ),
            (
                "a suffix no candidate carries misses",
                "shop.nope.save",
                PickMatch::Missed,
            ),
        ];

        let observed: Vec<(&str, PickMatch)> = cases
            .iter()
            .map(|(label, pick, _)| (*label, match_pick(pick, &CANDIDATES)))
            .collect();
        let expected: Vec<(&str, PickMatch)> = cases
            .iter()
            .map(|(label, _, outcome)| (*label, outcome.clone()))
            .collect();
        assert_eq!(observed, expected);
    }

    #[test]
    fn an_exact_fqn_wins_over_a_candidate_it_is_a_suffix_of() {
        let nested = ["OrderRepository.save", "shop.OrderRepository.save"];
        assert_eq!(
            match_pick("OrderRepository.save", &nested),
            PickMatch::Selected(0)
        );
    }

    #[test]
    fn shortest_unique_suffix_climbs_to_the_least_that_disambiguates() {
        let cases = [
            (
                "the last segment alone is shared, so climb one more",
                "shop.db.InMemoryOrderRepository.save",
                "InMemoryOrderRepository.save",
            ),
            (
                "a single candidate needs only its last segment",
                "shop.order.OrderRepository.save",
                "OrderRepository.save",
            ),
        ];

        let observed: Vec<(&str, String)> = cases
            .iter()
            .map(|(label, target, _)| (*label, shortest_unique_suffix(target, &CANDIDATES)))
            .collect();
        let expected: Vec<(&str, String)> = cases
            .iter()
            .map(|(label, _, suffix)| (*label, suffix.to_string()))
            .collect();
        assert_eq!(observed, expected);
    }

    #[test]
    fn a_lone_candidate_reduces_to_its_final_segment() {
        assert_eq!(
            shortest_unique_suffix("shop.order.OrderId", &["shop.order.OrderId"]),
            "OrderId"
        );
    }

    #[test]
    fn a_duplicated_fqn_falls_back_to_the_whole_name() {
        let duplicated = ["a.b.C.save", "a.b.C.save"];
        assert_eq!(
            shortest_unique_suffix("a.b.C.save", &duplicated),
            "a.b.C.save"
        );
    }
}
