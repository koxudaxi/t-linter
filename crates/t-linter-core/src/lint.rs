use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};
use tstring_html::{Attribute, AttributeLike, Node as HtmlNode};
use tstring_syntax::Diagnostic;
use tstring_tdom as backend_tdom;
use tstring_thtml as backend_thtml;

use crate::backend::TemplateBackend;
use crate::parser::{CallableParameter, CallableSignature, CallableValueType, ModuleContext};
use crate::{TemplateStringInfo, TemplateStringParser};

const RULE_EMBEDDED_PARSE_ERROR: &str = "embedded-parse-error";
const RULE_FILE_READ_ERROR: &str = "file-read-error";
const RULE_PYTHON_PARSE_ERROR: &str = "python-parse-error";
const RULE_COMPONENT_MISSING_PROP: &str = "component-missing-prop";
const RULE_COMPONENT_UNEXPECTED_PROP: &str = "component-unexpected-prop";
const RULE_COMPONENT_PROP_TYPE_ERROR: &str = "component-prop-type-error";
const RULE_COMPONENT_UNRESOLVED: &str = "component-unresolved";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LintSeverity {
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LintDiagnostic {
    pub rule: String,
    pub severity: LintSeverity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub message: String,
    pub file: PathBuf,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LintFileResult {
    pub file: PathBuf,
    pub template_count: usize,
    pub diagnostics: Vec<LintDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct LintRunSummary {
    pub files_scanned: usize,
    pub templates_scanned: usize,
    pub diagnostics: usize,
    pub failed_files: usize,
}

#[derive(Debug)]
struct ProcessedTemplate {
    content: String,
    processed_to_original: Vec<usize>,
}

#[derive(Debug, Clone, Default)]
struct StaticSpreadAnalysis {
    bindings: std::collections::HashMap<String, Vec<StaticSpreadBinding>>,
}

#[derive(Debug, Clone)]
struct StaticSpreadBinding {
    scope: ScopeKey,
    assignment_start: usize,
    dict: StaticDictLiteral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ScopeKey {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Default)]
struct StaticDictLiteral {
    entries: Vec<StaticDictEntry>,
}

#[derive(Debug, Clone)]
struct StaticDictEntry {
    key: String,
    value_type: Option<CallableValueType>,
    accepts_none: bool,
}

#[derive(Debug, Clone)]
struct ResolvedSpreadEntry {
    key: String,
    value_type: Option<CallableValueType>,
    accepts_none: bool,
}

#[derive(Debug, Clone)]
enum ResolvedPropValue {
    Explicit(Attribute),
    Spread {
        key: String,
        value_type: Option<CallableValueType>,
        accepts_none: bool,
        span: Option<tstring_syntax::SourceSpan>,
    },
}

pub fn lint_source(path: &Path, source: &str) -> Result<LintFileResult> {
    let python_diagnostic = lint_python_source(path, source)?;

    let mut parser = TemplateStringParser::new()?;
    let templates = parser.find_template_strings_in_file(source, path)?;
    let module_context = parser.module_context().clone();
    let static_spread_analysis = build_static_spread_analysis(source)?;

    let mut diagnostics = Vec::new();
    if let Some(diagnostic) = python_diagnostic {
        diagnostics.push(diagnostic);
    }

    for template in &templates {
        diagnostics.extend(lint_template(
            path,
            source,
            template,
            &module_context,
            &static_spread_analysis,
        )?);
    }

    sort_and_dedup_diagnostics(&mut diagnostics);

    Ok(LintFileResult {
        file: path.to_path_buf(),
        template_count: templates.len(),
        diagnostics,
    })
}

pub fn file_read_error(path: &Path) -> LintFileResult {
    LintFileResult {
        file: path.to_path_buf(),
        template_count: 0,
        diagnostics: vec![LintDiagnostic {
            rule: RULE_FILE_READ_ERROR.to_string(),
            severity: LintSeverity::Error,
            language: None,
            message: "Failed to read file".to_string(),
            file: path.to_path_buf(),
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
        }],
    }
}

fn lint_python_source(path: &Path, source: &str) -> Result<Option<LintDiagnostic>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .context("Failed to initialize Python parser")?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse Python source"))?;

    if !tree.root_node().has_error() {
        return Ok(None);
    }

    let error_node = collect_error_nodes(tree.root_node())
        .into_iter()
        .next()
        .unwrap_or_else(|| tree.root_node());

    Ok(Some(LintDiagnostic {
        rule: RULE_PYTHON_PARSE_ERROR.to_string(),
        severity: LintSeverity::Error,
        language: None,
        message: "Failed to parse Python source".to_string(),
        file: path.to_path_buf(),
        start_line: error_node.start_position().row + 1,
        start_column: error_node.start_position().column + 1,
        end_line: error_node.end_position().row + 1,
        end_column: error_node.end_position().column + 1,
    }))
}

fn lint_template(
    path: &Path,
    source: &str,
    template: &TemplateStringInfo,
    module_context: &ModuleContext,
    static_spread_analysis: &StaticSpreadAnalysis,
) -> Result<Vec<LintDiagnostic>> {
    let Some(language) = template
        .language
        .as_deref()
        .and_then(normalize_language)
        .map(str::to_string)
    else {
        return Ok(Vec::new());
    };

    if let Some(backend) = TemplateBackend::for_language(&language) {
        return lint_backend_template(
            path,
            source,
            template,
            &language,
            backend,
            module_context,
            static_spread_analysis,
        );
    }

    let processed = prepare_template_for_lint(template, &language);
    let tree = parse_embedded(&language, &processed.content)?;

    if !tree.root_node().has_error() {
        return Ok(Vec::new());
    }

    let mut diagnostics = Vec::new();
    let error_nodes = collect_error_nodes(tree.root_node());
    let nodes = if error_nodes.is_empty() {
        vec![tree.root_node()]
    } else {
        error_nodes
    };

    for node in nodes {
        let start_offset =
            map_processed_offset(&processed.processed_to_original, node.start_byte());
        let mut end_offset =
            map_processed_offset(&processed.processed_to_original, node.end_byte());

        if end_offset <= start_offset {
            end_offset = next_char_boundary(&template.content, start_offset);
        }

        let ((start_line, start_column), (end_line, end_column)) =
            map_content_range_to_document(template, start_offset, end_offset);

        diagnostics.push(LintDiagnostic {
            rule: RULE_EMBEDDED_PARSE_ERROR.to_string(),
            severity: LintSeverity::Error,
            language: Some(language.clone()),
            message: format!("Invalid {} syntax in template string", language),
            file: path.to_path_buf(),
            start_line,
            start_column,
            end_line,
            end_column,
        });
    }

    sort_and_dedup_diagnostics(&mut diagnostics);
    Ok(diagnostics)
}

fn lint_backend_template(
    path: &Path,
    source: &str,
    template: &TemplateStringInfo,
    language: &str,
    backend: TemplateBackend,
    module_context: &ModuleContext,
    static_spread_analysis: &StaticSpreadAnalysis,
) -> Result<Vec<LintDiagnostic>> {
    let input = template.to_template_input();
    let result = backend.check_template(&input);

    let Err(error) = result else {
        let mut diagnostics = Vec::new();
        if language == "thtml" {
            diagnostics.extend(lint_thtml_component_props(
                path,
                source,
                template,
                module_context,
                static_spread_analysis,
            )?);
        } else if language == "tdom" {
            diagnostics.extend(lint_tdom_component_props(
                path,
                source,
                template,
                module_context,
                static_spread_analysis,
            )?);
        }
        sort_and_dedup_diagnostics(&mut diagnostics);
        return Ok(diagnostics);
    };

    let backend_diagnostics = if error.diagnostics.is_empty() {
        vec![Diagnostic::error(
            format!("{language}.parse"),
            error.message,
            None,
        )]
    } else {
        error.diagnostics
    };

    let mut diagnostics = backend_diagnostics
        .into_iter()
        .map(|diagnostic| {
            let display_language = if language == "yml" { "yaml" } else { language };
            let location = diagnostic.span.as_ref().map_or_else(
                || template.location.clone(),
                |span| template.backend_span_to_location(span),
            );

            LintDiagnostic {
                rule: RULE_EMBEDDED_PARSE_ERROR.to_string(),
                severity: LintSeverity::Error,
                language: Some(display_language.to_string()),
                message: diagnostic.message,
                file: path.to_path_buf(),
                start_line: location.start_line,
                start_column: location.start_column,
                end_line: location.end_line,
                end_column: location.end_column,
            }
        })
        .collect::<Vec<_>>();

    sort_and_dedup_diagnostics(&mut diagnostics);
    Ok(diagnostics)
}

fn parse_embedded(language: &str, source: &str) -> Result<Tree> {
    let mut parser = Parser::new();

    match language {
        "html" => parser.set_language(&tree_sitter_html::LANGUAGE.into())?,
        "css" => parser.set_language(&tree_sitter_css::LANGUAGE.into())?,
        "javascript" => parser.set_language(&tree_sitter_javascript::LANGUAGE.into())?,
        "json" => parser.set_language(&tree_sitter_json::LANGUAGE.into())?,
        "yaml" => parser.set_language(&tree_sitter_yaml::LANGUAGE.into())?,
        "toml" => parser.set_language(&tree_sitter_toml_ng::LANGUAGE.into())?,
        #[cfg(feature = "sql")]
        "sql" => parser.set_language(&tree_sitter_sequel::LANGUAGE.into())?,
        #[cfg(not(feature = "sql"))]
        "sql" => return Ok(Parser::new().parse("", None).unwrap()),
        _ => return Err(anyhow::anyhow!("Unsupported language: {}", language)),
    }

    parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse embedded template"))
}

fn collect_error_nodes(node: Node<'_>) -> Vec<Node<'_>> {
    let mut nodes = Vec::new();
    collect_error_nodes_inner(node, &mut nodes);
    nodes
}

fn collect_error_nodes_inner<'tree>(node: Node<'tree>, nodes: &mut Vec<Node<'tree>>) {
    if node.is_error() || node.is_missing() {
        nodes.push(node);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.has_error() || child.is_error() || child.is_missing() {
            collect_error_nodes_inner(child, nodes);
        }
    }
}

fn prepare_template_for_lint(template: &TemplateStringInfo, language: &str) -> ProcessedTemplate {
    let placeholder = placeholder_for_language(language);
    let mut content = String::new();
    let mut processed_to_original = vec![0];
    let mut last_end = 0;
    let mut search_start = 0;

    while let Some(pos) = template.content[search_start..].find("{}") {
        let absolute_pos = search_start + pos;
        append_original_segment(
            &mut content,
            &mut processed_to_original,
            &template.content[last_end..absolute_pos],
            last_end,
        );
        append_placeholder_segment(
            &mut content,
            &mut processed_to_original,
            placeholder,
            absolute_pos,
            absolute_pos + 2,
        );
        last_end = absolute_pos + 2;
        search_start = absolute_pos + 2;
    }

    append_original_segment(
        &mut content,
        &mut processed_to_original,
        &template.content[last_end..],
        last_end,
    );

    ProcessedTemplate {
        content,
        processed_to_original,
    }
}

fn append_original_segment(
    processed: &mut String,
    processed_to_original: &mut Vec<usize>,
    segment: &str,
    original_start: usize,
) {
    processed.push_str(segment);
    for offset in 1..=segment.len() {
        processed_to_original.push(original_start + offset);
    }
}

fn append_placeholder_segment(
    processed: &mut String,
    processed_to_original: &mut Vec<usize>,
    placeholder: &str,
    original_start: usize,
    original_end: usize,
) {
    processed.push_str(placeholder);
    for _ in 0..placeholder.len() {
        processed_to_original.push(original_start);
    }
    if let Some(last) = processed_to_original.last_mut() {
        *last = original_end;
    }
}

fn map_processed_offset(processed_to_original: &[usize], processed_offset: usize) -> usize {
    let last_index = processed_to_original.len().saturating_sub(1);
    processed_to_original[processed_offset.min(last_index)]
}

fn map_content_range_to_document(
    template: &TemplateStringInfo,
    start_offset: usize,
    end_offset: usize,
) -> ((usize, usize), (usize, usize)) {
    let prefix_len = template.string_start.len();
    let suffix_len = template.string_end.len();
    let actual_content = &template.raw_content[prefix_len..template.raw_content.len() - suffix_len];
    let template_start_line = template.location.start_line - 1;
    let template_start_col = template.location.start_column - 1;

    let (start_line, start_col) = map_template_position_to_document(
        &template.content,
        actual_content,
        start_offset,
        template_start_line,
        template_start_col,
        prefix_len,
    );
    let (end_line, end_col) = map_template_position_to_document(
        &template.content,
        actual_content,
        end_offset,
        template_start_line,
        template_start_col,
        prefix_len,
    );

    ((start_line + 1, start_col + 1), (end_line + 1, end_col + 1))
}

fn map_template_position_to_document(
    template_content: &str,
    actual_content: &str,
    position_in_template: usize,
    template_start_line: usize,
    template_start_col: usize,
    prefix_len: usize,
) -> (usize, usize) {
    let mut template_idx = 0;
    let mut actual_idx = 0;
    let template_bytes = template_content.as_bytes();
    let actual_bytes = actual_content.as_bytes();

    while template_idx < position_in_template && actual_idx < actual_bytes.len() {
        if template_idx + 1 < template_bytes.len()
            && template_bytes[template_idx] == b'{'
            && template_bytes[template_idx + 1] == b'}'
        {
            if actual_idx < actual_bytes.len() && actual_bytes[actual_idx] == b'{' {
                let mut expr_end = actual_idx + 1;
                while expr_end < actual_bytes.len() && actual_bytes[expr_end] != b'}' {
                    expr_end += 1;
                }
                if expr_end < actual_bytes.len() {
                    expr_end += 1;
                }
                actual_idx = expr_end;
            }
            template_idx += 2;
        } else {
            template_idx += 1;
            actual_idx += 1;
        }
    }

    let mut line = template_start_line;
    let mut col = template_start_col + prefix_len;

    for byte in actual_bytes.iter().take(actual_idx) {
        if *byte == b'\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }

    (line, col)
}

fn next_char_boundary(content: &str, start_offset: usize) -> usize {
    content[start_offset..]
        .chars()
        .next()
        .map(|ch| start_offset + ch.len_utf8())
        .unwrap_or(start_offset)
}

fn placeholder_for_language(language: &str) -> &'static str {
    match language {
        "html" => "t_linter_expr",
        "thtml" => "t_linter_expr",
        "tdom" => "t_linter_expr",
        "css" => "0",
        "javascript" => "tLinterExpr",
        "json" => "\"t_linter_expr\"",
        "yaml" => "t_linter_expr",
        "toml" => "\"t_linter_expr\"",
        "sql" => "1",
        _ => "t_linter_expr",
    }
}

fn normalize_language(language: &str) -> Option<&str> {
    match language.to_ascii_lowercase().as_str() {
        "html" => Some("html"),
        "thtml" => Some("thtml"),
        "tdom" => Some("tdom"),
        "css" => Some("css"),
        "javascript" | "js" => Some("javascript"),
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        "sql" => Some("sql"),
        _ => None,
    }
}

fn lint_thtml_component_props(
    path: &Path,
    source: &str,
    template: &TemplateStringInfo,
    module_context: &ModuleContext,
    static_spread_analysis: &StaticSpreadAnalysis,
) -> Result<Vec<LintDiagnostic>> {
    let document = backend_thtml::prepare_template(&template.to_template_input())
        .map_err(|error| anyhow::anyhow!(error.message))?;
    let mut diagnostics = Vec::new();
    for child in &document.children {
        lint_thtml_component_node(
            path,
            source,
            template,
            module_context,
            static_spread_analysis,
            child,
            &mut diagnostics,
        );
    }
    Ok(diagnostics)
}

fn lint_tdom_component_props(
    path: &Path,
    source: &str,
    template: &TemplateStringInfo,
    module_context: &ModuleContext,
    static_spread_analysis: &StaticSpreadAnalysis,
) -> Result<Vec<LintDiagnostic>> {
    let document = backend_tdom::prepare_template(&template.to_template_input())
        .map_err(|error| anyhow::anyhow!(error.message))?;
    let mut diagnostics = Vec::new();
    for child in &document.children {
        lint_tdom_component_node(
            path,
            source,
            template,
            module_context,
            static_spread_analysis,
            child,
            &mut diagnostics,
        );
    }
    Ok(diagnostics)
}

fn lint_thtml_component_node(
    path: &Path,
    source: &str,
    template: &TemplateStringInfo,
    module_context: &ModuleContext,
    static_spread_analysis: &StaticSpreadAnalysis,
    node: &HtmlNode,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    match node {
        HtmlNode::ComponentTag(component) => {
            lint_component_signature(
                path,
                source,
                template,
                module_context,
                static_spread_analysis,
                component,
                diagnostics,
            );
            for child in &component.children {
                lint_thtml_component_node(
                    path,
                    source,
                    template,
                    module_context,
                    static_spread_analysis,
                    child,
                    diagnostics,
                );
            }
        }
        HtmlNode::Element(element) => {
            for child in &element.children {
                lint_thtml_component_node(
                    path,
                    source,
                    template,
                    module_context,
                    static_spread_analysis,
                    child,
                    diagnostics,
                );
            }
        }
        HtmlNode::RawTextElement(element) => {
            for child in &element.children {
                lint_thtml_component_node(
                    path,
                    source,
                    template,
                    module_context,
                    static_spread_analysis,
                    child,
                    diagnostics,
                );
            }
        }
        HtmlNode::Fragment(fragment) => {
            for child in &fragment.children {
                lint_thtml_component_node(
                    path,
                    source,
                    template,
                    module_context,
                    static_spread_analysis,
                    child,
                    diagnostics,
                );
            }
        }
        _ => {}
    }
}

fn lint_tdom_component_node(
    path: &Path,
    source: &str,
    template: &TemplateStringInfo,
    module_context: &ModuleContext,
    static_spread_analysis: &StaticSpreadAnalysis,
    node: &backend_tdom::Node,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    match node {
        backend_tdom::Node::ComponentTag(component) => {
            lint_tdom_component_signature(
                path,
                source,
                template,
                module_context,
                static_spread_analysis,
                component,
                diagnostics,
            );
            for child in &component.children {
                lint_tdom_component_node(
                    path,
                    source,
                    template,
                    module_context,
                    static_spread_analysis,
                    child,
                    diagnostics,
                );
            }
        }
        backend_tdom::Node::Element(element) => {
            for child in &element.children {
                lint_tdom_component_node(
                    path,
                    source,
                    template,
                    module_context,
                    static_spread_analysis,
                    child,
                    diagnostics,
                );
            }
        }
        backend_tdom::Node::RawTextElement(element) => {
            for child in &element.children {
                lint_tdom_component_node(
                    path,
                    source,
                    template,
                    module_context,
                    static_spread_analysis,
                    child,
                    diagnostics,
                );
            }
        }
        backend_tdom::Node::Fragment(fragment) => {
            for child in &fragment.children {
                lint_tdom_component_node(
                    path,
                    source,
                    template,
                    module_context,
                    static_spread_analysis,
                    child,
                    diagnostics,
                );
            }
        }
        _ => {}
    }
}

fn lint_component_signature(
    path: &Path,
    source: &str,
    template: &TemplateStringInfo,
    module_context: &ModuleContext,
    static_spread_analysis: &StaticSpreadAnalysis,
    component: &tstring_html::ComponentTagNode,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    let Some(signature) = module_context.callable_signatures.get(&component.name) else {
        diagnostics.push(make_component_diagnostic(
            path,
            template,
            "thtml",
            RULE_COMPONENT_UNRESOLVED,
            format!(
                "Component '{}' is not defined in local or imported callables visible to t-linter.",
                component.name
            ),
            component.span.as_ref(),
        ));
        return;
    };

    let mut provided_names = BTreeSet::new();
    let mut resolved_props = std::collections::BTreeMap::<String, ResolvedPropValue>::new();
    let mut has_unknown_spread = false;

    for attribute in &component.attributes {
        match attribute {
            AttributeLike::Attribute(attribute) => {
                provided_names.insert(attribute.name.clone());
                resolved_props.insert(
                    attribute.name.clone(),
                    ResolvedPropValue::Explicit(attribute.clone()),
                );
            }
            AttributeLike::SpreadAttribute(spread) => {
                if let Some(entries) = resolve_static_spread_entries(
                    source,
                    template,
                    static_spread_analysis,
                    &spread.interpolation.expression,
                    spread.interpolation.span.as_ref().or(spread.span.as_ref()),
                ) {
                    for entry in entries {
                        provided_names.insert(entry.key.clone());
                        resolved_props.insert(
                            entry.key.clone(),
                            ResolvedPropValue::Spread {
                                key: entry.key,
                                value_type: entry.value_type,
                                accepts_none: entry.accepts_none,
                                span: spread
                                    .interpolation
                                    .span
                                    .clone()
                                    .or_else(|| spread.span.clone()),
                            },
                        );
                    }
                } else {
                    has_unknown_spread = true;
                }
            }
        }
    }

    for (prop_name, resolved_prop) in &resolved_props {
        if let Some(parameter) = signature
            .parameters
            .iter()
            .find(|parameter| parameter.name == *prop_name && parameter.allows_keyword)
        {
            if let Some(diagnostic) =
                lint_component_attribute_type(path, template, component, resolved_prop, parameter)
            {
                diagnostics.push(diagnostic);
            }
            continue;
        }

        if !signature.accepts_kwargs {
            let span = match resolved_prop {
                ResolvedPropValue::Explicit(attribute) => {
                    attribute.span.as_ref().or(component.span.as_ref())
                }
                ResolvedPropValue::Spread { span, .. } => span.as_ref().or(component.span.as_ref()),
            };
            diagnostics.push(make_component_diagnostic(
                path,
                template,
                "thtml",
                RULE_COMPONENT_UNEXPECTED_PROP,
                format!(
                    "Component '{}' does not accept prop '{}'.",
                    component.name, prop_name
                ),
                span,
            ));
        }
    }

    for parameter in &signature.parameters {
        if !parameter.required
            || parameter.name == "children"
            || parameter.template_language.is_some()
        {
            continue;
        }
        if provided_names.contains(&parameter.name) {
            continue;
        }
        if has_unknown_spread {
            continue;
        }

        diagnostics.push(make_component_diagnostic(
            path,
            template,
            "thtml",
            RULE_COMPONENT_MISSING_PROP,
            format!(
                "Component '{}' is missing required prop '{}'.",
                component.name, parameter.name
            ),
            component.span.as_ref(),
        ));
    }
}

fn lint_component_attribute_type(
    path: &Path,
    template: &TemplateStringInfo,
    component: &tstring_html::ComponentTagNode,
    resolved_prop: &ResolvedPropValue,
    parameter: &CallableParameter,
) -> Option<LintDiagnostic> {
    if parameter.value_types.is_empty() {
        return None;
    }

    let (prop_name, value_kind, span) = match resolved_prop {
        ResolvedPropValue::Explicit(attribute) => (
            attribute.name.as_str(),
            classify_component_attribute_value(attribute),
            attribute.span.as_ref().or(component.span.as_ref()),
        ),
        ResolvedPropValue::Spread {
            key,
            value_type,
            accepts_none,
            span,
        } => {
            if spread_value_matches_parameter(*value_type, *accepts_none, parameter) {
                return None;
            }
            (
                key.as_str(),
                ComponentAttributeValueKind::StringLike,
                span.as_ref().or(component.span.as_ref()),
            )
        }
    };
    let accepts_string = parameter.value_types.contains(&CallableValueType::String);
    let accepts_bool = parameter.value_types.contains(&CallableValueType::Bool);

    match value_kind {
        ComponentAttributeValueKind::BareBoolean if accepts_bool => None,
        ComponentAttributeValueKind::StringLike if accepts_string => None,
        ComponentAttributeValueKind::BareBoolean => Some(make_component_diagnostic(
            path,
            template,
            "thtml",
            RULE_COMPONENT_PROP_TYPE_ERROR,
            format!(
                "Component '{}' prop '{}' expects {}, but bare T-HTML attributes pass boolean true.",
                component.name,
                prop_name,
                describe_callable_value_types(&parameter.value_types, parameter.accepts_none),
            ),
            span,
        )),
        ComponentAttributeValueKind::StringLike => Some(make_component_diagnostic(
            path,
            template,
            "thtml",
            RULE_COMPONENT_PROP_TYPE_ERROR,
            format!(
                "Component '{}' prop '{}' expects {}, but T-HTML attribute syntax passes strings here. Use a spread prop for typed values.",
                component.name,
                prop_name,
                describe_callable_value_types(&parameter.value_types, parameter.accepts_none),
            ),
            span,
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TdomComponentValueKind {
    BareBoolean,
    StringLike,
    Typed {
        value_type: Option<CallableValueType>,
        accepts_none: bool,
        known: bool,
    },
}

#[derive(Debug, Clone)]
struct TdomResolvedPropValue {
    name: String,
    value_kind: TdomComponentValueKind,
    span: Option<tstring_syntax::SourceSpan>,
}

fn lint_tdom_component_signature(
    path: &Path,
    source: &str,
    template: &TemplateStringInfo,
    module_context: &ModuleContext,
    static_spread_analysis: &StaticSpreadAnalysis,
    component: &backend_tdom::ComponentTagNode,
    diagnostics: &mut Vec<LintDiagnostic>,
) {
    let Some(signature) =
        resolve_tdom_component_signature(module_context, &component.start_tag.expression)
    else {
        // Be conservative for opaque or instance-callable targets: keep syntax,
        // check, format, and highlighting support, but skip prop diagnostics.
        return;
    };

    if signature.requires_positional {
        diagnostics.push(make_component_diagnostic(
            path,
            template,
            "tdom",
            RULE_COMPONENT_PROP_TYPE_ERROR,
            format!(
                "Component '{}' cannot be invoked from tdom because it requires positional arguments.",
                component.start_tag.expression
            ),
            component
                .start_tag
                .span
                .as_ref()
                .or(component.span.as_ref()),
        ));
        return;
    }

    let mut resolved_props = std::collections::BTreeMap::<String, TdomResolvedPropValue>::new();
    let mut has_unknown_spread = false;

    for attribute in &component.attributes {
        match attribute {
            backend_tdom::AttributeLike::LiteralAttribute(attribute) => {
                let normalized_name = normalize_tdom_prop_name(&attribute.name);
                resolved_props.insert(
                    normalized_name.clone(),
                    TdomResolvedPropValue {
                        name: normalized_name,
                        value_kind: if attribute.value.is_some() {
                            TdomComponentValueKind::StringLike
                        } else {
                            TdomComponentValueKind::BareBoolean
                        },
                        span: attribute.span.clone(),
                    },
                );
            }
            backend_tdom::AttributeLike::InterpolatedAttribute(attribute) => {
                extend_tdom_interpolated_attr_values(
                    source,
                    template,
                    static_spread_analysis,
                    attribute,
                    &mut resolved_props,
                    &mut has_unknown_spread,
                );
            }
            backend_tdom::AttributeLike::TemplatedAttribute(attribute) => {
                let normalized_name = normalize_tdom_prop_name(&attribute.name);
                resolved_props.insert(
                    normalized_name.clone(),
                    TdomResolvedPropValue {
                        name: normalized_name,
                        value_kind: TdomComponentValueKind::StringLike,
                        span: attribute.span.clone(),
                    },
                );
            }
            backend_tdom::AttributeLike::SpreadAttribute(spread) => {
                if let Some(entries) = resolve_static_spread_entries(
                    source,
                    template,
                    static_spread_analysis,
                    &spread.interpolation.expression,
                    spread.interpolation.span.as_ref().or(spread.span.as_ref()),
                ) {
                    for entry in entries {
                        let normalized_name = normalize_tdom_prop_name(&entry.key);
                        resolved_props.insert(
                            normalized_name.clone(),
                            TdomResolvedPropValue {
                                name: normalized_name,
                                value_kind: TdomComponentValueKind::Typed {
                                    value_type: entry.value_type,
                                    accepts_none: entry.accepts_none,
                                    known: entry.value_type.is_some() || entry.accepts_none,
                                },
                                span: spread
                                    .interpolation
                                    .span
                                    .clone()
                                    .or_else(|| spread.span.clone()),
                            },
                        );
                    }
                } else {
                    has_unknown_spread = true;
                }
            }
        }
    }

    for (prop_name, resolved_prop) in &resolved_props {
        let Some(parameter) = signature
            .parameters
            .iter()
            .find(|parameter| parameter.name == *prop_name && parameter.allows_keyword)
        else {
            // Match tdom runtime semantics: unknown attrs are ignored rather than
            // reported as unexpected props.
            continue;
        };

        if let Some(diagnostic) =
            lint_tdom_component_attribute_type(path, template, component, resolved_prop, parameter)
        {
            diagnostics.push(diagnostic);
        }
    }

    for parameter in &signature.parameters {
        if !parameter.required
            || parameter.name == "children"
            || parameter.template_language.is_some()
        {
            continue;
        }
        if resolved_props.contains_key(&parameter.name) {
            continue;
        }
        if has_unknown_spread {
            continue;
        }

        diagnostics.push(make_component_diagnostic(
            path,
            template,
            "tdom",
            RULE_COMPONENT_MISSING_PROP,
            format!(
                "Component '{}' is missing required prop '{}'.",
                component.start_tag.expression, parameter.name
            ),
            component
                .start_tag
                .span
                .as_ref()
                .or(component.span.as_ref()),
        ));
    }
}

fn extend_tdom_interpolated_attr_values(
    source: &str,
    template: &TemplateStringInfo,
    static_spread_analysis: &StaticSpreadAnalysis,
    attribute: &backend_tdom::InterpolatedAttribute,
    resolved_props: &mut std::collections::BTreeMap<String, TdomResolvedPropValue>,
    has_unknown_spread: &mut bool,
) {
    if attribute.name == "data" || attribute.name == "aria" {
        if let Some(entries) = resolve_static_spread_entries(
            source,
            template,
            static_spread_analysis,
            &attribute.interpolation.expression,
            attribute
                .interpolation
                .span
                .as_ref()
                .or(attribute.span.as_ref()),
        ) {
            for entry in entries {
                let expanded_name = format!("{}-{}", attribute.name, entry.key);
                let normalized_name = normalize_tdom_prop_name(&expanded_name);
                let value_kind = if attribute.name == "aria" {
                    TdomComponentValueKind::StringLike
                } else if matches!(entry.value_type, Some(CallableValueType::Bool))
                    || entry.accepts_none
                {
                    TdomComponentValueKind::Typed {
                        value_type: entry.value_type,
                        accepts_none: entry.accepts_none,
                        known: true,
                    }
                } else if entry.value_type.is_some() {
                    TdomComponentValueKind::StringLike
                } else {
                    TdomComponentValueKind::Typed {
                        value_type: entry.value_type,
                        accepts_none: entry.accepts_none,
                        known: false,
                    }
                };
                resolved_props.insert(
                    normalized_name.clone(),
                    TdomResolvedPropValue {
                        name: normalized_name,
                        value_kind,
                        span: attribute
                            .interpolation
                            .span
                            .clone()
                            .or_else(|| attribute.span.clone()),
                    },
                );
            }
            return;
        }

        *has_unknown_spread = true;
        return;
    }

    let normalized_name = normalize_tdom_prop_name(&attribute.name);
    let value_kind = if matches!(attribute.name.as_str(), "class" | "style") {
        TdomComponentValueKind::StringLike
    } else if let Some((value_type, accepts_none)) =
        parse_static_value_expression(&attribute.interpolation.expression)
    {
        TdomComponentValueKind::Typed {
            value_type,
            accepts_none,
            known: true,
        }
    } else {
        TdomComponentValueKind::Typed {
            value_type: None,
            accepts_none: false,
            known: false,
        }
    };
    resolved_props.insert(
        normalized_name.clone(),
        TdomResolvedPropValue {
            name: normalized_name,
            value_kind,
            span: attribute
                .interpolation
                .span
                .clone()
                .or_else(|| attribute.span.clone()),
        },
    );
}

fn lint_tdom_component_attribute_type(
    path: &Path,
    template: &TemplateStringInfo,
    component: &backend_tdom::ComponentTagNode,
    resolved_prop: &TdomResolvedPropValue,
    parameter: &CallableParameter,
) -> Option<LintDiagnostic> {
    if parameter.value_types.is_empty() {
        return None;
    }

    let span = resolved_prop
        .span
        .as_ref()
        .or(component.start_tag.span.as_ref())
        .or(component.span.as_ref());
    let accepts_string = parameter.value_types.contains(&CallableValueType::String);
    let accepts_bool = parameter.value_types.contains(&CallableValueType::Bool);

    match resolved_prop.value_kind {
        TdomComponentValueKind::BareBoolean if accepts_bool => None,
        TdomComponentValueKind::BareBoolean => Some(make_component_diagnostic(
            path,
            template,
            "tdom",
            RULE_COMPONENT_PROP_TYPE_ERROR,
            format!(
                "Component '{}' prop '{}' expects {}, but bare tdom attributes pass boolean true.",
                component.start_tag.expression,
                resolved_prop.name,
                describe_callable_value_types(&parameter.value_types, parameter.accepts_none),
            ),
            span,
        )),
        TdomComponentValueKind::StringLike if accepts_string => None,
        TdomComponentValueKind::StringLike => Some(make_component_diagnostic(
            path,
            template,
            "tdom",
            RULE_COMPONENT_PROP_TYPE_ERROR,
            format!(
                "Component '{}' prop '{}' expects {}, but this tdom attribute resolves to a string-like value.",
                component.start_tag.expression,
                resolved_prop.name,
                describe_callable_value_types(&parameter.value_types, parameter.accepts_none),
            ),
            span,
        )),
        TdomComponentValueKind::Typed {
            value_type,
            accepts_none,
            known,
        } => {
            if !known || spread_value_matches_parameter(value_type, accepts_none, parameter) {
                None
            } else {
                Some(make_component_diagnostic(
                    path,
                    template,
                    "tdom",
                    RULE_COMPONENT_PROP_TYPE_ERROR,
                    format!(
                        "Component '{}' prop '{}' expects {}, but the interpolated value resolves to an incompatible type.",
                        component.start_tag.expression,
                        resolved_prop.name,
                        describe_callable_value_types(
                            &parameter.value_types,
                            parameter.accepts_none
                        ),
                    ),
                    span,
                ))
            }
        }
    }
}

fn resolve_tdom_component_signature<'a>(
    module_context: &'a ModuleContext,
    expression: &str,
) -> Option<&'a CallableSignature> {
    module_context
        .callable_signatures
        .get(expression)
        .or_else(|| {
            let (base, suffix) = expression.split_once('.')?;
            let import_target = module_context.imports.get(base)?;
            module_context
                .callable_signatures
                .get(&format!("{import_target}.{suffix}"))
        })
}

fn normalize_tdom_prop_name(name: &str) -> String {
    name.replace('-', "_").to_ascii_lowercase()
}

fn make_component_diagnostic(
    path: &Path,
    template: &TemplateStringInfo,
    language: &str,
    rule: &str,
    message: String,
    span: Option<&tstring_syntax::SourceSpan>,
) -> LintDiagnostic {
    let location = span.map_or_else(
        || template.location.clone(),
        |span| template.backend_span_to_location(span),
    );

    LintDiagnostic {
        rule: rule.to_string(),
        severity: LintSeverity::Error,
        language: Some(language.to_string()),
        message,
        file: path.to_path_buf(),
        start_line: location.start_line,
        start_column: location.start_column,
        end_line: location.end_line,
        end_column: location.end_column,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComponentAttributeValueKind {
    BareBoolean,
    StringLike,
}

fn classify_component_attribute_value(attribute: &Attribute) -> ComponentAttributeValueKind {
    match &attribute.value {
        None => ComponentAttributeValueKind::BareBoolean,
        Some(_) => ComponentAttributeValueKind::StringLike,
    }
}

fn spread_value_matches_parameter(
    value_type: Option<CallableValueType>,
    accepts_none: bool,
    parameter: &CallableParameter,
) -> bool {
    match value_type {
        Some(CallableValueType::Int) => {
            parameter.value_types.contains(&CallableValueType::Int)
                || parameter.value_types.contains(&CallableValueType::Float)
        }
        Some(value_type) => parameter.value_types.contains(&value_type),
        None if accepts_none => parameter.accepts_none,
        None => true,
    }
}

fn describe_callable_value_types(value_types: &[CallableValueType], accepts_none: bool) -> String {
    let mut names = value_types
        .iter()
        .map(|value_type| match value_type {
            CallableValueType::Bool => "bool",
            CallableValueType::Int => "int",
            CallableValueType::Float => "float",
            CallableValueType::String => "str",
        })
        .collect::<Vec<_>>();
    if accepts_none {
        names.push("None");
    }
    names.sort_unstable();
    names.dedup();
    names.join(" | ")
}

fn build_static_spread_analysis(source: &str) -> Result<StaticSpreadAnalysis> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .context("Failed to initialize Python parser for spread analysis")?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse Python source for spread analysis"))?;

    let query = Query::new(
        &tree_sitter_python::LANGUAGE.into(),
        r#"
        (assignment
            left: (identifier) @name
            right: (dictionary) @value)
        "#,
    )
    .context("Failed to create spread analysis query")?;

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    let mut analysis = StaticSpreadAnalysis::default();

    while let Some(match_) = matches.next() {
        let mut name: Option<Node<'_>> = None;
        let mut value: Option<Node<'_>> = None;
        for capture in match_.captures {
            match query.capture_names()[capture.index as usize] {
                "name" => name = Some(capture.node),
                "value" => value = Some(capture.node),
                _ => {}
            }
        }

        let (Some(name_node), Some(value_node)) = (name, value) else {
            continue;
        };
        let Some(name_text) = name_node.utf8_text(source.as_bytes()).ok() else {
            continue;
        };
        let Some(dict) = parse_static_dictionary_node(value_node, source) else {
            continue;
        };

        analysis
            .bindings
            .entry(name_text.to_string())
            .or_default()
            .push(StaticSpreadBinding {
                scope: enclosing_scope(name_node),
                assignment_start: name_node.start_byte(),
                dict,
            });
    }

    for bindings in analysis.bindings.values_mut() {
        bindings.sort_by_key(|binding| binding.assignment_start);
    }

    Ok(analysis)
}

fn resolve_static_spread_entries(
    source: &str,
    template: &TemplateStringInfo,
    analysis: &StaticSpreadAnalysis,
    expression: &str,
    span: Option<&tstring_syntax::SourceSpan>,
) -> Option<Vec<ResolvedSpreadEntry>> {
    if let Some(dict) = parse_static_dictionary_expression(expression) {
        return Some(
            dict.entries
                .into_iter()
                .map(|entry| ResolvedSpreadEntry {
                    key: entry.key,
                    value_type: entry.value_type,
                    accepts_none: entry.accepts_none,
                })
                .collect(),
        );
    }

    if !is_identifier_expression(expression) {
        return None;
    }

    let use_offset = span
        .map(|span| template.backend_span_to_location(span))
        .and_then(|location| {
            location_to_byte_offset(source, location.start_line, location.start_column)
        })
        .unwrap_or(source.len());

    let bindings = analysis.bindings.get(expression)?;
    let scope_chain = scope_chain_for_offset(source, use_offset);
    let binding = scope_chain
        .iter()
        .find_map(|scope| {
            bindings
                .iter()
                .rev()
                .find(|binding| binding.scope == *scope && binding.assignment_start < use_offset)
        })
        .or_else(|| {
            bindings
                .iter()
                .rev()
                .find(|binding| binding.assignment_start < use_offset)
        })?;

    Some(
        binding
            .dict
            .entries
            .iter()
            .map(|entry| ResolvedSpreadEntry {
                key: entry.key.clone(),
                value_type: entry.value_type,
                accepts_none: entry.accepts_none,
            })
            .collect(),
    )
}

fn parse_static_dictionary_expression(expression: &str) -> Option<StaticDictLiteral> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(expression, None)?;
    let root = tree.root_node();
    let dict_node = match root.named_child(0) {
        Some(child) if child.kind() == "expression_statement" => child.named_child(0)?,
        Some(child) => child,
        None => root,
    };
    parse_static_dictionary_node(dict_node, expression)
}

fn parse_static_value_expression(expression: &str) -> Option<(Option<CallableValueType>, bool)> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(expression, None)?;
    let root = tree.root_node();
    let value_node = match root.named_child(0) {
        Some(child) if child.kind() == "expression_statement" => child.named_child(0)?,
        Some(child) => child,
        None => root,
    };

    match value_node.kind() {
        "true" | "false" | "integer" | "float" | "string" | "none" => {
            Some(parse_static_value_node(value_node))
        }
        _ => None,
    }
}

fn parse_static_dictionary_node(node: Node<'_>, source: &str) -> Option<StaticDictLiteral> {
    if node.kind() != "dictionary" {
        return None;
    }

    let mut cursor = node.walk();
    let mut dict = StaticDictLiteral::default();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "pair" {
            return None;
        }
        let key_node = child.child_by_field_name("key")?;
        let value_node = child.child_by_field_name("value")?;
        let key_text = key_node.utf8_text(source.as_bytes()).ok()?;
        let key = unquote_python_string(key_text)?;
        let (value_type, accepts_none) = parse_static_value_node(value_node);
        dict.entries.push(StaticDictEntry {
            key,
            value_type,
            accepts_none,
        });
    }

    Some(dict)
}

fn scope_chain_for_offset(source: &str, offset: usize) -> Vec<ScopeKey> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .is_err()
    {
        return vec![ScopeKey {
            start: 0,
            end: source.len(),
        }];
    }
    let Some(tree) = parser.parse(source, None) else {
        return vec![ScopeKey {
            start: 0,
            end: source.len(),
        }];
    };
    let root = tree.root_node();
    let Some(node) = root.named_descendant_for_byte_range(offset, offset) else {
        return vec![ScopeKey {
            start: root.start_byte(),
            end: root.end_byte(),
        }];
    };
    scope_chain(node)
}

fn scope_chain(node: Node) -> Vec<ScopeKey> {
    let mut scopes = Vec::new();
    let mut current = Some(node);

    while let Some(candidate) = current {
        if is_scope_node(candidate) {
            scopes.push(ScopeKey {
                start: candidate.start_byte(),
                end: candidate.end_byte(),
            });
        }
        current = candidate.parent();
    }

    if scopes.is_empty() {
        scopes.push(ScopeKey {
            start: node.start_byte(),
            end: node.end_byte(),
        });
    }

    scopes
}

fn enclosing_scope(node: Node) -> ScopeKey {
    scope_chain(node).into_iter().next().unwrap_or(ScopeKey {
        start: node.start_byte(),
        end: node.end_byte(),
    })
}

fn is_scope_node(node: Node) -> bool {
    matches!(
        node.kind(),
        "module" | "function_definition" | "class_definition" | "lambda"
    )
}

fn parse_static_value_node(node: Node<'_>) -> (Option<CallableValueType>, bool) {
    match node.kind() {
        "true" | "false" => (Some(CallableValueType::Bool), false),
        "integer" => (Some(CallableValueType::Int), false),
        "float" => (Some(CallableValueType::Float), false),
        "string" => (Some(CallableValueType::String), false),
        "none" => (None, true),
        _ => (None, false),
    }
}

fn unquote_python_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let without_prefix = trimmed.trim_start_matches(['r', 'R', 'u', 'U', 'b', 'B']);
    if without_prefix.len() < 2 {
        return None;
    }
    let first = without_prefix.as_bytes().first().copied()?;
    let last = without_prefix.as_bytes().last().copied()?;
    if !matches!(first, b'\'' | b'"') || first != last {
        return None;
    }
    Some(without_prefix[1..without_prefix.len() - 1].to_string())
}

