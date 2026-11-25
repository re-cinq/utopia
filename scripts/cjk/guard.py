"""Proves a translation changed only comments: same lines, same code, no CJK left in comments.

usage: guard.py [--base REV] PATH...
"""

import argparse
import subprocess
import sys
from pathlib import Path

from comments import CJK, blanked_comments, code_projection, copied_translations, reindented, missing_anchors, comment_text, is_rust_path, split_lines


def read_at_revision(revision: str, path: str) -> list[str]:
    shown = subprocess.run(
        ["git", "show", f"{revision}:{path}"], capture_output=True, text=True, check=True
    )
    return shown.stdout.split("\n")


def violations_in(path: str, base_revision: str) -> list[str]:
    is_rust = is_rust_path(path)
    before = read_at_revision(base_revision, path)
    after = Path(path).read_text().split("\n")
    if len(before) != len(after):
        return [f"{path}: line count {len(before)} -> {len(after)}"]
    before_code = code_projection(before, is_rust)
    after_code = code_projection(after, is_rust)
    changed_code = [
        f"{path}:{number}: code changed: {before_code[number - 1]!r} -> {after_code[number - 1]!r}"
        for number in range(1, len(after) + 1)
        if before_code[number - 1] != after_code[number - 1]
    ]
    left_in_comments = [
        f"{path}:{number}: chinese left in comment: {comment_text(segments).strip()}"
        for number, segments in enumerate(split_lines(after, is_rust), start=1)
        if CJK.search(comment_text(segments))
    ]
    before_comments = [comment_text(segments) for segments in split_lines(before, is_rust)]
    after_comments = [comment_text(segments) for segments in split_lines(after, is_rust)]
    moved_anchors = [
        f"{path}:{number}: names or issue numbers lost: {lost}"
        for number in range(1, len(after) + 1)
        if before_comments[number - 1] != after_comments[number - 1]
        and (lost := missing_anchors(before_comments[number - 1], neighbourhood(after_comments, number)))
    ]
    copies = [
        f"{path}:{number}: same translation sits on a line with a different original"
        for number in copied_translations(before_comments, after_comments)
    ]
    blanks = [
        f"{path}:{number}: comment was emptied"
        for number in blanked_comments(before_comments, after_comments)
    ]
    moved_indentation = [
        f"{path}:{number}: leading whitespace changed" for number in reindented(before, after)
    ]
    return changed_code + left_in_comments + moved_anchors + copies + blanks + moved_indentation


def neighbourhood(comments: list[str], number: int) -> str:
    """The line and its two neighbours: a translation may reflow a name onto the next line."""
    return "\n".join(comments[max(number - 2, 0) : number + 1])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="HEAD")
    parser.add_argument("paths", nargs="+")
    arguments = parser.parse_args()
    violations = [
        violation
        for path in arguments.paths
        for violation in violations_in(path, arguments.base)
    ]
    print("\n".join(violations))
    return 1 if violations else 0


if __name__ == "__main__":
    sys.exit(main())
