use clap::Parser;
use std::sync::Once;

static INIT: Once = Once::new();

#[derive(Parser)]
#[command(name = "t-linter")]
#[command(author = "Koudai Aono <koxudaxi@gmail.com>")]
#[command(version)]
#[command(about = "Python template string linter for PEP 750", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<t_linter_cli::Commands>,
}

fn init_logging(default_filter: &str) {
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                std::env::var("RUST_LOG").unwrap_or_else(|_| default_filter.to_string()),
            )
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .init();
    });
}

fn lsp_config(
    ruff_pipeline: bool,
    ruff_command: Option<String>,
    ruff_args: Vec<String>,
) -> t_linter_lsp::TLinterConfig {
    let mut config = t_linter_lsp::TLinterConfig::default();
    if ruff_pipeline {
        config.ruff_pipeline = t_linter_lsp::RuffPipelineConfig {
            enabled: true,
            command: ruff_command,
            args: if ruff_args.is_empty() {
                t_linter_lsp::RuffPipelineConfig::default().args
            } else {
                ruff_args
            },
            settings: serde_json::Value::Object(Default::default()),
        };
    }
    config
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let exit_code = match cli.command {
        Some(t_linter_cli::Commands::Lsp {
            stdio: _,
            ruff_pipeline,
            ruff_command,
            ruff_args,
        }) => {
            init_logging("info,tower_lsp=warn,t_linter=debug");
            match t_linter_lsp::run_server_with_config(lsp_config(
                ruff_pipeline,
                ruff_command,
                ruff_args,
            ))
            .await
            {
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
        Some(t_linter_cli::Commands::Format {
            paths,
            check,
            stdin_filename,
            line_length,
        }) => {
            init_logging("off");
            match t_linter_cli::format(paths, check, stdin_filename, line_length) {
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
