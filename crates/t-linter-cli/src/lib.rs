mod discovery;
mod sql_prepare;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Serialize;
use t_linter_core::{
    DiagnosticEdit, DiagnosticEditRange, FormatError, FormatOptions as CoreFormatOptions,
    LanguageDetection, LintDiagnostic, LintFileResult, LintRunSummary, LintSeverity,
    TemplateStringParser, apply_diagnostic_edits, apply_template_edits, file_read_error,
    format_document_in_file_with_options, format_document_with_options, lint_source,
    lint_source_with_config, load_project_config_for_path,
};
use tempfile::NamedTempFile;

use crate::discovery::{DiscoveryFailure, DiscoveryMode, collect_python_files};

#[derive(Subcommand)]
pub enum Commands {
    Lsp {
        #[arg(long, default_value = "true")]
        stdio: bool,

        #[arg(long = "ruff-pipeline")]
        ruff_pipeline: bool,

        #[arg(long = "ruff-command")]
        ruff_command: Option<String>,

        #[arg(long = "ruff-arg", action = clap::ArgAction::Append)]
        ruff_args: Vec<String>,
    },
    Check {
        #[arg(required = true)]
        paths: Vec<String>,

        #[arg(short, long, value_enum, default_value = "human")]
        format: OutputFormat,

        #[arg(long)]
        error_on_issues: bool,

        #[arg(long)]
        fix: bool,

        #[arg(long, conflicts_with = "fix")]
        diff: bool,
    },
    Format {
        paths: Vec<String>,

        #[arg(long)]
        check: bool,

        #[arg(long)]
        stdin_filename: Option<String>,

        #[arg(long)]
        line_length: Option<usize>,
    },
    Sql {
        #[command(subcommand)]
        command: SqlCommands,
    },
    Stats {
        #[arg(default_value = ".")]
        paths: Vec<String>,

        #[arg(short, long, value_enum, default_value = "human")]
        format: StatsFormat,
    },
}

#[derive(Subcommand)]
pub enum SqlCommands {
    Prepare {
        paths: Vec<String>,

        #[arg(long)]
        check: bool,
    },
}

#[derive(clap::ValueEnum, Clone, Debug)]
pub enum OutputFormat {
    Human,
    Json,
    Github,
    Sarif,
}

#[derive(clap::ValueEnum, Clone, Debug)]
pub enum StatsFormat {
    Human,
    Json,
}

#[derive(Debug, Serialize)]
struct CheckReport {
    files: Vec<LintFileResult>,
    diagnostics: Vec<LintDiagnostic>,
    summary: LintRunSummary,
}

#[derive(Debug, Serialize)]
struct StatsReport {
    files_scanned: usize,
    failed_files: usize,
    templates_total: usize,
    typed: usize,
    untyped: usize,
    by_language: BTreeMap<String, usize>,
    by_detection: BTreeMap<String, usize>,
    files: Vec<FileStats>,
}

#[derive(Debug, Serialize)]
struct FileStats {
    path: PathBuf,
    template_count: usize,
    by_language: BTreeMap<String, usize>,
}

#[derive(Default)]
struct FormatSummary {
    changed: usize,
    unchanged: usize,
    failed: usize,
}

#[derive(Default)]
struct FixSummary {
    fixed: usize,
    changed_files: usize,
    failed: usize,
}

