use crate::lsp_helpers::{is_uv_pyproject_table, response_id, server_request_id};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, OwnedMutexGuard, oneshot},
};
use tower_lsp::{
    Client,
    lsp_types::{
        ClientCapabilities, Diagnostic, DiagnosticClientCapabilities, DocumentDiagnosticParams,
        DocumentDiagnosticReport, DocumentDiagnosticReportResult, GeneralClientCapabilities,
        InitializeParams, MessageType, PartialResultParams, PositionEncodingKind,
        TextDocumentClientCapabilities, TextDocumentContentChangeEvent, TextDocumentIdentifier,
        TextDocumentItem, Url, VersionedTextDocumentIdentifier, WorkDoneProgressParams,
        WorkspaceClientCapabilities, WorkspaceFolder,
    },
};
use tracing::warn;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const TESTED_TY_VERSION_RANGE: &str = "0.0.11..=0.0.56";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TypeCheckerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default = "default_ty_args")]
    pub args: Vec<String>,
    #[serde(default)]
    pub python: Option<String>,
}

impl Default for TypeCheckerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: None,
            args: default_ty_args(),
            python: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiatedEncoding {
    Utf8,
    Utf16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TyLaunchConfig {
    pub command: String,
    pub args: Vec<String>,
}

impl TyLaunchConfig {
    fn new(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            command: command.into(),
            args,
        }
    }
}

#[derive(Debug)]
pub struct TypeCheckerClient {
    inner: Arc<TypeCheckerClientInner>,
    child: Mutex<Child>,
    position_encoding: NegotiatedEncoding,
    server_version: Option<String>,
}

#[derive(Debug)]
struct TypeCheckerClientInner {
    stdin: Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>,
    open_documents: Mutex<HashMap<Url, i32>>,
    dead: AtomicBool,
}

#[derive(Debug, Clone, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
    #[allow(dead_code)]
    data: Option<Value>,
}

impl TypeCheckerClientInner {
    async fn fail_pending(&self, message: impl Into<String>) {
        self.dead.store(true, Ordering::SeqCst);
        let message = message.into();
        let pending = std::mem::take(&mut *self.pending.lock().await);
        for sender in pending.into_values() {
            let _ = sender.send(Err(RpcError {
                code: -32000,
                message: message.clone(),
                data: None,
            }));
        }
    }

    async fn respond_method_not_found(&self, message: &Value) {
        let Some(id) = server_request_id(message) else {
            return;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {method}"),
            }
        });
        let result = {
            let mut stdin = self.stdin.lock().await;
            write_message(&mut *stdin, &response).await
        };
        if let Err(error) = result {
            tracing::warn!("failed to write ty MethodNotFound response: {error}");
            self.fail_pending(format!("ty server stdin write failed: {error}"))
                .await;
        }
    }
}

#[derive(Debug)]
pub struct TypeCheckerState {
    pub config: TypeCheckerConfig,
    pub workspace_roots: Vec<PathBuf>,
    pub client: Option<Arc<TypeCheckerClient>>,
    pub consecutive_failures: u8,
    pub disabled: bool,
    pub shutdown: bool,
    startup: Arc<Mutex<()>>,
}

impl TypeCheckerState {
    pub fn new(config: TypeCheckerConfig, workspace_roots: Vec<PathBuf>) -> Self {
        Self {
            config,
            workspace_roots,
            client: None,
            consecutive_failures: 0,
            disabled: false,
            shutdown: false,
            startup: Arc::new(Mutex::new(())),
        }
    }
}

