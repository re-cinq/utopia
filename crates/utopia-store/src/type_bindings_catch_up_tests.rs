//! An entity created after a judgement has landed also gets the class its category word is bound to (`apply_bound`).
//!
//! The projection at the moment of judgement (`apply`) only sees the entities that exist at that moment; a company created in the second half of a half-extracted document ends up with no class,
//! so its attribute statements never reach any attribute candidate whose domain is declared. Skipped when `UTOPIA_DATABASE_URL` is not set.

use super::{apply_bound, apply_bound_to};
use sqlx::PgPool;
use uuid::Uuid;

struct Fx {
    org: Uuid,
    kb: Uuid,
    class: Uuid,
    late: Uuid,
    human: Uuid,
    other: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fx> {
    let (org, ws, kb, class, late, human, other) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations(id,name) VALUES($1,'catch-up')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces(id,org_id,name) VALUES($1,$2,'catch-up')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases(id,workspace_id,name) VALUES($1,$2,'catch-up')")
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO entity_types(id,kb_id,key,label) VALUES($1,$2,'company','企业')")
        .bind(class)
        .bind(kb)
        .execute(pool)
        .await?;
    // "Company" is already bound to the enterprise class; the entities below did not exist at the time of judgement
    sqlx::query(
        "INSERT INTO type_bindings(id,kb_id,kind_word,type_id,status) VALUES($1,$2,'公司',$3,'bound')",
    )
    .bind(Uuid::now_v7())
    .bind(kb)
    .bind(class)
    .execute(pool)
    .await?;
    // A company extracted after the judgement: the category word is written with whitespace, and after normalisation it is the same word
    sqlx::query(
        "INSERT INTO entities(id,kb_id,canonical_name,specific_type) VALUES($1,$2,'蓝海物流有限公司',' 公司 ')",
    )
    .bind(late)
    .bind(kb)
    .execute(pool)
    .await?;
    // The person said it has no class: a binding does not override a human decision
    sqlx::query(
        "INSERT INTO entities(id,kb_id,canonical_name,specific_type,type_source) VALUES($1,$2,'某公司','公司','human')",
    )
    .bind(human)
    .bind(kb)
    .execute(pool)
    .await?;
    // Other category words: no binding, left alone
    sqlx::query(
        "INSERT INTO entities(id,kb_id,canonical_name,specific_type,proposed_type) VALUES($1,$2,'王海','董事长','chairman')",
    )
    .bind(other)
    .bind(kb)
    .execute(pool)
    .await?;
    Ok(Fx {
        org,
        kb,
        class,
        late,
        human,
        other,
    })
}

async fn class_of(
    pool: &PgPool,
    id: Uuid,
) -> anyhow::Result<(Option<Uuid>, String, Option<String>)> {
    Ok(
        sqlx::query_as("SELECT type_id, type_source, proposed_type FROM entities WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

#[tokio::test]
async fn an_entity_built_after_the_decision_still_gets_its_class() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    crate::db::migrate(&pool).await?;
    let f = seed(&pool).await?;
    let run = async {
        assert_eq!(apply_bound(&pool, f.kb).await?, 1);
        assert_eq!(
            class_of(&pool, f.late).await?,
            (Some(f.class), "aligned".into(), None)
        );
        assert_eq!(
            class_of(&pool, f.human).await?,
            (None, "human".into(), None)
        );
        assert_eq!(
            class_of(&pool, f.other).await?,
            (None, "extracted".into(), Some("chairman".into()))
        );
        // Idempotent: entities already on that class do not count as a change
        assert_eq!(apply_bound(&pool, f.kb).await?, 0);
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}

/// What one document's extraction runs (RC-011): the catch-up reaches the entities it names and the ends of the facts
/// the document is evidence for, and no other entity, so it does not read the whole base. The base-wide pass still
/// picks up the rest.
#[tokio::test]
async fn a_documents_catch_up_reaches_only_what_the_document_touches() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    crate::db::migrate(&pool).await?;
    let f = seed(&pool).await?;
    let run = async {
        let (doc, chunk, other_doc) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        for (id, name) in [(doc, "pushed.json"), (other_doc, "other.json")] {
            sqlx::query("INSERT INTO documents(id,kb_id,filename,sha256) VALUES($1,$2,$3,$4)")
                .bind(id)
                .bind(f.kb)
                .bind(name)
                .bind(id.to_string())
                .execute(&pool)
                .await?;
        }
        sqlx::query("INSERT INTO chunks(id,kb_id,document_id,seq,text) VALUES($1,$2,$3,0,'pushed')")
            .bind(chunk)
            .bind(f.kb)
            .bind(doc)
            .execute(&pool)
            .await?;
        // A second company built after the judgement, by another document
        let elsewhere = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO entities(id,kb_id,canonical_name,specific_type) VALUES($1,$2,'远洋货运有限公司','公司')",
        )
        .bind(elsewhere)
        .bind(f.kb)
        .execute(&pool)
        .await?;

        // Nothing named and no fact yet: nothing to do, and the base is not read
        assert_eq!(apply_bound_to(&pool, f.kb, Some(doc), &[]).await?, 0);
        assert_eq!(class_of(&pool, f.late).await?.0, None);

        // This run resolved the late company, the human-decided one and the chairman
        assert_eq!(
            apply_bound_to(&pool, f.kb, Some(doc), &[f.late, f.human, f.other]).await?,
            1
        );
        assert_eq!(
            class_of(&pool, f.late).await?,
            (Some(f.class), "aligned".into(), None)
        );
        assert_eq!(
            class_of(&pool, f.human).await?,
            (None, "human".into(), None)
        );
        assert_eq!(
            class_of(&pool, f.other).await?,
            (None, "extracted".into(), Some("chairman".into()))
        );
        assert_eq!(class_of(&pool, elsewhere).await?.0, None);
        // Idempotent for the same entities
        assert_eq!(apply_bound_to(&pool, f.kb, Some(doc), &[f.late]).await?, 0);

        // A fact the document is evidence for, which this run did not resolve (carried over from the item's previous
        // version, or written by an attempt that was interrupted): both of its ends are caught up
        let (subject, object, fact) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        sqlx::query(
            "INSERT INTO entities(id,kb_id,canonical_name,specific_type) VALUES($1,$3,'北港仓储有限公司','公司'),($2,$3,'南湾码头有限公司','公司')",
        )
        .bind(subject)
        .bind(object)
        .bind(f.kb)
        .execute(&pool)
        .await?;
        sqlx::query("INSERT INTO facts(id,kb_id,subject_id,object_id,layer,phrase) VALUES($1,$2,$3,$4,'open','租用')")
            .bind(fact)
            .bind(f.kb)
            .bind(subject)
            .bind(object)
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO fact_evidence(fact_id,chunk_id,quote,document_id,doc_version) VALUES($1,$2,'租用',$3,1)")
            .bind(fact)
            .bind(chunk)
            .bind(doc)
            .execute(&pool)
            .await?;
        // A fact only an earlier version of the document was evidence for: its chunk is superseded, and it is not read
        let (old_chunk, gone, old_fact) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        sqlx::query("INSERT INTO chunks(id,kb_id,document_id,seq,text,superseded_at) VALUES($1,$2,$3,0,'earlier',now())")
            .bind(old_chunk)
            .bind(f.kb)
            .bind(doc)
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO entities(id,kb_id,canonical_name,specific_type) VALUES($1,$2,'旧港物流有限公司','公司')")
            .bind(gone)
            .bind(f.kb)
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO facts(id,kb_id,subject_id,layer,phrase,object_value) VALUES($1,$2,$3,'open','成立于','{\"value\":\"2001\"}')")
            .bind(old_fact)
            .bind(f.kb)
            .bind(gone)
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO fact_evidence(fact_id,chunk_id,quote,document_id,doc_version) VALUES($1,$2,'成立于',$3,1)")
            .bind(old_fact)
            .bind(old_chunk)
            .bind(doc)
            .execute(&pool)
            .await?;

        // Another document's catch-up does not reach them
        assert_eq!(apply_bound_to(&pool, f.kb, Some(other_doc), &[]).await?, 0);
        assert_eq!(class_of(&pool, subject).await?.0, None);
        // Without a document to read evidence of (a memory log), only the named entities count
        assert_eq!(apply_bound_to(&pool, f.kb, None, &[]).await?, 0);
        assert_eq!(class_of(&pool, subject).await?.0, None);
        assert_eq!(apply_bound_to(&pool, f.kb, Some(doc), &[]).await?, 2);
        assert_eq!(class_of(&pool, subject).await?.0, Some(f.class));
        assert_eq!(class_of(&pool, object).await?.0, Some(f.class));
        assert_eq!(class_of(&pool, elsewhere).await?.0, None);
        assert_eq!(class_of(&pool, gone).await?.0, None);

        // The base-wide pass still reaches the ones no document touches now
        assert_eq!(apply_bound(&pool, f.kb).await?, 2);
        assert_eq!(
            class_of(&pool, elsewhere).await?,
            (Some(f.class), "aligned".into(), None)
        );
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
