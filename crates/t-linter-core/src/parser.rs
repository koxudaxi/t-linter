use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::Chars;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
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
    pub callable_signatures: HashMap<String, CallableSignature>,
    pub local_callable_signature_names: HashSet<String>,
    imported_module_paths: HashSet<String>,
    scoped_import_bindings: Vec<ScopedImportBinding>,
}

#[derive(Debug, Clone)]
struct ScopedImportBinding {
    scope: ScopeKey,
    name: String,
    binding_start: usize,
    import_target: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallableSignature {
    pub parameters: Vec<CallableParameter>,
    pub accepts_kwargs: bool,
    pub requires_positional: bool,
}

impl CallableSignature {
    fn is_empty(&self) -> bool {
        self.parameters.is_empty() && !self.accepts_kwargs && !self.requires_positional
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallableParameter {
    pub position: usize,
    pub name: String,
    pub type_annotation: Option<String>,
    pub type_annotation_module: Option<String>,
    pub template_language: Option<String>,
    pub template_profile: Option<String>,
    pub value_types: Vec<CallableValueType>,
    pub accepts_none: bool,
    pub required: bool,
    pub allows_keyword: bool,
    pub keyword_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallableValueType {
    Bool,
    Int,
    Float,
    String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TemplateHint {
    language: String,
    profile: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct CallArgument<'a> {
    position: usize,
    keyword: Option<&'a str>,
    value: Node<'a>,
}

#[derive(Debug, Clone)]
struct ExtractedInterpolation {
    debug_prefix: Option<String>,
    info: InterpolationInfo,
    format_expressions: Vec<Expression>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AssignmentKey {
    scope_start: usize,
    scope_end: usize,
    name: String,
    assignment_start: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ScopeKind {
    Module,
    FunctionLike,
    Class,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ScopeKey {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) kind: ScopeKind,
}

#[derive(Debug, Clone)]
struct VariableAssignment {
    key: AssignmentKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameBindingKind {
    Import,
    Definition,
    TypeAlias,
    Value,
}

#[derive(Debug, Clone)]
struct ScopeDirective {
    scope: ScopeKey,
    name: String,
    kind: ScopeDirectiveKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeDirectiveKind {
    Global,
    Nonlocal,
}

#[derive(Debug, Clone)]
struct NameBinding {
    scope: ScopeKey,
    name: String,
    binding_start: usize,
    kind: NameBindingKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ModuleCacheKey {
    Current,
    Path(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QualifiedName {
    parts: Vec<String>,
}

impl QualifiedName {
    fn as_string(&self) -> String {
        self.parts.join(".")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TypeExpr {
    Name(QualifiedName),
    StringLiteral(String),
    NoneLiteral,
    Generic {
        base: QualifiedName,
        args: Vec<TypeExpr>,
    },
    Union(Vec<TypeExpr>),
    Unknown(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ResolvedTypeInfo {
    template_language: Option<String>,
    template_profile: Option<String>,
    value_types: Vec<CallableValueType>,
    accepts_none: bool,
}

#[derive(Debug, Clone)]
struct ModuleTypeData {
    module_key: ModuleCacheKey,
    is_complete: bool,
    imports: HashMap<String, String>,
    alias_exprs: HashMap<String, TypeExpr>,
    callable_signatures: HashMap<String, CallableSignature>,
    local_callable_signature_names: HashSet<String>,
    imported_module_paths: HashSet<String>,
    scoped_import_bindings: Vec<ScopedImportBinding>,
}

impl Default for ModuleTypeData {
    fn default() -> Self {
        Self {
            module_key: ModuleCacheKey::Current,
            is_complete: true,
            imports: HashMap::new(),
            alias_exprs: HashMap::new(),
            callable_signatures: HashMap::new(),
            local_callable_signature_names: HashSet::new(),
            imported_module_paths: HashSet::new(),
            scoped_import_bindings: Vec::new(),
        }
    }
}

pub struct TemplateStringParser {
    parser: Parser,
    search_root: Option<PathBuf>,
    current_file_path: Option<PathBuf>,
    runtime_python_search_roots: Option<Vec<PathBuf>>,
    last_module_context: ModuleContext,
    last_module_type_data: ModuleTypeData,
    last_module_cache: HashMap<PathBuf, ModuleTypeData>,
    module_load_stack: Vec<ModuleCacheKey>,
    modules_with_incomplete_dependencies: HashSet<ModuleCacheKey>,
}

impl TemplateStringParser {
    pub fn new() -> Result<Self> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .context("Failed to set Python language")?;

        Ok(Self {
            parser,
            search_root: None,
            current_file_path: None,
            runtime_python_search_roots: None,
            last_module_context: ModuleContext::default(),
            last_module_type_data: ModuleTypeData::default(),
            last_module_cache: HashMap::new(),
            module_load_stack: Vec::new(),
            modules_with_incomplete_dependencies: HashSet::new(),
        })
    }

    pub fn find_template_strings(&mut self, source: &str) -> Result<Vec<TemplateStringInfo>> {
        self.search_root = None;
        self.current_file_path = None;
        self.find_template_strings_with_search_root(source, None)
    }

    pub fn find_template_string_locations(&mut self, source: &str) -> Result<Vec<Location>> {
        self.search_root = None;
        self.current_file_path = None;
        let tree = self
            .parser
            .parse(source, None)
            .context("Failed to parse source")?;
        let string_query = Query::new(
            &tree_sitter_python::LANGUAGE.into(),
            r#"
            (string) @string
            "#,
        )
        .context("Failed to create template string location query")?;
        let mut string_cursor = QueryCursor::new();
        let mut string_matches =
            string_cursor.matches(&string_query, tree.root_node(), source.as_bytes());
        let mut locations = Vec::new();

        while let Some(match_) = string_matches.next() {
            for capture in match_.captures {
                if string_query.capture_names()[capture.index as usize] != "string" {
                    continue;
                }
                let node = capture.node;
                if is_template_string_node(node, source)? {
                    locations.push(location_for_node(node));
                }
            }
        }

        Ok(locations)
    }

    pub fn find_template_strings_in_file(
        &mut self,
        source: &str,
        path: &Path,
    ) -> Result<Vec<TemplateStringInfo>> {
        let search_root = path.parent().map(Path::to_path_buf);
        self.current_file_path = Some(path.to_path_buf());
        self.find_template_strings_with_search_root(source, search_root)
    }

    fn find_template_strings_with_search_root(
        &mut self,
        source: &str,
        search_root: Option<PathBuf>,
    ) -> Result<Vec<TemplateStringInfo>> {
        self.search_root = search_root;
        self.last_module_cache.clear();
        self.module_load_stack.clear();
        self.modules_with_incomplete_dependencies.clear();
        if self.runtime_python_search_roots.is_none() {
            self.runtime_python_search_roots = Some(discover_runtime_python_search_roots());
        }
        let tree = self
            .parser
            .parse(source, None)
            .context("Failed to parse source")?;
        let scope_directives = collect_scope_directives(&tree, source)?;
        let assignments = collect_variable_assignments(&tree, source, &scope_directives)?;
        let name_bindings = collect_name_bindings(&tree, source)?;
        let import_resolution_filter =
            self.collect_template_relevant_import_roots(&tree, source)?;

        let mut context = ModuleContext::default();

        let module_type_data =
            self.collect_module_context(&tree, source, &import_resolution_filter, &mut context)?;
        let variable_language_hints = self.collect_variable_language_hints(
            &tree,
            source,
            &context,
            &assignments,
            &scope_directives,
            &name_bindings,
        )?;
        self.last_module_context = context.clone();
        self.last_module_type_data = module_type_data;

        let mut templates = Vec::new();
        self.find_strings_with_query(
            &tree,
            source,
            &mut templates,
            &context,
            &variable_language_hints,
            &assignments,
            &scope_directives,
            &name_bindings,
        )?;

        Ok(templates)
    }

    pub fn module_context(&self) -> &ModuleContext {
        &self.last_module_context
    }

    fn collect_module_context(
        &mut self,
        tree: &Tree,
        source: &str,
        import_resolution_filter: &HashSet<String>,
        context: &mut ModuleContext,
    ) -> Result<ModuleTypeData> {
        let mut module_cache = HashMap::new();
        let current_module_key = self
            .current_file_path
            .clone()
            .map(ModuleCacheKey::Path)
            .unwrap_or(ModuleCacheKey::Current);
        let module_type_data = self.build_module_type_data(
            tree,
            source,
            None,
            false,
            current_module_key,
            &mut module_cache,
            Some(import_resolution_filter),
        )?;

        context.imports = module_type_data.imports.clone();
        context.callable_signatures = module_type_data.callable_signatures.clone();
        context.local_callable_signature_names =
            module_type_data.local_callable_signature_names.clone();
        context.imported_module_paths = module_type_data.imported_module_paths.clone();
        context.scoped_import_bindings = module_type_data.scoped_import_bindings.clone();
        context.type_aliases =
            self.resolve_module_type_aliases(&module_type_data, &mut module_cache)?;
        self.last_module_cache = module_cache;

        Ok(module_type_data)
    }

    fn collect_template_relevant_import_roots(
        &self,
        tree: &Tree,
        source: &str,
    ) -> Result<HashSet<String>> {
        let mut roots = HashSet::new();
        let mut template_assignment_names = HashSet::new();
        let string_query = Query::new(
            &tree_sitter_python::LANGUAGE.into(),
            r#"
            (string) @string
            "#,
        )
        .context("Failed to create template string pre-scan query")?;
        let mut string_cursor = QueryCursor::new();
        let mut string_matches =
            string_cursor.matches(&string_query, tree.root_node(), source.as_bytes());

        while let Some(match_) = string_matches.next() {
            for capture in match_.captures {
                if string_query.capture_names()[capture.index as usize] != "string" {
                    continue;
                }
                let node = capture.node;
                if !is_template_string_node(node, source)? {
                    continue;
                }

                if let Some(function_node) = call_function_for_string_node(node) {
                    push_root_identifier_for_node(&mut roots, function_node, source)?;
                }

                if let Some((var_node, type_node)) = assignment_for_string_node(node) {
                    template_assignment_names
                        .insert(var_node.utf8_text(source.as_bytes())?.to_string());
                    if let Some(type_node) = type_node {
                        push_identifier_roots_from_text(
                            &mut roots,
                            type_node.utf8_text(source.as_bytes())?,
                        );
                    }
                }

                let is_raw = string_start_node(node).map(|start| {
                    self.parse_string_flags(start.utf8_text(source.as_bytes()).unwrap_or(""))
                        .is_raw
                })?;
                let (content, expressions, _) =
                    self.extract_content_and_interpolations(&node, source, is_raw, 0)?;
                push_component_roots_from_template_content(&mut roots, &content);
                for expression in expressions {
                    push_root_identifier_from_text(&mut roots, &expression.content);
                }
            }
        }

        if template_assignment_names.is_empty() {
            return Ok(roots);
        }

        let call_query = Query::new(
            &tree_sitter_python::LANGUAGE.into(),
            r#"
            (call
                function: (_) @func_expr
                arguments: (argument_list) @args)
            "#,
        )
        .context("Failed to create template call pre-scan query")?;
        let mut call_cursor = QueryCursor::new();
        let mut call_matches =
            call_cursor.matches(&call_query, tree.root_node(), source.as_bytes());

        while let Some(match_) = call_matches.next() {
            let mut func_expr_node = None;
            let mut args_node = None;

            for capture in match_.captures {
                match call_query.capture_names()[capture.index as usize] {
                    "func_expr" => func_expr_node = Some(capture.node),
                    "args" => args_node = Some(capture.node),
                    _ => {}
                }
            }

            let (Some(func_expr_node), Some(args_node)) = (func_expr_node, args_node) else {
                continue;
            };

            let arguments = call_arguments(args_node, source)?;
            let uses_template_assignment = arguments.iter().any(|argument| {
                argument.value.kind() == "identifier"
                    && argument
                        .value
                        .utf8_text(source.as_bytes())
                        .is_ok_and(|name| template_assignment_names.contains(name))
            });
            if uses_template_assignment {
                push_root_identifier_for_node(&mut roots, func_expr_node, source)?;
            }
        }

        Ok(roots)
    }

    fn build_module_type_data(
        &mut self,
        tree: &Tree,
        source: &str,
        current_module_name: Option<&str>,
        current_module_is_package: bool,
        module_key: ModuleCacheKey,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        import_resolution_filter: Option<&HashSet<String>>,
    ) -> Result<ModuleTypeData> {
        self.module_load_stack.push(module_key.clone());
        let build_result = (|| -> Result<ModuleTypeData> {
            let mut module_type_data = ModuleTypeData {
                module_key: module_key.clone(),
                ..ModuleTypeData::default()
            };
            self.collect_imports(
                tree,
                source,
                &mut module_type_data,
                current_module_name,
                current_module_is_package,
            )?;
            self.collect_type_aliases(tree, source, &mut module_type_data)?;
            self.collect_local_callable_signatures(
                tree,
                source,
                &mut module_type_data,
                module_cache,
            )?;
            self.collect_imported_callable_signatures(
                &mut module_type_data,
                module_cache,
                import_resolution_filter,
            )?;
            module_type_data.is_complete = !self
                .modules_with_incomplete_dependencies
                .remove(&module_key);
            Ok(module_type_data)
        })();
        self.module_load_stack.pop();
        build_result
    }

    fn collect_type_aliases(
        &mut self,
        tree: &Tree,
        source: &str,
        module_type_data: &mut ModuleTypeData,
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
                        if !is_module_level_statement(type_alias_node) {
                            continue;
                        }

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

                            module_type_data.alias_exprs.insert(
                                name_text.to_string(),
                                parse_type_expr(value.utf8_text(source.as_bytes())?),
                            );
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
                    if !is_module_level_statement(name_node.parent().unwrap_or(name_node)) {
                        continue;
                    }
                    let name = name_node.utf8_text(source.as_bytes())?;

                    if type_annotation_is_type_alias(type_node, source, module_type_data)? {
                        module_type_data.alias_exprs.insert(
                            name.to_string(),
                            parse_type_expr(value_node.utf8_text(source.as_bytes())?),
                        );
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
        module_type_data: &mut ModuleTypeData,
        current_module_name: Option<&str>,
        current_module_is_package: bool,
    ) -> Result<()> {
        let query_str = r#"
        ; Import statements
        (import_statement
            name: (dotted_name) @import_name)

        (import_statement
            (aliased_import
                name: (dotted_name) @import_name
                alias: (identifier) @import_alias))

        (import_from_statement
            module_name: [
                (dotted_name)
                (relative_import)
            ] @module_name
            name: (dotted_name) @import_name)

        (import_from_statement
            module_name: [
                (dotted_name)
                (relative_import)
            ] @module_name
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
            let mut has_module_name_capture = false;
            let mut import_name = None;
            let mut import_name_node = None;
            let mut import_alias = None;
            let mut import_alias_node = None;
            for capture in match_.captures {
                let name = query.capture_names()[capture.index as usize];
                match name {
                    "module_name" => {
                        has_module_name_capture = true;
                        module_name = resolve_import_module_name(
                            capture.node.utf8_text(source.as_bytes())?,
                            current_module_name,
                            current_module_is_package,
                        );
                    }
                    "import_name" => {
                        import_name = Some(capture.node.utf8_text(source.as_bytes())?);
                        import_name_node = Some(capture.node);
                    }
                    "import_alias" => {
                        import_alias = Some(capture.node.utf8_text(source.as_bytes())?);
                        import_alias_node = Some(capture.node);
                    }
                    _ => {}
                }
            }

            if has_module_name_capture && module_name.is_none() {
                continue;
            }

            if let Some(import) = import_name {
                let is_from_import = module_name.is_some();
                let key = if let Some(alias) = import_alias {
                    alias.to_string()
                } else if is_from_import {
                    import.split('.').last().unwrap_or(import).to_string()
                } else {
                    import.split('.').next().unwrap_or(import).to_string()
                };

                let value = if let Some(module) = module_name {
                    format!("{}.{}", module, import)
                } else if import_alias.is_some() {
                    import.to_string()
                } else {
                    import.split('.').next().unwrap_or(import).to_string()
                };

                if let Some(binding_node) = import_alias_node.or(import_name_node) {
                    let scope = enclosing_scope(binding_node);
                    module_type_data
                        .scoped_import_bindings
                        .push(ScopedImportBinding {
                            scope,
                            name: key.clone(),
                            binding_start: binding_node.start_byte(),
                            import_target: value.clone(),
                        });
                    if matches!(scope.kind, ScopeKind::Module) {
                        module_type_data.imports.insert(key, value);
                    }
                }

                if !is_from_import {
                    for module_path in imported_module_paths_for(import) {
                        module_type_data.imported_module_paths.insert(module_path);
                    }
                }
            }
        }

        module_type_data
            .scoped_import_bindings
            .sort_by_key(|binding| binding.binding_start);

        Ok(())
    }

    fn collect_local_callable_signatures(
        &mut self,
        tree: &Tree,
        source: &str,
        module_type_data: &mut ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
    ) -> Result<()> {
        let root = tree.root_node();
        let mut cursor = root.walk();

        for child in root.children(&mut cursor) {
            let Some(definition) = definition_node_for_statement(child) else {
                continue;
            };
            let decorators = decorators_for_statement(child);
            match definition.kind() {
                "function_definition" => {
                    let Some(name_node) = definition.child_by_field_name("name") else {
                        continue;
                    };
                    let Some(params_node) = definition.child_by_field_name("parameters") else {
                        continue;
                    };
                    let name = name_node.utf8_text(source.as_bytes())?.to_string();
                    let signature = self.extract_callable_signature(
                        params_node,
                        source,
                        module_type_data,
                        module_cache,
                        false,
                    )?;
                    if !signature.is_empty() {
                        module_type_data
                            .local_callable_signature_names
                            .insert(name.clone());
                        module_type_data.callable_signatures.insert(name, signature);
                    }
                }
                "class_definition" => {
                    let Some(name_node) = definition.child_by_field_name("name") else {
                        continue;
                    };
                    let Some(body_node) = definition.child_by_field_name("body") else {
                        continue;
                    };
                    let name = name_node.utf8_text(source.as_bytes())?.to_string();
                    let dataclass_generates_init =
                        dataclass_decorator_generates_init(decorators, source, module_type_data)?;
                    if let Some(signature) = self.extract_class_constructor_languages(
                        body_node,
                        source,
                        module_type_data,
                        module_cache,
                        dataclass_generates_init,
                    )? && !signature.is_empty()
                    {
                        module_type_data
                            .local_callable_signature_names
                            .insert(name.clone());
                        module_type_data.callable_signatures.insert(name, signature);
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn extract_class_constructor_languages(
        &mut self,
        body_node: Node,
        source: &str,
        module_type_data: &ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        dataclass_generates_init: bool,
    ) -> Result<Option<CallableSignature>> {
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
            let signature = self.extract_callable_signature(
                params_node,
                source,
                module_type_data,
                module_cache,
                true,
            )?;
            return Ok(Some(signature));
        }

        if dataclass_generates_init {
            return self.extract_dataclass_constructor_signature(
                body_node,
                source,
                module_type_data,
                module_cache,
            );
        }

        Ok(None)
    }

    fn extract_dataclass_constructor_signature(
        &mut self,
        body_node: Node,
        source: &str,
        module_type_data: &ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
    ) -> Result<Option<CallableSignature>> {
        let mut signature = CallableSignature::default();
        let mut cursor = body_node.walk();
        let mut position = 0;
        let mut keyword_only = false;

        for statement in body_node.children(&mut cursor) {
            let Some(assignment) = dataclass_field_assignment_node(statement) else {
                continue;
            };
            let Some(left_node) = assignment.child_by_field_name("left") else {
                continue;
            };
            if left_node.kind() != "identifier" {
                continue;
            }
            let Some(type_node) = assignment.child_by_field_name("type") else {
                continue;
            };
            let type_text = type_node.utf8_text(source.as_bytes())?;
            let type_expr = parse_type_expr(type_text);
            let field_type_expr = match dataclass_annotation_kind(&type_expr, module_type_data) {
                DataclassAnnotationKind::ClassVar => continue,
                DataclassAnnotationKind::KeywordOnlyMarker => {
                    keyword_only = true;
                    continue;
                }
                DataclassAnnotationKind::Field(expr) => expr,
            };
            let Some(required) =
                dataclass_field_requiredness(assignment, source, module_type_data)?
            else {
                continue;
            };

            let mut visited = HashSet::new();
            let type_hints = self.resolve_type_expr(
                field_type_expr,
                module_type_data,
                module_cache,
                &mut visited,
            )?;
            let type_annotation = checker_type_annotation_from_expr(field_type_expr)
                .or_else(|| checker_type_annotation_from_text(type_text));
            signature.parameters.push(CallableParameter {
                position,
                name: left_node.utf8_text(source.as_bytes())?.to_string(),
                type_annotation,
                type_annotation_module: None,
                template_language: type_hints.template_language,
                template_profile: type_hints.template_profile,
                value_types: type_hints.value_types,
                accepts_none: type_hints.accepts_none,
                required,
                allows_keyword: true,
                keyword_only,
            });
            position += 1;
        }

        Ok((!signature.is_empty()).then_some(signature))
    }

    fn extract_callable_signature(
        &mut self,
        params_node: Node,
        source: &str,
        module_type_data: &ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        skip_receiver: bool,
    ) -> Result<CallableSignature> {
        let mut signature = CallableSignature::default();
        let mut cursor = params_node.walk();
        let mut position = 0;
        let mut receiver_skipped = !skip_receiver;
        let mut keyword_only = false;
        for child in params_node.children(&mut cursor) {
            match child.kind() {
                "keyword_separator" => keyword_only = true,
                "positional_separator" => {
                    if signature
                        .parameters
                        .iter()
                        .any(|parameter| parameter.required && parameter.allows_keyword)
                    {
                        signature.requires_positional = true;
                    }
                    for parameter in &mut signature.parameters {
                        parameter.allows_keyword = false;
                    }
                }
                "dictionary_splat_pattern" => {
                    signature.accepts_kwargs = true;
                }
                "list_splat_pattern" => {
                    receiver_skipped = true;
                    keyword_only = true;
                    // Upstream tdom rejects component call targets backed by
                    // varargs callables in addition to positional-only ones.
                    signature.requires_positional = true;
                }
                "typed_parameter"
                | "typed_default_parameter"
                | "identifier"
                | "default_parameter" => {
                    let parameter_name = match child.kind() {
                        "typed_parameter" => {
                            let Some(name_node) = child.child(0) else {
                                continue;
                            };
                            match name_node.kind() {
                                "dictionary_splat_pattern" => {
                                    signature.accepts_kwargs = true;
                                    receiver_skipped = true;
                                    continue;
                                }
                                "list_splat_pattern" => {
                                    receiver_skipped = true;
                                    keyword_only = true;
                                    // Upstream tdom rejects component call
                                    // targets backed by varargs callables.
                                    signature.requires_positional = true;
                                    continue;
                                }
                                _ => name_node.utf8_text(source.as_bytes()).ok().unwrap_or(""),
                            }
                        }
                        "typed_default_parameter" | "default_parameter" => child
                            .child_by_field_name("name")
                            .and_then(|node| node.utf8_text(source.as_bytes()).ok())
                            .unwrap_or(""),
                        "identifier" => child.utf8_text(source.as_bytes()).ok().unwrap_or(""),
                        _ => "",
                    };
                    if !receiver_skipped && matches!(parameter_name, "self" | "cls") {
                        receiver_skipped = true;
                        continue;
                    }

                    let required = matches!(child.kind(), "typed_parameter" | "identifier");
                    let type_node = child.child_by_field_name("type");
                    let (type_hints, type_annotation) = if let Some(type_node) = type_node {
                        let type_text = type_node.utf8_text(source.as_bytes())?;
                        (
                            self.resolve_type_info_from_text(
                                type_text,
                                module_type_data,
                                module_cache,
                            )?,
                            checker_type_annotation_from_text(type_text),
                        )
                    } else {
                        (ResolvedTypeInfo::default(), None)
                    };

                    signature.parameters.push(CallableParameter {
                        position,
                        name: parameter_name.to_string(),
                        type_annotation,
                        type_annotation_module: None,
                        template_language: type_hints.template_language,
                        template_profile: type_hints.template_profile,
                        value_types: type_hints.value_types,
                        accepts_none: type_hints.accepts_none,
                        required,
                        allows_keyword: true,
                        keyword_only,
                    });
                    position += 1;
                    receiver_skipped = true;
                }
                _ => {}
            }
        }

        Ok(signature)
    }

    fn collect_imported_callable_signatures(
        &mut self,
        module_type_data: &mut ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        import_resolution_filter: Option<&HashSet<String>>,
    ) -> Result<()> {
        let import_aliases = module_type_data.imports.keys().cloned().collect::<Vec<_>>();
        for alias in import_aliases {
            let Some(import_path) = module_type_data.imports.get(&alias).cloned() else {
                continue;
            };
            if !import_matches_resolution_filter(&alias, &import_path, import_resolution_filter) {
                continue;
            }
            if !should_resolve_imported_signatures(&import_path) {
                continue;
            }

            if !self.import_path_resolves_to_module(&import_path) {
                if let Some(mut signatures) =
                    self.resolve_imported_callable_signature(&import_path, module_cache)?
                {
                    if let Some((module_name, _)) = import_path.rsplit_once('.') {
                        mark_signature_type_annotation_module(&mut signatures, module_name);
                    }
                    module_type_data
                        .callable_signatures
                        .entry(alias.clone())
                        .or_insert_with(|| signatures.clone());
                    module_type_data
                        .callable_signatures
                        .entry(import_path.clone())
                        .or_insert(signatures);
                }
            }

            if let Some(module_signatures) =
                self.load_imported_module_type_data(&import_path, module_cache)?
            {
                for (callable_name, mut signatures) in module_signatures.callable_signatures {
                    mark_signature_type_annotation_module(&mut signatures, &import_path);
                    module_type_data
                        .callable_signatures
                        .entry(format!("{import_path}.{callable_name}"))
                        .or_insert(signatures);
                }
            }
        }

        let imported_module_paths = module_type_data
            .imported_module_paths
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        for import_path in imported_module_paths {
            if !module_path_matches_resolution_filter(&import_path, import_resolution_filter)
                && !scoped_import_matches_resolution_filter(
                    &module_type_data.scoped_import_bindings,
                    &import_path,
                    import_resolution_filter,
                )
            {
                continue;
            }
            if !should_resolve_imported_signatures(&import_path) {
                continue;
            }

            if let Some(module_signatures) =
                self.load_imported_module_type_data(&import_path, module_cache)?
            {
                for (callable_name, mut signatures) in module_signatures.callable_signatures {
                    mark_signature_type_annotation_module(&mut signatures, &import_path);
                    module_type_data
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
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
    ) -> Result<Option<CallableSignature>> {
        let Some((module_name, symbol_name)) = import_path.rsplit_once('.') else {
            return Ok(None);
        };

        let Some(module_signatures) =
            self.load_imported_module_type_data(module_name, module_cache)?
        else {
            return Ok(None);
        };

        Ok(module_signatures
            .callable_signatures
            .get(symbol_name)
            .cloned())
    }

    fn load_imported_module_type_data(
        &mut self,
        module_name: &str,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
    ) -> Result<Option<ModuleTypeData>> {
        let Some(module_path) = self.resolve_python_module_path(module_name) else {
            return Ok(None);
        };

        let module_key = ModuleCacheKey::Path(module_path.clone());
        if self.module_load_stack.contains(&module_key) {
            if let Some(current_module) = self.module_load_stack.last() {
                self.modules_with_incomplete_dependencies
                    .insert(current_module.clone());
            }
            return Ok(None);
        }

        if let Some(module_type_data) = module_cache.get(&module_path) {
            if module_type_data.is_complete {
                return Ok(Some(module_type_data.clone()));
            }
        }

        module_cache.insert(
            module_path.clone(),
            ModuleTypeData {
                module_key: module_key.clone(),
                is_complete: false,
                ..ModuleTypeData::default()
            },
        );

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

        let imported_type_data = (|| -> Result<ModuleTypeData> {
            self.build_module_type_data(
                &tree,
                &source,
                Some(module_name),
                is_package_init_path(&module_path),
                module_key,
                module_cache,
                None,
            )
        })();

        self.search_root = original_search_root;

        let imported_type_data = imported_type_data?;

        module_cache.insert(module_path, imported_type_data.clone());
        Ok(Some(imported_type_data))
    }

    fn import_path_resolves_to_module(&self, import_path: &str) -> bool {
        self.resolve_python_module_path(import_path).is_some()
    }

    fn resolve_python_module_path(&self, module_name: &str) -> Option<PathBuf> {
        for search_root in self.python_search_roots() {
            if let Some(path) = resolve_local_module_path(&search_root, module_name) {
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
        assignments: &[VariableAssignment],
        scope_directives: &[ScopeDirective],
        name_bindings: &[NameBinding],
    ) -> Result<HashMap<AssignmentKey, TemplateHint>> {
        let query_str = r#"
        (call
            function: (_) @func_expr
            arguments: (argument_list) @args)
        "#;

        let query = Query::new(&tree_sitter_python::LANGUAGE.into(), query_str)
            .context("Failed to create call query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
        let mut hints = HashMap::<AssignmentKey, Option<TemplateHint>>::new();

        while let Some(match_) = matches.next() {
            let mut func_expr = None;
            let mut func_expr_node = None;
            let mut args_node = None;

            for capture in match_.captures {
                let name = query.capture_names()[capture.index as usize];
                match name {
                    "func_expr" => {
                        func_expr = Some(capture.node.utf8_text(source.as_bytes())?);
                        func_expr_node = Some(capture.node);
                    }
                    "args" => args_node = Some(capture.node),
                    _ => {}
                }
            }

            let (Some(callee), Some(callee_node), Some(args)) =
                (func_expr, func_expr_node, args_node)
            else {
                continue;
            };
            let Some(signatures) = self.lookup_callable_signatures(
                callee,
                Some(callee_node),
                source,
                context,
                assignments,
                scope_directives,
                name_bindings,
            )?
            else {
                continue;
            };

            let arguments = call_arguments(args, source)?;
            for argument in arguments {
                if argument.value.kind() != "identifier" {
                    continue;
                }
                let Some(parameter) = resolve_callable_parameter(
                    &signatures.parameters,
                    argument.position,
                    argument.keyword,
                ) else {
                    continue;
                };
                let Some(binding) = resolve_assignment_for_identifier(
                    argument.value,
                    source,
                    &assignments,
                    scope_directives,
                )?
                else {
                    continue;
                };
                let parameter_hint = parameter_template_hint(parameter);
                match (hints.get(&binding.key), parameter_hint) {
                    (Some(Some(existing)), Some(hint)) if existing != &hint => {
                        hints.insert(binding.key.clone(), None);
                    }
                    (Some(Some(_)), None) => {}
                    (Some(None), _) => {}
                    (_, Some(hint)) => {
                        hints.insert(binding.key.clone(), Some(hint));
                    }
                    (_, None) => {}
                }
            }
        }

        Ok(hints
            .into_iter()
            .filter_map(|(name, hint)| hint.map(|hint| (name, hint)))
            .collect())
    }

    fn find_strings_with_query(
        &mut self,
        tree: &Tree,
        source: &str,
        templates: &mut Vec<TemplateStringInfo>,
        context: &ModuleContext,
        variable_language_hints: &HashMap<AssignmentKey, TemplateHint>,
        assignments: &[VariableAssignment],
        scope_directives: &[ScopeDirective],
        name_bindings: &[NameBinding],
    ) -> Result<()> {
        let query_str = r#"
        (string) @string
    "#;

        let query = Query::new(&tree_sitter_python::LANGUAGE.into(), query_str)
            .context("Failed to create query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());

        let mut processed_nodes = HashSet::new();

        while let Some(match_) = matches.next() {
            let mut string_node = None;

            for capture in match_.captures {
                let name = query.capture_names()[capture.index as usize];
                match name {
                    "string" => string_node = Some(capture.node),
                    _ => {}
                }
            }

            if let Some(node) = string_node {
                let node = self
                    .template_concatenation_root(node, source)?
                    .unwrap_or(node);
                let node_id = node.id();
                if processed_nodes.contains(&node_id) {
                    continue;
                }

                if !node_is_template_or_template_concat(node, source)? {
                    continue;
                }

                let assignment = assignment_for_string_node(node);
                let var_name_node = assignment.as_ref().map(|(name_node, _)| *name_node);
                let var_name = var_name_node
                    .map(|name_node| name_node.utf8_text(source.as_bytes()))
                    .transpose()?;
                let type_annotation = assignment.and_then(|(_, type_node)| type_node);
                let func_name_node = call_function_for_string_node(node);
                let func_name = func_name_node
                    .map(|node| node.utf8_text(source.as_bytes()))
                    .transpose()?;
                processed_nodes.insert(node_id);
                let info = self.extract_template_info(
                    node,
                    source,
                    var_name,
                    var_name_node,
                    type_annotation,
                    func_name,
                    context,
                    variable_language_hints,
                    assignments,
                    scope_directives,
                    name_bindings,
                )?;
                templates.push(info);
            }
        }

        Ok(())
    }

    fn template_concatenation_root<'tree>(
        &self,
        node: Node<'tree>,
        source: &str,
    ) -> Result<Option<Node<'tree>>> {
        let mut current = node;
        let mut root = None;

        while let Some(parent) = current.parent() {
            match parent.kind() {
                "concatenated_string" => {
                    root = Some(parent);
                    current = parent;
                }
                "binary_operator" if is_plus_binary_operator(parent, source)? => {
                    root = Some(parent);
                    current = parent;
                }
                _ => break,
            }
        }

        let Some(root) = root else {
            return Ok(None);
        };
        let mut strings = Vec::new();
        collect_concat_strings(root, source, &mut strings)?;
        if strings.len() > 1 && all_template_string_nodes(&strings, source)? {
            return Ok(Some(root));
        }
        Ok(None)
    }

    fn extract_template_info(
        &mut self,
        node: Node,
        source: &str,
        var_name: Option<&str>,
        var_name_node: Option<Node>,
        type_annotation: Option<Node>,
        func_name: Option<&str>,
        context: &ModuleContext,
        variable_language_hints: &HashMap<AssignmentKey, TemplateHint>,
        assignments: &[VariableAssignment],
        scope_directives: &[ScopeDirective],
        name_bindings: &[NameBinding],
    ) -> Result<TemplateStringInfo> {
        if node.kind() != "string" {
            return self.extract_concatenated_template_info(
                node,
                source,
                var_name,
                var_name_node,
                type_annotation,
                func_name,
                context,
                variable_language_hints,
                assignments,
                scope_directives,
                name_bindings,
            );
        }

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
            self.extract_content_and_interpolations(&node, source, flags.is_raw, 0)?;

        let mut hint = if let Some(type_node) = type_annotation {
            if type_annotation_references_local_type_alias(
                type_node,
                source,
                name_bindings,
                scope_directives,
            )? {
                None
            } else {
                self.resolve_template_hint_from_type_node(type_node, source)?
            }
        } else if let Some(func) = func_name {
            self.infer_template_hint_from_function_call(
                func,
                &node,
                source,
                context,
                assignments,
                scope_directives,
                name_bindings,
            )?
        } else if let Some(return_type_node) = return_type_for_string_node(node) {
            self.resolve_template_hint_from_type_node(return_type_node, source)?
        } else {
            None
        };
        if hint.is_none() {
            if let Some(var_node) = var_name_node
                && let Some(binding) =
                    assignment_key_for_node_with_directives(var_node, source, scope_directives)
            {
                hint = variable_language_hints.get(&binding).cloned();
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
            language: hint.as_ref().map(|hint| hint.language.clone()),
            profile: hint.and_then(|hint| hint.profile),
            string_start: start_text.to_string(),
            string_end: end_text.to_string(),
            location: Location {
                start_line: start_position.row + 1,
                start_column: start_position.column + 1,
                end_line: end_position.row + 1,
                end_column: end_position.column + 1,
            },
            formatting_wrapper_location: formatting_wrapper_location(node),
            expressions,
            parts,
            flags,
        })
    }

    fn extract_concatenated_template_info(
        &mut self,
        node: Node,
        source: &str,
        var_name: Option<&str>,
        var_name_node: Option<Node>,
        type_annotation: Option<Node>,
        func_name: Option<&str>,
        context: &ModuleContext,
        variable_language_hints: &HashMap<AssignmentKey, TemplateHint>,
        assignments: &[VariableAssignment],
        scope_directives: &[ScopeDirective],
        name_bindings: &[NameBinding],
    ) -> Result<TemplateStringInfo> {
        let mut string_nodes = Vec::new();
        collect_concat_strings(node, source, &mut string_nodes)?;
        if string_nodes.is_empty() {
            return Err(anyhow::anyhow!("No string nodes in concatenated template"));
        }

        let first_string = string_nodes[0];
        let last_string = string_nodes[string_nodes.len() - 1];
        let first_start = string_start_node(first_string)?;
        let last_end = string_end_node(last_string)?;
        let start_text = first_start.utf8_text(source.as_bytes())?;
        let end_text = last_end.utf8_text(source.as_bytes())?;
        let raw_content = node.utf8_text(source.as_bytes())?;
        let flags = self.parse_string_flags(start_text);

        let mut content = String::new();
        let mut expressions = Vec::new();
        let mut parts = Vec::new();
        let mut interpolation_index = 0;
        let mut previous_content_end = None;

        for string_node in string_nodes {
            let string_start = string_start_node(string_node)?;
            let string_end = string_end_node(string_node)?;
            if let Some(previous_content_end) = previous_content_end {
                let gap = &source[previous_content_end..string_start.end_byte()];
                if !gap.is_empty() {
                    parts.push(TemplatePart::Static(StaticTextSegment {
                        text: String::new(),
                        raw_text: gap.to_string(),
                    }));
                }
            }

            let segment_flags = self.parse_string_flags(string_start.utf8_text(source.as_bytes())?);
            let (segment_content, mut segment_expressions, mut segment_parts) = self
                .extract_content_and_interpolations(
                    &string_node,
                    source,
                    segment_flags.is_raw,
                    interpolation_index,
                )?;
            interpolation_index += segment_parts
                .iter()
                .filter(|part| matches!(part, TemplatePart::Interpolation(_)))
                .count();
            content.push_str(&segment_content);
            expressions.append(&mut segment_expressions);
            parts.append(&mut segment_parts);
            previous_content_end = Some(string_end.start_byte());
        }

        let mut hint = if let Some(type_node) = type_annotation {
            if type_annotation_references_local_type_alias(
                type_node,
                source,
                name_bindings,
                scope_directives,
            )? {
                None
            } else {
                self.resolve_template_hint_from_type_node(type_node, source)?
            }
        } else if let Some(func) = func_name {
            self.infer_template_hint_from_function_call(
                func,
                &node,
                source,
                context,
                assignments,
                scope_directives,
                name_bindings,
            )?
        } else if let Some(return_type_node) = return_type_for_string_node(node) {
            self.resolve_template_hint_from_type_node(return_type_node, source)?
        } else {
            None
        };
        if hint.is_none() {
            if let Some(var_node) = var_name_node
                && let Some(binding) =
                    assignment_key_for_node_with_directives(var_node, source, scope_directives)
            {
                hint = variable_language_hints.get(&binding).cloned();
            }
        }

        let start_position = node.start_position();
        let end_position = node.end_position();

        Ok(TemplateStringInfo {
            content,
            raw_content: raw_content.to_string(),
            variable_name: var_name.map(String::from),
            function_name: func_name.map(String::from),
            language: hint.as_ref().map(|hint| hint.language.clone()),
            profile: hint.and_then(|hint| hint.profile),
            string_start: start_text.to_string(),
            string_end: end_text.to_string(),
            location: Location {
                start_line: start_position.row + 1,
                start_column: start_position.column + 1,
                end_line: end_position.row + 1,
                end_column: end_position.column + 1,
            },
            formatting_wrapper_location: formatting_wrapper_location(node),
            expressions,
            parts,
            flags,
        })
    }

    fn parse_string_flags(&self, start_text: &str) -> TemplateStringFlags {
        let mut flags = TemplateStringFlags::default();

        let prefix = string_prefix(start_text).unwrap_or_default();

        flags.is_template = true;
        flags.is_raw = prefix.bytes().any(|byte| byte.eq_ignore_ascii_case(&b'r'));
        flags.is_format = true;
        flags.is_triple = start_text.ends_with("'''") || start_text.ends_with("\"\"\"");

        flags
    }

    fn extract_content_and_interpolations(
        &self,
        string_node: &Node,
        source: &str,
        is_raw: bool,
        interpolation_index_start: usize,
    ) -> Result<(String, Vec<Expression>, Vec<TemplatePart>)> {
        let mut content_parts = Vec::new();
        let mut expressions = Vec::new();
        let mut parts = Vec::new();
        let mut cursor = string_node.walk();
        let mut last_end_byte = 0;
        let mut interpolation_index = interpolation_index_start;

        for child in string_node.children(&mut cursor) {
            match child.kind() {
                "string_content" => {
                    let start_byte = child.start_byte();
                    let end_byte = child.end_byte();

                    if last_end_byte > 0 && start_byte > last_end_byte {
                        let between = &source[last_end_byte..start_byte];
                        push_static_part(&mut content_parts, &mut parts, between, between);
                    }

                    let text = child.utf8_text(source.as_bytes())?;
                    let processed_content = unescape_template_text(text, is_raw);
                    push_static_part(&mut content_parts, &mut parts, &processed_content, text);
                    last_end_byte = end_byte;
                }
                "interpolation" => {
                    let start_byte = child.start_byte();

                    if last_end_byte > 0 && start_byte > last_end_byte {
                        let between = &source[last_end_byte..start_byte];
                        push_static_part(&mut content_parts, &mut parts, between, between);
                    }

                    if let Some(interpolation) =
                        self.extract_interpolation_expression(&child, source, interpolation_index)?
                    {
                        if let Some(prefix) = interpolation.debug_prefix.as_deref() {
                            content_parts.push(prefix.to_string());
                        }
                        content_parts.push("{}".to_string());
                        let expr = Expression {
                            content: interpolation.info.expression.clone(),
                            location: interpolation.info.location.clone(),
                        };
                        expressions.push(expr);
                        expressions.extend(interpolation.format_expressions);
                        parts.push(TemplatePart::Interpolation(interpolation.info));
                        interpolation_index += 1;
                    }

                    last_end_byte = child.end_byte();
                }
                "escape_interpolation" => {
                    let start_byte = child.start_byte();

                    if last_end_byte > 0 && start_byte > last_end_byte {
                        let between = &source[last_end_byte..start_byte];
                        push_static_part(&mut content_parts, &mut parts, between, between);
                    }

                    let text = child.utf8_text(source.as_bytes())?;
                    if text == "{{" {
                        push_static_part(&mut content_parts, &mut parts, "{", "{{");
                    } else if text == "}}" {
                        push_static_part(&mut content_parts, &mut parts, "}", "}}");
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
                        push_static_part(&mut content_parts, &mut parts, between, between);
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
    ) -> Result<Option<ExtractedInterpolation>> {
        let mut cursor = interpolation_node.walk();
        let mut expression = None;
        let mut location = None;
        let mut conversion = None;
        let mut format_spec = String::new();
        let mut format_expressions = Vec::new();
        let mut open_end = interpolation_node.start_byte();
        let mut equals_start = None;
        let mut debug_end = None;

        for child in interpolation_node.children(&mut cursor) {
            match child.kind() {
                "{" => open_end = child.end_byte(),
                "}" => {
                    if equals_start.is_some() && debug_end.is_none() {
                        debug_end = Some(child.start_byte());
                    }
                }
                "=" => {
                    equals_start = Some(child.start_byte());
                }
                "type_conversion" => {
                    if equals_start.is_some() && debug_end.is_none() {
                        debug_end = Some(child.start_byte());
                    }
                    let value = child.utf8_text(source.as_bytes())?;
                    conversion = value.strip_prefix('!').map(str::to_string);
                }
                "format_specifier" => {
                    if equals_start.is_some() && debug_end.is_none() {
                        debug_end = Some(child.start_byte());
                    }
                    let value = child.utf8_text(source.as_bytes())?;
                    format_spec = value.strip_prefix(':').unwrap_or(value).to_string();
                    format_expressions.extend(collect_format_specifier_expressions(child, source)?);
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

        Ok(expression.zip(location).map(|(expression, location)| {
            let debug_prefix = equals_start.map(|_| {
                let end = debug_end.unwrap_or(interpolation_node.end_byte());
                source[open_end..end].to_string()
            });
            let conversion = conversion.or_else(|| debug_prefix.as_ref().map(|_| "r".into()));
            ExtractedInterpolation {
                debug_prefix: debug_prefix.clone(),
                info: InterpolationInfo {
                    expression,
                    debug_prefix: debug_prefix.clone(),
                    conversion,
                    format_spec,
                    raw_source: source
                        [interpolation_node.start_byte()..interpolation_node.end_byte()]
                        .to_string(),
                    location,
                    interpolation_index,
                },
                format_expressions,
            }
        }))
    }

    fn infer_template_hint_from_function_call(
        &self,
        func_name: &str,
        string_node: &Node,
        source: &str,
        context: &ModuleContext,
        assignments: &[VariableAssignment],
        scope_directives: &[ScopeDirective],
        name_bindings: &[NameBinding],
    ) -> Result<Option<TemplateHint>> {
        let callee_node = string_node
            .parent()
            .and_then(|parent| match parent.kind() {
                "argument_list" => parent.parent(),
                "keyword_argument" => parent
                    .parent()
                    .and_then(|argument_list| argument_list.parent()),
                _ => None,
            })
            .and_then(|call_node| call_node.child_by_field_name("function"));

        let Some(signatures) = self.lookup_callable_signatures(
            func_name,
            callee_node,
            source,
            context,
            assignments,
            scope_directives,
            name_bindings,
        )?
        else {
            return self.infer_template_hint_from_explicit_callee_target(
                func_name,
                callee_node,
                source,
                context,
                assignments,
                scope_directives,
                name_bindings,
            );
        };

        let argument_list = match string_node.parent() {
            Some(parent) if parent.kind() == "argument_list" => Some(parent),
            Some(parent) if parent.kind() == "keyword_argument" => parent.parent(),
            _ => None,
        };

        if let Some(call_node) = argument_list {
            for argument in call_arguments(call_node, source)? {
                if argument.value.kind() == "string" && argument.value.id() == string_node.id() {
                    if let Some(parameter) = resolve_callable_parameter(
                        &signatures.parameters,
                        argument.position,
                        argument.keyword,
                    ) {
                        if let Some(hint) = parameter_template_hint(parameter) {
                            return Ok(Some(hint));
                        }
                    }
                    break;
                }
            }
        }

        self.infer_template_hint_from_explicit_callee_target(
            func_name,
            callee_node,
            source,
            context,
            assignments,
            scope_directives,
            name_bindings,
        )
    }

    fn infer_template_hint_from_explicit_callee_target(
        &self,
        func_name: &str,
        callee_node: Option<Node>,
        source: &str,
        context: &ModuleContext,
        assignments: &[VariableAssignment],
        scope_directives: &[ScopeDirective],
        name_bindings: &[NameBinding],
    ) -> Result<Option<TemplateHint>> {
        let Some(target) = self.resolve_callee_import_target(
            func_name,
            callee_node,
            source,
            context,
            assignments,
            scope_directives,
            name_bindings,
        )?
        else {
            return Ok(None);
        };

        self.resolve_template_hint_from_explicit_callee_target(&target, &mut HashSet::new())
    }

    fn resolve_template_hint_from_explicit_callee_target(
        &self,
        target: &str,
        visited: &mut HashSet<String>,
    ) -> Result<Option<TemplateHint>> {
        if !visited.insert(target.to_string()) {
            return Ok(None);
        }

        if let Some(hint) = tdom_template_processor_hint(target) {
            return Ok(Some(hint));
        }

        let Some((module_name, symbol_name)) = target.rsplit_once('.') else {
            return Ok(None);
        };
        let Some(module_path) = self.resolve_python_module_path(module_name) else {
            return Ok(None);
        };
        let Ok(source) = fs::read_to_string(&module_path) else {
            return Ok(None);
        };

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .context("Failed to set Python language")?;
        let Some(tree) = parser.parse(&source, None) else {
            return Ok(None);
        };

        let mut module_type_data = ModuleTypeData::default();
        let mut helper = Self::new()?;
        helper.search_root = module_path.parent().map(Path::to_path_buf);
        helper.runtime_python_search_roots = self.runtime_python_search_roots.clone();
        helper.collect_imports(
            &tree,
            &source,
            &mut module_type_data,
            Some(module_name),
            is_package_init_path(&module_path),
        )?;

        let Some(next_target) = module_type_data.imports.get(symbol_name) else {
            return Ok(None);
        };

        self.resolve_template_hint_from_explicit_callee_target(next_target, visited)
    }

    fn lookup_callable_signatures<'a>(
        &self,
        callee: &str,
        callee_node: Option<Node>,
        source: &str,
        context: &'a ModuleContext,
        assignments: &[VariableAssignment],
        scope_directives: &[ScopeDirective],
        name_bindings: &[NameBinding],
    ) -> Result<Option<&'a CallableSignature>> {
        if let Some(signatures) = context.callable_signatures.get(callee) {
            if !self.callee_matches_lookup_target(
                callee,
                callee_node,
                source,
                context,
                assignments,
                scope_directives,
                name_bindings,
            )? {
                return Ok(None);
            }
            if !callee.contains('.')
                && context.imports.contains_key(callee)
                && direct_callable_reference_is_shadowed(
                    callee_node,
                    source,
                    assignments,
                    scope_directives,
                    name_bindings,
                    context.local_callable_signature_names.contains(callee),
                )?
            {
                return Ok(None);
            }
            return Ok(Some(signatures));
        }

        let Some(import_target) = self.resolve_callee_import_target(
            callee,
            callee_node,
            source,
            context,
            assignments,
            scope_directives,
            name_bindings,
        )?
        else {
            return Ok(None);
        };

        Ok(context.callable_signatures.get(&import_target))
    }

    fn callee_matches_lookup_target(
        &self,
        callee: &str,
        callee_node: Option<Node>,
        source: &str,
        context: &ModuleContext,
        assignments: &[VariableAssignment],
        scope_directives: &[ScopeDirective],
        name_bindings: &[NameBinding],
    ) -> Result<bool> {
        if !callee.contains('.') {
            return Ok(true);
        }

        let Some(import_target) = self.resolve_callee_import_target(
            callee,
            callee_node,
            source,
            context,
            assignments,
            scope_directives,
            name_bindings,
        )?
        else {
            return Ok(false);
        };

        Ok(callable_matches_import_target(callee, &import_target))
    }

    fn resolve_callee_import_target(
        &self,
        callee: &str,
        callee_node: Option<Node>,
        source: &str,
        context: &ModuleContext,
        assignments: &[VariableAssignment],
        scope_directives: &[ScopeDirective],
        name_bindings: &[NameBinding],
    ) -> Result<Option<String>> {
        if callee.contains('.') {
            if module_reference_is_shadowed(
                callee_node,
                source,
                assignments,
                scope_directives,
                name_bindings,
            )? {
                return Ok(None);
            }

            let Some(root_identifier) = callee_node.and_then(root_identifier_node) else {
                return Ok(None);
            };
            let import_target = resolve_import_target_for_identifier(
                root_identifier,
                source,
                scope_directives,
                &context.scoped_import_bindings,
            )?
            .or_else(|| context.imports.get(root_identifier_text(callee)).cloned());
            let Some(import_target) = import_target else {
                return Ok(None);
            };

            let suffix = &callee[root_identifier_text(callee).len()..];
            return Ok(Some(format!("{import_target}{suffix}")));
        }

        if context.imports.contains_key(callee)
            && direct_callable_reference_is_shadowed(
                callee_node,
                source,
                assignments,
                scope_directives,
                name_bindings,
                context.local_callable_signature_names.contains(callee),
            )?
        {
            return Ok(None);
        }

        Ok(context.imports.get(callee).cloned().or_else(|| {
            context
                .callable_signatures
                .contains_key(callee)
                .then(|| callee.to_string())
        }))
    }

    fn resolve_template_hint_from_type_node(
        &mut self,
        type_node: Node,
        source: &str,
    ) -> Result<Option<TemplateHint>> {
        let module_type_data = self.last_module_type_data.clone();
        let mut module_cache = std::mem::take(&mut self.last_module_cache);
        let resolved = self.resolve_type_info_from_type_node(
            type_node,
            source,
            &module_type_data,
            &mut module_cache,
        );
        self.last_module_cache = module_cache;
        Ok(template_hint_from_type_info(resolved?))
    }

    fn resolve_module_type_aliases(
        &mut self,
        module_type_data: &ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
    ) -> Result<HashMap<String, String>> {
        let mut resolved = HashMap::new();
        for name in module_type_data.alias_exprs.keys() {
            let mut visited = HashSet::new();
            let Some(info) = self.resolve_module_alias_type_info(
                module_type_data,
                name,
                module_cache,
                &mut visited,
            )?
            else {
                continue;
            };
            if let Some(language) = info.template_language {
                resolved.insert(name.clone(), language);
            }
        }
        Ok(resolved)
    }

    fn resolve_type_info_from_type_node(
        &mut self,
        type_node: Node,
        source: &str,
        module_type_data: &ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
    ) -> Result<ResolvedTypeInfo> {
        let type_text = type_node.utf8_text(source.as_bytes())?;
        self.resolve_type_info_from_text(type_text, module_type_data, module_cache)
    }

    fn resolve_type_info_from_text(
        &mut self,
        type_text: &str,
        module_type_data: &ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
    ) -> Result<ResolvedTypeInfo> {
        let expr = parse_type_expr(type_text);
        let mut visited = HashSet::new();
        self.resolve_type_expr(&expr, module_type_data, module_cache, &mut visited)
    }

    fn resolve_type_expr(
        &mut self,
        expr: &TypeExpr,
        module_type_data: &ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        visited: &mut HashSet<(ModuleCacheKey, String)>,
    ) -> Result<ResolvedTypeInfo> {
        match expr {
            TypeExpr::Name(name) => {
                self.resolve_name_type_info(name, module_type_data, module_cache, visited)
            }
            TypeExpr::StringLiteral(_) => Ok(ResolvedTypeInfo::default()),
            TypeExpr::NoneLiteral => Ok(ResolvedTypeInfo {
                template_language: None,
                template_profile: None,
                value_types: Vec::new(),
                accepts_none: true,
            }),
            TypeExpr::Union(parts) => {
                let mut merged = ResolvedTypeInfo::default();
                for part in parts {
                    merge_resolved_type_info(
                        &mut merged,
                        self.resolve_type_expr(part, module_type_data, module_cache, visited)?,
                    );
                }
                Ok(merged)
            }
            TypeExpr::Generic { base, args } => {
                match self.resolve_special_type_name(
                    base,
                    module_type_data,
                    module_cache,
                    visited,
                )? {
                    Some("Annotated") => self.resolve_annotated_type_info(
                        args,
                        module_type_data,
                        module_cache,
                        visited,
                    ),
                    Some("Optional") => {
                        let mut resolved = args
                            .first()
                            .map(|arg| {
                                self.resolve_type_expr(arg, module_type_data, module_cache, visited)
                            })
                            .transpose()?
                            .unwrap_or_default();
                        resolved.accepts_none = true;
                        Ok(resolved)
                    }
                    Some("Union") => {
                        let mut merged = ResolvedTypeInfo::default();
                        for arg in args {
                            merge_resolved_type_info(
                                &mut merged,
                                self.resolve_type_expr(
                                    arg,
                                    module_type_data,
                                    module_cache,
                                    visited,
                                )?,
                            );
                        }
                        Ok(merged)
                    }
                    Some("Literal") => Ok(resolve_literal_type_info(args)),
                    _ => Ok(ResolvedTypeInfo::default()),
                }
            }
            TypeExpr::Unknown(_) => Ok(ResolvedTypeInfo::default()),
        }
    }

    fn resolve_special_type_name(
        &mut self,
        name: &QualifiedName,
        module_type_data: &ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        visited: &mut HashSet<(ModuleCacheKey, String)>,
    ) -> Result<Option<&'static str>> {
        if name.parts.len() == 1 {
            let alias_name = &name.parts[0];
            let has_local_binding = module_type_data.alias_exprs.contains_key(alias_name)
                || module_type_data.imports.contains_key(alias_name);
            if let Some(alias_expr) = module_type_data.alias_exprs.get(alias_name) {
                let visit_key = (module_type_data.module_key.clone(), alias_name.to_string());
                if !visited.insert(visit_key.clone()) {
                    return Ok(None);
                }
                let kind = self.resolve_special_type_expr(
                    alias_expr,
                    module_type_data,
                    module_cache,
                    visited,
                )?;
                visited.remove(&visit_key);
                if kind.is_some() {
                    return Ok(kind);
                }
            }
            if let Some(import_target) = module_type_data.imports.get(alias_name) {
                let kind =
                    self.resolve_special_import_target(import_target, module_cache, visited)?;
                if kind.is_some() {
                    return Ok(kind);
                }
            }
            if has_local_binding {
                return Ok(None);
            }
        } else if let Some(import_target) = expand_qualified_name(name, &module_type_data.imports) {
            let kind = self.resolve_special_import_target(&import_target, module_cache, visited)?;
            if kind.is_some() {
                return Ok(kind);
            }
        }

        let raw_name = name.as_string();
        if let Some(kind) = canonical_special_name(&raw_name) {
            return Ok(Some(kind));
        }

        Ok(None)
    }

    fn resolve_special_type_expr(
        &mut self,
        expr: &TypeExpr,
        module_type_data: &ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        visited: &mut HashSet<(ModuleCacheKey, String)>,
    ) -> Result<Option<&'static str>> {
        match expr {
            TypeExpr::Name(name) => {
                self.resolve_special_type_name(name, module_type_data, module_cache, visited)
            }
            _ => Ok(None),
        }
    }

    fn resolve_special_import_target(
        &mut self,
        import_target: &str,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        visited: &mut HashSet<(ModuleCacheKey, String)>,
    ) -> Result<Option<&'static str>> {
        if let Some(kind) = canonical_special_name(import_target) {
            return Ok(Some(kind));
        }
        if self.import_path_resolves_to_module(import_target) {
            return Ok(None);
        }
        let Some((module_name, symbol_name)) = import_target.rsplit_once('.') else {
            return Ok(None);
        };
        let Some(imported_module) =
            self.load_imported_module_type_data(module_name, module_cache)?
        else {
            return Ok(None);
        };
        if let Some(kind) = canonical_special_name(symbol_name) {
            return Ok(Some(kind));
        }
        if let Some(alias_expr) = imported_module.alias_exprs.get(symbol_name) {
            let visit_key = (imported_module.module_key.clone(), symbol_name.to_string());
            if !visited.insert(visit_key.clone()) {
                return Ok(None);
            }
            let kind = self.resolve_special_type_expr(
                alias_expr,
                &imported_module,
                module_cache,
                visited,
            )?;
            visited.remove(&visit_key);
            if kind.is_some() {
                return Ok(kind);
            }
        }
        if let Some(reexport_target) = imported_module.imports.get(symbol_name) {
            return self.resolve_special_import_target(reexport_target, module_cache, visited);
        }
        Ok(None)
    }

    fn resolve_name_type_info(
        &mut self,
        name: &QualifiedName,
        module_type_data: &ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        visited: &mut HashSet<(ModuleCacheKey, String)>,
    ) -> Result<ResolvedTypeInfo> {
        if let Some(kind) =
            self.resolve_special_type_name(name, module_type_data, module_cache, visited)?
        {
            return Ok(resolved_type_info_for_special_name(kind));
        }

        if name.parts.len() == 1 {
            let alias_name = &name.parts[0];
            if let Some(resolved) = self.resolve_module_alias_type_info(
                module_type_data,
                alias_name,
                module_cache,
                visited,
            )? {
                return Ok(resolved);
            }
        }

        if let Some(import_target) = expand_qualified_name(name, &module_type_data.imports) {
            return self.resolve_import_target_type_info(&import_target, module_cache, visited);
        }

        Ok(ResolvedTypeInfo::default())
    }

    fn resolve_module_alias_type_info(
        &mut self,
        module_type_data: &ModuleTypeData,
        alias_name: &str,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        visited: &mut HashSet<(ModuleCacheKey, String)>,
    ) -> Result<Option<ResolvedTypeInfo>> {
        let Some(expr) = module_type_data.alias_exprs.get(alias_name) else {
            return Ok(None);
        };
        let visit_key = (module_type_data.module_key.clone(), alias_name.to_string());
        if !visited.insert(visit_key.clone()) {
            return Ok(None);
        }
        let resolved = self.resolve_type_expr(expr, module_type_data, module_cache, visited)?;
        visited.remove(&visit_key);
        Ok(Some(resolved))
    }

    fn resolve_import_target_type_info(
        &mut self,
        import_target: &str,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        visited: &mut HashSet<(ModuleCacheKey, String)>,
    ) -> Result<ResolvedTypeInfo> {
        if let Some(kind) = canonical_special_name(import_target) {
            return Ok(resolved_type_info_for_special_name(kind));
        }

        if self.import_path_resolves_to_module(import_target) {
            return Ok(ResolvedTypeInfo::default());
        }

        let Some((module_name, symbol_name)) = import_target.rsplit_once('.') else {
            return Ok(ResolvedTypeInfo::default());
        };
        let Some(imported_module) =
            self.load_imported_module_type_data(module_name, module_cache)?
        else {
            return Ok(ResolvedTypeInfo::default());
        };

        self.resolve_symbol_in_module(&imported_module, symbol_name, module_cache, visited)
    }

    fn resolve_symbol_in_module(
        &mut self,
        module_type_data: &ModuleTypeData,
        symbol_name: &str,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        visited: &mut HashSet<(ModuleCacheKey, String)>,
    ) -> Result<ResolvedTypeInfo> {
        if let Some(kind) = canonical_special_name(symbol_name) {
            return Ok(resolved_type_info_for_special_name(kind));
        }

        if let Some(resolved) = self.resolve_module_alias_type_info(
            module_type_data,
            symbol_name,
            module_cache,
            visited,
        )? {
            return Ok(resolved);
        }

        if let Some(import_target) = module_type_data.imports.get(symbol_name) {
            return self.resolve_import_target_type_info(import_target, module_cache, visited);
        }

        Ok(ResolvedTypeInfo::default())
    }

    fn resolve_annotated_type_info(
        &mut self,
        args: &[TypeExpr],
        module_type_data: &ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        visited: &mut HashSet<(ModuleCacheKey, String)>,
    ) -> Result<ResolvedTypeInfo> {
        let mut resolved = args
            .first()
            .map(|arg| self.resolve_type_expr(arg, module_type_data, module_cache, visited))
            .transpose()?
            .unwrap_or_default();

        if args.len() >= 2
            && self.is_template_expr(&args[0], module_type_data, module_cache, visited)?
            && let TypeExpr::StringLiteral(language) = &args[1]
        {
            resolved.template_language = Some(language.clone());
            resolved.template_profile = args.iter().skip(2).find_map(template_profile_metadata);
        }

        Ok(resolved)
    }

    fn is_template_expr(
        &mut self,
        expr: &TypeExpr,
        module_type_data: &ModuleTypeData,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        visited: &mut HashSet<(ModuleCacheKey, String)>,
    ) -> Result<bool> {
        match expr {
            TypeExpr::Name(name) => {
                let expanded = expand_qualified_name(name, &module_type_data.imports)
                    .unwrap_or_else(|| name.as_string());
                if canonical_special_name(&expanded) == Some("Template") {
                    return Ok(true);
                }

                if name.parts.len() == 1 {
                    let Some(alias_name) = name.parts.first() else {
                        return Ok(false);
                    };
                    if let Some(alias_expr) = module_type_data.alias_exprs.get(alias_name) {
                        let visit_key = (module_type_data.module_key.clone(), alias_name.clone());
                        if !visited.insert(visit_key.clone()) {
                            return Ok(false);
                        }
                        let is_template = self.is_template_expr(
                            alias_expr,
                            module_type_data,
                            module_cache,
                            visited,
                        )?;
                        visited.remove(&visit_key);
                        return Ok(is_template);
                    }
                }

                if let Some(import_target) = expand_qualified_name(name, &module_type_data.imports)
                {
                    return self.import_target_is_template(&import_target, module_cache, visited);
                }

                Ok(false)
            }
            TypeExpr::Generic { .. }
            | TypeExpr::Union(_)
            | TypeExpr::StringLiteral(_)
            | TypeExpr::NoneLiteral
            | TypeExpr::Unknown(_) => Ok(false),
        }
    }

    fn import_target_is_template(
        &mut self,
        import_target: &str,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
        visited: &mut HashSet<(ModuleCacheKey, String)>,
    ) -> Result<bool> {
        if canonical_special_name(import_target) == Some("Template") {
            return Ok(true);
        }
        if self.import_path_resolves_to_module(import_target) {
            return Ok(false);
        }
        let Some((module_name, symbol_name)) = import_target.rsplit_once('.') else {
            return Ok(false);
        };
        let Some(imported_module) =
            self.load_imported_module_type_data(module_name, module_cache)?
        else {
            return Ok(false);
        };
        if let Some(alias_expr) = imported_module.alias_exprs.get(symbol_name) {
            let visit_key = (imported_module.module_key.clone(), symbol_name.to_string());
            if !visited.insert(visit_key.clone()) {
                return Ok(false);
            }
            let is_template =
                self.is_template_expr(alias_expr, &imported_module, module_cache, visited)?;
            visited.remove(&visit_key);
            return Ok(is_template);
        }
        if let Some(reexport_target) = imported_module.imports.get(symbol_name) {
            return self.import_target_is_template(reexport_target, module_cache, visited);
        }
        Ok(false)
    }
}

fn parse_type_expr(type_text: &str) -> TypeExpr {
    let type_text = type_text.trim();
    if type_text.is_empty() {
        return TypeExpr::Unknown(String::new());
    }

    if let Some(inner) = strip_wrapping_parens(type_text) {
        return parse_type_expr(inner);
    }

    let union_parts = split_top_level_tokens(type_text, '|');
    if union_parts.len() > 1 {
        return TypeExpr::Union(union_parts.into_iter().map(parse_type_expr).collect());
    }

    if matches!(type_text, "None" | "NoneType" | "builtins.NoneType") {
        return TypeExpr::NoneLiteral;
    }

    if let Some(string_literal) = parse_string_literal(type_text) {
        return TypeExpr::StringLiteral(string_literal);
    }

    if let Some((base, args_text)) = split_generic_expr(type_text) {
        return TypeExpr::Generic {
            base: parse_qualified_name(base).unwrap_or(QualifiedName {
                parts: vec![base.trim().to_string()],
            }),
            args: split_top_level_tokens(args_text, ',')
                .into_iter()
                .map(parse_type_expr)
                .collect(),
        };
    }

    if let Some(name) = parse_qualified_name(type_text) {
        return TypeExpr::Name(name);
    }

    TypeExpr::Unknown(type_text.to_string())
}

fn parse_qualified_name(type_text: &str) -> Option<QualifiedName> {
    let parts = type_text
        .split('.')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty()
        || parts.iter().any(|part| {
            let mut chars = part.chars();
            let Some(first) = chars.next() else {
                return true;
            };
            if !(first.is_ascii_alphabetic() || first == '_') {
                return true;
            }
            !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        })
    {
        return None;
    }
    Some(QualifiedName {
        parts: parts.into_iter().map(str::to_string).collect(),
    })
}

fn split_generic_expr(type_text: &str) -> Option<(&str, &str)> {
    let bracket_start = type_text.find('[')?;
    if !type_text.ends_with(']') {
        return None;
    }
    let base = type_text[..bracket_start].trim();
    if base.is_empty() || !is_balanced_delimited(&type_text[bracket_start..], '[', ']') {
        return None;
    }
    Some((base, &type_text[bracket_start + 1..type_text.len() - 1]))
}

fn strip_wrapping_parens(type_text: &str) -> Option<&str> {
    if !(type_text.starts_with('(') && type_text.ends_with(')')) {
        return None;
    }
    if !is_balanced_delimited(type_text, '(', ')') {
        return None;
    }
    Some(type_text[1..type_text.len() - 1].trim())
}

fn is_balanced_delimited(input: &str, open: char, close: char) -> bool {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (index, ch) in input.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            _ if ch == open => depth += 1,
            _ if ch == close => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
                if depth == 0 && index + ch.len_utf8() != input.len() {
                    return false;
                }
            }
            _ => {}
        }
    }

    depth == 0 && quote.is_none()
}

fn parse_string_literal(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let quote = bytes[0];
    if !matches!(quote, b'\'' | b'"') || bytes[bytes.len() - 1] != quote {
        return None;
    }
    Some(input[1..input.len() - 1].to_string())
}

fn checker_type_annotation_from_text(type_text: &str) -> Option<String> {
    let type_text = type_text.trim();
    if type_text.is_empty() {
        return None;
    }
    parse_string_literal(type_text).or_else(|| Some(type_text.to_string()))
}

fn checker_type_annotation_from_expr(expr: &TypeExpr) -> Option<String> {
    match expr {
        TypeExpr::Name(name) => Some(name.as_string()),
        TypeExpr::StringLiteral(value) => Some(value.clone()),
        TypeExpr::NoneLiteral => Some("None".to_string()),
        TypeExpr::Generic { base, args } => {
            let args = args
                .iter()
                .filter_map(checker_type_annotation_arg_from_expr)
                .collect::<Vec<_>>();
            Some(format!("{}[{}]", base.as_string(), args.join(", ")))
        }
        TypeExpr::Union(parts) => {
            let parts = parts
                .iter()
                .filter_map(checker_type_annotation_from_expr)
                .collect::<Vec<_>>();
            (!parts.is_empty()).then(|| parts.join(" | "))
        }
        TypeExpr::Unknown(value) => (!value.trim().is_empty()).then(|| value.trim().to_string()),
    }
}

fn checker_type_annotation_arg_from_expr(expr: &TypeExpr) -> Option<String> {
    match expr {
        TypeExpr::StringLiteral(value) => Some(crate::python::double_quoted_string_literal(value)),
        _ => checker_type_annotation_from_expr(expr),
    }
}

fn split_top_level_tokens(input: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut bracket_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (index, ch) in input.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ if ch == separator && bracket_depth == 0 && paren_depth == 0 && brace_depth == 0 => {
                parts.push(input[start..index].trim());
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }

    parts.push(input[start..].trim());
    parts
}

fn expand_qualified_name(
    name: &QualifiedName,
    imports: &HashMap<String, String>,
) -> Option<String> {
    let first = name.parts.first()?;
    let import_target = imports.get(first)?;
    if name.parts.len() == 1 {
        return Some(import_target.clone());
    }
    Some(format!("{import_target}.{}", name.parts[1..].join(".")))
}

fn canonical_special_name(name: &str) -> Option<&'static str> {
    match name.trim() {
        "Annotated" | "typing.Annotated" | "typing_extensions.Annotated" => Some("Annotated"),
        "Optional" | "typing.Optional" => Some("Optional"),
        "Union" | "typing.Union" => Some("Union"),
        "Literal" | "typing.Literal" => Some("Literal"),
        "Template" | "templatelib.Template" | "string.templatelib.Template" => Some("Template"),
        "bool" | "builtins.bool" => Some("bool"),
        "int" | "builtins.int" => Some("int"),
        "float" | "builtins.float" => Some("float"),
        "str" | "builtins.str" => Some("str"),
        "None" | "NoneType" | "builtins.NoneType" => Some("None"),
        _ => None,
    }
}

fn resolved_type_info_for_special_name(kind: &str) -> ResolvedTypeInfo {
    let mut resolved = ResolvedTypeInfo::default();
    match kind {
        "bool" => push_value_type(&mut resolved.value_types, CallableValueType::Bool),
        "int" => push_value_type(&mut resolved.value_types, CallableValueType::Int),
        "float" => push_value_type(&mut resolved.value_types, CallableValueType::Float),
        "str" => push_value_type(&mut resolved.value_types, CallableValueType::String),
        "None" => resolved.accepts_none = true,
        _ => {}
    }
    resolved
}

fn template_hint_from_type_info(info: ResolvedTypeInfo) -> Option<TemplateHint> {
    info.template_language.map(|language| TemplateHint {
        language,
        profile: info.template_profile,
    })
}

fn parameter_template_hint(parameter: &CallableParameter) -> Option<TemplateHint> {
    parameter
        .template_language
        .as_ref()
        .map(|language| TemplateHint {
            language: language.clone(),
            profile: parameter.template_profile.clone(),
        })
}

fn definition_node_for_statement(statement: Node) -> Option<Node> {
    match statement.kind() {
        "function_definition" | "class_definition" => Some(statement),
        "decorated_definition" => {
            let mut cursor = statement.walk();
            for child in statement.children(&mut cursor) {
                if matches!(child.kind(), "function_definition" | "class_definition") {
                    return Some(child);
                }
            }
            None
        }
        _ => None,
    }
}

fn decorators_for_statement(statement: Node) -> Option<Node> {
    (statement.kind() == "decorated_definition").then_some(statement)
}

fn dataclass_decorator_generates_init(
    decorators: Option<Node>,
    source: &str,
    module_type_data: &ModuleTypeData,
) -> Result<bool> {
    let Some(decorators) = decorators else {
        return Ok(false);
    };

    let mut cursor = decorators.walk();
    for child in decorators.children(&mut cursor) {
        if child.kind() != "decorator" {
            continue;
        }
        let decorator_text = child.utf8_text(source.as_bytes())?;
        let target = decorator_callable_target(decorator_text);
        if qualified_text_resolves_to(target, module_type_data, "dataclasses.dataclass") {
            return Ok(!decorator_keyword_is_false(decorator_text, "init"));
        }
    }

    Ok(false)
}

fn decorator_callable_target(text: &str) -> &str {
    let text = text.trim().strip_prefix('@').unwrap_or(text.trim()).trim();
    text.split_once('(')
        .map_or(text, |(target, _)| target)
        .trim()
}

fn decorator_keyword_is_false(text: &str, keyword: &str) -> bool {
    let Some(args_text) = decorator_arguments_text(text) else {
        return false;
    };

    split_top_level_tokens(args_text, ',')
        .into_iter()
        .filter_map(|argument| argument.split_once('='))
        .any(|(name, value)| name.trim() == keyword && value.trim() == "False")
}

fn decorator_arguments_text(text: &str) -> Option<&str> {
    let text = text.trim().strip_prefix('@').unwrap_or(text.trim()).trim();
    let (_, args_with_suffix) = text.split_once('(')?;
    args_with_suffix.trim_end().strip_suffix(')').map(str::trim)
}

fn dataclass_field_assignment_node(statement: Node) -> Option<Node> {
    match statement.kind() {
        "assignment" => Some(statement),
        "expression_statement" => {
            let mut cursor = statement.walk();
            for child in statement.children(&mut cursor) {
                if child.kind() == "assignment" {
                    return Some(child);
                }
            }
            None
        }
        _ => None,
    }
}

enum DataclassAnnotationKind<'a> {
    Field(&'a TypeExpr),
    ClassVar,
    KeywordOnlyMarker,
}

fn dataclass_annotation_kind<'a>(
    expr: &'a TypeExpr,
    module_type_data: &ModuleTypeData,
) -> DataclassAnnotationKind<'a> {
    match expr {
        TypeExpr::Name(name)
            if qualified_name_resolves_to(
                name,
                module_type_data,
                &["KW_ONLY", "dataclasses.KW_ONLY"],
            ) =>
        {
            DataclassAnnotationKind::KeywordOnlyMarker
        }
        TypeExpr::Name(name) | TypeExpr::Generic { base: name, .. }
            if qualified_name_resolves_to(
                name,
                module_type_data,
                &["ClassVar", "typing.ClassVar", "typing_extensions.ClassVar"],
            ) =>
        {
            DataclassAnnotationKind::ClassVar
        }
        TypeExpr::Generic { base, args }
            if qualified_name_resolves_to(
                base,
                module_type_data,
                &["InitVar", "dataclasses.InitVar"],
            ) =>
        {
            DataclassAnnotationKind::Field(args.first().unwrap_or(expr))
        }
        _ => DataclassAnnotationKind::Field(expr),
    }
}

fn dataclass_field_requiredness(
    assignment: Node,
    source: &str,
    module_type_data: &ModuleTypeData,
) -> Result<Option<bool>> {
    let Some(value_node) = assignment.child_by_field_name("right") else {
        return Ok(Some(true));
    };
    if !dataclass_field_value_is_field_call(value_node, source, module_type_data)? {
        return Ok(Some(false));
    }
    if call_keyword_value(value_node, source, "init")?.is_some_and(|value| {
        value
            .utf8_text(source.as_bytes())
            .is_ok_and(|text| text.trim() == "False")
    }) {
        return Ok(None);
    }

    let has_default = dataclass_field_call_supplies_default(value_node, source, module_type_data)?;
    Ok(Some(!has_default))
}

fn dataclass_field_value_is_field_call(
    value_node: Node,
    source: &str,
    module_type_data: &ModuleTypeData,
) -> Result<bool> {
    if value_node.kind() != "call" {
        return Ok(false);
    }
    let Some(function_node) = value_node.child_by_field_name("function") else {
        return Ok(false);
    };
    Ok(qualified_text_resolves_to(
        function_node.utf8_text(source.as_bytes())?,
        module_type_data,
        "dataclasses.field",
    ))
}

fn dataclass_field_call_supplies_default(
    call_node: Node,
    source: &str,
    module_type_data: &ModuleTypeData,
) -> Result<bool> {
    for keyword in ["default", "default_factory"] {
        let Some(value_node) = call_keyword_value(call_node, source, keyword)? else {
            continue;
        };
        if !qualified_text_resolves_to(
            value_node.utf8_text(source.as_bytes())?,
            module_type_data,
            "dataclasses.MISSING",
        ) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn call_keyword_value<'tree>(
    call_node: Node<'tree>,
    source: &'tree str,
    keyword: &str,
) -> Result<Option<Node<'tree>>> {
    let Some(arguments) = call_node.child_by_field_name("arguments") else {
        return Ok(None);
    };
    for argument in call_arguments(arguments, source)? {
        if argument.keyword == Some(keyword) {
            return Ok(Some(argument.value));
        }
    }
    Ok(None)
}

fn qualified_name_resolves_to(
    name: &QualifiedName,
    module_type_data: &ModuleTypeData,
    targets: &[&str],
) -> bool {
    let raw = name.as_string();
    if targets.contains(&raw.as_str()) {
        return true;
    }
    expand_qualified_name(name, &module_type_data.imports)
        .is_some_and(|expanded| targets.contains(&expanded.as_str()))
}

fn qualified_text_resolves_to(
    target: &str,
    module_type_data: &ModuleTypeData,
    expected: &str,
) -> bool {
    let target = target.trim();
    if target == expected {
        return true;
    }
    let Some(name) = parse_qualified_name(target) else {
        return false;
    };
    expand_qualified_name(&name, &module_type_data.imports)
        .is_some_and(|expanded| expanded == expected)
}

fn template_profile_metadata(arg: &TypeExpr) -> Option<String> {
    let TypeExpr::StringLiteral(value) = arg else {
        return None;
    };
    value
        .strip_prefix("profile:")
        .or_else(|| value.strip_prefix("profile="))
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
        .map(str::to_string)
}

fn resolve_literal_type_info(args: &[TypeExpr]) -> ResolvedTypeInfo {
    let mut resolved = ResolvedTypeInfo::default();
    for arg in args {
        match arg {
            TypeExpr::StringLiteral(_) => {
                push_value_type(&mut resolved.value_types, CallableValueType::String)
            }
            TypeExpr::NoneLiteral => resolved.accepts_none = true,
            TypeExpr::Name(name) => {
                let literal = name.as_string();
                if matches!(literal.as_str(), "True" | "False") {
                    push_value_type(&mut resolved.value_types, CallableValueType::Bool);
                }
            }
            TypeExpr::Unknown(value) => {
                if is_float_literal(value) {
                    push_value_type(&mut resolved.value_types, CallableValueType::Float);
                } else if is_int_literal(value) {
                    push_value_type(&mut resolved.value_types, CallableValueType::Int);
                }
            }
            TypeExpr::Generic { .. } | TypeExpr::Union(_) => {}
        }
    }
    resolved
}

fn merge_resolved_type_info(target: &mut ResolvedTypeInfo, other: ResolvedTypeInfo) {
    if target.template_language.is_none() {
        target.template_language = other.template_language;
    }
    if target.template_profile.is_none() {
        target.template_profile = other.template_profile;
    }
    for value_type in other.value_types {
        push_value_type(&mut target.value_types, value_type);
    }
    target.accepts_none |= other.accepts_none;
}

fn mark_signature_type_annotation_module(signature: &mut CallableSignature, module_name: &str) {
    for parameter in &mut signature.parameters {
        if parameter.type_annotation.is_some() && parameter.type_annotation_module.is_none() {
            parameter.type_annotation_module = Some(module_name.to_string());
        }
    }
}

fn resolve_import_module_name(
    module_name: &str,
    current_module_name: Option<&str>,
    current_module_is_package: bool,
) -> Option<String> {
    if !module_name.starts_with('.') {
        return Some(module_name.to_string());
    }

    let current_module_name = current_module_name?;
    let leading_dots = module_name.bytes().take_while(|byte| *byte == b'.').count();
    let remainder = module_name[leading_dots..]
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    let mut package_segments = current_module_name
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if !current_module_is_package {
        package_segments.pop()?;
        if package_segments.is_empty() {
            return None;
        }
    }

    let levels_up = leading_dots.saturating_sub(1);
    if levels_up > package_segments.len() {
        return None;
    }
    package_segments.truncate(package_segments.len() - levels_up);

    if package_segments.is_empty() && remainder.is_empty() {
        return None;
    }

    package_segments.extend(remainder);
    Some(package_segments.join("."))
}

fn imported_module_paths_for(import_path: &str) -> Vec<String> {
    let segments = import_path
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return Vec::new();
    }

    let mut paths = Vec::with_capacity(segments.len());
    for index in 1..=segments.len() {
        paths.push(segments[..index].join("."));
    }
    paths
}

fn root_identifier_text(callee: &str) -> &str {
    callee.split('.').next().unwrap_or(callee)
}

fn callable_matches_import_target(callee: &str, import_target: &str) -> bool {
    callee == import_target || callee.starts_with(&format!("{import_target}."))
}

fn import_matches_resolution_filter(
    alias: &str,
    import_path: &str,
    filter: Option<&HashSet<String>>,
) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let alias_root = root_identifier_text(alias);
    let import_root = root_identifier_text(import_path);

    if alias_root != import_root {
        return filter.contains(alias_root);
    }

    filter.contains(alias_root)
}

fn module_path_matches_resolution_filter(
    import_path: &str,
    filter: Option<&HashSet<String>>,
) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    filter.contains(root_identifier_text(import_path))
}

fn scoped_import_matches_resolution_filter(
    scoped_imports: &[ScopedImportBinding],
    import_path: &str,
    filter: Option<&HashSet<String>>,
) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    scoped_imports.iter().any(|binding| {
        binding.import_target == import_path && filter.contains(root_identifier_text(&binding.name))
    })
}

fn is_template_string_node(node: Node, source: &str) -> Result<bool> {
    if node.kind() != "string" {
        return Ok(false);
    }
    let start_text = string_start_node(node)?.utf8_text(source.as_bytes())?;
    Ok(is_template_string_start(start_text))
}

fn node_is_template_or_template_concat(node: Node, source: &str) -> Result<bool> {
    if node.kind() == "string" {
        return is_template_string_node(node, source);
    }
    let mut strings = Vec::new();
    collect_concat_strings(node, source, &mut strings)?;
    Ok(strings.len() > 1 && all_template_string_nodes(&strings, source)?)
}

fn all_template_string_nodes(nodes: &[Node<'_>], source: &str) -> Result<bool> {
    nodes
        .iter()
        .map(|node| is_template_string_node(*node, source))
        .try_fold(true, |all_template, is_template| {
            is_template.map(|is_template| all_template && is_template)
        })
}

fn collect_concat_strings<'tree>(
    node: Node<'tree>,
    source: &str,
    strings: &mut Vec<Node<'tree>>,
) -> Result<bool> {
    match node.kind() {
        "string" => {
            strings.push(node);
            Ok(true)
        }
        "concatenated_string" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if !collect_concat_strings(child, source, strings)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        "binary_operator" if is_plus_binary_operator(node, source)? => {
            let Some(left) = node.child_by_field_name("left") else {
                return Ok(false);
            };
            let Some(right) = node.child_by_field_name("right") else {
                return Ok(false);
            };
            Ok(collect_concat_strings(left, source, strings)?
                && collect_concat_strings(right, source, strings)?)
        }
        _ => Ok(false),
    }
}

fn is_plus_binary_operator(node: Node, source: &str) -> Result<bool> {
    if node.kind() != "binary_operator" {
        return Ok(false);
    }
    let Some(operator) = node.child_by_field_name("operator") else {
        return Ok(false);
    };
    Ok(operator.utf8_text(source.as_bytes())? == "+")
}

fn string_start_node(node: Node) -> Result<Node> {
    node.child_by_field_name("string_start")
        .or_else(|| node.child(0))
        .ok_or_else(|| anyhow::anyhow!("No string_start node"))
}

fn string_end_node(node: Node) -> Result<Node> {
    let last_child_index = u32::try_from(node.child_count().saturating_sub(1))?;
    node.child_by_field_name("string_end")
        .or_else(|| node.child(last_child_index))
        .ok_or_else(|| anyhow::anyhow!("No string_end node"))
}

fn string_prefix(start_text: &str) -> Option<&str> {
    start_text
        .find(['"', '\''])
        .map(|quote_index| &start_text[..quote_index])
}

fn is_template_string_start(start_text: &str) -> bool {
    let Some(prefix) = string_prefix(start_text) else {
        return false;
    };
    let mut saw_template = false;
    let mut saw_raw = false;
    for byte in prefix.bytes() {
        match byte.to_ascii_lowercase() {
            b't' if !saw_template => saw_template = true,
            b'r' if !saw_raw => saw_raw = true,
            _ => return false,
        }
    }
    saw_template
}

fn call_function_for_string_node(node: Node) -> Option<Node> {
    let parent = node.parent()?;
    let argument_list = match parent.kind() {
        "argument_list" => parent,
        "keyword_argument" => parent.parent()?,
        _ => return None,
    };
    if argument_list.kind() != "argument_list" {
        return None;
    }
    argument_list
        .parent()
        .and_then(|call| call.child_by_field_name("function"))
}

fn assignment_for_string_node(node: Node) -> Option<(Node, Option<Node>)> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "assignment" {
            let right = parent.child_by_field_name("right")?;
            if !node_contains(right, node) {
                return None;
            }
            let left = parent.child_by_field_name("left")?;
            if left.kind() != "identifier" {
                return None;
            }
            return Some((left, parent.child_by_field_name("type")));
        }
        current = parent;
    }
    None
}

fn return_type_for_string_node(node: Node) -> Option<Node> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "return_statement" => return enclosing_function_return_type(parent),
            "function_definition" | "lambda" => return None,
            _ => current = parent,
        }
    }
    None
}

fn enclosing_function_return_type(node: Node) -> Option<Node> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "function_definition" {
            return parent.child_by_field_name("return_type");
        }
        current = parent;
    }
    None
}

fn is_module_level_statement(node: Node) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "module" => return true,
            "expression_statement" => current = parent,
            _ => return false,
        }
    }
    false
}

fn type_annotation_is_type_alias(
    type_node: Node,
    source: &str,
    module_type_data: &ModuleTypeData,
) -> Result<bool> {
    let TypeExpr::Name(name) = parse_type_expr(type_node.utf8_text(source.as_bytes())?) else {
        return Ok(false);
    };
    let resolved =
        expand_qualified_name(&name, &module_type_data.imports).unwrap_or_else(|| name.as_string());
    Ok(matches!(
        resolved.as_str(),
        "TypeAlias" | "typing.TypeAlias" | "typing_extensions.TypeAlias"
    ))
}

fn push_root_identifier_for_node(
    roots: &mut HashSet<String>,
    node: Node,
    source: &str,
) -> Result<()> {
    if let Some(root) = root_identifier_node(node) {
        roots.insert(root.utf8_text(source.as_bytes())?.to_string());
    } else {
        push_root_identifier_from_text(roots, node.utf8_text(source.as_bytes())?);
    }
    Ok(())
}

fn push_root_identifier_from_text(roots: &mut HashSet<String>, text: &str) {
    let trimmed = text.trim_start();
    let mut chars = trimmed.char_indices();
    let Some((_, first)) = chars.next() else {
        return;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return;
    }
    let end = chars
        .find_map(|(index, ch)| (!(ch == '_' || ch.is_ascii_alphanumeric())).then_some(index))
        .unwrap_or(trimmed.len());
    roots.insert(trimmed[..end].to_string());
}

fn push_identifier_roots_from_text(roots: &mut HashSet<String>, text: &str) {
    let mut start = None;
    for (index, ch) in text.char_indices() {
        match start {
            None if ch == '_' || ch.is_ascii_alphabetic() => start = Some(index),
            Some(begin) if !(ch == '_' || ch.is_ascii_alphanumeric()) => {
                roots.insert(text[begin..index].to_string());
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        roots.insert(text[begin..].to_string());
    }
}

fn push_component_roots_from_template_content(roots: &mut HashSet<String>, content: &str) {
    let mut remaining = content;
    while let Some(tag_start) = remaining.find('<') {
        remaining = &remaining[tag_start + 1..];
        if let Some(stripped) = remaining.strip_prefix('/') {
            remaining = stripped;
        }
        let Some(first) = remaining.chars().next() else {
            return;
        };
        if !(first == '_' || first.is_ascii_uppercase()) {
            continue;
        }
        let end = remaining
            .char_indices()
            .skip(1)
            .find_map(|(index, ch)| (!(ch == '_' || ch.is_ascii_alphanumeric())).then_some(index))
            .unwrap_or(remaining.len());
        roots.insert(remaining[..end].to_string());
    }
}

impl TemplateStringParser {
    fn python_search_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();

        if let Some(root) = self.search_root.as_deref() {
            push_search_root(&mut roots, root.to_path_buf());
        }
        if let Some(paths) = env::var_os("PYTHONPATH") {
            for path in env::split_paths(&paths) {
                push_search_root(&mut roots, path);
            }
        }
        for root in ancestor_virtualenv_search_roots(self.search_root.as_deref()) {
            push_search_root(&mut roots, root);
        }
        for root in environment_python_search_roots() {
            push_search_root(&mut roots, root);
        }
        if let Some(runtime_roots) = self.runtime_python_search_roots.as_deref() {
            for root in runtime_roots {
                push_search_root(&mut roots, root.to_path_buf());
            }
        }

        roots
    }
}

fn call_arguments<'a>(argument_list: Node<'a>, source: &'a str) -> Result<Vec<CallArgument<'a>>> {
    let mut arguments = Vec::new();
    let mut cursor = argument_list.walk();
    let mut position = 0;
    let mut infer_positional = true;

    for child in argument_list.children(&mut cursor) {
        if !child.is_named() || child.kind() == "comment" {
            continue;
        }

        match child.kind() {
            "keyword_argument" => {
                if let Some(value) = child.child_by_field_name("value") {
                    let keyword = child
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source.as_bytes()).ok());
                    arguments.push(CallArgument {
                        position,
                        keyword,
                        value,
                    });
                }
            }
            "list_splat" | "dictionary_splat" => {
                infer_positional = false;
            }
            _ if infer_positional => {
                arguments.push(CallArgument {
                    position,
                    keyword: None,
                    value: child,
                });
                position += 1;
            }
            _ => {}
        }
    }

    Ok(arguments)
}

fn resolve_callable_parameter<'a>(
    signatures: &'a [CallableParameter],
    position: usize,
    keyword: Option<&str>,
) -> Option<&'a CallableParameter> {
    if let Some(keyword) = keyword {
        return signatures
            .iter()
            .find(|parameter| parameter.name == keyword && parameter.allows_keyword);
    }

    signatures
        .iter()
        .find(|parameter| parameter.position == position && !parameter.keyword_only)
}

fn push_value_type(types: &mut Vec<CallableValueType>, value_type: CallableValueType) {
    if !types.contains(&value_type) {
        types.push(value_type);
    }
}

fn is_int_literal(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }

    let digits = if let Some(rest) = value.strip_prefix(['+', '-']) {
        rest
    } else {
        value
    };

    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
}

fn is_float_literal(value: &str) -> bool {
    value.contains('.') && value.parse::<f64>().is_ok()
}

fn collect_variable_assignments(
    tree: &Tree,
    source: &str,
    scope_directives: &[ScopeDirective],
) -> Result<Vec<VariableAssignment>> {
    let query_str = r#"
    (assignment
        left: (identifier) @var_name)

    (delete_statement
        (_) @delete_target)
    "#;

    let query = Query::new(&tree_sitter_python::LANGUAGE.into(), query_str)
        .context("Failed to create assignment query")?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    let mut assignments = Vec::new();

    while let Some(match_) = matches.next() {
        for capture in match_.captures {
            match query.capture_names()[capture.index as usize] {
                "var_name" => {
                    if let Some(key) = assignment_key_for_node_with_directives(
                        capture.node,
                        source,
                        scope_directives,
                    ) {
                        assignments.push(VariableAssignment { key });
                    }
                }
                "delete_target" => {
                    collect_target_assignment_entries(
                        capture.node,
                        source,
                        scope_directives,
                        &mut assignments,
                    );
                }
                _ => {}
            }
        }
    }

    assignments.sort_by_key(|assignment| assignment.key.assignment_start);
    Ok(assignments)
}

fn collect_target_assignment_entries(
    node: Node,
    source: &str,
    scope_directives: &[ScopeDirective],
    assignments: &mut Vec<VariableAssignment>,
) {
    match node.kind() {
        "identifier" | "keyword_identifier" => {
            if let Some(key) =
                assignment_key_for_node_with_directives(node, source, scope_directives)
            {
                assignments.push(VariableAssignment { key });
            }
        }
        "tuple"
        | "list"
        | "expression_list"
        | "parenthesized_expression"
        | "tuple_pattern"
        | "list_pattern"
        | "pattern_list"
        | "list_splat_pattern"
        | "dictionary_splat_pattern" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_target_assignment_entries(child, source, scope_directives, assignments);
            }
        }
        _ => {}
    }
}

fn collect_scope_directives(tree: &Tree, source: &str) -> Result<Vec<ScopeDirective>> {
    let query_str = r#"
    (global_statement
        (identifier) @directive_name)

    (nonlocal_statement
        (identifier) @directive_name)
    "#;

    let query = Query::new(&tree_sitter_python::LANGUAGE.into(), query_str)
        .context("Failed to create scope directive query")?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    let mut directives = Vec::new();

    while let Some(match_) = matches.next() {
        for capture in match_.captures {
            let kind = match query.capture_names()[capture.index as usize] {
                "directive_name" => match capture.node.parent().map(|node| node.kind()) {
                    Some("global_statement") => ScopeDirectiveKind::Global,
                    Some("nonlocal_statement") => ScopeDirectiveKind::Nonlocal,
                    _ => continue,
                },
                _ => continue,
            };
            let scope = enclosing_scope(capture.node);
            if !matches!(scope.kind, ScopeKind::FunctionLike) {
                continue;
            }
            directives.push(ScopeDirective {
                scope,
                name: capture.node.utf8_text(source.as_bytes())?.to_string(),
                kind,
            });
        }
    }

    Ok(directives)
}

fn collect_name_bindings(tree: &Tree, source: &str) -> Result<Vec<NameBinding>> {
    let query_str = r#"
    (import_statement
        name: (dotted_name) @import_name)

    (import_statement
        (aliased_import
            name: (dotted_name) @import_name
            alias: (identifier) @import_alias))

    (import_from_statement
        module_name: (dotted_name)? @module_name
        name: (dotted_name) @import_name)

    (import_from_statement
        module_name: (dotted_name)? @module_name
        (aliased_import
            name: (dotted_name) @import_name
            alias: (identifier) @import_alias))

    (assignment
        left: (_) @binding_target)

    (for_statement
        left: (_) @binding_target)

    (named_expression
        name: (_) @binding_target)

    (with_item
        value: (as_pattern
            alias: (as_pattern_target) @binding_target))

    (function_definition
        name: (identifier) @definition_name)

    (class_definition
        name: (identifier) @definition_name)

    (type_alias_statement) @type_alias

    (lambda) @lambda
    "#;

    let query = Query::new(&tree_sitter_python::LANGUAGE.into(), query_str)
        .context("Failed to create binding query")?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    let mut bindings = Vec::new();

    while let Some(match_) = matches.next() {
        let mut module_name = None;
        let mut import_name = None;
        let mut import_name_node = None;
        let mut import_alias = None;
        let mut import_alias_node = None;
        let mut binding_target_node = None;
        let mut definition_name_node = None;
        let mut type_alias_node = None;
        let mut lambda_node = None;

        for capture in match_.captures {
            match query.capture_names()[capture.index as usize] {
                "module_name" => module_name = Some(capture.node.utf8_text(source.as_bytes())?),
                "import_name" => {
                    import_name = Some(capture.node.utf8_text(source.as_bytes())?);
                    import_name_node = Some(capture.node);
                }
                "import_alias" => {
                    import_alias = Some(capture.node.utf8_text(source.as_bytes())?);
                    import_alias_node = Some(capture.node);
                }
                "binding_target" => binding_target_node = Some(capture.node),
                "definition_name" => definition_name_node = Some(capture.node),
                "type_alias" => type_alias_node = Some(capture.node),
                "lambda" => lambda_node = Some(capture.node),
                _ => {}
            }
        }

        if let Some(node) = type_alias_node {
            let Some(name_node) = node.child_by_field_name("left") else {
                continue;
            };
            bindings.push(NameBinding {
                scope: enclosing_scope(name_node),
                name: name_node.utf8_text(source.as_bytes())?.to_string(),
                binding_start: name_node.start_byte(),
                kind: NameBindingKind::TypeAlias,
            });
            continue;
        }

        if let Some(node) = binding_target_node {
            collect_target_name_bindings(node, enclosing_scope(node), source, &mut bindings)?;
            continue;
        }

        if let Some(node) = definition_name_node {
            let scope = node
                .parent()
                .map(scope_containing_definition)
                .unwrap_or_else(|| enclosing_scope(node));
            bindings.push(NameBinding {
                scope,
                name: node.utf8_text(source.as_bytes())?.to_string(),
                binding_start: node.start_byte(),
                kind: NameBindingKind::Definition,
            });
            continue;
        }

        if let Some(lambda) = lambda_node {
            if let Some(parameters) = lambda.child_by_field_name("parameters") {
                collect_parameter_name_bindings(
                    parameters,
                    enclosing_scope(lambda),
                    source,
                    &mut bindings,
                )?;
            }
            continue;
        }

        if let Some(import_node) = import_name_node {
            let binding_name = import_alias.map(str::to_string).unwrap_or_else(|| {
                let import_name = import_name.unwrap_or_default();
                if module_name.is_some() {
                    import_name
                        .split('.')
                        .last()
                        .unwrap_or_default()
                        .to_string()
                } else {
                    import_name
                        .split('.')
                        .next()
                        .unwrap_or_default()
                        .to_string()
                }
            });
            let binding_node = import_alias_node.unwrap_or(import_node);
            bindings.push(NameBinding {
                scope: enclosing_scope(binding_node),
                name: binding_name,
                binding_start: binding_node.start_byte(),
                kind: NameBindingKind::Import,
            });
        }
    }

    let mut function_cursor = QueryCursor::new();
    let function_query = Query::new(
        &tree_sitter_python::LANGUAGE.into(),
        r#"
        (function_definition) @function
        "#,
    )?;
    let mut function_matches =
        function_cursor.matches(&function_query, tree.root_node(), source.as_bytes());
    while let Some(match_) = function_matches.next() {
        for capture in match_.captures {
            if function_query.capture_names()[capture.index as usize] != "function" {
                continue;
            }
            if let Some(parameters) = capture.node.child_by_field_name("parameters") {
                collect_parameter_name_bindings(
                    parameters,
                    enclosing_scope(capture.node),
                    source,
                    &mut bindings,
                )?;
            }
        }
    }

    bindings.sort_by_key(|binding| binding.binding_start);
    Ok(bindings)
}

fn type_annotation_references_local_type_alias(
    type_node: Node<'_>,
    source: &str,
    bindings: &[NameBinding],
    scope_directives: &[ScopeDirective],
) -> Result<bool> {
    let mut nodes = vec![type_node];
    while let Some(node) = nodes.pop() {
        if matches!(node.kind(), "identifier" | "keyword_identifier" | "type") {
            let name = node.utf8_text(source.as_bytes())?;
            for scope in filtered_scope_chain(node, name, scope_directives) {
                if matches!(scope.kind, ScopeKind::Module) {
                    continue;
                }
                if bindings.iter().rev().any(|binding| {
                    binding.kind == NameBindingKind::TypeAlias
                        && binding.name == name
                        && binding.scope.start == scope.start
                        && binding.scope.end == scope.end
                        && (scope_uses_function_like_binding_rules(scope.kind)
                            || binding.binding_start < node.start_byte())
                }) {
                    return Ok(true);
                }
            }
        }

        let mut cursor = node.walk();
        nodes.extend(node.children(&mut cursor));
    }

    Ok(false)
}

fn collect_parameter_name_bindings(
    params_node: Node,
    scope: ScopeKey,
    source: &str,
    bindings: &mut Vec<NameBinding>,
) -> Result<()> {
    let mut cursor = params_node.walk();
    for child in params_node.children(&mut cursor) {
        let Some(name_node) = (match child.kind() {
            "identifier" | "list_splat_pattern" | "dictionary_splat_pattern" => {
                parameter_pattern_name_node(child)
            }
            "typed_parameter" | "typed_default_parameter" | "default_parameter" => {
                parameter_pattern_name_node(child)
            }
            _ => None,
        }) else {
            continue;
        };

        bindings.push(NameBinding {
            scope,
            name: name_node.utf8_text(source.as_bytes())?.to_string(),
            binding_start: name_node.start_byte(),
            kind: NameBindingKind::Value,
        });
    }

    Ok(())
}

fn collect_target_name_bindings(
    node: Node,
    scope: ScopeKey,
    source: &str,
    bindings: &mut Vec<NameBinding>,
) -> Result<()> {
    match node.kind() {
        "identifier" | "keyword_identifier" => {
            bindings.push(NameBinding {
                scope,
                name: node.utf8_text(source.as_bytes())?.to_string(),
                binding_start: node.start_byte(),
                kind: NameBindingKind::Value,
            });
        }
        "tuple_pattern"
        | "list_pattern"
        | "pattern_list"
        | "list_splat_pattern"
        | "dictionary_splat_pattern"
        | "parenthesized_expression"
        | "as_pattern_target" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_target_name_bindings(child, scope, source, bindings)?;
            }
        }
        _ => {}
    }

    Ok(())
}

