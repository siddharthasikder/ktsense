//! Typed wrappers over the five requests kmp-lsp 0.26.0 actually advertises.
//!
//! kmp-lsp 0.26.0 reports no `callHierarchyProvider`, so there are deliberately no call-hierarchy
//! wrappers here; callers are derived downstream from `references` plus each site's enclosing
//! declaration. (see AGENTS.md)

use std::path::PathBuf;

use lsp_types::request::{
    DocumentSymbolRequest, GotoDefinition, GotoImplementation, HoverRequest, References, Request,
};
use lsp_types::{
    DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse,
    Hover, HoverParams, Location, Position, ReferenceContext, ReferenceParams,
    TextDocumentIdentifier, TextDocumentPositionParams, Uri,
};

use crate::client::{LspClient, LspError};

/// A point in a source file, addressed the way LSP requests expect it: a document URI plus a
/// zero-based line and character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePosition {
    pub uri: String,
    pub line: u32,
    pub character: u32,
}

impl FilePosition {
    fn to_position_params(&self) -> Result<TextDocumentPositionParams, LspError> {
        Ok(TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: parse_uri(&self.uri)?,
            },
            position: Position {
                line: self.line,
                character: self.character,
            },
        })
    }
}

/// Whether a references query counts the symbol's own declaration among the results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationScope {
    Included,
    Excluded,
}

impl DeclarationScope {
    fn includes_declaration(self) -> bool {
        matches!(self, DeclarationScope::Included)
    }
}

impl LspClient {
    /// `textDocument/definition`: where the symbol under `at` is defined.
    pub async fn definition(
        &self,
        at: &FilePosition,
    ) -> Result<Option<GotoDefinitionResponse>, LspError> {
        let params = GotoDefinitionParams {
            text_document_position_params: at.to_position_params()?,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self.typed::<GotoDefinition>(params).await
    }

    /// `textDocument/implementation`: the implementors of the symbol under `at`.
    pub async fn implementation(
        &self,
        at: &FilePosition,
    ) -> Result<Option<GotoDefinitionResponse>, LspError> {
        let params = GotoDefinitionParams {
            text_document_position_params: at.to_position_params()?,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self.typed::<GotoImplementation>(params).await
    }

    /// `textDocument/references`: every use of the symbol under `at`. A null reply is reported as
    /// an empty result rather than an error.
    pub async fn references(
        &self,
        at: &FilePosition,
        scope: DeclarationScope,
    ) -> Result<Vec<Location>, LspError> {
        let params = ReferenceParams {
            text_document_position: at.to_position_params()?,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: ReferenceContext {
                include_declaration: scope.includes_declaration(),
            },
        };
        Ok(self.typed::<References>(params).await?.unwrap_or_default())
    }

    /// `textDocument/documentSymbol`: the declaration outline of a single file.
    pub async fn document_symbols(
        &self,
        uri: &str,
    ) -> Result<Option<DocumentSymbolResponse>, LspError> {
        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier {
                uri: parse_uri(uri)?,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self.typed::<DocumentSymbolRequest>(params).await
    }

    /// `textDocument/hover`: the engine's summary of the symbol under `at`.
    pub async fn hover(&self, at: &FilePosition) -> Result<Option<Hover>, LspError> {
        let params = HoverParams {
            text_document_position_params: at.to_position_params()?,
            work_done_progress_params: Default::default(),
        };
        self.typed::<HoverRequest>(params).await
    }

    async fn typed<R: Request>(&self, params: R::Params) -> Result<R::Result, LspError> {
        let response = self
            .request(R::METHOD, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(response)?)
    }

    /// `textDocument/implementation` as filesystem sites, the shape the product's own reference
    /// model consumes; a null or empty reply is an empty list.
    pub async fn implementation_sites(
        &self,
        at: &FilePosition,
    ) -> Result<Vec<SiteLocation>, LspError> {
        Ok(flatten(self.implementation(at).await?)
            .into_iter()
            .map(SiteLocation::from)
            .collect())
    }

    /// `textDocument/references` as filesystem sites.
    pub async fn reference_sites(
        &self,
        at: &FilePosition,
        scope: DeclarationScope,
    ) -> Result<Vec<SiteLocation>, LspError> {
        Ok(self
            .references(at, scope)
            .await?
            .into_iter()
            .map(SiteLocation::from)
            .collect())
    }
}

/// A location as the engine reported it, translated to filesystem terms: the `file://` URI decoded
/// to an absolute path, and the zero-based LSP position moved to the 1-based line and column the
/// rest of the product speaks. This is the boundary where `lsp_types` stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteLocation {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
}

impl From<Location> for SiteLocation {
    fn from(location: Location) -> Self {
        Self {
            path: uri_to_path(location.uri.as_str()),
            line: location.range.start.line + 1,
            column: location.range.start.character + 1,
        }
    }
}

fn flatten(response: Option<GotoDefinitionResponse>) -> Vec<Location> {
    match response {
        Some(GotoDefinitionResponse::Scalar(location)) => vec![location],
        Some(GotoDefinitionResponse::Array(locations)) => locations,
        Some(GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|link| Location {
                uri: link.target_uri,
                range: link.target_selection_range,
            })
            .collect(),
        None => Vec::new(),
    }
}

/// A `file://` URI back to a filesystem path, undoing percent-encoding; anything else is kept as
/// written so it still appears in an answer rather than vanishing.
pub fn uri_to_path(uri: &str) -> PathBuf {
    match uri.strip_prefix("file://") {
        Some(encoded) => PathBuf::from(percent_decode(encoded)),
        None => PathBuf::from(uri),
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match escaped_byte(bytes, index) {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn escaped_byte(bytes: &[u8], index: usize) -> Option<u8> {
    if bytes[index] != b'%' {
        return None;
    }
    let hex = bytes.get(index + 1..index + 3)?;
    u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
}

fn parse_uri(uri: &str) -> Result<Uri, LspError> {
    uri.parse().map_err(|_invalid| LspError::InvalidUri {
        uri: uri.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_decode_percent_escapes_and_other_schemes_pass_through() {
        let observed = (
            uri_to_path("file:///work/My%20Project/A.kt"),
            uri_to_path("file:///plain/B.kt"),
            uri_to_path("untitled:Scratch.kt"),
            percent_decode("trailing%"),
            percent_decode("bad%zzescape"),
        );
        assert_eq!(
            observed,
            (
                PathBuf::from("/work/My Project/A.kt"),
                PathBuf::from("/plain/B.kt"),
                PathBuf::from("untitled:Scratch.kt"),
                "trailing%".to_string(),
                "bad%zzescape".to_string(),
            )
        );
    }
}
