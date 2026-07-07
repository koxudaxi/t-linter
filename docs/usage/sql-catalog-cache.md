# SQL Catalog Cache

The SQL catalog cache lets t-linter use PostgreSQL schema metadata when checking
psycopg SQL template parameters. It is useful when a template contains a plain
SQL parameter such as `{user_id}` and the database column tells t-linter that the
parameter should be an `int`, `str`, `datetime`, or another PostgreSQL-backed
Python type.

Catalog-backed checks are split into two phases:

1. `t-linter sql prepare` connects to PostgreSQL and writes cache files under
   `.t-linter/sql-cache/`.
2. LSP interpolation type checking reads those cache files and asks Ty, Pyright,
   or Pyrefly to check the Python expression against the cached parameter type.

The LSP does not need a live database when the cache is already present.

## Configure

Enable psycopg SQL handling and point t-linter at a PostgreSQL database:

```toml
[tool.t-linter.sql]
library = "psycopg"
database-url = "env:DATABASE_URL"
search-path = "public"
```

`database-url` accepts either a literal PostgreSQL URL or `env:NAME`, which
reads the connection string from an environment variable. `search-path` is
optional, but setting it keeps describe results aligned with how the application
resolves unqualified table names.

When `database-url` uses `env:NAME`, that environment variable must be set before
prepare runs. The offline cache fallback applies after the URL has been resolved
and the connection or describe request fails; it does not hide a missing
environment variable.

The CLI helper is a Python subprocess that imports `psycopg`. Run
`t-linter sql prepare` from an environment where `python3` resolves to a Python
with `psycopg` installed, or set `T_LINTER_SQL_PYTHON` explicitly:

```bash
T_LINTER_SQL_PYTHON=.venv/bin/python t-linter sql prepare .
```

## Prepare The Cache

Run prepare against the files that contain SQL templates:

```bash
DATABASE_URL=postgresql://postgres:postgres@localhost/app \
  t-linter sql prepare .
```

This command:

- finds psycopg SQL templates
- converts plain template interpolations into PostgreSQL placeholders such as
  `$1`, `$2`
- asks PostgreSQL for parameter and column types
- writes one cache file per normalized SQL query under `.t-linter/sql-cache/`

Commit the generated `.t-linter/sql-cache/` files with the code that uses those
queries. The committed cache is what editor diagnostics can use later when the
database is not running.

## Check In CI

Use `--check` to verify that committed cache files are still current:

```bash
DATABASE_URL=postgresql://postgres:postgres@localhost/app \
  t-linter sql prepare --check .
```

`--check` behavior depends on database availability:

| Situation | Behavior |
|---|---|
| PostgreSQL is reachable and the cache matches | exits `0` |
| PostgreSQL is reachable and the cache is stale or missing | exits `2` |
| Database URL resolves, PostgreSQL is unreachable, and a committed cache exists | trusts the existing cache and exits `0` |
| Database URL resolves, PostgreSQL is unreachable, and no cache exists | exits `2` |

That fallback is intentional. It lets lightweight CI or editor sessions reuse a
known-good cache, while a database-backed CI job can still catch schema drift.

## LSP Diagnostics

After the cache is prepared, enable interpolation type checking in the LSP:

```json
{
  "typeChecking": {
    "enabled": true,
    "checker": "ty"
  }
}
```

Given this table:

```sql
CREATE TABLE users (
  id integer PRIMARY KEY,
  name text NOT NULL
);
```

and this Python source:

```python
from typing import Annotated
from string.templatelib import Template

def find_user(user_id: str) -> None:
    query: Annotated[Template, "sql"] = (
        t"SELECT name FROM users WHERE id = {user_id}"
    )
```

`t-linter sql prepare` records that the first SQL parameter is PostgreSQL
`int4`. During LSP diagnostics, t-linter narrows `{user_id}` to Python `int` and
reports an `interpolation-type-error` because the expression is annotated as
`str`.

The diagnostic is reported on the original `{user_id}` expression. The generated
type-checking code is internal and is never written to the source file.

## What The Cache Covers

Catalog-backed narrowing applies to plain psycopg SQL parameters, including
`{value}`, `{value:s}`, `{value:b}`, and `{value:t}`. Structural psycopg
interpolations such as `{table:i}` and SQL fragments such as `{fragment:q}` are
checked by the normal psycopg rules instead of the catalog cache.

The cache is keyed by normalized SQL text. If the query text changes, prepare
writes a new cache entry. If the database schema changes in a way that changes
described parameter or column types, `t-linter sql prepare --check` reports the
old cache as stale when PostgreSQL is reachable.

## Timeouts

The describe helper has a default 10 second request timeout. Override it when a
slow database needs more time:

```bash
T_LINTER_SQL_DESCRIBE_TIMEOUT_SECONDS=30 t-linter sql prepare .
```
