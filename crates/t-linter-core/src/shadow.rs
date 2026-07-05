use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};
use tstring_tdom as backend_tdom;

use crate::backend::TemplateBackend;
use crate::parser::ModuleContext;
use crate::tdom::{
    ComponentPropExpectedType, expected_type_for_component_prop, resolve_component_signature,
};
use crate::{InterpolationInfo, Location, TemplatePart, TemplateStringInfo, TemplateStringParser};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowCheckSite {
    pub shadow_rhs_byte_range: Range<usize>,
    pub shadow_line: usize,
    pub original_location: Location,
    pub template_index: usize,
    pub interpolation_index: usize,
    pub expected_type: String,
    pub expected_description: String,
    pub expression: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowDocument {
    pub text: String,
    pub sites: Vec<ShadowCheckSite>,
}

#[derive(Debug, Default)]
struct PendingInsertion {
    text: String,
    sites: Vec<PendingSite>,
    datetime_imported: bool,
    type_imports: BTreeSet<ShadowTypeImport>,
    mode: InsertionMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum InsertionMode {
    #[default]
    AfterStatement,
    BeforeStatement,
}

#[derive(Debug, Clone, Copy)]
struct InsertionPoint {
    offset: usize,
    mode: InsertionMode,
}

#[derive(Debug)]
struct PendingSite {
    rhs_relative_range: Range<usize>,
    original_location: Location,
    template_index: usize,
    interpolation_index: usize,
    expected_type: String,
    expected_description: String,
    expression: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ShadowTypeImport {
    module: String,
    alias: String,
}

#[derive(Debug, Default)]
struct TemplateTypeRequirements {
    requirements: Vec<tstring_syntax::InterpolationTypeRequirement>,
    type_imports: Vec<ShadowTypeImport>,
}

impl TemplateTypeRequirements {
    fn is_empty(&self) -> bool {
        self.requirements.is_empty()
    }
}

pub fn synthesize_for_type_check(path: &Path, source: &str) -> Result<Option<ShadowDocument>> {
    let mut template_parser = TemplateStringParser::new()?;
    let templates = template_parser.find_template_strings_in_file(source, path)?;
    let module_context = template_parser.module_context().clone();
    let prefix = available_shadow_prefix(source);
    let requirements_by_template =
        type_requirements_by_template(&templates, &module_context, &prefix);
    if requirements_by_template
        .iter()
        .all(TemplateTypeRequirements::is_empty)
    {
        return Ok(None);
    }

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .context("Failed to set Python language")?;
    let tree = parser
        .parse(source, None)
        .context("Failed to parse source")?;
    let line_starts = line_start_offsets(source);
    let datetime_alias = format!("{prefix}datetime");
    let mut insertions = BTreeMap::<usize, PendingInsertion>::new();

    for ((template_index, template), template_requirements) in templates
        .iter()
        .enumerate()
        .zip(requirements_by_template.into_iter())
    {
        if template_requirements.requirements.is_empty() {
            continue;
        }
        let template_start = location_start_byte(source, &line_starts, &template.location)?;
        let Some(insertion_point) =
            enclosing_simple_statement_insertion(tree.root_node(), template_start, source.len())
        else {
            tracing::debug!(
                "Skipping interpolation type checks outside a simple statement at {}:{}",
                template.location.start_line,
                template.location.start_column
            );
            continue;
        };

        for requirement in template_requirements.requirements {
            let Some(interpolation) =
                interpolation_by_index(template, requirement.interpolation_index)
            else {
                continue;
            };
            if should_skip_interpolation(interpolation, source, &line_starts, tree.root_node())? {
                continue;
            }

            let name = format!(
                "{prefix}{template_index}_{}",
                interpolation.interpolation_index
            );
            let annotation =
                shadow_annotation_type(&requirement.expected_python_type, &datetime_alias);
            let lhs = format!(
                "{name}: {} = ",
                crate::python::double_quoted_string_literal(&annotation)
            );
            debug_assert!(!lhs.contains('\n'));
            debug_assert!(!interpolation.expression.contains('\n'));

            let insertion =
                insertions
                    .entry(insertion_point.offset)
                    .or_insert_with(|| PendingInsertion {
                        mode: insertion_point.mode,
                        ..PendingInsertion::default()
                    });
            if requires_datetime_alias(&requirement.expected_python_type)
                && !insertion.datetime_imported
            {
                push_shadow_statement_prefix(&mut insertion.text, insertion.mode);
                insertion.text.push_str("import datetime as ");
                insertion.text.push_str(&datetime_alias);
                insertion.datetime_imported = true;
            }
            push_shadow_type_imports(insertion, &template_requirements.type_imports);
            push_shadow_statement_prefix(&mut insertion.text, insertion.mode);
            let rhs_start = insertion.text.len() + lhs.len();
            insertion.text.push_str(&lhs);
            insertion.text.push_str(&interpolation.expression);
            let rhs_end = insertion.text.len();
            insertion.sites.push(PendingSite {
                rhs_relative_range: rhs_start..rhs_end,
                original_location: interpolation.location.clone(),
                template_index,
                interpolation_index: interpolation.interpolation_index,
                expected_type: requirement.expected_python_type,
                expected_description: requirement.expected_description.to_string(),
                expression: interpolation.expression.clone(),
            });
        }
    }

    if insertions.is_empty() {
        return Ok(None);
    }

    let inserted_len = insertions
        .values()
        .map(|insertion| insertion.text.len())
        .sum::<usize>();
    let mut text = String::with_capacity(source.len() + inserted_len);
    let mut sites = Vec::new();
    let mut cursor = 0;
    for (offset, insertion) in insertions {
        debug_assert!(!insertion.text.contains('\n'));
        let shadow_line = line_index_for_offset(&line_starts, offset);
        text.push_str(&source[cursor..offset]);
        let insertion_start = text.len();
        text.push_str(&insertion.text);
        if insertion.mode == InsertionMode::BeforeStatement && !insertion.text.is_empty() {
            text.push_str("; ");
        }
        cursor = offset;

        sites.extend(insertion.sites.into_iter().map(|site| ShadowCheckSite {
            shadow_rhs_byte_range: insertion_start + site.rhs_relative_range.start
                ..insertion_start + site.rhs_relative_range.end,
            shadow_line,
            original_location: site.original_location,
            template_index: site.template_index,
            interpolation_index: site.interpolation_index,
            expected_type: site.expected_type,
            expected_description: site.expected_description,
            expression: site.expression,
        }));
    }
    text.push_str(&source[cursor..]);

    debug_assert_eq!(line_count(source), line_count(&text));
    Ok(Some(ShadowDocument { text, sites }))
}

fn type_requirements_by_template(
    templates: &[TemplateStringInfo],
    module_context: &ModuleContext,
    prefix: &str,
) -> Vec<TemplateTypeRequirements> {
    templates
        .iter()
        .enumerate()
        .map(|(template_index, template)| {
            let Some(language) = template.language.as_deref() else {
                return TemplateTypeRequirements::default();
            };
            let Some(backend) = TemplateBackend::for_language(language) else {
                return TemplateTypeRequirements::default();
            };
            match (backend, template.profile.as_deref()) {
                (TemplateBackend::Tdom, profile)
                    if profile.is_none_or(|profile| profile.eq_ignore_ascii_case("svg")) =>
                {
                    requirement_result_or_default(
                        tdom_interpolation_type_requirements(
                            template,
                            module_context,
                            prefix,
                            template_index,
                        ),
                        language,
                        template,
                    )
                }
                _ => backend.interpolation_type_requirements(
                    &template.to_template_input(),
                    template.profile.as_deref(),
                )
                .map(|requirements| TemplateTypeRequirements {
                    requirements,
                    type_imports: Vec::new(),
                })
                .map_or_else(
                    |error| {
                        tracing::debug!(
                            "Skipping interpolation type requirements for {} template at {}:{}: {}",
                            language,
                            template.location.start_line,
                            template.location.start_column,
                            error.message
                        );
                        TemplateTypeRequirements::default()
                    },
                    |requirements| requirements,
                ),
            }
        })
        .collect()
}

fn requirement_result_or_default(
    result: tstring_syntax::BackendResult<TemplateTypeRequirements>,
    language: &str,
    template: &TemplateStringInfo,
) -> TemplateTypeRequirements {
    result.unwrap_or_else(|error| {
        tracing::debug!(
            "Skipping interpolation type requirements for {} template at {}:{}: {}",
            language,
            template.location.start_line,
            template.location.start_column,
            error.message
        );
        TemplateTypeRequirements::default()
    })
}

fn tdom_interpolation_type_requirements(
    template: &TemplateStringInfo,
    module_context: &ModuleContext,
    prefix: &str,
    template_index: usize,
) -> tstring_syntax::BackendResult<TemplateTypeRequirements> {
    let mut resolver = TdomTypeRequirementResolver::new(prefix, template_index);
    let requirements = backend_tdom::interpolation_type_requirements_with_component_props(
        &template.to_template_input(),
        |context| {
            if context.prop_name.as_ref() == "children" {
                return None;
            }
            let signature =
                resolve_component_signature(module_context, context.component_expression)?;
            if signature.requires_positional {
                return None;
            }
            let parameter = signature.parameters.iter().find(|parameter| {
                parameter.name == context.prop_name.as_ref() && parameter.allows_keyword
            })?;
            expected_type_for_component_prop(parameter, context.value_kind)
                .map(|expected| resolver.annotation_for_expected_type(expected))
        },
    )?;
    Ok(TemplateTypeRequirements {
        requirements,
        type_imports: resolver.into_imports(),
    })
}

struct TdomTypeRequirementResolver<'a> {
    prefix: &'a str,
    template_index: usize,
    import_aliases: BTreeMap<String, String>,
}

impl<'a> TdomTypeRequirementResolver<'a> {
    fn new(prefix: &'a str, template_index: usize) -> Self {
        Self {
            prefix,
            template_index,
            import_aliases: BTreeMap::new(),
        }
    }

    fn annotation_for_expected_type(&mut self, expected: ComponentPropExpectedType) -> String {
        let Some(module) = expected.import_module else {
            return expected.annotation;
        };
        let alias = self
            .import_aliases
            .get(&module)
            .cloned()
            .unwrap_or_else(|| {
                format!(
                    "{}type_{}_{}",
                    self.prefix,
                    self.template_index,
                    self.import_aliases.len()
                )
            });
        let (annotation, uses_import) =
            qualify_imported_type_annotation(&expected.annotation, &alias);
        if uses_import {
            self.import_aliases.entry(module).or_insert(alias);
            annotation
        } else {
            expected.annotation
        }
    }

    fn into_imports(self) -> Vec<ShadowTypeImport> {
        self.import_aliases
            .into_iter()
            .map(|(module, alias)| ShadowTypeImport { module, alias })
            .collect()
    }
}

fn qualify_imported_type_annotation(annotation: &str, module_alias: &str) -> (String, bool) {
    let mut qualified = String::with_capacity(annotation.len());
    let mut index = 0usize;
    let mut used_import = false;

    while index < annotation.len() {
        if push_python_string_literal_token_if_present(annotation, &mut index, &mut qualified) {
            continue;
        }
        let ch = annotation[index..]
            .chars()
            .next()
            .expect("valid char boundary");
        if is_python_identifier_start(ch) {
            let start = index;
            index += ch.len_utf8();
            while index < annotation.len() {
                let ch = annotation[index..]
                    .chars()
                    .next()
                    .expect("valid char boundary");
                if !is_python_identifier_continue(ch) {
                    break;
                }
                index += ch.len_utf8();
            }
            let identifier = &annotation[start..index];
            if (start > 0 && annotation.as_bytes().get(start - 1) == Some(&b'.'))
                || is_scope_safe_type_name(identifier)
            {
                qualified.push_str(identifier);
            } else {
                qualified.push_str(module_alias);
                qualified.push('.');
                qualified.push_str(identifier);
                used_import = true;
            }
            continue;
        }

        qualified.push(ch);
        index += ch.len_utf8();
    }

    (qualified, used_import)
}

fn push_python_string_literal_token_if_present(
    source: &str,
    index: &mut usize,
    target: &mut String,
) -> bool {
    let Some((prefix_end, quote)) = python_string_literal_prefix(source, *index) else {
        return false;
    };
    target.push_str(&source[*index..prefix_end]);
    *index = prefix_end;
    push_python_string_literal_token(source, index, target, quote);
    true
}

fn python_string_literal_prefix(source: &str, index: usize) -> Option<(usize, char)> {
    let mut prefix_end = index;
    while prefix_end < source.len() {
        let ch = source[prefix_end..]
            .chars()
            .next()
            .expect("valid char boundary");
        if !matches!(
            ch,
            'r' | 'R' | 'u' | 'U' | 'b' | 'B' | 'f' | 'F' | 't' | 'T'
        ) {
            break;
        }
        prefix_end += ch.len_utf8();
    }
    let ch = source[prefix_end..].chars().next()?;
    matches!(ch, '\'' | '"').then_some((prefix_end, ch))
}

fn push_python_string_literal_token(
    source: &str,
    index: &mut usize,
    target: &mut String,
    quote: char,
) {
    let mut escaped = false;
    let mut opening = true;
    while *index < source.len() {
        let ch = source[*index..]
            .chars()
            .next()
            .expect("valid char boundary");
        target.push(ch);
        *index += ch.len_utf8();
        if opening {
            opening = false;
            continue;
        }
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            break;
        }
    }
}

fn is_python_identifier_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_python_identifier_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn is_scope_safe_type_name(identifier: &str) -> bool {
    matches!(
        identifier,
        "None"
            | "True"
            | "False"
            | "bool"
            | "int"
            | "float"
            | "complex"
            | "str"
            | "bytes"
            | "bytearray"
            | "memoryview"
            | "object"
            | "list"
            | "dict"
            | "set"
            | "frozenset"
            | "tuple"
            | "type"
            | "range"
            | "slice"
    )
}

fn requires_datetime_alias(expected_type: &str) -> bool {
    expected_type.contains("datetime.")
}

fn shadow_annotation_type<'a>(expected_type: &'a str, datetime_alias: &str) -> Cow<'a, str> {
    if !requires_datetime_alias(expected_type) {
        return Cow::Borrowed(expected_type);
    }
    Cow::Owned(expected_type.replace("datetime.", &format!("{datetime_alias}.")))
}

