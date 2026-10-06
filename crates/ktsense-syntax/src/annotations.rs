//! The declarations an annotation is written on, found by a tree-sitter pass (KT-109).
//!
//! [`annotated_declarations`] extracts the file skeleton once (so the annotation text is already
//! attached to each declaration and whitespace-normalized), then walks the declarations tracking the
//! package and the enclosing names to compose each one's fully-qualified name. A declaration is kept
//! when any of its annotations names the queried annotation by simple name, so `@Audited`,
//! `@Audited(...)`, a fully-qualified `@com.example.Audited` and a use-site-targeted `@field:Audited`
//! all match `Audited`. The grouping and rendering are `ktsense-core`'s; this module only decides
//! attachment, the one part that needs the parser.

use ktsense_core::{AnnotatedDeclaration, Declaration};

/// Every declaration in `source` carrying an annotation whose simple name is `annotation`, as
/// [`AnnotatedDeclaration`] values keyed to `path`. A file that cannot be parsed contributes
/// nothing, the same best-effort rule the rest of the scan follows.
pub fn annotated_declarations(
    path: &str,
    source: &str,
    annotation: &str,
) -> Vec<AnnotatedDeclaration> {
    let Ok(file) = crate::extract(path, source) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    walk(
        &file.declarations,
        path,
        file.package.as_deref(),
        &mut Vec::new(),
        annotation,
        &mut found,
    );
    found
}

/// Records every declaration under `declarations` carrying `@<annotation>`, then recurses into each
/// one's members with its name pushed onto the enclosing chain, so a member's fully-qualified name
/// reads from the package down through its containers.
fn walk(
    declarations: &[Declaration],
    path: &str,
    package: Option<&str>,
    ancestors: &mut Vec<String>,
    annotation: &str,
    found: &mut Vec<AnnotatedDeclaration>,
) {
    for declaration in declarations {
        if declaration
            .annotations
            .iter()
            .any(|written| annotation_simple_name(written) == Some(annotation))
        {
            found.push(AnnotatedDeclaration::new(
                path,
                declaration.line,
                fully_qualified(package, ancestors, &declaration.name),
                declaration.kind,
            ));
        }
        if !declaration.name.is_empty() {
            ancestors.push(declaration.name.clone());
        }
        walk(
            &declaration.children,
            path,
            package,
            ancestors,
            annotation,
            found,
        );
        if !declaration.name.is_empty() {
            ancestors.pop();
        }
    }
}

/// The package, the enclosing declaration names and this declaration's name joined with dots, each
/// empty part dropped, so a top-level declaration reads `pkg.Name` and a member `pkg.Outer.Name`.
fn fully_qualified(package: Option<&str>, ancestors: &[String], name: &str) -> String {
    package
        .into_iter()
        .map(str::to_string)
        .chain(ancestors.iter().cloned())
        .chain(std::iter::once(name.to_string()))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(".")
}

/// The simple name of an annotation written as source text, or `None` when the text is not an
/// annotation. Strips the leading `@`, any argument list or type arguments, a use-site target such
/// as `field:` or `get:`, and any package qualifier, leaving the bare type name: `@field:Audited(x)`
/// and `@com.example.Audited` both reduce to `Audited`. A grouped annotation such as `@[A B]` is not
/// decomposed and reduces to nothing matchable, which is the honest answer for a shape this simple
/// matcher does not model.
fn annotation_simple_name(text: &str) -> Option<&str> {
    let rest = text.trim().strip_prefix('@')?;
    let head = rest.split(['(', '<', ' ', '\t']).next().unwrap_or(rest);
    let after_target = head.rsplit(':').next().unwrap_or(head);
    let simple = after_target
        .rsplit('.')
        .next()
        .unwrap_or(after_target)
        .trim();
    (!simple.is_empty() && simple.chars().all(is_identifier_char)).then_some(simple)
}

/// Whether a character can appear in a Kotlin identifier, so a reduced annotation name that still
/// carries a bracket or operator (as a grouped or malformed annotation would) is rejected rather
/// than matched against a plain name.
fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktsense_core::DeclKind;

    /// The attachment contract in one source: an annotation on a top-level class, on a member
    /// function, written fully-qualified, with an argument, and with a use-site target all attach to
    /// the right declaration with the right fully-qualified name and kind, while a different
    /// annotation and an unannotated declaration are left out. Checked as one composed value.
    #[test]
    fn attaches_each_matching_annotation_to_its_declaration_with_a_fully_qualified_name() {
        let source = concat!(
            "package app\n",
            "\n",
            "@Audited\n",
            "class SalesReport\n",
            "\n",
            "@com.example.Audited(\"x\")\n",
            "class AuditReport {\n",
            "    @field:Audited\n",
            "    val id: Int = 0\n",
            "    @Other\n",
            "    fun untouched() {}\n",
            "}\n",
            "\n",
            "class Plain\n",
        );

        let observed = annotated_declarations("app/Reports.kt", source, "Audited");

        assert_eq!(
            observed,
            vec![
                AnnotatedDeclaration::new("app/Reports.kt", 4, "app.SalesReport", DeclKind::Class),
                AnnotatedDeclaration::new("app/Reports.kt", 7, "app.AuditReport", DeclKind::Class),
                AnnotatedDeclaration::new("app/Reports.kt", 9, "app.AuditReport.id", DeclKind::Val),
            ]
        );
    }
}
