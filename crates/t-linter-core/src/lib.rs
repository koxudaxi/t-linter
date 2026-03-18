use anyhow::Result;

pub mod formatting;
pub mod highlighter;
pub mod lint;
pub mod parser;

pub use formatting::{TemplateEdit, apply_template_edits, format_document, format_document_range};
pub use highlighter::{HighlightedRange, TemplateHighlighter};
pub use lint::{
    LintDiagnostic, LintFileResult, LintRunSummary, LintSeverity, file_read_error, lint_source,
};
pub use parser::{
    Expression, InterpolationInfo, Location, StaticTextSegment, TemplatePart, TemplateStringInfo,
    TemplateStringParser,
};

pub fn init() -> Result<()> {
    tracing::info!("t-linter-core initialized");
    Ok(())
}