fn push_shadow_type_imports(insertion: &mut PendingInsertion, type_imports: &[ShadowTypeImport]) {
    for type_import in type_imports {
        if !insertion.type_imports.insert(type_import.clone()) {
            continue;
        }
        push_shadow_statement_prefix(&mut insertion.text, insertion.mode);
        insertion.text.push_str("import ");
        insertion.text.push_str(&type_import.module);
        insertion.text.push_str(" as ");
        insertion.text.push_str(&type_import.alias);
    }
}

fn interpolation_by_index(
    template: &TemplateStringInfo,
    interpolation_index: usize,
) -> Option<&InterpolationInfo> {
    template.parts.iter().find_map(|part| match part {
        TemplatePart::Interpolation(interpolation)
            if interpolation.interpolation_index == interpolation_index =>
        {
            Some(interpolation)
        }
        _ => None,
    })
}

fn should_skip_interpolation(
    interpolation: &crate::InterpolationInfo,
    source: &str,
    line_starts: &[usize],
    root: Node<'_>,
) -> Result<bool> {
    if interpolation.conversion.is_some() || !interpolation.format_spec.is_empty() {
        return Ok(true);
    }
    if interpolation.expression.contains('\n')
        || interpolation.location.start_line != interpolation.location.end_line
    {
        return Ok(true);
    }

    let byte_range = location_byte_range(source, line_starts, &interpolation.location)?;
    Ok(root
        .descendant_for_byte_range(byte_range.start, byte_range.end)
        .is_some_and(|node| contains_node_kind(node, "named_expression")))
}

