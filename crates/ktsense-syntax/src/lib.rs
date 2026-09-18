//! Syntactic adapter: parses Kotlin source with tree-sitter and produces `ktsense-core` values.
//!
//! Declaration extraction lands in KT-05. This module currently owns only parser construction, so
//! that the grammar and ABI pairing is proven by a test from the first commit.

#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use tree_sitter::{Parser, Tree};

/// Builds a parser bound to the Kotlin grammar.
pub fn kotlin_parser() -> Result<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_kotlin::LANGUAGE.into())
        .context("tree-sitter Kotlin grammar rejected by this tree-sitter version")?;
    Ok(parser)
}

/// Parses Kotlin source into a concrete syntax tree.
pub fn parse(source: &str) -> Result<Tree> {
    kotlin_parser()?
        .parse(source, None)
        .context("tree-sitter returned no tree for the given source")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLASS_WITH_ONE_FUNCTION: &str = r#"
package app

class UserService(private val repo: UserRepository) {
    fun createUser(name: String): String = repo.save(name)
}
"#;

    #[test]
    fn grammar_loads_and_parses_a_class_without_errors() {
        let tree = parse(CLASS_WITH_ONE_FUNCTION).expect("parse");
        let root = tree.root_node();
        assert_eq!(
            (root.kind(), root.has_error()),
            ("source_file", false),
            "expected a clean source_file root"
        );
    }
}
