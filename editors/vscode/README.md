# T-Linter for Visual Studio Code

Syntax highlighting for Python template strings (PEP 750) with embedded language support.

## Features

- 🎨 Syntax highlighting for template strings with embedded HTML, SQL, CSS, JavaScript, and JSON
- 🔍 Automatic language detection from type annotations
- 💡 IntelliSense support within template strings
- 🚀 Fast and lightweight
- 🔧 Configurable

## Requirements

- Python 3.14+ (or Python with PEP 750 support)
- `t-linter` language server installed

## Installation

1. Install the t-linter language server:
   ```bash
   cargo install t-linter
   ```
2. Install this extension from the VSCode marketplace
   Usage
   ```python
    from typing import Annotated
    from templatelib import Template
    
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

## Configuration

t-linter.enabled: Enable/disable the extension
t-linter.serverPath: Path to t-linter executable
t-linter.highlightUntyped: Highlight template strings without type annotations
t-linter.enableTypeChecking: Enable integration with Python type checkers

### Commands

T-Linter: Restart Server: Restart the language server
T-Linter: Show Template String Statistics: Display statistics about template strings in the current file

## Known Issues

Cross-module type resolution requires a Python type checker (Pyright/Pylsp) to be installed

## Release Notes
0.1.0
Initial release with basic syntax highlighting support.

### editors/vscode/.vscode/launch.json（development）

```json
{
    "version": "0.2.0",
    "configurations": [
        {
            "name": "Extension",
            "type": "extensionHost",
            "request": "launch",
            "args": [
                "--extensionDevelopmentPath=${workspaceFolder}"
            ],
            "outFiles": [
                "${workspaceFolder}/out/**/*.js"
            ],
            "preLaunchTask": "${defaultBuildTask}"
        }
    ]
}
```

### editors/vscode/.vscode/tasks.json
```json
{
    "version": "2.0.0",
    "tasks": [
        {
            "type": "npm",
            "script": "watch",
            "problemMatcher": "$tsc-watch",
            "isBackground": true,
            "presentation": {
                "reveal": "never"
            },
            "group": {
                "kind": "build",
                "isDefault": true
            }
        }
    ]
}
```
