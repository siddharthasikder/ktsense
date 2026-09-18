//! The acceptance test for PageRank: a six-node hand graph whose ranking is derivable from its
//! structure, not read back from whatever the implementation prints.
//!
//! The graph is a single hub `A` endorsed by every other node, plus a descending endorsement chain
//! `A -> B -> C -> D -> E` where each link also points back at the hub:
//!
//! ```text
//!   A -> B
//!   B -> A, C
//!   C -> A, D
//!   D -> A, E
//!   E -> A
//!   F -> A
//! ```
//!
//! Reading the ranking off the structure:
//!
//! - `A` is pointed at by all five other nodes, far more than any other node, so it ranks first.
//! - `B` is the only node the top node `A` points at, and `A` spends its whole out-mass on `B`, so
//!   `B` inherits the most concentrated endorsement there is and ranks second.
//! - `C`, `D`, `E` sit on the chain below `B`. Each is endorsed by the node above it, but that node
//!   splits its out-mass between the hub and the next link, so each rung receives roughly half of
//!   the rung above. That gives the strict descent `C > D > E`.
//! - `F` has no incoming edge at all, so it holds only the teleport floor and ranks last.
//!
//! The expected order is therefore `A, B, C, D, E, F`, and it follows from in-degree and the
//! concentration of the endorsing nodes rather than from any printed score.

use ktsense_core::{page_rank, Graph, PageRankOptions};

#[test]
fn six_node_hand_graph_ranks_by_endorsement_structure() {
    let graph = Graph::from_edges([
        ("A", "B"),
        ("B", "A"),
        ("B", "C"),
        ("C", "A"),
        ("C", "D"),
        ("D", "A"),
        ("D", "E"),
        ("E", "A"),
        ("F", "A"),
    ]);

    let order: Vec<&str> = page_rank(&graph, &PageRankOptions::default())
        .into_iter()
        .map(|ranked| ranked.node)
        .collect();

    assert_eq!(order, vec!["A", "B", "C", "D", "E", "F"]);
}
