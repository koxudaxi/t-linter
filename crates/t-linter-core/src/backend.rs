use std::str::FromStr;
use tstring_html as backend_html;
use tstring_json as backend_json;

use tstring_syntax::{BackendError, BackendResult, InterpolationTypeRequirement, TemplateInput};
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
        let language = language.trim();
        match language.len() {
            3 if language.eq_ignore_ascii_case("yml") => Some(Self::Yaml),
            4 if language.eq_ignore_ascii_case("html") => Some(Self::Html),
            4 if language.eq_ignore_ascii_case("tdom") => Some(Self::Tdom),
            4 if language.eq_ignore_ascii_case("json") => Some(Self::Json),
            4 if language.eq_ignore_ascii_case("yaml") => Some(Self::Yaml),
            4 if language.eq_ignore_ascii_case("toml") => Some(Self::Toml),
            5 if language.eq_ignore_ascii_case("thtml") => Some(Self::Thtml),
            _ => None,
        }
    }

    pub(crate) fn check_template(
        self,
        input: &TemplateInput,
        profile: Option<&str>,
    ) -> BackendResult<()> {
        match (self, profile) {
            (Self::Html, None) => backend_html::check_template(input),
            (Self::Thtml, None) => backend_thtml::check_template(input),
            (Self::Tdom, None) => backend_tdom::check_template(input),
            (Self::Tdom, Some(profile)) if profile.eq_ignore_ascii_case("svg") => {
                backend_tdom::check_template(input)
            }
            (Self::Json, None) => backend_json::check_template(input),
            (Self::Yaml, None) => backend_yaml::check_template(input),
            (Self::Toml, None) => backend_toml::check_template(input),
            (Self::Json, Some(profile)) => backend_json::check_template_with_profile(
                input,
                parse_profile::<backend_json::JsonProfile>(profile)?,
            ),
            (Self::Yaml, Some(profile)) => backend_yaml::check_template_with_profile(
                input,
                parse_profile::<backend_yaml::YamlProfile>(profile)?,
            ),
            (Self::Toml, Some(profile)) => backend_toml::check_template_with_profile(
                input,
                parse_profile::<backend_toml::TomlProfile>(profile)?,
            ),
            (backend @ (Self::Html | Self::Thtml | Self::Tdom), Some(profile)) => {
                Err(unsupported_profile_error(backend, profile))
            }
        }
    }

    pub(crate) fn interpolation_type_requirements(
        self,
        input: &TemplateInput,
        profile: Option<&str>,
    ) -> BackendResult<Vec<InterpolationTypeRequirement>> {
        match (self, profile) {
            (Self::Json, None) => backend_json::interpolation_type_requirements(input),
            (Self::Yaml, None) => backend_yaml::interpolation_type_requirements(input),
            (Self::Toml, None) => backend_toml::interpolation_type_requirements(input),
            (Self::Json, Some(profile)) => {
                backend_json::interpolation_type_requirements_with_profile(
                    input,
                    parse_profile::<backend_json::JsonProfile>(profile)?,
                )
            }
            (Self::Yaml, Some(profile)) => {
                backend_yaml::interpolation_type_requirements_with_profile(
                    input,
                    parse_profile::<backend_yaml::YamlProfile>(profile)?,
                )
            }
            (Self::Toml, Some(profile)) => {
                backend_toml::interpolation_type_requirements_with_profile(
                    input,
                    parse_profile::<backend_toml::TomlProfile>(profile)?,
                )
            }
            (Self::Html | Self::Thtml | Self::Tdom, None) => Ok(Vec::new()),
            (Self::Tdom, Some(profile)) if profile.eq_ignore_ascii_case("svg") => Ok(Vec::new()),
            (backend @ (Self::Html | Self::Thtml | Self::Tdom), Some(profile)) => {
                Err(unsupported_profile_error(backend, profile))
            }
        }
    }

    pub(crate) fn format_template(
        self,
        input: &TemplateInput,
        profile: Option<&str>,
        line_length: usize,
    ) -> BackendResult<String> {
        match (self, profile) {
            (Self::Html, None) => backend_html::format_template_with_options(
                input,
                &backend_html::FormatOptions { line_length },
            ),
            (Self::Thtml, None) => backend_thtml::format_template_with_options(
                input,
                &backend_html::FormatOptions { line_length },
            ),
            (Self::Tdom, None) => backend_tdom::format_template_with_options(
                input,
                &backend_tdom::FormatOptions { line_length },
            ),
            (Self::Tdom, Some(profile)) if profile.eq_ignore_ascii_case("svg") => {
                backend_tdom::format_template_as_svg_with_options(
                    input,
                    &backend_tdom::FormatOptions { line_length },
                )
            }
            (Self::Json, None) => backend_json::format_template(input),
            (Self::Yaml, None) => backend_yaml::format_template(input),
            (Self::Toml, None) => backend_toml::format_template(input),
            (Self::Json, Some(profile)) => backend_json::format_template_with_profile(
                input,
                parse_profile::<backend_json::JsonProfile>(profile)?,
            ),
            (Self::Yaml, Some(profile)) => backend_yaml::format_template_with_profile(
                input,
                parse_profile::<backend_yaml::YamlProfile>(profile)?,
            ),
            (Self::Toml, Some(profile)) => backend_toml::format_template_with_profile(
                input,
                parse_profile::<backend_toml::TomlProfile>(profile)?,
            ),
            (backend @ (Self::Html | Self::Thtml | Self::Tdom), Some(profile)) => {
                Err(unsupported_profile_error(backend, profile))
            }
        }
    }
}

