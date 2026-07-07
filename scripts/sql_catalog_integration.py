from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
import tempfile

import psycopg


def run(cmd: list[str], cwd: pathlib.Path, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(cmd, cwd=cwd, env=env, text=True, capture_output=True)
    print(f"$ {' '.join(cmd)}")
    print(result.stdout, end="")
    print(result.stderr, end="")
    return result


def write_project(root: pathlib.Path) -> pathlib.Path:
    (root / "pyproject.toml").write_text(
        """
[tool.t-linter.sql]
library = "psycopg"
database-url = "env:T_LINTER_TEST_DATABASE_URL"
search-path = "public"
""".strip()
        + "\n"
    )
    app = root / "app.py"
    app.write_text(
        """
from typing import Annotated
from string.templatelib import Template

query: Annotated[Template, "sql"] = t"SELECT name FROM users WHERE id = {user_id}"
""".lstrip()
    )
    return app


def reset_schema(database_url: str, id_type: str) -> None:
    with psycopg.connect(database_url, autocommit=True) as conn:
        conn.execute("DROP TABLE IF EXISTS users")
        conn.execute(f"CREATE TABLE users (id {id_type} PRIMARY KEY, name text NOT NULL)")


def first_cache(root: pathlib.Path) -> dict[str, object]:
    cache_files = sorted((root / ".t-linter" / "sql-cache").glob("query-*.json"))
    assert len(cache_files) == 1, cache_files
    return json.loads(cache_files[0].read_text())


def main() -> None:
    database_url = os.environ["T_LINTER_TEST_DATABASE_URL"]
    binary = os.environ["T_LINTER_BIN"]
    reset_schema(database_url, "integer")

    with tempfile.TemporaryDirectory(prefix="t-linter-sql-catalog-") as tmp:
        root = pathlib.Path(tmp)
        app = write_project(root)
        env = os.environ.copy()
        env["T_LINTER_SQL_PYTHON"] = sys.executable

        prepared = run([binary, "sql", "prepare", str(app)], root, env)
        assert prepared.returncode == 0, prepared.returncode
        cache = first_cache(root)
        assert cache["params"][0]["type_name"] == "int4", cache
        assert cache["schema_fingerprint"].startswith("sha256:"), cache

        checked = run([binary, "sql", "prepare", "--check", str(app)], root, env)
        assert checked.returncode == 0, checked.returncode

        offline_env = env.copy()
        offline_env["T_LINTER_TEST_DATABASE_URL"] = "postgresql://postgres:postgres@127.0.0.1:1/tlinter"
        offline = run([binary, "sql", "prepare", "--check", str(app)], root, offline_env)
        assert offline.returncode == 0, offline.returncode

        reset_schema(database_url, "bigint")
        stale = run([binary, "sql", "prepare", "--check", str(app)], root, env)
        assert stale.returncode == 2, stale.returncode


if __name__ == "__main__":
    main()
