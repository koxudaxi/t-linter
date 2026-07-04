# Configuration

t-linter can be configured via `pyproject.toml` and ignore files.

## pyproject.toml

Add a `[tool.t-linter]` section to your `pyproject.toml`:

```toml
[tool.t-linter]
line-length = 80
extend-exclude = ["generated", "vendor"]
ignore-file = ".t-linterignore"
```

### Supported Keys

| Key | Description |
|-----|-------------|
| `line-length` | Formatter print width for HTML, T-HTML, and TDOM templates only |
| `exclude` | Override the built-in default excludes |
| `extend-exclude` | Add more exclude patterns on top of the defaults |
| `ignore-file` | Path to a gitignore-style ignore file, relative to the project root |

## Ignore File

By default, t-linter reads `.t-linterignore` from the project root if it exists. This file follows the same syntax as `.gitignore`:

```text
# Ignore generated files
generated/
*_generated.py

# Ignore vendor directory
vendor/

# Ignore specific files
tests/fixtures/*.py
```

You can specify a custom ignore file path using the `ignore-file` key in `pyproject.toml`.
