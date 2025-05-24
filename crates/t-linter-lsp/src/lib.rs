use anyhow::Result;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use t_linter_core::{TemplateHighlighter, TemplateStringInfo, TemplateStringParser};
use tower_lsp::jsonrpc::Result as JsonRpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tracing::{debug, error, info};

const TOKEN_TYPE_MACRO: u32 = 14;
const TOKEN_MODIFIER_NONE: u32 = 0;

pub struct TLinterLanguageServer {
    client: Client,
    document_cache: Arc<DashMap<Url, String>>,
    parser: Arc<tokio::sync::Mutex<TemplateStringParser>>,
    highlighter: Arc<tokio::sync::Mutex<TemplateHighlighter>>,
}

impl TLinterLanguageServer {
    pub fn new(client: Client) -> Result<Self> {
        Ok(Self {
            client,
            document_cache: Arc::new(DashMap::new()),
            parser: Arc::new(tokio::sync::Mutex::new(TemplateStringParser::new()?)),
            highlighter: Arc::new(tokio::sync::Mutex::new(TemplateHighlighter::new()?)),
        })
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
                                // Match VSCode's expected token types
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

        if let Err(e) = self.analyze_document(&uri).await {
            self.client
                .log_message(MessageType::ERROR, format!("Analysis failed: {}", e))
                .await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;

        if let Some(change) = params.content_changes.into_iter().next() {
            self.document_cache.insert(uri.clone(), change.text);

            if let Err(e) = self.analyze_document(&uri).await {
                self.client
                    .log_message(MessageType::ERROR, format!("Analysis failed: {}", e))
                    .await;
            }
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        debug!("Configuration changed: {:?}", params);
    }
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.document_cache.remove(&params.text_document.uri);
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
    async fn analyze_document(&self, uri: &Url) -> Result<()> {
        let text = self
            .document_cache
            .get(uri)
            .ok_or_else(|| anyhow::anyhow!("Document not found in cache"))?
            .clone();

        let mut parser = self.parser.lock().await;
        let templates = parser.find_template_strings(&text)?;

        self.client
            .log_message(
                MessageType::INFO,
                format!("Found {} template strings in {}", templates.len(), uri),
            )
            .await;

        Ok(())
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
            template.raw_content.lines().next().map(|l| l.len()).unwrap_or(0) as u32
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
            template.raw_content.chars().take(30).collect::<String>(),
            template.location.start_line,
            template.location.start_column,
            template.location.end_line,
            template.location.end_column
        );

            if let Some(lang) = &template.language {
                info!("Attempting to highlight {} template", lang);

                let mut highlighter = self.highlighter.lock().await;
                match highlighter.highlight_template(template) {
                    Ok(ranges) => {
                        info!("Successfully highlighted {} ranges", ranges.len());

                        for (i, range) in ranges.iter().take(3).enumerate() {
                            info!("  Range {}: {}..{} type={}", 
                            i, range.start_byte, range.end_byte, range.highlight_name);
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

                        let start_line = (template.location.start_line - 1) as u32;
                        let start_col = (template.location.start_column - 1) as u32;
                        let end_col = (template.location.end_column - 1) as u32;
                        let length = end_col - start_col;

                        all_tokens.push((start_line, start_col, length, TOKEN_TYPE_MACRO, TOKEN_MODIFIER_NONE));
                    }
                }
            } else {
                info!("No language specified, using single token");

                let start_line = (template.location.start_line - 1) as u32;
                let start_col = (template.location.start_column - 1) as u32;
                let end_line = (template.location.end_line - 1) as u32;
                let end_col = (template.location.end_column - 1) as u32;

                if start_line == end_line {
                    let length = end_col - start_col;
                    info!(
                    "Single-line template token: line={}, col={}, len={}",
                    start_line, start_col, length
                );

                    all_tokens.push((start_line, start_col, length, TOKEN_TYPE_MACRO, TOKEN_MODIFIER_NONE));
                } else {
                    info!("Multi-line template from line {} to {}", start_line + 1, end_line + 1);

                    let first_line = text.lines().nth(start_line as usize).unwrap_or("");
                    let first_line_len = first_line.len() as u32 - start_col;
                    all_tokens.push((start_line, start_col, first_line_len, TOKEN_TYPE_MACRO, TOKEN_MODIFIER_NONE));

                    for line_idx in (start_line + 1)..end_line {
                        let line = text.lines().nth(line_idx as usize).unwrap_or("");
                        all_tokens.push((line_idx, 0, line.len() as u32, TOKEN_TYPE_MACRO, TOKEN_MODIFIER_NONE));
                    }

                    all_tokens.push((end_line, 0, end_col, TOKEN_TYPE_MACRO, TOKEN_MODIFIER_NONE));
                }
            }
        }

        all_tokens.sort_by(|a, b| {
            a.0.cmp(&b.0).then(a.1.cmp(&b.1))
        });

        all_tokens.dedup_by(|a, b| {
            a.0 == b.0 && a.1 == b.1 && a.2 == b.2 && a.3 == b.3 && a.4 == b.4
        });

        info!("Sorted tokens:");
        for (i, &(line, col, len, typ, _)) in all_tokens.iter().enumerate() {
            info!("  Token {}: line={}, col={}, len={}, type={}", i, line, col, len, typ);
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
