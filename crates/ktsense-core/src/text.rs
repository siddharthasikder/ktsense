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
