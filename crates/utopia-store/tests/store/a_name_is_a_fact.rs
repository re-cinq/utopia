//! A name is a fact about an entity (0041 decision 1).
//!
//! When an entity is created, its canonical name is already a name fact; a recorded alias lets later mentions find it; names are not in the fact list,
//! do not count towards degree; a merge moves names as ordinary facts, and undoing the merge moves them back.

use sqlx::PgPool;
use utopia_store::{graph, names, resolution};
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    equipment: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb, equipment) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'names-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'names-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'names-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'equipment', 'Equipment')")
        .bind(equipment)
        .bind(kb)
        .execute(pool)
        .await?;
    Ok(Fixture { org, kb, equipment })
}

async fn teardown(pool: &PgPool, f: &Fixture) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(pool)
        .await?;
    Ok(())
}

async fn mention(pool: &PgPool, f: &Fixture, name: &str) -> anyhow::Result<resolution::Resolution> {
    // No vector given: a candidate is recalled and then the one with the most facts is taken; what is measured is whether recall can find it at all
    Ok(
        resolution::resolve_mention(pool, f.kb, Some(f.equipment), name, None, None, None, &[])
            .await?,
    )
}

fn values(v: &[utopia_core::models::NameView]) -> Vec<String> {
    let mut out: Vec<String> = v.iter().map(|n| n.name.clone()).collect();
    out.sort();
    out
}

