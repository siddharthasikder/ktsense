//! Counts identifier occurrences across a corpus, which is how `map` ranks declarations.
//!
//! Imports cannot see usage inside a package, because files in one package never import each other,
//! so on a largely single-package repository the import proxy is blind to most real usage (KT-22a).
//! The skeleton cannot answer it either: bodies are elided by the compressor, so a call site inside
//! a function is invisible there. This pass therefore reads the source itself.
//!
//! It counts *identifier occurrences*, which is weaker than what `trace` reports and is labelled as
//! such wherever it surfaces:
//!
//! - Occurrences are counted by simple name, so two declarations sharing a name across packages
//!   share a count. Resolving a name to a declaration is the engine's job; `map` must stay
//!   engine-free, because it is the cheap "what is this repository" answer and a references request
//!   per declaration over a thousand files is not that.
//! - A name in a comment or a string literal is not counted, because the count walks the parse tree
//!   rather than the raw text.
//! - An import is not a reference. `trace` drops header sites for the same reason, and counting them
//!   would credit a cross-package use twice, once at the import and once at the call site, which is
//!   precisely the bias this pass exists to remove.
//! - A declaration's own name counts once, at its declaration site, as it does in a `trace` usage
//!   list.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use ktsense_core::ReferenceCounts;

/// The node kinds tree-sitter-kotlin gives a name occurrence: a value or callable reference, and a
/// reference in type position. Matching the kind rather than scanning text is what keeps a name
/// inside a comment or a string out of the count.
const IDENTIFIER_KINDS: [&str; 2] = ["simple_identifier", "type_identifier"];

/// Subtrees that name a declaration without using it.
const NON_REFERENCE_SUBTREES: [&str; 2] = ["import_header", "package_header"];

/// Counts every identifier occurrence in `sources`, keyed by name.
///
/// A file that cannot be read or parsed contributes nothing rather than failing the map: one
/// unreadable file in a large tree must not deny an answer for the rest, which is the same rule
/// `skeleton_for_deps` follows.
pub(crate) fn count_identifiers(sources: &[PathBuf]) -> ReferenceCounts {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for path in sources {
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        count_into(&mut counts, &source);
    }
    ReferenceCounts::from_counts(counts)
}

/// Walks one file's parse tree, adding its identifier occurrences to `counts`.
///
/// The walk is an explicit stack rather than recursion: a generated or adversarial file can nest
/// deeply enough to overflow the thread stack, and the whole point of a bounded traversal elsewhere
/// in this binary is that a map of an arbitrary repository degrades to a value instead of aborting.
fn count_into(counts: &mut BTreeMap<String, usize>, source: &str) {
    let Ok(tree) = ktsense_syntax::parse(source) else {
        return;
    };
    let bytes = source.as_bytes();
    let mut pending = vec![tree.root_node()];
    while let Some(node) = pending.pop() {
        let kind = node.kind();
        if NON_REFERENCE_SUBTREES.contains(&kind) {
            continue;
        }
        if IDENTIFIER_KINDS.contains(&kind) {
            if let Ok(name) = node.utf8_text(bytes) {
                *counts.entry(name.to_string()).or_insert(0) += 1;
            }
            // An identifier is the leaf of this walk. Descending would count an aliased inner node
            // a second time for the same occurrence.
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn counted(source: &str) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        count_into(&mut counts, source);
        counts
    }

    /// The three exclusions that decide whether a count is evidence of use: the import and package
    /// headers that name a declaration without using it, and the comment and string text that only
    /// mention it. Checked as one count table over a file carrying all of them.
    #[test]
    fn imports_package_headers_comments_and_strings_are_not_references() {
        let source = concat!(
            "package shop.order\n",
            "\n",
            "import shop.db.JdbcOrderRepository\n",
            "\n",
            "// JdbcOrderRepository in a comment\n",
            "/** JdbcOrderRepository in a KDoc */\n",
            "class Wiring {\n",
            "    val note: String = \"JdbcOrderRepository in a string\"\n",
            "    fun build(): JdbcOrderRepository = JdbcOrderRepository()\n",
            "}\n",
        );

        let counts = counted(source);

        let observed = (
            counts.get("JdbcOrderRepository").copied(),
            counts.get("shop").copied(),
            counts.get("order").copied(),
            counts.get("Wiring").copied(),
        );
        // Two occurrences: the return type and the constructor call. The import, the package header,
        // the comment, the KDoc and the string literal contribute nothing.
        assert_eq!(observed, (Some(2), None, None, Some(1)));
    }

    /// The count over the committed fixture, hand-checked against the source:
    ///
    /// - `save` 6: three declarations (interface, two overrides) and three call sites. The `trace`
    ///   snapshot over the same fixture reports six reference sites, so this pass agrees with the
    ///   engine on that symbol, which is the cross-check worth having.
    /// - `OrderRepository` 6: the interface, two supertype positions, three property types. The five
    ///   import statements naming it are excluded.
    /// - `findById` 3: three declarations and no call site at all, which is what distinguishes it
    ///   from `save` and is exactly the usage signal the import proxy could not see.
    #[test]
    fn the_fixture_counts_match_a_hand_count_and_agree_with_trace_on_save() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/multi-module");
        let sources = crate::collect_kotlin_files(&fixture).expect("walks the fixture");

        let counts = count_identifiers(&sources);

        let observed = (
            counts.for_name("save"),
            counts.for_name("OrderRepository"),
            counts.for_name("InMemoryOrderRepository"),
            counts.for_name("findById"),
            counts.for_name("nonexistent"),
        );
        assert_eq!(observed, (6, 6, 2, 3, 0));
    }
}
