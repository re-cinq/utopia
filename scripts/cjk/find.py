"""Lists lines that contain Chinese, split into comment lines and code lines.

usage: find.py [--code] [--since REV] [PATH...]
  default lists comment lines; --code lists lines where Chinese sits in code or strings
  --since REV keeps only lines added since REV (the post-sync pass)
"""

import argparse
import re
import subprocess
from pathlib import Path

from comments import CJK, code_text, comment_text, is_rust_path, is_supported_path, split_lines

HUNK_HEADER = re.compile(r"^@@ -\S+ \+(\d+)(?:,(\d+))? @@")
EXCLUDED_PREFIXES = ("web/src/i18n/zh.ts", "scripts/bench/")


def tracked_paths(roots: list[str]) -> list[str]:
    listed = subprocess.run(
        ["git", "ls-files", *roots], capture_output=True, text=True, check=True
    )
    return [
        path
        for path in listed.stdout.split("\n")
        if is_supported_path(path) and not path.startswith(EXCLUDED_PREFIXES)
    ]


def added_line_numbers(revision: str, path: str) -> set[int]:
    diff = subprocess.run(
        ["git", "diff", "-U0", revision, "--", path], capture_output=True, text=True, check=True
    )
    added: set[int] = set()
    for line in diff.stdout.split("\n"):
        header = HUNK_HEADER.match(line)
        if header:
            first, count = int(header.group(1)), int(header.group(2) or 1)
            added.update(range(first, first + count))
    return added


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--code", action="store_true")
    parser.add_argument("--since")
    parser.add_argument("paths", nargs="*")
    arguments = parser.parse_args()
    for path in tracked_paths(arguments.paths):
        lines = Path(path).read_text().split("\n")
        wanted = added_line_numbers(arguments.since, path) if arguments.since else None
        for number, segments in enumerate(split_lines(lines, is_rust_path(path)), start=1):
            text = code_text(segments) if arguments.code else comment_text(segments)
            if CJK.search(text) and (wanted is None or number in wanted):
                print(f"{path}:{number}: {lines[number - 1].strip()}")


if __name__ == "__main__":
    main()