#[tokio::test]
async fn a_new_entity_carries_its_name_and_an_alias_brings_a_later_mention_home(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;
    let run = async {
        let probe = mention(&pool, &f, "海洋探测器1号").await?;
        assert!(probe.created);
        let n = names::for_entity(&pool, f.kb, probe.entity_id, None).await?;
        assert_eq!(values(&n), vec!["海洋探测器1号"], "本名是一条名字事实");
        assert!(n[0].canonical);

        // A short name with no bridge: a separate entity is created (this is exactly what 0041 is meant to fix; this slice fixes it by recording the name)
        let before = mention(&pool, &f, "海探2").await?;
        assert!(before.created && before.entity_id != probe.entity_id);

        names::record(&pool, f.kb, probe.entity_id, "海探1", None, None).await?;
        let later = mention(&pool, &f, "海探1").await?;
        assert!(!later.created, "记下的别名让后来的提及找得到它");
        assert_eq!(later.entity_id, probe.entity_id);
        assert_eq!(
            resolution::existing_by_name(&pool, f.kb, "海探1").await?,
            Some(probe.entity_id)
        );

        // Recording the same name again yields only one row
        names::record(&pool, f.kb, probe.entity_id, " 海探1 ", None, None).await?;
        let n = names::for_entity(&pool, f.kb, probe.entity_id, None).await?;
        assert_eq!(values(&n), vec!["海探1", "海洋探测器1号"]);
        assert_eq!(
            names::record(&pool, f.kb, probe.entity_id, "  ", None, None).await?,
            None
        );
        anyhow::Ok(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

#[tokio::test]
async fn a_name_is_neither_a_listed_fact_nor_a_degree() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;
    let run = async {
        let probe = mention(&pool, &f, "海洋探测器1号").await?;
        names::record(&pool, f.kb, probe.entity_id, "海探1", None, None).await?;
        let (node, facts) = graph::entity_detail(&pool, f.kb, probe.entity_id, None, None).await?;
        assert!(facts.is_empty(), "名字不进事实列表：{facts:?}");
        assert_eq!(node.degree, 0, "名字不算度数");
        anyhow::Ok(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

#[tokio::test]
async fn merging_moves_names_and_reverting_brings_them_back() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;
    let run = async {
        let full = mention(&pool, &f, "海洋探测器1号").await?.entity_id;
        let short = mention(&pool, &f, "海探1").await?.entity_id;
        assert_ne!(full, short);

        let merge = resolution::merge_entities(&pool, f.kb, short, full, None, "test").await?;
        assert_eq!(
            values(&names::for_entity(&pool, f.kb, full, None).await?),
            vec!["海探1", "海洋探测器1号"],
            "被并方的名字随它的事实搬过来"
        );
        assert_eq!(
            resolution::existing_by_name(&pool, f.kb, "海探1").await?,
            Some(full),
            "合并之后按简称找到的是存活者"
        );

        resolution::revert_merge(&pool, f.kb, merge).await?;
        assert_eq!(
            values(&names::for_entity(&pool, f.kb, short, None).await?),
            vec!["海探1"]
        );
        assert_eq!(
            values(&names::for_entity(&pool, f.kb, full, None).await?),
            vec!["海洋探测器1号"],
            "撤回合并，名字搬回去"
        );
        anyhow::Ok(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

#[tokio::test]
async fn a_name_another_entity_already_has_queues_the_pair_without_merging() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;
    let run = async {
        // Arrived out of order: the document that writes only the short name creates "Haitan 1" first, and the one that writes the full name arrives later
        let short = mention(&pool, &f, "海探1").await?.entity_id;
        let full = mention(&pool, &f, "海洋探测器1号").await?.entity_id;
        names::record(&pool, f.kb, full, "海探1", None, None).await?;
        assert_eq!(
            names::pair_shared_name(&pool, f.kb, full, "海探1").await?,
            1
        );
        let (reason, stage, status): (String, String, String) = sqlx::query_as(
            "SELECT reason, stage, status FROM resolution_reviews
              WHERE kb_id = $1 AND least(left_id, right_id) = least($2, $3)
                AND greatest(left_id, right_id) = greatest($2, $3)",
        )
        .bind(f.kb)
        .bind(short)
        .bind(full)
        .fetch_one(&pool)
        .await?;
        assert_eq!(reason, "shared_name|海探1");
        assert_eq!(
            (stage.as_str(), status.as_str()),
            ("adjudicating", "pending")
        );
        let merged: Option<Uuid> =
            sqlx::query_scalar("SELECT merged_into FROM entities WHERE id = $1")
                .bind(short)
                .fetch_one(&pool)
                .await?;
        assert_eq!(merged, None, "只排队，不合并");
        // Reporting the same name again does not queue it twice
        assert_eq!(
            names::pair_shared_name(&pool, f.kb, full, "海探1").await?,
            1
        );
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM resolution_reviews WHERE kb_id = $1")
            .bind(f.kb)
            .fetch_one(&pool)
            .await?;
        assert_eq!(n, 1);
        anyhow::Ok(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

#[tokio::test]
async fn the_adjudicator_sees_the_other_names_and_never_the_shared_one() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;
    let run = async {
        let full = mention(&pool, &f, "海洋探测器1号").await?.entity_id;
        names::record(&pool, f.kb, full, "海探1", None, None).await?;
        let lines = resolution::entity_fact_lines(&pool, f.kb, full, 4).await?;
        assert_eq!(lines, vec!["also known as: 海探1".to_string()]);
        let bare = mention(&pool, &f, "海探2").await?.entity_id;
        assert!(
            resolution::entity_fact_lines(&pool, f.kb, bare, 4)
                .await?
                .is_empty(),
            "只有本名的实体没有「又名」这一行"
        );
        anyhow::Ok(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// A pair that was separated: when the same short name is read again it is not queued again — the decision to separate must be remembered
#[tokio::test]
async fn a_pair_kept_apart_is_not_queued_again() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;
    let run = async {
        mention(&pool, &f, "海探1").await?;
        let full = mention(&pool, &f, "海洋探测器1号").await?.entity_id;
        names::record(&pool, f.kb, full, "海探1", None, None).await?;
        assert_eq!(
            names::pair_shared_name(&pool, f.kb, full, "海探1").await?,
            1
        );
        sqlx::query(
            "UPDATE resolution_reviews SET status = 'kept', decided_at = now() WHERE kb_id = $1",
        )
        .bind(f.kb)
        .execute(&pool)
        .await?;
        assert_eq!(
            names::pair_shared_name(&pool, f.kb, full, "海探1").await?,
            0,
            "判过不是一个的，不再配"
        );
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM resolution_reviews WHERE kb_id = $1 AND status = 'pending'",
        )
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        assert_eq!(pending, 0);
        anyhow::Ok(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// A name attribute is not an ordinary attribute: it cannot be found by key, cannot be changed, and does not enter the ontology vector index.
/// Otherwise ontology proposals could merge values such as "short names" into it, and the uniqueness panel could mark it functional,
/// and the name checks and pairing would all be bypassed
#[tokio::test]
async fn the_name_attribute_is_not_an_ordinary_attribute() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;
    let run = async {
        let known_as = names::ensure_known_as(&pool, f.kb).await?;
        let edited = utopia_store::ontology::update_relation_type(
            &pool,
            f.kb,
            known_as,
            "known as",
            "state",
            utopia_core::models::RelationAxioms {
                functional: true,
                ..Default::default()
            },
            "",
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(edited.is_err(), "内建的名字属性不能改成 functional");
        let functional: bool =
            sqlx::query_scalar("SELECT functional FROM relation_types WHERE id = $1")
                .bind(known_as)
                .fetch_one(&pool)
                .await?;
        assert!(!functional);
        let stale = utopia_store::ontology::types_needing_embedding(
            &pool,
            f.kb,
            "test-model",
            Some(utopia_store::ontology::TypeKind::Relation),
        )
        .await?;
        assert!(stale.iter().all(|t| t.id != known_as), "名字属性不嵌");
        anyhow::Ok(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// A quote that writes only a name is not the original text: it is not in the snippet given to the governance agent, but an alias carrying a whole sentence still is.
/// Delete the document and the entity's canonical name remains; only aliases read from this document alone are invalidated along with it
#[tokio::test]
async fn a_bare_name_is_not_a_quote_and_a_deleted_document_keeps_the_name() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;
    let run = async {
        let full = mention(&pool, &f, "海洋探测器1号").await?.entity_id;
        let (doc, chunk) = (Uuid::now_v7(), Uuid::now_v7());
        sqlx::query("INSERT INTO documents (id, kb_id, filename, sha256) VALUES ($1, $2, 'probe.txt', $3)")
            .bind(doc)
            .bind(f.kb)
            .bind(doc.to_string())
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO chunks (id, kb_id, document_id, seq, text)
             VALUES ($1, $2, $3, 0, '海洋探测器1号（简称海探1）于2026年下水。')",
        )
        .bind(chunk)
        .bind(f.kb)
        .bind(doc)
        .execute(&pool)
        .await?;
        let source = |quote| {
            Some(names::NameSource {
                chunk_id: chunk,
                quote,
            })
        };
        let canonical = names::record(&pool, f.kb, full, "海洋探测器1号", source("海洋探测器1号"), None)
            .await?
            .expect("canonical name fact");
        let alias = names::record(&pool, f.kb, full, "海探1", source("简称海探1"), None)
            .await?
            .expect("alias fact");
        let launched: Uuid = sqlx::query_scalar(
            "INSERT INTO relation_types (id, kb_id, key, label, kind, datatype, temporal)
             VALUES ($1, $2, 'launch_year', 'launch year', 'attribute', 'text', 'event') RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(f.kb)
        .fetch_one(&pool)
        .await?;
        let (fact, _) = graph::insert_value_fact(
            &pool,
            f.kb,
            full,
            Some(launched),
            &serde_json::json!({ "value": "2026" }),
            graph::Validity::default(),
            0.9,
        )
        .await?;
        graph::add_evidence(&pool, fact, chunk, Some("于2026年下水"), Some("launch_year")).await?;

        let quotes: Vec<String> = utopia_store::governance::quotes_of(&pool, f.kb, full, 10)
            .await?
            .into_iter()
            .map(|(_, q)| q)
            .collect();
        assert!(quotes.iter().all(|q| q != "海洋探测器1号"), "{quotes:?}");
        assert_eq!(quotes.len(), 1, "一块一段：{quotes:?}");
        assert_eq!(quotes[0], "于2026年下水", "同一块里真正的句子优先");

        utopia_store::documents::delete(&pool, f.kb, doc, None).await?;
        let invalidated = |id: Uuid| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, bool>(
                    "SELECT invalidated_at IS NOT NULL FROM facts WHERE id = $1",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
            }
        };
        assert!(!invalidated(canonical).await?, "本名不随文档走");
        assert!(invalidated(alias).await?, "只凭这篇读到的别名作废");
        assert!(invalidated(fact).await?);
        anyhow::Ok(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}
