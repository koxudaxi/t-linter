use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::Glob;
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};
use tstring_html::{Attribute, AttributeLike, Node as HtmlNode};
use tstring_syntax::Diagnostic;
use tstring_tdom as backend_tdom;
use tstring_thtml as backend_thtml;

use crate::backend::TemplateBackend;
use crate::parser::{CallableParameter, CallableValueType, ModuleContext};
use crate::project_config::{ProjectConfig, RuleSeverity, SqlConfig, load_project_config_for_path};
use crate::tdom::resolve_component_signature;
use crate::{TemplatePart, TemplateStringInfo, TemplateStringParser};

const RULE_EMBEDDED_PARSE_ERROR: &str = "embedded-parse-error";
const RULE_FILE_READ_ERROR: &str = "file-read-error";
const RULE_PYTHON_PARSE_ERROR: &str = "python-parse-error";
const RULE_COMPONENT_MISSING_PROP: &str = "component-missing-prop";
const RULE_COMPONENT_UNEXPECTED_PROP: &str = "component-unexpected-prop";
const RULE_COMPONENT_PROP_TYPE_ERROR: &str = "component-prop-type-error";
const RULE_COMPONENT_UNRESOLVED: &str = "component-unresolved";
const RULE_TEMPLATE_SCHEMA_MISSING_KEY: &str = "template-schema-missing-key";
const RULE_TEMPLATE_SCHEMA_UNKNOWN_KEY: &str = "template-schema-unknown-key";
const RULE_TEMPLATE_SCHEMA_TYPE_SHAPE: &str = "template-schema-type-shape";
const RULE_TEMPLATE_METADATA_CONFLICT: &str = "template-metadata-conflict";
const RULE_TEMPLATE_METADATA_REDUNDANT_LANGUAGE: &str = "template-metadata-redundant-language";
const RULE_BINDING_UNRESOLVED: &str = "binding-unresolved";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LintSeverity {
    Error,
    Warning,
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
    pub expected_type: Option<String>,
    pub found_type: Option<String>,
    pub schema_pointer: Option<String>,
    pub source_of_truth: Option<String>,
    pub suggested_edits: Vec<DiagnosticEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticEdit {
    pub range: DiagnosticEditRange,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticEditRange {
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

impl DiagnosticEditRange {
    pub fn from_location(location: &crate::Location) -> Self {
        Self {
            start_line: location.start_line,
            start_column: location.start_column,
            end_line: location.end_line,
            end_column: location.end_column,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiagnosticData {
    pub expected_type: Option<String>,
    pub found_type: Option<String>,
    pub schema_pointer: Option<String>,
    pub source_of_truth: Option<String>,
    pub suggested_edits: Vec<DiagnosticEdit>,
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

#[derive(Debug, Clone, Default)]
struct SchemaModel {
    fields: std::collections::BTreeMap<String, SchemaField>,
}

#[derive(Debug, Clone)]
struct SchemaField {
    required: bool,
    scalar: Option<JsonScalarKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonScalarKind {
    Integer,
    Number,
    String,
    Boolean,
    Null,
    Array,
    Object,
}

#[derive(Debug, Clone, Default)]
struct JsonObjectOutline {
    pointer: String,
    static_keys: std::collections::BTreeMap<String, JsonKeyOutline>,
    has_interpolation_key: bool,
}

#[derive(Debug, Clone)]
struct JsonKeyOutline {
    key_location: crate::Location,
    value_location: crate::Location,
    value_kind: Option<JsonScalarKind>,
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
    let config = load_project_config_for_path(path)?;
    lint_source_with_config(path, source, &config)
}

pub fn lint_source_with_config(
    path: &Path,
    source: &str,
    config: &ProjectConfig,
) -> Result<LintFileResult> {
    let python_diagnostic = lint_python_source(path, source)?;

    let mut parser = TemplateStringParser::new()?;
    let templates = parser.find_template_strings_in_file(source, path)?;
    let module_context = parser.module_context().clone();
    let static_spread_analysis = build_static_spread_analysis(source)?;

    let mut diagnostics = Vec::new();
    if let Some(diagnostic) = python_diagnostic {
        diagnostics.push(diagnostic);
    }
    diagnostics.extend(lint_template_metadata_markers(
        path,
        source,
        &templates,
        &module_context,
    )?);

    for template in &templates {
        diagnostics.extend(lint_template(
            path,
            source,
            template,
            &module_context,
            &static_spread_analysis,
            &config.sql,
        )?);
    }
    diagnostics.extend(lint_json_schema_bindings(
        path,
        source,
        &templates,
        &module_context,
    )?);

    sort_and_dedup_diagnostics(&mut diagnostics);
    apply_suppressions(&mut diagnostics, source, &templates)?;
    apply_rule_config(&mut diagnostics, config, path);

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
            expected_type: None,
            found_type: None,
            schema_pointer: None,
            source_of_truth: None,
            suggested_edits: Vec::new(),
        }],
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Suppression {
    line: usize,
    own_line: bool,
    rules: Option<Vec<String>>,
}

fn apply_suppressions(
    diagnostics: &mut Vec<LintDiagnostic>,
    source: &str,
    templates: &[TemplateStringInfo],
) -> Result<()> {
    if diagnostics.is_empty() || !source.contains("t-linter:") {
        return Ok(());
    }

    let suppressions = collect_suppressions(source)?;
    if suppressions.is_empty() {
        return Ok(());
    }

    diagnostics.retain(|diagnostic| !is_suppressed(diagnostic, &suppressions, templates));
    Ok(())
}

fn collect_suppressions(source: &str) -> Result<Vec<Suppression>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .context("Failed to initialize Python parser")?;
    let Some(tree) = parser.parse(source, None) else {
        return Ok(Vec::new());
    };

    let query = Query::new(&tree_sitter_python::LANGUAGE.into(), "(comment) @comment")
        .context("Failed to create suppression comment query")?;
    let lines = source.lines().collect::<Vec<_>>();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    let mut suppressions = Vec::new();

    while let Some(match_) = matches.next() {
        for capture in match_.captures {
            let node = capture.node;
            let text = node.utf8_text(source.as_bytes())?;
            let Some(rules) = parse_suppression_rules(text) else {
                continue;
            };

            let start = node.start_position();
            let line = start.row + 1;
            let prefix = lines
                .get(start.row)
                .and_then(|line_text| line_text.get(..start.column))
                .unwrap_or_default();

            suppressions.push(Suppression {
                line,
                own_line: prefix.trim().is_empty(),
                rules,
            });
        }
    }

    Ok(suppressions)
}

fn parse_suppression_rules(comment: &str) -> Option<Option<Vec<String>>> {
    let body = comment.trim().strip_prefix('#')?.trim_start();
    let rest = body.strip_prefix("t-linter:")?.trim_start();
    let rest = rest.strip_prefix("ignore")?.trim();
    if rest.is_empty() {
        return Some(None);
    }

    let inner = rest.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return None;
    }

    let mut rules = Vec::new();
    for rule in inner.split(',').map(str::trim) {
        if rule.is_empty()
            || !rule
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        {
            return None;
        }
        rules.push(rule.to_string());
    }
    Some(Some(rules))
}

fn is_suppressed(
    diagnostic: &LintDiagnostic,
    suppressions: &[Suppression],
    templates: &[TemplateStringInfo],
) -> bool {
    if diagnostic.rule == RULE_FILE_READ_ERROR {
        return false;
    }

    if suppressions.iter().any(|suppression| {
        suppression_applies_to_line(suppression, diagnostic.start_line, diagnostic)
    }) {
        return true;
    }

    let Some(template) = templates
        .iter()
        .find(|template| template_contains_line(template, diagnostic.start_line))
    else {
        return false;
    };

    suppressions.iter().any(|suppression| {
        suppression_applies_to_line(suppression, template.location.start_line, diagnostic)
    })
}

fn suppression_applies_to_line(
    suppression: &Suppression,
    line: usize,
    diagnostic: &LintDiagnostic,
) -> bool {
    let line_matches = match suppression.own_line {
        true => suppression.line.checked_add(1) == Some(line),
        false => suppression.line == line,
    };

    line_matches && suppression_matches_rule(suppression, &diagnostic.rule)
}

fn suppression_matches_rule(suppression: &Suppression, rule: &str) -> bool {
    match &suppression.rules {
        Some(rules) => rules.iter().any(|candidate| candidate == rule),
        None => true,
    }
}

fn template_contains_line(template: &TemplateStringInfo, line: usize) -> bool {
    (template.location.start_line..=template.location.end_line).contains(&line)
}

fn apply_rule_config(diagnostics: &mut Vec<LintDiagnostic>, config: &ProjectConfig, path: &Path) {
    if diagnostics.is_empty()
        || (config.ignore.is_empty()
            && config.severity.is_empty()
            && config.per_file_ignores.is_empty())
    {
        return;
    }

    let per_file_ignored_rules = matching_per_file_ignored_rules(config, path);
    diagnostics.retain(|diagnostic| {
        if !rule_config_can_ignore(&diagnostic.rule) {
            return true;
        }
        if config.ignore.iter().any(|rule| rule == &diagnostic.rule) {
            return false;
        }
        !per_file_ignored_rules.contains(diagnostic.rule.as_str())
    });

    for diagnostic in diagnostics {
        if let Some(severity) = config.severity.get(&diagnostic.rule) {
            diagnostic.severity = configured_severity(*severity);
        }
    }
}

fn matching_per_file_ignored_rules<'a>(config: &'a ProjectConfig, path: &Path) -> HashSet<&'a str> {
    let mut rules = HashSet::new();
    let Ok(relative_path) = path.strip_prefix(&config.root) else {
        return rules;
    };

    for (pattern, pattern_rules) in &config.per_file_ignores {
        let Ok(glob) = Glob::new(pattern) else {
            continue;
        };
        if glob.compile_matcher().is_match(relative_path) {
            rules.extend(pattern_rules.iter().map(String::as_str));
        }
    }

    rules
}

fn rule_config_can_ignore(rule: &str) -> bool {
    !matches!(rule, RULE_FILE_READ_ERROR | RULE_PYTHON_PARSE_ERROR)
}

fn configured_severity(severity: RuleSeverity) -> LintSeverity {
    match severity {
        RuleSeverity::Error => LintSeverity::Error,
        RuleSeverity::Warning => LintSeverity::Warning,
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
        expected_type: None,
        found_type: None,
        schema_pointer: None,
        source_of_truth: None,
        suggested_edits: Vec::new(),
    }))
}

fn lint_template(
    path: &Path,
    source: &str,
    template: &TemplateStringInfo,
    module_context: &ModuleContext,
    static_spread_analysis: &StaticSpreadAnalysis,
    sql_config: &SqlConfig,
) -> Result<Vec<LintDiagnostic>> {
    let Some(language) = template
        .language
        .as_deref()
        .and_then(normalize_language)
        .map(str::to_string)
    else {
        return Ok(Vec::new());
    };

    #[cfg(not(feature = "sql"))]
    {
        let _ = sql_config;
        if language == "sql" {
            return Ok(Vec::new());
        }
    }

    if let Some(backend) = TemplateBackend::for_language(&language)
        && backend != TemplateBackend::Sql
    {
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

    let mut diagnostics = Vec::new();
    if tree.root_node().has_error() {
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
                expected_type: None,
                found_type: None,
                schema_pointer: None,
                source_of_truth: None,
                suggested_edits: Vec::new(),
            });
        }
    }

    #[cfg(feature = "sql")]
    if language == "sql" && crate::sql::psycopg::is_enabled(sql_config, template) {
        diagnostics.extend(crate::sql::psycopg::lint_rules(
            path,
            template,
            &tree,
            sql_config,
            module_context,
        ));
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
    let result = backend.check_template(&input, template.profile.as_deref());

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
                expected_type: None,
                found_type: None,
                schema_pointer: None,
                source_of_truth: None,
                suggested_edits: Vec::new(),
            }
        })
        .collect::<Vec<_>>();

    sort_and_dedup_diagnostics(&mut diagnostics);
    Ok(diagnostics)
}

