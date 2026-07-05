# T-Linter for Visual Studio Code

Intelligent syntax highlighting and validation for Python template strings (PEP 750) with embedded language support.

![T-Linter in action](images/img.png)

## Features

- 🎨 **Smart Syntax Highlighting** - Automatic detection and highlighting of embedded languages
- 🔍 **Type-based Detection** - Understands `Annotated[Template, "language"]` annotations
- 💾 **Save-time Template Formatting** - Use `source.fixAll.t-linter` alongside Ruff or keep t-linter as the formatter
- 🚀 **Fast & Lightweight** - Built with Rust for optimal performance
- 🔧 **Highly Configurable** - Customize behavior to match your workflow

### Supported Languages
- HTML
- T-HTML (component-based HTML)
- TDOM
- SQL
- JavaScript
- CSS
- JSON
- YAML
- TOML

## Requirements

- Visual Studio Code 1.110.0 or higher
- Python projects that use PEP 750 template strings still require Python 3.14+

## Installation

### Step 1: Install the VSCode extension
1. Open VSCode
2. Go to Extensions (Ctrl+Shift+X)
3. Search for "t-linter"
4. Click Install on "T-Linter - Python Template Strings Highlighter & Linter" by koxudaxi

The extension bundles `t-linter` binaries for Linux x64, macOS x64/arm64, and Windows x64, so those platforms do not need a separate CLI installation. On other platforms, install an external `t-linter` binary and set `t-linter.serverPath`.

### Step 2: Choose your save-time formatting mode

VSCode supports only one default formatter per language. t-linter therefore supports a Ruff coexistence code action mode, a composed Ruff + t-linter formatter mode, and a t-linter-only formatter mode.

#### Ruff coexistence mode

Use Ruff for Python formatting and t-linter for template literals:

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

#### Composed Ruff + t-linter formatter mode

Use t-linter as the default formatter and run Ruff's save pipeline before template formatting:

```json
{
  "[python]": {
    "editor.defaultFormatter": "koxudaxi.t-linter",
    "editor.formatOnSave": true
  },
  "t-linter.format.runRuffPipeline": true
}
```

t-linter starts a Ruff LSP server, applies Ruff fixAll, import organization, and formatting edits to an in-memory shadow document, applies t-linter template formatting, and returns one composed edit set. Keep the Ruff extension installed for its settings UI; t-linter reads safe `ruff.*` settings and uses `ruff.path` when it points to an executable. Otherwise the Rust resolver tries venv/conda, workspace `.venv` or `venv`, uv projects, and then `ruff` on `PATH`.

The same composed formatter is available outside VSCode through `t-linter lsp --ruff-pipeline` or LSP `initializationOptions.ruffPipeline`.

#### t-linter-only formatter mode

Keep the original formatter-only workflow without the Ruff pipeline:

```json
{
  "[python]": {
    "editor.defaultFormatter": "koxudaxi.t-linter",
    "editor.formatOnSave": true
  }
}
```

`source.fixAll.t-linter` formats every format-capable template literal in the current file. The manual `refactor.rewrite.t-linter` action appears only when your selection maps to exactly one template literal.

### Step 3: Disable Python Language Server (optional)
If another Python extension conflicts with t-linter's syntax highlighting, you can disable the Python language server:

1. Open VSCode Settings (Ctrl+, / Cmd+,)
2. Search for "python.languageServer"
3. Set it to "None"

Alternatively, add to your settings.json:
```json
{
    "python.languageServer": "None"
}
```

