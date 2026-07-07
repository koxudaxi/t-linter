use anyhow::Result;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use t_linter_core::{
    DiagnosticData, DiagnosticEdit, FormatOptions as CoreFormatOptions, LintDiagnostic,
    ShadowDocument, TemplateHighlighter, TemplateStringInfo, TemplateStringParser,
    format_document_range_with_options, format_document_with_options, lint_source,
    load_project_config_for_path, synthesize_for_type_check,
};
use tower_lsp::jsonrpc::Result as JsonRpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tracing::{debug, info, warn};

mod lsp_helpers;
mod ruff;
mod type_checker;

use ruff::RuffPipelineClient;
pub use ruff::RuffPipelineConfig;
use type_checker::{NegotiatedEncoding, TypeCheckerState, ensure_type_checker};
pub use type_checker::{TypeCheckerBackend, TypeCheckerConfig};

const TOKEN_TYPE_MACRO: u32 = 14;
const TOKEN_MODIFIER_NONE: u32 = 0;
const DIAGNOSTIC_DEBOUNCE_MS: u64 = 250;
const SOURCE_FIX_ALL_T_LINTER: &str = "source.fixAll.t-linter";
const REFACTOR_REWRITE_T_LINTER: &str = "refactor.rewrite.t-linter";
const INTERPOLATION_TYPE_RULE: &str = "interpolation-type-error";
const TY_INVALID_ASSIGNMENT_RULE: &str = "invalid-assignment";
const PYRIGHT_ASSIGNMENT_RULE: &str = "reportAssignmentType";
const PYREFLY_ASSIGNMENT_RULE: &str = "bad-assignment";

pub struct TLinterLanguageServer {
    client: Client,
    document_cache: Arc<DashMap<Url, DocumentState>>,
    diagnostic_tasks: Arc<DashMap<Url, DiagnosticTask>>,
    ruff_document_locks: Arc<DashMap<Url, Arc<tokio::sync::Mutex<()>>>>,
    parser: Arc<tokio::sync::Mutex<TemplateStringParser>>,
    highlighter: Arc<tokio::sync::Mutex<TemplateHighlighter>>,
    config: Arc<tokio::sync::RwLock<TLinterConfig>>,
    ruff: Arc<tokio::sync::RwLock<Option<Arc<RuffPipelineClient>>>>,
    type_checker: Arc<tokio::sync::Mutex<TypeCheckerState>>,
    client_support: Arc<tokio::sync::RwLock<ClientSupport>>,
    workspace_roots: Arc<tokio::sync::RwLock<Vec<PathBuf>>>,
    initialize_params: Arc<tokio::sync::RwLock<Option<serde_json::Value>>>,
}

#[derive(Debug, Clone)]
struct DocumentState {
    text: String,
    version: i32,
}

#[derive(Debug)]
struct DiagnosticTask {
    generation: u64,
    handle: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone, Copy, Default)]
struct ClientSupport {
    resolve_code_action_edits: bool,
    versioned_workspace_edits: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct TLinterCodeActionData {
    schema_version: u8,
    #[serde(flatten)]
    action: TLinterCodeAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "camelCase")]
enum TLinterCodeAction {
    SourceFixAll {
        uri: Url,
        document_version: i32,
    },
    RefactorRewrite {
        uri: Url,
        document_version: i32,
        range: Range,
    },
}

const CODE_ACTION_DATA_SCHEMA_VERSION: u8 = 1;

impl TLinterLanguageServer {
    pub fn new(client: Client) -> Result<Self> {
        Self::with_config(client, TLinterConfig::default())
    }

    pub fn with_config(client: Client, config: TLinterConfig) -> Result<Self> {
        let type_checker_config = config.type_checking.clone();
        Ok(Self {
            client,
            document_cache: Arc::new(DashMap::new()),
            diagnostic_tasks: Arc::new(DashMap::new()),
            ruff_document_locks: Arc::new(DashMap::new()),
            parser: Arc::new(tokio::sync::Mutex::new(TemplateStringParser::new()?)),
            highlighter: Arc::new(tokio::sync::Mutex::new(TemplateHighlighter::new()?)),
            config: Arc::new(tokio::sync::RwLock::new(config)),
            ruff: Arc::new(tokio::sync::RwLock::new(None)),
            type_checker: Arc::new(tokio::sync::Mutex::new(TypeCheckerState::new(
                type_checker_config,
                Vec::new(),
            ))),
            client_support: Arc::new(tokio::sync::RwLock::new(ClientSupport::default())),
            workspace_roots: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            initialize_params: Arc::new(tokio::sync::RwLock::new(None)),
        })
    }

    fn generate_fallback_tokens(
        &self,
        template: &TemplateStringInfo,
        text: &str,
    ) -> Vec<(u32, u32, u32, u32, u32)> {
        let mut tokens = Vec::new();

        let start_line = (template.location.start_line - 1) as u32;
        let start_col = (template.location.start_column - 1) as u32;
        let end_line = (template.location.end_line - 1) as u32;
        let end_col = (template.location.end_column - 1) as u32;

        if start_line == end_line {
            let length = end_col.saturating_sub(start_col);
            tokens.push((
                start_line,
                start_col,
                length,
                TOKEN_TYPE_MACRO,
                TOKEN_MODIFIER_NONE,
            ));
        } else {
            let lines: Vec<&str> = text.lines().collect();

            if let Some(first_line) = lines.get(start_line as usize) {
                let first_line_len = (first_line.len() as u32).saturating_sub(start_col);
                tokens.push((
                    start_line,
                    start_col,
                    first_line_len,
                    TOKEN_TYPE_MACRO,
                    TOKEN_MODIFIER_NONE,
                ));
            }

            for line_idx in (start_line + 1)..end_line {
                if let Some(line) = lines.get(line_idx as usize) {
                    tokens.push((
                        line_idx,
                        0,
                        line.len() as u32,
                        TOKEN_TYPE_MACRO,
                        TOKEN_MODIFIER_NONE,
                    ));
                }
            }

            tokens.push((end_line, 0, end_col, TOKEN_TYPE_MACRO, TOKEN_MODIFIER_NONE));
        }

        tokens
    }

    fn source_fix_all_kind() -> CodeActionKind {
        CodeActionKind::from(SOURCE_FIX_ALL_T_LINTER)
    }

