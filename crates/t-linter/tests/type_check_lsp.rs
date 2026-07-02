use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
            .stderr(Stdio::piped())
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

    fn initialize(&mut self, workspace: &Path, ty_command: &str) {
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
                        "command": ty_command,
                        "args": ["server"]
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
    format!("file://{}", path.display())
}

fn type_check_input() -> &'static str {
    r#"from typing import Annotated
from string.templatelib import Template

class User:
    name: str

def run_json(template: Annotated[Template, "json"]) -> None: ...

def handler(user: User, age: int) -> None:
    payload = t'{{"name": {user}, "age": {age}, "tag": {1 + 2}, "note": {user!s}}}'
    run_json(payload)
"#
}

fn expected_user_span(source: &str) -> (u64, u64, u64) {
    let line = source
        .lines()
        .position(|line| line.trim_start().starts_with("payload ="))
        .expect("payload line") as u64;
    let payload_line = source.lines().nth(line as usize).expect("payload line");
    let start = payload_line.find("{user}").expect("user interpolation") as u64 + 1;
    (line, start, start + "user".len() as u64)
}

#[test]
fn lsp_reports_interpolation_type_error_from_real_ty() {
    let Some(ty_command) = ty_command() else {
        eprintln!("skipped: ty not found");
        return;
    };
    let dir = test_dir("diagnostics");
    let file = dir.join("example.py");
    let source = type_check_input();
    write_file(&file, source);
    let uri = file_uri(&file);

    let mut client = LspClient::start(&dir);
    client.initialize(&dir, &ty_command);
    client.did_open(&uri, source);

    let diagnostics = client.wait_for_type_diagnostics(&uri);
    let type_diagnostics = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic["code"] == TYPE_DIAGNOSTIC_RULE)
        .collect::<Vec<_>>();
    assert_eq!(type_diagnostics.len(), 1, "diagnostics: {diagnostics:?}");

    let diagnostic = type_diagnostics[0];
    assert_eq!(diagnostic["source"], "t-linter (ty)");
    assert_eq!(diagnostic["severity"], 2);
    assert!(
        diagnostic["message"]
            .as_str()
            .expect("message")
            .contains("json template"),
        "diagnostic: {diagnostic:?}"
    );

    let (line, start, end) = expected_user_span(source);
    assert_eq!(diagnostic["range"]["start"]["line"], line);
    assert_eq!(diagnostic["range"]["start"]["character"], start);
    assert_eq!(diagnostic["range"]["end"]["line"], line);
    assert_eq!(diagnostic["range"]["end"]["character"], end);

    client.shutdown();
    let _ = fs::remove_dir_all(dir);
}