fn push_shadow_statement_prefix(text: &mut String, mode: InsertionMode) {
    match mode {
        InsertionMode::AfterStatement => text.push_str("; "),
        InsertionMode::BeforeStatement if !text.is_empty() => text.push_str("; "),
        InsertionMode::BeforeStatement => {}
    }
}

fn enclosing_simple_statement_insertion(
    root: Node<'_>,
    start_byte: usize,
    source_len: usize,
) -> Option<InsertionPoint> {
    let end_byte = start_byte.saturating_add(1).min(source_len);
    let mut node = root.descendant_for_byte_range(start_byte, end_byte)?;
    loop {
        let parent = node.parent()?;
        match parent.kind() {
            "module" | "block" => {
                return match node.kind() {
                    "return_statement" | "raise_statement" => Some(InsertionPoint {
                        offset: node.start_byte(),
                        mode: InsertionMode::BeforeStatement,
                    }),
                    kind if simple_statement_kind(kind) => Some(InsertionPoint {
                        offset: node.end_byte(),
                        mode: InsertionMode::AfterStatement,
                    }),
                    _ => None,
                };
            }
            _ => node = parent,
        }
    }
}

fn simple_statement_kind(kind: &str) -> bool {
    matches!(
        kind,
        "expression_statement" | "assert_statement" | "delete_statement"
    )
}

