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
/// `,`, generics `<`/`>`) is dropped during tokenizing, so the walk sees only the shapes that open,
/// name or close a declaration; `=` is kept because the declaration scan reads it (KT-117).
enum Token {
    Ident(String),
    LBrace,
    RBrace,
    LParen,
    RParen,
    Semi,
    At,
    Arrow,
    /// An `=` sign. The declaration scan (KT-117) reads it as the start of an initializer, which is
    /// what tells a field with a call in its initializer (`Logger log = factory.get();`) from an
    /// abstract method (`void run();`); the enclosing scan (KT-114) ignores it.
    Eq,
    /// A `<` or `>`. The declaration scan (KT-117) tracks the nesting they open so a type-parameter
    /// list before a member name (`<T> Base(...)`) is told from a return type, which is what
    /// separates a generic constructor from a method named like its class; the enclosing scan
    /// (KT-114) ignores them.
    LAngle,
    RAngle,
}

/// One token with the 1-based line it begins on and the byte offset it begins at, so the declaration
/// scan can slice the source for a signature.
struct Spanned {
    token: Token,
    line: u32,
    offset: usize,
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
            Token::Eq | Token::LAngle | Token::RAngle => index += 1,
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
                offset: states[start].0,
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
                offset: states[index].0,
            });
            index += 2;
            continue;
        }
        if let Some(token) = structural_token(character) {
            tokens.push(Spanned {
                token,
                line,
                offset: states[index].0,
            });
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
        '=' => Some(Token::Eq),
        '<' => Some(Token::LAngle),
        '>' => Some(Token::RAngle),
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

/// A Java declaration modifier, which precedes a member's return type and name and so is not counted
/// as a header identifier when a constructor is told from a method.
fn is_modifier(name: &str) -> bool {
    matches!(
        name,
        "public"
            | "private"
            | "protected"
            | "static"
            | "final"
            | "abstract"
            | "synchronized"
            | "native"
            | "strictfp"
            | "default"
            | "transient"
            | "volatile"
            | "sealed"
    )
}

/// The kind of a Java declaration the at-line query reports (KT-117). A Java candidate the engine
/// hands back as a bare `symbol` is given one of these, so `symbols` can show its real kind rather
/// than guessing. `@interface` is an annotation-type declaration, kept distinct from a plain
/// interface because its members are elements, not methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JavaDeclKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
    Method,
    Constructor,
    Field,
}

impl JavaDeclKind {
    /// The word a row shows for this kind, matching how the kind reads in Java source.
    pub fn label(self) -> &'static str {
        match self {
            JavaDeclKind::Class => "class",
            JavaDeclKind::Interface => "interface",
            JavaDeclKind::Enum => "enum",
            JavaDeclKind::Record => "record",
            JavaDeclKind::Annotation => "@interface",
            JavaDeclKind::Method => "method",
            JavaDeclKind::Constructor => "constructor",
            JavaDeclKind::Field => "field",
        }
    }

    /// Whether this kind is a type that can enclose other declarations and name supertypes, so a
    /// subtype scan (KT-116) considers it and a method or field is skipped.
    pub fn is_type(self) -> bool {
        matches!(
            self,
            JavaDeclKind::Class
                | JavaDeclKind::Interface
                | JavaDeclKind::Enum
                | JavaDeclKind::Record
                | JavaDeclKind::Annotation
        )
    }
}

/// One Java declaration located by the pure scan (KT-117): its kind, simple name, the 1-based line
/// its name sits on, the simple names of the types enclosing it (outermost first), the folded header
/// line, and, for a type, the simple names in its `extends`/`implements` clause (KT-116).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaDeclaration {
    pub kind: JavaDeclKind,
    pub name: String,
    pub line: u32,
    pub enclosing: Vec<String>,
    pub signature: String,
    pub supertypes: Vec<String>,
}