fn target_binds_name(node: Node, source: &str, name: &str) -> Result<bool> {
    match node.kind() {
        "identifier" | "keyword_identifier" => Ok(node.utf8_text(source.as_bytes())? == name),
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if target_binds_name(child, source, name)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

fn scope_containing_definition(definition_node: Node) -> ScopeKey {
    let mut current = definition_node.parent();
    while let Some(candidate) = current {
        if is_scope_node(candidate) {
            return ScopeKey {
                start: candidate.start_byte(),
                end: candidate.end_byte(),
                kind: scope_kind(candidate),
            };
        }
        current = candidate.parent();
    }

    ScopeKey {
        start: definition_node.start_byte(),
        end: definition_node.end_byte(),
        kind: scope_kind(definition_node),
    }
}

fn resolve_assignment_for_identifier(
    identifier: Node,
    source: &str,
    assignments: &[VariableAssignment],
    scope_directives: &[ScopeDirective],
) -> Result<Option<VariableAssignment>> {
    let name = identifier.utf8_text(source.as_bytes())?;
    let scope_chain = filtered_scope_chain(identifier, name, scope_directives);
    let use_position = identifier.start_byte();

    for scope in &scope_chain {
        let matches_scope = |assignment: &&VariableAssignment| {
            assignment.key.name == name
                && assignment.key.scope_start == scope.start
                && assignment.key.scope_end == scope.end
        };
        if let Some(assignment) = assignments.iter().rev().find(|assignment| {
            matches_scope(assignment) && assignment.key.assignment_start < use_position
        }) {
            return Ok(Some(assignment.clone()));
        }
        if scope_uses_function_like_binding_rules(scope.kind)
            && let Some(assignment) = assignments.iter().rev().find(matches_scope)
        {
            return Ok(Some(assignment.clone()));
        }
    }

    Ok(None)
}

fn resolve_name_binding_for_identifier(
    identifier: Node,
    source: &str,
    bindings: &[NameBinding],
    scope_directives: &[ScopeDirective],
) -> Result<Option<NameBindingKind>> {
    let name = identifier.utf8_text(source.as_bytes())?;
    let scope_chain = filtered_scope_chain(identifier, name, scope_directives);
    let use_position = identifier.start_byte();

    for scope in &scope_chain {
        if let Some(binding) = bindings.iter().rev().find(|binding| {
            binding.name == name
                && binding.scope.start == scope.start
                && binding.scope.end == scope.end
                && (scope_uses_function_like_binding_rules(scope.kind)
                    || binding.binding_start < use_position)
        }) {
            return Ok(Some(binding.kind));
        }
    }

    Ok(None)
}

fn resolve_import_target_for_identifier(
    identifier: Node,
    source: &str,
    scope_directives: &[ScopeDirective],
    bindings: &[ScopedImportBinding],
) -> Result<Option<String>> {
    let name = identifier.utf8_text(source.as_bytes())?;
    let scope_chain = filtered_scope_chain(identifier, name, scope_directives);
    let use_position = identifier.start_byte();

    for scope in &scope_chain {
        if let Some(binding) = bindings.iter().rev().find(|binding| {
            binding.name == name
                && binding.scope.start == scope.start
                && binding.scope.end == scope.end
                && (scope_uses_function_like_binding_rules(scope.kind)
                    || binding.binding_start < use_position)
        }) {
            return Ok(Some(binding.import_target.clone()));
        }
    }

    Ok(None)
}

fn assignment_key_for_node_with_directives(
    identifier: Node,
    source: &str,
    scope_directives: &[ScopeDirective],
) -> Option<AssignmentKey> {
    let name = identifier.utf8_text(source.as_bytes()).ok()?.to_string();
    let scope = filtered_scope_chain(identifier, &name, scope_directives)
        .into_iter()
        .next()
        .unwrap_or_else(|| enclosing_scope(identifier));
    Some(AssignmentKey {
        scope_start: scope.start,
        scope_end: scope.end,
        name,
        assignment_start: identifier.start_byte(),
    })
}

fn scope_chain(node: Node) -> Vec<ScopeKey> {
    let mut scopes = Vec::new();
    let mut current = Some(node);
    let mut seen_function_like_scope = false;

    while let Some(candidate) = current {
        match candidate.kind() {
            "function_definition" | "lambda" => {
                scopes.push(ScopeKey {
                    start: candidate.start_byte(),
                    end: candidate.end_byte(),
                    kind: ScopeKind::FunctionLike,
                });
                seen_function_like_scope = true;
            }
            "class_definition" if !seen_function_like_scope => {
                scopes.push(ScopeKey {
                    start: candidate.start_byte(),
                    end: candidate.end_byte(),
                    kind: ScopeKind::Class,
                });
            }
            "module" => {
                scopes.push(ScopeKey {
                    start: candidate.start_byte(),
                    end: candidate.end_byte(),
                    kind: ScopeKind::Module,
                });
            }
            _ => {}
        }
        current = candidate.parent();
    }

    if scopes.is_empty() {
        scopes.push(ScopeKey {
            start: node.start_byte(),
            end: node.end_byte(),
            kind: scope_kind(node),
        });
    }

    scopes
}

fn filtered_scope_chain(
    node: Node,
    name: &str,
    scope_directives: &[ScopeDirective],
) -> Vec<ScopeKey> {
    let mut scopes = scope_chain(node);

    loop {
        let Some((index, scope)) = scopes
            .iter()
            .copied()
            .enumerate()
            .find(|(_, scope)| matches!(scope.kind, ScopeKind::FunctionLike))
        else {
            return scopes;
        };

        let directive_kind = scope_directives.iter().find_map(|directive| {
            (directive.name == name
                && directive.scope.start == scope.start
                && directive.scope.end == scope.end)
                .then_some(directive.kind)
        });

        match directive_kind {
            Some(ScopeDirectiveKind::Global) => {
                return scopes
                    .into_iter()
                    .filter(|scope| matches!(scope.kind, ScopeKind::Module))
                    .collect();
            }
            Some(ScopeDirectiveKind::Nonlocal) => {
                scopes.remove(index);
            }
            None => return scopes,
        }
    }
}

fn scope_uses_function_like_binding_rules(kind: ScopeKind) -> bool {
    matches!(kind, ScopeKind::FunctionLike)
}

pub(crate) fn enclosing_scope(node: Node) -> ScopeKey {
    scope_chain(node).into_iter().next().unwrap_or(ScopeKey {
        start: node.start_byte(),
        end: node.end_byte(),
        kind: scope_kind(node),
    })
}

fn scope_kind(node: Node) -> ScopeKind {
    match node.kind() {
        "function_definition" | "lambda" => ScopeKind::FunctionLike,
        "class_definition" => ScopeKind::Class,
        _ => ScopeKind::Module,
    }
}

pub(crate) fn is_scope_node(node: Node) -> bool {
    matches!(
        node.kind(),
        "module" | "function_definition" | "class_definition" | "lambda"
    )
}

fn push_search_root(roots: &mut Vec<PathBuf>, path: PathBuf) {
    if !path.is_dir() {
        return;
    }
    if roots.iter().any(|existing| existing == &path) {
        return;
    }
    roots.push(path);
}

fn collect_format_specifier_expressions(
    format_specifier: Node<'_>,
    source: &str,
) -> Result<Vec<Expression>> {
    let mut expressions = Vec::new();
    let mut stack = vec![format_specifier];

    while let Some(node) = stack.pop() {
        if node.kind() == "format_expression" {
            if let Some(expression_node) = node.child_by_field_name("expression") {
                let start = expression_node.start_position();
                let end = expression_node.end_position();
                expressions.push(Expression {
                    content: expression_node.utf8_text(source.as_bytes())?.to_string(),
                    location: Location {
                        start_line: start.row + 1,
                        start_column: start.column + 1,
                        end_line: end.row + 1,
                        end_column: end.column + 1,
                    },
                });
            }
            continue;
        }

        let mut cursor = node.walk();
        let children = node.children(&mut cursor).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }

    Ok(expressions)
}

fn ancestor_virtualenv_search_roots(search_root: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let Some(search_root) = search_root else {
        return roots;
    };

    for ancestor in search_root.ancestors() {
        for venv_name in [".venv", "venv"] {
            let venv_root = ancestor.join(venv_name);
            for site_packages in site_packages_under(&venv_root) {
                push_search_root(&mut roots, site_packages);
            }
        }
    }

    roots
}

fn environment_python_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    for variable in ["VIRTUAL_ENV", "CONDA_PREFIX"] {
        if let Some(prefix) = env::var_os(variable) {
            for site_packages in site_packages_under(Path::new(&prefix)) {
                push_search_root(&mut roots, site_packages);
            }
        }
    }

    roots
}

fn site_packages_under(prefix: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    let windows_site_packages = prefix.join("Lib").join("site-packages");
    push_search_root(&mut roots, windows_site_packages);

    let lib_dir = prefix.join("lib");
    if let Ok(entries) = fs::read_dir(&lib_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_python_dir = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("python"));
            if !is_python_dir {
                continue;
            }

            push_search_root(&mut roots, path.join("site-packages"));
        }
    }

    roots
}

