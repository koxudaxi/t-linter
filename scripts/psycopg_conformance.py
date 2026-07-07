#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import pathlib
import subprocess
import sys
import tempfile
from dataclasses import dataclass


ROOT = pathlib.Path(__file__).resolve().parents[1]
CASES_DIR = ROOT / "tests" / "psycopg_conformance" / "cases"
ERROR_RULES = {
    "sql-conversion-unsupported",
    "sql-format-spec-unknown",
    "sql-composable-spec-mismatch",
    "sql-dict-needs-json-wrapper",
}


@dataclass(frozen=True)
class Case:
    path: pathlib.Path
    expected: frozenset[str]
    source: str


def read_cases(cases_dir: pathlib.Path) -> list[Case]:
    cases: list[Case] = []
    for path in sorted(cases_dir.glob("*.py")):
        source = path.read_text()
        first_line = source.splitlines()[0] if source.splitlines() else ""
        if not first_line.startswith("# expect:"):
            raise SystemExit(f"{path}: first line must be '# expect: ...'")
        expected = frozenset(
            item.strip()
            for item in first_line.removeprefix("# expect:").split(",")
            if item.strip()
        )
        cases.append(Case(path=path, expected=expected, source=source))
    return cases


def run_rust(cases: list[Case], t_linter_bin: str | None, cargo: str) -> bool:
    passed = True
    with tempfile.TemporaryDirectory(prefix="t-linter-psycopg-") as tmp:
        tmp_path = pathlib.Path(tmp)
        (tmp_path / "pyproject.toml").write_text(
            '[tool.t-linter.sql]\nlibrary = "psycopg"\n'
        )
        for case in cases:
            case_path = tmp_path / case.path.name
            case_path.write_text(case.source)
            if t_linter_bin:
                command = [t_linter_bin, "check", case_path.name, "--format", "json"]
            else:
                command = [
                    cargo,
                    "run",
                    "--quiet",
                    "--bin",
                    "t-linter",
                    "--",
                    "check",
                    str(case_path),
                    "--format",
                    "json",
                ]
            completed = subprocess.run(
                command,
                cwd=tmp_path if t_linter_bin else ROOT,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            if completed.returncode != 0:
                print(f"rust FAIL {case.path.name}: exit {completed.returncode}")
                print(completed.stderr)
                passed = False
                continue
            try:
                report = json.loads(completed.stdout)
            except json.JSONDecodeError:
                print(f"rust FAIL {case.path.name}: invalid json")
                print(completed.stdout)
                passed = False
                continue
            actual = {
                diagnostic["rule"]
                for diagnostic in report.get("diagnostics", [])
                if diagnostic.get("rule", "").startswith("sql-")
            }
            if actual != case.expected:
                print(
                    f"rust FAIL {case.path.name}: expected {sorted(case.expected)}, got {sorted(actual)}"
                )
                passed = False
            else:
                print(f"rust ok   {case.path.name}: {sorted(actual)}")
    return passed


def run_python(cases: list[Case]) -> bool:
    if sys.version_info < (3, 14):
        print(
            f"python SKIP: Python 3.14+ is required for t-strings, got {sys.version.split()[0]}"
        )
        return True

    try:
        import psycopg
        from psycopg import sql
    except Exception as exc:
        print(f"python FAIL: failed to import psycopg>=3.3: {exc}")
        return False

    print(f"python psycopg version: {psycopg.__version__}")
    passed = True
    for case in cases:
        expected_errors = case.expected & ERROR_RULES
        namespace: dict[str, object] = {"__name__": "__psycopg_conformance_case__"}
        try:
            exec(compile(case.source, str(case.path), "exec"), namespace)
            sql.as_string(namespace["TEMPLATE"])
            actual_exception = None
        except TypeError:
            actual_exception = "TypeError"
        except psycopg.ProgrammingError:
            actual_exception = "ProgrammingError"
        except Exception as exc:
            actual_exception = type(exc).__name__

        should_raise = bool(expected_errors)
        did_raise = actual_exception is not None
        if should_raise != did_raise:
            print(
                f"python FAIL {case.path.name}: expected_raise={should_raise}, exception={actual_exception}"
            )
            passed = False
        else:
            print(f"python ok   {case.path.name}: exception={actual_exception}")
    return passed


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cases-dir", type=pathlib.Path, default=CASES_DIR)
    parser.add_argument("--runner", choices=["all", "rust", "python"], default="all")
    parser.add_argument("--t-linter-bin", default=os.environ.get("T_LINTER_BIN"))
    parser.add_argument("--cargo", default=os.environ.get("CARGO", "cargo"))
    args = parser.parse_args()

    cases = read_cases(args.cases_dir)
    passed = True
    if args.runner in {"all", "rust"}:
        passed = run_rust(cases, args.t_linter_bin, args.cargo) and passed
    if args.runner in {"all", "python"}:
        passed = run_python(cases) and passed
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
