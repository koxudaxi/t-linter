use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::{TemplateStringInfo, TemplateStringParser};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatResult {
    pub formatted_source: String,
    pub changed: bool,
    pub formatted_templates: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatRange {
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

pub fn format_source(source: &str, workspace_root: &Path) -> Result<FormatResult> {
    format_source_with_runner(source, workspace_root, &ExternalFormatterRunner, None)
}

pub fn format_source_in_ranges(
    source: &str,
    workspace_root: &Path,
    ranges: &[FormatRange],
) -> Result<FormatResult> {
    format_source_with_runner(
        source,
        workspace_root,
        &ExternalFormatterRunner,
        Some(ranges),
    )
}

fn format_source_with_runner(
    source: &str,
    workspace_root: &Path,
    runner: &dyn FormatterRunner,
    ranges: Option<&[FormatRange]>,
) -> Result<FormatResult> {
    let mut parser = TemplateStringParser::new()?;
    let templates = parser.find_template_strings(source)?;
    let mut replacements = Vec::new();
    let mut formatted_templates = 0;

    for template in templates {
        if let Some(ranges) = ranges {
            if !ranges
                .iter()
                .any(|range| range_overlaps_template(range, &template))
            {
                continue;
            }
        }

        let Some(language) = template
            .language
            .as_deref()
            .and_then(normalize_language)
            .filter(|language| supports_formatting(language))
        else {
            continue;
        };

        let replacement = format_template(&template, language, workspace_root, runner)?;
        if replacement != template.raw_content {
            replacements.push((template.start_byte, template.end_byte, replacement));
            formatted_templates += 1;
        }
    }

    if replacements.is_empty() {
        return Ok(FormatResult {
            formatted_source: source.to_string(),
            changed: false,
            formatted_templates: 0,
        });
    }

    let mut formatted_source = source.to_string();
    replacements.sort_by_key(|(start, _, _)| *start);
    for (start, end, replacement) in replacements.into_iter().rev() {
        formatted_source.replace_range(start..end, &replacement);
    }

    Ok(FormatResult {
        changed: formatted_source != source,
        formatted_source,
        formatted_templates,
    })
}

trait FormatterRunner {
    fn run(&self, language: &str, input: &str, workspace_root: &Path) -> Result<String>;
}

struct ExternalFormatterRunner;

impl FormatterRunner for ExternalFormatterRunner {
    fn run(&self, language: &str, input: &str, workspace_root: &Path) -> Result<String> {
        match language {
            "html" | "css" | "javascript" | "json" | "yaml" => {
                run_prettier(language, input, workspace_root)
            }
            "toml" => run_taplo(input, workspace_root),
            _ => bail!("Unsupported formatter language: {language}"),
        }
    }
}

fn run_prettier(language: &str, input: &str, workspace_root: &Path) -> Result<String> {
    let parser = match language {
        "html" => "html",
        "css" => "css",
        "javascript" => "babel",
        "json" => "json",
        "yaml" => "yaml",
        _ => bail!("Unsupported Prettier language: {language}"),
    };

    let prettier = resolve_prettier(workspace_root);
    run_command(
        &prettier,
        &["--parser", parser],
        input,
        workspace_root,
        "prettier",
        Some(missing_prettier_message(language)),
    )
}

fn run_taplo(input: &str, workspace_root: &Path) -> Result<String> {
    run_command(
        &PathBuf::from("taplo"),
        &["format", "-"],
        input,
        workspace_root,
        "taplo",
        Some(missing_taplo_message()),
    )
}

fn run_command(
    executable: &Path,
    args: &[&str],
    input: &str,
    workspace_root: &Path,
    command_name: &str,
    missing_command_message: Option<String>,
) -> Result<String> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .current_dir(workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if let Some(message) = missing_command_message {
                bail!("{message}");
            }
            bail!("Failed to start {command_name}: {error}");
        }
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to start {command_name}"));
        }
    };

    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("Failed to open {command_name} stdin"))?
        .write_all(input.as_bytes())
        .with_context(|| format!("Failed to write to {command_name} stdin"))?;

    let output = child
        .wait_with_output()
        .with_context(|| format!("Failed to wait for {command_name}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("{command_name} exited with {}", output.status)
        } else {
            stderr
        };
        bail!("{message}");
    }

    String::from_utf8(output.stdout).context("Formatter returned non-UTF-8 output")
}