fn discover_runtime_python_search_roots() -> Vec<PathBuf> {
    const PYTHON_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);
    const PYTHON_DISCOVERY_SCRIPT: &str = r#"
import os
import site
import sys
import sysconfig

output_path = sys.argv[1]
seen = set()
paths = []

def add(path):
    if not path:
        return
    normalized = os.path.abspath(path)
    if os.path.isdir(normalized) and normalized not in seen:
        seen.add(normalized)
        paths.append(normalized)

for key in ("purelib", "platlib"):
    try:
        add(sysconfig.get_path(key))
    except Exception:
        pass

for scheme in ("posix_user", "nt_user", "osx_framework_user"):
    for key in ("purelib", "platlib"):
        try:
            add(sysconfig.get_path(key, scheme=scheme))
        except Exception:
            pass

getsitepackages = getattr(site, "getsitepackages", None)
if getsitepackages is not None:
    try:
        for path in getsitepackages():
            add(path)
    except Exception:
        pass

try:
    user_site = site.getusersitepackages()
except Exception:
    user_site = None
if isinstance(user_site, str):
    add(user_site)

with open(output_path, "w", encoding="utf-8") as f:
    for path in paths:
        f.write(path)
        f.write("\n")
"#;

    for (index, candidate) in ["python3", "python"].into_iter().enumerate() {
        let Some((output_dir, output_path)) = python_discovery_output_paths(index) else {
            continue;
        };
        let Ok(mut child) = Command::new(candidate)
            .args([
                "-I",
                "-S",
                "-c",
                PYTHON_DISCOVERY_SCRIPT,
                output_path.to_string_lossy().as_ref(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            let _ = fs::remove_dir_all(&output_dir);
            continue;
        };

        let Ok(Some(status)) = child.wait_timeout(PYTHON_DISCOVERY_TIMEOUT) else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_dir_all(&output_dir);
            continue;
        };

        if !status.success() {
            let _ = fs::remove_dir_all(&output_dir);
            continue;
        }

        let Ok(stdout) = fs::read_to_string(&output_path) else {
            let _ = fs::remove_dir_all(&output_dir);
            continue;
        };
        let _ = fs::remove_dir_all(&output_dir);

        let mut roots = Vec::new();
        for line in stdout.lines() {
            push_search_root(&mut roots, PathBuf::from(line));
        }
        if !roots.is_empty() {
            return roots;
        }
    }

    Vec::new()
}

