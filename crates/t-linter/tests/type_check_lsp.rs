use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tower_lsp::lsp_types::Url;

const LSP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TYPE_DIAGNOSTIC_RULE: &str = "interpolation-type-error";

fn test_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "t-linter-type-check-lsp-{name}-{}-{nanos}",
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

fn ty_command() -> Option<String> {
    if let Ok(command) = std::env::var("TY_BIN") {
        return Some(command);
    }

    let output = Command::new("ty").arg("--version").output().ok()?;
    if output.status.success() {
        Some("ty".to_string())
    } else {
        None
    }
}

fn pyright_command() -> Option<String> {
    if let Ok(command) = std::env::var("PYRIGHT_LANGSERVER_BIN") {
        return Some(command);
    }

    command_on_path("pyright-langserver")
}

fn pyrefly_command() -> Option<String> {
    if let Ok(command) = std::env::var("PYREFLY_BIN") {
        return Some(command);
    }

    command_on_path("pyrefly")
}

fn command_on_path(command: &str) -> Option<String> {
    let executable_names = if cfg!(windows) {
        vec![
            format!("{command}.cmd"),
            format!("{command}.exe"),
            command.to_string(),
        ]
    } else {
        vec![command.to_string()]
    };
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        for executable_name in &executable_names {
            if dir.join(executable_name).is_file() {
                return Some(command.to_string());
            }
        }
    }
    None
}

struct TypeCheckerCase {
    name: &'static str,
    checker: &'static str,
    command: String,
    args: Vec<&'static str>,
}

fn available_type_checkers() -> Vec<TypeCheckerCase> {
    let mut checkers = Vec::new();
    if let Some(command) = ty_command() {
        checkers.push(TypeCheckerCase {
            name: "ty",
            checker: "ty",
            command,
            args: vec!["server"],
        });
    } else {
        eprintln!("skipped ty: command not found");
    }
    if let Some(command) = pyright_command() {
        checkers.push(TypeCheckerCase {
            name: "pyright",
            checker: "pyright",
            command,
            args: vec!["--stdio"],
        });
    } else {
        eprintln!("skipped pyright: pyright-langserver not found");
    }
    if let Some(command) = pyrefly_command() {
        checkers.push(TypeCheckerCase {
            name: "pyrefly",
            checker: "pyrefly",
            command,
            args: vec!["lsp"],
        });
    } else {
        eprintln!("skipped pyrefly: command not found");
    }
    checkers
}

fn write_type_checker_config_files(workspace: &Path, checker: &str) {
    match checker {
        "pyright" => write_file(
            &workspace.join("pyrightconfig.json"),
            r#"{
  "pythonVersion": "3.14",
  "typeCheckingMode": "strict"
}
"#,
        ),
        "pyrefly" => write_file(
            &workspace.join("pyrefly.toml"),
            "python-version = \"3.14\"\n",
        ),
        _ => {}
    }
}

struct LspClient {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Value>,
    reader: Option<JoinHandle<()>>,
    next_id: i64,
}

impl LspClient {
    fn start(workspace: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_t-linter"))
            .args(["lsp", "--stdio"])
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (message_sender, messages) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            while let Some(message) = read_lsp_message(&mut stdout) {
                if message_sender.send(message).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            messages,
            reader: Some(reader),
            next_id: 1,
        }
    }

