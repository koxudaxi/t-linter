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
    run_check_with_pythonpath(dir, args, None)
}

fn run_check_with_pythonpath(
    dir: &Path,
    args: &[&str],
    pythonpath: Option<&Path>,
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_t-linter"));
    command.args(args).current_dir(dir);
    if let Some(pythonpath) = pythonpath {
        command.env("PYTHONPATH", pythonpath);
    }
    command.output().unwrap()
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
fn check_honors_toml_profile_metadata() {
    let dir = test_dir("toml-profile");
    let source_without_profile = r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "toml"] = t"""
value = {{ a = 1,
  b = 2 }}
"""
"#;
    write_file(&dir.join("ok.py"), source_without_profile);
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "toml", "profile:1.0"] = t"""
value = {{ a = 1,
  b = 2 }}
"""
"#,
    );

    let ok_output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let ok_stdout = String::from_utf8(ok_output.stdout).unwrap();
    let ok_json: serde_json::Value = serde_json::from_str(&ok_stdout).unwrap();
    assert_eq!(ok_output.status.code(), Some(0));
    assert_eq!(ok_json["summary"]["diagnostics"], 0);

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "toml");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_raw_first_template_prefixes() {
    let dir = test_dir("raw-first-prefix");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "html"] = rt"<div><"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "html");
    assert_eq!(
        json["diagnostics"][0]["expected_type"],
        serde_json::Value::Null
    );
    assert_eq!(
        json["diagnostics"][0]["found_type"],
        serde_json::Value::Null
    );
    assert_eq!(
        json["diagnostics"][0]["schema_pointer"],
        serde_json::Value::Null
    );
    assert_eq!(
        json["diagnostics"][0]["source_of_truth"],
        serde_json::Value::Null
    );
    assert_eq!(
        json["diagnostics"][0]["suggested_edits"],
        serde_json::json!([])
    );

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
fn check_reports_json_schema_binding_inward_diagnostics() {
    let dir = test_dir("json-schema-binding");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated, NotRequired, TypedDict
from string.templatelib import Template
from json_tstring import Json

class Order(TypedDict):
    id: int
    name: str
    active: bool
    note: NotRequired[str]

payload: Annotated[Template, Json(schema=Order)] = t'{{"id": "abc", "nme": "Ada"}}'
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let rules = json["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagnostic| diagnostic["rule"].as_str().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(output.status.code(), Some(0));
    assert!(rules.contains(&"template-schema-missing-key"));
    assert!(rules.contains(&"template-schema-unknown-key"));
    assert!(rules.contains(&"template-schema-type-shape"));
    let unknown = json["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|diagnostic| diagnostic["rule"] == "template-schema-unknown-key")
        .unwrap();
    assert_eq!(unknown["schema_pointer"], "/nme");
    assert_eq!(unknown["source_of_truth"], "Order");
    assert_eq!(unknown["suggested_edits"][0]["new_text"], "\"name\"");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_json_generic_binding_and_unresolved_binding() {
    let dir = test_dir("json-generic-binding");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import TypedDict
from json_tstring import Json

class Order(TypedDict):
    id: int

ok: Json[Order] = t'{{"id": 1}}'
missing: Json[MissingOrder] = t'{{"id": 1}}'
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["rule"], "binding-unresolved");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_resolves_json_schema_binding_across_imports() {
    let dir = test_dir("json-schema-import");
    write_file(
        &dir.join("models.py"),
        r#"from typing import TypedDict

class Order(TypedDict):
    id: int
    name: str
"#,
    );
    write_file(
        &dir.join("app.py"),
        r#"from typing import Annotated
from string.templatelib import Template
from json_tstring import Json
from models import Order

payload: Annotated[Template, Json(schema=Order)] = t'{{"id": 1}}'
"#,
    );

    let output = run_check(&dir, &["check", "app.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(
        json["diagnostics"][0]["rule"],
        "template-schema-missing-key"
    );
    assert_eq!(json["diagnostics"][0]["source_of_truth"], "Order");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_uses_dataclass_defaults_for_json_schema_requiredness() {
    let dir = test_dir("json-schema-dataclass");
    write_file(
        &dir.join("app.py"),
        r#"from dataclasses import dataclass
from json_tstring import Json

@dataclass
class Order:
    id: int
    note: str = ""

payload: Json[Order] = t'{{"note": "ok"}}'
"#,
    );

    let output = run_check(&dir, &["check", "app.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(
        json["diagnostics"][0]["rule"],
        "template-schema-missing-key"
    );
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("'id'")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_suppresses_json_schema_missing_key_when_key_is_interpolated() {
    let dir = test_dir("json-schema-interpolated-key");
    write_file(
        &dir.join("ok.py"),
        r#"from typing import Annotated, TypedDict
from string.templatelib import Template
from json_tstring import Json

class Order(TypedDict):
    id: int

payload: Annotated[Template, Json(schema=Order)] = t'{{{key}: 1}}'
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

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
fn check_reports_yaml_plain_scalars_via_keyword_function_annotation() {
    let dir = test_dir("yaml-keyword-function");
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

render_yaml_data(template=yaml_template)
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
fn check_psycopg_sql_rules_are_silent_without_sql_config() {
    let dir = test_dir("psycopg-silent-without-config");
    write_file(
        &dir.join("query.py"),
        r#"from typing import Annotated
from string.templatelib import Template

user_id = 1
query: Annotated[Template, "sql"] = t"SELECT * FROM users WHERE id = {user_id!r}"
"#,
    );

    let output = run_check(&dir, &["check", "query.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_psycopg_sql_static_rules_report_expected_diagnostics() {
    let dir = test_dir("psycopg-static-rules");
    write_file(
        &dir.join("pyproject.toml"),
        "[tool.t-linter.sql]\nlibrary = \"psycopg\"\n",
    );
    write_file(
        &dir.join("query.py"),
        r#"from typing import Annotated
from string.templatelib import Template
from psycopg import sql

user_id = 1
ids = [1, 2]

conversion: Annotated[Template, "sql"] = t"SELECT * FROM users WHERE id = {user_id!r}"
unknown_spec: Annotated[Template, "sql"] = t"SELECT * FROM users WHERE id = {user_id:x}"
identifier: Annotated[Template, "sql"] = t"SELECT * FROM {sql.Identifier('users')}"
dict_param: Annotated[Template, "sql"] = t"SELECT * FROM users WHERE data = { {'a': 1} }"
tuple_param: Annotated[Template, "sql"] = t"SELECT * FROM users WHERE id = {(1, 2)}"
in_clause: Annotated[Template, "sql"] = t"SELECT * FROM users WHERE id IN ({ids})"
multi_statement: Annotated[Template, "sql"] = t"SELECT 1; SELECT 2"
"#,
    );

    let output = run_check(&dir, &["check", "query.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let rules = json["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagnostic| diagnostic["rule"].as_str().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(output.status.code(), Some(0));
    assert!(rules.contains(&"sql-conversion-unsupported"));
    assert!(rules.contains(&"sql-format-spec-unknown"));
    assert!(rules.contains(&"sql-composable-spec-mismatch"));
    assert!(rules.contains(&"sql-dict-needs-json-wrapper"));
    assert!(rules.contains(&"sql-tuple-parameter"));
    assert!(rules.contains(&"sql-in-clause"));
    assert!(rules.contains(&"sql-multi-statement"));
    assert!(
        json["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| {
                diagnostic["severity"] == "warning" && diagnostic["rule"] == "sql-in-clause"
            })
    );
    assert!(
        json["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| {
                diagnostic["rule"] == "sql-conversion-unsupported"
                    && !diagnostic["suggested_edits"].as_array().unwrap().is_empty()
            })
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
fn check_reports_yaml_plain_scalars_via_reexported_function_annotation() {
    let dir = test_dir("yaml-reexported-function");
    write_file(
        &dir.join("bindings.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_data(template: Annotated[Template, "yaml"]) -> object:
    return {"ok": True}
"#,
    );
    write_file(
        &dir.join("typed_api.py"),
        r#"from bindings import render_data
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
fn check_reports_yaml_plain_scalars_via_nested_local_reexport_annotation() {
    let dir = test_dir("yaml-nested-local-reexport");
    write_file(
        &dir.join("package/bindings.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_data(template: Annotated[Template, "yaml"]) -> object:
    return {"ok": True}
"#,
    );
    write_file(
        &dir.join("package/typed_api.py"),
        r#"from bindings import render_data
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"from package.typed_api import render_data as render_yaml_data

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
fn check_uses_html_backend_semantics_for_raw_text_interpolations() {
    let dir = test_dir("html-raw-text");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "html"] = t"<script>{code}</script>"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("Interpolations are not allowed inside <script>")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_allows_title_interpolation_in_html_templates() {
    let dir = test_dir("html-title");
    write_file(
        &dir.join("ok.py"),
        r#"from typing import Annotated
from string.templatelib import Template

title = "Dashboard"
template: Annotated[Template, "html"] = t"<title>{title}</title>"
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_allows_title_interpolation_in_thtml_templates() {
    let dir = test_dir("thtml-title");
    write_file(
        &dir.join("ok.py"),
        r#"from typing import Annotated
from string.templatelib import Template

title = "Dashboard"
template: Annotated[Template, "thtml"] = t"<title>{title}</title>"
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_keeps_textarea_interpolation_rejected_for_html_templates() {
    let dir = test_dir("html-textarea");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

value = "hello"
template: Annotated[Template, "html"] = t"<textarea>{value}</textarea>"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("Interpolations are not allowed inside <textarea>")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_html_parse_errors_via_imported_function_annotation() {
    let dir = test_dir("html-imported-parse");
    write_file(
        &dir.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_markup(template: Annotated[Template, "html"]) -> str:
    return ""
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"from typed_api import render_markup as render_html_markup

page = t"<div><"

render_html_markup(page)
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["rule"], "embedded-parse-error");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_supported_diagnostics_via_installed_package_annotations() {
    let dir = test_dir("installed-package-supported-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_json(template: Annotated[Template, "json"]) -> object:
    return None

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None

def render_toml(template: Annotated[Template, "toml"]) -> object:
    return None

def render_html(template: Annotated[Template, "html"]) -> object:
    return None

def render_thtml(template: Annotated[Template, "thtml"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"from typed_api import render_html, render_json, render_thtml, render_toml, render_yaml

name = "api"
replicas = 3

def Button(*, label: str) -> object:
    return None

json_payload = t"""[1,,2]"""
yaml_payload = t"""
service:
  name: {name}
  replicas: fdsa fff {replicas}
"""
toml_payload = t"title ="
html_payload = t"<div><"
component_payload = t"<Button />"

render_json(json_payload)
render_yaml(yaml_payload)
render_toml(toml_payload)
render_html(html_payload)
render_thtml(component_payload)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "broken.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let diagnostics = json["diagnostics"].as_array().unwrap();
    let languages = diagnostics
        .iter()
        .filter_map(|diagnostic| diagnostic["language"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let rules = diagnostics
        .iter()
        .filter_map(|diagnostic| diagnostic["rule"].as_str())
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 5);
    assert_eq!(
        languages,
        std::collections::BTreeSet::from(["html", "json", "thtml", "toml", "yaml"])
    );
    assert!(rules.contains("embedded-parse-error"));
    assert!(rules.contains("component-missing-prop"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_keeps_unsupported_installed_package_languages_ignored() {
    let dir = test_dir("installed-package-unsupported-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_mydsl(template: Annotated[Template, "mydsl"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("ok.py"),
        r#"from typed_api import render_mydsl

config = t"broken < syntax {value}"
render_mydsl(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "ok.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);
    assert_eq!(json["summary"]["templates_scanned"], 1);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_diagnostics_via_installed_package_relative_reexport() {
    let dir = test_dir("installed-package-relative-reexport-check");
    let site_packages = dir.join("site-packages");
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
        &dir.join("broken.py"),
        r#"from typed_api import render_yaml

config = t"name: bad: {name}"
render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "broken.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "yaml");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_handles_installed_package_module_reexports_without_hanging() {
    let dir = test_dir("installed-package-module-reexports-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("html_tstring").join("__init__.py"),
        r#"from ._bindings import Renderable
from ._runtime import render_html
"#,
    );
    write_file(
        &site_packages.join("html_tstring").join("_bindings.py"),
        r#"class Renderable:
    def render(self) -> str:
        return ""
"#,
    );
    write_file(
        &site_packages.join("html_tstring").join("_runtime.py"),
        r#"from typing import Annotated
from string.templatelib import Template

from . import _bindings
from ._bindings import Renderable

type HtmlTemplate = Annotated[Template, "html"]

def render_html(template: HtmlTemplate | Renderable) -> str:
    return ""
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"from html_tstring import render_html

page = render_html(t"<div><")
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "broken.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "html");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_unresolved_relative_import_does_not_infer_language() {
    let dir = test_dir("unresolved-relative-import-check");
    write_file(
        &dir.join("ok.py"),
        r#"from .typed_api import render_yaml

config = t"name: bad: {name}"
render_yaml(config)
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);
    assert_eq!(json["summary"]["templates_scanned"], 1);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_invalid_relative_import_in_installed_module_does_not_infer_language() {
    let dir = test_dir("invalid-relative-import-in-installed-module-check");
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
    write_file(
        &dir.join("ok.py"),
        r#"from typed_api import render_yaml

config = t"name: bad: {name}"
render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "ok.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);
    assert_eq!(json["summary"]["templates_scanned"], 1);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_diagnostics_via_installed_package_parent_relative_reexport() {
    let dir = test_dir("installed-package-parent-relative-reexport-check");
    let site_packages = dir.join("site-packages");
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
        &dir.join("broken.py"),
        r#"from typed_api.sub import render_yaml

config = t"name: bad: {name}"
render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "broken.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "yaml");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_keeps_installed_package_direct_import_after_global_directive() {
    let dir = test_dir("installed-package-global-directive-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"from typed_api import render_yaml

def wrapper():
    global render_yaml
    name = "api"
    replicas = 3
    config = t"""
service:
  name: {name}
  replicas: fdsa fff {replicas}
"""
    render_yaml(config)
    render_yaml = render_yaml
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "broken.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "yaml");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_global_template_assignment_keeps_inferred_language_hint() {
    let dir = test_dir("installed-package-global-template-hint-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"from typed_api import render_yaml

config = ""

def wrapper():
    global config
    config = t"name: bad: {name}"
    render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "broken.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "yaml");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_keeps_module_scope_import_after_nested_global_directive() {
    let dir = test_dir("installed-package-nested-global-directive-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"from typed_api import render_yaml

def outer():
    render_yaml = None

    def inner():
        global render_yaml
        config = t"name: bad: {name}"
        render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "broken.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "yaml");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_keeps_outer_import_after_nonlocal_directive() {
    let dir = test_dir("installed-package-nonlocal-directive-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"def outer():
    import typed_api as api

    def inner():
        nonlocal api
        config = t"name: bad: {name}"
        api.render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "broken.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "yaml");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_outer_global_directive_does_not_leak_into_inner_local_scope() {
    let dir = test_dir("installed-package-outer-global-does-not-leak-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_json(template: Annotated[Template, "json"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("ok.py"),
        r#"from typed_api import render_json

def outer():
    global render_json

    def inner():
        render_json(t"[1,,2]")
        render_json = None
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "ok.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);
    assert_eq!(json["summary"]["templates_scanned"], 1);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_does_not_use_installed_package_direct_import_after_delete_statement() {
    let dir = test_dir("installed-package-delete-shadow-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("ok.py"),
        r#"from typed_api import render_html

del render_html
page = t'<div class = "x" >{name}</div>'
render_html(page)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "ok.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);
    assert_eq!(json["summary"]["templates_scanned"], 1);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_does_not_use_installed_package_direct_import_after_global_delete_statement() {
    let dir = test_dir("installed-package-global-delete-shadow-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("ok.py"),
        r#"from typed_api import render_html

def wrapper():
    global render_html
    del render_html
    page = t'<div class = "x" >{name}</div>'
    render_html(page)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "ok.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);
    assert_eq!(json["summary"]["templates_scanned"], 1);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_does_not_use_installed_package_module_alias_inside_comprehension_shadow() {
    let dir = test_dir("installed-package-comprehension-shadow-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("ok.py"),
        r#"import typed_api

pages = [typed_api.render_html(t'<div class = "x" >{name}</div>') for typed_api in [{}]]
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "ok.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);
    assert_eq!(json["summary"]["templates_scanned"], 1);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_uses_installed_package_module_alias_inside_comprehension_iterable() {
    let dir = test_dir("installed-package-comprehension-iterable-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"import typed_api

pages = [page for typed_api in [typed_api.render_yaml(t"name: bad: {name}")]]
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "broken.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "yaml");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_does_not_use_installed_package_module_alias_when_shadowed_by_parameter() {
    let dir = test_dir("installed-package-parameter-shadowed-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("ok.py"),
        r#"import typed_api as api

def wrapper(api):
    config = t"name: bad: {name}"
    api.render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "ok.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);
    assert_eq!(json["summary"]["templates_scanned"], 1);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_does_not_use_installed_package_module_alias_when_shadowed_by_except_alias() {
    let dir = test_dir("installed-package-except-shadowed-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("ok.py"),
        r#"import typed_api as api

try:
    pass
except Exception as api:
    config = t"name: bad: {name}"
    api.render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "ok.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);
    assert_eq!(json["summary"]["templates_scanned"], 1);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_does_not_use_installed_package_direct_import_when_shadowed_by_assignment() {
    let dir = test_dir("installed-package-direct-import-shadowed-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("ok.py"),
        r#"from typed_api import render_yaml

render_yaml = lambda template: template
config = t"name: bad: {name}"
render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "ok.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);
    assert_eq!(json["summary"]["templates_scanned"], 1);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_uses_package_root_after_dotted_import_from_installed_package() {
    let dir = test_dir("installed-package-dotted-import-package-root-check");
    let site_packages = dir.join("site-packages");
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
        &dir.join("broken.py"),
        r#"import typed_api.submodule

config = t"name: bad: {name}"
typed_api.render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "broken.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "yaml");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_uses_intermediate_package_after_dotted_import_from_installed_package() {
    let dir = test_dir("installed-package-dotted-import-intermediate-package-check");
    let site_packages = dir.join("site-packages");
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
        &dir.join("broken.py"),
        r#"import typed_api.subpkg.mod

config = t"name: bad: {name}"
typed_api.subpkg.render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "broken.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "yaml");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_does_not_bind_package_root_after_aliased_dotted_import() {
    let dir = test_dir("installed-package-aliased-dotted-import-root-check");
    let site_packages = dir.join("site-packages");
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
        &dir.join("ok.py"),
        r#"import typed_api.submodule as api

config = t"name: bad: {name}"
typed_api.render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "ok.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);
    assert_eq!(json["summary"]["templates_scanned"], 1);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_uses_function_local_import_within_scope() {
    let dir = test_dir("installed-package-function-local-import-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"def outer():
    import typed_api
    config = t"name: bad: {name}"
    typed_api.render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "broken.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "yaml");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_function_local_import_does_not_leak_to_module_scope() {
    let dir = test_dir("installed-package-function-local-import-no-leak-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_yaml(template: Annotated[Template, "yaml"]) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("ok.py"),
        r#"def outer():
    import typed_api

config = t"name: bad: {name}"
typed_api.render_yaml(config)
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "ok.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);
    assert_eq!(json["summary"]["templates_scanned"], 1);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_nested_import_does_not_clobber_outer_installed_package_module_alias() {
    let dir = test_dir("installed-package-nested-import-check");
    let site_packages = dir.join("site-packages");
    write_file(
        &site_packages.join("typed_api").join("__init__.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_json(template: Annotated[Template, "json"]) -> object:
    return None
"#,
    );
    write_file(&site_packages.join("other.py"), "value = 1\n");
    write_file(
        &dir.join("broken.py"),
        r#"import typed_api as api

config = t"[1,,2]"
api.render_json(config)

def wrapper():
    import other as api
    return api
"#,
    );

    let output = run_check_with_pythonpath(
        &dir,
        &["check", "broken.py", "--format", "json"],
        Some(&site_packages),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "json");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_html_raw_text_interpolation_via_reexported_annotation() {
    let dir = test_dir("html-reexported-raw-text");
    write_file(
        &dir.join("bindings.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_markup(template: Annotated[Template, "html"]) -> str:
    return ""
"#,
    );
    write_file(
        &dir.join("typed_api.py"),
        r#"from bindings import render_markup
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"from typed_api import render_markup as render_html_markup

css = "body { color: red; }"
page = t"<style>{css}</style>"

render_html_markup(page)
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["rule"], "embedded-parse-error");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("Interpolations are not allowed inside <style>")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_thtml_missing_required_component_props() {
    let dir = test_dir("thtml-missing-prop");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, label: str) -> object:
    return None

template: Annotated[Template, "thtml"] = t"<Button />"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["rule"], "component-missing-prop");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("missing required prop 'label'")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_thtml_unexpected_component_props() {
    let dir = test_dir("thtml-unexpected-prop");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, label: str) -> object:
    return None

template: Annotated[Template, "thtml"] = t"<Button label='Save' tone='info' />"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["rule"], "component-unexpected-prop");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("does not accept prop 'tone'")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_thtml_component_prop_type_errors() {
    let dir = test_dir("thtml-prop-type");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, disabled: bool = False, count: int = 0) -> object:
    return None

template: Annotated[Template, "thtml"] = t"""
<Button disabled="yes" count="{count}" />
"""
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 2);
    let rules = json["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagnostic| diagnostic["rule"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(rules.iter().all(|rule| rule == "component-prop-type-error"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_accepts_thtml_bool_props_and_kwargs_passthrough() {
    let dir = test_dir("thtml-bool-prop-ok");
    write_file(
        &dir.join("ok.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, disabled: bool = False, **props) -> object:
    return None

template: Annotated[Template, "thtml"] = t"<Button disabled tone='info' />"
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_unresolved_thtml_components() {
    let dir = test_dir("thtml-unresolved-component");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

template: Annotated[Template, "thtml"] = t"<MissingCard title='Hello' />"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["rule"], "component-unresolved");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_expands_static_thtml_spread_props() {
    let dir = test_dir("thtml-static-spread");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, label: str, disabled: bool = False) -> object:
    return None

props = {"label": "Save", "disabled": "yes", "tone": "info"}

template: Annotated[Template, "thtml"] = t"<Button {props} />"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 2);
    let rules = json["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagnostic| diagnostic["rule"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(rules.contains(&"component-prop-type-error".to_string()));
    assert!(rules.contains(&"component-unexpected-prop".to_string()));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_thtml_component_props_via_import_alias_signature() {
    let dir = test_dir("thtml-import-alias-function");
    write_file(
        &dir.join("components.py"),
        r#"def Button(*, label: str, disabled: bool = False) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template
from components import Button as PrimaryButton

template: Annotated[Template, "thtml"] = t"<PrimaryButton disabled='yes' />"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 2);
    let rules = json["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagnostic| diagnostic["rule"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(rules.contains(&"component-missing-prop".to_string()));
    assert!(rules.contains(&"component-prop-type-error".to_string()));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_thtml_component_props_via_imported_function_signature() {
    let dir = test_dir("thtml-imported-function");
    write_file(
        &dir.join("components.py"),
        r#"def Button(*, label: str, disabled: bool = False) -> object:
    return None
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template
from components import Button

template: Annotated[Template, "thtml"] = t"<Button disabled='yes' />"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 2);
    let rules = json["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagnostic| diagnostic["rule"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(rules.contains(&"component-missing-prop".to_string()));
    assert!(rules.contains(&"component-prop-type-error".to_string()));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_tdom_missing_required_props_via_direct_html_call() {
    let dir = test_dir("tdom-missing-prop");
    write_file(
        &dir.join("broken.py"),
        r#"from string.templatelib import Template
from tdom import html

def Button(*, label: str, disabled: bool = False, children: Template = t"") -> object:
    return None

page = html(t"<{Button} disabled />")
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "tdom");
    assert_eq!(json["diagnostics"][0]["rule"], "component-missing-prop");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("missing required prop 'label'")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_unknown_tdom_props_without_kwargs() {
    let dir = test_dir("tdom-unknown-props");
    write_file(
        &dir.join("broken.py"),
        r#"from string.templatelib import Template
from tdom import html

def Button(*, label: str, children: Template = t"") -> object:
    return None

page = html(t"<{Button} label='Save' tone='info' />")
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "tdom");
    assert_eq!(json["diagnostics"][0]["rule"], "component-unexpected-prop");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("does not accept prop 'tone'")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_allows_unknown_tdom_props_with_kwargs() {
    let dir = test_dir("tdom-kwargs-props");
    write_file(
        &dir.join("ok.py"),
        r#"from string.templatelib import Template
from tdom import html

def Button(*, label: str, children: Template = t"", **attrs: object) -> object:
    return None

page = html(t"<{Button} label='Save' tone='info' />")
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_rejects_tdom_children_attribute() {
    let dir = test_dir("tdom-children-attribute");
    write_file(
        &dir.join("broken.py"),
        r#"from string.templatelib import Template
from tdom import html

def Panel(*, children: Template = t"", **attrs: object) -> object:
    return None

page = html(t"<{Panel} children='manual'>Body</{Panel}>")
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "tdom");
    assert_eq!(json["diagnostics"][0]["rule"], "component-unexpected-prop");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("reserves it for component children")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_tdom_dataclass_missing_required_props() {
    let dir = test_dir("tdom-dataclass-missing-prop");
    write_file(
        &dir.join("broken.py"),
        r#"from dataclasses import dataclass
from string.templatelib import Template
from typing_extensions import ClassVar
from tdom import html

@dataclass
class Card:
    title: str
    variant: str = "info"
    component_type: ClassVar[str] = "card"
    children: Template = t""

page = html(t"<{Card} variant='success' />")
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "tdom");
    assert_eq!(json["diagnostics"][0]["rule"], "component-missing-prop");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("missing required prop 'title'")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_ignores_tdom_dataclass_fields_when_init_is_disabled() {
    let dir = test_dir("tdom-dataclass-init-false");
    write_file(
        &dir.join("ok.py"),
        r#"from dataclasses import dataclass
from tdom import html

@dataclass(init=False)
class Card:
    title: str

page = html(t"<{Card} />")
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_skips_tdom_dataclass_keyword_only_marker() {
    let dir = test_dir("tdom-dataclass-kw-only");
    write_file(
        &dir.join("broken.py"),
        r#"from dataclasses import KW_ONLY, dataclass
from string.templatelib import Template
from tdom import html

@dataclass
class Card:
    title: str
    _: KW_ONLY
    tone: str
    children: Template = t""

page = html(t"<{Card} title='Hello' />")
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "tdom");
    assert_eq!(json["diagnostics"][0]["rule"], "component-missing-prop");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("missing required prop 'tone'")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_uses_tdom_dataclass_init_var_inner_type() {
    let dir = test_dir("tdom-dataclass-init-var");
    write_file(
        &dir.join("broken.py"),
        r#"from dataclasses import InitVar, dataclass
from tdom import html

@dataclass
class Counter:
    count: InitVar[int]

page = html(t"<{Counter} count='many' />")
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "tdom");
    assert_eq!(json["diagnostics"][0]["rule"], "component-prop-type-error");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("count")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_allows_tdom_dataclass_defaulted_fields() {
    let dir = test_dir("tdom-dataclass-defaulted-fields");
    write_file(
        &dir.join("ok.py"),
        r#"from dataclasses import dataclass as component, field
from string.templatelib import Template
from tdom import html

@component(frozen=True)
class Card:
    title: str
    variant: str = field(default_factory=str)
    internal_id: str = field(init=False)
    children: Template = t""

page = html(t"<{Card} title='Hello' />")
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_tdom_props_via_reexported_dataclass_signature() {
    let dir = test_dir("tdom-reexported-dataclass");
    write_file(
        &dir.join("widgets.py"),
        r#"from dataclasses import dataclass
from string.templatelib import Template

@dataclass
class Card:
    title: str
    children: Template = t""
"#,
    );
    write_file(&dir.join("ui.py"), r#"from widgets import Card as Panel"#);
    write_file(
        &dir.join("broken.py"),
        r#"from tdom import html
from ui import Panel

page = html(t"<{Panel} />")
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "tdom");
    assert_eq!(json["diagnostics"][0]["rule"], "component-missing-prop");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_supports_explicit_tdom_annotations() {
    let dir = test_dir("tdom-annotated");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, label: str, children: Template = t"") -> object:
    return None

template: Annotated[Template, "tdom"] = t"<{Button} />"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "tdom");
    assert_eq!(json["diagnostics"][0]["rule"], "component-missing-prop");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_infers_tdom_via_svg_call() {
    let dir = test_dir("tdom-svg-call");
    write_file(
        &dir.join("broken.py"),
        r#"from tdom import svg

def Icon(*, label: str) -> object:
    return None

icon = svg(t"<{Icon} />")
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "tdom");
    assert_eq!(json["diagnostics"][0]["rule"], "component-missing-prop");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_rejects_tdom_components_that_require_positional_args() {
    let dir = test_dir("tdom-positional");
    write_file(
        &dir.join("broken.py"),
        r#"from tdom import html

def NeedsPositional(value, /, *, label: str = "ok") -> object:
    return None

page = html(t"<{NeedsPositional} />")
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "tdom");
    assert_eq!(json["diagnostics"][0]["rule"], "component-prop-type-error");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("requires positional arguments")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_rejects_tdom_components_backed_by_varargs_callables() {
    let dir = test_dir("tdom-varargs");
    write_file(
        &dir.join("broken.py"),
        r#"from tdom import html

def AcceptsVarArgs(*values: object, label: str = "ok") -> object:
    return None

page = html(t"<{AcceptsVarArgs} />")
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "tdom");
    assert_eq!(json["diagnostics"][0]["rule"], "component-prop-type-error");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("requires positional arguments")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_expands_tdom_data_attrs_before_kebab_normalization() {
    let dir = test_dir("tdom-data-attrs");
    write_file(
        &dir.join("broken.py"),
        r#"from string.templatelib import Template
from tdom import html

def Badge(*, data_user_id: int, children: Template = t"") -> object:
    return None

attrs = {"user-id": "wrong"}

page = html(t"<{Badge} data={attrs} />")
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["language"], "tdom");
    assert_eq!(json["diagnostics"][0]["rule"], "component-prop-type-error");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("data_user_id")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_thtml_component_props_via_reexported_class_signature() {
    let dir = test_dir("thtml-reexported-class");
    write_file(
        &dir.join("ui_impl.py"),
        r#"class Card:
    def __init__(self, *, title: str) -> None:
        self.title = title
"#,
    );
    write_file(
        &dir.join("ui.py"),
        r#"from ui_impl import Card
"#,
    );
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template
from ui import Card

template: Annotated[Template, "thtml"] = t"<Card title='Hello' tone='info' />"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["rule"], "component-unexpected-prop");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_rejects_inline_thtml_dict_spread_syntax() {
    let dir = test_dir("thtml-inline-spread");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, label: str, disabled: bool = False) -> object:
    return None

template: Annotated[Template, "thtml"] = t"<Button {{'label': 'Save', 'disabled': 'yes', 'tone': 'info'}} />"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["rule"], "embedded-parse-error");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_uses_latest_static_spread_assignment_before_template() {
    let dir = test_dir("thtml-spread-reassignment");
    write_file(
        &dir.join("ok.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, label: str) -> object:
    return None

props = {"tone": "info"}
props = {"label": "Save"}

template: Annotated[Template, "thtml"] = t"<Button {props} />"
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_unknown_thtml_spread_suppresses_missing_required_props() {
    let dir = test_dir("thtml-unknown-spread");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, label: str) -> object:
    return None

template: Annotated[Template, "thtml"] = t"<Button {props} tone='info' />"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["rule"], "component-unexpected-prop");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_explicit_prop_after_spread_overrides_static_spread_value() {
    let dir = test_dir("thtml-spread-explicit-overrides");
    write_file(
        &dir.join("ok.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, label: str, disabled: bool = False) -> object:
    return None

props = {"label": "Save", "disabled": "yes"}

template: Annotated[Template, "thtml"] = t"<Button {props} disabled />"
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_later_static_spread_overrides_earlier_explicit_prop() {
    let dir = test_dir("thtml-spread-overrides-explicit");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, label: str, disabled: bool = False) -> object:
    return None

props = {"label": "Save", "disabled": "yes"}

template: Annotated[Template, "thtml"] = t"<Button disabled {props} />"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["rule"], "component-prop-type-error");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_accepts_typed_static_spread_values_for_bool_int_and_none() {
    let dir = test_dir("thtml-typed-static-spread");
    write_file(
        &dir.join("ok.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, label: str, disabled: bool = False, count: int = 0, subtitle: str | None = None) -> object:
    return None

props = {"label": "Save", "disabled": True, "count": 3, "subtitle": None}

template: Annotated[Template, "thtml"] = t"<Button {props} />"
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_accepts_int_static_spread_values_for_float_props() {
    let dir = test_dir("thtml-float-from-int-spread");
    write_file(
        &dir.join("ok.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Meter(*, ratio: float) -> object:
    return None

props = {"ratio": 1}

template: Annotated[Template, "thtml"] = t"<Meter {props} />"
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_resolves_static_spread_bindings_in_the_nearest_scope() {
    let dir = test_dir("thtml-static-spread-scope");
    write_file(
        &dir.join("ok.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, label: str) -> object:
    return None

props = {"tone": "info"}

def render() -> None:
    props = {"label": "Save"}
    template: Annotated[Template, "thtml"] = t"<Button {props} />"
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_prefers_static_spread_bindings_from_the_template_scope() {
    let dir = test_dir("thtml-scope-aware-spread");
    write_file(
        &dir.join("ok.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, label: str) -> object:
    return None

props = {"label": "Save"}

def helper() -> None:
    props = {"disabled": True}
    return None

template: Annotated[Template, "thtml"] = t"<Button {props} />"
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_mixed_known_and_unknown_spreads_still_report_known_unexpected_props() {
    let dir = test_dir("thtml-known-and-unknown-spread");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Button(*, label: str) -> object:
    return None

known = {"tone": "info"}

template: Annotated[Template, "thtml"] = t"<Button {known} {dynamic} />"
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["rule"], "component-unexpected-prop");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("does not accept prop 'tone'")
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_reports_unresolved_nested_component_without_flagging_known_siblings() {
    let dir = test_dir("thtml-nested-unresolved");
    write_file(
        &dir.join("broken.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def Layout(*, children: str | None = None) -> object:
    return None

def Button(*, label: str) -> object:
    return None

template: Annotated[Template, "thtml"] = t"""
<Layout>
  <MissingCard />
  <Button label='Save' />
</Layout>
"""
"#,
    );

    let output = run_check(&dir, &["check", "broken.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 1);
    assert_eq!(json["diagnostics"][0]["rule"], "component-unresolved");
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("MissingCard")
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
fn check_does_not_infer_html_language_from_unknown_keyword_for_keyword_only_params() {
    let dir = test_dir("html-keyword-only-no-fallback");
    write_file(
        &dir.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_markup(*, template: Annotated[Template, "html"]) -> str:
    return ""
"#,
    );
    write_file(
        &dir.join("ok.py"),
        r#"from typed_api import render_markup

page = t"<div><"

render_markup(markup=page)
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(json["summary"]["diagnostics"], 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn check_does_not_infer_html_language_positionally_after_args_splat() {
    let dir = test_dir("html-varargs-keyword-only");
    write_file(
        &dir.join("typed_api.py"),
        r#"from typing import Annotated
from string.templatelib import Template

def render_markup(*args: object, template: Annotated[Template, "html"]) -> str:
    return ""
"#,
    );
    write_file(
        &dir.join("ok.py"),
        r#"from typed_api import render_markup

page = t"<div><"

render_markup(page)
"#,
    );

    let output = run_check(&dir, &["check", "ok.py", "--format", "json"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
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

#[test]
fn check_reports_invalid_explicit_non_python_files() {
    let dir = test_dir("invalid-explicit-non-python");
    write_file(&dir.join("notes.txt"), "not python\n");

    let output = run_check(&dir, &["check", "notes.txt"]);
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(stdout.contains("notes.txt:1:1: error[file-read-error]"));
    assert!(stdout.contains("Explicit file operands must use the .py extension"));
    assert!(stdout.contains("1 files scanned"));
    assert!(stdout.contains("0 templates scanned"));
    assert!(stdout.contains("1 diagnostics"));
    assert!(stdout.contains("1 failed files"));

    let _ = fs::remove_dir_all(dir);
}