#[derive(Debug, Clone)]
struct JsonSchemaBinding {
    model_name: String,
    location: crate::Location,
}

fn lint_json_schema_bindings(
    path: &Path,
    source: &str,
    templates: &[TemplateStringInfo],
    module_context: &ModuleContext,
) -> Result<Vec<LintDiagnostic>> {
    if !source.contains("Json") || !(source.contains("schema") || source.contains("Json[")) {
        return Ok(Vec::new());
    }

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .context("Failed to initialize Python parser")?;
    let Some(tree) = parser.parse(source, None) else {
        return Ok(Vec::new());
    };
    let marker_names = json_marker_names(module_context);
    let type_aliases = collect_json_schema_type_aliases(source, tree.root_node(), module_context)?;
    let mut models = collect_schema_models(source, tree.root_node())?;
    extend_imported_schema_models(path, module_context, &mut models)?;

    let mut diagnostics = Vec::new();
    for template in templates {
        let Some(binding) = json_schema_binding_for_template(
            source,
            tree.root_node(),
            template,
            &marker_names,
            &type_aliases,
        )?
        else {
            continue;
        };
        let Some(model) = models.get(&binding.model_name) else {
            diagnostics.push(binding_unresolved_diagnostic(path, &binding));
            continue;
        };
        let Some(outline) = json_root_outline(template) else {
            continue;
        };
        diagnostics.extend(lint_json_model_against_outline(
            path,
            model,
            &outline,
            &binding.model_name,
        ));
    }

    Ok(diagnostics)
}

