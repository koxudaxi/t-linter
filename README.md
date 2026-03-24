# t-linter 🐍✨

Intelligent syntax highlighting, validation, and formatting for Python template strings (PEP 750).

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![PyPI](https://img.shields.io/pypi/v/t-linter.svg)](https://pypi.org/project/t-linter/)
[![VSCode Marketplace](https://img.shields.io/visual-studio-marketplace/v/koxudaxi.t-linter.svg)](https://marketplace.visualstudio.com/items?itemName=koxudaxi.t-linter)

> 📣 💼 Maintainer update: Open to opportunities. 🔗 [koxudaxi.dev](https://koxudaxi.dev/?utm_source=github_readme&utm_medium=top&utm_campaign=open_to_work)

## 📖 Documentation

**👉 [t-linter.koxudaxi.dev](https://t-linter.koxudaxi.dev)**

- 📦 [Installation](https://t-linter.koxudaxi.dev/installation/) - PyPI, VSCode Extension, Build from source
- 🔍 [Check Command](https://t-linter.koxudaxi.dev/usage/cli/check/) - CLI validation & output formats
- 🧹 [Format Command](https://t-linter.koxudaxi.dev/usage/cli/format/) - Canonical formatting for supported templates
- 🖥️ [LSP Server](https://t-linter.koxudaxi.dev/usage/cli/lsp/) - Editor integration (VSCode, Claude Code, Codex, Neovim, etc.)
- ⚙️ [Configuration](https://t-linter.koxudaxi.dev/usage/configuration/) - pyproject.toml & ignore files
- 🌐 [Supported Languages](https://t-linter.koxudaxi.dev/supported-languages/) - HTML, T-HTML, TDOM, SQL, JS, CSS, JSON, YAML, TOML

---

## Overview

t-linter validates and formats embedded languages inside Python template strings (PEP 750). Built with Rust and Tree-sitter for speed, it ships as a single binary that works as both a CLI tool and an LSP server.

- **`t-linter check`** — validate template string syntax
- **`t-linter format`** — canonically reformat supported template literals
- **`t-linter lsp`** — start the Language Server Protocol server for editor integration

## Features

- 🔍 **Linting** - Detect syntax errors in embedded HTML, JSON, YAML, TOML, CSS, JavaScript, SQL
- 🧹 **Formatting** - Canonical formatting for HTML, T-HTML, TDOM, JSON, YAML, TOML templates
- 🎨 **Syntax Highlighting** - Smart highlighting via LSP semantic tokens
- 🔧 **Type-based Detection** - Understands `Annotated[Template, "html"]` and type aliases
- 🧩 **Callee Inference** - Detects backend languages from helpers such as `tdom.html(...)`
- 🚀 **Fast** - Single Rust binary with Tree-sitter parsers

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

## Installation

### PyPI (Recommended)

```bash
pip install t-linter
```

Or add to your project's dependencies:
```bash
# Using uv (recommended)
uv add t-linter

# Or using pip with requirements.txt
echo "t-linter" >> requirements.txt
pip install -r requirements.txt
```

This provides the `t-linter` CLI tool and LSP server.

**[→ View on PyPI](https://pypi.org/project/t-linter/)**

### VSCode Extension

If you use VSCode, install the extension for seamless editor integration:

1. Open VSCode → Extensions (Ctrl+Shift+X / Cmd+Shift+X)
2. Search for "t-linter" → Install "T-Linter" by koxudaxi
3. On Linux x64, macOS x64/arm64, and Windows x64, the extension bundles the `t-linter` binary, so no separate PyPI install is required.
4. On unsupported platforms, or if you want to override the bundled binary, install `t-linter` via PyPI (see above) and set `t-linter.serverPath` to the full executable path.
5. Choose one save-time formatting mode:
   - **Ruff coexistence mode** keeps Ruff as the Python formatter and runs t-linter through `source.fixAll.t-linter`:
     ```json
     {
       "[python]": {
         "editor.defaultFormatter": "charliermarsh.ruff",
         "editor.formatOnSave": true,
         "editor.codeActionsOnSave": {
           "source.fixAll.t-linter": "explicit"
         }
       }
     }
     ```
   - **t-linter formatter mode** keeps the existing formatter-only workflow:
     ```json
     {
       "[python]": {
         "editor.defaultFormatter": "koxudaxi.t-linter",
         "editor.formatOnSave": true
       }
     }
     ```
   VSCode only allows one default formatter per language, which is why t-linter also exposes a save-time code action lane for template-string formatting.
6. If semantic highlighting conflicts with another Python extension, set the Python language server to `"None"` in your workspace settings:
   ```json
   {
       "python.languageServer": "None"
   }
   ```
   If you need those features, you can keep the Python language server enabled. t-linter will still provide template-string diagnostics, code actions, and formatting, though semantic highlighting may conflict.

If you use an external `t-linter` binary and it is not in your PATH, set `t-linter.serverPath` in VSCode settings to the full path of the executable.

**[→ Install from VSCode Marketplace](https://marketplace.visualstudio.com/items?itemName=koxudaxi.t-linter)**

### Build from Source

```bash
git clone https://github.com/koxudaxi/t-linter
cd t-linter
cargo install --path crates/t-linter
```

## Usage

### Check

Validate template strings for syntax errors:

```bash
# Check a single file
t-linter check file.py

# Check a directory
t-linter check src/

# Output formats: human (default), json, github
t-linter check file.py --format json
t-linter check file.py --format github  # GitHub Actions annotations

# Exit with error code if issues found (useful for CI)
t-linter check file.py --error-on-issues
```

Example output:
```text
example.py:4:46: error[embedded-parse-error] Invalid json syntax in template string (language=json)
1 files scanned, 1 templates scanned, 1 diagnostics, 0 failed files
```

Exit codes:

| Code | Meaning |
|------|---------|
| `0` | Run completed successfully |
| `1` | Issues were found and `--error-on-issues` was set |
| `2` | Operational failure such as an unreadable file |

### Format

Rewrite supported template literals (HTML, T-HTML, JSON, YAML, TOML) in place:

```bash
# Format a single file
t-linter format file.py

# Format a directory
t-linter format src/

# Check whether formatting would change any files (for CI)
t-linter format --check file.py

# Override the formatter line length
t-linter format --line-length 100 file.py

# Format stdin
cat file.py | t-linter format --stdin-filename file.py -
```

Templates in unsupported languages (CSS, JavaScript, SQL) are left unchanged.

### LSP Server

Start the Language Server Protocol server for editor integration:

```bash
t-linter lsp
```

The LSP server provides:

- **Semantic Tokens** — syntax highlighting for embedded languages
- **Diagnostics** — real-time validation with 250ms debouncing
- **Document Formatting** — full document and range formatting
- **Code Actions** — `source.fixAll.t-linter` for document-level rewrites and `refactor.rewrite.t-linter` for single-template selection rewrites

#### Claude Code

Add t-linter as an LSP server in your project's `.claude/settings.json`:

```json
{
  "lsp": {
    "t-linter": {
      "command": "t-linter",
      "args": ["lsp"],
      "languages": ["python"]
    }
  }
}
```

Claude Code will then use t-linter's diagnostics when editing Python files containing template strings.

#### Codex

Add the LSP configuration to your project's `codex.json` or start t-linter's LSP server as part of your development environment. The `t-linter check` and `t-linter format` commands can also be used directly in Codex's sandbox:

```bash
t-linter check src/
t-linter format --check src/
```

#### Neovim

```lua
vim.lsp.start({
  name = "t-linter",
  cmd = { "t-linter", "lsp" },
  filetypes = { "python" },
})
```

#### Other Editors

Any editor with LSP support can use t-linter. Configure the LSP client to start `t-linter lsp` as the server command for Python files.

### Configuration

Configuration via `pyproject.toml`:

```toml
[tool.t-linter]
line-length = 80
extend-exclude = ["generated", "vendor"]
ignore-file = ".t-linterignore"
```

| Key | Description |
|-----|-------------|
| `line-length` | Formatter print width (applies to HTML and T-HTML only; JSON, YAML, and TOML use fixed formatting rules) |
| `exclude` | Override the built-in default excludes |
| `extend-exclude` | Add more exclude patterns on top of the defaults |
| `ignore-file` | Path to a gitignore-style ignore file, relative to the project root |

By default, `t-linter` also reads `.t-linterignore` from the project root if it exists.

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

## Roadmap

### Planned Features
- ✅ **Language Server Protocol (LSP)** - Fully implemented
- ✅ **Syntax Highlighting** - Supports HTML, T-HTML, SQL, JavaScript, CSS, JSON, YAML, TOML
- ✅ **Type Alias Support** - Recognizes `type html = Annotated[Template, "html"]`
- ✅ **Linting (`check` command)** - Validate template strings for syntax errors
- ✅ **Formatting (`format` command)** - Canonical formatting for HTML, T-HTML, JSON, YAML, TOML
- 🚧 **Statistics (`stats` command)** - Analyze template string usage across codebases
- 📋 **Cross-file Type Resolution** - Track type aliases across module boundaries
- 📋 **Auto-completion** - Context-aware completions within template strings
