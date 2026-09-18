//! Renders a [`FileSkeleton`] back into text an agent can read cheaply.
//!
//! Two entry points. [`render_skeleton`] produces the Kotlin-like body, which is the thing golden
//! tests pin byte for byte. [`render_markdown`] wraps that body in the heading and fence a CLI or
//! MCP response carries.
//!
//! The rules, stated once so they can be argued with:
//!
//! 1. Indentation is four spaces per nesting level.
//! 2. Public visibility is never printed; Kotlin's default needs no ceremony.
//! 3. Modifiers print in the canonical order defined by [`Modifier`], not source order.
//! 4. A container with no children prints its header alone, with no empty braces.
//! 5. A container whose only child is a leaf collapses onto one line: `companion object { const
//!    val MAX_PAGE: Int }`. This is what keeps a one-constant companion from costing three lines.
//! 6. Everything else opens a brace, prints its children one per line, and closes it.
//! 7. Private and internal declarations are omitted unless asked for. A file that has nothing else
//!    renders as an empty skeleton, which is the correct answer rather than an error.

use crate::skeleton::{Declaration, FileSkeleton, Modifier, Parameter, Visibility};

const INDENT: &str = "    ";

/// What to include. The default is the cheapest useful answer: public API, no documentation.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions {
    pub include_private: bool,
    pub include_doc: bool,
    pub include_lines: bool,
}

impl RenderOptions {
    pub fn with_private(mut self) -> Self {
        self.include_private = true;
        self
    }

    pub fn with_doc(mut self) -> Self {
        self.include_doc = true;
        self
    }

    pub fn with_lines(mut self) -> Self {
        self.include_lines = true;
        self
    }

    fn admits(&self, declaration: &Declaration) -> bool {
        self.include_private || is_visible_api(declaration.visibility)
    }
}

fn is_visible_api(visibility: Visibility) -> bool {
    matches!(visibility, Visibility::Public | Visibility::Protected)
}

/// The Kotlin-like skeleton body. No trailing newline, so callers decide how to join it.
pub fn render_skeleton(file: &FileSkeleton, options: &RenderOptions) -> String {
    let mut lines = Vec::new();
    for declaration in &file.declarations {
        render_declaration(declaration, 0, options, &mut lines);
    }
    lines.join("\n")
}

/// The skeleton wrapped for an agent: path heading, package line, then a fenced Kotlin block.
///
/// An empty skeleton still renders its heading and says so, because "this file has no public API"
/// is an answer and a bare heading looks like a bug.
pub fn render_markdown(file: &FileSkeleton, options: &RenderOptions) -> String {
    let body = render_skeleton(file, options);
    let mut out = format!("## {}\n", file.path);
    if let Some(package) = &file.package {
        out.push_str(&format!("\npackage {package}\n"));
    }
    if body.is_empty() {
        out.push_str("\nNo public declarations.\n");
        return out;
    }
    out.push_str("\n```kotlin\n");
    out.push_str(&body);
    out.push_str("\n```\n");
    out
}

fn render_declaration(
    declaration: &Declaration,
    depth: usize,
    options: &RenderOptions,
    lines: &mut Vec<String>,
) {
    if !options.admits(declaration) {
        return;
    }

    let padding = INDENT.repeat(depth);

    if options.include_doc {
        if let Some(doc) = &declaration.doc {
            lines.push(format!("{padding}/** {doc} */"));
        }
    }

    let header = header_of(declaration, options);
    let children: Vec<&Declaration> = declaration
        .children
        .iter()
        .filter(|child| options.admits(child))
        .collect();

    if !declaration.is_container() || children.is_empty() {
        lines.push(format!("{padding}{header}"));
        return;
    }

    if let [only] = children.as_slice() {
        if only.children.is_empty() && !(options.include_doc && only.doc.is_some()) {
            let inner = header_of(only, options);
            lines.push(format!("{padding}{header} {{ {inner} }}"));
            return;
        }
    }

    lines.push(format!("{padding}{header} {{"));
    for child in children {
        render_declaration(child, depth + 1, options, lines);
    }
    lines.push(format!("{padding}}}"));
}