fn contains_node_kind(node: Node<'_>, kind: &str) -> bool {
    if node.kind() == kind {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| contains_node_kind(child, kind))
}

fn available_shadow_prefix(source: &str) -> String {
    let mut salt = 0usize;
    loop {
        let prefix = if salt == 0 {
            "__tl_".to_string()
        } else {
            format!("__tl{salt}_")
        };
        if !source.contains(&prefix) {
            return prefix;
        }
        salt += 1;
    }
}

fn location_byte_range(
    source: &str,
    line_starts: &[usize],
    location: &Location,
) -> Result<Range<usize>> {
    let start = location_start_byte(source, line_starts, location)?;
    let end = location_end_byte(source, line_starts, location)?;
    if start > end {
        return Err(anyhow::anyhow!("Location start is after end"));
    }
    Ok(start..end)
}

fn location_start_byte(source: &str, line_starts: &[usize], location: &Location) -> Result<usize> {
    location_position_byte(
        source,
        line_starts,
        location.start_line,
        location.start_column,
    )
}

fn location_end_byte(source: &str, line_starts: &[usize], location: &Location) -> Result<usize> {
    location_position_byte(source, line_starts, location.end_line, location.end_column)
}

fn location_position_byte(
    source: &str,
    line_starts: &[usize],
    line: usize,
    column: usize,
) -> Result<usize> {
    let line_start = *line_starts
        .get(line.saturating_sub(1))
        .ok_or_else(|| anyhow::anyhow!("Line {line} is out of bounds"))?;
    let offset = line_start + column.saturating_sub(1);
    if offset > source.len() {
        return Err(anyhow::anyhow!("Column {column} is out of bounds"));
    }
    Ok(offset)
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

fn line_index_for_offset(line_starts: &[usize], offset: usize) -> usize {
    line_starts
        .partition_point(|line_start| *line_start <= offset)
        .saturating_sub(1)
}

fn line_count(source: &str) -> usize {
    source.bytes().filter(|byte| *byte == b'\n').count() + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn shadow_test_dir(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "t-linter-shadow-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn synthesize(source: &str) -> Option<ShadowDocument> {
        synthesize_for_type_check(Path::new("example.py"), source).expect("synthesize")
    }

    fn assert_python_parses(source: &str) {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .expect("python language");
        let tree = parser.parse(source, None).expect("parse source");
        assert!(
            !tree.root_node().has_error(),
            "shadow source has parse errors"
        );
    }

    #[test]
    fn shadow_preserves_lines_and_maps_rhs_ranges() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "json"] = t'{{"name": {user}, "age": {age}, "note": {user!s}}}'
"#;
        let shadow = synthesize(source).expect("shadow");
        assert_eq!(line_count(&shadow.text), line_count(source));
        assert_eq!(shadow.sites.len(), 2);
        assert_eq!(
            &shadow.text[shadow.sites[0].shadow_rhs_byte_range.clone()],
            "user"
        );
        assert_eq!(
            &shadow.text[shadow.sites[1].shadow_rhs_byte_range.clone()],
            "age"
        );
        assert!(shadow.text.contains("__tl_0_0"));
        assert!(shadow.text.contains("__tl_0_1"));
        assert!(!shadow.text.contains("__tl_0_2"));
        assert_python_parses(&shadow.text);
    }

    #[test]
    fn insertion_uses_enclosing_statement_end() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

