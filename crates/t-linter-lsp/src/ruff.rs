use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
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
use tower_lsp::lsp_types::{DocumentFormattingParams, TextEdit};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuffFormatConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_ruff_command")]
    pub command: String,
    #[serde(default = "default_ruff_args")]
    pub args: Vec<String>,
    #[serde(default)]
    pub settings: Value,
}

impl Default for RuffFormatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: default_ruff_command(),
            args: default_ruff_args(),
            settings: Value::Object(Default::default()),
        }
    }
}

#[derive(Debug)]
pub struct RuffFormatClient {
    inner: Arc<RuffFormatClientInner>,
    child: Mutex<Child>,
}

#[derive(Debug)]
struct RuffFormatClientInner {
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

impl RuffFormatClient {
    pub async fn start(config: &RuffFormatConfig, initialize_params: Value) -> Result<Self> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to spawn Ruff server: {}", config.command))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Ruff server did not expose stdin"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Ruff server did not expose stdout"))?;
        let inner = Arc::new(RuffFormatClientInner {
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

fn default_ruff_command() -> String {
    "ruff".to_string()
}

fn default_ruff_args() -> Vec<String> {
    vec!["server".to_string()]
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
    writer.flush().await
}
