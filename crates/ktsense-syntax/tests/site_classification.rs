//! KT-83: each reference site is classified by the syntax node it falls in, so `core` can keep
//! comment, KDoc and string text and same-named declarations out of a caller answer. One source
//! carries every case, and the whole-word occurrences of the traced name are classified in source
//! order and asserted as one table.

use ktsense_core::SiteKind;
use ktsense_syntax::classify_reference_sites;

const SOURCE: &str = r#"package p

/** A [Widget] link. */
class WidgetUser {
    // A Widget in a comment.
    fun run(): Int {
        val s = "a Widget here"
        val t = "x ${Widget}"
        return Widget().hashCode()
    }
}

/* plain Widget note */
class Widget

class Holder {
    companion object Widget
}
"#;

/// The 1-based (line, UTF-16 column) of every whole-word occurrence of `name`, in source order, so
/// the classifier is exercised at the positions a whole-word engine `references` search reports:
/// LSP counts columns in UTF-16 code units, not bytes or characters.
fn whole_word_sites(source: &str, name: &str) -> Vec<(u32, u32)> {
    let is_ident = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut sites = Vec::new();
    for (row, line) in source.lines().enumerate() {
        let bytes = line.as_bytes();
        let mut cursor = 0;
        while let Some(offset) = line[cursor..].find(name) {
            let start = cursor + offset;
            let end = start + name.len();
            let before_ok = start == 0 || !is_ident(bytes[start - 1]);
            let after_ok = end >= line.len() || !is_ident(bytes[end]);
            if before_ok && after_ok {
                let utf16_column = line[..start].encode_utf16().count() as u32 + 1;
                sites.push((row as u32 + 1, utf16_column));
            }
            cursor = end;
        }
    }
    sites
}

/// A real call after multibyte text on the same line stays code. `é` is two bytes but one UTF-16
/// unit, and the astral `😀` is four bytes but two units, so reading the engine's UTF-16 column as a
/// byte offset lands three bytes early, on the closing quote, and would misfile the call as string
/// text and hide the caller.
#[test]
fn a_call_after_multibyte_text_is_still_code() {
    let source = "package p\n\nfun f() {\n    val s = \"é😀\"; Widget()\n}\n\nclass Widget\n";
    let sites = whole_word_sites(source, "Widget");
    let observed = classify_reference_sites(source, &sites);

    assert_eq!(
        (sites, observed),
        (
            vec![(4, 20), (7, 7)],
            vec![SiteKind::Code, SiteKind::DeclarationName]
        )
    );
}

#[test]
fn every_site_kind_is_recognized_from_the_node_it_falls_in() {
    let sites = whole_word_sites(SOURCE, "Widget");
    let observed = classify_reference_sites(SOURCE, &sites);

    assert_eq!(
        (sites.len(), observed),
        (
            8,
            vec![
                SiteKind::Kdoc,
                SiteKind::Comment,
                SiteKind::String,
                SiteKind::Code,
                SiteKind::Code,
                SiteKind::Comment,
                SiteKind::DeclarationName,
                SiteKind::DeclarationName,
            ]
        )
    );
}

/// A file the parser cannot make sense of must not hide a reference: every site falls back to
/// `Code`, which lists it as a caller rather than dropping it. The count is preserved and the order
/// is unchanged.
#[test]
fn an_unparseable_file_classifies_every_site_as_code() {
    let broken = "this is not )( kotlin at all ${";
    let observed = classify_reference_sites(broken, &[(1, 1), (1, 6)]);

    assert_eq!(observed, vec![SiteKind::Code, SiteKind::Code]);
}
