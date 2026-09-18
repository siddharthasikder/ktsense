//! Strongly-connected components of a [`Graph`] by Tarjan's algorithm, and the cycle subset a
//! dependency report reads from them.
//!
//! Pure, like the rest of `core`: it consumes a hand-built graph and returns owned node
//! identities, so cycle detection is tested without a filesystem or a parser.
//!
//! The traversal is iterative rather than recursive. A textbook Tarjan recurses once per tree
//! edge, so a long dependency chain from an arbitrary repository would overflow the worker
//! thread's stack; an explicit work stack keeps that depth on the heap instead.

use std::collections::BTreeMap;

use crate::rank::Graph;

/// Every strongly-connected component, each returned with its members sorted and the components
/// themselves ordered, so the result is identical no matter what order the edges were built in.
pub fn strongly_connected_components<N: Ord + Clone>(graph: &Graph<N>) -> Vec<Vec<N>> {
    Tarjan::new(graph).run()
}

/// The cycles in the graph: every component of two or more nodes, plus any single node that links
/// to itself. A lone node with no self-loop is not a cycle and is dropped.
pub fn cycles<N: Ord + Clone>(graph: &Graph<N>) -> Vec<Vec<N>> {
    strongly_connected_components(graph)
        .into_iter()
        .filter(|component| match component.as_slice() {
            [only] => graph.contains_edge(only, only),
            _ => true,
        })
        .collect()
}

const UNVISITED: usize = usize::MAX;

struct Tarjan<'g, N> {
    nodes: Vec<&'g N>,
    adjacency: Vec<Vec<usize>>,
    discovery: Vec<usize>,
    lowlink: Vec<usize>,
    on_stack: Vec<bool>,
    component_stack: Vec<usize>,
    next_index: usize,
    components: Vec<Vec<N>>,
}

impl<'g, N: Ord + Clone> Tarjan<'g, N> {
    fn new(graph: &'g Graph<N>) -> Self {
        let nodes: Vec<&N> = graph.nodes().collect();
        let index_of: BTreeMap<&N, usize> = nodes
            .iter()
            .enumerate()
            .map(|(index, &node)| (node, index))
            .collect();

        let mut adjacency = vec![Vec::new(); nodes.len()];
        for (from, to) in graph.edges() {
            adjacency[index_of[from]].push(index_of[to]);
        }
        for targets in &mut adjacency {
            targets.sort_unstable();
            targets.dedup();
        }

        let count = nodes.len();
        Self {
            nodes,
            adjacency,
            discovery: vec![UNVISITED; count],
            lowlink: vec![0; count],
            on_stack: vec![false; count],
            component_stack: Vec::new(),
            next_index: 0,
            components: Vec::new(),
        }
    }

    fn run(mut self) -> Vec<Vec<N>> {
        for start in 0..self.nodes.len() {
            if self.discovery[start] == UNVISITED {
                self.walk(start);
            }
        }
        for component in &mut self.components {
            component.sort();
        }
        self.components.sort();
        self.components
    }

    fn walk(&mut self, start: usize) {
        self.enter(start);
        let mut call_stack = vec![(start, 0usize)];
        while let Some(&(node, next_child)) = call_stack.last() {
            if next_child < self.adjacency[node].len() {
                call_stack.last_mut().expect("frame present").1 += 1;
                let child = self.adjacency[node][next_child];
                if self.discovery[child] == UNVISITED {
                    self.enter(child);
                    call_stack.push((child, 0));
                } else if self.on_stack[child] {
                    self.lowlink[node] = self.lowlink[node].min(self.discovery[child]);
                }
            } else {
                if self.lowlink[node] == self.discovery[node] {
                    self.close_component(node);
                }
                call_stack.pop();
                if let Some(&(parent, _)) = call_stack.last() {
                    self.lowlink[parent] = self.lowlink[parent].min(self.lowlink[node]);
                }
            }
        }
    }

    fn enter(&mut self, node: usize) {
        self.discovery[node] = self.next_index;
        self.lowlink[node] = self.next_index;
        self.next_index += 1;
        self.component_stack.push(node);
        self.on_stack[node] = true;
    }

    fn close_component(&mut self, root: usize) {
        let mut component = Vec::new();
        loop {
            let node = self.component_stack.pop().expect("root on the stack");
            self.on_stack[node] = false;
            component.push(self.nodes[node].clone());
            if node == root {
                break;
            }
        }
        self.components.push(component);
    }
}