/// Every type, method, constructor and field declared in `source`, found by the same pure
/// brace-and-signature walk the enclosing scan uses, over the shared Java lexer so a declaration
/// keyword inside a comment or a string never counts. Nested types compose through the enclosing
/// chain. A field with a call in its initializer is told from an abstract method by the `=` the
/// lexer now keeps. Enum constants and multi-declarator fields past the first are out of scope and
/// may be missed, which only costs a bare row for a candidate the engine points straight at.
pub fn java_declarations(source: &str) -> Vec<JavaDeclaration> {
    let tokens = tokenize(source);
    let mut declarations = Vec::new();
    let mut type_stack: Vec<String> = Vec::new();
    let mut scopes: Vec<ScopeKind> = Vec::new();
    let mut pending = Pending::default();
    let mut paren_depth = 0u32;

    let mut index = 0;
    while index < tokens.len() {
        match &tokens[index].token {
            Token::At => index = step_annotation(&tokens, index, &mut pending),
            Token::Ident(name) => {
                pending.observe_ident(name, tokens[index].offset, tokens[index].line);
                index += 1;
            }
            Token::Eq => {
                pending.observe_assignment();
                index += 1;
            }
            Token::LParen => {
                if paren_depth == 0 {
                    pending.observe_param_list_open();
                }
                paren_depth += 1;
                index += 1;
            }
            Token::RParen => {
                paren_depth = paren_depth.saturating_sub(1);
                index += 1;
            }
            Token::Arrow => {
                pending.saw_arrow = true;
                index += 1;
            }
            Token::LAngle => {
                pending.start_offset.get_or_insert(tokens[index].offset);
                pending.angle_depth += 1;
                index += 1;
            }
            Token::RAngle => {
                pending.angle_depth = pending.angle_depth.saturating_sub(1);
                index += 1;
            }
            Token::LBrace => {
                let brace_offset = tokens[index].offset;
                match pending.classify_block(&type_stack) {
                    Block::Type(kind) => {
                        let name = pending.type_name.clone().unwrap_or_default();
                        let header = header_slice(source, pending.start_offset, brace_offset);
                        declarations.push(JavaDeclaration {
                            kind,
                            name: name.clone(),
                            line: pending.name_line,
                            enclosing: type_stack.clone(),
                            signature: fold_whitespace(&header),
                            supertypes: supertypes_in_header(&header),
                        });
                        type_stack.push(name);
                        scopes.push(ScopeKind::Type);
                    }
                    Block::Member(kind) => {
                        let header = header_slice(source, pending.start_offset, brace_offset);
                        declarations.push(JavaDeclaration {
                            kind,
                            name: pending.method_name.clone().unwrap_or_default(),
                            line: pending.method_name_line,
                            enclosing: type_stack.clone(),
                            signature: fold_whitespace(&header),
                            supertypes: Vec::new(),
                        });
                        scopes.push(ScopeKind::Member);
                    }
                    Block::Anonymous => scopes.push(ScopeKind::Anonymous),
                }
                pending = Pending::default();
                index += 1;
            }
            Token::RBrace => {
                if let Some(ScopeKind::Type) = scopes.pop() {
                    type_stack.pop();
                }
                pending = Pending::default();
                index += 1;
            }
            Token::Semi => {
                if scopes.last() == Some(&ScopeKind::Type) {
                    if let Some(declaration) =
                        pending.classify_statement(&type_stack, source, tokens[index].offset)
                    {
                        declarations.push(declaration);
                    }
                }
                pending = Pending::default();
                index += 1;
            }
        }
    }
    declarations
}

/// The source between a header's first token and its terminating brace, folded onto one line. An
/// absent start (a `{` with nothing before it) yields an empty header.
fn header_slice(source: &str, start: Option<usize>, end: usize) -> String {
    match start {
        Some(start) if start <= end => source[start..end].to_string(),
        _ => String::new(),
    }
}

/// The scopes a `{` can open: a named type, a named member (method or constructor), or an anonymous
/// block (a control block, a lambda, an anonymous class, an array initializer, a static block).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Type,
    Member,
    Anonymous,
}