fn json_marker_names(module_context: &ModuleContext) -> BTreeSet<String> {
    let mut names = BTreeSet::from(["Json".to_string(), "json_tstring.Json".to_string()]);
    for (local, target) in &module_context.imports {
        if target == "json_tstring.Json" {
            names.insert(local.clone());
        }
    }
    names
}

fn template_marker_languages(module_context: &ModuleContext) -> Vec<(String, String)> {
    let mut markers = json_marker_names(module_context)
        .into_iter()
        .map(|name| (name, "json".to_string()))
        .collect::<BTreeMap<_, _>>();
    for (name, language) in &module_context.template_language_markers {
        markers.insert(name.clone(), language.clone());
    }
    let mut markers = markers.into_iter().collect::<Vec<_>>();
    markers.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    markers
}

fn lint_template_metadata_markers(
    path: &Path,
    source: &str,
    templates: &[TemplateStringInfo],
    module_context: &ModuleContext,
) -> Result<Vec<LintDiagnostic>> {
    if !source.contains("Annotated") || templates.is_empty() {
        return Ok(Vec::new());
    }

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .context("Failed to initialize Python parser")?;
    let Some(tree) = parser.parse(source, None) else {
        return Ok(Vec::new());
    };
    let markers = template_marker_languages(module_context);
    let mut diagnostics = Vec::new();

    for template in templates {
        let Some(template_start) = location_to_byte_offset(
            source,
            template.location.start_line,
            template.location.start_column,
        ) else {
            continue;
        };
        let Some(type_node) = type_annotation_for_template_start(tree.root_node(), template_start)
        else {
            continue;
        };
        let type_text = type_node.utf8_text(source.as_bytes())?;
        let Some(metadata) = annotated_metadata_items(type_text) else {
            continue;
        };

        let string_languages = metadata
            .iter()
            .filter_map(|item| metadata_language_string(item.text).map(|language| (language, item)))
            .collect::<Vec<_>>();
        let marker_languages = metadata
            .iter()
            .filter_map(|item| marker_metadata_language(item.text, &markers))
            .collect::<Vec<_>>();

        if marker_languages.len() > 1 {
            diagnostics.push(metadata_diagnostic(
                path,
                type_node,
                RULE_TEMPLATE_METADATA_CONFLICT,
                LintSeverity::Error,
                "Template metadata must contain at most one language marker.".to_string(),
                Vec::new(),
            ));
            continue;
        }
        let Some(marker_language) = marker_languages.first() else {
            continue;
        };

        let conflicting_language = string_languages
            .iter()
            .map(|(language, _)| language)
            .find(|language| *language != marker_language);
        if let Some(language) = conflicting_language {
            diagnostics.push(metadata_diagnostic(
                path,
                type_node,
                RULE_TEMPLATE_METADATA_CONFLICT,
                LintSeverity::Error,
                format!(
                    "Template metadata declares language '{language}' and marker language '{marker_language}'."
                ),
                Vec::new(),
            ));
        } else if let Some((language, item)) = string_languages
            .iter()
            .find(|(language, _)| language == marker_language)
        {
            diagnostics.push(metadata_diagnostic(
                path,
                type_node,
                RULE_TEMPLATE_METADATA_REDUNDANT_LANGUAGE,
                LintSeverity::Warning,
                format!(
                    "Template metadata redundantly declares language '{language}' alongside a marker."
                ),
                redundant_language_edit(source, type_node, item),
            ));
        }
    }

    Ok(diagnostics)
}

#[derive(Debug)]
struct MetadataItem<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

fn annotated_metadata_items(type_text: &str) -> Option<Vec<MetadataItem<'_>>> {
    let start = find_marker_followed_by(type_text, "Annotated", '[')?;
    let bracket_start = start + "Annotated".len();
    let contents = bracket_contents_at(&type_text[bracket_start..], '[', ']')?;
    let args = split_top_level_type_tokens_with_offsets(contents, ',');
    if args.len() < 2 {
        return None;
    }
    let contents_start = bracket_start + 1;
    Some(
        args.into_iter()
            .skip(1)
            .map(|item| MetadataItem {
                text: item.text,
                start: contents_start + item.start,
                end: contents_start + item.end,
            })
            .collect(),
    )
}

fn metadata_language_string(text: &str) -> Option<String> {
    let language = parse_metadata_string_literal(text.trim())?;
    if template_profile_text(&language).is_some() {
        return None;
    }
    Some(language)
}

fn parse_metadata_string_literal(text: &str) -> Option<String> {
    let mut chars = text.chars();
    let quote = chars.next()?;
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    let suffix = text.strip_prefix(quote)?.strip_suffix(quote)?;
    Some(suffix.to_string())
}

fn template_profile_text(value: &str) -> Option<&str> {
    value
        .strip_prefix("profile:")
        .or_else(|| value.strip_prefix("profile="))
}

fn marker_metadata_language(text: &str, markers: &[(String, String)]) -> Option<String> {
    let text = text.trim();
    markers.iter().find_map(|(marker, language)| {
        if text == marker
            || find_marker_followed_by(text, marker, '(').is_some_and(|index| index == 0)
            || find_marker_followed_by(text, marker, '[').is_some_and(|index| index == 0)
        {
            return Some(language.clone());
        }
        None
    })
}

fn metadata_diagnostic(
    path: &Path,
    node: Node<'_>,
    rule: &str,
    severity: LintSeverity,
    message: String,
    suggested_edits: Vec<DiagnosticEdit>,
) -> LintDiagnostic {
    let location = location_for_node(node);
    LintDiagnostic {
        rule: rule.to_string(),
        severity,
        language: None,
        message,
        file: path.to_path_buf(),
        start_line: location.start_line,
        start_column: location.start_column,
        end_line: location.end_line,
        end_column: location.end_column,
        expected_type: None,
        found_type: None,
        schema_pointer: None,
        source_of_truth: None,
        suggested_edits,
    }
}

