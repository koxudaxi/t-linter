use anyhow::Result;

pub mod highlighter;
pub mod parser;

pub use highlighter::{HighlightedRange, TemplateHighlighter};
pub use parser::{Expression, Location, TemplateStringInfo, TemplateStringParser};

pub fn init() -> Result<()> {
    tracing::info!("t-linter-core initialized");
    Ok(())
}
