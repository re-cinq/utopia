//! A statement carrying only one date, bound to a state attribute (#966). The date of "Lin Zhao joined Meridian Systems on 2023-06-01"
//! is written at both ends, following the way 0031 writes events by time resolution; copying that over once bound to works_for (a state) gives a span
//! 2023-06-01 → 2023-06-01. The world axis reads a state as `from <= T < to`, so this span holds at no moment.
//! The binding now says what that date marks (`phrase_bindings.marks`):
//!
//! - start: from that day. Whether "joined" comes before, after, or is the only one with "works for … from 2023-06-01"
//!   (A, B, C in #966), it is a state from 2023-06-01, open until now;
//! - end: up to that day, closing the open span. An end that states a period ("left in 2024") while the open span began within
//!   the period closes at the end of the period;
//! - none, or not yet stated: the relation is not a typed row, and the statement stays in the open graph with its date; the attribute is read as a period named by that date
//!   (0053 revision 2026-10-22, `materialize::period_tests`).
//!
//! An empty span copied over earlier is voided and recomputed by materialization; rows a person wrote, or changed to a moment, are not touched. Under an attribute declared as a state, both ends
//! equal: writing, changing the interval, and closing at its own start are all refused (a same-start conflict also cannot be closed at that moment); rows without an attribute
//! stay as before; an existing empty span is not merged with later observations, nor closed by them; a correction cannot fix a span that holds at no time, nor turn
//! a moment event into a state; rules have no marks, so a statement that states only a moment does not imply a state, and one with only a start stays as before.
//!
//! An end moved to the end of a period therefore records that the source said that period (`end_after_period`, #994): both the closing and the refining paths record it; a row closed at "until 2025" that is later read as "left in 2024" also records it (as does one a person changed only the start of); when the end drawn on the timeline is stated by the source it records it too; rows carried by moving subject/object or by a correction carry it, and one changed to an event does not.
//! Both paths record it; a row closed at "until 2025" that is later read as "left in 2024" also records it (as does one a person changed only the start of), and the end drawn on the timeline
//! the end drawn on the timeline is recorded when the source says it; rows produced by swapping subject and object or by an erratum carry it, and rows changed into events do not.
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::PgPool;
use utopia_store::errata::{self, ActionInput, Candidate, Proposed};
use utopia_store::graph::{self, Validity};
use utopia_store::materialize::{materialize, Outcome};
use uuid::Uuid;

/// One typed fact row: start, end, and whether it was computed by a rule
type ImpliedRow = (Option<DateTime<Utc>>, Option<DateTime<Utc>>, bool);

fn t(s: &str) -> DateTime<Utc> {
    s.parse().expect("fixed timestamp")
}

/// 2023-06-01, day precision
fn day() -> DateTime<Utc> {
    t("2023-06-01T00:00:00Z")
}

/// Both ends are that same day: the shape time resolution writes for a moment
fn moment() -> Validity<'static> {
    Validity {
        from: Some(day()),
        from_precision: Some("day"),
        to: Some(day()),
        to_precision: Some("day"),
        attested_at: None,
        from_grade: None,
        ..Default::default()
    }
}

/// Only the end is known
fn ending(at: DateTime<Utc>) -> Validity<'static> {
    Validity {
        from: None,
        from_precision: None,
        to: Some(at),
        to_precision: Some("day"),
        attested_at: None,
        from_grade: None,
        ..Default::default()
    }
}

/// One store: person, organization; works_for (state), acquired (event); Lin Zhao,
/// Meridian Systems, Aster Labs
struct Base {
    pool: PgPool,
    org: Uuid,
    kb: Uuid,
    person: Uuid,
    company: Uuid,
    works_for: Uuid,
    acquired: Uuid,
    lin: Uuid,
    meridian: Uuid,
    aster: Uuid,
}

