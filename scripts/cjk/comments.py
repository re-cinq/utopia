"""Splits source lines into code and comment segments, for Rust and TypeScript."""

import re
from dataclasses import dataclass

CJK = re.compile(r"[一-鿿　-〿＀-￯]")
RUST_SUFFIXES = (".rs",)
TS_SUFFIXES = (".ts", ".tsx", ".mjs", ".js")
ANCHOR = re.compile(r"#\d+|\d{2,}(?:\.\d+)?|[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z0-9_]+)+|[A-Za-z]+_[A-Za-z0-9_]+|[A-Z][a-z0-9]+(?:[A-Z][A-Za-z0-9]*)+")
MIN_COPY_LENGTH = 40
MIN_REPEATED_CODE_LENGTH = 6
LINE_COMMENT_ONLY = re.compile(r"^\s*//[/!]?( *)\S")
COMMENT_MARKER = re.compile(r"^(?://[/!]?|/\*[*!]?|\*/?)")
REWRITTEN_NUMBER_WORDS = re.compile(r"[万亿千百年月日]")
RAW_STRING_OPENER = re.compile(r'(?<![A-Za-z0-9_])b?r(#*)"')


@dataclass(frozen=True)
class Segment:
    is_comment: bool
    text: str


@dataclass(frozen=True)
class LineState:
    block_depth: int = 0
    raw_string_closer: str = ""
    open_quote: str = ""


def split_lines(source_lines: list[str], is_rust: bool) -> list[list[Segment]]:
    state = LineState()
    result = []
    for line in source_lines:
        segments, state = _split_line(line, state, is_rust)
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


def anchors(comment: str) -> list[str]:
    """Names (snake_case, paths, CamelCase) and issue numbers a translation must carry over."""
    return sorted(set(ANCHOR.findall(re.sub(r"(?<=\d),(?=\d{3})", "", comment))))


def missing_anchors(before: str, after: str) -> list[str]:
    lost = set(anchors(before)) - set(anchors(after))
    if REWRITTEN_NUMBER_WORDS.search(before):
        lost = {anchor for anchor in lost if not anchor[0].isdigit()}
    return sorted(lost)


def reindented(before_lines: list[str], after_lines: list[str]) -> list[int]:
    """Changed lines whose leading whitespace is not what it was (rustfmt moves block comments)."""
    return [
        number
        for number, (old, new) in enumerate(zip(before_lines, after_lines), start=1)
        if old != new and _leading_whitespace(old) != _leading_whitespace(new)
    ]


def _leading_whitespace(line: str) -> str:
    return line[: len(line) - len(line.lstrip())]


def marker_spacing_changed(before_lines: list[str], after_lines: list[str]) -> list[int]:
    """Comment-only lines whose spaces after the marker changed: markdown list items depend on them."""
    return [
        number
        for number, (old, new) in enumerate(zip(before_lines, after_lines), start=1)
        if old != new
        and (old_spacing := _spacing_after_marker(old)) is not None
        and old_spacing != _spacing_after_marker(new)
    ]


def _spacing_after_marker(line: str):
    found = LINE_COMMENT_ONLY.match(line)
    return found.group(1) if found else None


def code_repeated_in_comment(before_lines: list[str], after_lines: list[str], is_rust: bool) -> list[int]:
    """Changed lines whose trailing comment contains the code written before it."""
    flagged = []
    for number, (old, new, segments) in enumerate(
        zip(before_lines, after_lines, split_lines(after_lines, is_rust)), start=1
    ):
        code, comment = code_text(segments).strip(), comment_text(segments).strip()
        if old != new and len(code) >= MIN_REPEATED_CODE_LENGTH and code in comment[2:]:
            flagged.append(number)
    return flagged


def blanked_comments(before_comments: list[str], after_comments: list[str]) -> list[int]:
    """Lines whose comment had words and has none after the translation."""
    return [
        number
        for number, (old, new) in enumerate(zip(before_comments, after_comments), start=1)
        if _words_of(old) and not _words_of(new)
    ]


def _words_of(comment: str) -> str:
    return COMMENT_MARKER.sub("", comment.strip()).strip()


def copied_translations(before_comments: list[str], after_comments: list[str]) -> list[int]:
    """Lines whose translation is a long text that also sits on a line with a different original."""
    originals_by_text: dict[str, set[str]] = {}
    for old, new in zip(before_comments, after_comments):
        if old != new and len(new.strip()) >= MIN_COPY_LENGTH:
            originals_by_text.setdefault(new.strip(), set()).add(old.strip())
    return [
        number
        for number, (old, new) in enumerate(zip(before_comments, after_comments), start=1)
        if old != new and len(originals_by_text.get(new.strip(), ())) > 1
    ]


def is_rust_path(path: str) -> bool:
    return path.endswith(RUST_SUFFIXES)


def is_supported_path(path: str) -> bool:
    return path.endswith(RUST_SUFFIXES + TS_SUFFIXES)


def _split_line(line: str, state: LineState, is_rust: bool):
    block_depth = state.block_depth
    raw_string_closer = state.raw_string_closer
    segments: list[Segment] = []
    start = 0
    position = 0
    quote = state.open_quote
    while position < len(line):
        if raw_string_closer:
            end = line.find(raw_string_closer, position)
            if end == -1:
                position = len(line)
            else:
                position = end + len(raw_string_closer)
                raw_string_closer = ""
            continue
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
        raw_opener = RAW_STRING_OPENER.match(line, position) if is_rust else None
        if raw_opener:
            raw_string_closer = '"' + raw_opener.group(1)
            position = raw_opener.end()
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
            return segments, LineState(block_depth, raw_string_closer, _quote_that_spans_lines(quote, is_rust))
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
    return segments, LineState(block_depth, raw_string_closer, _quote_that_spans_lines(quote, is_rust))


def _quote_that_spans_lines(quote: str, is_rust: bool) -> str:
    spans_lines = '"' if is_rust else "`"
    return quote if quote == spans_lines else ""


def _skip_rust_char_literal(line: str, position: int) -> int:
    literal = re.match(r"'(\\.[^']*|[^\\'])'", line[position:])
    return position + (len(literal.group(0)) if literal else 1)