fn redundant_language_edit(
    source: &str,
    type_node: Node<'_>,
    item: &MetadataItem<'_>,
) -> Vec<DiagnosticEdit> {
    let type_text = type_node.utf8_text(source.as_bytes()).unwrap_or("");
    let (delete_start, delete_end) = redundant_language_delete_range(type_text, item);
    let Some(location) = location_for_byte_range(
        source,
        type_node.start_byte() + delete_start,
        type_node.start_byte() + delete_end,
    ) else {
        return Vec::new();
    };
    vec![DiagnosticEdit {
        range: DiagnosticEditRange::from_location(&location),
        new_text: String::new(),
    }]
}

fn redundant_language_delete_range(type_text: &str, item: &MetadataItem<'_>) -> (usize, usize) {
    let bytes = type_text.as_bytes();
    let mut end = item.end;
    while end < bytes.len() && bytes[end].is_ascii_whitespace() {
        end += 1;
    }
    if end < bytes.len() && bytes[end] == b',' {
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_whitespace() {
            end += 1;
        }
        return (item.start, end);
    }

    let mut start = item.start;
    while start > 0 && bytes[start - 1].is_ascii_whitespace() {
        start -= 1;
    }
    if let Some(comma) = type_text[..start].rfind(',') {
        return (comma, item.end);
    }
    (item.start, item.end)
}

fn location_for_byte_range(source: &str, start: usize, end: usize) -> Option<crate::Location> {
    if start > end || end > source.len() {
        return None;
    }
    let (start_line, start_column) = byte_position_to_line_column(source, start)?;
    let (end_line, end_column) = byte_position_to_line_column(source, end)?;
    Some(crate::Location {
        start_line,
        start_column,
        end_line,
        end_column,
    })
}

fn byte_position_to_line_column(source: &str, offset: usize) -> Option<(usize, usize)> {
    if offset > source.len() {
        return None;
    }
    let mut line = 1usize;
    let mut line_start = 0usize;
    for (index, ch) in source.char_indices() {
        if index >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = index + 1;
        }
    }
    Some((line, offset - line_start + 1))
}

#[derive(Debug)]
struct TypeToken<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

fn split_top_level_type_tokens_with_offsets(input: &str, separator: char) -> Vec<TypeToken<'_>> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut bracket_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in input.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                _ if ch == active_quote => quote = None,
                _ => {}
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ if ch == separator && bracket_depth == 0 && paren_depth == 0 && brace_depth == 0 => {
                push_type_token(input, start, index, &mut parts);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    push_type_token(input, start, input.len(), &mut parts);
    parts
}

fn push_type_token<'a>(
    input: &'a str,
    raw_start: usize,
    raw_end: usize,
    parts: &mut Vec<TypeToken<'a>>,
) {
    let mut start = raw_start;
    let mut end = raw_end;
    while start < end && input.as_bytes()[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && input.as_bytes()[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    parts.push(TypeToken {
        text: &input[start..end],
        start,
        end,
    });
}

fn collect_json_schema_type_aliases(
    source: &str,
    root: Node<'_>,
    module_context: &ModuleContext,
) -> Result<BTreeMap<String, String>> {
    let mut aliases = BTreeMap::new();
    collect_pep695_json_schema_type_aliases(source, root, &mut aliases)?;
    collect_typing_json_schema_type_aliases(source, root, module_context, &mut aliases)?;
    Ok(aliases)
}

fn collect_pep695_json_schema_type_aliases(
    source: &str,
    root: Node<'_>,
    aliases: &mut BTreeMap<String, String>,
) -> Result<()> {
    let query = Query::new(
        &tree_sitter_python::LANGUAGE.into(),
        r#"
        (type_alias_statement) @type_alias
        "#,
    )
    .context("Failed to create JSON schema type alias query")?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source.as_bytes());

    while let Some(match_) = matches.next() {
        for capture in match_.captures {
            let type_alias_node = capture.node;
            if !is_lint_module_level_statement(type_alias_node) {
                continue;
            }
            let mut cursor = type_alias_node.walk();
            let mut name_node = None;
            let mut value_node = None;

            for child in type_alias_node.children(&mut cursor) {
                if child.kind() == "type" && name_node.is_none() {
                    if child.utf8_text(source.as_bytes()).unwrap_or("") != "type" {
                        name_node = Some(child);
                    }
                } else if child.kind() == "type" && name_node.is_some() {
                    value_node = Some(child);
                }
            }

            let (Some(name), Some(value)) = (name_node, value_node) else {
                continue;
            };
            aliases.insert(
                name.utf8_text(source.as_bytes())?.to_string(),
                value.utf8_text(source.as_bytes())?.to_string(),
            );
        }
    }

    Ok(())
}

fn collect_typing_json_schema_type_aliases(
    source: &str,
    root: Node<'_>,
    module_context: &ModuleContext,
    aliases: &mut BTreeMap<String, String>,
) -> Result<()> {
    let query = Query::new(
        &tree_sitter_python::LANGUAGE.into(),
        r#"
        (assignment
            left: (identifier) @alias_name
            type: (_) @type_annotation
            right: (_) @alias_value)
        "#,
    )
    .context("Failed to create JSON schema typed alias query")?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source.as_bytes());

    while let Some(match_) = matches.next() {
        let mut alias_name = None;
        let mut alias_value = None;
        let mut type_annotation = None;
        for capture in match_.captures {
            match query.capture_names()[capture.index as usize] {
                "alias_name" => alias_name = Some(capture.node),
                "alias_value" => alias_value = Some(capture.node),
                "type_annotation" => type_annotation = Some(capture.node),
                _ => {}
            }
        }
        let (Some(name), Some(value), Some(annotation)) =
            (alias_name, alias_value, type_annotation)
        else {
            continue;
        };
        if !is_lint_module_level_statement(name.parent().unwrap_or(name)) {
            continue;
        }
        if !json_schema_type_annotation_is_type_alias(annotation, source, module_context)? {
            continue;
        }
        aliases.insert(
            name.utf8_text(source.as_bytes())?.to_string(),
            value.utf8_text(source.as_bytes())?.to_string(),
        );
    }

    Ok(())
}