    fn refactor_rewrite_kind() -> CodeActionKind {
        CodeActionKind::from(REFACTOR_REWRITE_T_LINTER)
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for TLinterLanguageServer {
    async fn initialize(&self, params: InitializeParams) -> JsonRpcResult<InitializeResult> {
        let startup_config = self.config.read().await.clone();
        *self.client_support.write().await = client_support_from_capabilities(&params.capabilities);
        let config =
            parse_initialization_config(params.initialization_options.clone(), startup_config);
        *self.config.write().await = config.clone();
        let workspace_roots = workspace_roots_from_initialize_params(&params);
        *self.workspace_roots.write().await = workspace_roots.clone();
        let initialize_params = serde_json::to_value(&params)
            .map_err(|error| internal_error(anyhow::Error::new(error)))?;
        *self.initialize_params.write().await = Some(initialize_params.clone());
        *self.type_checker.lock().await =
            TypeCheckerState::new(config.type_checking.clone(), workspace_roots.clone());
        if config.pyright_path.is_some() {
            self.client
                .log_message(
                    MessageType::WARNING,
                    "t-linter.pyrightPath is deprecated and has no effect; use typeChecking.command",
                )
                .await;
        }
        if config.ruff_pipeline.enabled {
            match RuffPipelineClient::start(
                &config.ruff_pipeline,
                initialize_params,
                &workspace_roots,
            )
            .await
            {
                Ok(client) => {
                    *self.ruff.write().await = Some(Arc::new(client));
                }
                Err(error) => {
                    return Err(internal_error(
                        error.context("Failed to initialize Ruff pipeline"),
                    ));
                }
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                position_encoding: Some(PositionEncodingKind::UTF16),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: vec![
                                    SemanticTokenType::NAMESPACE,
                                    SemanticTokenType::TYPE,
                                    SemanticTokenType::CLASS,
                                    SemanticTokenType::ENUM,
                                    SemanticTokenType::INTERFACE,
                                    SemanticTokenType::STRUCT,
                                    SemanticTokenType::TYPE_PARAMETER,
                                    SemanticTokenType::PARAMETER,
                                    SemanticTokenType::VARIABLE,
                                    SemanticTokenType::PROPERTY,
                                    SemanticTokenType::ENUM_MEMBER,
                                    SemanticTokenType::EVENT,
                                    SemanticTokenType::FUNCTION,
                                    SemanticTokenType::METHOD,
                                    SemanticTokenType::MACRO,
                                    SemanticTokenType::KEYWORD,
                                    SemanticTokenType::MODIFIER,
                                    SemanticTokenType::COMMENT,
                                    SemanticTokenType::STRING,
                                    SemanticTokenType::NUMBER,
                                    SemanticTokenType::REGEXP,
                                    SemanticTokenType::OPERATOR,
                                    SemanticTokenType::DECORATOR,
                                ],
                                token_modifiers: vec![
                                    SemanticTokenModifier::DECLARATION,
                                    SemanticTokenModifier::DEFINITION,
                                    SemanticTokenModifier::READONLY,
                                    SemanticTokenModifier::STATIC,
                                    SemanticTokenModifier::DEPRECATED,
                                    SemanticTokenModifier::ABSTRACT,
                                    SemanticTokenModifier::ASYNC,
                                    SemanticTokenModifier::MODIFICATION,
                                    SemanticTokenModifier::DOCUMENTATION,
                                    SemanticTokenModifier::DEFAULT_LIBRARY,
                                ],
                            },
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: Some(false),
                            ..Default::default()
                        },
                    ),
                ),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_range_formatting_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            Self::source_fix_all_kind(),
                            Self::refactor_rewrite_kind(),
                        ]),
                        resolve_provider: Some(true),
                        ..Default::default()
                    }
                    .into(),
                ),
                ..Default::default()
            },
            ..Default::default()
        })
    }
    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "t-linter LSP server initialized")
            .await;
    }

    async fn shutdown(&self) -> JsonRpcResult<()> {
        let ruff = self.ruff.write().await.take();
        if let Some(ruff) = ruff {
            ruff.shutdown().await;
        }
        let type_checker = {
            let mut state = self.type_checker.lock().await;
            state.shutdown = true;
            state.client.take()
        };
        if let Some(type_checker) = type_checker {
            type_checker.shutdown().await;
        }
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        let version = params.text_document.version;

        let forwarded = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: params.text_document.language_id,
                version,
                text: text.clone(),
            },
        };
        self.document_cache
            .insert(uri.clone(), DocumentState { text, version });
        let ruff = self.current_ruff_client().await;
        if let Some(ruff) = ruff {
            let lock = self.ruff_document_lock(&uri);
            let _guard = lock.lock().await;
            match serde_json::to_value(&forwarded) {
                Ok(value) => {
                    if let Err(error) = ruff.did_open(value).await {
                        self.client
                            .log_message(
                                MessageType::ERROR,
                                format!("Failed to sync didOpen to Ruff: {error}"),
                            )
                            .await;
                    }
                }
                Err(error) => {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            format!("Failed to serialize didOpen for Ruff: {error}"),
                        )
                        .await;
                }
            }
        }
        self.schedule_diagnostics(uri);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let DidChangeTextDocumentParams {
            text_document,
            content_changes,
        } = params;
        if content_changes.is_empty() {
            return;
        }

        let uri = text_document.uri.clone();
        let current_text = self
            .document_cache
            .get(&uri)
            .map(|entry| entry.text.clone());
        let text =
            match apply_text_document_content_changes(current_text.as_deref(), &content_changes) {
                Ok(text) => text,
                Err(error) => {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            format!("Failed to apply didChange for {uri}: {error}"),
                        )
                        .await;
                    return;
                }
            };
        self.document_cache.insert(
            uri.clone(),
            DocumentState {
                text,
                version: text_document.version,
            },
        );
        let ruff = self.ruff.read().await.clone();
        if let Some(ruff) = ruff {
            let lock = self.ruff_document_lock(&uri);
            let _guard = lock.lock().await;
            let forwarded = DidChangeTextDocumentParams {
                text_document,
                content_changes,
            };
            match serde_json::to_value(&forwarded) {
                Ok(value) => {
                    if let Err(error) = ruff.did_change(value).await {
                        self.client
                            .log_message(
                                MessageType::ERROR,
                                format!("Failed to sync didChange to Ruff: {error}"),
                            )
                            .await;
                    }
                }
                Err(error) => {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            format!("Failed to serialize didChange for Ruff: {error}"),
                        )
                        .await;
                }
            }
        }
        self.schedule_diagnostics(uri);
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        debug!("Configuration changed: {:?}", params);
        let Some(settings) = configuration_settings(params.settings) else {
            return;
        };
        let previous = self.config.read().await.clone();
        let next = parse_initialization_config(Some(settings), previous.clone());
        if next == previous {
            return;
        }
        *self.config.write().await = next.clone();
        self.apply_runtime_config(previous, next).await;
    }
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.document_cache.remove(&params.text_document.uri);
        let ruff = self.ruff.read().await.clone();
        if let Some(ruff) = ruff {
            let lock = self.ruff_document_lock(&params.text_document.uri);
            let _guard = lock.lock().await;
            match serde_json::to_value(&params) {
                Ok(value) => {
                    if let Err(error) = ruff.did_close(value).await {
                        self.client
                            .log_message(
                                MessageType::ERROR,
                                format!("Failed to sync didClose to Ruff: {error}"),
                            )
                            .await;
                    }
                }
                Err(error) => {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            format!("Failed to serialize didClose for Ruff: {error}"),
                        )
                        .await;
                }
            }
        }
        self.ruff_document_locks.remove(&params.text_document.uri);
        let type_checker = self.type_checker.lock().await.client.clone();
        if let Some(type_checker) = type_checker
            && let Err(error) = type_checker.close_shadow(&params.text_document.uri).await
        {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("Failed to close type checker shadow document: {error}"),
                )
                .await;
        }
        if let Some((_, task)) = self.diagnostic_tasks.remove(&params.text_document.uri) {
            task.handle.abort();
        }
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> JsonRpcResult<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;

        match self.generate_semantic_tokens(&uri).await {
            Ok(tokens) => Ok(Some(SemanticTokensResult::Tokens(tokens))),
            Err(e) => {
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!("Token generation failed: {}", e),
                    )
                    .await;
                Ok(None)
            }
        }
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> JsonRpcResult<Option<Vec<TextEdit>>> {
        self.format_uri(&params.text_document.uri, None, Some(&params.options))
            .await
            .map(Some)
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> JsonRpcResult<Option<Vec<TextEdit>>> {
        self.format_uri(
            &params.text_document.uri,
            Some(params.range),
            Some(&params.options),
        )
        .await
        .map(Some)
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> JsonRpcResult<Option<CodeActionResponse>> {
        let mut actions = Vec::new();
        let requested_kinds = params.context.only.as_deref();
        let source_fix_all_kind = Self::source_fix_all_kind();
        let rewrite_kind = Self::refactor_rewrite_kind();
        let quickfix_kind = CodeActionKind::QUICKFIX;
        let defer_edits = self.client_support.read().await.resolve_code_action_edits;

        if requested_code_action_kinds_include(requested_kinds, &quickfix_kind) {
            let source = self
                .document_cache
                .get(&params.text_document.uri)
                .map(|entry| entry.text.clone())
                .ok_or_else(|| tower_lsp::jsonrpc::Error::invalid_params("Document not found"))?;
            actions.extend(
                suggested_edit_code_actions(
                    &params.text_document.uri,
                    &source,
                    &params.context.diagnostics,
                )
                .map_err(internal_error)?,
            );
        }

        if requested_code_action_kinds_include(requested_kinds, &source_fix_all_kind) {
            if defer_edits {
                if let Some(action) = self
                    .deferred_source_fix_all_action(&params.text_document.uri)
                    .await?
                {
                    actions.push(action.into());
                }
            } else {
                let edits = match self
                    .collect_document_format_edits(&params.text_document.uri, None)
                    .await
                {
                    Ok(edits) => edits,
                    Err(err) if is_internal_lsp_error(&err) => {
                        self.log_skipped_code_action_error(&source_fix_all_kind, &err)
                            .await;
                        Vec::new()
                    }
                    Err(err) => return Err(err),
                };

                if !edits.is_empty() {
                    let title = source_fix_all_title(self.ruff.read().await.is_some());

                    actions.push(
                        CodeAction {
                            title: title.to_string(),
                            kind: Some(source_fix_all_kind),
                            edit: Some(workspace_edit_for_uri(&params.text_document.uri, edits)),
                            is_preferred: Some(true),
                            ..Default::default()
                        }
                        .into(),
                    );
                }
            }
        }

        if requested_code_action_kinds_include(requested_kinds, &rewrite_kind) {
            if defer_edits {
                if let Some(action) = self
                    .deferred_rewrite_action(&params.text_document.uri, &params.range)
                    .await?
                {
                    actions.push(action.into());
                }
            } else {
                match self
                    .collect_single_template_selection_format_edits(
                        &params.text_document.uri,
                        &params.range,
                        None,
                    )
                    .await
                {
                    Ok(SelectionFormatEdits::Edits(edits)) if !edits.is_empty() => {
                        actions.push(
                            CodeAction {
                                title: "Rewrite template string with t-linter".to_string(),
                                kind: Some(rewrite_kind),
                                edit: Some(workspace_edit_for_uri(
                                    &params.text_document.uri,
                                    edits,
                                )),
                                ..Default::default()
                            }
                            .into(),
                        );
                    }
                    Ok(
                        SelectionFormatEdits::NoTemplate
                        | SelectionFormatEdits::MultipleTemplates
                        | SelectionFormatEdits::Edits(_),
                    ) => {}
                    Err(err) if is_internal_lsp_error(&err) => {
                        self.log_skipped_code_action_error(&rewrite_kind, &err)
                            .await;
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(actions))
        }
    }

    async fn code_action_resolve(&self, mut params: CodeAction) -> JsonRpcResult<CodeAction> {
        if params.edit.is_some() {
            return Ok(params);
        }

        let data = decode_code_action_data(params.data.clone())?;
        let (uri, document_version, edits) = match data.action {
            TLinterCodeAction::SourceFixAll {
                uri,
                document_version,
            } => {
                let kind = Self::source_fix_all_kind();
                let edits = match self
                    .collect_document_format_edits_matching_version(
                        &uri,
                        Some(document_version),
                        None,
                    )
                    .await
                {
                    Ok(edits) => edits,
                    Err(err) if is_internal_lsp_error(&err) => {
                        self.log_skipped_code_action_error(&kind, &err).await;
                        Vec::new()
                    }
                    Err(err) => return Err(err),
                };
                (uri, document_version, edits)
            }
            TLinterCodeAction::RefactorRewrite {
                uri,
                document_version,
                range,
            } => {
                let kind = Self::refactor_rewrite_kind();
                let edits = match self
                    .collect_single_template_selection_format_edits_matching_version(
                        &uri,
                        &range,
                        Some(document_version),
                        None,
                    )
                    .await
                {
                    Ok(SelectionFormatEdits::Edits(edits)) => edits,
                    Ok(
                        SelectionFormatEdits::NoTemplate | SelectionFormatEdits::MultipleTemplates,
                    ) => Vec::new(),
                    Err(err) if is_internal_lsp_error(&err) => {
                        self.log_skipped_code_action_error(&kind, &err).await;
                        Vec::new()
                    }
                    Err(err) => return Err(err),
                };
                (uri, document_version, edits)
            }
        };

        if edits.is_empty() {
            return Ok(params);
        }

        let versioned = self.client_support.read().await.versioned_workspace_edits;
        params.edit = Some(workspace_edit_for_uri_with_optional_version(
            &uri,
            if versioned {
                Some(document_version)
            } else {
                None
            },
            edits,
        ));
        Ok(params)
    }
}

enum SelectionFormatEdits {
    NoTemplate,
    MultipleTemplates,
    Edits(Vec<TextEdit>),
}

impl TLinterLanguageServer {
    fn ruff_document_lock(&self, uri: &Url) -> Arc<tokio::sync::Mutex<()>> {
        let entry = self
            .ruff_document_locks
            .entry(uri.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())));
        Arc::clone(entry.value())
    }

    async fn apply_runtime_config(&self, previous: TLinterConfig, next: TLinterConfig) {
        if previous.type_checking != next.type_checking {
            let type_checker = {
                let mut state = self.type_checker.lock().await;
                state.config = next.type_checking.clone();
                state.consecutive_failures = 0;
                state.disabled = false;
                state.client.take()
            };
            if let Some(type_checker) = type_checker {
                type_checker.shutdown().await;
            }
        }

        if previous.ruff_pipeline == next.ruff_pipeline {
            return;
        }

        let ruff = self.ruff.write().await.take();
        if let Some(ruff) = ruff {
            ruff.shutdown().await;
        }
        if !next.ruff_pipeline.enabled {
            return;
        }

        let Some(initialize_params) = self.initialize_params.read().await.clone() else {
            self.client
                .log_message(
                    MessageType::WARNING,
                    "Cannot restart Ruff pipeline before LSP initialization completes",
                )
                .await;
            return;
        };
        let workspace_roots = self.workspace_roots.read().await.clone();
        match RuffPipelineClient::start(&next.ruff_pipeline, initialize_params, &workspace_roots)
            .await
        {
            Ok(ruff) => {
                *self.ruff.write().await = Some(Arc::new(ruff));
            }
            Err(error) => {
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!(
                            "Failed to restart Ruff pipeline after configuration change: {error}"
                        ),
                    )
                    .await;
            }
        }
    }

    async fn current_ruff_client(&self) -> Option<Arc<RuffPipelineClient>> {
        let current = self.ruff.read().await.clone();
        if let Some(ruff) = current {
            if ruff.is_running().await {
                return Some(ruff);
            }
            return self.restart_ruff_pipeline_after_exit().await;
        }
        None
    }

    async fn restart_ruff_pipeline_after_exit(&self) -> Option<Arc<RuffPipelineClient>> {
        let config = self.config.read().await.ruff_pipeline.clone();
        if !config.enabled {
            return None;
        }
        let Some(initialize_params) = self.initialize_params.read().await.clone() else {
            self.client
                .log_message(
                    MessageType::WARNING,
                    "Cannot restart Ruff pipeline before LSP initialization completes",
                )
                .await;
            return None;
        };
        let workspace_roots = self.workspace_roots.read().await.clone();
        let previous = self.ruff.write().await.take();
        if let Some(previous) = previous {
            previous.shutdown().await;
        }
        match RuffPipelineClient::start(&config, initialize_params, &workspace_roots).await {
            Ok(ruff) => {
                let ruff = Arc::new(ruff);
                *self.ruff.write().await = Some(Arc::clone(&ruff));
                Some(ruff)
            }
            Err(error) => {
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!("Failed to restart Ruff pipeline after exit: {error}"),
                    )
                    .await;
                None
            }
        }
    }

    fn schedule_diagnostics(&self, uri: Url) {
        let generation = match self.diagnostic_tasks.remove(&uri) {
            Some((_, task)) => {
                task.handle.abort();
                task.generation.saturating_add(1)
            }
            None => 1,
        };

        let client = self.client.clone();
        let document_cache = Arc::clone(&self.document_cache);
        let diagnostic_tasks = Arc::clone(&self.diagnostic_tasks);
        let type_checker_state = Arc::clone(&self.type_checker);
        let task_uri = uri.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(DIAGNOSTIC_DEBOUNCE_MS)).await;

            let Some((text, version)) = document_cache
                .get(&task_uri)
                .map(|entry| (entry.text.clone(), entry.version))
            else {
                remove_diagnostic_task_if_current(&diagnostic_tasks, &task_uri, generation);
                return;
            };

            let Some(path) = uri_to_path(&task_uri) else {
                client
                    .log_message(
                        MessageType::ERROR,
                        format!("Unable to resolve filesystem path for {}", task_uri),
                    )
                    .await;
                remove_diagnostic_task_if_current(&diagnostic_tasks, &task_uri, generation);
                return;
            };

            let diagnostics = match lint_source(&path, &text) {
                Ok(result) => result
                    .diagnostics
                    .iter()
                    .map(|diagnostic| lint_diagnostic_to_lsp(diagnostic, &text))
                    .collect::<Result<Vec<_>>>(),
                Err(err) => {
                    client
                        .log_message(
                            MessageType::ERROR,
                            format!("Diagnostic analysis failed for {}: {}", task_uri, err),
                        )
                        .await;
                    remove_diagnostic_task_if_current(&diagnostic_tasks, &task_uri, generation);
                    return;
                }
            };
            let diagnostics = match diagnostics {
                Ok(diagnostics) => diagnostics,
                Err(err) => {
                    client
                        .log_message(
                            MessageType::ERROR,
                            format!(
                                "Diagnostic range conversion failed for {}: {}",
                                task_uri, err
                            ),
                        )
                        .await;
                    remove_diagnostic_task_if_current(&diagnostic_tasks, &task_uri, generation);
                    return;
                }
            };
            let current_version = document_cache.get(&task_uri).map(|entry| entry.version);
            if current_version != Some(version) {
                remove_diagnostic_task_if_current(&diagnostic_tasks, &task_uri, generation);
                return;
            }

            client
                .publish_diagnostics(task_uri.clone(), diagnostics.clone(), Some(version))
                .await;

            let type_checking_enabled = {
                let state = type_checker_state.lock().await;
                state.config.enabled && !state.disabled
            };
            if type_checking_enabled {
                match synthesize_for_type_check(&path, &text) {
                    Ok(Some(shadow)) => {
                        if let Some(checker) =
                            ensure_type_checker(&type_checker_state, &client).await
                        {
                            match run_interpolation_type_check(
                                &checker, &task_uri, &text, version, &shadow,
                            )
                            .await
                            {
                                Ok(type_diagnostics) if !type_diagnostics.is_empty() => {
                                    let current_version =
                                        document_cache.get(&task_uri).map(|entry| entry.version);
                                    if current_version == Some(version) {
                                        let mut merged = Vec::with_capacity(
                                            diagnostics.len() + type_diagnostics.len(),
                                        );
                                        merged.extend(diagnostics);
                                        merged.extend(type_diagnostics);
                                        client
                                            .publish_diagnostics(
                                                task_uri.clone(),
                                                merged,
                                                Some(version),
                                            )
                                            .await;
                                    }
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    client
                                        .log_message(
                                            MessageType::WARNING,
                                            format!(
                                                "Interpolation type check failed for {}: {}",
                                                task_uri, error
                                            ),
                                        )
                                        .await;
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        client
                            .log_message(
                                MessageType::WARNING,
                                format!(
                                    "Failed to synthesize interpolation type-check shadow for {}: {}",
                                    task_uri, error
                                ),
                            )
                            .await;
                    }
                }
            }
            remove_diagnostic_task_if_current(&diagnostic_tasks, &task_uri, generation);
        });

        self.diagnostic_tasks
            .insert(uri, DiagnosticTask { generation, handle });
    }

    async fn collect_document_format_edits(
        &self,
        uri: &Url,
        formatting_options: Option<&FormattingOptions>,
    ) -> JsonRpcResult<Vec<TextEdit>> {
        self.collect_document_format_edits_matching_version(uri, None, formatting_options)
            .await
    }

    async fn collect_document_format_edits_matching_version(
        &self,
        uri: &Url,
        expected_version: Option<i32>,
        formatting_options: Option<&FormattingOptions>,
    ) -> JsonRpcResult<Vec<TextEdit>> {
        let state = self
            .document_cache
            .get(uri)
            .ok_or_else(|| tower_lsp::jsonrpc::Error::invalid_params("Document not found"))?
            .clone();
        ensure_document_version(uri, state.version, expected_version)?;
        let text = state.text;
        let ruff = self.ruff.read().await.clone();
        if ruff.is_none() && !might_contain_template_string(&text) {
            ensure_cached_document_version(&self.document_cache, uri, expected_version)?;
            return Ok(Vec::new());
        }
        let options =
            resolve_lsp_format_options(uri, formatting_options).map_err(internal_error)?;
        let ruff_text = if let Some(ruff) = ruff {
            self.apply_ruff_pipeline(&ruff, uri, &text, state.version, formatting_options)
                .await
                .map_err(internal_error)?
        } else {
            text.clone()
        };
        let edits = format_document_with_options(&ruff_text, &options).map_err(internal_error)?;
        let final_text =
            t_linter_core::apply_template_edits(&ruff_text, &edits).map_err(internal_error)?;

        ensure_cached_document_version(&self.document_cache, uri, expected_version)?;
        final_text_edit(&text, &final_text).map_err(internal_error)
    }

    async fn apply_ruff_pipeline(
        &self,
        ruff: &RuffPipelineClient,
        uri: &Url,
        text: &str,
        version: i32,
        formatting_options: Option<&FormattingOptions>,
    ) -> Result<String> {
        let lock = self.ruff_document_lock(uri);
        let _guard = lock.lock().await;
        let mut shadow_text = text.to_string();
        let mut shadow_version = version.saturating_add(1);

        sync_ruff_shadow_document(ruff, uri, shadow_version, &shadow_text).await?;
        let result = async {
            let actions = ruff
                .source_fix_all(&ruff_code_action_params(uri, &shadow_text))
                .await?;
            apply_ruff_code_action_step(ruff, uri, &mut shadow_text, &mut shadow_version, &actions)
                .await?;

            let actions = ruff
                .organize_imports(&ruff_code_action_params(uri, &shadow_text))
                .await?;
            apply_ruff_code_action_step(ruff, uri, &mut shadow_text, &mut shadow_version, &actions)
                .await?;

            let ruff_document_format_edits = ruff
                .format(&DocumentFormattingParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    options: formatting_options
                        .cloned()
                        .unwrap_or_else(default_formatting_options),
                    work_done_progress_params: Default::default(),
                })
                .await?;
            let next_text = apply_lsp_text_edits(&shadow_text, &ruff_document_format_edits)?;
            if next_text != shadow_text {
                shadow_text = next_text;
                shadow_version = shadow_version.saturating_add(1);
                sync_ruff_shadow_document(ruff, uri, shadow_version, &shadow_text).await?;
            }

            Ok(shadow_text)
        }
        .await;

        let restore_version = shadow_version.saturating_add(1);
        let restore_result = sync_ruff_shadow_document(ruff, uri, restore_version, text).await;
        match (result, restore_result) {
            (Ok(shadow_text), Ok(())) => Ok(shadow_text),
            (Ok(_), Err(restore_error)) => {
                Err(restore_error
                    .context("Failed to restore Ruff document after formatting pipeline"))
            }
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(restore_error)) => Err(error.context(format!(
                "Ruff formatting pipeline failed and document restore also failed: {restore_error}"
            ))),
        }
    }

    async fn collect_single_template_selection_format_edits(
        &self,
        uri: &Url,
        range: &Range,
        formatting_options: Option<&FormattingOptions>,
    ) -> JsonRpcResult<SelectionFormatEdits> {
        self.collect_single_template_selection_format_edits_matching_version(
            uri,
            range,
            None,
            formatting_options,
        )
        .await
    }

    async fn collect_single_template_selection_format_edits_matching_version(
        &self,
        uri: &Url,
        range: &Range,
        expected_version: Option<i32>,
        formatting_options: Option<&FormattingOptions>,
    ) -> JsonRpcResult<SelectionFormatEdits> {
        let text = self
            .document_cache
            .get(uri)
            .ok_or_else(|| tower_lsp::jsonrpc::Error::invalid_params("Document not found"))?
            .clone();
        ensure_document_version(uri, text.version, expected_version)?;
        let source = text.text;
        if !might_contain_template_string(&source) {
            return Ok(SelectionFormatEdits::NoTemplate);
        }
        let options =
            resolve_lsp_format_options(uri, formatting_options).map_err(internal_error)?;
        let location = lsp_range_to_location(range, &source).map_err(internal_error)?;
        let mut parser = self.parser.lock().await;
        let templates = parser
            .find_template_strings(&source)
            .map_err(internal_error)?;
        drop(parser);

        let matches = templates
            .iter()
            .filter(|template| locations_overlap(&template.location, &location))
            .count();

        if matches == 0 {
            return Ok(SelectionFormatEdits::NoTemplate);
        }

        if matches > 1 {
            return Ok(SelectionFormatEdits::MultipleTemplates);
        }

        let edits = format_document_range_with_options(&source, &location, &options)
            .map_err(internal_error)?;
        ensure_cached_document_version(&self.document_cache, uri, expected_version)?;
        Ok(SelectionFormatEdits::Edits(
            template_edits_to_lsp(edits, &source).map_err(internal_error)?,
        ))
    }

    async fn deferred_source_fix_all_action(&self, uri: &Url) -> JsonRpcResult<Option<CodeAction>> {
        let Some((document_version, might_have_template)) = self
            .document_cache
            .get(uri)
            .map(|entry| (entry.version, might_contain_template_string(&entry.text)))
        else {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(
                "Document not found",
            ));
        };
        let ruff_enabled = self.ruff.read().await.is_some();
        if !ruff_enabled && !might_have_template {
            return Ok(None);
        }

        Ok(Some(CodeAction {
            title: source_fix_all_title(ruff_enabled).to_string(),
            kind: Some(Self::source_fix_all_kind()),
            edit: None,
            data: Some(encode_code_action_data(TLinterCodeAction::SourceFixAll {
                uri: uri.clone(),
                document_version,
            })?),
            is_preferred: Some(true),
            ..Default::default()
        }))
    }

    async fn deferred_rewrite_action(
        &self,
        uri: &Url,
        range: &Range,
    ) -> JsonRpcResult<Option<CodeAction>> {
        let state = self
            .document_cache
            .get(uri)
            .ok_or_else(|| tower_lsp::jsonrpc::Error::invalid_params("Document not found"))?
            .clone();
        if !might_contain_template_string(&state.text) {
            return Ok(None);
        }

        let location = lsp_range_to_location(range, &state.text).map_err(internal_error)?;
        let mut parser = self.parser.lock().await;
        let locations = parser
            .find_template_string_locations(&state.text)
            .map_err(internal_error)?;
        drop(parser);

        let mut matches = locations
            .iter()
            .filter(|template_location| locations_overlap(template_location, &location));
        if matches.next().is_none() {
            return Ok(None);
        }
        if matches.next().is_some() {
            return Ok(None);
        }

        Ok(Some(CodeAction {
            title: "Rewrite template string with t-linter".to_string(),
            kind: Some(Self::refactor_rewrite_kind()),
            edit: None,
            data: Some(encode_code_action_data(
                TLinterCodeAction::RefactorRewrite {
                    uri: uri.clone(),
                    document_version: state.version,
                    range: *range,
                },
            )?),
            ..Default::default()
        }))
    }

    async fn format_uri(
        &self,
        uri: &Url,
        range: Option<Range>,
        formatting_options: Option<&FormattingOptions>,
    ) -> JsonRpcResult<Vec<TextEdit>> {
        match range {
            Some(range) => {
                match self
                    .collect_single_template_selection_format_edits(uri, &range, formatting_options)
                    .await?
                {
                    SelectionFormatEdits::NoTemplate => Ok(Vec::new()),
                    SelectionFormatEdits::MultipleTemplates => {
                        Err(internal_error(anyhow::anyhow!(
                            "Range formatting must target exactly one template string."
                        )))
                    }
                    SelectionFormatEdits::Edits(edits) => Ok(edits),
                }
            }
            None => {
                self.collect_document_format_edits(uri, formatting_options)
                    .await
            }
        }
    }

    async fn log_skipped_code_action_error(
        &self,
        kind: &CodeActionKind,
        err: &tower_lsp::jsonrpc::Error,
    ) {
        warn!("Skipping {} code action: {}", kind.as_str(), err.message);
        self.client
            .log_message(
                MessageType::WARNING,
                format!("Skipping {} code action: {}", kind.as_str(), err.message),
            )
            .await;
    }

    async fn generate_semantic_tokens(&self, uri: &Url) -> Result<SemanticTokens> {
        let text = self
            .document_cache
            .get(uri)
            .ok_or_else(|| anyhow::anyhow!("Document not found in cache"))?
            .text
            .clone();

        debug!("Generating semantic tokens for: {}", uri);

        let mut parser = self.parser.lock().await;
        let templates = parser.find_template_strings(&text)?;
        let highlight_untyped_templates = self.config.read().await.highlight_untyped_templates;

        let mut all_tokens = Vec::new();

        for (idx, template) in templates.iter().enumerate() {
            info!(
                "Template {}: language={:?}, raw='{}', location={}:{}-{}:{}",
                idx,
                template.language,
                template
                    .raw_content
                    .chars()
                    .take(50)
                    .collect::<String>()
                    .replace('\n', "\\n"),
                template.location.start_line,
                template.location.start_column,
                template.location.end_line,
                template.location.end_column
            );

            if let Some(lang) = &template.language {
                info!("Attempting to highlight {} template", lang);

                let mut highlighter = self.highlighter.lock().await;
                if highlighter.supports_language(lang) {
                    match highlighter.highlight_template(template) {
                        Ok(ranges) => {
                            info!("Successfully highlighted {} ranges", ranges.len());

                            for (i, range) in ranges.iter().take(5).enumerate() {
                                info!(
                                    "  Range {}: {}..{} type={}",
                                    i, range.start_byte, range.end_byte, range.highlight_name
                                );
                            }

                            let tokens = highlighter.to_lsp_tokens(ranges, template);
                            info!("Converted to {} LSP tokens", tokens.len());

                            all_tokens.extend(tokens);
                        }
                        Err(e) => {
                            self.client
                                .log_message(
                                    MessageType::ERROR,
                                    format!("Failed to highlight {} template: {}", lang, e),
                                )
                                .await;

                            let tokens = self.generate_fallback_tokens(template, &text);
                            all_tokens.extend(tokens);
                        }
                    }
                } else {
                    info!(
                        "Unsupported highlight language {}, using fallback tokens",
                        lang
                    );
                    let tokens = self.generate_fallback_tokens(template, &text);
                    all_tokens.extend(tokens);
                }
            } else {
                if !highlight_untyped_templates {
                    info!("No language specified and untyped highlighting is disabled");
                    continue;
                }
                info!("No language specified, using fallback tokens");
                let tokens = self.generate_fallback_tokens(template, &text);
                all_tokens.extend(tokens);
            }
        }

        all_tokens.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

        all_tokens
            .dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2 && a.3 == b.3 && a.4 == b.4);

        info!("Final sorted tokens:");
        for (i, &(line, col, len, typ, _)) in all_tokens.iter().enumerate().take(10) {
            info!(
                "  Token {}: line={}, col={}, len={}, type={}",
                i, line, col, len, typ
            );
        }

        let lsp_tokens = byte_tokens_to_lsp_tokens(&text, all_tokens)?;
        let data = self.convert_to_semantic_tokens(lsp_tokens);

        info!("Generated {} semantic token values", data.len());

        Ok(SemanticTokens {
            result_id: None,
            data,
        })
    }
    fn convert_to_semantic_tokens(
        &self,
        tokens: Vec<(u32, u32, u32, u32, u32)>,
    ) -> Vec<SemanticToken> {
        let mut semantic_tokens = Vec::new();
        let mut prev_line = 0;
        let mut prev_start = 0;

        for (line, start, length, token_type, modifiers) in tokens {
            let delta_line = line.saturating_sub(prev_line);
            let delta_start = if delta_line == 0 {
                start.saturating_sub(prev_start)
            } else {
                start
            };

            semantic_tokens.push(SemanticToken {
                delta_line,
                delta_start,
                length,
                token_type,
                token_modifiers_bitset: modifiers,
            });

            prev_line = line;
            prev_start = start;
        }

        semantic_tokens
    }
}

