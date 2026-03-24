use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{self as unix_fs, PermissionsExt};

fn test_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "t-linter-format-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn run_t_linter(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
    run_t_linter_with_pythonpath(dir, args, stdin, None)
}

fn run_t_linter_with_pythonpath(
    dir: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    pythonpath: Option<&Path>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_t-linter"));
    command.args(args).current_dir(dir);
    if let Some(pythonpath) = pythonpath {
        command.env("PYTHONPATH", pythonpath);
    }

    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }

    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(stdin_bytes) = stdin {
        use std::io::Write;

        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(stdin_bytes)
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn assert_ruff_style_format_failure(
    stderr: &str,
    path_label: &str,
    line: usize,
    column: usize,
    language: &str,
    message: &str,
) {
    assert!(stderr.contains(&format!(
        "error: Failed to format {path_label}:{line}:{column}: {message} (language={language})"
    )));
}

fn assert_format_round_trip_is_stable(
    source: &str,
    expected_fragments: &[&str],
    unexpected_fragments: &[&str],
) {
    assert_format_round_trip_is_stable_with_first_pass(source, expected_fragments, unexpected_fragments, true);
}

fn assert_format_round_trip_is_stable_allowing_noop_first_pass(
    source: &str,
    expected_fragments: &[&str],
    unexpected_fragments: &[&str],
) {
    assert_format_round_trip_is_stable_with_first_pass(source, expected_fragments, unexpected_fragments, false);
}

fn assert_format_round_trip_is_stable_with_first_pass(
    source: &str,
    expected_fragments: &[&str],
    unexpected_fragments: &[&str],
    require_first_reformat: bool,
) {
    let dir = test_dir("format-round-trip-stable");
    let path = dir.join("example.py");
    write_file(&path, source);

    let first = run_t_linter(&dir, &["format", "example.py"], None);
    let second = run_t_linter(&dir, &["format", "example.py"], None);
    let check = run_t_linter(&dir, &["format", "--check", "example.py"], None);

    let first_stderr = String::from_utf8(first.stderr).unwrap();
    let second_stderr = String::from_utf8(second.stderr).unwrap();
    let check_stdout = String::from_utf8(check.stdout).unwrap();
    let check_stderr = String::from_utf8(check.stderr).unwrap();
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(first.status.code(), Some(0));
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(check.status.code(), Some(0));
    if require_first_reformat {
        assert!(first_stderr.contains("Reformatted example.py"));
    } else {
        assert!(
            first_stderr.contains("Reformatted example.py")
                || first_stderr
                    .contains("0 files reformatted, 1 files left unchanged, 0 inputs failed")
        );
    }
    assert!(second_stderr.contains("0 files reformatted, 1 files left unchanged, 0 inputs failed"));
    assert!(check_stdout.is_empty());
    assert!(
        check_stderr
            .contains("0 files would be reformatted, 1 files already formatted, 0 inputs failed")
    );
    for fragment in expected_fragments {
        assert!(content.contains(fragment));
    }
    for fragment in unexpected_fragments {
        assert!(!content.contains(fragment));
    }

    let _ = fs::remove_dir_all(dir);
}

fn assert_stdin_format_round_trip_is_stable(
    source: &str,
    stdin_filename: Option<&str>,
    expected_fragments: &[&str],
    unexpected_fragments: &[&str],
) {
    let dir = test_dir("format-stdin-round-trip-stable");

    let mut format_args = vec!["format"];
    if let Some(stdin_filename) = stdin_filename {
        format_args.extend(["--stdin-filename", stdin_filename]);
    }
    format_args.push("-");

    let first = run_t_linter(
        &dir,
        &format_args,
        Some(source.as_bytes()),
    );
    let first_stdout = String::from_utf8(first.stdout).unwrap();
    let first_stderr = String::from_utf8(first.stderr).unwrap();

    let second = run_t_linter(
        &dir,
        &format_args,
        Some(first_stdout.as_bytes()),
    );
    let second_stdout = String::from_utf8(second.stdout).unwrap();
    let second_stderr = String::from_utf8(second.stderr).unwrap();

    let mut check_args = vec!["format", "--check"];
    if let Some(stdin_filename) = stdin_filename {
        check_args.extend(["--stdin-filename", stdin_filename]);
    }
    check_args.push("-");

    let check = run_t_linter(
        &dir,
        &check_args,
        Some(second_stdout.as_bytes()),
    );
    let check_stdout = String::from_utf8(check.stdout).unwrap();
    let check_stderr = String::from_utf8(check.stderr).unwrap();

    assert_eq!(first.status.code(), Some(0));
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(check.status.code(), Some(0));
    assert!(first_stderr.is_empty());
    assert!(second_stderr.is_empty());
    assert!(check_stdout.is_empty());
    assert!(check_stderr.is_empty());
    assert_eq!(first_stdout, second_stdout);
    for fragment in expected_fragments {
        assert!(second_stdout.contains(fragment));
    }
    for fragment in unexpected_fragments {
        assert!(!second_stdout.contains(fragment));
    }

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_rewrites_file_in_place() {
    let dir = test_dir("in-place");
    let path = dir.join("example.py");
    write_file(
        &path,
        r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "toml"] = t'title={title}'
"#,
    );

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let stderr = String::from_utf8(output.stderr).unwrap();
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(stderr.contains("Reformatted example.py"));
    assert!(stderr.contains("1 files reformatted, 0 files left unchanged, 0 inputs failed"));
    assert!(content.contains(r#"t'title = {title}'"#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_check_detects_changes_without_rewriting() {
    let dir = test_dir("check");
    let path = dir.join("example.py");
    let original = r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "toml"] = t'title={title}'
"#;
    write_file(&path, original);

    let output = run_t_linter(&dir, &["format", "--check", "example.py"], None);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("Would reformat example.py"));
    assert_eq!(fs::read_to_string(&path).unwrap(), original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_stdin_outputs_formatted_source() {
    let dir = test_dir("stdin");
    let input = br#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "toml"] = t'title={title}'
"#;

    let output = run_t_linter(&dir, &["format", "-"], Some(input));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains(r#"t'title = {title}'"#));
    assert!(stderr.is_empty());

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_stdin_check_uses_stdin_filename_label() {
    let dir = test_dir("stdin-check");
    let input = br#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "toml"] = t'title={title}'
"#;

    let output = run_t_linter(
        &dir,
        &["format", "--check", "--stdin-filename", "sample.py", "-"],
        Some(input),
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("Would reformat sample.py"));
    assert!(stdout.is_empty());

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_uses_pyproject_line_length_when_cli_override_is_missing() {
    let dir = test_dir("pyproject-line-length");
    let path = dir.join("example.py");
    write_file(
        &dir.join("pyproject.toml"),
        "[tool.t-linter]\nline-length = 20\n",
    );
    write_file(
        &path,
        r#"from typing import Annotated
from string.templatelib import Template

markup: Annotated[Template, "html"] = t'<div data-a="12345" data-b="67890"></div>'
"#,
    );

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains("<div\n  data-a=\"12345\"\n  data-b=\"67890\"\n></div>"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_cli_line_length_overrides_pyproject() {
    let dir = test_dir("cli-line-length");
    let path = dir.join("example.py");
    write_file(
        &dir.join("pyproject.toml"),
        "[tool.t-linter]\nline-length = 120\n",
    );
    write_file(
        &path,
        r#"from typing import Annotated
from string.templatelib import Template

markup: Annotated[Template, "html"] = t'<div data-a="12345" data-b="67890"></div>'
"#,
    );

    let output = run_t_linter(&dir, &["format", "--line-length", "20", "example.py"], None);
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains("<div\n  data-a=\"12345\"\n  data-b=\"67890\"\n></div>"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_cli_line_length_ignores_invalid_pyproject_line_length() {
    let dir = test_dir("invalid-pyproject-line-length");
    let path = dir.join("example.py");
    write_file(
        &dir.join("pyproject.toml"),
        "[tool.t-linter]\nline-length = \"bad\"\n",
    );
    write_file(
        &path,
        r#"from typing import Annotated
from string.templatelib import Template

markup: Annotated[Template, "html"] = t'<div data-a="12345" data-b="67890"></div>'
"#,
    );

    let output = run_t_linter(&dir, &["format", "--line-length", "20", "example.py"], None);
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains("<div\n  data-a=\"12345\"\n  data-b=\"67890\"\n></div>"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_uses_default_line_length_without_config() {
    let dir = test_dir("default-line-length");
    let path = dir.join("example.py");
    write_file(
        &path,
        r#"from typing import Annotated
from string.templatelib import Template

markup: Annotated[Template, "html"] = t'<div data-a="12345" data-b="67890"></div>'
"#,
    );

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t'<div data-a="12345" data-b="67890"></div>'"#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_stdin_uses_stdin_filename_to_resolve_config_root() {
    let dir = test_dir("stdin-line-length");
    let project = dir.join("project");
    let runner = dir.join("runner");
    fs::create_dir_all(&runner).unwrap();
    write_file(
        &project.join("pyproject.toml"),
        "[tool.t-linter]\nline-length = 20\n",
    );

    let input = br#"from typing import Annotated
from string.templatelib import Template

markup: Annotated[Template, "html"] = t'<div data-a="12345" data-b="67890"></div>'
"#;

    let output = run_t_linter(
        &runner,
        &[
            "format",
            "--stdin-filename",
            project.join("example.py").to_str().unwrap(),
            "-",
        ],
        Some(input),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("<div\n  data-a=\"12345\"\n  data-b=\"67890\"\n></div>"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_rejects_invalid_stdin_combinations() {
    let dir = test_dir("stdin-invalid");

    let output = run_t_linter(&dir, &["format", "-", "example.py"], Some(b""));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr.contains("`-` must be the only format path operand"));

    let output = run_t_linter(
        &dir,
        &["format", "--stdin-filename", "x.py", "example.py"],
        None,
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr.contains("`--stdin-filename` is only supported when formatting stdin"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_preserves_multibyte_prefix_and_crlf_bytes() {
    let dir = test_dir("bytes");
    let path = dir.join("example.py");
    let input = "from typing import Annotated\r\nfrom string.templatelib import Template\r\n\r\ntitle = \"こんにちは\"\r\npayload: Annotated[Template, \"toml\"] = t'title={title}'\r\n";
    fs::write(&path, input.as_bytes()).unwrap();

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let bytes = fs::read(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        bytes,
        b"from typing import Annotated\r\nfrom string.templatelib import Template\r\n\r\ntitle = \"\xe3\x81\x93\xe3\x82\x93\xe3\x81\xab\xe3\x81\xa1\xe3\x81\xaf\"\r\npayload: Annotated[Template, \"toml\"] = t'title = {title}'\r\n"
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_rewrites_multiple_templates_in_reverse_order_safely() {
    let dir = test_dir("multiple");
    let path = dir.join("example.py");
    write_file(
        &path,
        r#"from typing import Annotated
from string.templatelib import Template

one: Annotated[Template, "toml"] = t'title={title}'
two: Annotated[Template, "toml"] = t'name={name}'
"#,
    );

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"one: Annotated[Template, "toml"] = t'title = {title}'"#));
    assert!(content.contains(r#"two: Annotated[Template, "toml"] = t'name = {name}'"#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_leaves_file_untouched_when_formatter_fails() {
    let dir = test_dir("formatter-error");
    let path = dir.join("example.py");
    let original = r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "html"] = t"<div><"
"#;
    write_file(&path, original);

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_ruff_style_format_failure(
        &stderr,
        "example.py",
        4,
        40,
        "html",
        "Expected a tag or attribute name.",
    );
    assert!(stderr.contains("0 files reformatted, 0 files left unchanged, 1 inputs failed"));
    assert_eq!(fs::read_to_string(&path).unwrap(), original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_check_reports_ruff_style_failure_details() {
    let dir = test_dir("formatter-error-check");
    let path = dir.join("example.py");
    write_file(
        &path,
        r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "html"] = t"<div><"
"#,
    );

    let output = run_t_linter(&dir, &["format", "--check", "example.py"], None);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_ruff_style_format_failure(
        &stderr,
        "example.py",
        4,
        40,
        "html",
        "Expected a tag or attribute name.",
    );
    assert!(
        stderr.contains("0 files would be reformatted, 0 files already formatted, 1 inputs failed")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_stdin_reports_ruff_style_failure_details() {
    let dir = test_dir("formatter-error-stdin");
    let input = br#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "html"] = t"<div><"
"#;

    let output = run_t_linter(
        &dir,
        &["format", "--stdin-filename", "sample.py", "-"],
        Some(input),
    );
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_ruff_style_format_failure(
        &stderr,
        "sample.py",
        4,
        40,
        "html",
        "Expected a tag or attribute name.",
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_reports_ruff_style_failure_details_for_thtml() {
    let dir = test_dir("formatter-error-thtml");
    let path = dir.join("example.py");
    let original = r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "thtml"] = t"<Card><"
"#;
    write_file(&path, original);

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_ruff_style_format_failure(
        &stderr,
        "example.py",
        4,
        41,
        "thtml",
        "Expected a tag or attribute name.",
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_reformats_valid_files_even_when_other_inputs_fail() {
    let dir = test_dir("formatter-error-mixed");
    let good = dir.join("good.py");
    let bad = dir.join("bad.py");
    write_file(
        &good,
        r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "toml"] = t'title={title}'
"#,
    );
    let bad_original = r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "html"] = t"<div><"
"#;
    write_file(&bad, bad_original);

    let output = run_t_linter(&dir, &["format", "good.py", "bad.py"], None);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(stderr.contains("Reformatted good.py"));
    assert_ruff_style_format_failure(
        &stderr,
        "bad.py",
        4,
        40,
        "html",
        "Expected a tag or attribute name.",
    );
    assert!(stderr.contains("1 files reformatted, 0 files left unchanged, 1 inputs failed"));
    assert!(
        fs::read_to_string(&good)
            .unwrap()
            .contains(r#"t'title = {title}'"#)
    );
    assert_eq!(fs::read_to_string(&bad).unwrap(), bad_original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_check_reports_failures_alongside_reformat_candidates() {
    let dir = test_dir("formatter-error-check-mixed");
    let good = dir.join("good.py");
    let bad = dir.join("bad.py");
    write_file(
        &good,
        r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "toml"] = t'title={title}'
"#,
    );
    write_file(
        &bad,
        r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "html"] = t"<div><"
"#,
    );

    let output = run_t_linter(&dir, &["format", "--check", "good.py", "bad.py"], None);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(stderr.contains("Would reformat good.py"));
    assert_ruff_style_format_failure(
        &stderr,
        "bad.py",
        4,
        40,
        "html",
        "Expected a tag or attribute name.",
    );
    assert!(
        stderr.contains("1 files would be reformatted, 0 files already formatted, 1 inputs failed")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_normalizes_yml_failure_language_to_yaml() {
    let dir = test_dir("formatter-error-yml");
    let path = dir.join("example.py");
    let original = r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "yml"] = t"key: [1,,2]"
"#;
    write_file(&path, original);

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_ruff_style_format_failure(
        &stderr,
        "example.py",
        4,
        49,
        "yaml",
        "Expected a YAML value.",
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_keeps_triple_quoted_html_templates_idempotent() {
    assert_format_round_trip_is_stable(
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> str:
    return ""

title = "Title"
heading = "Heading"
content = "Content"

render_html(t"""
<!DOCTYPE html>
<html>
  <head><title>{title}</title></head>
  <body><h1 style="color: #007acc">{heading}</h1>
  <p>{content}</p></body>
</html>
""")
"#,
        &[r#"style="color: #007acc""#],
        &[r#"style=\"color: #007acc\""#],
    );
}

#[test]
fn format_keeps_triple_single_quoted_html_templates_idempotent() {
    assert_format_round_trip_is_stable(
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> str:
    return ""

title = "Title"
heading = "Heading"
content = "Content"

render_html(t'''
<!DOCTYPE html>
        <html>
  <head><title>{title}</title></head>
  <body><h1 data-tone='info'>{heading}</h1>
  <p>{content}</p></body>
</html>
''')
"#,
        &[r#"data-tone="info""#],
        &[r#"data-tone=\'info\'"#],
    );
}

#[test]
fn format_keeps_triple_quoted_html_templates_with_mixed_quotes_idempotent() {
    assert_format_round_trip_is_stable(
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> str:
    return ""

heading = "Heading"

render_html(t"""
<section class='hero' data-tone="info">
  <h1 title='Greeting'>{heading}</h1>
  <p>"Quoted" text and it's still valid.</p>
</section>
""")
"#,
        &[
            r#"class="hero""#,
            r#"data-tone="info""#,
            r#""Quoted" text and it's still valid."#,
        ],
        &[r#"\"Quoted\""#, r#"it\'s"#, r#"title=\'Greeting\'"#],
    );
}

#[test]
fn format_keeps_triple_quoted_thtml_templates_idempotent() {
    assert_format_round_trip_is_stable(
        r#"from typing import Annotated
from string.templatelib import Template

def render_thtml(template: Annotated[Template, "thtml"]) -> str:
    return ""

title = "Title"
status = "Ready"

render_thtml(t"""
<Card title="{title}" tone='info'>
  <Badge label="status">{status}</Badge>
</Card>
""")
"#,
        &[r#"title="{title}""#, r#"tone="info""#, r#"label="status""#],
        &[r#"tone=\'info\'"#, r#"label=\"status\""#],
    );
}

#[test]
fn format_stdin_keeps_triple_quoted_html_templates_idempotent() {
    assert_stdin_format_round_trip_is_stable(
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> str:
    return ""

title = "Title"
heading = "Heading"
content = "Content"

render_html(t"""
<!DOCTYPE html>
<html>
  <head><title>{title}</title></head>
  <body><h1 style="color: #007acc">{heading}</h1>
  <p>{content}</p></body>
</html>
""")
"#,
        Some("sample.py"),
        &[r#"style="color: #007acc""#],
        &[r#"style=\"color: #007acc\""#],
    );
}

#[test]
fn format_stdin_keeps_triple_quoted_thtml_templates_idempotent() {
    assert_stdin_format_round_trip_is_stable(
        r#"from typing import Annotated
from string.templatelib import Template

def render_thtml(template: Annotated[Template, "thtml"]) -> str:
    return ""

title = "Title"
status = "Ready"

render_thtml(t"""
<Card title="{title}" tone='info'>
  <Badge label="status">{status}</Badge>
</Card>
""")
"#,
        Some("component.py"),
        &[r#"title="{title}""#, r#"tone="info""#, r#"label="status""#],
        &[r#"tone=\'info\'"#, r#"label=\"status\""#],
    );
}

#[test]
fn format_keeps_triple_quoted_toml_templates_with_quoted_strings_idempotent() {
    assert_format_round_trip_is_stable(
        r#"from typing import Annotated
from string.templatelib import Template

type toml_config = Annotated[Template, "toml"]

def load_toml(template: toml_config) -> None:
    pass

project_name = "demo-project"
version = "0.1.0"

load_toml(t"""
[project]
name = "{project_name}"
version = "{version}"
""")
"#,
        &[r#"name = "{project_name}""#, r#"version = "{version}""#],
        &[r#"name = \"{project_name}\""#, r#"version = \"{version}\""#],
    );
}

#[test]
fn format_keeps_triple_quoted_html_templates_with_embedded_delimiters_idempotent() {
    assert_format_round_trip_is_stable_allowing_noop_first_pass(
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> str:
    return ""

render_html(t"""
<div><p>Double delimiter: """</p><p>Single delimiter: '''</p></div>
""")
"#,
        &[r#"Double delimiter: """"#, r#"Single delimiter: '''"#],
        &[],
    );
}

#[test]
fn format_keeps_triple_single_quoted_html_templates_with_embedded_delimiters_idempotent() {
    assert_format_round_trip_is_stable_allowing_noop_first_pass(
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> str:
    return ""

render_html(t'''
<div><p>Double delimiter: """</p><p>Single delimiter: '''</p></div>
''')
"#,
        &[r#"Double delimiter: """"#, r#"Single delimiter: '''"#],
        &[],
    );
}

#[test]
fn format_stdin_without_filename_keeps_triple_quoted_html_templates_idempotent() {
    assert_stdin_format_round_trip_is_stable(
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> str:
    return ""

render_html(t"""
<div>
  <p>Double delimiter: """</p>
  <p>Single delimiter: '''</p>
</div>
""")
"#,
        None,
        &[r#"Double delimiter: """"#, r#"Single delimiter: '''"#],
        &[],
    );
}

#[test]
fn format_keeps_mixed_language_template_suite_idempotent() {
    assert_format_round_trip_is_stable(
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> None:
    pass


def run_sql(template: Annotated[Template, "sql"]) -> None:
    pass


type css = Annotated[Template, "css"]
type yaml_config = Annotated[Template, "yaml"]
type toml_config = Annotated[Template, "toml"]


def load_styles(template: css) -> None:
    pass


def load_yaml(template: yaml_config) -> None:
    pass


def load_toml(template: toml_config) -> None:
    pass


title = "t-linter"
heading = "Template strings with syntax highlighting"
content = "Interpolations stay as normal Python expressions."

render_html(t"""
<!DOCTYPE html>
<html>
    <head>
        <title>{title}</title>
    </head>
    <body>
        <h1 style="color: #007acc">{heading}</h1>
        <p>{content}</p>
    </body>
</html>
""")

start_date = "2026-01-01"

run_sql(t"""
SELECT u.name, u.email, p.title
FROM users u
JOIN posts p ON u.id = p.author_id
WHERE u.created_at > {start_date}
ORDER BY u.name
""")

padding = 24

load_styles(t"""
.container {{
    max-width: 1200px;
    margin: 0 auto;
    padding: {padding}px;
}}
""")

app_name = "demo-app"

load_yaml(t"""
app:
  name: {app_name}
  debug: true
""")

project_name = "demo-project"
version = "0.1.0"

load_toml(t"""
[project]
name = "{project_name}"
version = "{version}"
""")
"#,
        &[
            r#"style="color: #007acc""#,
            r#"WHERE u.created_at > {start_date}"#,
            r#"padding: {padding}px;"#,
            r#"name: {app_name}"#,
            r#"name = "{project_name}""#,
            r#"version = "{version}""#,
        ],
        &[
            r#"style=\"color: #007acc\""#,
            r#"name = \"{project_name}\""#,
            r#"version = \"{version}\""#,
        ],
    );
}

#[test]
fn format_handles_mixed_supported_and_unsupported_templates() {
    let dir = test_dir("mixed");
    let path = dir.join("example.py");
    write_file(
        &path,
        r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "toml"] = t'title={title}'
markup: Annotated[Template, "html"] = t"<div>{name}</div>"
"#,
    );

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t'title = {title}'"#));
    assert!(content.contains(r#"t"<div>{name}</div>""#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_rewrites_html_and_thtml_backend_templates() {
    let dir = test_dir("html-thtml-backend");
    let path = dir.join("example.py");
    write_file(
        &path,
        r#"from typing import Annotated
from string.templatelib import Template

html_markup: Annotated[Template, "html"] = t'<div class = "x" >{name}</div>'
component_markup: Annotated[Template, "thtml"] = t'<Card title = "{title}" disabled ></Card>'
"#,
    );

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t'<div class="x">{name}</div>'"#));
    assert!(content.contains(r#"t'<Card title="{title}" disabled></Card>'"#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_rewrites_html_template_inferred_via_imported_function_annotation() {
    let dir = test_dir("html-imported-format");
    let path = dir.join("example.py");
    write_file(
        &dir.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_markup(template: Annotated[Template, "html"]) -> str:
    return ""
"#,
    );
    write_file(
        &path,
        r#"from typed_api import render_markup as render_html_markup

page = t'<div class = "x" >{name}</div>'

render_html_markup(page)
"#,
    );

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t'<div class="x">{name}</div>'"#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_rewrites_supported_templates_inferred_via_installed_package_annotations() {
    let dir = test_dir("installed-package-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> object:
    return None

def render_json(template: Annotated[Template, "json"]) -> object:
    return None

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None

def render_toml(template: Annotated[Template, "toml"]) -> object:
    return None

def render_thtml(template: Annotated[Template, "thtml"]) -> object:
    return None
"#,
    );
    write_file(
        &path,
        r#"from typed_api import render_html, render_json, render_thtml, render_toml, render_yaml

html_page = t'<div class = "x" >{name}</div>'
json_payload = t'[1,{count}]'
yaml_payload = t"name : {name}"
toml_payload = t'title={title}'
component_markup = t'<Card title = "{title}" disabled ></Card>'

render_html(html_page)
render_json(json_payload)
render_yaml(yaml_payload)
render_toml(toml_payload)
render_thtml(component_markup)
"#,
    );

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t'<div class="x">{name}</div>'"#));
    assert!(content.contains(r#"t'[1, {count}]'"#));
    assert!(content.contains(r#"t"name: {name}""#));
    assert!(content.contains(r#"t'title = {title}'"#));
    assert!(content.contains(r#"t'<Card title="{title}" disabled></Card>'"#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_skips_unsupported_templates_inferred_via_installed_package_annotations() {
    let dir = test_dir("installed-package-unsupported-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    let original = r#"from typed_api import render_mydsl

config = t"<value = {name}>"
render_mydsl(config)
"#;
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_mydsl(template: Annotated[Template, "mydsl"]) -> object:
    return None
"#,
    );
    write_file(&path, original);

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(content, original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_rewrites_template_via_installed_package_relative_reexport() {
    let dir = test_dir("installed-package-relative-reexport-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        "from .impl import render_yaml\n",
    );
    write_file(
        &site_packages.join("typed_api").join("impl.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &path,
        r#"from typed_api import render_yaml

config = t"name : {name}"
render_yaml(config)
"#,
    );

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t"name: {name}""#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_unresolved_relative_import_does_not_rewrite_template() {
    let dir = test_dir("unresolved-relative-import-format");
    let path = dir.join("example.py");
    let original = r#"from .typed_api import render_yaml

config = t"name : {name}"
render_yaml(config)
"#;
    write_file(&path, original);

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(content, original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_invalid_relative_import_in_installed_module_does_not_rewrite_template() {
    let dir = test_dir("invalid-relative-import-in-installed-module-format");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api.py"),
        "from .fallback import render_yaml\n",
    );
    write_file(
        &dir.join("fallback.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    let path = dir.join("example.py");
    let original = r#"from typed_api import render_yaml

config = t"name : {name}"
render_yaml(config)
"#;
    write_file(&path, original);

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(content, original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_rewrites_template_via_installed_package_parent_relative_reexport() {
    let dir = test_dir("installed-package-parent-relative-reexport-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    write_file(&site_packages.join("typed_api").join("__init__.py"), "");
    write_file(
        &site_packages
            .join("typed_api")
            .join("sub")
            .join("__init__.py"),
        "from ..impl import render_yaml\n",
    );
    write_file(
        &site_packages.join("typed_api").join("impl.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &path,
        r#"from typed_api.sub import render_yaml

config = t"name : {name}"
render_yaml(config)
"#,
    );

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t"name: {name}""#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_does_not_rewrite_template_after_delete_statement_shadows_import() {
    let dir = test_dir("installed-package-delete-shadow-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    let original = r#"from typed_api import render_html

del render_html
page = t'<div class = "x" >{name}</div>'
render_html(page)
"#;
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> object:
    return None
"#,
    );
    write_file(&path, original);

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(content, original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_does_not_rewrite_template_after_global_delete_statement_shadows_import() {
    let dir = test_dir("installed-package-global-delete-shadow-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    let original = r#"from typed_api import render_html

def wrapper():
    global render_html
    del render_html
    page = t'<div class = "x" >{name}</div>'
    render_html(page)
"#;
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> object:
    return None
"#,
    );
    write_file(&path, original);

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(content, original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_does_not_rewrite_template_inside_comprehension_shadow() {
    let dir = test_dir("installed-package-comprehension-shadow-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    let original = r#"import typed_api

pages = [typed_api.render_html(t'<div class = "x" >{name}</div>') for typed_api in [{}]]
"#;
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> object:
    return None
"#,
    );
    write_file(&path, original);

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(content, original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_rewrites_template_inside_comprehension_iterable() {
    let dir = test_dir("installed-package-comprehension-iterable-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> object:
    return None
"#,
    );
    write_file(
        &path,
        r#"import typed_api

pages = [page for typed_api in [typed_api.render_html(t'<div class = "x" >{name}</div>')]]
"#,
    );

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t'<div class="x">{name}</div>'"#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_uses_package_root_after_dotted_import_from_installed_package() {
    let dir = test_dir("installed-package-dotted-import-package-root-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &site_packages.join("typed_api").join("submodule.py"),
        "value = 1\n",
    );
    write_file(
        &path,
        r#"import typed_api.submodule

config = t"name : {name}"
typed_api.render_yaml(config)
"#,
    );

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t"name: {name}""#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_uses_intermediate_package_after_dotted_import_from_installed_package() {
    let dir = test_dir("installed-package-dotted-import-intermediate-package-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    write_file(&site_packages.join("typed_api").join("__init__.py"), "");
    write_file(
        &site_packages
            .join("typed_api")
            .join("subpkg")
            .join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &site_packages
            .join("typed_api")
            .join("subpkg")
            .join("mod.py"),
        "value = 1\n",
    );
    write_file(
        &path,
        r#"import typed_api.subpkg.mod

config = t"name : {name}"
typed_api.subpkg.render_yaml(config)
"#,
    );

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t"name: {name}""#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_uses_module_scope_import_after_nested_global_directive() {
    let dir = test_dir("installed-package-nested-global-directive-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &path,
        r#"from typed_api import render_yaml

def outer():
    render_yaml = None

    def inner():
        global render_yaml
        config = t"name : {name}"
        render_yaml(config)
"#,
    );

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t"name: {name}""#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_uses_outer_import_after_nonlocal_directive() {
    let dir = test_dir("installed-package-nonlocal-directive-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &path,
        r#"def outer():
    import typed_api as api

    def inner():
        nonlocal api
        config = t"name : {name}"
        api.render_yaml(config)
"#,
    );

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t"name: {name}""#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_nonlocal_template_assignment_keeps_inferred_language_hint() {
    let dir = test_dir("installed-package-nonlocal-template-hint-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &path,
        r#"import typed_api as api

def outer():
    config = ""

    def inner():
        nonlocal config
        config = t"name : {name}"
        api.render_yaml(config)
"#,
    );

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t"name: {name}""#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_outer_global_directive_does_not_leak_into_inner_local_scope() {
    let dir = test_dir("installed-package-outer-global-does-not-leak-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    let original = r#"from typed_api import render_json

def outer():
    global render_json

    def inner():
        render_json(t"[1,,2]")
        render_json = None
"#;
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_json(template: Annotated[Template, "json"]) -> object:
    return None
"#,
    );
    write_file(&path, original);

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(content, original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_does_not_bind_package_root_after_aliased_dotted_import() {
    let dir = test_dir("installed-package-aliased-dotted-import-root-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    let original = r#"import typed_api.submodule as api

config = t"name : {name}"
typed_api.render_yaml(config)
"#;
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &site_packages.join("typed_api").join("submodule.py"),
        "value = 1\n",
    );
    write_file(&path, original);

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(content, original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_uses_function_local_import_within_scope() {
    let dir = test_dir("installed-package-function-local-import-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &path,
        r#"def outer():
    import typed_api
    config = t"name : {name}"
    typed_api.render_yaml(config)
"#,
    );

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t"name: {name}""#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_function_local_import_does_not_leak_to_module_scope() {
    let dir = test_dir("installed-package-function-local-import-no-leak-format");
    let site_packages = dir.join("site-packages");
    let path = dir.join("example.py");
    let original = r#"def outer():
    import typed_api

config = t"name : {name}"
typed_api.render_yaml(config)
"#;
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(&path, original);

    let output =
        run_t_linter_with_pythonpath(&dir, &["format", "example.py"], None, Some(&site_packages));
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(content, original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_stdin_filename_uses_installed_package_import_inference() {
    let dir = test_dir("installed-package-stdin-format");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> object:
    return None
"#,
    );
    let input = br#"from typed_api import render_html

page = t'<div class = "x" >{name}</div>'
render_html(page)
"#;

    let output = run_t_linter_with_pythonpath(
        &dir,
        &["format", "--stdin-filename", "example.py", "-"],
        Some(input),
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(stderr.is_empty());
    assert!(stdout.contains(r#"t'<div class="x">{name}</div>'"#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_rewrites_thtml_template_inferred_via_reexported_class_signature() {
    let dir = test_dir("thtml-reexported-format");
    let path = dir.join("example.py");
    write_file(
        &dir.join("ui_impl.py"),
        r#"from typing import Annotated
from string.templatelib import Template

class Renderer:
    def __init__(self, template: Annotated[Template, "thtml"]) -> None:
        self.template = template
"#,
    );
    write_file(
        &dir.join("ui.py"),
        r#"from ui_impl import Renderer
"#,
    );
    write_file(
        &path,
        r#"from ui import Renderer

card = t'<Card title = "{title}" ><Badge>{status}</Badge></Card>'

Renderer(card)
"#,
    );

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t'<Card title="{title}"><Badge>{status}</Badge></Card>'"#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_check_recognizes_already_formatted_thtml_templates() {
    let dir = test_dir("thtml-check-clean");
    let path = dir.join("example.py");
    let original = r#"from typing import Annotated
from string.templatelib import Template

component_markup: Annotated[Template, "thtml"] = t'<Card title="{title}" disabled><Badge>{status}</Badge></Card>'
"#;
    write_file(&path, original);

    let output = run_t_linter(&dir, &["format", "--check", "example.py"], None);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(
        stderr.contains("0 files would be reformatted, 1 files already formatted, 0 inputs failed")
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), original);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_stdin_uses_stdin_filename_for_import_inferred_html() {
    let dir = test_dir("stdin-import-inferred-html");
    write_file(
        &dir.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_markup(template: Annotated[Template, "html"]) -> str:
    return ""
"#,
    );
    let input = br#"from typed_api import render_markup as render_html_markup

page = t'<div class = "x" >{name}</div>'

render_html_markup(page)
"#;

    let output = run_t_linter(
        &dir,
        &["format", "--stdin-filename", "example.py", "-"],
        Some(input),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(stderr.is_empty());
    assert!(stdout.contains(r#"t'<div class="x">{name}</div>'"#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_rewrites_multiple_html_and_thtml_templates_together() {
    let dir = test_dir("html-thtml-multiple");
    let path = dir.join("example.py");
    write_file(
        &path,
        r#"from typing import Annotated
from string.templatelib import Template

page: Annotated[Template, "html"] = t'<main class = "app" >{body}</main>'
card: Annotated[Template, "thtml"] = t'<Card title = "{title}" ><Badge tone = "ok" >{status}</Badge></Card>'
"#,
    );

    let output = run_t_linter(&dir, &["format", "example.py"], None);
    let content = fs::read_to_string(&path).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(content.contains(r#"t'<main class="app">{body}</main>'"#));
    assert!(
        content.contains(r#"t'<Card title="{title}"><Badge tone="ok">{status}</Badge></Card>'"#)
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_reports_invalid_explicit_non_python_files() {
    let dir = test_dir("non-py");
    write_file(&dir.join("notes.txt"), "hello");

    let output = run_t_linter(&dir, &["format", "notes.txt"], None);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(stderr.contains("notes.txt: Explicit file operands must use the .py extension"));
    assert!(stderr.contains("0 files reformatted, 0 files left unchanged, 1 inputs failed"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_uses_operand_root_config_outside_cwd() {
    let dir = test_dir("operand-root");
    let project = dir.join("project");
    let runner = dir.join("runner");
    fs::create_dir_all(&runner).unwrap();

    write_file(
        &project.join("pyproject.toml"),
        r#"[tool.t-linter]
extend-exclude = ["generated"]
"#,
    );
    write_file(
        &project.join("generated/example.py"),
        r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "toml"] = t'title={title}'
"#,
    );

    let output = run_t_linter(&runner, &["format", project.to_str().unwrap()], None);
    let stderr = String::from_utf8(output.stderr).unwrap();
    let content = fs::read_to_string(project.join("generated/example.py")).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(stderr.contains("0 files reformatted, 0 files left unchanged, 0 inputs failed"));
    assert!(content.contains(r#"t'title={title}'"#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_leaves_empty_and_template_free_files_unchanged() {
    let dir = test_dir("unchanged");
    write_file(&dir.join("empty.py"), "");
    write_file(&dir.join("plain.py"), "print('hello')\n");

    let output = run_t_linter(&dir, &["format", "."], None);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(stderr.contains("0 files reformatted, 2 files left unchanged, 0 inputs failed"));

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn format_follows_explicit_symlink_and_uses_first_label() {
    let dir = test_dir("symlink-file");
    let real = dir.join("real.py");
    let link = dir.join("link.py");
    write_file(
        &real,
        r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "toml"] = t'title={title}'
"#,
    );
    unix_fs::symlink(&real, &link).unwrap();

    let output = run_t_linter(&dir, &["format", "link.py", "real.py"], None);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(stderr.contains("Reformatted link.py"));
    assert!(!stderr.contains("Reformatted real.py"));

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn format_reports_descendants_under_symlink_directory_path() {
    let dir = test_dir("symlink-dir");
    let real_dir = dir.join("real");
    let link_dir = dir.join("linkdir");
    write_file(
        &real_dir.join("sub/example.py"),
        r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "toml"] = t'title={title}'
"#,
    );
    unix_fs::symlink(&real_dir, &link_dir).unwrap();

    let output = run_t_linter(&dir, &["format", "linkdir"], None);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(stderr.contains("Reformatted linkdir/sub/example.py"));

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn format_reports_unreadable_nested_directory_with_nested_display_path() {
    let dir = test_dir("nested-read-error");
    let real_dir = dir.join("real");
    let link_dir = dir.join("linkdir");
    let unreadable = real_dir.join("sub");
    fs::create_dir_all(&unreadable).unwrap();
    write_file(
        &real_dir.join("ok.py"),
        r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "toml"] = t'title={title}'
"#,
    );
    unix_fs::symlink(&real_dir, &link_dir).unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();

    let output = run_t_linter(&dir, &["format", "linkdir"], None);
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o755)).unwrap();

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr.contains("linkdir/sub: Failed to read directory"));

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn format_reports_broken_symlink_operands_as_failures() {
    let dir = test_dir("broken-symlink");
    unix_fs::symlink(dir.join("missing.py"), dir.join("broken.py")).unwrap();

    let output = run_t_linter(&dir, &["format", "broken.py"], None);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(stderr.contains("broken.py: Failed to resolve symlink"));

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn format_preserves_executable_permissions() {
    let dir = test_dir("permissions");
    let path = dir.join("script.py");
    write_file(
        &path,
        r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "toml"] = t'title={title}'
"#,
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

    let output = run_t_linter(&dir, &["format", "script.py"], None);
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(mode, 0o755);

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn format_write_failure_leaves_original_file_unchanged() {
    let dir = test_dir("write-failure");
    let locked = dir.join("locked");
    fs::create_dir_all(&locked).unwrap();
    let path = locked.join("example.py");
    let original = r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "toml"] = t'title={title}'
"#;
    write_file(&path, original);

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    let output = run_t_linter(&dir, &["format", "locked/example.py"], None);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr.contains("locked/example.py:"));
    assert_eq!(fs::read_to_string(&path).unwrap(), original);

    let _ = fs::remove_dir_all(dir);
}