fn json_schema_type_annotation_is_type_alias(
    annotation: Node<'_>,
    source: &str,
    module_context: &ModuleContext,
) -> Result<bool> {
    let raw = annotation.utf8_text(source.as_bytes())?.trim();
    let resolved = expand_lint_imported_name(raw, &module_context.imports);
    Ok(matches!(
        resolved.as_str(),
        "TypeAlias" | "typing.TypeAlias" | "typing_extensions.TypeAlias"
    ))
}

fn expand_lint_imported_name(
    name: &str,
    imports: &std::collections::HashMap<String, String>,
) -> String {
    let Some((first, rest)) = name.split_once('.') else {
        return imports
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string());
    };
    imports
        .get(first)
        .map(|target| format!("{target}.{rest}"))
        .unwrap_or_else(|| name.to_string())
}

fn json_schema_binding_for_template(
    source: &str,
    root: Node<'_>,
    template: &TemplateStringInfo,
    marker_names: &BTreeSet<String>,
    type_aliases: &BTreeMap<String, String>,
) -> Result<Option<JsonSchemaBinding>> {
    let Some(template_start) = location_to_byte_offset(
        source,
        template.location.start_line,
        template.location.start_column,
    ) else {
        return Ok(None);
    };
    let Some(type_node) = type_annotation_for_template_start(root, template_start) else {
        return Ok(None);
    };
    let type_text = type_node.utf8_text(source.as_bytes())?;
    let Some(model_name) =
        json_schema_binding_from_type_text(type_text, marker_names, type_aliases)
    else {
        return Ok(None);
    };
    Ok(Some(JsonSchemaBinding {
        model_name,
        location: location_for_node(type_node),
    }))
}

fn type_annotation_for_template_start(root: Node<'_>, template_start: usize) -> Option<Node<'_>> {
    let mut node = root.descendant_for_byte_range(template_start, template_start + 1)?;
    loop {
        if node.kind() == "assignment" {
            let right = node.child_by_field_name("right")?;
            if right.start_byte() <= template_start && template_start < right.end_byte() {
                return node.child_by_field_name("type");
            }
        }
        node = node.parent()?;
    }
}

fn is_lint_module_level_statement(node: Node<'_>) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "module" => return true,
            "expression_statement" => current = parent,
            _ => return false,
        }
    }
    false
}

fn json_schema_binding_from_type_text(
    type_text: &str,
    marker_names: &BTreeSet<String>,
    type_aliases: &BTreeMap<String, String>,
) -> Option<String> {
    json_schema_binding_from_type_text_inner(
        type_text,
        marker_names,
        type_aliases,
        &mut BTreeSet::new(),
    )
}

fn json_schema_binding_from_type_text_inner(
    type_text: &str,
    marker_names: &BTreeSet<String>,
    type_aliases: &BTreeMap<String, String>,
    seen_aliases: &mut BTreeSet<String>,
) -> Option<String> {
    let markers = {
        let mut markers = marker_names.iter().map(String::as_str).collect::<Vec<_>>();
        markers.sort_by_key(|marker| std::cmp::Reverse(marker.len()));
        markers
    };
    for marker in markers {
        if let Some(model) = generic_binding_arg(type_text, marker) {
            return Some(model.to_string());
        }
        if let Some(model) = marker_call_schema_arg(type_text, marker) {
            return Some(model.to_string());
        }
    }
    let alias_name = type_text.trim();
    if seen_aliases.insert(alias_name.to_string())
        && let Some(alias_text) = type_aliases.get(alias_name)
    {
        return json_schema_binding_from_type_text_inner(
            alias_text,
            marker_names,
            type_aliases,
            seen_aliases,
        );
    }
    None
}

fn generic_binding_arg<'a>(type_text: &'a str, marker: &str) -> Option<&'a str> {
    let start = find_marker_followed_by(type_text, marker, '[')?;
    let args = bracket_contents_at(&type_text[start + marker.len()..], '[', ']')?;
    split_top_level_type_tokens(args, ',')
        .first()
        .map(|arg| arg.trim())
        .filter(|arg| !arg.is_empty())
}

fn marker_call_schema_arg<'a>(type_text: &'a str, marker: &str) -> Option<&'a str> {
    let start = find_marker_followed_by(type_text, marker, '(')?;
    let args = bracket_contents_at(&type_text[start + marker.len()..], '(', ')')?;
    split_top_level_type_tokens(args, ',')
        .into_iter()
        .filter_map(|arg| arg.split_once('='))
        .find_map(|(name, value)| (name.trim() == "schema").then(|| value.trim()))
        .filter(|arg| !arg.is_empty())
}

fn find_marker_followed_by(type_text: &str, marker: &str, next: char) -> Option<usize> {
    let mut search_start = 0usize;
    while let Some(relative) = type_text[search_start..].find(marker) {
        let start = search_start + relative;
        let end = start + marker.len();
        let before_ok = start == 0
            || !type_text[..start]
                .chars()
                .next_back()
                .is_some_and(is_identifier_continue);
        let after_ok = type_text[end..].starts_with(next);
        if before_ok && after_ok {
            return Some(start);
        }
        search_start = end;
    }
    None
}

fn bracket_contents_at(text: &str, open: char, close: char) -> Option<&str> {
    if !text.starts_with(open) {
        return None;
    }
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                _ if ch == active_quote => quote = None,
                _ => {}
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            _ if ch == open => depth += 1,
            _ if ch == close => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&text[1..index]);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_type_tokens(input: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut bracket_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in input.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                _ if ch == active_quote => quote = None,
                _ => {}
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            _ if ch == separator && bracket_depth == 0 && paren_depth == 0 => {
                parts.push(input[start..index].trim());
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(input[start..].trim());
    parts
}

fn collect_schema_models(
    source: &str,
    root: Node<'_>,
) -> Result<std::collections::BTreeMap<String, SchemaModel>> {
    let query = Query::new(
        &tree_sitter_python::LANGUAGE.into(),
        r#"
        (class_definition
            name: (identifier) @name
            body: (block) @body) @class
        "#,
    )
    .context("Failed to create schema model query")?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source.as_bytes());
    let mut models = std::collections::BTreeMap::new();

    while let Some(match_) = matches.next() {
        let mut class_node = None;
        let mut name_node = None;
        let mut body_node = None;
        for capture in match_.captures {
            match query.capture_names()[capture.index as usize] {
                "class" => class_node = Some(capture.node),
                "name" => name_node = Some(capture.node),
                "body" => body_node = Some(capture.node),
                _ => {}
            }
        }
        let (Some(class_node), Some(name_node), Some(body_node)) =
            (class_node, name_node, body_node)
        else {
            continue;
        };
        let header = &source[class_node.start_byte()..body_node.start_byte()];
        let decorated_prefix = class_node
            .parent()
            .filter(|parent| parent.kind() == "decorated_definition")
            .map(|parent| &source[parent.start_byte()..class_node.start_byte()])
            .unwrap_or("");
        let is_typed_dict = header.contains("TypedDict");
        let is_dataclass = decorated_prefix.contains("dataclass");
        if !is_typed_dict && !is_dataclass {
            continue;
        }
        let total = !header.contains("total=False");
        let name = name_node.utf8_text(source.as_bytes())?.to_string();
        let body = body_node.utf8_text(source.as_bytes())?;
        let model = parse_schema_model_body(body, total, is_dataclass);
        models.insert(name, model);
    }
    Ok(models)
}

