# VSCode Extension

After installing the VSCode extension, t-linter will automatically provide syntax highlighting for Python template strings.

The extension bundles `t-linter` binaries for Linux x64, macOS x64/arm64, and Windows x64. On those platforms, no extra CLI installation is required.

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

If you want to override the bundled binary, or if you are on an unsupported platform, configure the server path in VSCode settings:

1. **Install and find your t-linter path**:

    ```bash
    pip install t-linter
    ```

2. Find your `t-linter` path by running in terminal:

    === "macOS/Linux"

        ```bash
        which t-linter
        ```

    === "Windows"

        ```bash
        where t-linter
        ```

3. Open VSCode Settings (Ctrl+, / Cmd+,)
4. Search for "t-linter.serverPath"
5. Set the full path to your t-linter executable

Common paths:

| OS | Path |
|---|---|
| **Windows** | `C:\Users\YourName\AppData\Local\Programs\Python\Python3xx\Scripts\t-linter.exe` |
| **macOS** | `/Users/yourname/.local/bin/t-linter` or `/usr/local/bin/t-linter` |
| **Linux** | `/home/yourname/.local/bin/t-linter` or `/usr/local/bin/t-linter` |

## Troubleshooting

If syntax highlighting doesn't work:

1. **Reinstall the extension** to restore the bundled binary
2. **Check that Python language server is disabled**: `python.languageServer` should be set to `"None"`
3. **Check the server path**: If you use an external binary, verify `t-linter.serverPath` in VSCode settings
4. **Restart VSCode** after making changes
