//! Record-axis predicates (0019): `held_at(T)`: **which rows we believed to hold at moment T**.
//!
//! The world axis (`valid_from` / `valid_to`) answers "what the world was like then", the record axis (`recorded_at` /
//! `invalidated_at`) answers "what we believed the world was like then". The write side has recorded both axes since the graph migration,
//! but the read side could only reach the first one: a fact corrected in March does not exist at any slider position.
//!
//! **Predicates are composed only here.** Once the defence spreads to every read point, missing one is silent: SQL raises no error,
//! and `cargo check` does not say a word either (0009 tripped exactly here; the `human_type_decisions` test is what that episode left behind).
//! So read paths call the functions here and do not write `invalidated_at` themselves.
//!
//! **Write paths do not use it** (0019): `confirm_fact` / `reject_fact`, undoing an adoption, dedupe checks
//! are all guards on "the current row": corrections always happen now; there is no "edit a row as of March" thing.
//!
//! The arguments are `Option<usize>` (the bound parameter index): `None` means "now", and the predicate is reduced in the **SQL text**
//! to `invalidated_at IS NULL`: not merely equivalent in meaning, but so the optimizer can see it; otherwise the two-branch
//! `IS NULL OR > now()` would make the partial index on `WHERE invalidated_at IS NULL`
//! unusable across the whole tree (the comment on `held()` records the 61.6s lesson from that). So read paths call this in exactly one place,
//! rather than writing one for replay and one for the present: two copies is where the next missed edit would happen.
//!
//! **Reducing to `IS NULL` rests on a write invariant**: no row carries a future
//! `invalidated_at` / `deleted_at` / `decided_at` / `resolved_at`: revocation always stamps `now()`. With that, `IS NULL` means the same as `IS NULL OR > now()`; without a CHECK
//! guarding it, there is only this prose. If it breaks (say a future invalidation time is pre-filled), the `None` path
//! would treat "invalidation not yet in effect" as "never invalidated". Before the reduction, one call site happened to tolerate
//! this kind of violation; after it, all four predicates depend on it,
//! so it must be written here, so that the next reader hits it.

/// The record-axis interval formed by the start and end columns: `since <= T < invalidated_at`.
///
/// **`None` must reduce to a single-branch predicate**, not output the two-branch `IS NULL OR > now()` as before:
/// partial indexes (`facts_live_subject_idx` etc., `WHERE invalidated_at IS NULL`) are usable only when
/// the query predicate can imply `invalidated_at IS NULL`; `OR > now()` leaves the optimizer
/// unable to prove it: read paths fall back to a full per-entity table scan (on a database with 9311 entities x 94683 facts,
/// the `graph/overview` node query took 61.6s, and 4.3s after the reduction).
fn held(alias: &str, since: &str, as_of: Option<usize>) -> String {
    match as_of {
        None => format!("{alias}.invalidated_at IS NULL"),
        Some(param) => format!(
            "{alias}.{since} <= coalesce(${param}, now()) \
             AND ({alias}.invalidated_at IS NULL OR {alias}.invalidated_at > coalesce(${param}, now()))"
        ),
    }
}

/// `facts`: the assertion is still held by us at moment T.
pub fn facts_held_at(alias: &str, as_of: Option<usize>) -> String {
    held(alias, "recorded_at", as_of)
}

/// `derived_facts`: the derivation had been inferred at T and not overturned. The replayed graph keeps the edges inferred **at that time**,
/// rather than the conclusions of today's rules.
pub fn derived_held_at(alias: &str, as_of: Option<usize>) -> String {
    held(alias, "derived_at", as_of)
}

/// `axiom_violations`: the violation was still open at T. Column names differ from the two tables above (`detected_at` /
/// `decided_at` + `status`), but it asks the same question, so it lives here too rather than being assembled ad hoc at read points.
///
/// Pruned historical rows without `decided_at` count as "not open at the time": better to draw one fewer ghost edge
/// than to conjure a contradiction onto March's graph that was only discovered today.
pub fn violation_open_at(alias: &str, as_of: Option<usize>) -> String {
    match as_of {
        None => format!("{alias}.status = 'open'"),
        Some(param) => format!(
            "{alias}.detected_at <= coalesce(${param}, now()) \
             AND ({alias}.status = 'open' OR {alias}.decided_at > coalesce(${param}, now()))"
        ),
    }
}

/// `fact_conflicts`: the temporal conflict was still open at T.
pub fn conflict_open_at(alias: &str, as_of: Option<usize>) -> String {
    match as_of {
        None => format!("{alias}.status = 'open'"),
        Some(param) => format!(
            "{alias}.created_at <= coalesce(${param}, now()) \
             AND ({alias}.status = 'open' OR {alias}.resolved_at > coalesce(${param}, now()))"
        ),
    }
}

