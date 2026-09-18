//! Typed wrappers over the five requests kmp-lsp 0.26.0 actually advertises.
//!
//! kmp-lsp 0.26.0 reports no `callHierarchyProvider`, so there are deliberately no call-hierarchy
//! wrappers here; callers are derived downstream from `references` plus each site's enclosing
//! declaration. (see AGENTS.md)

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
}

fn parse_uri(uri: &str) -> Result<Uri, LspError> {
    uri.parse().map_err(|_invalid| LspError::InvalidUri {
        uri: uri.to_string(),
    })
}
