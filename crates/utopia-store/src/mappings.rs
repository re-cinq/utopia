//! Semantic layer's "business concept → data asset" mappings (see `docs/decisions/0011`).
//!
//! It used to be a `mapped_to` fact, with the object a JSON document stuffed into `object_value`.
//! The reason it was moved out is written in the comment on the `concept_mappings` CREATE TABLE, in one line: **it is not an assertion about the world, it is configuration**.
//!
//! The difference from the ledger is visible right here: this table **allows in-place edits**. `confirm` changes
//! "whether this configuration has taken effect", not "our understanding of the world has changed" -- so it does not need
//! to be append-only; the version before the change goes into `concept_mapping_revisions` as a record.

use sqlx::PgPool;
use utopia_core::models::{ConceptMapping, MappingRevision};
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// The exploration task proposes a mapping.
///
/// **There is only one per (concept, source)**, enforced by the primary key -- previously this uniqueness was hidden inside `object_value`,
/// invisible to the database, and could only be closed off explicitly by the confirmation flow.
///
/// Entries that someone has already decided on are not overwritten: a rerun of exploration would recompute the one that was rejected, and without this line
/// it would be reset to pending review, which means every run wipes out a person's rejection (`ontology_proposals`
/// hit the same trap there, see `ontology_proposals`).
#[allow(clippy::too_many_arguments)]
pub async fn propose(
    pool: &PgPool,
    kb_id: Uuid,
    concept_id: Uuid,
    source: &str,
    table_name: Option<&str>,
    expr: Option<&str>,
    sql: Option<&str>,
    unit: Option<&str>,
    summary: Option<&str>,
    derived: bool,
) -> AppResult<(Uuid, bool)> {
    // **When the `DO UPDATE ... WHERE` condition is not met, `RETURNING` returns no rows at all.**
    //
    // This is how Postgres actually behaves, not intuition: when the condition blocks the update, that row does not count as
    // touched by this statement, so it does not appear in RETURNING. The test hit this on the spot -- proposing a mapping a second time that was already
    // rejected made `fetch_one` report "no rows returned".
    //
    // So query the id separately: whether the update happens is one thing, "which row this mapping is" is another.
    let existing: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM concept_mappings
          WHERE kb_id = $1 AND concept_id = $2 AND source = $3",
    )
    .bind(kb_id)
    .bind(concept_id)
    .bind(source)
    .fetch_optional(pool)
    .await?;
    let id = existing.map(|(i,)| i).unwrap_or_else(Uuid::now_v7);
    let written = sqlx::query(
        "INSERT INTO concept_mappings
             (id, kb_id, concept_id, source, table_name, expr, sql, unit, summary, derived)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (kb_id, concept_id, source) DO UPDATE
           SET table_name = EXCLUDED.table_name, expr = EXCLUDED.expr,
               sql = EXCLUDED.sql, unit = EXCLUDED.unit,
               summary = EXCLUDED.summary, derived = EXCLUDED.derived,
               updated_at = now()
           WHERE concept_mappings.status = 'proposed'",
    )
    .bind(id)
    .bind(kb_id)
    .bind(concept_id)
    .bind(source)
    .bind(table_name)
    .bind(expr)
    .bind(sql)
    .bind(unit)
    .bind(summary)
    .bind(derived)
    .execute(pool)
    .await?
    .rows_affected();
    // An insert or refresh counts as a write; hitting a row that is already confirmed or rejected, the WHERE blocks the update and the write count is zero --
    // the caller uses this bit to tell "went into pending review" from "the decision still stands"
    Ok((id, written > 0))
}

/// A person writes a definition from scratch (#562). **It is confirmed as it is written**: the person writing it is the one taking the decision,
/// so it does not need to go through review again.
///
/// Previously this table had exploration as its only way in. A data team has its own metrics definition documents but no
/// place to enter them -- and definitions feed the question-answering prompt, which took the wide-table corpus from 1/18 to 17/18 (#520).
///
/// If a row for the same (concept, source) already exists, report a conflict rather than silently overwriting: that row may have been proposed by exploration and
/// already confirmed by a person; overwriting would erase a decision. Use `revise` instead.
/// Exploration cannot overwrite this in turn either: `propose` only refreshes rows that are `proposed`.
#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &PgPool,
    kb_id: Uuid,
    concept_id: Uuid,
    source: &str,
    table_name: Option<&str>,
    expr: Option<&str>,
    sql: Option<&str>,
    unit: Option<&str>,
    summary: Option<&str>,
    derived: bool,
    actor: Uuid,
) -> AppResult<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO concept_mappings
             (id, kb_id, concept_id, source, table_name, expr, sql, unit, summary, derived,
              status, decided_by, decided_at, written_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'confirmed', $11, now(), $11)",
    )
    .bind(id)
    .bind(kb_id)
    .bind(concept_id)
    .bind(source)
    .bind(table_name)
    .bind(expr)
    .bind(sql)
    .bind(unit)
    .bind(summary)
    .bind(derived)
    .bind(actor)
    .execute(pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => AppError::Conflict(
            "A definition for this concept on this source already exists; revise it instead".into(),
        ),
        _ => AppError::Db(e),
    })?;
    Ok(id)
}

