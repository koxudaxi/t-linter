# Configuration

t-linter can be configured via `pyproject.toml` and ignore files.

## pyproject.toml

Add a `[tool.t-linter]` section to your `pyproject.toml`:

```toml
[tool.t-linter]
line-length = 80
extend-exclude = ["generated", "vendor"]
ignore-file = ".t-linterignore"
ignore = ["component-unexpected-prop"]

[tool.t-linter.severity]
component-missing-prop = "warning"

[tool.t-linter.per-file-ignores]
"tests/**" = ["component-unexpected-prop"]
```

### Supported Keys

| Key | Description |
|-----|-------------|
| `line-length` | Formatter print width for HTML, T-HTML, and TDOM templates only |
| `exclude` | Override the built-in default excludes |
| `extend-exclude` | Add more exclude patterns on top of the defaults |
| `ignore-file` | Path to a gitignore-style ignore file, relative to the project root |
| `ignore` | Disable lint rules globally |
| `severity` | Override rule severity with `error` or `warning` |
| `per-file-ignores` | Disable lint rules for paths matching project-root-relative globs |

Unknown rule names are accepted so projects can share configuration across
different t-linter versions. `python-parse-error` and `file-read-error` are not
disabled by rule ignore settings.

Changing a rule to `warning` changes the printed severity only. `check
--error-on-issues` still exits with code `1` when any diagnostics remain.

## Inline Suppression

Use `# t-linter: ignore` to suppress all lint diagnostics for a line, or
`# t-linter: ignore[rule-a, rule-b]` for specific rules:

```python
payload: Annotated[Template, "html"] = t"<div><"  # t-linter: ignore

# t-linter: ignore[component-unexpected-prop]
template: Annotated[Template, "thtml"] = t"<Button label='Save' tone='info' />"
```

Line-end comments suppress diagnostics that start on the same line. Standalone
comments suppress diagnostics that start on the following line. When the comment
is placed on the template string start line, or immediately before it, it also
suppresses diagnostics that start inside that template string.

Inline suppression applies to t-linter lint diagnostics. LSP diagnostics from
external type checkers are not suppressed by these comments.

## Rule Names

- `embedded-parse-error`
- `file-read-error`
- `python-parse-error`
- `component-missing-prop`
- `component-unexpected-prop`
- `component-prop-type-error`
- `component-unresolved`
- `template-schema-missing-key`
- `template-schema-unknown-key`
- `template-schema-type-shape`
- `binding-unresolved`
- `sql-conversion-unsupported`
- `sql-format-spec-unknown`
- `sql-composable-spec-mismatch`
- `sql-dict-needs-json-wrapper`
- `sql-in-clause`
- `sql-multi-statement`
- `sql-tuple-parameter`

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