fn python_discovery_output_paths(index: usize) -> Option<(PathBuf, PathBuf)> {
    let base = env::temp_dir();

    for attempt in 0..16 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = base.join(format!(
            "t-linter-python-search-roots-{}-{index}-{attempt}-{nanos}",
            std::process::id()
        ));
        match fs::create_dir(&dir) {
            Ok(()) => return Some((dir.clone(), dir.join("roots.txt"))),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }

    None
}

fn module_reference_is_shadowed(
    callee_node: Option<Node>,
    source: &str,
    assignments: &[VariableAssignment],
    scope_directives: &[ScopeDirective],
    name_bindings: &[NameBinding],
) -> Result<bool> {
    let Some(callee_node) = callee_node else {
        return Ok(false);
    };
    let Some(root_identifier) = root_identifier_node(callee_node) else {
        return Ok(false);
    };

    if resolve_assignment_for_identifier(root_identifier, source, assignments, scope_directives)?
        .is_some()
    {
        return Ok(true);
    }

    if comprehension_target_shadows_identifier(root_identifier, source)? {
        return Ok(true);
    }

    if except_alias_shadows_identifier(root_identifier, source)? {
        return Ok(true);
    }

    Ok(matches!(
        resolve_name_binding_for_identifier(
            root_identifier,
            source,
            name_bindings,
            scope_directives,
        )?,
        Some(NameBindingKind::Definition | NameBindingKind::TypeAlias | NameBindingKind::Value)
    ))
}