pub fn check(
    paths: Vec<String>,
    format: OutputFormat,
    error_on_issues: bool,
    fix: bool,
    diff: bool,
) -> Result<i32> {
    let walk_report = collect_python_files(&paths, DiscoveryMode::Check)?;
    let mut file_results = walk_report
        .failures
        .iter()
        .map(check_failure_to_result)
        .collect::<Vec<_>>();
    let mut fix_summary = FixSummary {
        failed: walk_report.failures.len(),
        ..FixSummary::default()
    };
    let mut rendered_diffs = Vec::new();

    for file in walk_report.python_files {
        match fs::read_to_string(&file.canonical_path) {
            Ok(source) => {
                let mut result = if fix || diff {
                    match run_check_fixpoint(&file.canonical_path, &source) {
                        Ok(outcome) => {
                            if outcome.exhausted {
                                eprintln!(
                                    "warning: fix loop did not converge for {} after 10 iterations",
                                    file.display_path.display()
                                );
                            }
                            if outcome.source != source {
                                if diff {
                                    rendered_diffs.push(render_unified_diff(
                                        &file.display_path,
                                        &source,
                                        &outcome.source,
                                    ));
                                }
                                if fix {
                                    match write_formatted_file(
                                        &file.canonical_path,
                                        outcome.source.as_bytes(),
                                    ) {
                                        Ok(()) => {
                                            fix_summary.changed_files += 1;
                                            fix_summary.fixed += outcome.fixed;
                                        }
                                        Err(error) => {
                                            fix_summary.failed += 1;
                                            let mut result = file_read_error(&file.display_path);
                                            if let Some(diagnostic) = result.diagnostics.first_mut()
                                            {
                                                diagnostic.message =
                                                    format!("Failed to write fixed file: {error}");
                                            }
                                            file_results.push(result);
                                            continue;
                                        }
                                    }
                                }
                            }
                            outcome.result
                        }
                        Err(error) => {
                            fix_summary.failed += 1;
                            let mut result = file_read_error(&file.display_path);
                            if let Some(diagnostic) = result.diagnostics.first_mut() {
                                diagnostic.message = format!("Failed to apply fixes: {error}");
                            }
                            file_results.push(result);
                            continue;
                        }
                    }
                } else {
                    lint_source(&file.canonical_path, &source)?
                };
                rewrite_lint_result_path(&mut result, &file.display_path);
                file_results.push(result);
            }
            Err(_) => {
                if fix {
                    fix_summary.failed += 1;
                }
                file_results.push(file_read_error(&file.display_path));
            }
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

    if diff {
        for rendered_diff in rendered_diffs {
            print!("{rendered_diff}");
        }
    } else {
        match format {
            OutputFormat::Human => print_human_report(&report),
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
            OutputFormat::Github => print_github_report(&report),
            OutputFormat::Sarif => print_sarif_report(&report)?,
        }
    }

    if fix {
        eprintln!(
            "Fixed {} issues in {} files, {} inputs failed",
            fix_summary.fixed, fix_summary.changed_files, fix_summary.failed
        );
    }

    if report.summary.failed_files > 0 {
        Ok(2)
    } else if error_on_issues && !report.diagnostics.is_empty() {
        Ok(1)
    } else {
        Ok(0)
    }
}

struct FixOutcome {
    source: String,
    result: LintFileResult,
    fixed: usize,
    exhausted: bool,
}

fn run_check_fixpoint(path: &Path, source: &str) -> Result<FixOutcome> {
    let config = load_project_config_for_path(path)?;
    let mut current = source.to_string();
    let mut fixed = 0;

    for _ in 0..10 {
        let result = lint_source_with_config(path, &current, &config)?;
        let edits = non_overlapping_diagnostic_edits(&result.diagnostics);
        if edits.is_empty() {
            return Ok(FixOutcome {
                source: current,
                result,
                fixed,
                exhausted: false,
            });
        }

        let next = apply_diagnostic_edits(&current, &edits)?;
        if next == current {
            return Ok(FixOutcome {
                source: current,
                result,
                fixed,
                exhausted: false,
            });
        }
        fixed += edits.len();
        current = next;
    }

    let result = lint_source_with_config(path, &current, &config)?;
    Ok(FixOutcome {
        source: current,
        result,
        fixed,
        exhausted: true,
    })
}

fn non_overlapping_diagnostic_edits(diagnostics: &[LintDiagnostic]) -> Vec<DiagnosticEdit> {
    let mut edits = diagnostics
        .iter()
        .flat_map(|diagnostic| diagnostic.suggested_edits.iter().cloned())
        .collect::<Vec<_>>();
    edits.sort_by(|left, right| {
        edit_start(&left.range)
            .cmp(&edit_start(&right.range))
            .then(edit_end(&left.range).cmp(&edit_end(&right.range)))
    });

    let mut selected = Vec::new();
    let mut previous_end = None;
    for edit in edits {
        let start = edit_start(&edit.range);
        if previous_end.is_some_and(|end| start < end) {
            continue;
        }
        previous_end = Some(edit_end(&edit.range));
        selected.push(edit);
    }
    selected
}

fn edit_start(range: &DiagnosticEditRange) -> (usize, usize) {
    (range.start_line, range.start_column)
}

fn edit_end(range: &DiagnosticEditRange) -> (usize, usize) {
    (range.end_line, range.end_column)
}

fn render_unified_diff(path: &Path, old: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }

    let old_lines = old.split_inclusive('\n').collect::<Vec<_>>();
    let new_lines = new.split_inclusive('\n').collect::<Vec<_>>();
    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0;
    while suffix < old_lines.len().saturating_sub(prefix)
        && suffix < new_lines.len().saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_changed = &old_lines[prefix..old_lines.len() - suffix];
    let new_changed = &new_lines[prefix..new_lines.len() - suffix];
    let display_path = path
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    let mut diff = format!(
        "--- a/{display_path}\n+++ b/{display_path}\n@@ -{},{} +{},{} @@\n",
        prefix + 1,
        old_changed.len(),
        prefix + 1,
        new_changed.len()
    );
    for line in old_changed {
        push_diff_line(&mut diff, '-', line);
    }
    for line in new_changed {
        push_diff_line(&mut diff, '+', line);
    }
    diff
}

fn push_diff_line(output: &mut String, prefix: char, line: &str) {
    output.push(prefix);
    output.push_str(line);
    if !line.ends_with('\n') {
        output.push('\n');
    }
}

pub fn format(
    paths: Vec<String>,
    check: bool,
    stdin_filename: Option<String>,
    line_length: Option<usize>,
) -> Result<i32> {
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
        return format_stdin(check, stdin_filename, line_length);
    }

    if stdin_filename.is_some() {
        return Err(anyhow::anyhow!(
            "`--stdin-filename` is only supported when formatting stdin"
        ));
    }

    format_files(paths, check, line_length)
}

