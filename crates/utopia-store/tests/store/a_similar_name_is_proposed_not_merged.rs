//! Name-vector recall (0041 decision 3, channel 2, slice 2): **similar names come and report in, but are only proposed, never merged.**
//!
//! The store has 海洋探测器1号 (Ocean Probe 1), and a new document writes only 海探1. Literal recall (channel 1) does not reach it — it is neither equal,
//! nor shorter than the four-character containment threshold — so before this it silently grew a second entity (#709). Now the mention's
//! name vector takes its nearest neighbour among the store's name vectors; if close enough, it queues a pair `name_vector|<cosine>` for the arbiter;
//! the mention itself still takes the literal path as usual (here, a new entity). Whether they are one thing is judged by the arbiter on two profiles.
//!
//! Only testable against a database: the nearest neighbour is `<=>` in SQL. Skipped when there is no `UTOPIA_DATABASE_URL`; sets up and tears down its own data.
//! The vectors are all hand-placed in three dimensions — it tests the rules of recall and proposal, not how near any embedding model considers things.

use sqlx::PgPool;
use utopia_store::resolution::ReviewStage;
use utopia_store::{name_vectors, names};
use uuid::Uuid;

struct Fx {
    org: Uuid,
    kb: Uuid,
    device: Uuid,
    person: Uuid,
    probe: Uuid,
    captain: Uuid,
}

async fn seed(pool: &PgPool, tag: &str) -> anyhow::Result<Fx> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (device, person) = (Uuid::now_v7(), Uuid::now_v7());
    let (probe, captain) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, $2)")
        .bind(org)
        .bind(tag)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(org)
        .bind(tag)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, $3)")
        .bind(kb)
        .bind(ws)
        .bind(tag)
        .execute(pool)
        .await?;
    // Two major classes: device (type_family does not recognize it, None) and person (Person)
    for (id, key, label) in [(device, "device", "Device"), (person, "person", "Person")] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(pool)
            .await?;
    }
    for (id, type_id, name) in [
        (probe, device, "海洋探测器1号"),
        (captain, person, "海洋探测队长"),
    ] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(type_id)
        .bind(name)
        .execute(pool)
        .await?;
        let fact = names::record(pool, kb, id, name, None, None)
            .await?
            .expect("a named entity gets a name fact");
        // The two names' vectors are deliberately the same: only the major class can tell them apart
        name_vectors::set(pool, kb, &[(fact, id, vec![1.0, 0.0, 0.0])]).await?;
    }
    Ok(Fx {
        org,
        kb,
        device,
        person,
        probe,
        captain,
    })
}