impl Base {
    async fn new(pool: &PgPool) -> anyhow::Result<Self> {
        let ids: Vec<Uuid> = (0..10).map(|_| Uuid::now_v7()).collect();
        let (org, ws, kb, person, company) = (ids[0], ids[1], ids[2], ids[3], ids[4]);
        let (works_for, acquired, lin, meridian, aster) = (ids[5], ids[6], ids[7], ids[8], ids[9]);
        // Only locally generated UUIDs are interpolated into fixture SQL.
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations (id, name) VALUES ('{org}', 'a-moment-marks-its-state');
             INSERT INTO workspaces (id, org_id, name) VALUES ('{ws}', '{org}', 'a-moment-marks-its-state');
             INSERT INTO knowledge_bases (id, workspace_id, name)
                  VALUES ('{kb}', '{ws}', 'a-moment-marks-its-state');
             INSERT INTO entity_types (id, kb_id, key, label, color, shape) VALUES
                 ('{person}', '{kb}', 'person', 'person', '#7fd0ff', 'circle'),
                 ('{company}', '{kb}', 'organization', 'organization', '#7fd0ff', 'circle');
             INSERT INTO relation_types (id, kb_id, key, label, temporal) VALUES
                 ('{works_for}', '{kb}', 'works_for', 'works for', 'state'),
                 ('{acquired}', '{kb}', 'acquired', 'acquired', 'event');
             INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES
                 ('{lin}', '{kb}', '{person}', 'Lin Zhao'),
                 ('{meridian}', '{kb}', '{company}', 'Meridian Systems'),
                 ('{aster}', '{kb}', '{company}', 'Aster Labs');"
        ))
        .execute(pool)
        .await?;
        Ok(Self {
            pool: pool.clone(),
            org,
            kb,
            person,
            company,
            works_for,
            acquired,
            lin,
            meridian,
            aster,
        })
    }

    /// One open statement Lin Zhao —phrase→ Meridian Systems, given the shape time resolution writes for both ends: a moment has
    /// the same value at both ends; "from" has only a start, "until" has only an end
    async fn statement(
        &self,
        phrase: &str,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Uuid> {
        self.statement_in(phrase, from, to, "day").await
    }

    /// Same as [`Base::statement`], with both ends at this precision ("left in 2024" is a moment at year precision)
    async fn statement_in(
        &self,
        phrase: &str,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        precision: &str,
    ) -> anyhow::Result<Uuid> {
        self.statement_to(phrase, self.meridian, from, to, precision)
            .await
    }

    /// Same as [`Base::statement_in`], with this one object
    async fn statement_to(
        &self,
        phrase: &str,
        object: Uuid,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        precision: &str,
    ) -> anyhow::Result<Uuid> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, object_id, layer, phrase, confidence,
                                valid_from, valid_from_precision, valid_from_grade,
                                valid_to, valid_to_precision)
             VALUES ($1, $2, $3, $4, 'open', $5, 0.9,
                     $6, CASE WHEN $6 IS NULL THEN NULL ELSE $8 END,
                     CASE WHEN $6 IS NULL THEN NULL ELSE 'A' END,
                     $7, CASE WHEN $7 IS NULL THEN NULL ELSE $8 END)",
        )
        .bind(id)
        .bind(self.kb)
        .bind(self.lin)
        .bind(object)
        .bind(phrase)
        .bind(from)
        .bind(to)
        .bind(precision)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Signature (phrase, person, organization) bound to works_for, in the original direction, saying what that date marks
    async fn bind(&self, phrase: &str, marks: Option<&str>) -> anyhow::Result<()> {
        self.bind_to(phrase, self.works_for, marks).await
    }

    /// Same as [`Base::bind`], bound to this attribute
    async fn bind_to(
        &self,
        phrase: &str,
        property: Uuid,
        marks: Option<&str>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO phrase_bindings
                 (id, kb_id, phrase, subject_type_id, object_type_id, object_is_value,
                  relation_type_id, direction, status, marks)
             VALUES ($1, $2, $3, $4, $5, false, $6, 'forward', 'bound', $7)
             ON CONFLICT (kb_id, phrase, subject_type_id, object_type_id, object_is_value)
             DO UPDATE SET marks = EXCLUDED.marks",
        )
        .bind(Uuid::now_v7())
        .bind(self.kb)
        .bind(phrase)
        .bind(self.person)
        .bind(self.company)
        .bind(property)
        .bind(marks)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A typed Lin Zhao —works_for→ object row, with both ends that day: the shape copied over before #966.
    /// If `statement` is given, it is what materialization computed (`from_statement_id`); if not, it was written by a person
    async fn empty_row(&self, object: Uuid, statement: Option<Uuid>) -> anyhow::Result<Uuid> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, layer,
                                valid_from, valid_from_precision, valid_to, valid_to_precision,
                                confidence, from_statement_id)
             VALUES ($1, $2, $3, $4, $5, 'typed', $6, 'day', $6, 'day', 0.9, $7)",
        )
        .bind(id)
        .bind(self.kb)
        .bind(self.lin)
        .bind(self.works_for)
        .bind(object)
        .bind(day())
        .bind(statement)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Live typed rows between Lin Zhao and Meridian: (start, end, number of sources), sorted by start
    async fn rows(
        &self,
    ) -> anyhow::Result<Vec<(Option<DateTime<Utc>>, Option<DateTime<Utc>>, i64)>> {
        Ok(sqlx::query_as(
            "SELECT t.valid_from, t.valid_to,
                    (SELECT count(*) FROM typed_fact_sources s WHERE s.fact_id = t.id)
               FROM facts t
              WHERE t.kb_id = $1 AND t.layer = 'typed' AND t.invalidated_at IS NULL
                AND t.object_id = $2
              ORDER BY t.valid_from NULLS FIRST, t.valid_to NULLS LAST",
        )
        .bind(self.kb)
        .bind(self.meridian)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Who works_for Meridian at a given moment on the world axis
    async fn at_meridian(&self, at: &str) -> anyhow::Result<Vec<String>> {
        let (nodes, edges) =
            graph::neighborhood(&self.pool, self.kb, self.meridian, 1, Some(t(at)), None).await?;
        Ok(edges
            .iter()
            .filter(|e| e.target == self.meridian && e.predicate.as_deref() == Some("works_for"))
            .filter_map(|e| nodes.iter().find(|n| n.id == e.source))
            .map(|n| n.name.clone())
            .collect())
    }

    /// Edges on Meridian ignoring the world axis: (id, start, end)
    async fn edges(
        &self,
    ) -> anyhow::Result<Vec<(Uuid, Option<DateTime<Utc>>, Option<DateTime<Utc>>)>> {
        let (_, edges) =
            graph::neighborhood(&self.pool, self.kb, self.meridian, 1, None, None).await?;
        Ok(edges
            .iter()
            .filter(|e| e.target == self.meridian)
            .map(|e| (e.id, e.valid_from, e.valid_to))
            .collect())
    }

    async fn live(&self, fact: Uuid) -> anyhow::Result<(bool, Option<DateTime<Utc>>)> {
        Ok(
            sqlx::query_as("SELECT invalidated_at IS NULL, valid_to FROM facts WHERE id = $1")
                .bind(fact)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Lin Zhao —works_for→ object, through the write entry point (extraction, confirmation, and person-written facts share one gate)
    async fn write_to(&self, object: Uuid, v: Validity<'_>) -> anyhow::Result<Uuid> {
        let (fact, _) = graph::insert_fact(
            &self.pool,
            self.kb,
            self.lin,
            Some(self.works_for),
            object,
            v,
            0.9,
        )
        .await?;
        Ok(fact)
    }

    async fn remove(self) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(self.org)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

async fn pool() -> anyhow::Result<Option<PgPool>> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(None);
    };
    Ok(Some(PgPool::connect(&url).await?))
}

fn refused_as_empty(e: &utopia_core::AppError) -> bool {
    format!("{e:?}").contains("empty_state_span")
}

/// The three orders of #966: "joined" (a moment) before "works for … from" (from), after it, or the only one.
/// The binding says what "joined" marks is the start: all three are a state from 2023-06-01, open until now
#[tokio::test]
async fn a_date_that_starts_a_state_opens_it_whatever_the_order() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let cases: [(&str, &[&str]); 3] = [
        ("A", &["joined", "works for"]),
        ("B", &["works for", "joined"]),
        ("C", &["joined"]),
    ];
    for (label, phrases) in cases {
        let base = Base::new(&pool).await?;
        let run = async {
            for phrase in phrases {
                let point = *phrase == "joined";
                base.statement(phrase, Some(day()), point.then(day)).await?;
                base.bind(phrase, point.then_some("start")).await?;
            }
            materialize(&pool, base.kb).await?;
            assert_eq!(
                base.rows().await?,
                vec![(Some(day()), None, phrases.len() as i64)],
                "case {label}: one row from 2023-06-01, every statement its source"
            );
            for at in [
                "2023-06-01T12:00:00Z",
                "2023-07-01T00:00:00Z",
                "2026-01-01T00:00:00Z",
            ] {
                assert_eq!(
                    base.at_meridian(at).await?,
                    vec!["Lin Zhao".to_string()],
                    "case {label} at {at}"
                );
            }
            assert_eq!(
                materialize(&pool, base.kb).await?,
                Outcome::default(),
                "case {label}: a second run changes nothing"
            );
            anyhow::Ok(())
        }
        .await;
        base.remove().await?;
        run?;
    }
    Ok(())
}

/// Phrase marking the end: "left … on 2025-01-31" closes the open span on that day. It does not matter which sentence is read first
#[tokio::test]
async fn a_date_that_ends_a_state_closes_it_there() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let left = t("2025-01-31T00:00:00Z");
    for order in [["works for", "left"], ["left", "works for"]] {
        let base = Base::new(&pool).await?;
        let run = async {
            for phrase in order {
                if phrase == "left" {
                    base.statement(phrase, Some(left), Some(left)).await?;
                } else {
                    base.statement(phrase, Some(day()), None).await?;
                }
            }
            base.bind("works for", None).await?;
            base.bind("left", Some("end")).await?;
            materialize(&pool, base.kb).await?;
            assert_eq!(
                base.rows().await?,
                vec![(Some(day()), Some(left), 2)],
                "{order:?}: one span, closed where he left"
            );
            assert_eq!(
                base.at_meridian("2024-06-01T00:00:00Z").await?,
                vec!["Lin Zhao".to_string()],
                "{order:?}"
            );
            assert!(
                base.at_meridian("2025-06-01T00:00:00Z").await?.is_empty(),
                "{order:?}: not after he left"
            );
            assert_eq!(materialize(&pool, base.kb).await?, Outcome::default());
            anyhow::Ok(())
        }
        .await;
        base.remove().await?;
        run?;
    }
    Ok(())
}

