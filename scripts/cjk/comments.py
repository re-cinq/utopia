"""Splits source lines into code and comment segments, for Rust and TypeScript."""

import re
from dataclasses import dataclass

CJK = re.compile(r"[一-鿿　-〿＀-￯]")
RUST_SUFFIXES = (".rs",)
TS_SUFFIXES = (".ts", ".tsx", ".mjs", ".js")


@dataclass(frozen=True)
class Segment:
    is_comment: bool
    text: str


def split_lines(source_lines: list[str], is_rust: bool) -> list[list[Segment]]:
    block_depth = 0
    result = []
    for line in source_lines:
        segments, block_depth = _split_line(line, block_depth, is_rust)
        result.append(segments)
    return result


def code_projection(source_lines: list[str], is_rust: bool) -> list[str]:
    return [
        "".join(segment.text for segment in segments if not segment.is_comment)
        for segments in split_lines(source_lines, is_rust)
    ]


def comment_text(segments: list[Segment]) -> str:
    return "".join(segment.text for segment in segments if segment.is_comment)


def code_text(segments: list[Segment]) -> str:
    return "".join(segment.text for segment in segments if not segment.is_comment)


def is_rust_path(path: str) -> bool:
    return path.endswith(RUST_SUFFIXES)


def is_supported_path(path: str) -> bool:
    return path.endswith(RUST_SUFFIXES + TS_SUFFIXES)


def _split_line(line: str, block_depth: int, is_rust: bool):
    segments: list[Segment] = []
    start = 0
    position = 0
    quote = ""
    while position < len(line):
        if block_depth:
            end = line.find("*/", position)
            nested = line.find("/*", position) if is_rust else -1
            if nested != -1 and (end == -1 or nested < end):
                block_depth += 1
                position = nested + 2
            elif end == -1:
                position = len(line)
            else:
                block_depth -= 1
                position = end + 2
                if not block_depth:
                    segments.append(Segment(True, line[start:position]))
                    start = position
            continue
        char = line[position]
        two = line[position : position + 2]
        if quote:
            if char == "\\":
                position += 2
                continue
            if char == quote:
                quote = ""
            position += 1
            continue
        if char == '"' or (char == "`" and not is_rust):
            quote = char
        elif is_rust and char == "'":
            position = _skip_rust_char_literal(line, position)
            continue
        elif not is_rust and char in "'":
            quote = char
        elif two == "//":
            if start < position:
                segments.append(Segment(False, line[start:position]))
            segments.append(Segment(True, line[position:]))
            return segments, block_depth
        elif two == "/*":
            if start < position:
                segments.append(Segment(False, line[start:position]))
            start = position
            block_depth = 1
            position += 2
            continue
        position += 1
    if block_depth and start < len(line):
        segments.append(Segment(True, line[start:]))
    elif start < len(line):
        segments.append(Segment(False, line[start:]))
    return segments, block_depth


def _skip_rust_char_literal(line: str, position: int) -> int:
    literal = re.match(r"'(\\.[^']*|[^\\'])'", line[position:])
    return position + (len(literal.group(0)) if literal else 1)
