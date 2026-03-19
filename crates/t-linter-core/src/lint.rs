use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use tree_sitter::{Node, Parser, Tree};
use tstring_json as backend_json;
use tstring_syntax::Diagnostic;
use tstring_toml as backend_toml;
use tstring_yaml as backend_yaml;

use crate::{TemplateStringInfo, TemplateStringParser};

const RULE_EMBEDDED_PARSE_ERROR: &str = "embedded-parse-error";
const RULE_FILE_READ_ERROR: &str = "file-read-error";
const RULE_PYTHON_PARSE_ERROR: &str = "python-parse-error";

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

pub fn lint_source(path: &Path, source: &str) -> Result<LintFileResult> {
    let python_diagnostic = lint_python_source(path, source)?;

    let mut parser = TemplateStringParser::new()?;
    let templates = parser.find_template_strings_in_file(source, path)?;

    let mut diagnostics = Vec::new();
    if let Some(diagnostic) = python_diagnostic {
        diagnostics.push(diagnostic);
    }

    for template in &templates {
        diagnostics.extend(lint_template(path, template)?);
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

fn lint_template(path: &Path, template: &TemplateStringInfo) -> Result<Vec<LintDiagnostic>> {
    let Some(language) = template
        .language
        .as_deref()
        .and_then(normalize_language)
        .map(str::to_string)
    else {
        return Ok(Vec::new());
    };

    if matches!(language.as_str(), "json" | "yaml" | "yml" | "toml") {
        return lint_backend_template(path, template, &language);
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
    template: &TemplateStringInfo,
    language: &str,
) -> Result<Vec<LintDiagnostic>> {
    let input = template.to_template_input();
    let result = match language {
        "json" => backend_json::check_template(&input),
        "yaml" | "yml" => backend_yaml::check_template(&input),
        "toml" => backend_toml::check_template(&input),
        other => return Err(anyhow::anyhow!("Unsupported backend language: {other}")),
    };

    let Err(error) = result else {
        return Ok(Vec::new());
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
    let prefix_len = calculate_template_content_offset(&template.raw_content);
    let suffix_len = if template.flags.is_triple { 3 } else { 1 };
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

fn calculate_template_content_offset(raw_content: &str) -> usize {
    if raw_content.starts_with("t\"\"\"") || raw_content.starts_with("t'''") {
        4
    } else if raw_content.starts_with("tr\"\"\"") || raw_content.starts_with("tr'''") {
        5
    } else if raw_content.starts_with("t\"") || raw_content.starts_with("t'") {
        2
    } else if raw_content.starts_with("tr\"") || raw_content.starts_with("tr'") {
        3
    } else {
        0
    }
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
        "css" => Some("css"),
        "javascript" | "js" => Some("javascript"),
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        "sql" => Some("sql"),
        _ => None,
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
        let source = format!(
            r#"from typing import Annotated
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