fn resolve_lsp_format_options(
    uri: &Url,
    formatting_options: Option<&FormattingOptions>,
) -> Result<CoreFormatOptions> {
    let line_length = formatting_options
        .and_then(extract_line_length_from_lsp_options)
        .or_else(|| {
            uri.to_file_path()
                .ok()
                .and_then(|path| load_project_config_for_path(&path).ok())
                .and_then(|config| config.line_length)
        })
        .unwrap_or(80)
        .max(1);

    Ok(CoreFormatOptions { line_length })
}

fn extract_line_length_from_lsp_options(options: &FormattingOptions) -> Option<usize> {
    options
        .properties
        .get("printWidth")
        .and_then(json_value_to_usize)
        .or_else(|| {
            options
                .properties
                .get("lineLength")
                .and_then(json_value_to_usize)
        })
}

fn json_value_to_usize(value: &FormattingProperty) -> Option<usize> {
    match value {
        FormattingProperty::Number(value) => usize::try_from(*value).ok(),
        FormattingProperty::String(value) => value.parse::<usize>().ok(),
        FormattingProperty::Bool(_) => None,
    }
}

fn remove_diagnostic_task_if_current(
    diagnostic_tasks: &DashMap<Url, DiagnosticTask>,
    uri: &Url,
    generation: u64,
) {
    diagnostic_tasks.remove_if(uri, |_, task| task.generation == generation);
}

fn lint_diagnostic_to_lsp(diagnostic: &LintDiagnostic, source: &str) -> Result<Diagnostic> {
    let location = t_linter_core::Location {
        start_line: diagnostic.start_line,
        start_column: diagnostic.start_column,
        end_line: diagnostic.end_line,
        end_column: diagnostic.end_column,
    };
    Ok(Diagnostic {
        range: location_to_lsp_range(&location, source)?,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String(diagnostic.rule.clone())),
        source: Some("t-linter".to_string()),
        message: diagnostic.message.clone(),
        data: Some(serde_json::to_value(lint_diagnostic_data(diagnostic))?),
        ..Default::default()
    })
}

fn lint_diagnostic_data(diagnostic: &LintDiagnostic) -> DiagnosticData {
    DiagnosticData {
        expected_type: diagnostic.expected_type.clone(),
        found_type: diagnostic.found_type.clone(),
        schema_pointer: diagnostic.schema_pointer.clone(),
        source_of_truth: diagnostic.source_of_truth.clone(),
        suggested_edits: diagnostic.suggested_edits.clone(),
    }
}

fn location_to_lsp_range(location: &t_linter_core::Location, source: &str) -> Result<Range> {
    let line_starts = line_start_offsets(source);
    let start = location_position_to_byte_offset(
        source,
        &line_starts,
        location.start_line,
        location.start_column,
    )?;
    let end = location_position_to_byte_offset(
        source,
        &line_starts,
        location.end_line,
        location.end_column,
    )?;
    Ok(Range {
        start: byte_offset_to_lsp_position(source, &line_starts, start)?,
        end: byte_offset_to_lsp_position(source, &line_starts, end)?,
    })
}

fn lsp_range_to_location(range: &Range, source: &str) -> Result<t_linter_core::Location> {
    let line_starts = line_start_offsets(source);
    let start = lsp_position_to_byte_offset(source, &line_starts, range.start)?;
    let end = lsp_position_to_byte_offset(source, &line_starts, range.end)?;
    Ok(t_linter_core::Location {
        start_line: range.start.line as usize + 1,
        start_column: byte_column_for_offset(&line_starts, range.start.line as usize, start)? + 1,
        end_line: range.end.line as usize + 1,
        end_column: byte_column_for_offset(&line_starts, range.end.line as usize, end)? + 1,
    })
}

fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()
}

fn template_edits_to_lsp(
    edits: Vec<t_linter_core::TemplateEdit>,
    source: &str,
) -> Result<Vec<TextEdit>> {
    edits
        .into_iter()
        .map(|edit| {
            Ok(TextEdit {
                range: location_to_lsp_range(&edit.location, source)?,
                new_text: edit.replacement,
            })
        })
        .collect()
}