/// Still waiting for a person's decision. The Review page reads this.
pub async fn proposed(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<ConceptMapping>> {
    Ok(sqlx::query_as(
        "SELECT m.id, m.concept_id, e.canonical_name AS concept_name, m.source,
                m.table_name, m.expr, m.sql, m.unit, m.summary, m.derived, m.status, m.written_by
         FROM concept_mappings m
         JOIN entities e ON e.id = m.concept_id
         WHERE m.kb_id = $1 AND m.status = 'proposed'
         ORDER BY e.canonical_name, m.source, m.id
         LIMIT $2 OFFSET $3",
    )
    .bind(kb_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

/// Confirmed by a person. Question answering injects these into the system prompt -- **only confirmed definitions are used**,
/// rather than guessing from the schema every time.
pub async fn confirmed(pool: &PgPool, kb_id: Uuid, limit: i64) -> AppResult<Vec<ConceptMapping>> {
    Ok(sqlx::query_as(
        "SELECT m.id, m.concept_id, e.canonical_name AS concept_name, m.source,
                m.table_name, m.expr, m.sql, m.unit, m.summary, m.derived, m.status, m.written_by
         FROM concept_mappings m
         JOIN entities e ON e.id = m.concept_id
         WHERE m.kb_id = $1 AND m.status = 'confirmed'
         ORDER BY e.canonical_name, m.source
         LIMIT $2",
    )
    .bind(kb_id)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// A person has decided.
///
/// **Changing status does not delete the row**: both the confirmation and the rejection happened. The rejection record also has a
/// role that matters right now -- `propose`'s `WHERE status = 'proposed'` relies on it so the row is not reset to pending.
pub async fn decide(
    pool: &PgPool,
    kb_id: Uuid,
    mapping_id: Uuid,
    status: &str,
    actor: Uuid,
) -> AppResult<()> {
    let res = sqlx::query(
        "UPDATE concept_mappings
            SET status = $3, decided_by = $4, decided_at = now(), updated_at = now()
          WHERE id = $2 AND kb_id = $1",
    )
    .bind(kb_id)
    .bind(mapping_id)
    .bind(status)
    .bind(actor)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Edit a confirmed definition. **The previous version is written to revisions first** -- so that when question answering looks back
/// through historical reports, it can answer "how was this number calculated last quarter".
///
/// Store the whole version as a snapshot rather than a diff: reads need "what it was at that time", and answering that from a diff would
/// mean replaying from the start.
#[allow(clippy::too_many_arguments)]
pub async fn revise(
    pool: &PgPool,
    kb_id: Uuid,
    mapping_id: Uuid,
    table_name: Option<&str>,
    expr: Option<&str>,
    sql: Option<&str>,
    unit: Option<&str>,
    summary: Option<&str>,
    derived: bool,
    actor: Uuid,
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    let before: Option<(serde_json::Value,)> = sqlx::query_as(
        "SELECT to_jsonb(m) - 'id' - 'kb_id' FROM concept_mappings m
          WHERE m.id = $2 AND m.kb_id = $1",
    )
    .bind(kb_id)
    .bind(mapping_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((before,)) = before else {
        tx.rollback().await?;
        return Err(AppError::NotFound);
    };
    sqlx::query(
        "INSERT INTO concept_mapping_revisions (id, mapping_id, before, changed_by)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(mapping_id)
    .bind(before)
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE concept_mappings
            SET table_name = $3, expr = $4, sql = $5, unit = $6, summary = $7,
                derived = $8, updated_at = now()
          WHERE id = $2 AND kb_id = $1",
    )
    .bind(kb_id)
    .bind(mapping_id)
    .bind(table_name)
    .bind(expr)
    .bind(sql)
    .bind(unit)
    .bind(summary)
    .bind(derived)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Read by the data mapping page: a page of definitions, filterable by status and keyword.
///
/// **Neither the `proposed` nor the `confirmed` query is enough** -- the first only fetches those awaiting a decision,
/// the second builds the prompt for question answering (no pagination, no filter, capped at 30 rows). People need to see everything,
/// including the ones they themselves rejected: a rejection leaves a record, and it should be visible, otherwise "why this concept
/// was not mapped" could never be answered.
pub async fn page(
    pool: &PgPool,
    kb_id: Uuid,
    status: Option<&str>,
    q: Option<&str>,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<ConceptMapping>, i64)> {
    // Both the concept name and the source name are searchable: what people remember is "GMV", or possibly "the one connected to orders"
    const WHERE: &str = "WHERE m.kb_id = $1
           AND ($2::text IS NULL OR m.status = $2)
           AND ($3::text IS NULL
                OR e.canonical_name ILIKE '%' || $3 || '%'
                OR m.source ILIKE '%' || $3 || '%'
                OR m.table_name ILIKE '%' || $3 || '%')";
    let rows: Vec<ConceptMapping> = sqlx::query_as(&format!(
        "SELECT m.id, m.concept_id, e.canonical_name AS concept_name, m.source,
                m.table_name, m.expr, m.sql, m.unit, m.summary, m.derived, m.status, m.written_by
         FROM concept_mappings m
         JOIN entities e ON e.id = m.concept_id
         {WHERE}
         ORDER BY e.canonical_name, m.source, m.id
         LIMIT $4 OFFSET $5"
    ))
    .bind(kb_id)
    .bind(status)
    .bind(q)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) = sqlx::query_as(&format!(
        "SELECT count(*) FROM concept_mappings m
         JOIN entities e ON e.id = m.concept_id {WHERE}"
    ))
    .bind(kb_id)
    .bind(status)
    .bind(q)
    .fetch_one(pool)
    .await?;
    Ok((rows, total))
}

/// The count for each status. The page's filter bar shows counts, and three separate queries would mean three full-table scans.
pub async fn status_counts(pool: &PgPool, kb_id: Uuid) -> AppResult<(i64, i64, i64)> {
    let row: (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status = 'proposed'),
                count(*) FILTER (WHERE status = 'confirmed'),
                count(*) FILTER (WHERE status = 'rejected')
           FROM concept_mappings WHERE kb_id = $1",
    )
    .bind(kb_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Two pieces of text for retrieval of one definition: one for embedding (name + description + unit -- questions match against these),
/// and one for lexical matching (plus the table name and expression -- questions sometimes name a column directly).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MappingText {
    pub id: Uuid,
    pub concept_name: String,
    pub summary: Option<String>,
    pub unit: Option<String>,
    pub table_name: Option<String>,
    pub expr: Option<String>,
}

impl MappingText {
    pub fn embed_text(&self) -> String {
        let mut s = self.concept_name.clone();
        if let Some(x) = self.summary.as_deref().filter(|x| !x.trim().is_empty()) {
            s.push_str(": ");
            s.push_str(x.trim());
        }
        if let Some(u) = self.unit.as_deref().filter(|u| !u.trim().is_empty()) {
            s.push_str(" (");
            s.push_str(u.trim());
            s.push(')');
        }
        s
    }
    pub fn lexical_text(&self) -> String {
        [
            Some(self.concept_name.as_str()),
            self.summary.as_deref(),
            self.table_name.as_deref(),
            self.expr.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
    }
}

/// Confirmed definitions that are not yet embedded, or were embedded with different text or a different model than now (#574), at most `limit` rows.
/// Same rule as `ontology::types_needing_embedding`: change the text or switch the model and it gets re-embedded.
/// There is a cap because backfilling runs on the question-answering request path: after switching embedding models, the first question should not
/// embed hundreds of rows before answering; the rest is backfilled on the next question.
pub async fn needing_embedding(
    pool: &PgPool,
    kb_id: Uuid,
    model: &str,
    limit: i64,
) -> AppResult<Vec<MappingText>> {
    let rows: Vec<MappingText> = sqlx::query_as(
        "SELECT m.id, e.canonical_name AS concept_name, m.summary, m.unit, m.table_name, m.expr
           FROM concept_mappings m
           JOIN entities e ON e.id = m.concept_id
          WHERE m.kb_id = $1 AND m.status = 'confirmed'
            AND (m.embedding IS NULL OR m.embedded_model IS DISTINCT FROM $2)
          ORDER BY m.id
          LIMIT $3",
    )
    .bind(kb_id)
    .bind(model)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    // Text that changed also needs re-embedding. Building this text in SQL would mean copying `embed_text` by hand,
    // and the two copies would eventually diverge, so fetch it back and compare in Rust.
    let stale_text: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT m.id FROM concept_mappings m
          WHERE m.kb_id = $1 AND m.status = 'confirmed'
            AND m.embedding IS NOT NULL AND m.embedded_model = $2",
    )
    .bind(kb_id)
    .bind(model)
    .fetch_all(pool)
    .await?;
    let mut out = rows;
    if !stale_text.is_empty() {
        let ids: Vec<Uuid> = stale_text.into_iter().map(|(id,)| id).collect();
        let current: Vec<MappingText> = sqlx::query_as(
            "SELECT m.id, e.canonical_name AS concept_name, m.summary, m.unit, m.table_name, m.expr
               FROM concept_mappings m JOIN entities e ON e.id = m.concept_id
              WHERE m.id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(pool)
        .await?;
        let embedded: std::collections::HashMap<Uuid, Option<String>> =
            sqlx::query_as::<_, (Uuid, Option<String>)>(
                "SELECT id, embedded_text FROM concept_mappings WHERE id = ANY($1)",
            )
            .bind(&ids)
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect();
        for t in current {
            let was = embedded.get(&t.id).cloned().flatten();
            if was.as_deref() != Some(t.embed_text().as_str()) {
                out.push(t);
            }
        }
    }
    out.truncate(limit.max(0) as usize);
    Ok(out)
}

/// Write the vector back, together with which text and which model it was embedded from.
pub async fn set_embeddings(
    pool: &PgPool,
    model: &str,
    items: &[(MappingText, Vec<f32>)],
) -> AppResult<()> {
    for (t, v) in items {
        sqlx::query(
            "UPDATE concept_mappings
                SET embedding = $2, embedded_model = $3, embedded_text = $4
              WHERE id = $1",
        )
        .bind(t.id)
        .bind(pgvector::Vector::from(v.clone()))
        .bind(model)
        .bind(t.embed_text())
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Vector path: the confirmed definitions closest to the question. Only rows embedded by the same model are compared -- rows from a model switch that have not been re-embedded
/// do not take part: even with matching dimensions, two models' spaces do not line up, so the closeness they produce is false. Rows with different dimensions
/// are blocked by one more check, so pgvector does not raise an error directly
pub async fn vector_search(
    pool: &PgPool,
    kb_id: Uuid,
    model: &str,
    embedding: &[f32],
    limit: i64,
) -> AppResult<Vec<Uuid>> {
    let q = pgvector::Vector::from(embedding.to_vec());
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM concept_mappings
          WHERE kb_id = $1 AND status = 'confirmed' AND embedding IS NOT NULL
            AND embedded_model = $4
            AND vector_dims(embedding) = vector_dims($2)
          ORDER BY embedding <=> $2
          LIMIT $3",
    )
    .bind(kb_id)
    .bind(&q)
    .bind(limit)
    .bind(model)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// The text of all confirmed definitions; the lexical path scores in Rust -- a library has tens to hundreds of them,
/// and fetching them back is far cheaper than doing Chinese word segmentation in SQL, and avoids pg_trgm too
pub async fn confirmed_texts(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<MappingText>> {
    Ok(sqlx::query_as(
        "SELECT m.id, e.canonical_name AS concept_name, m.summary, m.unit, m.table_name, m.expr
           FROM concept_mappings m JOIN entities e ON e.id = m.concept_id
          WHERE m.kb_id = $1 AND m.status = 'confirmed'",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?)
}

/// Fetch definitions by id, **keeping the given order** -- the order comes from retrieval ranking, and the ones earlier in the prompt are read first
pub async fn by_ids(pool: &PgPool, kb_id: Uuid, ids: &[Uuid]) -> AppResult<Vec<ConceptMapping>> {
    let rows: Vec<ConceptMapping> = sqlx::query_as(
        "SELECT m.id, m.concept_id, e.canonical_name AS concept_name, m.source,
                m.table_name, m.expr, m.sql, m.unit, m.summary, m.derived, m.status, m.written_by
           FROM concept_mappings m JOIN entities e ON e.id = m.concept_id
          WHERE m.kb_id = $1 AND m.id = ANY($2)",
    )
    .bind(kb_id)
    .bind(ids)
    .fetch_all(pool)
    .await?;
    let mut by_id: std::collections::HashMap<Uuid, ConceptMapping> =
        rows.into_iter().map(|m| (m.id, m)).collect();
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// How many times a definition was revised, and what it looked like before each revision.
///
/// `revise` has been writing `concept_mapping_revisions` since the table was created, but **until now nothing**
/// read it -- the record was kept for nothing. 0006 says the record exists so that when question answering looks back at historical reports it can answer
/// "how was this number calculated last quarter", so someone has to be able to see it.
pub async fn revisions(
    pool: &PgPool,
    kb_id: Uuid,
    mapping_id: Uuid,
) -> AppResult<Vec<MappingRevision>> {
    // kb_id is checked via a JOIN for ownership: the revisions table has no kb_id of its own,
    // without this check, another library's definition history could be fetched
    Ok(sqlx::query_as(
        "SELECT r.id, r.before, u.display_name AS changed_by_name, r.changed_at
           FROM concept_mapping_revisions r
           JOIN concept_mappings m ON m.id = r.mapping_id
           LEFT JOIN users u ON u.id = r.changed_by
          WHERE r.mapping_id = $2 AND m.kb_id = $1
          ORDER BY r.changed_at DESC
          LIMIT 50",
    )
    .bind(kb_id)
    .bind(mapping_id)
    .fetch_all(pool)
    .await?)
}