fn missing_prettier_message(language: &str) -> String {
    format!(
        "prettier is required to format {language} templates. Install it with `npm install --save-dev prettier` or make `prettier` available on PATH."
    )
}

fn missing_taplo_message() -> String {
    "taplo is required to format toml templates. Install it with `cargo install taplo-cli` or make `taplo` available on PATH.".to_string()
}

fn resolve_prettier(workspace_root: &Path) -> PathBuf {
    let local_bin = workspace_root.join("node_modules").join(".bin");
    let prettier = local_bin.join(if cfg!(windows) {
        "prettier.cmd"
    } else {
        "prettier"
    });
    if prettier.is_file() {
        prettier
    } else {
        PathBuf::from("prettier")
    }
}

fn format_template(
    template: &TemplateStringInfo,
    language: &str,
    workspace_root: &Path,
    runner: &dyn FormatterRunner,
) -> Result<String> {
    let prepared = prepare_template_for_format(template, language)?;
    let formatted = runner.run(language, &prepared.formatter_input, workspace_root)?;
    let restored = restore_formatted_content(&formatted, &prepared)?;

    Ok(rebuild_raw_template(template, &restored, &prepared.slots))
}

struct PreparedTemplate {
    formatter_input: String,
    slots: Vec<PlaceholderSlot>,
    layout: TemplateLayout,
}

struct PlaceholderSlot {
    placeholder: String,
    raw_expression: String,
}

struct TemplateLayout {
    had_leading_newline: bool,
    had_trailing_newline: bool,
    indent: String,
}

fn prepare_template_for_format(
    template: &TemplateStringInfo,
    language: &str,
) -> Result<PreparedTemplate> {
    let (with_placeholders, slots) = inject_placeholders(template, language)?;
    let (dedented, layout) = dedent_for_formatting(&with_placeholders);

    Ok(PreparedTemplate {
        formatter_input: dedented,
        slots,
        layout,
    })
}

fn inject_placeholders(
    template: &TemplateStringInfo,
    language: &str,
) -> Result<(String, Vec<PlaceholderSlot>)> {
    let mut rendered = String::new();
    let mut slots = Vec::new();
    let mut last_end = 0;

    for (index, expression) in template.expressions.iter().enumerate() {
        let relative = template.content[last_end..]
            .find("{}")
            .ok_or_else(|| anyhow::anyhow!("Failed to match interpolation placeholder"))?;
        let placeholder_start = last_end + relative;
        rendered.push_str(&template.content[last_end..placeholder_start]);

        let placeholder = placeholder_for_language(language, index);
        rendered.push_str(&placeholder);
        slots.push(PlaceholderSlot {
            placeholder,
            raw_expression: expression.raw_content.clone(),
        });

        last_end = placeholder_start + 2;
    }

    rendered.push_str(&template.content[last_end..]);
    Ok((rendered, slots))
}

fn placeholder_for_language(language: &str, index: usize) -> String {
    match language {
        "json" | "toml" => format!("\"__T_LINTER_SLOT_{index}__\""),
        "css" => format!("var(--t-linter-slot-{index})"),
        _ => format!("__T_LINTER_SLOT_{index}__"),
    }
}

