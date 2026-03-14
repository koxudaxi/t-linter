use anyhow::Result;

pub mod format;
pub mod highlighter;
pub mod lint;
pub mod parser;

pub use format::{FormatResult, format_source};
pub use highlighter::{HighlightedRange, TemplateHighlighter};
pub use lint::{
    LintDiagnostic, LintFileResult, LintRunSummary, LintSeverity, file_read_error, lint_source,
};
pub use parser::{Expression, Location, TemplateStringInfo, TemplateStringParser};

pub fn init() -> Result<()> {
    tracing::info!("t-linter-core initialized");
    Ok(())
}
