use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tracing::info;
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};
use tstring_syntax::{
    SourcePosition, SourceSpan, TemplateInput, TemplateInterpolation, TemplateSegment,
};
use wait_timeout::ChildExt;

#[derive(Debug, Clone, Default)]
pub struct ModuleContext {
    pub type_aliases: HashMap<String, String>,
    pub imports: HashMap<String, String>,
    pub callable_signatures: HashMap<String, Vec<CallableParameter>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallableParameter {
    pub position: usize,
    pub name: String,
    pub language: String,
}

#[derive(Debug, Clone, Copy)]
struct CallArgument<'a> {
    position: usize,
    keyword: Option<&'a str>,
    value: Node<'a>,
}

pub struct TemplateStringParser {
    parser: Parser,
    search_root: Option<PathBuf>,
}

impl TemplateStringParser {
    const PYTHON_MODULE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

    pub fn new() -> Result<Self> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .context("Failed to set Python language")?;

        Ok(Self {
            parser,
            search_root: None,
        })
    }

    pub fn find_template_strings(&mut self, source: &str) -> Result<Vec<TemplateStringInfo>> {
        self.search_root = None;
        self.find_template_strings_with_search_root(source, None)
    }

    pub fn find_template_strings_in_file(
        &mut self,
        source: &str,
        path: &Path,
    ) -> Result<Vec<TemplateStringInfo>> {
        let search_root = path.parent().map(Path::to_path_buf);
        self.find_template_strings_with_search_root(source, search_root)
    }

    fn find_template_strings_with_search_root(
        &mut self,
        source: &str,
        search_root: Option<PathBuf>,
    ) -> Result<Vec<TemplateStringInfo>> {
        self.search_root = search_root;
        let tree = self
            .parser
            .parse(source, None)
            .context("Failed to parse source")?;

        let mut context = ModuleContext::default();

        self.collect_module_context(&tree, source, &mut context)?;
        let variable_language_hints =
            self.collect_variable_language_hints(&tree, source, &context)?;

        let mut templates = Vec::new();
        self.find_strings_with_query(
            &tree,
            source,
            &mut templates,
            &context,
            &variable_language_hints,
        )?;

        Ok(templates)
    }

    fn collect_module_context(
        &mut self,
        tree: &Tree,
        source: &str,
        context: &mut ModuleContext,
    ) -> Result<()> {
        self.collect_type_aliases(tree, source, context)?;
        self.collect_imports(tree, source, context)?;
        self.collect_local_callable_signatures(tree, source, context)?;
        self.collect_imported_callable_signatures(context)?;
        Ok(())
    }

    fn collect_type_aliases(
        &mut self,
        tree: &Tree,
        source: &str,
        context: &mut ModuleContext,
    ) -> Result<()> {
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

                            if let Some(lang) =
                                self.extract_language_from_annotation(value, source, context)?
                            {
                                context.type_aliases.insert(name_text.to_string(), lang);
                                info!(
                                    "Found type alias: {} -> {}",
                                    name_text,
                                    value.utf8_text(source.as_bytes())?
                                );
                            } else {
                            }
                        }
                    }
                }
            }
            Err(_) => {}
        }

        let typed_assignment_query = r#"
        (assignment
            left: (identifier) @alias_name
            type: (_) @type_annotation
            right: (_) @alias_value)
        "#;

        if let Ok(query) = Query::new(&tree_sitter_python::LANGUAGE.into(), typed_assignment_query)
        {
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

                if let (Some(name_node), Some(value_node), Some(type_node)) =
                    (alias_name, alias_value, type_annotation)
                {
                    let name = name_node.utf8_text(source.as_bytes())?;
                    let type_text = type_node.utf8_text(source.as_bytes())?;

                    if type_text.contains("TypeAlias") {
                        if let Some(lang) =
                            self.extract_language_from_annotation(value_node, source, context)?
                        {
                            context.type_aliases.insert(name.to_string(), lang);
                            info!(
                                "Found TypeAlias style alias: {} -> {}",
                                name,
                                value_node.utf8_text(source.as_bytes())?
                            );
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn collect_imports(
        &mut self,
        tree: &Tree,
        source: &str,
        context: &mut ModuleContext,
    ) -> Result<()> {
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

        "#;

        let query = Query::new(&tree_sitter_python::LANGUAGE.into(), query_str)
            .context("Failed to create context query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());

        while let Some(match_) = matches.next() {
            let mut module_name = None;
            let mut import_name = None;
            let mut import_alias = None;
            for capture in match_.captures {
                let name = query.capture_names()[capture.index as usize];
                match name {
                    "module_name" => module_name = Some(capture.node.utf8_text(source.as_bytes())?),
                    "import_name" => import_name = Some(capture.node.utf8_text(source.as_bytes())?),
                    "import_alias" => {
                        import_alias = Some(capture.node.utf8_text(source.as_bytes())?)
                    }
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
        }

        Ok(())
    }

    fn collect_local_callable_signatures(
        &mut self,
        tree: &Tree,
        source: &str,
        context: &mut ModuleContext,
    ) -> Result<()> {
        let root = tree.root_node();
        let mut cursor = root.walk();

        for child in root.children(&mut cursor) {
            match child.kind() {
                "function_definition" => {
                    let Some(name_node) = child.child_by_field_name("name") else {
                        continue;
                    };
                    let Some(params_node) = child.child_by_field_name("parameters") else {
                        continue;
                    };
                    let parameter_languages = self.extract_callable_parameter_languages(
                        params_node,
                        source,
                        context,
                        false,
                    )?;
                    if !parameter_languages.is_empty() {
                        context.callable_signatures.insert(
                            name_node.utf8_text(source.as_bytes())?.to_string(),
                            parameter_languages,
                        );
                    }
                }
                "class_definition" => {
                    let Some(name_node) = child.child_by_field_name("name") else {
                        continue;
                    };
                    let Some(body_node) = child.child_by_field_name("body") else {
                        continue;
                    };
                    if let Some(parameter_languages) =
                        self.extract_class_constructor_languages(body_node, source, context)?
                        && !parameter_languages.is_empty()
                    {
                        context.callable_signatures.insert(
                            name_node.utf8_text(source.as_bytes())?.to_string(),
                            parameter_languages,
                        );
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn extract_class_constructor_languages(
        &self,
        body_node: Node,
        source: &str,
        context: &ModuleContext,
    ) -> Result<Option<Vec<CallableParameter>>> {
        let mut cursor = body_node.walk();
        for child in body_node.children(&mut cursor) {
            if child.kind() != "function_definition" {
                continue;
            }

            let Some(name_node) = child.child_by_field_name("name") else {
                continue;
            };
            if name_node.utf8_text(source.as_bytes())? != "__init__" {
                continue;
            }

            let Some(params_node) = child.child_by_field_name("parameters") else {
                continue;
            };
            let parameter_languages =
                self.extract_callable_parameter_languages(params_node, source, context, true)?;
            return Ok(Some(parameter_languages));
        }

        Ok(None)
    }

    fn extract_callable_parameter_languages(
        &self,
        params_node: Node,
        source: &str,
        context: &ModuleContext,
        skip_receiver: bool,
    ) -> Result<Vec<CallableParameter>> {
        let mut parameter_languages = Vec::new();
        let mut cursor = params_node.walk();
        let mut position = 0;
        let mut receiver_skipped = !skip_receiver;

        for child in params_node.children(&mut cursor) {
            if child.kind() == "typed_parameter" || child.kind() == "typed_default_parameter" {
                let parameter_name = child
                    .child_by_field_name("name")
                    .and_then(|node| node.utf8_text(source.as_bytes()).ok());
                if !receiver_skipped && matches!(parameter_name, Some("self" | "cls")) {
                    receiver_skipped = true;
                    continue;
                }
                if let Some(type_node) = child.child_by_field_name("type") {
                    if let Some(language) =
                        self.resolve_language_from_type_node(type_node, source, context)?
                    {
                        parameter_languages.push(CallableParameter {
                            position,
                            name: parameter_name.unwrap_or("").to_string(),
                            language,
                        });
                    }
                }
                position += 1;
                receiver_skipped = true;
            } else if child.kind() == "identifier" || child.kind() == "default_parameter" {
                let parameter_name = child.utf8_text(source.as_bytes()).ok();
                if !receiver_skipped && matches!(parameter_name, Some("self" | "cls")) {
                    receiver_skipped = true;
                    continue;
                }
                position += 1;
                receiver_skipped = true;
            }
        }

        Ok(parameter_languages)
    }

    fn collect_imported_callable_signatures(&mut self, context: &mut ModuleContext) -> Result<()> {
        let mut module_cache = HashMap::new();

        for (alias, import_path) in context.imports.clone() {
            if let Some(signatures) =
                self.resolve_imported_callable_signature(&import_path, &mut module_cache)?
            {
                context
                    .callable_signatures
                    .entry(alias.clone())
                    .or_insert_with(|| signatures.clone());
                context
                    .callable_signatures
                    .entry(import_path.clone())
                    .or_insert(signatures);
            }

            if let Some(module_signatures) =
                self.load_imported_module_signatures(&import_path, &mut module_cache)?
            {
                for (callable_name, signatures) in module_signatures {
                    context
                        .callable_signatures
                        .entry(format!("{import_path}.{callable_name}"))
                        .or_insert(signatures);
                }
            }
        }

        Ok(())
    }

    fn resolve_imported_callable_signature(
        &mut self,
        import_path: &str,
        module_cache: &mut HashMap<String, HashMap<String, Vec<CallableParameter>>>,
    ) -> Result<Option<Vec<CallableParameter>>> {
        let Some((module_name, symbol_name)) = import_path.rsplit_once('.') else {
            return Ok(None);
        };

        let Some(module_signatures) =
            self.load_imported_module_signatures(module_name, module_cache)?
        else {
            return Ok(None);
        };

        Ok(module_signatures.get(symbol_name).cloned())
    }

    fn load_imported_module_signatures(
        &mut self,
        module_name: &str,
        module_cache: &mut HashMap<String, HashMap<String, Vec<CallableParameter>>>,
    ) -> Result<Option<HashMap<String, Vec<CallableParameter>>>> {
        if let Some(signatures) = module_cache.get(module_name) {
            return Ok(Some(signatures.clone()));
        }

        module_cache.insert(module_name.to_string(), HashMap::new());

        let Some(module_path) = self.resolve_python_module_path(module_name) else {
            return Ok(None);
        };

        let source = match fs::read_to_string(&module_path) {
            Ok(source) => source,
            Err(_) => return Ok(None),
        };

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .context("Failed to set Python language")?;
        let Some(tree) = parser.parse(&source, None) else {
            return Ok(None);
        };

        let original_search_root = self.search_root.clone();
        self.search_root = module_path.parent().map(Path::to_path_buf);

        let imported_signatures = (|| -> Result<HashMap<String, Vec<CallableParameter>>> {
            let mut imported_context = ModuleContext::default();
            self.collect_type_aliases(&tree, &source, &mut imported_context)?;
            self.collect_imports(&tree, &source, &mut imported_context)?;
            self.collect_local_callable_signatures(&tree, &source, &mut imported_context)?;
            self.collect_imported_callable_signatures(&mut imported_context)?;
            Ok(imported_context.callable_signatures)
        })();

        self.search_root = original_search_root;

        let imported_signatures = imported_signatures?;

        module_cache.insert(module_name.to_string(), imported_signatures.clone());
        Ok(Some(imported_signatures))
    }

    fn resolve_python_module_path(&self, module_name: &str) -> Option<PathBuf> {
        if let Some(search_root) = self.search_root.as_deref()
            && let Some(path) = resolve_local_module_path(search_root, module_name)
        {
            return Some(path);
        }

        if let Ok(current_dir) = std::env::current_dir()
            && let Some(path) = resolve_local_module_path(&current_dir, module_name)
        {
            return Some(path);
        }

        for interpreter in ["python3", "python"] {
            let mut child = Command::new(interpreter)
                .arg("-c")
                .arg(
                    "import importlib.util, sys\n\
spec = importlib.util.find_spec(sys.argv[1])\n\
if spec and spec.origin:\n\
    print(spec.origin)\n",
                )
                .arg(module_name)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let status = child
                .wait_timeout(Self::PYTHON_MODULE_LOOKUP_TIMEOUT)
                .ok()?;
            let Some(_status) = status else {
                let _ = child.kill();
                let _ = child.wait();
                continue;
            };
            let output = child.wait_with_output().ok()?;

            if !output.status.success() {
                continue;
            }

            let origin = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if origin.is_empty() || origin == "built-in" || origin == "frozen" {
                continue;
            }

            let mut path = PathBuf::from(origin);
            if path.extension().and_then(|ext| ext.to_str()) == Some("pyc") {
                path.set_extension("py");
            }

            if let Some(stub_path) = preferred_stub_path(&path) {
                return Some(stub_path);
            }
            if path.exists() {
                return Some(path);
            }
        }

        None
    }

    fn collect_variable_language_hints(
        &self,
        tree: &Tree,
        source: &str,
        context: &ModuleContext,
    ) -> Result<HashMap<String, String>> {
        let query_str = r#"
        (call
            function: (_) @func_expr
            arguments: (argument_list) @args)
        "#;

        let query = Query::new(&tree_sitter_python::LANGUAGE.into(), query_str)
            .context("Failed to create call query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
        let mut hints = HashMap::<String, Option<String>>::new();

        while let Some(match_) = matches.next() {
            let mut func_expr = None;
            let mut args_node = None;

            for capture in match_.captures {
                let name = query.capture_names()[capture.index as usize];
                match name {
                    "func_expr" => func_expr = Some(capture.node.utf8_text(source.as_bytes())?),
                    "args" => args_node = Some(capture.node),
                    _ => {}
                }
            }

            let (Some(callee), Some(args)) = (func_expr, args_node) else {
                continue;
            };
            let Some(signatures) = self.lookup_callable_signatures(callee, context) else {
                continue;
            };

            let arguments = call_arguments(args, source)?;
            for argument in arguments {
                if argument.value.kind() != "identifier" {
                    continue;
                }
                let Some(parameter) =
                    resolve_callable_parameter(signatures, argument.position, argument.keyword)
                else {
                    continue;
                };
                let variable_name = argument.value.utf8_text(source.as_bytes())?.to_string();
                match hints.get(&variable_name) {
                    Some(Some(existing)) if existing != &parameter.language => {
                        hints.insert(variable_name, None);
                    }
                    Some(None) => {}
                    _ => {
                        hints.insert(variable_name, Some(parameter.language.clone()));
                    }
                }
            }
        }

        Ok(hints
            .into_iter()
            .filter_map(|(name, language)| language.map(|language| (name, language)))
            .collect())
    }

    fn find_strings_with_query(
        &self,
        tree: &Tree,
        source: &str,
        templates: &mut Vec<TemplateStringInfo>,
        context: &ModuleContext,
        variable_language_hints: &HashMap<String, String>,
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
            function: (_) @func_name
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
                            variable_language_hints,
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
        variable_language_hints: &HashMap<String, String>,
    ) -> Result<TemplateStringInfo> {
        let start_position = node.start_position();
        let end_position = node.end_position();

        let string_start = node
            .child(0)
            .ok_or_else(|| anyhow::anyhow!("No string_start node"))?;
        let last_child_index = u32::try_from(node.child_count() - 1)?;
        let string_end = node
            .child(last_child_index)
            .ok_or_else(|| anyhow::anyhow!("No string_end node"))?;

        let start_text = string_start.utf8_text(source.as_bytes())?;
        let end_text = string_end.utf8_text(source.as_bytes())?;
        let raw_content = node.utf8_text(source.as_bytes())?;

        let flags = self.parse_string_flags(start_text);

        let (content, expressions, parts) =
            self.extract_content_and_interpolations(&node, source)?;

        let mut language = if let Some(type_node) = type_annotation {
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
        if language.is_none() {
            if let Some(name) = var_name {
                language = variable_language_hints.get(name).cloned();
            }
        }

        info!(
            "Extracted template: triple={}, content length={}, raw length={}",
            flags.is_triple,
            content.len(),
            raw_content.len()
        );
        info!(
            "Content preview: '{}'",
            content
                .chars()
                .take(50)
                .collect::<String>()
                .replace('\n', "\\n")
        );

        Ok(TemplateStringInfo {
            content,
            raw_content: raw_content.to_string(),
            variable_name: var_name.map(String::from),
            function_name: func_name.map(String::from),
            language,
            string_start: start_text.to_string(),
            string_end: end_text.to_string(),
            location: Location {
                start_line: start_position.row + 1,
                start_column: start_position.column + 1,
                end_line: end_position.row + 1,
                end_column: end_position.column + 1,
            },
            expressions,
            parts,
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
    ) -> Result<(String, Vec<Expression>, Vec<TemplatePart>)> {
        let mut content_parts = Vec::new();
        let mut expressions = Vec::new();
        let mut parts = Vec::new();
        let mut cursor = string_node.walk();
        let mut last_end_byte = 0;
        let mut interpolation_index = 0;

        for child in string_node.children(&mut cursor) {
            match child.kind() {
                "string_content" => {
                    let start_byte = child.start_byte();
                    let end_byte = child.end_byte();

                    if last_end_byte > 0 && start_byte > last_end_byte {
                        let between = &source[last_end_byte..start_byte];
                        push_static_part(&mut content_parts, &mut parts, between);
                    }

                    let text = child.utf8_text(source.as_bytes())?;
                    let processed_content = unescape_template_text(text);
                    push_static_part(&mut content_parts, &mut parts, &processed_content);
                    last_end_byte = end_byte;
                }
                "interpolation" => {
                    let start_byte = child.start_byte();

                    if last_end_byte > 0 && start_byte > last_end_byte {
                        let between = &source[last_end_byte..start_byte];
                        push_static_part(&mut content_parts, &mut parts, between);
                    }

                    content_parts.push("{}".to_string());

                    if let Some(interpolation) =
                        self.extract_interpolation_expression(&child, source, interpolation_index)?
                    {
                        let expr = Expression {
                            content: interpolation.expression.clone(),
                            location: interpolation.location.clone(),
                        };
                        expressions.push(expr);
                        parts.push(TemplatePart::Interpolation(interpolation));
                        interpolation_index += 1;
                    }

                    last_end_byte = child.end_byte();
                }
                "escape_interpolation" => {
                    let start_byte = child.start_byte();

                    if last_end_byte > 0 && start_byte > last_end_byte {
                        let between = &source[last_end_byte..start_byte];
                        push_static_part(&mut content_parts, &mut parts, between);
                    }

                    let text = child.utf8_text(source.as_bytes())?;
                    if text == "{{" {
                        push_static_part(&mut content_parts, &mut parts, "{");
                    } else if text == "}}" {
                        push_static_part(&mut content_parts, &mut parts, "}");
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
                        push_static_part(&mut content_parts, &mut parts, between);
                    }
                    last_end_byte = child.end_byte();
                }
            }
        }

        let full_content = content_parts.join("");
        Ok((full_content, expressions, parts))
    }

    fn extract_interpolation_expression(
        &self,
        interpolation_node: &Node,
        source: &str,
        interpolation_index: usize,
    ) -> Result<Option<InterpolationInfo>> {
        let mut cursor = interpolation_node.walk();
        let mut expression = None;
        let mut location = None;
        let mut conversion = None;
        let mut format_spec = String::new();

        for child in interpolation_node.children(&mut cursor) {
            match child.kind() {
                "{" | "}" | "=" => {}
                "type_conversion" => {
                    let value = child.utf8_text(source.as_bytes())?;
                    conversion = value.strip_prefix('!').map(str::to_string);
                }
                "format_specifier" => {
                    let value = child.utf8_text(source.as_bytes())?;
                    format_spec = value.strip_prefix(':').unwrap_or(value).to_string();
                }
                _ => {
                    if expression.is_none() {
                        let expr_content = child.utf8_text(source.as_bytes())?;
                        let start = child.start_position();
                        let end = child.end_position();
                        expression = Some(expr_content.to_string());
                        location = Some(Location {
                            start_line: start.row + 1,
                            start_column: start.column + 1,
                            end_line: end.row + 1,
                            end_column: end.column + 1,
                        });
                    }
                }
            }
        }

        Ok(expression
            .zip(location)
            .map(|(expression, location)| InterpolationInfo {
                expression,
                conversion,
                format_spec,
                raw_source: source[interpolation_node.start_byte()..interpolation_node.end_byte()]
                    .to_string(),
                location,
                interpolation_index,
            }))
    }

    fn extract_language_from_annotation(
        &self,
        node: Node,
        source: &str,
        context: &ModuleContext,
    ) -> Result<Option<String>> {
        let subscript_node = if node.kind() == "subscript" {
            Some(node)
        } else if node.kind() == "type" {
            let node_text = node.utf8_text(source.as_bytes())?;
            if node_text.contains('[') && node_text.contains(']') {
                Some(node)
            } else {
                let cursor = node.walk();
                node.children(&mut cursor.clone())
                    .find(|child| child.kind() == "subscript")
            }
        } else {
            None
        };

        if let Some(subscript) = subscript_node {
            if subscript.kind() == "type" {
                let text = subscript.utf8_text(source.as_bytes())?;
                if let Some(bracket_start) = text.find('[') {
                    let base_name = &text[..bracket_start];

                    let is_annotated = base_name == "Annotated"
                        || context.imports.get(base_name).map_or(false, |v| {
                            v == "typing.Annotated" || v.ends_with(".Annotated")
                        });

                    if is_annotated {
                        if let Some(bracket_end) = text.rfind(']') {
                            let args = &text[bracket_start + 1..bracket_end];

                            let parts: Vec<&str> = args.split(',').map(|s| s.trim()).collect();
                            if parts.len() >= 2 {
                                let template_part = parts[0];
                                let lang_part = parts[1].trim_matches(|c| c == '"' || c == '\'');

                                let is_template = template_part == "Template"
                                    || context.imports.get(template_part).map_or(false, |v| {
                                        v == "string.templatelib.Template"
                                            || v == "templatelib.Template"
                                            || v.ends_with(".Template")
                                    });

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

                    let is_annotated = value_text == "Annotated"
                        || context.imports.get(value_text).map_or(false, |v| {
                            v == "typing.Annotated" || v.ends_with(".Annotated")
                        });

                    if is_annotated {
                        if let Some(slice_node) = subscript.child_by_field_name("slice") {
                            let mut cursor = slice_node.walk();
                            let mut found_template = false;

                            for child in slice_node.children(&mut cursor) {
                                match child.kind() {
                                    "identifier" => {
                                        let text = child.utf8_text(source.as_bytes())?;
                                        found_template = text == "Template"
                                            || context.imports.get(text).map_or(false, |v| {
                                                v == "string.templatelib.Template"
                                                    || v == "templatelib.Template"
                                                    || v.ends_with(".Template")
                                            });
                                    }
                                    "attribute" => {
                                        let text = child.utf8_text(source.as_bytes())?;
                                        let attr_name = text.split('.').last().unwrap_or(text);
                                        found_template = attr_name == "Template"
                                            || context.imports.get(attr_name).map_or(false, |v| {
                                                v == "string.templatelib.Template"
                                                    || v == "templatelib.Template"
                                                    || v.ends_with(".Template")
                                            });
                                    }
                                    "string" => {
                                        if found_template {
                                            let string_content =
                                                child.utf8_text(source.as_bytes())?;
                                            let lang = string_content
                                                .trim_matches(|c| c == '"' || c == '\'');
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

    fn infer_language_from_function_call(
        &self,
        func_name: &str,
        string_node: &Node,
        source: &str,
        context: &ModuleContext,
    ) -> Result<Option<String>> {
        let Some(signatures) = self.lookup_callable_signatures(func_name, context) else {
            return Ok(None);
        };

        if let Some(call_node) = string_node.parent() {
            if call_node.kind() == "argument_list" {
                for argument in call_arguments(call_node, source)? {
                    if argument.value.kind() == "string" && argument.value.id() == string_node.id()
                    {
                        if let Some(parameter) = resolve_callable_parameter(
                            signatures,
                            argument.position,
                            argument.keyword,
                        ) {
                            return Ok(Some(parameter.language.clone()));
                        }
                        break;
                    }
                }
            }
        }

        Ok(None)
    }

    fn lookup_callable_signatures<'a>(
        &self,
        callee: &str,
        context: &'a ModuleContext,
    ) -> Option<&'a Vec<CallableParameter>> {
        if let Some(signatures) = context.callable_signatures.get(callee) {
            return Some(signatures);
        }

        let (base, member) = callee.split_once('.')?;
        let import_target = context.imports.get(base)?;
        context
            .callable_signatures
            .get(&format!("{import_target}.{member}"))
    }

    fn resolve_language_from_type_node(
        &self,
        type_node: Node,
        source: &str,
        context: &ModuleContext,
    ) -> Result<Option<String>> {
        if let Some(language) = self.extract_language_from_annotation(type_node, source, context)? {
            return Ok(Some(language));
        }

        let type_text = type_node.utf8_text(source.as_bytes())?;
        if let Some(language) = context.type_aliases.get(type_text) {
            return Ok(Some(language.clone()));
        }

        self.extract_language_from_type_string(type_text)
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

fn call_arguments<'a>(argument_list: Node<'a>, source: &'a str) -> Result<Vec<CallArgument<'a>>> {
    let mut arguments = Vec::new();
    let mut cursor = argument_list.walk();
    let mut position = 0;

    for child in argument_list.children(&mut cursor) {
        if matches!(
            child.kind(),
            "string"
                | "identifier"
                | "call"
                | "attribute"
                | "integer"
                | "float"
                | "true"
                | "false"
                | "none"
                | "list"
                | "dictionary"
                | "tuple"
                | "set"
        ) {
            arguments.push(CallArgument {
                position,
                keyword: None,
                value: child,
            });
            position += 1;
            continue;
        }

        if child.kind() == "keyword_argument"
            && let Some(value) = child.child_by_field_name("value")
        {
            let keyword = child
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source.as_bytes()).ok());
            arguments.push(CallArgument {
                position,
                keyword,
                value,
            });
            position += 1;
        }
    }

    Ok(arguments)
}

fn resolve_callable_parameter<'a>(
    signatures: &'a [CallableParameter],
    position: usize,
    keyword: Option<&str>,
) -> Option<&'a CallableParameter> {
    if let Some(keyword) = keyword
        && let Some(parameter) = signatures
            .iter()
            .find(|parameter| parameter.name == keyword)
    {
        return Some(parameter);
    }

    signatures
        .iter()
        .find(|parameter| parameter.position == position)
}

fn preferred_stub_path(path: &Path) -> Option<PathBuf> {
    if path.file_name().and_then(|name| name.to_str()) == Some("__init__.py") {
        let stub = path.with_file_name("__init__.pyi");
        if stub.exists() {
            return Some(stub);
        }
    }

    if path.extension().and_then(|ext| ext.to_str()) == Some("py") {
        let stub = path.with_extension("pyi");
        if stub.exists() {
            return Some(stub);
        }
    }

    None
}

fn resolve_local_module_path(search_root: &Path, module_name: &str) -> Option<PathBuf> {
    let mut module_path = search_root.to_path_buf();
    for segment in module_name.split('.') {
        module_path.push(segment);
    }

    let module_file = module_path.with_extension("py");
    if let Some(stub_path) = preferred_stub_path(&module_file) {
        return Some(stub_path);
    }
    if module_file.exists() {
        return Some(module_file);
    }

    let package_init = module_path.join("__init__.py");
    if let Some(stub_path) = preferred_stub_path(&package_init) {
        return Some(stub_path);
    }
    if package_init.exists() {
        return Some(package_init);
    }

    None
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
    pub string_start: String,
    pub string_end: String,
    pub location: Location,
    pub expressions: Vec<Expression>,
    pub parts: Vec<TemplatePart>,
    pub flags: TemplateStringFlags,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
pub enum TemplatePart {
    Static(StaticTextSegment),
    Interpolation(InterpolationInfo),
}

#[derive(Debug, Clone)]
pub struct StaticTextSegment {
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct InterpolationInfo {
    pub expression: String,
    pub conversion: Option<String>,
    pub format_spec: String,
    pub raw_source: String,
    pub location: Location,
    pub interpolation_index: usize,
}

impl TemplateStringInfo {
    pub fn to_template_input(&self) -> TemplateInput {
        let mut segments = Vec::with_capacity(self.parts.len().max(1));

        for part in &self.parts {
            match part {
                TemplatePart::Static(part) => {
                    if !part.text.is_empty() {
                        segments.push(TemplateSegment::StaticText(part.text.clone()));
                    }
                }
                TemplatePart::Interpolation(part) => {
                    segments.push(TemplateSegment::Interpolation(TemplateInterpolation {
                        expression: part.expression.clone(),
                        conversion: part.conversion.clone(),
                        format_spec: part.format_spec.clone(),
                        interpolation_index: part.interpolation_index,
                        raw_source: Some(part.raw_source.clone()),
                    }));
                }
            }
        }

        if segments.is_empty() {
            segments.push(TemplateSegment::StaticText(String::new()));
        }

        TemplateInput::from_segments(segments)
    }

    pub fn backend_span_to_location(&self, span: &SourceSpan) -> Location {
        let start_offset = self.token_position_to_content_offset(&span.start);
        let end_offset = self.token_position_to_content_offset(&span.end);
        let ((start_line, start_column), (end_line, end_column)) =
            self.map_content_range_to_document(start_offset, end_offset);
        Location {
            start_line,
            start_column,
            end_line,
            end_column,
        }
    }

    pub fn formatted_literal(&self, content: &str) -> String {
        let preferred_quote = if self.string_start.contains('\'') {
            '\''
        } else {
            '"'
        };
        let prefix = self.literal_prefix();

        if self.flags.is_raw
            && let Some((string_start, string_end)) =
                choose_raw_delimiters(content, preferred_quote, self.flags.is_triple)
        {
            return format!("{prefix}{string_start}{content}{string_end}");
        }

        let normalized_prefix = normalize_literal_prefix(prefix, self.flags.is_raw);
        let use_triple = self.flags.is_triple || content.contains('\n');
        let quote = choose_non_raw_quote(content, preferred_quote, use_triple);
        let delimiter = if use_triple {
            std::iter::repeat_n(quote, 3).collect::<String>()
        } else {
            quote.to_string()
        };
        let escaped_content = escape_python_literal_content(content, quote, use_triple);

        format!("{normalized_prefix}{delimiter}{escaped_content}{delimiter}")
    }

    fn token_position_to_content_offset(&self, position: &SourcePosition) -> usize {
        let mut offset = 0;

        for (token_index, part) in self.parts.iter().enumerate() {
            if token_index == position.token_index {
                return match part {
                    TemplatePart::Static(part) => offset + position.offset.min(part.text.len()),
                    TemplatePart::Interpolation(_) => offset + position.offset.min(2),
                };
            }

            offset += match part {
                TemplatePart::Static(part) => part.text.len(),
                TemplatePart::Interpolation(_) => 2,
            };
        }

        offset
    }

    fn map_content_range_to_document(
        &self,
        start_offset: usize,
        end_offset: usize,
    ) -> ((usize, usize), (usize, usize)) {
        let prefix_len = self.string_start.len();
        let actual_content =
            &self.raw_content[prefix_len..self.raw_content.len() - self.string_end.len()];
        let template_start_line = self.location.start_line - 1;
        let template_start_col = self.location.start_column - 1;

        let (start_line, start_col) = map_template_position_to_document(
            &self.content,
            actual_content,
            start_offset,
            template_start_line,
            template_start_col,
            prefix_len,
        );
        let (end_line, end_col) = map_template_position_to_document(
            &self.content,
            actual_content,
            end_offset,
            template_start_line,
            template_start_col,
            prefix_len,
        );

        ((start_line + 1, start_col + 1), (end_line + 1, end_col + 1))
    }

    fn literal_prefix(&self) -> &str {
        self.string_start
            .trim_end_matches(['\'', '"'])
            .trim_end_matches(['\'', '"'])
            .trim_end_matches(['\'', '"'])
    }
}

fn push_static_part(content_parts: &mut Vec<String>, parts: &mut Vec<TemplatePart>, text: &str) {
    if text.is_empty() {
        return;
    }

    let text = text.to_string();
    content_parts.push(text.clone());
    parts.push(TemplatePart::Static(StaticTextSegment { text }));
}

fn unescape_template_text(text: &str) -> String {
    let mut processed_content = String::new();
    let mut chars = text.chars();

    while let Some(ch) = chars.next() {
        if ch == '{' {
            if let Some(next_ch) = chars.clone().next()
                && next_ch == '{'
            {
                processed_content.push('{');
                chars.next();
                continue;
            }
        } else if ch == '}'
            && let Some(next_ch) = chars.clone().next()
            && next_ch == '}'
        {
            processed_content.push('}');
            chars.next();
            continue;
        }
        processed_content.push(ch);
    }

    processed_content
}

fn map_template_position_to_document(
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

    for byte in actual_bytes.iter().take(actual_idx) {
        if *byte == b'\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }

    (line, col)
}

fn choose_raw_delimiters(
    content: &str,
    preferred_quote: char,
    prefer_triple: bool,
) -> Option<(String, String)> {
    let trailing_backslashes = content.chars().rev().take_while(|&ch| ch == '\\').count();
    if trailing_backslashes % 2 != 0 {
        return None;
    }

    let quote_candidates = if preferred_quote == '\'' {
        ['\'', '"']
    } else {
        ['"', '\'']
    };

    let triple_modes = if prefer_triple || content.contains('\n') {
        [true, false]
    } else {
        [false, true]
    };

    for use_triple in triple_modes {
        if !use_triple && content.contains('\n') {
            continue;
        }

        for quote in quote_candidates {
            let delimiter = if use_triple {
                std::iter::repeat_n(quote, 3).collect::<String>()
            } else {
                quote.to_string()
            };
            if !content.contains(&delimiter) {
                return Some((delimiter.clone(), delimiter));
            }
        }
    }

    None
}

fn normalize_literal_prefix(prefix: &str, was_raw: bool) -> String {
    if !was_raw {
        return prefix.to_string();
    }

    let normalized = prefix
        .chars()
        .filter(|ch| !matches!(ch, 'r' | 'R'))
        .collect::<String>();
    if normalized.is_empty() {
        "t".to_owned()
    } else {
        normalized
    }
}

fn choose_non_raw_quote(content: &str, preferred_quote: char, use_triple: bool) -> char {
    let alternate_quote = if preferred_quote == '\'' { '"' } else { '\'' };
    let preferred_cost = quote_escape_cost(content, preferred_quote, use_triple);
    let alternate_cost = quote_escape_cost(content, alternate_quote, use_triple);

    if alternate_cost < preferred_cost {
        alternate_quote
    } else {
        preferred_quote
    }
}

fn quote_escape_cost(content: &str, quote: char, use_triple: bool) -> usize {
    if use_triple {
        let delimiter = std::iter::repeat_n(quote, 3).collect::<String>();
        content.matches(&delimiter).count() * 3
    } else {
        content.matches(quote).count()
    }
}

fn escape_python_literal_content(content: &str, quote: char, use_triple: bool) -> String {
    let mut escaped = String::new();

    for ch in content.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\n' if use_triple => escaped.push('\n'),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\0' => escaped.push_str("\\0"),
            '\u{0008}' => escaped.push_str("\\b"),
            '\u{000C}' => escaped.push_str("\\f"),
            '\'' if quote == '\'' => escaped.push_str("\\'"),
            '"' if quote == '"' => escaped.push_str("\\\""),
            ch if ch.is_control() => escaped.push_str(&format!("\\x{:02x}", ch as u32)),
            _ => escaped.push(ch),
        }
    }

    if use_triple {
        let delimiter = std::iter::repeat_n(quote, 3).collect::<String>();
        let escaped_delimiter = if quote == '\'' {
            "\\'\\'\\'"
        } else {
            "\\\"\\\"\\\""
        };
        escaped.replace(&delimiter, escaped_delimiter)
    } else {
        escaped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn parser_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "t-linter-parser-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

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
        let TemplatePart::Interpolation(interpolation) = &templates[0].parts[1] else {
            panic!("expected interpolation part");
        };
        assert_eq!(interpolation.format_spec, ".2f");
        assert_eq!(interpolation.raw_source, "{price:.2f}");
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
    fn test_template_input_preserves_raw_interpolations() {
        let source = r#"payload = t"Hello {name!r:>5}!""#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        let input = templates[0].to_template_input();
        let interpolation = input.interpolation(0).expect("expected interpolation");
        assert_eq!(interpolation.expression, "name");
        assert_eq!(interpolation.conversion.as_deref(), Some("r"));
        assert_eq!(interpolation.format_spec, ">5");
        assert_eq!(interpolation.raw_source.as_deref(), Some("{name!r:>5}"));
    }

    #[test]
    fn test_formatted_literal_requotes_json_content() {
        let source = r#"payload = t"placeholder""#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(
            templates[0].formatted_literal(r#"{"name": {name}, "message": "Hello"}"#),
            r#"t'{"name": {name}, "message": "Hello"}'"#
        );
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
    fn test_yaml_annotation_detection() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

config: Annotated[Template, "yaml"] = t"name: {name}"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("yaml".to_string()));
        assert_eq!(templates[0].content, "name: {}");
    }

    #[test]
    fn test_toml_type_alias_detection() {
        let source = r#"
type toml_config = Annotated[Template, "toml"]
config: toml_config = t"title = {title}"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("toml".to_string()));
        assert_eq!(templates[0].content, "title = {}");
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
    fn test_function_parameter_inference_propagates_to_template_variable() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

def load_config(template: Annotated[Template, "yaml"]) -> None:
    return None

config = t"name: {name}"
load_config(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_function_keyword_parameter_inference_propagates_to_template_variable() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

def load_config(template: Annotated[Template, "yaml"]) -> None:
    return None

config = t"name: {name}"
load_config(template=config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_class_constructor_inference_propagates_to_template_variable() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

class Loader:
    def __init__(self, template: Annotated[Template, "yaml"]) -> None:
        self.template = template

config = t"name: {name}"
Loader(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_class_constructor_keyword_inference_propagates_to_template_variable() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

class Loader:
    def __init__(self, template: Annotated[Template, "yaml"]) -> None:
        self.template = template

config = t"name: {name}"
Loader(template=config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_imported_function_annotation_propagates_to_template_variable() {
        let dir = parser_test_dir("imported-function");
        fs::write(
            dir.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_data(template: Annotated[Template, "yaml"]) -> object:
    return {"ok": True}
"#,
        )
        .unwrap();

        let source = r#"
from typed_api import render_data as render_yaml_data

config = t"name: {name}"
render_yaml_data(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("broken.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_imported_reexported_function_annotation_propagates_to_template_variable() {
        let dir = parser_test_dir("imported-reexported-function");
        fs::write(
            dir.join("bindings.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_data(template: Annotated[Template, "yaml"]) -> object:
    return {"ok": True}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("typed_api.py"),
            r#"from bindings import render_data
"#,
        )
        .unwrap();

        let source = r#"
from typed_api import render_data as render_yaml_data

config = t"name: {name}"
render_yaml_data(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("broken.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_imported_nested_local_reexport_annotation_propagates_to_template_variable() {
        let dir = parser_test_dir("imported-nested-local-reexport");
        fs::create_dir_all(dir.join("package")).unwrap();
        fs::write(
            dir.join("package").join("bindings.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_data(template: Annotated[Template, "yaml"]) -> object:
    return {"ok": True}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("package").join("typed_api.py"),
            r#"from bindings import render_data
"#,
        )
        .unwrap();

        let source = r#"
from package.typed_api import render_data as render_yaml_data

config = t"name: {name}"
render_yaml_data(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("broken.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_imported_class_annotation_propagates_to_template_variable() {
        let dir = parser_test_dir("imported-class");
        fs::write(
            dir.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

class Loader:
    def __init__(self, template: Annotated[Template, "yaml"]) -> None:
        self.template = template
"#,
        )
        .unwrap();

        let source = r#"
from typed_api import Loader

config = t"name: {name}"
Loader(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("broken.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_method_annotations_do_not_leak_to_top_level_calls() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

class Loader:
    def render_data(self, template: Annotated[Template, "yaml"]) -> None:
        self.template = template

config = t"name: {name}"
render_data(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
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

        let html_template = templates
            .iter()
            .find(|t| t.variable_name == Some("page".to_string()))
            .unwrap();
        assert_eq!(html_template.language, Some("html".to_string()));

        let sql_template = templates
            .iter()
            .find(|t| t.variable_name == Some("query".to_string()))
            .unwrap();
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
