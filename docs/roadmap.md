# Roadmap

## Completed

- **Language Server Protocol (LSP)** — Fully implemented
- **Syntax Highlighting** — Supports HTML, T-HTML, TDOM, SQL, JavaScript, CSS, JSON, YAML, TOML
- **Type Alias and Marker Support** — Recognizes `type html = Annotated[Template, "html"]` and metadata markers such as `Json(schema=...)`
- **Linting (`check` command)** — Validate template strings for syntax errors
- **Statistics (`stats` command)** — Analyze template string usage across codebases

## Planned

- **Cross-file Type Resolution** — Track type aliases across module boundaries
