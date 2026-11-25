//! Storage and boundaries of `inverseOf` / `subPropertyOf` — run against a real DB.
//!
//! **The first thing guarded here is knowledge-base isolation.** The foreign key on the column is written as
//! `REFERENCES relation_types(id)`, which does not know about `kb_id`: at the database level, a relation in KB A
//! can perfectly well point at a relation in KB B. The RDF import path naturally cannot do this (it looks up by IRI within this KB), but
//! the HTTP interface takes a bare UUID — if that is not blocked, then holding any id lets the reasoner
//! read axioms across KBs, and derived edges land in this KB carrying another KB's semantics.
//!
//! The frontend only lists this KB's relations. **That is interface politeness, not a boundary**: the interface itself takes a UUID,
//! and a curl gets around it. So the check must live in the store layer, and the test must live here too.
//!
//! The other three rules are the same family of shape constraints: a property has no inverse (its object is a literal, so the notion does not apply),
//! a property cannot be another's inverse, and a sub-property cannot be itself (the DB has a CHECK, but hitting it gives a 500,
//! so a human-readable error must be given before the CHECK is reached).

use sqlx::PgPool;
use utopia_core::models::RelationAxioms;
use uuid::Uuid;

/// Two KBs under one org. **Both are required** — the protagonist of this test group is exactly the cross-KB line.
async fn two_kbs(pool: &PgPool) -> anyhow::Result<(Uuid, Uuid, Uuid)> {
    let (org, ws, a, b) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'link-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'link-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    for (id, name) in [(a, "link-a"), (b, "link-b")] {
        sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(ws)
            .bind(name)
            .execute(pool)
            .await?;
    }
    Ok((org, a, b))
}

/// Creates the simplest possible relation, with no chain at all.
async fn plain(pool: &PgPool, kb: Uuid, key: &str) -> anyhow::Result<Uuid> {
    Ok(utopia_store::ontology::create_relation_type(
        pool,
        kb,
        key,
        key,
        "state",
        RelationAxioms::default(),
        "",
        "relation",
        &[],
        &[],
        None,
        None,
    )
    .await?)
}

