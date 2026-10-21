//! 源级衰减（RC-003）：Lore 的 `scoreImportance` 逐条移植，同样的输入给出同样的分。
//!
//! 一个文档的分是 0–10 的整数：`round(10 · 0.5^(年龄/半衰期))` 加上几项修正。
//! 年龄从最近一次被检索算起，没被检索过就从创建算起。强度与分都不存，读的时候算。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

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

impl DecaySettings {
    /// 数据库不拦的范围：半衰期 1 到 365 天，容量和陈旧天数至少 1。
    pub fn validate(&self) -> Result<(), AppError> {
        let half_life_ok = (1..=MAX_HALF_LIFE_DAYS as u32).contains(&self.half_life_days);
        if !half_life_ok || self.max_items == 0 || self.stale_after_days == 0 {
            return Err(AppError::Validation(format!(
                "decay needs max_items >= 1, stale_after_days >= 1 and half_life_days within 1 to {}",
                MAX_HALF_LIFE_DAYS as u32
            )));
        }
        Ok(())
    }
}

/// 这些文档刚被一次检索交了出去：属于打开了衰减的源的，计数加一、最近检索时间记成现在、
/// 半衰期加 [`RETRIEVAL_HALF_LIFE_GAIN_DAYS`] 天（封顶 [`MAX_HALF_LIFE_DAYS`]）。别的源的文档
/// 一行也不写。一条语句写完所有文档；第一次被检索的文档从源里的半衰期起算。
pub async fn record_retrievals(pool: &PgPool, document_ids: &[Uuid]) -> AppResult<u64> {
    if document_ids.is_empty() {
        return Ok(0);
    }
    let written = sqlx::query(
        "INSERT INTO document_salience (document_id, retrieval_count, last_retrieved_at, half_life_days)
         SELECT d.id, 1, now(),
                LEAST($3::real, COALESCE((s.decay->>'half_life_days')::real, $4::real) + $2::real)
         FROM documents d JOIN sources s ON s.id = d.source_id
         WHERE d.id = ANY($1) AND s.decay IS NOT NULL
         ON CONFLICT (document_id) DO UPDATE SET
             retrieval_count = document_salience.retrieval_count + 1,
             last_retrieved_at = now(),
             half_life_days = LEAST($3::real, document_salience.half_life_days + $2::real)",
    )
    .bind(document_ids)
    .bind(RETRIEVAL_HALF_LIFE_GAIN_DAYS as f32)
    .bind(MAX_HALF_LIFE_DAYS as f32)
    .bind(DEFAULT_HALF_LIFE_DAYS as f32)
    .execute(pool)
    .await?;
    Ok(written.rows_affected())
}

/// 打分用到的一个文档：`key` 是文档名，`value_len` 是它的文字有多长（UTF-16 码元数，
/// 和 Lore 的 JS 字符串长度一致；库里按字符数，非基本平面的字符会少算，只差在边界上）。
pub struct Memory {
    pub key: String,
    pub value_len: usize,
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
    value_length_adjustment(memory.value_len)
        + key_prefix_adjustment(&memory.key)
        + key_topic_adjustment(&memory.key)
        + retrieval_adjustment(memory.retrieval_count)
        + if memory.stale { -1 } else { 0 }
}

fn value_length_adjustment(value_len: usize) -> i64 {
    match value_len {
        0..=49 => -2,
        501.. => 1,
        _ => 0,
    }
}

/// 一段文字的 UTF-16 码元数，Lore 里的 `string.length`：表情符号算两个。
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
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

/// 一直没被检索：最近一次检索（没有就创建）距今超过 `stale_after_days` 天。陈旧是读的时候
/// 算的，不写进库，也不碰事实的 `confidence`。
pub fn is_stale(last_touched: DateTime<Utc>, stale_after_days: u32, now: DateTime<Utc>) -> bool {
    now - last_touched > chrono::Duration::days(i64::from(stale_after_days))
}

/// 陈旧文档的检索分乘这个数，和 Lore 对陈旧事实的权重一样。
pub const STALE_SEARCH_WEIGHT: f64 = 0.6;

