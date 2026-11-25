import unittest

from comments import code_projection, comment_text, split_lines


def comments_of(lines, is_rust=True):
    return [comment_text(segments) for segments in split_lines(lines, is_rust)]


class SplitLines(unittest.TestCase):
    def test_line_comment_after_code_keeps_code_in_projection(self):
        self.assertEqual(code_projection(["let a = 1; // 注释"], True), ["let a = 1; "])

    def test_double_slash_inside_string_is_code(self):
        self.assertEqual(comments_of(['let url = "http://x"; // 注释']), ["// 注释"])

    def test_rust_char_literal_quote_does_not_open_a_string(self):
        self.assertEqual(comments_of(["let q = '\"'; // 注释"]), ["// 注释"])

    def test_rust_lifetime_does_not_open_a_string(self):
        self.assertEqual(comments_of(["fn f<'a>(x: &'a str) {} // 注释"]), ["// 注释"])

    def test_block_comment_spanning_lines_is_comment_on_every_line(self):
        lines = ["/* 第一行", "第二行", "第三行 */ let a = 1;"]
        self.assertEqual(code_projection(lines, True), ["", "", " let a = 1;"])

    def test_rust_nested_block_comment_closes_at_outer_end(self):
        lines = ["/* 外 /* 内 */ 外 */ let a = 1;"]
        self.assertEqual(code_projection(lines, True), [" let a = 1;"])

    def test_typescript_template_literal_hides_double_slash(self):
        self.assertEqual(comments_of(["const u = `a//b`; // 注释"], False), ["// 注释"])

    def test_cjk_in_string_stays_in_code_projection(self):
        self.assertEqual(code_projection(['let s = "名字";'], True), ['let s = "名字";'])


if __name__ == "__main__":
    unittest.main()
