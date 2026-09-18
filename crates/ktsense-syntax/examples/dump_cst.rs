//! Prints the CST shape for a fixture file, so extraction is written against the grammar the
//! `brokk-tree-sitter-kotlin` fork actually produces rather than an assumed one.
//!
//! Run with: cargo run -p ktsense-syntax --example dump_cst -- <file> [max-depth]

use std::env;
use std::fs;

fn main() -> anyhow::Result<()> {
    let mut args = env::args().skip(1);
    let path = args.next().expect("usage: dump_cst <file> [max-depth]");
    let max_depth: usize = args
        .next()
        .map(|value| value.parse().expect("depth must be a number"))
        .unwrap_or(4);

    let source = fs::read_to_string(&path)?;
    let tree = ktsense_syntax::parse(&source)?;
    let mut cursor = tree.walk();

    fn walk(
        cursor: &mut tree_sitter::TreeCursor<'_>,
        source: &str,
        depth: usize,
        max_depth: usize,
    ) {
        loop {
            let node = cursor.node();
            if node.is_named() {
                let field = cursor.field_name().unwrap_or("-");
                let text = source[node.byte_range()]
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(56)
                    .collect::<String>();
                println!(
                    "{:indent$}{} [{}] L{} {}",
                    "",
                    node.kind(),
                    field,
                    node.start_position().row + 1,
                    text,
                    indent = depth * 2
                );
            }
            if depth < max_depth && cursor.goto_first_child() {
                walk(cursor, source, depth + 1, max_depth);
                cursor.goto_parent();
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    walk(&mut cursor, &source, 0, max_depth);
    Ok(())
}
