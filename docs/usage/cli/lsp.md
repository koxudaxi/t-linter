# LSP Server

t-linter includes a built-in Language Server Protocol (LSP) server for editor integration.

## Starting the Server

```bash
t-linter lsp
```

The LSP server communicates over stdin/stdout using the standard LSP protocol.

To make document formatting run Ruff before t-linter, enable the composed LSP pipeline at server startup:

```bash
t-linter lsp --ruff-pipeline
```

By default t-linter resolves a Ruff server executable automatically. Override the executable or server arguments when your editor or agent needs a pinned binary:

```bash
t-linter lsp --ruff-pipeline --ruff-command /path/to/ruff --ruff-arg server
```

## Features

The LSP server provides:

- **Semantic Tokens** — syntax highlighting for embedded languages in template strings
- **Diagnostics** — real-time validation of embedded language syntax (debounced at 250ms)
- **Interpolation Type Checking** — optional JSON, YAML, and TOML interpolation value diagnostics through `ty`
- **Document Formatting** — full document formatting of template literals
- **Range Formatting** — format a single template literal by selecting its range
- **Code Actions** — save-time and manual rewrite actions for VSCode and other editors

### Feature Support by Language

| Language | Diagnostics | Formatting | Semantic Tokens |
|----------|:----------:|:----------:|:---------------:|
| HTML | ✅ | ✅ | ✅ |
| T-HTML | ✅ | ✅ | ✅ |
| JSON | ✅ | ✅ | ✅ |
| YAML | ✅ | ✅ | ✅ |
| TOML | ✅ | ✅ | ✅ |
| CSS | ✅ | — | ✅ |
| JavaScript | ✅ | — | ✅ |
| SQL | ✅ | — | ✅ |

For HTML, T-HTML, JSON, YAML, and TOML templates:

- Diagnostics are published from the dedicated Rust backends for strict validation
- Formatting requests and code actions rewrite the whole template literal using the backend formatter

### Interpolation Type Checking

Interpolation value type checking is opt-in and applies to JSON, YAML, and TOML templates. When enabled, t-linter starts a separate `ty server`, sends it an in-memory shadow copy of the open Python document, and maps `ty` assignment diagnostics back to the original interpolation expression.

For example, a `User` object interpolated into a structured-data value position is reported on the `{user}` expression, while backend-compatible values such as `int`, `str`, `list`, and `dict` are accepted.

Enable it from an LSP client with initialization options:

```json
{
  "typeChecking": {
    "enabled": true,
    "command": "/path/to/ty",
    "args": ["server"]
  }
}
```

`command` is optional. Without it, t-linter tries candidates in this order:

1. `$VIRTUAL_ENV/bin/ty` or `$CONDA_PREFIX/bin/ty`
2. workspace `.venv/bin/ty` or `venv/bin/ty`
3. uv project server: `uv run --project <workspace> --frozen --no-progress ty server`
4. PATH fallback: `ty server`

The `t-linter check` CLI does not run interpolation value type checking yet.

## Code Action Kinds

The server advertises `textDocument/codeAction` support with two t-linter-specific kinds:

- **`source.fixAll.t-linter`** — document-level formatting for all format-capable template literals in the file
- **`refactor.rewrite.t-linter`** — selection-based rewrite for exactly one template literal

`source.fixAll.t-linter` returns a direct `WorkspaceEdit` instead of a follow-up command so save-time execution stays deterministic.

`refactor.rewrite.t-linter` is returned only when the requested range maps to exactly one template literal. If the selection hits no templates, or spans multiple templates, the server returns no action.

The existing `textDocument/formatting` and `textDocument/rangeFormatting` endpoints remain available for backward compatibility.

### Ruff Save Pipeline

When the Ruff pipeline is enabled, t-linter handles `textDocument/formatting` and `source.fixAll.t-linter` transactionally:

1. Request `source.fixAll.ruff` edits from Ruff.
2. Request `source.organizeImports.ruff` edits from Ruff.
3. Request `textDocument/formatting` edits from Ruff.
4. Apply each Ruff step to an in-memory shadow copy and sync that shadow copy back to Ruff with full-text `didChange`.
5. Run t-linter template formatting on the shadow copy.
6. Return one final edit set from the original document to the composed result.

If Ruff returns no action for a step, that step is skipped. If Ruff returns an error, the formatting request fails. `textDocument/rangeFormatting` and `refactor.rewrite.t-linter` do not run the Ruff pipeline.

t-linter does not call `ruff check --fix` or `ruff format` for each save. It starts a Ruff LSP server (`ruff server`, or `uv run ... ruff server`) and communicates with it using LSP requests.

For CLI and CI workflows, run Ruff and t-linter as separate commands:

```bash
ruff check --fix . && ruff format . && t-linter format .
ruff check . && ruff format --check . && t-linter format --check .
```

### Ruff Executable Resolution

When `ruffPipeline.command` or `--ruff-command` is set, that command is used first and failures are reported directly. Without an explicit command, t-linter tries candidates in this order:

1. `$VIRTUAL_ENV/bin/ruff` or `$CONDA_PREFIX/bin/ruff`
2. workspace `.venv/bin/ruff` or `venv/bin/ruff`
3. uv project server: `uv run --project <workspace> --frozen --no-progress ruff server`
4. PATH fallback: `ruff server`

On Windows, t-linter also checks `Scripts/ruff.exe` and `ruff.exe` variants.

The uv candidate is added only when the workspace has `uv.lock` or a `pyproject.toml` containing `[tool.uv]` or `[dependency-groups]`. t-linter does not use `uv run --with ruff` automatically, because that would implicitly choose a Ruff version outside the project lock.

### Initialization Options

Editors that support LSP initialization options can enable the same behavior without CLI flags:

```json
{
  "ruffPipeline": {
    "enabled": true,
    "command": "/path/to/ruff",
    "args": ["server"],
    "settings": {
      "lineLength": 100
    }
  }
}
```

If both CLI flags and `initializationOptions.ruffPipeline` are provided, the initialization options take precedence for that LSP session. This lets editor extensions or coding agents choose the Ruff binary and settings explicitly while keeping `t-linter lsp --ruff-pipeline` useful for simpler clients.

### Line Length Resolution

For HTML and T-HTML formatting, line length is resolved in this order:

1. `textDocument/formatting` or `textDocument/rangeFormatting` custom option `printWidth`
2. custom option `lineLength`
3. `pyproject.toml` `tool.t-linter.line-length`
4. default `80`

Code actions do not carry formatting options, so they use steps 3 and 4 only.

## Editor Integration

### Claude Code

Add t-linter as an LSP server in your project's `.claude/settings.json`:

```json
{
  "lsp": {
    "t-linter": {
      "command": "t-linter",
      "args": ["lsp", "--ruff-pipeline"],
      "languages": ["python"]
    }
  }
}
```

Claude Code will then use t-linter's diagnostics when editing Python files containing template strings.

You can also use the CLI commands directly:

```bash
t-linter check src/
t-linter format --check src/
```

### Codex

Use t-linter's CLI commands directly within your Codex workflow:

```bash
# Validate template strings
t-linter check src/

# Check formatting without modifying files
t-linter format --check src/
```

To integrate into your development workflow, add t-linter checks to your project's lint configuration or CI pipeline.

### VSCode

The [VSCode extension](../vscode.md) uses this server automatically. No additional LSP configuration is needed.

Recommended Ruff coexistence settings:

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

VSCode supports only one default formatter per language, which is why t-linter exposes save-time template formatting through `source.fixAll.t-linter` instead of asking you to replace Ruff.

### Neovim

```lua
vim.lsp.start({
  name = "t-linter",
  cmd = { "t-linter", "lsp", "--ruff-pipeline" },
  filetypes = { "python" },
})
```

### Other Editors

Any editor with LSP support can use t-linter. Configure the LSP client to start `t-linter lsp` as the server command for Python files.