/// `documents`: the document was still in the database at T. Deletion leaves a tombstone (#268), so a "deleted document"
/// should still appear as normal at any moment before its deletion: its chunks were genuinely retrievable then.
pub fn document_live_at(alias: &str, as_of: Option<usize>) -> String {
    match as_of {
        None => format!("{alias}.deleted_at IS NULL"),
        Some(param) => format!(
            "{alias}.created_at <= coalesce(${param}, now()) \
             AND ({alias}.deleted_at IS NULL OR {alias}.deleted_at > coalesce(${param}, now()))"
        ),
    }
}

/// `chunks`: the chunk was still the current version at T. Whether evidence "has vanished" must be judged by the version at that time:
/// a passage superseded by today's re-parse is still live evidence on March's graph.
pub fn chunk_live_at(alias: &str, as_of: Option<usize>) -> String {
    match as_of {
        None => format!("{alias}.superseded_at IS NULL"),
        Some(param) => format!(
            "{alias}.created_at <= coalesce(${param}, now()) \
             AND ({alias}.superseded_at IS NULL OR {alias}.superseded_at > coalesce(${param}, now()))"
        ),
    }
}

/// A fact's evidence, selected as `rw_ee` (evidence), `rw_ec` (chunk), `rw_ed` (document), for [`role_word_fact_ends`] to aggregate
pub const ROLE_WORD_EVIDENCE: &str = "fact_evidence rw_ee
         JOIN chunks rw_ec ON rw_ec.id = rw_ee.chunk_id
         JOIN documents rw_ed ON rw_ed.id = rw_ec.document_id";

/// Aggregates [`ROLE_WORD_EVIDENCE`] by fact: whether at T there is any live chunk (`any_live`), and among the chunks existing at T
/// the latest end point (`last_end`). A chunk's end is the earlier of its `superseded_at` and its document's `deleted_at`.
pub fn role_word_fact_ends(as_of: Option<usize>) -> String {
    format!(
        "bool_or({} AND {}) AS any_live,
         max(least(rw_ec.superseded_at, rw_ed.deleted_at)) FILTER (WHERE {}) AS last_end",
        chunk_live_at("rw_ec", as_of),
        document_live_at("rw_ed", as_of),
        chunk_existed_at("rw_ec", as_of),
    )
}

fn chunk_existed_at(alias: &str, as_of: Option<usize>) -> String {
    match as_of {
        None => "true".into(),
        Some(param) => format!("{alias}.created_at <= coalesce(${param}, now())"),
    }
}

/// Whether a role word speaks for its fact at T (0070 decisions 2 and 3), given its chunk `c`, that chunk's document `d`
/// and its fact's [`role_word_fact_ends`] row `f`.
///
/// If its chunk and that chunk's document were both still alive at T, it speaks; if several live chunks have different values, all of them speak.
/// When the fact has no live evidence at all at T, the chunks that ended last speak (those ending at the same moment count too),
/// looking only at chunks existing at T. Each fact's evidence is aggregated only once: a statement re-derived hundreds of times is not rescanned row by row.
pub fn role_word_speaks_given(c: &str, d: &str, f: &str, as_of: Option<usize>) -> String {
    format!(
        "(({c_live} AND {d_live})
          OR (NOT {f}.any_live AND {existed}
              AND least({c}.superseded_at, {d}.deleted_at) = {f}.last_end))",
        c_live = chunk_live_at(c, as_of),
        d_live = document_live_at(d, as_of),
        existed = chunk_existed_at(c, as_of),
    )
}

/// `statement_qualifiers`: does this row `q` speak right now; the single-row shape of [`role_word_speaks_given`].
/// Used only at write entry points, so no T is taken; aliases all carry the `rw_` prefix so they do not collide with the caller's.
pub fn role_word_speaks_now(q: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM chunks rw_c JOIN documents rw_d ON rw_d.id = rw_c.document_id
                  CROSS JOIN LATERAL (SELECT {ends} FROM {evidence}
                                       WHERE rw_ee.fact_id = {q}.fact_id) rw_f
                  WHERE rw_c.id = {q}.chunk_id AND {speaks})",
        ends = role_word_fact_ends(None),
        evidence = ROLE_WORD_EVIDENCE,
        speaks = role_word_speaks_given("rw_c", "rw_d", "rw_f", None),
    )
}

