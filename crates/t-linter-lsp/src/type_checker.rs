use crate::lsp_helpers::{is_uv_pyproject_table, response_id, server_request_id};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
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
    sync::{Mutex, Notify, OwnedMutexGuard, oneshot},
};
use tower_lsp::{
    Client,
    lsp_types::{
        ClientCapabilities, Diagnostic, DiagnosticClientCapabilities, DocumentDiagnosticParams,
        DocumentDiagnosticReport, DocumentDiagnosticReportResult, GeneralClientCapabilities,
        InitializeParams, MessageType, PartialResultParams, PositionEncodingKind,
        PublishDiagnosticsParams, TextDocumentClientCapabilities, TextDocumentContentChangeEvent,
        TextDocumentIdentifier, TextDocumentItem, Url, VersionedTextDocumentIdentifier,
        WorkDoneProgressParams, WorkspaceClientCapabilities, WorkspaceFolder,
    },
};
use tracing::warn;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const TESTED_TY_VERSION_RANGE: &str = "0.0.11..=0.0.56";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TypeCheckerBackend {
    #[default]
    Ty,
    Pyright,
    Pyrefly,
}

impl TypeCheckerBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ty => "ty",
            Self::Pyright => "pyright",
            Self::Pyrefly => "pyrefly",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Ty => "ty",
            Self::Pyright => "Pyright",
            Self::Pyrefly => "Pyrefly",
        }
    }

    fn default_command(self) -> &'static str {
        match self {
            Self::Ty => "ty",
            Self::Pyright => "pyright-langserver",
            Self::Pyrefly => "pyrefly",
        }
    }

    fn default_args(self) -> Vec<String> {
        match self {
            Self::Ty => vec!["server".to_string()],
            Self::Pyright => vec!["--stdio".to_string()],
            Self::Pyrefly => vec!["lsp".to_string()],
        }
    }

    fn diagnostic_transport(self) -> DiagnosticTransport {
        match self {
            Self::Ty => DiagnosticTransport::Pull,
            Self::Pyright | Self::Pyrefly => DiagnosticTransport::Publish,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticTransport {
    Pull,
    Publish,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TypeCheckerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub checker: TypeCheckerBackend,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub python: Option<String>,
}

impl Default for TypeCheckerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            checker: TypeCheckerBackend::Ty,
            command: None,
            args: Vec::new(),
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
pub struct TypeCheckerLaunchConfig {
    pub command: String,
    pub args: Vec<String>,
}

impl TypeCheckerLaunchConfig {
    fn new(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            command: command.into(),
            args,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PythonLaunchConfig {
    pub command: String,
    pub args: Vec<String>,
}

impl PythonLaunchConfig {
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
    backend: TypeCheckerBackend,
    position_encoding: NegotiatedEncoding,
    server_version: Option<String>,
}

#[derive(Debug)]
struct TypeCheckerClientInner {
    stdin: Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>,
    open_documents: Mutex<HashMap<Url, i32>>,
    published_diagnostics: Mutex<HashMap<Url, PublishedDiagnostics>>,
    diagnostics_notify: Notify,
    dead: AtomicBool,
}

#[derive(Debug, Clone)]
struct PublishedDiagnostics {
    diagnostics: Vec<Diagnostic>,
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

    async fn record_published_diagnostics(&self, params: Value) {
        let Ok(params) = serde_json::from_value::<PublishDiagnosticsParams>(params) else {
            tracing::debug!("Ignoring invalid type checker publishDiagnostics payload");
            return;
        };
        self.published_diagnostics.lock().await.insert(
            params.uri,
            PublishedDiagnostics {
                diagnostics: params.diagnostics,
            },
        );
        self.diagnostics_notify.notify_waiters();
    }

    async fn respond_to_server_request(
        &self,
        message: &Value,
        backend: TypeCheckerBackend,
        workspace_folders: &[WorkspaceFolder],
        python: Option<&str>,
    ) {
        let Some(id) = server_request_id(message) else {
            return;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let response = match method {
            "workspace/configuration" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": workspace_configuration_response(message, backend, python),
            }),
            "workspace/workspaceFolders" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": workspace_folders,
            }),
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create"
            | "workspace/diagnostic/refresh"
            | "workspace/inlayHint/refresh"
            | "workspace/semanticTokens/refresh" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": null,
            }),
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("Method not found: {method}"),
                }
            }),
        };
        let result = {
            let mut stdin = self.stdin.lock().await;
            write_message(&mut *stdin, &response).await
        };
        if let Err(error) = result {
            tracing::warn!("failed to write type checker response: {error}");
            self.fail_pending(format!("type checker stdin write failed: {error}"))
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
        let candidates = type_checker_launch_candidates(config, workspace_roots);
        if candidates.is_empty() {
            return Err(anyhow!(
                "no {} launch candidates available",
                config.checker.as_str()
            ));
        }

        let explicit = explicit_command(config).is_some();
        let mut last_error = None;
        for launch in candidates {
            match Self::start_with_launch(config, &launch, workspace_roots).await {
                Ok(client) => {
                    tracing::info!(
                        "Started {} server with command: {} {}",
                        config.checker.display_name(),
                        launch.command,
                        launch.args.join(" ")
                    );
                    return Ok(client);
                }
                Err(error) if explicit => return Err(error),
                Err(error) => {
                    warn!(
                        "failed to start {} candidate {} {}: {error}",
                        config.checker.display_name(),
                        launch.command,
                        launch.args.join(" ")
                    );
                    last_error = Some(error);
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| anyhow!("failed to start {} server", config.checker.as_str())))
    }

    async fn start_with_launch(
        config: &TypeCheckerConfig,
        launch: &TypeCheckerLaunchConfig,
        workspace_roots: &[PathBuf],
    ) -> Result<Self> {
        let mut child = Command::new(&launch.command)
            .args(&launch.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn {} server: {}",
                    config.checker.as_str(),
                    launch.command
                )
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("{} server did not expose stdin", config.checker.as_str()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("{} server did not expose stdout", config.checker.as_str()))?;
        let workspace_folders = workspace_folders(workspace_roots);
        let inner = Arc::new(TypeCheckerClientInner {
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            open_documents: Mutex::new(HashMap::new()),
            published_diagnostics: Mutex::new(HashMap::new()),
            diagnostics_notify: Notify::new(),
            dead: AtomicBool::new(false),
        });
        let reader_inner = Arc::clone(&inner);
        let backend = config.checker;
        let python = config.python.clone();
        tokio::spawn(async move {
            let exit_message = loop {
                let message = match read_message(&mut stdout).await {
                    Ok(Some(message)) => message,
                    Ok(None) => break format!("{} server stdout closed", backend.as_str()),
                    Err(error) => {
                        tracing::warn!(
                            "failed to read {} server message: {error}",
                            backend.as_str()
                        );
                        break format!("{} server reader failed: {error}", backend.as_str());
                    }
                };
                if let Some(method) = message.get("method").and_then(Value::as_str) {
                    if server_request_id(&message).is_some() {
                        reader_inner
                            .respond_to_server_request(
                                &message,
                                backend,
                                &workspace_folders,
                                python.as_deref(),
                            )
                            .await;
                    } else if method == "textDocument/publishDiagnostics"
                        && let Some(params) = message.get("params")
                    {
                        reader_inner
                            .record_published_diagnostics(params.clone())
                            .await;
                    }
                    continue;
                }
                if let Some(id) = response_id(&message) {
                    let result = if let Some(error) = message.get("error") {
                        serde_json::from_value::<RpcError>(error.clone())
                            .map(Err)
                            .unwrap_or_else(|error| {
                                Err(RpcError {
                                    code: -32603,
                                    message: format!(
                                        "invalid {} error response: {error}",
                                        backend.as_str()
                                    ),
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
            backend: config.checker,
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

        if self.backend.diagnostic_transport() == DiagnosticTransport::Publish {
            self.inner.published_diagnostics.lock().await.remove(uri);
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
        if self.backend.diagnostic_transport() == DiagnosticTransport::Publish {
            return self.wait_for_published_diagnostics(uri).await;
        }

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
        match serde_json::from_value::<DocumentDiagnosticReportResult>(result).with_context(
            || {
                format!(
                    "{} returned invalid diagnostic report",
                    self.backend.as_str()
                )
            },
        )? {
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(report)) => {
                Ok(report.full_document_diagnostic_report.items)
            }
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(_))
            | DocumentDiagnosticReportResult::Partial(_) => Ok(Vec::new()),
        }
    }

    async fn wait_for_published_diagnostics(&self, uri: &Url) -> Result<Vec<Diagnostic>> {
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            loop {
                let notified = self.inner.diagnostics_notify.notified();
                if let Some(published) = self.inner.published_diagnostics.lock().await.get(uri) {
                    return Ok(published.diagnostics.clone());
                }
                notified.await;
            }
        })
        .await
        .with_context(|| format!("{} publishDiagnostics timed out", self.backend.as_str()))?
    }

    pub fn position_encoding(&self) -> NegotiatedEncoding {
        self.position_encoding
    }

    pub fn backend(&self) -> TypeCheckerBackend {
        self.backend
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
                tracing::warn!(
                    "{} server exited before reuse: {status}",
                    self.backend.as_str()
                );
                self.inner
                    .fail_pending(format!("{} server exited: {status}", self.backend.as_str()))
                    .await;
                false
            }
            Err(error) => {
                tracing::warn!(
                    "failed to inspect {} server process: {error}",
                    self.backend.as_str()
                );
                self.inner
                    .fail_pending(format!(
                        "failed to inspect {} server process: {error}",
                        self.backend.as_str()
                    ))
                    .await;
                false
            }
        }
    }

    pub async fn close_shadow(&self, uri: &Url) -> Result<()> {
        let removed = self.inner.open_documents.lock().await.remove(uri).is_some();
        if self.backend.diagnostic_transport() == DiagnosticTransport::Publish {
            self.inner.published_diagnostics.lock().await.remove(uri);
        }
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
        self.inner
            .fail_pending(format!("{} server is shutting down", self.backend.as_str()))
            .await;
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
            return Err(anyhow!("{} server is not running", self.backend.as_str()));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().await;
            if self.inner.dead.load(Ordering::SeqCst) {
                return Err(anyhow!("{} server is not running", self.backend.as_str()));
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
            Ok(response) => response.with_context(|| {
                format!(
                    "{} request {method} response channel closed",
                    self.backend.as_str()
                )
            })?,
            Err(error) => {
                self.inner.pending.lock().await.remove(&id);
                return Err(error).with_context(|| {
                    format!("{} request {method} timed out", self.backend.as_str())
                });
            }
        };
        response.map_err(|error| {
            anyhow!(
                "{} request {method} failed: {} ({})",
                self.backend.as_str(),
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
            return Err(anyhow!("{} server is not running", self.backend.as_str()));
        }
        let result = {
            let mut stdin = self.inner.stdin.lock().await;
            write_message(&mut *stdin, &message).await
        };
        if let Err(error) = result {
            self.inner
                .fail_pending(format!(
                    "{} server stdin write failed: {error}",
                    self.backend.as_str()
                ))
                .await;
            return Err(error).with_context(|| {
                format!("failed to write {} server message", self.backend.as_str())
            });
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
                let version_warning = (client.backend() == TypeCheckerBackend::Ty)
                    .then(|| {
                        client
                            .server_version()
                            .filter(|version| !tested_ty_version(version))
                            .map(ToOwned::to_owned)
                    })
                    .flatten();
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
                                "Disabled t-linter interpolation type check after repeated {} startup failures: {error}",
                                config.checker.as_str()
                            )
                        } else {
                            format!(
                                "Failed to start {} for interpolation type check: {error}",
                                config.checker.as_str()
                            )
                        },
                    )
                    .await;
                return None;
            }
        }
    }
}

pub fn type_checker_launch_candidates(
    config: &TypeCheckerConfig,
    workspace_roots: &[PathBuf],
) -> Vec<TypeCheckerLaunchConfig> {
    if let Some(command) = explicit_command(config) {
        return vec![TypeCheckerLaunchConfig::new(command, checker_args(config))];
    }

    let mut candidates = Vec::new();
    for env_name in ["VIRTUAL_ENV", "CONDA_PREFIX"] {
        if let Some(path) = std::env::var_os(env_name)
            .map(PathBuf::from)
            .and_then(|prefix| {
                first_existing_checker(&environment_checker_paths(&prefix, config.checker))
            })
        {
            candidates.push(TypeCheckerLaunchConfig::new(
                path_string(path),
                checker_args(config),
            ));
        }
    }

    for root in workspace_roots {
        for relative in workspace_checker_relative_paths(config.checker) {
            let path = root.join(relative);
            if is_executable_file(&path) {
                candidates.push(TypeCheckerLaunchConfig::new(
                    path_string(path),
                    checker_args(config),
                ));
            }
        }
    }

    for root in workspace_roots {
        if is_uv_project(root) {
            candidates.push(TypeCheckerLaunchConfig::new(
                "uv",
                uv_args(root, config.checker),
            ));
        }
    }

    candidates.push(TypeCheckerLaunchConfig::new(
        config.checker.default_command(),
        checker_args(config),
    ));
    dedupe_launch_candidates(candidates)
}

pub(crate) fn python_inline_script_launch_candidates(
    workspace_roots: &[PathBuf],
    explicit_command: Option<&str>,
    env_command_name: &str,
    script: &str,
) -> Vec<PythonLaunchConfig> {
    let helper_args = || vec!["-c".to_string(), script.to_string()];
    if let Some(command) = explicit_command
        .map(str::trim)
        .filter(|command| !command.is_empty())
    {
        return vec![PythonLaunchConfig::new(command, helper_args())];
    }
    if let Ok(command) = std::env::var(env_command_name)
        && !command.trim().is_empty()
    {
        return vec![PythonLaunchConfig::new(command, helper_args())];
    }

    let mut candidates = Vec::new();
    for env_name in ["VIRTUAL_ENV", "CONDA_PREFIX"] {
        if let Some(path) = std::env::var_os(env_name)
            .map(PathBuf::from)
            .and_then(|prefix| first_existing_python(&environment_python_paths(&prefix)))
        {
            candidates.push(PythonLaunchConfig::new(path_string(path), helper_args()));
        }
    }
    for root in workspace_roots {
        for relative in workspace_python_relative_paths() {
            let path = root.join(relative);
            if is_executable_file(&path) {
                candidates.push(PythonLaunchConfig::new(path_string(path), helper_args()));
            }
        }
    }
    for root in workspace_roots {
        if is_uv_project(root) {
            candidates.push(PythonLaunchConfig::new(
                "uv",
                vec![
                    "run".to_string(),
                    "--project".to_string(),
                    path_string(root.clone()),
                    "--frozen".to_string(),
                    "--no-progress".to_string(),
                    "python".to_string(),
                    "-c".to_string(),
                    script.to_string(),
                ],
            ));
        }
    }
    candidates.push(PythonLaunchConfig::new("python3", helper_args()));
    candidates.push(PythonLaunchConfig::new("python", helper_args()));
    dedupe_python_launch_candidates(candidates)
}

fn initialize_params(config: &TypeCheckerConfig, workspace_roots: &[PathBuf]) -> Result<Value> {
    let root_uri = workspace_roots
        .first()
        .and_then(|root| Url::from_file_path(root).ok());
    let params = InitializeParams {
        process_id: None,
        root_uri,
        capabilities: ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                workspace_folders: Some(true),
                ..Default::default()
            }),
            text_document: Some(TextDocumentClientCapabilities {
                diagnostic: (config.checker.diagnostic_transport() == DiagnosticTransport::Pull)
                    .then_some(DiagnosticClientCapabilities {
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
        workspace_folders: Some(workspace_folders(workspace_roots)),
        initialization_options: initialization_options(config),
        ..Default::default()
    };
    serde_json::to_value(params).with_context(|| {
        format!(
            "failed to serialize {} initialize params",
            config.checker.as_str()
        )
    })
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

fn checker_args(config: &TypeCheckerConfig) -> Vec<String> {
    if config.args.is_empty() {
        config.checker.default_args()
    } else {
        config.args.clone()
    }
}

fn uv_args(root: &Path, backend: TypeCheckerBackend) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--project".to_string(),
        root.display().to_string(),
        "--frozen".to_string(),
        "--no-progress".to_string(),
        backend.default_command().to_string(),
    ];
    args.extend(backend.default_args());
    args
}

fn environment_checker_paths(prefix: &Path, backend: TypeCheckerBackend) -> Vec<PathBuf> {
    environment_checker_paths_for_platform(prefix, backend, cfg!(windows))
}

fn environment_checker_paths_for_platform(
    prefix: &Path,
    backend: TypeCheckerBackend,
    windows: bool,
) -> Vec<PathBuf> {
    let executable_names = checker_executable_names(backend, windows);
    if windows {
        executable_names
            .iter()
            .flat_map(|name| {
                [
                    prefix.join("Scripts").join(name),
                    prefix.join("bin").join(name),
                ]
            })
            .collect()
    } else {
        executable_names
            .iter()
            .map(|name| prefix.join("bin").join(name))
            .collect()
    }
}

fn workspace_checker_relative_paths(backend: TypeCheckerBackend) -> Vec<PathBuf> {
    workspace_checker_relative_paths_for_platform(backend, cfg!(windows))
}

fn environment_python_paths(prefix: &Path) -> Vec<PathBuf> {
    environment_python_paths_for_platform(prefix, cfg!(windows))
}

fn environment_python_paths_for_platform(prefix: &Path, windows: bool) -> Vec<PathBuf> {
    let executable_names = python_executable_names(windows);
    if windows {
        executable_names
            .iter()
            .flat_map(|name| {
                [
                    prefix.join("Scripts").join(name),
                    prefix.join("bin").join(name),
                ]
            })
            .collect()
    } else {
        executable_names
            .iter()
            .map(|name| prefix.join("bin").join(name))
            .collect()
    }
}

fn workspace_python_relative_paths() -> Vec<PathBuf> {
    workspace_python_relative_paths_for_platform(cfg!(windows))
}

fn workspace_python_relative_paths_for_platform(windows: bool) -> Vec<PathBuf> {
    let executable_names = python_executable_names(windows);
    let envs = [".venv", "venv"];
    if windows {
        envs.iter()
            .flat_map(|env| {
                executable_names.iter().flat_map(move |name| {
                    [
                        PathBuf::from(env).join("Scripts").join(name),
                        PathBuf::from(env).join("bin").join(name),
                    ]
                })
            })
            .collect()
    } else {
        envs.iter()
            .flat_map(|env| {
                executable_names
                    .iter()
                    .map(move |name| PathBuf::from(env).join("bin").join(name))
            })
            .collect()
    }
}

fn workspace_checker_relative_paths_for_platform(
    backend: TypeCheckerBackend,
    windows: bool,
) -> Vec<PathBuf> {
    let executable_names = checker_executable_names(backend, windows);
    let envs = [".venv", "venv"];
    if windows {
        envs.iter()
            .flat_map(|env| {
                executable_names.iter().flat_map(move |name| {
                    [
                        PathBuf::from(env).join("Scripts").join(name),
                        PathBuf::from(env).join("bin").join(name),
                    ]
                })
            })
            .collect()
    } else {
        envs.iter()
            .flat_map(|env| {
                executable_names
                    .iter()
                    .map(move |name| PathBuf::from(env).join("bin").join(name))
            })
            .collect()
    }
}

fn checker_executable_names(backend: TypeCheckerBackend, windows: bool) -> Vec<&'static str> {
    match (backend, windows) {
        (TypeCheckerBackend::Ty, true) => vec!["ty.exe", "ty"],
        (TypeCheckerBackend::Ty, false) => vec!["ty"],
        (TypeCheckerBackend::Pyright, true) => {
            vec![
                "pyright-langserver.cmd",
                "pyright-langserver.exe",
                "pyright-langserver",
            ]
        }
        (TypeCheckerBackend::Pyright, false) => vec!["pyright-langserver"],
        (TypeCheckerBackend::Pyrefly, true) => vec!["pyrefly.exe", "pyrefly"],
        (TypeCheckerBackend::Pyrefly, false) => vec!["pyrefly"],
    }
}

fn python_executable_names(windows: bool) -> Vec<&'static str> {
    if windows {
        vec!["python.exe", "python"]
    } else {
        vec!["python"]
    }
}

fn first_existing_checker(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|path| is_executable_file(path)).cloned()
}

