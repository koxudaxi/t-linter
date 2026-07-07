# Supported Languages

t-linter supports syntax highlighting and validation for the following embedded languages in Python template strings.

## Language Detection

Languages are detected through direct annotations, function parameter annotations, type aliases, and supported callee inference such as `tdom.html(...)`.

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

| Language | Annotation | Check | Format | Highlight | Engine |
|----------|-----------|:-----:|:------:|:---------:|--------|
| **HTML** | `"html"` | ✅ | ✅ | ✅ | `tstring-html` backend |
| **T-HTML** | `"thtml"` | ✅ | ✅ | ✅ | `tstring-thtml` backend |
| **TDOM** | `"tdom"` | ✅ | ✅ | ✅ | `tstring-tdom` backend |
| **JSON** | `"json"` | ✅ | ✅ | ✅ | `tstring-json` backend |
| **YAML** | `"yaml"`, `"yml"` | ✅ | ✅ | ✅ | `tstring-yaml` backend |
| **TOML** | `"toml"` | ✅ | ✅ | ✅ | `tstring-toml` backend |
| **CSS** | `"css"` | ✅ | — | ✅ | Tree-sitter |
| **JavaScript** | `"javascript"`, `"js"` | ✅ | — | ✅ | Tree-sitter |
| **SQL** | `"sql"` | ✅ | — | ✅ | Tree-sitter |

- **Check** — syntax validation via `t-linter check` CLI and LSP diagnostics
- **Format** — canonical formatting via `t-linter format` CLI and LSP formatting
- **Highlight** — semantic tokens via LSP for editor syntax highlighting

## Backend-powered vs Tree-sitter Languages

For backend-powered languages (HTML, T-HTML, TDOM, JSON, YAML, TOML), t-linter splits responsibilities:

- **Highlighting**: Tree-sitter only, for low-latency semantic tokens
- **Validation**: Strict parsing through the dedicated Rust backends (`tstring-*` crates)
- **Formatting**: Canonical formatting through the same Rust backends

For Tree-sitter-only languages (CSS, JavaScript, SQL), t-linter uses Tree-sitter for both highlighting and validation. Formatting is not yet available for these languages.

## JSON Notes

JSON templates can be bound to Python schema models for additional
`t-linter check` diagnostics:

```python
from typing import Annotated, TypedDict
from string.templatelib import Template
from json_tstring import Json

class Order(TypedDict):
    id: int
    name: str

payload: Annotated[Template, "json", Json(schema=Order)] = t'{{"id": 1}}'
```

`Json(schema=Order)` binds the schema, while `"json"` keeps normal JSON syntax
validation, formatting, and highlighting enabled. `Annotated[Template,
Json(schema=Order)]` and `payload: Json[Order] = t"..."` are also supported for
schema-binding checks, but they do not replace `"json"` language metadata.
t-linter checks required keys, unknown static keys, and static scalar value
shapes against local or imported `TypedDict` and dataclass models. See
[Check Command](usage/cli/check.md#json-schema-binding) for details.

## SQL Notes

SQL templates always receive Tree-sitter syntax validation when they are annotated as `"sql"`.
psycopg-specific t-string checks are opt-in because the rules describe psycopg semantics, not SQL in general.

Enable them with:

```toml
[tool.t-linter.sql]
library = "psycopg"
```

When enabled, t-linter reports psycopg t-string errors for unsupported conversions, unknown format specs, composable/spec mismatches, direct `dict` parameters that need `Json` or `Jsonb`, tuple parameters, `IN ({ids})` list-parameter patterns, and multiple SQL statements in a single template.

For catalog-backed parameter narrowing, configure a PostgreSQL connection and
prepare the offline cache:

```toml
[tool.t-linter.sql]
library = "psycopg"
database-url = "env:DATABASE_URL"
search-path = "public"
```

```bash
t-linter sql prepare .
t-linter sql prepare --check .
```

The cache is written to `.t-linter/sql-cache/` and should be committed with the
queries that use it. `t-linter sql prepare --check` reports stale or missing
cache entries when PostgreSQL is reachable. If the configured database URL
resolves but the database is unavailable and a committed cache exists, it trusts
that cache and exits successfully. When the cache is present, LSP interpolation
type checking can narrow plain psycopg parameters from PostgreSQL types even if
the editor session has no live database.

See [SQL Catalog Cache](usage/sql-catalog-cache.md) for the end-to-end workflow,
CI behavior, and an example diagnostic.

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

from tdom import html

page = html(t"""
<{Card} title={title}>
    <span>{status}</span>
</{Card}>
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

Use `{{` and `}}` when the embedded language needs literal braces, such as CSS or JSON objects.
