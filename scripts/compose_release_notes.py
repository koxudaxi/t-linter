#!/usr/bin/env python3
"""Compose structured release notes with required headings."""

from __future__ import annotations

import argparse
from pathlib import Path

REQUIRED_HEADINGS = [
    "t-linter",
    "VSCode Extension",
]


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Compose a structured GitHub release body.",
    )
    parser.add_argument(
        "--generated-notes-file",
        required=True,
        help="Path to the GitHub-generated notes markdown file.",
    )
    parser.add_argument(
        "--breaking-pr-number",
        type=int,
        default=None,
        help="Merged PR number to mention in the breaking changes section.",
    )
    return parser


def render_release_notes(generated_notes: str, breaking_pr_number: int | None) -> str:
    lines = [
        "<!--",
        "Fill in the package sections before publishing.",
        "Keep the required headings unchanged so validation and changelog sync can parse the release body.",
        "-->",
        "",
        "## Highlights",
        "",
        "- Summary of the release.",
        "",
    ]

    if breaking_pr_number is not None:
        lines.extend(
            [
                "## Breaking Changes",
                "",
                f"- Review PR #{breaking_pr_number} before publishing. It is labeled `breaking-change`.",
                "",
            ]
        )

    for heading in REQUIRED_HEADINGS:
        lines.extend(
            [
                f"## {heading}",
                "",
                "- None.",
                "",
            ]
        )

    lines.extend(
        [
            "## Merged Changes",
            "",
            generated_notes.rstrip(),
            "",
        ]
    )
    return "\n".join(lines)


def main() -> int:
    args = build_parser().parse_args()
    generated_notes = Path(args.generated_notes_file).read_text(encoding="utf-8")
    print(render_release_notes(generated_notes, args.breaking_pr_number))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