/// 这些文档里，属于打开了衰减的源、并且陈旧的那些（[`is_stale`] 的判据，在库里算）。
pub async fn stale_documents(
    pool: &PgPool,
    document_ids: &[Uuid],
) -> AppResult<std::collections::HashSet<Uuid>> {
    let ids: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT d.id
           FROM documents d
           JOIN sources s ON s.id = d.source_id
           LEFT JOIN document_salience sal ON sal.document_id = d.id
          WHERE d.id = ANY($1) AND s.decay IS NOT NULL
            AND COALESCE(sal.last_retrieved_at, d.created_at)
                < now() - make_interval(days => COALESCE((s.decay->>'stale_after_days')::int, $2))",
    )
    .bind(document_ids)
    .bind(i32::try_from(DecaySettings::default().stale_after_days).unwrap_or(30))
    .fetch_all(pool)
    .await?;
    Ok(ids.into_iter().map(|(id,)| id).collect())
}

/// 淘汰候选：一个文档和给它打分用的材料。
pub struct Candidate {
    pub document_id: Uuid,
    pub memory: Memory,
}

/// 分最低的 `count` 个，分相同的保持候选的先后（候选按创建时间从旧到新给）。
pub fn lowest_scoring(
    candidates: &[Candidate],
    now: DateTime<Utc>,
    count: usize,
) -> Vec<(Uuid, u8)> {
    let mut scored: Vec<(Uuid, u8)> = candidates
        .iter()
        .map(|c| (c.document_id, score_importance(&c.memory, now)))
        .collect();
    scored.sort_by_key(|(_, score)| *score);
    scored.truncate(count);
    scored
}

#[derive(sqlx::FromRow)]
struct CandidateRow {
    id: Uuid,
    filename: String,
    created_at: DateTime<Utc>,
    last_retrieved_at: Option<DateTime<Utc>>,
    half_life_days: Option<f32>,
    retrieval_count: i32,
    text_len: i64,
}

impl CandidateRow {
    fn into_candidate(self, settings: &DecaySettings, now: DateTime<Utc>) -> Candidate {
        let last_touched = self.last_retrieved_at.unwrap_or(self.created_at);
        Candidate {
            document_id: self.id,
            memory: Memory {
                key: self.filename,
                value_len: self.text_len as usize,
                created_at: self.created_at,
                last_retrieved_at: self.last_retrieved_at,
                half_life_days: Some(
                    self.half_life_days
                        .map_or(f64::from(settings.half_life_days), f64::from),
                ),
                retrieval_count: self.retrieval_count as u32,
                stale: is_stale(last_touched, settings.stale_after_days, now),
            },
        }
    }
}

/// 淘汰的计划：哪个源、哪些文档、其中最低的分。
pub struct Eviction {
    pub kb_id: Uuid,
    pub documents: Vec<Uuid>,
    pub lowest_score: u8,
}

/// 打开了衰减、活文档数超过 `max_items` 的源。
pub async fn sources_over_cap(pool: &PgPool) -> AppResult<Vec<Uuid>> {
    let ids: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT s.id FROM sources s
          WHERE s.decay IS NOT NULL
            AND (SELECT count(*) FROM documents d
                  WHERE d.source_id = s.id AND d.deleted_at IS NULL)
                > COALESCE((s.decay->>'max_items')::bigint, $1)",
    )
    .bind(i64::from(DecaySettings::default().max_items))
    .fetch_all(pool)
    .await?;
    Ok(ids.into_iter().map(|(id,)| id).collect())
}

