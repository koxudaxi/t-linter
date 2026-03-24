use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

fn test_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let unique_id = NEXT_TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "t-linter-docs-{name}-{}-{nanos}-{unique_id}",
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

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn extract_python_block_after_heading(markdown: &str, heading: &str) -> String {
    let mut saw_heading = false;
    let mut in_python_block = false;
    let mut lines = Vec::new();

    for line in markdown.lines() {
        if !saw_heading {
            if line.trim() == heading {
                saw_heading = true;
            }
            continue;
        }

        if !in_python_block {
            if line.trim() == "```python" {
                in_python_block = true;
            }
            continue;
        }

        if line.trim() == "```" {
            return lines.join("\n");
        }

        lines.push(line);
    }

    panic!("Failed to extract python block after heading: {heading}");
}

fn assert_doc_example_is_clean(relative_path: &str, heading: &str, expected_templates: u64) {
    let markdown_path = repo_root().join(relative_path);
    let markdown = fs::read_to_string(&markdown_path).unwrap();
    let code = extract_python_block_after_heading(&markdown, heading);

    let dir = test_dir("docs-example");
    let path = dir.join("example.py");
    write_file(&path, &code);

    let output = run_check(&dir, &["check", "example.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "check failed for {relative_path} / {heading}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        json["summary"]["diagnostics"].as_u64(),
        Some(0),
        "docs example produced diagnostics for {relative_path} / {heading}\nstdout:\n{stdout}"
    );
    assert_eq!(
        json["summary"]["templates_scanned"].as_u64(),
        Some(expected_templates),
        "unexpected template count for {relative_path} / {heading}\nstdout:\n{stdout}"
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn readme_quick_start_example_stays_lintable() {
    assert_doc_example_is_clean("README.md", "## Quick Start Example", 5);
}

#[test]
fn docs_index_quick_start_example_stays_lintable() {
    assert_doc_example_is_clean("docs/index.md", "## Quick Start Example", 5);
}

#[test]
fn llms_full_quick_start_example_stays_lintable() {
    assert_doc_example_is_clean("docs/llms-full.txt", "## Quick Start Example", 5);
}

#[test]
fn supported_languages_detection_example_stays_lintable() {
    assert_doc_example_is_clean("docs/supported-languages.md", "## Language Detection", 2);
}

#[test]
fn supported_languages_examples_stay_lintable() {
    assert_doc_example_is_clean("docs/supported-languages.md", "## Examples", 9);
}

#[test]
fn llms_full_supported_languages_examples_stay_lintable() {
    assert_doc_example_is_clean("docs/llms-full.txt", "## Examples", 9);
}
