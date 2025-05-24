use anyhow::Result;
use clap::Parser;
use std::sync::Once;

static INIT: Once = Once::new();

#[derive(Parser)]
#[command(name = "t-linter")]
#[command(author = "Koudai Aono <koxudaxi@gmail.com>")]
#[command(version = "0.1.0")]
#[command(about = "Python template string linter for PEP 750", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<t_linter_cli::Commands>,
}

fn init_logging() {
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                std::env::var("RUST_LOG")
                    .unwrap_or_else(|_| "info,tower_lsp=warn,t_linter=debug".to_string()),
            )
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .init();
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(t_linter_cli::Commands::Lsp { stdio: _ }) => {
            init_logging();
            t_linter_lsp::run_server().await?;
            
        }
        Some(t_linter_cli::Commands::Check { paths, format, error_on_issues }) => {
            init_logging();
            t_linter_cli::check(paths, format, error_on_issues)?;
        }
        Some(t_linter_cli::Commands::Stats { path }) => {
            init_logging();
            t_linter_cli::stats(path)?;
        }
        None => {
            init_logging();
            t_linter_lsp::run_server().await?;
        }
    }

    Ok(())
}