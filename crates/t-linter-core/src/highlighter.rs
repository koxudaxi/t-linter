#[cfg(feature = "sql")]
use tree_sitter_sequel;

use crate::parser::{TemplatePart, TemplateStringInfo};
use anyhow::Result;
use std::collections::HashMap;
use tracing::info;
use tree_sitter::{Language, Parser};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

#[derive(Debug, Clone)]
pub struct HighlightedRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub highlight_name: String,
    pub highlight_index: usize,
}

pub struct TemplateHighlighter {
    highlighter: Highlighter,
    language_configs: HashMap<String, LanguageConfig>,
    highlight_names: Vec<String>,
}

#[derive(Clone)]
struct LanguageConfig {
    language: Language,
}

#[derive(Debug, Clone)]
struct ProcessedHighlightContent {
    content: String,
    processed_to_original: Vec<usize>,
    placeholders: Vec<Placeholder>,
}

#[derive(Debug, Clone)]
struct Placeholder {
    start: usize,
    end: usize,
}

impl TemplateHighlighter {
    pub fn new() -> Result<Self> {
        let highlight_names: Vec<String> = vec![
            "attribute",
            "boolean",
            "comment",
            "constant",
            "constant.builtin",
            "constructor",
            "embedded",
            "function",
            "function.builtin",
            "keyword",
            "label",
            "number",
            "operator",
            "property",
            "punctuation",
            "punctuation.bracket",
            "punctuation.delimiter",
            "punctuation.special",
            "string",
            "string.special",
            "string.special.key",
            "tag",
            "type",
            "type.builtin",
            "variable",
            "variable.builtin",
            "variable.parameter",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let mut language_configs = HashMap::new();

        language_configs.insert(
            "html".to_string(),
            LanguageConfig {
                language: tree_sitter_html::LANGUAGE.into(),
            },
        );
        language_configs.insert(
            "thtml".to_string(),
            LanguageConfig {
                language: tree_sitter_html::LANGUAGE.into(),
            },
        );
        language_configs.insert(
            "tdom".to_string(),
            LanguageConfig {
                language: tree_sitter_html::LANGUAGE.into(),
            },
        );

        language_configs.insert(
            "css".to_string(),
            LanguageConfig {
                language: tree_sitter_css::LANGUAGE.into(),
            },
        );

        let js_config = LanguageConfig {
            language: tree_sitter_javascript::LANGUAGE.into(),
        };
        language_configs.insert("javascript".to_string(), js_config.clone());
        language_configs.insert("js".to_string(), js_config);

        language_configs.insert(
            "json".to_string(),
            LanguageConfig {
                language: tree_sitter_json::LANGUAGE.into(),
            },
        );

        let yaml_config = LanguageConfig {
            language: tree_sitter_yaml::LANGUAGE.into(),
        };
        language_configs.insert("yaml".to_string(), yaml_config.clone());
        language_configs.insert("yml".to_string(), yaml_config);

        language_configs.insert(
            "toml".to_string(),
            LanguageConfig {
                language: tree_sitter_toml_ng::LANGUAGE.into(),
            },
        );

        #[cfg(feature = "sql")]
        language_configs.insert(
            "sql".to_string(),
            LanguageConfig {
                language: tree_sitter_sequel::LANGUAGE.into(),
            },
        );

        Ok(Self {
            highlighter: Highlighter::new(),
            language_configs,
            highlight_names,
        })
    }

    pub fn supports_language(&self, language: &str) -> bool {
        self.language_configs
            .contains_key(language.to_ascii_lowercase().as_str())
    }

    pub fn highlight_template(
        &mut self,
        template: &TemplateStringInfo,
    ) -> Result<Vec<HighlightedRange>> {
        let language = template
            .language
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No language specified for template"))?;

        info!(
            "Highlighting {} template, content: '{}'",
            language, template.content
        );

        let config = self
            .language_configs
            .get(language.to_lowercase().as_str())
            .ok_or_else(|| anyhow::anyhow!("Unsupported language: {}", language))?;

        let processed = self.prepare_content_for_highlighting(template, language);
        let processed_content = processed.content.as_str();

        let mut parser = Parser::new();
        parser.set_language(&config.language)?;
        let _tree = parser
            .parse(processed_content, None)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse template content"))?;

        let mut temp_config = HighlightConfiguration::new(
            config.language.clone(),
            language,
            match language.to_lowercase().as_str() {
                "html" => tree_sitter_html::HIGHLIGHTS_QUERY,
                "thtml" => tree_sitter_html::HIGHLIGHTS_QUERY,
                "tdom" => tree_sitter_html::HIGHLIGHTS_QUERY,
                "css" => tree_sitter_css::HIGHLIGHTS_QUERY,
                "javascript" | "js" => tree_sitter_javascript::HIGHLIGHT_QUERY,
                "json" => tree_sitter_json::HIGHLIGHTS_QUERY,
                "yaml" | "yml" => tree_sitter_yaml::HIGHLIGHTS_QUERY,
                "toml" => tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
                #[cfg(feature = "sql")]
                "sql" => tree_sitter_sequel::HIGHLIGHTS_QUERY,
                _ => {
                    return Err(anyhow::anyhow!(
                        "No highlight query for language: {}",
                        language
                    ));
                }
            },
            "",
            "",
        )?;
        temp_config.configure(&self.highlight_names);

        let highlights =
            self.highlighter
                .highlight(&temp_config, processed_content.as_bytes(), None, |_| None)?;

        let mut highlighted_ranges = Vec::new();
        let mut active_highlights: Vec<usize> = Vec::new();

        for event in highlights {
            match event? {
                HighlightEvent::Source { start, end } => {
                    let start_byte =
                        Self::map_processed_offset(&processed.processed_to_original, start);
                    let end_byte =
                        Self::map_processed_offset(&processed.processed_to_original, end);

                    for &highlight_index in &active_highlights {
                        for (segment_start, segment_end) in Self::subtract_placeholder_ranges(
                            start_byte,
                            end_byte,
                            &processed.placeholders,
                        ) {
                            if segment_start < segment_end {
                                highlighted_ranges.push(HighlightedRange {
                                    start_byte: segment_start,
                                    end_byte: segment_end,
                                    highlight_name: self.highlight_names[highlight_index].clone(),
                                    highlight_index,
                                });
                            }
                        }
                    }
                }
                HighlightEvent::HighlightStart(highlight) => {
                    active_highlights.push(highlight.0);
                }
                HighlightEvent::HighlightEnd => {
                    active_highlights.pop();
                }
            }
        }

        for placeholder in &processed.placeholders {
            highlighted_ranges.push(HighlightedRange {
                start_byte: placeholder.start,
                end_byte: placeholder.end,
                highlight_name: "variable.parameter".to_string(),
                highlight_index: self.get_highlight_index("variable.parameter"),
            });
        }

        highlighted_ranges.sort_by_key(|r| r.start_byte);

        info!("Found {} highlight ranges", highlighted_ranges.len());
        for (i, range) in highlighted_ranges.iter().take(5).enumerate() {
            info!(
                "  Range {}: {}..{} '{}'",
                i, range.start_byte, range.end_byte, range.highlight_name
            );
        }

        Ok(highlighted_ranges)
    }

    fn prepare_content_for_highlighting(
        &self,
        template: &TemplateStringInfo,
        language: &str,
    ) -> ProcessedHighlightContent {
        if language.eq_ignore_ascii_case("tdom") {
            return Self::prepare_tdom_content_for_highlighting(&template.content);
        }

        let mut processed = String::new();
        let mut processed_to_original = vec![0];
        let mut placeholders = Vec::new();
        let mut original_offset = 0;
        let placeholder_text = Self::placeholder_text_for_language(language);

        for part in &template.parts {
            match part {
                TemplatePart::Static(part) => {
                    Self::append_original_segment(
                        &mut processed,
                        &mut processed_to_original,
                        &part.text,
                        original_offset,
                    );
                    original_offset += part.text.len();
                }
                TemplatePart::Interpolation(_) => {
                    Self::append_placeholder_segment(
                        &mut processed,
                        &mut processed_to_original,
                        placeholder_text,
                        original_offset,
                        original_offset + 2,
                    );
                    placeholders.push(Placeholder {
                        start: original_offset,
                        end: original_offset + 2,
                    });
                    original_offset += 2;
                }
            }
        }

        ProcessedHighlightContent {
            content: processed,
            processed_to_original,
            placeholders,
        }
    }

    fn prepare_tdom_content_for_highlighting(content: &str) -> ProcessedHighlightContent {
        let mut processed = String::new();
        let mut processed_to_original = vec![0];
        let mut placeholders = Vec::new();
        let mut search_start = 0;

        while let Some(pos) = content[search_start..].find("{}") {
            let absolute_pos = search_start + pos;
            Self::append_original_segment(
                &mut processed,
                &mut processed_to_original,
                &content[search_start..absolute_pos],
                search_start,
            );

            let placeholder_text = if content[..absolute_pos].ends_with("</")
                || content[..absolute_pos].ends_with('<')
            {
                "tdom_component"
            } else {
                "t_linter_expr"
            };
            Self::append_placeholder_segment(
                &mut processed,
                &mut processed_to_original,
                placeholder_text,
                absolute_pos,
                absolute_pos + 2,
            );
            placeholders.push(Placeholder {
                start: absolute_pos,
                end: absolute_pos + 2,
            });
            search_start = absolute_pos + 2;
        }

        Self::append_original_segment(
            &mut processed,
            &mut processed_to_original,
            &content[search_start..],
            search_start,
        );

        ProcessedHighlightContent {
            content: processed,
            processed_to_original,
            placeholders,
        }
    }

    fn get_highlight_index(&self, name: &str) -> usize {
        self.highlight_names
            .iter()
            .position(|n| n == name)
            .unwrap_or(0)
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

    fn subtract_placeholder_ranges(
        start: usize,
        end: usize,
        placeholders: &[Placeholder],
    ) -> Vec<(usize, usize)> {
        let mut segments = vec![(start, end)];

        for placeholder in placeholders {
            let mut next_segments = Vec::new();

            for (segment_start, segment_end) in segments {
                if segment_end <= placeholder.start || placeholder.end <= segment_start {
                    next_segments.push((segment_start, segment_end));
                    continue;
                }

                if segment_start < placeholder.start {
                    next_segments.push((segment_start, placeholder.start));
                }
                if placeholder.end < segment_end {
                    next_segments.push((placeholder.end, segment_end));
                }
            }

            segments = next_segments;
            if segments.is_empty() {
                break;
            }
        }

        segments
    }

    fn placeholder_text_for_language(language: &str) -> &'static str {
        match language.to_ascii_lowercase().as_str() {
            "html" | "thtml" | "tdom" => "t_linter_expr",
            "css" | "javascript" | "js" | "json" | "yaml" | "yml" | "toml" | "sql" => "0",
            _ => "t_linter_expr",
        }
    }
    pub fn to_lsp_tokens(
        &self,
        ranges: Vec<HighlightedRange>,
        template: &TemplateStringInfo,
    ) -> Vec<(u32, u32, u32, u32, u32)> {
        let mut tokens = Vec::new();

        let template_start_line = template.location.start_line - 1;
        let template_start_col = template.location.start_column - 1;

        let prefix_len = self.calculate_template_content_offset(&template.raw_content);

        for expr in &template.expressions {
            tokens.push((
                (expr.location.start_line - 1) as u32,
                (expr.location.start_column - 1) as u32,
                (expr.location.end_column - expr.location.start_column) as u32,
                self.token_type_to_index("variable.parameter"),
                0,
            ));
        }

        let suffix_len = if template.flags.is_triple { 3 } else { 1 };
        let actual_content =
            &template.raw_content[prefix_len..template.raw_content.len() - suffix_len];

        for (i, range) in ranges.iter().enumerate() {
            if range.highlight_name == "variable.parameter" {
                continue;
            }

            let (doc_line, doc_col) = self.map_template_position_to_document(
                &template.content,
                actual_content,
                range.start_byte,
                template_start_line,
                template_start_col,
                prefix_len,
            );

            let length = range.end_byte - range.start_byte;

            info!(
                "Range {}: {} content[{}..{}]='{}' -> line {} col {}",
                i,
                range.highlight_name,
                range.start_byte,
                range.end_byte,
                &template.content[range.start_byte..range.end_byte].replace('\n', "\\n"),
                doc_line,
                doc_col
            );

            if length > 0 {
                tokens.push((
                    doc_line as u32,
                    doc_col as u32,
                    length as u32,
                    self.token_type_to_index(&range.highlight_name),
                    0,
                ));
            }
        }

        tokens
    }
    fn calculate_template_content_offset(&self, raw_content: &str) -> usize {
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
        &self,
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

        for i in 0..actual_idx {
            if actual_bytes[i] == b'\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }

        (line, col)
    }

    fn token_type_to_index(&self, highlight_name: &str) -> u32 {
        match highlight_name {
            "keyword" => 15,
            "boolean" => 19,
            "function" | "function.builtin" => 12,
            "variable" | "variable.builtin" | "variable.parameter" => 8,
            "string" | "string.special" => 18,
            "string.special.key" => 9,
            "number" => 19,
            "comment" => 17,
            "label" => 22,
            "type" | "type.builtin" => 1,
            "class" | "constructor" => 2,
            "property" => 9,
            "tag" => 2,
            "attribute" => 9,
            "operator" => 21,
            "punctuation" | "punctuation.bracket" | "punctuation.delimiter" => 21,
            _ => 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{
        Expression, InterpolationInfo, Location, StaticTextSegment, TemplatePart,
        TemplateStringFlags, TemplateStringInfo, TemplateStringParser,
    };

    fn make_template(
        content: &str,
        raw_content: &str,
        language: &str,
        location: Location,
        expressions: Vec<Expression>,
        flags: TemplateStringFlags,
    ) -> TemplateStringInfo {
        let mut parts = Vec::new();
        let mut search_start = 0;
        let mut interpolation_index = 0;

        while let Some(pos) = content[search_start..].find("{}") {
            let absolute_pos = search_start + pos;
            let before = &content[search_start..absolute_pos];
            if !before.is_empty() {
                parts.push(TemplatePart::Static(StaticTextSegment {
                    text: before.to_string(),
                }));
            }
            let expression = expressions
                .get(interpolation_index)
                .map(|expr| expr.content.clone())
                .unwrap_or_else(|| format!("slot_{interpolation_index}"));
            parts.push(TemplatePart::Interpolation(InterpolationInfo {
                expression: expression.clone(),
                conversion: None,
                format_spec: String::new(),
                raw_source: format!("{{{expression}}}"),
                location: expressions
                    .get(interpolation_index)
                    .map(|expr| expr.location.clone())
                    .unwrap_or_else(|| location.clone()),
                interpolation_index,
            }));
            interpolation_index += 1;
            search_start = absolute_pos + 2;
        }

        if search_start < content.len() {
            parts.push(TemplatePart::Static(StaticTextSegment {
                text: content[search_start..].to_string(),
            }));
        }
        if parts.is_empty() {
            parts.push(TemplatePart::Static(StaticTextSegment {
                text: String::new(),
            }));
        }

        let string_start = if flags.is_triple { "t\"\"\"" } else { "t\"" }.to_string();
        let string_end = if flags.is_triple { "\"\"\"" } else { "\"" }.to_string();

        TemplateStringInfo {
            content: content.to_string(),
            raw_content: raw_content.to_string(),
            variable_name: Some(language.to_string()),
            function_name: None,
            language: Some(language.to_string()),
            string_start,
            string_end,
            location,
            formatting_wrapper_location: None,
            expressions,
            parts,
            flags,
        }
    }

    fn range_overlaps(range: &HighlightedRange, start: usize, end: usize) -> bool {
        range.end_byte > start && end > range.start_byte
    }

    fn parse_single_template(source: &str) -> TemplateStringInfo {
        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();
        assert_eq!(templates.len(), 1);
        templates.into_iter().next().unwrap()
    }

    fn placeholder_ranges(template: &TemplateStringInfo) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        let mut offset = 0;

        for part in &template.parts {
            match part {
                TemplatePart::Static(part) => offset += part.text.len(),
                TemplatePart::Interpolation(_) => {
                    ranges.push((offset, offset + 2));
                    offset += 2;
                }
            }
        }

        ranges
    }

    fn assert_non_variable_ranges_avoid_placeholders(
        ranges: &[HighlightedRange],
        template: &TemplateStringInfo,
    ) {
        for (start, end) in placeholder_ranges(template) {
            assert!(
                ranges
                    .iter()
                    .filter(|r| r.highlight_name != "variable.parameter")
                    .all(|r| !range_overlaps(r, start, end))
            );
        }
    }

    fn assert_expression_tokens_match_template(
        tokens: &[(u32, u32, u32, u32, u32)],
        template: &TemplateStringInfo,
    ) {
        for expr in &template.expressions {
            assert!(tokens.iter().any(|token| {
                token.0 == (expr.location.start_line - 1) as u32
                    && token.1 == (expr.location.start_column - 1) as u32
                    && token.2 == (expr.location.end_column - expr.location.start_column) as u32
                    && token.3 == 8
            }));
        }
    }

    #[test]
    fn test_html_highlighting() {
        let mut highlighter = TemplateHighlighter::new().unwrap();

        let template = make_template(
            "<div class=\"test\">{}</div>",
            r#"t"<div class=\"test\">{value}</div>""#,
            "html",
            Location {
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 40,
            },
            vec![Expression {
                content: "value".to_string(),
                location: Location {
                    start_line: 1,
                    start_column: 20,
                    end_line: 1,
                    end_column: 25,
                },
            }],
            TemplateStringFlags::default(),
        );

        let ranges = highlighter.highlight_template(&template).unwrap();

        assert!(ranges.iter().any(|r| r.highlight_name == "tag"));
        assert!(ranges.iter().any(|r| r.highlight_name == "attribute"));
        assert!(
            ranges
                .iter()
                .any(|r| r.highlight_name == "variable.parameter")
        );
    }

    #[test]
    fn test_thtml_highlighting() {
        let mut highlighter = TemplateHighlighter::new().unwrap();

        let template = make_template(
            "<Card title=\"{}\"><Badge>{}</Badge></Card>",
            r#"t"<Card title=\"{title}\"><Badge>{status}</Badge></Card>""#,
            "thtml",
            Location {
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 58,
            },
            vec![
                Expression {
                    content: "title".to_string(),
                    location: Location {
                        start_line: 1,
                        start_column: 15,
                        end_line: 1,
                        end_column: 20,
                    },
                },
                Expression {
                    content: "status".to_string(),
                    location: Location {
                        start_line: 1,
                        start_column: 30,
                        end_line: 1,
                        end_column: 36,
                    },
                },
            ],
            TemplateStringFlags::default(),
        );

        let ranges = highlighter.highlight_template(&template).unwrap();

        assert!(ranges.iter().any(|r| r.highlight_name == "tag"));
        assert!(ranges.iter().any(|r| r.highlight_name == "attribute"));
        assert!(
            ranges
                .iter()
                .any(|r| r.highlight_name == "variable.parameter")
        );
    }

    #[test]
    fn test_tdom_highlighting() {
        let mut highlighter = TemplateHighlighter::new().unwrap();

        let template = make_template(
            "<{} title={}><span>{}</span></{}>",
            r#"t"<{Card} title={title}><span>{status}</span></{Card}>""#,
            "tdom",
            Location {
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 60,
            },
            vec![
                Expression {
                    content: "Card".to_string(),
                    location: Location {
                        start_line: 1,
                        start_column: 3,
                        end_line: 1,
                        end_column: 7,
                    },
                },
                Expression {
                    content: "title".to_string(),
                    location: Location {
                        start_line: 1,
                        start_column: 15,
                        end_line: 1,
                        end_column: 20,
                    },
                },
                Expression {
                    content: "status".to_string(),
                    location: Location {
                        start_line: 1,
                        start_column: 29,
                        end_line: 1,
                        end_column: 35,
                    },
                },
                Expression {
                    content: "Card".to_string(),
                    location: Location {
                        start_line: 1,
                        start_column: 44,
                        end_line: 1,
                        end_column: 48,
                    },
                },
            ],
            TemplateStringFlags::default(),
        );

        let ranges = highlighter.highlight_template(&template).unwrap();

        assert!(ranges.iter().any(|r| r.highlight_name == "tag"));
        assert!(ranges.iter().any(|r| r.highlight_name == "attribute"));
        assert_eq!(
            ranges
                .iter()
                .filter(|r| r.highlight_name == "variable.parameter")
                .count(),
            4
        );
    }

    #[test]
    fn test_multiline_html_highlighting() {
        let mut highlighter = TemplateHighlighter::new().unwrap();

        let mut flags = TemplateStringFlags::default();
        flags.is_triple = true;

        let template = make_template(
            "<div>\n  <span>{}</span>\n  {}\n</div>",
            r#"t"""<div>
  <span>{name}</span>
  {123}
</div>""""#,
            "html",
            Location {
                start_line: 1,
                start_column: 1,
                end_line: 4,
                end_column: 10,
            },
            vec![
                Expression {
                    content: "name".to_string(),
                    location: Location {
                        start_line: 2,
                        start_column: 10,
                        end_line: 2,
                        end_column: 14,
                    },
                },
                Expression {
                    content: "123".to_string(),
                    location: Location {
                        start_line: 3,
                        start_column: 4,
                        end_line: 3,
                        end_column: 7,
                    },
                },
            ],
            flags,
        );

        let ranges = highlighter.highlight_template(&template).unwrap();

        assert!(ranges.iter().any(|r| r.highlight_name == "tag"));
        assert_eq!(
            ranges
                .iter()
                .filter(|r| r.highlight_name == "variable.parameter")
                .count(),
            2
        );

        let tokens = highlighter.to_lsp_tokens(ranges, &template);
        assert!(!tokens.is_empty());

        let lines: Vec<_> = tokens.iter().map(|t| t.0).collect();
        assert!(lines.iter().max().unwrap() > lines.iter().min().unwrap());
    }

    #[test]
    fn test_json_highlighting() {
        let mut highlighter = TemplateHighlighter::new().unwrap();

        let template = make_template(
            r#"{"name": "codex", "count": 3, "value": {}, "enabled": true}"#,
            r#"t"{\"name\": \"codex\", \"count\": 3, \"value\": {value}, \"enabled\": true}""#,
            "json",
            Location {
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 52,
            },
            vec![Expression {
                content: "value".to_string(),
                location: Location {
                    start_line: 1,
                    start_column: 30,
                    end_line: 1,
                    end_column: 35,
                },
            }],
            TemplateStringFlags::default(),
        );

        let ranges = highlighter.highlight_template(&template).unwrap();

        assert!(ranges.iter().any(|r| r.highlight_name == "string"));
        assert!(ranges.iter().any(|r| r.highlight_name == "number"));
        assert!(
            ranges
                .iter()
                .any(|r| r.highlight_name == "constant.builtin")
        );
        assert!(
            ranges
                .iter()
                .any(|r| r.highlight_name == "variable.parameter")
        );
    }

    #[test]
    fn test_yaml_highlighting_with_yml_alias() {
        let mut highlighter = TemplateHighlighter::new().unwrap();

        let template = make_template(
            "name: codex\nenabled: true\nvalue: {}\n",
            "t\"name: codex\\nenabled: true\\nvalue: {value}\\n\"",
            "yml",
            Location {
                start_line: 1,
                start_column: 1,
                end_line: 3,
                end_column: 10,
            },
            vec![Expression {
                content: "value".to_string(),
                location: Location {
                    start_line: 3,
                    start_column: 8,
                    end_line: 3,
                    end_column: 13,
                },
            }],
            TemplateStringFlags::default(),
        );

        let ranges = highlighter.highlight_template(&template).unwrap();

        assert!(ranges.iter().any(|r| r.highlight_name == "property"));
        assert!(ranges.iter().any(|r| r.highlight_name == "boolean"));
        assert!(
            ranges
                .iter()
                .any(|r| r.highlight_name == "variable.parameter")
        );
    }

    #[test]
    fn test_css_highlighting_keeps_property_aligned_around_interpolation() {
        let mut highlighter = TemplateHighlighter::new().unwrap();

        let template = make_template(
            "padding: {}px;",
            r#"t"padding: {padding}px;""#,
            "css",
            Location {
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 24,
            },
            vec![Expression {
                content: "padding".to_string(),
                location: Location {
                    start_line: 1,
                    start_column: 12,
                    end_line: 1,
                    end_column: 19,
                },
            }],
            TemplateStringFlags::default(),
        );

        let ranges = highlighter.highlight_template(&template).unwrap();
        let placeholder_start = template.content.find("{}").unwrap();
        let placeholder_end = placeholder_start + 2;

        assert!(ranges.iter().any(|r| {
            r.highlight_name == "property"
                && &template.content[r.start_byte..r.end_byte] == "padding"
        }));
        assert!(
            ranges
                .iter()
                .filter(|r| r.highlight_name != "variable.parameter")
                .all(|r| !range_overlaps(r, placeholder_start, placeholder_end))
        );
    }

    #[test]
    fn test_yaml_highlighting_keeps_property_aligned_around_interpolation() {
        let mut highlighter = TemplateHighlighter::new().unwrap();

        let template = make_template(
            "name: {}\n",
            "t\"name: {app_name}\\n\"",
            "yaml",
            Location {
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 19,
            },
            vec![Expression {
                content: "app_name".to_string(),
                location: Location {
                    start_line: 1,
                    start_column: 10,
                    end_line: 1,
                    end_column: 18,
                },
            }],
            TemplateStringFlags::default(),
        );

        let ranges = highlighter.highlight_template(&template).unwrap();
        let placeholder_start = template.content.find("{}").unwrap();
        let placeholder_end = placeholder_start + 2;

        assert!(ranges.iter().any(|r| {
            r.highlight_name == "property" && &template.content[r.start_byte..r.end_byte] == "name"
        }));
        assert!(
            ranges
                .iter()
                .filter(|r| r.highlight_name != "variable.parameter")
                .all(|r| !range_overlaps(r, placeholder_start, placeholder_end))
        );
    }

    #[test]
    fn test_toml_highlighting() {
        let mut highlighter = TemplateHighlighter::new().unwrap();

        let template = make_template(
            "title = \"T-Linter\"\nenabled = true\nvalue = {}\n",
            "t\"title = \\\"T-Linter\\\"\\nenabled = true\\nvalue = {value}\\n\"",
            "toml",
            Location {
                start_line: 1,
                start_column: 1,
                end_line: 3,
                end_column: 11,
            },
            vec![Expression {
                content: "value".to_string(),
                location: Location {
                    start_line: 3,
                    start_column: 9,
                    end_line: 3,
                    end_column: 14,
                },
            }],
            TemplateStringFlags::default(),
        );

        let ranges = highlighter.highlight_template(&template).unwrap();

        assert!(ranges.iter().any(|r| r.highlight_name == "property"));
        assert!(ranges.iter().any(|r| r.highlight_name == "string"));
        assert!(ranges.iter().any(|r| r.highlight_name == "boolean"));
        assert!(ranges.iter().any(|r| r.highlight_name == "operator"));
        assert!(
            ranges
                .iter()
                .any(|r| r.highlight_name == "variable.parameter")
        );
    }

    #[test]
    fn test_toml_string_highlighting_does_not_cover_interpolation() {
        let mut highlighter = TemplateHighlighter::new().unwrap();

        let template = make_template(
            "name = \"{}\"\n",
            "t\"name = \\\"{project_name}\\\"\\n\"",
            "toml",
            Location {
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 29,
            },
            vec![Expression {
                content: "project_name".to_string(),
                location: Location {
                    start_line: 1,
                    start_column: 13,
                    end_line: 1,
                    end_column: 25,
                },
            }],
            TemplateStringFlags::default(),
        );

        let ranges = highlighter.highlight_template(&template).unwrap();
        let placeholder_start = template.content.find("{}").unwrap();
        let placeholder_end = placeholder_start + 2;

        assert!(ranges.iter().any(|r| r.highlight_name == "string"));
        assert!(
            ranges
                .iter()
                .filter(|r| r.highlight_name != "variable.parameter")
                .all(|r| !range_overlaps(r, placeholder_start, placeholder_end))
        );
    }

    #[test]
    fn test_literal_braces_are_not_marked_as_interpolations() {
        let mut highlighter = TemplateHighlighter::new().unwrap();

        let template = TemplateStringInfo {
            content: "literal {} braces".to_string(),
            raw_content: r#"t"literal {{}} braces""#.to_string(),
            variable_name: Some("html".to_string()),
            function_name: None,
            language: Some("html".to_string()),
            string_start: "t\"".to_string(),
            string_end: "\"".to_string(),
            location: Location {
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 23,
            },
            formatting_wrapper_location: None,
            expressions: vec![],
            parts: vec![TemplatePart::Static(StaticTextSegment {
                text: "literal {} braces".to_string(),
            })],
            flags: TemplateStringFlags::default(),
        };

        let ranges = highlighter.highlight_template(&template).unwrap();

        assert!(
            !ranges
                .iter()
                .any(|r| r.highlight_name == "variable.parameter")
        );
    }

    #[test]
    fn test_css_semantic_tokens_handle_escaped_braces_and_long_expressions() {
        let mut highlighter = TemplateHighlighter::new().unwrap();
        let template = parse_single_template(
            r#"from typing import Annotated
from string.templatelib import Template

theme = {"spacing": {"lg": 24}}

styles: Annotated[Template, "css"] = t"""
.card {{
    content: "{{}}";
    padding: {theme["spacing"]["lg"]}px;
}}
"""
"#,
        );

        assert_eq!(template.content.matches("{}").count(), 2);

        let ranges = highlighter.highlight_template(&template).unwrap();
        assert_eq!(
            ranges
                .iter()
                .filter(|r| r.highlight_name == "variable.parameter")
                .count(),
            1
        );
        assert_non_variable_ranges_avoid_placeholders(&ranges, &template);

        let tokens = highlighter.to_lsp_tokens(ranges, &template);
        assert_expression_tokens_match_template(&tokens, &template);
    }

    #[test]
    fn test_json_semantic_tokens_handle_nested_objects_and_escaped_braces() {
        let mut highlighter = TemplateHighlighter::new().unwrap();
        let template = parse_single_template(
            r#"from typing import Annotated
from string.templatelib import Template

project_name = "demo-project"
payload = {"nested": {"enabled": True}}

config: Annotated[Template, "json"] = t"""
{
  "meta": {
    "pattern": "{{}}"
  },
  "payload": {payload["nested"]}
}
"""
"#,
        );

        assert_eq!(template.expressions.len(), 1);

        let ranges = highlighter.highlight_template(&template).unwrap();
        assert_eq!(
            ranges
                .iter()
                .filter(|r| r.highlight_name == "variable.parameter")
                .count(),
            1
        );
        assert_non_variable_ranges_avoid_placeholders(&ranges, &template);

        let tokens = highlighter.to_lsp_tokens(ranges, &template);
        assert_expression_tokens_match_template(&tokens, &template);
    }
}