/// A date marked "neither", and a date the binding has not yet said what to mark: not a typed row. The statement keeps its date
/// in the open graph, and it still appears on the canvas
#[tokio::test]
async fn a_date_that_marks_neither_or_nothing_yet_computes_no_row() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    for marks in [Some("none"), None] {
        let base = Base::new(&pool).await?;
        let run = async {
            let seen = base
                .statement("was employed by", Some(day()), Some(day()))
                .await?;
            base.bind("was employed by", marks).await?;
            assert_eq!(
                materialize(&pool, base.kb).await?,
                Outcome::default(),
                "{marks:?}"
            );
            assert!(base.rows().await?.is_empty(), "{marks:?}: no typed row");
            assert_eq!(
                base.edges().await?,
                vec![(seen, Some(day()), Some(day()))],
                "{marks:?}: the statement stays in the open graph with its date"
            );
            anyhow::Ok(())
        }
        .await;
        base.remove().await?;
        run?;
    }
    Ok(())
}

/// An empty span copied over earlier (#966's A): materialization voids it (it remains in the table), and the statement is recomputed by the binding — "works for
/// … from" becomes a state from that day; the "joined" binding has not said what to mark, so it stays in the open graph; once the binding says
/// start, it merges into that span. A person-written empty span is not touched
#[tokio::test]
async fn a_row_that_held_at_no_moment_is_computed_again() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = Base::new(&pool).await?;
    let run = async {
        let joined = base.statement("joined", Some(day()), Some(day())).await?;
        let works = base.statement("works for", Some(day()), None).await?;
        base.bind("joined", None).await?;
        base.bind("works for", None).await?;
        let computed = base.empty_row(base.meridian, Some(joined)).await?;
        sqlx::query(
            "INSERT INTO typed_fact_sources (fact_id, statement_id) VALUES ($1, $2), ($1, $3)",
        )
        .bind(computed)
        .bind(joined)
        .bind(works)
        .execute(&pool)
        .await?;
        let by_hand = base.empty_row(base.aster, None).await?;
        assert!(
            base.at_meridian("2023-07-01T00:00:00Z").await?.is_empty(),
            "the ledger as #966 found it: nobody works for Meridian"
        );

        assert_eq!(
            materialize(&pool, base.kb).await?,
            Outcome {
                retired: 1,
                added: 1,
                ..Outcome::default()
            }
        );
        assert_eq!(
            base.live(computed).await?,
            (false, Some(day())),
            "retired, still in the table"
        );
        assert_eq!(base.rows().await?, vec![(Some(day()), None, 1)]);
        assert_eq!(
            base.at_meridian("2023-07-01T00:00:00Z").await?,
            vec!["Lin Zhao".to_string()]
        );
        let edges = base.edges().await?;
        assert!(
            edges.iter().any(|(id, _, _)| *id == joined),
            "joined has no typed row yet, so it shows as itself: {edges:?}"
        );
        assert!(!edges.iter().any(|(id, _, _)| *id == works), "{edges:?}");
        assert_eq!(
            base.live(by_hand).await?,
            (true, Some(day())),
            "a row a person wrote is not touched"
        );

        base.bind("joined", Some("start")).await?;
        assert_eq!(
            materialize(&pool, base.kb).await?,
            Outcome {
                merged: 1,
                ..Outcome::default()
            }
        );
        assert_eq!(base.rows().await?, vec![(Some(day()), None, 2)]);
        assert_eq!(materialize(&pool, base.kb).await?, Outcome::default());
        anyhow::Ok(())
    }
    .await;
    base.remove().await?;
    run
}