impl TypeCheckerClient {
    pub async fn start(config: &TypeCheckerConfig, workspace_roots: &[PathBuf]) -> Result<Self> {
        let candidates = ty_launch_candidates(config, workspace_roots);
        if candidates.is_empty() {
            return Err(anyhow!("no ty launch candidates available"));
        }

        let explicit = explicit_command(config).is_some();
        let mut last_error = None;
        for launch in candidates {
            match Self::start_with_launch(config, &launch, workspace_roots).await {
                Ok(client) => {
                    tracing::info!(
                        "Started ty server with command: {} {}",
                        launch.command,
                        launch.args.join(" ")
                    );
                    return Ok(client);
                }
                Err(error) if explicit => return Err(error),
                Err(error) => {
                    warn!(
                        "failed to start ty candidate {} {}: {error}",
                        launch.command,
                        launch.args.join(" ")
                    );
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("failed to start ty server")))
    }

    async fn start_with_launch(
        config: &TypeCheckerConfig,
        launch: &TyLaunchConfig,
        workspace_roots: &[PathBuf],
    ) -> Result<Self> {
        let mut child = Command::new(&launch.command)
            .args(&launch.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to spawn ty server: {}", launch.command))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("ty server did not expose stdin"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("ty server did not expose stdout"))?;
        let inner = Arc::new(TypeCheckerClientInner {
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            open_documents: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
        });
        let reader_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let exit_message = loop {
                let message = match read_message(&mut stdout).await {
                    Ok(Some(message)) => message,
                    Ok(None) => break "ty server stdout closed".to_string(),
                    Err(error) => {
                        tracing::warn!("failed to read ty server message: {error}");
                        break format!("ty server reader failed: {error}");
                    }
                };
                if message.get("method").is_some() {
                    reader_inner.respond_method_not_found(&message).await;
                    continue;
                }
                if let Some(id) = response_id(&message) {
                    let result = if let Some(error) = message.get("error") {
                        serde_json::from_value::<RpcError>(error.clone())
                            .map(Err)
                            .unwrap_or_else(|error| {
                                Err(RpcError {
                                    code: -32603,
                                    message: format!("invalid ty error response: {error}"),
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
            };
            reader_inner.fail_pending(exit_message).await;
        });

        let mut client = Self {
            inner,
            child: Mutex::new(child),
            position_encoding: NegotiatedEncoding::Utf16,
            server_version: None,
        };

        let initialize = client
            .request("initialize", initialize_params(config, workspace_roots)?)
            .await;
        let initialize = match initialize {
            Ok(value) => value,
            Err(error) => {
                client.terminate().await;
                return Err(error);
            }
        };
        client.position_encoding = negotiated_position_encoding(&initialize);
        client.server_version = initialize
            .pointer("/serverInfo/version")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let Err(error) = client.notify("initialized", json!({})).await {
            client.terminate().await;
            return Err(error);
        }
        Ok(client)
    }

    pub async fn open_or_update_shadow(&self, uri: &Url, text: &str, version: i32) -> Result<()> {
        enum SyncAction {
            Open(i32),
            Change(i32),
        }

        let action = {
            let mut open_documents = self.inner.open_documents.lock().await;
            match open_documents.get_mut(uri) {
                Some(shadow_version) => {
                    *shadow_version = shadow_version.saturating_add(1).max(version);
                    SyncAction::Change(*shadow_version)
                }
                None => {
                    let shadow_version = version.max(1);
                    open_documents.insert(uri.clone(), shadow_version);
                    SyncAction::Open(shadow_version)
                }
            }
        };

        match action {
            SyncAction::Open(version) => {
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": TextDocumentItem {
                            uri: uri.clone(),
                            language_id: "python".to_string(),
                            version,
                            text: text.to_string(),
                        }
                    }),
                )
                .await
            }
            SyncAction::Change(version) => {
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": VersionedTextDocumentIdentifier {
                            uri: uri.clone(),
                            version,
                        },
                        "contentChanges": [TextDocumentContentChangeEvent {
                            range: None,
                            range_length: None,
                            text: text.to_string(),
                        }]
                    }),
                )
                .await
            }
        }
    }

    pub async fn pull_diagnostics(&self, uri: &Url) -> Result<Vec<Diagnostic>> {
        let params = DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            identifier: None,
            previous_result_id: None,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let result = self
            .request("textDocument/diagnostic", serde_json::to_value(params)?)
            .await?;
        match serde_json::from_value::<DocumentDiagnosticReportResult>(result)
            .context("ty returned invalid diagnostic report")?
        {
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(report)) => {
                Ok(report.full_document_diagnostic_report.items)
            }
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(_))
            | DocumentDiagnosticReportResult::Partial(_) => Ok(Vec::new()),
        }
    }

    pub fn position_encoding(&self) -> NegotiatedEncoding {
        self.position_encoding
    }

    pub fn server_version(&self) -> Option<&str> {
        self.server_version.as_deref()
    }

    async fn is_running(&self) -> bool {
        if self.inner.dead.load(Ordering::SeqCst) {
            return false;
        }
        let status = {
            let mut child = self.child.lock().await;
            child.try_wait()
        };
        match status {
            Ok(None) => true,
            Ok(Some(status)) => {
                tracing::warn!("ty server exited before reuse: {status}");
                self.inner
                    .fail_pending(format!("ty server exited: {status}"))
                    .await;
                false
            }
            Err(error) => {
                tracing::warn!("failed to inspect ty server process: {error}");
                self.inner
                    .fail_pending(format!("failed to inspect ty server process: {error}"))
                    .await;
                false
            }
        }
    }

    pub async fn close_shadow(&self, uri: &Url) -> Result<()> {
        let removed = self.inner.open_documents.lock().await.remove(uri).is_some();
        if !removed {
            return Ok(());
        }
        self.notify(
            "textDocument/didClose",
            json!({
                "textDocument": TextDocumentIdentifier { uri: uri.clone() }
            }),
        )
        .await
    }

    pub async fn shutdown(&self) {
        let _ = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", Value::Null).await;
        self.terminate().await;
    }

    async fn terminate(&self) {
        self.inner.fail_pending("ty server is shutting down").await;
        let mut child = self.child.lock().await;
        match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            Ok(Ok(_)) => {}
            _ => {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            }
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if self.inner.dead.load(Ordering::SeqCst) {
            return Err(anyhow!("ty server is not running"));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().await;
            if self.inner.dead.load(Ordering::SeqCst) {
                return Err(anyhow!("ty server is not running"));
            }
            pending.insert(id, sender);
        }
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
            Ok(response) => {
                response.with_context(|| format!("ty request {method} response channel closed"))?
            }
            Err(error) => {
                self.inner.pending.lock().await.remove(&id);
                return Err(error).with_context(|| format!("ty request {method} timed out"));
            }
        };
        response.map_err(|error| {
            anyhow!(
                "ty request {method} failed: {} ({})",
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
        if self.inner.dead.load(Ordering::SeqCst) {
            return Err(anyhow!("ty server is not running"));
        }
        let result = {
            let mut stdin = self.inner.stdin.lock().await;
            write_message(&mut *stdin, &message).await
        };
        if let Err(error) = result {
            self.inner
                .fail_pending(format!("ty server stdin write failed: {error}"))
                .await;
            return Err(error).context("failed to write ty server message");
        }
        Ok(())
    }
}

pub async fn ensure_type_checker(
    state: &Mutex<TypeCheckerState>,
    lsp_client: &Client,
) -> Option<Arc<TypeCheckerClient>> {
    enum StartupAction {
        Disabled,
        Reuse(Arc<TypeCheckerClient>),
        Start(Arc<Mutex<()>>),
    }

    enum StoreResult {
        Discard,
        Existing(Arc<TypeCheckerClient>),
        Stored,
    }

    loop {
        let action = {
            let state = state.lock().await;
            if state.shutdown || state.disabled || !state.config.enabled {
                StartupAction::Disabled
            } else if let Some(client) = &state.client {
                StartupAction::Reuse(Arc::clone(client))
            } else {
                StartupAction::Start(Arc::clone(&state.startup))
            }
        };

        let (config, workspace_roots, _startup): (
            TypeCheckerConfig,
            Vec<PathBuf>,
            Option<OwnedMutexGuard<()>>,
        ) = match action {
            StartupAction::Disabled => return None,
            StartupAction::Reuse(client) => {
                if client.is_running().await {
                    return Some(client);
                }
                let mut state = state.lock().await;
                if state
                    .client
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &client))
                {
                    state.client = None;
                }
                continue;
            }
            StartupAction::Start(startup) => {
                let startup = startup.lock_owned().await;
                loop {
                    let snapshot = {
                        let state = state.lock().await;
                        if state.shutdown || state.disabled || !state.config.enabled {
                            return None;
                        }
                        match &state.client {
                            Some(client) => Err(Arc::clone(client)),
                            None => Ok((state.config.clone(), state.workspace_roots.clone())),
                        }
                    };
                    match snapshot {
                        Ok((config, workspace_roots)) => {
                            break (config, workspace_roots, Some(startup));
                        }
                        Err(client) if client.is_running().await => return Some(client),
                        Err(client) => {
                            let mut state = state.lock().await;
                            if state
                                .client
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &client))
                            {
                                state.client = None;
                            }
                        }
                    }
                }
            }
        };

        match TypeCheckerClient::start(&config, &workspace_roots).await {
            Ok(client) => {
                let version_warning = client
                    .server_version()
                    .filter(|version| !tested_ty_version(version))
                    .map(ToOwned::to_owned);
                let client = Arc::new(client);
                let store_result = {
                    let mut state = state.lock().await;
                    if state.shutdown
                        || state.disabled
                        || !state.config.enabled
                        || state.config != config
                        || state.workspace_roots != workspace_roots
                    {
                        StoreResult::Discard
                    } else if let Some(existing) = &state.client {
                        StoreResult::Existing(Arc::clone(existing))
                    } else {
                        state.consecutive_failures = 0;
                        state.client = Some(Arc::clone(&client));
                        StoreResult::Stored
                    }
                };

                match store_result {
                    StoreResult::Discard => {
                        client.shutdown().await;
                        return None;
                    }
                    StoreResult::Existing(existing) => {
                        client.shutdown().await;
                        if existing.is_running().await {
                            return Some(existing);
                        }
                        let mut state = state.lock().await;
                        if state
                            .client
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(current, &existing))
                        {
                            state.client = None;
                        }
                    }
                    StoreResult::Stored => {
                        if let Some(version) = version_warning {
                            lsp_client
                                .log_message(
                                    MessageType::WARNING,
                                    format!(
                                        "t-linter interpolation type check was tested against ty {TESTED_TY_VERSION_RANGE}; found {version}"
                                    ),
                                )
                                .await;
                        }
                        return Some(client);
                    }
                }
            }
            Err(error) => {
                let disabled = {
                    let mut state = state.lock().await;
                    if state.shutdown
                        || state.disabled
                        || !state.config.enabled
                        || state.config != config
                        || state.workspace_roots != workspace_roots
                    {
                        return None;
                    }
                    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                    if state.consecutive_failures >= 3 {
                        state.disabled = true;
                    }
                    state.disabled
                };
                lsp_client
                    .log_message(
                        MessageType::WARNING,
                        if disabled {
                            format!(
                                "Disabled t-linter interpolation type check after repeated ty startup failures: {error}"
                            )
                        } else {
                            format!("Failed to start ty for interpolation type check: {error}")
                        },
                    )
                    .await;
                return None;
            }
        }
    }
}

