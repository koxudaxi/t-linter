#[cfg(feature = "sql")]
use tree_sitter_sequel;

use anyhow::{Result};
use std::collections::HashMap;
use tracing::info;
use tree_sitter::{Parser, Language};
use tree_sitter_highlight::{Highlighter, HighlightConfiguration, HighlightEvent};
use crate::parser::{TemplateStringInfo, Expression, Location};



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
struct Placeholder {
    start: usize,
    end: usize,
}

impl TemplateHighlighter {
    pub fn new() -> Result<Self> {
        let highlight_names: Vec<String> = vec![
            "attribute",
            "comment",
            "constant",
            "constant.builtin",
            "constructor",
            "embedded",
            "function",
            "function.builtin",
            "keyword",
            "number",
            "operator",
            "property",
            "punctuation",
            "punctuation.bracket",
            "punctuation.delimiter",
            "punctuation.special",
            "string",
            "string.special",
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

        language_configs.insert("html".to_string(), LanguageConfig {
            language: tree_sitter_html::LANGUAGE.into(),
        });

        language_configs.insert("css".to_string(), LanguageConfig {
            language: tree_sitter_css::LANGUAGE.into(),
        });

        let js_config = LanguageConfig {
            language: tree_sitter_javascript::LANGUAGE.into(),
        };
        language_configs.insert("javascript".to_string(), js_config.clone());
        language_configs.insert("js".to_string(), js_config);

        language_configs.insert("json".to_string(), LanguageConfig {
            language: tree_sitter_json::LANGUAGE.into(),
        });

        #[cfg(feature = "sql")]
        language_configs.insert("sql".to_string(), LanguageConfig {
            language: tree_sitter_sequel::LANGUAGE.into(),
        });

        Ok(Self {
            highlighter: Highlighter::new(),
            language_configs,
            highlight_names,
        })
    }

    pub fn highlight_template(&mut self, template: &TemplateStringInfo) -> Result<Vec<HighlightedRange>> {
        let language = template.language.as_ref()
            .ok_or_else(|| anyhow::anyhow!("No language specified for template"))?;

        info!("Highlighting {} template, content: '{}'", language, template.content);

        let config = self.language_configs.get(language.to_lowercase().as_str())
            .ok_or_else(|| anyhow::anyhow!("Unsupported language: {}", language))?;

        let processed_content = template.content.clone();

        let mut parser = Parser::new();
        parser.set_language(&config.language)?;
        let tree = parser.parse(&processed_content, None)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse template content"))?;

        let mut temp_config = HighlightConfiguration::new(
            config.language.clone(),
            language,
            match language.to_lowercase().as_str() {
                "html" => tree_sitter_html::HIGHLIGHTS_QUERY,
                "css" => tree_sitter_css::HIGHLIGHTS_QUERY,
                "javascript" | "js" => tree_sitter_javascript::HIGHLIGHT_QUERY,
                "json" => tree_sitter_json::HIGHLIGHTS_QUERY,
                #[cfg(feature = "sql")]
                "sql" => tree_sitter_sequel::HIGHLIGHTS_QUERY,
                _ => return Err(anyhow::anyhow!("No highlight query for language: {}", language)),
            },
            "",
            "",
        )?;
        temp_config.configure(&self.highlight_names);

        let highlights = self.highlighter.highlight(
            &temp_config,
            processed_content.as_bytes(),
            None,
            |_| None,
        )?;

        let mut highlighted_ranges = Vec::new();
        let mut active_highlights: Vec<usize> = Vec::new();

        for event in highlights {
            match event? {
                HighlightEvent::Source { start, end } => {
                    for &highlight_index in &active_highlights {
                        highlighted_ranges.push(HighlightedRange {
                            start_byte: start,
                            end_byte: end,
                            highlight_name: self.highlight_names[highlight_index].clone(),
                            highlight_index,
                        });
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

        let mut search_start = 0;
        while let Some(pos) = template.content[search_start..].find("{}") {
            let absolute_pos = search_start + pos;
            highlighted_ranges.push(HighlightedRange {
                start_byte: absolute_pos,
                end_byte: absolute_pos + 2,
                highlight_name: "variable.parameter".to_string(),
                highlight_index: self.get_highlight_index("variable.parameter"),
            });
            search_start = absolute_pos + 2;
        }

        highlighted_ranges.sort_by_key(|r| r.start_byte);

        info!("Found {} highlight ranges", highlighted_ranges.len());
        for (i, range) in highlighted_ranges.iter().take(5).enumerate() {
            info!("  Range {}: {}..{} '{}'", i, range.start_byte, range.end_byte, range.highlight_name);
        }

        Ok(highlighted_ranges)
    }
    fn prepare_content_with_placeholders(
        &self,
        content: &str,
        expressions: &[Expression],
    ) -> (String, Vec<Placeholder>) {
        let mut processed = String::new();
        let mut placeholders = Vec::new();
        let mut last_end = 0;
        let mut expr_iter = expressions.iter();

        let mut search_start = 0;
        while let Some(pos) = content[search_start..].find("{}") {
            let absolute_pos = search_start + pos;

            processed.push_str(&content[last_end..absolute_pos]);

            let placeholder_text = if let Some(_expr) = expr_iter.next() {
                "_"
            } else {
                "_"
            };

            let placeholder_start = processed.len();
            processed.push_str(&placeholder_text);
            let placeholder_end = processed.len();

            placeholders.push(Placeholder {
                start: placeholder_start,
                end: placeholder_end,
            });

            last_end = absolute_pos + 2;
            search_start = absolute_pos + 2;
        }

        if last_end < content.len() {
            processed.push_str(&content[last_end..]);
        }

        (processed, placeholders)
    }
    fn sanitize_identifier(&self, expr: &str) -> String {
        let sanitized: String = expr.chars()
            .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
            .collect();

        if sanitized.chars().next().map_or(false, |c| c.is_numeric()) {
            format!("_{}", sanitized)
        } else if sanitized.is_empty() {
            "placeholder".to_string()
        } else {
            sanitized
        }
    }


    fn get_highlight_index(&self, name: &str) -> usize {
        self.highlight_names.iter()
            .position(|n| n == name)
            .unwrap_or(0)
    }

    fn byte_to_line_col_in_content(&self, content: &str, byte_offset: usize) -> (usize, usize) {
        let mut line = 0;
        let mut col = 0;
        let mut current_byte = 0;

        for ch in content.chars() {
            if current_byte >= byte_offset {
                break;
            }

            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }

            current_byte += ch.len_utf8();
        }

        (line, col)
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
        let actual_content = &template.raw_content[prefix_len..template.raw_content.len() - suffix_len];

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

            info!("Range {}: {} content[{}..{}]='{}' -> line {} col {}", 
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
    fn create_placeholder_mappings(
        &self,
        content: &str,
        expressions: &[Expression],
    ) -> Vec<(usize, usize, usize, usize)> {
        let mut mappings = Vec::new();
        let mut search_start = 0;
        let mut expr_iter = expressions.iter();

        while let Some(pos) = content[search_start..].find("{}") {
            let absolute_pos = search_start + pos;

            if let Some(expr) = expr_iter.next() {
                let placeholder_text = format!("__{}", self.sanitize_identifier(&expr.content));
                let placeholder_start = absolute_pos;
                let placeholder_end = placeholder_start + placeholder_text.len();

                mappings.push((absolute_pos, absolute_pos + 2, placeholder_start, placeholder_end));
            }

            search_start = absolute_pos + 2;
        }

        mappings
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
            if template_idx + 1 < template_bytes.len() &&
                template_bytes[template_idx] == b'{' &&
                template_bytes[template_idx + 1] == b'}' {
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
            "function" | "function.builtin" => 12,
            "variable" | "variable.builtin" | "variable.parameter" => 8,
            "string" | "string.special" => 18,
            "number" => 19,
            "comment" => 17,
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
    use crate::parser::{Location, TemplateStringFlags};

    #[test]
    fn test_html_highlighting() {
        let mut highlighter = TemplateHighlighter::new().unwrap();

        let template = TemplateStringInfo {
            content: "<div class=\"test\">{}</div>".to_string(),
            raw_content: r#"t"<div class=\"test\">{value}</div>""#.to_string(),
            variable_name: Some("html".to_string()),
            function_name: None,
            language: Some("html".to_string()),
            location: Location {
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 40,
            },
            expressions: vec![Expression {
                content: "value".to_string(),
                location: Location {
                    start_line: 1,
                    start_column: 20,
                    end_line: 1,
                    end_column: 25,
                },
            }],
            flags: TemplateStringFlags::default(),
        };

        let ranges = highlighter.highlight_template(&template).unwrap();

        assert!(ranges.iter().any(|r| r.highlight_name == "tag"));
        assert!(ranges.iter().any(|r| r.highlight_name == "attribute"));
        assert!(ranges.iter().any(|r| r.highlight_name == "variable.parameter"));
    }

    #[test]
    fn test_multiline_html_highlighting() {
        let mut highlighter = TemplateHighlighter::new().unwrap();

        let mut flags = TemplateStringFlags::default();
        flags.is_triple = true;

        let template = TemplateStringInfo {
            content: "<div>\n  <span>{}</span>\n  {}\n</div>".to_string(),
            raw_content: r#"t"""<div>
  <span>{name}</span>
  {123}
</div>""""#.to_string(),
            variable_name: Some("html".to_string()),
            function_name: None,
            language: Some("html".to_string()),
            location: Location {
                start_line: 1,
                start_column: 1,
                end_line: 4,
                end_column: 10,
            },
            expressions: vec![
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
        };

        let ranges = highlighter.highlight_template(&template).unwrap();

        assert!(ranges.iter().any(|r| r.highlight_name == "tag"));
        assert_eq!(ranges.iter().filter(|r| r.highlight_name == "variable.parameter").count(), 2);

        let tokens = highlighter.to_lsp_tokens(ranges, &template);
        assert!(!tokens.is_empty());

        let lines: Vec<_> = tokens.iter().map(|t| t.0).collect();
        assert!(lines.iter().max().unwrap() > lines.iter().min().unwrap());
    }
}