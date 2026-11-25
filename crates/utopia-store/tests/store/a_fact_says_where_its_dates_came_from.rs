//! Where a fact's dates come from (#970). Each fact row in the graph tool carries `[n]`, which opens the source sentence that fact was read from (#968).
//! Two kinds of dates can appear that the source sentence does not contain:
//!
//! - An end point derived from the timeline: Li Si has led Aurora since 2024-07-05, Zhou Qi took over on 2025-09-01, and `leads` allows only one lead per project,
//!     so reconciliation closes Li Si's span at 2025-09-01 -- `end_derived`;
//! - A span a person edited: Zhang San's start was changed by a person from 2023-01-10 to 2023-02-01 -- `time_corrected`, and the edit changed the
//!     start (`corrected_ends`, written with the correction row). If the row is rewritten later it stays marked; editing the end too makes both ends person-set.
//!     The mark lives on the row, not on the audit that the correction route writes after the fact: when Li Si's start was edited there was no audit, yet the mark is still there.
//!     When a person hands over the whole span, Li Si's derived end is pinned even if the person kept its value unchanged, so both ends count as the person's.
//!
//! A span a person closed by hand is neither of those -- it is still a correction row (`corrected`, which the wording in the web evidence panel follows).
//!
//! Step two: the derived end can find the row that closed it (Li Si's span is closed by Zhou Qi's row; for functional the employer is the next employer),
//! and a span a person edited carries the note written at correction time. Rewinding the record axis to before the takeover, Li Si's span is still open,
//! and nobody has closed it.
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set. It creates and tears down its own data and never touches an existing database.

use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::PgPool;
use utopia_core::models::EntityFact;
use utopia_store::graph::{self, Validity};
use uuid::Uuid;

fn t(s: &str) -> DateTime<Utc> {
    s.parse().expect("fixed timestamp")
}

/// A day-precision date
fn day(s: &str) -> DateTime<Utc> {
    t(&format!("{s}T00:00:00Z"))
}

fn since(at: &str) -> Validity<'static> {
    Validity::starting(Some(day(at)), Some("day"))
}

fn between(from: &str, to: &str) -> Validity<'static> {
    Validity {
        to: Some(day(to)),
        to_precision: Some("day"),
        ..since(from)
    }
}

fn fact_of<'f>(facts: &'f [EntityFact], who: &str) -> &'f EntityFact {
    facts
        .iter()
        .find(|f| f.other_name.as_deref() == Some(who))
        .unwrap_or_else(|| panic!("no fact of {who}"))
}

/// That person's row: (end is derived, span was edited by a person, is a correction row)
fn flags(facts: &[EntityFact], who: &str) -> (bool, bool, bool) {
    let f = fact_of(facts, who);
    (f.end_derived, f.time_corrected, f.corrected)
}

/// That person's row, and which end a person edited
fn ends(facts: &[EntityFact], who: &str) -> Option<String> {
    fact_of(facts, who).corrected_ends.clone()
}

/// Who closed that person's row: (the row that closed it, and which end the successor took over)
fn closer(facts: &[EntityFact], who: &str) -> (Option<Uuid>, Option<String>) {
    let f = fact_of(facts, who);
    (f.closed_by_id, f.closed_by.clone())
}