pub fn ty_launch_candidates(
    config: &TypeCheckerConfig,
    workspace_roots: &[PathBuf],
) -> Vec<TyLaunchConfig> {
    if let Some(command) = explicit_command(config) {
        return vec![TyLaunchConfig::new(command, ty_args(config))];
    }

    let mut candidates = Vec::new();
    for env_name in ["VIRTUAL_ENV", "CONDA_PREFIX"] {
        if let Some(path) = std::env::var_os(env_name)
            .map(PathBuf::from)
            .and_then(|prefix| first_existing_ty(&environment_ty_paths(&prefix)))
        {
            candidates.push(TyLaunchConfig::new(path_string(path), ty_args(config)));
        }
    }

    for root in workspace_roots {
        for relative in workspace_ty_relative_paths() {
            let path = root.join(relative);
            if is_executable_file(&path) {
                candidates.push(TyLaunchConfig::new(path_string(path), ty_args(config)));
            }
        }
    }

    for root in workspace_roots {
        if is_uv_project(root) {
            candidates.push(TyLaunchConfig::new("uv", uv_args(root)));
        }
    }

    candidates.push(TyLaunchConfig::new("ty", ty_args(config)));
    dedupe_launch_candidates(candidates)
}

fn initialize_params(config: &TypeCheckerConfig, workspace_roots: &[PathBuf]) -> Result<Value> {
    let root_uri = workspace_roots
        .first()
        .and_then(|root| Url::from_file_path(root).ok());
    let workspace_folders = workspace_roots
        .iter()
        .filter_map(|root| {
            let uri = Url::from_file_path(root).ok()?;
            Some(WorkspaceFolder {
                name: root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("workspace")
                    .to_string(),
                uri,
            })
        })
        .collect::<Vec<_>>();
    let params = InitializeParams {
        process_id: None,
        root_uri,
        capabilities: ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                workspace_folders: Some(true),
                ..Default::default()
            }),
            text_document: Some(TextDocumentClientCapabilities {
                diagnostic: Some(DiagnosticClientCapabilities {
                    dynamic_registration: Some(false),
                    related_document_support: Some(false),
                }),
                ..Default::default()
            }),
            general: Some(GeneralClientCapabilities {
                position_encodings: Some(vec![
                    PositionEncodingKind::UTF8,
                    PositionEncodingKind::UTF16,
                ]),
                ..Default::default()
            }),
            ..Default::default()
        },
        workspace_folders: Some(workspace_folders),
        initialization_options: config.python.as_ref().map(|python| {
            json!({
                "settings": {
                    "python": python
                }
            })
        }),
        ..Default::default()
    };
    serde_json::to_value(params).context("failed to serialize ty initialize params")
}