/// 把一个源淘汰回 `max_items`：在比 `min_age_days` 老的文档里取最旧的 `2 × 多出来的数`，
/// 打分，其中最低的 `多出来的数` 个。多取一倍是为了让分有挑的余地，而不是只拿最旧的。
/// 源没开衰减或没超出，返回 None。
pub async fn plan_eviction(
    pool: &PgPool,
    source_id: Uuid,
    now: DateTime<Utc>,
) -> AppResult<Option<Eviction>> {
    let source: Option<(Uuid, Option<serde_json::Value>)> =
        sqlx::query_as("SELECT kb_id, decay FROM sources WHERE id = $1")
            .bind(source_id)
            .fetch_optional(pool)
            .await?;
    let Some((kb_id, Some(decay))) = source else {
        return Ok(None);
    };
    let settings: DecaySettings =
        serde_json::from_value(decay).map_err(|e| AppError::Other(e.into()))?;
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM documents WHERE source_id = $1 AND deleted_at IS NULL",
    )
    .bind(source_id)
    .fetch_one(pool)
    .await?;
    let excess = live - i64::from(settings.max_items);
    if excess <= 0 {
        return Ok(None);
    }
    let rows: Vec<CandidateRow> = sqlx::query_as(
        "SELECT d.id, d.filename, d.created_at, sal.last_retrieved_at, sal.half_life_days,
                COALESCE(sal.retrieval_count, 0) AS retrieval_count,
                COALESCE((SELECT sum(char_length(c.text)) FROM chunks c
                           WHERE c.document_id = d.id AND c.superseded_at IS NULL), 0)::bigint
                    AS text_len
           FROM documents d LEFT JOIN document_salience sal ON sal.document_id = d.id
          WHERE d.source_id = $1 AND d.deleted_at IS NULL AND d.created_at <= $2
          ORDER BY d.created_at LIMIT $3",
    )
    .bind(source_id)
    .bind(now - chrono::Duration::days(i64::from(settings.min_age_days)))
    .bind(excess * 2)
    .fetch_all(pool)
    .await?;
    let candidates: Vec<Candidate> = rows
        .into_iter()
        .map(|row| row.into_candidate(&settings, now))
        .collect();
    let chosen = lowest_scoring(&candidates, now, excess as usize);
    let Some((_, lowest_score)) = chosen.first().copied() else {
        return Ok(None);
    };
    Ok(Some(Eviction {
        kb_id,
        documents: chosen.into_iter().map(|(id, _)| id).collect(),
        lowest_score,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 21, 0, 0, 0).unwrap()
    }

    fn memory(key: &str, value: &str, created_days_ago: i64) -> Memory {
        Memory {
            key: key.into(),
            value_len: utf16_len(value),
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

    fn candidate(key: &str, days_old: i64) -> Candidate {
        Candidate {
            document_id: Uuid::now_v7(),
            memory: memory(key, &"x".repeat(100), days_old),
        }
    }

    #[test]
    fn the_lowest_scores_go_and_ties_keep_the_oldest_first() {
        let candidates = [
            candidate("notes/a", 200),
            candidate("decision/db", 200),
            candidate("session-summary/x", 200),
            candidate("notes/b", 100),
        ];
        let chosen = lowest_scoring(&candidates, now(), 2);
        let ids: Vec<Uuid> = chosen.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            vec![candidates[2].document_id, candidates[0].document_id]
        );
        assert!(chosen[0].1 <= chosen[1].1);
    }

    #[test]
    fn a_document_is_stale_only_after_the_days_have_passed() {
        let touched = now() - Duration::days(30);
        assert!(!is_stale(touched, 30, now()));
        assert!(is_stale(touched, 29, now()));
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
    fn a_setting_out_of_range_is_refused() {
        let ok = DecaySettings::default();
        assert!(ok.validate().is_ok());
        let bad = [
            DecaySettings {
                max_items: 0,
                ..ok.clone()
            },
            DecaySettings {
                half_life_days: 0,
                ..ok.clone()
            },
            DecaySettings {
                half_life_days: 366,
                ..ok.clone()
            },
            DecaySettings {
                stale_after_days: 0,
                ..ok.clone()
            },
        ];
        assert!(bad.iter().all(|settings| settings.validate().is_err()));
    }

    #[test]
    fn a_misspelled_setting_is_refused() {
        let parsed = serde_json::from_str::<DecaySettings>(r#"{"max_item": 5}"#);
        assert!(parsed.is_err());
    }
}
