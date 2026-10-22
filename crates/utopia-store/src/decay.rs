//! 源级衰减（RC-003）：Lore 的 `scoreImportance` 逐条移植，同样的输入给出同样的分。
//!
//! 一个文档的分是 0–10 的整数：`round(10 · 0.5^(年龄/半衰期))` 加上几项修正。
//! 年龄从最近一次被检索算起，没被检索过就从创建算起。强度与分都不存，读的时候算。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const DEFAULT_HALF_LIFE_DAYS: f64 = 60.0;
pub const MAX_HALF_LIFE_DAYS: f64 = 365.0;
/// 每次被检索，半衰期加的天数（到 [`MAX_HALF_LIFE_DAYS`] 封顶）。
pub const RETRIEVAL_HALF_LIFE_GAIN_DAYS: f64 = 2.0;

const MILLIS_PER_DAY: f64 = 86_400_000.0;

/// `sources.decay`：缺的字段取 Lore 的默认值，所以 `{}` 就是 Lore 今天的行为。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DecaySettings {
    /// 源里文档超过这个数，每天的扫描才淘汰多出来的。
    pub max_items: u32,
    pub half_life_days: u32,
    /// 这么多天没被检索就算陈旧。
    pub stale_after_days: u32,
    /// 比这新的文档不参与淘汰。
    pub min_age_days: u32,
}

impl Default for DecaySettings {
    fn default() -> Self {
        Self {
            max_items: 500,
            half_life_days: 60,
            stale_after_days: 30,
            min_age_days: 30,
        }
    }
}

/// 打分用到的一个文档：`key` 是文档名，`value` 是它的文字。
pub struct Memory<'a> {
    pub key: &'a str,
    pub value: &'a str,
    pub created_at: DateTime<Utc>,
    pub last_retrieved_at: Option<DateTime<Utc>>,
    /// 0 或没有都取 [`DEFAULT_HALF_LIFE_DAYS`]。
    pub half_life_days: Option<f64>,
    pub retrieval_count: u32,
    pub stale: bool,
}

pub fn score_importance(memory: &Memory, now: DateTime<Utc>) -> u8 {
    let strength_points = (decay_strength(memory, now) * 10.0).round() as i64;
    let score = strength_points + importance_adjustments(memory);
    score.clamp(0, 10) as u8
}

fn decay_strength(memory: &Memory, now: DateTime<Utc>) -> f64 {
    let half_life = memory
        .half_life_days
        .filter(|days| *days > 0.0)
        .unwrap_or(DEFAULT_HALF_LIFE_DAYS);
    let since = memory.last_retrieved_at.unwrap_or(memory.created_at);
    let age_days = (now - since).num_milliseconds() as f64 / MILLIS_PER_DAY;
    0.5_f64.powf(age_days / half_life)
}

fn importance_adjustments(memory: &Memory) -> i64 {
    value_length_adjustment(memory.value)
        + key_prefix_adjustment(memory.key)
        + key_topic_adjustment(memory.key)
        + retrieval_adjustment(memory.retrieval_count)
        + if memory.stale { -1 } else { 0 }
}

/// 长度按 UTF-16 码元数，和 Lore（JS 的 `string.length`）一致，表情符号算两个。
fn value_length_adjustment(value: &str) -> i64 {
    match value.encode_utf16().count() {
        0..=49 => -2,
        501.. => 1,
        _ => 0,
    }
}

fn key_prefix_adjustment(key: &str) -> i64 {
    let prefixes = ["auto-curation/", "session-summary/"];
    -(prefixes.iter().filter(|p| key.starts_with(**p)).count() as i64)
}

fn key_topic_adjustment(key: &str) -> i64 {
    let topics = ["gotcha", "decision", "convention", "pattern"];
    2 * topics.iter().filter(|t| key.contains(**t)).count() as i64
}