fn direct_callable_reference_is_shadowed(
    callee_node: Option<Node>,
    source: &str,
    assignments: &[VariableAssignment],
    scope_directives: &[ScopeDirective],
    name_bindings: &[NameBinding],
    has_local_callable_signature: bool,
) -> Result<bool> {
    let Some(callee_node) = callee_node else {
        return Ok(false);
    };
    let Some(identifier) = root_identifier_node(callee_node) else {
        return Ok(false);
    };

    if resolve_assignment_for_identifier(identifier, source, assignments, scope_directives)?
        .is_some()
    {
        return Ok(true);
    }

    if comprehension_target_shadows_identifier(identifier, source)? {
        return Ok(true);
    }

    if except_alias_shadows_identifier(identifier, source)? {
        return Ok(true);
    }

    Ok(
        match resolve_name_binding_for_identifier(
            identifier,
            source,
            name_bindings,
            scope_directives,
        )? {
            Some(NameBindingKind::Value) => true,
            Some(NameBindingKind::TypeAlias) => true,
            Some(NameBindingKind::Definition) => !has_local_callable_signature,
            _ => false,
        },
    )
}

fn root_identifier_node(node: Node) -> Option<Node> {
    let mut current = node;
    loop {
        match current.kind() {
            "identifier" => return Some(current),
            "attribute" => current = current.child_by_field_name("object")?,
            _ => return None,
        }
    }
}