pub fn stats(paths: Vec<String>, format: StatsFormat) -> Result<i32> {
    let walk_report = collect_python_files(&paths, DiscoveryMode::Check)?;
    let mut parser = TemplateStringParser::new()?;
    let mut report = StatsReport {
        files_scanned: 0,
        failed_files: walk_report.failures.len(),
        templates_total: 0,
        typed: 0,
        untyped: 0,
        by_language: BTreeMap::new(),
        by_detection: BTreeMap::new(),
        files: Vec::new(),
    };

    for failure in walk_report.failures {
        eprintln!("{}: {}", failure.display_path.display(), failure.message);
    }

    for file in walk_report.python_files {
        let source = match fs::read_to_string(&file.canonical_path) {
            Ok(source) => source,
            Err(error) => {
                report.failed_files += 1;
                eprintln!(
                    "{}: Failed to read file: {error}",
                    file.display_path.display()
                );
                continue;
            }
        };

        let templates = match parser.find_template_strings_in_file(&source, &file.canonical_path) {
            Ok(templates) => templates,
            Err(error) => {
                report.failed_files += 1;
                eprintln!(
                    "{}: Failed to parse file: {error}",
                    file.display_path.display()
                );
                continue;
            }
        };

        let template_count = templates.len();
        report.files_scanned += 1;
        report.templates_total += template_count;
        let mut by_language = BTreeMap::new();

        for template in templates {
            match template.language {
                Some(language) => {
                    report.typed += 1;
                    *report.by_language.entry(language.clone()).or_default() += 1;
                    *by_language.entry(language).or_default() += 1;
                }
                None => report.untyped += 1,
            }
            if let Some(detection) = template.language_detection {
                *report
                    .by_detection
                    .entry(language_detection_label(detection).to_string())
                    .or_default() += 1;
            }
        }

        report.files.push(FileStats {
            path: file.display_path,
            template_count,
            by_language,
        });
    }

    report.files.sort_by(|left, right| {
        right
            .template_count
            .cmp(&left.template_count)
            .then(left.path.cmp(&right.path))
    });

    match format {
        StatsFormat::Human => print_stats_report(&report),
        StatsFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
    }

    if report.failed_files > 0 {
        Ok(2)
    } else {
        Ok(0)
    }
}

fn language_detection_label(detection: LanguageDetection) -> &'static str {
    match detection {
        LanguageDetection::Annotation => "annotation",
        LanguageDetection::CalleeInference => "callee-inference",
        LanguageDetection::ReturnAnnotation => "return-annotation",
        LanguageDetection::VariableHint => "variable-hint",
    }
}

