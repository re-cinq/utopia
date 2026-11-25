import unittest

from strings import apply_translation, changed_beyond_strings, message_target, message_tasks, placeholders_of, translation_problems


def target_text(line):
    found = message_target(line)
    return found.text if found else None


class MessageTarget(unittest.TestCase):
    def test_log_message_after_fields(self):
        self.assertEqual(target_text('tracing::warn!(error = %e, "清理过期告警失败");'), "清理过期告警失败")

    def test_log_message_only(self):
        self.assertEqual(target_text('info!("开始同步");'), "开始同步")

    def test_error_context(self):
        self.assertEqual(target_text('    .context("抽取失败")?;'), "抽取失败")

    def test_anyhow_with_placeholder(self):
        self.assertEqual(target_text('.map_err(|e| anyhow::anyhow!("base IRI 无效：{e}"))?;'), "base IRI 无效：{e}")

    def test_expect_message(self):
        self.assertEqual(target_text('let w = self.writer.lock().expect("writer 锁中毒");'), "writer 锁中毒")

    def test_assert_message_is_the_second_argument(self):
        self.assertEqual(target_text('assert!(x.truncated, "截断要标出来");'), "截断要标出来")

    def test_assert_eq_message_is_the_third_argument(self):
        self.assertEqual(target_text('assert_eq!(x.path, sorted, "组按 id 排序");'), "组按 id 排序")

    def test_assert_eq_expected_value_is_data(self):
        self.assertIsNone(message_target('assert_eq!(parse_quantity("2亿美元"), None);'))

    def test_assert_eq_with_data_and_message_takes_the_message(self):
        self.assertEqual(target_text('assert_eq!(f("2亿"), None, "不该解析");'), "不该解析")

    def test_field_with_chinese_value_before_an_ascii_message_is_data(self):
        self.assertIsNone(message_target('warn!(name = "张伟", "ok");'))

    def test_plain_data_string_is_not_a_message(self):
        self.assertIsNone(message_target('"千万" => 7,'))

    def test_string_inside_nested_call_before_the_message_is_skipped(self):
        self.assertEqual(target_text('assert!(is_ok(f("张伟")), "应当成功");'), "应当成功")

    def test_raw_string_is_not_a_message(self):
        self.assertIsNone(message_target('expect(r#"张伟"#);'))

    def test_literal_outside_the_macro_is_not_a_message(self):
        self.assertIsNone(message_target('assert!(ok); let s = "数据";'))

    def test_span_points_at_the_text_between_the_quotes(self):
        line = 'x.context("抽取失败")?;'
        found = message_target(line)
        self.assertEqual(line[found.start : found.end], "抽取失败")


class Placeholders(unittest.TestCase):
    def test_braces_and_escapes_are_placeholders(self):
        self.assertEqual(sorted(placeholders_of("失败 {e} {{x}} {0:?}")), ["{0:?}", "{e}", "{{", "}}"])


class TranslationProblems(unittest.TestCase):
    def test_sound_translation_has_no_problems(self):
        self.assertEqual(translation_problems("base IRI 无效：{e}", "base IRI is invalid: {e}"), [])

    def test_lost_placeholder_is_a_problem(self):
        self.assertEqual(translation_problems("无效：{e}", "invalid"), ["placeholders differ"])

    def test_unescaped_quote_is_a_problem(self):
        self.assertEqual(translation_problems("无效", 'say "no"'), ["unescaped quote"])

    def test_escaped_quote_is_fine(self):
        self.assertEqual(translation_problems("无效", 'say \\"no\\"'), [])

    def test_chinese_left_is_a_problem(self):
        self.assertEqual(translation_problems("无效", "无效 invalid"), ["chinese left"])

    def test_empty_is_a_problem(self):
        self.assertEqual(translation_problems("无效", " "), ["empty"])

    def test_new_backslash_is_a_problem(self):
        self.assertEqual(translation_problems("无效", "a\\nb"), ["backslashes differ"])


class ApplyTranslation(unittest.TestCase):
    def test_replaces_only_the_message_text(self):
        line = 'x.context("抽取失败")?; // 注释'
        found = message_target(line)
        self.assertEqual(apply_translation(line, found, "extraction failed"), 'x.context("extraction failed")?; // 注释')


class MessageTasks(unittest.TestCase):
    def test_quote_inside_a_trailing_comment_is_not_a_message(self):
        self.assertEqual(message_tasks("a.rs", ['x.expect("ok"); // .expect("错误")']), [])

    def test_line_inside_a_multi_line_string_is_not_a_message(self):
        lines = ['let q = "', '  .context("抽取失败")', '";']
        self.assertEqual(message_tasks("a.rs", lines), [])

    def test_message_line_becomes_a_task_with_its_id(self):
        self.assertEqual(
            message_tasks("a.rs", ["fn f() {", '    x.context("抽取失败")?;']),
            [{"id": "a.rs:2", "path": "a.rs", "line": 2, "text": "抽取失败"}],
        )


class ChangedBeyondStrings(unittest.TestCase):
    def test_only_the_string_text_changed_is_fine(self):
        self.assertEqual(changed_beyond_strings(['x.context("抽取失败")?;'], ['x.context("extraction failed")?;']), [])

    def test_code_outside_the_string_changed_is_reported(self):
        self.assertEqual(changed_beyond_strings(['x.context("抽取失败")?;'], ['y.context("extraction failed")?;']), [1])

    def test_a_string_that_became_two_is_reported(self):
        self.assertEqual(changed_beyond_strings(['x.context("抽取失败")?;'], ['x.context("a", "b")?;']), [1])

    def test_unchanged_line_is_fine(self):
        self.assertEqual(changed_beyond_strings(["let a = 1;"], ["let a = 1;"]), [])


if __name__ == "__main__":
    unittest.main()
