use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;
use tree_sitter::{Node, Tree};
use tstring_syntax::InterpolationTypeRequirement;

use super::catalog::{CachedSqlCatalog, SqlCatalogQuery};
use crate::lint::{DiagnosticEdit, DiagnosticEditRange, LintDiagnostic, LintSeverity};
use crate::parser::ModuleContext;
use crate::project_config::SqlConfig;
use crate::{TemplatePart, TemplateStringInfo};

pub const TESTED_PSYCOPG_VERSION_RANGE: &str = "3.3.0..=3.3";

const RULE_CONVERSION_UNSUPPORTED: &str = "sql-conversion-unsupported";
const RULE_FORMAT_SPEC_UNKNOWN: &str = "sql-format-spec-unknown";
const RULE_COMPOSABLE_SPEC_MISMATCH: &str = "sql-composable-spec-mismatch";
const RULE_DICT_NEEDS_JSON_WRAPPER: &str = "sql-dict-needs-json-wrapper";
const RULE_IN_CLAUSE: &str = "sql-in-clause";
const RULE_MULTI_STATEMENT: &str = "sql-multi-statement";
const RULE_TUPLE_PARAMETER: &str = "sql-tuple-parameter";
const PSYCOPG_TYPE_MAP: &str = include_str!("manifests/psycopg.tmap.toml");
const TOP_SPEC_KEY: &str = "top";

#[derive(Debug, Deserialize)]
struct PsycopgTypeMap {
    #[serde(rename = "param-accepts")]
    param_accepts: std::collections::BTreeMap<String, TypeMapEntry>,
    #[serde(rename = "spec-accepts")]
    spec_accepts: std::collections::BTreeMap<String, TypeMapEntry>,
}

#[derive(Debug, Deserialize)]
struct TypeMapEntry {
    #[serde(rename = "type")]
    python_type: String,
    #[allow(dead_code)]
    imports: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy)]