fn print_stats_report(report: &StatsReport) {
    println!("Files scanned:        {}", report.files_scanned);
    println!("Template strings:     {}", report.templates_total);
    let typed_percentage = if report.templates_total == 0 {
        0.0
    } else {
        (report.typed as f64 / report.templates_total as f64) * 100.0
    };
    println!(
        "  typed:              {} ({typed_percentage:.1}%)",
        report.typed
    );
    println!("  untyped:            {}", report.untyped);

    println!("\nBy language:");
    for (language, count) in &report.by_language {
        println!("  {language:<18}{count}");
    }

    println!("\nBy detection method:");
    for (detection, count) in &report.by_detection {
        println!("  {detection:<18}{count}");
    }

    println!("\nTop files by template count:");
    for file in report.files.iter().take(10) {
        println!("  {:<18}{}", file.path.display(), file.template_count);
    }
}

pub fn sql_prepare(paths: Vec<String>, check: bool) -> Result<i32> {
    sql_prepare::prepare(paths, check)
}

fn check_failure_to_result(failure: &DiscoveryFailure) -> LintFileResult {
    let mut result = file_read_error(&failure.display_path);
    if let Some(diagnostic) = result.diagnostics.first_mut() {
        diagnostic.message = failure.message.clone();
    }
    result
}

fn has_read_failure(result: &LintFileResult) -> bool {
    result
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.rule == "file-read-error")
}

fn rewrite_lint_result_path(result: &mut LintFileResult, display_path: &Path) {
    result.file = display_path.to_path_buf();
    for diagnostic in &mut result.diagnostics {
        diagnostic.file = display_path.to_path_buf();
    }
}

fn format_stdin(
    check: bool,
    stdin_filename: Option<String>,
    cli_line_length: Option<usize>,
) -> Result<i32> {
    let current_dir = std::env::current_dir().context("Failed to resolve current directory")?;
    let stdin_path = stdin_filename
        .as_ref()
        .map(|path| resolve_input_path(&current_dir, path));
    let option_source = stdin_path
        .as_deref()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| current_dir.clone());
    let label = stdin_filename.unwrap_or_else(|| "-".to_string());
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .context("Failed to read stdin")?;
    let source =
        String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("stdin is not valid UTF-8"))?;
    let options = resolve_format_options(cli_line_length, &option_source)?;
    let formatted = format_source(&source, stdin_path.as_deref(), options)
        .map_err(|error| anyhow::anyhow!("{}", render_format_failure(Path::new(&label), &error)))?;
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

