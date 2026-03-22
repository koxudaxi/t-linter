# Supported Languages

t-linter supports syntax highlighting and validation for the following embedded languages in Python template strings.

## Language Detection

Languages are detected through type annotations:

```python
from typing import Annotated
from string.templatelib import Template

# Direct annotation
template: Annotated[Template, "html"] = t"<p>{content}</p>"

# Type alias
type html = Annotated[Template, "html"]
page: html = t"<div>{content}</div>"
```

## Supported Languages

| Language | Annotation | Highlighting | Validation |
|----------|-----------|:------------:|:----------:|
| **HTML** | `"html"` | Tree-sitter | `tstring-html` backend |
| **T-HTML** | `"thtml"` | Tree-sitter (HTML-like) | `tstring-thtml` backend |
| **SQL** | `"sql"` | Tree-sitter | Tree-sitter |
| **JavaScript** | `"javascript"` | Tree-sitter | Tree-sitter |
| **CSS** | `"css"` | Tree-sitter | Tree-sitter |
| **JSON** | `"json"` | Tree-sitter | `tstring-json` backend |
| **YAML** | `"yaml"` | Tree-sitter | `tstring-yaml` backend |
| **TOML** | `"toml"` | Tree-sitter | `tstring-toml` backend |

## Backend-powered Validation and Formatting

For backend-powered languages, t-linter splits responsibilities:

- **`semanticTokens`**: Tree-sitter only, for low-latency highlighting
- **`check`**: Strict parsing through the dedicated Rust backends
- **`formatting`**: Canonical formatting through the same Rust backends

This currently applies to:

- `html` via `tstring-html`
- `thtml` via `tstring-thtml`
- `json` via `tstring-json`
- `yaml` via `tstring-yaml`
- `toml` via `tstring-toml`

## Examples

```python
from typing import Annotated
from string.templatelib import Template

# HTML
page: Annotated[Template, "html"] = t"""
<html>
    <body><h1>{title}</h1></body>
</html>
"""

# T-HTML
card: Annotated[Template, "thtml"] = t"""
<Card title="{title}">
    <Badge tone="success">{status}</Badge>
</Card>
"""

# SQL
query: Annotated[Template, "sql"] = t"""
SELECT * FROM users WHERE id = {user_id}
"""

# JavaScript
script: Annotated[Template, "javascript"] = t"""
console.log({message});
"""

# CSS
styles: Annotated[Template, "css"] = t"""
.container { max-width: {width}px; }
"""

# JSON
data: Annotated[Template, "json"] = t"""
{"name": {name}, "age": {age}}
"""

# YAML
config: Annotated[Template, "yaml"] = t"""
app:
  name: {app_name}
  debug: true
"""

# TOML
settings: Annotated[Template, "toml"] = t"""
[project]
name = "{project_name}"
version = "{version}"
"""
```
