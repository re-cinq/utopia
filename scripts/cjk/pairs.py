"""Prints the changed comment lines of files as numbered old/new pairs, for a reviewer to check.

usage: pairs.py [--base REV] PATH...
"""

import argparse
import subprocess
from pathlib import Path

from comments import comment_text, is_rust_path, split_lines


def changed_pairs(path: str, base_revision: str) -> list[tuple[int, str, str]]:
    shown = subprocess.run(
        ["git", "show", f"{base_revision}:{path}"], capture_output=True, text=True, check=True
    )
    is_rust = is_rust_path(path)
    before = [comment_text(segments).strip() for segments in split_lines(shown.stdout.split("\n"), is_rust)]
    after = [
        comment_text(segments).strip()
        for segments in split_lines(Path(path).read_text().split("\n"), is_rust)
    ]
    return [
        (number, old, new)
        for number, (old, new) in enumerate(zip(before, after), start=1)
        if old != new
    ]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="HEAD")
    parser.add_argument("paths", nargs="+")
    arguments = parser.parse_args()
    for path in arguments.paths:
        print(f"=== {path}")
        for number, old, new in changed_pairs(path, arguments.base):
            print(f"{number}\n  O {old}\n  N {new}")


if __name__ == "__main__":
    main()