fn extend_imported_schema_models(
    path: &Path,
    module_context: &ModuleContext,
    models: &mut std::collections::BTreeMap<String, SchemaModel>,
) -> Result<()> {
    let Some(root) = path.parent() else {
        return Ok(());
    };
    for target in module_context.imports.values() {
        let Some((module, symbol)) = target.rsplit_once('.') else {
            continue;
        };
        if models.contains_key(symbol) {
            continue;
        }
        let Some(module_path) = imported_module_file(root, module) else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(&module_path) else {
            continue;
        };
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .context("Failed to initialize Python parser")?;
        let Some(tree) = parser.parse(&source, None) else {
            continue;
        };
        let imported = collect_schema_models(&source, tree.root_node())?;
        if let Some(model) = imported.get(symbol).cloned() {
            models.insert(symbol.to_string(), model.clone());
            models.insert(format!("{module}.{symbol}"), model);
        }
    }
    Ok(())
}

fn imported_module_file(root: &Path, module: &str) -> Option<PathBuf> {
    let relative = module
        .split('.')
        .fold(PathBuf::new(), |path, part| path.join(part));
    let file = root.join(&relative).with_extension("py");
    if file.is_file() {
        return Some(file);
    }
    let init = root.join(relative).join("__init__.py");
    init.is_file().then_some(init)
}

fn parse_schema_model_body(body: &str, total: bool, is_dataclass: bool) -> SchemaModel {
    let mut fields = std::collections::BTreeMap::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "pass" {
            continue;
        }
        let Some((name, mut type_text)) = trimmed.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if !is_identifier_expression(name) {
            continue;
        }
        type_text = type_text.split('#').next().unwrap_or(type_text).trim();
        let has_default = type_text.contains('=');
        type_text = type_text.split('=').next().unwrap_or(type_text).trim();
        let (required_override, inner_type) = requiredness_from_type_text(type_text);
        fields.insert(
            name.to_string(),
            SchemaField {
                required: required_override.unwrap_or(if is_dataclass {
                    !has_default
                } else {
                    total
                }),
                scalar: json_scalar_from_type(inner_type),
            },
        );
    }
    SchemaModel { fields }
}

fn requiredness_from_type_text(type_text: &str) -> (Option<bool>, &str) {
    for (prefix, required) in [("Required[", true), ("typing.Required[", true)] {
        if let Some(inner) = type_text
            .strip_prefix(prefix)
            .and_then(|text| text.strip_suffix(']'))
        {
            return (Some(required), inner.trim());
        }
    }
    for (prefix, required) in [("NotRequired[", false), ("typing.NotRequired[", false)] {
        if let Some(inner) = type_text
            .strip_prefix(prefix)
            .and_then(|text| text.strip_suffix(']'))
        {
            return (Some(required), inner.trim());
        }
    }
    (None, type_text)
}

fn json_scalar_from_type(type_text: &str) -> Option<JsonScalarKind> {
    let text = type_text.trim();
    match text {
        "int" | "builtins.int" => Some(JsonScalarKind::Integer),
        "float" | "builtins.float" => Some(JsonScalarKind::Number),
        "str" | "builtins.str" => Some(JsonScalarKind::String),
        "bool" | "builtins.bool" => Some(JsonScalarKind::Boolean),
        "None" | "NoneType" => Some(JsonScalarKind::Null),
        _ if text.starts_with("list[") || text.starts_with("typing.List[") => {
            Some(JsonScalarKind::Array)
        }
        _ if text.starts_with("dict[") || text.starts_with("typing.Dict[") => {
            Some(JsonScalarKind::Object)
        }
        _ => None,
    }
}

fn json_root_outline(template: &TemplateStringInfo) -> Option<JsonObjectOutline> {
    let processed = prepare_template_for_lint(template, "json");
    let tree = parse_embedded("json", &processed.content).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let root = tree.root_node().named_child(0)?;
    (root.kind() == "object").then(|| collect_json_object_outline(template, &processed, root, ""))
}

fn collect_json_object_outline(
    template: &TemplateStringInfo,
    processed: &ProcessedTemplate,
    object: Node<'_>,
    pointer: &str,
) -> JsonObjectOutline {
    let interpolation_ranges = interpolation_content_ranges(template);
    let mut outline = JsonObjectOutline {
        pointer: pointer.to_string(),
        ..JsonObjectOutline::default()
    };
    let mut cursor = object.walk();
    for child in object.named_children(&mut cursor) {
        if child.kind() != "pair" {
            continue;
        }
        let Some(key_node) = child.child_by_field_name("key") else {
            continue;
        };
        let Some(value_node) = child.child_by_field_name("value") else {
            continue;
        };
        let key_original_range = processed_original_range(processed, key_node);
        if ranges_overlap_usize(&key_original_range, &interpolation_ranges) {
            outline.has_interpolation_key = true;
            continue;
        }
        let Ok(key_text) = key_node.utf8_text(processed.content.as_bytes()) else {
            continue;
        };
        let Ok(key) = serde_json::from_str::<String>(key_text) else {
            continue;
        };
        let value_original_range = processed_original_range(processed, value_node);
        let value_kind = if ranges_overlap_usize(&value_original_range, &interpolation_ranges) {
            None
        } else {
            json_scalar_from_value_node(value_node, &processed.content)
        };
        outline.static_keys.insert(
            key,
            JsonKeyOutline {
                key_location: processed_node_location(template, processed, key_node),
                value_location: processed_node_location(template, processed, value_node),
                value_kind,
            },
        );
    }
    outline
}

