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

#[test]
fn stats_command_succeeds() {
    let dir = test_dir("stats");
    write_file(&dir.join("example.py"), "print('hello')\n");

    let output = Command::new(env!("CARGO_BIN_EXE_t-linter"))
        .args(["stats", "."])
        .current_dir(&dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8(output.stdout).unwrap().contains("Analyzing statistics for: ."));
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
fn lsp_subcommand_exits_cleanly_with_closed_stdio() {
    let output = Command::new(env!("CARGO_BIN_EXE_t-linter"))
        .args(["lsp", "--stdio"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn default_command_exits_cleanly_with_closed_stdio() {
    let output = Command::new(env!("CARGO_BIN_EXE_t-linter")).output().unwrap();

    assert_eq!(output.status.code(), Some(0));
}
