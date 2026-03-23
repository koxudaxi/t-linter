# t-linter

Intelligent syntax highlighting and validation for Python template strings ([PEP 750](https://peps.python.org/pep-0750/)).

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![VSCode Marketplace](https://img.shields.io/visual-studio-marketplace/v/koxudaxi.t-linter.svg)](https://marketplace.visualstudio.com/items?itemName=koxudaxi.t-linter)
[![PyPI](https://img.shields.io/pypi/v/t-linter.svg)](https://pypi.org/project/t-linter/)

## Overview

t-linter provides intelligent syntax highlighting and linting for Python template strings (PEP 750) through multiple distribution channels:

- **Command-line tool**: Install via PyPI (`pip install t-linter`) for direct CLI usage and LSP server
- **VSCode Extension**: Install from the Visual Studio Code Marketplace for seamless editor integration

## Features

- **Smart Syntax Highlighting** - Detects embedded languages in `t"..."` strings
- **Type-based Detection** - Understands `Annotated[Template, "html"]` annotations
- **Fast** - Built with Rust and Tree-sitter for optimal performance
- **Extensible** - Support for HTML, T-HTML, SQL, JavaScript, CSS, JSON, YAML, TOML, and more

For HTML, T-HTML, JSON, YAML, and TOML, t-linter splits responsibilities:

- `semanticTokens`: Tree-sitter only, for low-latency highlighting
- `check`: strict parsing through the `tstring-html`, `tstring-thtml`, `tstring-json`, `tstring-yaml`, and `tstring-toml` backends
- `formatting`: canonical formatting through the same Rust backends

## Quick Start

Install t-linter:

```bash
pip install t-linter
```

Check your Python files for template string issues:

```bash
t-linter check src/
```

Or rewrite supported template literals in place:

```bash
t-linter format src/
```

Or start the LSP server for editor integration:

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
