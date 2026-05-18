use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, oneshot},
};
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, DocumentFormattingParams,
    TextEdit,
};
use tracing::warn;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const SOURCE_FIX_ALL_RUFF: &str = "source.fixAll.ruff";
const SOURCE_ORGANIZE_IMPORTS_RUFF: &str = "source.organizeImports.ruff";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuffPipelineConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub settings: Value,
}

impl Default for RuffPipelineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: None,
            args: default_ruff_args(),
            settings: Value::Object(Default::default()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuffLaunchConfig {
    pub command: String,
    pub args: Vec<String>,
}

impl RuffLaunchConfig {
    fn new(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            command: command.into(),
            args,
        }
    }
}

#[derive(Debug)]
pub struct RuffPipelineClient {
    inner: Arc<RuffPipelineClientInner>,
    child: Mutex<Child>,
}

#[derive(Debug)]
struct RuffPipelineClientInner {
    stdin: Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>,
}

#[derive(Debug, Clone, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
    #[allow(dead_code)]
    data: Option<Value>,
}

impl RuffPipelineClient {
    pub async fn start(
        config: &RuffPipelineConfig,
        initialize_params: Value,
        workspace_roots: &[PathBuf],
    ) -> Result<Self> {
        let candidates = ruff_launch_candidates(config, workspace_roots);
        if candidates.is_empty() {
            return Err(anyhow!("no Ruff launch candidates available"));
        }

        let explicit = explicit_command(config).is_some();
        let mut last_error = None;
        for launch in candidates {
            match Self::start_with_launch(config, &launch, initialize_params.clone()).await {
                Ok(client) => {
                    tracing::info!(
                        "Started Ruff pipeline server with command: {} {}",
                        launch.command,
                        launch.args.join(" ")
                    );
                    return Ok(client);
                }
                Err(error) if explicit => return Err(error),
                Err(error) => {
                    warn!(
                        "failed to start Ruff candidate {} {}: {error}",
                        launch.command,
                        launch.args.join(" ")
                    );
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("failed to start Ruff pipeline server")))
    }

    async fn start_with_launch(
        config: &RuffPipelineConfig,
        launch: &RuffLaunchConfig,
        initialize_params: Value,
    ) -> Result<Self> {
        let mut child = Command::new(&launch.command)
            .args(&launch.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to spawn Ruff server: {}", launch.command))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Ruff server did not expose stdin"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Ruff server did not expose stdout"))?;
        let inner = Arc::new(RuffPipelineClientInner {
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        });
        let reader_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            loop {
                let message = match read_message(&mut stdout).await {
                    Ok(Some(message)) => message,
                    Ok(None) => break,
                    Err(error) => {
                        tracing::warn!("failed to read Ruff server message: {error}");
                        break;
                    }
                };
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    let result = if let Some(error) = message.get("error") {
                        serde_json::from_value::<RpcError>(error.clone())
                            .map(Err)
                            .unwrap_or_else(|error| {
                                Err(RpcError {
                                    code: -32603,
                                    message: format!("invalid Ruff error response: {error}"),
                                    data: None,
                                })
                            })
                    } else {
                        Ok(message.get("result").cloned().unwrap_or(Value::Null))
                    };
                    if let Some(sender) = reader_inner.pending.lock().await.remove(&id) {
                        let _ = sender.send(result);
                    }
                }
            }
        });

        let client = Self {
            inner,
            child: Mutex::new(child),
        };

