use tstring_html as backend_html;
use tstring_json as backend_json;
use tstring_syntax::{BackendResult, InterpolationTypeRequirement, TemplateInput};
use tstring_tdom as backend_tdom;
use tstring_thtml as backend_thtml;
use tstring_toml as backend_toml;
use tstring_yaml as backend_yaml;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TemplateBackend {
    Html,
    Thtml,
    Tdom,
    Json,
    Yaml,
    Toml,
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
            Self::Json => backend_json::interpolation_type_requirements(input),
            Self::Html | Self::Thtml | Self::Tdom | Self::Yaml | Self::Toml => Ok(Vec::new()),
        }
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
    fn json_backend_delegates_contextual_type_requirements() {
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

        assert_eq!(requirements.len(), 3);
        assert_eq!(requirements[0].expected_description, "json object key");
        assert_eq!(requirements[1].expected_description, "json value");
        assert_eq!(requirements[2].expected_description, "json string fragment");
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