fn is_comprehension_node(node: Node) -> bool {
    matches!(
        node.kind(),
        "list_comprehension"
            | "dictionary_comprehension"
            | "set_comprehension"
            | "generator_expression"
    )
}

fn node_contains(ancestor: Node, descendant: Node) -> bool {
    ancestor.start_byte() <= descendant.start_byte() && descendant.end_byte() <= ancestor.end_byte()
}

fn comprehension_target_shadows_identifier(identifier: Node, source: &str) -> Result<bool> {
    let name = identifier.utf8_text(source.as_bytes())?;
    let mut current = identifier.parent();

    while let Some(node) = current {
        if is_comprehension_node(node) {
            let mut clauses = Vec::new();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "for_in_clause" {
                    clauses.push(child);
                }
            }

            let containing_right_index = clauses.iter().position(|clause| {
                clause
                    .child_by_field_name("right")
                    .is_some_and(|right| node_contains(right, identifier))
            });

            for (index, clause) in clauses.into_iter().enumerate() {
                if containing_right_index.is_some_and(|limit| index >= limit) {
                    break;
                }
                let Some(left) = clause.child_by_field_name("left") else {
                    continue;
                };
                if target_binds_name(left, source, name)? {
                    return Ok(true);
                }
            }
        }
        current = node.parent();
    }

    Ok(false)
}

fn except_clause_alias_node(node: Node) -> Option<Node> {
    if let Some(alias) = node.child_by_field_name("alias") {
        return Some(alias);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "as_pattern" {
            return child.child_by_field_name("alias").or(Some(child));
        }
    }

    None
}

fn except_alias_shadows_identifier(identifier: Node, source: &str) -> Result<bool> {
    let name = identifier.utf8_text(source.as_bytes())?;
    let use_position = identifier.start_byte();
    let mut current = identifier.parent();

    while let Some(node) = current {
        if node.kind() == "except_clause"
            && let Some(alias) = except_clause_alias_node(node)
            && alias.start_byte() < use_position
            && target_binds_name(alias, source, name)?
        {
            return Ok(true);
        }
        current = node.parent();
    }

    Ok(false)
}