    fn initialize(&mut self, workspace: &Path, checker: &TypeCheckerCase) {
        let workspace_uri = file_uri(workspace);
        let response = self.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": workspace_uri,
                "capabilities": {
                    "workspace": {
                        "configuration": true,
                        "workspaceFolders": true
                    },
                    "textDocument": {
                        "publishDiagnostics": {},
                        "diagnostic": {},
                        "formatting": {}
                    }
                },
                "initializationOptions": {
                    "enableTypeChecking": false,
                    "highlightUntyped": true,
                    "typeChecking": {
                        "enabled": true,
                        "checker": checker.checker,
                        "command": checker.command.clone(),
                        "args": checker.args.clone()
                    }
                },
                "workspaceFolders": [
                    {
                        "uri": workspace_uri,
                        "name": "fixture"
                    }
                ]
            }),
        );
        assert!(
            response.get("error").is_none(),
            "initialize failed: {response}"
        );
        self.notify("initialized", json!({}));
    }

    fn did_open(&mut self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "python",
                    "version": 1,
                    "text": text
                }
            }),
        );
    }

    fn wait_for_type_diagnostics(&mut self, uri: &str) -> Vec<Value> {
        let deadline = Instant::now() + LSP_REQUEST_TIMEOUT;
        let mut observed = Vec::new();
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            let message = match self.messages.recv_timeout(remaining) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => {
                    panic!(
                        "timed out waiting for type diagnostics after {LSP_REQUEST_TIMEOUT:?}; observed: {observed:?}"
                    );
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("LSP stdout reader exited before diagnostics");
                }
            };
            if message["method"] != "textDocument/publishDiagnostics" {
                if message["method"] == "window/logMessage" {
                    observed.push(message.clone());
                }
                continue;
            }
            if message["params"]["uri"].as_str() != Some(uri) {
                continue;
            }
            let diagnostics = message["params"]["diagnostics"]
                .as_array()
                .expect("diagnostics array")
                .clone();
            observed.push(message.clone());
            if diagnostics
                .iter()
                .any(|diagnostic| diagnostic["code"] == TYPE_DIAGNOSTIC_RULE)
            {
                return diagnostics;
            }
        }
    }

    fn shutdown(mut self) {
        let response = self.request_without_params("shutdown");
        assert!(
            response.get("error").is_none(),
            "shutdown failed: {response}"
        );
        self.notify_without_params("exit");
        drop(self.stdin.take());
        assert_eq!(
            wait_for_child_with_timeout(&mut self.child, Duration::from_secs(10)).code(),
            Some(0)
        );
        if let Some(reader) = self.reader.take() {
            reader.join().expect("LSP stdout reader panicked");
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        self.wait_for_response(method, id)
    }

    fn request_without_params(&mut self, method: &str) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        }));
        self.wait_for_response(method, id)
    }

    fn wait_for_response(&mut self, method: &str, id: i64) -> Value {
        let deadline = Instant::now() + LSP_REQUEST_TIMEOUT;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            let message = match self.messages.recv_timeout(remaining) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => {
                    panic!(
                        "LSP request '{method}' with id {id} timed out after {:?}",
                        LSP_REQUEST_TIMEOUT
                    );
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("LSP stdout reader exited before response to '{method}' id {id}");
                }
            };
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return message;
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.write(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    fn notify_without_params(&mut self, method: &str) {
        self.write(json!({
            "jsonrpc": "2.0",
            "method": method,
        }));
    }

    fn write(&mut self, message: Value) {
        let payload = serde_json::to_vec(&message).unwrap();
        let stdin = self.stdin.as_mut().expect("stdin is open");
        write!(stdin, "Content-Length: {}\r\n\r\n", payload.len()).unwrap();
        stdin.write_all(&payload).unwrap();
        stdin.flush().unwrap();
    }
}

fn read_lsp_message(stdout: &mut BufReader<ChildStdout>) -> Option<Value> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let bytes = stdout.read_line(&mut line).unwrap();
        if bytes == 0 {
            return None;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let content_length = content_length.expect("missing Content-Length");
    let mut payload = vec![0; content_length];
    stdout.read_exact(&mut payload).unwrap();
    Some(serde_json::from_slice(&payload).unwrap())
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
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

fn file_uri(path: &Path) -> String {
    Url::from_file_path(path).expect("file URI").to_string()
}

#[test]
fn file_uri_round_trips_special_path_characters() {
    let dir = test_dir("uri special chars");
    let file = dir.join("space #hash.py");
    let uri = file_uri(&file);
    let parsed = Url::parse(&uri).expect("file URI parses");

    assert_eq!(parsed.to_file_path().expect("file path"), file);
    assert!(!uri.contains(' '));
    assert!(!uri.contains('#'));

    let _ = fs::remove_dir_all(dir);
}

fn type_check_input() -> &'static str {
    r#"from typing import Annotated
from string.templatelib import Template
from tdom import html

class User:
    name: str

def Card(*, title: str, count: int, owner: User, labels: list[str], label: str) -> object: ...

def run_json(template: Annotated[Template, "json"]) -> None: ...
def run_yaml(template: Annotated[Template, "yaml"]) -> None: ...
def run_toml(template: Annotated[Template, "toml"]) -> None: ...

def handler(user: User, age: int, name: str) -> None:
    payload_json = t'{{"name": {user}, "label": "{age}", "age": {age}, "tag": {1 + 2}, "note": {user!s}}}'
    run_json(payload_json)
    payload_yaml = t'{user}: {age}\nlabel: "{age}"\n'
    run_yaml(payload_yaml)
    payload_toml = t'{user} = {age}\nlabel = "{age}"\n'
    run_toml(payload_toml)
    payload_tdom = html(t'<{Card} title={age} count={name} owner={age} labels={name} label="Hello {age}" />')
"#
}

