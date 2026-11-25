//! 0040: where a piece of text came from, stored on the chunk.
//!
//! 1. **Provenance is stored and claimed as-is.** Within the same body, one chunk of original text and one chunk from a scanned page read back as separate things;
//!    when processed again, a chunk whose provenance is unchanged claims its original row (same id), and the same sentence with a changed provenance counts as a new chunk.
//! 2. **The anchor's shape is determined by the source.** A transcript must carry a speaker; original text must not carry an anchor — the database rejects it.
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is not set. It builds and tears down its own data and never touches an existing DB.

use sqlx::PgPool;
use utopia_ingest::{ChunkPiece, Origin, Provenance};
use uuid::Uuid;

fn piece(seq: i32, text: &str, provenance: Provenance) -> ChunkPiece {
    ChunkPiece {
        seq,
        text: text.into(),
        char_start: 0,
        char_end: text.len() as i32,
        heading: None,
        provenance,
    }
}

fn ocr(page: i64) -> Provenance {
    Provenance {
        origin: Origin::Ocr,
        model: Some("mineru".into()),
        anchor: Some(serde_json::json!({ "page": page, "bbox": [10, 20, 300, 400] })),
    }
}

async fn seed(pool: &PgPool) -> anyhow::Result<(Uuid, Uuid, Uuid)> {
    let (org, ws, kb, doc) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'origin-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'origin-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'origin-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256, status)
         VALUES ($1, $2, 'scan.pdf', $1::text, 'ready')",
    )
    .bind(doc)
    .bind(kb)
    .execute(pool)
    .await?;
    Ok((org, kb, doc))
}

type Row = (
    Uuid,
    String,
    String,
    Option<String>,
    Option<serde_json::Value>,
);

async fn stored(pool: &PgPool, doc: Uuid) -> anyhow::Result<Vec<Row>> {
    Ok(sqlx::query_as(
        "SELECT id, text, origin, origin_model, anchor FROM chunks
          WHERE document_id = $1 AND superseded_at IS NULL ORDER BY seq",
    )
    .bind(doc)
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
async fn a_chunk_keeps_its_provenance_and_is_claimed_by_it() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, kb, doc) = seed(&pool).await?;
    let run = async {
        let first = vec![
            piece(0, "Rent is paid monthly.", Provenance::stated()),
            piece(1, "The rent is 1,000 per month.", ocr(1)),
        ];
        utopia_store::documents::replace_chunks(&pool, kb, doc, &first).await?;
        let rows = stored(&pool, doc).await?;
        assert_eq!(rows[0].2, "stated");
        assert_eq!(rows[0].4, None);
        assert_eq!(rows[1].2, "ocr");
        assert_eq!(rows[1].3.as_deref(), Some("mineru"));
        assert_eq!(
            rows[1].4.as_ref().map(|a| a["page"].clone()),
            Some(serde_json::json!(1))
        );

        // Processed again: a chunk whose provenance is unchanged claims its original row; the same sentence moved to another page is a new chunk
        let second = vec![
            piece(0, "Rent is paid monthly.", Provenance::stated()),
            piece(1, "The rent is 1,000 per month.", ocr(2)),
        ];
        utopia_store::documents::replace_chunks(&pool, kb, doc, &second).await?;
        let again = stored(&pool, doc).await?;
        assert_eq!(again[0].0, rows[0].0, "the stated chunk is claimed");
        assert_ne!(
            again[1].0, rows[1].0,
            "a different anchor is a different chunk"
        );
        assert_eq!(
            again[1].4.as_ref().map(|a| a["page"].clone()),
            Some(serde_json::json!(2))
        );

        // Anchor shape: a transcript must distinguish speakers; original text carries no anchor
        let no_speaker = Provenance {
            origin: Origin::Transcribed,
            model: Some("whisper".into()),
            anchor: Some(serde_json::json!({ "start_ms": 0, "end_ms": 900 })),
        };
        assert!(utopia_store::documents::replace_chunks(
            &pool,
            kb,
            doc,
            &[piece(0, "We ship in Q3.", no_speaker)]
        )
        .await
        .is_err());
        let stated_with_anchor = Provenance {
            origin: Origin::Stated,
            model: None,
            anchor: Some(serde_json::json!({ "page": 1 })),
        };
        assert!(utopia_store::documents::replace_chunks(
            &pool,
            kb,
            doc,
            &[piece(0, "x", stated_with_anchor)]
        )
        .await
        .is_err());
        anyhow::Ok(())
    }
    .await;
    let _ = sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await;
    run
}
