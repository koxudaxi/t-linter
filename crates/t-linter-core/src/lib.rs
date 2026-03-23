use anyhow::Result;

pub mod formatting;
pub mod highlighter;
pub mod lint;
pub mod parser;
pub mod project_config;

pub use formatting::{
    FormatError, FormatOptions, TemplateEdit, apply_template_edits, format_document,
    format_document_in_file, format_document_in_file_with_options, format_document_range,
    format_document_range_with_options, format_document_with_options,
};
pub use highlighter::{HighlightedRange, TemplateHighlighter};
pub use lint::{
    LintDiagnostic, LintFileResult, LintRunSummary, LintSeverity, file_read_error, lint_source,
};
pub use parser::{
    Expression, InterpolationInfo, Location, StaticTextSegment, TemplatePart, TemplateStringInfo,
    TemplateStringParser,
};
pub use project_config::{
    ProjectConfig, find_config_root, load_project_config, load_project_config_for_path,
};

pub fn init() -> Result<()> {
    tracing::info!("t-linter-core initialized");
    Ok(())
}