fn dedent_for_formatting(content: &str) -> (String, TemplateLayout) {
    let had_leading_newline = content.starts_with('\n');
    let had_trailing_newline = content.ends_with('\n');
    let mut body = content;

    if had_leading_newline {
        body = &body[1..];
    }
    if had_trailing_newline && !body.is_empty() {
        body = &body[..body.len() - 1];
    }

    let indent = common_indent(body);
    let dedented = if indent.is_empty() {
        body.to_string()
    } else {
        body.lines()
            .map(|line| {
                if line.trim().is_empty() {
                    String::new()
                } else {
                    line.strip_prefix(&indent).unwrap_or(line).to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    (
        dedented,
        TemplateLayout {
            had_leading_newline,
            had_trailing_newline,
            indent,
        },
    )
}

fn common_indent(content: &str) -> String {
    let mut indent: Option<String> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let current = line
            .chars()
            .take_while(|ch| matches!(ch, ' ' | '\t'))
            .collect::<String>();

        indent = Some(match indent {
            None => current,
            Some(existing) => common_prefix(&existing, &current),
        });
    }

    indent.unwrap_or_default()
}

fn common_prefix(left: &str, right: &str) -> String {
    left.chars()
        .zip(right.chars())
        .take_while(|(left, right)| left == right)
        .map(|(ch, _)| ch)
        .collect()
}

fn restore_formatted_content(formatted: &str, prepared: &PreparedTemplate) -> Result<String> {
    let mut normalized = formatted.replace("\r\n", "\n");
    while normalized.ends_with('\n') {
        normalized.pop();
    }

    let mut content = if prepared.layout.indent.is_empty() {
        normalized
    } else {
        normalized
            .lines()
            .map(|line| {
                if line.trim().is_empty() {
                    String::new()
                } else {
                    format!("{}{}", prepared.layout.indent, line)
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    if prepared.layout.had_leading_newline {
        content.insert(0, '\n');
    }
    if prepared.layout.had_trailing_newline {
        content.push('\n');
    }

    for slot in &prepared.slots {
        if !content.contains(&slot.placeholder) {
            bail!("Formatter output lost interpolation placeholder");
        }
    }

    Ok(content)
}

fn rebuild_raw_template(
    template: &TemplateStringInfo,
    formatted_content: &str,
    slots: &[PlaceholderSlot],
) -> String {
    let prefix_len = calculate_template_content_offset(&template.raw_content);
    let suffix_len = if template.flags.is_triple { 3 } else { 1 };
    let prefix = &template.raw_content[..prefix_len];
    let suffix = &template.raw_content[template.raw_content.len() - suffix_len..];
    let mut escaped_content = escape_literal_braces(formatted_content);
    for slot in slots {
        escaped_content = escaped_content.replace(&slot.placeholder, &slot.raw_expression);
    }
    format!("{prefix}{escaped_content}{suffix}")
}

fn escape_literal_braces(content: &str) -> String {
    content.replace('{', "{{").replace('}', "}}")
}

fn supports_formatting(language: &str) -> bool {
    matches!(
        language,
        "html" | "css" | "javascript" | "json" | "yaml" | "toml"
    )
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

fn range_overlaps_template(range: &FormatRange, template: &TemplateStringInfo) -> bool {
    let range_start = (range.start_line, range.start_column);
    let range_end = (range.end_line, range.end_column);
    let template_start = (template.location.start_line, template.location.start_column);
    let template_end = (template.location.end_line, template.location.end_column);

    range_start < template_end && template_start < range_end
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockFormatterRunner;

    impl FormatterRunner for MockFormatterRunner {
        fn run(&self, language: &str, input: &str, _: &Path) -> Result<String> {
            Ok(match language {
                "json" => "{\n  \"name\": \"__T_LINTER_SLOT_0__\"\n}\n".to_string(),
                "html" => "<div>\n  __T_LINTER_SLOT_0__\n</div>\n".to_string(),
                "yaml" => "name: __T_LINTER_SLOT_0__\n".to_string(),
                "toml" => "title = \"__T_LINTER_SLOT_0__\"\n".to_string(),
                "javascript" => "const value = __T_LINTER_SLOT_0__;\n".to_string(),
                "css" => ".card {\n  width: var(--t-linter-slot-0);\n}\n".to_string(),
                _ => input.to_string(),
            })
        }
    }

    struct MissingPlaceholderRunner;

    impl FormatterRunner for MissingPlaceholderRunner {
        fn run(&self, _: &str, _: &str, _: &Path) -> Result<String> {
            Ok("{\n  \"name\": \"lost\"\n}\n".to_string())
        }
    }

    #[test]
    fn format_json_round_trips_placeholder() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "json"] = t"""{{"name": {value}}}"""
"#;

        let result =
            format_source_with_runner(source, Path::new("."), &MockFormatterRunner, None).unwrap();

        assert!(result.changed);
        assert!(
            result
                .formatted_source
                .contains("t\"\"\"{{\n  \"name\": {value}\n}}\"\"\"")
        );
    }

    #[test]
    fn format_multiline_reindents_content() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

page: Annotated[Template, "html"] = t"""
    <div>{value}</div>
"""
"#;

        let result =
            format_source_with_runner(source, Path::new("."), &MockFormatterRunner, None).unwrap();

        assert!(
            result
                .formatted_source
                .contains("t\"\"\"\n    <div>\n      {value}\n    </div>\n\"\"\"")
        );
    }

    #[test]
    fn format_multiple_templates_updates_both_without_offset_errors() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

first: Annotated[Template, "json"] = t"""{{"name": {first}}}"""
second: Annotated[Template, "toml"] = t"title = {second}"
"#;

        let result =
            format_source_with_runner(source, Path::new("."), &MockFormatterRunner, None).unwrap();

        assert!(
            result
                .formatted_source
                .contains(r#"first: Annotated[Template, "json"] = t"""{{"#,)
        );
        assert!(
            result
                .formatted_source
                .contains(r#"second: Annotated[Template, "toml"] = t"title = {second}""#)
        );
    }

    #[test]
    fn format_skips_unsupported_languages() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

query: Annotated[Template, "sql"] = t"select * from users"
"#;

        let result =
            format_source_with_runner(source, Path::new("."), &MockFormatterRunner, None).unwrap();

        assert!(!result.changed);
        assert_eq!(result.formatted_source, source);
    }

    #[test]
    fn format_errors_when_placeholder_is_lost() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "json"] = t"""{"name": {value}}"""
"#;

        let error =
            format_source_with_runner(source, Path::new("."), &MissingPlaceholderRunner, None)
                .unwrap_err();

        assert!(error.to_string().contains("placeholder"));
    }

    #[test]
    fn format_only_updates_templates_that_overlap_requested_ranges() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

first: Annotated[Template, "json"] = t"""{{"name": {first}}}"""
second: Annotated[Template, "html"] = t"<div>{second}</div>"
"#;

        let result = format_source_with_runner(
            source,
            Path::new("."),
            &MockFormatterRunner,
            Some(&[FormatRange {
                start_line: 4,
                start_column: 45,
                end_line: 4,
                end_column: 52,
            }]),
        )
        .unwrap();

        assert!(result.formatted_source.contains(
            "first: Annotated[Template, \"json\"] = t\"\"\"{{\n  \"name\": {first}\n}}\"\"\""
        ));
        assert!(
            result
                .formatted_source
                .contains(r#"second: Annotated[Template, "html"] = t"<div>{second}</div>""#)
        );
        assert_eq!(result.formatted_templates, 1);
    }

    #[test]
    fn format_range_is_noop_when_selection_misses_templates() {
        let source = r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "json"] = t"""{{"name": {value}}}"""
"#;

        let result = format_source_with_runner(
            source,
            Path::new("."),
            &MockFormatterRunner,
            Some(&[FormatRange {
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 5,
            }]),
        )
        .unwrap();

        assert!(!result.changed);
        assert_eq!(result.formatted_source, source);
    }
}
