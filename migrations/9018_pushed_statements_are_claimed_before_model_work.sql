-- re:cinq fork only (RE-CINQ.md: fork migrations are numbered from 9000 and idempotent).
--
-- A document of a `statements` source is extracted without calling the chat model, but it was queued
-- as extract_document at priority 2, behind every model extraction. It now has its own kind,
-- extract_statements, at priority 1 next to resolve_time; the handler is the same. Extractions
-- already queued for such documents change kind; running ones are left alone.
--
-- jobs_priority_claim_idx is an expression index on the IMMUTABLE function and is rebuilt with it
-- (as in 9003, 9008, 9016, 9017). The one-queued-extraction-per-document index of 9011 only
-- knew extract_document, so it is replaced by one that covers both kinds.
DO $$
DECLARE
    body text := (SELECT prosrc FROM pg_proc WHERE oid = to_regproc('public.job_priority'));
BEGIN
    IF body LIKE '%extract_statements%' THEN
        RETURN;
    END IF;
    CREATE OR REPLACE FUNCTION job_priority(kind text) RETURNS smallint
    LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $f$
        SELECT CASE WHEN kind IN ('sync_source', 'process_document', 'hydrate_rss_entry',
                                  'embed_ontology', 'memory_ingest', 'build_vector_index',
                                  'materialize_inferences', 'materialize_typed', 'noop',
                                  'embed_names')
                    THEN 0::smallint
                    WHEN kind IN ('resolve_time', 'prune_superseded', 'extract_statements')
                    THEN 1::smallint
                    ELSE 2::smallint END
    $f$;
    REINDEX INDEX jobs_priority_claim_idx;
END
$$;

DROP INDEX IF EXISTS jobs_queued_extraction_document;

CREATE INDEX IF NOT EXISTS jobs_queued_extraction
    ON jobs ((payload->>'document_id'))
 WHERE kind IN ('extract_document', 'extract_statements') AND status = 'queued';

UPDATE jobs
   SET kind = 'extract_statements'
 WHERE kind = 'extract_document' AND status = 'queued'
   AND (payload->>'document_id')::uuid IN (
        SELECT d.id FROM documents d JOIN sources s ON s.id = d.source_id
         WHERE s.kind = 'statements');