/// What a `{` reaching the current unit opens.
enum Block {
    Type(JavaDeclKind),
    Member(JavaDeclKind),
    Anonymous,
}

/// What the walk has seen since the last brace, semicolon or annotation: enough to decide what the
/// next `{` or `;` declares. Reset at every boundary so one statement never bleeds into the next.
#[derive(Default)]
struct Pending {
    start_offset: Option<usize>,
    kind: Option<JavaDeclKind>,
    expect_type_name: bool,
    type_name: Option<String>,
    name_line: u32,
    prev_ident: Option<String>,
    prev_ident_line: u32,
    method_name: Option<String>,
    method_name_line: u32,
    field_name: Option<String>,
    field_name_line: u32,
    saw_param_list: bool,
    saw_new: bool,
    saw_arrow: bool,
    saw_assignment: bool,
    angle_depth: u32,
    /// How many identifiers outside modifiers and generic type-parameter lists have been seen before
    /// a member's parameter list. A constructor is just its name, counting one; a method's return
    /// type pushes the count past one, so a name equal to its class is a method, not a constructor,
    /// when a return type precedes it.
    header_ident_count: u32,
}

impl Pending {
    fn observe_ident(&mut self, name: &str, offset: usize, line: u32) {
        self.start_offset.get_or_insert(offset);
        if is_class_keyword(name) {
            self.kind = Some(type_kind(name));
            self.expect_type_name = true;
        } else if self.expect_type_name {
            self.type_name = Some(name.to_string());
            self.name_line = line;
            self.expect_type_name = false;
        } else if name == "new" {
            self.saw_new = true;
        } else if self.angle_depth == 0 && !self.saw_param_list && !is_modifier(name) {
            self.header_ident_count += 1;
        }
        self.prev_ident = Some(name.to_string());
        self.prev_ident_line = line;
    }

    /// At the opening paren of a top-level list, the preceding identifier names a method or
    /// constructor, unless this unit is itself a type (a record's parameter list) or the list
    /// belongs to a `new` expression or a control keyword.
    fn observe_param_list_open(&mut self) {
        if self.type_name.is_some() {
            return;
        }
        self.saw_param_list = true;
        if let Some(ident) = &self.prev_ident {
            if !is_control_keyword(ident) && !self.saw_new {
                self.method_name = Some(ident.clone());
                self.method_name_line = self.prev_ident_line;
            }
        }
    }

    /// At an `=`, the identifier just before it names the field, unless a method header already
    /// claimed this unit. Recording it here is what lets a field whose initializer calls a method
    /// (`Logger log = factory.get();`) keep its own name rather than the called method's.
    fn observe_assignment(&mut self) {
        self.saw_assignment = true;
        if self.field_name.is_none() && self.method_name.is_none() {
            if let Some(ident) = &self.prev_ident {
                self.field_name = Some(ident.clone());
                self.field_name_line = self.prev_ident_line;
            }
        }
    }

    /// What the `{` reaching this unit opens: a type when a type keyword named one, a method or
    /// constructor when a parameter list followed a name directly (not a lambda arrow), else an
    /// anonymous block.
    fn classify_block(&self, type_stack: &[String]) -> Block {
        if let (Some(kind), Some(_)) = (self.kind, &self.type_name) {
            return Block::Type(kind);
        }
        if self.saw_param_list && !self.saw_arrow {
            if let Some(name) = &self.method_name {
                return Block::Member(self.member_kind(name, type_stack));
            }
        }
        Block::Anonymous
    }