/// Invariant: under an attribute declared as a state, rows with equal ends cannot be written — refused at writing (also those that become equal only after truncation to precision),
/// at changing the interval, and at closing at their own start; the original row does not change. Events are still written as a moment; rows with no attribute still carry their date
#[tokio::test]
async fn a_state_that_ends_where_it_starts_is_never_written() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = Base::new(&pool).await?;
    let run = async {
        let (kb, lin, meridian, aster) = (base.kb, base.lin, base.meridian, base.aster);
        let refused = graph::insert_fact(
            &pool,
            kb,
            lin,
            Some(base.works_for),
            meridian,
            moment(),
            0.9,
        )
        .await
        .expect_err("an empty state span");
        assert!(refused_as_empty(&refused), "{refused:?}");
        let same_day = Validity {
            from: Some(t("2023-06-01T09:00:00Z")),
            to: Some(t("2023-06-01T17:00:00Z")),
            ..moment()
        };
        let refused = graph::insert_fact(
            &pool,
            kb,
            lin,
            Some(base.works_for),
            meridian,
            same_day,
            0.9,
        )
        .await
        .expect_err("equal once truncated to the day");
        assert!(refused_as_empty(&refused), "{refused:?}");
        graph::insert_fact(
            &pool,
            kb,
            meridian,
            Some(base.acquired),
            aster,
            moment(),
            0.9,
        )
        .await?;
        let (bare, _) = graph::insert_fact(&pool, kb, lin, None, aster, moment(), 0.9).await?;
        let (_, to) = base.live(bare).await?;
        assert_eq!(to, Some(day()), "a row without a property keeps its date");

        let (open, _) = graph::insert_fact(
            &pool,
            kb,
            lin,
            Some(base.works_for),
            meridian,
            Validity::starting(Some(day()), Some("day")),
            0.9,
        )
        .await?;
        let refused = utopia_store::temporal::correct_interval(&pool, open, moment())
            .await
            .expect_err("corrected into an empty span");
        assert!(refused_as_empty(&refused), "{refused:?}");
        let refused = utopia_store::temporal::close_superseded(&pool, open, day(), "day")
            .await
            .expect_err("closed where it starts");
        assert!(refused_as_empty(&refused), "{refused:?}");
        assert_eq!(
            base.live(open).await?,
            (true, None),
            "the open row is untouched"
        );
        assert!(
            utopia_store::temporal::close_superseded(&pool, open, t("2024-01-01T00:00:00Z"), "day")
                .await?
                .is_some(),
            "closing after the start still works"
        );
        let (unnamed, _) = graph::insert_fact(
            &pool,
            kb,
            lin,
            None,
            meridian,
            Validity::starting(Some(day()), Some("day")),
            0.9,
        )
        .await?;
        assert!(
            utopia_store::temporal::correct_interval(&pool, unnamed, moment())
                .await?
                .is_some(),
            "a row without a property can be given a single date"
        );
        anyhow::Ok(())
    }
    .await;
    base.remove().await?;
    run
}

/// Guard: an empty span written earlier (here person-written, so materialization does not void it) does not merge later observations of "from that day onward" into itself,
/// nor is it closed by observations of "up to that day". A state that starts and ends on the same day is not written as an empty span, whichever sentence is read first,
/// and the open span does not stay open forever: it is closed at the end of that day
#[tokio::test]
async fn a_row_that_holds_at_no_moment_neither_absorbs_nor_closes() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = Base::new(&pool).await?;
    let run = async {
        let (kb, lin, meridian) = (base.kb, base.lin, base.meridian);
        let empty = base.empty_row(meridian, None).await?;
        let since = Validity::starting(Some(day()), Some("day"));
        let (open, created) =
            graph::insert_fact(&pool, kb, lin, Some(base.works_for), meridian, since, 0.9).await?;
        assert!(
            created && open != empty,
            "a new row, not merged into the empty one"
        );
        let left = t("2025-01-31T00:00:00Z");
        graph::insert_fact(
            &pool,
            kb,
            lin,
            Some(base.works_for),
            meridian,
            ending(left),
            0.9,
        )
        .await?;
        assert_eq!(
            base.rows().await?,
            vec![(Some(day()), Some(day()), 0), (Some(day()), Some(left), 0)],
            "the open row is the one that closes"
        );
        assert_eq!(base.live(empty).await?, (true, Some(day())));
        anyhow::Ok(())
    }
    .await;
    base.remove().await?;
    run?;

    for start_first in [true, false] {
        let base = Base::new(&pool).await?;
        let run = async {
            let (kb, lin, meridian) = (base.kb, base.lin, base.meridian);
            let since = Validity::starting(Some(day()), Some("day"));
            let writes = if start_first {
                [since, ending(day())]
            } else {
                [ending(day()), since]
            };
            for v in writes {
                graph::insert_fact(&pool, kb, lin, Some(base.works_for), meridian, v, 0.9).await?;
            }
            assert_eq!(
                base.rows().await?,
                vec![(Some(day()), Some(t("2023-06-02T00:00:00Z")), 0)],
                "start first: {start_first}: one row, through that day"
            );
            anyhow::Ok(())
        }
        .await;
        base.remove().await?;
        run?;
    }
    Ok(())
}

/// A correction to an earlier-written empty span: the new row cannot hold a span that holds at no time. Refused at validation; for the one left to a person before the upgrade,
/// approving it does not void the old row first
#[tokio::test]
async fn a_revision_cannot_take_a_time_that_holds_at_no_moment() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = Base::new(&pool).await?;
    let run = async {
        let (kb, lin, meridian, aster) = (base.kb, base.lin, base.meridian, base.aster);
        let text = "Lin Zhao joined Aster Labs on 2023-06-01.";
        let (doc, chunk) = (Uuid::now_v7(), Uuid::now_v7());
        sqlx::query(
            "INSERT INTO documents (id, kb_id, filename, sha256) VALUES ($1, $2, 'offer.txt', 'x')",
        )
        .bind(doc)
        .bind(kb)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, $4)",
        )
        .bind(chunk)
        .bind(kb)
        .bind(doc)
        .bind(text)
        .execute(&pool)
        .await?;
        let empty = base.empty_row(meridian, None).await?;
        let run_id = errata::start_run(&pool, kb, doc, 1, 0).await?;
        let candidate = Candidate {
            fact_id: empty,
            statement_id: None,
            predicate_id: base.works_for,
            subject_id: lin,
            subject: "Lin Zhao".into(),
            subject_class: Some("person".into()),
            property: "works_for".into(),
            property_label: "works for".into(),
            object_id: Some(meridian),
            object: "Meridian Systems".into(),
            object_class: Some("organization".into()),
            flag: Some("name_absent".into()),
            quote: None,
        };
        let recorded = errata::record(
            &pool,
            kb,
            ActionInput {
                run_id,
                document_id: doc,
                candidate: Some(&candidate),
                proposed: Proposed::Revise {
                    property: None,
                    object: Some("Aster Labs".into()),
                },
                reason: "the document names Aster Labs",
                quote: Some("Lin Zhao joined Aster Labs"),
                document_text: text,
            },
        )
        .await?;
        assert_eq!(
            (recorded.status, recorded.detail.as_deref()),
            (
                "refused",
                Some("a state cannot take a time whose start and end are equal")
            )
        );
        assert_eq!(base.live(empty).await?, (true, Some(day())));

        let held = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO errata_actions (id, kb_id, run_id, document_id, fact_id, predicate_id,
                                         action, reason, quote, proposed, status)
             VALUES ($1, $2, $3, $4, $5, $6, 'revise', 'held before the upgrade',
                     'Lin Zhao joined Aster Labs', $7, 'held')",
        )
        .bind(held)
        .bind(kb)
        .bind(run_id)
        .bind(doc)
        .bind(empty)
        .bind(base.works_for)
        .bind(json!({
            "subject": "Lin Zhao", "property": "works_for", "object": "Aster Labs",
            "subject_id": lin, "predicate_id": base.works_for, "object_id": aster,
            "object_value": null,
        }))
        .execute(&pool)
        .await?;
        let refused = errata::decide_held(&pool, kb, held, true, Uuid::now_v7())
            .await
            .expect_err("the revision cannot be written");
        assert!(refused_as_empty(&refused), "{refused:?}");
        assert_eq!(
            base.live(empty).await?,
            (true, Some(day())),
            "the old row is not retracted first"
        );
        let status: String = sqlx::query_scalar("SELECT status FROM errata_actions WHERE id = $1")
            .bind(held)
            .fetch_one(&pool)
            .await?;
        assert_eq!(status, "held");

        // A moment event changed into a state attribute: the new row would be a span that holds at no time, likewise refused
        let (acquisition, _) = graph::insert_fact(
            &pool,
            kb,
            meridian,
            Some(base.acquired),
            aster,
            moment(),
            0.9,
        )
        .await?;
        let event = Candidate {
            fact_id: acquisition,
            predicate_id: base.acquired,
            subject_id: meridian,
            subject: "Meridian Systems".into(),
            subject_class: Some("organization".into()),
            property: "acquired".into(),
            property_label: "acquired".into(),
            object_id: Some(aster),
            object: "Aster Labs".into(),
            ..candidate
        };
        let recorded = errata::record(
            &pool,
            kb,
            ActionInput {
                run_id,
                document_id: doc,
                candidate: Some(&event),
                proposed: Proposed::Revise {
                    property: Some("works_for".into()),
                    object: None,
                },
                reason: "the document says an employment",
                quote: Some("Lin Zhao joined Aster Labs"),
                document_text: text,
            },
        )
        .await?;
        assert_eq!(
            (recorded.status, recorded.detail.as_deref()),
            (
                "refused",
                Some("a state cannot take a time whose start and end are equal")
            )
        );
        assert_eq!(base.live(acquisition).await?, (true, Some(day())));
        anyhow::Ok(())
    }
    .await;
    base.remove().await?;
    run
}

