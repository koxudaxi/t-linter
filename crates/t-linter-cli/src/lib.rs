use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    Lsp {
        #[arg(long, default_value = "true")]
        stdio: bool,
    },
    Check {
        #[arg(required = true)]
        paths: Vec<String>,

        #[arg(short, long, value_enum, default_value = "human")]
        format: OutputFormat,

        #[arg(long)]
        error_on_issues: bool,
    },
    Stats {
        #[arg(default_value = ".")]
        path: String,
    },
}

#[derive(clap::ValueEnum, Clone, Debug)]
pub enum OutputFormat {
    Human,
    Json,
    Junit,
    Github,
}

pub fn check(paths: Vec<String>, format: OutputFormat, _error_on_issues: bool) -> Result<()> {
    println!("Checking {} paths with format {:?}", paths.len(), format);
    // TODO: Implement actual checking
    Ok(())
}

pub fn stats(path: String) -> Result<()> {
    println!("Analyzing statistics for: {}", path);
    // TODO: Implement statistics
    Ok(())
}