enum PartRef<'a> {
    Static(&'a str),
    Interpolation(&'a crate::parser::InterpolationInfo),
}

pub fn is_enabled(config: &SqlConfig, template: &TemplateStringInfo) -> bool {
    match template.library.as_deref().or(config.library.as_deref()) {
        Some(library) => library.eq_ignore_ascii_case("psycopg"),
        None => false,
    }
}

pub fn lint_rules(
    path: &Path,
    template: &TemplateStringInfo,
    tree: &Tree,
    _config: &SqlConfig,
    module_context: &ModuleContext,
) -> Vec<LintDiagnostic> {
    let mut diagnostics = Vec::new();
    let indexed = indexed_parts(template);

    for (index, part) in indexed.iter().enumerate() {
        let PartRef::Interpolation(interpolation) = part.part else {
            continue;
        };
        let spec = interpolation.format_spec.trim();

        if let Some(conversion) = interpolation.conversion.as_deref() {
            diagnostics.push(conversion_unsupported(path, interpolation, conversion));
        }

        if !spec.is_empty() && !is_known_format_spec(spec) {
            diagnostics.push(format_spec_unknown(path, interpolation, spec));
        }

        let expression_target =
            resolve_expression_target(&interpolation.expression, module_context);
        if let Some(expected_spec) = composable_expected_spec(expression_target.as_deref())
            && spec != expected_spec
        {
            diagnostics.push(composable_spec_mismatch(
                path,
                interpolation,
                expected_spec,
                composable_found_type(expression_target.as_deref()),
            ));
        }

        if is_dict_literal(&interpolation.expression) && !matches!(spec, "i" | "l" | "q") {
            diagnostics.push(dict_needs_json_wrapper(path, interpolation));
        }

        if is_tuple_literal(&interpolation.expression) && !matches!(spec, "i" | "l" | "q") {
            diagnostics.push(tuple_parameter(path, interpolation));
        }

        if is_plain_parameter_spec(spec)
            && let Some((previous, next)) = in_clause_static_neighbors(&indexed, index)
        {
            diagnostics.push(in_clause(path, template, previous, interpolation, next));
        }
    }

    if !tree.root_node().has_error() && statement_count(tree.root_node()) > 1 {
        diagnostics.push(multi_statement(path, template));
    }

    diagnostics
}

#[allow(dead_code)]
pub fn interpolation_type_requirements(
    template: &TemplateStringInfo,
    config: &SqlConfig,
) -> Vec<InterpolationTypeRequirement> {
    interpolation_type_requirements_with_catalog(template, config, None)
}

pub fn interpolation_type_requirements_with_catalog(
    template: &TemplateStringInfo,
    config: &SqlConfig,
    catalog: Option<(&SqlCatalogQuery, &CachedSqlCatalog)>,
) -> Vec<InterpolationTypeRequirement> {
    let mut requirements = Vec::new();
    let mut server_param_index = 0;
    for part in &template.parts {
        let TemplatePart::Interpolation(interpolation) = part else {
            continue;
        };
        let spec = interpolation.format_spec.trim();
        let catalog_type = if is_catalog_parameter_spec(spec) {
            let current_index = server_param_index;
            server_param_index += 1;
            catalog
                .and_then(|(query, entry)| {
                    query
                        .parameter_interpolation_indices
                        .get(current_index)
                        .filter(|interpolation_index| {
                            **interpolation_index == interpolation.interpolation_index
                        })
                        .and_then(|_| entry.params.get(current_index))
                })
                .map(|param| (current_index + 1, param.type_name.as_str()))
        } else {
            None
        };
        let expected_type = catalog_type
            .map(|(_, type_name)| expected_type_for_pg_type(type_name))
            .unwrap_or_else(|| expected_type_for_spec(spec, config));
        let expected_description = if let Some((index, type_name)) = catalog_type {
            format!("PostgreSQL parameter {index} ({type_name})")
        } else {
            match spec {
                "i" => "psycopg format spec ':i'",
                "q" => "psycopg format spec ':q'",
                "l" => "psycopg format spec ':l'",
                "s" => "psycopg format spec ':s'",
                "b" => "psycopg format spec ':b'",
                "t" => "psycopg format spec ':t'",
                _ => "psycopg SQL parameter",
            }
            .to_string()
        };
        requirements.push(InterpolationTypeRequirement::new(
            interpolation.interpolation_index,
            expected_type,
            expected_description,
        ));
    }
    requirements
}

fn expected_type_for_spec(spec: &str, config: &SqlConfig) -> String {
    let entry_key = match spec {
        "i" | "q" => spec,
        "s" | "b" | "t" | "l" => spec,
        _ => TOP_SPEC_KEY,
    };
    let mut expected_type = type_map_entry(entry_key)
        .or_else(|| type_map_entry(TOP_SPEC_KEY))
        .map(|entry| entry.python_type.clone())
        .unwrap_or_else(|| "object".to_string());
    if entry_key != "i" && entry_key != "q" {
        extend_union(&mut expected_type, &config.extra_param_types);
    }
    expected_type
}

fn type_map_entry(key: &str) -> Option<&'static TypeMapEntry> {
    type_map().and_then(|type_map| type_map.spec_accepts.get(key))
}

fn param_type_map_entry(type_name: &str) -> Option<&'static TypeMapEntry> {
    let normalized = normalize_pg_type_name(type_name);
    type_map().and_then(|type_map| {
        type_map
            .param_accepts
            .iter()
            .find(|(key, _)| param_key_matches(key, &normalized))
            .map(|(_, entry)| entry)
            .or_else(|| type_map.param_accepts.get("default"))
    })
}

fn type_map() -> Option<&'static PsycopgTypeMap> {
    static TYPE_MAP: OnceLock<Option<PsycopgTypeMap>> = OnceLock::new();
    TYPE_MAP
        .get_or_init(|| toml::from_str(PSYCOPG_TYPE_MAP).ok())
        .as_ref()
}