def run_json(template: Annotated[Template, "json"]) -> None: ...

run_json(
    t'{{"name": {user}}}',
    other,
)
"#;
        let shadow = synthesize(source).expect("shadow");
        assert!(shadow.text.contains("other,\n); __tl_0_0:"));
        let insertion_line = shadow
            .text
            .lines()
            .position(|line| line.contains("); __tl_0_0:"))
            .expect("insertion line");
        assert_eq!(shadow.sites[0].shadow_line, insertion_line);
        assert_eq!(
            &shadow.text[shadow.sites[0].shadow_rhs_byte_range.clone()],
            "user"
        );
        assert_python_parses(&shadow.text);
    }

    #[test]
    fn return_templates_insert_shadow_checks_before_return() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

def run_json(template: Annotated[Template, "json"]) -> None: ...

def build(user) -> Annotated[Template, "json"]:
    return run_json(t'{{"name": {user}}}')
"#;
        let shadow = synthesize(source).expect("shadow");

        assert!(shadow.text.contains("__tl_0_0: "));
        assert!(shadow.text.contains(" = user; return run_json("));
        assert_eq!(
            &shadow.text[shadow.sites[0].shadow_rhs_byte_range.clone()],
            "user"
        );
        assert_python_parses(&shadow.text);
    }

    #[test]
    fn class_body_and_nested_function_sources_parse() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

