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
- **Diagnostics** — real-time validation of embedded language syntax (debounced for performance)
- **Formatting** — canonical formatting of template literals while preserving interpolation expressions like `{name!r:>5}`

For HTML, T-HTML, JSON, YAML, and TOML templates:

- Diagnostics are published from the dedicated Rust backends for strict validation
- Formatting requests rewrite the whole template literal using the backend formatter

## Editor Integration

The LSP server can be used with any editor that supports LSP. The VSCode extension uses this server automatically.

For other editors, configure the LSP client to start `t-linter lsp` as the server command.