fn expected_type_for_pg_type(type_name: &str) -> String {
    param_type_map_entry(type_name)
        .map(|entry| entry.python_type.clone())
        .unwrap_or_else(|| "object".to_string())
}

fn normalize_pg_type_name(type_name: &str) -> String {
    type_name
        .trim()
        .trim_matches('"')
        .rsplit('.')
        .next()
        .unwrap_or(type_name)
        .to_ascii_lowercase()
}

fn param_key_matches(key: &str, type_name: &str) -> bool {
    if key == "default" {
        return false;
    }
    if key == "_*" {
        return type_name.starts_with('_');
    }
    key.split('|').any(|part| part.trim() == type_name)
}

fn is_catalog_parameter_spec(spec: &str) -> bool {
    matches!(spec, "" | "s" | "b" | "t")
}

fn extend_union(expected_type: &mut String, extra_param_types: &[String]) {
    for extra_type in extra_param_types.iter().map(|value| value.trim()) {
        if extra_type.is_empty() || union_contains_type(expected_type, extra_type) {
            continue;
        }
        expected_type.push_str(" | ");
        expected_type.push_str(extra_type);
    }
}

fn union_contains_type(union: &str, needle: &str) -> bool {
    union.split('|').map(str::trim).any(|part| part == needle)
}

#[derive(Debug, Clone, Copy)]
struct IndexedPart<'a> {
    part: PartRef<'a>,
    content_start: usize,
}

fn indexed_parts(template: &TemplateStringInfo) -> Vec<IndexedPart<'_>> {
    let mut parts = Vec::with_capacity(template.parts.len());
    let mut offset = 0;
    for part in &template.parts {
        match part {
            TemplatePart::Static(part) => {
                let end = offset + part.text.len();
                parts.push(IndexedPart {
                    part: PartRef::Static(&part.text),
                    content_start: offset,
                });
                offset = end;
            }
            TemplatePart::Interpolation(part) => {
                let end = offset + 2;
                parts.push(IndexedPart {
                    part: PartRef::Interpolation(part),
                    content_start: offset,
                });
                offset = end;
            }
        }
    }
    parts
}

fn is_known_format_spec(spec: &str) -> bool {
    matches!(spec, "s" | "b" | "t" | "i" | "l" | "q")
}

fn is_plain_parameter_spec(spec: &str) -> bool {
    !matches!(spec, "i" | "l" | "q")
}

fn resolve_expression_target(expression: &str, context: &ModuleContext) -> Option<String> {
    let expression = expression.trim();
    if is_nested_template_literal(expression) {
        return Some("string.templatelib.Template".to_string());
    }
    let callee = expression.split_once('(')?.0.trim();
    let root = callee.split('.').next()?;
    let target = context
        .imports
        .get(root)
        .map(|import_target| {
            let suffix = callee.strip_prefix(root).unwrap_or_default();
            format!("{import_target}{suffix}")
        })
        .unwrap_or_else(|| callee.to_string());
    Some(target)
}

fn composable_expected_spec(target: Option<&str>) -> Option<&'static str> {
    let target = target?;
    if matches!(target, "psycopg.sql.Identifier") {
        Some("i")
    } else if matches!(
        target,
        "psycopg.sql.SQL" | "psycopg.sql.Composed" | "string.templatelib.Template"
    ) {
        Some("q")
    } else if matches!(target, "psycopg.sql.Literal") {
        Some("l")
    } else {
        None
    }
}

fn composable_found_type(target: Option<&str>) -> Option<&'static str> {
    match target? {
        "psycopg.sql.Identifier" => Some("psycopg.sql.Identifier"),
        "psycopg.sql.SQL" => Some("psycopg.sql.SQL"),
        "psycopg.sql.Composed" => Some("psycopg.sql.Composed"),
        "psycopg.sql.Literal" => Some("psycopg.sql.Literal"),
        "string.templatelib.Template" => Some("string.templatelib.Template"),
        _ => None,
    }
}