#[tokio::test]
async fn a_fact_says_where_its_dates_came_from() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let ids: Vec<Uuid> = (0..17).map(|_| Uuid::now_v7()).collect();
    let (org, ws, kb, user, person, project) = (ids[0], ids[1], ids[2], ids[3], ids[4], ids[5]);
    let (leads, advises, zhang, li, zhou, wang, aurora) =
        (ids[6], ids[7], ids[8], ids[9], ids[10], ids[11], ids[12]);
    let (company, employer, acme, globex) = (ids[13], ids[14], ids[15], ids[16]);
    // Only locally generated UUIDs are interpolated into fixture SQL.
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name) VALUES ('{org}', 'dates-came-from');
         INSERT INTO workspaces (id, org_id, name) VALUES ('{ws}', '{org}', 'dates-came-from');
         INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ('{kb}', '{ws}', 'dates-came-from');
         INSERT INTO users (id, org_id, email, display_name, password_hash)
              VALUES ('{user}', '{org}', '{user}@dates.test', 'editor', 'unused');
         INSERT INTO entity_types (id, kb_id, key, label, color, shape) VALUES
             ('{person}', '{kb}', 'person', 'person', '#7fd0ff', 'circle'),
             ('{project}', '{kb}', 'project', 'project', '#7fd0ff', 'circle'),
             ('{company}', '{kb}', 'organization', 'organization', '#7fd0ff', 'circle');
         INSERT INTO relation_types (id, kb_id, key, label, temporal, inverse_functional) VALUES
             ('{leads}', '{kb}', 'leads', 'leads', 'state', TRUE),
             ('{advises}', '{kb}', 'advises', 'advises', 'state', FALSE);
         INSERT INTO relation_types (id, kb_id, key, label, temporal, functional) VALUES
             ('{employer}', '{kb}', 'employer', 'employer', 'state', TRUE);
         INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES
             ('{zhang}', '{kb}', '{person}', 'Zhang San'), ('{li}', '{kb}', '{person}', 'Li Si'),
             ('{zhou}', '{kb}', '{person}', 'Zhou Qi'), ('{wang}', '{kb}', '{person}', 'Wang Wu'),
             ('{aurora}', '{kb}', '{project}', 'Aurora'),
             ('{acme}', '{kb}', '{company}', 'Acme'), ('{globex}', '{kb}', '{company}', 'Globex');"
    ))
    .execute(&pool)
    .await?;

    let run = async {
        let fact = |who: Uuid, predicate: Uuid, validity: Validity<'static>| {
            let pool = pool.clone();
            async move {
                graph::insert_fact(&pool, kb, who, Some(predicate), aurora, validity, 0.9)
                    .await
                    .map(|(id, _)| id)
            }
        };
        // Zhou Qi takes over: a project has only one lead at a time, so reconciliation closes Li Si's span at 2025-09-01
        let li_row = fact(li, leads, since("2024-07-05")).await?;
        let zhou_row = fact(zhou, leads, since("2025-09-01")).await?;
        let before_close: DateTime<Utc> =
            sqlx::query_scalar("SELECT now()").fetch_one(&pool).await?;
        utopia_store::temporal::reconcile_moved_facts(&pool, kb, &[li_row, zhou_row]).await?;

        // Wang Wu changes employer: a person has only one employer at a time (functional), so reconciliation closes the Acme span on the day of the change
        let acme_row = graph::insert_fact(
            &pool,
            kb,
            wang,
            Some(employer),
            acme,
            since("2020-01-01"),
            0.9,
        )
        .await?
        .0;
        let globex_row = graph::insert_fact(
            &pool,
            kb,
            wang,
            Some(employer),
            globex,
            since("2022-03-01"),
            0.9,
        )
        .await?
        .0;
        utopia_store::temporal::reconcile_moved_facts(&pool, kb, &[acme_row, globex_row]).await?;

        // A person edits Zhang San's start, the end stays put; the audit is recorded on the edited row (same path as PATCH /facts/{id})
        let zhang_row = fact(zhang, leads, between("2023-01-10", "2024-07-05")).await?;
        let corrected = utopia_store::temporal::correct_interval(
            &pool,
            zhang_row,
            between("2023-02-01", "2024-07-05"),
        )
        .await?
        .expect("the row was live");
        utopia_store::audit::record(
            &pool,
            Some(kb),
            user,
            "fact.time_corrected",
            "fact",
            Some(zhang_row),
            json!({ "note": "The charter date was the approval date" }),
        )
        .await?;

        // A span closed by hand: one correction row, but the end was given by the person and the span was not edited
        let wang_row = graph::insert_fact(
            &pool,
            kb,
            wang,
            Some(advises),
            aurora,
            since("2023-03-02"),
            0.9,
        )
        .await?
        .0;
        utopia_store::temporal::close_superseded(&pool, wang_row, t("2024-01-01T00:00:00Z"), "day")
            .await?;

        let (_, facts) = graph::entity_detail(&pool, kb, aurora, None, None).await?;
        assert_eq!(
            flags(&facts, "Li Si"),
            (true, false, true),
            "the timeline closed it"
        );
        assert_eq!(flags(&facts, "Zhou Qi"), (false, false, false));
        assert_eq!(
            flags(&facts, "Zhang San"),
            (false, true, true),
            "a person corrected it"
        );
        assert_eq!(
            flags(&facts, "Wang Wu"),
            (false, false, true),
            "closed by hand: rewritten, but neither mark"
        );
        // Step two: who closed it, and what the person said when editing it
        assert_eq!(
            closer(&facts, "Li Si"),
            (Some(zhou_row), Some("Zhou Qi".to_string())),
            "Zhou Qi's row took over"
        );
        assert_eq!(closer(&facts, "Zhou Qi"), (None, None), "still open");
        assert_eq!(closer(&facts, "Wang Wu"), (None, None), "closed by hand");
        assert_eq!(
            fact_of(&facts, "Zhang San").correction_note.as_deref(),
            Some("The charter date was the approval date")
        );
        assert_eq!(ends(&facts, "Zhang San").as_deref(), Some("start"));
        assert_eq!(
            ends(&facts, "Wang Wu"),
            None,
            "closed by hand, not corrected"
        );
        let (_, wang_facts) = graph::entity_detail(&pool, kb, wang, None, None).await?;
        assert_eq!(
            closer(&wang_facts, "Acme"),
            (Some(globex_row), Some("Globex".to_string())),
            "a functional property: the next employer took over"
        );
        // Rewind the record axis to before the takeover: Li Si's span is still open and nobody has closed it
        let (_, then) = graph::entity_detail(&pool, kb, aurora, None, Some(before_close)).await?;
        assert!(!flags(&then, "Li Si").0, "not closed yet");
        assert_eq!(closer(&then, "Li Si"), (None, None));

        // An edited span is rewritten once more (here, closed): the mark is carried on the row, and the note is still findable along the chain
        utopia_store::temporal::close_superseded(
            &pool,
            corrected,
            t("2024-06-30T00:00:00Z"),
            "day",
        )
        .await?;
        let (_, facts) = graph::entity_detail(&pool, kb, aurora, None, None).await?;
        assert_eq!(flags(&facts, "Zhang San"), (false, true, true));
        assert_eq!(ends(&facts, "Zhang San").as_deref(), Some("start"));
        assert_eq!(
            fact_of(&facts, "Zhang San").correction_note.as_deref(),
            Some("The charter date was the approval date"),
            "the note is found up the chain"
        );

        // A person edited only Li Si's start and copied the end: the whole span was given, so the derived end is pinned by the person from now on
        // (the timeline no longer redraws it), and both ends count as the person's -- the row cannot claim the end comes from the source sentence. No audit this time,
        // but the mark is still on the row
        let li_now = fact_of(&facts, "Li Si").id;
        utopia_store::temporal::correct_interval(
            &pool,
            li_now,
            between("2024-07-08", "2025-09-01"),
        )
        .await?
        .expect("the row was live");
        let (_, facts) = graph::entity_detail(&pool, kb, aurora, None, None).await?;
        assert_eq!(
            flags(&facts, "Li Si"),
            (false, true, true),
            "a person's interval: the timeline no longer owns its end"
        );
        assert_eq!(
            ends(&facts, "Li Si").as_deref(),
            Some("both"),
            "the derived end the person kept is theirs too"
        );
        assert_eq!(
            fact_of(&facts, "Li Si").correction_note,
            None,
            "no note written"
        );

        // Zhang San edits once more, this time the end: both ends are now person-edited; the note takes the most recent edit
        let zhang_now = fact_of(&facts, "Zhang San").id;
        utopia_store::temporal::correct_interval(
            &pool,
            zhang_now,
            between("2023-02-01", "2024-07-01"),
        )
        .await?
        .expect("the row was live");
        utopia_store::audit::record(
            &pool,
            Some(kb),
            user,
            "fact.time_corrected",
            "fact",
            Some(zhang_now),
            json!({ "note": "The handover was on 1 July" }),
        )
        .await?;
        let (_, facts) = graph::entity_detail(&pool, kb, aurora, None, None).await?;
        assert_eq!(ends(&facts, "Zhang San").as_deref(), Some("both"));
        assert_eq!(
            fact_of(&facts, "Zhang San").correction_note.as_deref(),
            Some("The handover was on 1 July")
        );
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}

