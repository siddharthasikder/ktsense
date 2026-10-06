//! A pure, parser-free scan that names the Java declaration enclosing a source line (KT-114).
//!
//! A `## Java text references` listing reads like a KT-102 grep hit only when each site carries the
//! `Type.method` it sits in. tree-sitter Kotlin cannot parse Java, so KT-112 scans Java as text; this
//! module adds the attribution that scan lacked, computed the same honest way: a brace-depth walk
//! over the shared Java lexer ([`crate::java_text`]) rather than a real parser, so it stays pure
//! string processing in `ktsense-core`.
//!
//! The walk tracks a stack of brace scopes. A `{` that follows a class, interface, enum or record
//! header opens a named type scope; a `{` that follows a method or constructor header (an identifier,
//! then a parameter list, then the brace, with the control keywords `if`, `for`, `while`, `switch`,
//! `catch`, `synchronized` and `try` excluded, and `new` or `->` marking an anonymous class or a
//! lambda) opens a named member scope; every other `{` opens an anonymous scope that contributes
//! nothing, so a lambda, an anonymous class, a control block and a static initializer all read as
//! their enclosing method or type. Nested types compose through the stack, so a site reads
//! `Outer.Inner.method`. Annotation argument lists are skipped whole, so `@Component(modules = {...})`
//! never looks like a block.
//!
//! A line is attributed to the innermost scope whose brace range contains it, so a one-line method
//! body attributes to the method and a blank line between two methods attributes to their class. A
//! line inside no scope (an import or the package statement) has no enclosing declaration.

use crate::java_text::lex;

/// The enclosing declaration of each requested 1-based `line` in `source`, as a dotted
/// `Type.method` path, or `None` when the line sits outside every declaration (an import or the
/// package statement). The returned vector is aligned to `lines`.
pub fn java_enclosing_declarations(source: &str, lines: &[u32]) -> Vec<Option<String>> {
    let intervals = enclosing_intervals(source);
    lines
        .iter()
        .map(|&line| innermost_path(&intervals, line))
        .collect()
}

/// A brace scope that has closed: the lines its braces spanned and the dotted path of named scopes
/// enclosing it (including itself when it is named).
struct Interval {
    open_line: u32,
    close_line: u32,
    path: Option<String>,
}

/// The path of the innermost closed scope whose brace range contains `line`. Nested scopes open
/// later, so the containing scope with the greatest `open_line` is the innermost one.
fn innermost_path(intervals: &[Interval], line: u32) -> Option<String> {
    intervals
        .iter()
        .filter(|interval| interval.open_line <= line && line <= interval.close_line)
        .max_by_key(|interval| interval.open_line)
        .and_then(|interval| interval.path.clone())
}

/// One lexical token the structure walk cares about. Punctuation that does not shape scope (`.`,
/// `,`, `=`, generics `<`/`>`) is dropped during tokenizing, so the walk sees only the shapes that
/// open, name or close a declaration.
enum Token {
    Ident(String),
    LBrace,
    RBrace,
    LParen,
    RParen,
    Semi,
    At,
    Arrow,
}

/// One token with the 1-based line it begins on.
struct Spanned {
    token: Token,
    line: u32,
}

/// Walks `source` and returns every brace scope that opened, as a closed [`Interval`]. A scope left
/// open at end of input is closed at the last line, so a truncated or partial file still attributes
/// its sites rather than losing them.
fn enclosing_intervals(source: &str) -> Vec<Interval> {
    let tokens = tokenize(source);
    let last_line = source.lines().count().max(1) as u32;

    let mut intervals = Vec::new();
    let mut named_stack: Vec<String> = Vec::new();
    let mut open: Vec<OpenScope> = Vec::new();
    let mut unit = Unit::default();
    let mut paren_depth = 0u32;

    let mut index = 0;
    while index < tokens.len() {
        match &tokens[index].token {
            Token::At => index = skip_annotation(&tokens, index + 1, &mut unit),
            Token::Ident(name) => {
                unit.observe_ident(name);
                index += 1;
            }
            Token::LParen => {
                if paren_depth == 0 {
                    unit.observe_param_list_open();
                }
                paren_depth += 1;
                index += 1;
            }
            Token::RParen => {
                paren_depth = paren_depth.saturating_sub(1);
                index += 1;
            }
            Token::Arrow => {
                unit.saw_arrow = true;
                index += 1;
            }
            Token::LBrace => {
                let named = unit.scope_name(paren_depth);
                if let Some(name) = &named {
                    named_stack.push(name.clone());
                }
                let path = (!named_stack.is_empty()).then(|| named_stack.join("."));
                open.push(OpenScope {
                    open_line: tokens[index].line,
                    named: named.is_some(),
                    path,
                });
                unit = Unit::default();
                index += 1;
            }
            Token::RBrace => {
                if let Some(scope) = open.pop() {
                    if scope.named {
                        named_stack.pop();
                    }
                    intervals.push(Interval {
                        open_line: scope.open_line,
                        close_line: tokens[index].line,
                        path: scope.path,
                    });
                }
                unit = Unit::default();
                index += 1;
            }
            Token::Semi => {
                unit = Unit::default();
                index += 1;
            }
        }
    }

    while let Some(scope) = open.pop() {
        if scope.named {
            named_stack.pop();
        }
        intervals.push(Interval {
            open_line: scope.open_line,
            close_line: last_line,
            path: scope.path,
        });
    }
    intervals
}

