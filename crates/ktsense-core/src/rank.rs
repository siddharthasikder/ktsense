//! Ranks the nodes of a directed graph by PageRank, so a caller can emit the most important first.
//!
//! Like the rest of `core` this is pure: the graph is built from hand-held values and carries no
//! filesystem, process, or parser. It is generic over node identity because the node is whatever a
//! later crate decides an import edge connects - a file path today, something else tomorrow - and
//! this layer must not assume.
//!
//! The conventions, stated once so they can be argued with:
//!
//! 1. Damping is 0.85 and the power iteration runs 30 rounds by default, the values the plan pins.
//! 2. The graph is a *simple* directed graph: parallel edges are collapsed, so "A imports B" is a
//!    boolean relation and importing B twice does not double B's rank. A self-loop is a real edge
//!    and is kept; a node that links to itself retains a share of its own rank each round.
//! 3. Dangling nodes - nodes with no outgoing edge - have their rank redistributed uniformly across
//!    every node each round, rather than being dropped. Dropping it would let total rank leak below
//!    one and understate everything downstream; redistribution keeps the mass at one exactly, which
//!    is the convention the original formulation converges to and the one this crate's tests pin.
//! 4. Scores are returned ranked high to low, ties broken by node identity ascending, so the order
//!    is deterministic for a golden test even when two nodes score identically.

use std::cmp::Ordering;
use std::collections::BTreeMap;

/// A directed graph over an arbitrary node identity.
///
/// Built once from its edges; nodes are discovered from the edges themselves, so a node that only
/// ever appears as an edge target still exists and can be ranked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Graph<N> {
    node_ids: Vec<N>,
    out_edges: Vec<Vec<usize>>,
}

impl<N: Ord + Clone> Graph<N> {
    /// Builds a graph from directed `(from, to)` edges. Node order follows first appearance, and
    /// parallel edges between the same ordered pair are collapsed to one.
    pub fn from_edges(edges: impl IntoIterator<Item = (N, N)>) -> Self {
        let mut lookup: BTreeMap<N, usize> = BTreeMap::new();
        let mut node_ids: Vec<N> = Vec::new();
        let mut pairs: Vec<(usize, usize)> = Vec::new();

        for (from, to) in edges {
            let from_idx = intern(&mut lookup, &mut node_ids, from);
            let to_idx = intern(&mut lookup, &mut node_ids, to);
            pairs.push((from_idx, to_idx));
        }

        let mut out_edges = vec![Vec::new(); node_ids.len()];
        for (from, to) in pairs {
            out_edges[from].push(to);
        }
        for adjacency in &mut out_edges {
            adjacency.sort_unstable();
            adjacency.dedup();
        }

        Self {
            node_ids,
            out_edges,
        }
    }

    pub fn node_count(&self) -> usize {
        self.node_ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.node_ids.is_empty()
    }

    pub fn nodes(&self) -> impl Iterator<Item = &N> {
        self.node_ids.iter()
    }

    /// Every directed edge as a `(from, to)` pair, parallel edges already collapsed. The order
    /// follows node interning, so a consumer that needs determinism sorts its own result.
    pub fn edges(&self) -> impl Iterator<Item = (&N, &N)> + '_ {
        self.out_edges
            .iter()
            .enumerate()
            .flat_map(move |(from, targets)| {
                targets
                    .iter()
                    .map(move |&to| (&self.node_ids[from], &self.node_ids[to]))
            })
    }

    /// Whether the graph holds the directed edge `from -> to`. A self-loop is the case where the
    /// two arguments are equal, which is how a single-node strongly-connected component is told
    /// apart from a genuine cycle.
    pub fn contains_edge(&self, from: &N, to: &N) -> bool {
        match (self.position(from), self.position(to)) {
            (Some(from), Some(to)) => self.out_edges[from].binary_search(&to).is_ok(),
            _ => false,
        }
    }

    fn position(&self, node: &N) -> Option<usize> {
        self.node_ids.iter().position(|candidate| candidate == node)
    }
}

fn intern<N: Ord + Clone>(
    lookup: &mut BTreeMap<N, usize>,
    node_ids: &mut Vec<N>,
    node: N,
) -> usize {
    if let Some(&index) = lookup.get(&node) {
        return index;
    }
    let index = node_ids.len();
    node_ids.push(node.clone());
    lookup.insert(node, index);
    index
}

/// PageRank parameters. Defaults to the pinned damping of 0.85 over 30 power iterations.
#[derive(Debug, Clone, Copy)]
pub struct PageRankOptions {
    pub damping: f64,
    pub iterations: u32,
}

impl Default for PageRankOptions {
    fn default() -> Self {
        Self {
            damping: 0.85,
            iterations: 30,
        }
    }
}

/// A node paired with its PageRank score. Scores across a whole graph sum to one.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedNode<N> {
    pub node: N,
    pub score: f64,
}

/// Ranks every node by PageRank, highest first, ties broken by node identity.
///
/// An empty graph ranks nothing. Dangling nodes, self-loops, and disconnected components are all
/// handled per the module conventions, so the returned scores always sum to one.
pub fn page_rank<N: Ord + Clone>(
    graph: &Graph<N>,
    options: &PageRankOptions,
) -> Vec<RankedNode<N>> {
    let count = graph.node_count();
    if count == 0 {
        return Vec::new();
    }

    let population = count as f64;
    let mut scores = vec![1.0 / population; count];
    let out_degree: Vec<usize> = graph.out_edges.iter().map(Vec::len).collect();

    for _ in 0..options.iterations {
        let dangling_mass: f64 = (0..count)
            .filter(|&node| out_degree[node] == 0)
            .map(|node| scores[node])
            .sum();
        let floor =
            (1.0 - options.damping) / population + options.damping * dangling_mass / population;

        let mut next = vec![floor; count];
        for source in 0..count {
            if out_degree[source] == 0 {
                continue;
            }
            let share = options.damping * scores[source] / out_degree[source] as f64;
            for &target in &graph.out_edges[source] {
                next[target] += share;
            }
        }
        scores = next;
    }

    let mut ranked: Vec<RankedNode<N>> = (0..count)
        .map(|node| RankedNode {
            node: graph.node_ids[node].clone(),
            score: scores[node],
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.node.cmp(&right.node))
    });
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONSERVATION_EPSILON: f64 = 1e-9;

    #[test]
    fn an_empty_graph_ranks_nothing() {
        let graph: Graph<&str> = Graph::from_edges(std::iter::empty());

        assert_eq!(page_rank(&graph, &PageRankOptions::default()), Vec::new());
    }

    /// A graph carrying every awkward shape at once - a dangling sink, a self-loop, and a
    /// disconnected pair - must still conserve mass at one, which is the whole point of
    /// redistributing dangling rank rather than dropping it.
    #[test]
    fn rank_is_conserved_across_dangling_self_loop_and_disconnected_nodes() {
        let graph = Graph::from_edges([("a", "b"), ("b", "b"), ("c", "d"), ("e", "a"), ("e", "f")]);

        let ranked = page_rank(&graph, &PageRankOptions::default());
        let total: f64 = ranked.iter().map(|entry| entry.score).sum();
        let observed = (ranked.len(), (total - 1.0).abs() < CONSERVATION_EPSILON);

        assert_eq!(observed, (6, true));
    }
}