        let mut params = initialize_params;
        set_ruff_initialization_options(&mut params, config.settings.clone());
        if let Err(error) = client.request("initialize", params).await {
            client.terminate().await;
            return Err(error);
        }
        if let Err(error) = client.notify("initialized", json!({})).await {
            client.terminate().await;
            return Err(error);
        }
        Ok(client)
    }

    pub async fn did_open(&self, params: Value) -> Result<()> {
        self.notify("textDocument/didOpen", params).await
    }

    pub async fn did_change(&self, params: Value) -> Result<()> {
        self.notify("textDocument/didChange", params).await
    }

    pub async fn did_close(&self, params: Value) -> Result<()> {
        self.notify("textDocument/didClose", params).await
    }

    pub async fn source_fix_all(&self, params: &CodeActionParams) -> Result<Vec<CodeAction>> {
        self.code_actions(params, CodeActionKind::from(SOURCE_FIX_ALL_RUFF))
            .await
    }

    pub async fn organize_imports(&self, params: &CodeActionParams) -> Result<Vec<CodeAction>> {
        self.code_actions(params, CodeActionKind::from(SOURCE_ORGANIZE_IMPORTS_RUFF))
            .await
    }

    pub async fn format(&self, params: &DocumentFormattingParams) -> Result<Vec<TextEdit>> {
        let result = self
            .request("textDocument/formatting", serde_json::to_value(params)?)
            .await?;
        if result.is_null() {
            return Ok(Vec::new());
        }
        serde_json::from_value(result).context("Ruff returned invalid formatting edits")
    }

    pub async fn shutdown(&self) {
        let _ = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", json!({})).await;
        self.terminate().await;
    }

    async fn code_actions(
        &self,
        params: &CodeActionParams,
        only: CodeActionKind,
    ) -> Result<Vec<CodeAction>> {
        let mut params = params.clone();
        params.context.only = Some(vec![only]);
        let result = self
            .request("textDocument/codeAction", serde_json::to_value(params)?)
            .await?;
        if result.is_null() {
            return Ok(Vec::new());
        }
        let actions = serde_json::from_value::<Vec<CodeActionOrCommand>>(result)
            .context("Ruff returned invalid code actions")?;
        let mut resolved = Vec::new();
        for action in actions {
            let CodeActionOrCommand::CodeAction(action) = action else {
                continue;
            };
            let action = if action.edit.is_none() {
                self.resolve_code_action(action).await?
            } else {
                action
            };
            resolved.push(action);
        }
        Ok(resolved)
    }

    async fn resolve_code_action(&self, action: CodeAction) -> Result<CodeAction> {
        let result = self
            .request("codeAction/resolve", serde_json::to_value(action)?)
            .await?;
        serde_json::from_value(result).context("Ruff returned invalid resolved code action")
    }

    async fn terminate(&self) {
        let mut child = self.child.lock().await;
        match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            Ok(Ok(_)) => {}
            _ => {
                let _ = child.kill().await;
            }
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, sender);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(error) = self.write(message).await {
            self.inner.pending.lock().await.remove(&id);
            return Err(error);
        }

        let response = match tokio::time::timeout(REQUEST_TIMEOUT, receiver).await {
            Ok(response) => response
                .with_context(|| format!("Ruff request {method} response channel closed"))?,
            Err(error) => {
                self.inner.pending.lock().await.remove(&id);
                return Err(error).with_context(|| format!("Ruff request {method} timed out"));
            }
        };
        response.map_err(|error| {
            anyhow!(
                "Ruff request {method} failed: {} ({})",
                error.message,
                error.code
            )
        })
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn write(&self, message: Value) -> Result<()> {
        let mut stdin = self.inner.stdin.lock().await;
        write_message(&mut *stdin, &message).await?;
        Ok(())
    }
}

pub fn ruff_launch_candidates(
    config: &RuffPipelineConfig,
    workspace_roots: &[PathBuf],
) -> Vec<RuffLaunchConfig> {
    if let Some(command) = explicit_command(config) {
        return vec![RuffLaunchConfig::new(command, ruff_args(config))];
    }

    let mut candidates = Vec::new();
    for env_name in ["VIRTUAL_ENV", "CONDA_PREFIX"] {
        if let Some(path) = std::env::var_os(env_name)
            .map(PathBuf::from)
            .and_then(|prefix| first_existing_ruff(&environment_ruff_paths(&prefix)))
        {
            candidates.push(RuffLaunchConfig::new(path_string(path), ruff_args(config)));
        }
    }

    for root in workspace_roots {
        for relative in workspace_ruff_relative_paths() {
            let path = root.join(relative);
            if is_executable_file(&path) {
                candidates.push(RuffLaunchConfig::new(path_string(path), ruff_args(config)));
            }
        }
    }

    for root in workspace_roots {
        if is_uv_project(root) {
            candidates.push(RuffLaunchConfig::new("uv", uv_args(root)));
        }
    }

    candidates.push(RuffLaunchConfig::new("ruff", ruff_args(config)));
    dedupe_launch_candidates(candidates)
}