fn header_of(declaration: &Declaration, options: &RenderOptions) -> String {
    let mut parts: Vec<String> = Vec::new();

    let visibility = declaration.visibility.keyword();
    if !visibility.is_empty() {
        parts.push(visibility.to_string());
    }

    let mut modifiers = declaration.modifiers.clone();
    modifiers.sort();
    modifiers.dedup();
    parts.extend(
        modifiers
            .iter()
            .map(|modifier| modifier.keyword().to_string()),
    );

    let keyword = declaration.kind.keyword();
    if !keyword.is_empty() {
        parts.push(keyword.to_string());
    }

    let mut header = parts.join(" ");

    if !declaration.name.is_empty() {
        if !header.is_empty() {
            header.push(' ');
        }
        header.push_str(&declaration.name);
    }

    if let Some(type_parameters) = &declaration.type_parameters {
        header.push_str(type_parameters);
    }

    if declaration.kind.takes_parentheses() || !declaration.parameters.is_empty() {
        header.push('(');
        header.push_str(
            &declaration
                .parameters
                .iter()
                .map(render_parameter)
                .collect::<Vec<_>>()
                .join(", "),
        );
        header.push(')');
    }

    if let Some(return_type) = &declaration.return_type {
        header.push_str(": ");
        header.push_str(return_type);
    }

    if !declaration.supertypes.is_empty() {
        header.push_str(" : ");
        header.push_str(&declaration.supertypes.join(", "));
    }

    if let Some(constraints) = &declaration.type_constraints {
        header.push_str(" where ");
        header.push_str(constraints);
    }

    if options.include_lines {
        header.push_str(&format!("  # L{}", declaration.line));
    }

    header
}

