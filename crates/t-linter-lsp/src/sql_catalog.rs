#![allow(dead_code)]

use std::io::Read as _;
use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use t_linter_core::{
    DescribeEnvelope, DescribeRequest, DescribeResponse, SchemaProvider, SqlCatalogQuery,
    response_from_describe_envelope, sql_describe_timeout,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{Instant, timeout};
use wait_timeout::ChildExt;

use crate::type_checker::python_inline_script_launch_candidates;

const SQL_DESCRIBE_HELPER: &str = include_str!("../helpers/sql_describe.py");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogLaunchConfig {
    pub command: String,
    pub args: Vec<String>,
}

impl SqlCatalogLaunchConfig {
    fn new(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            command: command.into(),
            args,
        }
    }
}

#[derive(Debug)]
pub struct SqlCatalogClient {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
    request: Mutex<()>,
    next_id: AtomicU64,
}

#[derive(Debug, Clone)]
pub struct SqlCatalogProvider {
    launch: SqlCatalogLaunchConfig,
    database_url: String,
    search_path: Option<String>,
}

impl SqlCatalogClient {
    pub async fn start(launch: &SqlCatalogLaunchConfig) -> Result<Self> {
        let mut child = Command::new(&launch.command)
            .args(&launch.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("Failed to start {}", launch.command))?;
        let stdin = child
            .stdin
            .take()
            .context("SQL catalog helper stdin is not available")?;
        let stdout = child
            .stdout
            .take()
            .context("SQL catalog helper stdout is not available")?;
        Ok(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            request: Mutex::new(()),
            next_id: AtomicU64::new(1),
        })
    }

    pub async fn describe(
        &self,
        database_url: &str,
        sql: &str,
        search_path: Option<&str>,
    ) -> Result<DescribeResponse> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = DescribeRequest {
            id,
            op: "describe",
            database_url,
            sql,
            search_path,
        };
        let payload = serde_json::to_string(&request).context("Failed to serialize SQL request")?;
        let _request = self.request.lock().await;
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(payload.as_bytes())
                .await
                .context("Failed to write SQL request")?;
            stdin
                .write_all(b"\n")
                .await
                .context("Failed to finish SQL request")?;
            stdin.flush().await.context("Failed to flush SQL request")?;
        }

        let timeout_duration = sql_describe_timeout();
        let deadline = Instant::now() + timeout_duration;
        let mut stdout = self.stdout.lock().await;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                self.shutdown().await;
                return Err(anyhow::anyhow!(
                    "SQL describe timed out after {:.3}s",
                    timeout_duration.as_secs_f64()
                ));
            };
            let mut line = String::new();
            let bytes = match timeout(remaining, stdout.read_line(&mut line)).await {
                Ok(result) => result.context("Failed to read SQL response")?,
                Err(_) => {
                    self.shutdown().await;
                    return Err(anyhow::anyhow!(
                        "SQL describe timed out after {:.3}s",
                        timeout_duration.as_secs_f64()
                    ));
                }
            };
            if bytes == 0 {
                return Err(anyhow::anyhow!(
                    "SQL catalog helper exited without response"
                ));
            }
            if line.trim().is_empty() {
                continue;
            }
            let envelope: DescribeEnvelope =
                serde_json::from_str(&line).context("Failed to parse SQL response")?;
            if envelope.id == Some(id) {
                return response_from_describe_envelope(envelope);
            }
        }
    }

    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
    }
}

impl SqlCatalogProvider {
    pub fn new(
        launch: SqlCatalogLaunchConfig,
        database_url: String,
        search_path: Option<String>,
    ) -> Self {
        Self {
            launch,
            database_url,
            search_path,
        }
    }
}

impl SchemaProvider for SqlCatalogProvider {
    fn describe(&mut self, query: &SqlCatalogQuery) -> Result<DescribeResponse> {
        let request = DescribeRequest {
            id: 1,
            op: "describe",
            database_url: &self.database_url,
            sql: &query.sql,
            search_path: self.search_path.as_deref(),
        };
        let payload = serde_json::to_string(&request).context("Failed to serialize SQL request")?;
        let mut child = StdCommand::new(&self.launch.command)
            .args(&self.launch.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to start {}", self.launch.command))?;

        {
            let stdin = child
                .stdin
                .as_mut()
                .context("SQL catalog helper stdin is not available")?;
            use std::io::Write as _;
            stdin
                .write_all(payload.as_bytes())
                .context("Failed to write SQL request")?;
            stdin
                .write_all(b"\n")
                .context("Failed to finish SQL request")?;
        }

        let output = child
            .stdout
            .take()
            .context("SQL catalog helper stdout is not available")?;
        drop(child.stdin.take());
        let timeout_duration = sql_describe_timeout();
        let status = match child
            .wait_timeout(timeout_duration)
            .context("Failed to wait for SQL response")?
        {
            Some(status) => status,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!(
                    "SQL describe timed out after {:.3}s",
                    timeout_duration.as_secs_f64()
                ));
            }
        };
        if !status.success() {
            return Err(anyhow::anyhow!("SQL catalog helper exited with {}", status));
        }
        let mut stdout = output;
        let mut stdout_text = String::new();
        stdout
            .read_to_string(&mut stdout_text)
            .context("Failed to read SQL response")?;
        let stdout = stdout_text;
        let line = stdout
            .lines()
            .find(|line| !line.trim().is_empty())
            .context("SQL catalog helper emitted no response")?;
        let envelope: DescribeEnvelope =
            serde_json::from_str(line).context("Failed to parse SQL response")?;
        response_from_describe_envelope(envelope)
    }
}

pub fn sql_catalog_launch_candidates(
    workspace_roots: &[PathBuf],
    explicit_python: Option<&str>,
) -> Vec<SqlCatalogLaunchConfig> {
    python_inline_script_launch_candidates(
        workspace_roots,
        explicit_python,
        "T_LINTER_SQL_PYTHON",
        SQL_DESCRIBE_HELPER,
    )
    .into_iter()
    .map(|candidate| SqlCatalogLaunchConfig::new(candidate.command, candidate.args))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_python_wins_launch_candidates() {
        let candidates = sql_catalog_launch_candidates(&[], Some("/tmp/python"));

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].command, "/tmp/python");
        assert_eq!(candidates[0].args[0], "-c");
    }
}