/// The rule has no marks: a statement with only a start still implies a state from that day; a statement stating only a moment, when implying a state,
/// has an unknown reading, so no implied row is created
#[tokio::test]
async fn a_rule_reads_a_start_date_but_not_a_single_date() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = Base::new(&pool).await?;
    let run = async {
        base.statement("was hired by", Some(day()), None).await?;
        base.statement("signed with", Some(day()), Some(day()))
            .await?;
        for phrase in ["was hired by", "signed with"] {
            sqlx::query(
                "INSERT INTO implication_rules
                     (id, kb_id, trigger, phrase, subject_type_id, object_type_id, object_is_value,
                      conclude_property_id, status, decided_by)
                 VALUES ($1, $2, 'phrase', $3, $4, $5, false, $6, 'approved', 'person')",
            )
            .bind(Uuid::now_v7())
            .bind(base.kb)
            .bind(phrase)
            .bind(base.person)
            .bind(base.company)
            .bind(base.works_for)
            .execute(&pool)
            .await?;
        }
        let outcome = materialize(&pool, base.kb).await?;
        assert_eq!(outcome.implied, 1, "{outcome:?}");
        let rows: Vec<ImpliedRow> = sqlx::query_as(
            "SELECT valid_from, valid_to, implied FROM facts
              WHERE kb_id = $1 AND layer = 'typed' AND invalidated_at IS NULL",
        )
        .bind(base.kb)
        .fetch_all(&pool)
        .await?;
        assert_eq!(rows, vec![(Some(day()), None, true)]);
        anyhow::Ok(())
    }
    .await;
    base.remove().await?;
    run
}

/// A row a person changed to equal start and end (#975 review): it carries from_statement_id, implied and the source link, and looks the same as one
/// computed from a moment copied over. But its source statement is not that moment, so materialization does not void it. A copied-over implied row from a moment is voided,
/// and an implied row a person has changed is not touched
#[tokio::test]
async fn a_row_a_person_set_to_a_single_date_is_not_retired() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = Base::new(&pool).await?;
    let run = async {
        // "works for … from" yields one row; "was hired by … from" yields one row through a rule, with a different organization as object,
        // so the two do not merge; "signed with" states only a moment, and before #966 the rule copied an empty span
        let works = base.statement("works for", Some(day()), None).await?;
        base.bind("works for", None).await?;
        let hired = base
            .statement_to("was hired by", base.aster, Some(day()), None, "day")
            .await?;
        let signed = base
            .statement("signed with", Some(day()), Some(day()))
            .await?;
        let mut rules = Vec::new();
        for phrase in ["was hired by", "signed with"] {
            let rule = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO implication_rules
                     (id, kb_id, trigger, phrase, subject_type_id, object_type_id, object_is_value,
                      conclude_property_id, status, decided_by)
                 VALUES ($1, $2, 'phrase', $3, $4, $5, false, $6, 'approved', 'person')",
            )
            .bind(rule)
            .bind(base.kb)
            .bind(phrase)
            .bind(base.person)
            .bind(base.company)
            .bind(base.works_for)
            .execute(&pool)
            .await?;
            rules.push(rule);
        }
        materialize(&pool, base.kb).await?;
        let row_of = |statement: Uuid, table: &'static str| {
            let pool = pool.clone();
            async move {
                let id: Uuid = sqlx::query_scalar(&format!(
                    "SELECT fact_id FROM {table} WHERE statement_id = $1"
                ))
                .bind(statement)
                .fetch_one(&pool)
                .await?;
                anyhow::Ok(id)
            }
        };
        let typed = row_of(works, "typed_fact_sources").await?;
        let implied = row_of(hired, "implied_fact_sources").await?;
        assert_ne!(typed, implied);
        // A person changes both rows to that day: before the invariant existed, `correct_interval` wrote them this way, and the row carries its source
        sqlx::query(
            "UPDATE facts SET valid_to = valid_from, valid_to_precision = valid_from_precision
              WHERE id = ANY($1)",
        )
        .bind(vec![typed, implied])
        .execute(&pool)
        .await?;
        let copied = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, layer,
                                valid_from, valid_from_precision, valid_to, valid_to_precision,
                                confidence, implied)
             VALUES ($1, $2, $3, $4, $5, 'typed', $6, 'day', $6, 'day', 0.9, true)",
        )
        .bind(copied)
        .bind(base.kb)
        .bind(base.lin)
        .bind(base.works_for)
        .bind(base.meridian)
        .bind(day())
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO implied_fact_sources (fact_id, rule_id, statement_id) VALUES ($1, $2, $3)",
        )
        .bind(copied)
        .bind(rules[1])
        .bind(signed)
        .execute(&pool)
        .await?;

        let outcome = materialize(&pool, base.kb).await?;
        assert_eq!(
            (outcome.retired, outcome.added, outcome.implied),
            (1, 0, 0),
            "{outcome:?}"
        );
        assert_eq!(
            base.live(copied).await?,
            (false, Some(day())),
            "copied: retired"
        );
        assert_eq!(
            base.live(typed).await?,
            (true, Some(day())),
            "a typed row a person set to a single date is kept"
        );
        assert_eq!(
            base.live(implied).await?,
            (true, Some(day())),
            "an implied row a person set to a single date is kept"
        );
        assert_eq!(materialize(&pool, base.kb).await?, Outcome::default());
        anyhow::Ok(())
    }
    .await;
    base.remove().await?;
    run
}

