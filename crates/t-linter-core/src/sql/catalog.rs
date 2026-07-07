use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project_config::SqlConfig;
use crate::{TemplatePart, TemplateStringInfo};

const CACHE_DIR: &str = ".t-linter/sql-cache";
pub const SQL_DESCRIBE_TIMEOUT_ENV: &str = "T_LINTER_SQL_DESCRIBE_TIMEOUT_SECONDS";
pub const DEFAULT_SQL_DESCRIBE_TIMEOUT_SECONDS: f64 = 10.0;
static CACHE_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlCatalogQuery {
    pub sql: String,
    pub sql_hash: String,
    pub parameter_interpolation_indices: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SqlCatalogParam {
    pub oid: u32,
    pub type_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SqlCatalogColumn {
    pub name: String,
    pub oid: u32,
    pub type_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DescribeResponse {
    #[serde(default)]
    pub params: Vec<SqlCatalogParam>,
    #[serde(default)]
    pub columns: Vec<SqlCatalogColumn>,
    #[serde(default)]
    pub psycopg_version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DescribeRequest<'a> {
    pub id: u64,
    pub op: &'static str,
    pub database_url: &'a str,
    pub sql: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_path: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
pub struct DescribeEnvelope {
    pub id: Option<u64>,
    #[serde(default)]
    pub params: Vec<SqlCatalogParam>,
    #[serde(default)]
    pub columns: Vec<SqlCatalogColumn>,
    #[serde(default)]
    pub psycopg_version: Option<String>,
    #[serde(default)]
    pub error: Option<SqlCatalogError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SqlCatalogError {
    pub message: String,
    #[serde(default)]
    pub position: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedSqlCatalog {
    pub sql: String,
    #[serde(default)]
    pub params: Vec<SqlCatalogParam>,
    #[serde(default)]
    pub columns: Vec<SqlCatalogColumn>,
    pub schema_fingerprint: String,
    #[serde(default)]
    pub psycopg_version: Option<String>,
    #[serde(default)]
    pub search_path: Option<String>,
}

pub trait SchemaProvider {
    fn describe(&mut self, query: &SqlCatalogQuery) -> Result<DescribeResponse>;
}

pub fn catalog_query_for_template(
    template: &TemplateStringInfo,
    config: &SqlConfig,
) -> Option<SqlCatalogQuery> {
    if !template
        .language
        .as_deref()
        .is_some_and(|language| language.eq_ignore_ascii_case("sql"))
    {
        return None;
    }
    if !super::psycopg::is_enabled(config, template) {
        return None;
    }

    let mut sql = String::with_capacity(template.content.len());
    let mut parameter_interpolation_indices = Vec::new();
    for part in &template.parts {
        match part {
            TemplatePart::Static(part) => sql.push_str(&part.text),
            TemplatePart::Interpolation(interpolation) => {
                let spec = interpolation.format_spec.trim();
                if !matches!(spec, "" | "s" | "b" | "t") {
                    return None;
                }
                parameter_interpolation_indices.push(interpolation.interpolation_index);
                sql.push('$');
                sql.push_str(&parameter_interpolation_indices.len().to_string());
            }
        }
    }

    let sql = normalize_sql(&sql);
    let sql_hash = sha256_hex(sql.as_bytes());
    Some(SqlCatalogQuery {
        sql,
        sql_hash,
        parameter_interpolation_indices,
    })
}

pub fn catalog_entry_from_response(
    query: &SqlCatalogQuery,
    response: DescribeResponse,
    search_path: Option<String>,
) -> Result<CachedSqlCatalog> {
    let fingerprint_payload = serde_json::to_vec(&response)
        .context("Failed to serialize SQL describe response for fingerprint")?;
    Ok(CachedSqlCatalog {
        sql: query.sql.clone(),
        params: response.params,
        columns: response.columns,
        schema_fingerprint: format!("sha256:{}", sha256_hex(&fingerprint_payload)),
        psycopg_version: response.psycopg_version,
        search_path,
    })
}

pub fn cache_path_for_query(root: &Path, query: &SqlCatalogQuery) -> PathBuf {
    root.join(CACHE_DIR)
        .join(format!("query-{}.json", query.sql_hash))
}

pub fn read_cached_catalog(path: &Path) -> Result<Option<CachedSqlCatalog>> {
    match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content)
            .map(Some)
            .with_context(|| format!("Failed to parse {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

pub fn write_cached_catalog(path: &Path, entry: &CachedSqlCatalog) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(entry)
        .context("Failed to serialize SQL catalog cache entry")?;
    let tmp = temp_cache_path(path);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .with_context(|| format!("Failed to create {}", tmp.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("Failed to write {}", tmp.display()))?;
        file.write_all(b"\n")
            .with_context(|| format!("Failed to write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync {}", tmp.display()))?;
        drop(file);
        fs::rename(&tmp, path).with_context(|| {
            format!("Failed to persist {} as {}", tmp.display(), path.display())
        })?;
        sync_parent_dir(path);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn temp_cache_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "cache.json".into());
    let counter = CACHE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{file_name}.{}.{counter}.tmp", std::process::id()))
}

fn sync_parent_dir(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
}

pub fn cached_catalog_for_template(
    root: &Path,
    template: &TemplateStringInfo,
    config: &SqlConfig,
) -> Result<Option<(SqlCatalogQuery, CachedSqlCatalog)>> {
    let Some(query) = catalog_query_for_template(template, config) else {
        return Ok(None);
    };
    let cache_path = cache_path_for_query(root, &query);
    let Some(entry) = read_cached_catalog(&cache_path)? else {
        return Ok(None);
    };
    if normalize_sql(&entry.sql) != query.sql {
        return Ok(None);
    }
    Ok(Some((query, entry)))
}

pub fn resolve_database_url(value: &str) -> Result<String> {
    let value = value.trim();
    if let Some(name) = value.strip_prefix("env:") {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow::anyhow!("database-url env reference is empty"));
        }
        return std::env::var(name)
            .with_context(|| format!("Environment variable {name} is not set"));
    }
    Ok(value.to_string())
}

pub fn response_from_describe_envelope(envelope: DescribeEnvelope) -> Result<DescribeResponse> {
    if let Some(error) = envelope.error {
        return Err(anyhow::anyhow!(error.message));
    }
    Ok(DescribeResponse {
        params: envelope.params,
        columns: envelope.columns,
        psycopg_version: envelope.psycopg_version,
    })
}

pub fn sql_describe_timeout() -> Duration {
    let Ok(raw) = std::env::var(SQL_DESCRIBE_TIMEOUT_ENV) else {
        return Duration::from_secs_f64(DEFAULT_SQL_DESCRIBE_TIMEOUT_SECONDS);
    };
    let Ok(seconds) = raw.trim().parse::<f64>() else {
        return Duration::from_secs_f64(DEFAULT_SQL_DESCRIBE_TIMEOUT_SECONDS);
    };
    if seconds.is_finite() {
        Duration::from_secs_f64(seconds.max(1.0))
    } else {
        Duration::from_secs_f64(DEFAULT_SQL_DESCRIBE_TIMEOUT_SECONDS)
    }
}

fn normalize_sql(sql: &str) -> String {
    sql.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TemplateStringParser;

    fn first_template(source: &str) -> TemplateStringInfo {
        let mut parser = TemplateStringParser::new().expect("parser");
        parser
            .find_template_strings_in_file(source, Path::new("app.py"))
            .expect("templates")
            .into_iter()
            .next()
            .expect("template")
    }

    #[test]
    fn catalog_query_replaces_plain_parameters_with_postgres_placeholders() {
        let template = first_template(
            r#"from typing import Annotated
from string.templatelib import Template

query: Annotated[Template, "sql"] = t"SELECT * FROM users WHERE id = {user_id} AND name = {name:s}"
"#,
        );
        let config = SqlConfig {
            library: Some("psycopg".to_string()),
            ..SqlConfig::default()
        };

        let query = catalog_query_for_template(&template, &config).expect("query");

        assert_eq!(query.sql, "SELECT * FROM users WHERE id = $1 AND name = $2");
        assert_eq!(query.parameter_interpolation_indices, vec![0, 1]);
        assert_eq!(query.sql_hash.len(), 64);
    }

    #[test]
    fn catalog_query_skips_structural_specs() {
        let template = first_template(
            r#"from typing import Annotated
from string.templatelib import Template

query: Annotated[Template, "sql"] = t"SELECT * FROM {table:i} WHERE id = {user_id}"
"#,
        );
        let config = SqlConfig {
            library: Some("psycopg".to_string()),
            ..SqlConfig::default()
        };

        assert!(catalog_query_for_template(&template, &config).is_none());
    }

    #[test]
    fn catalog_query_skips_non_sql_templates_even_when_config_enabled() {
        let template = first_template(
            r#"from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "json"] = t'{{"id": {user_id}}}'
"#,
        );
        let config = SqlConfig {
            library: Some("psycopg".to_string()),
            ..SqlConfig::default()
        };

        assert!(catalog_query_for_template(&template, &config).is_none());
    }

    #[test]
    fn cache_entry_uses_describe_response_fingerprint() {
        let query = SqlCatalogQuery {
            sql: "SELECT $1::int4".to_string(),
            sql_hash: "abc".to_string(),
            parameter_interpolation_indices: vec![0],
        };
        let response = DescribeResponse {
            params: vec![SqlCatalogParam {
                oid: 23,
                type_name: "int4".to_string(),
            }],
            columns: Vec::new(),
            psycopg_version: Some("3.3.1".to_string()),
        };

        let entry = catalog_entry_from_response(&query, response, Some("public".to_string()))
            .expect("entry");

        assert_eq!(entry.sql, query.sql);
        assert_eq!(entry.params[0].type_name, "int4");
        assert!(entry.schema_fingerprint.starts_with("sha256:"));
        assert_eq!(entry.search_path.as_deref(), Some("public"));
    }

    #[test]
    fn sha256_matches_known_digest() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