async fn teardown(pool: &PgPool, f: &Fx) -> anyhow::Result<()> {
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

fn vector_reviews(r: &utopia_store::resolution::Resolution) -> Vec<(Uuid, f32)> {
    r.reviews
        .iter()
        .filter(|v| v.reason.starts_with("name_vector|"))
        .map(|v| (v.other_id, v.score))
        .collect()
}

fn contained(r: &utopia_store::resolution::Resolution) -> Vec<Uuid> {
    r.reviews
        .iter()
        .filter(|v| v.reason.starts_with("contains|"))
        .map(|v| v.other_id)
        .collect()
}

/// Similar names: create a new entity, queue a pair for the arbiter, never silently merge
#[tokio::test]
async fn a_near_name_creates_an_entity_and_proposes_a_pair() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "name-vector-near").await?;
    let run = async {
        let near: Vec<f32> = vec![0.95, 0.31, 0.0];
        let r = utopia_store::resolution::resolve_mention(
            &pool,
            f.kb,
            Some(f.device),
            "海探1",
            None,
            Some(&near),
            None,
            &[],
        )
        .await?;
        assert!(
            r.created,
            "no literal match, mention should create a new entity"
        );
        assert_ne!(
            r.entity_id, f.probe,
            "向量召回只提议，不能直接归并到 海洋探测器1号"
        );
        let proposed = vector_reviews(&r);
        assert!(
            proposed.iter().any(|(id, _)| *id == f.probe),
            "海洋探测器1号 该被提议：{:?}",
            r.reviews
        );
        // In the fixture the two names have the same vector, and Device cannot recognize the major class so it does not block Person: 海洋探测队长 also comes in.
        // This is not a leak, it is the rule — when the major class cannot be told apart, it is better to ask more; the blocking when it can be told apart is in the next test
        assert_eq!(
            proposed.len(),
            2,
            "Device 无大类，不拦同向量的 Person：{:?}",
            r.reviews
        );
        assert!(
            proposed.iter().all(|(_, s)| *s >= name_vectors::SIM_FLOOR),
            "分数是余弦本身"
        );
        assert!(
            r.reviews
                .iter()
                .all(|v| v.stage == ReviewStage::Adjudicating),
            "名字相近的对交给批量裁决器，不是人"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// Not proposed when the major classes differ: however similar a device's name is, it is not that person
#[tokio::test]
async fn a_near_name_of_another_family_is_not_proposed() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "name-vector-family").await?;
    let run = async {
        // The mention is a person; 海洋探测器1号's class Device does not recognize the major class (None), so by the rule it is not blocked;
        // 海洋探测队长 is a Person, same class, which cannot block it — so both come in. Only with an Organization
        // mention can one see the Person being blocked:
        let organization = Uuid::now_v7();
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'organization', 'Organization')")
            .bind(organization)
            .bind(f.kb)
            .execute(&pool)
            .await?;
        let near: Vec<f32> = vec![1.0, 0.0, 0.0];
        let r = utopia_store::resolution::resolve_mention(
            &pool,
            f.kb,
            Some(organization),
            "海洋探测公司",
            None,
            Some(&near),
            None,
            &[],
        )
        .await?;
        let proposed: Vec<Uuid> = vector_reviews(&r).into_iter().map(|(id, _)| id).collect();
        assert!(
            !proposed.contains(&f.captain),
            "Person 与 Organization 大类不同，海洋探测队长 不该被提议：{:?}",
            r.reviews
        );
        assert!(
            proposed.contains(&f.probe),
            "Device 认不出大类，不拦；海洋探测器1号 照常提议：{:?}",
            r.reviews
        );
        let _ = f.person;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// Not close enough, not proposed; those given no vector keep as before
#[tokio::test]
async fn a_far_name_or_no_vector_proposes_nothing() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "name-vector-far").await?;
    let run = async {
        let far: Vec<f32> = vec![0.0, 1.0, 0.0];
        let r = utopia_store::resolution::resolve_mention(
            &pool,
            f.kb,
            Some(f.device),
            "深海机器人",
            None,
            Some(&far),
            None,
            &[],
        )
        .await?;
        assert!(
            vector_reviews(&r).is_empty(),
            "余弦 0 低于下限，不提议：{:?}",
            r.reviews
        );
        let r = utopia_store::resolution::resolve_mention(
            &pool,
            f.kb,
            Some(f.device),
            "海探1",
            None,
            None,
            None,
            &[],
        )
        .await?;
        assert!(
            vector_reviews(&r).is_empty(),
            "没给名字向量就不走通道 2：{:?}",
            r.reviews
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// Literal hits among candidates are channel 1's business: the same name is not reconsidered through channel 2
#[tokio::test]
async fn the_same_literal_name_is_not_proposed_twice() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "name-vector-literal").await?;
    let run = async {
        let same: Vec<f32> = vec![1.0, 0.0, 0.0];
        // A second entity with the same name stays a channel-1 decision, including
        // when the mention carries Markdown. Neither candidate is excluded here.
        sqlx::query(
            "UPDATE entities SET canonical_name = '海洋探测器1号', type_id = $2 WHERE id = $1",
        )
        .bind(f.captain)
        .bind(f.device)
        .execute(&pool)
        .await?;
        sqlx::query(
            "UPDATE facts SET object_value = '{\"value\":\"海洋探测器1号\"}' WHERE subject_id = $1",
        )
        .bind(f.captain)
        .execute(&pool)
        .await?;
        for mention in ["海洋探测器1号", "**海洋探测器1号**"] {
            let r = utopia_store::resolution::resolve_mention(
                &pool,
                f.kb,
                Some(f.device),
                mention,
                None,
                Some(&same),
                None,
                &[],
            )
            .await?;
            assert!(!r.created);
            assert!(
                vector_reviews(&r).is_empty(),
                "a namesake must not be reconsidered by name vector: {:?}",
                r.reviews
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// Vector backfill round trip: a newly recorded name fact is in `pending`, and once a vector is written it is gone from there. A hole found by an end-to-end run —
/// previously `pending` sorted by a column that does not exist, so every document's vector backfill silently failed, and channel 2 never fired
#[tokio::test]
async fn a_new_name_is_pending_until_its_vector_is_set() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "name-vector-pending").await?;
    let run = async {
        let fresh = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, '远洋重工')",
        )
        .bind(fresh)
        .bind(f.kb)
        .bind(f.device)
        .execute(&pool)
        .await?;
        let fact = names::record(&pool, f.kb, fresh, "远洋重工", None, None)
            .await?
            .expect("a name fact");
        let pending = name_vectors::pending(&pool, f.kb, 100).await?;
        assert!(
            pending.iter().any(|(fid, eid, name)| *fid == fact && *eid == fresh && name == "远洋重工"),
            "新记的名字该在待补清单里：{pending:?}"
        );
        // The two names with existing vectors in the fixture are not on the list
        assert!(pending.iter().all(|(_, eid, _)| *eid != f.probe && *eid != f.captain));
        name_vectors::set(&pool, f.kb, &[(fact, fresh, vec![0.0, 0.0, 1.0])]).await?;
        let after = name_vectors::pending(&pool, f.kb, 100).await?;
        assert!(after.iter().all(|(fid, _, _)| *fid != fact), "once a vector is written it should not be pending backfill");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// A name the previous document did not backfill is backfilled by the next one as well: `pending` takes every name in the whole store with no vector, earliest recorded first,
/// not just the one recorded by this document. Names that already have a vector are not in it (#1296 kept this retry when it changed this query)
#[tokio::test]
async fn a_name_left_without_a_vector_earlier_is_pending_before_a_new_one() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "name-vector-backlog").await?;
    let run = async {
        let mut recorded = Vec::new();
        for name in ["远洋重工", "深蓝船舶"] {
            let entity = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
            )
            .bind(entity)
            .bind(f.kb)
            .bind(f.device)
            .bind(name)
            .execute(&pool)
            .await?;
            let fact = names::record(&pool, f.kb, entity, name, None, None)
                .await?
                .expect("a name fact");
            recorded.push((fact, entity, name.to_string()));
        }

        assert_eq!(name_vectors::pending(&pool, f.kb, 100).await?, recorded);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// A pair the arbiter has judged "not the same" is not queued again when an entity is mentioned again (#1104). The name-vector channel runs on every mention,
/// while the review table deduplicates only rows that are pending: a pair the arbiter once kept apart would become a new pending row every time the next document mentioned it.
/// A pair not yet judged is unaffected and is still proposed as usual
#[tokio::test]
async fn a_pair_kept_apart_is_not_proposed_again() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "name-vector-kept-apart").await?;
    let run = async {
        let near: Vec<f32> = vec![0.95, 0.31, 0.0];
        let mention = || {
            utopia_store::resolution::resolve_mention(
                &pool,
                f.kb,
                Some(f.device),
                "海探1",
                None,
                Some(&near),
                None,
                &[],
            )
        };
        let first = mention().await?;
        assert!(first.created);
        for v in &first.reviews {
            utopia_store::resolution::create_review(
                &pool,
                f.kb,
                first.entity_id,
                v.other_id,
                v.score,
                &v.reason,
                v.stage,
            )
            .await?;
        }
        // The arbiter judges that 海探1 and 海洋探测器1号 are not the same; the pair with 海洋探测队长 has not been judged yet
        let (review,): (Uuid,) = sqlx::query_as(
            "SELECT id FROM resolution_reviews WHERE kb_id = $1 AND status = 'pending'
              AND $2 IN (left_id, right_id)",
        )
        .bind(f.kb)
        .bind(f.probe)
        .fetch_one(&pool)
        .await?;
        utopia_store::resolution::close_review_auto(&pool, review, "kept", "kept_apart|test")
            .await?;

        let again = mention().await?;
        assert!(
            !again.created,
            "the same literal name goes to the entity created in the previous round"
        );
        assert_eq!(again.entity_id, first.entity_id);
        let proposed: Vec<Uuid> = vector_reviews(&again)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(
            !proposed.contains(&f.probe),
            "分开过的一对不该再提议：{:?}",
            again.reviews
        );
        assert!(
            proposed.contains(&f.captain),
            "还没判的一对照常提议（入队时对 pending 幂等）：{:?}",
            again.reviews
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// Names differing by only one number do not pair (#1104): nearly coincident as vectors, mutually contained as literals, yet not the same thing.
/// Each of the two channels guards it once; a short name with no number on one side is still proposed as usual
#[tokio::test]
async fn names_that_carry_different_numbers_are_not_proposed() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "name-vector-numbers").await?;
    let run = async {
        let resolve = |name: &'static str, vector: Option<&'static [f32]>| {
            utopia_store::resolution::resolve_mention(
                &pool,
                f.kb,
                Some(f.device),
                name,
                None,
                vector,
                None,
                &[],
            )
        };
        // Name vector: the vector of 2号 is identical to that of 1号, and it still is not proposed; 队长 with no number is unaffected
        let second = resolve("海洋探测器2号", Some(&[1.0, 0.0, 0.0])).await?;
        let proposed: Vec<Uuid> = vector_reviews(&second)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(
            !proposed.contains(&f.probe),
            "1号 与 2号 不该配对：{:?}",
            second.reviews
        );
        assert!(
            proposed.contains(&f.captain),
            "没有数字的名字照常提议：{:?}",
            second.reviews
        );

        // Containment: `migration 39` is a substring of `migration 395`, but not its short name
        let long = resolve("migration 395", None).await?;
        let short = resolve("migration 39", None).await?;
        assert!(short.created);
        assert!(
            !contained(&short).contains(&long.entity_id),
            "数字不同的包含不配对：{:?}",
            short.reviews
        );
        // No number on one side: a short name against its full name is exactly what this channel should catch
        let full = resolve("星云推理平台 2.0", None).await?;
        let brief = resolve("星云推理平台", None).await?;
        assert!(
            contained(&brief).contains(&full.entity_id),
            "简称照常配到带版本号的全称上：{:?}",
            brief.reviews
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// `_` in a name is compared literally, not treated as a wildcard (#1293): `src/a_b.ts` is not contained in `src/axb.ts::adds`,
/// and conversely `a_b.ts` is not in the latter either; the truly contained `src/a_b.ts` pairs as usual
#[tokio::test]
async fn an_underscore_in_a_name_is_not_a_wildcard() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "contain-underscore").await?;
    let run = async {
        let resolve = |name: &'static str| {
            utopia_store::resolution::resolve_mention(
                &pool,
                f.kb,
                Some(f.device),
                name,
                None,
                None,
                None,
                &[],
            )
        };
        let file = resolve("src/a_b.ts").await?;
        let other = resolve("src/axb.ts::adds").await?;
        assert!(
            !contained(&other).contains(&file.entity_id),
            "库里名字的 _ 不匹配 x：{:?}",
            other.reviews
        );
        let brief = resolve("a_b.ts").await?;
        assert_eq!(
            contained(&brief),
            vec![file.entity_id],
            "mention 的 _ 不匹配 x，真包含的照常配对：{:?}",
            brief.reviews
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// An alias is also a bridge for recall: `Holmes` is an alias of `Sherlock Holmes`, and `Mr. Holmes` contains `Holmes`,
/// but does not contain the primary name `Sherlock Holmes`, nor is it contained by it
#[tokio::test]
async fn mr_holmes_is_paired_with_sherlock_holmes_through_its_alias_holmes() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "contain-alias").await?;
    let run = async {
        let resolve = |name: &'static str| {
            utopia_store::resolution::resolve_mention(
                &pool,
                f.kb,
                Some(f.device),
                name,
                None,
                None,
                None,
                &[],
            )
        };
        let detective = resolve("Sherlock Holmes").await?;
        names::record(&pool, f.kb, detective.entity_id, "Holmes", None, None).await?;
        let mr = resolve("Mr. Holmes").await?;
        assert_eq!(
            contained(&mr),
            vec![detective.entity_id],
            "别名 Holmes 被 Mr. Holmes 包含：{:?}",
            mr.reviews
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// An entity created from a description has no name facts (0044), so the description text is not a bridge for recall;
/// the same text on the `known_as` of an ordinary entity pairs as usual
#[tokio::test]
async fn a_described_entity_is_not_paired_by_containment() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "contain-described").await?;
    let run = async {
        let described = utopia_store::resolution::create_described(
            &pool,
            f.kb,
            "the museum Sherlock Holmes fans visit",
            None,
        )
        .await?;
        let named = utopia_store::resolution::resolve_mention(
            &pool,
            f.kb,
            Some(f.device),
            "the museum Sherlock Holmes fans visit",
            None,
            None,
            None,
            &[],
        )
        .await?;
        let mention = utopia_store::resolution::resolve_mention(
            &pool,
            f.kb,
            Some(f.device),
            "Sherlock Holmes",
            None,
            None,
            None,
            &[],
        )
        .await?;
        assert_eq!(
            contained(&mention),
            vec![named.entity_id],
            "描述建的 {described} 不配对，同文字的普通实体配对：{:?}",
            mention.reviews
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}

/// Latin names count containment only at word boundaries (#1104); Han characters still match as substrings
#[tokio::test]
async fn a_latin_name_is_contained_only_at_word_edges() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool, "contain-word-edges").await?;
    let run = async {
        let resolve = |name: &'static str| {
            utopia_store::resolution::resolve_mention(
                &pool,
                f.kb,
                Some(f.device),
                name,
                None,
                None,
                None,
                &[],
            )
        };
        // (name in the store, its alias, the new name, whether it is paired)
        for (stored, alias, new, want) in [
            ("SERVICE_TOKEN", None, "token", false),
            ("install_scheduled_task.ps1", None, "task", false),
            ("AreaScorecard", None, "area", false),
            ("Sherlock Holmes", None, "Holmes", true),
            ("星云科技上海研究院", None, "上海研究院", true),
            ("pump", None, "pumpkin soup", false),
            ("Watson", None, "Doctor Watson", true),
            (
                "Scotland Yard",
                Some("Inspector Lestrade"),
                "Lestrade",
                true,
            ),
            ("Gregson file", Some("gregson_notes"), "notes", false),
            ("Baker Street", Some("Hudson"), "Mrs Hudson", true),
            ("Prefect file", Some("Ford"), "Fordham Road", false),
        ] {
            let old = resolve(stored).await?;
            if let Some(alias) = alias {
                utopia_store::names::record(&pool, f.kb, old.entity_id, alias, None, None).await?;
            }
            let r = resolve(new).await?;
            assert_eq!(
                contained(&r).contains(&old.entity_id),
                want,
                "{new} against {stored}: {:?}",
                r.reviews
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown(&pool, &f).await?;
    run
}