/// The end states a period, but the comparison is on a moment (#975 review): "left … in 2024" is stored as 2024-01-01, year precision.
/// "works for … from 2024-03-01" starts within that period and closes at the period's end 2025-01-01, rather than forming a separate row
/// `- → 2024` that stays open past 2025. It does not matter which sentence is read first; the row records that the end stated 2024
/// (#994: both the closing path and the refining path record it)
#[tokio::test]
async fn a_coarse_end_closes_a_row_that_starts_inside_its_period() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let (march, year, next_year) = (
        t("2024-03-01T00:00:00Z"),
        t("2024-01-01T00:00:00Z"),
        t("2025-01-01T00:00:00Z"),
    );
    let mut marks: Vec<Vec<bool>> = Vec::new();
    for order in [["works for", "left"], ["left", "works for"]] {
        let base = Base::new(&pool).await?;
        let run = async {
            for phrase in order {
                if phrase == "left" {
                    base.statement_in(phrase, Some(year), Some(year), "year")
                        .await?;
                } else {
                    base.statement(phrase, Some(march), None).await?;
                }
            }
            base.bind("works for", None).await?;
            base.bind("left", Some("end")).await?;
            materialize(&pool, base.kb).await?;
            assert_eq!(
                base.rows().await?,
                vec![(Some(march), Some(next_year), 2)],
                "{order:?}: one span, closed at the end of 2024"
            );
            assert_eq!(
                base.at_meridian("2024-06-01T00:00:00Z").await?,
                vec!["Lin Zhao".to_string()],
                "{order:?}"
            );
            assert!(
                base.at_meridian("2025-06-01T00:00:00Z").await?.is_empty(),
                "{order:?}: not still working there in 2025"
            );
            assert_eq!(materialize(&pool, base.kb).await?, Outcome::default());
            marks.push(
                sqlx::query_scalar(
                    "SELECT end_after_period FROM facts
                      WHERE kb_id = $1 AND layer = 'typed' AND invalidated_at IS NULL",
                )
                .bind(base.kb)
                .fetch_all(&pool)
                .await?,
            );
            anyhow::Ok(())
        }
        .await;
        base.remove().await?;
        run?;
    }
    assert_eq!(marks, [[true], [true]], "the end names 2024, not 2025");
    Ok(())
}

/// For the same period's end, rows starting before the period still close at its beginning; it does not apply to rows that start after the period —
/// that is the end of an earlier span, each on its own row, and the open span stays open as before. It does not matter which sentence is written first
#[tokio::test]
async fn a_coarse_end_leaves_rows_outside_its_period_as_before() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let year = t("2024-01-01T00:00:00Z");
    let left_in_2024 = Validity {
        to_precision: Some("year"),
        ..ending(year)
    };
    let cases = [
        (
            t("2020-01-01T00:00:00Z"),
            vec![(Some(t("2020-01-01T00:00:00Z")), Some(year), 0)],
        ),
        (
            t("2025-06-01T00:00:00Z"),
            vec![
                (None, Some(year), 0),
                (Some(t("2025-06-01T00:00:00Z")), None, 0),
            ],
        ),
    ];
    for (start, expected) in cases {
        for start_first in [true, false] {
            let base = Base::new(&pool).await?;
            let run = async {
                let since = Validity::starting(Some(start), Some("day"));
                let writes = if start_first {
                    [since, left_in_2024]
                } else {
                    [left_in_2024, since]
                };
                for v in writes {
                    graph::insert_fact(
                        &pool,
                        base.kb,
                        base.lin,
                        Some(base.works_for),
                        base.meridian,
                        v,
                        0.9,
                    )
                    .await?;
                }
                assert_eq!(
                    base.rows().await?,
                    expected,
                    "from {start}, start first: {start_first}"
                );
                anyhow::Ok(())
            }
            .await;
            base.remove().await?;
            run?;
        }
    }
    Ok(())
}

/// One end, two phrasings (#994): a row starting 2024-03-01 that a person wrote, closed at "until 2025", and a document that says "left in
/// 2024", the same end: marked and merged into that row, which stays the person's (the reverse order is the closing path); another document saying it again
/// keeps it as it was marked. A row whose start a person moved only to April (Dune) is also marked. 2025-01-01 stated at day precision (Aster)
/// is that day, and an end a person changed (Cobalt) stays the person's; none of these are rewritten
#[tokio::test]
async fn one_end_named_two_ways_is_marked() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = Base::new(&pool).await?;
    let run = async {
        let (year, next_year) = (t("2024-01-01T00:00:00Z"), t("2025-01-01T00:00:00Z"));
        let since = Validity::starting(Some(t("2024-03-01T00:00:00Z")), Some("day"));
        let until_2025 = Validity {
            to_precision: Some("year"),
            ..ending(next_year)
        };
        let (cobalt, dune) = (Uuid::now_v7(), Uuid::now_v7());
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name)
                  VALUES ($1, $2, $3, 'Cobalt'), ($4, $2, $3, 'Dune')",
        )
        .bind(cobalt)
        .bind(base.kb)
        .bind(base.company)
        .bind(dune)
        .execute(&pool)
        .await?;
        for (object, end) in [(base.meridian, until_2025), (base.aster, ending(next_year))] {
            base.write_to(object, since).await?;
            base.write_to(object, end).await?;
        }
        let by_hand = Validity {
            to: Some(next_year),
            to_precision: Some("year"),
            ..since
        };
        let open = base.write_to(cobalt, since).await?;
        utopia_store::temporal::correct_interval(&pool, open, by_hand).await?;
        let stated = base.write_to(dune, by_hand).await?;
        let april = Validity {
            from: Some(t("2024-04-01T00:00:00Z")),
            ..by_hand
        };
        utopia_store::temporal::correct_interval(&pool, stated, april).await?;
        for object in [base.meridian, base.meridian, base.aster, cobalt, dune] {
            base.statement_to("left", object, Some(year), Some(year), "year")
                .await?;
        }
        base.bind("left", Some("end")).await?;
        materialize(&pool, base.kb).await?;
        // Each object's live row: marked, still the person's (not computed from this sentence); that side was superseded several times
        let rows: Vec<(String, bool, bool, i64)> = sqlx::query_as(
            "SELECT o.canonical_name, f.end_after_period, f.from_statement_id IS NULL,
                    (SELECT count(*) FROM facts g WHERE g.layer = 'typed'
                        AND g.object_id = f.object_id AND g.invalidated_at IS NOT NULL)
               FROM facts f JOIN entities o ON o.id = f.object_id
              WHERE f.kb_id = $1 AND f.layer = 'typed' AND f.invalidated_at IS NULL
              ORDER BY 1",
        )
        .bind(base.kb)
        .fetch_all(&pool)
        .await?;
        // The open row for Meridian was first closed by "until 2025", then marked; each superseded once. Dune had its start changed once
        let expected = [
            ("Aster Labs", false, 1),
            ("Cobalt", false, 1),
            ("Dune", true, 2),
            ("Meridian Systems", true, 2),
        ]
        .map(|(o, mark, gone)| (o.to_string(), mark, true, gone));
        assert_eq!(rows, expected);
        anyhow::Ok(())
    }
    .await;
    base.remove().await?;
    run
}

