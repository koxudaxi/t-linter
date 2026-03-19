use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{self as unix_fs, PermissionsExt};

fn test_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("t-linter-{name}-{}-{nanos}", std::process::id()));
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
fn check_reports_yaml_plain_scalars_with_whitespace_interpolation() {
    let dir = test_dir("yaml-plain-scalar");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

name = "api"
replicas = 3

template: Annotated[Template, "yaml"] = t"""
service:
  name: {name}
  replicas: fdsa fff fds{replicas}
"""
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(
        json["diagnostics"][0]["message"],
        "Quote YAML plain scalars that mix whitespace and interpolations."
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_yaml_plain_scalars_via_imported_function_annotation() {
    let dir = test_dir("yaml-imported-function");
    write_file(
        &dir.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_data(template: Annotated[Template, "yaml"]) -> object:
    return {"ok": True}
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"from typed_api import render_data as render_yaml_data

name = "api"
replicas = 3

yaml_template = t"""
service:
  name: {name}
  replicas: fdsa fff {replicas}
"""

render_yaml_data(yaml_template)
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(
        json["diagnostics"][0]["message"],
        "Quote YAML plain scalars that mix whitespace and interpolations."
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_yaml_plain_scalars_via_imported_class_annotation() {
    let dir = test_dir("yaml-imported-class");
    write_file(
        &dir.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

class Loader:
    def __init__(self, template: Annotated[Template, "yaml"]) -> None:
        self.template = template
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"from typed_api import Loader

name = "api"
replicas = 3

yaml_template = t"""
service:
  name: {name}
  replicas: fdsa fff {replicas}
"""

Loader(yaml_template)
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(
        json["diagnostics"][0]["message"],
        "Quote YAML plain scalars that mix whitespace and interpolations."
    );

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
        &[
            "check",
            "broken.py",
            "--format",
            "github",
            "--error-on-issues",
        ],
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

#[test]
fn check_uses_operand_root_config_outside_cwd() {
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
        &project.join("generated/broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "html"] = t"<div><"
"#,
    );

    let output = run_check(
        &runner,
        &["check", project.to_str().unwrap(), "--format", "json"],
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["files_scanned"], 0);
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn check_follows_explicit_symlink_file_operands() {
    let dir = test_dir("symlink-file");
    let real = dir.join("real.py");
    let link = dir.join("link.py");

    write_file(
        &real,
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "html"] = t"<div><"
"#,
    );
    unix_fs::symlink(&real, &link).unwrap();

    let output = run_check(&dir, &["check", "link.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["files_scanned"], 1);
    assert_eq!(json["diagnostics"][0]["file"], "link.py");

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn check_reports_descendants_under_symlink_directory_operand() {
    let dir = test_dir("symlink-dir");
    let real_dir = dir.join("real");
    let link_dir = dir.join("linkdir");

    write_file(
        &real_dir.join("sub/broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "html"] = t"<div><"
"#,
    );
    unix_fs::symlink(&real_dir, &link_dir).unwrap();

    let output = run_check(&dir, &["check", "linkdir", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["files_scanned"], 1);
    assert_eq!(json["diagnostics"][0]["file"], "linkdir/sub/broken.py");

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn check_uses_first_explicit_operand_for_duplicate_targets() {
    let dir = test_dir("duplicate-target");
    let real = dir.join("real.py");
    let link = dir.join("link.py");

    write_file(
        &real,
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "html"] = t"<div><"
"#,
    );
    unix_fs::symlink(&real, &link).unwrap();

    let output = run_check(&dir, &["check", "link.py", "real.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["files_scanned"], 1);
    assert_eq!(json["diagnostics"][0]["file"], "link.py");

    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn check_reports_unreadable_nested_directory_with_nested_display_path() {
    let dir = test_dir("nested-read-error");
    let real_dir = dir.join("real");
    let link_dir = dir.join("linkdir");
    let unreadable = real_dir.join("sub");
    fs::create_dir_all(&unreadable).unwrap();
    write_file(
        &real_dir.join("ok.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "yaml"] = t"name: {value}"
"#,
    );
    unix_fs::symlink(&real_dir, &link_dir).unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();

    let output = run_check(&dir, &["check", "linkdir", "--format", "json"]);
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o755)).unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json["summary"]["failed_files"], 1);
    assert_eq!(json["diagnostics"][0]["file"], "linkdir/sub");

    let _ = fs::remove_dir_all(dir);
}
