use anyhow::Result;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use t_linter_core::{
    FormatOptions as CoreFormatOptions, LintDiagnostic, TemplateHighlighter, TemplateStringInfo,
    TemplateStringParser, format_document_range_with_options, format_document_with_options,
    lint_source, load_project_config_for_path,
};
use tower_lsp::jsonrpc::Result as JsonRpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tracing::{debug, info};

const TOKEN_TYPE_MACRO: u32 = 14;
const TOKEN_MODIFIER_NONE: u32 = 0;
const DIAGNOSTIC_DEBOUNCE_MS: u64 = 250;
const SOURCE_FIX_ALL_T_LINTER: &str = "source.fixAll.t-linter";
const REFACTOR_REWRITE_T_LINTER: &str = "refactor.rewrite.t-linter";

pub struct TLinterLanguageServer {
    client: Client,
    document_cache: Arc<DashMap<Url, String>>,
    diagnostic_tasks: Arc<DashMap<Url, tokio::task::JoinHandle<()>>>,
    parser: Arc<tokio::sync::Mutex<TemplateStringParser>>,
    highlighter: Arc<tokio::sync::Mutex<TemplateHighlighter>>,
}

impl TLinterLanguageServer {
    pub fn new(client: Client) -> Result<Self> {
        Ok(Self {
            client,
            document_cache: Arc::new(DashMap::new()),
            diagnostic_tasks: Arc::new(DashMap::new()),
            parser: Arc::new(tokio::sync::Mutex::new(TemplateStringParser::new()?)),
            highlighter: Arc::new(tokio::sync::Mutex::new(TemplateHighlighter::new()?)),
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
            let length = end_col - start_col;
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
                let first_line_len = first_line.len() as u32 - start_col;
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
    async fn initialize(&self, _: InitializeParams) -> JsonRpcResult<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
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
                        resolve_provider: Some(false),
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
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;

        self.document_cache.insert(uri.clone(), text);
        self.schedule_diagnostics(uri);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;

        if let Some(change) = params.content_changes.into_iter().next() {
            self.document_cache.insert(uri.clone(), change.text);
            self.schedule_diagnostics(uri);
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        debug!("Configuration changed: {:?}", params);
    }
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.document_cache.remove(&params.text_document.uri);
        if let Some((_, handle)) = self.diagnostic_tasks.remove(&params.text_document.uri) {
            handle.abort();
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

        if requested_code_action_kinds_include(requested_kinds, &source_fix_all_kind) {
            let edits = self
                .collect_document_format_edits(&params.text_document.uri, None)
                .await?;

            if !edits.is_empty() {
                actions.push(
                    CodeAction {
                        title: "Format template strings with t-linter".to_string(),
                        kind: Some(source_fix_all_kind),
                        edit: Some(workspace_edit_for_uri(&params.text_document.uri, edits)),
                        is_preferred: Some(true),
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }

        if requested_code_action_kinds_include(requested_kinds, &rewrite_kind) {
            match self
                .collect_single_template_selection_format_edits(
                    &params.text_document.uri,
                    &params.range,
                    None,
                )
                .await?
            {
                SelectionFormatEdits::Edits(edits) if !edits.is_empty() => {
                    actions.push(
                        CodeAction {
                            title: "Rewrite template string with t-linter".to_string(),
                            kind: Some(rewrite_kind),
                            edit: Some(workspace_edit_for_uri(&params.text_document.uri, edits)),
                            ..Default::default()
                        }
                        .into(),
                    );
                }
                SelectionFormatEdits::NoTemplate
                | SelectionFormatEdits::MultipleTemplates
                | SelectionFormatEdits::Edits(_) => {}
            }
        }

        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(actions))
        }
    }
}

enum SelectionFormatEdits {
    NoTemplate,
    MultipleTemplates,
    Edits(Vec<TextEdit>),
}

impl TLinterLanguageServer {
    fn schedule_diagnostics(&self, uri: Url) {
        if let Some((_, handle)) = self.diagnostic_tasks.remove(&uri) {
            handle.abort();
        }

        let client = self.client.clone();
        let document_cache = Arc::clone(&self.document_cache);
        let diagnostic_tasks = Arc::clone(&self.diagnostic_tasks);
        let task_uri = uri.clone();

        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(DIAGNOSTIC_DEBOUNCE_MS)).await;

            let Some(text) = document_cache.get(&task_uri).map(|entry| entry.clone()) else {
                diagnostic_tasks.remove(&task_uri);
                return;
            };

            let Some(path) = uri_to_path(&task_uri) else {
                client
                    .log_message(
                        MessageType::ERROR,
                        format!("Unable to resolve filesystem path for {}", task_uri),
                    )
                    .await;
                diagnostic_tasks.remove(&task_uri);
                return;
            };

            let diagnostics = match lint_source(&path, &text) {
                Ok(result) => result
                    .diagnostics
                    .iter()
                    .map(lint_diagnostic_to_lsp)
                    .collect::<Vec<_>>(),
                Err(err) => {
                    client
                        .log_message(
                            MessageType::ERROR,
                            format!("Diagnostic analysis failed for {}: {}", task_uri, err),
                        )
                        .await;
                    diagnostic_tasks.remove(&task_uri);
                    return;
                }
            };

            client
                .publish_diagnostics(task_uri.clone(), diagnostics, None)
                .await;
            diagnostic_tasks.remove(&task_uri);
        });

        self.diagnostic_tasks.insert(uri, handle);
    }

    async fn collect_document_format_edits(
        &self,
        uri: &Url,
        formatting_options: Option<&FormattingOptions>,
    ) -> JsonRpcResult<Vec<TextEdit>> {
        let text = self
            .document_cache
            .get(uri)
            .ok_or_else(|| tower_lsp::jsonrpc::Error::invalid_params("Document not found"))?
            .clone();
        let options =
            resolve_lsp_format_options(uri, formatting_options).map_err(internal_error)?;
        let edits = format_document_with_options(&text, &options).map_err(internal_error)?;

        Ok(template_edits_to_lsp(edits))
    }

    async fn collect_single_template_selection_format_edits(
        &self,
        uri: &Url,
        range: &Range,
        formatting_options: Option<&FormattingOptions>,
    ) -> JsonRpcResult<SelectionFormatEdits> {
        let text = self
            .document_cache
            .get(uri)
            .ok_or_else(|| tower_lsp::jsonrpc::Error::invalid_params("Document not found"))?
            .clone();
        let options =
            resolve_lsp_format_options(uri, formatting_options).map_err(internal_error)?;
        let location = lsp_range_to_location(range);
        let mut parser = self.parser.lock().await;
        let templates = parser
            .find_template_strings(&text)
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

        let edits = format_document_range_with_options(&text, &location, &options)
            .map_err(internal_error)?;
        Ok(SelectionFormatEdits::Edits(template_edits_to_lsp(edits)))
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

    async fn generate_semantic_tokens(&self, uri: &Url) -> Result<SemanticTokens> {
        let text = self
            .document_cache
            .get(uri)
            .ok_or_else(|| anyhow::anyhow!("Document not found in cache"))?
            .clone();

        debug!("Generating semantic tokens for: {}", uri);

        let mut parser = self.parser.lock().await;
        let templates = parser.find_template_strings(&text)?;

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

        let data = self.convert_to_semantic_tokens(all_tokens);

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
            let delta_line = line - prev_line;
            let delta_start = if delta_line == 0 {
                start - prev_start
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

fn lint_diagnostic_to_lsp(diagnostic: &LintDiagnostic) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: diagnostic.start_line.saturating_sub(1) as u32,
                character: diagnostic.start_column.saturating_sub(1) as u32,
            },
            end: Position {
                line: diagnostic.end_line.saturating_sub(1) as u32,
                character: diagnostic.end_column.saturating_sub(1) as u32,
            },
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String(diagnostic.rule.clone())),
        source: Some("t-linter".to_string()),
        message: diagnostic.message.clone(),
        ..Default::default()
    }
}

fn location_to_lsp_range(location: &t_linter_core::Location) -> Range {
    Range {
        start: Position {
            line: location.start_line.saturating_sub(1) as u32,
            character: location.start_column.saturating_sub(1) as u32,
        },
        end: Position {
            line: location.end_line.saturating_sub(1) as u32,
            character: location.end_column.saturating_sub(1) as u32,
        },
    }
}

fn lsp_range_to_location(range: &Range) -> t_linter_core::Location {
    t_linter_core::Location {
        start_line: range.start.line as usize + 1,
        start_column: range.start.character as usize + 1,
        end_line: range.end.line as usize + 1,
        end_column: range.end.character as usize + 1,
    }
}

fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()
}

