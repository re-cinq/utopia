//! Text cleanup before it goes into a `TEXT` column.
//!
//! Postgres `TEXT` does not accept 0x00: inserting it fails with `invalid byte sequence for encoding "UTF8": 0x00`,
//! the transaction rolls back, and only a few bytes are bad while a whole document or a whole memory is lost. Text reaches `chunks.text` by two paths,
//! and both must be stripped: parsed document bodies (#611 / #630), and memories written by the `remember` tool
//! (#665: the `\u0000` escape in JSON is legal, and `serde_json` decodes it back to NUL).

use std::borrow::Cow;

/// Strip NUL (`\0`).
///
/// The vast majority of text contains no NUL at all; in that case it is borrowed as is, rather than copying a whole copy for every document body.
pub fn without_nul(text: &str) -> Cow<'_, str> {
    if text.contains('\0') {
        Cow::Owned(text.replace('\0', ""))
    } else {
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrows_when_there_is_nothing_to_strip() {
        assert!(matches!(without_nul("plain text"), Cow::Borrowed(_)));
        assert!(matches!(without_nul("中文正文"), Cow::Borrowed(_)));
    }

    #[test]
    fn strips_every_nul_and_keeps_everything_else() {
        assert_eq!(without_nul("a\0b\0\0c"), "abc");
        assert_eq!(without_nul("\0"), "");
        assert_eq!(without_nul("营收\0增长"), "营收增长");
    }
}
