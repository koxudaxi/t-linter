#!/usr/bin/env python3
"""Validate required headings in a GitHub release body."""

from __future__ import annotations

import argparse
from pathlib import Path

REQUIRED_HEADINGS = [
    "## t-linter",
    "## VSCode Extension",
    "## Merged Changes",
]


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Validate required headings in a release body.",
    )
    parser.add_argument(
        "release_notes_file",
        help="Path to the markdown file containing the release body.",
    )
    return parser


def main() -> int:
    args = build_parser().parse_args()
    content = Path(args.release_notes_file).read_text(encoding="utf-8")
    missing = [heading for heading in REQUIRED_HEADINGS if heading not in content]
    if missing:
        for heading in missing:
            print(f"Missing required release heading: {heading}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
