# LSP Server

t-linter includes a built-in Language Server Protocol (LSP) server for editor integration.

## Starting the Server

```bash
t-linter lsp
```

The LSP server communicates over stdin/stdout using the standard LSP protocol.

## Features

The LSP server provides:

- **Semantic Tokens** — syntax highlighting for embedded languages in template strings
- **Diagnostics** — real-time validation of embedded language syntax (debounced at 250ms)
- **Document Formatting** — full document formatting of template literals
- **Range Formatting** — format a single template literal by selecting its range

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
- Formatting requests rewrite the whole template literal using the backend formatter

### Line Length Resolution

For HTML and T-HTML formatting, line length is resolved in this order:

1. `textDocument/formatting` or `textDocument/rangeFormatting` custom option `printWidth`
2. custom option `lineLength`
3. `pyproject.toml` `tool.t-linter.line-length`
4. default `80`

## Editor Integration

### Claude Code

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

### Neovim

```lua
vim.lsp.start({
  name = "t-linter",
  cmd = { "t-linter", "lsp" },
  filetypes = { "python" },
})
```

### Other Editors

Any editor with LSP support can use t-linter. Configure the LSP client to start `t-linter lsp` as the server command for Python files.
