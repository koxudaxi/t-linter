use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::project_config::SqlConfig;
use crate::{TemplatePart, TemplateStringInfo};

const CACHE_DIR: &str = ".t-linter/sql-cache";

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
    fs::write(path, format!("{content}\n"))
        .with_context(|| format!("Failed to write {}", path.display()))
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

fn normalize_sql(sql: &str) -> String {
    sql.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha256(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (input.len() as u64) * 8;
    let mut message = Vec::with_capacity(input.len() + 72);
    message.extend_from_slice(input);
    message.push(0x80);
    while (message.len() % 64) != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    let mut words = [0u32; 64];
    for chunk in message.chunks_exact(64) {
        for (index, word) in words.iter_mut().take(16).enumerate() {
            let offset = index * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut digest = [0u8; 32];
    for (index, value) in state.into_iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&value.to_be_bytes());
    }
    digest
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
