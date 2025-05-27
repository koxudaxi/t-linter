use anyhow::{Context, Result};
use std::collections::HashMap;
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};
use tracing::info;

#[derive(Debug, Clone, Default)]
pub struct ModuleContext {
    pub type_aliases: HashMap<String, String>,
    pub imports: HashMap<String, String>,
    pub function_signatures: HashMap<String, Vec<(usize, String)>>,
}

pub struct TemplateStringParser {
    parser: Parser,
}

impl TemplateStringParser {
    pub fn new() -> Result<Self> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .context("Failed to set Python language")?;

        Ok(Self { 
            parser,
        })
    }

    pub fn find_template_strings(&mut self, source: &str) -> Result<Vec<TemplateStringInfo>> {
        let tree = self
            .parser
            .parse(source, None)
            .context("Failed to parse source")?;

        let mut context = ModuleContext::default();
        
        self.collect_module_context(&tree, source, &mut context)?;

        let mut templates = Vec::new();
        self.find_strings_with_query(&tree, source, &mut templates, &context)?;

        Ok(templates)
    }

    fn collect_module_context(&mut self, tree: &Tree, source: &str, context: &mut ModuleContext) -> Result<()> {
        let type_alias_query = r#"
        (type_alias_statement) @type_alias
        "#;
        
        match Query::new(&tree_sitter_python::LANGUAGE.into(), type_alias_query) {
            Ok(query) => {
                let mut cursor = QueryCursor::new();
                let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
                while let Some(match_) = matches.next() {
                    
                    for capture in match_.captures {
                        let type_alias_node = capture.node;
                        
                        let mut cursor = type_alias_node.walk();
                        let mut name_node = None;
                        let mut value_node = None;
                        
                        for child in type_alias_node.children(&mut cursor) {
                            
                            if child.kind() == "type" && name_node.is_none() {
                                if child.utf8_text(source.as_bytes()).unwrap_or("") != "type" {
                                    name_node = Some(child);
                                }
                            } else if child.kind() == "type" && name_node.is_some() {
                                value_node = Some(child);
                            }
                        }
                        
                        if let (Some(name), Some(value)) = (name_node, value_node) {
                            let name_text = name.utf8_text(source.as_bytes())?;
                            
                            if let Some(lang) = self.extract_language_from_annotation(value, source, context)? {
                                context.type_aliases.insert(name_text.to_string(), lang);
                                info!("Found type alias: {} -> {}", name_text, value.utf8_text(source.as_bytes())?);
                            } else {
                            }
                        }
                    }
                }
            }
            Err(_) => {
            }
        }
        
        let typed_assignment_query = r#"
        (assignment
            left: (identifier) @alias_name
            type: (_) @type_annotation
            right: (_) @alias_value)
        "#;
        
        if let Ok(query) = Query::new(&tree_sitter_python::LANGUAGE.into(), typed_assignment_query) {
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
            
            while let Some(match_) = matches.next() {
                let mut alias_name = None;
                let mut alias_value = None;
                let mut type_annotation = None;
                
                for capture in match_.captures {
                    let name = query.capture_names()[capture.index as usize];
                    match name {
                        "alias_name" => alias_name = Some(capture.node),
                        "alias_value" => alias_value = Some(capture.node),
                        "type_annotation" => type_annotation = Some(capture.node),
                        _ => {}
                    }
                }
                
                if let (Some(name_node), Some(value_node), Some(type_node)) = (alias_name, alias_value, type_annotation) {
                    let name = name_node.utf8_text(source.as_bytes())?;
                    let type_text = type_node.utf8_text(source.as_bytes())?;
                    
                    if type_text.contains("TypeAlias") {
                        if let Some(lang) = self.extract_language_from_annotation(value_node, source, context)? {
                            context.type_aliases.insert(name.to_string(), lang);
                            info!("Found TypeAlias style alias: {} -> {}", name, value_node.utf8_text(source.as_bytes())?);
                        }
                    }
                }
            }
        }
        
        let query_str = r#"
        ; Import statements
        (import_statement
            name: (dotted_name) @import_name)

        (import_from_statement
            module_name: (dotted_name)? @module_name
            name: (dotted_name) @import_name)

        (import_from_statement
            module_name: (dotted_name)? @module_name
            (aliased_import
                name: (dotted_name) @import_name
                alias: (identifier) @import_alias))

        ; Function definitions
        (function_definition
            name: (identifier) @func_name
            parameters: (parameters) @params)
        "#;

        let query = Query::new(&tree_sitter_python::LANGUAGE.into(), query_str)
            .context("Failed to create context query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());

        while let Some(match_) = matches.next() {
            let mut module_name = None;
            let mut import_name = None;
            let mut import_alias = None;
            let mut func_name = None;
            let mut params = None;

            for capture in match_.captures {
                let name = query.capture_names()[capture.index as usize];
                match name {
                    "module_name" => module_name = Some(capture.node.utf8_text(source.as_bytes())?),
                    "import_name" => import_name = Some(capture.node.utf8_text(source.as_bytes())?),
                    "import_alias" => import_alias = Some(capture.node.utf8_text(source.as_bytes())?),
                    "func_name" => func_name = Some(capture.node.utf8_text(source.as_bytes())?),
                    "params" => params = Some(capture.node),
                    _ => {}
                }
            }


            if let Some(import) = import_name {
                let key = if let Some(alias) = import_alias {
                    alias.to_string()
                } else {
                    import.split('.').last().unwrap_or(import).to_string()
                };
                
                let value = if let Some(module) = module_name {
                    format!("{}.{}", module, import)
                } else {
                    import.to_string()
                };
                
                context.imports.insert(key, value);
            }

            if let (Some(name), Some(params_node)) = (func_name, params) {
                let param_types = self.extract_function_parameters(params_node, source)?;
                if !param_types.is_empty() {
                    context.function_signatures.insert(name.to_string(), param_types);
                }
            }
        }

        Ok(())
    }

    fn extract_function_parameters(&self, params_node: Node, source: &str) -> Result<Vec<(usize, String)>> {
        let mut param_types = Vec::new();
        let mut cursor = params_node.walk();
        let mut position = 0;

        for child in params_node.children(&mut cursor) {
            if child.kind() == "typed_parameter" || child.kind() == "typed_default_parameter" {
                if let Some(type_node) = child.child_by_field_name("type") {
                    let type_text = type_node.utf8_text(source.as_bytes())?;
                    param_types.push((position, type_text.to_string()));
                }
                position += 1;
            } else if child.kind() == "identifier" || child.kind() == "default_parameter" {
                position += 1;
            }
        }

        Ok(param_types)
    }

    fn find_strings_with_query(
        &self,
        tree: &Tree,
        source: &str,
        templates: &mut Vec<TemplateStringInfo>,
        context: &ModuleContext,
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
                            context,
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
        context: &ModuleContext,
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
            if let Some(lang) = self.extract_language_from_annotation(type_node, source, context)? {
                Some(lang)
            } else {
                let type_text = type_node.utf8_text(source.as_bytes())?;
                context.type_aliases.get(type_text).cloned()
            }
        } else if let Some(func) = func_name {
            self.infer_language_from_function_call(func, &node, source, context)?
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

    fn extract_language_from_annotation(&self, node: Node, source: &str, context: &ModuleContext) -> Result<Option<String>> {
        let subscript_node = if node.kind() == "subscript" {
            Some(node)
        } else if node.kind() == "type" {
            let node_text = node.utf8_text(source.as_bytes())?;
            if node_text.contains('[') && node_text.contains(']') {
                Some(node)
            } else {
                let cursor = node.walk();
                node.children(&mut cursor.clone()).find(|child| child.kind() == "subscript")
            }
        } else {
            None
        };
        
        if let Some(subscript) = subscript_node {
            if subscript.kind() == "type" {
                let text = subscript.utf8_text(source.as_bytes())?;
                if let Some(bracket_start) = text.find('[') {
                    let base_name = &text[..bracket_start];
                    
                    let is_annotated = base_name == "Annotated" || 
                        context.imports.get(base_name).map_or(false, |v| 
                            v == "typing.Annotated" || v.ends_with(".Annotated")
                        );
                    
                    if is_annotated {
                        if let Some(bracket_end) = text.rfind(']') {
                            let args = &text[bracket_start + 1..bracket_end];
                            
                            let parts: Vec<&str> = args.split(',').map(|s| s.trim()).collect();
                            if parts.len() >= 2 {
                                let template_part = parts[0];
                                let lang_part = parts[1].trim_matches(|c| c == '"' || c == '\'');
                                
                                let is_template = template_part == "Template" || 
                                    context.imports.get(template_part).map_or(false, |v| 
                                        v == "string.templatelib.Template" || 
                                        v == "templatelib.Template" ||
                                        v.ends_with(".Template")
                                    );
                                
                                if is_template {
                                    return Ok(Some(lang_part.to_string()));
                                }
                            }
                        }
                    }
                }
            } else {
                if let Some(value_node) = subscript.child_by_field_name("value") {
                    let value_text = value_node.utf8_text(source.as_bytes())?;
                    
                    let is_annotated = value_text == "Annotated" || 
                        context.imports.get(value_text).map_or(false, |v| 
                            v == "typing.Annotated" || v.ends_with(".Annotated")
                        );
                    
                    if is_annotated {
                        if let Some(slice_node) = subscript.child_by_field_name("slice") {
                            let mut cursor = slice_node.walk();
                            let mut found_template = false;
                        
                        for child in slice_node.children(&mut cursor) {
                            match child.kind() {
                                "identifier" => {
                                    let text = child.utf8_text(source.as_bytes())?;
                                    found_template = text == "Template" || 
                                        context.imports.get(text).map_or(false, |v| 
                                            v == "string.templatelib.Template" || 
                                            v == "templatelib.Template" ||
                                            v.ends_with(".Template")
                                        );
                                }
                                "attribute" => {
                                    let text = child.utf8_text(source.as_bytes())?;
                                    let attr_name = text.split('.').last().unwrap_or(text);
                                    found_template = attr_name == "Template" || 
                                        context.imports.get(attr_name).map_or(false, |v| 
                                            v == "string.templatelib.Template" || 
                                            v == "templatelib.Template" ||
                                            v.ends_with(".Template")
                                        );
                                }
                                "string" => {
                                    if found_template {
                                        let string_content = child.utf8_text(source.as_bytes())?;
                                        let lang = string_content.trim_matches(|c| c == '"' || c == '\'');
                                        return Ok(Some(lang.to_string()));
                                    }
                                }
                                _ => {}
                            }
                        }
                        }
                    }
                }
            }
        }
        
        let annotation_text = node.utf8_text(source.as_bytes())?;
        let re = regex::Regex::new(r#"Annotated\s*\[\s*Template\s*,\s*["'](\w+)["']\s*]"#)?;

        if let Some(captures) = re.captures(annotation_text) {
            if let Some(lang) = captures.get(1) {
                return Ok(Some(lang.as_str().to_string()));
            }
        }

        Ok(None)
    }

    fn infer_language_from_function_call(&self, func_name: &str, string_node: &Node, _source: &str, context: &ModuleContext) -> Result<Option<String>> {
        let signatures = match context.function_signatures.get(func_name) {
            Some(sigs) => sigs,
            None => return Ok(None),
        };

        if let Some(call_node) = string_node.parent() {
            if call_node.kind() == "argument_list" {
                let mut position = 0;
                let mut cursor = call_node.walk();
                
                for child in call_node.children(&mut cursor) {
                    if child.kind() == "string" && child.id() == string_node.id() {
                        for (param_pos, type_name) in signatures {
                            if *param_pos == position {
                                if let Some(lang) = context.type_aliases.get(type_name) {
                                    return Ok(Some(lang.clone()));
                                }
                                if let Some(lang) = self.extract_language_from_type_string(type_name)? {
                                    return Ok(Some(lang));
                                }
                            }
                        }
                        break;
                    }
                    
                    if matches!(child.kind(), "string" | "identifier" | "call" | "attribute" | 
                               "integer" | "float" | "true" | "false" | "none") {
                        position += 1;
                    }
                }
            }
        }

        Ok(None)
    }

    fn extract_language_from_type_string(&self, type_str: &str) -> Result<Option<String>> {
        let re = regex::Regex::new(r#"Annotated\s*\[\s*Template\s*,\s*["'](\w+)["']\s*]"#)?;
        
        if let Some(captures) = re.captures(type_str) {
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

    #[test]
    fn test_type_alias_detection() {
        let source = r#"
type html = Annotated[Template, "html"]
content: html = t"<h1>Title</h1>"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
        assert_eq!(templates[0].variable_name, Some("content".to_string()));
        assert_eq!(templates[0].content, "<h1>Title</h1>");
    }

    #[test]
    fn test_typing_typealias_detection() {
        let source = r#"
import typing
from string.templatelib import Template
from typing import Annotated

sql: typing.TypeAlias = Annotated[Template, "sql"]
query: sql = t"SELECT * FROM users WHERE id = {user_id}"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("sql".to_string()));
        assert_eq!(templates[0].variable_name, Some("query".to_string()));
    }

    #[test]
    fn test_import_tracking() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

page: Annotated[Template, "html"] = t"<div>Test</div>"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
    }

    #[test]
    fn test_function_parameter_inference() {
        let source = r#"
type sql = Annotated[Template, "sql"]

def execute_query(query: sql) -> list:
    return db.execute(query)

result = execute_query(t"SELECT * FROM users WHERE id = {user_id}")
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("sql".to_string()));
        assert_eq!(templates[0].content, "SELECT * FROM users WHERE id = {}");
    }

    #[test]
    fn test_mixed_type_aliases() {
        let source = r#"
type html = Annotated[Template, "html"]
type sql = Annotated[Template, "sql"]

page: html = t"<h1>Welcome</h1>"
query: sql = t"SELECT name FROM users"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 2);
        
        let html_template = templates.iter().find(|t| t.variable_name == Some("page".to_string())).unwrap();
        assert_eq!(html_template.language, Some("html".to_string()));
        
        let sql_template = templates.iter().find(|t| t.variable_name == Some("query".to_string())).unwrap();
        assert_eq!(sql_template.language, Some("sql".to_string()));
    }

    #[test]
    fn test_aliased_imports() {
        let source = r#"
from typing import Annotated as Ann
from string.templatelib import Template as Tmpl

content: Ann[Tmpl, "html"] = t"<p>Hello</p>"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
    }

    #[test]
    fn test_context_cleared_between_parses() {
        let mut parser = TemplateStringParser::new().unwrap();
        
        let source1 = r#"
type html = Annotated[Template, "html"]
content: html = t"<div>Test</div>"
"#;
        let templates1 = parser.find_template_strings(source1).unwrap();
        assert_eq!(templates1.len(), 1);
        assert_eq!(templates1[0].language, Some("html".to_string()));
        
        let source2 = r#"
# type alias removed
content: html = t"<div>Test</div>"
"#;
        let templates2 = parser.find_template_strings(source2).unwrap();
        assert_eq!(templates2.len(), 1);
        assert_eq!(templates2[0].language, None);
    }
}