/// A brace scope still open: where it opened, whether it names a declaration, and the dotted path it
/// carries.
struct OpenScope {
    open_line: u32,
    named: bool,
    path: Option<String>,
}

/// What the walk has seen since the last brace, semicolon or annotation: enough to decide what the
/// next `{` opens. Reset at every boundary so one statement never bleeds into the next.
#[derive(Default)]
struct Unit {
    class_name: Option<String>,
    method_name: Option<String>,
    prev_ident: Option<String>,
    expect_class_name: bool,
    saw_new: bool,
    saw_arrow: bool,
}

impl Unit {
    fn observe_ident(&mut self, name: &str) {
        if is_class_keyword(name) {
            self.expect_class_name = true;
        } else if self.expect_class_name {
            self.class_name = Some(name.to_string());
            self.expect_class_name = false;
        } else if name == "new" {
            self.saw_new = true;
        }
        self.prev_ident = Some(name.to_string());
    }

    /// At the opening paren of a top-level parameter list, the preceding identifier names the method
    /// or constructor, unless it is a control keyword or the list belongs to a `new` expression.
    fn observe_param_list_open(&mut self) {
        if self.class_name.is_some() {
            return;
        }
        if let Some(ident) = &self.prev_ident {
            if !is_control_keyword(ident) && !self.saw_new {
                self.method_name = Some(ident.clone());
            }
        }
    }

    /// The name a `{` reaching this unit opens, or `None` for an anonymous scope. A class header wins
    /// over a method name (so a record's parameter list does not mask it); a method header names the
    /// scope only when the brace follows its parameter list directly, not a lambda arrow.
    fn scope_name(&self, paren_depth: u32) -> Option<String> {
        if let Some(class_name) = &self.class_name {
            return Some(class_name.clone());
        }
        if paren_depth == 0 && !self.saw_arrow {
            return self.method_name.clone();
        }
        None
    }
}