fn parse_profile<T>(profile: &str) -> BackendResult<T>
where
    T: FromStr<Err = String>,
{
    profile.parse().map_err(BackendError::semantic)
}

fn unsupported_profile_error(backend: TemplateBackend, profile: &str) -> BackendError {
    BackendError::semantic(format!(
        "Profiles are not supported for {} templates: {profile:?}.",
        backend.name()
    ))
}

impl TemplateBackend {
    fn name(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Thtml => "thtml",
            Self::Tdom => "tdom",
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
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
            .interpolation_type_requirements(&input, None)
            .expect("requirements");

        assert_eq!(requirements.len(), 3);
        assert_eq!(requirements[0].expected_description, "json object key");
        assert_eq!(requirements[1].expected_description, "json value");
        assert_eq!(requirements[2].expected_description, "json string fragment");
    }

    #[test]
    fn yaml_backend_delegates_contextual_type_requirements() {
        let input = TemplateInput::from_segments(vec![
            interpolation(0, "key"),
            TemplateSegment::StaticText(": ".to_string()),
            interpolation(1, "value"),
            TemplateSegment::StaticText("\nlabel: \"".to_string()),
            interpolation(2, "label"),
            TemplateSegment::StaticText("\"".to_string()),
        ]);

        let requirements = TemplateBackend::Yaml
            .interpolation_type_requirements(&input, None)
            .expect("requirements");

        assert_eq!(requirements.len(), 3);
        assert_eq!(requirements[0].expected_description, "yaml mapping key");
        assert_eq!(requirements[1].expected_description, "yaml value");
        assert_eq!(requirements[2].expected_description, "yaml scalar fragment");
    }

    #[test]
    fn toml_backend_delegates_contextual_type_requirements() {
        let input = TemplateInput::from_segments(vec![
            interpolation(0, "key"),
            TemplateSegment::StaticText(" = ".to_string()),
            interpolation(1, "value"),
            TemplateSegment::StaticText("\nlabel = \"".to_string()),
            interpolation(2, "label"),
            TemplateSegment::StaticText("\"".to_string()),
        ]);

        let requirements = TemplateBackend::Toml
            .interpolation_type_requirements(&input, None)
            .expect("requirements");

        assert_eq!(requirements.len(), 3);
        assert_eq!(requirements[0].expected_description, "toml key");
        assert_eq!(requirements[1].expected_description, "toml value");
        assert_eq!(requirements[2].expected_description, "toml string fragment");
    }

    #[test]
    fn backend_lookup_normalizes_language_names() {
        assert_eq!(
            TemplateBackend::for_language(" JSON "),
            Some(TemplateBackend::Json)
        );
        assert_eq!(
            TemplateBackend::for_language("YML"),
            Some(TemplateBackend::Yaml)
        );
        assert_eq!(
            TemplateBackend::for_language("Html"),
            Some(TemplateBackend::Html)
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
                .interpolation_type_requirements(&input, None)
                .expect("requirements")
                .is_empty()
        );
    }
}