    /// The declaration a `;` inside a type body closes, or `None` when it closes a statement that is
    /// not a declaration. A parameter list without an initializer is an abstract or interface method
    /// (or an annotation element); anything else with a name is a field.
    fn classify_statement(
        &self,
        type_stack: &[String],
        source: &str,
        semi_offset: usize,
    ) -> Option<JavaDeclaration> {
        let start = self.start_offset?;
        let header = fold_whitespace(&header_slice(source, Some(start), semi_offset));
        if self.saw_param_list && !self.saw_arrow && !self.saw_assignment {
            let name = self.method_name.clone()?;
            return Some(JavaDeclaration {
                kind: JavaDeclKind::Method,
                name,
                line: self.method_name_line,
                enclosing: type_stack.to_vec(),
                signature: header,
                supertypes: Vec::new(),
            });
        }
        let (name, line) = match (&self.field_name, self.saw_assignment) {
            (Some(name), true) => (name.clone(), self.field_name_line),
            _ => (self.prev_ident.clone()?, self.prev_ident_line),
        };
        Some(JavaDeclaration {
            kind: JavaDeclKind::Field,
            name,
            line,
            enclosing: type_stack.to_vec(),
            signature: header,
            supertypes: Vec::new(),
        })
    }

    /// Whether a member named `name` is a constructor or a method. A constructor shares its class's
    /// simple name and has no return type: after modifiers and an optional generic type-parameter
    /// list, its name is the only header identifier. A method named like its class (`Node Node()`)
    /// carries a return type, so the header holds a second identifier and it stays a method.
    fn member_kind(&self, name: &str, type_stack: &[String]) -> JavaDeclKind {
        let names_enclosing_type = type_stack.last().map(String::as_str) == Some(name);
        if names_enclosing_type && self.header_ident_count <= 1 {
            JavaDeclKind::Constructor
        } else {
            JavaDeclKind::Method
        }
    }
}

/// Advances past the token at the `@` at `index`. `@interface` opens an annotation-type declaration,
/// recorded on the pending unit as a type header beginning at the `@`; every other `@Name(args)` is
/// an annotation written on a declaration and is skipped whole, so it never looks like one.
fn step_annotation(tokens: &[Spanned], index: usize, pending: &mut Pending) -> usize {
    if let Some(Spanned {
        token: Token::Ident(name),
        ..
    }) = tokens.get(index + 1)
    {
        if name == "interface" {
            pending.start_offset.get_or_insert(tokens[index].offset);
            pending.kind = Some(JavaDeclKind::Annotation);
            pending.expect_type_name = true;
            return index + 2;
        }
    }
    skip_annotation_use(tokens, index + 1)
}

