use anyhow::Result;

pub mod parser;
pub mod highlighter;

pub use parser::{TemplateStringParser, TemplateStringInfo, Location, Expression};
pub use highlighter::{TemplateHighlighter, HighlightedRange};

pub fn init() -> Result<()> {
    tracing::info!("t-linter-core initialized");
    Ok(())
}