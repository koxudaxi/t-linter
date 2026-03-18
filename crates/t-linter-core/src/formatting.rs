use anyhow::{Context, Result};

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

pub fn apply_template_edits(source: &str, edits: &[TemplateEdit]) -> Result<String> {
    if edits.is_empty() {
        return Ok(source.to_string());
    }

    let line_starts = line_start_offsets(source.as_bytes());
    let mut byte_edits = edits
        .iter()
        .map(|edit| {
            let start = location_to_byte_offset(
                source,
                &line_starts,
                edit.location.start_line,
                edit.location.start_column,
            )
            .with_context(|| "Failed to compute edit start offset")?;
            let end = location_to_byte_offset(
                source,
                &line_starts,
                edit.location.end_line,
                edit.location.end_column,
            )
            .with_context(|| "Failed to compute edit end offset")?;

            if end < start {
                return Err(anyhow::anyhow!(
                    "Edit end offset precedes start offset for {}:{}-{}:{}",
                    edit.location.start_line,
                    edit.location.start_column,
                    edit.location.end_line,
                    edit.location.end_column
                ));
            }

            Ok((start, end, &edit.replacement))
        })
        .collect::<Result<Vec<_>>>()?;

    byte_edits.sort_by(|left, right| right.0.cmp(&left.0).then(right.1.cmp(&left.1)));

    for window in byte_edits.windows(2) {
        let previous = &window[0];
        let current = &window[1];
        // Formatter edits replace whole template literals, so overlap is unexpected.
        // Keep the check anyway so a malformed edit list can't corrupt the buffer.
        if current.1 > previous.0 {
            return Err(anyhow::anyhow!(
                "Overlapping template edits detected at byte ranges {}..{} and {}..{}",
                current.0,
                current.1,
                previous.0,
                previous.1
            ));
        }
    }

    let mut output = source.as_bytes().to_vec();
    for (start, end, replacement) in byte_edits {
        output.splice(start..end, replacement.as_bytes().iter().copied());
    }

    String::from_utf8(output).context("Formatted output is not valid UTF-8")
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

fn line_start_offsets(source: &[u8]) -> Vec<usize> {
    let mut starts = vec![0];
    let mut index = 0;

    while index < source.len() {
        match source[index] {
            b'\n' => {
                index += 1;
                starts.push(index);
            }
            b'\r' => {
                index += 1;
                if index < source.len() && source[index] == b'\n' {
                    index += 1;
                }
                starts.push(index);
            }
            _ => {
                index += 1;
            }
        }
    }

    starts
}

fn location_to_byte_offset(
    source: &str,
    line_starts: &[usize],
    line: usize,
    column: usize,
) -> Result<usize> {
    if line == 0 || column == 0 {
        return Err(anyhow::anyhow!("Locations must be 1-based"));
    }

    let Some(&line_start) = line_starts.get(line - 1) else {
        return Err(anyhow::anyhow!("Line {line} is out of bounds"));
    };

    let offset = line_start + (column - 1);
    if offset > source.len() {
        return Err(anyhow::anyhow!(
            "Column {column} on line {line} is out of bounds"
        ));
    }

    Ok(offset)
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

    #[test]
    fn apply_template_edits_preserves_multibyte_prefix_and_crlf() {
        let source = "title = \"こんにちは\"\r\npayload = t'{\"b\": 2, \"a\": 1}'\r\n";
        let edits = vec![TemplateEdit {
            location: Location {
                start_line: 2,
                start_column: 11,
                end_line: 2,
                end_column: 30,
            },
            replacement: "t'{\"a\": 1, \"b\": 2}'".to_string(),
        }];

        let output = apply_template_edits(source, &edits).expect("expected apply success");

        assert_eq!(
            output,
            "title = \"こんにちは\"\r\npayload = t'{\"a\": 1, \"b\": 2}'\r\n"
        );
    }
}
