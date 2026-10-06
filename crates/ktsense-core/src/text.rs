//! Making source-derived text safe to put in front of a reader.
//!
//! Every answer ktsense renders quotes the source file: names, types, defaults, KDoc, paths and
//! packages. The reader is a language model, for which prose outside a fence is instructions, so
//! that text must not be able to break its line, reorder what is shown, or close the code fence it
//! sits in. [`neutralize`] handles the first two and [`fence_for`] the third.
//!
//! This lives in the pure crate because both front-ends need it and because it is exactly what
//! `ktsense-core` is for: string-in, string-out, no filesystem, no process, no parser. It was
//! duplicated verbatim in the CLI until KT-69; the two copies were byte-identical, which is what
//! made the duplication safe to collapse rather than merely tempting.

use std::borrow::Cow;

/// The shortest Markdown fence, used whenever the body carries no backtick run that would close it.
pub const MIN_FENCE_BACKTICKS: usize = 3;

/// Codepoints that reorder visible text independently of its logical order: the "Trojan Source"
/// set (CVE-2021-42574). They are Unicode category Cf, not Cc, so [`char::is_control`] returns
/// false for them, yet they let source-derived text render in an order that differs from what it
/// says. They must be listed out because the standard library has no predicate that names them.
const BIDIRECTIONAL_OVERRIDES: [char; 12] = [
    '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}',
    '\u{2069}', '\u{200E}', '\u{200F}', '\u{061C}',
];

/// A fence long enough to survive the body: one backtick past its longest backtick run, never
/// fewer than [`MIN_FENCE_BACKTICKS`]. A KDoc or string literal carrying a run of backticks then
/// sits inside the block as text instead of closing it, and the body itself is left byte for byte
/// alone, which is what the pinned output requires. Escaping the backticks instead would mangle a
/// legitimate Kotlin `backtick-quoted identifier`, trading one honesty problem for another.
pub fn fence_for(body: &str) -> String {
    let longest_backtick_run = body
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    "`".repeat((longest_backtick_run + 1).max(MIN_FENCE_BACKTICKS))
}

/// Text lifted from the source file, made safe to embed in a line of output without letting it
/// break that line or reorder what a reader sees. A line break, any other control character, or a
/// bidirectional override becomes a visible `<U+XXXX>` marker; every other byte passes through
/// untouched, so ordinary Kotlin renders exactly as written and the substitution, when it happens,
/// is announced rather than silent. Backtick runs are the fence's job, not this one's, because a
/// backtick is legitimate inside a Kotlin identifier.
pub fn neutralize(text: &str) -> Cow<'_, str> {
    if !text.contains(is_unsafe_in_output) {
        return Cow::Borrowed(text);
    }
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if is_unsafe_in_output(character) {
            escaped.push_str(&format!("<U+{:04X}>", character as u32));
        } else {
            escaped.push(character);
        }
    }
    Cow::Owned(escaped)
}

fn is_unsafe_in_output(character: char) -> bool {
    character.is_control() || BIDIRECTIONAL_OVERRIDES.contains(&character)
}

/// A piece of layout-tokenized signature text. [`Piece::Verbatim`] carries a run that must survive
/// byte for byte: ordinary identifiers and operators, but also whole string literals and comments,
/// so the folding rules never reach inside a `"..."` default or a `//` note. Every other variant is
/// a layout token the rules may move or drop.
#[derive(Debug, PartialEq)]
enum Piece {
    Open(char),
    Close(char),
    Comma,
    Space,
    Verbatim(String),
}

