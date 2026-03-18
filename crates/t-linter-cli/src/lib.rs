mod discovery;

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Serialize;
use t_linter_core::{
    LintDiagnostic, LintFileResult, LintRunSummary, apply_template_edits, file_read_error,
    format_document, lint_source,
};
use tempfile::NamedTempFile;

use crate::discovery::{DiscoveryFailure, DiscoveryMode, collect_python_files};

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
    Format {
        paths: Vec<String>,

        #[arg(long)]
        check: bool,

        #[arg(long)]
        stdin_filename: Option<String>,
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
    Github,
}

#[derive(Debug, Serialize)]
struct CheckReport {
    files: Vec<LintFileResult>,
    diagnostics: Vec<LintDiagnostic>,
    summary: LintRunSummary,
}

#[derive(Default)]
struct FormatSummary {
    changed: usize,
    unchanged: usize,
    failed: usize,
}

pub fn check(paths: Vec<String>, format: OutputFormat, error_on_issues: bool) -> Result<i32> {
    let walk_report = collect_python_files(&paths, DiscoveryMode::Check)?;
    let mut file_results = walk_report
        .failures
        .iter()
        .map(check_failure_to_result)
        .collect::<Vec<_>>();

    for file in walk_report.python_files {
        match fs::read_to_string(&file.canonical_path) {
            Ok(source) => file_results.push(lint_source(&file.display_path, &source)?),
            Err(_) => file_results.push(file_read_error(&file.display_path)),
        }
    }

    file_results.sort_by(|left, right| left.file.cmp(&right.file));

    let mut diagnostics = file_results
        .iter()
        .flat_map(|result| result.diagnostics.clone())
        .collect::<Vec<_>>();
    diagnostics.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then(left.start_line.cmp(&right.start_line))
            .then(left.start_column.cmp(&right.start_column))
            .then(left.end_line.cmp(&right.end_line))
            .then(left.end_column.cmp(&right.end_column))
            .then(left.rule.cmp(&right.rule))
            .then(left.message.cmp(&right.message))
    });

    let summary = LintRunSummary {
        files_scanned: file_results.len(),
        templates_scanned: file_results
            .iter()
            .map(|result| result.template_count)
            .sum(),
        diagnostics: diagnostics.len(),
        failed_files: file_results
            .iter()
            .filter(|result| has_read_failure(result))
            .count(),
    };

    let report = CheckReport {
        files: file_results,
        diagnostics,
        summary,
    };

    match format {
        OutputFormat::Human => print_human_report(&report),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        OutputFormat::Github => print_github_report(&report),
    }

    if report.summary.failed_files > 0 {
        Ok(2)
    } else if error_on_issues && !report.diagnostics.is_empty() {
        Ok(1)
    } else {
        Ok(0)
    }
}

pub fn format(paths: Vec<String>, check: bool, stdin_filename: Option<String>) -> Result<i32> {
    let paths = if paths.is_empty() {
        vec![".".to_string()]
    } else {
        paths
    };

    let uses_stdin = paths.iter().any(|path| path == "-");
    if uses_stdin {
        if paths.len() != 1 {
            return Err(anyhow::anyhow!("`-` must be the only format path operand"));
        }
        return format_stdin(check, stdin_filename);
    }

    if stdin_filename.is_some() {
        return Err(anyhow::anyhow!(
            "`--stdin-filename` is only supported when formatting stdin"
        ));
    }

    format_files(paths, check)
}

pub fn stats(path: String) -> Result<()> {
    println!("Analyzing statistics for: {}", path);
    Ok(())
}

fn check_failure_to_result(failure: &DiscoveryFailure) -> LintFileResult {
    file_read_error(&failure.display_path)
}

fn has_read_failure(result: &LintFileResult) -> bool {
    result
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.rule == "file-read-error")
}

fn format_stdin(check: bool, stdin_filename: Option<String>) -> Result<i32> {
    let label = stdin_filename.unwrap_or_else(|| "-".to_string());
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .context("Failed to read stdin")?;
    let source =
        String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("stdin is not valid UTF-8"))?;
    let formatted = format_source(&source)?;
    let changed = formatted != source;

    if check {
        if changed {
            eprintln!("Would reformat {}", Path::new(&label).display());
            return Ok(1);
        }
        return Ok(0);
    }

    print!("{formatted}");
    std::io::stdout()
        .flush()
        .context("Failed to flush stdout")?;
    Ok(0)
}

