//! Extraction drop signals: which facts were extracted but failed to land, and why.
//!
//! Keeping this apart from `ontology_misses` is deliberate: that table says "your ontology lacks these", and its reader is
//! the ontology maintainer, whose action is to add a type; this one says "these facts did not land", and its reader is the person who uploaded the document,
//! whose action is to edit the document or the ontology. Mixed into one panel, neither side can be explained clearly.
//!
//! Failing to record does not affect extraction (callers always use `let _ =`): missing one signal is far better than
//! aborting the extraction of a whole document because recording a signal failed.

use sqlx::PgPool;
use utopia_core::models::ExtractionDrop;
use utopia_core::AppResult;
use uuid::Uuid;

/// Reason codes. The frontend looks up its copy by these, so they are a stable contract; do not change the literals.
/// All are emitted by the open extraction path (0044); the typed path's reason codes were retired along with it
pub mod reason {
    /// The whole block was not extracted: the call failed (endpoint unavailable, request not delivered, timed out) or the reply could not be parsed.
    ///
    /// **Unlike the other reason codes, it drops a whole block of source text, not one statement.** The task's `last_error`
    /// also carries this, but that is for people debugging; once a document has finished extracting, the only place that still shows "this block did not get in" is
    /// this table (0001: a drop is a row, not silence)
    pub const CHUNK_UNEXTRACTED: &str = "chunk_unextracted";
    /// The statement has neither an object nor a value
    pub const OBJECT_MISSING: &str = "object_missing";
    /// The model's entry is malformed (e.g. missing phrase) -> skip only this entry, not the whole block
    pub const MALFORMED_ITEM: &str = "malformed_item";
    /// Model output was truncated (hit max_tokens) -> keep the complete entries, drop the tail
    pub const TRUNCATED_REPLY: &str = "truncated_reply";
    /// The statement's value is not in its own quote (#729): the evidence does not state this number.
    ///
    /// Tables are where this error comes from: the model squeezes one five-column row into five statements that differ only in value, while the quote points at
    /// the lead-in sentence above the table. At most one of the five is right, and nothing in the graph can tell which. **The quote is this statement's
    /// whole basis**: if the value is not in it, it is not what the document says
    pub const VALUE_NOT_IN_QUOTE: &str = "value_not_in_quote";
    /// The alias the model reported (or its quote) is not in this block of source text (0041 decision 2): do not record the name.
    /// A name is a retrieval bridge; a bridge built from nothing would join two unrelated entities
    pub const NAME_NOT_IN_TEXT: &str = "name_not_in_text";
    /// The alias the model reported is already the name of another entity in this reply (or in earlier blocks of this document) (0041 decision 2):
    /// one name cannot be the name of two things. If "Haitan-1 project" is declared an organization, it is not an alias of the probe
    pub const NAME_CLAIMED_BY_ANOTHER: &str = "name_claimed_by_another";
    /// The object is neither listed in the reply nor is there anything with this name in the database: the statement still lands,
    /// with the object stored as a literal value rather than a node (#559). It is recorded so the count is known: how many of this kind
    /// should have been entities (the model missed them), and how many were descriptions all along
    pub const OBJECT_UNDECLARED: &str = "object_undeclared";
    /// The model's quote cannot be found verbatim in this block: the statement lands and the quote is kept, but without offsets
    /// (`quote_start` / `quote_end` left empty). This signal counts how many could not be located
    pub const QUOTE_NOT_IN_CHUNK: &str = "quote_not_in_chunk";
    /// A time phrase that is in neither its quote nor this block: do not record this time mention. A time phrase is copied text,
    /// and words that cannot be copied are not what the document says
    pub const TIME_NOT_IN_QUOTE: &str = "time_not_in_quote";
    /// A statement, attribute or name points at an entity number or known handle that does not exist in the reply: skip this entry
    pub const UNKNOWN_REF: &str = "unknown_ref";
    /// The phrase is the value itself (e.g. "director nominees -ten (10)-> ten (10)"): one of the ways the model
    /// writes it when the table has lost its column header. The statement still lands (subject and value are both there, nothing lost), and one record makes it countable.
    /// Purely structural check: the two strings are equal once leading and trailing whitespace is trimmed
    pub const PHRASE_IS_VALUE: &str = "phrase_is_value";
    /// The phrase is the subject's name (e.g. "physical goods online retail sales" -"physical goods online retail sales"-> "127878 hundred million yuan"):
    /// Same as above, another way of writing it; it still lands
    pub const PHRASE_IS_SUBJECT: &str = "phrase_is_subject";
    /// The object is really a value (a quantity, a date, a table cell): move it into the value slot, do not create an entity
    /// (`open::values_are_not_things`). It counts how many times rule 2 and the table rule still failed to keep the model in check
    pub const OBJECT_IS_VALUE: &str = "object_is_value";
    /// The statement is bound to an attribute, but the value cannot be read under the datatype and unit the attribute declares ("over 2000 people" cannot be read as
    /// a number, "USD 50 million" cannot be converted to 10k-yuan units): the statement stays in the open graph and does not land as a typed row (`materialize`).
    /// This entry is emitted by materialization, not extraction; each materialization pass rewrites it per database, and detail is the attribute's key
    pub const ATTR_DATATYPE: &str = "attr_datatype";
}