fn suggested_edit_code_actions(
    uri: &Url,
    source: &str,
    diagnostics: &[Diagnostic],
) -> Result<Vec<CodeActionOrCommand>> {
    diagnostics
        .iter()
        .filter_map(|diagnostic| {
            let data = diagnostic
                .data
                .clone()
                .and_then(|data| serde_json::from_value::<DiagnosticData>(data).ok())?;
            if data.suggested_edits.is_empty() {
                return None;
            }
            Some((diagnostic, data.suggested_edits))
        })
        .map(|(diagnostic, edits)| {
            let edits = diagnostic_edits_to_lsp(edits, source)?;
            Ok(CodeAction {
                title: "Apply t-linter suggested edit".to_string(),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic.clone()]),
                edit: Some(workspace_edit_for_uri(uri, edits)),
                is_preferred: Some(true),
                ..Default::default()
            }
            .into())
        })
        .collect()
}

fn diagnostic_edits_to_lsp(edits: Vec<DiagnosticEdit>, source: &str) -> Result<Vec<TextEdit>> {
    edits
        .into_iter()
        .map(|edit| {
            let location = t_linter_core::Location {
                start_line: edit.range.start_line,
                start_column: edit.range.start_column,
                end_line: edit.range.end_line,
                end_column: edit.range.end_column,
            };
            Ok(TextEdit {
                range: location_to_lsp_range(&location, source)?,
                new_text: edit.new_text,
            })
        })
        .collect()
}

fn workspace_edit_for_uri(uri: &Url, edits: Vec<TextEdit>) -> WorkspaceEdit {
    workspace_edit_for_uri_with_optional_version(uri, None, edits)
}

fn workspace_edit_for_uri_with_optional_version(
    uri: &Url,
    version: Option<i32>,
    edits: Vec<TextEdit>,
) -> WorkspaceEdit {
    match version {
        Some(version) => WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier::new(uri.clone(), version),
                edits: edits.into_iter().map(OneOf::Left).collect(),
            }])),
            change_annotations: None,
        },
        None => WorkspaceEdit::new(HashMap::from([(uri.clone(), edits)])),
    }
}

async fn run_interpolation_type_check(
    checker: &type_checker::TypeCheckerClient,
    uri: &Url,
    source: &str,
    version: i32,
    shadow: &ShadowDocument,
) -> Result<Vec<Diagnostic>> {
    checker
        .open_or_update_shadow(uri, &shadow.text, version)
        .await?;
    let diagnostics = checker.pull_diagnostics(uri).await?;
    remap_interpolation_type_diagnostics(
        shadow,
        source,
        &diagnostics,
        checker.position_encoding(),
        checker.backend(),
    )
}

fn remap_interpolation_type_diagnostics(
    shadow: &ShadowDocument,
    source: &str,
    diagnostics: &[Diagnostic],
    encoding: NegotiatedEncoding,
    backend: TypeCheckerBackend,
) -> Result<Vec<Diagnostic>> {
    let mut candidates = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic_rule_code(diagnostic)
                .is_some_and(|code| is_assignment_diagnostic(backend, code))
        })
        .filter_map(|diagnostic| {
            match diagnostic_range_to_byte_range(&shadow.text, &diagnostic.range, encoding) {
                Ok(range) => Some((range, diagnostic)),
                Err(error) => {
                    debug!(
                        "Skipping {} diagnostic with invalid range: {error}",
                        backend.as_str()
                    );
                    None
                }
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(range, _)| range.start);

    let mut remapped = Vec::new();
    for site in &shadow.sites {
        let Some((_, diagnostic)) = candidates
            .iter()
            .filter(|(range, _)| ranges_intersect(range, &site.shadow_rhs_byte_range))
            .min_by_key(|(range, _)| range.start)
        else {
            continue;
        };
        let found_type = found_type_from_message(&diagnostic.message, backend)
            .unwrap_or("unknown")
            .to_string();
        remapped.push(Diagnostic {
            range: location_to_lsp_range(&site.original_location, source)?,
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String(INTERPOLATION_TYPE_RULE.to_string())),
            source: Some(format!("t-linter ({})", backend.as_str())),
            message: interpolation_type_message(diagnostic, site, &found_type),
            data: Some(serde_json::to_value(DiagnosticData {
                expected_type: Some(site.expected_type.clone()),
                found_type: Some(found_type),
                schema_pointer: None,
                source_of_truth: Some(site.expected_description.clone()),
                suggested_edits: Vec::new(),
            })?),
            ..Default::default()
        });
    }
    Ok(remapped)
}

fn is_assignment_diagnostic(backend: TypeCheckerBackend, code: &str) -> bool {
    match backend {
        TypeCheckerBackend::Ty => code == TY_INVALID_ASSIGNMENT_RULE,
        TypeCheckerBackend::Pyright => code == PYRIGHT_ASSIGNMENT_RULE,
        TypeCheckerBackend::Pyrefly => code == PYREFLY_ASSIGNMENT_RULE,
    }
}

fn diagnostic_rule_code(diagnostic: &Diagnostic) -> Option<&str> {
    match diagnostic.code.as_ref()? {
        NumberOrString::String(code) => Some(code.as_str()),
        NumberOrString::Number(_) => None,
    }
}

fn interpolation_type_message(
    diagnostic: &Diagnostic,
    site: &t_linter_core::ShadowCheckSite,
    found: &str,
) -> String {
    if found != "unknown" {
        return format!(
            "Interpolation value of type `{found}` is not assignable to {} (expected: {})",
            site.expected_description,
            compact_expected_type(&site.expected_type)
        );
    }
    diagnostic.message.clone()
}

fn found_type_from_message(message: &str, backend: TypeCheckerBackend) -> Option<&str> {
    match backend {
        TypeCheckerBackend::Ty => {
            for marker in ["found `", "Object of type `"] {
                let Some(start) = message.find(marker).map(|start| start + marker.len()) else {
                    continue;
                };
                let end = message[start..].find('`')?;
                return Some(&message[start..start + end]);
            }
            None
        }
        TypeCheckerBackend::Pyright => quoted_type_before(message, " is not assignable")
            .or_else(|| quoted_type_after_marker(message, "Type \""))
            .or_else(|| quoted_type_after_marker(message, "Expression of type \"")),
        TypeCheckerBackend::Pyrefly => {
            let message = message.trim_start();
            let remainder = message.strip_prefix('`')?;
            let end = remainder.find('`')?;
            Some(&remainder[..end])
        }
    }
}

fn quoted_type_before<'a>(message: &'a str, suffix: &str) -> Option<&'a str> {
    let suffix_start = message.find(suffix)?;
    let before = &message[..suffix_start];
    let end = before.rfind('"')?;
    let start = before[..end].rfind('"')? + 1;
    Some(&before[start..end])
}

fn quoted_type_after_marker<'a>(message: &'a str, marker: &str) -> Option<&'a str> {
    let start = message.find(marker)? + marker.len();
    let end = message[start..].find('"')?;
    Some(&message[start..start + end])
}

fn compact_expected_type(expected_type: &str) -> String {
    expected_type
        .replace("dict[str, object]", "dict")
        .replace("list[object]", "list")
}

fn diagnostic_range_to_byte_range(
    source: &str,
    range: &Range,
    encoding: NegotiatedEncoding,
) -> Result<std::ops::Range<usize>> {
    let line_starts = line_start_offsets(source);
    let start = match encoding {
        NegotiatedEncoding::Utf8 => utf8_position_to_byte_offset(source, &line_starts, range.start),
        NegotiatedEncoding::Utf16 => lsp_position_to_byte_offset(source, &line_starts, range.start),
    }?;
    let end = match encoding {
        NegotiatedEncoding::Utf8 => utf8_position_to_byte_offset(source, &line_starts, range.end),
        NegotiatedEncoding::Utf16 => lsp_position_to_byte_offset(source, &line_starts, range.end),
    }?;
    Ok(start..end)
}

fn utf8_position_to_byte_offset(
    source: &str,
    line_starts: &[usize],
    position: Position,
) -> Result<usize> {
    let line_start = *line_starts
        .get(position.line as usize)
        .ok_or_else(|| anyhow::anyhow!("Line {} is out of bounds", position.line))?;
    let line_end = line_starts
        .get(position.line as usize + 1)
        .map(|next| next.saturating_sub(1))
        .unwrap_or(source.len());
    let offset = line_start + position.character as usize;
    if offset > line_end {
        return Err(anyhow::anyhow!(
            "UTF-8 character {} is out of bounds",
            position.character
        ));
    }
    Ok(offset)
}

fn ranges_intersect(left: &std::ops::Range<usize>, right: &std::ops::Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn byte_tokens_to_lsp_tokens(
    source: &str,
    tokens: Vec<(u32, u32, u32, u32, u32)>,
) -> Result<Vec<(u32, u32, u32, u32, u32)>> {
    let line_starts = line_start_offsets(source);
    let mut converted = Vec::with_capacity(tokens.len());
    for token in tokens {
        let Some(token) = byte_token_to_lsp_token(source, &line_starts, token)? else {
            continue;
        };
        converted.push(token);
    }
    Ok(converted)
}

fn byte_token_to_lsp_token(
    source: &str,
    line_starts: &[usize],
    token: (u32, u32, u32, u32, u32),
) -> Result<Option<(u32, u32, u32, u32, u32)>> {
    let (line, start_col, length, token_type, modifiers) = token;
    let line_index = line as usize;
    let line_start = *line_starts
        .get(line_index)
        .ok_or_else(|| anyhow::anyhow!("Semantic token line {line} is out of bounds"))?;
    let line_end = line_starts
        .get(line_index + 1)
        .map(|next| next.saturating_sub(1))
        .unwrap_or(source.len());
    let start = line_start
        .checked_add(start_col as usize)
        .ok_or_else(|| anyhow::anyhow!("Semantic token start offset overflowed"))?;
    let end = start
        .checked_add(length as usize)
        .ok_or_else(|| anyhow::anyhow!("Semantic token end offset overflowed"))?;
    if start > line_end || end > line_end {
        return Err(anyhow::anyhow!(
            "Semantic token byte range {start_col}..{} is out of bounds",
            start_col.saturating_add(length)
        ));
    }

    let start = byte_offset_to_lsp_position(source, line_starts, start)?;
    let end = byte_offset_to_lsp_position(source, line_starts, end)?;
    if start.line != end.line {
        return Err(anyhow::anyhow!("Semantic token spans multiple lines"));
    }
    let length = end.character.saturating_sub(start.character);
    if length == 0 {
        return Ok(None);
    }

    Ok(Some((line, start.character, length, token_type, modifiers)))
}

fn default_formatting_options() -> FormattingOptions {
    FormattingOptions {
        tab_size: 4,
        insert_spaces: true,
        ..Default::default()
    }
}

fn ruff_code_action_params(uri: &Url, text: &str) -> CodeActionParams {
    CodeActionParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        range: full_lsp_range(text),
        context: CodeActionContext {
            diagnostics: Vec::new(),
            only: None,
            trigger_kind: None,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn full_lsp_range(text: &str) -> Range {
    let line_starts = line_start_offsets(text);
    Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: byte_offset_to_lsp_position(text, &line_starts, text.len())
            .expect("document end is always a valid LSP position"),
    }
}

async fn apply_ruff_code_action_step(
    ruff: &RuffPipelineClient,
    uri: &Url,
    shadow_text: &mut String,
    shadow_version: &mut i32,
    actions: &[CodeAction],
) -> Result<()> {
    let next_text = apply_ruff_code_actions(uri, shadow_text, actions)?;
    if next_text != *shadow_text {
        *shadow_text = next_text;
        *shadow_version = shadow_version.saturating_add(1);
        sync_ruff_shadow_document(ruff, uri, *shadow_version, shadow_text).await?;
    }

    Ok(())
}

async fn sync_ruff_shadow_document(
    ruff: &RuffPipelineClient,
    uri: &Url,
    version: i32,
    text: &str,
) -> Result<()> {
    ruff.did_change(serde_json::to_value(DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier {
            uri: uri.clone(),
            version,
        },
        content_changes: vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: text.to_string(),
        }],
    })?)
    .await
}

fn apply_ruff_code_actions(uri: &Url, source: &str, actions: &[CodeAction]) -> Result<String> {
    let mut text = source.to_string();
    for action in actions {
        let Some(edit) = &action.edit else {
            continue;
        };
        text = apply_workspace_edit_for_uri(uri, &text, edit)?;
    }
    Ok(text)
}