fn negotiated_position_encoding(initialize_result: &Value) -> NegotiatedEncoding {
    match initialize_result
        .pointer("/capabilities/positionEncoding")
        .and_then(Value::as_str)
    {
        Some("utf-8") => NegotiatedEncoding::Utf8,
        _ => NegotiatedEncoding::Utf16,
    }
}

fn tested_ty_version(version: &str) -> bool {
    let Some((major, minor, patch)) = parse_semver_triplet(version) else {
        return false;
    };
    major == 0 && minor == 0 && (11..=56).contains(&patch)
}

fn parse_semver_triplet(version: &str) -> Option<(u64, u64, u64)> {
    let version = version.strip_prefix("ty ").unwrap_or(version);
    let mut parts = version
        .split(|character: char| !character.is_ascii_digit() && character != '.')
        .find(|part| !part.is_empty())?
        .split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

fn explicit_command(config: &TypeCheckerConfig) -> Option<String> {
    config
        .command
        .as_deref()
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(ToOwned::to_owned)
}

fn ty_args(config: &TypeCheckerConfig) -> Vec<String> {
    if config.args.is_empty() {
        default_ty_args()
    } else {
        config.args.clone()
    }
}

fn default_ty_args() -> Vec<String> {
    vec!["server".to_string()]
}

fn uv_args(root: &Path) -> Vec<String> {
    vec![
        "run".to_string(),
        "--project".to_string(),
        root.display().to_string(),
        "--frozen".to_string(),
        "--no-progress".to_string(),
        "ty".to_string(),
        "server".to_string(),
    ]
}

fn environment_ty_paths(prefix: &Path) -> Vec<PathBuf> {
    environment_ty_paths_for_platform(prefix, cfg!(windows))
}

fn environment_ty_paths_for_platform(prefix: &Path, windows: bool) -> Vec<PathBuf> {
    if windows {
        vec![
            prefix.join("Scripts").join("ty.exe"),
            prefix.join("bin").join("ty.exe"),
            prefix.join("bin").join("ty"),
        ]
    } else {
        vec![prefix.join("bin").join("ty")]
    }
}

fn workspace_ty_relative_paths() -> Vec<PathBuf> {
    workspace_ty_relative_paths_for_platform(cfg!(windows))
}

fn workspace_ty_relative_paths_for_platform(windows: bool) -> Vec<PathBuf> {
    if windows {
        vec![
            PathBuf::from(".venv").join("Scripts").join("ty.exe"),
            PathBuf::from("venv").join("Scripts").join("ty.exe"),
            PathBuf::from(".venv").join("bin").join("ty.exe"),
            PathBuf::from("venv").join("bin").join("ty.exe"),
            PathBuf::from(".venv").join("bin").join("ty"),
            PathBuf::from("venv").join("bin").join("ty"),
        ]
    } else {
        vec![
            PathBuf::from(".venv").join("bin").join("ty"),
            PathBuf::from("venv").join("bin").join("ty"),
        ]
    }
}

fn first_existing_ty(paths: &[PathBuf]) -> Option<PathBuf> {
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
    content.lines().any(is_uv_pyproject_table)
}

fn dedupe_launch_candidates(candidates: Vec<TyLaunchConfig>) -> Vec<TyLaunchConfig> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for candidate in candidates {
        let key = (candidate.command.clone(), candidate.args.clone());
        if seen.insert(key) {
            deduped.push(candidate);
        }
    }
    deduped
}

