use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use anyhow::{Context, Result};
use t_linter_core::{
    CachedSqlCatalog, DescribeEnvelope, DescribeRequest, DescribeResponse, SchemaProvider,
    SqlCatalogQuery, TemplateStringParser, cache_path_for_query, catalog_entry_from_response,
    catalog_query_for_template, load_project_config_for_path, read_cached_catalog,
    resolve_database_url, response_from_describe_envelope, sql_describe_timeout,
    write_cached_catalog,
};

use crate::discovery::{DiscoveryMode, collect_python_files};

const SQL_DESCRIBE_HELPER: &str = include_str!("../../t-linter-lsp/helpers/sql_describe.py");

#[derive(Debug, Default)]
struct PrepareSummary {
    templates: usize,
    described: usize,
    cached: usize,
    written: usize,
    unchanged: usize,
    failed: usize,
}

#[derive(Debug)]
struct PythonDescribeProvider {
    python: String,
    database_url: String,
    search_path: Option<String>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<Receiver<std::result::Result<String, String>>>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
}

enum DescribeAttemptError {
    Helper(anyhow::Error),
    Response(anyhow::Error),
    Timeout(anyhow::Error),
}

impl PythonDescribeProvider {
    fn new(database_url: String, search_path: Option<String>) -> Self {
        Self {
            python: sql_python_command(),
            database_url,
            search_path,
            child: None,
            stdin: None,
            stdout: None,
            reader: None,
            next_id: 1,
        }
    }

    fn ensure_helper(&mut self) -> Result<()> {
        if let Some(child) = self.child.as_mut() {
            if child
                .try_wait()
                .context("Failed to inspect SQL describe helper")?
                .is_none()
                && self.stdin.is_some()
                && self.stdout.is_some()
            {
                return Ok(());
            }
            self.stop_helper();
        }

        let mut child = Command::new(&self.python)
            .arg("-c")
            .arg(SQL_DESCRIBE_HELPER)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to start {}", self.python))?;
        self.stdin = Some(
            child
                .stdin
                .take()
                .context("SQL describe helper stdin is not available")?,
        );
        let stdout = child
            .stdout
            .take()
            .context("SQL describe helper stdout is not available")?;
        let (stdout, reader) = start_stdout_reader(stdout);
        self.stdout = Some(stdout);
        self.reader = Some(reader);
        self.child = Some(child);
        Ok(())
    }

    fn describe_once(
        &mut self,
        query: &SqlCatalogQuery,
    ) -> std::result::Result<DescribeResponse, DescribeAttemptError> {
        self.ensure_helper().map_err(DescribeAttemptError::Helper)?;
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = DescribeRequest {
            id,
            op: "describe",
            database_url: &self.database_url,
            sql: &query.sql,
            search_path: self.search_path.as_deref(),
        };
        let payload = serde_json::to_string(&request)
            .context("Failed to serialize SQL request")
            .map_err(DescribeAttemptError::Helper)?;
        let stdin = self
            .stdin
            .as_mut()
            .context("SQL describe helper stdin is not available")
            .map_err(DescribeAttemptError::Helper)?;
        stdin
            .write_all(payload.as_bytes())
            .context("Failed to write SQL describe request")
            .map_err(DescribeAttemptError::Helper)?;
        stdin
            .write_all(b"\n")
            .context("Failed to finish SQL describe request")
            .map_err(DescribeAttemptError::Helper)?;
        stdin
            .flush()
            .context("Failed to flush SQL describe request")
            .map_err(DescribeAttemptError::Helper)?;

        let stdout = self
            .stdout
            .as_mut()
            .context("SQL describe helper stdout is not available")
            .map_err(DescribeAttemptError::Helper)?;
        let timeout = sql_describe_timeout();
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    DescribeAttemptError::Timeout(anyhow::anyhow!(
                        "SQL describe timed out after {:.3}s",
                        timeout.as_secs_f64()
                    ))
                })?;
            let line = match stdout.recv_timeout(remaining) {
                Ok(Ok(line)) => line,
                Ok(Err(error)) => {
                    return Err(DescribeAttemptError::Helper(anyhow::anyhow!(
                        "Failed to read SQL describe response: {error}"
                    )));
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(DescribeAttemptError::Timeout(anyhow::anyhow!(
                        "SQL describe timed out after {:.3}s",
                        timeout.as_secs_f64()
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(DescribeAttemptError::Helper(anyhow::anyhow!(
                        "SQL describe helper exited without response"
                    )));
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let envelope: DescribeEnvelope = serde_json::from_str(&line)
                .context("Failed to parse SQL describe response")
                .map_err(DescribeAttemptError::Helper)?;
            if envelope.id == Some(id) {
                return response_from_describe_envelope(envelope)
                    .map_err(DescribeAttemptError::Response);
            }
        }
    }

    fn stop_helper(&mut self) {
        drop(self.stdin.take());
        drop(self.stdout.take());
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn start_stdout_reader(
    stdout: ChildStdout,
) -> (
    Receiver<std::result::Result<String, String>>,
    JoinHandle<()>,
) {
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut stdout = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match stdout.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if sender.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    break;
                }
            }
        }
    });
    (receiver, reader)
}