fn apply_workspace_edit_for_uri(uri: &Url, source: &str, edit: &WorkspaceEdit) -> Result<String> {
    let mut text = source.to_string();
    if let Some(changes) = &edit.changes {
        for (edit_uri, edits) in changes {
            if edit_uri != uri {
                return Err(anyhow::anyhow!(
                    "Ruff returned edits for unsupported URI {edit_uri}"
                ));
            }
            text = apply_lsp_text_edits(&text, edits)?;
        }
    }

    if let Some(document_changes) = &edit.document_changes {
        match document_changes {
            DocumentChanges::Edits(edits) => {
                for edit in edits {
                    if edit.text_document.uri != *uri {
                        return Err(anyhow::anyhow!(
                            "Ruff returned documentChanges for unsupported URI {}",
                            edit.text_document.uri
                        ));
                    }
                    text = apply_lsp_text_edits(&text, &text_document_edit_edits(edit)?)?;
                }
            }
            DocumentChanges::Operations(operations) => {
                for operation in operations {
                    match operation {
                        DocumentChangeOperation::Edit(edit) => {
                            if edit.text_document.uri != *uri {
                                return Err(anyhow::anyhow!(
                                    "Ruff returned documentChanges for unsupported URI {}",
                                    edit.text_document.uri
                                ));
                            }
                            text = apply_lsp_text_edits(&text, &text_document_edit_edits(edit)?)?;
                        }
                        DocumentChangeOperation::Op(_) => {
                            return Err(anyhow::anyhow!(
                                "Ruff returned unsupported resource operation"
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(text)
}

fn text_document_edit_edits(edit: &TextDocumentEdit) -> Result<Vec<TextEdit>> {
    edit.edits
        .iter()
        .map(|edit| match edit {
            OneOf::Left(edit) => Ok(edit.clone()),
            OneOf::Right(edit) => Ok(edit.text_edit.clone()),
        })
        .collect()
}

fn requested_code_action_kinds_include(
    requested_kinds: Option<&[CodeActionKind]>,
    action_kind: &CodeActionKind,
) -> bool {
    requested_kinds.is_none_or(|requested_kinds| {
        requested_kinds
            .iter()
            .any(|requested_kind| code_action_kind_matches(requested_kind, action_kind))
    })
}

fn encode_code_action_data(action: TLinterCodeAction) -> JsonRpcResult<serde_json::Value> {
    serde_json::to_value(TLinterCodeActionData {
        schema_version: CODE_ACTION_DATA_SCHEMA_VERSION,
        action,
    })
    .map_err(|error| internal_error(anyhow::Error::new(error)))
}

fn decode_code_action_data(
    data: Option<serde_json::Value>,
) -> JsonRpcResult<TLinterCodeActionData> {
    let Some(data) = data else {
        return Err(tower_lsp::jsonrpc::Error::invalid_params(
            "Missing t-linter code action data",
        ));
    };
    let data = serde_json::from_value::<TLinterCodeActionData>(data).map_err(|error| {
        tower_lsp::jsonrpc::Error::invalid_params(format!(
            "Invalid t-linter code action data: {error}"
        ))
    })?;
    if data.schema_version != CODE_ACTION_DATA_SCHEMA_VERSION {
        return Err(tower_lsp::jsonrpc::Error::invalid_params(format!(
            "Unsupported t-linter code action data schema {}",
            data.schema_version
        )));
    }
    Ok(data)
}

fn ensure_document_version(
    uri: &Url,
    actual_version: i32,
    expected_version: Option<i32>,
) -> JsonRpcResult<()> {
    if let Some(expected_version) = expected_version
        && actual_version != expected_version
    {
        return Err(stale_code_action_error(
            uri,
            expected_version,
            Some(actual_version),
        ));
    }
    Ok(())
}

fn ensure_cached_document_version(
    document_cache: &DashMap<Url, DocumentState>,
    uri: &Url,
    expected_version: Option<i32>,
) -> JsonRpcResult<()> {
    let Some(expected_version) = expected_version else {
        return Ok(());
    };
    let actual_version = document_cache.get(uri).map(|entry| entry.version);
    if actual_version == Some(expected_version) {
        return Ok(());
    }
    Err(stale_code_action_error(
        uri,
        expected_version,
        actual_version,
    ))
}

fn stale_code_action_error(
    uri: &Url,
    expected_version: i32,
    actual_version: Option<i32>,
) -> tower_lsp::jsonrpc::Error {
    let actual = actual_version
        .map(|version| version.to_string())
        .unwrap_or_else(|| "missing".to_string());
    tower_lsp::jsonrpc::Error::invalid_params(format!(
        "Stale t-linter code action for {uri}: expected document version {expected_version}, found {actual}"
    ))
}

fn might_contain_template_string(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if matches!(byte, b't' | b'T') {
            match bytes.get(index + 1) {
                Some(b'\'' | b'"') => return true,
                Some(b'r' | b'R') if matches!(bytes.get(index + 2), Some(b'\'' | b'"')) => {
                    return true;
                }
                _ => {}
            }
        }
        index += 1;
    }
    false
}

fn is_internal_lsp_error(err: &tower_lsp::jsonrpc::Error) -> bool {
    matches!(err.code, tower_lsp::jsonrpc::ErrorCode::InternalError)
}

fn source_fix_all_title(ruff_enabled: bool) -> &'static str {
    if ruff_enabled {
        "Apply Ruff pipeline and format template strings with t-linter"
    } else {
        "Format template strings with t-linter"
    }
}

fn code_action_kind_matches(requested_kind: &CodeActionKind, action_kind: &CodeActionKind) -> bool {
    let requested = requested_kind.as_str();
    let action = action_kind.as_str();

    action == requested
        || action
            .strip_prefix(requested)
            .is_some_and(|suffix| suffix.starts_with('.'))
}

fn locations_overlap(left: &t_linter_core::Location, right: &t_linter_core::Location) -> bool {
    let left_start = (left.start_line, left.start_column);
    let left_end = (left.end_line, left.end_column);
    let right_start = (right.start_line, right.start_column);
    let right_end = (right.end_line, right.end_column);

    if right_start == right_end {
        left_start <= right_start && right_start < left_end
    } else {
        left_start < right_end && right_start < left_end
    }
}

fn client_support_from_capabilities(capabilities: &ClientCapabilities) -> ClientSupport {
    let code_action = capabilities
        .text_document
        .as_ref()
        .and_then(|text_document| text_document.code_action.as_ref());
    let resolve_code_action_edits = code_action
        .and_then(|code_action| code_action.resolve_support.as_ref())
        .is_some_and(|resolve| resolve.properties.iter().any(|property| property == "edit"))
        && code_action
            .and_then(|code_action| code_action.data_support)
            .unwrap_or(false);
    let versioned_workspace_edits = capabilities
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.workspace_edit.as_ref())
        .and_then(|workspace_edit| workspace_edit.document_changes)
        .unwrap_or(false);

    ClientSupport {
        resolve_code_action_edits,
        versioned_workspace_edits,
    }
}

fn internal_error(err: anyhow::Error) -> tower_lsp::jsonrpc::Error {
    tower_lsp::jsonrpc::Error {
        code: tower_lsp::jsonrpc::ErrorCode::InternalError,
        message: err.to_string().into(),
        data: None,
    }
}

fn workspace_roots_from_initialize_params(params: &InitializeParams) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(workspace_folders) = &params.workspace_folders {
        roots.extend(
            workspace_folders
                .iter()
                .filter_map(|folder| folder.uri.to_file_path().ok()),
        );
    }
    if let Some(root_uri) = &params.root_uri
        && let Ok(path) = root_uri.to_file_path()
    {
        roots.push(path);
    }
    if roots.is_empty()
        && let Ok(current_dir) = std::env::current_dir()
    {
        roots.push(current_dir);
    }
    roots.sort();
    roots.dedup();
    roots
}

pub async fn run_server() -> Result<()> {
    run_server_with_config(TLinterConfig::default()).await
}

pub async fn run_server_with_config(config: TLinterConfig) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(move |client| {
        TLinterLanguageServer::with_config(client, config.clone())
            .expect("Failed to create language server")
    });

    Server::new(stdin, stdout, socket).serve(service).await;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TLinterInitializationOptions {
    #[serde(default)]
    enable_type_checking: Option<bool>,
    #[serde(default)]
    pyright_path: Option<String>,
    #[serde(default)]
    highlight_untyped: Option<bool>,
    #[serde(default)]
    highlight_untyped_templates: Option<bool>,
    #[serde(default)]
    ruff_pipeline: Option<RuffPipelineConfig>,
    #[serde(default)]
    type_checking: Option<TypeCheckerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TLinterConfig {
    pub enable_type_checking: bool,
    pub pyright_path: Option<String>,
    pub highlight_untyped_templates: bool,
    pub ruff_pipeline: RuffPipelineConfig,
    pub type_checking: TypeCheckerConfig,
}

impl Default for TLinterConfig {
    fn default() -> Self {
        Self {
            enable_type_checking: true,
            pyright_path: None,
            highlight_untyped_templates: true,
            ruff_pipeline: RuffPipelineConfig::default(),
            type_checking: TypeCheckerConfig::default(),
        }
    }
}

fn parse_initialization_config(
    initialization_options: Option<serde_json::Value>,
    defaults: TLinterConfig,
) -> TLinterConfig {
    let Some(value) = initialization_options else {
        return defaults;
    };
    let Ok(options) = serde_json::from_value::<TLinterInitializationOptions>(value) else {
        return defaults;
    };
    TLinterConfig {
        enable_type_checking: options
            .enable_type_checking
            .unwrap_or(defaults.enable_type_checking),
        pyright_path: options.pyright_path,
        highlight_untyped_templates: options
            .highlight_untyped_templates
            .or(options.highlight_untyped)
            .unwrap_or(defaults.highlight_untyped_templates),
        ruff_pipeline: options.ruff_pipeline.unwrap_or(defaults.ruff_pipeline),
        type_checking: options.type_checking.unwrap_or(defaults.type_checking),
    }
}

fn configuration_settings(settings: serde_json::Value) -> Option<serde_json::Value> {
    if settings.is_null() {
        return None;
    }
    match settings {
        serde_json::Value::Object(mut object) => {
            if let Some(settings) = object.remove("t-linter") {
                return Some(settings);
            }
            if let Some(settings) = object.remove("tLinter") {
                return Some(settings);
            }
            Some(serde_json::Value::Object(object))
        }
        settings => Some(settings),
    }
}

fn apply_text_document_content_changes(
    current_text: Option<&str>,
    content_changes: &[TextDocumentContentChangeEvent],
) -> Result<String> {
    let mut text = String::new();
    let mut initialized = false;
    for change in content_changes {
        let Some(range) = change.range else {
            text.clear();
            text.push_str(&change.text);
            initialized = true;
            continue;
        };
        if !initialized {
            let Some(current_text) = current_text else {
                return Err(anyhow::anyhow!(
                    "Received ranged didChange for a document that is not open"
                ));
            };
            text.push_str(current_text);
            initialized = true;
        }
        text = apply_lsp_content_change(&text, range, &change.text)?;
    }
    if initialized {
        Ok(text)
    } else {
        Ok(current_text.unwrap_or_default().to_string())
    }
}

fn apply_lsp_content_change(source: &str, range: Range, new_text: &str) -> Result<String> {
    let line_starts = line_start_offsets(source);
    let start = lsp_position_to_byte_offset(source, &line_starts, range.start)?;
    let end = lsp_position_to_byte_offset(source, &line_starts, range.end)?;
    if start > end {
        return Err(anyhow::anyhow!(
            "Content change start offset is after end offset"
        ));
    }
    let mut output = String::with_capacity(source.len() - (end - start) + new_text.len());
    output.push_str(&source[..start]);
    output.push_str(new_text);
    output.push_str(&source[end..]);
    Ok(output)
}

fn apply_lsp_text_edits(source: &str, edits: &[TextEdit]) -> Result<String> {
    if edits.is_empty() {
        return Ok(source.to_string());
    }

    let line_starts = line_start_offsets(source);
    let mut byte_edits = edits
        .iter()
        .enumerate()
        .map(|(index, edit)| {
            let start = lsp_position_to_byte_offset(source, &line_starts, edit.range.start)?;
            let end = lsp_position_to_byte_offset(source, &line_starts, edit.range.end)?;
            if start > end {
                return Err(anyhow::anyhow!("TextEdit start offset is after end offset"));
            }
            Ok((start, end, index, edit.new_text.as_str()))
        })
        .collect::<Result<Vec<_>>>()?;

    byte_edits.sort_by_key(|(start, end, index, _)| (*start, *end, *index));
    let mut output = String::with_capacity(
        source.len()
            + byte_edits
                .iter()
                .map(|(_, _, _, text)| text.len())
                .sum::<usize>(),
    );
    let mut cursor = 0;
    let mut edit_index = 0;
    while edit_index < byte_edits.len() {
        let (start, end, _, _) = byte_edits[edit_index];
        if start < cursor {
            return Err(anyhow::anyhow!("Overlapping TextEdit ranges"));
        }
        output.push_str(&source[cursor..start]);

        let mut group_end = edit_index + 1;
        while group_end < byte_edits.len()
            && byte_edits[group_end].0 == start
            && byte_edits[group_end].1 == end
        {
            group_end += 1;
        }

        if start == end {
            let mut insertions = byte_edits[edit_index..group_end].to_vec();
            insertions.sort_by_key(|(_, _, index, _)| *index);
            for (_, _, _, text) in insertions {
                output.push_str(text);
            }
            cursor = start;
            edit_index = group_end;
        } else {
            if group_end != edit_index + 1 {
                return Err(anyhow::anyhow!("Overlapping TextEdit ranges"));
            }
            output.push_str(byte_edits[edit_index].3);
            cursor = end;
            edit_index += 1;
        }
    }
    output.push_str(&source[cursor..]);
    Ok(output)
}

fn final_text_edit(source: &str, final_text: &str) -> Result<Vec<TextEdit>> {
    if source == final_text {
        return Ok(Vec::new());
    }

    let prefix = common_prefix_byte_len(source, final_text);
    let suffix = common_suffix_byte_len(&source[prefix..], &final_text[prefix..]);
    let source_start = prefix;
    let source_end = source.len() - suffix;
    let final_start = prefix;
    let final_end = final_text.len() - suffix;
    let line_starts = line_start_offsets(source);

    Ok(vec![TextEdit {
        range: Range {
            start: byte_offset_to_lsp_position(source, &line_starts, source_start)?,
            end: byte_offset_to_lsp_position(source, &line_starts, source_end)?,
        },
        new_text: final_text[final_start..final_end].to_string(),
    }])
}

fn common_prefix_byte_len(left: &str, right: &str) -> usize {
    let mut prefix = 0;
    for ((left_index, left_char), (right_index, right_char)) in
        left.char_indices().zip(right.char_indices())
    {
        if left_index != right_index || left_char != right_char {
            break;
        }
        prefix = left_index + left_char.len_utf8();
    }
    prefix
}

fn common_suffix_byte_len(left: &str, right: &str) -> usize {
    let mut suffix = 0;
    for (left_char, right_char) in left.chars().rev().zip(right.chars().rev()) {
        if left_char != right_char {
            break;
        }
        suffix += left_char.len_utf8();
    }
    suffix
}

fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

fn location_position_to_byte_offset(
    source: &str,
    line_starts: &[usize],
    line: usize,
    column: usize,
) -> Result<usize> {
    let line_index = line.saturating_sub(1);
    let line_start = *line_starts
        .get(line_index)
        .ok_or_else(|| anyhow::anyhow!("Line {line} is out of bounds"))?;
    let line_end = line_starts
        .get(line_index + 1)
        .map(|next| next.saturating_sub(1))
        .unwrap_or(source.len());
    let offset = line_start + column.saturating_sub(1);
    if offset > line_end {
        return Err(anyhow::anyhow!("Column {column} is out of bounds"));
    }
    Ok(offset)
}

fn byte_column_for_offset(
    line_starts: &[usize],
    line_index: usize,
    offset: usize,
) -> Result<usize> {
    let line_start = *line_starts
        .get(line_index)
        .ok_or_else(|| anyhow::anyhow!("Line {line_index} is out of bounds"))?;
    if offset < line_start {
        return Err(anyhow::anyhow!("Offset {offset} is before line start"));
    }
    Ok(offset - line_start)
}

fn lsp_position_to_byte_offset(
    source: &str,
    line_starts: &[usize],
    position: Position,
) -> Result<usize> {
    let line_start = *line_starts
        .get(position.line as usize)
        .ok_or_else(|| anyhow::anyhow!("Line {} is out of bounds", position.line))?;
    let line_end = line_starts
        .get(position.line as usize + 1)
        .map(|next| next.saturating_sub(1))
        .unwrap_or(source.len());
    let line = &source[line_start..line_end];
    let mut utf16_units = 0_u32;
    for (byte_offset, character) in line.char_indices() {
        if utf16_units == position.character {
            return Ok(line_start + byte_offset);
        }
        utf16_units += character.len_utf16() as u32;
        if utf16_units > position.character {
            return Err(anyhow::anyhow!(
                "Character {} is inside a UTF-16 surrogate pair",
                position.character
            ));
        }
    }
    if utf16_units == position.character {
        Ok(line_end)
    } else {
        Err(anyhow::anyhow!(
            "Character {} is out of bounds",
            position.character
        ))
    }
}

fn byte_offset_to_lsp_position(
    source: &str,
    line_starts: &[usize],
    offset: usize,
) -> Result<Position> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        return Err(anyhow::anyhow!("Invalid byte offset {offset}"));
    }
    let line = match line_starts.binary_search(&offset) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    };
    let line_start = line_starts[line];
    let character = source[line_start..offset]
        .chars()
        .map(|character| character.len_utf16() as u32)
        .sum();
    Ok(Position {
        line: line as u32,
        character,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use t_linter_core::LintSeverity;
    use tempfile::TempDir;
    use tower_lsp::LspService;

    #[test]
    fn lsp_format_options_prefer_print_width_then_line_length() {
        let mut options = FormattingOptions {
            tab_size: 2,
            insert_spaces: true,
            ..Default::default()
        };
        options.properties = HashMap::from([
            ("printWidth".to_string(), FormattingProperty::Number(40)),
            ("lineLength".to_string(), FormattingProperty::Number(20)),
        ]);

        assert_eq!(extract_line_length_from_lsp_options(&options), Some(40));

        options.properties.remove("printWidth");
        assert_eq!(extract_line_length_from_lsp_options(&options), Some(20));
    }

    #[test]
    fn lsp_format_options_handle_string_bool_and_default_values() {
        let mut options = FormattingOptions {
            tab_size: 2,
            insert_spaces: true,
            ..Default::default()
        };
        options.properties = HashMap::from([
            (
                "lineLength".to_string(),
                FormattingProperty::String("120".to_string()),
            ),
            ("printWidth".to_string(), FormattingProperty::Bool(true)),
        ]);

        assert_eq!(extract_line_length_from_lsp_options(&options), Some(120));

        options.properties =
            HashMap::from([("lineLength".to_string(), FormattingProperty::Bool(true))]);
        assert_eq!(extract_line_length_from_lsp_options(&options), None);

        let uri = Url::parse("untitled:example.py").expect("uri");
        let resolved = resolve_lsp_format_options(&uri, None).expect("default options");
        assert_eq!(resolved.line_length, 80);
    }

    #[test]
    fn lsp_format_options_fall_back_to_pyproject() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("pyproject.toml"),
            "[tool.t-linter]\nline-length = 88\n",
        )
        .expect("write pyproject");
        let uri = Url::from_file_path(temp.path().join("example.py")).expect("file url");

        let options = resolve_lsp_format_options(&uri, None).expect("resolve options");
        assert_eq!(options.line_length, 88);
    }

    #[test]
    fn requested_code_action_kinds_match_specific_and_parent_kinds() {
        assert!(code_action_kind_matches(
            &CodeActionKind::SOURCE_FIX_ALL,
            &TLinterLanguageServer::source_fix_all_kind()
        ));
        assert!(code_action_kind_matches(
            &CodeActionKind::REFACTOR_REWRITE,
            &TLinterLanguageServer::refactor_rewrite_kind()
        ));
        assert!(code_action_kind_matches(
            &TLinterLanguageServer::source_fix_all_kind(),
            &TLinterLanguageServer::source_fix_all_kind()
        ));
        assert!(!code_action_kind_matches(
            &CodeActionKind::QUICKFIX,
            &TLinterLanguageServer::source_fix_all_kind()
        ));
    }

    #[test]
    fn locations_overlap_uses_half_open_ranges() {
        let template = t_linter_core::Location {
            start_line: 1,
            start_column: 5,
            end_line: 1,
            end_column: 10,
        };

        let inside_cursor = t_linter_core::Location {
            start_line: 1,
            start_column: 9,
            end_line: 1,
            end_column: 9,
        };
        assert!(locations_overlap(&template, &inside_cursor));

        let end_cursor = t_linter_core::Location {
            start_line: 1,
            start_column: 10,
            end_line: 1,
            end_column: 10,
        };
        assert!(!locations_overlap(&template, &end_cursor));

        let adjacent_range = t_linter_core::Location {
            start_line: 1,
            start_column: 10,
            end_line: 1,
            end_column: 12,
        };
        assert!(!locations_overlap(&template, &adjacent_range));
    }

    #[test]
    fn fallback_tokens_cover_single_and_multiline_templates() {
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();
        let source = r#"from typing import Annotated
from string.templatelib import Template

single_line: Annotated[Template, "unknown"] = t"<div>{value}</div>"
multiline: Annotated[Template, "unknown"] = t"""<div>
<span>{{ brace }}</span>
{value}
</div>"""
"#;
        let mut parser = TemplateStringParser::new().expect("parser");
        let templates = parser
            .find_template_strings(source)
            .expect("template discovery");
        let single_line = templates
            .iter()
            .find(|template| template.variable_name.as_deref() == Some("single_line"))
            .expect("single-line template");
        assert_eq!(
            server.generate_fallback_tokens(single_line, source),
            vec![(3, 46, 21, TOKEN_TYPE_MACRO, TOKEN_MODIFIER_NONE)]
        );

        let multiline = templates
            .iter()
            .find(|template| template.variable_name.as_deref() == Some("multiline"))
            .expect("multiline template");
        assert_eq!(
            server.generate_fallback_tokens(multiline, source),
            vec![
                (4, 44, 9, TOKEN_TYPE_MACRO, TOKEN_MODIFIER_NONE),
                (5, 0, 24, TOKEN_TYPE_MACRO, TOKEN_MODIFIER_NONE),
                (6, 0, 7, TOKEN_TYPE_MACRO, TOKEN_MODIFIER_NONE),
                (7, 0, 9, TOKEN_TYPE_MACRO, TOKEN_MODIFIER_NONE),
            ]
        );
    }

    #[test]
    fn conversion_helpers_round_trip_and_wrap_errors() {
        let source = "alpha\n  beta\nthird\n line four\nabcdefghi\n";
        let diagnostic = LintDiagnostic {
            file: PathBuf::from("example.py"),
            rule: "demo-rule".to_string(),
            severity: LintSeverity::Error,
            message: "boom".to_string(),
            language: Some("html".to_string()),
            start_line: 2,
            start_column: 3,
            end_line: 2,
            end_column: 6,
            expected_type: Some("str".to_string()),
            found_type: Some("int".to_string()),
            schema_pointer: None,
            source_of_truth: Some("demo schema".to_string()),
            suggested_edits: vec![DiagnosticEdit {
                range: t_linter_core::DiagnosticEditRange {
                    start_line: 2,
                    start_column: 3,
                    end_line: 2,
                    end_column: 6,
                },
                new_text: "cat".to_string(),
            }],
        };
        let lsp_diagnostic = lint_diagnostic_to_lsp(&diagnostic, source).expect("diagnostic");
        assert_eq!(lsp_diagnostic.range.start.line, 1);
        assert_eq!(lsp_diagnostic.range.start.character, 2);
        assert_eq!(
            lsp_diagnostic.code,
            Some(NumberOrString::String("demo-rule".to_string()))
        );
        assert_eq!(
            lsp_diagnostic
                .data
                .as_ref()
                .and_then(|data| data.get("expected_type")),
            Some(&serde_json::json!("str"))
        );

        let location = t_linter_core::Location {
            start_line: 4,
            start_column: 2,
            end_line: 5,
            end_column: 8,
        };
        let range = location_to_lsp_range(&location, source).expect("range");
        assert_eq!(
            lsp_range_to_location(&range, source).expect("location"),
            location
        );

        let edit = t_linter_core::TemplateEdit {
            location: location.clone(),
            replacement: "hello".to_string(),
        };
        let lsp_edits = template_edits_to_lsp(vec![edit], source).expect("lsp edits");
        assert_eq!(lsp_edits.len(), 1);
        let uri = Url::from_file_path(std::env::temp_dir().join("example.py")).expect("uri");
        let quickfixes =
            suggested_edit_code_actions(&uri, source, std::slice::from_ref(&lsp_diagnostic))
                .expect("quickfixes");
        assert_eq!(quickfixes.len(), 1);
        let CodeActionOrCommand::CodeAction(quickfix) = &quickfixes[0] else {
            panic!("expected code action");
        };
        let fixed = apply_workspace_edit_for_uri(
            &uri,
            source,
            quickfix.edit.as_ref().expect("quickfix edit"),
        )
        .expect("apply quickfix");
        assert_eq!(fixed, "alpha\n  cata\nthird\n line four\nabcdefghi\n");

        let workspace_edit = workspace_edit_for_uri(&uri, lsp_edits.clone());
        assert_eq!(
            workspace_edit
                .changes
                .as_ref()
                .and_then(|changes| changes.get(&uri))
                .cloned(),
            Some(lsp_edits)
        );

        assert!(uri_to_path(&uri).is_some());
        assert!(uri_to_path(&Url::parse("untitled:demo").expect("uri")).is_none());
        assert_eq!(
            internal_error(anyhow::anyhow!("oops")).code,
            tower_lsp::jsonrpc::ErrorCode::InternalError
        );
        assert_eq!(TLinterConfig::default().pyright_path, None);
        assert!(TLinterConfig::default().enable_type_checking);
        assert!(!TLinterConfig::default().type_checking.enabled);
    }

    #[test]
    fn interpolation_type_diagnostics_are_filtered_and_remapped() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

名前 = object()
age = 1
payload: Annotated[Template, "json"] = t'{{"name": {名前}, "age": {age}}}'
"#;
        let shadow = synthesize_for_type_check(PathBuf::from("example.py").as_path(), source)
            .expect("shadow synthesis")
            .expect("shadow document");
        let site = shadow
            .sites
            .iter()
            .find(|site| site.expression == "名前")
            .expect("name site");
        let shadow_line_starts = line_start_offsets(&shadow.text);
        let shadow_line_start = shadow_line_starts[site.shadow_line];
        let matching = Diagnostic {
            range: Range {
                start: Position {
                    line: site.shadow_line as u32,
                    character: (site.shadow_rhs_byte_range.start - shadow_line_start) as u32,
                },
                end: Position {
                    line: site.shadow_line as u32,
                    character: (site.shadow_rhs_byte_range.end - shadow_line_start) as u32,
                },
            },
            code: Some(NumberOrString::String(
                TY_INVALID_ASSIGNMENT_RULE.to_string(),
            )),
            message: "Type `User` is not assignable; found `User`".to_string(),
            ..Default::default()
        };
        let matching_range = matching.range;
        let ignored = Diagnostic {
            range: matching.range,
            code: Some(NumberOrString::String("unresolved-reference".to_string())),
            message: "unresolved".to_string(),
            ..Default::default()
        };
        let invalid_range = Diagnostic {
            range: Range {
                start: Position {
                    line: 999,
                    character: 0,
                },
                end: Position {
                    line: 999,
                    character: 1,
                },
            },
            code: Some(NumberOrString::String(
                TY_INVALID_ASSIGNMENT_RULE.to_string(),
            )),
            message: "bad range".to_string(),
            ..Default::default()
        };

        let remapped = remap_interpolation_type_diagnostics(
            &shadow,
            source,
            &[ignored, invalid_range, matching],
            NegotiatedEncoding::Utf8,
            TypeCheckerBackend::Ty,
        )
        .expect("remap");

        assert_eq!(remapped.len(), 1);
        assert_eq!(
            remapped[0].code,
            Some(NumberOrString::String(INTERPOLATION_TYPE_RULE.to_string()))
        );
        assert_eq!(remapped[0].source.as_deref(), Some("t-linter (ty)"));
        assert!(remapped[0].message.contains("`User`"));
        assert_eq!(
            remapped[0].range.start.line,
            site.original_location.start_line as u32 - 1
        );
        assert_eq!(
            remapped[0].range.end.character - remapped[0].range.start.character,
            2
        );
        assert_eq!(
            remapped[0]
                .data
                .as_ref()
                .and_then(|data| data.get("expected_type")),
            Some(&serde_json::json!(site.expected_type))
        );
        assert_eq!(
            remapped[0]
                .data
                .as_ref()
                .and_then(|data| data.get("found_type")),
            Some(&serde_json::json!("User"))
        );

        let pyright_matching = Diagnostic {
            range: matching_range,
            code: Some(NumberOrString::String(PYRIGHT_ASSIGNMENT_RULE.to_string())),
            message: "Type \"User\" is not assignable to declared type \"str\"".to_string(),
            ..Default::default()
        };
        let pyright = remap_interpolation_type_diagnostics(
            &shadow,
            source,
            &[pyright_matching],
            NegotiatedEncoding::Utf8,
            TypeCheckerBackend::Pyright,
        )
        .expect("pyright remap");
        assert_eq!(pyright[0].source.as_deref(), Some("t-linter (pyright)"));
        assert!(pyright[0].message.contains("`User`"));

        let pyrefly_matching = Diagnostic {
            range: matching_range,
            code: Some(NumberOrString::String(PYREFLY_ASSIGNMENT_RULE.to_string())),
            message: "`User` is not assignable to `str`".to_string(),
            ..Default::default()
        };
        let pyrefly = remap_interpolation_type_diagnostics(
            &shadow,
            source,
            &[pyrefly_matching],
            NegotiatedEncoding::Utf8,
            TypeCheckerBackend::Pyrefly,
        )
        .expect("pyrefly remap");
        assert_eq!(pyrefly[0].source.as_deref(), Some("t-linter (pyrefly)"));
        assert!(pyrefly[0].message.contains("`User`"));
    }

    #[test]
    fn initialization_config_accepts_ruff_pipeline_options() {
        let config = parse_initialization_config(
            Some(serde_json::json!({
                "enableTypeChecking": false,
                "highlightUntyped": false,
                "ruffPipeline": {
                    "enabled": true,
                    "command": "/tmp/ruff",
                    "args": ["server"],
                    "settings": {
                        "lineLength": 100,
                        "format": {"preview": true}
                    }
                },
                "typeChecking": {
                    "enabled": true,
                    "command": "/tmp/ty",
                    "args": ["server"],
                    "python": "/tmp/python"
                }
            })),
            TLinterConfig::default(),
        );

        assert!(!config.enable_type_checking);
        assert!(!config.highlight_untyped_templates);
        assert!(config.ruff_pipeline.enabled);
        assert_eq!(config.ruff_pipeline.command.as_deref(), Some("/tmp/ruff"));
        assert_eq!(config.ruff_pipeline.settings["lineLength"], 100);
        assert!(config.type_checking.enabled);
        assert_eq!(config.type_checking.command.as_deref(), Some("/tmp/ty"));
        assert_eq!(config.type_checking.python.as_deref(), Some("/tmp/python"));
    }

    #[test]
    fn initialization_config_preserves_startup_ruff_defaults_when_omitted() {
        let defaults = TLinterConfig {
            ruff_pipeline: RuffPipelineConfig {
                enabled: true,
                command: Some("/opt/ruff".to_string()),
                args: vec!["server".to_string()],
                settings: serde_json::json!({"lineLength": 120}),
            },
            ..Default::default()
        };

        let config = parse_initialization_config(
            Some(serde_json::json!({
                "enableTypeChecking": false
            })),
            defaults,
        );

        assert!(!config.enable_type_checking);
        assert!(config.ruff_pipeline.enabled);
        assert_eq!(config.ruff_pipeline.command.as_deref(), Some("/opt/ruff"));
        assert_eq!(config.ruff_pipeline.settings["lineLength"], 120);
    }

    #[test]
    fn lsp_text_edit_helpers_apply_utf16_and_compute_final_edit() {
        let source = "name = '世界'\ntext = t\"<div>{ name }</div>\"\n";
        let edits = vec![
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 10,
                    },
                    end: Position {
                        line: 0,
                        character: 10,
                    },
                },
                new_text: "!".to_string(),
            },
            TextEdit {
                range: Range {
                    start: Position {
                        line: 1,
                        character: 14,
                    },
                    end: Position {
                        line: 1,
                        character: 22,
                    },
                },
                new_text: "{name}".to_string(),
            },
        ];
        let applied = apply_lsp_text_edits(source, &edits).expect("apply edits");
        assert_eq!(applied, "name = '世界!'\ntext = t\"<div>{name}</div>\"\n");

        let final_edits = final_text_edit(source, &applied).expect("final edit");
        assert_eq!(
            apply_lsp_text_edits(source, &final_edits).expect("apply final edit"),
            applied
        );
    }

    #[test]
    fn lsp_text_edit_helpers_reject_overlap() {
        let source = "abcdef";
        let edits = vec![
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 1,
                    },
                    end: Position {
                        line: 0,
                        character: 4,
                    },
                },
                new_text: "x".to_string(),
            },
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 2,
                    },
                    end: Position {
                        line: 0,
                        character: 5,
                    },
                },
                new_text: "y".to_string(),
            },
        ];

        assert!(apply_lsp_text_edits(source, &edits).is_err());
    }

    #[test]
    fn full_lsp_range_ends_at_document_end() {
        assert_eq!(
            full_lsp_range("abc").end,
            Position {
                line: 0,
                character: 3
            }
        );
        assert_eq!(
            full_lsp_range("abc\n").end,
            Position {
                line: 1,
                character: 0
            }
        );
    }

    #[test]
    fn workspace_edit_helpers_apply_changes_and_document_changes() {
        let uri = Url::parse("file:///tmp/example.py").expect("uri");
        let changes_edit = WorkspaceEdit {
            changes: Some(HashMap::from([(
                uri.clone(),
                vec![TextEdit {
                    range: Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 1,
                        },
                    },
                    new_text: "b".to_string(),
                }],
            )])),
            document_changes: None,
            change_annotations: None,
        };
        assert_eq!(
            apply_workspace_edit_for_uri(&uri, "a\n", &changes_edit).expect("changes edit"),
            "b\n"
        );

        let document_changes_edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: None,
                },
                edits: vec![OneOf::Left(TextEdit {
                    range: Range {
                        start: Position {
                            line: 0,
                            character: 1,
                        },
                        end: Position {
                            line: 0,
                            character: 1,
                        },
                    },
                    new_text: "y".to_string(),
                })],
            }])),
            change_annotations: None,
        };
        assert_eq!(
            apply_workspace_edit_for_uri(&uri, "x\n", &document_changes_edit)
                .expect("documentChanges edit"),
            "xy\n"
        );
    }

    #[test]
    fn workspace_edit_helpers_reject_other_uri_and_resource_operations() {
        let uri = Url::parse("file:///tmp/example.py").expect("uri");
        let other_uri = Url::parse("file:///tmp/other.py").expect("other uri");
        let other_uri_edit = WorkspaceEdit {
            changes: Some(HashMap::from([(
                other_uri.clone(),
                vec![TextEdit {
                    range: Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 0,
                        },
                    },
                    new_text: "x".to_string(),
                }],
            )])),
            document_changes: None,
            change_annotations: None,
        };
        assert!(apply_workspace_edit_for_uri(&uri, "", &other_uri_edit).is_err());

        let resource_operation_edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: other_uri,
                    options: None,
                    annotation_id: None,
                })),
            ])),
            change_annotations: None,
        };
        assert!(apply_workspace_edit_for_uri(&uri, "", &resource_operation_edit).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initialize_advertises_code_actions_and_formatting_capabilities() {
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });

        let result = service
            .inner()
            .initialize(InitializeParams::default())
            .await
            .expect("initialize");

        assert_eq!(
            result.capabilities.document_formatting_provider,
            Some(OneOf::Left(true))
        );
        assert_eq!(
            result.capabilities.document_range_formatting_provider,
            Some(OneOf::Left(true))
        );

        let code_action_provider = result
            .capabilities
            .code_action_provider
            .expect("code action provider");
        let CodeActionProviderCapability::Options(options) = code_action_provider else {
            panic!("expected code action options");
        };
        assert_eq!(
            options.code_action_kinds,
            Some(vec![
                TLinterLanguageServer::source_fix_all_kind(),
                TLinterLanguageServer::refactor_rewrite_kind()
            ])
        );
        assert_eq!(options.resolve_provider, Some(true));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lifecycle_methods_update_cache_and_handle_missing_documents() {
        let temp = TempDir::new().expect("tempdir");
        let source = sample_python_document();
        let uri = write_source_file(temp.path(), "example.py", source);
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();

        server.initialized(InitializedParams {}).await;
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "python".to_string(),
                    version: 1,
                    text: source.to_string(),
                },
            })
            .await;
        assert_eq!(
            server
                .document_cache
                .get(&uri)
                .expect("cached after open")
                .value()
                .text,
            source
        );
        assert!(server.diagnostic_tasks.contains_key(&uri));

        server
            .did_change(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: source.replace("12345", "x"),
                }],
            })
            .await;
        assert!(
            server
                .document_cache
                .get(&uri)
                .expect("cached after change")
                .text
                .contains("data-a=\"x\"")
        );

        let changed_once = source.replace("12345", "x");
        let line_starts = line_start_offsets(&changed_once);
        let first_start = changed_once.find("x\"").expect("first marker");
        let second_start = changed_once.find("67890").expect("second marker");
        server
            .did_change(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: 3,
                },
                content_changes: vec![
                    TextDocumentContentChangeEvent {
                        range: Some(Range {
                            start: byte_offset_to_lsp_position(
                                &changed_once,
                                &line_starts,
                                first_start,
                            )
                            .expect("first start"),
                            end: byte_offset_to_lsp_position(
                                &changed_once,
                                &line_starts,
                                first_start + 1,
                            )
                            .expect("first end"),
                        }),
                        range_length: None,
                        text: "y".to_string(),
                    },
                    TextDocumentContentChangeEvent {
                        range: Some(Range {
                            start: byte_offset_to_lsp_position(
                                &changed_once,
                                &line_starts,
                                second_start,
                            )
                            .expect("second start"),
                            end: byte_offset_to_lsp_position(
                                &changed_once,
                                &line_starts,
                                second_start + 5,
                            )
                            .expect("second end"),
                        }),
                        range_length: None,
                        text: "42".to_string(),
                    },
                ],
            })
            .await;
        {
            let changed_twice = server.document_cache.get(&uri).expect("cached twice");
            assert!(changed_twice.text.contains("data-a=\"y\""));
            assert!(changed_twice.text.contains("data-b=\"42\""));
        }

        server
            .did_change_configuration(DidChangeConfigurationParams {
                settings: serde_json::json!({
                    "t-linter": {
                        "highlightUntyped": false
                    }
                }),
            })
            .await;
        let config = server.config.read().await.clone();
        assert!(!config.highlight_untyped_templates);

        let missing = Url::parse("file:///tmp/missing.py").expect("uri");
        let err = server
            .collect_document_format_edits(&missing, None)
            .await
            .expect_err("missing doc should error");
        assert_eq!(err.code, tower_lsp::jsonrpc::ErrorCode::InvalidParams);

        let no_tokens = server
            .semantic_tokens_full(SemanticTokensParams {
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
                text_document: TextDocumentIdentifier { uri: missing },
            })
            .await
            .expect("semantic tokens response");
        assert!(no_tokens.is_none());

        server
            .did_close(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            })
            .await;
        assert!(!server.document_cache.contains_key(&uri));
        assert!(!server.ruff_document_locks.contains_key(&uri));
        assert!(!server.diagnostic_tasks.contains_key(&uri));

        server.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn schedule_diagnostics_covers_file_untitled_and_cancelled_tasks() {
        let temp = TempDir::new().expect("tempdir");
        let source = r#"from typing import Annotated
from string.templatelib import Template

page: Annotated[Template, "html"] = t"<div>{value}</div>"
"#;
        let file_uri = write_source_file(temp.path(), "diag.py", source);
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();

        open_cached_document(server, &file_uri, source).await;
        assert!(server.diagnostic_tasks.contains_key(&file_uri));
        await_scheduled_diagnostics(server, &file_uri).await;
        assert!(server.diagnostic_tasks.is_empty());

        let untitled_uri = Url::parse("untitled:diag.py").expect("untitled uri");
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: untitled_uri.clone(),
                    language_id: "python".to_string(),
                    version: 1,
                    text: source.to_string(),
                },
            })
            .await;
        assert!(server.diagnostic_tasks.contains_key(&untitled_uri));
        await_scheduled_diagnostics(server, &untitled_uri).await;
        assert!(server.diagnostic_tasks.is_empty());

        let cancelled_uri = write_source_file(temp.path(), "cancelled.py", source);
        open_cached_document(server, &cancelled_uri, source).await;
        server
            .did_close(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier {
                    uri: cancelled_uri.clone(),
                },
            })
            .await;
        assert!(server.document_cache.get(&cancelled_uri).is_none());
        assert!(server.diagnostic_tasks.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn formatting_helpers_cover_range_errors_and_fallback_semantic_tokens() {
        let temp = tempdir_with_pyproject(20);
        let source = sample_python_document();
        let uri = write_source_file(temp.path(), "example.py", source);
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();
        open_cached_document(server, &uri, source).await;

        let no_template = server
            .format_uri(
                &uri,
                Some(Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 2,
                    },
                }),
                None,
            )
            .await
            .expect("no template response");
        assert!(no_template.is_empty());

        let multi_err = server
            .format_uri(
                &uri,
                Some(Range {
                    start: Position {
                        line: 3,
                        character: 0,
                    },
                    end: Position {
                        line: 5,
                        character: 60,
                    },
                }),
                None,
            )
            .await
            .expect_err("multi template should error");
        assert_eq!(multi_err.code, tower_lsp::jsonrpc::ErrorCode::InternalError);

        let unsupported = r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "unsupported"] = t"<body>demo</body>"