/// The end drawn on the timeline is stated in the source (#994): works_for is declared functional, the successor starts 2025-01-01; on that day the timeline
/// draws Meridian's end (day precision); "left in 2024" states the same moment, changed to a stated year-precision end, recording that it was stated
/// 2024
#[tokio::test]
async fn a_drawn_end_the_text_names_as_a_period_is_marked() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = Base::new(&pool).await?;
    let run = async {
        let next_year = t("2025-01-01T00:00:00Z");
        sqlx::query("UPDATE relation_types SET functional = true WHERE id = $1")
            .bind(base.works_for)
            .execute(&pool)
            .await?;
        let since = Validity::starting(Some(t("2024-03-01T00:00:00Z")), Some("day"));
        base.write_to(base.meridian, since).await?;
        let next = Validity::starting(Some(next_year), Some("day"));
        let next = base.write_to(base.aster, next).await?;
        utopia_store::temporal::reconcile_moved_facts(&pool, base.kb, &[next]).await?;
        // Meridian's row end: precision, whether the timeline drew it, the mark
        let end = || {
            sqlx::query_as::<_, (String, bool, bool)>(
                "SELECT valid_to_precision, end_derived, end_after_period FROM facts
                  WHERE object_id = $1 AND invalidated_at IS NULL",
            )
            .bind(base.meridian)
            .fetch_one(&pool)
        };
        let drawn = end().await?;
        let left_in_2024 = Validity {
            to_precision: Some("year"),
            ..ending(t("2024-01-01T00:00:00Z"))
        };
        base.write_to(base.meridian, left_in_2024).await?;
        assert_eq!(
            (drawn, end().await?),
            (("day".into(), true, false), ("year".into(), false, true)),
            "drawn, then stated as the end of 2024"
        );
        anyhow::Ok(())
    }
    .await;
    base.remove().await?;
    run
}

/// The mark travels with the row (#994): `rehome_tx`, used by retraction of a merge when it moves subject/object, carries it; a row produced by a correction inherits the old row's mark — changed to
/// the object of an open row with the same start, that row is closed by the same-start path; changed to the object of a row whose end the timeline drew, that end becomes a stated one — both carry it;
/// when the attribute becomes an event, the write entry point drops it
#[tokio::test]
async fn a_marked_end_rides_through_a_rehome_and_a_revision() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = Base::new(&pool).await?;
    let run = async {
        let kb = base.kb;
        let since = Validity::starting(Some(t("2024-03-01T00:00:00Z")), Some("day"));
        let closed = Validity {
            to: Some(t("2025-01-01T00:00:00Z")),
            to_precision: Some("year"),
            ..since
        };
        let row = base.write_to(base.meridian, closed).await?;
        let (cobalt, drawn, doc) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let (company, lin, works_for) = (base.company, base.lin, base.works_for);
        // Only locally generated UUIDs are interpolated into fixture SQL.
        sqlx::raw_sql(&format!(
            "UPDATE facts SET end_after_period = TRUE WHERE id = '{row}';
             INSERT INTO entities (id, kb_id, type_id, canonical_name)
                  VALUES ('{cobalt}', '{kb}', '{company}', 'Cobalt');
             INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, layer, confidence,
                                valid_from, valid_from_precision, valid_to, valid_to_precision,
                                end_derived)
                  VALUES ('{drawn}', '{kb}', '{lin}', '{works_for}', '{cobalt}', 'typed', 0.9,
                          '2024-03-01T00:00:00Z', 'day', '2025-01-01T00:00:00Z', 'day', TRUE);
             INSERT INTO documents (id, kb_id, filename, sha256)
                  VALUES ('{doc}', '{kb}', 'cv.txt', 'x');"
        ))
        .execute(&pool)
        .await?;
        let mut tx = pool.begin().await?;
        let moved = utopia_store::temporal::rehome_tx(&mut tx, row, None, Some(base.aster))
            .await?
            .expect("the row was live");
        tx.commit().await?;

        // Correction made three times: object changed back to Meridian, where a row starting 2024-03-01 was already open; changed to Cobalt, where the same-start row
        // has an end drawn by the timeline (day precision, written above); attribute changed to acquired (an event)
        let open = base.write_to(base.meridian, since).await?;
        let mut candidate = Candidate {
            fact_id: moved,
            statement_id: None,
            predicate_id: base.works_for,
            subject_id: base.lin,
            subject: "Lin Zhao".into(),
            subject_class: Some("person".into()),
            property: "works_for".into(),
            property_label: "works for".into(),
            object_id: Some(base.aster),
            object: "Aster Labs".into(),
            object_class: Some("organization".into()),
            flag: Some("name_absent".into()),
            quote: None,
        };
        // A row's mark, and which row it superseded
        let read = |id: Uuid| {
            sqlx::query_as::<_, (bool, Option<Uuid>)>(
                "SELECT end_after_period, supersedes FROM facts WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
        };
        let mut rows = vec![read(moved).await?];
        let run_id = errata::start_run(&pool, kb, doc, 3, 0).await?;
        for (property, object) in [
            (None, Some("Meridian Systems")),
            (None, Some("Cobalt")),
            (Some("acquired"), None),
        ] {
            let recorded = errata::record(
                &pool,
                kb,
                ActionInput {
                    run_id,
                    document_id: doc,
                    candidate: Some(&candidate),
                    proposed: Proposed::Revise {
                        property: property.map(Into::into),
                        object: object.map(Into::into),
                    },
                    reason: "the document says otherwise",
                    quote: Some("Lin Zhao left Aster Labs for Meridian Systems"),
                    document_text: "Lin Zhao left Aster Labs for Meridian Systems in 2024.",
                },
            )
            .await?;
            candidate.fact_id = recorded
                .new_fact_id
                .unwrap_or_else(|| panic!("{recorded:?}"));
            rows.push(read(candidate.fact_id).await?);
        }
        assert_eq!(
            rows,
            [
                (true, Some(row)),
                (true, Some(open)),
                (true, Some(drawn)),
                (false, None)
            ],
            "rehomed; revised onto an open row and a drawn end; onto an event"
        );
        anyhow::Ok(())
    }
    .await;
    base.remove().await?;
    run
}