class Example:
    payload: Annotated[Template, "json"] = t'{{"name": {user}}}'

def outer(user):
    def inner():
        payload: Annotated[Template, "json"] = t'{{"name": {user}}}'
        return payload
"#;
        let shadow = synthesize(source).expect("shadow");
        assert_eq!(shadow.sites.len(), 2);
        assert_python_parses(&shadow.text);
    }

    #[test]
    fn datetime_types_use_shadow_alias_without_shifting_lines() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

def run_toml(template: Annotated[Template, "toml"]) -> None: ...

def handler(created, label):
    payload: Annotated[Template, "toml"] = t'when = {created}\nlabel = "{label}"'
    run_toml(payload)
"#;
        let shadow = synthesize(source).expect("shadow");

        assert_eq!(line_count(&shadow.text), line_count(source));
        assert!(shadow.text.contains("; import datetime as __tl_datetime; __tl_0_0: \"str | int | float | bool | __tl_datetime.date | __tl_datetime.time | __tl_datetime.datetime | list[object] | dict[str, object]\" = created"));
        assert!(shadow.text.contains("__tl_0_1: \"str\" = label"));
        assert_eq!(shadow.sites[0].expected_description, "toml value");
        assert!(shadow.sites[0].expected_type.contains("datetime.date"));
        assert_eq!(
            &shadow.text[shadow.sites[0].shadow_rhs_byte_range.clone()],
            "created"
        );
        assert_python_parses(&shadow.text);
    }

    #[test]
    fn tdom_component_prop_requirements_are_synthesized_from_backend() {
        let source = r#"from string.templatelib import Template
from typing import Literal
from tdom import html

class User:
    name: str

def Card(*, title: str, count: int, owner: User, labels: list[str], label: str, state: Literal["open", "closed"]) -> object: ...

def handler(user: User, age: int, name: str) -> None:
    payload = html(t'<{Card} title={age} count={name} owner={age} labels={name} label="Hello {age}" state={name} />')
"#;
        let shadow = synthesize(source).expect("shadow");

        assert_eq!(line_count(&shadow.text), line_count(source));
        assert_eq!(shadow.sites.len(), 6);
        assert!(shadow.text.contains("__tl_0_1: \"str\" = age"));
        assert!(shadow.text.contains("__tl_0_2: \"int\" = name"));
        assert!(shadow.text.contains("__tl_0_3: \"User\" = age"));
        assert!(shadow.text.contains("__tl_0_4: \"list[str]\" = name"));
        assert!(shadow.text.contains("__tl_0_5: \"str\" = age"));
        assert!(
            shadow
                .text
                .contains("__tl_0_6: \"Literal[\\\"open\\\", \\\"closed\\\"]\" = name")
        );
        assert_eq!(
            shadow.sites[0].expected_description,
            "tdom component prop 'title'"
        );
        assert_eq!(shadow.sites[2].expected_type, "User");
        assert_eq!(shadow.sites[3].expected_type, "list[str]");
        assert_eq!(
            shadow.sites[4].expected_description,
            "tdom component prop 'label' string fragment"
        );
        assert_eq!(
            shadow.sites[5].expected_type,
            "Literal[\"open\", \"closed\"]"
        );
        assert_python_parses(&shadow.text);
    }

    #[test]
    fn imported_tdom_component_prop_types_are_qualified_in_shadow_source() {
        let dir = shadow_test_dir("imported-tdom-component-prop-types");
        write_file(
            &dir.join("components.py"),
            r#"from typing import Literal

class User:
    name: str

def Card(*, owner: User, items: list[User], state: Literal["open"]) -> object:
    return object()
"#,
        );
        let source = r#"from components import Card
from tdom import html

def handler(age: int, name: str) -> None:
    payload = html(t'<{Card} owner={age} items={name} state={name} />')
"#;
        let shadow = synthesize_for_type_check(&dir.join("app.py"), source)
            .expect("synthesize")
            .expect("shadow");

        let _ = fs::remove_dir_all(dir);

        assert!(shadow.text.contains("import components as __tl_type_0_0"));
        assert!(
            shadow
                .text
                .contains("__tl_0_1: \"__tl_type_0_0.User\" = age")
        );
        assert!(
            shadow
                .text
                .contains("__tl_0_2: \"list[__tl_type_0_0.User]\" = name")
        );
        assert!(
            shadow
                .text
                .contains("__tl_0_3: \"__tl_type_0_0.Literal[\\\"open\\\"]\" = name"),
            "{}",
            shadow.text
        );
        assert_python_parses(&shadow.text);
    }

    #[test]
    fn imported_tdom_component_prop_type_aliases_do_not_collide_in_one_statement() {
        let dir = shadow_test_dir("imported-tdom-component-prop-alias-collision");
        write_file(
            &dir.join("card_a.py"),
            r#"class UserA:
    name: str

def CardA(*, owner: UserA) -> object:
    return object()
"#,
        );
        write_file(
            &dir.join("card_b.py"),
            r#"class UserB:
    name: str

def CardB(*, owner: UserB) -> object:
    return object()
"#,
        );
        let source = r#"from card_a import CardA
from card_b import CardB
from tdom import html

def handler(age: int, name: str) -> None:
    payload = (html(t'<{CardA} owner={age} />'), html(t'<{CardB} owner={name} />'))
"#;
        let shadow = synthesize_for_type_check(&dir.join("app.py"), source)
            .expect("synthesize")
            .expect("shadow");

        let _ = fs::remove_dir_all(dir);

        assert!(shadow.text.contains("import card_a as __tl_type_0_0"));
        assert!(shadow.text.contains("import card_b as __tl_type_1_0"));
        assert!(
            shadow
                .text
                .contains("__tl_0_1: \"__tl_type_0_0.UserA\" = age")
        );
        assert!(
            shadow
                .text
                .contains("__tl_1_1: \"__tl_type_1_0.UserB\" = name"),
            "{}",
            shadow.text
        );
        assert_python_parses(&shadow.text);
    }

    #[test]
    fn imported_tdom_component_prop_annotation_keeps_prefixed_string_literals() {
        let (annotation, uses_import) = qualify_imported_type_annotation(
            r#"Literal[r"open", b"closed", u'pending']"#,
            "__tl_type_0_0",
        );

        assert!(uses_import);
        assert_eq!(
            annotation,
            r#"__tl_type_0_0.Literal[r"open", b"closed", u'pending']"#
        );
    }

    #[test]
    fn salt_avoids_existing_prefix() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

