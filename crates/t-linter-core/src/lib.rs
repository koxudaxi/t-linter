use anyhow::Result;

pub mod format;
pub mod highlighter;
pub mod lint;
pub mod parser;

pub use format::{FormatRange, FormatResult, format_source, format_source_in_ranges};
pub use highlighter::{HighlightedRange, TemplateHighlighter};
pub use lint::{
    LintDiagnostic, LintFileResult, LintRunSummary, LintSeverity, file_read_error, lint_source,
};
pub use parser::{Expression, Location, TemplateStringInfo, TemplateStringParser};

pub fn init() -> Result<()> {
    tracing::info!("t-linter-core initialized");
    Ok(())
}