fn is_identifier_expression(expression: &str) -> bool {
    let mut chars = expression.chars();
    match chars.next() {
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn location_to_byte_offset(source: &str, line: usize, column: usize) -> Option<usize> {
    if line == 0 || column == 0 {
        return None;
    }

    let mut current_line = 1usize;
    let mut current_column = 1usize;
    for (offset, ch) in source.char_indices() {
        if current_line == line && current_column == column {
            return Some(offset);
        }
        if ch == '\n' {
            current_line += 1;
            current_column = 1;
        } else {
            current_column += 1;
        }
    }

    if current_line == line && current_column == column {
        Some(source.len())
    } else {
        None
    }
}

fn sort_and_dedup_diagnostics(diagnostics: &mut Vec<LintDiagnostic>) {
    diagnostics.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then(left.start_line.cmp(&right.start_line))
            .then(left.start_column.cmp(&right.start_column))
            .then(left.end_line.cmp(&right.end_line))
            .then(left.end_column.cmp(&right.end_column))
            .then(left.rule.cmp(&right.rule))
            .then(left.language.cmp(&right.language))
            .then(left.message.cmp(&right.message))
    });
    diagnostics.dedup_by(|left, right| {
        left.file == right.file
            && left.start_line == right.start_line
            && left.start_column == right.start_column
            && left.end_line == right.end_line
            && left.end_column == right.end_column
            && left.rule == right.rule
            && left.language == right.language
            && left.message == right.message
    });
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn to_python_template_source(template: &str) -> String {
        template
            .replace('{', "{{")
            .replace('}', "}}")
            .replace("{{}}", "{value}")
    }

    fn lint_embedded(language: &str, template: &str) -> LintFileResult {
        let python_template = to_python_template_source(template);
        let prefix = if language == "thtml" {
            r#"def Card(*, title: str, children: str | None = None) -> object:
    return None

def Badge(*, children: str | None = None) -> object:
    return None

"#
        } else if language == "tdom" {
            r#"def Card(*, title: str, children: tuple[object, ...] = ()) -> object:
    return None

"#
        } else {
            ""
        };
        let source = format!(
            r#"{prefix}from typing import Annotated
from string.templatelib import Template

value: Annotated[Template, "{language}"] = t"""{python_template}"""
"#
        );

        lint_source(Path::new("sample.py"), &source).unwrap()
    }

    #[test]
    fn valid_templates_do_not_report_diagnostics() {
        let valid_cases = [
            ("html", "<div>{}</div>"),
            ("thtml", "<Card title=\"{}\"><Badge>{}</Badge></Card>"),
            ("tdom", "<{} title={}><span>{}</span></{}>"),
            ("css", "body { color: {}; }"),
            ("javascript", "const value = {};"),
            ("json", r#"{"name": {}, "enabled": true}"#),
            ("yaml", "name: {}\nenabled: true\n"),
            ("toml", "name = {}\nenabled = true\n"),
            ("sql", "SELECT * FROM users WHERE id = {}"),
        ];

        for (language, template) in valid_cases {
            let result = lint_embedded(language, template);
            assert!(
                result.diagnostics.is_empty(),
                "expected no diagnostics for {language}, got {:?}",
                result.diagnostics
            );
        }
    }

    #[test]
    fn invalid_templates_report_embedded_parse_errors() {
        let invalid_cases = [
            ("html", "<div><"),
            ("thtml", "<Card><"),
            ("tdom", "<{}></div>"),
            ("css", "body { color: ; }"),
            ("javascript", "function {"),
            ("json", "[1,,2]"),
            ("yaml", "name: [1, 2\n"),
            ("toml", "title =\n"),
            ("sql", "SELECT * FROM"),
        ];

        for (language, template) in invalid_cases {
            let result = lint_embedded(language, template);
            assert!(
                result
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.rule == RULE_EMBEDDED_PARSE_ERROR),
                "expected parse error for {language}, got {:?}",
                result.diagnostics
            );
        }
    }

    #[test]
    fn aliases_are_normalized() {
        let js_result = lint_embedded("js", "const value = {};");
        assert!(js_result.diagnostics.is_empty());

        let yaml_result = lint_embedded("yml", "name: {}\n");
        assert!(yaml_result.diagnostics.is_empty());
    }

    #[test]
    fn unknown_languages_are_ignored() {
        let result = lint_embedded("ruby", "puts {}");
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn python_parse_errors_are_reported() {
        let result = lint_source(Path::new("broken.py"), "def broken(\n").unwrap();
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.rule == RULE_PYTHON_PARSE_ERROR)
        );
    }
}