fn format_files(paths: Vec<String>, check: bool, cli_line_length: Option<usize>) -> Result<i32> {
    let walk_report = collect_python_files(&paths, DiscoveryMode::Format)?;
    let mut summary = FormatSummary::default();

    for failure in walk_report.failures {
        summary.failed += 1;
        print_format_failure(&failure.display_path, &anyhow::anyhow!(failure.message));
    }

    for file in walk_report.python_files {
        let source = match fs::read(&file.canonical_path) {
            Ok(source) => source,
            Err(error) => {
                summary.failed += 1;
                print_format_failure(
                    &file.display_path,
                    &anyhow::anyhow!("Failed to read file: {error}"),
                );
                continue;
            }
        };

        let source = match String::from_utf8(source) {
            Ok(source) => source,
            Err(_) => {
                summary.failed += 1;
                print_format_failure(
                    &file.display_path,
                    &anyhow::anyhow!("File is not valid UTF-8"),
                );
                continue;
            }
        };

        let options = match resolve_format_options(cli_line_length, &file.canonical_path) {
            Ok(options) => options,
            Err(error) => {
                summary.failed += 1;
                print_format_failure(&file.display_path, &error);
                continue;
            }
        };

        let formatted = match format_source(&source, Some(&file.canonical_path), options) {
            Ok(formatted) => formatted,
            Err(error) => {
                summary.failed += 1;
                print_format_failure(&file.display_path, &error);
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
            print_format_failure(&file.display_path, &error);
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

fn format_source(source: &str, path: Option<&Path>, options: CoreFormatOptions) -> Result<String> {
    let edits = match path {
        Some(path) => format_document_in_file_with_options(source, path, &options)?,
        None => format_document_with_options(source, &options)?,
    };
    apply_template_edits(source, &edits)
}

fn resolve_format_options(
    cli_line_length: Option<usize>,
    path: &Path,
) -> Result<CoreFormatOptions> {
    let config = load_project_config_for_path(path)?;
    Ok(CoreFormatOptions {
        line_length: cli_line_length.or(config.line_length).unwrap_or(80).max(1),
    })
}

fn resolve_input_path(current_dir: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        current_dir.join(path)
    }
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

fn print_format_failure(path: &Path, error: &anyhow::Error) {
    eprintln!("{}", render_format_failure(path, error));
}

fn render_format_failure(path: &Path, error: &anyhow::Error) -> String {
    if let Some(format_error) = error.downcast_ref::<FormatError>() {
        let language = format_error
            .language
            .as_ref()
            .map(|language| format!(" (language={language})"))
            .unwrap_or_default();

        if let Some(location) = &format_error.location {
            return format!(
                "error: Failed to format {}:{}:{}: {}{}",
                path.display(),
                location.start_line,
                location.start_column,
                format_error.message,
                language
            );
        }

        return format!(
            "error: Failed to format {}: {}{}",
            path.display(),
            format_error.message,
            language
        );
    }

    format!("{}: {}", path.display(), error)
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
            "{}:{}:{}: {}[{}] {}{}",
            diagnostic.file.display(),
            diagnostic.start_line,
            diagnostic.start_column,
            severity_label(diagnostic.severity),
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
        let annotation = severity_label(diagnostic.severity);

        println!(
            "::{annotation} file={},line={},col={},title={}::{}",
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

fn print_sarif_report(report: &CheckReport) -> Result<()> {
    let rules = report
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.rule.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|rule| {
            serde_json::json!({
                "id": rule,
                "name": rule,
            })
        })
        .collect::<Vec<_>>();

    let results = report
        .diagnostics
        .iter()
        .map(|diagnostic| {
            let mut message = diagnostic.message.clone();
            if let Some(language) = &diagnostic.language {
                message.push_str(&format!(" (language={language})"));
            }

            let mut properties = serde_json::Map::new();
            if let Some(language) = &diagnostic.language {
                properties.insert("language".to_string(), serde_json::json!(language));
            }
            if let Some(expected_type) = &diagnostic.expected_type {
                properties.insert("expectedType".to_string(), serde_json::json!(expected_type));
            }
            if let Some(found_type) = &diagnostic.found_type {
                properties.insert("foundType".to_string(), serde_json::json!(found_type));
            }
            if let Some(schema_pointer) = &diagnostic.schema_pointer {
                properties.insert(
                    "schemaPointer".to_string(),
                    serde_json::json!(schema_pointer),
                );
            }
            if let Some(source_of_truth) = &diagnostic.source_of_truth {
                properties.insert(
                    "sourceOfTruth".to_string(),
                    serde_json::json!(source_of_truth),
                );
            }
            if !diagnostic.suggested_edits.is_empty() {
                properties.insert(
                    "suggestedEdits".to_string(),
                    serde_json::json!(diagnostic.suggested_edits),
                );
            }

            serde_json::json!({
                "ruleId": diagnostic.rule,
                "level": severity_label(diagnostic.severity),
                "message": { "text": message },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": {
                            "uri": sarif_path(&diagnostic.file),
                        },
                        "region": {
                            "startLine": diagnostic.start_line,
                            "startColumn": diagnostic.start_column,
                            "endLine": diagnostic.end_line,
                            "endColumn": diagnostic.end_column,
                        }
                    }
                }],
                "properties": properties,
            })
        })
        .collect::<Vec<_>>();

    let sarif = serde_json::json!({
        "version": "2.1.0",
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "t-linter",
                    "informationUri": "https://github.com/koxudaxi/t-linter",
                    "version": env!("CARGO_PKG_VERSION"),
                    "rules": rules,
                }
            },
            "results": results,
        }]
    });

    println!("{}", serde_json::to_string_pretty(&sarif)?);
    Ok(())
}

fn sarif_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
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

fn severity_label(severity: LintSeverity) -> &'static str {
    match severity {
        LintSeverity::Error => "error",
        LintSeverity::Warning => "warning",
    }
}
