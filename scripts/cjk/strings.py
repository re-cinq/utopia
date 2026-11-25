"""Finds the message string of a line (a log, error or assertion message), checks a translation of
it, and swaps it in. The translator only returns {id: english}; this script edits the files.

usage: strings.py extract PATH...            writes tasks as JSON to stdout
       strings.py check TASKS.json TRANSLATIONS.json   reports problems, edits nothing
       strings.py apply TASKS.json TRANSLATIONS.json
       strings.py verify BASE_REV PATH...           only string text may differ from BASE_REV
"""

import json
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from comments import CJK, code_text, is_rust_path, lines_starting_in_code, split_lines

STRING_LITERAL = re.compile(r'(?<![A-Za-z0-9_#])b?"((?:[^"\\]|\\.)*)"')
RAW_STRING_OR_QUOTE_CHAR = re.compile(r"""\br#*"|'"'""")
PLACEHOLDER = re.compile(r"\{\{|\}\}|\{[^{}]*\}")
UNESCAPED_QUOTE = re.compile(r'(?<!\\)"')
BACKSLASH_NOT_BEFORE_QUOTE = re.compile(r'\\(?!")')
OPENERS = (
    (re.compile(r"\b(?:trace|debug|info|warn|error)!\("), None),
    (re.compile(r"\b(?:anyhow|bail|panic|unreachable|todo|unimplemented)!\("), 0),
    (re.compile(r"\.(?:context|expect)\("), 0),
    (re.compile(r"\b(?:debug_)?assert!\("), 1),
    (re.compile(r"\b(?:debug_)?assert_(?:eq|ne)!\("), 2),
)


@dataclass(frozen=True)
class Target:
    start: int
    end: int
    text: str


def message_target(line: str) -> Optional[Target]:
    if RAW_STRING_OR_QUOTE_CHAR.search(line):
        return None
    literals = list(STRING_LITERAL.finditer(line))
    if not literals or not CJK.search(literals[-1].group(1)):
        return None
    last = literals[-1]
    for opener, commas_before_message in OPENERS:
        for opening in opener.finditer(line, 0, last.start()):
            region = line[opening.end() : last.start()]
            if _message_follows(region, commas_before_message):
                return Target(last.start(1), last.end(1), last.group(1))
    return None


def _message_follows(region: str, commas_before_message: Optional[int]) -> bool:
    without_strings = STRING_LITERAL.sub('""', region)
    depth = 0
    commas = 0
    for char in without_strings:
        if char in "([{":
            depth += 1
        elif char in ")]}":
            depth -= 1
            if depth < 0:
                return False
        elif char == "," and depth == 0:
            commas += 1
    if depth != 0:
        return False
    ends_at_argument = not without_strings.strip() or without_strings.rstrip().endswith(",")
    return ends_at_argument and (commas_before_message is None or commas == commas_before_message)


def placeholders_of(text: str) -> list[str]:
    return PLACEHOLDER.findall(text)


def translation_problems(original: str, english: str) -> list[str]:
    checks = [
        ("empty", not english.strip()),
        ("chinese left", bool(CJK.search(english))),
        ("placeholders differ", sorted(placeholders_of(original)) != sorted(placeholders_of(english))),
        ("unescaped quote", bool(UNESCAPED_QUOTE.search(english))),
        (
            "backslashes differ",
            len(BACKSLASH_NOT_BEFORE_QUOTE.findall(original)) != len(BACKSLASH_NOT_BEFORE_QUOTE.findall(english)),
        ),
    ]
    return [problem for problem, found in checks if found]


def apply_translation(line: str, target: Target, english: str) -> str:
    return line[: target.start] + english + line[target.end :]


def changed_beyond_strings(before_lines: list[str], after_lines: list[str]) -> list[int]:
    """Changed lines where more than the text inside the string literals differs."""
    return [
        number
        for number, (old, new) in enumerate(zip(before_lines, after_lines), start=1)
        if old != new and STRING_LITERAL.sub('""', old) != STRING_LITERAL.sub('""', new)
    ]


def message_tasks(path: str, lines: list[str]) -> list[dict]:
    in_code = lines_starting_in_code(lines, is_rust_path(path))
    tasks = []
    for number, (line, segments) in enumerate(zip(lines, split_lines(lines, is_rust_path(path))), start=1):
        code_before_comment = _code_before_first_comment(segments)
        target = message_target(code_before_comment) if in_code[number - 1] else None
        if target:
            tasks.append({"id": f"{path}:{number}", "path": path, "line": number, "text": target.text})
    return tasks


def _code_before_first_comment(segments) -> str:
    code = ""
    for segment in segments:
        if segment.is_comment:
            break
        code += segment.text
    return code


def check_all(tasks: list[dict], translations: dict[str, str]) -> list[str]:
    problems = []
    for task in tasks:
        english = translations.get(task["id"])
        found = ["no translation"] if english is None else translation_problems(task["text"], english)
        if found:
            problems.append(f"{task['id']}: {', '.join(found)}")
    return problems


def apply_all(tasks: list[dict], translations: dict[str, str]) -> list[str]:
    problems = []
    by_path: dict[str, list[dict]] = {}
    for task in tasks:
        by_path.setdefault(task["path"], []).append(task)
    for path, path_tasks in by_path.items():
        lines = Path(path).read_text().split("\n")
        for task in path_tasks:
            english = translations.get(task["id"])
            if english is None:
                problems.append(f"{task['id']}: no translation")
                continue
            found = translation_problems(task["text"], english)
            target = message_target(lines[task["line"] - 1])
            if found or not target or target.text != task["text"]:
                problems.append(f"{task['id']}: {', '.join(found) or 'the line changed since the extraction'}")
                continue
            lines[task["line"] - 1] = apply_translation(lines[task["line"] - 1], target, english)
        Path(path).write_text("\n".join(lines))
    return problems


def verify(base_revision: str, paths: list[str]) -> int:
    findings = []
    for path in paths:
        shown = subprocess.run(["git", "show", f"{base_revision}:{path}"], capture_output=True, text=True, check=True)
        before, after = shown.stdout.split("\n"), Path(path).read_text().split("\n")
        if len(before) != len(after):
            findings.append(f"{path}: line count {len(before)} -> {len(after)}")
            continue
        findings += [f"{path}:{number}: more than a string changed" for number in changed_beyond_strings(before, after)]
    print("\n".join(findings))
    return 1 if findings else 0


def main() -> int:
    command, *arguments = sys.argv[1:]
    if command == "verify":
        return verify(arguments[0], arguments[1:])
    if command == "extract":
        tasks = [
            task for path in arguments for task in message_tasks(path, Path(path).read_text().split("\n"))
        ]
        print(json.dumps(tasks, ensure_ascii=False, indent=1))
        return 0
    tasks = json.loads(Path(arguments[0]).read_text())
    run = check_all if command == "check" else apply_all
    problems = run(tasks, json.loads(Path(arguments[1]).read_text()))
    print("\n".join(problems))
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
