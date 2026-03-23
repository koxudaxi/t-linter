# Supported Languages

t-linter supports syntax highlighting and validation for the following embedded languages in Python template strings.

## Language Detection

Languages are detected through direct annotations, function parameter
annotations, and type aliases. The examples below use typed helper functions so
you can run them through `t-linter check` as-is:

```python
from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> None:
    pass

content = "Hello from t-linter"
render_html(t"<p>{content}</p>")

# Type alias reused by another helper
type html = Annotated[Template, "html"]


def render_page(template: html) -> None:
    pass


page_content = "Reused through a type alias"
render_page(t"<div>{page_content}</div>")
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

## HTML Notes

For `html`, `<title>{value}</title>` is allowed and rendered as escaped text.
`<script>`, `<style>`, and `<textarea>` still reject interpolations.

Use these patterns:

- use `<title>{value}</title>` for dynamic document titles
- keep `<script>`, `<style>`, and `<textarea>` static
- move dynamic values into attributes
- move dynamic values into normal element content such as `<p>{content}</p>`

## Examples

```python
from typing import Annotated
from string.templatelib import Template

def render_html(template: Annotated[Template, "html"]) -> None:
    pass


def render_thtml(template: Annotated[Template, "thtml"]) -> None:
    pass


def run_sql(template: Annotated[Template, "sql"]) -> None:
    pass


def run_javascript(template: Annotated[Template, "javascript"]) -> None:
    pass


type css = Annotated[Template, "css"]
type json_payload = Annotated[Template, "json"]
type yaml_config = Annotated[Template, "yaml"]
type toml_config = Annotated[Template, "toml"]


def load_css(template: css) -> None:
    pass


def load_json(template: json_payload) -> None:
    pass


def load_yaml(template: yaml_config) -> None:
    pass


def load_toml(template: toml_config) -> None:
    pass


def Card(*, title: str, children: str | None = None) -> object:
    return None


def Badge(*, tone: str = "neutral", children: str | None = None) -> object:
    return None


title = "Dashboard"
render_html(t"""
<html>
    <body><h1>{title}</h1></body>
</html>
""")

status = "ready"
render_thtml(t"""
<Card title="{title}">
    <Badge tone="success">{status}</Badge>
</Card>
""")

user_id = 42
run_sql(t"""
SELECT * FROM users WHERE id = {user_id}
""")

message = "'hello from t-linter'"
run_javascript(t"""
console.log({message});
""")

width = 1200
load_css(t"""
.container {{
    max-width: {width}px;
}}
""")

name = "Ada"
age = 37
load_json(t"""
{{
  "name": {name},
  "age": {age}
}}
""")

app_name = "demo-app"
load_yaml(t"""
app:
  name: {app_name}
  debug: true
""")

project_name = "demo-project"
version = "0.1.0"
load_toml(t"""
[project]
name = "{project_name}"
version = "{version}"
""")
```

Use `{{` and `}}` when the embedded language needs literal braces, such as CSS
or JSON objects.
