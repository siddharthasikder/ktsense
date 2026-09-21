//! Turns a Kotlin CST into [`FileSkeleton`] values.
//!
//! Written against the grammar `brokk-tree-sitter-kotlin` 0.4 actually produces, dumped with
//! `cargo run -p ktsense-syntax --example dump_cst`. Three of its properties shape everything here
//! and are not what a reader of the tree-sitter docs would assume:
//!
//! 1. The grammar exposes almost no field names. Only `receiver_type` carries one, so navigation is
//!    by node kind and sibling order rather than `child_by_field_name`.
//! 2. In `function_value_parameters`, a parameter's default value is a *sibling* that follows the
//!    `parameter` node, and `vararg` is a `parameter_modifiers` sibling that *precedes* it. In
//!    `class_parameter` the same default is a child instead. Association is positional.
//! 3. A property's `getter` is a sibling of `property_declaration` inside the class body, not a
//!    child of it, so a body walk must skip `getter` and `setter` or it invents declarations.
//!
//! Resolution here is syntactic. A type is whatever the source wrote, never a resolved or inferred
//! one: `val x = 3` has no type in the skeleton, because inferring `Int` is a type checker's job.

use std::cell::Cell;

use anyhow::{Context, Result};
use ktsense_core::{
    DeclKind, Declaration, FileSkeleton, Modifier, Parameter, Visibility, MAX_NESTING_DEPTH,
};
use tree_sitter::Node;

/// Node kinds that can stand where a type is expected.
const TYPE_KINDS: &[&str] = &[
    "user_type",
    "nullable_type",
    "function_type",
    "parenthesized_type",
    "dynamic_type",
    "type_reference",
];

/// Extracts the skeleton of one Kotlin file.
///
/// `path` is carried through verbatim: this function does not touch the filesystem, so the caller
/// decides what a path means and keeps it relative to the workspace root.
///
/// tree-sitter is error tolerant and returns a tree for broken input. When that tree carries a
/// localized ERROR node the surrounding declarations still parse, so the result is a best-effort
/// recovery marked [`FileSkeleton::partial`] rather than a whole-file rejection (KT-52a). The
/// declarations skeletons keep are signatures whose fields are copied verbatim from source spans.
/// For each of the four grammar gaps KT-52 isolated, and for every file measured in the two pinned
/// corpora, the ERROR node sits in a region the extractor already discards (a function or property
/// body, an initializer) or skips (a stray token between members), so those recovered signatures
/// come out complete. That does not generalize: `partial` is set for ANY file carrying an error, and
/// an ERROR that lands inside a signature itself (a truncated type, a garbled default) leaks that
/// malformed span into the shown signature. The recovery is therefore best-effort, not verified; the
/// partial notice warns that shown signatures may be incomplete or malformed, and no caller may read
/// a recovered signature as trustworthy. A file whose every token is garbage collapses to a single
/// top-level ERROR node with no declaration beneath it, so nothing is recovered; that case returns an
/// error rather than an empty confident skeleton.
pub fn extract(path: impl Into<String>, source: &str) -> Result<FileSkeleton> {
    let tree = crate::parse(source).context("parsing Kotlin source")?;
    let root = tree.root_node();
    let extractor = Extractor {
        source,
        truncated: Cell::new(false),
    };

    let mut file = FileSkeleton::new(path);
    let mut cursor = root.walk();
    for node in root.children(&mut cursor) {
        match node.kind() {
            "package_header" => {
                file.package = extractor
                    .first_child_of_kind(node, "identifier")
                    .map(|identifier| extractor.text(identifier).to_string());
            }
            "import_list" => {
                let mut imports = node.walk();
                file.imports = node
                    .children(&mut imports)
                    .filter(|child| child.kind() == "import_header")
                    .filter_map(|header| extractor.first_child_of_kind(header, "identifier"))
                    .map(|identifier| extractor.text(identifier).to_string())
                    .collect();
            }
            _ => {
                if let Some(declaration) = extractor.declaration(node, 0) {
                    file.declarations.push(declaration);
                }
            }
        }
    }
    file.truncated = extractor.truncated.get();
    file.partial = root.has_error();

    if file.partial && file.is_empty() {
        anyhow::bail!("Kotlin syntax errors left no declaration to recover");
    }
    Ok(file)
}

struct Extractor<'a> {
    source: &'a str,
    truncated: Cell<bool>,
}

