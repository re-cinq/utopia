//! The near-neighbour path of type resolution (#514), run against a real DB.
//!
//! The rule is: the answer to resolution is a function of the ledger, independent of the order or timing in which candidates are fetched.
//! So fetching concurrently is only safe when "the same KB gives the same batch of neighbours". Four things are pinned here:
//! - **Order**: a batch of subjects' neighbours come back in the order they were sent in; downstream verdicts read in that order
//! - **The concurrency cap does not change the answer**: a cap of 1 and a cap of 8 give the same result; so does having an index or not
//! - **Three gates in the SQL**: an entity is not its own neighbour; an untyped entity is not used as evidence; ones from the same document are marked
//! - **Memory of descendant sets**: within one batch, each coarse class is asked only once; a class with no coarse class does not enter the table
//!
//! One more thing only a live DB can verify: two pooled connections can still run a whole batch to completion, without hitting a connection-acquisition timeout.

use pgvector::Vector;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::collections::HashSet;
use utopia_store::resolution::{self, DescendantsMemo};
use utopia_store::vector_index::{self, Target};
use uuid::Uuid;

/// The dimension this test owns exclusively: other DB tests use 3 dimensions
const DIMS: usize = 5;
const TYPED: usize = 40;

fn lcg(seed: &mut u64) -> f32 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5
}

struct Fixture {
    org: Uuid,
    kb: Uuid,
    thing: Uuid,
    person: Uuid,
    company: Uuid,
    employee: Uuid,
    /// Three subjects awaiting resolution
    subjects: Vec<Uuid>,
    /// Subjects whose own vector is 3-dimensional
    three_dims_subject: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (thing, person, company, employee) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let tag = Uuid::now_v7();

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'gather-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'gather-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'gather-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    for (id, key) in [
        (thing, "thing"),
        (person, "person"),
        (company, "company"),
        (employee, "employee"),
    ] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $3)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .execute(pool)
            .await?;
    }
    for (child, parent) in [(person, thing), (company, thing), (employee, person)] {
        sqlx::query("INSERT INTO entity_type_parents (child_id, parent_id) VALUES ($1, $2)")
            .bind(child)
            .bind(parent)
            .execute(pool)
            .await?;
    }

    let mut seed = 0xfeed_u64;
    let mut vector = |dims: usize| -> Vector {
        Vector::from((0..dims).map(|_| lcg(&mut seed)).collect::<Vec<f32>>())
    };
    let insert = |id: Uuid, name: String, type_id: Option<Uuid>, v: Vector| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO entities (id, kb_id, type_id, canonical_name, profile_embedding, profile_n)
                 VALUES ($1, $2, $3, $4, $5, 1)",
            )
            .bind(id)
            .bind(kb)
            .bind(type_id)
            .bind(name)
            .bind(v)
            .execute(&pool)
            .await
        }
    };
    let types = [person, company, employee];
    let mut typed = Vec::with_capacity(TYPED);
    for i in 0..TYPED {
        let id = Uuid::now_v7();
        insert(id, format!("typed {i}"), Some(types[i % 3]), vector(DIMS)).await?;
        typed.push(id);
    }
    let subjects: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
    for (i, s) in subjects.iter().enumerate() {
        insert(*s, format!("subject {i}"), Some(thing), vector(DIMS)).await?;
    }
    let untyped = Uuid::now_v7();
    insert(untyped, "untyped".into(), None, vector(DIMS)).await?;
    let three_dims = Uuid::now_v7();
    insert(three_dims, "three dims".into(), Some(person), vector(3)).await?;
    let three_dims_subject = Uuid::now_v7();
    insert(
        three_dims_subject,
        "three dims subject".into(),
        Some(thing),
        vector(3),
    )
    .await?;

    // Subject 0 and typed[0] are in the same document: one fact, with its evidence on a chunk of that document
    let same_doc_neighbour = typed[0];
    let (doc, chunk, fact) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256, status)
         VALUES ($1, $2, 'shared.md', $3, 'ready')",
    )
    .bind(doc)
    .bind(kb)
    .bind(format!("sha-{tag}"))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, 'a sentence')",
    )
    .bind(chunk)
    .bind(kb)
    .bind(doc)
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO facts (id, kb_id, subject_id, object_id) VALUES ($1, $2, $3, $4)")
        .bind(fact)
        .bind(kb)
        .bind(subjects[0])
        .bind(same_doc_neighbour)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id)
         VALUES ($1, $2, 'a sentence', $3)",
    )
    .bind(fact)
    .bind(chunk)
    .bind(doc)
    .execute(pool)
    .await?;

    Ok(Fixture {
        org,
        kb,
        thing,
        person,
        company,
        employee,
        subjects,
        three_dims_subject,
    })
}

type Neighbours = Vec<(String, Uuid, String, f64, bool)>;

fn names(list: &Neighbours) -> Vec<&str> {
    list.iter().map(|(n, ..)| n.as_str()).collect()
}