fn format_files(paths: Vec<String>, check: bool) -> Result<i32> {
    let walk_report = collect_python_files(&paths, DiscoveryMode::Format)?;
    let mut summary = FormatSummary::default();

    for failure in walk_report.failures {
        summary.failed += 1;
        print_format_failure(&failure.display_path, &failure.message);
    }

    for file in walk_report.python_files {
        let source = match fs::read(&file.canonical_path) {
            Ok(source) => source,
            Err(error) => {
                summary.failed += 1;
                print_format_failure(&file.display_path, &format!("Failed to read file: {error}"));
                continue;
            }
        };

        let source = match String::from_utf8(source) {
            Ok(source) => source,
            Err(_) => {
                summary.failed += 1;
                print_format_failure(&file.display_path, "File is not valid UTF-8");
                continue;
            }
        };

        let formatted = match format_source(&source) {
            Ok(formatted) => formatted,
            Err(error) => {
                summary.failed += 1;
                print_format_failure(&file.display_path, &error.to_string());
                continue;
            }
        };

        if formatted == source {
            summary.unchanged += 1;
            continue;
        }

        if check {
            summary.changed += 1;
            eprintln!("Would reformat {}", file.display_path.display());
            continue;
        }

        if let Err(error) = write_formatted_file(&file.canonical_path, formatted.as_bytes()) {
            summary.failed += 1;
            print_format_failure(&file.display_path, &error.to_string());
            continue;
        }

        summary.changed += 1;
        eprintln!("Reformatted {}", file.display_path.display());
    }

    print_format_summary(&summary, check);

    if summary.failed > 0 {
        Ok(2)
    } else if check && summary.changed > 0 {
        Ok(1)
    } else {
        Ok(0)
    }
}

fn format_source(source: &str) -> Result<String> {
    let edits = format_document(source)?;
    apply_template_edits(source, &edits)
}

fn write_formatted_file(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        anyhow::anyhow!("Failed to resolve parent directory for {}", path.display())
    })?;
    let metadata = fs::metadata(path)
        .with_context(|| format!("Failed to read metadata for {}", path.display()))?;

    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("Failed to create temporary file in {}", parent.display()))?;

    temp.as_file_mut()
        .write_all(contents)
        .with_context(|| format!("Failed to write temporary file for {}", path.display()))?;
    temp.as_file_mut()
        .sync_all()
        .with_context(|| format!("Failed to flush temporary file for {}", path.display()))?;
    fs::set_permissions(temp.path(), metadata.permissions())
        .with_context(|| format!("Failed to preserve permissions for {}", path.display()))?;

    temp.persist(path).map_err(|error| {
        anyhow::anyhow!(
            "Failed to replace {} with formatted output: {}",
            path.display(),
            error.error
        )
    })?;

    Ok(())
}

fn print_format_failure(path: &Path, message: &str) {
    eprintln!("{}: {}", path.display(), message);
}

fn print_format_summary(summary: &FormatSummary, check: bool) {
    if check {
        eprintln!(
            "{} files would be reformatted, {} files already formatted, {} inputs failed",
            summary.changed, summary.unchanged, summary.failed
        );
    } else {
        eprintln!(
            "{} files reformatted, {} files left unchanged, {} inputs failed",
            summary.changed, summary.unchanged, summary.failed
        );
    }
}

fn print_human_report(report: &CheckReport) {
    for diagnostic in &report.diagnostics {
        let language = diagnostic
            .language
            .as_ref()
            .map(|language| format!(" (language={language})"))
            .unwrap_or_default();

        println!(
            "{}:{}:{}: error[{}] {}{}",
            diagnostic.file.display(),
            diagnostic.start_line,
            diagnostic.start_column,
            diagnostic.rule,
            diagnostic.message,
            language
        );
    }

    println!(
        "{} files scanned, {} templates scanned, {} diagnostics, {} failed files",
        report.summary.files_scanned,
        report.summary.templates_scanned,
        report.summary.diagnostics,
        report.summary.failed_files
    );
}

fn print_github_report(report: &CheckReport) {
    for diagnostic in &report.diagnostics {
        let mut message = diagnostic.message.clone();
        if let Some(language) = &diagnostic.language {
            message.push_str(&format!(" (language={language})"));
        }

        println!(
            "::error file={},line={},col={},title={}::{}",
            escape_github_property(&diagnostic.file.display().to_string()),
            diagnostic.start_line,
            diagnostic.start_column,
            escape_github_property(&format!("t-linter({})", diagnostic.rule)),
            escape_github_message(&message)
        );
    }

    eprintln!(
        "{} files scanned, {} templates scanned, {} diagnostics, {} failed files",
        report.summary.files_scanned,
        report.summary.templates_scanned,
        report.summary.diagnostics,
        report.summary.failed_files
    );
}

fn escape_github_property(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
        .replace(':', "%3A")
        .replace(',', "%2C")
}

fn escape_github_message(message: &str) -> String {
    message
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}
