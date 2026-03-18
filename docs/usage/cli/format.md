# Format Command

The `format` command rewrites supported Python template strings in place.

Supported formatter backends:

- JSON
- YAML / YML
- TOML

Unsupported embedded languages are left unchanged.

## Basic Usage

```bash
# Format a single file
t-linter format file.py

# Format a directory recursively
t-linter format src/

# Default target is the current directory
t-linter format
```

## Check Mode

Use `--check` to report files that would change without rewriting them:

```bash
t-linter format --check file.py
```

Exit codes:

| Code | Meaning |
|------|---------|
| `0` | Formatting check succeeded with no changes needed |
| `1` | `--check` found at least one file that would be reformatted |
| `2` | Operational failure such as an unreadable file or invalid input |

## Stdin

Use `-` to read a Python document from stdin:

```bash
cat file.py | t-linter format -
cat file.py | t-linter format --check --stdin-filename file.py -
```

`--stdin-filename` only affects display labels in v1. It does not change config
resolution or formatter behavior.

## Notes

- `format` respects `pyproject.toml` excludes and `.t-linterignore`
- explicit file operands must use the `.py` extension
- formatting is atomic per file: on failure, the original file is left untouched
