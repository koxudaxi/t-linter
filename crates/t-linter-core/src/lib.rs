use anyhow::Result;

pub(crate) mod backend;
pub mod formatting;
pub mod highlighter;
pub mod lint;
pub mod parser;
pub mod project_config;
pub(crate) mod python;
pub mod shadow;
pub(crate) mod tdom;

pub use formatting::{
    FormatError, FormatOptions, TemplateEdit, apply_template_edits, format_document,
    format_document_in_file, format_document_in_file_with_options, format_document_range,
    format_document_range_with_options, format_document_with_options,
};
pub use highlighter::{HighlightedRange, TemplateHighlighter};
pub use lint::{
    DiagnosticData, DiagnosticEdit, DiagnosticEditRange, LintDiagnostic, LintFileResult,
    LintRunSummary, LintSeverity, file_read_error, lint_source,
};
pub use parser::{
    Expression, InterpolationInfo, Location, StaticTextSegment, TemplatePart, TemplateStringInfo,
    TemplateStringParser,
};
pub use project_config::{
    ProjectConfig, find_config_root, load_project_config, load_project_config_for_path,
};
pub use shadow::{ShadowCheckSite, ShadowDocument, synthesize_for_type_check};

pub fn init() -> Result<()> {
    tracing::info!("t-linter-core initialized");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_succeeds() {
        init().expect("core init");
    }
}