fn is_nested_template_literal(expression: &str) -> bool {
    let expression = expression.trim_start();
    let mut chars = expression.chars().peekable();
    let mut saw_t = false;
    while let Some(ch) = chars.peek().copied() {
        match ch.to_ascii_lowercase() {
            'r' => {
                chars.next();
            }
            't' => {
                saw_t = true;
                chars.next();
            }
            '\'' | '"' => return saw_t,
            _ => return false,
        }
    }
    false
}

fn is_dict_literal(expression: &str) -> bool {
    let expression = expression.trim();
    expression.starts_with('{') && expression.ends_with('}')
}

fn is_tuple_literal(expression: &str) -> bool {
    let expression = expression.trim();
    expression.starts_with('(') && expression.ends_with(')') && expression.contains(',')
}

fn in_clause_static_neighbors<'a>(
    parts: &'a [IndexedPart<'a>],
    interpolation_index: usize,
) -> Option<(IndexedPart<'a>, IndexedPart<'a>)> {
    let previous = previous_static(parts, interpolation_index)?;
    let next = next_static(parts, interpolation_index)?;
    let previous_text = match previous.part {
        PartRef::Static(text) => text,
        PartRef::Interpolation(_) => return None,
    };
    let next_text = match next.part {
        PartRef::Static(text) => text,
        PartRef::Interpolation(_) => return None,
    };

    if static_ends_with_in_open_paren(previous_text) && next_text.trim_start().starts_with(')') {
        return Some((previous, next));
    }
    if static_ends_with_in(previous_text) {
        return Some((previous, next));
    }
    None
}

