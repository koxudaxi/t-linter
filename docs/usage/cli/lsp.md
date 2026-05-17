# LSP Server

t-linter includes a built-in Language Server Protocol (LSP) server for editor integration.

## Starting the Server

```bash
t-linter lsp
```

The LSP server communicates over stdin/stdout using the standard LSP protocol.

To make document formatting run Ruff before t-linter, enable the composed formatter at server startup:

```bash
t-linter lsp --ruff-format
```

By default this starts `ruff server`. Override the executable or server arguments when your editor or agent needs a pinned binary:

```bash
t-linter lsp --ruff-format --ruff-command /path/to/ruff --ruff-arg server
```

## Features

The LSP server provides:

- **Semantic Tokens** — syntax highlighting for embedded languages in template strings
- **Diagnostics** — real-time validation of embedded language syntax (debounced at 250ms)
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

## Code Action Kinds

The server advertises `textDocument/codeAction` support with two t-linter-specific kinds:

- **`source.fixAll.t-linter`** — document-level formatting for all format-capable template literals in the file
- **`refactor.rewrite.t-linter`** — selection-based rewrite for exactly one template literal

`source.fixAll.t-linter` returns a direct `WorkspaceEdit` instead of a follow-up command so save-time execution stays deterministic.

`refactor.rewrite.t-linter` is returned only when the requested range maps to exactly one template literal. If the selection hits no templates, or spans multiple templates, the server returns no action.

The existing `textDocument/formatting` and `textDocument/rangeFormatting` endpoints remain available for backward compatibility.

### Composed Ruff Formatting

When Ruff formatting is enabled, t-linter handles `textDocument/formatting` transactionally:

1. Request formatting edits from `ruff server`.
2. Apply those edits to an in-memory shadow copy of the document.
3. Run t-linter template formatting on the shadow copy.
4. Return one final edit set from the original document to the composed result.

Ruff is used only for the formatting pass. t-linter does not forward Ruff diagnostics, code actions, or workspace edits.

Editors that support LSP initialization options can enable the same behavior without CLI flags:

```json
{
  "ruffFormat": {
    "enabled": true,
    "command": "ruff",
    "args": ["server"],
    "settings": {
      "lineLength": 100
    }
  }
}
```

If both CLI flags and `initializationOptions.ruffFormat` are provided, the initialization options take precedence for that LSP session. This lets editor extensions or coding agents choose the Ruff binary and settings explicitly while keeping `t-linter lsp --ruff-format` useful for simpler clients.

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
      "args": ["lsp", "--ruff-format"],
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
  cmd = { "t-linter", "lsp", "--ruff-format" },
  filetypes = { "python" },
})
```

### Other Editors

Any editor with LSP support can use t-linter. Configure the LSP client to start `t-linter lsp` as the server command for Python files.