/// Folds signature, supertype and annotation text lifted from source onto one line so a declaration
/// reads as a single signature instead of leaking the source file's line breaks into the output.
///
/// Every run of whitespace collapses to one space; a space just inside a bracket (`(`, `[`, `<`) or
/// just before a closer (`)`, `]`, `>`) or a comma is dropped; and a trailing comma before a closer
/// is dropped with it. String literals (regular and raw) and comments are copied verbatim, so a
/// default such as `= "a  b"` keeps its double space, a raw string keeps its newlines for
/// [`neutralize`] to escape, and a comment keeps its meaning. This is a layout fold only: no
/// character outside a collapsed whitespace run or a dropped trailing comma is removed.
pub fn normalize_signature_layout(text: &str) -> String {
    let pieces = tokenize_layout(text);
    let mut kept: Vec<Piece> = Vec::with_capacity(pieces.len());
    let mut index = 0;
    while index < pieces.len() {
        match &pieces[index] {
            Piece::Space => {
                let leading = kept.is_empty();
                let after_open = matches!(kept.last(), Some(Piece::Open(_)));
                let before_close_or_comma =
                    matches!(pieces.get(index + 1), Some(Piece::Close(_) | Piece::Comma));
                if !(leading || after_open || before_close_or_comma) {
                    kept.push(Piece::Space);
                }
            }
            Piece::Comma => {
                let mut next = index + 1;
                while matches!(pieces.get(next), Some(Piece::Space)) {
                    next += 1;
                }
                if matches!(pieces.get(next), Some(Piece::Close(_))) {
                    if matches!(kept.last(), Some(Piece::Space)) {
                        kept.pop();
                    }
                    index = next;
                    continue;
                }
                kept.push(Piece::Comma);
            }
            Piece::Open(character) => kept.push(Piece::Open(*character)),
            Piece::Close(character) => kept.push(Piece::Close(*character)),
            Piece::Verbatim(run) => kept.push(Piece::Verbatim(run.clone())),
        }
        index += 1;
    }
    if matches!(kept.last(), Some(Piece::Space)) {
        kept.pop();
    }

    let mut out = String::with_capacity(text.len());
    for piece in kept {
        match piece {
            Piece::Open(character) | Piece::Close(character) => out.push(character),
            Piece::Comma => out.push(','),
            Piece::Space => out.push(' '),
            Piece::Verbatim(run) => out.push_str(&run),
        }
    }
    out
}

/// Splits signature text into layout tokens, gathering ordinary characters, whole string literals
/// and whole comments into [`Piece::Verbatim`] runs while emitting brackets, commas and collapsed
/// whitespace as their own tokens.
fn tokenize_layout(text: &str) -> Vec<Piece> {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut run = String::new();
    let characters: Vec<char> = text.chars().collect();
    let mut index = 0;
    let mut angle_depth = 0usize;
    while index < characters.len() {
        let character = characters[index];
        match character {
            '"' => {
                let end = consume_string(&characters, index);
                run.extend(&characters[index..end]);
                index = end;
            }
            '/' if characters.get(index + 1) == Some(&'/') => {
                let end = consume_line_comment(&characters, index);
                run.extend(&characters[index..end]);
                index = end;
            }
            '/' if characters.get(index + 1) == Some(&'*') => {
                let end = consume_block_comment(&characters, index);
                run.extend(&characters[index..end]);
                index = end;
            }
            _ if character.is_whitespace() => {
                flush_run(&mut pieces, &mut run);
                pieces.push(Piece::Space);
                while characters
                    .get(index)
                    .is_some_and(|following| following.is_whitespace())
                {
                    index += 1;
                }
            }
            '(' | '[' => {
                flush_run(&mut pieces, &mut run);
                pieces.push(Piece::Open(character));
                index += 1;
            }
            '<' if opens_type_arguments(&characters, index) => {
                flush_run(&mut pieces, &mut run);
                pieces.push(Piece::Open(character));
                angle_depth += 1;
                index += 1;
            }
            ')' | ']' => {
                flush_run(&mut pieces, &mut run);
                pieces.push(Piece::Close(character));
                index += 1;
            }
            '>' if angle_depth > 0 && closes_type_arguments(&characters, index) => {
                flush_run(&mut pieces, &mut run);
                pieces.push(Piece::Close(character));
                angle_depth -= 1;
                index += 1;
            }
            ',' => {
                flush_run(&mut pieces, &mut run);
                pieces.push(Piece::Comma);
                index += 1;
            }
            _ => {
                run.push(character);
                index += 1;
            }
        }
    }
    flush_run(&mut pieces, &mut run);
    pieces
}

/// Whether the `<` at `index` opens a type-argument or type-parameter list rather than being a
/// comparison. It opens one when it directly follows a name (`List<`, `Map<`), a nullable marker or
/// a closing bracket, or when it follows `fun ` (`fun <T>`); `a < b` and `<=` are comparisons, and
/// their spacing belongs to the expression.
fn opens_type_arguments(characters: &[char], index: usize) -> bool {
    if characters.get(index + 1) == Some(&'=') {
        return false;
    }
    let before: String = characters[..index].iter().collect();
    match before.chars().last() {
        Some(previous)
            if previous.is_alphanumeric() || matches!(previous, '_' | '?' | '>' | ')') =>
        {
            true
        }
        Some(previous) if previous.is_whitespace() => {
            let trimmed = before.trim_end();
            trimmed == "fun" || trimmed.ends_with(" fun") || trimmed.ends_with("(fun")
        }
        _ => false,
    }
}

