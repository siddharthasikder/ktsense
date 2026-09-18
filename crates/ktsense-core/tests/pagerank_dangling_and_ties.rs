//! Acceptance tests for the two PageRank design decisions the six-node hand graph never exercises:
//! how a dangling node's rank is handled, and how a genuine scored tie is ordered.
//!
//! # Dangling sink
//!
//! ```text
//!   A -> B
//!   A -> C
//!   B -> C
//!   C -> (nothing)
//! ```
//!
//! `C` has no outgoing edge, so it is dangling. The module redistributes a dangling node's rank
//! uniformly across every node each round rather than dropping it, which keeps total mass at one
//! and lets `C` still accumulate the rank flowing into it. The expected scores are not read back
//! from the implementation: they come from an independent power iteration (30 rounds, damping 0.85,
//! uniform 1/N start), recorded in `.agents/scratchpad/ktsense/review-fix-core.md`:
//!
//! ```text
//!   C = 0.5208693504569001
//!   B = 0.281551000246976
//!   A = 0.19757964929612412
//! ```
//!
//! Each score is pinned to 1e-12 as an integer, not merely ordered, because both the damping
//! constant and the dangling convention move these values while leaving the order intact; only an
//! exact score distinguishes this convention from the alternatives.
//!
//! # Genuine tie
//!
//! ```text
//!   X -> B, X -> A
//!   B -> X, A -> X
//! ```
//!
//! `A` and `B` are structurally symmetric, so their scores are bit-identical. First-appearance node
//! order is `X, B, A` and the ranking sort is stable, so if the tie-break did nothing the equal
//! pair would stay `B, A`. The ranking instead reports `X, A, B`, so `A` before `B` can only be the
//! documented ascending-by-node tie-break, not an artefact of stable ordering.

use ktsense_core::{page_rank, Graph, PageRankOptions};

fn scaled_ranking(graph: &Graph<&'static str>) -> Vec<(&'static str, i64)> {
    page_rank(graph, &PageRankOptions::default())
        .into_iter()
        .map(|ranked| (ranked.node, (ranked.score * 1e12).round() as i64))
        .collect()
}

#[test]
fn a_dangling_sink_keeps_its_rank_and_pins_exact_scores() {
    let graph = Graph::from_edges([("A", "B"), ("A", "C"), ("B", "C")]);

    assert_eq!(
        scaled_ranking(&graph),
        vec![
            ("C", 520_869_350_457),
            ("B", 281_551_000_247),
            ("A", 197_579_649_296),
        ]
    );
}

#[test]
fn equal_scores_are_ordered_by_node_ascending() {
    let graph = Graph::from_edges([("X", "B"), ("X", "A"), ("B", "X"), ("A", "X")]);
    let ranked = page_rank(&graph, &PageRankOptions::default());

    let order: Vec<&str> = ranked.iter().map(|entry| entry.node).collect();
    let score_of = |node| {
        ranked
            .iter()
            .find(|entry| entry.node == node)
            .expect("node is ranked")
            .score
    };

    assert_eq!(
        (order, score_of("A") == score_of("B")),
        (vec!["X", "A", "B"], true)
    );
}
