# Interpolation Type Checking

Interpolation type checking is an opt-in LSP feature that checks values inserted
into typed JSON, YAML, and TOML template strings. It uses each template backend
to decide what Python type an interpolation position accepts, asks `ty` to check
that expression, and reports the result back on the original `{expression}`.

The feature is intentionally separate from `t-linter check`. The CLI still runs
the built-in template syntax checks only; interpolation value diagnostics require
an LSP session and a running `ty server`.

## Architecture

The type checker has five parts:

| Layer | Responsibility |
|---|---|
| Parser and language resolver | Finds PEP 750 template strings and resolves their embedded language from annotations, aliases, function parameters, and supported callees. |
| Template backend | Computes contextual interpolation type requirements for a language. JSON, YAML, and TOML currently return requirements. |
| Shadow document synthesizer | Creates an in-memory Python document that preserves the original line count and adds type-check assignments for `ty`. |
| `ty` client | Starts and reuses a dedicated `ty server`, syncs the shadow document, and pulls diagnostics. |
| Diagnostic remapper | Filters `ty` assignment errors and maps them back to the original interpolation ranges. |

The data flow is:

```text
open Python document
  -> parse template strings and resolve template languages
  -> ask the JSON/YAML/TOML backend for interpolation requirements
  -> synthesize same-line annotated assignments in a shadow Python document
  -> sync that shadow document to a dedicated ty server
  -> pull ty diagnostics
  -> keep invalid-assignment diagnostics that intersect generated RHS ranges
  -> publish t-linter diagnostics on the original interpolation expressions
```

The `ty` process never sees the user's editor buffer directly. It sees a shadow
copy owned by t-linter's LSP server. The shadow copy uses the same URI and a
separate `ty server` process, so it does not affect other Python language
servers attached to the editor.

## Shadow Document Strategy

The implementation appends annotated assignments at the end of the same Python
simple statement that contains the template. This is the core trick that lets
`ty` check arbitrary interpolation expressions without changing the source file.

Original source:

```python
from typing import Annotated
from string.templatelib import Template

class User:
    name: str

def send(template: Annotated[Template, "json"]) -> None:
    ...

def handler(user: User, age: int) -> None:
    send(t'{{"name": {user}, "label": "{age}", "age": {age}}}')
```

Shadow source sent to `ty`:

```python
from typing import Annotated
from string.templatelib import Template

class User:
    name: str

def send(template: Annotated[Template, "json"]) -> None:
    ...

def handler(user: User, age: int) -> None:
    send(t'{{"name": {user}, "label": "{age}", "age": {age}}}'); __tl_0_0: "str | int | float | bool | None | dict[str, object] | list[object]" = user; __tl_0_1: "str" = age; __tl_0_2: "str | int | float | bool | None | dict[str, object] | list[object]" = age
```

If `ty` reports that assigning `user` to the generated JSON value type is
invalid, t-linter publishes one warning on `{user}` in the original template.
The generated names and assignments are never written to disk.

When a backend requirement mentions `datetime.date`, `datetime.time`, or
`datetime.datetime`, the shadow synthesizer inserts
`; import datetime as __tl_datetime` before the generated assignments and
rewrites those annotations through the collision-free alias. This keeps the
shadow document parseable without adding imports that would shift line numbers.

Earlier prototypes used generated helper calls. That approach was too
context-sensitive: class bodies, nested statements, indentation, and line
mapping all required special cases. The current same-line assignment strategy
keeps the shadow document parseable in class bodies and nested functions, avoids
line shifts, and gives each checked expression a stable RHS byte range that can
be matched against `ty` diagnostics.

## Backend Requirements

Backends own the logic for their DSL. t-linter does not hard-code JSON, YAML, or
TOML grammar rules in the LSP layer.

Each backend returns an `InterpolationTypeRequirement` containing:

- the interpolation index in the template
- a Python type expression that `ty` can evaluate
- a human-readable description such as `json value`, `yaml mapping key`, or
  `toml string fragment`

Examples of backend decisions:

| Template context | Expected Python annotation |
|---|---|
| JSON object key | `str` |
| JSON string fragment | `str` |
| JSON value | `str | int | float | bool | None | dict[str, object] | list[object]` |
| YAML mapping key or value | `str | int | float | bool | None | datetime.date | datetime.time | datetime.datetime | list[object] | dict[object, object]` |
| YAML scalar or metadata fragment | `str` |
| TOML key | `str` |
| TOML string fragment | `str` |
| TOML value | `str | int | float | bool | datetime.date | datetime.time | datetime.datetime | list[object] | dict[str, object]` |

