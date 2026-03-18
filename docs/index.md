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
- **Extensible** - Support for HTML, SQL, JavaScript, CSS, JSON, YAML, TOML, and more

For JSON, YAML, and TOML, t-linter splits responsibilities:

- `semanticTokens`: Tree-sitter only, for low-latency highlighting
- `check`: strict parsing through the `tstring-json`, `tstring-yaml`, and `tstring-toml` backends
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

# HTML template with syntax highlighting
page: Annotated[Template, "html"] = t"""
<!DOCTYPE html>
<html>
    <head>
        <title>{title}</title>
    </head>
    <body>
        <h1>{heading}</h1>
        <p>{content}</p>
    </body>
</html>
"""

# SQL query with syntax highlighting
query: Annotated[Template, "sql"] = t"""
SELECT u.name, u.email, p.title
FROM users u
JOIN posts p ON u.id = p.author_id
WHERE u.created_at > {start_date}
ORDER BY u.name
"""

# Type aliases for reusable templates
type css = Annotated[Template, "css"]
type js = Annotated[Template, "javascript"]
type yaml_config = Annotated[Template, "yaml"]
type toml_config = Annotated[Template, "toml"]

styles: css = t"""
.container {
    max-width: 1200px;
    margin: 0 auto;
    padding: {padding}px;
}
"""
```
