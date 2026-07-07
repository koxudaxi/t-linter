# t-linter

Intelligent syntax highlighting, validation, and formatting for Python template strings ([PEP 750](https://peps.python.org/pep-0750/)).

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![PyPI](https://img.shields.io/pypi/v/t-linter.svg)](https://pypi.org/project/t-linter/)
[![VSCode Marketplace](https://img.shields.io/visual-studio-marketplace/v/koxudaxi.t-linter.svg)](https://marketplace.visualstudio.com/items?itemName=koxudaxi.t-linter)

## Overview

t-linter validates and formats embedded languages inside Python template strings (PEP 750). Built with Rust and Tree-sitter for speed, it ships as a single binary that works as both a CLI tool and an LSP server.

- **`t-linter check`** — validate template string syntax
- **`t-linter format`** — canonically reformat supported template literals
- **`t-linter lsp`** — start the Language Server Protocol server for editor integration

## Features

- **Linting** - Detect syntax errors in embedded HTML, JSON, YAML, TOML, CSS, JavaScript, SQL
- **Formatting** - Canonical formatting for HTML, T-HTML, TDOM, JSON, YAML, TOML templates
- **Syntax Highlighting** - Smart highlighting via LSP semantic tokens
- **Type-based Detection** - Understands `Annotated[Template, "html"]`, metadata markers such as `Json(schema=...)`, and type aliases
- **Interpolation Type Checking** - Optional LSP diagnostics for JSON, YAML, TOML interpolation values and TDOM component prop interpolations through Ty, Pyright, or Pyrefly
- **Callee Inference** - Detects backend languages from helpers such as `tdom.html(...)`
- **Fast** - Single Rust binary with Tree-sitter parsers

## Supported Languages

| Language | Annotation | Check | Format | Highlight |
|----------|-----------|:-----:|:------:|:---------:|
| **HTML** | `"html"` | ✅ | ✅ | ✅ |
| **T-HTML** | `"thtml"` | ✅ | ✅ | ✅ |
| **TDOM** | `"tdom"` | ✅ | ✅ | ✅ |
| **JSON** | `"json"` | ✅ | ✅ | ✅ |
| **YAML** | `"yaml"`, `"yml"` | ✅ | ✅ | ✅ |
| **TOML** | `"toml"` | ✅ | ✅ | ✅ |
| **CSS** | `"css"` | ✅ | — | ✅ |
| **JavaScript** | `"javascript"`, `"js"` | ✅ | — | ✅ |
| **SQL** | `"sql"` | ✅ | — | ✅ |

For HTML, T-HTML, TDOM, JSON, YAML, and TOML, t-linter uses dedicated Rust backends (`tstring-*` crates) for strict validation and canonical formatting. CSS, JavaScript, and SQL use Tree-sitter for syntax validation and highlighting.

## Quick Start

Install t-linter:

```bash
pip install t-linter
```

Check your Python files for template string issues:

```bash
t-linter check src/
```

Rewrite supported template literals in place:

```bash
t-linter format src/
```

Start the LSP server for editor integration:

```bash
t-linter lsp
```

## Quick Start Example

```python
from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> None:
    pass


def run_sql(template: Annotated[Template, "sql"]) -> None:
    pass


type css = Annotated[Template, "css"]
type yaml_config = Annotated[Template, "yaml"]
type toml_config = Annotated[Template, "toml"]


def load_styles(template: css) -> None:
    pass


def load_yaml(template: yaml_config) -> None:
    pass


def load_toml(template: toml_config) -> None:
    pass


title = "t-linter"
heading = "Template strings with syntax highlighting"
content = "Interpolations stay as normal Python expressions."

render_html(t"""
<!DOCTYPE html>
<html>
    <head>
        <title>{title}</title>
    </head>
    <body>
        <h1 style="color: #007acc">{heading}</h1>
        <p>{content}</p>
    </body>
</html>
""")

start_date = "2026-01-01"

run_sql(t"""
SELECT u.name, u.email, p.title
FROM users u
JOIN posts p ON u.id = p.author_id
WHERE u.created_at > {start_date}
ORDER BY u.name
""")

padding = 24

load_styles(t"""
.container {{
    max-width: 1200px;
    margin: 0 auto;
    padding: {padding}px;
}}
""")

app_name = "demo-app"

load_yaml(t"""
app:
  name: {app_name}
  debug: true
""")

project_name = "demo-project"
version = "0.1.0"

load_toml(t"""
[project]
name = "{project_name}"
version = "{version}"
""")
```

Use `{{` and `}}` when the embedded language needs literal braces, such as CSS
or JSON objects.

For `html`, `<title>{value}</title>` is allowed and treated as escaped text.
`<script>`, `<style>`, and `<textarea>` still reject interpolations for safety.
