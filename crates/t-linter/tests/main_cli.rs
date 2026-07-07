use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn test_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "t-linter-main-{name}-{}-{nanos}",
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

fn wait_for_child_with_timeout(child: &mut Child, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("child process timed out after {:?}", timeout);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn stats_command_succeeds() {
    let dir = test_dir("stats");
    write_file(
        &dir.join("example.py"),
        r#"from typing import Annotated
from string.templatelib import Template

page: Annotated[Template, "html"] = t"<h1>{title}</h1>"
plain = t"{title}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_t-linter"))
        .args(["stats", "."])
        .current_dir(&dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Files scanned:"));
    assert!(stdout.contains("Template strings:"));
    assert!(stdout.contains("html"));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn stats_command_outputs_json_counts() {
    let dir = test_dir("stats-json");
    write_file(
        &dir.join("example.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> None:
    pass

page: Annotated[Template, "html"] = t"<h1>{title}</h1>"
render_yaml(t"name: {name}")
plain = t"{title}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_t-linter"))
        .args(["stats", ".", "--format", "json"])
        .current_dir(&dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["files_scanned"], 1);
    assert_eq!(json["templates_total"], 3);
    assert_eq!(json["typed"], 2);
    assert_eq!(json["untyped"], 1);
    assert_eq!(json["by_language"]["html"], 1);
    assert_eq!(json["by_language"]["yaml"], 1);
    assert_eq!(json["by_detection"]["annotation"], 1);
    assert_eq!(json["by_detection"]["callee-inference"], 1);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn stats_command_accepts_rust_log_env() {
    let dir = test_dir("stats-rust-log");
    write_file(&dir.join("example.py"), "print('hello')\n");

    let output = Command::new(env!("CARGO_BIN_EXE_t-linter"))
        .args(["stats", "."])
        .env("RUST_LOG", "debug")
        .current_dir(&dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_command_reports_invalid_config_errors() {
    let dir = test_dir("check-invalid-config");
    write_file(
        &dir.join("pyproject.toml"),
        "[tool.t-linter]\nline-length = \"bad\"\nexclude = [\n",
    );
    write_file(&dir.join("example.py"), "print('hello')\n");

    let output = Command::new(env!("CARGO_BIN_EXE_t-linter"))
        .args(["check", "."])
        .current_dir(&dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Failed to parse"));
    assert!(stderr.contains("pyproject.toml"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn sql_prepare_requires_database_url_without_cache() {
    let dir = test_dir("sql-prepare-no-database-url");
    write_file(
        &dir.join("pyproject.toml"),
        "[tool.t-linter.sql]\nlibrary = \"psycopg\"\n",
    );
    write_file(
        &dir.join("app.py"),
        r#"from typing import Annotated
from string.templatelib import Template

query: Annotated[Template, "sql"] = t"SELECT * FROM users WHERE id = {user_id}"
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_t-linter"))
        .args(["sql", "prepare", "app.py"])
        .current_dir(&dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("SQL database-url is required"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn lsp_subcommand_exits_cleanly_with_closed_stdio() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_t-linter"))
        .args(["lsp", "--stdio"])
        .spawn()
        .unwrap();

    assert_eq!(
        wait_for_child_with_timeout(&mut child, Duration::from_secs(10)).code(),
        Some(0)
    );
}

#[test]
fn lsp_subcommand_accepts_ruff_pipeline_startup_options_with_closed_stdio() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_t-linter"))
        .args([
            "lsp",
            "--stdio",
            "--ruff-pipeline",
            "--ruff-command",
            "ruff",
            "--ruff-arg",
            "server",
        ])
        .spawn()
        .unwrap();

    assert_eq!(
        wait_for_child_with_timeout(&mut child, Duration::from_secs(10)).code(),
        Some(0)
    );
}

#[test]
fn default_command_exits_cleanly_with_closed_stdio() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_t-linter"))
        .spawn()
        .unwrap();

    assert_eq!(
        wait_for_child_with_timeout(&mut child, Duration::from_secs(10)).code(),
        Some(0)
    );
}