fn retrieval_adjustment(retrievals: u32) -> i64 {
    match retrievals {
        20.. => 2,
        5..=19 => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 21, 0, 0, 0).unwrap()
    }

    fn memory<'a>(key: &'a str, value: &'a str, created_days_ago: i64) -> Memory<'a> {
        Memory {
            key,
            value,
            created_at: now() - Duration::days(created_days_ago),
            last_retrieved_at: None,
            half_life_days: None,
            retrieval_count: 0,
            stale: false,
        }
    }

    /// 期望值由 Lore 的 `scoreImportance`（libs/shared/src/domain/memory-ranking.ts）在同一个
    /// `now` 下算出，不是照这里的实现推的。
    #[test]
    fn a_score_matches_lores_for_every_vector() {
        let mid = "x".repeat(100);
        let long = "x".repeat(600);
        let short = "x".repeat(10);
        let thirty_emoji = "😀".repeat(30);
        let twenty_emoji = "😀".repeat(20);
        let at = |key, value: &str, days, tweak: fn(&mut Memory)| {
            let mut m = memory(key, value, days);
            tweak(&mut m);
            score_importance(&m, now())
        };
        let same: fn(&mut Memory) = |_| {};

        assert_eq!(at("notes/a", &mid, 0, same), 10, "fresh");
        assert_eq!(at("notes/a", &mid, 60, same), 5, "one half-life");
        assert_eq!(at("notes/a", &mid, 120, same), 3, "two half-lives");
        assert_eq!(
            at("notes/a", &mid, 300, |m| {
                m.last_retrieved_at = Some(now() - Duration::days(1))
            }),
            10,
            "a retrieval restarts the age"
        );
        assert_eq!(
            at("notes/a", &mid, 30, |m| m.half_life_days = Some(30.0)),
            5,
            "own half-life"
        );
        assert_eq!(
            at("notes/a", &mid, 60, |m| m.half_life_days = Some(0.0)),
            5,
            "zero half-life means the default"
        );
        assert_eq!(at("notes/a", &short, 0, same), 8, "short value");
        assert_eq!(at("notes/a", &long, 0, same), 10, "long value");
        assert_eq!(at("auto-curation/x", &mid, 0, same), 9, "auto-curation");
        assert_eq!(at("session-summary/x", &mid, 0, same), 9, "session-summary");
        assert_eq!(at("gotcha-pattern", &mid, 0, same), 10, "two topics");
        assert_eq!(at("decision/db", &mid, 90, same), 6, "topic, aged");
        assert_eq!(
            at("notes/a", &mid, 60, |m| m.retrieval_count = 5),
            6,
            "five retrievals"
        );
        assert_eq!(
            at("notes/a", &mid, 60, |m| m.retrieval_count = 20),
            7,
            "twenty retrievals"
        );
        assert_eq!(at("notes/a", &mid, 0, |m| m.stale = true), 9, "stale");
        assert_eq!(
            at("session-summary/x", &short, 900, |m| m.stale = true),
            0,
            "floor"
        );
        assert_eq!(
            at("decision-convention-gotcha-pattern", &long, 0, |m| {
                m.retrieval_count = 30
            }),
            10,
            "ceiling"
        );
        assert_eq!(
            at("notes/a", &thirty_emoji, 0, same),
            10,
            "60 UTF-16 units is not short"
        );
        assert_eq!(
            at("notes/a", &twenty_emoji, 0, same),
            8,
            "40 UTF-16 units is short"
        );
    }

    #[test]
    fn an_empty_settings_object_is_lores_defaults() {
        let settings: DecaySettings = serde_json::from_str("{}").unwrap();
        assert_eq!(
            settings,
            DecaySettings {
                max_items: 500,
                half_life_days: 60,
                stale_after_days: 30,
                min_age_days: 30
            }
        );
    }

    #[test]
    fn a_misspelled_setting_is_refused() {
        let parsed = serde_json::from_str::<DecaySettings>(r#"{"max_item": 5}"#);
        assert!(parsed.is_err());
    }
}