fn first_existing_python(paths: &[PathBuf]) -> Option<PathBuf> {
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

fn workspace_folders(workspace_roots: &[PathBuf]) -> Vec<WorkspaceFolder> {
    workspace_roots
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
        .collect()
}

fn initialization_options(config: &TypeCheckerConfig) -> Option<Value> {
    match config.checker {
        TypeCheckerBackend::Ty => config.python.as_ref().map(|python| {
            json!({
                "settings": {
                    "python": python
                }
            })
        }),
        TypeCheckerBackend::Pyright => Some(json!({
            "disablePullDiagnostics": true,
            "settings": {
                "python": pyright_python_settings(config.python.as_deref()),
                "python.analysis": pyright_analysis_settings(),
            }
        })),
        TypeCheckerBackend::Pyrefly => {
            Some(pyrefly_initialization_options(config.python.as_deref()))
        }
    }
}

fn workspace_configuration_response(
    message: &Value,
    backend: TypeCheckerBackend,
    python: Option<&str>,
) -> Value {
    let Some(items) = message.pointer("/params/items").and_then(Value::as_array) else {
        return Value::Array(Vec::new());
    };
    Value::Array(
        items
            .iter()
            .map(|item| {
                let section = item
                    .get("section")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                backend_configuration_section(backend, section, python)
            })
            .collect(),
    )
}

fn backend_configuration_section(
    backend: TypeCheckerBackend,
    section: &str,
    python: Option<&str>,
) -> Value {
    match backend {
        TypeCheckerBackend::Ty => ty_configuration_section(section, python),
        TypeCheckerBackend::Pyright => pyright_configuration_section(section, python),
        TypeCheckerBackend::Pyrefly => pyrefly_configuration_section(section, python),
    }
}

fn ty_configuration_section(section: &str, python: Option<&str>) -> Value {
    match (section, python) {
        ("python", Some(python)) => json!(python),
        _ => json!({}),
    }
}

fn pyright_configuration_section(section: &str, python: Option<&str>) -> Value {
    match section {
        "python" => pyright_python_settings(python),
        "python.analysis" => pyright_analysis_settings(),
        "pyright" => json!({
            "disablePullDiagnostics": true,
        }),
        _ => json!({}),
    }
}

fn pyright_python_settings(python: Option<&str>) -> Value {
    let mut settings = Map::new();
    if let Some(python) = python {
        settings.insert("defaultInterpreterPath".to_string(), json!(python));
        settings.insert("pythonPath".to_string(), json!(python));
    }
    Value::Object(settings)
}

fn pyright_analysis_settings() -> Value {
    json!({
        "typeCheckingMode": "strict",
        "diagnosticMode": "openFilesOnly",
        "pythonVersion": "3.14",
    })
}

fn pyrefly_configuration_section(section: &str, python: Option<&str>) -> Value {
    match section {
        "python" | "pyrefly" => pyrefly_workspace_settings(python),
        _ => json!({}),
    }
}

fn pyrefly_initialization_options(python: Option<&str>) -> Value {
    let mut options = pyrefly_workspace_settings(python);
    if let Value::Object(settings) = &mut options
        && let Some(python) = python
    {
        settings.insert("pythonPath".to_string(), json!(python));
    }
    options
}

fn pyrefly_workspace_settings(python: Option<&str>) -> Value {
    let mut settings = Map::new();
    settings.insert(
        "pyrefly".to_string(),
        json!({
            "typeCheckingMode": "strict",
            "disableTypeErrors": false,
            "analysis": {
                "diagnosticMode": "openFilesOnly",
            },
        }),
    );
    if let Some(python) = python {
        settings.insert("defaultInterpreterPath".to_string(), json!(python));
    }
    Value::Object(settings)
}

fn dedupe_launch_candidates(
    candidates: Vec<TypeCheckerLaunchConfig>,
) -> Vec<TypeCheckerLaunchConfig> {
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

fn dedupe_python_launch_candidates(candidates: Vec<PythonLaunchConfig>) -> Vec<PythonLaunchConfig> {
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
        assert_eq!(config.checker, TypeCheckerBackend::Ty);
        assert_eq!(config.command.as_deref(), Some("/tmp/ty"));
        assert!(config.args.is_empty());
        assert_eq!(checker_args(&config), vec!["server"]);
        assert_eq!(TypeCheckerConfig::default().enabled, false);
    }

    #[test]
    fn type_checker_launch_candidates_use_explicit_command_first() {
        let config = TypeCheckerConfig {
            enabled: true,
            checker: TypeCheckerBackend::Ty,
            command: Some("/opt/ty".to_string()),
            args: Vec::new(),
            python: None,
        };
        assert_eq!(
            type_checker_launch_candidates(&config, &[]),
            vec![TypeCheckerLaunchConfig::new(
                "/opt/ty",
                vec!["server".to_string()]
            )]
        );
    }

    #[test]
    fn type_checker_launch_candidates_include_workspace_uv_and_path_fallback() {
        let temp = TempDir::new().expect("tempdir");
        std::fs::write(temp.path().join("uv.lock"), "").expect("uv lock");
        let candidates =
            type_checker_launch_candidates(&TypeCheckerConfig::default(), &[temp.path().into()]);
        assert!(candidates.iter().any(|candidate| candidate.command == "uv"));
        assert!(candidates.iter().any(|candidate| candidate.command == "ty"));
    }

    #[test]
    fn type_checker_launch_candidates_ignore_uvicorn_pyproject_table() {
        let temp = TempDir::new().expect("tempdir");
        std::fs::write(
            temp.path().join("pyproject.toml"),
            "[project]\nname = \"demo\"\n\n[tool.uvicorn]\nreload = true\n",
        )
        .expect("pyproject");
        let candidates =
            type_checker_launch_candidates(&TypeCheckerConfig::default(), &[temp.path().into()]);
        assert!(!candidates.iter().any(|candidate| candidate.command == "uv"));
        assert!(candidates.iter().any(|candidate| candidate.command == "ty"));
    }

    #[test]
    fn type_checker_launch_candidates_use_backend_defaults() {
        let pyright = TypeCheckerConfig {
            enabled: true,
            checker: TypeCheckerBackend::Pyright,
            command: None,
            args: Vec::new(),
            python: None,
        };
        let pyrefly = TypeCheckerConfig {
            checker: TypeCheckerBackend::Pyrefly,
            ..pyright.clone()
        };

        assert!(
            type_checker_launch_candidates(&pyright, &[])
                .iter()
                .any(|candidate| candidate.command == "pyright-langserver"
                    && candidate.args == ["--stdio"])
        );
        assert!(
            type_checker_launch_candidates(&pyrefly, &[])
                .iter()
                .any(|candidate| candidate.command == "pyrefly" && candidate.args == ["lsp"])
        );
    }

    #[test]
    fn workspace_configuration_response_is_backend_specific() {
        let message = json!({
            "params": {
                "items": [
                    {"section": "python"},
                    {"section": "python.analysis"},
                    {"section": "pyright"}
                ]
            }
        });

        assert_eq!(
            workspace_configuration_response(
                &message,
                TypeCheckerBackend::Pyright,
                Some("/venv/bin/python")
            ),
            json!([
                {
                    "defaultInterpreterPath": "/venv/bin/python",
                    "pythonPath": "/venv/bin/python"
                },
                {
                    "typeCheckingMode": "strict",
                    "diagnosticMode": "openFilesOnly",
                    "pythonVersion": "3.14"
                },
                {
                    "disablePullDiagnostics": true
                }
            ])
        );

        assert_eq!(
            workspace_configuration_response(&message, TypeCheckerBackend::Pyrefly, None)
                .pointer("/0/pyrefly/typeCheckingMode")
                .and_then(Value::as_str),
            Some("strict")
        );
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
                published_diagnostics: Mutex::new(HashMap::new()),
                diagnostics_notify: Notify::new(),
                dead: AtomicBool::new(false),
            }),
            child: Mutex::new(child),
            backend: TypeCheckerBackend::Ty,
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