fn render_parameter(parameter: &Parameter) -> String {
    let mut rendered = String::new();

    if let Some(property) = &parameter.property {
        let visibility = property.visibility.keyword();
        if !visibility.is_empty() {
            rendered.push_str(visibility);
            rendered.push(' ');
        }
        rendered.push_str(if property.mutable { "var " } else { "val " });
    }

    if parameter.vararg {
        rendered.push_str(Modifier::Vararg.keyword());
        rendered.push(' ');
    }

    rendered.push_str(&parameter.name);
    rendered.push_str(": ");
    rendered.push_str(&parameter.type_name);

    if let Some(default) = &parameter.default {
        rendered.push_str(" = ");
        rendered.push_str(default);
    }

    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton::DeclKind;

    /// The skeleton the plan pins as the compressor's contract. Built by hand: this crate has no
    /// parser, and that is the point.
    fn reference_file() -> FileSkeleton {
        FileSkeleton::new("app/service/UserService.kt")
            .in_package("app.service")
            .with_declarations(vec![Declaration::class("UserService", 12)
                .with_parameters(vec![Parameter::new("repo", "UserRepository")
                    .declaring_property(Visibility::Private, false)])
                .extending(vec!["Service".to_string()])
                .containing(vec![
                    Declaration::function("createUser", 14)
                        .with_parameters(vec![Parameter::new("dto", "UserDto")])
                        .returning("Result<User>"),
                    Declaration::function("findAll", 18)
                        .with_modifiers(vec![Modifier::Suspend])
                        .with_parameters(vec![Parameter::new("page", "Int").defaulting_to("0")])
                        .returning("List<User>"),
                    Declaration::object("", 22)
                        .with_modifiers(vec![Modifier::Companion])
                        .containing(vec![Declaration::val_property("MAX_PAGE", 23)
                            .with_modifiers(vec![Modifier::Const])
                            .returning("Int")]),
                ])])
    }

    #[test]
    fn renders_the_reference_skeleton_byte_for_byte() {
        let expected = concat!(
            "class UserService(private val repo: UserRepository) : Service {\n",
            "    fun createUser(dto: UserDto): Result<User>\n",
            "    suspend fun findAll(page: Int = 0): List<User>\n",
            "    companion object { const val MAX_PAGE: Int }\n",
            "}"
        );

        assert_eq!(
            render_skeleton(&reference_file(), &RenderOptions::default()),
            expected
        );
    }

    #[test]
    fn markdown_wraps_the_body_with_path_package_and_fence() {
        let expected = concat!(
            "## app/service/UserService.kt\n",
            "\n",
            "package app.service\n",
            "\n",
            "```kotlin\n",
            "class UserService(private val repo: UserRepository) : Service {\n",
            "    fun createUser(dto: UserDto): Result<User>\n",
            "    suspend fun findAll(page: Int = 0): List<User>\n",
            "    companion object { const val MAX_PAGE: Int }\n",
            "}\n",
            "```\n",
        );

        assert_eq!(
            render_markdown(&reference_file(), &RenderOptions::default()),
            expected
        );
    }

    /// Mirrors `fixtures/tiny-app/src/main/kotlin/app/util/internals.kt`, whose whole job is to be
    /// invisible by default and complete under `--private`.
    #[test]
    fn private_declarations_appear_only_when_asked_for() {
        let file = FileSkeleton::new("app/util/internals.kt").with_declarations(vec![
            Declaration::val_property("SALT", 7)
                .with_visibility(Visibility::Private)
                .with_modifiers(vec![Modifier::Const])
                .returning("String"),
            Declaration::class("Hasher", 9)
                .with_visibility(Visibility::Private)
                .containing(vec![Declaration::function("hash", 10)
                    .with_parameters(vec![Parameter::new("input", "String")])
                    .returning("Int")]),
            Declaration::function("internalHash", 13)
                .with_visibility(Visibility::Internal)
                .with_parameters(vec![Parameter::new("input", "String")])
                .returning("Int"),
        ]);

        let default_and_private = (
            render_skeleton(&file, &RenderOptions::default()),
            render_skeleton(&file, &RenderOptions::default().with_private()),
        );

        assert_eq!(
            default_and_private,
            (
                String::new(),
                concat!(
                    "private const val SALT: String\n",
                    "private class Hasher { fun hash(input: String): Int }\n",
                    "internal fun internalHash(input: String): Int"
                )
                .to_string()
            )
        );
    }

    #[test]
    fn a_file_with_no_public_api_says_so_rather_than_rendering_an_empty_fence() {
        let file = FileSkeleton::new("app/util/internals.kt").with_declarations(vec![
            Declaration::function("unusedHelper", 15).with_visibility(Visibility::Private),
        ]);

        assert_eq!(
            render_markdown(&file, &RenderOptions::default()),
            "## app/util/internals.kt\n\nNo public declarations.\n"
        );
    }

    #[test]
    fn modifiers_render_in_canonical_order_and_protected_members_stay_visible() {
        let declaration = Declaration::function("onEviction", 52)
            .with_visibility(Visibility::Protected)
            .with_modifiers(vec![Modifier::Open, Modifier::Suspend, Modifier::Override])
            .with_parameters(vec![Parameter::new("user", "User")]);
        let file =
            FileSkeleton::new("app/service/UserService.kt").with_declarations(vec![declaration]);

        assert_eq!(
            render_skeleton(&file, &RenderOptions::default()),
            "protected open override suspend fun onEviction(user: User)"
        );
    }

    #[test]
    fn a_container_with_several_children_or_a_nested_container_does_not_collapse() {
        let file = FileSkeleton::new("app/service/Nested.kt").with_declarations(vec![
            Declaration::interface("Empty", 1),
            Declaration::object("Two", 3).containing(vec![
                Declaration::val_property("a", 4).returning("Int"),
                Declaration::val_property("b", 5).returning("Int"),
            ]),
            Declaration::class("Outer", 8).containing(vec![Declaration::class("Inner", 9)
                .with_modifiers(vec![Modifier::Inner])
                .containing(vec![Declaration::val_property("deep", 10).returning("Int")])]),
        ]);

        assert_eq!(
            render_skeleton(&file, &RenderOptions::default()),
            concat!(
                "interface Empty\n",
                "object Two {\n",
                "    val a: Int\n",
                "    val b: Int\n",
                "}\n",
                "class Outer {\n",
                "    inner class Inner { val deep: Int }\n",
                "}"
            )
        );
    }

    #[test]
    fn signatures_carry_type_parameters_constraints_varargs_and_line_numbers_on_request() {
        let file = FileSkeleton::new("app/util/Page.kt").with_declarations(vec![
            Declaration::new(DeclKind::Class, "Validator", 12)
                .with_type_parameters("<T>")
                .constrained_by("T : Any"),
            Declaration::function("hasAnyEmailDomain", 24)
                .with_parameters(vec![
                    Parameter::new("domains", "String").variadic(),
                    Parameter::new("ignoreCase", "Boolean").defaulting_to("true"),
                ])
                .returning("Boolean")
                .documented("Extension function with a vararg and a default."),
        ]);

        assert_eq!(
            render_skeleton(&file, &RenderOptions::default().with_doc().with_lines()),
            concat!(
                "class Validator<T> where T : Any  # L12\n",
                "/** Extension function with a vararg and a default. */\n",
                "fun hasAnyEmailDomain(vararg domains: String, ignoreCase: Boolean = true): Boolean  # L24"
            )
        );
    }
}
