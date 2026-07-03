use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LSP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn test_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "t-linter-lsp-e2e-{name}-{}-{nanos}",
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

fn ruff_command() -> String {
    if let Ok(command) = std::env::var("RUFF") {
        return command;
    }

    let output = Command::new("ruff")
        .arg("--version")
        .output()
        .expect("Ruff must be installed and available on PATH for LSP Ruff pipeline e2e tests");
    assert!(
        output.status.success(),
        "Ruff must be installed and available on PATH for LSP Ruff pipeline e2e tests"
    );
    "ruff".to_string()
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
            .args([
                "lsp",
                "--stdio",
                "--ruff-pipeline",
                "--ruff-command",
                &ruff_command(),
                "--ruff-arg",
                "server",
            ])
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

    fn initialize(&mut self, workspace: &Path) {
        let workspace_uri = file_uri(workspace);
        let response = self.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": workspace_uri,
                "capabilities": {
                    "workspace": {
                        "configuration": true,
                        "workspaceFolders": true,
                        "workspaceEdit": {
                            "documentChanges": true
                        }
                    },
                    "textDocument": {
                        "codeAction": {
                            "dataSupport": true,
                            "codeActionLiteralSupport": {
                                "codeActionKind": {
                                    "valueSet": [
                                        "",
                                        "quickfix",
                                        "refactor",
                                        "refactor.rewrite",
                                        "source",
                                        "source.fixAll",
                                        "source.organizeImports"
                                    ]
                                }
                            },
                            "resolveSupport": {
                                "properties": ["edit"]
                            }
                        },
                        "formatting": {}
                    }
                },
                "initializationOptions": {
                    "enableTypeChecking": false,
                    "highlightUntyped": true,
                    "ruffPipeline": {
                        "enabled": true,
                        "command": ruff_command(),
                        "args": ["server"],
                        "settings": {
                            "fixAll": true,
                            "organizeImports": true,
                            "lint": {
                                "select": ["F401", "I"]
                            }
                        }
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

    fn document_formatting(&mut self, uri: &str) -> Value {
        self.request(
            "textDocument/formatting",
            json!({
                "textDocument": { "uri": uri },
                "options": {
                    "tabSize": 4,
                    "insertSpaces": true
                }
            }),
        )
    }

    fn source_fix_all(&mut self, uri: &str, end_line: u32) -> Value {
        self.request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": end_line, "character": 0 }
                },
                "context": {
                    "diagnostics": [],
                    "only": ["source.fixAll.t-linter"],
                    "triggerKind": 2
                }
            }),
        )
    }

    fn rewrite_template(&mut self, uri: &str, line: u32, start: u32, end: u32) -> Value {
        self.request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": line, "character": start },
                    "end": { "line": line, "character": end }
                },
                "context": {
                    "diagnostics": [],
                    "only": ["refactor.rewrite.t-linter"],
                    "triggerKind": 1
                }
            }),
        )
    }

    fn code_action_resolve(&mut self, action: &Value) -> Value {
        self.request("codeAction/resolve", action.clone())
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

fn line_offsets(source: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(index + 1);
        }
    }
    offsets
}

fn byte_offset(source: &str, line_offsets: &[usize], line: u64, character: u64) -> usize {
    let line_start = line_offsets[line as usize];
    let line_end = line_offsets
        .get(line as usize + 1)
        .map(|offset| offset.saturating_sub(1))
        .unwrap_or(source.len());
    line_start + character.min((line_end - line_start) as u64) as usize
}

fn apply_text_edits(source: &str, edits: &[Value]) -> String {
    let offsets = line_offsets(source);
    let mut edits = edits
        .iter()
        .map(|edit| {
            let range = &edit["range"];
            let start = byte_offset(
                source,
                &offsets,
                range["start"]["line"].as_u64().unwrap(),
                range["start"]["character"].as_u64().unwrap(),
            );
            let end = byte_offset(
                source,
                &offsets,
                range["end"]["line"].as_u64().unwrap(),
                range["end"]["character"].as_u64().unwrap(),
            );
            (start, end, edit["newText"].as_str().unwrap().to_string())
        })
        .collect::<Vec<_>>();
    edits.sort_by_key(|(start, end, _)| (*start, *end));
    let mut output = source.to_string();
    for (start, end, replacement) in edits.into_iter().rev() {
        output.replace_range(start..end, &replacement);
    }
    output
}

fn apply_workspace_edit(source: &str, uri: &str, edit: &Value) -> String {
    if let Some(edits) = edit["changes"][uri].as_array() {
        return apply_text_edits(source, edits);
    }

    for change in edit["documentChanges"].as_array().expect("documentChanges") {
        if change["textDocument"]["uri"].as_str() == Some(uri) {
            let edits = change["edits"].as_array().expect("document edits");
            return apply_text_edits(source, edits);
        }
    }
    panic!("workspace edit did not contain edits for {uri}");
}