HTML, T-HTML, and TDOM currently return no interpolation type requirements.
Tree-sitter-only languages such as CSS, JavaScript, and SQL are also not part of
this mechanism.

## Runtime Behavior

Interpolation type checking is disabled by default. Enable it through LSP
initialization options:

```json
{
  "typeChecking": {
    "enabled": true,
    "command": "/path/to/ty",
    "args": ["server"]
  }
}
```

For VS Code, use:

```json
{
  "t-linter.typeChecking.enabled": true,
  "t-linter.typeChecking.tyPath": "/path/to/ty"
}
```

When `command` or `tyPath` is omitted, t-linter tries `ty` from the active
virtual environment or conda environment, workspace `.venv` or `venv`, `uv run`
for uv projects, and finally `ty` on `PATH`.

The LSP diagnostic loop first publishes normal template diagnostics. If
interpolation type checking is enabled and the document contains supported
requirements, it then starts or reuses `ty`, syncs the shadow document, pulls
diagnostics, and publishes a merged diagnostic set for the same document
version. If the document changed while `ty` was running, stale type diagnostics
are discarded.

Published type diagnostics use:

| Field | Value |
|---|---|
| `source` | `t-linter (ty)` |
| `code` | `interpolation-type-error` |
| `severity` | warning |
| range | the original interpolation expression, not the generated assignment |

## Skipped Cases

t-linter skips an interpolation when checking it would require changing Python
semantics or producing unstable source mapping:

- the template language is unsupported or untyped
- the backend reports no requirement for that interpolation
- the interpolation uses a conversion such as `{value!r}`
- the interpolation uses a format spec such as `{value:.2f}`
- the interpolation expression spans multiple lines
- the interpolation contains a named expression such as `{(value := make())}`
- the template is outside a supported Python simple statement

Skipping one interpolation does not disable the whole document. Other safe
interpolations in the same file are still checked.

## `ty` Process Lifecycle

t-linter starts `ty server` lazily on the first document that needs type
checking. The server is reused across diagnostics in the same LSP session.

Startup is guarded so concurrent diagnostics do not launch multiple `ty`
processes unnecessarily. The external process is started outside the shared
state lock, so a slow `ty` startup does not block close or shutdown paths. Before
reusing a cached client, t-linter checks whether the child process is still
running; if it exited, the cache is cleared and startup is retried.

After repeated startup failures, t-linter disables interpolation type checking
for the session and logs a warning through LSP. Request and response handling use
JSON-RPC over stdin/stdout with a 10 second request timeout.

t-linter advertises both UTF-8 and UTF-16 position encodings to `ty`, records
the negotiated encoding from `initialize`, and uses that encoding when converting
`ty` diagnostic ranges back to byte ranges in the shadow document.

## Implementation Map

The relevant implementation files are:

| File | Role |
|---|---|
| `crates/t-linter-core/src/backend.rs` | Normalizes language names and delegates syntax, formatting, and interpolation requirement calls to `tstring-*` backends. |
| `crates/t-linter-core/src/shadow.rs` | Builds `ShadowDocument` and `ShadowCheckSite` values, inserts same-line assignments, skips unsafe expressions, and preserves line counts. |
| `crates/t-linter-lsp/src/type_checker.rs` | Manages `TypeCheckerConfig`, `TypeCheckerClient`, `ty` discovery, startup, JSON-RPC, document sync, diagnostic pulls, and shutdown. |
| `crates/t-linter-lsp/src/lib.rs` | Runs the LSP diagnostic pipeline, calls shadow synthesis, filters `invalid-assignment`, remaps diagnostics, and merges the final publish payload. |
| `crates/t-linter/tests/type_check_lsp.rs` | End-to-end coverage against a real `ty` binary for JSON, YAML, and TOML remapping. |

The backend crates expose their requirements through
`interpolation_type_requirements`. Adding another DSL to this mechanism should
start in that DSL backend, not in the LSP remapper. The LSP layer should remain
language-agnostic: it consumes requirement records, synthesizes Python
assignments, and maps `ty` diagnostics back to source locations.