/// Advances past an annotation beginning after its `@`: skips the annotation name, then its balanced
/// argument list when present, so `@Component(modules = {...})` cannot look like a declaration. The
/// annotation-type declaration `@interface` is the one exception, recorded as a class header.
fn skip_annotation(tokens: &[Spanned], mut index: usize, unit: &mut Unit) -> usize {
    if let Some(Spanned {
        token: Token::Ident(name),
        ..
    }) = tokens.get(index)
    {
        if name == "interface" {
            unit.expect_class_name = true;
            return index + 1;
        }
        index += 1;
    }
    if let Some(Spanned {
        token: Token::LParen,
        ..
    }) = tokens.get(index)
    {
        let mut depth = 0u32;
        while index < tokens.len() {
            match tokens[index].token {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        return index + 1;
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }
    index
}

/// Splits `source` into the tokens the structure walk needs, over the Java lexer so a brace, paren
/// or identifier inside a comment or a literal is never seen as code. Line numbers count every
/// character, including those in multi-line comments and text blocks, so a token's line is accurate.
fn tokenize(source: &str) -> Vec<Spanned> {
    let states = lex(source);
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut line = 1u32;
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '\n' {
            line += 1;
            index += 1;
            continue;
        }
        if !states[index].1.is_code() || character.is_whitespace() {
            index += 1;
            continue;
        }
        if is_ident_start(character) {
            let start = index;
            while index < chars.len()
                && states[index].1.is_code()
                && is_ident_continue(chars[index])
            {
                index += 1;
            }
            let name: String = chars[start..index].iter().collect();
            tokens.push(Spanned {
                token: Token::Ident(name),
                line,
            });
            continue;
        }
        if character == '-'
            && chars.get(index + 1) == Some(&'>')
            && states.get(index + 1).is_some_and(|state| state.1.is_code())
        {
            tokens.push(Spanned {
                token: Token::Arrow,
                line,
            });
            index += 2;
            continue;
        }
        if let Some(token) = structural_token(character) {
            tokens.push(Spanned { token, line });
        }
        index += 1;
    }
    tokens
}

/// The scope-shaping token a single character stands for, or `None` for punctuation the walk ignores.
fn structural_token(character: char) -> Option<Token> {
    match character {
        '{' => Some(Token::LBrace),
        '}' => Some(Token::RBrace),
        '(' => Some(Token::LParen),
        ')' => Some(Token::RParen),
        ';' => Some(Token::Semi),
        '@' => Some(Token::At),
        _ => None,
    }
}

fn is_ident_start(character: char) -> bool {
    character.is_alphabetic() || character == '_' || character == '$'
}

fn is_ident_continue(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

fn is_class_keyword(name: &str) -> bool {
    matches!(name, "class" | "interface" | "enum" | "record")
}

fn is_control_keyword(name: &str) -> bool {
    matches!(
        name,
        "if" | "for" | "while" | "switch" | "catch" | "synchronized" | "try"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One source carrying every construct the scan must get right, and the enclosing declaration it
    /// reports for each line. Annotations with argument braces, a generic bound, a throws clause and
    /// a multi-line signature must not derail header detection; a static initializer and a default
    /// interface method must be named by their type; a control block, a block lambda and the braces
    /// of an anonymous class must read as their enclosing method, while a real method inside that
    /// anonymous class is still named; nested and inner types compose. The brace line of a method is
    /// inside it; the signature lines above the brace are not. Asserted as one line-to-enclosing map.
    #[test]
    fn each_line_reads_its_enclosing_java_declaration() {
        let source = concat!(
            "package shop;\n",                                        // 1
            "import java.util.List;\n",                               // 2
            "@Component(modules = { Dagger.class })\n",               // 3
            "public class Outer {\n",                                 // 4
            "    private int count = 0;\n",                           // 5
            "    static { count = seed(); }\n",                       // 6
            "    @Override\n",                                        // 7
            "    public <T extends Number> T pick(\n",                // 8
            "            T first,\n",                                 // 9
            "            T second) throws IllegalStateException {\n", // 10
            "        if (first == null) {\n",                         // 11
            "            return second;\n",                           // 12
            "        }\n",                                            // 13
            "        Runnable r = new Runnable() {\n",                // 14
            "            public void run() { log(first); }\n",        // 15
            "        };\n",                                           // 16
            "        List.of(first).forEach(x -> {\n",                // 17
            "            consume(x);\n",                              // 18
            "        });\n",                                          // 19
            "        return first;\n",                                // 20
            "    }\n",                                                // 21
            "    interface Inner {\n",                                // 22
            "        default void greet() { say(\"hi\"); }\n",        // 23
            "    }\n",                                                // 24
            "    static class Nested {\n",                            // 25
            "        Nested() { init(); }\n",                         // 26
            "    }\n",                                                // 27
            "}\n",                                                    // 28
        );
        let lines: Vec<u32> = (1..=28).collect();

        let observed: Vec<(u32, Option<String>)> = lines
            .iter()
            .copied()
            .zip(java_enclosing_declarations(source, &lines))
            .collect();

        let enclosing = |line: u32| -> Option<&'static str> {
            match line {
                1..=3 => None,
                4 | 5 | 6 | 7 | 8 | 9 | 28 => Some("Outer"),
                10 | 11 | 12 | 13 | 14 | 16 | 17 | 18 | 19 | 20 | 21 => Some("Outer.pick"),
                15 => Some("Outer.pick.run"),
                22 | 24 => Some("Outer.Inner"),
                23 => Some("Outer.Inner.greet"),
                25 | 27 => Some("Outer.Nested"),
                26 => Some("Outer.Nested.Nested"),
                _ => None,
            }
        };
        let expected: Vec<(u32, Option<String>)> = lines
            .iter()
            .map(|&line| (line, enclosing(line).map(str::to_string)))
            .collect();

        assert_eq!(observed, expected);
    }
}
