use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use t_linter_core::{
    CachedSqlCatalog, DescribeResponse, SchemaProvider, SqlCatalogError, SqlCatalogQuery,
    TemplateStringParser, cache_path_for_query, catalog_entry_from_response,
    catalog_query_for_template, load_project_config_for_path, read_cached_catalog,
    resolve_database_url, write_cached_catalog,
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
}

#[derive(Debug, Serialize)]
struct DescribeRequest<'a> {
    id: u64,
    op: &'static str,
    database_url: &'a str,
    sql: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    search_path: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct DescribeEnvelope {
    #[allow(dead_code)]
    id: Option<u64>,
    #[serde(default)]
    params: Vec<t_linter_core::SqlCatalogParam>,
    #[serde(default)]
    columns: Vec<t_linter_core::SqlCatalogColumn>,
    #[serde(default)]
    psycopg_version: Option<String>,
    #[serde(default)]
    error: Option<SqlCatalogError>,
}

impl PythonDescribeProvider {
    fn new(database_url: String, search_path: Option<String>) -> Self {
        Self {
            python: sql_python_command(),
            database_url,
            search_path,
        }
    }
}

impl SchemaProvider for PythonDescribeProvider {
    fn describe(&mut self, query: &SqlCatalogQuery) -> Result<DescribeResponse> {
        let request = DescribeRequest {
            id: 1,
            op: "describe",
            database_url: &self.database_url,
            sql: &query.sql,
            search_path: self.search_path.as_deref(),
        };
        let payload = serde_json::to_string(&request).context("Failed to serialize SQL request")?;
        let mut child = Command::new(&self.python)
            .arg("-c")
            .arg(SQL_DESCRIBE_HELPER)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to start {}", self.python))?;

        {
            let stdin = child
                .stdin
                .as_mut()
                .context("SQL describe helper stdin is not available")?;
            stdin
                .write_all(payload.as_bytes())
                .context("Failed to write SQL describe request")?;
            stdin
                .write_all(b"\n")
                .context("Failed to finish SQL describe request")?;
        }

        let output = child
            .wait_with_output()
            .context("Failed to read SQL describe response")?;
        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "SQL describe helper exited with {}",
                output.status
            ));
        }
        let stdout = String::from_utf8(output.stdout)
            .context("SQL describe helper emitted non-UTF-8 output")?;
        let line = stdout
            .lines()
            .find(|line| !line.trim().is_empty())
            .context("SQL describe helper emitted no response")?;
        let envelope: DescribeEnvelope =
            serde_json::from_str(line).context("Failed to parse SQL describe response")?;
        if let Some(error) = envelope.error {
            return Err(anyhow::anyhow!(error.message));
        }
        Ok(DescribeResponse {
            params: envelope.params,
            columns: envelope.columns,
            psycopg_version: envelope.psycopg_version,
        })
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
    );

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
) -> Option<PythonDescribeProvider> {
    let database_url = database_url
        .as_deref()
        .and_then(|value| resolve_database_url(value).ok())?;
    Some(PythonDescribeProvider::new(
        database_url,
        search_path.clone(),
    ))
}

fn sql_python_command() -> String {
    std::env::var("T_LINTER_SQL_PYTHON")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "python3".to_string())
}
