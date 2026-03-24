# VSCode Extension

t-linter now supports two save-time formatting modes in VSCode:

- **Ruff coexistence mode** keeps `Ruff` as the Python formatter and runs t-linter as a save-time code action for template literals.
- **t-linter formatter mode** keeps the existing formatter-only workflow for users who want t-linter to own formatting directly.

This split exists because VSCode allows only one `editor.defaultFormatter` per language. t-linter therefore adds a dedicated code action lane for template-string rewrites instead of replacing Ruff.

## Recommended Setup

### Ruff Coexistence Mode

Use this when you want Ruff to format Python code and t-linter to format the contents of template literals.

```json
{
  "[python]": {
    "editor.defaultFormatter": "charliermarsh.ruff",
    "editor.formatOnSave": true,
    "editor.codeActionsOnSave": [
      "source.fixAll.t-linter"
    ]
  }
}
```

On save, VSCode asks t-linter for the `source.fixAll.t-linter` code action. The server responds with a direct `WorkspaceEdit`, so save-time behavior stays deterministic.

You can also run the manual range action from the Command Palette or lightbulb UI:

- `refactor.rewrite.t-linter` rewrites exactly one selected template literal
- the action is hidden when the selection covers zero templates or spans multiple templates

### t-linter Formatter Mode

Use this when you want t-linter to stay the active formatter for Python files.

```json
{
  "[python]": {
    "editor.defaultFormatter": "koxudaxi.t-linter",
    "editor.formatOnSave": true
  }
}
```

This mode continues to use `textDocument/formatting` and `textDocument/rangeFormatting` for backward compatibility.

## Bundled Binary Matrix

The extension bundles `t-linter` on these platforms:

| Platform | Bundled binary | `t-linter.serverPath` required |
|---|:---:|:---:|
| Linux x64 | ✅ | No |
| macOS x64 | ✅ | No |
| macOS arm64 | ✅ | No |
| Windows x64 | ✅ | No |
| Other platforms | — | Yes |

If you are on an unsupported platform, or want to override the bundled binary, install `t-linter` separately and set `t-linter.serverPath`.

## Configure `serverPath` When Needed

1. Install t-linter:

   ```bash
   pip install t-linter
   ```

2. Find the executable:

   === "macOS/Linux"

       ```bash
       which t-linter
       ```

   === "Windows"

       ```bash
       where t-linter
       ```

3. Set `t-linter.serverPath` in VSCode settings.

Common locations:

| OS | Path |
|---|---|
| Windows | `C:\Users\YourName\AppData\Local\Programs\Python\Python3xx\Scripts\t-linter.exe` |
| macOS | `/Users/yourname/.local/bin/t-linter` or `/usr/local/bin/t-linter` |
| Linux | `/home/yourname/.local/bin/t-linter` or `/usr/local/bin/t-linter` |

## Migration From Formatter-Only Setup

If you previously followed the formatter-only guide:

1. Leave `Ruff` as `editor.defaultFormatter`.
2. Keep `editor.formatOnSave = true`.
3. Add `editor.codeActionsOnSave = ["source.fixAll.t-linter"]`.
4. Remove `koxudaxi.t-linter` from `editor.defaultFormatter` only if you want Ruff coexistence mode.

If you prefer the old workflow, you can keep t-linter formatter mode unchanged.

## Python Language Server Note

If semantic highlighting from another Python extension conflicts with t-linter, you can still disable the Python language server in workspace settings:

```json
{
  "python.languageServer": "None"
}
```

If you need Python completions and navigation features, keep the Python language server enabled and use t-linter primarily for template diagnostics, highlighting, and formatting.

## Troubleshooting

If save-time formatting does not run:

1. Confirm the correct mode is configured in `settings.json`.
2. Verify the extension found a bundled binary, or set `t-linter.serverPath`.
3. Restart VSCode after changing formatter or code action settings.
4. Reinstall the extension if the bundled binary is missing on a supported platform.