[Learn more about Python language server settings](https://code.visualstudio.com/docs/python/settings-reference#_intellisense-engine-settings)

### Step 4: Configure server path (optional)
If you want to override the bundled binary, or if you are on an unsupported platform:

```bash
pip install t-linter
```

1. **Find your t-linter path**:
   ```bash
   which t-linter     # macOS/Linux
   where t-linter     # Windows
   ```

2. **Set the path in VSCode settings**:
   - Open Settings (Ctrl+, / Cmd+,)
   - Search for `t-linter.serverPath`
   - Enter the full path to your t-linter executable

Bundled binary support matrix:

| Platform | Bundled binary | `t-linter.serverPath` required |
|---|:---:|:---:|
| Linux x64 | ✅ | No |
| macOS x64 | ✅ | No |
| macOS arm64 | ✅ | No |
| Windows x64 | ✅ | No |
| Other platforms | — | Yes |

### Python Language Server

t-linter handles template diagnostics, highlighting, formatting, and supported
cross-module template-language inference itself. Keep your normal Python
language server enabled for Python completions, navigation, and non-template
type checking.

## Usage

### Basic Example
```python
from typing import Annotated
from string.templatelib import Template

# Automatic HTML syntax highlighting
def render_page(content: Annotated[Template, "html"]) -> str:
    return content.render()

page = t"""
<!DOCTYPE html>
<html>
    <body>
        <h1>{title}</h1>
        <p>{content}</p>
    </body>
</html>
"""

# SQL highlighting
query: Annotated[Template, "sql"] = t"SELECT * FROM users WHERE id = {user_id}"

# YAML highlighting
config: Annotated[Template, "yaml"] = t"""
app:
  name: {app_name}
  debug: true
"""
```

### Type Alias Support
```python
# Define reusable type aliases
type html = Annotated[Template, "html"]
type sql = Annotated[Template, "sql"]
type toml_config = Annotated[Template, "toml"]

# Use with automatic language detection
content: html = t"<div>{message}</div>"
db_query: sql = t"UPDATE users SET name = {name} WHERE id = {id}"
package: toml_config = t"name = {package_name}"
```

### Function Parameter Inference
```python
def execute_query(query: sql) -> list:
    return db.execute(query)

# Language inferred from function parameter type
execute_query(t"SELECT * FROM products WHERE price < {max_price}")
```

## Migration Notes

If you previously used t-linter only as `editor.defaultFormatter`, you can switch to Ruff coexistence mode by:

1. Changing `editor.defaultFormatter` to `charliermarsh.ruff`
2. Keeping `editor.formatOnSave = true`
3. Adding `editor.codeActionsOnSave = { "source.fixAll.t-linter": "explicit" }`

If you prefer the previous setup, t-linter formatter mode remains supported.

## Configuration

This extension contributes the following settings:

- **`t-linter.enabled`**: Enable/disable the t-linter extension
- **`t-linter.serverPath`**: Path to t-linter executable (leave empty for automatic detection)
- **`t-linter.format.runRuffPipeline`**: Run Ruff fixAll, import organization, and formatting before t-linter formatting when t-linter is the Python formatter (default: false)
- **`t-linter.highlightUntyped`**: Highlight template strings without type annotations (default: true)
- **`t-linter.enableTypeChecking`**: Enable integration with Python type checkers for cross-module resolution (default: true)
- **`t-linter.typeChecking.enabled`**: Enable JSON, YAML, TOML interpolation value type checking and TDOM component prop interpolation type checking through Ty, Pyright, or Pyrefly (default: false)
- **`t-linter.typeChecking.checker`**: Type checker backend for interpolation value type checking (`ty`, `pyright`, or `pyrefly`; default: `ty`)
- **`t-linter.typeChecking.command`**: Optional path to the selected type checker executable
- **`t-linter.typeChecking.tyPath`**: Deprecated optional path to a `ty` executable; use `t-linter.typeChecking.command`
- **`t-linter.trace.server`**: Trace communication between VSCode and the language server (off/messages/verbose)

## Commands

This extension contributes the following commands:

- **`T-Linter: Restart Server`**: Restart the t-linter language server
- **`T-Linter: Show Template String Statistics`**: Display statistics about template strings in the current file (🚧 Coming soon)

## Troubleshooting

### Language server not found
If you see "t-linter binary not found":

1. **Reinstall the extension** to restore the bundled binary
2. **If you are using a custom binary, ensure it is installed**:
   ```bash
   pip install t-linter
   # Or if using requirements.txt:
   pip install -r requirements.txt
   ```

3. **Verify installation**:
   ```bash
   t-linter --version
   ```

4. **Configure server path manually**:
   - Find the path: `which t-linter` (macOS/Linux) or `where t-linter` (Windows)
   - Common installation paths:
     - **Windows**: `C:\Users\YourName\AppData\Local\Programs\Python\Python3xx\Scripts\t-linter.exe`
     - **macOS**: `/Users/yourname/.local/bin/t-linter` or `/usr/local/bin/t-linter`
     - **Linux**: `/home/yourname/.local/bin/t-linter` or `/usr/local/bin/t-linter`
   - Set `t-linter.serverPath` in VSCode settings
   - Restart VSCode

### No syntax highlighting
1. Ensure the VSCode extension is installed
2. **Verify that Python language server is disabled**: `python.languageServer` should be set to "None"
3. Check that Python semantic highlighting is enabled in VSCode
4. Verify your template strings use the `t"..."` syntax
5. Ensure type annotations are correctly formatted
6. Try restarting the language server: `Ctrl+Shift+P` → "T-Linter: Restart Server"

### Performance issues
- Disable `t-linter.enableTypeChecking` if you don't need cross-module type resolution
- Set `t-linter.trace.server` to "off" in production
- Restart VSCode after changing settings

## Known Issues

- t-linter does not provide Python completions or navigation.

## Contributing

Found a bug or have a feature request? Please open an issue on our [GitHub repository](https://github.com/koxudaxi/t-linter/issues).

## License

MIT - See [LICENSE](https://github.com/koxudaxi/t-linter/blob/main/LICENSE) for details.
