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

fn init_logging(default_filter: &str) {
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                std::env::var("RUST_LOG")
                    .unwrap_or_else(|_| default_filter.to_string()),
            )
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .init();
    });
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let exit_code = match cli.command {
        Some(t_linter_cli::Commands::Lsp { stdio: _ }) => {
            init_logging("info,tower_lsp=warn,t_linter=debug");
            match t_linter_lsp::run_server().await {
                Ok(_) => 0,
                Err(error) => {
                    eprintln!("{error}");
                    1
                }
            }
        }
        Some(t_linter_cli::Commands::Check {
            paths,
            format,
            error_on_issues,
        }) => {
            init_logging("off");
            match t_linter_cli::check(paths, format, error_on_issues) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("{error}");
                    2
                }
            }
        }
        Some(t_linter_cli::Commands::Stats { path }) => {
            init_logging("off");
            match t_linter_cli::stats(path) {
                Ok(_) => 0,
                Err(error) => {
                    eprintln!("{error}");
                    1
                }
            }
        }
        None => {
            init_logging("info,tower_lsp=warn,t_linter=debug");
            match t_linter_lsp::run_server().await {
                Ok(_) => 0,
                Err(error) => {
                    eprintln!("{error}");
                    1
                }
            }
        }
    };

    std::process::exit(exit_code);
}
