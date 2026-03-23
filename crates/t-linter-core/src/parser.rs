use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
}

impl CallableSignature {
    fn is_empty(&self) -> bool {
        self.parameters.is_empty() && !self.accepts_kwargs
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallableParameter {
    pub position: usize,
    pub name: String,
    pub template_language: Option<String>,
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

#[derive(Debug, Clone, Copy)]
struct CallArgument<'a> {
    position: usize,
    keyword: Option<&'a str>,
    value: Node<'a>,
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
            module_load_stack: Vec::new(),
            modules_with_incomplete_dependencies: HashSet::new(),
        })
    }

    pub fn find_template_strings(&mut self, source: &str) -> Result<Vec<TemplateStringInfo>> {
        self.search_root = None;
        self.current_file_path = None;
        self.find_template_strings_with_search_root(source, None)
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

        let mut context = ModuleContext::default();

        let module_type_data = self.collect_module_context(&tree, source, &mut context)?;
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
        )?;

        context.imports = module_type_data.imports.clone();
        context.callable_signatures = module_type_data.callable_signatures.clone();
        context.local_callable_signature_names =
            module_type_data.local_callable_signature_names.clone();
        context.imported_module_paths = module_type_data.imported_module_paths.clone();
        context.scoped_import_bindings = module_type_data.scoped_import_bindings.clone();
        context.type_aliases =
            self.resolve_module_type_aliases(&module_type_data, &mut module_cache)?;

        Ok(module_type_data)
    }

    fn build_module_type_data(
        &mut self,
        tree: &Tree,
        source: &str,
        current_module_name: Option<&str>,
        current_module_is_package: bool,
        module_key: ModuleCacheKey,
        module_cache: &mut HashMap<PathBuf, ModuleTypeData>,
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
            self.collect_imported_callable_signatures(&mut module_type_data, module_cache)?;
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
                    let name = name_node.utf8_text(source.as_bytes())?;
                    let type_text = type_node.utf8_text(source.as_bytes())?;

                    if type_text.contains("TypeAlias") {
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
            match child.kind() {
                "function_definition" => {
                    let Some(name_node) = child.child_by_field_name("name") else {
                        continue;
                    };
                    let Some(params_node) = child.child_by_field_name("parameters") else {
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
                    let Some(name_node) = child.child_by_field_name("name") else {
                        continue;
                    };
                    let Some(body_node) = child.child_by_field_name("body") else {
                        continue;
                    };
                    let name = name_node.utf8_text(source.as_bytes())?.to_string();
                    if let Some(signature) = self.extract_class_constructor_languages(
                        body_node,
                        source,
                        module_type_data,
                        module_cache,
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

        Ok(None)
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
                    let type_hints = if let Some(type_node) = type_node {
                        self.resolve_type_info_from_type_node(
                            type_node,
                            source,
                            module_type_data,
                            module_cache,
                        )?
                    } else {
                        ResolvedTypeInfo::default()
                    };

                    signature.parameters.push(CallableParameter {
                        position,
                        name: parameter_name.to_string(),
                        template_language: type_hints.template_language,
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
    ) -> Result<()> {
        for (alias, import_path) in module_type_data.imports.clone() {
            if !should_resolve_imported_signatures(&import_path) {
                continue;
            }

            if !self.import_path_resolves_to_module(&import_path) {
                if let Some(signatures) =
                    self.resolve_imported_callable_signature(&import_path, module_cache)?
                {
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
                for (callable_name, signatures) in module_signatures.callable_signatures {
                    module_type_data
                        .callable_signatures
                        .entry(format!("{import_path}.{callable_name}"))
                        .or_insert(signatures);
                }
            }
        }

        for import_path in module_type_data.imported_module_paths.clone() {
            if !should_resolve_imported_signatures(&import_path) {
                continue;
            }

            if let Some(module_signatures) =
                self.load_imported_module_type_data(&import_path, module_cache)?
            {
                for (callable_name, signatures) in module_signatures.callable_signatures {
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
    ) -> Result<HashMap<AssignmentKey, String>> {
        let query_str = r#"
        (call
            function: (_) @func_expr
            arguments: (argument_list) @args)
        "#;

        let query = Query::new(&tree_sitter_python::LANGUAGE.into(), query_str)
            .context("Failed to create call query")?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
        let mut hints = HashMap::<AssignmentKey, Option<String>>::new();

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
                match hints.get(&binding.key) {
                    Some(Some(existing))
                        if parameter
                            .template_language
                            .as_ref()
                            .is_some_and(|lang| existing != lang) =>
                    {
                        hints.insert(binding.key.clone(), None);
                    }
                    Some(None) => {}
                    _ => {
                        if let Some(language) = &parameter.template_language {
                            hints.insert(binding.key.clone(), Some(language.clone()));
                        }
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
        &mut self,
        tree: &Tree,
        source: &str,
        templates: &mut Vec<TemplateStringInfo>,
        context: &ModuleContext,
        variable_language_hints: &HashMap<AssignmentKey, String>,
        assignments: &[VariableAssignment],
        scope_directives: &[ScopeDirective],
        name_bindings: &[NameBinding],
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
                [
                    (string) @string
                    (keyword_argument
                        value: (string) @string)
                ]
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
            let mut var_name_node = None;
            let mut type_annotation = None;
            let mut func_name = None;

            for capture in match_.captures {
                let name = query.capture_names()[capture.index as usize];
                match name {
                    "string" => string_node = Some(capture.node),
                    "var_name" => {
                        var_name = Some(capture.node.utf8_text(source.as_bytes())?);
                        var_name_node = Some(capture.node);
                    }
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
            }
        }

        Ok(())
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
        variable_language_hints: &HashMap<AssignmentKey, String>,
        assignments: &[VariableAssignment],
        scope_directives: &[ScopeDirective],
        name_bindings: &[NameBinding],
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
            self.resolve_language_from_type_node(type_node, source, context)?
        } else if let Some(func) = func_name {
            self.infer_language_from_function_call(
                func,
                &node,
                source,
                context,
                assignments,
                scope_directives,
                name_bindings,
            )?
        } else {
            None
        };
        if language.is_none() {
            if let Some(var_node) = var_name_node
                && let Some(binding) =
                    assignment_key_for_node_with_directives(var_node, source, scope_directives)
            {
                language = variable_language_hints.get(&binding).cloned();
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

    fn infer_language_from_function_call(
        &self,
        func_name: &str,
        string_node: &Node,
        source: &str,
        context: &ModuleContext,
        assignments: &[VariableAssignment],
        scope_directives: &[ScopeDirective],
        name_bindings: &[NameBinding],
    ) -> Result<Option<String>> {
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
            return Ok(None);
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
                        return Ok(parameter.template_language.clone());
                    }
                    break;
                }
            }
        }

        Ok(None)
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
                if !callable_matches_import_target(callee, &import_target) {
                    return Ok(None);
                }
            } else if context.imports.contains_key(callee)
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

        let Some((base, member)) = callee.split_once('.') else {
            return Ok(None);
        };
        if module_reference_is_shadowed(
            callee_node,
            source,
            assignments,
            scope_directives,
            name_bindings,
        )? {
            return Ok(None);
        }
        let import_target =
            if let Some(root_identifier) = callee_node.and_then(root_identifier_node) {
                resolve_import_target_for_identifier(
                    root_identifier,
                    source,
                    scope_directives,
                    &context.scoped_import_bindings,
                )?
            } else {
                None
            };
        let Some(import_target) = import_target.or_else(|| context.imports.get(base).cloned())
        else {
            return Ok(None);
        };
        Ok(context
            .callable_signatures
            .get(&format!("{import_target}.{member}")))
    }

    fn resolve_language_from_type_node(
        &mut self,
        type_node: Node,
        source: &str,
        _context: &ModuleContext,
    ) -> Result<Option<String>> {
        let mut module_cache = HashMap::new();
        let module_type_data = self.last_module_type_data.clone();
        Ok(self
            .resolve_type_info_from_type_node(
                type_node,
                source,
                &module_type_data,
                &mut module_cache,
            )?
            .template_language)
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
    for value_type in other.value_types {
        push_value_type(&mut target.value_types, value_type);
    }
    target.accepts_none |= other.accepts_none;
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

impl TemplateStringParser {
    fn python_search_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();

        if let Some(root) = self.search_root.as_deref() {
            push_search_root(&mut roots, root.to_path_buf());
        }
        if let Ok(current_dir) = env::current_dir() {
            push_search_root(&mut roots, current_dir);
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
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_digit() || ch == '-' || ch == '+')
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
                "lambda" => lambda_node = Some(capture.node),
                _ => {}
            }
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
        if let Some(assignment) = assignments.iter().rev().find(|assignment| {
            assignment.key.name == name
                && assignment.key.scope_start == scope.start
                && assignment.key.scope_end == scope.end
                && (scope_uses_function_like_binding_rules(scope.kind)
                    || assignment.key.assignment_start < use_position)
        }) {
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
        Some(NameBindingKind::Definition | NameBindingKind::Value)
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
            | "typing.Annotated"
            | "typing_extensions.Annotated"
    )
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
        assert!(should_resolve_imported_signatures("typed_api.render_yaml"));
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
