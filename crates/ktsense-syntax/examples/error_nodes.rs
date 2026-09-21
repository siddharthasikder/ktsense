//! Reports every `ERROR` and `MISSING` node in a Kotlin file, with the source line, so a file the
//! CLI rejects can be reduced to the construct the grammar cannot parse.
//!
//! Run with: cargo run -p ktsense-syntax --example error_nodes -- <file>...

use std::env;
use std::fs;

use tree_sitter::Node;

fn main() -> anyhow::Result<()> {
    for path in env::args().skip(1) {
        let source = fs::read_to_string(&path)?;
        let tree = ktsense_syntax::parse(&source)?;
        let root = tree.root_node();
        println!("{path}: has_error={}", root.has_error());
        report(root, &source);
    }
    Ok(())
}

fn report(node: Node<'_>, source: &str) {
    if node.is_error() || node.is_missing() {
        let line = node.start_position().row + 1;
        let column = node.start_position().column + 1;
        let label = if node.is_missing() {
            "MISSING"
        } else {
            "ERROR"
        };
        let text = source[node.byte_range()]
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(90)
            .collect::<String>();
        let context = source.lines().nth(line - 1).unwrap_or("").trim_end();
        println!("  {label} at {line}:{column} kind={} `{text}`", node.kind());
        println!("    line {line}: {context}");
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        report(child, source);
    }
}
