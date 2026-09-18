//! The skeleton model: what ktsense knows about a source file once bodies are thrown away.
//!
//! These are plain values. Nothing here reads a file, spawns a process, or touches a parser; the
//! `ktsense-syntax` adapter builds them from a tree-sitter tree and the renderers turn them back
//! into text. That separation is what lets every rendering rule be tested from hand-built values.
//!
//! Deliberate elisions, so a reader does not mistake them for gaps:
//!
//! - Function and property bodies, and initializer expressions, are dropped. That is the point.
//! - An enum entry's constructor arguments are values, so they are part of the body and go too:
//!   `VIEWER(0)` renders as `VIEWER`.
//! - Annotations are dropped. An outline answers "what can I call and what does it return"; the
//!   annotation set is a separate question, and printing it doubles the width of every line.

use serde::{Deserialize, Serialize};

/// One source file reduced to its declarations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSkeleton {
    /// Path as the user will see it: relative to the workspace root, never absolute, never a URI.
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declarations: Vec<Declaration>,
}

impl FileSkeleton {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            package: None,
            imports: Vec::new(),
            declarations: Vec::new(),
        }
    }

    pub fn in_package(mut self, package: impl Into<String>) -> Self {
        self.package = Some(package.into());
        self
    }

    pub fn with_imports(mut self, imports: Vec<String>) -> Self {
        self.imports = imports;
        self
    }

    pub fn with_declarations(mut self, declarations: Vec<Declaration>) -> Self {
        self.declarations = declarations;
        self
    }

    /// Total declarations including nested ones, which is what a caller reports as "N symbols".
    pub fn declaration_count(&self) -> usize {
        fn count(declarations: &[Declaration]) -> usize {
            declarations
                .iter()
                .map(|declaration| 1 + count(&declaration.children))
                .sum()
        }
        count(&self.declarations)
    }
}

/// A single declaration, possibly containing others.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    pub kind: DeclKind,
    /// Empty for a declaration that has no name of its own, such as an unnamed companion object.
    pub name: String,
    /// 1-based line in the source file, so output can point an agent at it.
    pub line: u32,
    #[serde(default, skip_serializing_if = "Visibility::is_public")]
    pub visibility: Visibility,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<Modifier>,
    /// Rendered as written, for example `<T : Any>` or `<out T>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_parameters: Option<String>,
    /// Function parameters, or a class's primary constructor parameters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
    /// Visibility of a class's primary constructor when it differs from public.
    ///
    /// Without this a `class X private constructor(...)` renders as though `X(...)` were callable,
    /// which is exactly the kind of confident wrong answer the accuracy rule forbids.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constructor_visibility: Option<Visibility>,
    /// Absent means Unit for a function, or an inferred type for a property.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supertypes: Vec<String>,
    /// A trailing constraint clause without the keyword, for example `T : Any`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_constraints: Option<String>,
    /// First sentence of the KDoc, already stripped of `/**`, `*` and `*/`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Declaration>,
}

impl Declaration {
    pub fn new(kind: DeclKind, name: impl Into<String>, line: u32) -> Self {
        Self {
            kind,
            name: name.into(),
            line,
            visibility: Visibility::Public,
            modifiers: Vec::new(),
            type_parameters: None,
            parameters: Vec::new(),
            constructor_visibility: None,
            return_type: None,
            supertypes: Vec::new(),
            type_constraints: None,
            doc: None,
            children: Vec::new(),
        }
    }

    pub fn class(name: impl Into<String>, line: u32) -> Self {
        Self::new(DeclKind::Class, name, line)
    }

    pub fn interface(name: impl Into<String>, line: u32) -> Self {
        Self::new(DeclKind::Interface, name, line)
    }

    pub fn object(name: impl Into<String>, line: u32) -> Self {
        Self::new(DeclKind::Object, name, line)
    }

    pub fn function(name: impl Into<String>, line: u32) -> Self {
        Self::new(DeclKind::Function, name, line)
    }

    pub fn val_property(name: impl Into<String>, line: u32) -> Self {
        Self::new(DeclKind::Val, name, line)
    }

    pub fn var_property(name: impl Into<String>, line: u32) -> Self {
        Self::new(DeclKind::Var, name, line)
    }

    pub fn with_visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }

    pub fn with_modifiers(mut self, modifiers: Vec<Modifier>) -> Self {
        self.modifiers = modifiers;
        self
    }

    pub fn with_type_parameters(mut self, type_parameters: impl Into<String>) -> Self {
        self.type_parameters = Some(type_parameters.into());
        self
    }

    pub fn with_parameters(mut self, parameters: Vec<Parameter>) -> Self {
        self.parameters = parameters;
        self
    }

    pub fn with_constructor_visibility(mut self, visibility: Visibility) -> Self {
        self.constructor_visibility = Some(visibility);
        self
    }

    pub fn returning(mut self, return_type: impl Into<String>) -> Self {
        self.return_type = Some(return_type.into());
        self
    }

    pub fn extending(mut self, supertypes: Vec<String>) -> Self {
        self.supertypes = supertypes;
        self
    }

    pub fn constrained_by(mut self, type_constraints: impl Into<String>) -> Self {
        self.type_constraints = Some(type_constraints.into());
        self
    }

    pub fn documented(mut self, doc: impl Into<String>) -> Self {
        self.doc = Some(doc.into());
        self
    }

    pub fn containing(mut self, children: Vec<Declaration>) -> Self {
        self.children = children;
        self
    }

    /// True when this declaration can hold others, which decides whether braces are rendered.
    pub fn is_container(&self) -> bool {
        matches!(
            self.kind,
            DeclKind::Class | DeclKind::Interface | DeclKind::Object
        )
    }
}

