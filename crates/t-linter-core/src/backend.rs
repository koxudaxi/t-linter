use tstring_html as backend_html;
use tstring_json as backend_json;
use tstring_syntax::{BackendResult, TemplateInput};
use tstring_tdom as backend_tdom;
use tstring_thtml as backend_thtml;
use tstring_toml as backend_toml;
use tstring_yaml as backend_yaml;

const JSON_VALUE_TYPE: &str = "str | int | float | bool | None | dict[str, object] | list[object]";
const STRING_TYPE: &str = "str";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TemplateBackend {
    Html,
    Thtml,
    Tdom,
    Json,
    Yaml,
    Toml,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterpolationTypeRequirement {
    pub interpolation_index: usize,
    pub expected_type: &'static str,
    pub expected_description: &'static str,
}

impl TemplateBackend {
    pub(crate) fn for_language(language: &str) -> Option<Self> {
        match language {
            "html" => Some(Self::Html),
            "thtml" => Some(Self::Thtml),
            "tdom" => Some(Self::Tdom),
            "json" => Some(Self::Json),
            "yaml" | "yml" => Some(Self::Yaml),
            "toml" => Some(Self::Toml),
            _ => None,
        }
    }

    pub(crate) fn check_template(self, input: &TemplateInput) -> BackendResult<()> {
        match self {
            Self::Html => backend_html::check_template(input),
            Self::Thtml => backend_thtml::check_template(input),
            Self::Tdom => backend_tdom::check_template(input),
            Self::Json => backend_json::check_template(input),
            Self::Yaml => backend_yaml::check_template(input),
            Self::Toml => backend_toml::check_template(input),
        }
    }

    pub(crate) fn interpolation_type_requirements(
        self,
        input: &TemplateInput,
    ) -> BackendResult<Vec<InterpolationTypeRequirement>> {
        match self {
            Self::Json => json_interpolation_type_requirements(input),
            Self::Html | Self::Thtml | Self::Tdom | Self::Yaml | Self::Toml => Ok(Vec::new()),
        }
    }
}

fn json_interpolation_type_requirements(
    input: &TemplateInput,
) -> BackendResult<Vec<InterpolationTypeRequirement>> {
    let document = backend_json::parse_template(input)?;
    let mut requirements = Vec::new();
    collect_json_value_requirements(&document.value, &mut requirements);
    requirements.sort_by_key(|requirement| requirement.interpolation_index);
    Ok(requirements)
}

fn collect_json_value_requirements(
    value: &backend_json::JsonValueNode,
    requirements: &mut Vec<InterpolationTypeRequirement>,
) {
    match value {
        backend_json::JsonValueNode::String(node) => {
            collect_json_string_requirements(node, requirements);
        }
        backend_json::JsonValueNode::Literal(_) => {}
        backend_json::JsonValueNode::Interpolation(node) => {
            requirements.push(json_requirement(node));
        }
        backend_json::JsonValueNode::Object(node) => {
            for member in &node.members {
                collect_json_key_requirements(&member.key, requirements);
                collect_json_value_requirements(&member.value, requirements);
            }
        }
        backend_json::JsonValueNode::Array(node) => {
            for item in &node.items {
                collect_json_value_requirements(item, requirements);
            }
        }
    }
}

fn collect_json_key_requirements(
    key: &backend_json::JsonKeyNode,
    requirements: &mut Vec<InterpolationTypeRequirement>,
) {
    match &key.value {
        backend_json::JsonKeyValue::String(node) => {
            collect_json_string_requirements(node, requirements);
        }
        backend_json::JsonKeyValue::Interpolation(node) => {
            requirements.push(json_requirement(node));
        }
    }
}

fn collect_json_string_requirements(
    string: &backend_json::JsonStringNode,
    requirements: &mut Vec<InterpolationTypeRequirement>,
) {
    for chunk in &string.chunks {
        if let backend_json::JsonStringPart::Interpolation(node) = chunk {
            requirements.push(json_requirement(node));
        }
    }
}

fn json_requirement(node: &backend_json::JsonInterpolationNode) -> InterpolationTypeRequirement {
    match node.role.as_str() {
        "value" => InterpolationTypeRequirement {
            interpolation_index: node.interpolation_index,
            expected_type: JSON_VALUE_TYPE,
            expected_description: "json value",
        },
        "key" => InterpolationTypeRequirement {
            interpolation_index: node.interpolation_index,
            expected_type: STRING_TYPE,
            expected_description: "json object key",
        },
        "string_fragment" => InterpolationTypeRequirement {
            interpolation_index: node.interpolation_index,
            expected_type: STRING_TYPE,
            expected_description: "json string fragment",
        },
        _ => InterpolationTypeRequirement {
            interpolation_index: node.interpolation_index,
            expected_type: JSON_VALUE_TYPE,
            expected_description: "json interpolation",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tstring_syntax::{TemplateInput, TemplateInterpolation, TemplateSegment};

    fn interpolation(index: usize, expression: &str) -> TemplateSegment {
        TemplateSegment::Interpolation(TemplateInterpolation {
            expression: expression.to_string(),
            conversion: None,
            format_spec: String::new(),
            interpolation_index: index,
            raw_source: None,
        })
    }

    #[test]
    fn json_backend_reports_contextual_type_requirements() {
        let input = TemplateInput::from_segments(vec![
            TemplateSegment::StaticText("{".to_string()),
            interpolation(0, "key"),
            TemplateSegment::StaticText(": ".to_string()),
            interpolation(1, "value"),
            TemplateSegment::StaticText(", \"label\": \"".to_string()),
            interpolation(2, "label"),
            TemplateSegment::StaticText("\"}".to_string()),
        ]);

        let requirements = TemplateBackend::Json
            .interpolation_type_requirements(&input)
            .expect("requirements");

        assert_eq!(
            requirements,
            vec![
                InterpolationTypeRequirement {
                    interpolation_index: 0,
                    expected_type: STRING_TYPE,
                    expected_description: "json object key",
                },
                InterpolationTypeRequirement {
                    interpolation_index: 1,
                    expected_type: JSON_VALUE_TYPE,
                    expected_description: "json value",
                },
                InterpolationTypeRequirement {
                    interpolation_index: 2,
                    expected_type: STRING_TYPE,
                    expected_description: "json string fragment",
                },
            ]
        );
    }

    #[test]
    fn backend_without_type_requirements_returns_empty_list() {
        let input = TemplateInput::from_segments(vec![
            TemplateSegment::StaticText("<div>".to_string()),
            interpolation(0, "value"),
            TemplateSegment::StaticText("</div>".to_string()),
        ]);

        assert!(
            TemplateBackend::Html
                .interpolation_type_requirements(&input)
                .expect("requirements")
                .is_empty()
        );
    }
}