fn interpolation_content_ranges(template: &TemplateStringInfo) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    for part in &template.parts {
        match part {
            TemplatePart::Static(part) => offset += part.text.len(),
            TemplatePart::Interpolation(interpolation) => {
                if let Some(debug_prefix) = &interpolation.debug_prefix {
                    offset += debug_prefix.len();
                }
                ranges.push(offset..offset + 2);
                offset += 2;
            }
        }
    }
    ranges
}

fn processed_original_range(
    processed: &ProcessedTemplate,
    node: Node<'_>,
) -> std::ops::Range<usize> {
    map_processed_offset(&processed.processed_to_original, node.start_byte())
        ..map_processed_offset(&processed.processed_to_original, node.end_byte())
}

fn processed_node_location(
    template: &TemplateStringInfo,
    processed: &ProcessedTemplate,
    node: Node<'_>,
) -> crate::Location {
    let range = processed_original_range(processed, node);
    let ((start_line, start_column), (end_line, end_column)) =
        template.map_content_range_to_document(range.start, range.end);
    crate::Location {
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

fn ranges_overlap_usize(
    range: &std::ops::Range<usize>,
    candidates: &[std::ops::Range<usize>],
) -> bool {
    candidates
        .iter()
        .any(|candidate| range.start < candidate.end && candidate.start < range.end)
}

fn json_scalar_from_value_node(node: Node<'_>, source: &str) -> Option<JsonScalarKind> {
    match node.kind() {
        "string" => Some(JsonScalarKind::String),
        "number" => {
            let text = node.utf8_text(source.as_bytes()).ok()?;
            if text.contains(['.', 'e', 'E']) {
                Some(JsonScalarKind::Number)
            } else {
                Some(JsonScalarKind::Integer)
            }
        }
        "true" | "false" => Some(JsonScalarKind::Boolean),
        "null" => Some(JsonScalarKind::Null),
        "array" => Some(JsonScalarKind::Array),
        "object" => Some(JsonScalarKind::Object),
        _ => None,
    }
}

fn lint_json_model_against_outline(
    path: &Path,
    model: &SchemaModel,
    outline: &JsonObjectOutline,
    source_of_truth: &str,
) -> Vec<LintDiagnostic> {
    let mut diagnostics = Vec::new();
    if !outline.has_interpolation_key {
        for (field_name, field) in &model.fields {
            if field.required && !outline.static_keys.contains_key(field_name) {
                diagnostics.push(schema_diagnostic(
                    path,
                    RULE_TEMPLATE_SCHEMA_MISSING_KEY,
                    format!("JSON template is missing required key '{field_name}'."),
                    None,
                    None,
                    Some(json_pointer_child(&outline.pointer, field_name)),
                    source_of_truth,
                    outline_location(outline),
                    Vec::new(),
                ));
            }
        }
    }

    for (key, key_outline) in &outline.static_keys {
        let Some(field) = model.fields.get(key) else {
            let suggestion = closest_key(key, model.fields.keys().map(String::as_str));
            let edits = suggestion
                .and_then(|suggestion| serde_json::to_string(suggestion).ok())
                .map(|new_text| DiagnosticEdit {
                    range: DiagnosticEditRange::from_location(&key_outline.key_location),
                    new_text,
                })
                .into_iter()
                .collect();
            diagnostics.push(schema_diagnostic(
                path,
                RULE_TEMPLATE_SCHEMA_UNKNOWN_KEY,
                format!("JSON template key '{key}' is not present in schema '{source_of_truth}'."),
                None,
                None,
                Some(json_pointer_child(&outline.pointer, key)),
                source_of_truth,
                key_outline.key_location.clone(),
                edits,
            ));
            continue;
        };
        if let (Some(expected), Some(found)) = (field.scalar, key_outline.value_kind)
            && json_type_shape_mismatch(expected, found)
        {
            diagnostics.push(schema_diagnostic(
                path,
                RULE_TEMPLATE_SCHEMA_TYPE_SHAPE,
                format!(
                    "JSON template key '{key}' has static {found} value but schema '{source_of_truth}' expects {expected}."
                ),
                Some(expected.to_string()),
                Some(found.to_string()),
                Some(json_pointer_child(&outline.pointer, key)),
                source_of_truth,
                key_outline.value_location.clone(),
                Vec::new(),
            ));
        }
    }
    diagnostics
}

fn outline_location(outline: &JsonObjectOutline) -> crate::Location {
    outline
        .static_keys
        .values()
        .next()
        .map(|key| key.key_location.clone())
        .unwrap_or(crate::Location {
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
        })
}

fn binding_unresolved_diagnostic(path: &Path, binding: &JsonSchemaBinding) -> LintDiagnostic {
    schema_diagnostic(
        path,
        RULE_BINDING_UNRESOLVED,
        format!(
            "Could not resolve JSON schema binding '{}'.",
            binding.model_name
        ),
        None,
        None,
        None,
        &binding.model_name,
        binding.location.clone(),
        Vec::new(),
    )
}

fn schema_diagnostic(
    path: &Path,
    rule: &str,
    message: String,
    expected_type: Option<String>,
    found_type: Option<String>,
    schema_pointer: Option<String>,
    source_of_truth: &str,
    location: crate::Location,
    suggested_edits: Vec<DiagnosticEdit>,
) -> LintDiagnostic {
    LintDiagnostic {
        rule: rule.to_string(),
        severity: LintSeverity::Error,
        language: Some("json".to_string()),
        message,
        file: path.to_path_buf(),
        start_line: location.start_line,
        start_column: location.start_column,
        end_line: location.end_line,
        end_column: location.end_column,
        expected_type,
        found_type,
        schema_pointer,
        source_of_truth: Some(source_of_truth.to_string()),
        suggested_edits,
    }
}

fn json_type_shape_mismatch(expected: JsonScalarKind, found: JsonScalarKind) -> bool {
    !matches!(
        (expected, found),
        (
            JsonScalarKind::Number,
            JsonScalarKind::Number | JsonScalarKind::Integer
        ) | (JsonScalarKind::Integer, JsonScalarKind::Integer)
            | (JsonScalarKind::String, JsonScalarKind::String)
            | (JsonScalarKind::Boolean, JsonScalarKind::Boolean)
            | (JsonScalarKind::Null, JsonScalarKind::Null)
            | (JsonScalarKind::Array, JsonScalarKind::Array)
            | (JsonScalarKind::Object, JsonScalarKind::Object)
    )
}

fn json_pointer_child(parent: &str, key: &str) -> String {
    let escaped = key.replace('~', "~0").replace('/', "~1");
    match parent {
        "" => format!("/{escaped}"),
        _ => format!("{parent}/{escaped}"),
    }
}

fn closest_key<'a>(key: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    candidates
        .map(|candidate| (levenshtein(key, candidate), candidate))
        .min_by_key(|(distance, _)| *distance)
        .and_then(|(distance, candidate)| (distance <= 3).then_some(candidate))
}

fn levenshtein(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.chars().count()).collect::<Vec<_>>();
    let mut current = vec![0; previous.len()];
    for (left_index, left_ch) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_ch) in right.chars().enumerate() {
            let replace_cost = usize::from(left_ch != right_ch);
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + replace_cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.chars().count()]
}