fn previous_static<'a>(parts: &'a [IndexedPart<'a>], index: usize) -> Option<IndexedPart<'a>> {
    parts[..index]
        .iter()
        .rev()
        .copied()
        .find(|part| matches!(part.part, PartRef::Static(_)))
}

fn next_static<'a>(parts: &'a [IndexedPart<'a>], index: usize) -> Option<IndexedPart<'a>> {
    parts[index + 1..]
        .iter()
        .copied()
        .find(|part| matches!(part.part, PartRef::Static(_)))
}

fn static_ends_with_in_open_paren(text: &str) -> bool {
    let trimmed = text.trim_end();
    let Some(before_paren) = trimmed.strip_suffix('(').map(str::trim_end) else {
        return false;
    };
    static_ends_with_in(before_paren)
}

fn static_ends_with_in(text: &str) -> bool {
    let trimmed = text.trim_end();
    let mut end = trimmed.len();
    while end > 0 && is_identifier_byte(trimmed.as_bytes()[end - 1]) {
        end -= 1;
    }
    let word = &trimmed[end..];
    if !word.eq_ignore_ascii_case("in") {
        return false;
    }
    end == 0 || !is_identifier_byte(trimmed.as_bytes()[end - 1])
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn statement_count(root: Node<'_>) -> usize {
    let mut count = 0;
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.is_named() && !child.has_error() {
            count += 1;
        }
    }
    count
}

fn conversion_unsupported(
    path: &Path,
    interpolation: &crate::parser::InterpolationInfo,
    conversion: &str,
) -> LintDiagnostic {
    let conversion_text = format!("!{conversion}");
    let suggested_edits = interpolation
        .raw_source
        .find(&conversion_text)
        .map(|start| {
            let range =
                interpolation_raw_subrange(interpolation, start, start + conversion_text.len());
            DiagnosticEdit {
                range,
                new_text: String::new(),
            }
        })
        .into_iter()
        .collect();
    interpolation_diagnostic(
        path,
        RULE_CONVERSION_UNSUPPORTED,
        LintSeverity::Error,
        "psycopg rejects conversions in SQL templates (raises TypeError); remove the conversion",
        interpolation,
        None,
        Some(&conversion_text),
        suggested_edits,
    )
}

fn format_spec_unknown(
    path: &Path,
    interpolation: &crate::parser::InterpolationInfo,
    spec: &str,
) -> LintDiagnostic {
    interpolation_diagnostic(
        path,
        RULE_FORMAT_SPEC_UNKNOWN,
        LintSeverity::Error,
        format!("format spec '{spec}' is not supported by psycopg (allowed: s, b, t, i, l, q)"),
        interpolation,
        Some("s | b | t | i | l | q"),
        Some(spec),
        Vec::new(),
    )
}

fn composable_spec_mismatch(
    path: &Path,
    interpolation: &crate::parser::InterpolationInfo,
    expected_spec: &str,
    found_type: Option<&str>,
) -> LintDiagnostic {
    let expected_type = match expected_spec {
        "i" => "format spec ':i'",
        "q" => "format spec ':q'",
        "l" => "format spec ':l'",
        _ => "matching psycopg format spec",
    };
    let mut suggested_edits = Vec::new();
    if let Some(edit) = replace_format_spec_edit(interpolation, expected_spec) {
        suggested_edits.push(edit);
    }
    interpolation_diagnostic(
        path,
        RULE_COMPOSABLE_SPEC_MISMATCH,
        LintSeverity::Error,
        format!(
            "{} requires format spec ':{expected_spec}'",
            found_type.unwrap_or("psycopg composable")
        ),
        interpolation,
        Some(expected_type),
        found_type,
        suggested_edits,
    )
}

fn dict_needs_json_wrapper(
    path: &Path,
    interpolation: &crate::parser::InterpolationInfo,
) -> LintDiagnostic {
    interpolation_diagnostic(
        path,
        RULE_DICT_NEEDS_JSON_WRAPPER,
        LintSeverity::Error,
        "dict is not adapted by psycopg; wrap it in psycopg.types.json.Json or Jsonb",
        interpolation,
        Some("psycopg.types.json.Json | psycopg.types.json.Jsonb"),
        Some("dict"),
        vec![DiagnosticEdit {
            range: location_range(&interpolation.location),
            new_text: format!("Json({})", interpolation.expression),
        }],
    )
}

fn tuple_parameter(
    path: &Path,
    interpolation: &crate::parser::InterpolationInfo,
) -> LintDiagnostic {
    let mut list_expr = interpolation.expression.trim().to_string();
    if list_expr.starts_with('(') && list_expr.ends_with(')') {
        list_expr.replace_range(..1, "[");
        list_expr.replace_range(list_expr.len() - 1.., "]");
    }
    interpolation_diagnostic(
        path,
        RULE_TUPLE_PARAMETER,
        LintSeverity::Warning,
        "tuple is not a general psycopg parameter; use a list",
        interpolation,
        Some("list"),
        Some("tuple"),
        vec![DiagnosticEdit {
            range: location_range(&interpolation.location),
            new_text: list_expr,
        }],
    )
}

fn in_clause(
    path: &Path,
    template: &TemplateStringInfo,
    previous: IndexedPart<'_>,
    interpolation: &crate::parser::InterpolationInfo,
    next: IndexedPart<'_>,
) -> LintDiagnostic {
    let previous_text = match previous.part {
        PartRef::Static(text) => text,
        PartRef::Interpolation(_) => "",
    };
    let next_text = match next.part {
        PartRef::Static(text) => text,
        PartRef::Interpolation(_) => "",
    };
    let mut suggested_edits = Vec::new();
    if let Some(edit) = replace_in_prefix_edit(template, previous, previous_text) {
        suggested_edits.push(edit);
    }
    if let Some(edit) = remove_closing_paren_edit(template, next, next_text) {
        suggested_edits.push(edit);
    }
    interpolation_diagnostic(
        path,
        RULE_IN_CLAUSE,
        LintSeverity::Warning,
        "'IN ({x})' does not work with a list parameter; use '= ANY({x})' with a list",
        interpolation,
        Some("list usable with = ANY(...)"),
        None,
        suggested_edits,
    )
}

fn multi_statement(path: &Path, template: &TemplateStringInfo) -> LintDiagnostic {
    let range = location_range(&template.location);
    LintDiagnostic {
        rule: RULE_MULTI_STATEMENT.to_string(),
        severity: LintSeverity::Warning,
        language: Some("sql".to_string()),
        message:
            "multiple SQL statements in one template cannot be prepared; split into separate execute() calls"
                .to_string(),
        file: path.to_path_buf(),
        start_line: range.start_line,
        start_column: range.start_column,
        end_line: range.end_line,
        end_column: range.end_column,
        expected_type: Some("single SQL statement".to_string()),
        found_type: Some("multiple SQL statements".to_string()),
        schema_pointer: None,
        source_of_truth: Some(psycopg_source_of_truth()),
        suggested_edits: Vec::new(),
    }
}

fn interpolation_diagnostic(
    path: &Path,
    rule: &str,
    severity: LintSeverity,
    message: impl Into<String>,
    interpolation: &crate::parser::InterpolationInfo,
    expected_type: Option<&str>,
    found_type: Option<&str>,
    suggested_edits: Vec<DiagnosticEdit>,
) -> LintDiagnostic {
    LintDiagnostic {
        rule: rule.to_string(),
        severity,
        language: Some("sql".to_string()),
        message: message.into(),
        file: path.to_path_buf(),
        start_line: interpolation.location.start_line,
        start_column: interpolation.location.start_column,
        end_line: interpolation.location.end_line,
        end_column: interpolation.location.end_column,
        expected_type: expected_type.map(str::to_string),
        found_type: found_type.map(str::to_string),
        schema_pointer: None,
        source_of_truth: Some(psycopg_source_of_truth()),
        suggested_edits,
    }
}

fn replace_format_spec_edit(
    interpolation: &crate::parser::InterpolationInfo,
    expected_spec: &str,
) -> Option<DiagnosticEdit> {
    if interpolation.format_spec.is_empty() {
        let insertion = interpolation.raw_source.rfind('}')?;
        return Some(DiagnosticEdit {
            range: interpolation_raw_subrange(interpolation, insertion, insertion),
            new_text: format!(":{expected_spec}"),
        });
    }
    let spec_start = interpolation.raw_source.rfind(':')? + 1;
    let spec_end = interpolation.raw_source.rfind('}')?;
    Some(DiagnosticEdit {
        range: interpolation_raw_subrange(interpolation, spec_start, spec_end),
        new_text: expected_spec.to_string(),
    })
}

fn replace_in_prefix_edit(
    template: &TemplateStringInfo,
    previous: IndexedPart<'_>,
    previous_text: &str,
) -> Option<DiagnosticEdit> {
    let trimmed = previous_text.trim_end();
    let in_start = trimmed
        .to_ascii_lowercase()
        .rfind("in")
        .filter(|start| *start == 0 || !is_identifier_byte(trimmed.as_bytes()[start - 1]))?;
    let prefix_after_in = &trimmed[in_start + 2..];
    let has_open_paren = prefix_after_in.trim_end().ends_with('(');
    let end = if has_open_paren {
        previous.content_start + trimmed.len()
    } else {
        previous.content_start + in_start + 2
    };
    let range = content_subrange(template, previous.content_start + in_start, end);
    Some(DiagnosticEdit {
        range,
        new_text: if has_open_paren {
            "= ANY(".to_string()
        } else {
            "= ANY".to_string()
        },
    })
}

fn remove_closing_paren_edit(
    template: &TemplateStringInfo,
    next: IndexedPart<'_>,
    next_text: &str,
) -> Option<DiagnosticEdit> {
    let leading_ws = next_text.len() - next_text.trim_start().len();
    let start = next.content_start + leading_ws;
    let end = start + 1;
    Some(DiagnosticEdit {
        range: content_subrange(template, start, end),
        new_text: String::new(),
    })
}

fn interpolation_raw_subrange(
    interpolation: &crate::parser::InterpolationInfo,
    start: usize,
    end: usize,
) -> DiagnosticEditRange {
    same_line_range(
        interpolation.location.start_line,
        interpolation.location.start_column + start,
        interpolation.location.start_column + end,
    )
}

fn content_subrange(
    template: &TemplateStringInfo,
    start: usize,
    end: usize,
) -> DiagnosticEditRange {
    let ((start_line, start_column), (end_line, end_column)) =
        template.map_content_range_to_document(start, end);
    DiagnosticEditRange {
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

fn same_line_range(line: usize, start_column: usize, end_column: usize) -> DiagnosticEditRange {
    DiagnosticEditRange {
        start_line: line,
        start_column,
        end_line: line,
        end_column,
    }
}

fn psycopg_source_of_truth() -> String {
    format!("psycopg {TESTED_PSYCOPG_VERSION_RANGE}")
}

fn location_range(location: &crate::parser::Location) -> DiagnosticEditRange {
    DiagnosticEditRange {
        start_line: location.start_line,
        start_column: location.start_column,
        end_line: location.end_line,
        end_column: location.end_column,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TemplateStringParser;

    fn first_psycopg_template(source: &str) -> TemplateStringInfo {
        let mut parser = TemplateStringParser::new().expect("parser");
        parser
            .find_template_strings_in_file(source, Path::new("example.py"))
            .expect("templates")
            .into_iter()
            .find(|template| template.library.as_deref() == Some("psycopg"))
            .expect("psycopg template")
    }

    #[test]
    fn detects_in_clause_context() {
        assert!(static_ends_with_in_open_paren("WHERE id IN ("));
        assert!(static_ends_with_in("WHERE id in "));
        assert!(!static_ends_with_in("WHERE pin "));
    }

    #[test]
    fn classifies_static_literals() {
        assert!(is_dict_literal("{'a': 1}"));
        assert!(is_tuple_literal("(1, 2)"));
        assert!(!is_tuple_literal("(value)"));
    }

    #[test]
    fn type_requirements_follow_psycopg_format_specs() {
        let template = first_psycopg_template(
            r#"import psycopg

conn = psycopg.connect("dbname=app")
cur = conn.cursor()
cur.execute(t"SELECT * FROM {table:i} WHERE id = {value} AND extra = {fragment:q} AND literal = {literal:l}")
"#,
        );

        let requirements = interpolation_type_requirements(&template, &SqlConfig::default());

        assert_eq!(requirements.len(), 4);
        assert_eq!(
            requirements[0].expected_python_type,
            "str | psycopg.sql.Identifier"
        );
        assert_eq!(
            requirements[0].expected_description,
            "psycopg format spec ':i'"
        );
        assert!(requirements[1].expected_python_type.contains("uuid.UUID"));
        assert_eq!(
            requirements[2].expected_python_type,
            "string.templatelib.Template | psycopg.sql.SQL | psycopg.sql.Composed"
        );
        assert_eq!(
            requirements[3].expected_description,
            "psycopg format spec ':l'"
        );
    }

    #[test]
    fn extra_param_types_extend_top_union_once() {
        let template = first_psycopg_template(
            r#"import psycopg

conn = psycopg.connect("dbname=app")
cur = conn.cursor()
cur.execute(t"SELECT * FROM users WHERE money = {money} AND table_name = {table:i}")
"#,
        );
        let config = SqlConfig {
            extra_param_types: vec![
                "myapp.Money".to_string(),
                "myapp.Money".to_string(),
                "uuid.UUID".to_string(),
            ],
            ..SqlConfig::default()
        };

        let requirements = interpolation_type_requirements(&template, &config);

        assert!(
            requirements[0]
                .expected_python_type
                .ends_with(" | myapp.Money")
        );
        assert_eq!(
            requirements[0]
                .expected_python_type
                .matches("myapp.Money")
                .count(),
            1
        );
        assert_eq!(
            requirements[1].expected_python_type,
            "str | psycopg.sql.Identifier"
        );
    }
}
