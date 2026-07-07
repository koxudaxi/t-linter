# Check Command

The `check` command validates Python template strings for syntax errors in embedded languages.

`check --format` controls the report output format only. To rewrite supported
HTML, T-HTML, TDOM, JSON, YAML, and TOML template literals, use [`t-linter format`](./format.md).

## Basic Usage

```bash
# Check a single file
t-linter check file.py

# Check a directory
t-linter check src/
```

## Output Formats

Use the `--format` flag to control output format:

### Human (default)

```bash
t-linter check file.py
```

```text
example.py:4:47: error[embedded-parse-error] Expected a JSON value. (language=json)
1 files scanned, 1 templates scanned, 1 diagnostics, 0 failed files
```

### JSON

```bash
t-linter check file.py --format json
```

```json
{
  "files": [
    {
      "file": "example.py",
      "template_count": 1,
      "diagnostics": [
        {
          "rule": "embedded-parse-error",
          "severity": "error",
          "language": "json",
          "message": "Expected a JSON value.",
          "file": "example.py",
          "start_line": 4,
          "start_column": 47,
          "end_line": 4,
          "end_column": 48
        }
      ]
    }
  ],
  "diagnostics": [
    {
      "rule": "embedded-parse-error",
      "severity": "error",
      "language": "json",
      "message": "Expected a JSON value.",
      "file": "example.py",
      "start_line": 4,
      "start_column": 47,
      "end_line": 4,
      "end_column": 48
    }
  ],
  "summary": {
    "files_scanned": 1,
    "templates_scanned": 1,
    "diagnostics": 1,
    "failed_files": 0
  }
}
```

### GitHub Actions Annotations

```bash
t-linter check file.py --format github
```

```text
::error file=example.py,line=4,col=47,title=t-linter(embedded-parse-error)::Expected a JSON value. (language=json)
```

### SARIF

```bash
t-linter check file.py --format sarif
```

Use SARIF output with GitHub code scanning:

```yaml
- name: Run t-linter
  run: t-linter check . --format sarif > t-linter.sarif

- name: Upload SARIF
  uses: github/codeql-action/upload-sarif@v3
  with:
    sarif_file: t-linter.sarif
```

## Fixes

Some diagnostics include suggested edits. Apply them in place with `--fix`:

```bash
t-linter check file.py --fix
```

Preview the same edits without writing files with `--diff`:

```bash
t-linter check file.py --diff
```

`--fix` and `--diff` are mutually exclusive. Fixes are taken from the filtered
diagnostic list, so ignored or suppressed diagnostics are not rewritten. The
initial fixable rules are selected `sql-*` diagnostics and selected
`template-schema-*` diagnostics.

## JSON Schema Bindings

For JSON templates, t-linter can compare static object keys and values against
`TypedDict` schema annotations. The schema binding is carried by
`json_tstring.Json` marker kwargs:

```python
from typing import Annotated, TypedDict
from string.templatelib import Template
from json_tstring import Json

class Order(TypedDict):
    id: int
    name: str

payload: Annotated[Template, Json(schema=Order)] = t'{"id": "abc"}'
```

This template is parsed as JSON and checked against `Order`. t-linter reports
`template-schema-type-shape` for `"id": "abc"` and
`template-schema-missing-key` for the missing `name` key.

The supported binding forms are:

```python
payload: Annotated[Template, Json(schema=Order)] = t'{"id": 1, "name": "Ada"}'
default_json: Annotated[Template, Json] = t'{"id": 1, "name": "Ada"}'

type OrderPayload = Annotated[Template, Json(schema=Order)]
aliased: OrderPayload = t'{"id": 1, "name": "Ada"}'
```

`Json(schema=...)` may be imported directly, imported with an alias, or used as
`json_tstring.Json(...)`. The direct `Json[Order]` annotation is also accepted
as a shorthand, but `Json(schema=Order)` is the preferred form because marker
kwargs are where schema, dialect, and future options live.

String language metadata is still supported as the lightweight tier:
`Annotated[Template, "json"]`. Do not combine it with a marker for new code. If
`"json"` and `Json(...)` are used together, t-linter reports
`template-metadata-redundant-language` and suggests removing the string. If the
string and marker disagree, or if an annotation contains multiple language
markers, t-linter reports `template-metadata-conflict`.

## Error on Issues

Use `--error-on-issues` to exit with a non-zero code when issues are found:

```bash
t-linter check file.py --error-on-issues
```

This is useful for CI/CD pipelines.

## Exit Codes

| Code | Meaning |
|------|---------|
| `0` | Run completed successfully |
| `1` | Issues were found and `--error-on-issues` was set |
| `2` | Operational failure such as an unreadable file |

## Example

Given this input:

```python
from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "json"] = t"""[1,,2]"""
```

t-linter will report the invalid JSON syntax:

```text
example.py:4:47: error[embedded-parse-error] Expected a JSON value. (language=json)
1 files scanned, 1 templates scanned, 1 diagnostics, 0 failed files
```
