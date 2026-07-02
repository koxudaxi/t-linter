use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use crate::backend::TemplateBackend;
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

pub fn synthesize_for_type_check(path: &Path, source: &str) -> Result<Option<ShadowDocument>> {
    let mut template_parser = TemplateStringParser::new()?;
    let templates = template_parser.find_template_strings_in_file(source, path)?;
    let requirements_by_template = type_requirements_by_template(&templates);
    if requirements_by_template.iter().all(Vec::is_empty) {
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
    let prefix = available_shadow_prefix(source);
    let mut insertions = BTreeMap::<usize, PendingInsertion>::new();

    for ((template_index, template), requirements) in templates
        .iter()
        .enumerate()
        .zip(requirements_by_template.into_iter())
    {
        if requirements.is_empty() {
            continue;
        }
        let template_start = location_start_byte(source, &line_starts, &template.location)?;
        let Some(statement_end) =
            enclosing_simple_statement_end(tree.root_node(), template_start, source.len())
        else {
            tracing::debug!(
                "Skipping interpolation type checks outside a simple statement at {}:{}",
                template.location.start_line,
                template.location.start_column
            );
            continue;
        };

        for requirement in requirements {
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
            let lhs = format!("; {name}: \"{}\" = ", requirement.expected_python_type);
            debug_assert!(!lhs.contains('\n'));
            debug_assert!(!interpolation.expression.contains('\n'));

            let insertion = insertions.entry(statement_end).or_default();
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
        text.push_str(&source[cursor..offset]);
        let insertion_start = text.len();
        text.push_str(&insertion.text);
        cursor = offset;

        sites.extend(insertion.sites.into_iter().map(|site| ShadowCheckSite {
            shadow_rhs_byte_range: insertion_start + site.rhs_relative_range.start
                ..insertion_start + site.rhs_relative_range.end,
            shadow_line: site.original_location.start_line.saturating_sub(1),
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
) -> Vec<Vec<tstring_syntax::InterpolationTypeRequirement>> {
    templates
        .iter()
        .map(|template| {
            let Some(language) = template.language.as_deref() else {
                return Vec::new();
            };
            let Some(backend) = TemplateBackend::for_language(language) else {
                return Vec::new();
            };
            match backend.interpolation_type_requirements(&template.to_template_input()) {
                Ok(requirements) => requirements,
                Err(error) => {
                    tracing::debug!(
                        "Skipping interpolation type requirements for {} template at {}:{}: {}",
                        language,
                        template.location.start_line,
                        template.location.start_column,
                        error.message
                    );
                    Vec::new()
                }
            }
        })
        .collect()
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

fn enclosing_simple_statement_end(
    root: Node<'_>,
    start_byte: usize,
    source_len: usize,
) -> Option<usize> {
    let end_byte = start_byte.saturating_add(1).min(source_len);
    let mut node = root.descendant_for_byte_range(start_byte, end_byte)?;
    loop {
        let parent = node.parent()?;
        match parent.kind() {
            "module" | "block" => {
                return simple_statement_kind(node.kind()).then_some(node.end_byte());
            }
            _ => node = parent,
        }
    }
}

fn simple_statement_kind(kind: &str) -> bool {
    matches!(
        kind,
        "expression_statement"
            | "return_statement"
            | "assert_statement"
            | "delete_statement"
            | "raise_statement"
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

fn line_count(source: &str) -> usize {
    source.bytes().filter(|byte| *byte == b'\n').count() + 1
}

#[cfg(test)]
mod tests {
    use super::*;

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