#[tokio::test]
async fn a_relation_points_only_inside_its_own_kb() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, kb_a, kb_b) = two_kbs(&pool).await?;

    let run = async {
        let works_at = plain(&pool, kb_a, "works_at").await?;
        let employs = plain(&pool, kb_a, "employs").await?;
        let ceo_of = plain(&pool, kb_a, "ceo_of").await?;
        // A relation in the other KB. The name is the same — **a same name in a different KB is exactly the kind most easily taken
        // for one's own**
        let foreign = plain(&pool, kb_b, "employs").await?;

        // ---- 1. Cross-KB pointing: should be rejected at creation time
        let err = utopia_store::ontology::create_relation_type(
            &pool,
            kb_a,
            "leaks",
            "leaks",
            "state",
            RelationAxioms {
                inverse_of: Some(foreign),
                ..Default::default()
            },
            "",
            "relation",
            &[],
            &[],
            None,
            None,
        )
        .await
        .expect_err("指向别的知识库的关系必须被拒");
        assert!(
            format!("{err:?}").contains("unknown_relation"),
            "**拒绝的理由不能透露那个 id 存在于别处**——不区分「不存在」\
             与「在别的库」，实得 {err:?}"
        );
        let leaked: i64 =
            sqlx::query_scalar("SELECT count(*) FROM relation_types WHERE kb_id = $1 AND key = $2")
                .bind(kb_a)
                .bind("leaks")
                .fetch_one(&pool)
                .await?;
        assert_eq!(leaked, 0, "a rejected attempt must not leave a half row");

        // ---- 2. Cross-KB pointing: should be rejected on update too
        let err = utopia_store::ontology::update_relation_type(
            &pool,
            kb_a,
            works_at,
            "works at",
            "state",
            RelationAxioms {
                sub_property_of: Some(foreign),
                ..Default::default()
            },
            "",
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("改成指向别的知识库同样必须被拒");
        assert!(
            format!("{err:?}").contains("unknown_relation"),
            "实得 {err:?}"
        );

        // ---- 3. Within this KB: can be written, and read back
        utopia_store::ontology::update_relation_type(
            &pool,
            kb_a,
            works_at,
            "works at",
            "state",
            RelationAxioms {
                inverse_of: Some(employs),
                ..Default::default()
            },
            "",
            None,
            None,
            None,
            None,
        )
        .await?;
        utopia_store::ontology::update_relation_type(
            &pool,
            kb_a,
            ceo_of,
            "ceo of",
            "state",
            RelationAxioms {
                sub_property_of: Some(works_at),
                ..Default::default()
            },
            "",
            None,
            None,
            None,
            None,
        )
        .await?;
        let views = utopia_store::ontology::relation_type_views(&pool, kb_a).await?;
        let find = |id: Uuid| views.iter().find(|v| v.id == id).expect("stored");
        assert_eq!(
            find(works_at).inverse_of,
            Some(employs),
            "**视图必须回这两个值**——下拉框要显示当前选的是谁，\
             读不回来的话打开表单是空的，保存一次就把声明抹了"
        );
        assert_eq!(find(ceo_of).sub_property_of, Some(works_at));

        // ---- 4. Default = clear, the same rule as the six axioms above
        utopia_store::ontology::update_relation_type(
            &pool,
            kb_a,
            works_at,
            "works at",
            "state",
            RelationAxioms::default(),
            "",
            None,
            None,
            None,
            None,
        )
        .await?;
        let views = utopia_store::ontology::relation_type_views(&pool, kb_a).await?;
        assert_eq!(
            views.iter().find(|v| v.id == works_at).unwrap().inverse_of,
            None,
            "不传 = 清空。一半覆盖一半保留，会让「我把逆去掉了」\
             和「我没碰逆」长得一模一样"
        );

        // ---- 5. Properties: cannot have a chain, and cannot be the target of a chain
        let salary = utopia_store::ontology::create_relation_type(
            &pool,
            kb_a,
            "salary",
            "salary",
            "state",
            RelationAxioms::default(),
            "",
            "attribute",
            // A property must attach at least one class; here we borrow an existing class
            &[class(&pool, kb_a).await?],
            &[],
            Some("number"),
            None,
        )
        .await?;
        let err = utopia_store::ontology::update_relation_type(
            &pool,
            kb_a,
            ceo_of,
            "ceo of",
            "state",
            RelationAxioms {
                inverse_of: Some(salary),
                ..Default::default()
            },
            "",
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("属性不能当逆——它的宾语是字面值，反过来指回来无从谈起");
        assert!(
            format!("{err:?}").contains("link_target_is_attr"),
            "实得 {err:?}"
        );
        let err = utopia_store::ontology::update_relation_type(
            &pool,
            kb_a,
            salary,
            "salary",
            "state",
            RelationAxioms {
                sub_property_of: Some(ceo_of),
                ..Default::default()
            },
            "",
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("属性自己也不能有父属性");
        assert!(
            format!("{err:?}").contains("attr_has_no_link"),
            "实得 {err:?}"
        );

        // ---- 6. A sub-property cannot be itself. **The DB has a CHECK, but that is a 500**
        let err = utopia_store::ontology::update_relation_type(
            &pool,
            kb_a,
            ceo_of,
            "ceo of",
            "state",
            RelationAxioms {
                sub_property_of: Some(ceo_of),
                ..Default::default()
            },
            "",
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("自己不能是自己的父属性");
        assert!(
            format!("{err:?}").contains("sub_property_self"),
            "得在撞上 CHECK 之前给出人话，而不是让人看到一个 500，实得 {err:?}"
        );

        // Being its own inverse is **not blocked**: that amounts to symmetric, which is a legal declaration.
        // R0 would suggest switching to `symmetric`, which is more direct, but that is a hint, not an error
        utopia_store::ontology::update_relation_type(
            &pool,
            kb_a,
            employs,
            "employs",
            "state",
            RelationAxioms {
                inverse_of: Some(employs),
                ..Default::default()
            },
            "",
            None,
            None,
            None,
            None,
        )
        .await?;

        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    let _ = kb_b;
    run
}

/// Borrow a class to serve as a property's domain — a property cannot be created without a class.
async fn class(pool: &PgPool, kb: Uuid) -> anyhow::Result<Uuid> {
    Ok(utopia_store::ontology::create_entity_type(
        pool,
        kb,
        "person",
        "Person",
        "#888888",
        "circle",
        &[],
        "",
    )
    .await?)
}