fn expected_payload_span(source: &str, assignment: &str, marker: &str) -> (u64, u64, u64) {
    let line = source
        .lines()
        .position(|line| line.trim_start().starts_with(assignment))
        .expect("payload line") as u64;
    let payload_line = source.lines().nth(line as usize).expect("payload line");
    let marker_start = payload_line.find(marker).expect("interpolation marker");
    let interpolation_start = marker.find('{').expect("interpolation start") + 1;
    let interpolation_end = marker.find('}').expect("interpolation end");
    let start = (marker_start + interpolation_start) as u64;
    let end = (marker_start + interpolation_end) as u64;
    (line, start, end)
}

fn assert_diagnostic_span(diagnostic: &Value, span: (u64, u64, u64)) {
    let (line, start, end) = span;
    assert_eq!(diagnostic["range"]["start"]["line"], line);
    assert_eq!(diagnostic["range"]["start"]["character"], start);
    assert_eq!(diagnostic["range"]["end"]["line"], line);
    assert_eq!(diagnostic["range"]["end"]["character"], end);
}

fn diagnostic_with_message<'a>(diagnostics: &'a [&Value], needle: &str) -> &'a Value {
    diagnostics
        .iter()
        .copied()
        .find(|diagnostic| {
            diagnostic["message"]
                .as_str()
                .expect("message")
                .contains(needle)
        })
        .unwrap_or_else(|| panic!("missing diagnostic containing {needle:?}: {diagnostics:?}"))
}

#[test]
fn lsp_reports_interpolation_type_error_from_real_type_checkers() {
    let checkers = available_type_checkers();
    if checkers.is_empty() {
        eprintln!("skipped: no supported type checker found");
        return;
    }

    for checker in checkers {
        let dir = test_dir(checker.name);
        write_type_checker_config_files(&dir, checker.checker);
        let file = dir.join("example.py");
        let source = type_check_input();
        write_file(&file, source);
        let uri = file_uri(&file);

        let mut client = LspClient::start(&dir);
        client.initialize(&dir, &checker);
        client.did_open(&uri, source);

        let diagnostics = client.wait_for_type_diagnostics(&uri);
        let type_diagnostics = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic["code"] == TYPE_DIAGNOSTIC_RULE)
            .collect::<Vec<_>>();
        assert_eq!(
            type_diagnostics.len(),
            11,
            "{} diagnostics: {diagnostics:?}",
            checker.name
        );

        for diagnostic in &type_diagnostics {
            assert_eq!(
                diagnostic["source"],
                format!("t-linter ({})", checker.checker)
            );
            assert_eq!(diagnostic["severity"], 2);
        }

        assert_diagnostic_span(
            diagnostic_with_message(&type_diagnostics, "json value"),
            expected_payload_span(source, "payload_json =", "{user}"),
        );
        assert_diagnostic_span(
            diagnostic_with_message(&type_diagnostics, "json string fragment"),
            expected_payload_span(source, "payload_json =", "\"label\": \"{age}\""),
        );
        assert_diagnostic_span(
            diagnostic_with_message(&type_diagnostics, "yaml mapping key"),
            expected_payload_span(source, "payload_yaml =", "{user}"),
        );
        assert_diagnostic_span(
            diagnostic_with_message(&type_diagnostics, "yaml scalar fragment"),
            expected_payload_span(source, "payload_yaml =", "label: \"{age}\""),
        );
        assert_diagnostic_span(
            diagnostic_with_message(&type_diagnostics, "toml key"),
            expected_payload_span(source, "payload_toml =", "{user}"),
        );
        assert_diagnostic_span(
            diagnostic_with_message(&type_diagnostics, "toml string fragment"),
            expected_payload_span(source, "payload_toml =", "label = \"{age}\""),
        );
        assert_diagnostic_span(
            diagnostic_with_message(&type_diagnostics, "tdom component prop 'title'"),
            expected_payload_span(source, "payload_tdom =", "title={age}"),
        );
        assert_diagnostic_span(
            diagnostic_with_message(&type_diagnostics, "tdom component prop 'count'"),
            expected_payload_span(source, "payload_tdom =", "count={name}"),
        );
        assert_diagnostic_span(
            diagnostic_with_message(&type_diagnostics, "tdom component prop 'owner'"),
            expected_payload_span(source, "payload_tdom =", "owner={age}"),
        );
        assert_diagnostic_span(
            diagnostic_with_message(&type_diagnostics, "tdom component prop 'labels'"),
            expected_payload_span(source, "payload_tdom =", "labels={name}"),
        );
        assert_diagnostic_span(
            diagnostic_with_message(
                &type_diagnostics,
                "tdom component prop 'label' string fragment",
            ),
            expected_payload_span(source, "payload_tdom =", "label=\"Hello {age}\""),
        );

        client.shutdown();
        let _ = fs::remove_dir_all(dir);
    }
}