impl<'a> Extractor<'a> {
    fn text(&self, node: Node<'_>) -> &'a str {
        &self.source[node.byte_range()]
    }

    fn first_child_of_kind<'t>(&self, node: Node<'t>, kind: &str) -> Option<Node<'t>> {
        let mut cursor = node.walk();
        let mut children = node.children(&mut cursor);
        children.find(|child| child.kind() == kind)
    }

    fn children<'t>(&self, node: Node<'t>) -> Vec<Node<'t>> {
        let mut cursor = node.walk();
        node.children(&mut cursor).collect()
    }

    /// Dispatches on node kind. Returns `None` for anything that is not a declaration, which is how
    /// comments, getters, semicolons and expression statements are dropped.
    fn declaration(&self, node: Node<'_>, depth: usize) -> Option<Declaration> {
        match node.kind() {
            "class_declaration" => Some(self.class_like(node, depth)),
            "object_declaration" => Some(self.object_of(node, depth)),
            "companion_object" => Some(self.companion_object(node, depth)),
            "function_declaration" => Some(self.function(node)),
            "property_declaration" => self.property(node),
            "type_alias" => Some(self.type_alias(node)),
            "enum_entry" => Some(self.enum_entry(node)),
            "secondary_constructor" => Some(self.secondary_constructor(node)),
            _ => None,
        }
    }

    /// `class`, `interface`, `enum class`, `fun interface`, `annotation class`, `data class` and
    /// `value class` all arrive as `class_declaration`; the keyword and the modifiers tell them
    /// apart. `enum` and the `fun` of `fun interface` are unnamed keyword children rather than
    /// entries in the `modifiers` node, so they are read from the keyword set.
    fn class_like(&self, node: Node<'_>, depth: usize) -> Declaration {
        let children = self.children(node);
        let keywords: Vec<&str> = children
            .iter()
            .filter(|child| !child.is_named())
            .map(|child| child.kind())
            .collect();

        let kind = if keywords.contains(&"interface") {
            DeclKind::Interface
        } else {
            DeclKind::Class
        };

        let (visibility, mut modifiers) = self.modifiers_of(node);
        if keywords.contains(&"enum") {
            modifiers.push(Modifier::Enum);
        }
        if keywords.contains(&"fun") && kind == DeclKind::Interface {
            modifiers.push(Modifier::Fun);
        }

        let name_node = self.first_child_of_kind(node, "type_identifier");
        let mut declaration = Declaration::new(
            kind,
            name_node.map(|n| self.text(n)).unwrap_or_default(),
            self.line_of(name_node.unwrap_or(node)),
        )
        .with_visibility(visibility)
        .with_modifiers(modifiers);

        self.attach_type_parameters(&mut declaration, node);
        if let Some(constructor) = self.first_child_of_kind(node, "primary_constructor") {
            declaration.parameters = self.class_parameters(constructor);
            let (constructor_visibility, _) = self.modifiers_of(constructor);
            if !constructor_visibility.is_public() {
                declaration.constructor_visibility = Some(constructor_visibility);
            }
        }
        declaration.supertypes = self.supertypes_of(node);
        self.attach_constraints(&mut declaration, node);
        declaration.doc = self.doc_of(node);
        declaration.children = self.body_of(node, depth);
        declaration
    }

    fn object_of(&self, node: Node<'_>, depth: usize) -> Declaration {
        let (visibility, modifiers) = self.modifiers_of(node);
        let name_node = self.first_child_of_kind(node, "type_identifier");
        let mut declaration = Declaration::new(
            DeclKind::Object,
            name_node.map(|n| self.text(n)).unwrap_or_default(),
            self.line_of(name_node.unwrap_or(node)),
        )
        .with_visibility(visibility)
        .with_modifiers(modifiers);

        declaration.supertypes = self.supertypes_of(node);
        declaration.doc = self.doc_of(node);
        declaration.children = self.body_of(node, depth);
        declaration
    }

    /// A companion object is an object carrying [`Modifier::Companion`]; the renderer sorts it into
    /// canonical order, so appending it after the source modifiers is enough.
    fn companion_object(&self, node: Node<'_>, depth: usize) -> Declaration {
        let mut declaration = self.object_of(node, depth);
        declaration.modifiers.push(Modifier::Companion);
        declaration
    }

    fn function(&self, node: Node<'_>) -> Declaration {
        let (visibility, modifiers) = self.modifiers_of(node);
        let name_node = self.first_child_of_kind(node, "simple_identifier");
        let bare_name = name_node.map(|n| self.text(n)).unwrap_or_default();

        let mut declaration = Declaration::new(
            DeclKind::Function,
            self.name_with_receiver(node, bare_name),
            self.line_of(name_node.unwrap_or(node)),
        )
        .with_visibility(visibility)
        .with_modifiers(modifiers);

        self.attach_type_parameters(&mut declaration, node);
        if let Some(parameters) = self.first_child_of_kind(node, "function_value_parameters") {
            declaration.parameters = self.function_parameters(parameters);
        }
        declaration.return_type = self.return_type_of(node);
        if declaration.return_type.is_none() {
            declaration.type_inferred = self.has_expression_body(node);
        }
        self.attach_constraints(&mut declaration, node);
        declaration.doc = self.doc_of(node);
        declaration
    }

    /// An expression body (`fun f() = expr`) has a return type Kotlin infers, so an absent written
    /// type means "inferred", not Unit. A block body (`fun f() {}`) or no body at all is genuinely
    /// Unit. The grammar gives no field for this; an expression body's `function_body` opens with
    /// `=`, a block body's opens with `{`.
    fn has_expression_body(&self, node: Node<'_>) -> bool {
        self.first_child_of_kind(node, "function_body")
            .map(|body| self.text(body).trim_start().starts_with('='))
            .unwrap_or(false)
    }

    fn property(&self, node: Node<'_>) -> Option<Declaration> {
        let (visibility, modifiers) = self.modifiers_of(node);
        let mutable = self
            .first_child_of_kind(node, "binding_pattern_kind")
            .map(|binding| self.text(binding) == "var")
            .unwrap_or(false);

        let variable = self.first_child_of_kind(node, "variable_declaration")?;
        let name_node = self.first_child_of_kind(variable, "simple_identifier")?;

        let mut declaration = Declaration::new(
            if mutable {
                DeclKind::Var
            } else {
                DeclKind::Val
            },
            self.name_with_receiver(node, self.text(name_node)),
            self.line_of(name_node),
        )
        .with_visibility(visibility)
        .with_modifiers(modifiers);

        declaration.return_type = self
            .children(variable)
            .into_iter()
            .find(|child| TYPE_KINDS.contains(&child.kind()))
            .map(|type_node| self.text(type_node).to_string());
        declaration.type_inferred = declaration.return_type.is_none();
        declaration.doc = self.doc_of(node);
        Some(declaration)
    }

    fn type_alias(&self, node: Node<'_>) -> Declaration {
        let (visibility, modifiers) = self.modifiers_of(node);
        let name_node = self.first_child_of_kind(node, "type_identifier");
        let mut declaration = Declaration::new(
            DeclKind::TypeAlias,
            name_node.map(|n| self.text(n)).unwrap_or_default(),
            self.line_of(name_node.unwrap_or(node)),
        )
        .with_visibility(visibility)
        .with_modifiers(modifiers);

        self.attach_type_parameters(&mut declaration, node);
        // The aliased type sits after the name; rendering it as the "return type" keeps one field
        // doing one job, and `typealias P = (User) -> Boolean` reads correctly.
        declaration.return_type = self
            .children(node)
            .into_iter()
            .skip_while(|child| child.kind() != "type_identifier")
            .skip(1)
            .find(|child| TYPE_KINDS.contains(&child.kind()))
            .map(|type_node| self.text(type_node).to_string());
        declaration.doc = self.doc_of(node);
        declaration
    }

    /// An enum entry keeps only its name. Its constructor arguments are values, so they are body
    /// and are elided like any other body.
    fn enum_entry(&self, node: Node<'_>) -> Declaration {
        let name_node = self.first_child_of_kind(node, "simple_identifier");
        Declaration::new(
            DeclKind::EnumEntry,
            name_node.map(|n| self.text(n)).unwrap_or_default(),
            self.line_of(name_node.unwrap_or(node)),
        )
    }

    fn secondary_constructor(&self, node: Node<'_>) -> Declaration {
        let (visibility, modifiers) = self.modifiers_of(node);
        let mut declaration =
            Declaration::new(DeclKind::Constructor, "constructor", self.line_of(node))
                .with_visibility(visibility)
                .with_modifiers(modifiers);
        if let Some(parameters) = self.first_child_of_kind(node, "function_value_parameters") {
            declaration.parameters = self.function_parameters(parameters);
        }
        declaration.doc = self.doc_of(node);
        declaration
    }

    /// Prefixes an extension's receiver onto its name, because the receiver is what makes the
    /// declaration callable: `User.initials` can be used, a bare `initials` cannot.
    fn name_with_receiver(&self, node: Node<'_>, bare_name: &str) -> String {
        match self.first_child_of_kind(node, "receiver_type") {
            Some(receiver) => format!("{}.{}", self.text(receiver), bare_name),
            None => bare_name.to_string(),
        }
    }

    fn modifiers_of(&self, node: Node<'_>) -> (Visibility, Vec<Modifier>) {
        let Some(modifiers) = self.first_child_of_kind(node, "modifiers") else {
            return (Visibility::Public, Vec::new());
        };

        let mut visibility = Visibility::Public;
        let mut collected = Vec::new();
        for group in self.children(modifiers) {
            if group.kind() == "annotation" {
                continue;
            }
            match self.text(group).trim() {
                "private" => visibility = Visibility::Private,
                "internal" => visibility = Visibility::Internal,
                "protected" => visibility = Visibility::Protected,
                "public" => visibility = Visibility::Public,
                keyword => {
                    if let Some(modifier) = modifier_from_keyword(keyword) {
                        collected.push(modifier);
                    }
                }
            }
        }
        (visibility, collected)
    }

    fn attach_type_parameters(&self, declaration: &mut Declaration, node: Node<'_>) {
        if let Some(parameters) = self.first_child_of_kind(node, "type_parameters") {
            declaration.type_parameters = Some(self.text(parameters).to_string());
        }
    }

    fn attach_constraints(&self, declaration: &mut Declaration, node: Node<'_>) {
        if let Some(constraints) = self.first_child_of_kind(node, "type_constraints") {
            let text = self.text(constraints);
            declaration.type_constraints =
                Some(text.trim_start_matches("where").trim().to_string());
        }
    }

    fn supertypes_of(&self, node: Node<'_>) -> Vec<String> {
        self.children(node)
            .into_iter()
            .filter(|child| child.kind() == "delegation_specifier")
            .map(|specifier| self.text(specifier).to_string())
            .collect()
    }

    /// Primary-constructor parameters, where a `val`/`var` makes the parameter a property and the
    /// default value is a child rather than a sibling.
    fn class_parameters(&self, constructor: Node<'_>) -> Vec<Parameter> {
        self.children(constructor)
            .into_iter()
            .filter(|child| child.kind() == "class_parameter")
            .map(|node| self.class_parameter(node))
            .collect()
    }

    fn class_parameter(&self, node: Node<'_>) -> Parameter {
        let (visibility, _) = self.modifiers_of(node);
        let binding = self.first_child_of_kind(node, "binding_pattern_kind");
        let name = self
            .first_child_of_kind(node, "simple_identifier")
            .map(|n| self.text(n))
            .unwrap_or_default();
        let children = self.children(node);
        let type_index = children
            .iter()
            .position(|child| TYPE_KINDS.contains(&child.kind()));
        let type_name = type_index
            .map(|index| self.text(children[index]))
            .unwrap_or_default();

        let mut parameter = Parameter::new(name, type_name);
        if let Some(binding) = binding {
            parameter = parameter.declaring_property(visibility, self.text(binding) == "var");
        }
        if let Some(default) = type_index
            .and_then(|index| children.get(index + 1..))
            .and_then(|rest| rest.iter().find(|child| child.is_named()))
        {
            parameter = parameter.defaulting_to(self.text(*default));
        }
        parameter
    }

    /// Function parameters. The grammar puts `vararg` in a `parameter_modifiers` sibling *before*
    /// the parameter and the default value in a sibling *after* it, so this walks the list in order
    /// and attaches by position rather than by lookup.
    fn function_parameters(&self, node: Node<'_>) -> Vec<Parameter> {
        let mut parameters: Vec<Parameter> = Vec::new();
        let mut next_is_vararg = false;

        for child in self.children(node) {
            if !child.is_named() {
                continue;
            }
            match child.kind() {
                "parameter_modifiers" => {
                    next_is_vararg = self.text(child).contains("vararg");
                }
                "parameter" => {
                    let name = self
                        .first_child_of_kind(child, "simple_identifier")
                        .map(|n| self.text(n))
                        .unwrap_or_default();
                    let mut parameter = Parameter::new(name, self.parameter_type(child));
                    if next_is_vararg {
                        parameter = parameter.variadic();
                        next_is_vararg = false;
                    }
                    parameters.push(parameter);
                }
                _ => {
                    if let Some(last) = parameters.last_mut() {
                        if last.default.is_none() {
                            last.default = Some(self.text(child).to_string());
                        }
                    }
                }
            }
        }
        parameters
    }

    /// A parameter's declared type, keeping any type modifier that precedes it.
    ///
    /// `block: suspend () -> T` stores its `suspend` in a `type_modifiers` sibling that sits before
    /// the `function_type`, so reading the type node alone silently drops it.
    fn parameter_type(&self, parameter: Node<'_>) -> String {
        let children = self.children(parameter);
        let Some(index) = children
            .iter()
            .position(|child| TYPE_KINDS.contains(&child.kind()))
        else {
            return String::new();
        };
        let modifier = children
            .get(index.wrapping_sub(1))
            .filter(|_| index > 0)
            .filter(|previous| previous.kind() == "type_modifiers")
            .map(|previous| format!("{} ", self.text(*previous)))
            .unwrap_or_default();
        format!("{modifier}{}", self.text(children[index]))
    }

    /// The return type is the first type-shaped child after the parameter list. Anything before it
    /// belongs to the receiver or the parameters, and anything after is the body.
    fn return_type_of(&self, node: Node<'_>) -> Option<String> {
        self.children(node)
            .into_iter()
            .skip_while(|child| child.kind() != "function_value_parameters")
            .skip(1)
            .take_while(|child| child.kind() != "function_body")
            .find(|child| TYPE_KINDS.contains(&child.kind()))
            .map(|type_node| self.text(type_node).to_string())
    }

    /// Members of a class, interface, object or enum body.
    fn body_of(&self, node: Node<'_>, depth: usize) -> Vec<Declaration> {
        let body = self
            .first_child_of_kind(node, "class_body")
            .or_else(|| self.first_child_of_kind(node, "enum_class_body"));
        let Some(body) = body else {
            return Vec::new();
        };
        if depth >= MAX_NESTING_DEPTH {
            if body.named_child_count() > 0 {
                self.truncated.set(true);
            }
            return Vec::new();
        }
        self.children(body)
            .into_iter()
            .filter_map(|child| self.declaration(child, depth + 1))
            .collect()
    }

    /// KDoc from the nearest preceding sibling comment. Only `/** ... */` counts: a `//` comment or
    /// a plain `/* */` block is a note to a maintainer, not documentation of the declaration.
    fn doc_of(&self, node: Node<'_>) -> Option<String> {
        let previous = node.prev_named_sibling()?;
        if previous.kind() != "multiline_comment" {
            return None;
        }
        let raw = self.text(previous);
        if !raw.starts_with("/**") {
            return None;
        }
        summarize_kdoc(raw)
    }

    fn line_of(&self, node: Node<'_>) -> u32 {
        node.start_position().row as u32 + 1
    }
}

