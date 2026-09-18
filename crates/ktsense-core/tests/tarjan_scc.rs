//! Pure tests for Tarjan cycle detection. Every graph is hand-built from edges, so the algorithm
//! is exercised with no filesystem, process, or parser in reach.

use ktsense_core::{cycles, strongly_connected_components, Graph};

fn scc(edges: &[(&str, &str)]) -> Vec<Vec<String>> {
    let graph = Graph::from_edges(
        edges
            .iter()
            .map(|&(from, to)| (from.to_string(), to.to_string())),
    );
    strongly_connected_components(&graph)
}

fn cycle_sets(edges: &[(&str, &str)]) -> Vec<Vec<String>> {
    let graph = Graph::from_edges(
        edges
            .iter()
            .map(|&(from, to)| (from.to_string(), to.to_string())),
    );
    cycles(&graph)
}

#[test]
fn an_acyclic_graph_has_every_node_in_its_own_component_and_no_cycle() {
    let edges = [("a", "b"), ("b", "c"), ("a", "c")];

    let observed = (scc(&edges), cycle_sets(&edges));

    assert_eq!(
        observed,
        (
            vec![
                vec!["a".to_string()],
                vec!["b".to_string()],
                vec!["c".to_string()]
            ],
            Vec::<Vec<String>>::new(),
        )
    );
}

#[test]
fn a_multi_node_cycle_is_one_component_and_one_reported_cycle() {
    let edges = [("a", "b"), ("b", "c"), ("c", "a"), ("c", "d")];

    let observed = cycle_sets(&edges);

    assert_eq!(
        observed,
        vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]]
    );
}

#[test]
fn a_self_loop_is_a_cycle_but_a_lone_node_is_not() {
    let looping = cycle_sets(&[("a", "a"), ("a", "b")]);
    let acyclic_single = cycle_sets(&[("a", "b")]);

    assert_eq!(
        (looping, acyclic_single),
        (vec![vec!["a".to_string()]], Vec::<Vec<String>>::new())
    );
}

#[test]
fn disconnected_components_each_report_their_own_cycle_independently() {
    let edges = [("a", "b"), ("b", "a"), ("c", "d"), ("d", "c"), ("e", "f")];

    let observed = cycle_sets(&edges);

    assert_eq!(
        observed,
        vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["c".to_string(), "d".to_string()],
        ]
    );
}

#[test]
fn output_is_identical_no_matter_what_order_the_edges_arrive_in() {
    let one_order = cycle_sets(&[("a", "b"), ("b", "c"), ("c", "a"), ("x", "y"), ("y", "x")]);
    let shuffled = cycle_sets(&[("y", "x"), ("c", "a"), ("x", "y"), ("b", "c"), ("a", "b")]);

    assert_eq!(one_order, shuffled);
}
