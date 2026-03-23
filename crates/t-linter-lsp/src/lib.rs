use anyhow::Result;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
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
}

impl TLinterLanguageServer {
    async fn debug_template_positions(&self, uri: &Url) -> Result<()> {
        let text = self
            .document_cache
            .get(uri)
            .ok_or_else(|| anyhow::anyhow!("Document not found in cache"))?
            .clone();

        let lines: Vec<&str> = text.lines().collect();
        let mut parser = self.parser.lock().await;
        let templates = parser.find_template_strings(&text)?;

        info!("=== TEMPLATE POSITION DEBUG ===");

        for (idx, template) in templates.iter().enumerate() {
            info!("\n--- Template {} ---", idx);
            info!("Raw content: {:?}", template.raw_content);
            info!("Processed content: {:?}", template.content);
            info!(
                "Document position: line {} col {} to line {} col {}",
                template.location.start_line,
                template.location.start_column,
                template.location.end_line,
                template.location.end_column
            );

            let start_line_idx = template.location.start_line - 1;
            let start_col_idx = template.location.start_column - 1;
            let end_line_idx = template.location.end_line - 1;
            let end_col_idx = template.location.end_column - 1;

            if start_line_idx < lines.len() {
                let line = lines[start_line_idx];
                info!("Line {}: '{}'", template.location.start_line, line);

                if start_col_idx < line.len() {
                    let template_text = if start_line_idx == end_line_idx {
                        &line[start_col_idx..end_col_idx.min(line.len())]
                    } else {
                        &line[start_col_idx..]
                    };
                    info!("Extracted template text: '{}'", template_text);
                }
            }

            for (i, expr) in template.expressions.iter().enumerate() {
                info!(
                    "  Expression {}: '{}' at {}:{}-{}:{}",
                    i,
                    expr.content,
                    expr.location.start_line,
                    expr.location.start_column,
                    expr.location.end_line,
                    expr.location.end_column
                );
            }
        }

        info!("=== END POSITION DEBUG ===\n");
        Ok(())
    }
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

    async fn format_uri(
        &self,
        uri: &Url,
        range: Option<Range>,
        formatting_options: Option<&FormattingOptions>,
    ) -> JsonRpcResult<Vec<TextEdit>> {
        let text = self
            .document_cache
            .get(uri)
            .ok_or_else(|| tower_lsp::jsonrpc::Error::invalid_params("Document not found"))?
            .clone();
        let options =
            resolve_lsp_format_options(uri, formatting_options).map_err(internal_error)?;

        let edits = match range {
            Some(range) => {
                let location = lsp_range_to_location(&range);
                format_document_range_with_options(&text, &location, &options)
                    .map_err(internal_error)?
            }
            None => format_document_with_options(&text, &options).map_err(internal_error)?,
        };

        Ok(edits
            .into_iter()
            .map(|edit| TextEdit {
                range: location_to_lsp_range(&edit.location),
                new_text: edit.replacement,
            })
            .collect())
    }

    fn generate_basic_template_tokens(
        &self,
        template: &TemplateStringInfo,
    ) -> Vec<(u32, u32, u32, u32, u32)> {
        let mut tokens = Vec::new();

        let line = (template.location.start_line - 1) as u32;
        let start_col = (template.location.start_column - 1) as u32;

        let end_col = (template.location.end_column - 1) as u32;
        let length = if template.location.start_line == template.location.end_line {
            end_col - start_col
        } else {
            template
                .raw_content
                .lines()
                .next()
                .map(|l| l.len())
                .unwrap_or(0) as u32
        };

        tokens.push((line, start_col, length, 18, 0));

        tokens
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
                    info!("Unsupported highlight language {}, using fallback tokens", lang);
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
    use std::collections::HashMap;

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
}
