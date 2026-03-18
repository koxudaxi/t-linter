use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

const DEFAULT_EXCLUDES: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
];

#[derive(Debug, Clone)]
pub struct DiscoveredPythonFile {
    pub canonical_path: PathBuf,
    pub display_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct DiscoveryFailure {
    pub display_path: PathBuf,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct WalkReport {
    pub python_files: Vec<DiscoveredPythonFile>,
    pub failures: Vec<DiscoveryFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryMode {
    Check,
    Format,
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
}

#[derive(Debug)]
struct DiscoveryConfig {
    root: PathBuf,
    matcher: Gitignore,
}

pub fn collect_python_files(paths: &[String], mode: DiscoveryMode) -> Result<WalkReport> {
    let mut collector = DiscoveryCollector::default();

    for path in paths {
        collector.collect_explicit_operand(Path::new(path), mode)?;
    }

    collector.report.python_files.sort_by(|left, right| {
        left.display_path
            .cmp(&right.display_path)
            .then(left.canonical_path.cmp(&right.canonical_path))
    });
    collector
        .report
        .failures
        .sort_by(|left, right| left.display_path.cmp(&right.display_path));

    Ok(collector.report)
}

#[derive(Default)]
struct DiscoveryCollector {
    seen_files: HashSet<PathBuf>,
    config_cache: HashMap<PathBuf, DiscoveryConfig>,
    report: WalkReport,
}

impl DiscoveryCollector {
    fn collect_explicit_operand(&mut self, operand: &Path, mode: DiscoveryMode) -> Result<()> {
        let metadata = match fs::symlink_metadata(operand) {
            Ok(metadata) => metadata,
            Err(_) => {
                self.push_failure(operand.to_path_buf(), "Failed to read file");
                return Ok(());
            }
        };

        let resolved = match fs::canonicalize(operand) {
            Ok(path) => path,
            Err(error) => {
                let message = if metadata.file_type().is_symlink() {
                    format!("Failed to resolve symlink: {error}")
                } else {
                    format!("Failed to resolve path: {error}")
                };
                self.push_failure(operand.to_path_buf(), message);
                return Ok(());
            }
        };

        let resolved_metadata = match fs::metadata(&resolved) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.push_failure(
                    operand.to_path_buf(),
                    format!("Failed to access resolved path: {error}"),
                );
                return Ok(());
            }
        };

        if resolved_metadata.is_dir() {
            let discovery_root = self.discovery_root_for_dir(&resolved)?;
            self.collect_directory(&resolved, operand, &discovery_root);
            return Ok(());
        }

        if !resolved_metadata.is_file() {
            self.push_failure(operand.to_path_buf(), "Path is not a file or directory");
            return Ok(());
        }

        if !is_python_file(&resolved) {
            if mode == DiscoveryMode::Format {
                self.push_failure(
                    operand.to_path_buf(),
                    "Explicit file operands must use the .py extension",
                );
            }
            return Ok(());
        }

        let discovery_root = self.discovery_root_for_file(&resolved)?;
        if self.should_ignore_path(&resolved, false, &discovery_root) {
            return Ok(());
        }

        self.add_file(resolved, operand.to_path_buf());
        Ok(())
    }

    fn collect_directory(
        &mut self,
        current_dir: &Path,
        current_display_dir: &Path,
        discovery_root: &Path,
    ) {
        if self.should_ignore_path(current_dir, true, discovery_root) {
            return;
        }

        let mut entries = match fs::read_dir(current_dir) {
            Ok(entries) => {
                let mut collected = Vec::new();
                for entry in entries {
                    match entry {
                        Ok(entry) => collected.push(entry),
                        Err(error) => {
                            self.push_failure(
                                current_display_dir.to_path_buf(),
                                format!("Failed to read directory entry: {error}"),
                            );
                        }
                    }
                }
                collected
            }
            Err(error) => {
                self.push_failure(
                    current_display_dir.to_path_buf(),
                    format!("Failed to read directory: {error}"),
                );
                return;
            }
        };
        entries.sort_by_key(|entry| entry.path());

        for entry in entries {
            let path = entry.path();
            let display_path = display_path_for_child(current_dir, current_display_dir, &path);

            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.push_failure(
                        display_path,
                        format!("Failed to read path metadata: {error}"),
                    );
                    continue;
                }
            };

            if metadata.file_type().is_symlink() {
                continue;
            }

            if self.should_ignore_path(&path, metadata.is_dir(), discovery_root) {
                continue;
            }

            if metadata.is_dir() {
                self.collect_directory(&path, &display_path, discovery_root);
                continue;
            }

            if metadata.is_file() && is_python_file(&path) {
                self.add_file(path, display_path);
            }
        }
    }

    fn add_file(&mut self, canonical_path: PathBuf, display_path: PathBuf) {
        if self.seen_files.insert(canonical_path.clone()) {
            self.report.python_files.push(DiscoveredPythonFile {
                canonical_path,
                display_path,
            });
        }
    }

    fn push_failure(&mut self, display_path: PathBuf, message: impl Into<String>) {
        self.report.failures.push(DiscoveryFailure {
            display_path,
            message: message.into(),
        });
    }

    fn should_ignore_path(&self, path: &Path, is_dir: bool, discovery_root: &Path) -> bool {
        self.config_cache
            .get(discovery_root)
            .map(|discovery| should_ignore_path(path, is_dir, discovery))
            .unwrap_or(false)
    }

    fn discovery_root_for_dir(&mut self, path: &Path) -> Result<PathBuf> {
        self.discovery_root_for_path(path)
    }

    fn discovery_root_for_file(&mut self, path: &Path) -> Result<PathBuf> {
        let start = path.parent().unwrap_or(path);
        self.discovery_root_for_path(start)
    }

    fn discovery_root_for_path(&mut self, path: &Path) -> Result<PathBuf> {
        let root = find_config_root(path);
        if !self.config_cache.contains_key(&root) {
            let config = load_discovery_config(&root)?;
            self.config_cache.insert(root.clone(), config);
        }
        Ok(root)
    }
}