/// First sentence of a KDoc block, with the comment furniture stripped.
fn summarize_kdoc(raw: &str) -> Option<String> {
    let body = raw
        .trim_start_matches("/**")
        .trim_end_matches("*/")
        .lines()
        .map(|line| line.trim().trim_start_matches('*').trim())
        .take_while(|line| !line.starts_with('@'))
        .collect::<Vec<_>>()
        .join(" ");

    let summary = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let sentence = match summary.find(". ") {
        Some(end) => summary[..=end].trim_end().to_string(),
        None => summary,
    };
    (!sentence.is_empty()).then_some(sentence)
}

fn modifier_from_keyword(keyword: &str) -> Option<Modifier> {
    Some(match keyword {
        "expect" => Modifier::Expect,
        "actual" => Modifier::Actual,
        "final" => Modifier::Final,
        "open" => Modifier::Open,
        "abstract" => Modifier::Abstract,
        "sealed" => Modifier::Sealed,
        "const" => Modifier::Const,
        "external" => Modifier::External,
        "override" => Modifier::Override,
        "lateinit" => Modifier::Lateinit,
        "tailrec" => Modifier::Tailrec,
        "vararg" => Modifier::Vararg,
        "suspend" => Modifier::Suspend,
        "inner" => Modifier::Inner,
        "enum" => Modifier::Enum,
        "annotation" => Modifier::Annotation,
        "fun" => Modifier::Fun,
        "companion" => Modifier::Companion,
        "inline" => Modifier::Inline,
        "value" => Modifier::Value,
        "infix" => Modifier::Infix,
        "operator" => Modifier::Operator,
        "data" => Modifier::Data,
        _ => return None,
    })
}