/// Conflict of two values starting at the same moment (works_for declared functional): a person chooses "close the old one" but gives no date, so the old one is closed at
/// the new start — which is also its own start, a state that holds at no time, so it is refused (`empty_state_span`). The conflict stays for a person
/// to give a date
#[tokio::test]
async fn closing_a_simultaneous_conflict_at_its_own_start_is_refused() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = Base::new(&pool).await?;
    let run = async {
        sqlx::query("UPDATE relation_types SET functional = true WHERE id = $1")
            .bind(base.works_for)
            .execute(&pool)
            .await?;
        let since = Validity::starting(Some(day()), Some("day"));
        let mut facts = Vec::new();
        for object in [base.meridian, base.aster] {
            let (fact, _) = graph::insert_fact(
                &pool,
                base.kb,
                base.lin,
                Some(base.works_for),
                object,
                since,
                0.9,
            )
            .await?;
            facts.push(fact);
        }
        let report = utopia_store::temporal::reconcile_new_fact(
            &pool,
            base.kb,
            facts[1],
            base.lin,
            base.works_for,
            Some(base.aster),
            None,
            utopia_store::temporal::Uniqueness::SubjectSide,
            since,
            0.9,
        )
        .await?;
        assert_eq!(report.conflicts, 1);
        let conflicts = utopia_store::temporal::list_conflicts(&pool, base.kb, 10, 0).await?;
        let [conflict] = conflicts.as_slice() else {
            anyhow::bail!("one conflict: {conflicts:?}");
        };
        assert_eq!(conflict.reason, "simultaneous");
        let refused = utopia_store::temporal::resolve_conflict(
            &pool,
            base.kb,
            conflict.id,
            "close",
            None,
            "day",
        )
        .await
        .expect_err("closed where it starts");
        assert!(refused_as_empty(&refused), "{refused:?}");
        assert_eq!(
            base.live(facts[0]).await?,
            (true, None),
            "the old row is untouched"
        );
        assert_eq!(
            utopia_store::temporal::list_conflicts(&pool, base.kb, 10, 0)
                .await?
                .len(),
            1,
            "the conflict stays open"
        );
        utopia_store::temporal::resolve_conflict(
            &pool,
            base.kb,
            conflict.id,
            "close",
            Some(t("2024-01-01T00:00:00Z")),
            "day",
        )
        .await?;
        assert!(
            utopia_store::temporal::list_conflicts(&pool, base.kb, 10, 0)
                .await?
                .is_empty()
        );
        anyhow::Ok(())
    }
    .await;
    base.remove().await?;
    run
}

/// A person-written row is not voided by step 0 (the guard added at merge time, #975). Built from its origin: the attribute was an event, and a person wrote a moment;
/// a statement that states only that moment, when materialized, merged into this row, leaving a source link, but the row did not therefore become a computed one
/// (`from_statement_id` stays empty). Later, on the ontology page, the attribute was changed to a state: this row has equal ends, and has a source statement with equal ends;
/// but it was written by a person, so it is not voided. The row computed separately from the same sentence (with `from_statement_id`) is still voided as before
#[tokio::test]
async fn a_row_a_person_wrote_is_kept_when_a_single_date_merged_into_it() -> anyhow::Result<()> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let base = Base::new(&pool).await?;
    let run = async {
        let (by_hand, _) = graph::insert_fact(
            &pool,
            base.kb,
            base.lin,
            Some(base.acquired),
            base.meridian,
            moment(),
            0.9,
        )
        .await?;
        base.statement("signed with", Some(day()), Some(day()))
            .await?;
        let to_aster = base
            .statement_to("signed with", base.aster, Some(day()), Some(day()), "day")
            .await?;
        base.bind_to("signed with", base.acquired, None).await?;
        let outcome = materialize(&pool, base.kb).await?;
        assert_eq!(
            (outcome.added, outcome.merged),
            (1, 1),
            "one merged into the person's row, one computed: {outcome:?}"
        );
        let computed: Uuid =
            sqlx::query_scalar("SELECT fact_id FROM typed_fact_sources WHERE statement_id = $1")
                .bind(to_aster)
                .fetch_one(&pool)
                .await?;
        let (from_statement, links): (Option<Uuid>, i64) = sqlx::query_as(
            "SELECT from_statement_id,
                    (SELECT count(*) FROM typed_fact_sources WHERE fact_id = f.id)
               FROM facts f WHERE id = $1",
        )
        .bind(by_hand)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            (from_statement, links),
            (None, 1),
            "the statement is a source of the person's row, which stays a person's"
        );

        // On the ontology page, acquired is changed to a state
        sqlx::query("UPDATE relation_types SET temporal = 'state' WHERE id = $1")
            .bind(base.acquired)
            .execute(&pool)
            .await?;
        let outcome = materialize(&pool, base.kb).await?;
        assert_eq!(outcome.retired, 1, "only the computed row: {outcome:?}");
        assert_eq!(
            base.live(by_hand).await?,
            (true, Some(day())),
            "a row a person wrote is kept"
        );
        assert_eq!(
            base.live(computed).await?,
            (false, Some(day())),
            "the computed one is retired"
        );
        anyhow::Ok(())
    }
    .await;
    base.remove().await?;
    run
}