fn path_string(path: PathBuf) -> String {
    path.display().to_string()
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
                "ty JSON-RPC header exceeded 16KiB",
            ));
        }
    }

    let header = std::str::from_utf8(&header)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut content_length = None;
    for line in header.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length =
                Some(value.trim().parse::<usize>().map_err(|error| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                })?);
        }
    }

    let Some(content_length) = content_length else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "ty JSON-RPC message missing Content-Length",
        ));
    };

    let mut payload = vec![0; content_length];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &Value) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
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

    #[test]
    fn type_checker_config_defaults_to_disabled() {
        let config: TypeCheckerConfig =
            serde_json::from_value(json!({"enabled": true, "command": "/tmp/ty"})).unwrap();
        assert!(config.enabled);
        assert_eq!(config.command.as_deref(), Some("/tmp/ty"));
        assert_eq!(config.args, default_ty_args());
        assert_eq!(TypeCheckerConfig::default().enabled, false);
    }

    #[test]
    fn ty_launch_candidates_use_explicit_command_first() {
        let config = TypeCheckerConfig {
            enabled: true,
            command: Some("/opt/ty".to_string()),
            args: vec!["server".to_string()],
            python: None,
        };
        assert_eq!(
            ty_launch_candidates(&config, &[]),
            vec![TyLaunchConfig::new("/opt/ty", vec!["server".to_string()])]
        );
    }

    #[test]
    fn ty_launch_candidates_include_workspace_uv_and_path_fallback() {
        let temp = TempDir::new().expect("tempdir");
        std::fs::write(temp.path().join("uv.lock"), "").expect("uv lock");
        let candidates = ty_launch_candidates(&TypeCheckerConfig::default(), &[temp.path().into()]);
        assert!(candidates.iter().any(|candidate| candidate.command == "uv"));
        assert!(candidates.iter().any(|candidate| candidate.command == "ty"));
    }

    #[test]
    fn ty_launch_candidates_ignore_uvicorn_pyproject_table() {
        let temp = TempDir::new().expect("tempdir");
        std::fs::write(
            temp.path().join("pyproject.toml"),
            "[project]\nname = \"demo\"\n\n[tool.uvicorn]\nreload = true\n",
        )
        .expect("pyproject");
        let candidates = ty_launch_candidates(&TypeCheckerConfig::default(), &[temp.path().into()]);
        assert!(!candidates.iter().any(|candidate| candidate.command == "uv"));
        assert!(candidates.iter().any(|candidate| candidate.command == "ty"));
    }

    #[test]
    fn ty_version_range_accepts_tested_versions_only() {
        assert!(tested_ty_version("0.0.11"));
        assert!(tested_ty_version("ty 0.0.56"));
        assert!(!tested_ty_version("0.0.10"));
        assert!(!tested_ty_version("0.0.57"));
        assert!(!tested_ty_version("1.0.0"));
    }

    #[test]
    fn negotiated_encoding_prefers_utf8_response() {
        assert_eq!(
            negotiated_position_encoding(&json!({
                "capabilities": {
                    "positionEncoding": "utf-8"
                }
            })),
            NegotiatedEncoding::Utf8
        );
        assert_eq!(
            negotiated_position_encoding(&json!({})),
            NegotiatedEncoding::Utf16
        );
    }

    #[test]
    fn json_rpc_messages_with_methods_are_not_responses() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "workspace/configuration",
            "params": {}
        });
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": null
        });

        assert_eq!(response_id(&request), None);
        assert_eq!(server_request_id(&request), Some(json!(1)));
        assert_eq!(response_id(&response), Some(1));
        assert_eq!(server_request_id(&response), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_liveness_detects_exited_child() {
        let mut child = Command::new(std::env::current_exe().expect("current exe"))
            .arg("--help")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived child");
        let stdin = child.stdin.take().expect("child stdin");
        let client = TypeCheckerClient {
            inner: Arc::new(TypeCheckerClientInner {
                stdin: Mutex::new(stdin),
                next_id: AtomicU64::new(1),
                pending: Mutex::new(HashMap::new()),
                open_documents: Mutex::new(HashMap::new()),
                dead: AtomicBool::new(false),
            }),
            child: Mutex::new(child),
            position_encoding: NegotiatedEncoding::Utf16,
            server_version: None,
        };

        tokio::time::timeout(Duration::from_secs(2), async {
            while client.is_running().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child exits");
    }
}
