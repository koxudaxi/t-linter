#!/usr/bin/env python3
"""Synchronize release versions across project metadata files."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PYPROJECT_PATH = ROOT / "pyproject.toml"
CARGO_TOML_PATH = ROOT / "Cargo.toml"
VSCODE_PACKAGE_PATH = ROOT / "editors" / "vscode" / "package.json"
VSCODE_LOCK_PATH = ROOT / "editors" / "vscode" / "package-lock.json"


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Update release version fields for Python and VSCode artifacts.",
    )
    parser.add_argument(
        "--python-version",
        help="Version to write into pyproject.toml and Cargo.toml.",
    )
    parser.add_argument(
        "--vscode-version",
        help="Version to write into the VSCode extension metadata files.",
    )
    return parser


def replace_pattern(path: Path, pattern: str, replacement: str) -> None:
    content = path.read_text(encoding="utf-8")
    updated, count = re.subn(pattern, replacement, content, count=1, flags=re.MULTILINE)
    if count != 1:
        raise ValueError(f"Could not update version in {path}")
    path.write_text(updated, encoding="utf-8")


def set_python_version(version: str) -> None:
    replace_pattern(
        PYPROJECT_PATH,
        r'^(version\s*=\s*")[^"]+(")$',
        rf'\g<1>{version}\2',
    )
    replace_pattern(
        CARGO_TOML_PATH,
        r'^version\s*=\s*"[^"]+"$',
        f'version = "{version}"',
    )


def set_json_version(path: Path, version: str) -> None:
    data = json.loads(path.read_text(encoding="utf-8"))
    data["version"] = version
    packages = data.get("packages")
    if isinstance(packages, dict) and "" in packages and isinstance(packages[""], dict):
        packages[""]["version"] = version
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


def set_vscode_version(version: str) -> None:
    set_json_version(VSCODE_PACKAGE_PATH, version)
    set_json_version(VSCODE_LOCK_PATH, version)


def main() -> int:
    args = build_parser().parse_args()
    if not args.python_version and not args.vscode_version:
        raise SystemExit("At least one of --python-version or --vscode-version is required.")

    if args.python_version:
        set_python_version(args.python_version)
    if args.vscode_version:
        set_vscode_version(args.vscode_version)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