#[tokio::test]
async fn the_neighbours_come_back_in_order_and_the_gates_hold() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        vector_index::drop(&pool, Target::EntityProfiles, DIMS).await?;

        // ---- 1. Ask one at a time, as the baseline
        let mut serial: Vec<Neighbours> = Vec::new();
        for s in &f.subjects {
            serial.push(resolution::nearest_typed_entities(&pool, f.kb, *s, 10).await?);
        }
        for (i, list) in serial.iter().enumerate() {
            assert_eq!(list.len(), 10, "主语 {i} 该有十个邻居");
            let me = format!("subject {i}");
            assert!(
                list.iter().all(|(n, ..)| *n != me),
                "自己不是自己的邻居（别的主语可以是：它们挂着 thing，是已定类的）"
            );
            assert!(
                list.iter().all(|(n, ..)| n != "untyped"),
                "没判出类型的不当证据"
            );
            assert!(
                list.iter().all(|(n, ..)| n != "three dims"),
                "另一维度的画像不是候选"
            );
        }
        // Among subject 0's neighbours, typed 0 is in the same document as it, and none of the others are
        let flagged: Vec<&str> = serial[0]
            .iter()
            .filter(|(_, _, _, _, same)| *same)
            .map(|(n, ..)| n.as_str())
            .collect();
        let has_typed0 = serial[0].iter().any(|(n, ..)| n == "typed 0");
        if has_typed0 {
            assert_eq!(flagged, vec!["typed 0"], "同一篇文档的要标出来，且只标它");
        } else {
            assert!(flagged.is_empty(), "不在邻居里就没什么可标");
        }
        // Subjects whose own vector is 3-dimensional: there are no typed neighbours of 3 dimensions to compare against... except that one 3-dimensional one
        let odd = resolution::nearest_typed_entities(&pool, f.kb, f.three_dims_subject, 10).await?;
        assert_eq!(
            names(&odd),
            vec!["three dims"],
            "3 维的查询只找 3 维的，也不报错"
        );

        // ---- 2. A batch fetch: order and cap
        let gathered_1 =
            resolution::nearest_typed_for_each_with(&pool, f.kb, &f.subjects, 10, 1).await?;
        let gathered_8 =
            resolution::nearest_typed_for_each_with(&pool, f.kb, &f.subjects, 10, 8).await?;
        assert_eq!(gathered_1.len(), f.subjects.len());
        for i in 0..f.subjects.len() {
            assert_eq!(
                names(&gathered_1[i]),
                names(&serial[i]),
                "上限 1：第 {i} 个位置是第 {i} 个主语的邻居"
            );
            assert_eq!(
                names(&gathered_8[i]),
                names(&serial[i]),
                "上限 8：第 {i} 个位置是第 {i} 个主语的邻居"
            );
        }

        // ---- 3. Two pooled connections can still finish all sixty
        let tiny = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(&url)
            .await?;
        let sixty: Vec<Uuid> = f.subjects.iter().cycle().take(60).copied().collect();
        let over_tiny = resolution::nearest_typed_for_each_with(&tiny, f.kb, &sixty, 10, 8).await?;
        assert_eq!(over_tiny.len(), 60);
        for (i, list) in over_tiny.iter().enumerate() {
            assert_eq!(names(list), names(&serial[i % 3]));
        }
        tiny.close().await;

        // ---- 4. The same answer once the index is in place
        let built = vector_index::build(&pool, Target::EntityProfiles, DIMS).await?;
        assert!(built.created);
        assert_eq!(built.name, "entities_profile_embedding_hnsw_5");
        let indexed = resolution::nearest_typed_for_each(&pool, f.kb, &f.subjects, 10).await?;
        for i in 0..f.subjects.len() {
            assert_eq!(
                names(&indexed[i]),
                names(&serial[i]),
                "索引改了主语 {i} 的邻居"
            );
        }

        // ---- 5. Descendant sets are remembered per coarse class within a batch
        let mut memo = DescendantsMemo::default();
        let direct = |root: Uuid| {
            let pool = pool.clone();
            async move {
                Ok::<HashSet<Uuid>, anyhow::Error>(
                    resolution::descendants_of(&pool, f.kb, root)
                        .await?
                        .into_iter()
                        .collect(),
                )
            }
        };
        assert_eq!(
            memo.get(&pool, f.kb, Some(f.thing)).await?,
            direct(f.thing).await?
        );
        assert_eq!(
            memo.get(&pool, f.kb, Some(f.person)).await?,
            [f.person, f.employee].into_iter().collect::<HashSet<_>>()
        );
        assert_eq!(
            memo.get(&pool, f.kb, Some(f.company)).await?,
            direct(f.company).await?
        );
        assert_eq!(memo.len(), 3);
        assert!(
            memo.get(&pool, f.kb, None).await?.is_empty(),
            "没有粗类：没有后代这个轴"
        );
        assert_eq!(memo.len(), 3, "没有粗类的不进表");
        // The ontology changed within the batch: memory still returns the copy that was asked for, while a direct ask gives the new one — that is what "remembering" means
        let intern = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'intern', 'intern')",
        )
        .bind(intern)
        .bind(f.kb)
        .execute(&pool)
        .await?;
        sqlx::query("INSERT INTO entity_type_parents (child_id, parent_id) VALUES ($1, $2)")
            .bind(intern)
            .bind(f.person)
            .execute(&pool)
            .await?;
        assert!(!memo
            .get(&pool, f.kb, Some(f.person))
            .await?
            .contains(&intern));
        assert!(direct(f.person).await?.contains(&intern));
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = vector_index::drop(&pool, Target::EntityProfiles, DIMS).await;
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
