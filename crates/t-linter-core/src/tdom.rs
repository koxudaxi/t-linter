use tstring_tdom::ComponentPropValueKind;

use crate::parser::{CallableParameter, CallableSignature, CallableValueType, ModuleContext};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComponentPropExpectedType {
    pub annotation: String,
    pub import_module: Option<String>,
}

pub(crate) fn resolve_component_signature<'a>(
    module_context: &'a ModuleContext,
    expression: &str,
) -> Option<&'a CallableSignature> {
    module_context
        .callable_signatures
        .get(expression)
        .or_else(|| {
            let (base, suffix) = expression.split_once('.')?;
            let import_target = module_context.imports.get(base)?;
            module_context
                .callable_signatures
                .get(&format!("{import_target}.{suffix}"))
        })
}

pub(crate) fn expected_type_for_component_prop(
    parameter: &CallableParameter,
    value_kind: ComponentPropValueKind,
) -> Option<ComponentPropExpectedType> {
    match value_kind {
        ComponentPropValueKind::Typed => parameter
            .type_annotation
            .as_ref()
            .map(|annotation| ComponentPropExpectedType {
                annotation: annotation.clone(),
                import_module: parameter.type_annotation_module.clone(),
            })
            .or_else(|| fallback_expected_type(parameter).map(ComponentPropExpectedType::local)),
        ComponentPropValueKind::StringLike | ComponentPropValueKind::StringFragment
            if parameter.value_types.contains(&CallableValueType::String) =>
        {
            Some(ComponentPropExpectedType::local("str".to_string()))
        }
        ComponentPropValueKind::StringLike | ComponentPropValueKind::StringFragment => None,
        _ => None,
    }
}

impl ComponentPropExpectedType {
    fn local(annotation: String) -> Self {
        Self {
            annotation,
            import_module: None,
        }
    }
}

fn fallback_expected_type(parameter: &CallableParameter) -> Option<String> {
    let mut names = parameter
        .value_types
        .iter()
        .map(|value_type| match value_type {
            CallableValueType::Bool => "bool",
            CallableValueType::Int => "int",
            CallableValueType::Float => "float",
            CallableValueType::String => "str",
        })
        .collect::<Vec<_>>();
    if parameter.accepts_none {
        names.push("None");
    }
    names.sort_unstable();
    names.dedup();
    (!names.is_empty()).then(|| names.join(" | "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parameter(
        type_annotation: Option<&str>,
        value_types: Vec<CallableValueType>,
    ) -> CallableParameter {
        CallableParameter {
            position: 0,
            name: "value".to_string(),
            type_annotation: type_annotation.map(str::to_string),
            type_annotation_module: None,
            template_language: None,
            template_profile: None,
            value_types,
            accepts_none: false,
            required: true,
            allows_keyword: true,
            keyword_only: true,
        }
    }

    #[test]
    fn typed_component_props_use_python_annotation() {
        let parameter = parameter(Some("list[str]"), vec![CallableValueType::String]);

        assert_eq!(
            expected_type_for_component_prop(&parameter, ComponentPropValueKind::Typed),
            Some(ComponentPropExpectedType::local("list[str]".to_string()))
        );
    }

    #[test]
    fn string_like_component_props_use_string_when_parameter_accepts_it() {
        let parameter = parameter(Some("str | None"), vec![CallableValueType::String]);

        assert_eq!(
            expected_type_for_component_prop(&parameter, ComponentPropValueKind::StringFragment),
            Some(ComponentPropExpectedType::local("str".to_string()))
        );
    }

    #[test]
    fn typed_component_props_keep_import_module() {
        let mut parameter = parameter(Some("User"), Vec::new());
        parameter.type_annotation_module = Some("components".to_string());

        assert_eq!(
            expected_type_for_component_prop(&parameter, ComponentPropValueKind::Typed),
            Some(ComponentPropExpectedType {
                annotation: "User".to_string(),
                import_module: Some("components".to_string()),
            })
        );
    }
}
