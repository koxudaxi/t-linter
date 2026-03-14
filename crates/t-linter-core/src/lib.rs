use anyhow::Result;

pub mod highlighter;
pub mod lint;
pub mod parser;

pub use highlighter::{HighlightedRange, TemplateHighlighter};
pub use lint::{file_read_error, lint_source, LintDiagnostic, LintFileResult, LintRunSummary, LintSeverity};
pub use parser::{Expression, Location, TemplateStringInfo, TemplateStringParser};

pub fn init() -> Result<()> {
    tracing::info!("t-linter-core initialized");
    Ok(())
}