/// The base keyword of a declaration. Variations such as `data`, `sealed` and `companion` are
/// modifiers rather than kinds, so `enum class` is [`DeclKind::Class`] plus [`Modifier::Enum`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclKind {
    Class,
    Interface,
    Object,
    Function,
    Val,
    Var,
    TypeAlias,
    EnumEntry,
    Constructor,
}

impl DeclKind {
    /// The keyword as it appears in source. Empty for kinds that are spelled by their modifiers or
    /// carry no keyword of their own.
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Object => "object",
            Self::Function => "fun",
            Self::Val => "val",
            Self::Var => "var",
            Self::TypeAlias => "typealias",
            // A constructor's keyword is its name, so it arrives as the name and the kind
            // contributes no keyword of its own. That keeps `constructor()` from rendering as
            // `constructor ()`.
            Self::Constructor => "",
            Self::EnumEntry => "",
        }
    }

    /// Whether the declaration prints a parameter list even when it has none.
    ///
    /// `fun start()` keeps its empty parentheses because they are part of how it is called;
    /// `class Empty` drops them, because a class with no primary constructor has none.
    pub fn takes_parentheses(self) -> bool {
        matches!(self, Self::Function | Self::Constructor)
    }

    /// Whether type parameters print before the name rather than after it.
    ///
    /// Kotlin writes `fun <T> List<T>.first()` but `class Box<T>`, so the position depends on the
    /// kind rather than being a single rule.
    pub fn type_parameters_precede_name(self) -> bool {
        matches!(self, Self::Function | Self::Constructor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    #[default]
    Public,
    Internal,
    Protected,
    Private,
}

impl Visibility {
    /// Public is the Kotlin default and is never printed, so it is also skipped in JSON.
    pub fn is_public(&self) -> bool {
        matches!(self, Self::Public)
    }

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Public => "",
            Self::Internal => "internal",
            Self::Protected => "protected",
            Self::Private => "private",
        }
    }
}

/// Declaration modifiers.
///
/// Variant order is the canonical render order from the Kotlin coding conventions. The renderer
/// sorts by it, so output does not depend on the order the parser happened to see them in and two
/// files that differ only in modifier order produce the same skeleton.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    Expect,
    Actual,
    Final,
    Open,
    Abstract,
    Sealed,
    Const,
    External,
    Override,
    Lateinit,
    Tailrec,
    Vararg,
    Suspend,
    Inner,
    Enum,
    Annotation,
    Fun,
    Companion,
    Inline,
    Value,
    Infix,
    Operator,
    Data,
}

impl Modifier {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Expect => "expect",
            Self::Actual => "actual",
            Self::Final => "final",
            Self::Open => "open",
            Self::Abstract => "abstract",
            Self::Sealed => "sealed",
            Self::Const => "const",
            Self::External => "external",
            Self::Override => "override",
            Self::Lateinit => "lateinit",
            Self::Tailrec => "tailrec",
            Self::Vararg => "vararg",
            Self::Suspend => "suspend",
            Self::Inner => "inner",
            Self::Enum => "enum",
            Self::Annotation => "annotation",
            Self::Fun => "fun",
            Self::Companion => "companion",
            Self::Inline => "inline",
            Self::Value => "value",
            Self::Infix => "infix",
            Self::Operator => "operator",
            Self::Data => "data",
        }
    }
}

/// A function parameter, or a primary-constructor parameter that also declares a property.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parameter {
    pub name: String,
    pub type_name: String,
    /// Kept verbatim because the fact that a default exists, and what it is, changes how an agent
    /// calls the function. This is the one piece of body-like text an outline keeps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Present when the parameter also declares a property, as in `private val repo: Repo`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub property: Option<ParameterProperty>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vararg: bool,
}

impl Parameter {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            type_name: type_name.into(),
            default: None,
            property: None,
            vararg: false,
        }
    }

    pub fn defaulting_to(mut self, default: impl Into<String>) -> Self {
        self.default = Some(default.into());
        self
    }

    pub fn declaring_property(mut self, visibility: Visibility, mutable: bool) -> Self {
        self.property = Some(ParameterProperty {
            visibility,
            mutable,
        });
        self
    }

    pub fn variadic(mut self) -> Self {
        self.vararg = true;
        self
    }
}

/// The `val`/`var` half of a primary-constructor parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterProperty {
    #[serde(default, skip_serializing_if = "Visibility::is_public")]
    pub visibility: Visibility,
    pub mutable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaration_count_includes_nested_declarations() {
        let file = FileSkeleton::new("app/Sample.kt").with_declarations(vec![
            Declaration::class("Outer", 1).containing(vec![
                Declaration::function("member", 2),
                Declaration::object("", 3).containing(vec![Declaration::val_property("ID", 4)]),
            ]),
            Declaration::function("topLevel", 8),
        ]);

        assert_eq!(file.declaration_count(), 5);
    }

    #[test]
    fn modifier_order_is_canonical_regardless_of_input_order() {
        let mut modifiers = vec![Modifier::Suspend, Modifier::Override, Modifier::Data];
        modifiers.sort();

        assert_eq!(
            modifiers,
            vec![Modifier::Override, Modifier::Suspend, Modifier::Data]
        );
    }
}