fn display_path_for_child(
    current_dir: &Path,
    current_display_dir: &Path,
    canonical_path: &Path,
) -> PathBuf {
    canonical_path
        .strip_prefix(current_dir)
        .map(|relative| {
            if relative.as_os_str().is_empty() {
                current_display_dir.to_path_buf()
            } else {
                current_display_dir.join(relative)
            }
        })
        .unwrap_or_else(|_| canonical_path.to_path_buf())
}

fn load_discovery_config(root: &Path) -> Result<DiscoveryConfig> {
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

    let mut builder = GitignoreBuilder::new(root);

    let base_excludes = config.exclude.clone().unwrap_or_else(|| {
        DEFAULT_EXCLUDES
            .iter()
            .map(|value| (*value).to_string())
            .collect()
    });
    for pattern in base_excludes {
        builder
            .add_line(None, &pattern)
            .with_context(|| format!("Invalid exclude pattern: {pattern}"))?;
    }

    for pattern in config.extend_exclude.unwrap_or_default() {
        builder
            .add_line(None, &pattern)
            .with_context(|| format!("Invalid extend-exclude pattern: {pattern}"))?;
    }

    let ignore_file = config
        .ignore_file
        .map(|path| root.join(path))
        .unwrap_or_else(|| root.join(".t-linterignore"));
    if ignore_file.is_file() {
        builder.add(ignore_file);
    }

    let matcher = builder.build()?;
    Ok(DiscoveryConfig {
        root: root.to_path_buf(),
        matcher,
    })
}

fn find_config_root(start_dir: &Path) -> PathBuf {
    for dir in start_dir.ancestors() {
        if dir.join("pyproject.toml").is_file() || dir.join(".t-linterignore").is_file() {
            return dir.to_path_buf();
        }
    }
    start_dir.to_path_buf()
}

fn should_ignore_path(path: &Path, is_dir: bool, discovery: &DiscoveryConfig) -> bool {
    let relative = path.strip_prefix(&discovery.root).unwrap_or(path);
    if relative.as_os_str().is_empty() {
        return false;
    }

    discovery.matcher.matched(relative, is_dir).is_ignore()
}

fn is_python_file(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|extension| extension.eq_ignore_ascii_case("py"))
        .unwrap_or(false)
}
