use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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

fn write_executable(path: &Path, contents: &str) {
    write_file(path, contents);
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }
}

fn install_mock_prettier(dir: &Path) {
    write_executable(
        &dir.join("node_modules/.bin/prettier"),
        r#"#!/bin/sh
parser=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--parser" ]; then
    parser="$2"
    shift 2
  else
    shift
  fi
done

case "$parser" in
  json)
    printf '{\n  "name": "__T_LINTER_SLOT_0__"\n}\n'
    ;;
  html)
    printf '<div>\n  __T_LINTER_SLOT_0__\n</div>\n'
    ;;
  *)
    cat
    ;;
esac
"#,
    );
}

fn run_command(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_t-linter"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

#[test]
fn format_rewrites_supported_templates_in_place() {
    let dir = test_dir("in-place");
    install_mock_prettier(&dir);
    write_file(
        &dir.join("example.py"),
        r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "json"] = t"""{{"name": {value}}}"""
"#,
    );

    let output = run_command(&dir, &["format", "example.py"]);
    let contents = fs::read_to_string(dir.join("example.py")).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("1 file reformatted"));
    assert!(contents.contains("t\"\"\"{{\n  \"name\": {value}\n}}\"\"\""));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_check_returns_exit_code_one_when_changes_are_needed() {
    let dir = test_dir("check");
    install_mock_prettier(&dir);
    write_file(
        &dir.join("example.py"),
        r#"from typing import Annotated
from string.templatelib import Template

page: Annotated[Template, "html"] = t"<div>{value}</div>"
"#,
    );

    let output = run_command(&dir, &["format", "example.py", "--check"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let contents = fs::read_to_string(dir.join("example.py")).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout.contains("Would reformat: example.py"));
    assert!(contents.contains(r#"t"<div>{value}</div>""#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_respects_default_excludes() {
    let dir = test_dir("exclude");
    install_mock_prettier(&dir);
    write_file(
        &dir.join("src/example.py"),
        r#"from typing import Annotated
from string.templatelib import Template

page: Annotated[Template, "html"] = t"<div>{value}</div>"
"#,
    );
    write_file(
        &dir.join(".venv/ignored.py"),
        r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "json"] = t"""{{"name": {value}}}"""
"#,
    );

    let output = run_command(&dir, &["format", "."]);
    let kept = fs::read_to_string(dir.join(".venv/ignored.py")).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(kept.contains(r#"t"""{{"name": {value}}}""""#));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_returns_exit_code_two_when_formatter_is_missing() {
    let dir = test_dir("missing");
    write_file(
        &dir.join("example.py"),
        r#"from typing import Annotated
from string.templatelib import Template

config: Annotated[Template, "toml"] = t"title = {value}"
"#,
    );

    let output = run_command(&dir, &["format", "example.py"]);
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(stderr.contains("Failed to format example.py"));
    assert!(stderr.contains("cargo install taplo-cli"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn format_help_mentions_required_formatters() {
    let dir = test_dir("help");
    let output = run_command(&dir, &["format", "--help"]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("prettier"));
    assert!(stdout.contains("taplo"));
    assert!(stdout.contains("npm install --save-dev prettier"));

    let _ = fs::remove_dir_all(dir);
}
