//! Resolves the subtypes of a Java type by name (KT-116), purely.
//!
//! The engine resolves Kotlin only, so a Java class or interface has no implementors it can answer.
//! When `trace` resolves to one, the adapter scans the workspace's `.java` and `.kt` sources for
//! types that name it in an `extends`/`implements` clause or a `:` supertype list, reduces each
//! supertype to its simple name, and hands the result here as [`TypeNode`] values. This module owns
//! the transitive closure: direct subtypes first, then their subtypes, found by repeating the match
//! on each subtype's own name, bounded and cycle-safe.
//!
//! The match is by simple name, never by resolved type, so two unrelated `Foo`s in different
//! packages both count as subtypes of a queried `Foo`. The answer is labelled a text match for that
//! reason, and this crate stays pure: the adapter reads the files, this walks the names.

use std::collections::{BTreeMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

/// The precision every supertype-implementor answer carries, because supertype clauses are matched
/// by simple name rather than resolved to the queried declaration.
pub const SUPERTYPE_PRECISION: &str = "text match (supertypes matched by name)";

/// How many levels of transitive subtype the closure follows before stopping, so a deep or
/// pathological hierarchy cannot run away.
const DEPTH_CAP: usize = 8;

/// One type declared in the workspace, as the adapter extracted it: its fully-qualified name, its
/// simple name, where it is declared, and the simple names in its supertype clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeNode {
    pub fqn: String,
    pub simple_name: String,
    pub path: String,
    pub line: u32,
    pub supertypes: Vec<String>,
}

/// One subtype of the queried type: its fully-qualified name and location, and the simple name of
/// the parent it was reached through when it is not a direct subtype. A direct subtype has no `via`,
/// which is skipped from JSON so a direct row serializes without the field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupertypeImplementor {
    pub fqn: String,
    pub path: String,
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

/// The subtypes of a Java type, direct first then transitive, with the text-match precision they
/// carry because supertypes were matched by simple name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupertypeImplementors {
    #[serde(skip_deserializing, default = "supertype_precision")]
    pub precision: &'static str,
    pub implementors: Vec<SupertypeImplementor>,
}

fn supertype_precision() -> &'static str {
    SUPERTYPE_PRECISION
}

/// The transitive subtypes of `target` among `nodes`, matched by simple name. Direct subtypes (a
/// node whose supertype clause names `target`) come first with no `via`; a subtype reached through
/// another is marked `via <parent simple name>`. The walk is breadth-first, so a type is attributed
/// to the shallowest parent that reaches it, dedup'd by location, and bounded at [`DEPTH_CAP`] with a
/// per-name expansion guard so a cycle cannot loop. The caller excludes the queried declaration
/// itself from `nodes`, so a type is never listed as its own subtype.
pub fn resolve_supertype_implementors(target: &str, nodes: &[TypeNode]) -> SupertypeImplementors {
    let mut children: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, node) in nodes.iter().enumerate() {
        for supertype in &node.supertypes {
            children.entry(supertype.as_str()).or_default().push(index);
        }
    }

    let mut implementors = Vec::new();
    let mut emitted: HashSet<(String, u32)> = HashSet::new();
    let mut expanded: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    queue.push_back((target.to_string(), 0));

    while let Some((parent, depth)) = queue.pop_front() {
        if depth >= DEPTH_CAP || !expanded.insert(parent.clone()) {
            continue;
        }
        let mut indices = children.get(parent.as_str()).cloned().unwrap_or_default();
        indices.sort_by(|&left, &right| {
            (&nodes[left].path, nodes[left].line).cmp(&(&nodes[right].path, nodes[right].line))
        });
        for index in indices {
            let node = &nodes[index];
            if !emitted.insert((node.path.clone(), node.line)) {
                continue;
            }
            implementors.push(SupertypeImplementor {
                fqn: node.fqn.clone(),
                path: node.path.clone(),
                line: node.line,
                via: (parent != target).then(|| parent.clone()),
            });
            queue.push_back((node.simple_name.clone(), depth + 1));
        }
    }

    SupertypeImplementors {
        precision: SUPERTYPE_PRECISION,
        implementors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(simple: &str, path: &str, line: u32, supertypes: &[&str]) -> TypeNode {
        TypeNode {
            fqn: format!("pkg.{simple}"),
            simple_name: simple.to_string(),
            path: path.to_string(),
            line,
            supertypes: supertypes.iter().map(|name| name.to_string()).collect(),
        }
    }

    /// Direct subtypes come first in path order with no `via`; a subtype reached through a direct one
    /// is marked `via` its parent; an unrelated type is absent; and a supertype cycle (`P` and `Q`
    /// each naming the other) terminates rather than looping. Composed into one `(simple_name, via)`
    /// list with the precision and asserted once.
    #[test]
    fn direct_subtypes_precede_transitive_ones_each_marked_via_its_parent() {
        let nodes = vec![
            node("Mid", "a.java", 10, &["Base"]),
            node("P", "c.java", 1, &["Base", "Q"]),
            node("Q", "d.java", 2, &["P"]),
            node("LeafA", "b.java", 4, &["Mid"]),
            node("Unrelated", "e.java", 5, &["Other"]),
        ];

        let answer = resolve_supertype_implementors("Base", &nodes);

        let observed: Vec<(String, Option<String>)> = answer
            .implementors
            .iter()
            .map(|implementor| {
                (
                    implementor.fqn.trim_start_matches("pkg.").to_string(),
                    implementor.via.clone(),
                )
            })
            .collect();
        assert_eq!(
            (answer.precision, observed),
            (
                "text match (supertypes matched by name)",
                vec![
                    ("Mid".to_string(), None),
                    ("P".to_string(), None),
                    ("LeafA".to_string(), Some("Mid".to_string())),
                    ("Q".to_string(), Some("P".to_string())),
                ],
            )
        );
    }
}
