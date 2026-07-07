use std::path::Path;

use anyhow::{Context, Result};
use tstring_syntax::BackendError;

use crate::backend::TemplateBackend;
use crate::lint::DiagnosticEdit;
use crate::{Location, TemplateStringInfo, TemplateStringParser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatOptions {
    pub line_length: usize,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self { line_length: 80 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateEdit {
    pub location: Location,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError {
    pub message: String,
    pub language: Option<String>,
    pub location: Option<Location>,
}

impl FormatError {
    fn from_backend_error(
        template: &TemplateStringInfo,
        language: &str,
        error: BackendError,
    ) -> Self {
        let display_language = if language == "yml" { "yaml" } else { language };
        let primary = error.diagnostics.first();
        let location = primary
            .and_then(|diagnostic| diagnostic.span.as_ref())
            .map(|span| template.backend_span_to_location(span))
            .or_else(|| Some(template.location.clone()));

        Self {
            message: primary
                .map(|diagnostic| diagnostic.message.clone())
                .unwrap_or(error.message),
            language: Some(display_language.to_string()),
            location,
        }
    }
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FormatError {}

pub fn format_document(source: &str) -> Result<Vec<TemplateEdit>> {
    format_document_with_options(source, &FormatOptions::default())
}

pub fn format_document_with_options(
    source: &str,
    options: &FormatOptions,
) -> Result<Vec<TemplateEdit>> {
    let mut parser = TemplateStringParser::new()?;
    let templates = parser.find_template_strings(source)?;
    format_templates(&templates, options)
}

pub fn format_document_in_file(source: &str, path: &Path) -> Result<Vec<TemplateEdit>> {
    format_document_in_file_with_options(source, path, &FormatOptions::default())
}

pub fn format_document_in_file_with_options(
    source: &str,
    path: &Path,
    options: &FormatOptions,
) -> Result<Vec<TemplateEdit>> {
    let mut parser = TemplateStringParser::new()?;
    let templates = parser.find_template_strings_in_file(source, path)?;
    format_templates(&templates, options)
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

pub fn apply_diagnostic_edits(source: &str, edits: &[DiagnosticEdit]) -> Result<String> {
    if edits.is_empty() {
        return Ok(source.to_string());
    }

    let template_edits = edits
        .iter()
        .map(|edit| TemplateEdit {
            location: Location {
                start_line: edit.range.start_line,
                start_column: edit.range.start_column,
                end_line: edit.range.end_line,
                end_column: edit.range.end_column,
            },
            replacement: edit.new_text.clone(),
        })
        .collect::<Vec<_>>();

    apply_template_edits(source, &template_edits)
}

pub fn format_document_range(source: &str, range: &Location) -> Result<Vec<TemplateEdit>> {
    format_document_range_with_options(source, range, &FormatOptions::default())
}

pub fn format_document_range_with_options(
    source: &str,
    range: &Location,
    options: &FormatOptions,
) -> Result<Vec<TemplateEdit>> {
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

    format_template_edit(matches[0], options)
        .transpose()
        .map(|edit| edit.into_iter().collect())
}

fn format_template_edit(
    template: &TemplateStringInfo,
    options: &FormatOptions,
) -> Option<Result<TemplateEdit>> {
    let language = template.language.as_deref()?.to_ascii_lowercase();
    let input = template.to_template_input();
    let line_length = options.line_length.max(1);

    let backend = TemplateBackend::for_language(&language)?;
    if backend == TemplateBackend::Sql {
        return None;
    }
    let formatted = backend.format_template(&input, template.profile.as_deref(), line_length);

    Some(
        formatted
            .map(|content| TemplateEdit {
                location: template.formatting_location(&content).clone(),
                replacement: template.formatted_literal(&content),
            })
            .map_err(|error| FormatError::from_backend_error(template, &language, error).into()),
    )
}

fn format_templates(
    templates: &[TemplateStringInfo],
    options: &FormatOptions,
) -> Result<Vec<TemplateEdit>> {
    templates
        .iter()
        .filter_map(|template| format_template_edit(template, options))
        .collect::<Result<Vec<_>>>()
}

fn ranges_overlap(left: &Location, right: &Location) -> bool {
    let left_start = (left.start_line, left.start_column);
    let left_end = (left.end_line, left.end_column);
    let right_start = (right.start_line, right.start_column);
    let right_end = (right.end_line, right.end_column);

    // A zero-width `right` range acts as a cursor and uses the template's
    // inclusive start / exclusive end boundary.
    if right_start == right_end {
        left_start <= right_start && right_start < left_end
    } else {
        left_start < right_end && right_start < left_end
    }
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
    fn range_formatting_uses_half_open_boundaries() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

config: Annotated[Template, "json"] = t'{"name": {name}}'
"#;

        let edits = format_document_range(
            source,
            &Location {
                start_line: 5,
                start_column: 58,
                end_line: 5,
                end_column: 60,
            },
        )
        .expect("expected range format success");

        assert!(edits.is_empty());
    }

    #[test]
    fn ranges_overlap_includes_zero_width_cursor_at_start() {
        let template = Location {
            start_line: 1,
            start_column: 5,
            end_line: 1,
            end_column: 10,
        };
        let start_cursor = Location {
            start_line: 1,
            start_column: 5,
            end_line: 1,
            end_column: 5,
        };
        let end_cursor = Location {
            start_line: 1,
            start_column: 10,
            end_line: 1,
            end_column: 10,
        };

        assert!(ranges_overlap(&template, &start_cursor));
        assert!(!ranges_overlap(&template, &end_cursor));
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

    #[test]
    fn apply_diagnostic_edits_reuses_template_edit_offsets() {
        let source = "query = t\"SELECT {value!r}\"\n";
        let edits = vec![DiagnosticEdit {
            range: crate::lint::DiagnosticEditRange {
                start_line: 1,
                start_column: 24,
                end_line: 1,
                end_column: 26,
            },
            new_text: String::new(),
        }];

        let output = apply_diagnostic_edits(source, &edits).expect("expected apply success");

        assert_eq!(output, "query = t\"SELECT {value}\"\n");
    }

    #[test]
    fn apply_diagnostic_edits_rejects_overlaps() {
        let source = "value = t\"abcdef\"\n";
        let edits = vec![
            DiagnosticEdit {
                range: crate::lint::DiagnosticEditRange {
                    start_line: 1,
                    start_column: 11,
                    end_line: 1,
                    end_column: 14,
                },
                new_text: "x".to_string(),
            },
            DiagnosticEdit {
                range: crate::lint::DiagnosticEditRange {
                    start_line: 1,
                    start_column: 13,
                    end_line: 1,
                    end_column: 16,
                },
                new_text: "y".to_string(),
            },
        ];

        let error = apply_diagnostic_edits(source, &edits).expect_err("overlapping edits");

        assert!(error.to_string().contains("Overlapping template edits"));
    }

    #[test]
    fn format_document_with_options_passes_line_length_to_html_backend() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

markup: Annotated[Template, "html"] = t'<div data-a="12345" data-b="67890"></div>'
"#;

        let edits = format_document_with_options(source, &FormatOptions { line_length: 20 })
            .expect("expected format success");

        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].replacement,
            "t\"\"\"<div\n  data-a=\"12345\"\n  data-b=\"67890\"\n></div>\"\"\""
        );
    }

    #[test]
    fn range_formatting_with_options_reformats_whole_matching_template() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

markup: Annotated[Template, "html"] = t'<div data-a="12345" data-b="67890"></div>'
"#;

        let edits = format_document_range_with_options(
            source,
            &Location {
                start_line: 5,
                start_column: 50,
                end_line: 5,
                end_column: 55,
            },
            &FormatOptions { line_length: 20 },
        )
        .expect("expected range format success");

        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].replacement,
            "t\"\"\"<div\n  data-a=\"12345\"\n  data-b=\"67890\"\n></div>\"\"\""
        );
    }
}
