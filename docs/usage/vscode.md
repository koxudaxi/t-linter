# VSCode Extension

After installing both the PyPI package and VSCode extension, t-linter will automatically provide syntax highlighting for Python template strings.

## Setup

### Disable Python Language Server

To prevent conflicts with t-linter's syntax highlighting, disable the Python language server:

1. Open VSCode Settings (Ctrl+, / Cmd+,)
2. Search for "python.languageServer"
3. Set it to "None"

Alternatively, add to your `settings.json`:

```json
{
    "python.languageServer": "None"
}
```

### Configure the Server Path (if needed)

If t-linter is not in your PATH, configure the server path in VSCode settings:

1. **Find your t-linter path** by running in terminal:

    === "macOS/Linux"

        ```bash
        which t-linter
        ```

    === "Windows"

        ```bash
        where t-linter
        ```

2. Open VSCode Settings (Ctrl+, / Cmd+,)
3. Search for "t-linter.serverPath"
4. Set the full path to your t-linter executable

Common paths:

| OS | Path |
|---|---|
| **Windows** | `C:\Users\YourName\AppData\Local\Programs\Python\Python3xx\Scripts\t-linter.exe` |
| **macOS** | `/Users/yourname/.local/bin/t-linter` or `/usr/local/bin/t-linter` |
| **Linux** | `/home/yourname/.local/bin/t-linter` or `/usr/local/bin/t-linter` |

## Troubleshooting

If syntax highlighting doesn't work:

1. **Ensure t-linter is installed**: Run `t-linter --version` in terminal
2. **Check that Python language server is disabled**: `python.languageServer` should be set to `"None"`
3. **Check the server path**: Verify `t-linter.serverPath` in VSCode settings
4. **Restart VSCode** after making changes