fn template_edits_to_lsp(edits: Vec<t_linter_core::TemplateEdit>) -> Vec<TextEdit> {
    edits
        .into_iter()
        .map(|edit| TextEdit {
            range: location_to_lsp_range(&edit.location),
            new_text: edit.replacement,
        })
        .collect()
}

fn workspace_edit_for_uri(uri: &Url, edits: Vec<TextEdit>) -> WorkspaceEdit {
    WorkspaceEdit::new(HashMap::from([(uri.clone(), edits)]))
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

fn internal_error(err: anyhow::Error) -> tower_lsp::jsonrpc::Error {
    tower_lsp::jsonrpc::Error {
        code: tower_lsp::jsonrpc::ErrorCode::InternalError,
        message: err.to_string().into(),
        data: None,
    }
}

pub async fn run_server() -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| {
        TLinterLanguageServer::new(client).expect("Failed to create language server")
    });

    Server::new(stdin, stdout, socket).serve(service).await;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TLinterConfig {
    pub enable_type_checking: bool,
    pub pyright_path: Option<String>,
    pub highlight_untyped_templates: bool,
}

impl Default for TLinterConfig {
    fn default() -> Self {
        Self {
            enable_type_checking: true,
            pyright_path: None,
            highlight_untyped_templates: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use t_linter_core::LintSeverity;
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

        options.properties = HashMap::from([(
            "lineLength".to_string(),
            FormattingProperty::Bool(true),
        )]);
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
        };
        let lsp_diagnostic = lint_diagnostic_to_lsp(&diagnostic);
        assert_eq!(lsp_diagnostic.range.start.line, 1);
        assert_eq!(lsp_diagnostic.range.start.character, 2);
        assert_eq!(
            lsp_diagnostic.code,
            Some(NumberOrString::String("demo-rule".to_string()))
        );

        let location = t_linter_core::Location {
            start_line: 4,
            start_column: 2,
            end_line: 5,
            end_column: 8,
        };
        let range = location_to_lsp_range(&location);
        assert_eq!(lsp_range_to_location(&range), location);

        let edit = t_linter_core::TemplateEdit {
            location: location.clone(),
            replacement: "hello".to_string(),
        };
        let lsp_edits = template_edits_to_lsp(vec![edit]);
        assert_eq!(lsp_edits.len(), 1);
        let uri = Url::from_file_path(std::env::temp_dir().join("example.py")).expect("uri");
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
        assert_eq!(options.resolve_provider, Some(false));
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
                .value(),
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
                .contains("data-a=\"x\"")
        );

        server
            .did_change_configuration(DidChangeConfigurationParams {
                settings: serde_json::json!({"demo": true}),
            })
            .await;

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
            .expect("range formatting response");
        assert!(changed.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn semantic_tokens_cover_supported_unsupported_and_untyped_templates() {
        let temp = TempDir::new().expect("tempdir");
        let source = r#"from typing import Annotated
from string.templatelib import Template

typed_html: Annotated[Template, "html"] = t"<div class='card'>{value}</div>"
unsupported: Annotated[Template, "unknown"] = t"""<odd>{{ brace }}</odd>
<span>{value}</span>"""
plain = t"hello {name}"
"#;
        let uri = write_source_file(temp.path(), "tokens.py", source);
        let (service, _) = LspService::new(|client| {
            TLinterLanguageServer::new(client).expect("create language server")
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

        assert!(!tokens.data.is_empty());
        assert!(tokens.data.iter().any(|token| token.token_type == TOKEN_TYPE_MACRO));
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
        let (_, handle) = server
            .diagnostic_tasks
            .remove(uri)
            .expect("scheduled diagnostic task");
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("diagnostic task timed out")
            .expect("diagnostic task completed");
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
}