/// Whether the `>` at `index`, with a type-argument list open, closes it. The `>` of `->` and of
/// `>=` never does.
fn closes_type_arguments(characters: &[char], index: usize) -> bool {
    let previous = index.checked_sub(1).and_then(|at| characters.get(at));
    previous != Some(&'-') && characters.get(index + 1) != Some(&'=')
}

fn flush_run(pieces: &mut Vec<Piece>, run: &mut String) {
    if !run.is_empty() {
        pieces.push(Piece::Verbatim(std::mem::take(run)));
    }
}

/// The index one past the string literal that starts at `start`. A `"""` opens a raw string that
/// runs to the next `"""`; a lone `"` opens a regular string that runs to the next unescaped `"`.
fn consume_string(characters: &[char], start: usize) -> usize {
    if characters.get(start + 1) == Some(&'"') && characters.get(start + 2) == Some(&'"') {
        let mut index = start + 3;
        while index < characters.len() {
            if characters[index] == '"'
                && characters.get(index + 1) == Some(&'"')
                && characters.get(index + 2) == Some(&'"')
            {
                return index + 3;
            }
            index += 1;
        }
        return characters.len();
    }
    let mut index = start + 1;
    while index < characters.len() {
        match characters[index] {
            '\\' => index += 2,
            '"' => return index + 1,
            _ => index += 1,
        }
    }
    characters.len()
}

/// The index one past a `//` line comment that starts at `start`, including its terminating newline
/// so the comment keeps its meaning: folding that newline into a space would pull the following code
/// onto the comment line. [`neutralize`] escapes the surviving newline rather than dropping it.
fn consume_line_comment(characters: &[char], start: usize) -> usize {
    let mut index = start + 2;
    while index < characters.len() && characters[index] != '\n' {
        index += 1;
    }
    if index < characters.len() {
        index + 1
    } else {
        index
    }
}

fn consume_block_comment(characters: &[char], start: usize) -> usize {
    let mut index = start + 2;
    while index < characters.len() {
        if characters[index] == '*' && characters.get(index + 1) == Some(&'/') {
            return index + 2;
        }
        index += 1;
    }
    characters.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIPELINE: &str = "Pipeline<Unit, PipelineCall>(\n    Setup,\n    Monitoring,\n    Plugins,\n    Call,\n    Fallback\n)";
    const COMPONENT: &str =
        "@Component(\n    modules = [\n        JacksonModule::class,\n        ApiClientModule::class,\n    ],\n)";
    const RAW_WITH_NEWLINE: &str = "= \"\"\"first\n    second\"\"\"";

    #[test]
    fn layout_normalization_folds_signatures_onto_one_line_and_leaves_string_contents_alone() {
        let cases = [
            (
                PIPELINE,
                "Pipeline<Unit, PipelineCall>(Setup, Monitoring, Plugins, Call, Fallback)",
            ),
            (
                COMPONENT,
                "@Component(modules = [JacksonModule::class, ApiClientModule::class])",
            ),
            ("Map< String , Int >", "Map<String, Int>"),
            ("= \"a  b\"", "= \"a  b\""),
            (RAW_WITH_NEWLINE, RAW_WITH_NEWLINE),
            (
                "limit: Int = if (a > b) 1 else 0",
                "limit: Int = if (a > b) 1 else 0",
            ),
            (
                "ok: Boolean = n >= 0 && m <= 9",
                "ok: Boolean = n >= 0 && m <= 9",
            ),
            ("less: Boolean = a < b", "less: Boolean = a < b"),
            ("block: (Int) -> Unit", "block: (Int) -> Unit"),
            ("fun <T : Any> f(x: List<T>)", "fun <T : Any> f(x: List<T>)"),
        ];

        let observed: Vec<(String, String)> = cases
            .iter()
            .map(|(input, _)| {
                let once = normalize_signature_layout(input);
                let twice = normalize_signature_layout(&once);
                (once, twice)
            })
            .collect();
        let expected: Vec<(String, String)> = cases
            .iter()
            .map(|(_, want)| (want.to_string(), want.to_string()))
            .collect();

        assert_eq!(observed, expected);
    }
}