fn explicit_command(config: &RuffPipelineConfig) -> Option<String> {
    config
        .command
        .as_deref()
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(ToOwned::to_owned)
}

fn ruff_args(config: &RuffPipelineConfig) -> Vec<String> {
    if config.args.is_empty() {
        default_ruff_args()
    } else {
        config.args.clone()
    }
}

fn default_ruff_args() -> Vec<String> {
    vec!["server".to_string()]
}

fn uv_args(root: &Path) -> Vec<String> {
    vec![
        "run".to_string(),
        "--project".to_string(),
        root.display().to_string(),
        "--frozen".to_string(),
        "--no-progress".to_string(),
        "ruff".to_string(),
        "server".to_string(),
    ]
}

fn environment_ruff_paths(prefix: &Path) -> Vec<PathBuf> {
    environment_ruff_paths_for_platform(prefix, cfg!(windows))
}

fn environment_ruff_paths_for_platform(prefix: &Path, windows: bool) -> Vec<PathBuf> {
    if windows {
        vec![
            prefix.join("Scripts").join("ruff.exe"),
            prefix.join("bin").join("ruff.exe"),
            prefix.join("bin").join("ruff"),
        ]
    } else {
        vec![prefix.join("bin").join("ruff")]
    }
}

fn workspace_ruff_relative_paths() -> Vec<PathBuf> {
    workspace_ruff_relative_paths_for_platform(cfg!(windows))
}

fn workspace_ruff_relative_paths_for_platform(windows: bool) -> Vec<PathBuf> {
    if windows {
        vec![
            PathBuf::from(".venv").join("Scripts").join("ruff.exe"),
            PathBuf::from("venv").join("Scripts").join("ruff.exe"),
            PathBuf::from(".venv").join("bin").join("ruff.exe"),
            PathBuf::from("venv").join("bin").join("ruff.exe"),
            PathBuf::from(".venv").join("bin").join("ruff"),
            PathBuf::from("venv").join("bin").join("ruff"),
        ]
    } else {
        vec![
            PathBuf::from(".venv").join("bin").join("ruff"),
            PathBuf::from("venv").join("bin").join("ruff"),
        ]
    }
}

fn first_existing_ruff(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|path| is_executable_file(path)).cloned()
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn is_uv_project(root: &Path) -> bool {
    if root.join("uv.lock").is_file() {
        return true;
    }
    let pyproject = root.join("pyproject.toml");
    let Ok(content) = std::fs::read_to_string(pyproject) else {
        return false;
    };
    content.contains("[tool.uv") || content.contains("[dependency-groups")
}

fn dedupe_launch_candidates(candidates: Vec<RuffLaunchConfig>) -> Vec<RuffLaunchConfig> {
    let mut deduped = Vec::new();
    for candidate in candidates {
        if !deduped.iter().any(|existing| existing == &candidate) {
            deduped.push(candidate);
        }
    }
    deduped
}

fn path_string(path: PathBuf) -> String {
    path.display().to_string()
}

fn set_ruff_initialization_options(params: &mut Value, settings: Value) {
    params["initializationOptions"] = json!({ "settings": settings });
}

