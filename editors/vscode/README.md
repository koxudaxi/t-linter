# T-Linter for Visual Studio Code

Intelligent syntax highlighting and validation for Python template strings (PEP 750) with embedded language support.

## Features

- 🎨 **Smart Syntax Highlighting** - Automatic detection and highlighting of embedded languages
- 🔍 **Type-based Detection** - Understands `Annotated[Template, "language"]` annotations
- 💡 **IntelliSense Support** - Code completion within template strings
- 🚀 **Fast & Lightweight** - Built with Rust for optimal performance
- 🔧 **Highly Configurable** - Customize behavior to match your workflow

### Supported Languages
- HTML
- SQL
- JavaScript
- CSS
- JSON

## Requirements

- Visual Studio Code 1.74.0 or higher
- Python 3.9+ (PEP 750 template strings require Python 3.14+)
- `t-linter` language server (installed automatically or manually)

## Installation

### Option 1: Install from VSCode Marketplace
1. Open VSCode
2. Go to Extensions (Ctrl+Shift+X)
3. Search for "t-linter"
4. Click Install

### Option 2: Install t-linter manually
```bash
pip install t-linter
```

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
```

### Type Alias Support (Python 3.12+)
```python
# Define reusable type aliases
type html = Annotated[Template, "html"]
type sql = Annotated[Template, "sql"]

# Use with automatic language detection
content: html = t"<div>{message}</div>"
db_query: sql = t"UPDATE users SET name = {name} WHERE id = {id}"
```

### Function Parameter Inference
```python
def execute_query(query: sql) -> list:
    return db.execute(query)

# Language inferred from function parameter type
execute_query(t"SELECT * FROM products WHERE price < {max_price}")
```

## Configuration

This extension contributes the following settings:

- **`t-linter.enabled`**: Enable/disable the t-linter extension
- **`t-linter.serverPath`**: Path to t-linter executable (leave empty for automatic detection)
- **`t-linter.highlightUntyped`**: Highlight template strings without type annotations (default: true)
- **`t-linter.enableTypeChecking`**: Enable integration with Python type checkers for cross-module resolution (default: true)
- **`t-linter.trace.server`**: Trace communication between VSCode and the language server (off/messages/verbose)

## Commands

This extension contributes the following commands:

- **`T-Linter: Restart Server`**: Restart the t-linter language server
- **`T-Linter: Show Template String Statistics`**: Display statistics about template strings in the current file

## Troubleshooting

### Language server not found
If you see "t-linter binary not found", install it using:
```bash
pip install t-linter
```

### No syntax highlighting
1. Ensure Python semantic highlighting is enabled
2. Check that your template strings use the `t"..."` syntax
3. Verify type annotations are correctly formatted

### Performance issues
- Disable `t-linter.enableTypeChecking` if you don't need cross-module type resolution
- Set `t-linter.trace.server` to "off" in production

## Known Issues

- Cross-module type resolution requires a Python type checker (Pyright/Pylsp) to be installed
- Limited to single-file analysis scope in the current version

## Contributing

Found a bug or have a feature request? Please open an issue on our [GitHub repository](https://github.com/koxudaxi/t-linter/issues).

## License

MIT - See [LICENSE](https://github.com/koxudaxi/t-linter/blob/main/LICENSE) for details.