/// Advances past an annotation use beginning after its `@`: its name, then its balanced argument
/// list when present.
fn skip_annotation_use(tokens: &[Spanned], mut index: usize) -> usize {
    if let Some(Spanned {
        token: Token::Ident(_),
        ..
    }) = tokens.get(index)
    {
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

fn type_kind(name: &str) -> JavaDeclKind {
    match name {
        "interface" => JavaDeclKind::Interface,
        "enum" => JavaDeclKind::Enum,
        "record" => JavaDeclKind::Record,
        _ => JavaDeclKind::Class,
    }
}

/// Collapses every run of whitespace in `text` to a single space and trims the ends, so a header
/// spread across several source lines reads as one signature line.
fn fold_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The simple names in a type header's `extends`/`implements` clause (KT-116), read over the shared
/// lexer so a keyword in a comment or string never counts. A qualified name `a.b.Foo` reduces to its
/// last segment, a generic `Foo<Bar>` to `Foo` (the argument is dropped), and a `permits` clause ends
/// the supertype list. Several supertypes are separated by top-level commas.
fn supertypes_in_header(header: &str) -> Vec<String> {
    let states = lex(header);
    let chars: Vec<char> = header.chars().collect();
    let mut supertypes = Vec::new();
    let mut in_clause = false;
    let mut angle_depth = 0usize;
    let mut current: Option<String> = None;
    let mut index = 0;
    while index < chars.len() {
        if !states[index].1.is_code() {
            index += 1;
            continue;
        }
        let character = chars[index];
        if is_ident_start(character) {
            let start = index;
            while index < chars.len()
                && states[index].1.is_code()
                && is_ident_continue(chars[index])
            {
                index += 1;
            }
            if angle_depth == 0 {
                let word: String = chars[start..index].iter().collect();
                match word.as_str() {
                    "extends" | "implements" => {
                        push_current(&mut supertypes, &mut current, in_clause);
                        in_clause = true;
                    }
                    "permits" => {
                        push_current(&mut supertypes, &mut current, in_clause);
                        in_clause = false;
                    }
                    _ if in_clause => current = Some(word),
                    _ => {}
                }
            }
            continue;
        }
        match character {
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            ',' if angle_depth == 0 && in_clause => {
                push_current(&mut supertypes, &mut current, true)
            }
            '{' | ';' if angle_depth == 0 => {
                push_current(&mut supertypes, &mut current, in_clause);
                in_clause = false;
            }
            _ => {}
        }
        index += 1;
    }
    push_current(&mut supertypes, &mut current, in_clause);
    supertypes
}

fn push_current(supertypes: &mut Vec<String>, current: &mut Option<String>, in_clause: bool) {
    if let Some(name) = current.take() {
        if in_clause {
            supertypes.push(name);
        }
    }
}

/// The declared `package` of `source`, or `None` when it declares none, read over the shared lexer
/// so a `package` word in a comment never counts.
pub fn java_package(source: &str) -> Option<String> {
    let states = lex(source);
    let chars: Vec<char> = source.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if !states[index].1.is_code() {
            index += 1;
            continue;
        }
        if is_ident_start(chars[index]) {
            let start = index;
            while index < chars.len()
                && states[index].1.is_code()
                && is_ident_continue(chars[index])
            {
                index += 1;
            }
            let word: String = chars[start..index].iter().collect();
            if word == "package" {
                return read_package_name(&chars, &states, index);
            }
            continue;
        }
        index += 1;
    }
    None
}

/// The dotted package name following the `package` keyword, up to its `;`, or `None` when empty.
fn read_package_name(
    chars: &[char],
    states: &[(usize, crate::java_text::LexState)],
    mut index: usize,
) -> Option<String> {
    let mut name = String::new();
    while index < chars.len() {
        if states[index].1.is_code() {
            let character = chars[index];
            if character == ';' {
                break;
            }
            if is_ident_continue(character) || character == '.' {
                name.push(character);
            }
        }
        index += 1;
    }
    (!name.is_empty()).then_some(name)
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

    /// One source carrying every declaration kind the at-line query must name, with the enclosing
    /// chain, line, folded signature and (for a type) extends/implements simple names it reports for
    /// each. A leading annotation, a multi-line class header with a generic bound, a field whose
    /// initializer calls a method, an abstract method, a nested interface, an enum and record with
    /// supertype clauses, and an annotation type with an element all appear, so the walk is proven on
    /// every construct at once. Composed into one map and asserted once.
    #[test]
    fn each_declaration_reports_its_kind_chain_signature_and_supertypes() {
        let source = concat!(
            "package shop.app;\n",                                      // 1
            "import java.util.List;\n",                                 // 2
            "\n",                                                       // 3
            "@Deprecated\n",                                            // 4
            "public abstract class Base<T extends Number>\n",           // 5
            "        extends Parent<T> implements a.b.Alpha, Beta {\n", // 6
            "    private int count = 0;\n",                             // 7
            "    protected final List<String> names;\n",                // 8
            "    static final Logger log = Factory.get(Base.class);\n", // 9
            "    Base() { init(); }\n",                                 // 10
            "    public <R> R pick(T first, R second) {\n",             // 11
            "        return second;\n",                                 // 12
            "    }\n",                                                  // 13
            "    abstract void handle(Request r);\n",                   // 14
            "    interface Inner extends Gamma {\n",                    // 15
            "        void greet();\n",                                  // 16
            "    }\n",                                                  // 17
            "    enum Color implements Hue { RED, GREEN }\n",           // 18
            "    record Point(int x, int y) implements Loc {}\n",       // 19
            "    @interface Marker { String value(); }\n",              // 20
            "}\n",                                                      // 21
        );

        let observed: Vec<(&str, String, u32, String, String)> = java_declarations(source)
            .into_iter()
            .map(|declaration| {
                let mut qualified = declaration.enclosing.clone();
                qualified.push(declaration.name.clone());
                (
                    declaration.kind.label(),
                    qualified.join("."),
                    declaration.line,
                    declaration.signature,
                    declaration.supertypes.join("|"),
                )
            })
            .collect();

        let expected = vec![
            (
                "class",
                "Base".to_string(),
                5,
                "public abstract class Base<T extends Number> extends Parent<T> implements a.b.Alpha, Beta".to_string(),
                "Parent|Alpha|Beta".to_string(),
            ),
            ("field", "Base.count".to_string(), 7, "private int count = 0".to_string(), String::new()),
            ("field", "Base.names".to_string(), 8, "protected final List<String> names".to_string(), String::new()),
            (
                "field",
                "Base.log".to_string(),
                9,
                "static final Logger log = Factory.get(Base.class)".to_string(),
                String::new(),
            ),
            ("constructor", "Base.Base".to_string(), 10, "Base()".to_string(), String::new()),
            ("method", "Base.pick".to_string(), 11, "public <R> R pick(T first, R second)".to_string(), String::new()),
            ("method", "Base.handle".to_string(), 14, "abstract void handle(Request r)".to_string(), String::new()),
            ("interface", "Base.Inner".to_string(), 15, "interface Inner extends Gamma".to_string(), "Gamma".to_string()),
            ("method", "Base.Inner.greet".to_string(), 16, "void greet()".to_string(), String::new()),
            ("enum", "Base.Color".to_string(), 18, "enum Color implements Hue".to_string(), "Hue".to_string()),
            ("record", "Base.Point".to_string(), 19, "record Point(int x, int y) implements Loc".to_string(), "Loc".to_string()),
            ("@interface", "Base.Marker".to_string(), 20, "@interface Marker".to_string(), String::new()),
            ("method", "Base.Marker.value".to_string(), 20, "String value()".to_string(), String::new()),
        ];

        assert_eq!(observed, expected);
    }

    /// A member whose name equals its class is a constructor only when no return type precedes it.
    /// A bare name and a name after a generic type-parameter list are constructors; the same name
    /// with a return type in front is a method. Were that method mislabelled a constructor,
    /// `fold_java_constructors` would fold it into the class and drop a real declaration, so the kind
    /// and folded signature the scan reports for all four declarations are asserted at once.
    #[test]
    fn a_member_named_like_its_class_is_a_constructor_only_without_a_return_type() {
        let source = concat!(
            "class Node {\n",               // 1
            "    Node() { }\n",             // 2
            "    <T> Node(T seed) { }\n",   // 3
            "    Node Node(int depth) {\n", // 4
            "        return this;\n",       // 5
            "    }\n",                      // 6
            "}\n",                          // 7
        );

        let observed: Vec<(&str, String, u32)> = java_declarations(source)
            .into_iter()
            .filter(|declaration| declaration.name == "Node")
            .map(|declaration| {
                (
                    declaration.kind.label(),
                    declaration.signature,
                    declaration.line,
                )
            })
            .collect();

        let expected = vec![
            ("class", "class Node".to_string(), 1),
            ("constructor", "Node()".to_string(), 2),
            ("constructor", "<T> Node(T seed)".to_string(), 3),
            ("method", "Node Node(int depth)".to_string(), 4),
        ];

        assert_eq!(observed, expected);
    }

    #[test]
    fn the_package_is_read_over_the_lexer_and_absent_when_undeclared() {
        let declared =
            java_package("// package wrong;\npackage com.example.billing ;\nclass A {}\n");
        let bare = java_package("class A {}\n");

        assert_eq!(
            (declared, bare),
            (Some("com.example.billing".to_string()), None)
        );
    }
}
