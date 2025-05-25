use anyhow::{Context, Result};
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};
use tracing::info;

pub struct TemplateStringParser {
    parser: Parser,
}

impl TemplateStringParser {
    pub fn new() -> Result<Self> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .context("Failed to set Python language")?;

        Ok(Self { parser })
    }

    pub fn find_template_strings(&mut self, source: &str) -> Result<Vec<TemplateStringInfo>> {
        let tree = self
            .parser
            .parse(source, None)
            .context("Failed to parse source")?;

        let mut templates = Vec::new();
        self.find_strings_with_query(&tree, source, &mut templates)?;

        Ok(templates)
    }

    fn find_strings_with_query(
        &self,
        tree: &Tree,
        source: &str,
        templates: &mut Vec<TemplateStringInfo>,
    ) -> Result<()> {
        let query_str = r#"
        (expression_statement
            (string) @string
        )
        
        (assignment
            left: (identifier) @var_name
            type: (_) @type_annotation
            right: (string) @string
        )
        
        (assignment
            left: (identifier) @var_name
            right: (string) @string
        )
        
        (call
            function: (identifier) @func_name
            arguments: (argument_list
                (string) @string
            )
        )
    "#;

        let query = Query::new(&tree_sitter_python::LANGUAGE.into(), query_str)
            .context("Failed to create query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());

        let mut processed_nodes = std::collections::HashSet::new();

        while let Some(match_) = matches.next() {
            let mut string_node = None;
            let mut var_name = None;
            let mut type_annotation = None;
            let mut func_name = None;

            for capture in match_.captures {
                let name = query.capture_names()[capture.index as usize];
                match name {
                    "string" => string_node = Some(capture.node),
                    "var_name" => var_name = Some(capture.node.utf8_text(source.as_bytes())?),
                    "type_annotation" => type_annotation = Some(capture.node),
                    "func_name" => func_name = Some(capture.node.utf8_text(source.as_bytes())?),
                    _ => {}
                }
            }

            if let Some(node) = string_node {
                let node_id = node.id();
                if processed_nodes.contains(&node_id) {
                    continue;
                }

                if let Some(start_node) = node
                    .child_by_field_name("string_start")
                    .or_else(|| node.child(0))
                {
                    let start_text = start_node.utf8_text(source.as_bytes())?;

                    if start_text.starts_with('t') || start_text.starts_with('T') {
                        processed_nodes.insert(node_id);
                        let info = self.extract_template_info(
                            node,
                            source,
                            var_name,
                            type_annotation,
                            func_name,
                        )?;
                        templates.push(info);
                    }
                }
            }
        }

        Ok(())
    }

    fn extract_template_info(
        &self,
        node: Node,
        source: &str,
        var_name: Option<&str>,
        type_annotation: Option<Node>,
        func_name: Option<&str>,
    ) -> Result<TemplateStringInfo> {
        let start_position = node.start_position();
        let end_position = node.end_position();

        let string_start = node
            .child(0)
            .ok_or_else(|| anyhow::anyhow!("No string_start node"))?;
        let _string_end = node
            .child(node.child_count() - 1)
            .ok_or_else(|| anyhow::anyhow!("No string_end node"))?;

        let start_text = string_start.utf8_text(source.as_bytes())?;
        let raw_content = node.utf8_text(source.as_bytes())?;

        let flags = self.parse_string_flags(start_text);

        let (content, expressions) = self.extract_content_and_interpolations(&node, source)?;

        let language = if let Some(type_node) = type_annotation {
            self.extract_language_from_annotation(type_node, source)?
        } else {
            None
        };

        info!("Extracted template: triple={}, content length={}, raw length={}", 
            flags.is_triple, content.len(), raw_content.len());
        info!("Content preview: '{}'", content.chars().take(50).collect::<String>().replace('\n', "\\n"));

        Ok(TemplateStringInfo {
            content,
            raw_content: raw_content.to_string(),
            variable_name: var_name.map(String::from),
            function_name: func_name.map(String::from),
            language,
            location: Location {
                start_line: start_position.row + 1,
                start_column: start_position.column + 1,
                end_line: end_position.row + 1,
                end_column: end_position.column + 1,
            },
            expressions,
            flags,
        })
    }

    fn parse_string_flags(&self, start_text: &str) -> TemplateStringFlags {
        let mut flags = TemplateStringFlags::default();

        let prefix = start_text.to_lowercase();

        flags.is_template = true;
        flags.is_raw = prefix.contains('r');
        flags.is_format = true;
        flags.is_triple = start_text.ends_with("'''") || start_text.ends_with("\"\"\"");

        flags
    }

    fn extract_content_and_interpolations(
        &self,
        string_node: &Node,
        source: &str,
    ) -> Result<(String, Vec<Expression>)> {
        let mut content_parts = Vec::new();
        let mut expressions = Vec::new();
        let mut cursor = string_node.walk();
        let mut last_end_byte = 0;

        for child in string_node.children(&mut cursor) {
            match child.kind() {
                "string_content" => {
                    let start_byte = child.start_byte();
                    let end_byte = child.end_byte();

                    if last_end_byte > 0 && start_byte > last_end_byte {
                        let between = &source[last_end_byte..start_byte];
                        content_parts.push(between.to_string());
                    }

                    let text = child.utf8_text(source.as_bytes())?;
                    let mut processed_content = String::new();
                    let mut chars = text.chars();

                    while let Some(ch) = chars.next() {
                        if ch == '{' {
                            if let Some(next_ch) = chars.clone().next() {
                                if next_ch == '{' {
                                    processed_content.push('{');
                                    chars.next();
                                    continue;
                                }
                            }
                        } else if ch == '}' {
                            if let Some(next_ch) = chars.clone().next() {
                                if next_ch == '}' {
                                    processed_content.push('}');
                                    chars.next();
                                    continue;
                                }
                            }
                        }
                        processed_content.push(ch);
                    }

                    content_parts.push(processed_content);
                    last_end_byte = end_byte;
                }
                "interpolation" => {
                    let start_byte = child.start_byte();

                    if last_end_byte > 0 && start_byte > last_end_byte {
                        let between = &source[last_end_byte..start_byte];
                        content_parts.push(between.to_string());
                    }

                    content_parts.push("{}".to_string());

                    if let Some(expr) = self.extract_interpolation_expression(&child, source)? {
                        expressions.push(expr);
                    }

                    last_end_byte = child.end_byte();
                }
                "escape_interpolation" => {
                    let start_byte = child.start_byte();

                    if last_end_byte > 0 && start_byte > last_end_byte {
                        let between = &source[last_end_byte..start_byte];
                        content_parts.push(between.to_string());
                    }

                    let text = child.utf8_text(source.as_bytes())?;
                    if text == "{{" {
                        content_parts.push("{".to_string());
                    } else if text == "}}" {
                        content_parts.push("}".to_string());
                    }

                    last_end_byte = child.end_byte();
                }
                "string_start" | "string_end" => {
                    last_end_byte = child.end_byte();
                }
                _ => {
                    let start_byte = child.start_byte();
                    if last_end_byte > 0 && start_byte > last_end_byte {
                        let between = &source[last_end_byte..start_byte];
                        content_parts.push(between.to_string());
                    }
                    last_end_byte = child.end_byte();
                }
            }
        }

        let full_content = content_parts.join("");
        Ok((full_content, expressions))
    }
    fn extract_interpolation_expression(
        &self,
        interpolation_node: &Node,
        source: &str,
    ) -> Result<Option<Expression>> {
        let mut cursor = interpolation_node.walk();

        for child in interpolation_node.children(&mut cursor) {
            if child.kind() != "{"
                && child.kind() != "}"
                && child.kind() != "="
                && child.kind() != "format_specifier"
                && child.kind() != "type_conversion"
            {
                let expr_content = child.utf8_text(source.as_bytes())?;
                let start = child.start_position();
                let end = child.end_position();

                return Ok(Some(Expression {
                    content: expr_content.to_string(),
                    location: Location {
                        start_line: start.row + 1,
                        start_column: start.column + 1,
                        end_line: end.row + 1,
                        end_column: end.column + 1,
                    },
                }));
            }
        }

        Ok(None)
    }

    fn extract_language_from_annotation(&self, node: Node, source: &str) -> Result<Option<String>> {
        let annotation_text = node.utf8_text(source.as_bytes())?;

        let re = regex::Regex::new(r#"Annotated\s*\[\s*Template\s*,\s*["'](\w+)["']\s*\]"#)?;

        if let Some(captures) = re.captures(annotation_text) {
            if let Some(lang) = captures.get(1) {
                return Ok(Some(lang.as_str().to_string()));
            }
        }

        Ok(None)
    }
}

#[derive(Debug, Clone, Default)]
pub struct TemplateStringFlags {
    pub is_template: bool,
    pub is_raw: bool,
    pub is_format: bool,
    pub is_triple: bool,
}

#[derive(Debug, Clone)]
pub struct TemplateStringInfo {
    pub content: String,
    pub raw_content: String,
    pub variable_name: Option<String>,
    pub function_name: Option<String>,
    pub language: Option<String>,
    pub location: Location,
    pub expressions: Vec<Expression>,
    pub flags: TemplateStringFlags,
}

#[derive(Debug, Clone)]
pub struct Location {
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

#[derive(Debug, Clone)]
pub struct Expression {
    pub content: String,
    pub location: Location,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_template_string() {
        let source = r#"msg = t"Hello {name}!""#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].content, "Hello {}!");
        assert_eq!(templates[0].variable_name, Some("msg".to_string()));
        assert_eq!(templates[0].expressions.len(), 1);
        assert_eq!(templates[0].expressions[0].content, "name");
    }

    #[test]
    fn test_template_with_format_spec() {
        let source = r#"price_str = t"Price: {price:.2f}""#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].expressions[0].content, "price");
    }

    #[test]
    fn test_raw_template_string() {
        let source = r#"path = tr"Path: {path}\n""#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert!(templates[0].flags.is_raw);
        assert!(templates[0].flags.is_template);
    }

    #[test]
    fn test_triple_quoted_template() {
        let source = r#"
html = t"""
<div>
    {content}
</div>
""""#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert!(templates[0].flags.is_triple);
        assert!(templates[0].content.contains("<div>"));
        assert_eq!(templates[0].expressions.len(), 1);
    }

    #[test]
    fn test_escaped_braces() {
        let source = r#"css = t"Use {{braces}} in {var}""#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].content, "Use {braces} in {}");
        assert_eq!(templates[0].expressions.len(), 1);
        assert_eq!(templates[0].expressions[0].content, "var");
    }

    #[test]
    fn test_annotated_template_string() {
        let source = r#"
from typing import Annotated
from templatelib import Template

html_content: Annotated[Template, "html"] = t"<h1>{title}</h1>"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
        assert_eq!(templates[0].variable_name, Some("html_content".to_string()));
    }

    #[test]
    fn test_multiline_template_with_expressions() {
        let source = r#"
html: Annotated[Template, "html"] = t"""<div>
  <span>{name}</span>
  {123}
</div>"""
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        let template = &templates[0];

        assert_eq!(template.language, Some("html".to_string()));
        assert!(template.flags.is_triple);
        assert_eq!(template.expressions.len(), 2);
        assert_eq!(template.expressions[0].content, "name");
        assert_eq!(template.expressions[1].content, "123");

        assert!(template.content.contains('\n'));
        assert!(template.content.contains("<div>\n"));
        assert!(template.content.contains("<span>{}</span>"));
    }
}