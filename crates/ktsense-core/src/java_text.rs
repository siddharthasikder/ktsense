//! A pure, parser-free classifier for sites in Java source (KT-112).
//!
//! `trace` and `context` resolve Kotlin only, so when a mixed workspace holds `.java` sources the
//! only evidence of a name's use in them is a text scan. tree-sitter Kotlin cannot parse Java, so a
//! Java hit cannot be classified the way [`crate::references::SiteKind`] sites in Kotlin are
//! (`ktsense_syntax::classify_reference_sites`). This module supplies the one thing that
//! classification needs for honesty: whether a hit sits in code, a comment, or a string, so a
//! mention in Java prose is counted apart from a use in Java code exactly as it is for Kotlin.
//!
//! It is deliberately a small lexer, not a parser: it tracks only the lexical states Java shares
//! with every C-family language, so it stays pure string processing and lives in `ktsense-core`
//! rather than pulling a grammar into the pure crate. Line comments (`//`), block comments
//! (`/* */`, including Javadoc `/** */`), string literals, text blocks (`"""`) and character
//! literals are recognised; everything else is code. A site is reported `Code` when its position
//! cannot be located, so a classification gap can never hide a real reference, the same rule the
//! Kotlin classifier follows.

use crate::references::SiteKind;

/// Classifies each `(line, column)` site in `source` by the lexical state it falls in, returning one
/// [`SiteKind`] per input site in order. `line` is 1-based; `column` is the 1-based UTF-16 column
/// `word_bounded_matches` produces, the same coordinate `ktsense_syntax::classify_reference_sites`
/// takes for Kotlin, so the Java and Kotlin scans speak one coordinate system. A site whose position
/// does not resolve to a character is `Code`.
pub fn classify_java_sites(source: &str, sites: &[(u32, u32)]) -> Vec<SiteKind> {
    let wanted: std::collections::HashMap<usize, usize> = sites
        .iter()
        .enumerate()
        .filter_map(|(index, &(line, column))| {
            byte_offset_of(source, line, column).map(|offset| (offset, index))
        })
        .collect();

    let mut kinds = vec![SiteKind::Code; sites.len()];
    for (offset, state) in lex(source) {
        if let Some(&index) = wanted.get(&offset) {
            kinds[index] = state.site_kind();
        }
    }
    kinds
}

/// The lexical state a character sits in. Only the distinctions honesty needs: code, the two comment
/// forms, and the three literal forms that carry text a name can hide in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LexState {
    Code,
    LineComment,
    BlockComment,
    StringLiteral,
    TextBlock,
    CharLiteral,
}

impl LexState {
    /// Whether a character in this state is Java code rather than a comment or a literal, so the
    /// enclosing scan (KT-114) can walk structure over the same lexer this classifier uses.
    pub(crate) fn is_code(self) -> bool {
        matches!(self, LexState::Code)
    }

    fn site_kind(self) -> SiteKind {
        match self {
            LexState::Code => SiteKind::Code,
            LexState::LineComment | LexState::BlockComment => SiteKind::Comment,
            LexState::StringLiteral | LexState::TextBlock | LexState::CharLiteral => {
                SiteKind::String
            }
        }
    }
}