/// `entity_merges`: is this merge **in effect** at T (0019 second slice / #336)?
///
/// Entities carry no record axis: `merged_into` only says a merge happened, not when. The moment lives on this table,
/// and since it asks the same question as the other tables, the column names differ but the shape is the same.
pub fn merge_in_effect_at(alias: &str, as_of: Option<usize>) -> String {
    match as_of {
        None => format!("{alias}.reverted_at IS NULL"),
        Some(param) => format!(
            "{alias}.created_at <= coalesce(${param}, now()) \
             AND ({alias}.reverted_at IS NULL OR {alias}.reverted_at > coalesce(${param}, now()))"
        ),
    }
}

/// Whether an entity was an independent node at T: it already existed then, and had not been swallowed by a merge in effect.
///
/// **Replaces `merged_into IS NULL` on read paths.** With `None` the two are equivalent (a revoked merge
/// is not in effect now, so that entity should appear), but once a moment is passed, an entity merged away in March
/// grows back in February, which is exactly what this slice wants.
pub fn entity_visible_at(alias: &str, as_of: Option<usize>) -> String {
    let merged = merge_in_effect_at("m", as_of);
    match as_of {
        None => format!(
            "NOT EXISTS (SELECT 1 FROM entity_merges m \
                          WHERE m.source_id = {alias}.id AND {merged})"
        ),
        Some(param) => format!(
            "{alias}.created_at <= coalesce(${param}, now()) \
             AND NOT EXISTS (SELECT 1 FROM entity_merges m \
                          WHERE m.source_id = {alias}.id AND {merged})"
        ),
    }
}

/// A fact's subject (`on_object = false`) or object at moment T.
///
/// **The present path does not go through the function**: wrapping the columns with `fact_owner_at` would make the index unusable, and
/// "now" is the path every graph drawing takes. Only replay pays this cost, which is rare anyway.
pub fn owner_at(fact_alias: &str, column: &str, as_of: Option<usize>, on_object: bool) -> String {
    match as_of {
        None => format!("{fact_alias}.{column}"),
        Some(param) => {
            format!("fact_owner_at({fact_alias}.id, {fact_alias}.{column}, ${param}, {on_object})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The "now" path must reduce to a single branch. This is not a style matter: `IS NULL OR > now()` stops
    /// the optimizer proving `invalidated_at IS NULL`, so partial indexes such as `facts_live_subject_idx`
    /// become unusable across the whole tree (measured: the two-branch form scans `facts_live_time_idx` at 9,877 rows each time,
    /// the single branch uses BitmapOr and scans 4 rows). This performance property has no guard elsewhere; only here.
    #[test]
    fn a_predicate_for_now_has_no_second_branch() {
        for (what, sql) in [
            ("facts_held_at", facts_held_at("f", None)),
            ("derived_held_at", derived_held_at("d", None)),
            ("violation_open_at", violation_open_at("v", None)),
            ("conflict_open_at", conflict_open_at("c", None)),
            ("document_live_at", document_live_at("d", None)),
            ("chunk_live_at", chunk_live_at("c", None)),
            ("merge_in_effect_at", merge_in_effect_at("m", None)),
        ] {
            assert!(
                !sql.contains(" OR "),
                "{what} 在「现在」下还留着双支：{sql}"
            );
            assert!(
                !sql.contains('$'),
                "{what} 在「现在」下不该有绑定参数：{sql}"
            );
            assert!(
                !sql.contains("now()"),
                "{what} 在「现在」下不该比时刻：{sql}"
            );
        }
    }

    /// The replay path is kept as-is: it takes the moment argument and keeps both branches. Drop either branch and March's graph would
    /// gain rows invalidated only today, or lose rows that were still alive then.
    #[test]
    fn a_predicate_for_a_moment_keeps_both_branches() {
        let sql = facts_held_at("f", Some(3));
        assert!(
            sql.contains("f.recorded_at <= coalesce($3, now())"),
            "{sql}"
        );
        assert!(
            sql.contains("f.invalidated_at IS NULL OR f.invalidated_at > coalesce($3, now())"),
            "{sql}"
        );
    }

    /// `entity_visible_at(None)` replaces `merged_into IS NULL` on read paths: it becomes
    /// an anti join against `entity_merges` (one extra hash anti join), but no longer judges by the entity row.
    /// Only when a moment is passed does it additionally require "it had already been created by then".
    #[test]
    fn an_entity_is_visible_now_unless_a_live_merge_swallowed_it() {
        let now = entity_visible_at("e", None);
        assert!(now.starts_with("NOT EXISTS"), "{now}");
        assert!(now.contains("m.reverted_at IS NULL"), "{now}");
        assert!(!now.contains("e.created_at"), "{now}");
        assert!(entity_visible_at("e", Some(2)).contains("e.created_at <= coalesce($2, now())"));
    }
}