/// Two kinds of closer (#986 review at merge time):
/// - Two successors starting the same day: the timeline takes the earliest row by id to close the predecessor, and the row says so too;
/// - The closer was itself later closed by a next successor and rewritten into a new row: it refers to the live version, and the name does not change
#[tokio::test]
async fn a_closer_is_the_row_the_timeline_closed_with() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let ids: Vec<Uuid> = (0..12).map(|_| Uuid::now_v7()).collect();
    let (org, ws, kb, person, project, leads) = (ids[0], ids[1], ids[2], ids[3], ids[4], ids[5]);
    let (li, zhou, sun, qian, aurora, helios) = (ids[6], ids[7], ids[8], ids[9], ids[10], ids[11]);
    // Only locally generated UUIDs are interpolated into fixture SQL.
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name) VALUES ('{org}', 'closers');
         INSERT INTO workspaces (id, org_id, name) VALUES ('{ws}', '{org}', 'closers');
         INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ('{kb}', '{ws}', 'closers');
         INSERT INTO entity_types (id, kb_id, key, label, color, shape) VALUES
             ('{person}', '{kb}', 'person', 'person', '#7fd0ff', 'circle'),
             ('{project}', '{kb}', 'project', 'project', '#7fd0ff', 'circle');
         INSERT INTO relation_types (id, kb_id, key, label, temporal, inverse_functional)
              VALUES ('{leads}', '{kb}', 'leads', 'leads', 'state', TRUE);
         INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES
             ('{li}', '{kb}', '{person}', 'Li Si'), ('{zhou}', '{kb}', '{person}', 'Zhou Qi'),
             ('{sun}', '{kb}', '{person}', 'Sun Ba'), ('{qian}', '{kb}', '{person}', 'Qian Qi'),
             ('{aurora}', '{kb}', '{project}', 'Aurora'), ('{helios}', '{kb}', '{project}', 'Helios');"
    ))
    .execute(&pool)
    .await?;

    let run = async {
        let lead = |who: Uuid, project: Uuid, at: &'static str| {
            let pool = pool.clone();
            async move {
                graph::insert_fact(&pool, kb, who, Some(leads), project, since(at), 0.9)
                    .await
                    .map(|(id, _)| id)
            }
        };
        let live = |who: Uuid, project: Uuid| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, Uuid>(
                    "SELECT id FROM facts WHERE kb_id = $1 AND subject_id = $2 AND object_id = $3
                        AND invalidated_at IS NULL",
                )
                .bind(kb)
                .bind(who)
                .bind(project)
                .fetch_one(&pool)
                .await
            }
        };

        // Aurora: Zhou Qi and Sun Ba take over the same day. A project has only one lead at a time, so Li Si's span closes on that day,
        // and the one that closes it is Zhou Qi's row, the earlier id
        let li_a = lead(li, aurora, "2024-07-05").await?;
        let zhou_a = lead(zhou, aurora, "2025-09-01").await?;
        let sun_a = lead(sun, aurora, "2025-09-01").await?;
        assert!(zhou_a < sun_a);
        utopia_store::temporal::reconcile_moved_facts(&pool, kb, &[li_a, zhou_a, sun_a]).await?;
        let (_, facts) = graph::entity_detail(&pool, kb, aurora, None, None).await?;
        assert!(flags(&facts, "Li Si").0, "the timeline closed it");
        assert_eq!(fact_of(&facts, "Li Si").valid_to, Some(day("2025-09-01")));
        assert_eq!(
            closer(&facts, "Li Si"),
            (Some(zhou_a), Some("Zhou Qi".to_string())),
            "the first of the two by id, as the engine orders them"
        );

        // Helios: Zhou Qi takes over, then Qian Qi takes over from him: Zhou Qi's row is closed and rewritten into a new row. Li Si's
        // closer is Zhou Qi's live version
        let li_h = lead(li, helios, "2024-07-05").await?;
        let zhou_h = lead(zhou, helios, "2025-09-01").await?;
        utopia_store::temporal::reconcile_moved_facts(&pool, kb, &[li_h, zhou_h]).await?;
        let qian_h = lead(qian, helios, "2026-03-01").await?;
        utopia_store::temporal::reconcile_moved_facts(&pool, kb, &[qian_h]).await?;
        let zhou_now = live(zhou, helios).await?;
        assert_ne!(
            zhou_now, zhou_h,
            "the closer was rewritten when Qian Qi took over"
        );
        let (_, facts) = graph::entity_detail(&pool, kb, helios, None, None).await?;
        assert_eq!(fact_of(&facts, "Zhou Qi").valid_to, Some(day("2026-03-01")));
        assert_eq!(fact_of(&facts, "Li Si").valid_to, Some(day("2025-09-01")));
        assert_eq!(
            closer(&facts, "Li Si"),
            (Some(zhou_now), Some("Zhou Qi".to_string())),
            "the live version of the closer"
        );
        assert_eq!(
            closer(&facts, "Zhou Qi"),
            (Some(qian_h), Some("Qian Qi".to_string()))
        );
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