async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Option<Value>> {
    let mut header = Vec::with_capacity(128);
    let mut byte = [0_u8; 1];
    loop {
        match reader.read_exact(&mut byte).await {
            Ok(_) => header.push(byte[0]),
            Err(error)
                if error.kind() == std::io::ErrorKind::UnexpectedEof && header.is_empty() =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        if header.len() > 16 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Ruff JSON-RPC header exceeded 16KiB",
            ));
        }
    }

    let header = std::str::from_utf8(&header)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "header is not UTF-8"))?;
    let mut content_length = None;
    for line in header.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(value.trim().parse::<usize>().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid Content-Length")
            })?);
        }
    }
    let content_length = content_length.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
    })?;
    let mut payload = vec![0_u8; content_length];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Value,
) -> std::io::Result<()> {
    let payload = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes())
        .await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("set executable bit");
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}

    fn write_executable(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, "").expect("write executable");
        make_executable(path);
    }

    fn with_env_var<T>(name: &str, value: Option<&Path>, f: impl FnOnce() -> T) -> T {
        let previous = std::env::var_os(name);
        match value {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
        let result = f();
        match previous {
            Some(previous) => unsafe { std::env::set_var(name, previous) },
            None => unsafe { std::env::remove_var(name) },
        }
        result
    }

    #[test]
    fn explicit_command_is_first_and_only_candidate() {
        let config = RuffPipelineConfig {
            enabled: true,
            command: Some("/custom/ruff".to_string()),
            args: vec!["server".to_string(), "--preview".to_string()],
            settings: Value::Null,
        };

        let candidates = ruff_launch_candidates(&config, &[]);

        assert_eq!(
            candidates,
            vec![RuffLaunchConfig::new(
                "/custom/ruff",
                vec!["server".to_string(), "--preview".to_string()]
            )]
        );
    }

    #[test]
    fn resolver_orders_env_workspace_uv_and_path_candidates() {
        let virtual_env = TempDir::new().expect("virtual env");
        let conda_env = TempDir::new().expect("conda env");
        let workspace = TempDir::new().expect("workspace");
        write_executable(&virtual_env.path().join("bin").join("ruff"));
        write_executable(&conda_env.path().join("bin").join("ruff"));
        write_executable(&workspace.path().join(".venv").join("bin").join("ruff"));
        write_executable(&workspace.path().join("venv").join("bin").join("ruff"));
        std::fs::write(workspace.path().join("uv.lock"), "").expect("uv lock");

        with_env_var("VIRTUAL_ENV", Some(virtual_env.path()), || {
            with_env_var("CONDA_PREFIX", Some(conda_env.path()), || {
                let candidates = ruff_launch_candidates(
                    &RuffPipelineConfig::default(),
                    &[workspace.path().into()],
                );

                let commands = candidates
                    .iter()
                    .map(|candidate| candidate.command.as_str())
                    .collect::<Vec<_>>();
                assert_eq!(
                    commands[0],
                    virtual_env
                        .path()
                        .join("bin")
                        .join("ruff")
                        .display()
                        .to_string()
                );
                assert_eq!(
                    commands[1],
                    conda_env
                        .path()
                        .join("bin")
                        .join("ruff")
                        .display()
                        .to_string()
                );
                assert_eq!(
                    commands[2],
                    workspace
                        .path()
                        .join(".venv")
                        .join("bin")
                        .join("ruff")
                        .display()
                        .to_string()
                );
                assert_eq!(
                    commands[3],
                    workspace
                        .path()
                        .join("venv")
                        .join("bin")
                        .join("ruff")
                        .display()
                        .to_string()
                );
                assert_eq!(commands[4], "uv");
                assert_eq!(commands[5], "ruff");
                assert_eq!(
                    candidates[4].args,
                    vec![
                        "run",
                        "--project",
                        workspace.path().to_str().expect("utf8 path"),
                        "--frozen",
                        "--no-progress",
                        "ruff",
                        "server"
                    ]
                );
            })
        });
    }

    #[test]
    fn resolver_detects_uv_pyproject_markers() {
        let workspace = TempDir::new().expect("workspace");
        std::fs::write(
            workspace.path().join("pyproject.toml"),
            "[project]\nname = \"demo\"\n\n[dependency-groups]\ndev = [\"ruff\"]\n",
        )
        .expect("pyproject");

        let candidates =
            ruff_launch_candidates(&RuffPipelineConfig::default(), &[workspace.path().into()]);

        assert!(candidates.iter().any(|candidate| {
            candidate.command == "uv"
                && candidate.args
                    == vec![
                        "run",
                        "--project",
                        workspace.path().to_str().expect("utf8 path"),
                        "--frozen",
                        "--no-progress",
                        "ruff",
                        "server",
                    ]
        }));
    }

    #[test]
    fn windows_path_helpers_include_scripts_ruff_exe() {
        let prefix = PathBuf::from("C:/env");
        assert_eq!(
            environment_ruff_paths_for_platform(&prefix, true)[0],
            prefix.join("Scripts").join("ruff.exe")
        );

        let relative_paths = workspace_ruff_relative_paths_for_platform(true);
        assert!(relative_paths.contains(&PathBuf::from(".venv").join("Scripts").join("ruff.exe")));
        assert!(relative_paths.contains(&PathBuf::from("venv").join("Scripts").join("ruff.exe")));
    }
}
