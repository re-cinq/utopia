# Chinese comments

Tools to keep the comments of this fork in English while upstream writes them in Chinese.

| Script | Does |
|---|---|
| `find.py [--code] [--since REV] [PATH...]` | Lists lines with Chinese: comments by default, code and strings with `--code`. `--since REV` keeps only lines added since REV. |
| `guard.py [--base REV] PATH...` | Compares the working tree with REV (default `HEAD`) and proves only comments changed. Prints nothing when the translation is sound. |
| `pairs.py [--base REV] PATH...` | Prints the changed comment lines as old and new pairs, for a reviewer. |
| `test_comments.py` | Unit tests of the comment splitter and the checks: `python3 -m unittest test_comments` |

Run Python with `PYTHONDONTWRITEBYTECODE=1`.

## What the guard reports

Changed code, a changed line count, Chinese left in a comment, a name, number or `#ref` of the
original that is gone from the line and its neighbours, one long translation on two lines with
different originals, an emptied comment, a changed indentation or spacing after the comment marker,
and a trailing comment that repeats the code before it. Each one was found in a real translation.

## Rules for whoever translates

1. Change only the text of comments. Never touch code, string literals (SQL inside a string too),
   identifiers or test data.
2. One line in, one line out. Key a batch by line number, and compare old and new lines of a sample
   before applying. A comment holding another line's text is the worst mistake, and the guard
   cannot see all of them.
3. Keep the marker, the indentation and the spaces after the marker (markdown lists depend on them;
   clippy and rustfmt check them). For `code // 中文` only the part after `//` changes.
4. Keep names in backticks, numbers, `#1234` and ADR numbers as they are.
5. Do not write a quote or `*/` that could end a string or a block comment.
6. Translate Chinese examples inside comments too, unless the comment only makes sense with the
   characters. Those lines stay, and the guard reports them every time.
7. Do not run a formatter or build in the translating step. Check with `cargo fmt --check` and
   `cargo check`, or `tsc` and `vitest` for `web`, before the PR.

A comment line in a multi-line string is not a comment: the splitter follows strings and raw strings
across lines, and a SQL `/* */` inside one is left alone.