impl Drop for PythonDescribeProvider {
    fn drop(&mut self) {
        self.stop_helper();
    }
}

impl SchemaProvider for PythonDescribeProvider {
    fn describe(&mut self, query: &SqlCatalogQuery) -> Result<DescribeResponse> {
        let mut last_error = None;
        for attempt in 0..2 {
            match self.describe_once(query) {
                Ok(response) => return Ok(response),
                Err(DescribeAttemptError::Response(error)) => return Err(error),
                Err(DescribeAttemptError::Timeout(error)) => {
                    self.stop_helper();
                    return Err(error);
                }
                Err(DescribeAttemptError::Helper(error)) => {
                    last_error = Some(error);
                    if attempt == 0 {
                        self.stop_helper();
                        continue;
                    }
                }
            }
        }
        Err(last_error.expect("describe error"))
    }
}

pub fn prepare(paths: Vec<String>, check: bool) -> Result<i32> {
    let paths = if paths.is_empty() {
        vec![".".to_string()]
    } else {
        paths
    };
    let walk_report = collect_python_files(&paths, DiscoveryMode::Check)?;
    let mut summary = PrepareSummary {
        failed: walk_report.failures.len(),
        ..PrepareSummary::default()
    };
    for failure in &walk_report.failures {
        eprintln!("{}: {}", failure.display_path.display(), failure.message);
    }

    for file in walk_report.python_files {
        if let Err(error) = prepare_file(&file.canonical_path, check, &mut summary) {
            summary.failed += 1;
            eprintln!("{}: {error}", file.display_path.display());
        }
    }

    eprintln!(
        "{} SQL templates, {} described, {} cache hits, {} written, {} unchanged, {} failed",
        summary.templates,
        summary.described,
        summary.cached,
        summary.written,
        summary.unchanged,
        summary.failed
    );

    if summary.failed > 0 { Ok(2) } else { Ok(0) }
}

fn prepare_file(path: &Path, check: bool, summary: &mut PrepareSummary) -> Result<()> {
    let source =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let project_config = load_project_config_for_path(path)?;
    let mut parser = TemplateStringParser::new()?;
    let templates = parser.find_template_strings_in_file(&source, path)?;
    let mut provider = database_provider(
        &project_config.sql.database_url,
        &project_config.sql.search_path,
    )?;

    for template in templates {
        let Some(query) = catalog_query_for_template(&template, &project_config.sql) else {
            continue;
        };
        summary.templates += 1;
        let cache_path = cache_path_for_query(&project_config.root, &query);
        let cached = read_cached_catalog(&cache_path)?;
        let entry = match provider.as_mut() {
            Some(provider) => match provider.describe(&query) {
                Ok(response) => {
                    summary.described += 1;
                    catalog_entry_from_response(
                        &query,
                        response,
                        project_config.sql.search_path.clone(),
                    )?
                }
                Err(error) => match cached.clone() {
                    Some(entry) => {
                        summary.cached += 1;
                        eprintln!(
                            "Using cached SQL catalog for {} after describe failed: {error}",
                            path.display()
                        );
                        entry
                    }
                    None => return Err(error),
                },
            },
            None => match cached.clone() {
                Some(entry) => {
                    summary.cached += 1;
                    entry
                }
                None => {
                    return Err(anyhow::anyhow!(
                        "SQL database-url is required to prepare {}",
                        query.sql
                    ));
                }
            },
        };

        if check {
            check_cache_entry(&cache_path, cached.as_ref(), &entry)?;
            summary.unchanged += 1;
        } else if cached.as_ref() == Some(&entry) {
            summary.unchanged += 1;
        } else {
            write_cached_catalog(&cache_path, &entry)?;
            summary.written += 1;
        }
    }
    Ok(())
}

fn check_cache_entry(
    cache_path: &Path,
    cached: Option<&CachedSqlCatalog>,
    entry: &CachedSqlCatalog,
) -> Result<()> {
    let Some(cached) = cached else {
        return Err(anyhow::anyhow!(
            "{} is missing; run `t-linter sql prepare`",
            cache_path.display()
        ));
    };
    if cached != entry {
        return Err(anyhow::anyhow!(
            "{} is stale; run `t-linter sql prepare`",
            cache_path.display()
        ));
    }
    Ok(())
}

fn database_provider(
    database_url: &Option<String>,
    search_path: &Option<String>,
) -> Result<Option<PythonDescribeProvider>> {
    let Some(database_url) = database_url.as_deref() else {
        return Ok(None);
    };
    let database_url = resolve_database_url(database_url)?;
    Ok(Some(PythonDescribeProvider::new(
        database_url,
        search_path.clone(),
    )))
}

fn sql_python_command() -> String {
    // CLI prepare runs in shell/CI contexts: use the explicit helper interpreter
    // when provided, otherwise rely on the active PATH.
    std::env::var("T_LINTER_SQL_PYTHON")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "python3".to_string())
}