/// Walks `source` once, yielding each character's byte offset and the lexical state it falls in. The
/// state reported for a character is the state active at its first byte, so an identifier inside a
/// string reports [`LexState::StringLiteral`] and one in code reports [`LexState::Code`].
pub(crate) fn lex(source: &str) -> Vec<(usize, LexState)> {
    let chars: Vec<(usize, char)> = source.char_indices().collect();
    let mut classified = Vec::with_capacity(chars.len());
    let peek = |index: usize| chars.get(index).map(|&(_, character)| character);

    let mut state = LexState::Code;
    let mut index = 0;
    while index < chars.len() {
        let (offset, character) = chars[index];
        classified.push((offset, state));
        match state {
            LexState::Code => {
                if character == '/' && peek(index + 1) == Some('/') {
                    classified.push((chars[index + 1].0, state));
                    state = LexState::LineComment;
                    index += 2;
                    continue;
                }
                if character == '/' && peek(index + 1) == Some('*') {
                    classified.push((chars[index + 1].0, state));
                    state = LexState::BlockComment;
                    index += 2;
                    continue;
                }
                if character == '"' && peek(index + 1) == Some('"') && peek(index + 2) == Some('"')
                {
                    classified.push((chars[index + 1].0, LexState::TextBlock));
                    classified.push((chars[index + 2].0, LexState::TextBlock));
                    state = LexState::TextBlock;
                    index += 3;
                    continue;
                }
                if character == '"' {
                    state = LexState::StringLiteral;
                } else if character == '\'' {
                    state = LexState::CharLiteral;
                }
                index += 1;
            }
            LexState::LineComment => {
                if character == '\n' {
                    state = LexState::Code;
                }
                index += 1;
            }
            LexState::BlockComment => {
                if character == '*' && peek(index + 1) == Some('/') {
                    classified.push((chars[index + 1].0, state));
                    state = LexState::Code;
                    index += 2;
                    continue;
                }
                index += 1;
            }
            LexState::StringLiteral => {
                if character == '\\' {
                    if let Some(&(next_offset, _)) = chars.get(index + 1) {
                        classified.push((next_offset, state));
                    }
                    index += 2;
                    continue;
                }
                if character == '"' {
                    state = LexState::Code;
                }
                index += 1;
            }
            LexState::CharLiteral => {
                if character == '\\' {
                    if let Some(&(next_offset, _)) = chars.get(index + 1) {
                        classified.push((next_offset, state));
                    }
                    index += 2;
                    continue;
                }
                if character == '\'' {
                    state = LexState::Code;
                }
                index += 1;
            }
            LexState::TextBlock => {
                if character == '"' && peek(index + 1) == Some('"') && peek(index + 2) == Some('"')
                {
                    classified.push((chars[index + 1].0, state));
                    classified.push((chars[index + 2].0, state));
                    state = LexState::Code;
                    index += 3;
                    continue;
                }
                index += 1;
            }
        }
    }
    classified
}

/// The byte offset in `source` of the character at 1-based `line` and 1-based UTF-16 `column`, or
/// `None` when the line or column runs past the text. The column is counted in UTF-16 code units so
/// a multi-byte character before the match shifts it exactly as the scan that produced it counted.
fn byte_offset_of(source: &str, line: u32, column: u32) -> Option<usize> {
    if line == 0 || column == 0 {
        return None;
    }
    let line_start = line_start_offset(source, line)?;
    let line_text = source[line_start..].split('\n').next().unwrap_or("");
    let mut utf16_seen = 0u32;
    for (byte, character) in line_text.char_indices() {
        if utf16_seen + 1 == column {
            return Some(line_start + byte);
        }
        utf16_seen += character.len_utf16() as u32;
    }
    None
}

/// The byte offset of the first character of 1-based `line`, or `None` when the source has fewer
/// lines.
fn line_start_offset(source: &str, line: u32) -> Option<usize> {
    if line == 1 {
        return Some(0);
    }
    let mut remaining = line - 1;
    for (byte, character) in source.char_indices() {
        if character == '\n' {
            remaining -= 1;
            if remaining == 0 {
                return Some(byte + 1);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One table covering every state the classifier must separate: a bare call in code, a name
    /// after a dot, a name in a line comment and a block comment, a name in a string literal and in
    /// a text block, and a name in code on a line that also carries a trailing comment. The UTF-16
    /// column accounts for a multi-byte character before a match. Sites are given in the coordinate
    /// `word_bounded_matches` produces so the two scans share one contract.
    #[test]
    fn each_site_is_classified_by_the_lexical_state_it_falls_in() {
        let source = concat!(
            "int x = executeUpdate(1);\n",                  // 1: code at col 9
            "// executeUpdate here\n",                      // 2: comment at col 4
            "/* executeUpdate */\n",                        // 3: block comment at col 4
            "String s = \"executeUpdate\";\n",              // 4: string at col 13
            "String t = \"\"\"\nexecuteUpdate\n\"\"\";\n",  // 5-7: text block, name at line 6 col 1
            "int y = executeUpdate(2); // executeUpdate\n", // 8: code at col 9, comment at col 30
            "int é = executeUpdate(3);\n", // 9: code at col 9 (é is one UTF-16 unit)
        );
        let sites = [
            (1, 9),
            (2, 4),
            (3, 4),
            (4, 13),
            (6, 1),
            (8, 9),
            (8, 30),
            (9, 9),
        ];

        let observed = classify_java_sites(source, &sites);

        assert_eq!(
            observed,
            vec![
                SiteKind::Code,
                SiteKind::Comment,
                SiteKind::Comment,
                SiteKind::String,
                SiteKind::String,
                SiteKind::Code,
                SiteKind::Comment,
                SiteKind::Code,
            ]
        );
    }
}