fn parameter_pattern_name_node(node: Node) -> Option<Node> {
    match node.kind() {
        "identifier" => Some(node),
        _ => {
            if let Some(name_node) = node.child_by_field_name("name")
                && let Some(found) = parameter_pattern_name_node(name_node)
            {
                return Some(found);
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if let Some(found) = parameter_pattern_name_node(child) {
                    return Some(found);
                }
            }

            None
        }
    }
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

fn is_package_init_path(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("__init__.py" | "__init__.pyi")
    )
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

fn should_resolve_imported_signatures(import_path: &str) -> bool {
    !matches!(
        import_path,
        "typing"
            | "typing_extensions"
            | "string.templatelib"
            | "templatelib"
            | "string.templatelib.Template"
            | "templatelib.Template"
            | "tdom"
            | "tdom.html"
            | "tdom.svg"
            | "tdom.processor"
            | "tdom.processor.html"
            | "tdom.processor.svg"
            | "typing.Annotated"
            | "typing_extensions.Annotated"
    )
}

fn tdom_template_processor_hint(target: &str) -> Option<TemplateHint> {
    match target {
        "tdom.html" | "tdom.processor.html" => Some(TemplateHint {
            language: "tdom".to_string(),
            profile: None,
        }),
        "tdom.svg" | "tdom.processor.svg" => Some(TemplateHint {
            language: "tdom".to_string(),
            profile: Some("svg".to_string()),
        }),
        _ => None,
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
    pub profile: Option<String>,
    pub string_start: String,
    pub string_end: String,
    pub location: Location,
    pub formatting_wrapper_location: Option<Location>,
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
    pub raw_text: String,
}

#[derive(Debug, Clone)]
pub struct InterpolationInfo {
    pub expression: String,
    pub debug_prefix: Option<String>,
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
                    if let Some(debug_prefix) = &part.debug_prefix {
                        segments.push(TemplateSegment::StaticText(debug_prefix.clone()));
                    }
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
        let preferred_quote = if !self.flags.is_triple && content.contains('\n') {
            '"'
        } else if self.string_start.contains('\'') {
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
        let escaped_content = escape_formatted_template_content(
            content,
            quote,
            use_triple,
            self.parts.iter().filter_map(|part| match part {
                TemplatePart::Interpolation(part) => Some(part.raw_source.as_str()),
                TemplatePart::Static(_) => None,
            }),
        );

        format!("{normalized_prefix}{delimiter}{escaped_content}{delimiter}")
    }

    pub fn formatting_location(&self, content: &str) -> &Location {
        if content.contains('\n') {
            self.formatting_wrapper_location
                .as_ref()
                .unwrap_or(&self.location)
        } else {
            &self.location
        }
    }

    fn token_position_to_content_offset(&self, position: &SourcePosition) -> usize {
        let mut offset = 0;
        let mut visible_token_index = 0;

        for part in &self.parts {
            match part {
                TemplatePart::Static(part) => {
                    let contributes_token = !part.text.is_empty();
                    if contributes_token && visible_token_index == position.token_index {
                        return offset + char_offset_to_byte_offset(&part.text, position.offset);
                    }
                    offset += part.text.len();
                    if contributes_token {
                        visible_token_index += 1;
                    }
                }
                TemplatePart::Interpolation(part) => {
                    if let Some(debug_prefix) = &part.debug_prefix {
                        if visible_token_index == position.token_index {
                            return offset
                                + char_offset_to_byte_offset(debug_prefix, position.offset);
                        }
                        offset += debug_prefix.len();
                        visible_token_index += 1;
                    }

                    if visible_token_index == position.token_index {
                        return offset + position.offset.min(2);
                    }
                    offset += 2;
                    visible_token_index += 1;
                }
            }
        }

        offset
    }

    pub(crate) fn map_content_range_to_document(
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
            &self.parts,
            self.flags.is_raw,
            actual_content,
            start_offset,
            template_start_line,
            template_start_col,
            prefix_len,
        );
        let (end_line, end_col) = map_template_position_to_document(
            &self.parts,
            self.flags.is_raw,
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

fn char_offset_to_byte_offset(text: &str, char_offset: usize) -> usize {
    text.char_indices()
        .nth(char_offset)
        .map_or(text.len(), |(offset, _)| offset)
}

fn formatting_wrapper_location(node: Node) -> Option<Location> {
    let parent = node.parent()?;
    if parent.kind() != "parenthesized_expression" || parent.named_child_count() != 1 {
        return None;
    }

    let start = parent.start_position();
    let end = parent.end_position();

    Some(Location {
        start_line: start.row + 1,
        start_column: start.column + 1,
        end_line: end.row + 1,
        end_column: end.column + 1,
    })
}

fn location_for_node(node: Node) -> Location {
    let start = node.start_position();
    let end = node.end_position();
    Location {
        start_line: start.row + 1,
        start_column: start.column + 1,
        end_line: end.row + 1,
        end_column: end.column + 1,
    }
}

fn push_static_part(
    content_parts: &mut Vec<String>,
    parts: &mut Vec<TemplatePart>,
    text: &str,
    raw_text: &str,
) {
    if text.is_empty() && raw_text.is_empty() {
        return;
    }

    let text = text.to_string();
    let raw_text = raw_text.to_string();
    content_parts.push(text.clone());
    parts.push(TemplatePart::Static(StaticTextSegment { text, raw_text }));
}

struct DecodedTemplateUnit {
    decoded: String,
    raw_len: usize,
}

fn unescape_template_text(text: &str, is_raw: bool) -> String {
    let mut processed_content = String::new();
    let mut chars = text.chars().peekable();

    while let Some(unit) = next_template_unit(&mut chars, is_raw) {
        processed_content.push_str(&unit.decoded);
    }

    processed_content
}

fn next_template_unit(
    chars: &mut Peekable<Chars<'_>>,
    is_raw: bool,
) -> Option<DecodedTemplateUnit> {
    let ch = chars.next()?;

    if !is_raw
        && ch == '\\'
        && let Some((decoded, extra_raw_len)) = decode_python_escape(chars)
    {
        return Some(DecodedTemplateUnit {
            decoded,
            raw_len: ch.len_utf8() + extra_raw_len,
        });
    }

    if ch == '{' && chars.peek() == Some(&'{') {
        chars.next();
        return Some(DecodedTemplateUnit {
            decoded: "{".to_string(),
            raw_len: 2,
        });
    }

    if ch == '}' && chars.peek() == Some(&'}') {
        chars.next();
        return Some(DecodedTemplateUnit {
            decoded: "}".to_string(),
            raw_len: 2,
        });
    }

    Some(DecodedTemplateUnit {
        decoded: ch.to_string(),
        raw_len: ch.len_utf8(),
    })
}

fn decode_python_escape(chars: &mut Peekable<Chars<'_>>) -> Option<(String, usize)> {
    let mut probe = chars.clone();
    let next = probe.next()?;

    let simple_escape = match next {
        '\\' => Some('\\'),
        '\'' => Some('\''),
        '"' => Some('"'),
        'a' => Some('\u{0007}'),
        'b' => Some('\u{0008}'),
        'f' => Some('\u{000C}'),
        'n' => Some('\n'),
        'r' => Some('\r'),
        't' => Some('\t'),
        'v' => Some('\u{000B}'),
        _ => None,
    };
    if let Some(ch) = simple_escape {
        advance_chars(chars, 1);
        return Some((ch.to_string(), 1));
    }

    match next {
        '\n' => {
            advance_chars(chars, 1);
            Some((String::new(), 1))
        }
        '\r' => {
            let mut consumed = 1;
            if probe.next() == Some('\n') {
                consumed += 1;
            }
            advance_chars(chars, consumed);
            Some((String::new(), consumed))
        }
        'x' => decode_fixed_width_escape(chars, 2, 16),
        'u' => decode_fixed_width_escape(chars, 4, 16),
        'U' => decode_fixed_width_escape(chars, 8, 16),
        'N' => decode_named_escape(chars),
        '0'..='7' => decode_octal_escape(chars),
        _ => None,
    }
}

fn decode_fixed_width_escape(
    chars: &mut Peekable<Chars<'_>>,
    digits: usize,
    radix: u32,
) -> Option<(String, usize)> {
    let mut probe = chars.clone();
    let prefix = probe.next()?;
    let mut raw = String::new();
    raw.push(prefix);

    for _ in 0..digits {
        let ch = probe.next()?;
        if !ch.is_digit(radix) {
            return None;
        }
        raw.push(ch);
    }

    let value = u32::from_str_radix(&raw[1..], radix).ok()?;
    let decoded = char::from_u32(value)?;
    advance_chars(chars, raw.len());
    Some((decoded.to_string(), raw.len()))
}

fn decode_octal_escape(chars: &mut Peekable<Chars<'_>>) -> Option<(String, usize)> {
    let mut probe = chars.clone();
    let first = probe.next()?;
    let mut digits = String::new();
    digits.push(first);

    while digits.len() < 3 {
        let Some(next) = probe.peek().copied() else {
            break;
        };
        if !matches!(next, '0'..='7') {
            break;
        }
        digits.push(probe.next().expect("peeked char should exist"));
    }

    let value = u32::from_str_radix(&digits, 8).ok()?;
    let decoded = char::from_u32(value)?;
    advance_chars(chars, digits.len());
    Some((decoded.to_string(), digits.len()))
}

fn decode_named_escape(chars: &mut Peekable<Chars<'_>>) -> Option<(String, usize)> {
    let mut probe = chars.clone();
    if probe.next()? != 'N' || probe.next()? != '{' {
        return None;
    }

    let mut name = String::new();
    let mut raw_len = 2;
    while let Some(ch) = probe.next() {
        raw_len += ch.len_utf8();
        if ch == '}' {
            let decoded = unicode_names2::character(&name)?;
            advance_chars(chars, raw_len);
            return Some((decoded.to_string(), raw_len));
        }
        name.push(ch);
    }

    None
}

fn advance_chars(chars: &mut Peekable<Chars<'_>>, count: usize) {
    for _ in 0..count {
        chars.next();
    }
}

pub(crate) fn raw_static_prefix_len(
    segment: &StaticTextSegment,
    decoded_prefix_len: usize,
    is_raw: bool,
) -> usize {
    if segment.raw_text.is_empty() {
        return 0;
    }

    if segment.text.len() <= decoded_prefix_len {
        return segment.raw_text.len();
    }

    let mut raw_chars = segment.raw_text.chars().peekable();
    let mut decoded_bytes = 0;
    let mut consumed_raw_bytes = 0;

    while let Some(unit) = next_template_unit(&mut raw_chars, is_raw) {
        if !unit.decoded.is_empty() && decoded_bytes >= decoded_prefix_len {
            break;
        }

        consumed_raw_bytes += unit.raw_len;
        decoded_bytes += unit.decoded.len();
    }

    consumed_raw_bytes
}

fn map_template_position_to_document(
    parts: &[TemplatePart],
    is_raw: bool,
    actual_content: &str,
    position_in_template: usize,
    template_start_line: usize,
    template_start_col: usize,
    prefix_len: usize,
) -> (usize, usize) {
    let mut template_idx = 0;
    let mut actual_idx = 0;
    let actual_bytes = actual_content.as_bytes();
    let mut part_iter = parts.iter();

    while actual_idx < actual_bytes.len() {
        let Some(part) = part_iter.next() else {
            break;
        };

        match part {
            TemplatePart::Static(part) => {
                if part.text.is_empty() {
                    actual_idx = (actual_idx + part.raw_text.len()).min(actual_bytes.len());
                    continue;
                }

                let remaining_template = position_in_template.saturating_sub(template_idx);
                let consumed = remaining_template.min(part.text.len());
                actual_idx = (actual_idx + raw_static_prefix_len(part, consumed, is_raw))
                    .min(actual_bytes.len());
                template_idx += consumed;

                if consumed < part.text.len() {
                    break;
                }
            }
            TemplatePart::Interpolation(part) => {
                if let Some(prefix) = &part.debug_prefix {
                    let raw_prefix_start = debug_prefix_raw_start(part, prefix);
                    let raw_prefix_end =
                        (raw_prefix_start + prefix.len()).min(part.raw_source.len());
                    let prefix_end = template_idx + prefix.len();

                    if position_in_template <= template_idx {
                        actual_idx = (actual_idx + raw_prefix_start).min(actual_bytes.len());
                        break;
                    }

                    if position_in_template < prefix_end {
                        let prefix_offset = position_in_template - template_idx;
                        actual_idx =
                            (actual_idx + raw_prefix_start + prefix_offset.min(prefix.len()))
                                .min(actual_bytes.len());
                        break;
                    }

                    template_idx = prefix_end;
                    actual_idx = (actual_idx + raw_prefix_end).min(actual_bytes.len());

                    if position_in_template < template_idx + 2 {
                        break;
                    }

                    template_idx += 2;
                    actual_idx = (actual_idx
                        + part.raw_source.len().saturating_sub(raw_prefix_end))
                    .min(actual_bytes.len());
                    continue;
                }

                if template_idx >= position_in_template {
                    break;
                }

                if template_idx + 2 <= position_in_template {
                    template_idx += 2;
                    actual_idx = (actual_idx + part.raw_source.len()).min(actual_bytes.len());
                } else {
                    break;
                }
            }
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

fn debug_prefix_raw_start(part: &InterpolationInfo, prefix: &str) -> usize {
    part.raw_source
        .strip_prefix('{')
        .and_then(|raw| raw.find(prefix).map(|start| start + 1))
        .unwrap_or(1)
        .min(part.raw_source.len())
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
        content.matches(&delimiter).count() * 3 + trailing_quote_run_len(content, quote)
    } else {
        content.matches(quote).count()
    }
}

fn trailing_quote_run_len(content: &str, quote: char) -> usize {
    content.chars().rev().take_while(|&ch| ch == quote).count()
}

fn escape_python_literal_content(content: &str, quote: char, use_triple: bool) -> String {
    escape_python_literal_content_with_options(content, quote, use_triple, true)
}

fn escape_python_literal_content_with_options(
    content: &str,
    quote: char,
    use_triple: bool,
    escape_trailing_quotes: bool,
) -> String {
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
            '\'' if quote == '\'' && !use_triple => escaped.push_str("\\'"),
            '"' if quote == '"' && !use_triple => escaped.push_str("\\\""),
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
        let trailing_quotes = escape_trailing_quotes
            .then(|| trailing_quote_run_len(content, quote))
            .unwrap_or(0);
        if trailing_quotes > 0 {
            let split_at = escaped.len() - trailing_quotes;
            let mut escaped = escaped[..split_at].replace(&delimiter, escaped_delimiter);
            let replacement =
                std::iter::repeat_n(format!("\\{quote}"), trailing_quotes).collect::<String>();
            escaped.push_str(&replacement);
            escaped
        } else {
            escaped.replace(&delimiter, escaped_delimiter)
        }
    } else {
        escaped
    }
}

fn escape_formatted_template_content<'a>(
    content: &str,
    quote: char,
    use_triple: bool,
    raw_interpolations: impl Iterator<Item = &'a str>,
) -> String {
    let mut escaped = String::with_capacity(content.len());
    let mut cursor = 0;

    for raw_source in raw_interpolations {
        let Some(relative_start) = content[cursor..].find(raw_source) else {
            return escape_python_literal_content(content, quote, use_triple);
        };
        let start = cursor + relative_start;
        let static_segment =
            unescape_quote_before_raw_interpolation(&content[cursor..start], quote, use_triple);
        escaped.push_str(&escape_python_literal_content_with_options(
            &static_segment,
            quote,
            use_triple,
            false,
        ));
        escaped.push_str(raw_source);
        cursor = start + raw_source.len();
    }

    escaped.push_str(&escape_python_literal_content(
        &content[cursor..],
        quote,
        use_triple,
    ));
    escaped
}

fn unescape_quote_before_raw_interpolation(segment: &str, quote: char, use_triple: bool) -> String {
    if !use_triple || !segment.ends_with(quote) {
        return segment.to_string();
    }

    let Some(quote_start) = segment.len().checked_sub(quote.len_utf8()) else {
        return segment.to_string();
    };
    if quote_start == 0 || !segment[..quote_start].ends_with('\\') {
        return segment.to_string();
    }

    let slash_start = quote_start - 1;
    let mut normalized = String::with_capacity(segment.len() - 1);
    normalized.push_str(&segment[..slash_start]);
    normalized.push(quote);
    normalized
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

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn project_site_packages(dir: &Path) -> PathBuf {
        // Tests synthesize a virtualenv-style layout; production code accepts any `python*`
        // segment under `.venv/lib`, so the version string here is just fixture data.
        dir.join(".venv")
            .join("lib")
            .join("python3.12")
            .join("site-packages")
    }

    #[test]
    fn test_python_search_roots_do_not_add_process_cwd_implicitly() {
        let dir = parser_test_dir("search-roots-no-cwd");
        let mut parser = TemplateStringParser::new().unwrap();
        parser.search_root = Some(dir.clone());
        parser.runtime_python_search_roots = Some(Vec::new());

        let roots = parser.python_search_roots();
        let cwd = std::env::current_dir().unwrap();
        let cwd_is_explicit = std::env::var_os("PYTHONPATH")
            .map(|paths| std::env::split_paths(&paths).any(|path| path == cwd))
            .unwrap_or(false)
            || environment_python_search_roots()
                .into_iter()
                .any(|path| path == cwd);

        let _ = fs::remove_dir_all(dir);

        assert_eq!(roots.iter().any(|path| path == &cwd), cwd_is_explicit);
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
    fn test_nested_format_spec_expressions_are_tracked() {
        let source = r#"price_str = t"Price: {price:{width}.{precision}f}""#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(
            templates[0]
                .expressions
                .iter()
                .map(|expression| expression.content.as_str())
                .collect::<Vec<_>>(),
            vec!["price", "width", "precision"]
        );
        let TemplatePart::Interpolation(interpolation) = &templates[0].parts[1] else {
            panic!("expected interpolation part");
        };
        assert_eq!(interpolation.format_spec, "{width}.{precision}f");
    }

    #[test]
    fn test_debug_interpolation_adds_static_prefix_and_repr_conversion() {
        let source = r#"payload = t"{value = }""#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].content, "value = {}");
        let input = templates[0].to_template_input();
        assert!(matches!(
            input.segments.first(),
            Some(TemplateSegment::StaticText(text)) if text == "value = "
        ));
        let interpolation = input.interpolation(0).expect("expected interpolation");
        assert_eq!(interpolation.expression, "value");
        assert_eq!(interpolation.conversion.as_deref(), Some("r"));
        assert_eq!(interpolation.raw_source.as_deref(), Some("{value = }"));
    }

    #[test]
    fn test_debug_interpolation_maps_backend_positions_after_prefix() {
        let source = r#"payload = t"{value = } tail""#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();
        let template = &templates[0];

        assert_eq!(
            template.token_position_to_content_offset(&SourcePosition {
                token_index: 2,
                offset: 0,
            }),
            "value = {}".len()
        );

        let tail_location = template.backend_span_to_location(&SourceSpan {
            start: SourcePosition {
                token_index: 2,
                offset: 0,
            },
            end: SourcePosition {
                token_index: 2,
                offset: 5,
            },
        });
        assert_eq!(
            tail_location.start_column,
            source.find(" tail").unwrap() + 1
        );
        assert_eq!(tail_location.end_column, source.len());
    }

    #[test]
    fn test_raw_template_string() {
        for prefix in ["tr", "rt", "Rt", "tR", "RT", "TR"] {
            let source = format!(r#"path = {prefix}"Path: {{path}}\n""#);

            let mut parser = TemplateStringParser::new().unwrap();
            let templates = parser.find_template_strings(&source).unwrap();

            assert_eq!(templates.len(), 1, "{prefix}");
            assert!(templates[0].flags.is_raw, "{prefix}");
            assert!(templates[0].flags.is_template, "{prefix}");
        }
    }

    #[test]
    fn test_invalid_template_prefix_combinations_are_ignored() {
        for prefix in ["tf", "ft", "tb", "bt", "ut", "tu"] {
            let source = format!(r#"path = {prefix}"Path: {{path}}""#);

            let mut parser = TemplateStringParser::new().unwrap();
            let templates = parser.find_template_strings(&source).unwrap();

            assert!(templates.is_empty(), "{prefix}");
        }
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
    fn test_raw_static_prefix_len_handles_known_and_unknown_escapes() {
        let unknown_escape = StaticTextSegment {
            text: r#"\q<span>"#.to_string(),
            raw_text: r#"\q<span>"#.to_string(),
        };
        assert_eq!(raw_static_prefix_len(&unknown_escape, 1, false), 1);
        assert_eq!(raw_static_prefix_len(&unknown_escape, 2, false), 2);

        let known_escape = StaticTextSegment {
            text: "\n<span>".to_string(),
            raw_text: r#"\n<span>"#.to_string(),
        };
        assert_eq!(raw_static_prefix_len(&known_escape, 1, false), 2);
        assert_eq!(raw_static_prefix_len(&known_escape, 2, false), 3);

        let unicode_escape = StaticTextSegment {
            text: "A<span>".to_string(),
            raw_text: r#"\u0041<span>"#.to_string(),
        };
        assert_eq!(raw_static_prefix_len(&unicode_escape, 1, false), 6);

        let named_escape = StaticTextSegment {
            text: "A<span>".to_string(),
            raw_text: r#"\N{LATIN CAPITAL LETTER A}<span>"#.to_string(),
        };
        assert_eq!(raw_static_prefix_len(&named_escape, 1, false), 26);

        let continued_line = StaticTextSegment {
            text: "<span>".to_string(),
            raw_text: "\\\n<span>".to_string(),
        };
        assert_eq!(raw_static_prefix_len(&continued_line, 0, false), 2);
        assert_eq!(raw_static_prefix_len(&continued_line, 1, false), 3);
    }

    #[test]
    fn test_raw_static_prefix_len_boundary_cases() {
        let cases = [
            (
                StaticTextSegment {
                    text: "AB".to_string(),
                    raw_text: "A\\\nB".to_string(),
                },
                vec![(0, 0), (1, 3), (2, 4)],
            ),
            (
                StaticTextSegment {
                    text: "<span>".to_string(),
                    raw_text: "\\\n<span>".to_string(),
                },
                vec![(0, 2), (1, 3), (6, 8)],
            ),
            (
                StaticTextSegment {
                    text: "AB".to_string(),
                    raw_text: "\\u0041\\\nB".to_string(),
                },
                vec![(0, 0), (1, 8), (2, 9)],
            ),
        ];

        for (segment, expectations) in cases {
            for (decoded_prefix_len, expected_raw_len) in expectations {
                assert_eq!(
                    raw_static_prefix_len(&segment, decoded_prefix_len, false),
                    expected_raw_len,
                    "segment {:?}, decoded_prefix_len {}",
                    segment.raw_text,
                    decoded_prefix_len
                );
            }
        }
    }

    #[test]
    fn test_map_template_position_to_document_consumes_zero_length_static_segments() {
        let parts = vec![
            TemplatePart::Interpolation(InterpolationInfo {
                expression: "value".to_string(),
                debug_prefix: None,
                conversion: None,
                format_spec: String::new(),
                raw_source: "{value}".to_string(),
                location: Location {
                    start_line: 1,
                    start_column: 1,
                    end_line: 1,
                    end_column: 8,
                },
                interpolation_index: 0,
            }),
            TemplatePart::Static(StaticTextSegment {
                text: "<span>".to_string(),
                raw_text: "\\\n<span>".to_string(),
            }),
        ];

        assert_eq!(
            map_template_position_to_document(&parts, false, "{value}\\\n<span>", 2, 0, 0, 0),
            (1, 0)
        );
    }

    #[test]
    fn test_map_template_position_to_document_boundary_cases() {
        let static_with_continuation = vec![TemplatePart::Static(StaticTextSegment {
            text: "AB".to_string(),
            raw_text: "A\\\nB".to_string(),
        })];
        assert_eq!(
            map_template_position_to_document(
                &static_with_continuation,
                false,
                "A\\\nB",
                0,
                0,
                0,
                0
            ),
            (0, 0)
        );
        assert_eq!(
            map_template_position_to_document(
                &static_with_continuation,
                false,
                "A\\\nB",
                1,
                0,
                0,
                0
            ),
            (1, 0)
        );
        assert_eq!(
            map_template_position_to_document(
                &static_with_continuation,
                false,
                "A\\\nB",
                2,
                0,
                0,
                0
            ),
            (1, 1)
        );

        let interpolation_then_continuation = vec![
            TemplatePart::Interpolation(InterpolationInfo {
                expression: "value".to_string(),
                debug_prefix: None,
                conversion: None,
                format_spec: String::new(),
                raw_source: "{value}".to_string(),
                location: Location {
                    start_line: 1,
                    start_column: 1,
                    end_line: 1,
                    end_column: 8,
                },
                interpolation_index: 0,
            }),
            TemplatePart::Static(StaticTextSegment {
                text: "<span>".to_string(),
                raw_text: "\\\n<span>".to_string(),
            }),
        ];
        assert_eq!(
            map_template_position_to_document(
                &interpolation_then_continuation,
                false,
                "{value}\\\n<span>",
                2,
                0,
                0,
                0
            ),
            (1, 0)
        );
        assert_eq!(
            map_template_position_to_document(
                &interpolation_then_continuation,
                false,
                "{value}\\\n<span>",
                3,
                0,
                0,
                0
            ),
            (1, 1)
        );
    }

    #[test]
    fn test_token_position_to_content_offset_skips_raw_only_static_parts() {
        let template = TemplateStringInfo {
            content: "{}{}".to_string(),
            raw_content: r#"t"{a}\
{b}""#
                .to_string(),
            variable_name: None,
            function_name: None,
            language: Some("html".to_string()),
            profile: None,
            string_start: "t\"".to_string(),
            string_end: "\"".to_string(),
            location: Location {
                start_line: 1,
                start_column: 1,
                end_line: 2,
                end_column: 5,
            },
            formatting_wrapper_location: None,
            expressions: vec![],
            parts: vec![
                TemplatePart::Interpolation(InterpolationInfo {
                    expression: "a".to_string(),
                    debug_prefix: None,
                    conversion: None,
                    format_spec: String::new(),
                    raw_source: "{a}".to_string(),
                    location: Location {
                        start_line: 1,
                        start_column: 3,
                        end_line: 1,
                        end_column: 6,
                    },
                    interpolation_index: 0,
                }),
                TemplatePart::Static(StaticTextSegment {
                    text: String::new(),
                    raw_text: "\\\n".to_string(),
                }),
                TemplatePart::Interpolation(InterpolationInfo {
                    expression: "b".to_string(),
                    debug_prefix: None,
                    conversion: None,
                    format_spec: String::new(),
                    raw_source: "{b}".to_string(),
                    location: Location {
                        start_line: 2,
                        start_column: 1,
                        end_line: 2,
                        end_column: 4,
                    },
                    interpolation_index: 1,
                }),
            ],
            flags: TemplateStringFlags::default(),
        };

        assert_eq!(
            template.token_position_to_content_offset(&SourcePosition {
                token_index: 1,
                offset: 0,
            }),
            2
        );
    }

    #[test]
    fn test_token_position_to_content_offset_treats_backend_offsets_as_chars() {
        let template = TemplateStringInfo {
            content: "説明: {}".to_string(),
            raw_content: "t\"説明: {value}\"".to_string(),
            variable_name: None,
            function_name: None,
            language: Some("yaml".to_string()),
            profile: None,
            string_start: "t\"".to_string(),
            string_end: "\"".to_string(),
            location: Location {
                start_line: 1,
                start_column: 1,
                end_line: 1,
                end_column: 18,
            },
            formatting_wrapper_location: None,
            expressions: vec![],
            parts: vec![
                TemplatePart::Static(StaticTextSegment {
                    text: "説明: ".to_string(),
                    raw_text: "説明: ".to_string(),
                }),
                TemplatePart::Interpolation(InterpolationInfo {
                    expression: "value".to_string(),
                    debug_prefix: None,
                    conversion: None,
                    format_spec: String::new(),
                    raw_source: "{value}".to_string(),
                    location: Location {
                        start_line: 1,
                        start_column: 8,
                        end_line: 1,
                        end_column: 15,
                    },
                    interpolation_index: 0,
                }),
            ],
            flags: TemplateStringFlags::default(),
        };

        assert_eq!(
            template.token_position_to_content_offset(&SourcePosition {
                token_index: 0,
                offset: 3,
            }),
            "説明:".len()
        );
        assert_eq!(
            template.token_position_to_content_offset(&SourcePosition {
                token_index: 0,
                offset: usize::MAX,
            }),
            "説明: ".len()
        );
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
    fn test_formatted_literal_keeps_plain_quotes_in_triple_quoted_strings() {
        let source = "payload = t\"\"\"placeholder\"\"\"";

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(
            templates[0]
                .formatted_literal(r#"<body><h1 style="color: #007acc">{heading}</h1></body>"#),
            r#"t"""<body><h1 style="color: #007acc">{heading}</h1></body>""""#
        );
    }

    #[test]
    fn test_formatted_literal_prefers_triple_double_quotes_when_promoting_multiline_output() {
        let source = "payload = t'placeholder'";

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(
            templates[0].formatted_literal("<div>\n  <span>{value}</span>\n</div>"),
            "t\"\"\"<div>\n  <span>{value}</span>\n</div>\"\"\""
        );
    }

    #[test]
    fn test_formatted_literal_avoids_triple_quote_boundary_collision() {
        let source = "payload = t\"\"\"placeholder\"\"\"";

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(
            templates[0]
                .formatted_literal("[project]\nname = \"{project_name}\"\nversion = \"{version}\""),
            "t'''[project]\nname = \"{project_name}\"\nversion = \"{version}\"'''"
        );
    }

    #[test]
    fn test_formatted_literal_escapes_trailing_quotes_individually() {
        let source = "payload = t\"\"\"placeholder\"\"\"";

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();
        let formatted = templates[0].formatted_literal("'''\"\"");

        assert_eq!(formatted, r#"t"""'''\"\"""""#);

        let reparsed_source = format!("payload = {formatted}");
        let mut reparsed = TemplateStringParser::new().unwrap();
        let reparsed_templates = reparsed.find_template_strings(&reparsed_source).unwrap();
        assert_eq!(reparsed_templates.len(), 1);
        assert_eq!(reparsed_templates[0].content, "'''\"\"");
    }

    #[test]
    fn test_formatted_literal_escapes_trailing_quotes_after_triple_delimiter_replacement() {
        let source = "payload = t'''placeholder'''";

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();
        let formatted = templates[0].formatted_literal(r#""""x"""x'''"#);

        assert_eq!(formatted, r#"t'''"""x"""x\'\'\''''"#);

        let reparsed_source = format!("payload = {formatted}");
        let mut reparsed = TemplateStringParser::new().unwrap();
        let reparsed_templates = reparsed.find_template_strings(&reparsed_source).unwrap();
        assert_eq!(reparsed_templates.len(), 1);
        assert_eq!(reparsed_templates[0].content, r#""""x"""x'''"#);
    }

    #[test]
    fn test_formatted_literal_preserves_interpolation_source_escapes() {
        let source = r#"payload = t'<a href="{url.replace("\\", "/")}">{d['k']}</a>'"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();
        let formatted =
            templates[0].formatted_literal(r#"<a href="{url.replace("\\", "/")}">{d['k']}</a>"#);

        assert_eq!(
            formatted,
            r#"t'<a href="{url.replace("\\", "/")}">{d['k']}</a>'"#
        );
        let reparsed_source = format!("payload = {formatted}");
        let mut reparsed = TemplateStringParser::new().unwrap();
        let reparsed_templates = reparsed.find_template_strings(&reparsed_source).unwrap();
        assert_eq!(
            reparsed_templates[0]
                .parts
                .iter()
                .filter_map(|part| match part {
                    TemplatePart::Interpolation(interpolation) =>
                        Some(interpolation.raw_source.as_str()),
                    TemplatePart::Static(_) => None,
                })
                .collect::<Vec<_>>(),
            vec![r#"{url.replace("\\", "/")}"#, "{d['k']}"]
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
        assert!(templates[0].formatting_wrapper_location.is_none());
    }

    #[test]
    fn test_parenthesized_annotated_template_string() {
        let source = r#"
from typing import Annotated
from templatelib import Template

html_content: Annotated[Template, "html"] = (
    t"<h1>{title}</h1>"
)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
        assert_eq!(templates[0].variable_name, Some("html_content".to_string()));
        assert!(templates[0].formatting_wrapper_location.is_some());
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
    fn test_type_alias_detection_inside_union_annotation() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

type html = Annotated[Template, "html"]

page: html | Renderable = t"<div><"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
    }

    #[test]
    fn test_typing_optional_parenthesized_union_alias_detection() {
        let source = r#"
import typing
from typing import Annotated
from string.templatelib import Template

type HtmlTemplate = Annotated[Template, "html"]

page: typing.Optional[(HtmlTemplate | Renderable)] = t"<div><"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
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
    fn test_annotated_template_profile_metadata_detection() {
        let source = r#"
config: Annotated[Template, "toml", "profile:1.0"] = t"title = {title}"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("toml".to_string()));
        assert_eq!(templates[0].profile, Some("1.0".to_string()));
    }

    #[test]
    fn test_function_annotation_profile_metadata_detection() {
        let source = r#"
def render_toml(template: Annotated[Template, "toml", "profile=1.0"]) -> None:
    pass

render_toml(t"title = {title}")
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("toml".to_string()));
        assert_eq!(templates[0].profile, Some("1.0".to_string()));
    }

    #[test]
    fn test_return_annotation_inference() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

def render_page() -> Annotated[Template, "html"]:
    return t"<main>{body}</main>"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].content, "<main>{}</main>");
        assert_eq!(templates[0].language, Some("html".to_string()));
    }

    #[test]
    fn test_templates_in_collection_and_concat_contexts_are_detected() {
        let source = r#"
items = [t"<li>{first}</li>"]
implicit = t"<span>" t"{label}</span>"
explicit = t"<b>" + t"{label}</b>"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();
        let contents = templates
            .iter()
            .map(|template| template.content.as_str())
            .collect::<Vec<_>>();

        assert!(contents.contains(&"<li>{}</li>"));
        assert!(contents.contains(&"<span>{}</span>"));
        assert!(contents.contains(&"<b>{}</b>"));
        assert_eq!(templates.len(), 3);

        let explicit = templates
            .iter()
            .find(|template| template.content == "<b>{}</b>")
            .expect("explicit concatenation template");
        let input = explicit.to_template_input();
        assert!(matches!(
            &input.segments[..],
            [
                TemplateSegment::StaticText(prefix),
                TemplateSegment::Interpolation(interpolation),
                TemplateSegment::StaticText(suffix),
            ] if prefix == "<b>" && interpolation.expression == "label" && suffix == "</b>"
        ));

        let suffix_location = explicit.backend_span_to_location(&SourceSpan {
            start: SourcePosition {
                token_index: 2,
                offset: 0,
            },
            end: SourcePosition {
                token_index: 2,
                offset: 4,
            },
        });
        let explicit_line = source
            .lines()
            .find(|line| line.contains("explicit"))
            .expect("explicit line");
        assert_eq!(
            suffix_location.start_column,
            explicit_line.find("</b>").expect("suffix") + 1
        );
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
    fn test_function_keyword_parameter_inference() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

def load_config(template: Annotated[Template, "yaml"]) -> None:
    return None

load_config(template=t"name: {name}")
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("yaml".to_string()));
        assert_eq!(templates[0].content, "name: {}");
    }

    #[test]
    fn test_function_keyword_only_parameter_inference() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

def load_config(*, template: Annotated[Template, "yaml"]) -> None:
    return None

load_config(template=t"name: {name}")
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_keyword_only_parameter_does_not_accept_positional_template_argument() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

def load_config(*, template: Annotated[Template, "yaml"]) -> None:
    return None

load_config(t"name: {name}")
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_positional_only_parameter_does_not_accept_keyword_template_argument() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

def load_config(template: Annotated[Template, "yaml"], /) -> None:
    return None

load_config(template=t"name: {name}")
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_tdom_language_is_inferred_from_direct_module_call() {
        let source = r#"
import tdom

page = tdom.html(t"<div>{name}</div>")
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("tdom".to_string()));
        assert_eq!(templates[0].profile, None);
    }

    #[test]
    fn test_tdom_language_is_inferred_from_direct_svg_call() {
        let source = r#"
import tdom

icon = tdom.svg(t"<clipPath id='mask'></clipPath>")
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("tdom".to_string()));
        assert_eq!(templates[0].profile, Some("svg".to_string()));
    }

    #[test]
    fn test_tdom_language_is_inferred_from_reexported_module_call() {
        let dir = parser_test_dir("tdom-reexport");
        write_file(&dir.join("bindings.py"), r#"from tdom import svg"#);
        write_file(
            &dir.join("api.py"),
            r#"from bindings import svg as render_svg"#,
        );
        let source = r#"from api import render_svg

icon = render_svg(t"<clipPath id='mask'></clipPath>")
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("main.py"))
            .unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("tdom".to_string()));
        assert_eq!(templates[0].profile, Some("svg".to_string()));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_installed_package_tdom_language_inference_works_for_processor_html() {
        let dir = parser_test_dir("tdom-installed-package");
        let site_packages = project_site_packages(&dir);
        write_file(&site_packages.join("tdom/__init__.py"), "");
        write_file(
            &site_packages.join("tdom/processor.py"),
            r#"def html(template):
    return template
"#,
        );
        let source = r#"from tdom.processor import html as render_html

page = render_html(t"<div>{name}</div>")
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("main.py"))
            .unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("tdom".to_string()));
        assert_eq!(templates[0].profile, None);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_installed_package_tdom_language_inference_works_for_processor_svg() {
        let dir = parser_test_dir("tdom-installed-package-svg");
        let site_packages = project_site_packages(&dir);
        write_file(&site_packages.join("tdom/__init__.py"), "");
        write_file(
            &site_packages.join("tdom/processor.py"),
            r#"def svg(template):
    return template
"#,
        );
        let source = r#"from tdom.processor import svg as render_svg

icon = render_svg(t"<clipPath id='mask'></clipPath>")
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("main.py"))
            .unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("tdom".to_string()));
        assert_eq!(templates[0].profile, Some("svg".to_string()));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_callable_signature_tracks_requires_positional() {
        let source = r#"
def NeedsPositional(value, /, *, title: str = "ok") -> None:
    return None

def Flexible(value="ok", /, *, title: str = "ok") -> None:
    return None

def VarArgs(*values: object, title: str = "ok") -> None:
    return None
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let _ = parser.find_template_strings(source).unwrap();
        let context = parser.module_context();

        assert!(
            context
                .callable_signatures
                .get("NeedsPositional")
                .unwrap()
                .requires_positional
        );
        assert!(
            !context
                .callable_signatures
                .get("Flexible")
                .unwrap()
                .requires_positional
        );
        assert!(
            context
                .callable_signatures
                .get("VarArgs")
                .unwrap()
                .requires_positional
        );
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
    fn test_function_scope_uses_latest_prior_assignment_for_language_hints() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

def run_sql(template: Annotated[Template, "sql"]) -> None:
    return None

def f():
    config = t"SELECT 1"
    run_sql(config)
    config = t"<div>plain</div>"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 2);
        assert_eq!(templates[0].content, "SELECT 1");
        assert_eq!(templates[0].language, Some("sql".to_string()));
        assert_eq!(templates[1].content, "<div>plain</div>");
        assert_eq!(templates[1].language, None);
    }

    #[test]
    fn test_reassigned_template_variables_keep_distinct_language_hints() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

def load_yaml(template: Annotated[Template, "yaml"]) -> None:
    return None

def load_toml(template: Annotated[Template, "toml"]) -> None:
    return None

config = t"name: {name}"
load_yaml(config)

config = t"title = {title}"
load_toml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 2);
        assert_eq!(templates[0].language, Some("yaml".to_string()));
        assert_eq!(templates[1].language, Some("toml".to_string()));
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
        write_file(
            &dir.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

class Loader:
    def __init__(self, template: Annotated[Template, "yaml"]) -> None:
        self.template = template
"#,
        );

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
    fn test_installed_package_stub_annotations_prefer_pyi_and_support_multiple_languages() {
        let dir = parser_test_dir("installed-package-stub");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.pyi"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_json(template: Annotated[Template, "json"]) -> object: ...
def render_yaml(template: Annotated[Template, "yaml"]) -> object: ...
def render_toml(template: Annotated[Template, "toml"]) -> object: ...
def render_sql(template: Annotated[Template, "sql"]) -> object: ...
"#,
        );
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_json(template: Annotated[Template, "html"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_json, render_yaml, render_toml, render_sql

json_payload = t'{"name": {name}}'
yaml_payload = t"name: {name}"
toml_payload = t"title = {title}"
sql_query = t"SELECT * FROM users WHERE id = {user_id}"

render_json(json_payload)
render_yaml(yaml_payload)
render_toml(toml_payload)
render_sql(sql_query)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 4);
        assert_eq!(templates[0].language, Some("json".to_string()));
        assert_eq!(templates[1].language, Some("yaml".to_string()));
        assert_eq!(templates[2].language, Some("toml".to_string()));
        assert_eq!(templates[3].language, Some("sql".to_string()));
    }

    #[test]
    fn test_installed_package_source_annotation_preserves_unknown_language_strings() {
        let dir = parser_test_dir("installed-package-unknown-language");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_template(template: Annotated[Template, "mydsl"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_template

config = t"entry {value}"
render_template(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("mydsl".to_string()));
    }

    #[test]
    fn test_installed_package_source_annotation_preserves_dotted_language_strings() {
        let dir = parser_test_dir("installed-package-dotted-language");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_template(template: Annotated[Template, "graphql.js"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_template

config = t"entry {value}"
render_template(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("graphql.js".to_string()));
    }

    #[test]
    fn test_installed_package_source_annotation_preserves_hyphenated_language_strings() {
        let dir = parser_test_dir("installed-package-hyphen-language");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_template(template: Annotated[Template, "t-html"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_template

config = t"entry {value}"
render_template(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("t-html".to_string()));
    }

    #[test]
    fn test_installed_package_module_qualified_import_annotation_propagates_to_template_variable() {
        let dir = parser_test_dir("installed-package-module-qualified");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api as api

config = t"name: {name}"
api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_installed_package_relative_reexport_annotation_propagates_to_template_variable() {
        let dir = parser_test_dir("installed-package-relative-reexport");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            "from .impl import render_yaml\n",
        );
        write_file(
            &site_packages.join("typed_api").join("impl.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_yaml

config = t"name: bad: {name}"
render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_unresolved_relative_import_does_not_infer_language() {
        let dir = parser_test_dir("unresolved-relative-import");
        let source = r#"
from .typed_api import render_yaml

config = t"name: bad: {name}"
render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_invalid_relative_import_in_installed_module_does_not_infer_language() {
        let dir = parser_test_dir("invalid-relative-import-in-installed-module");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            "from .fallback import render_yaml\n",
        );
        write_file(
            &dir.join("fallback.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_yaml

config = t"name: bad: {name}"
render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_installed_package_parent_relative_reexport_annotation_propagates_to_template_variable()
    {
        let dir = parser_test_dir("installed-package-parent-relative-reexport");
        let site_packages = project_site_packages(&dir);
        write_file(&site_packages.join("typed_api").join("__init__.py"), "");
        write_file(
            &site_packages
                .join("typed_api")
                .join("sub")
                .join("__init__.py"),
            "from ..impl import render_yaml\n",
        );
        write_file(
            &site_packages.join("typed_api").join("impl.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api.sub import render_yaml

config = t"name: bad: {name}"
render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_installed_package_is_found_via_ancestor_virtualenv_root() {
        let dir = parser_test_dir("installed-package-ancestor-venv");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source_path = dir.join("src").join("nested").join("app.py");
        let source = r#"
from typed_api import render_yaml

config = t"name: {name}"
render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &source_path)
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_installed_package_dotted_module_alias_annotation_propagates_to_template_variable() {
        let dir = parser_test_dir("installed-package-dotted-module-qualified");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("submodule.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api.submodule as api

config = t"name: {name}"
api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_installed_package_dotted_module_import_annotation_propagates_to_template_variable() {
        let dir = parser_test_dir("installed-package-dotted-module-import");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("submodule.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api.submodule

config = t"name: {name}"
typed_api.submodule.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_installed_package_dotted_import_keeps_package_root_binding() {
        let dir = parser_test_dir("installed-package-dotted-import-package-root");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );
        write_file(
            &site_packages.join("typed_api").join("submodule.py"),
            "value = 1\n",
        );

        let source = r#"
import typed_api.submodule

config = t"name: bad: {name}"
typed_api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_installed_package_dotted_import_keeps_intermediate_package_binding() {
        let dir = parser_test_dir("installed-package-dotted-import-intermediate-package");
        let site_packages = project_site_packages(&dir);
        write_file(&site_packages.join("typed_api").join("__init__.py"), "");
        write_file(
            &site_packages
                .join("typed_api")
                .join("subpkg")
                .join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );
        write_file(
            &site_packages
                .join("typed_api")
                .join("subpkg")
                .join("mod.py"),
            "value = 1\n",
        );

        let source = r#"
import typed_api.subpkg.mod

config = t"name: bad: {name}"
typed_api.subpkg.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_aliased_dotted_import_does_not_bind_installed_package_root() {
        let dir = parser_test_dir("installed-package-aliased-dotted-import-root-shadow");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );
        write_file(
            &site_packages.join("typed_api").join("submodule.py"),
            "value = 1\n",
        );

        let source = r#"
import typed_api.submodule as api

config = t"name: bad: {name}"
typed_api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_function_local_import_infers_installed_package_module_alias_within_scope() {
        let dir = parser_test_dir("installed-package-function-local-import");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
def outer():
    import typed_api
    config = t"name: bad: {name}"
    typed_api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_function_local_import_does_not_leak_to_module_scope() {
        let dir = parser_test_dir("installed-package-function-local-import-no-leak");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
def outer():
    import typed_api

config = t"name: bad: {name}"
typed_api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_local_callable_shadows_installed_package_signature() {
        let dir = parser_test_dir("installed-package-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_data(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_data

def render_data(template):
    return template

config = t"name: {name}"
render_data(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_local_variable_shadows_installed_package_dotted_module_import() {
        let dir = parser_test_dir("installed-package-dotted-module-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("submodule.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api.submodule

class LocalSubmodule:
    def render_yaml(self, template):
        return template

class Local:
    submodule = LocalSubmodule()

typed_api = Local()
config = t"name: bad: {name}"
typed_api.submodule.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_local_assignment_shadows_installed_package_direct_import() {
        let dir = parser_test_dir("installed-package-direct-import-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_yaml

render_yaml = lambda template: template
config = t"name: bad: {name}"
render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_local_class_without_signature_shadows_installed_package_direct_import() {
        let dir = parser_test_dir("installed-package-direct-import-class-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

class Loader:
    def __init__(self, template: Annotated[Template, "yaml"]) -> None:
        self.template = template
"#,
        );

        let source = r#"
from typed_api import Loader

class Loader:
    pass

config = t"name: bad: {name}"
Loader(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_local_value_assignment_after_definition_shadows_installed_package_direct_import() {
        let dir = parser_test_dir("installed-package-direct-import-reassigned");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_yaml

def render_yaml(template):
    return template

render_yaml = lambda template: template
config = t"name: bad: {name}"
render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_local_variable_shadows_installed_package_module_alias() {
        let dir = parser_test_dir("installed-package-module-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api as api

class Local:
    def render_yaml(self, template):
        return template

api = Local()
config = t"name: {name}"
api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_for_loop_variable_shadows_installed_package_module_alias() {
        let dir = parser_test_dir("installed-package-module-for-loop-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api as api

class Local:
    def render_yaml(self, template):
        return template

for api in [Local()]:
    config = t"name: bad: {name}"
    api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_with_alias_shadows_installed_package_module_alias() {
        let dir = parser_test_dir("installed-package-module-with-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api as api

class Local:
    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        return False

    def render_yaml(self, template):
        return template

with Local() as api:
    config = t"name: bad: {name}"
    api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_except_alias_shadows_installed_package_module_alias() {
        let dir = parser_test_dir("installed-package-module-except-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api as api

try:
    pass
except Exception as api:
    config = t"name: bad: {name}"
    api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_function_parameter_shadows_installed_package_module_alias() {
        let dir = parser_test_dir("installed-package-module-parameter-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api as api

def wrapper(api):
    config = t"name: {name}"
    api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_later_assignment_still_shadows_installed_package_module_alias_in_function_scope() {
        let dir = parser_test_dir("installed-package-module-later-assignment-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api as api

class Local:
    def render_yaml(self, template):
        return template

def wrapper():
    config = t"name: bad: {name}"
    api.render_yaml(config)
    api = Local()
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_global_directive_keeps_installed_package_direct_import_in_function_scope() {
        let dir = parser_test_dir("installed-package-direct-import-global");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_yaml

def wrapper():
    global render_yaml
    config = t"name: bad: {name}"
    render_yaml(config)
    render_yaml = render_yaml
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_global_template_assignment_keeps_inferred_language_hint() {
        let dir = parser_test_dir("installed-package-global-template-hint");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_yaml

config = ""

def wrapper():
    global config
    config = t"name: bad: {name}"
    render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_global_directive_in_nested_function_keeps_module_scope_import() {
        let dir = parser_test_dir("installed-package-direct-import-nested-global");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_yaml

def outer():
    render_yaml = None

    def inner():
        global render_yaml
        config = t"name: bad: {name}"
        render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_nonlocal_directive_keeps_outer_import_binding() {
        let dir = parser_test_dir("installed-package-direct-import-nonlocal");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
def outer():
    import typed_api as api

    def inner():
        nonlocal api
        config = t"name: bad: {name}"
        api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_nonlocal_template_assignment_keeps_inferred_language_hint() {
        let dir = parser_test_dir("installed-package-nonlocal-template-hint");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api as api

def outer():
    config = ""

    def inner():
        nonlocal config
        config = t"name: bad: {name}"
        api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_outer_global_directive_does_not_leak_into_inner_local_scope() {
        let dir = parser_test_dir("installed-package-outer-global-does-not-leak");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_json(template: Annotated[Template, "json"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_json

def outer():
    global render_json

    def inner():
        render_json(t"[1,,2]")
        render_json = None
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_delete_statement_shadows_installed_package_direct_import() {
        let dir = parser_test_dir("installed-package-direct-import-deleted");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_html

del render_html
page = t"<div class = 'x' >{name}</div>"
render_html(page)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("page".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_global_delete_statement_shadows_installed_package_direct_import() {
        let dir = parser_test_dir("installed-package-direct-import-global-deleted");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_html

def wrapper():
    global render_html
    del render_html
    page = t"<div class = 'x' >{name}</div>"
    render_html(page)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("page".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_comprehension_target_shadows_installed_package_module_alias() {
        let dir = parser_test_dir("installed-package-comprehension-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api

pages = [typed_api.render_html(t"<div class = 'x' >{name}</div>") for typed_api in [{}]]
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_comprehension_target_does_not_shadow_iterable_expression() {
        let dir = parser_test_dir("installed-package-comprehension-iterable");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api

pages = [page for typed_api in [typed_api.render_yaml(t"name: bad: {name}")]]
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_nested_import_does_not_clobber_outer_installed_package_module_alias() {
        let dir = parser_test_dir("installed-package-module-nested-import");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_json(template: Annotated[Template, "json"]) -> object:
    return None
"#,
        );
        write_file(&site_packages.join("other.py"), "value = 1\n");

        let source = r#"
import typed_api as api

config = t"[1,,2]"
api.render_json(config)

def wrapper():
    import other as api
    return api
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("json".to_string()));
    }

    #[test]
    fn test_outer_function_parameter_shadows_installed_package_module_alias_in_inner_scope() {
        let dir = parser_test_dir("installed-package-module-outer-parameter-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api as api

def outer(api):
    def inner():
        config = t"name: {name}"
        api.render_yaml(config)
    inner()
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_outer_function_parameter_shadows_installed_package_module_alias_inside_nested_class_method()
     {
        let dir = parser_test_dir("installed-package-module-class-method-parameter-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api as api

def outer(api):
    class Renderer:
        def render(self):
            config = t"name: {name}"
            api.render_yaml(config)

    Renderer().render()
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_class_body_binding_does_not_shadow_installed_package_module_alias_inside_method() {
        let dir = parser_test_dir("installed-package-class-body-shadow");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api as api

class Wrapper:
    api = object()

    def render(self):
        config = t"name: {name}"
        api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_splat_parameter_shadows_installed_package_module_alias() {
        let dir = parser_test_dir("installed-package-module-splat-parameter-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api as api

def outer(**api):
    config = t"name: {name}"
    api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_function_definition_shadows_installed_package_module_alias() {
        let dir = parser_test_dir("installed-package-module-function-shadowed");
        let site_packages = project_site_packages(&dir);
        write_file(
            &site_packages.join("typed_api").join("__init__.py"),
            r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
        );

        let source = r#"
import typed_api as api

def api(template):
    return template

config = t"name: {name}"
api.render_yaml(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_unresolved_installed_package_does_not_infer_language() {
        let dir = parser_test_dir("installed-package-missing");
        let source = r#"
from missing_api import render_data

config = t"name: {name}"
render_data(config)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].variable_name, Some("config".to_string()));
        assert_eq!(templates[0].language, None);
    }

    #[test]
    fn test_skip_list_avoids_resolving_annotation_helper_modules() {
        assert!(!should_resolve_imported_signatures("typing"));
        assert!(!should_resolve_imported_signatures("typing.Annotated"));
        assert!(!should_resolve_imported_signatures(
            "string.templatelib.Template"
        ));
        assert!(!should_resolve_imported_signatures("tdom.html"));
        assert!(!should_resolve_imported_signatures("tdom.svg"));
        assert!(!should_resolve_imported_signatures("tdom.processor.svg"));
        assert!(should_resolve_imported_signatures("typed_api.render_yaml"));
    }

    #[test]
    fn test_template_relevant_import_filter_skips_unused_installed_imports() {
        let dir = parser_test_dir("unused-installed-import");
        let site_packages = project_site_packages(&dir);
        let unrelated = site_packages.join("unrelated.py");
        write_file(
            &unrelated,
            r#"from typing import Annotated
from string.templatelib import Template

def render_json(template: Annotated[Template, "json"]) -> object:
    return None
"#,
        );

        let source = r#"
from tdom import html
from unrelated import render_json

page = html(t"<div>{name}</div>")
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let loaded_unrelated = parser.last_module_cache.contains_key(&unrelated);
        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("tdom".to_string()));
        assert!(!loaded_unrelated);
    }

    #[test]
    fn test_template_relevant_import_filter_keeps_called_template_helpers() {
        let dir = parser_test_dir("used-installed-import");
        let site_packages = project_site_packages(&dir);
        let typed_api = site_packages.join("typed_api.py");
        write_file(
            &typed_api,
            r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> object:
    return None
"#,
        );

        let source = r#"
from typed_api import render_html

page = t"<div>{name}</div>"
render_html(page)
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let loaded_typed_api = parser.last_module_cache.contains_key(&typed_api);
        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
        assert!(loaded_typed_api);
    }

    #[test]
    fn test_template_relevant_import_filter_does_not_match_alias_by_module_root() {
        let mut filter = HashSet::new();
        filter.insert("typed_api".to_string());

        assert!(!import_matches_resolution_filter(
            "api",
            "typed_api",
            Some(&filter)
        ));

        filter.insert("api".to_string());
        assert!(import_matches_resolution_filter(
            "api",
            "typed_api",
            Some(&filter)
        ));
    }

    #[test]
    fn test_template_location_scan_skips_import_resolution() {
        let source = r#"
from typed_api import render_html

page = t"<div>{name}</div>"
other = "not a template"
raw = tr"<span>{name}</span>"
"#;
        let mut parser = TemplateStringParser::new().unwrap();
        let locations = parser.find_template_string_locations(source).unwrap();

        assert_eq!(locations.len(), 2);
        assert_eq!(locations[0].start_line, 4);
        assert_eq!(locations[1].start_line, 6);
        assert!(parser.last_module_cache.is_empty());
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
    fn test_function_local_type_aliases_do_not_clobber_module_aliases() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

type T = Annotated[Template, "sql"]

def a():
    type T = Annotated[Template, "html"]
    local: T = t"<div>local</div>"

query: T = t"SELECT 1"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 2);
        assert_eq!(templates[0].content, "<div>local</div>");
        assert_eq!(templates[0].language, None);
        assert_eq!(templates[1].content, "SELECT 1");
        assert_eq!(templates[1].language, Some("sql".to_string()));
    }

    #[test]
    fn test_module_context_type_aliases_preserve_public_resolution() {
        let source = r#"
from typing_extensions import Annotated
from string.templatelib import Template

type HtmlTemplate = Annotated[Template, "html"]
type MaybeHtml = HtmlTemplate | Renderable
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert!(templates.is_empty());
        assert_eq!(
            parser.module_context().type_aliases.get("HtmlTemplate"),
            Some(&"html".to_string())
        );
        assert_eq!(
            parser.module_context().type_aliases.get("MaybeHtml"),
            Some(&"html".to_string())
        );
    }

    #[test]
    fn test_cyclic_alias_chain_safely_falls_back_to_unknown_language() {
        let source = r#"
type HtmlTemplate = AliasTemplate
type AliasTemplate = HtmlTemplate

page: HtmlTemplate = t"<div><"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, None);
        assert!(parser.module_context().type_aliases.is_empty());
    }

    #[test]
    fn test_imported_alias_reexport_annotation_propagates_to_template_variable() {
        let dir = parser_test_dir("imported-alias-reexport");
        fs::write(
            dir.join("bindings.py"),
            r#"from typing import Annotated
from string.templatelib import Template

type HtmlTemplate = Annotated[Template, "html"]
"#,
        )
        .unwrap();
        fs::write(
            dir.join("typed_api.py"),
            r#"from bindings import HtmlTemplate
"#,
        )
        .unwrap();

        let source = r#"
from typed_api import HtmlTemplate

page: HtmlTemplate | Renderable = t"<div><"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("broken.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
    }

    #[test]
    fn test_annotated_metadata_with_nested_commas_keeps_language() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

type HtmlTemplate = Annotated[Template, "html", Meta(x, y)]
page: HtmlTemplate = t"<div><"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
    }

    #[test]
    fn test_typing_extensions_annotated_alias_detection() {
        let source = r#"
from typing_extensions import Annotated
from string.templatelib import Template

page: Annotated[Template, "html"] = t"<div><"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
    }

    #[test]
    fn test_generic_base_alias_to_annotated_resolves_template_language() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

type Ann = Annotated
page: Ann[Template, "html"] = t"<div><"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
    }

    #[test]
    fn test_alias_named_like_special_type_still_resolves_local_template_alias() {
        let source = r#"
from typing import Annotated
from string.templatelib import Template

type Literal = Annotated[Template, "html"]
page: Literal = t"<div><"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
    }

    #[test]
    fn test_cyclic_import_reexport_rebuilds_incomplete_module_cache() {
        let dir = parser_test_dir("cyclic-alias-reexport");
        fs::write(
            dir.join("bindings_a.py"),
            r#"from bindings_b import Marker
from typing import Annotated
from string.templatelib import Template

type HtmlTemplate = Annotated[Template, "html"]
"#,
        )
        .unwrap();
        fs::write(
            dir.join("bindings_b.py"),
            r#"from bindings_a import HtmlTemplate

type Marker = HtmlTemplate
"#,
        )
        .unwrap();

        let source = r#"
from bindings_b import Marker

page: Marker = t"<div><"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("broken.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
    }

    #[test]
    fn test_entry_file_back_edge_short_circuits_cycle_without_losing_alias_resolution() {
        let dir = parser_test_dir("entry-file-back-edge");
        fs::write(
            dir.join("helper.py"),
            r#"from app import page
from typing import Annotated
from string.templatelib import Template

type HtmlTemplate = Annotated[Template, "html"]
"#,
        )
        .unwrap();

        let source = r#"
from helper import HtmlTemplate

page: HtmlTemplate = t"<div><"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].language, Some("html".to_string()));
    }

    #[test]
    fn test_cycle_tracking_state_is_cleared_between_parses() {
        let dir = parser_test_dir("cycle-state-reset");
        fs::write(
            dir.join("helper.py"),
            r#"from app import page
from typing import Annotated
from string.templatelib import Template

type HtmlTemplate = Annotated[Template, "html"]
"#,
        )
        .unwrap();

        let cyclic_source = r#"
from helper import HtmlTemplate

page: HtmlTemplate = t"<div><"
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let first_templates = parser
            .find_template_strings_in_file(cyclic_source, &dir.join("app.py"))
            .unwrap();
        assert_eq!(first_templates[0].language, Some("html".to_string()));

        let second_templates = parser
            .find_template_strings(
                r#"
from typing import Annotated
from string.templatelib import Template

page: Annotated[Template, "yaml"] = t"name: {name}"
"#,
            )
            .unwrap();

        let _ = fs::remove_dir_all(dir);

        assert_eq!(second_templates.len(), 1);
        assert_eq!(second_templates[0].language, Some("yaml".to_string()));
    }

    #[test]
    fn test_literal_value_type_inference_uses_shared_type_parser() {
        let source = r#"
from typing import Literal

def render(flag: Literal[True, False] | None, value: Literal[1, 1.5, "x", None]) -> None:
    return None
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert!(templates.is_empty());

        let signature = parser
            .module_context()
            .callable_signatures
            .get("render")
            .unwrap();
        assert_eq!(
            signature.parameters[0].value_types,
            vec![CallableValueType::Bool]
        );
        assert!(signature.parameters[0].accepts_none);
        assert_eq!(
            signature.parameters[1].value_types,
            vec![
                CallableValueType::Int,
                CallableValueType::Float,
                CallableValueType::String
            ]
        );
        assert!(signature.parameters[1].accepts_none);
    }

    #[test]
    fn test_callable_signatures_preserve_python_type_annotations() {
        let source = r#"
from dataclasses import InitVar, dataclass
from typing import Literal

class User:
    name: str

def render(user: User, labels: list[str], state: Literal["open", "closed"], maybe: "User | None") -> None:
    return None

@dataclass
class Card:
    title: str
    owner: InitVar[User]
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser.find_template_strings(source).unwrap();

        assert!(templates.is_empty());
        let context = parser.module_context();
        let render = context.callable_signatures.get("render").unwrap();
        assert_eq!(
            render.parameters[0].type_annotation,
            Some("User".to_string())
        );
        assert_eq!(
            render.parameters[1].type_annotation,
            Some("list[str]".to_string())
        );
        assert_eq!(
            render.parameters[2].type_annotation,
            Some("Literal[\"open\", \"closed\"]".to_string())
        );
        assert_eq!(
            render.parameters[3].type_annotation,
            Some("User | None".to_string())
        );

        let card = context.callable_signatures.get("Card").unwrap();
        assert_eq!(card.parameters[0].type_annotation, Some("str".to_string()));
        assert_eq!(card.parameters[1].type_annotation, Some("User".to_string()));
    }

    #[test]
    fn test_imported_callable_signature_tracks_type_annotation_module() {
        let dir = parser_test_dir("imported-callable-type-annotation-module");
        write_file(
            &dir.join("components.py"),
            r#"
class User:
    name: str

def Card(*, owner: User) -> object:
    return object()
"#,
        );
        let source = r#"
from components import Card
from tdom import html

def handler(age: int) -> None:
    html(t'<{Card} owner={age} />')
"#;

        let mut parser = TemplateStringParser::new().unwrap();
        let templates = parser
            .find_template_strings_in_file(source, &dir.join("app.py"))
            .unwrap();

        assert_eq!(templates.len(), 1);
        let signature = parser
            .module_context()
            .callable_signatures
            .get("Card")
            .unwrap();
        assert_eq!(
            signature.parameters[0].type_annotation,
            Some("User".to_string())
        );
        assert_eq!(
            signature.parameters[0].type_annotation_module,
            Some("components".to_string())
        );

        let _ = fs::remove_dir_all(dir);
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