fn messy_python_with_template() -> &'static str {
    "from typing import Annotated\nfrom string.templatelib import Template\n\ntitle=\"demo\"\nnumbers=[1,2,3]\npayload:Annotated[Template,\"toml\"]=t\"title={title}\"\n"
}

fn expected_python_with_template() -> &'static str {
    "from string.templatelib import Template\nfrom typing import Annotated\n\ntitle = \"demo\"\nnumbers = [1, 2, 3]\npayload: Annotated[Template, \"toml\"] = t\"title = {title}\"\n"
}

fn messy_rewrite_template() -> &'static str {
    "from typing import Annotated\nfrom string.templatelib import Template\n\nclass Card:\n    pass\n\ntitle = \"demo\"\ntemplate: Annotated[Template, \"tdom\"] = t'<{Card} title = {title}><span class = \"badge\" >ok</span></{Card}>'\n"
}

fn expected_rewrite_template() -> &'static str {
    "from typing import Annotated\nfrom string.templatelib import Template\n\nclass Card:\n    pass\n\ntitle = \"demo\"\ntemplate: Annotated[Template, \"tdom\"] = t'<{Card} title={title}><span class=\"badge\">ok</span></{Card}>'\n"
}

#[test]
fn lsp_document_formatting_runs_real_ruff_pipeline_then_t_linter() {
    let dir = test_dir("formatting");
    let file = dir.join("example.py");
    let source = messy_python_with_template();
    write_file(&file, source);
    let uri = file_uri(&file);

    let mut client = LspClient::start(&dir);
    client.initialize(&dir);
    client.did_open(&uri, source);

    let response = client.document_formatting(&uri);
    assert!(
        response.get("error").is_none(),
        "formatting failed: {response}"
    );
    let edits = response["result"].as_array().expect("formatting edits");
    let formatted = apply_text_edits(source, edits);

    assert_eq!(formatted, expected_python_with_template());

    client.shutdown();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn lsp_source_fix_all_runs_real_ruff_pipeline_then_t_linter() {
    let dir = test_dir("source-fix-all");
    let file = dir.join("example.py");
    let source = messy_python_with_template();
    write_file(&file, source);
    let uri = file_uri(&file);

    let mut client = LspClient::start(&dir);
    client.initialize(&dir);
    client.did_open(&uri, source);

    let response = client.source_fix_all(&uri, source.lines().count() as u32);
    assert!(
        response.get("error").is_none(),
        "codeAction failed: {response}"
    );
    let actions = response["result"].as_array().expect("code actions");
    assert_eq!(actions.len(), 1, "expected one t-linter fixAll action");
    assert_eq!(actions[0]["kind"], "source.fixAll.t-linter");
    assert!(
        actions[0].get("edit").is_none(),
        "code action should defer edit until resolve: {}",
        actions[0]
    );

    let resolved = client.code_action_resolve(&actions[0]);
    assert!(
        resolved.get("error").is_none(),
        "codeAction/resolve failed: {resolved}"
    );

    let formatted = apply_workspace_edit(source, &uri, &resolved["result"]["edit"]);
    assert_eq!(formatted, expected_python_with_template());

    client.shutdown();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn lsp_rewrite_code_action_resolves_template_edit() {
    let dir = test_dir("rewrite-resolve");
    let file = dir.join("example.py");
    let source = messy_rewrite_template();
    write_file(&file, source);
    let uri = file_uri(&file);
    let template_line = source
        .lines()
        .position(|line| line.starts_with("template:"))
        .expect("template line") as u32;

    let mut client = LspClient::start(&dir);
    client.initialize(&dir);
    client.did_open(&uri, source);

    let response = client.rewrite_template(&uri, template_line, 55, 65);
    assert!(
        response.get("error").is_none(),
        "rewrite codeAction failed: {response}"
    );
    let actions = response["result"].as_array().expect("code actions");
    assert_eq!(actions.len(), 1, "expected one rewrite action");
    assert_eq!(actions[0]["kind"], "refactor.rewrite.t-linter");
    assert!(
        actions[0].get("edit").is_none(),
        "rewrite action should defer edit until resolve: {}",
        actions[0]
    );

    let resolved = client.code_action_resolve(&actions[0]);
    assert!(
        resolved.get("error").is_none(),
        "rewrite codeAction/resolve failed: {resolved}"
    );

    let formatted = apply_workspace_edit(source, &uri, &resolved["result"]["edit"]);
    assert_eq!(formatted, expected_rewrite_template());

    client.shutdown();
    let _ = fs::remove_dir_all(dir);
}