"#;
        let unsupported_uri = write_source_file(temp.path(), "unsupported.py", unsupported);
        open_cached_document(server, &unsupported_uri, unsupported).await;
        let tokens = server
            .generate_semantic_tokens(&unsupported_uri)
            .await
            .expect("fallback tokens");
        assert!(!tokens.data.is_empty());

        let changed = server
            .range_formatting(DocumentRangeFormattingParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range: Range {
                    start: Position {
                        line: 3,
                        character: 25,
                    },
                    end: Position {
                        line: 3,
                        character: 55,
                    },
                },
                options: FormattingOptions {
                    tab_size: 4,
                    insert_spaces: true,
                    ..Default::default()
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            })
            .await
            .expect("range formatting response")
            .expect("range formatting should produce edits for this selection");
        assert!(!changed.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn semantic_tokens_cover_supported_unsupported_and_untyped_templates() {
        let temp = TempDir::new().expect("tempdir");
        let source = r#"from typing import Annotated
from string.templatelib import Template

typed_html: Annotated[Template, "html"] = t"<div class='card'>{value}</div>"
unsupported: Annotated[Template, "unknown"] = t"""<odd>{{ brace }}</odd>
<span>{value}</span>"""
plain = "世界🙂"; plain_tpl = t"こんにちは {name}"
"#;
        let uri = write_source_file(temp.path(), "tokens.py", source);
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();
        open_cached_document(server, &uri, source).await;

        let response = server
            .semantic_tokens_full(SemanticTokensParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await
            .expect("semantic tokens response");
        let Some(SemanticTokensResult::Tokens(tokens)) = response else {
            panic!("expected semantic tokens");
        };

        assert!(!tokens.data.is_empty());

        let absolute_tokens = semantic_token_positions(&tokens.data);
        let typed_html_line = source
            .lines()
            .position(|line| line.starts_with("typed_html:"))
            .expect("typed_html line") as u32;
        let unsupported_line = source
            .lines()
            .position(|line| line.starts_with("unsupported:"))
            .expect("unsupported line") as u32;
        let plain_line = source
            .lines()
            .position(|line| line.starts_with("plain ="))
            .expect("plain line") as u32;

        assert!(absolute_tokens.iter().any(|(line, _, _, token_type)| {
            *line == typed_html_line && *token_type != TOKEN_TYPE_MACRO
        }));
        assert!(absolute_tokens.iter().any(|(line, _, _, token_type)| {
            *line == unsupported_line && *token_type == TOKEN_TYPE_MACRO
        }));
        let line_starts = line_start_offsets(source);
        let plain_line_start = line_starts[plain_line as usize];
        let plain_text = source
            .lines()
            .nth(plain_line as usize)
            .expect("plain line text");
        let token_start = plain_line_start + plain_text.find("t\"").expect("template start");
        let token_end = plain_line_start + plain_text.rfind('"').expect("template end") + 1;
        let expected_start = byte_offset_to_lsp_position(source, &line_starts, token_start)
            .expect("template start position");
        let expected_end = byte_offset_to_lsp_position(source, &line_starts, token_end)
            .expect("template end position");
        assert!(
            absolute_tokens
                .iter()
                .any(|(line, start, length, token_type)| {
                    *line == plain_line
                        && *start == expected_start.character
                        && *length
                            == expected_end
                                .character
                                .saturating_sub(expected_start.character)
                        && *token_type == TOKEN_TYPE_MACRO
                })
        );

        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::with_config(
                client,
                TLinterConfig {
                    highlight_untyped_templates: false,
                    ..TLinterConfig::default()
                },
            )
            .expect("create language server")
        });
        let server = service.inner();
        open_cached_document(server, &uri, source).await;

        let response = server
            .semantic_tokens_full(SemanticTokensParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await
            .expect("semantic tokens response");
        let Some(SemanticTokensResult::Tokens(tokens)) = response else {
            panic!("expected semantic tokens");
        };
        let absolute_tokens = semantic_token_positions(&tokens.data);
        assert!(
            !absolute_tokens
                .iter()
                .any(|(line, start, length, token_type)| {
                    *line == plain_line
                        && *start == expected_start.character
                        && *length
                            == expected_end
                                .character
                                .saturating_sub(expected_start.character)
                        && *token_type == TOKEN_TYPE_MACRO
                })
        );
    }

    #[test]
    fn semantic_token_conversion_and_requested_kinds_helpers_cover_remaining_paths() {
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();
        let converted = server.convert_to_semantic_tokens(vec![
            (3, 4, 2, 1, 0),
            (3, 9, 1, 2, 0),
            (5, 2, 3, 3, 1),
        ]);

        assert_eq!(converted[0].delta_line, 3);
        assert_eq!(converted[0].delta_start, 4);
        assert_eq!(converted[1].delta_line, 0);
        assert_eq!(converted[1].delta_start, 5);
        assert_eq!(converted[2].delta_line, 2);
        assert_eq!(converted[2].delta_start, 2);

        let source = "名前 = t\"界\"\n";
        let line_starts = line_start_offsets(source);
        let start = source.find("t\"").expect("token start");
        let end = source.rfind('"').expect("token end") + 1;
        let utf16_start = byte_offset_to_lsp_position(source, &line_starts, start)
            .expect("utf16 start")
            .character;
        let utf16_end = byte_offset_to_lsp_position(source, &line_starts, end)
            .expect("utf16 end")
            .character;
        let lsp_tokens = byte_tokens_to_lsp_tokens(
            source,
            vec![(0, start as u32, (end - start) as u32, TOKEN_TYPE_MACRO, 0)],
        )
        .expect("byte token conversion");
        assert_eq!(
            lsp_tokens,
            vec![(
                0,
                utf16_start,
                utf16_end.saturating_sub(utf16_start),
                TOKEN_TYPE_MACRO,
                0
            )]
        );

        assert!(requested_code_action_kinds_include(
            None,
            &TLinterLanguageServer::source_fix_all_kind()
        ));
        assert!(!requested_code_action_kinds_include(
            Some(&[CodeActionKind::QUICKFIX]),
            &TLinterLanguageServer::source_fix_all_kind()
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn source_fix_all_code_action_matches_formatting_endpoint_edits() {
        let temp = tempdir_with_pyproject(20);
        let source = sample_python_document();
        let uri = write_source_file(temp.path(), "example.py", source);
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();
        open_cached_document(server, &uri, source).await;

        let formatting_edits = server
            .formatting(DocumentFormattingParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                options: FormattingOptions {
                    tab_size: 4,
                    insert_spaces: true,
                    ..Default::default()
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            })
            .await
            .expect("formatting response")
            .expect("formatting edits");

        let actions = server
            .code_action(code_action_params(
                uri.clone(),
                full_document_range(),
                Some(vec![TLinterLanguageServer::source_fix_all_kind()]),
            ))
            .await
            .expect("code action response")
            .expect("code action");

        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected code action");
        };
        assert_eq!(
            action.kind,
            Some(TLinterLanguageServer::source_fix_all_kind())
        );
        assert_eq!(action.title, source_fix_all_title(false));
        let edit = action.edit.as_ref().expect("workspace edit");
        assert_eq!(
            edit.changes
                .as_ref()
                .and_then(|changes| changes.get(&uri))
                .cloned()
                .expect("edits for uri"),
            formatting_edits
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn source_fix_all_code_action_resolves_deferred_edit() {
        let temp = tempdir_with_pyproject(20);
        let source = sample_python_document();
        let uri = write_source_file(temp.path(), "example.py", source);
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();
        *server.client_support.write().await = ClientSupport {
            resolve_code_action_edits: true,
            versioned_workspace_edits: true,
        };
        open_cached_document(server, &uri, source).await;

        let formatting_edits = server
            .formatting(DocumentFormattingParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                options: FormattingOptions {
                    tab_size: 4,
                    insert_spaces: true,
                    ..Default::default()
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
            })
            .await
            .expect("formatting response")
            .expect("formatting edits");

        let actions = server
            .code_action(code_action_params(
                uri.clone(),
                full_document_range(),
                Some(vec![TLinterLanguageServer::source_fix_all_kind()]),
            ))
            .await
            .expect("code action response")
            .expect("code action");
        let CodeActionOrCommand::CodeAction(action) = actions.into_iter().next().unwrap() else {
            panic!("expected code action");
        };
        assert!(action.edit.is_none());
        assert!(action.data.is_some());

        let resolved = server
            .code_action_resolve(action)
            .await
            .expect("resolve code action");
        let edit = resolved.edit.as_ref().expect("resolved workspace edit");
        let expected = apply_lsp_text_edits(source, &formatting_edits).expect("formatting edits");
        let actual = apply_workspace_edit_for_uri(&uri, source, edit).expect("resolved edits");
        assert_eq!(actual, expected);
        assert!(matches!(
            edit.document_changes,
            Some(DocumentChanges::Edits(_))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deferred_code_action_rejects_stale_document_version() {
        let temp = tempdir_with_pyproject(20);
        let source = sample_python_document();
        let uri = write_source_file(temp.path(), "example.py", source);
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();
        *server.client_support.write().await = ClientSupport {
            resolve_code_action_edits: true,
            versioned_workspace_edits: true,
        };
        open_cached_document(server, &uri, source).await;

        let actions = server
            .code_action(code_action_params(
                uri.clone(),
                full_document_range(),
                Some(vec![TLinterLanguageServer::source_fix_all_kind()]),
            ))
            .await
            .expect("code action response")
            .expect("code action");
        let CodeActionOrCommand::CodeAction(action) = actions.into_iter().next().unwrap() else {
            panic!("expected code action");
        };

        server
            .did_change(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: 2,
                },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: source.replace("12345", "changed"),
                }],
            })
            .await;

        let err = server
            .code_action_resolve(action)
            .await
            .expect_err("stale code action should fail");
        assert_eq!(err.code, tower_lsp::jsonrpc::ErrorCode::InvalidParams);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rewrite_code_action_resolves_deferred_edit() {
        let temp = tempdir_with_pyproject(20);
        let source = sample_python_document();
        let uri = write_source_file(temp.path(), "example.py", source);
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();
        *server.client_support.write().await = ClientSupport {
            resolve_code_action_edits: true,
            versioned_workspace_edits: false,
        };
        open_cached_document(server, &uri, source).await;
        let range = Range {
            start: Position {
                line: 3,
                character: 50,
            },
            end: Position {
                line: 3,
                character: 55,
            },
        };

        let expected_edits = server
            .format_uri(&uri, Some(range), None)
            .await
            .expect("range formatting edits");
        let actions = server
            .code_action(code_action_params(
                uri.clone(),
                range,
                Some(vec![CodeActionKind::REFACTOR_REWRITE]),
            ))
            .await
            .expect("code action response")
            .expect("rewrite action");
        let CodeActionOrCommand::CodeAction(action) = actions.into_iter().next().unwrap() else {
            panic!("expected code action");
        };
        assert!(action.edit.is_none());

        let resolved = server
            .code_action_resolve(action)
            .await
            .expect("resolve rewrite action");
        let edit = resolved.edit.as_ref().expect("resolved workspace edit");
        let expected = apply_lsp_text_edits(source, &expected_edits).expect("range edits");
        let actual = apply_workspace_edit_for_uri(&uri, source, edit).expect("resolved edits");
        assert_eq!(actual, expected);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rewrite_code_action_requires_single_template_selection() {
        let temp = tempdir_with_pyproject(20);
        let source = sample_python_document();
        let uri = write_source_file(temp.path(), "example.py", source);
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();
        open_cached_document(server, &uri, source).await;

        let actions = server
            .code_action(code_action_params(
                uri.clone(),
                Range {
                    start: Position {
                        line: 3,
                        character: 50,
                    },
                    end: Position {
                        line: 3,
                        character: 55,
                    },
                },
                Some(vec![CodeActionKind::REFACTOR_REWRITE]),
            ))
            .await
            .expect("code action response")
            .expect("rewrite action");
        assert_eq!(actions.len(), 1);

        let no_template = server
            .code_action(code_action_params(
                uri.clone(),
                Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 4,
                    },
                },
                Some(vec![TLinterLanguageServer::refactor_rewrite_kind()]),
            ))
            .await
            .expect("code action response");
        assert!(no_template.is_none());

        let multiple_templates = server
            .code_action(code_action_params(
                uri,
                Range {
                    start: Position {
                        line: 3,
                        character: 0,
                    },
                    end: Position {
                        line: 5,
                        character: 60,
                    },
                },
                Some(vec![TLinterLanguageServer::refactor_rewrite_kind()]),
            ))
            .await
            .expect("code action response");
        assert!(multiple_templates.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn code_action_kind_filtering_and_noop_behavior_work() {
        let temp = TempDir::new().expect("tempdir");
        let source = r#"from typing import Annotated
from string.templatelib import Template

query: Annotated[Template, "sql"] = t"SELECT * FROM users WHERE id = {user_id}"
"#;
        let uri = write_source_file(temp.path(), "query.py", source);
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();
        open_cached_document(server, &uri, source).await;

        let no_fix_all = server
            .code_action(code_action_params(
                uri.clone(),
                full_document_range(),
                Some(vec![CodeActionKind::SOURCE_FIX_ALL]),
            ))
            .await
            .expect("code action response");
        assert!(no_fix_all.is_none());

        let no_rewrite_from_fix_all_filter = server
            .code_action(code_action_params(
                uri,
                Range {
                    start: Position {
                        line: 3,
                        character: 40,
                    },
                    end: Position {
                        line: 3,
                        character: 50,
                    },
                },
                Some(vec![TLinterLanguageServer::source_fix_all_kind()]),
            ))
            .await
            .expect("code action response");
        assert!(no_rewrite_from_fix_all_filter.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn code_action_ignores_transient_template_format_errors() {
        let temp = TempDir::new().expect("tempdir");
        let source = malformed_template_document();
        let uri = write_source_file(temp.path(), "example.py", source);
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();
        open_cached_document(server, &uri, source).await;

        let no_fix_all = server
            .code_action(code_action_params(
                uri.clone(),
                full_document_range(),
                Some(vec![TLinterLanguageServer::source_fix_all_kind()]),
            ))
            .await
            .expect("code action should not fail on malformed template content");
        assert!(no_fix_all.is_none());

        let no_rewrite = server
            .code_action(code_action_params(
                uri,
                Range {
                    start: Position {
                        line: 5,
                        character: 20,
                    },
                    end: Position {
                        line: 5,
                        character: 20,
                    },
                },
                Some(vec![TLinterLanguageServer::refactor_rewrite_kind()]),
            ))
            .await
            .expect("rewrite code action should not fail on malformed template content");
        assert!(no_rewrite.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn code_action_resolve_ignores_transient_template_format_errors() {
        let temp = TempDir::new().expect("tempdir");
        let source = malformed_template_document();
        let uri = write_source_file(temp.path(), "example.py", source);
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
        });
        let server = service.inner();
        *server.client_support.write().await = ClientSupport {
            resolve_code_action_edits: true,
            versioned_workspace_edits: true,
        };
        open_cached_document(server, &uri, source).await;

        let fix_all = server
            .code_action(code_action_params(
                uri.clone(),
                full_document_range(),
                Some(vec![TLinterLanguageServer::source_fix_all_kind()]),
            ))
            .await
            .expect("deferred fixAll code action should not fail")
            .expect("deferred fixAll action");
        let CodeActionOrCommand::CodeAction(action) = fix_all.into_iter().next().unwrap() else {
            panic!("expected code action");
        };
        assert!(action.edit.is_none());
        let resolved = server
            .code_action_resolve(action)
            .await
            .expect("resolve should ignore malformed template content");
        assert!(resolved.edit.is_none());

        let rewrite = server
            .code_action(code_action_params(
                uri,
                Range {
                    start: Position {
                        line: 5,
                        character: 20,
                    },
                    end: Position {
                        line: 5,
                        character: 20,
                    },
                },
                Some(vec![TLinterLanguageServer::refactor_rewrite_kind()]),
            ))
            .await
            .expect("deferred rewrite code action should not fail")
            .expect("deferred rewrite action");
        let CodeActionOrCommand::CodeAction(action) = rewrite.into_iter().next().unwrap() else {
            panic!("expected code action");
        };
        assert!(action.edit.is_none());
        let resolved = server
            .code_action_resolve(action)
            .await
            .expect("resolve should ignore malformed rewrite content");
        assert!(resolved.edit.is_none());
    }

    #[test]
    fn source_fix_all_title_mentions_ruff_when_composed_formatting_is_enabled() {
        assert_eq!(
            source_fix_all_title(false),
            "Format template strings with t-linter"
        );
        assert_eq!(
            source_fix_all_title(true),
            "Apply Ruff pipeline and format template strings with t-linter"
        );
    }

    fn tempdir_with_pyproject(line_length: usize) -> TempDir {
        let temp = TempDir::new().expect("tempdir");
        std::fs::write(
            temp.path().join("pyproject.toml"),
            format!("[tool.t-linter]\nline-length = {line_length}\n"),
        )
        .expect("write pyproject");
        temp
    }

    fn write_source_file(dir: &std::path::Path, file_name: &str, source: &str) -> Url {
        let file_path = dir.join(file_name);
        std::fs::write(&file_path, source).expect("write source");
        Url::from_file_path(file_path).expect("file url")
    }

    async fn open_cached_document(server: &TLinterLanguageServer, uri: &Url, source: &str) {
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "python".to_string(),
                    version: 1,
                    text: source.to_string(),
                },
            })
            .await;
    }

    async fn await_scheduled_diagnostics(server: &TLinterLanguageServer, uri: &Url) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !server.diagnostic_tasks.contains_key(uri) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("diagnostic task timed out");
    }

    fn semantic_token_positions(tokens: &[SemanticToken]) -> Vec<(u32, u32, u32, u32)> {
        let mut line = 0;
        let mut start = 0;
        let mut absolute = Vec::with_capacity(tokens.len());

        for token in tokens {
            line += token.delta_line;
            if token.delta_line == 0 {
                start += token.delta_start;
            } else {
                start = token.delta_start;
            }
            absolute.push((line, start, token.length, token.token_type));
        }

        absolute
    }

    fn code_action_params(
        uri: Url,
        range: Range,
        only: Option<Vec<CodeActionKind>>,
    ) -> CodeActionParams {
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri },
            range,
            context: CodeActionContext {
                diagnostics: Vec::new(),
                only,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }
    }

    fn full_document_range() -> Range {
        Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 10,
                character: 0,
            },
        }
    }

    fn sample_python_document() -> &'static str {
        r#"from typing import Annotated
from string.templatelib import Template

html_template: Annotated[Template, "html"] = t'<div data-a="12345" data-b="67890"></div>'
query: Annotated[Template, "sql"] = t"SELECT * FROM users WHERE id = {user_id}"
payload: Annotated[Template, "json"] = t'{"b": 2, "a": 1}'
"#
    }

    fn malformed_template_document() -> &'static str {
        r#"from typing import Annotated
from string.templatelib import Template

title = "demo"
payload: Annotated[Template, "html"] = t"""
<html><div>{title}< /div></html>

"""
"#
    }
}
