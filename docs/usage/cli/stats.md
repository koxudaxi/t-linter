# Stats Command

The `stats` command counts Python template strings without running lint
diagnostics.

## Basic Usage

```bash
# Analyze a directory
t-linter stats src/

# Analyze multiple paths
t-linter stats src/ tests/
```

## Output Formats

### Human (default)

```bash
t-linter stats .
```

```text
Files scanned:        12
Template strings:     34
  typed:              30 (88.2%)
  untyped:            4

By language:
  html              18
  sql               8
  json              4

By detection method:
  annotation        20
  callee-inference  10

Top files by template count:
  src/views.py      12
```

### JSON

```bash
t-linter stats . --format json
```

The JSON report includes totals, language counts, detection-method counts, and
per-file template counts.

## Exit Codes

| Code | Meaning |
|------|---------|
| `0` | Run completed successfully |
| `2` | One or more inputs could not be read or parsed |