/// Values one materialization pass could not read: (document, attribute key, one sample).
pub struct Unreadable {
    pub document_id: Uuid,
    pub detail: String,
    pub example: String,
}

/// Replace all rows of one reason code in a database with this batch (within a transaction). Materialization is a set operation recomputed every pass,
/// so its signals are also written as a set: delete all rows for this reason code in this database, then count by (document, detail)
/// and write them back. With `record`, every recomputation would count the same statement again
pub async fn replace_reason_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kb_id: Uuid,
    reason: &str,
    rows: &[Unreadable],
) -> AppResult<()> {
    // Whole-database statements are not cached (#1298): after a few runs the cache switches to a generic plan, which cannot see how many rows this database has
    sqlx::query("DELETE FROM extraction_drops WHERE kb_id = $1 AND reason = $2")
        .persistent(false)
        .bind(kb_id)
        .bind(reason)
        .execute(&mut **tx)
        .await?;
    for r in rows {
        sqlx::query(
            "INSERT INTO extraction_drops (kb_id, document_id, reason, detail, example)
             VALUES ($1, $2, $3, left($4, 120), left($5, 200))
             ON CONFLICT (kb_id, document_id, reason, detail)
             DO UPDATE SET count = extraction_drops.count + 1, updated_at = now()",
        )
        .bind(kb_id)
        .bind(r.document_id)
        .bind(reason)
        .bind(&r.detail)
        .bind(&r.example)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

pub async fn record(
    pool: &PgPool,
    kb_id: Uuid,
    document_id: Uuid,
    reason: &str,
    detail: &str,
    example: Option<&str>,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO extraction_drops (kb_id, document_id, reason, detail, example)
         VALUES ($1, $2, $3, left($4, 120), left($5, 200))
         ON CONFLICT (kb_id, document_id, reason, detail)
         DO UPDATE SET count = extraction_drops.count + 1,
                       example = COALESCE(EXCLUDED.example, extraction_drops.example),
                       updated_at = now()",
    )
    .bind(kb_id)
    .bind(document_id)
    .bind(reason)
    .bind(detail)
    .bind(example)
    .execute(pool)
    .await?;
    Ok(())
}

/// Clear this document's old signals when re-extraction starts: this pass tells the document's story again from the beginning.
pub async fn clear_for_document(pool: &PgPool, document_id: Uuid) -> AppResult<()> {
    sqlx::query("DELETE FROM extraction_drops WHERE document_id = $1")
        .bind(document_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// All drop signals for one KB. Rows are aggregated by (document x reason x specific object) and stay small,
/// so one fetch lets Library both compute each document's total and expand the details directly, without a request per row.
pub async fn for_kb(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<ExtractionDrop>> {
    Ok(sqlx::query_as(
        "SELECT document_id, reason, detail, count, example FROM extraction_drops
         WHERE kb_id = $1 ORDER BY count DESC, reason LIMIT 2000",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?)
}
