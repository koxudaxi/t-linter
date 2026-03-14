use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn test_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "t-linter-{name}-{}-{nanos}",
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

fn run_check(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_t-linter"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

#[test]
fn check_human_reports_issues_without_failing_by_default() {
    let dir = test_dir("human");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "html"] = t"<div><"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py"]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("broken.py:"));
    assert!(stdout.contains("error[embedded-parse-error]"));
    assert!(stdout.contains("1 files scanned"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_error_on_issues_returns_exit_code_one() {
    let dir = test_dir("exit-one");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "json"] = t"""[1,,2]"""
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--error-on-issues"]);
    assert_eq!(output.status.code(), Some(1));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_json_recurses_and_skips_default_excludes() {
    let dir = test_dir("json");
    write_file(
        &dir.join("src/ok.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "yaml"] = t"name: {value}"
"#,
    );
    write_file(
        &dir.join(".venv/ignored.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "json"] = t"{"name": ]}"
"#,
    );

    let output = run_check(&dir, &["check", ".", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["files_scanned"], 1);
    assert_eq!(json["summary"]["diagnostics"], 0);
    assert_eq!(json["summary"]["failed_files"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_github_outputs_annotations() {
    let dir = test_dir("github");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "toml"] = t"title ="
"#,
    );

    let output = run_check(
        &dir,
        &["check", "broken.py", "--format", "github", "--error-on-issues"],
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout.contains("::error file=broken.py"));
    assert!(stderr.contains("1 files scanned"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_file_read_errors_return_exit_code_two() {
    let dir = test_dir("read-error");
    let path = dir.join("invalid.py");
    fs::write(&path, b"\xff\xfe\xfd").unwrap();

    let output = run_check(&dir, &["check", "invalid.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json["summary"]["failed_files"], 1);
    assert_eq!(json["diagnostics"][0]["rule"], "file-read-error");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_respects_pyproject_extend_exclude() {
    let dir = test_dir("pyproject-exclude");
    write_file(
        &dir.join("pyproject.toml"),
        r#"[tool.t-linter]
extend-exclude = ["generated"]
"#,
    );
    write_file(
        &dir.join("src/ok.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "yaml"] = t"name: {value}"
"#,
    );
    write_file(
        &dir.join("generated/broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "html"] = t"<div><"
"#,
    );

    let output = run_check(&dir, &["check", ".", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["files_scanned"], 1);
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_respects_t_linterignore() {
    let dir = test_dir("ignore-file");
    write_file(&dir.join(".t-linterignore"), "ignored.py\n");
    write_file(
        &dir.join("ignored.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "html"] = t"<div><"
"#,
    );
    write_file(
        &dir.join("kept.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "yaml"] = t"name: {value}"
"#,
    );

    let output = run_check(&dir, &["check", ".", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["files_scanned"], 1);
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}
