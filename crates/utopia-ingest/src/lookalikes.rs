//! Characters in the PDF text layer that look alike but have different code points: radicals and ligatures are mapped back to the character they display.
//!
//! A PDF draws glyphs, and the text layer maps each glyph back to a code point through the font's ToUnicode table. In the font, one glyph often
//! is both a radical and a unified Han character: in PingFang, "⼀" (U+2F00, Kangxi radical) and "一" (U+4E00) are
//! the same glyph. ToUnicode points at the radical code point, so the text looks identical but the code point is the radical: searching for "日期"
//! does not find "⽇期", and "王⼤⼒" and "王大力" are two different names. PDFs generated on macOS (Quartz, Chrome,
//! using PingFang) mostly come out this way for characters that have a radical twin code point. Latin ligatures work the same: the ﬁ in "ﬁscal" is one
//! glyph and one code point (U+FB01).
//!
//! Radicals are replaced with the unified Han character that Unicode says they are equivalent to: UCD's `EquivalentUnifiedIdeograph.txt`,
//! vendored alongside. NFKC also replaces Kangxi radicals, but not the Radicals Supplement (⻄, ⻓), so this table is used instead.
//! Stroke forms in the table are not replaced: a document that writes a stroke is using it as a stroke. Ligatures are split into letters. Nothing else is touched; this is not
//! NFKC: full-width punctuation and full-width digits stay as they are.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Unicode 18.0.0's EquivalentUnifiedIdeograph.txt, vendored as-is
const EQUIVALENTS: &str = include_str!("EquivalentUnifiedIdeograph.txt");

/// Two radical blocks: CJK Radicals Supplement (U+2E80–2EFF) and Kangxi Radicals (U+2F00–2FDF)
fn is_radical(c: char) -> bool {
    matches!(c, '\u{2E80}'..='\u{2EFF}' | '\u{2F00}'..='\u{2FDF}')
}

/// Radical → the unified Han character it is equivalent to. Radicals not in the table (⺀) are not here
fn radicals() -> &'static HashMap<char, char> {
    static MAP: OnceLock<HashMap<char, char>> = OnceLock::new();
    MAP.get_or_init(|| {
        let hex = |s: &str| u32::from_str_radix(s.trim(), 16).ok();
        let mut map = HashMap::new();
        // One entry per line: `2F08       ; 4EBA  #  KANGXI RADICAL MAN`, and the left side can also be a range `2E8C..2E8D`
        for line in EQUIVALENTS.lines() {
            let data = line.split('#').next().unwrap_or_default();
            let Some((from, to)) = data.split_once(';') else {
                continue;
            };
            let (lo, hi) = from.split_once("..").unwrap_or((from, from));
            let (Some(lo), Some(hi), Some(to)) =
                (hex(lo), hex(hi), hex(to).and_then(char::from_u32))
            else {
                continue;
            };
            for c in (lo..=hi)
                .filter_map(char::from_u32)
                .filter(|&c| is_radical(c))
            {
                map.insert(c, to);
            }
        }
        map
    })
}

/// Latin ligatures → letters (U+FB00–FB06, split the same way as NFKC)
fn ligature(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{FB00}' => "ff",
        '\u{FB01}' => "fi",
        '\u{FB02}' => "fl",
        '\u{FB03}' => "ffi",
        '\u{FB04}' => "ffl",
        '\u{FB05}' | '\u{FB06}' => "st",
        _ => return None,
    })
}

/// Radicals and ligatures are mapped back to the character they display; when there are none, the input is borrowed unchanged
pub(crate) fn plain(text: &str) -> Cow<'_, str> {
    if !text.chars().any(|c| is_radical(c) || ligature(c).is_some()) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match ligature(c) {
            Some(letters) => out.push_str(letters),
            None if is_radical(c) => out.push(radicals().get(&c).copied().unwrap_or(c)),
            None => out.push(c),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kangxi_radical_reads_as_the_ideograph_unicode_names() {
        let kangxi: Vec<char> = ('\u{2F00}'..='\u{2FD5}').collect();
        assert_eq!(kangxi.len(), 214);
        for c in kangxi {
            let ideograph = radicals().get(&c).copied();
            assert!(
                ideograph.is_some_and(|i| !is_radical(i)),
                "{c} (U+{:04X}) → {ideograph:?}",
                c as u32
            );
        }
        // The supplement block is in the table too, except ⺀, which Unicode says has no equivalent character
        assert_eq!(radicals().len(), 214 + 114);
        assert_eq!(radicals().get(&'\u{2E80}'), None);
    }

    #[test]
    fn the_lookalikes_of_a_mac_pdf_read_as_the_characters_they_show() {
        let read = "项⽬负责⼈为王⼤⼒，现⾦流量净额同⽐增⻓。⼀⾏⻄⺟⻳⺅ ﬁscal ﬂow ﬄ";
        assert_eq!(
            plain(read),
            "项目负责人为王大力，现金流量净额同比增长。一行西母龟亻 fiscal flow ffl"
        );
        // Radicals without an equivalent character are kept as-is
        assert_eq!(plain("⺀"), "⺀");
    }

    #[test]
    fn other_text_is_borrowed_as_it_is() {
        // Full-width punctuation, digits and letters are not touched: this is not NFKC
        let text = "营业收入１２５，４３０元（同比增长１３．７７％）；ＡＢＣ “引号” café";
        assert!(matches!(plain(text), Cow::Borrowed(t) if t == text));
    }
}
