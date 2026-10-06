//! Classifies engine reference sites by the syntax node they fall in, so `core` can keep comment,
//! KDoc and string text, and same-named declarations, out of a caller answer (KT-83).
//!
//! `kmp-lsp` 0.26.0 answers `textDocument/references` with a whole-word text match (it runs `rg`),
//! so the sites it returns include every prose mention of the name and every other declaration that
//! happens to share the simple name, not only the code that uses it. This module is the adapter that
//! turns each `(line, column)` into a [`SiteKind`] value; the decision of what to do with each kind
//! lives in `ktsense-core`, which never parses.
//!
//! A site that cannot be located or classified is [`SiteKind::Code`]: misclassifying a real use as
//! prose would silently hide a caller, which is the one error the card forbids. A parse failure, a
//! point past the end of the file, and a partially recovered tree all fall back to `Code` for the
//! same reason.

use ktsense_core::SiteKind;
use tree_sitter::{Node, Point};

/// The string-literal node kinds `brokk-tree-sitter-kotlin` 0.4 produces. A site inside one of these
/// is [`SiteKind::String`] unless it sits inside a `${...}` interpolation, which is code.
const STRING_KINDS: &[&str] = &[
    "string_literal",
    "line_string_literal",
    "multi_line_string_literal",
    "character_literal",
];

/// Nodes that hold a code expression inside a string template (`"...${expr}..."`). Reaching one of
/// these before the enclosing string literal means the site is code, not string text.
const INTERPOLATION_KINDS: &[&str] = &["interpolated_expression", "interpolated_identifier"];

/// Classifies each `(1-based line, 1-based column)` site against `source`. The column is in UTF-16
/// code units, the unit an LSP `Position` uses and `kmp-lsp` reports. The returned vector is in
/// the same order as `sites`; every entry defaults to [`SiteKind::Code`] on any uncertainty.
pub fn classify_reference_sites(source: &str, sites: &[(u32, u32)]) -> Vec<SiteKind> {
    let Ok(tree) = crate::parse(source) else {
        return vec![SiteKind::Code; sites.len()];
    };
    let root = tree.root_node();
    sites
        .iter()
        .map(|&(line, column)| classify_site(root, source, line, column))
        .collect()
}

fn classify_site(root: Node<'_>, source: &str, line: u32, column: u32) -> SiteKind {
    let row = line.saturating_sub(1) as usize;
    let Some(text) = source.lines().nth(row) else {
        return SiteKind::Code;
    };
    let point = Point {
        row,
        column: byte_column_of_utf16(text, column.saturating_sub(1)),
    };
    let Some(node) = root.descendant_for_point_range(point, point) else {
        return SiteKind::Code;
    };

    let mut ancestor = Some(node);
    while let Some(current) = ancestor {
        let kind = current.kind();
        if kind == "line_comment" {
            return SiteKind::Comment;
        }
        if kind == "multiline_comment" {
            return if source[current.byte_range()].starts_with("/**") {
                SiteKind::Kdoc
            } else {
                SiteKind::Comment
            };
        }
        if INTERPOLATION_KINDS.contains(&kind) {
            return SiteKind::Code;
        }
        if STRING_KINDS.contains(&kind) {
            return SiteKind::String;
        }
        ancestor = current.parent();
    }

    if is_declaration_name(node) {
        SiteKind::DeclarationName
    } else {
        SiteKind::Code
    }
}

/// The byte offset within `line` of the character holding UTF-16 offset `units`, which is what a
/// tree-sitter `Point` column means. An offset past the end clamps to the line's length, so a stale
/// or out-of-range engine position still lands on the line rather than on the next one.
fn byte_column_of_utf16(line: &str, units: u32) -> usize {
    let mut consumed = 0u32;
    for (byte, character) in line.char_indices() {
        let width = character.len_utf16() as u32;
        if consumed + width > units {
            return byte;
        }
        consumed += width;
    }
    line.len()
}

/// Whether `node` is the name identifier of a declaration whose simple name the site matched: the
/// `type_identifier` of a class, object, companion object or type alias, or the `simple_identifier`
/// of a function, property or enum entry. The queried definition's own name is such a node too; the
/// core keeps its site listed regardless, so this classifier does not special-case it.
fn is_declaration_name(node: Node<'_>) -> bool {
    if !matches!(node.kind(), "simple_identifier" | "type_identifier") {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "class_declaration" | "object_declaration" | "companion_object" | "type_alias" => {
            names_with(parent, "type_identifier", node)
        }
        "function_declaration" | "enum_entry" => names_with(parent, "simple_identifier", node),
        "variable_declaration" => {
            names_with(parent, "simple_identifier", node)
                && parent
                    .parent()
                    .is_some_and(|grandparent| grandparent.kind() == "property_declaration")
        }
        _ => false,
    }
}

/// Whether `node` is the first child of `parent` of kind `name_kind`, i.e. the declared name rather
/// than a later identifier such as a supertype or a parameter.
fn names_with(parent: Node<'_>, name_kind: &str, node: Node<'_>) -> bool {
    let mut cursor = parent.walk();
    let matches = parent
        .children(&mut cursor)
        .find(|child| child.kind() == name_kind)
        .is_some_and(|name| name.id() == node.id());
    matches
}

#[cfg(test)]
mod tests {
    use super::byte_column_of_utf16;

    #[test]
    fn a_utf16_offset_maps_to_the_byte_holding_it_and_clamps_past_the_end() {
        let line = "aé😀b";
        let observed: Vec<usize> = (0..=6)
            .map(|units| byte_column_of_utf16(line, units))
            .collect();

        assert_eq!(observed, vec![0, 1, 3, 3, 7, 8, 8]);
    }
}
