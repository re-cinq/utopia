-- re:cinq fork only (RE-CINQ.md: fork migrations are numbered from 9000 and idempotent).
--
-- Evidence is read by document on every pushed document: `resolve_time` selects and
-- updates a document's evidence, errata picks a document's candidates by it, and
-- deleting a document cascades into fact_evidence through this column. The table's
-- key is (fact_id, chunk_id), so with no index here each of those reads scanned the
-- evidence of every knowledge base. On 2026-10-17 that held the database at its
-- CPU limit while a source pushed.
CREATE INDEX IF NOT EXISTS fact_evidence_document_idx ON fact_evidence (document_id);
