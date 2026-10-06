//! The declarations an annotation is written on, grouped by file (KT-109).
//!
//! When `trace` resolves a name to an annotation class, or finds no declaration but the name is
//! written as `@Name` in the workspace, the answer lists every declaration carrying that
//! annotation. The `ktsense-syntax` adapter attaches each `@Name` to the declaration it modifies
//! and hands over [`AnnotatedDeclaration`] values; like the rest of `core` this module only groups,
//! counts and renders them, never touching a parser or the filesystem.
//!
//! The annotation's target is a parse fact, but which annotation it is was matched by simple name,
//! never resolved through imports, so every answer carries [`ANNOTATION_PRECISION`] and can never be
//! read as a type-checked result.

use crate::skeleton::DeclKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The precision an annotation-use answer carries: the attachment is a parse fact, the annotation's
/// identity matched by simple name only.
pub const ANNOTATION_PRECISION: &str = "syntax (matched by name)";

/// The precision as the default when deserializing, since it is a fixed constant rather than data a
/// consumer supplies.
fn annotation_precision() -> &'static str {
    ANNOTATION_PRECISION
}

/// One declaration an annotation is written on: its fully-qualified name, kind and location, as the
/// syntax adapter produces it before grouping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotatedDeclaration {
    pub path: String,
    pub line: u32,
    pub fqn: String,
    pub kind: DeclKind,
}

impl AnnotatedDeclaration {
    pub fn new(path: impl Into<String>, line: u32, fqn: impl Into<String>, kind: DeclKind) -> Self {
        Self {
            path: path.into(),
            line,
            fqn: fqn.into(),
            kind,
        }
    }
}

/// A declaration carrying the annotation, without its path: the group names the file once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotatedMember {
    pub line: u32,
    pub fqn: String,
    pub kind: DeclKind,
}

/// Every declaration carrying the annotation in one file, ordered by line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotatedGroup {
    pub path: String,
    pub declarations: Vec<AnnotatedMember>,
}

/// Where an annotation is written across the workspace: the total, the file count, and the sites
/// grouped by file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotatedDeclarations {
    pub annotation: String,
    /// Always [`ANNOTATION_PRECISION`]: the attachment is syntactic, the identity by name. A fixed
    /// constant rather than data, so it is defaulted on the way in rather than read.
    #[serde(skip_deserializing, default = "annotation_precision")]
    pub precision: &'static str,
    pub total: usize,
    pub file_count: usize,
    pub groups: Vec<AnnotatedGroup>,
}

impl AnnotatedDeclarations {
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }
}

/// Groups annotated declarations by file, ordering files by path and each file's declarations by
/// line then fully-qualified name, so two declarations annotated on one line keep a stable order.
pub fn build_annotated(
    annotation: &str,
    declarations: &[AnnotatedDeclaration],
) -> AnnotatedDeclarations {
    let mut by_path: BTreeMap<&str, Vec<&AnnotatedDeclaration>> = BTreeMap::new();
    for declaration in declarations {
        by_path
            .entry(declaration.path.as_str())
            .or_default()
            .push(declaration);
    }
    let groups: Vec<AnnotatedGroup> = by_path
        .into_iter()
        .map(|(path, mut members)| {
            members.sort_by(|a, b| (a.line, &a.fqn).cmp(&(b.line, &b.fqn)));
            AnnotatedGroup {
                path: path.to_string(),
                declarations: members
                    .into_iter()
                    .map(|member| AnnotatedMember {
                        line: member.line,
                        fqn: member.fqn.clone(),
                        kind: member.kind,
                    })
                    .collect(),
            }
        })
        .collect();
    AnnotatedDeclarations {
        annotation: annotation.to_string(),
        precision: ANNOTATION_PRECISION,
        total: declarations.len(),
        file_count: groups.len(),
        groups,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two things grouping must get right at once: declarations grouped by file with the path
    /// named once, and each file's declarations ordered by line, while the total counts every site
    /// before grouping and the file count the groups. Checked as one composed value.
    #[test]
    fn groups_by_file_orders_by_line_and_counts_the_whole_and_the_files() {
        let declarations = vec![
            AnnotatedDeclaration::new("app/Reports.kt", 8, "app.AuditReport", DeclKind::Class),
            AnnotatedDeclaration::new("app/Reports.kt", 4, "app.SalesReport", DeclKind::Class),
            AnnotatedDeclaration::new(
                "app/handlers/Tasks.kt",
                5,
                "app.handlers.runTask",
                DeclKind::Function,
            ),
        ];

        let annotated = build_annotated("Audited", &declarations);

        assert_eq!(
            annotated,
            AnnotatedDeclarations {
                annotation: "Audited".to_string(),
                precision: "syntax (matched by name)",
                total: 3,
                file_count: 2,
                groups: vec![
                    AnnotatedGroup {
                        path: "app/Reports.kt".to_string(),
                        declarations: vec![
                            AnnotatedMember {
                                line: 4,
                                fqn: "app.SalesReport".to_string(),
                                kind: DeclKind::Class,
                            },
                            AnnotatedMember {
                                line: 8,
                                fqn: "app.AuditReport".to_string(),
                                kind: DeclKind::Class,
                            },
                        ],
                    },
                    AnnotatedGroup {
                        path: "app/handlers/Tasks.kt".to_string(),
                        declarations: vec![AnnotatedMember {
                            line: 5,
                            fqn: "app.handlers.runTask".to_string(),
                            kind: DeclKind::Function,
                        }],
                    },
                ],
            }
        );
    }

    /// The rendered section groups by file with the path named once, lists each declaration as its
    /// FQN, kind and line, and states the by-name precision, so the shape an agent reads is pinned.
    #[test]
    fn renders_grouped_by_file_with_fqn_kind_line_and_the_by_name_precision() {
        let declarations = vec![
            AnnotatedDeclaration::new("app/Reports.kt", 4, "app.SalesReport", DeclKind::Class),
            AnnotatedDeclaration::new("app/Reports.kt", 8, "app.AuditReport", DeclKind::Class),
            AnnotatedDeclaration::new(
                "app/handlers/Tasks.kt",
                5,
                "app.handlers.runTask",
                DeclKind::Function,
            ),
        ];
        let rendered = crate::render_annotated_markdown(&build_annotated("Audited", &declarations));

        assert_eq!(
            rendered,
            concat!(
                "## Annotated (3)\n",
                "precision: syntax (matched by name)\n",
                "\n",
                "app/Reports.kt\n",
                "- app.SalesReport  class  4\n",
                "- app.AuditReport  class  8\n",
                "\n",
                "app/handlers/Tasks.kt\n",
                "- app.handlers.runTask  fun  5\n",
            )
        );
    }
}
