import unittest

from comments import anchors, blanked_comments, code_projection, copied_translations, missing_anchors, comment_text, split_lines


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

    def test_raw_string_hides_double_slash_and_keeps_cjk_as_code(self):
        line = 'const X: &str = r#"<a href="http://x">项目</a>"#; // 注释'
        self.assertEqual(comments_of([line]), ["// 注释"])

    def test_raw_string_spanning_lines_is_code_on_every_line(self):
        lines = ['let x = r#"first // 一', 'second "quoted" // 二"#; // 三']
        self.assertEqual(comments_of(lines), ["", "// 三"])

    def test_identifier_ending_in_r_before_a_string_is_not_a_raw_string(self):
        self.assertEqual(comments_of(['let bar = "a//b"; // 注释']), ["// 注释"])


class Anchors(unittest.TestCase):
    def test_snake_case_paths_camel_case_and_issue_numbers_are_anchors(self):
        self.assertEqual(
            anchors("// 见 max_len 与 db::migrate、SearchIndex，参考 #642"),
            ["#642", "SearchIndex", "db::migrate", "max_len"],
        )

    def test_numbers_of_two_or_more_digits_are_anchors(self):
        self.assertEqual(anchors("// 页长取质数（37），0070 之后 1 次"), ["0070", "37"])

    def test_plain_words_are_not_anchors(self):
        self.assertEqual(anchors("// the `facts` table and None"), [])

    def test_translation_that_keeps_anchors_has_none_missing(self):
        self.assertEqual(
            missing_anchors("// 先查 `max_len`（#12）", "// look up `max_len` first (#12)"), []
        )

    def test_line_holding_another_lines_text_reports_missing_anchors(self):
        self.assertEqual(
            missing_anchors("// 先查 max_len（#12）", "// then check bar_baz"), ["#12", "max_len"]
        )

    def test_numbers_rewritten_with_chinese_magnitude_or_date_words_are_not_missing(self):
        self.assertEqual(missing_anchors("// 250 万行，到 12 月 31 日", "// 2.5 million rows, until 31 December"), [])

    def test_thousands_separators_do_not_change_a_number(self):
        self.assertEqual(missing_anchors("// 1500 条关系", "// 1,500 relations"), [])

    def test_translation_may_add_anchors(self):
        self.assertEqual(missing_anchors("// 先查 max_len", "// look up max_len and min_len"), [])


class CopiedTranslations(unittest.TestCase):
    def test_same_translation_of_two_identical_originals_is_fine(self):
        before = ["// 没有数据库时跳过而不是失败，自建自拆", "// 一行", "// 没有数据库时跳过而不是失败，自建自拆"]
        copy = "// Skipped rather than failed when there is no database, sets up and tears down its own data"
        self.assertEqual(copied_translations(before, [copy, "// A line", copy]), [])

    def test_translation_pasted_over_a_different_line_is_reported(self):
        before = ["// 终点被它说出来也记，时间线也记", "// 一行", "// 没有数据库时跳过而不是失败，自建自拆"]
        copy = "// Skipped rather than failed when there is no database, sets up and tears down its own data"
        self.assertEqual(copied_translations(before, [copy, "// A line", copy]), [1, 3])

    def test_short_repeated_lines_are_ignored(self):
        before = ["// 是", "// 否"]
        self.assertEqual(copied_translations(before, ["// yes", "// yes"]), [])


class BlankedComments(unittest.TestCase):
    def test_comment_with_text_that_became_empty_is_reported(self):
        self.assertEqual(blanked_comments(["// 有字", "//!", "/// 也有字"], ["//", "//!", "/// has text"]), [1])

    def test_already_empty_comment_is_fine(self):
        self.assertEqual(blanked_comments(["///", "// a"], ["///", "// a"]), [])

    def test_trailing_comment_that_was_emptied_is_reported(self):
        self.assertEqual(blanked_comments(["// 注释"], ["//"]), [1])


if __name__ == "__main__":
    unittest.main()
