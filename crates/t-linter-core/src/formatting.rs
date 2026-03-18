use anyhow::Result;

use crate::{Location, TemplateStringInfo, TemplateStringParser};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateEdit {
    pub location: Location,
    pub replacement: String,
}

pub fn format_document(source: &str) -> Result<Vec<TemplateEdit>> {
    let mut parser = TemplateStringParser::new()?;
    let templates = parser.find_template_strings(source)?;

    templates
        .iter()
        .filter_map(format_template_edit)
        .collect::<Result<Vec<_>>>()
}

pub fn format_document_range(source: &str, range: &Location) -> Result<Vec<TemplateEdit>> {
    let mut parser = TemplateStringParser::new()?;
    let templates = parser.find_template_strings(source)?;

    let matches = templates
        .iter()
        .filter(|template| ranges_overlap(&template.location, range))
        .collect::<Vec<_>>();

    if matches.is_empty() {
        return Ok(Vec::new());
    }
    if matches.len() > 1 {
        return Err(anyhow::anyhow!(
            "Range formatting must target exactly one template string."
        ));
    }

    format_template_edit(matches[0])
        .transpose()
        .map(|edit| edit.into_iter().collect())
}

fn format_template_edit(template: &TemplateStringInfo) -> Option<Result<TemplateEdit>> {
    let language = template.language.as_deref()?.to_ascii_lowercase();
    let input = template.to_template_input();

    let formatted = match language.as_str() {
        "json" => tstring_json::format_template(&input),
        "yaml" | "yml" => tstring_yaml::format_template(&input),
        "toml" => tstring_toml::format_template(&input),
        _ => return None,
    };

    Some(
        formatted
            .map(|content| TemplateEdit {
                location: template.location.clone(),
                replacement: template.formatted_literal(&content),
            })
            .map_err(Into::into),
    )
}

fn ranges_overlap(left: &Location, right: &Location) -> bool {
    let left_start = (left.start_line, left.start_column);
    let left_end = (left.end_line, left.end_column);
    let right_start = (right.start_line, right.start_column);
    let right_end = (right.end_line, right.end_column);

    left_start <= right_end && right_start <= left_end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_supported_templates_only() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

config: Annotated[Template, "json"] = t'{"name": {name}, "message": "Hello {user!r:>5}"}'
plain = t"hello {name}"
"#;

        let edits = format_document(source).expect("expected format success");
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].replacement,
            "t'{\"name\": {name}, \"message\": \"Hello {user!r:>5}\"}'"
        );
    }

    #[test]
    fn range_formatting_requires_single_template() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

config: Annotated[Template, "json"] = t'{"name": {name}}'
"#;

        let edits = format_document_range(
            source,
            &Location {
                start_line: 5,
                start_column: 40,
                end_line: 5,
                end_column: 55,
            },
        )
        .expect("expected range format success");

        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].replacement, "t'{\"name\": {name}}'");
    }
}