__tl_0_0 = "taken"
payload: Annotated[Template, "json"] = t'{{"name": {user}}}'
"#;
        let shadow = synthesize(source).expect("shadow");
        assert!(shadow.text.contains("__tl1_0_0"));
    }

    #[test]
    fn skips_unsafe_or_untyped_interpolations() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

json_payload: Annotated[Template, "json"] = t'{{"a": {user!r}, "b": {value:.2f}, "c": {(x := 1)}}}'
html_payload: Annotated[Template, "html"] = t"<div>{user}</div>"
if t'{{"name": {user}}}':
    pass
"#;
        assert!(synthesize(source).is_none());
    }

    #[test]
    fn multiple_templates_keep_indexes_and_expression_ranges() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

first: Annotated[Template, "json"] = t'{{"name": {user}}}'
second: Annotated[Template, "json"] = t'{{"age": {age}, "tag": {tag}}}'
"#;
        let shadow = synthesize(source).expect("shadow");
        let expressions = shadow
            .sites
            .iter()
            .map(|site| {
                (
                    site.template_index,
                    site.interpolation_index,
                    site.expression.as_str(),
                    &shadow.text[site.shadow_rhs_byte_range.clone()],
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            expressions,
            vec![
                (0, 0, "user", "user"),
                (1, 0, "age", "age"),
                (1, 1, "tag", "tag")
            ]
        );
    }
}
