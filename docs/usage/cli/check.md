# Check Command

The `check` command validates Python template strings for syntax errors in embedded languages.

`check --format` controls the report output format only. To rewrite supported
JSON, YAML, and TOML template literals, use [`t-linter format`](./format.md).

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
example.py:4:46: error[embedded-parse-error] Invalid json syntax in template string (language=json)
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
          "message": "Invalid json syntax in template string",
          "file": "example.py",
          "start_line": 4,
          "start_column": 46,
          "end_line": 4,
          "end_column": 47
        }
      ]
    }
  ],
  "diagnostics": [
    {
      "rule": "embedded-parse-error",
      "severity": "error",
      "language": "json",
      "message": "Invalid json syntax in template string",
      "file": "example.py",
      "start_line": 4,
      "start_column": 46,
      "end_line": 4,
      "end_column": 47
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
::error file=example.py,line=4,col=46,title=t-linter(embedded-parse-error)::Invalid json syntax in template string (language=json)
```

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
example.py:4:46: error[embedded-parse-error] Invalid json syntax in template string (language=json)
1 files scanned, 1 templates scanned, 1 diagnostics, 0 failed files
```