fn location_for_node(node: Node<'_>) -> crate::Location {
    crate::Location {
        start_line: node.start_position().row + 1,
        start_column: node.start_position().column + 1,
        end_line: node.end_position().row + 1,
        end_column: node.end_position().column + 1,
    }
}

fn is_identifier_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

impl std::fmt::Display for JsonScalarKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Integer => "integer",
            Self::Number => "number",
            Self::String => "string",
            Self::Boolean => "boolean",
            Self::Null => "null",
            Self::Array => "array",
            Self::Object => "object",
        })
    }
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
        "sql" => return Err(anyhow::anyhow!("Unsupported language: sql")),
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
    let mut original_offset = 0;

    for part in &template.parts {
        match part {
            TemplatePart::Static(part) => {
                append_original_segment(
                    &mut content,
                    &mut processed_to_original,
                    &part.text,
                    original_offset,
                );
                original_offset += part.text.len();
            }
            TemplatePart::Interpolation(part) => {
                if let Some(debug_prefix) = &part.debug_prefix {
                    append_original_segment(
                        &mut content,
                        &mut processed_to_original,
                        debug_prefix,
                        original_offset,
                    );
                    original_offset += debug_prefix.len();
                }
                append_placeholder_segment(
                    &mut content,
                    &mut processed_to_original,
                    placeholder,
                    original_offset,
                    original_offset + 2,
                );
                original_offset += 2;
            }
        }
    }

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
    template.map_content_range_to_document(start_offset, end_offset)
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
        resolve_component_signature(module_context, &component.start_tag.expression)
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
        if prop_name == "children" {
            diagnostics.push(make_component_diagnostic(
                path,
                template,
                "tdom",
                RULE_COMPONENT_UNEXPECTED_PROP,
                format!(
                    "Component '{}' cannot receive prop 'children' because tdom reserves it for component children.",
                    component.start_tag.expression
                ),
                resolved_prop
                    .span
                    .as_ref()
                    .or(component.start_tag.span.as_ref())
                    .or(component.span.as_ref()),
            ));
            continue;
        }

        match signature
            .parameters
            .iter()
            .find(|parameter| parameter.name == *prop_name && parameter.allows_keyword)
        {
            Some(parameter) => {
                if let Some(diagnostic) = lint_tdom_component_attribute_type(
                    path,
                    template,
                    component,
                    resolved_prop,
                    parameter,
                ) {
                    diagnostics.push(diagnostic);
                }
            }
            None if signature.accepts_kwargs => {}
            None => diagnostics.push(make_component_diagnostic(
                path,
                template,
                "tdom",
                RULE_COMPONENT_UNEXPECTED_PROP,
                format!(
                    "Component '{}' does not accept prop '{}'.",
                    component.start_tag.expression, prop_name
                ),
                resolved_prop
                    .span
                    .as_ref()
                    .or(component.start_tag.span.as_ref())
                    .or(component.span.as_ref()),
            )),
        }
    }

    for parameter in &signature.parameters {
        if !parameter.required || parameter.name == "children" {
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

fn normalize_tdom_prop_name(name: &str) -> String {
    backend_tdom::normalize_component_prop_name(name).into_owned()
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
        expected_type: None,
        found_type: None,
        schema_pointer: None,
        source_of_truth: None,
        suggested_edits: Vec::new(),
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

    let mut line_start = 0usize;
    for current_line in 1..line {
        let next_newline = source[line_start..].find('\n')?;
        line_start += next_newline + 1;
        if current_line + 1 == line {
            break;
        }
    }

    let offset = line_start.checked_add(column - 1)?;
    if offset > source.len() {
        return None;
    }
    Some(offset)
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
        #[cfg_attr(not(feature = "sql"), allow(unused_mut))]
        let mut valid_cases = vec![
            ("html", "<div>{}</div>"),
            ("thtml", "<Card title=\"{}\"><Badge>{}</Badge></Card>"),
            ("tdom", "<{} title={}><span>{}</span></{}>"),
            ("css", "body { color: {}; }"),
            ("javascript", "const value = {};"),
            ("json", r#"{"name": {}, "enabled": true}"#),
            ("yaml", "name: {}\nenabled: true\n"),
            ("toml", "name = {}\nenabled = true\n"),
        ];
        #[cfg(feature = "sql")]
        valid_cases.push(("sql", "SELECT * FROM users WHERE id = {}"));

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
    fn literal_brace_pairs_are_not_treated_as_interpolation_placeholders() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

value: Annotated[Template, "json"] = t'{{"literal": "{{}}", "value": {value}}}'
"#;

        let result = lint_source(Path::new("sample.py"), source).unwrap();

        assert!(
            result.diagnostics.is_empty(),
            "expected no diagnostics, got {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn invalid_templates_report_embedded_parse_errors() {
        #[cfg_attr(not(feature = "sql"), allow(unused_mut))]
        let mut invalid_cases = vec![
            ("html", "<div><"),
            ("thtml", "<Card><"),
            ("tdom", "<{}></div>"),
            ("css", "body { color: ; }"),
            ("javascript", "function {"),
            ("json", "[1,,2]"),
            ("yaml", "name: [1, 2\n"),
            ("toml", "title =\n"),
        ];
        #[cfg(feature = "sql")]
        invalid_cases.push(("sql", "SELECT * FROM"));

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

    #[test]
    fn type_token_splitter_ignores_commas_inside_braces() {
        let tokens = split_top_level_type_tokens_with_offsets(
            r#"Template, {"dialect": "json", "strict": True}, "json", Json"#,
            ',',
        );
        let texts = tokens
            .into_iter()
            .map(|token| token.text)
            .collect::<Vec<_>>();

        assert_eq!(
            texts,
            vec![
                "Template",
                r#"{"dialect": "json", "strict": True}"#,
                r#""json""#,
                "Json"
            ]
        );
    }
}
