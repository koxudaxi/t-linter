use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Serialize;
use t_linter_core::{
    LintDiagnostic, LintFileResult, LintRunSummary, file_read_error, format_source, lint_source,
};

const DEFAULT_EXCLUDES: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
];

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
        #[arg(required = true)]
        paths: Vec<String>,

        #[arg(long)]
        check: bool,
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
struct WalkReport {
    python_files: Vec<PathBuf>,
    failures: Vec<PathBuf>,
}

#[derive(Default)]
struct FormatSummary {
    reformatted: usize,
    unchanged: usize,
    failed: usize,
}

#[derive(Debug, Default, serde::Deserialize)]
struct PyprojectToml {
    tool: Option<ToolSection>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ToolSection {
    #[serde(rename = "t-linter")]
    t_linter: Option<TLinterConfig>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct TLinterConfig {
    exclude: Option<Vec<String>>,
    #[serde(rename = "extend-exclude")]
    extend_exclude: Option<Vec<String>>,
    #[serde(rename = "ignore-file")]
    ignore_file: Option<String>,
}

#[derive(Debug)]
struct DiscoveryConfig {
    root: PathBuf,
    matcher: Gitignore,
}

pub fn check(paths: Vec<String>, format: OutputFormat, error_on_issues: bool) -> Result<i32> {
    let walk_report = collect_python_files(&paths)?;
    let mut file_results = walk_report
        .failures
        .into_iter()
        .map(|path| file_read_error(&path))
        .collect::<Vec<_>>();

    for path in walk_report.python_files {
        match fs::read_to_string(&path) {
            Ok(source) => file_results.push(lint_source(&path, &source)?),
            Err(_) => file_results.push(file_read_error(&path)),
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
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
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

pub fn stats(path: String) -> Result<()> {
    println!("Analyzing statistics for: {}", path);
    Ok(())
}

pub fn format(paths: Vec<String>, check: bool) -> Result<i32> {
    let discovery = load_discovery_config(&std::env::current_dir()?)?;
    let walk_report = collect_python_files_with_discovery(&paths, &discovery);
    let mut summary = FormatSummary::default();

    for failure in walk_report.failures {
        eprintln!("Failed to read {}", failure.display());
        summary.failed += 1;
    }

    for path in walk_report.python_files {
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(_) => {
                eprintln!("Failed to read {}", path.display());
                summary.failed += 1;
                continue;
            }
        };

        match format_source(&source, &discovery.root) {
            Ok(result) => {
                if result.changed {
                    if check {
                        println!("Would reformat: {}", path.display());
                    } else {
                        fs::write(&path, result.formatted_source)
                            .with_context(|| format!("Failed to write {}", path.display()))?;
                    }
                    summary.reformatted += 1;
                } else {
                    summary.unchanged += 1;
                }
            }
            Err(error) => {
                eprintln!("Failed to format {}: {}", path.display(), error);
                summary.failed += 1;
            }
        }
    }

    if check {
        println!(
            "{} file{} would be reformatted, {} file{} already formatted",
            summary.reformatted,
            plural_suffix(summary.reformatted),
            summary.unchanged,
            plural_suffix(summary.unchanged),
        );
    } else {
        println!(
            "{} file{} reformatted, {} file{} left unchanged",
            summary.reformatted,
            plural_suffix(summary.reformatted),
            summary.unchanged,
            plural_suffix(summary.unchanged),
        );
    }

    if summary.failed > 0 {
        eprintln!(
            "{} file{} failed to format",
            summary.failed,
            plural_suffix(summary.failed),
        );
        Ok(2)
    } else if check && summary.reformatted > 0 {
        Ok(1)
    } else {
        Ok(0)
    }
}

fn collect_python_files(paths: &[String]) -> Result<WalkReport> {
    let discovery = load_discovery_config(&std::env::current_dir()?)?;
    Ok(collect_python_files_with_discovery(paths, &discovery))
}

fn collect_python_files_with_discovery(
    paths: &[String],
    discovery: &DiscoveryConfig,
) -> WalkReport {
    let mut report = WalkReport::default();
    for path in paths {
        collect_path(Path::new(path), &mut report, &discovery);
    }

    report.python_files.sort();
    report.python_files.dedup();
    report.failures.sort();
    report.failures.dedup();
    report
}

fn collect_path(path: &Path, report: &mut WalkReport, discovery: &DiscoveryConfig) {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => {
            report.failures.push(path.to_path_buf());
            return;
        }
    };

    if metadata.file_type().is_symlink() {
        return;
    }

    if should_ignore_path(path, metadata.is_dir(), discovery) {
        return;
    }

    if metadata.is_dir() {
        let mut entries = match fs::read_dir(path) {
            Ok(entries) => entries.filter_map(Result::ok).collect::<Vec<_>>(),
            Err(_) => {
                report.failures.push(path.to_path_buf());
                return;
            }
        };
        entries.sort_by_key(|entry| entry.path());

        for entry in entries {
            collect_path(&entry.path(), report, discovery);
        }
        return;
    }

    if metadata.is_file() && is_python_file(path) {
        report.python_files.push(path.to_path_buf());
    }
}

fn has_read_failure(result: &LintFileResult) -> bool {
    result
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.rule == "file-read-error")
}

fn load_discovery_config(start_dir: &Path) -> Result<DiscoveryConfig> {
    let root = find_config_root(start_dir);
    let pyproject_path = root.join("pyproject.toml");
    let pyproject = if pyproject_path.is_file() {
        let content = fs::read_to_string(&pyproject_path)
            .with_context(|| format!("Failed to read {}", pyproject_path.display()))?;
        toml::from_str::<PyprojectToml>(&content)
            .with_context(|| format!("Failed to parse {}", pyproject_path.display()))?
    } else {
        PyprojectToml::default()
    };

    let config = pyproject
        .tool
        .and_then(|tool| tool.t_linter)
        .unwrap_or_default();

    let mut builder = GitignoreBuilder::new(&root);

    let base_excludes = config.exclude.clone().unwrap_or_else(|| {
        DEFAULT_EXCLUDES
            .iter()
            .map(|value| (*value).to_string())
            .collect()
    });
    for pattern in base_excludes {
        builder
            .add_line(None, &pattern)
            .with_context(|| format!("Invalid exclude pattern: {pattern}"))?;
    }

    for pattern in config.extend_exclude.unwrap_or_default() {
        builder
            .add_line(None, &pattern)
            .with_context(|| format!("Invalid extend-exclude pattern: {pattern}"))?;
    }

    let ignore_file = config
        .ignore_file
        .map(|path| root.join(path))
        .unwrap_or_else(|| root.join(".t-linterignore"));
    if ignore_file.is_file() {
        builder.add(ignore_file);
    }

    let matcher = builder.build()?;
    Ok(DiscoveryConfig { root, matcher })
}

fn find_config_root(start_dir: &Path) -> PathBuf {
    for dir in start_dir.ancestors() {
        if dir.join("pyproject.toml").is_file() || dir.join(".t-linterignore").is_file() {
            return dir.to_path_buf();
        }
    }
    start_dir.to_path_buf()
}

fn should_ignore_path(path: &Path, is_dir: bool, discovery: &DiscoveryConfig) -> bool {
    let relative = path.strip_prefix(&discovery.root).unwrap_or(path);
    if relative.as_os_str().is_empty() {
        return false;
    }

    discovery.matcher.matched(relative, is_dir).is_ignore()
}

fn is_python_file(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|extension| extension.eq_ignore_ascii_case("py"))
        .unwrap_or(false)
}

fn plural_suffix(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
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
