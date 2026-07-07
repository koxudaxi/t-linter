use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectConfig {
    pub root: PathBuf,
    pub exclude: Option<Vec<String>>,
    pub extend_exclude: Vec<String>,
    pub ignore_file: Option<String>,
    pub line_length: Option<usize>,
    pub ignore: Vec<String>,
    pub severity: HashMap<String, RuleSeverity>,
    pub per_file_ignores: HashMap<String, Vec<String>>,
    pub sql: SqlConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuleSeverity {
    Error,
    Warning,
}

impl<'de> Deserialize<'de> for RuleSeverity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "error" => Ok(Self::Error),
            "warning" => Ok(Self::Warning),
            _ => Err(serde::de::Error::custom(format!(
                "invalid severity `{value}`; expected `error` or `warning`"
            ))),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct SqlConfig {
    pub library: Option<String>,
    #[serde(alias = "databaseUrl")]
    pub database_url: Option<String>,
    #[serde(alias = "searchPath")]
    pub search_path: Option<String>,
    #[serde(alias = "extraParamTypes")]
    pub extra_param_types: Vec<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct PyprojectToml {
    tool: Option<ToolSection>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ToolSection {
    #[serde(rename = "t-linter")]
    t_linter: Option<TLinterConfig>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct TLinterConfig {
    exclude: Option<Vec<String>>,
    #[serde(rename = "extend-exclude")]
    extend_exclude: Option<Vec<String>>,
    #[serde(rename = "ignore-file")]
    ignore_file: Option<String>,
    #[serde(
        rename = "line-length",
        default,
        deserialize_with = "deserialize_optional_line_length"
    )]
    line_length: Option<usize>,
    ignore: Option<Vec<String>>,
    severity: Option<HashMap<String, RuleSeverity>>,
    #[serde(rename = "per-file-ignores")]
    per_file_ignores: Option<HashMap<String, Vec<String>>>,
    sql: Option<SqlConfig>,
}

pub fn load_project_config_for_path(path: &Path) -> Result<ProjectConfig> {
    let start = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    let root = find_config_root(start);
    load_project_config(&root)
}

pub fn load_project_config(root: &Path) -> Result<ProjectConfig> {
    let pyproject_path = root.join("pyproject.toml");
    let pyproject = if pyproject_path.is_file() {
        let content = fs::read_to_string(&pyproject_path)
            .with_context(|| format!("Failed to read {}", pyproject_path.display()))?;
        toml::from_str::<PyprojectToml>(&content)
            .with_context(|| format!("Failed to parse {}", pyproject_path.display()))?
    } else {
        PyprojectToml::default()
    };

    let config = pyproject
        .tool
        .and_then(|tool| tool.t_linter)
        .unwrap_or_default();

    Ok(ProjectConfig {
        root: root.to_path_buf(),
        exclude: config.exclude,
        extend_exclude: config.extend_exclude.unwrap_or_default(),
        ignore_file: config.ignore_file,
        line_length: config.line_length,
        ignore: config.ignore.unwrap_or_default(),
        severity: config.severity.unwrap_or_default(),
        per_file_ignores: config.per_file_ignores.unwrap_or_default(),
        sql: config.sql.unwrap_or_default(),
    })
}

pub fn find_config_root(start_dir: &Path) -> PathBuf {
    for dir in start_dir.ancestors() {
        if dir.join("pyproject.toml").is_file() || dir.join(".t-linterignore").is_file() {
            return dir.to_path_buf();
        }
    }
    start_dir.to_path_buf()
}

fn deserialize_optional_line_length<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<toml::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(toml::Value::Integer(value)) => usize::try_from(value).ok(),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_project_config_reads_line_length() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(
            temp.path().join("pyproject.toml"),
            "[tool.t-linter]\nline-length = 96\nextend-exclude = [\"vendor\"]\n",
        )
        .expect("write pyproject");

        let config = load_project_config(temp.path()).expect("load config");

        assert_eq!(config.root, temp.path());
        assert_eq!(config.line_length, Some(96));
        assert_eq!(config.extend_exclude, vec!["vendor".to_string()]);
    }

    #[test]
    fn load_project_config_reads_sql_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(
            temp.path().join("pyproject.toml"),
            "[tool.t-linter.sql]\nlibrary = \"psycopg\"\ndatabase-url = \"env:DATABASE_URL\"\nsearch-path = \"public\"\nextra-param-types = [\"myapp.Money\"]\n",
        )
        .expect("write pyproject");

        let config = load_project_config(temp.path()).expect("load config");

        assert_eq!(config.sql.library.as_deref(), Some("psycopg"));
        assert_eq!(config.sql.database_url.as_deref(), Some("env:DATABASE_URL"));
        assert_eq!(config.sql.search_path.as_deref(), Some("public"));
        assert_eq!(config.sql.extra_param_types, vec!["myapp.Money"]);
    }

    #[test]
    fn load_project_config_reads_rule_filter_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(
            temp.path().join("pyproject.toml"),
            "[tool.t-linter]\nignore = [\"component-unexpected-prop\"]\n\n[tool.t-linter.severity]\ncomponent-missing-prop = \"warning\"\n\n[tool.t-linter.per-file-ignores]\n\"tests/**\" = [\"embedded-parse-error\"]\n",
        )
        .expect("write pyproject");

        let config = load_project_config(temp.path()).expect("load config");

        assert_eq!(config.ignore, vec!["component-unexpected-prop".to_string()]);
        assert_eq!(
            config.severity.get("component-missing-prop"),
            Some(&RuleSeverity::Warning)
        );
        assert_eq!(
            config.per_file_ignores.get("tests/**"),
            Some(&vec!["embedded-parse-error".to_string()])
        );
    }

    #[test]
    fn load_project_config_rejects_invalid_rule_severity() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(
            temp.path().join("pyproject.toml"),
            "[tool.t-linter.severity]\ncomponent-missing-prop = \"info\"\n",
        )
        .expect("write pyproject");

        let error = load_project_config(temp.path()).expect_err("invalid severity");

        assert!(error.to_string().contains("Failed to parse"));
    }

    #[test]
    fn load_project_config_ignores_invalid_line_length_type() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(
            temp.path().join("pyproject.toml"),
            "[tool.t-linter]\nline-length = \"bad\"\n",
        )
        .expect("write pyproject");

        let config = load_project_config(temp.path()).expect("load config");

        assert_eq!(config.line_length, None);
    }
}
