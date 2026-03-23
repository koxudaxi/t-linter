# t-linter 🐍✨

Intelligent syntax highlighting and validation for Python template strings (PEP 750).

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![VSCode Marketplace](https://img.shields.io/visual-studio-marketplace/v/koxudaxi.t-linter.svg)](https://marketplace.visualstudio.com/items?itemName=koxudaxi.t-linter)
[![PyPI](https://img.shields.io/pypi/v/t-linter.svg)](https://pypi.org/project/t-linter/)

> 📣 💼 Maintainer update: Open to opportunities. 🔗 [koxudaxi.dev](https://koxudaxi.dev/?utm_source=github_readme&utm_medium=top&utm_campaign=open_to_work)

## 📖 Documentation

**👉 [t-linter.koxudaxi.dev](https://t-linter.koxudaxi.dev)**

- 📦 [Installation](https://t-linter.koxudaxi.dev/installation/) - VSCode Extension, PyPI, Build from source
- 🎨 [VSCode Extension](https://t-linter.koxudaxi.dev/usage/vscode/) - Editor integration & setup
- 🔍 [Check Command](https://t-linter.koxudaxi.dev/usage/cli/check/) - CLI validation & output formats
- 🧹 [Format Command](https://t-linter.koxudaxi.dev/usage/cli/format/) - Canonical formatting for supported templates
- ⚙️ [Configuration](https://t-linter.koxudaxi.dev/usage/configuration/) - pyproject.toml & ignore files
- 🌐 [Supported Languages](https://t-linter.koxudaxi.dev/supported-languages/) - HTML, T-HTML, SQL, JS, CSS, JSON, YAML, TOML

---

## Overview

t-linter provides intelligent syntax highlighting and linting for Python template strings (PEP 750) through multiple distribution channels:

- **🔧 Command-line tool**: Install via PyPI (`pip install t-linter`) for direct CLI usage and LSP server
- **🎨 VSCode Extension**: Install from the Visual Studio Code Marketplace for seamless editor integration

![T-Linter VSCode Extension in action](editors/vscode/images/img.png)

## Features

- 🎨 **Smart Syntax Highlighting** - Detects embedded languages in `t"..."` strings
- 🔍 **Type-based Detection** - Understands `Annotated[Template, "html"]` annotations
- 🚀 **Fast** - Built with Rust and Tree-sitter for optimal performance
- 🔧 **Extensible** - Support for HTML, T-HTML, SQL, JavaScript, CSS, JSON, YAML, TOML, and more

For HTML, T-HTML, JSON, YAML, and TOML, `t-linter` now splits responsibilities:

- `semanticTokens`: Tree-sitter only, for low-latency highlighting
- `check`: strict parsing through the `tstring-html`, `tstring-thtml`, `tstring-json`, `tstring-yaml`, and `tstring-toml` backends
- `formatting`: canonical formatting through the same Rust backends

## Installation

### Option 1: VSCode Extension (Recommended for VSCode users)

**Step 1: Install the t-linter binary**
Install t-linter as a project dependency (recommended):
```bash
pip install t-linter
```

For better project isolation, add it to your project's requirements:
```bash
# Using pip with requirements.txt
echo "t-linter" >> requirements.txt
pip install -r requirements.txt

# Or using uv (recommended for faster installs)
uv add t-linter
```

**Step 2: Install the VSCode extension**
Install the extension from the Visual Studio Code Marketplace:

1. Open VSCode
2. Go to Extensions (Ctrl+Shift+X / Cmd+Shift+X)
3. Search for "t-linter"
4. Click Install on "T-Linter - Python Template Strings Highlighter & Linter" by koxudaxi

**Step 3: Disable Python Language Server**
To prevent conflicts with t-linter's syntax highlighting, disable the Python language server:

1. Open VSCode Settings (Ctrl+, / Cmd+,)
2. Search for "python.languageServer"
3. Set it to "None"

Alternatively, add to your settings.json:
```json
{
    "python.languageServer": "None"
}
```

**Step 4: Configure the server path (if needed)**
If t-linter is not in your PATH, configure the server path in VSCode settings:

1. **Find your t-linter path** by running in terminal:
   ```bash
   which t-linter     # macOS/Linux
   where t-linter     # Windows
   ```

2. Open VSCode Settings (Ctrl+, / Cmd+,)
3. Search for "t-linter.serverPath"
4. Set the full path to your t-linter executable:
   - **Windows**: `C:\Users\YourName\AppData\Local\Programs\Python\Python3xx\Scripts\t-linter.exe`
   - **macOS**: `/Users/yourname/.local/bin/t-linter` or `/usr/local/bin/t-linter`
   - **Linux**: `/home/yourname/.local/bin/t-linter` or `/usr/local/bin/t-linter`

**[→ Install from VSCode Marketplace](https://marketplace.visualstudio.com/items?itemName=koxudaxi.t-linter)**

### Option 2: PyPI Package Only (CLI tool and LSP server)

For command-line usage or integration with other editors, install t-linter as a project dependency:

```bash
pip install t-linter
```

Or add to your project's dependencies:
```bash
# Using requirements.txt
t-linter

# Using uv
uv add t-linter

# Or manually in pyproject.toml
[project]
dependencies = [
    "t-linter",
    # other dependencies...
]
```

This provides the `t-linter` command-line tool and LSP server without the VSCode extension.

**[→ View on PyPI](https://pypi.org/project/t-linter/)**

### Option 3: Build from Source

For development or bleeding-edge features:

```bash
git clone https://github.com/koxudaxi/t-linter
cd t-linter
cargo install --path crates/t-linter
```

## Usage

### VSCode Extension
After installing both the PyPI package and VSCode extension, t-linter will automatically provide syntax highlighting for Python template strings. 

**Troubleshooting**: If syntax highlighting doesn't work:
1. Ensure `t-linter` is installed: Run `t-linter --version` in terminal
2. Check that Python language server is disabled: `python.languageServer` should be set to "None"
3. Check the server path in VSCode settings: `t-linter.serverPath`
4. Restart VSCode after making changes

### Command Line Interface
If you installed via PyPI, you can use t-linter from the command line:

**Run the language server** (for editor integration):
```bash
t-linter lsp
```

In the LSP, diagnostics are debounced and published from the dedicated Rust
backends for HTML, T-HTML, JSON, YAML, and TOML templates. Formatting requests
rewrite the whole template literal using the backend formatter while keeping
interpolation source such as `{name!r:>5}` intact.

**Check files for issues**:
```bash
# Check Python files for template string issues
t-linter check file.py
t-linter check src/

# Output formats
t-linter check file.py --format json
t-linter check file.py --format github  # GitHub Actions annotations
t-linter check file.py --error-on-issues  # Exit with error code if issues found
```

`check` supports `human`, `json`, and `github` output formats.

`check --format` controls report output formatting only. Use the `format`
subcommand to rewrite supported template literals in place:

```bash
# Format Python files containing HTML/T-HTML/JSON/YAML/TOML templates
t-linter format file.py
t-linter format src/

# Check whether formatting would change any files
t-linter format --check file.py

# Override the formatter line length
t-linter format --line-length 100 file.py

# Format stdin
cat file.py | t-linter format --stdin-filename file.py -
```

Configuration can be provided via `pyproject.toml`:

```toml
[tool.t-linter]
line-length = 80
extend-exclude = ["generated", "vendor"]
ignore-file = ".t-linterignore"
```

Supported keys:
- `line-length`: formatter print width for HTML and T-HTML templates only
- `exclude`: override the built-in default excludes
- `extend-exclude`: add more exclude patterns on top of the defaults
- `ignore-file`: path to a gitignore-style ignore file, relative to the project root

By default, `t-linter` also reads `.t-linterignore` from the project root if it exists.

Exit codes:
- `0`: Run completed successfully
- `1`: Issues were found and `--error-on-issues` was set
- `2`: Operational failure such as an unreadable file

Example input:
```python
from typing import Annotated
from string.templatelib import Template

payload: Annotated[Template, "json"] = t"""[1,,2]"""
```

Example `human` output:
```text
example.py:4:46: error[embedded-parse-error] Invalid json syntax in template string (language=json)
1 files scanned, 1 templates scanned, 1 diagnostics, 0 failed files
```

Example `json` output:
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

Example `github` output:
```text
::error file=example.py,line=4,col=46,title=t-linter(embedded-parse-error)::Invalid json syntax in template string (language=json)
```

**Get template string statistics** (🚧 Coming soon):
```bash
# Analyze template string usage in your codebase
t-linter stats .  # Current directory
t-linter stats src/  # Specific directory

# Expected output (when implemented):
# - Number of template strings by language
# - Template string locations
# - Language detection methods used
# - Type alias usage statistics
```

## Roadmap

### Planned Features
- ✅ **Language Server Protocol (LSP)** - Fully implemented
- ✅ **Syntax Highlighting** - Supports HTML, T-HTML, SQL, JavaScript, CSS, JSON, YAML, TOML
- ✅ **Type Alias Support** - Recognizes `type html = Annotated[Template, "html"]`
- ✅ **Linting (`check` command)** - Validate template strings for syntax errors
- 🚧 **Statistics (`stats` command)** - Analyze template string usage across codebases
- 📋 **Cross-file Type Resolution** - Track type aliases across module boundaries
- 📋 **Auto-completion** - Context-aware completions within template strings

## Quick Start Example

Here's a complete example you can run through `t-linter check` right away. Each
helper function declares the embedded language once, and the interpolation
values are defined next to the template usage:

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
