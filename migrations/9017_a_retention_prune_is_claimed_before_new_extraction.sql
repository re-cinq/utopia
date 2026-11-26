-- re:cinq fork only (RE-CINQ.md: fork migrations are numbered from 9000 and idempotent).
--
-- prune_superseded runs once a day per knowledge base and frees the disk, but at priority 2 it
-- waited behind every queued extraction: on the internal cluster 23 of them sat for a day behind
-- 41k extractions. It joins resolve_time at 1; intake stays at 0 and the fast lane is unchanged.
--
-- jobs_priority_claim_idx is an expression index on the IMMUTABLE function and is rebuilt with it
-- (as in 9003, 9008, 9016).
DO $$
DECLARE
    body text := (SELECT prosrc FROM pg_proc WHERE oid = to_regproc('public.job_priority'));
BEGIN
    IF body LIKE '%prune_superseded%' THEN
        RETURN;
    END IF;
    CREATE OR REPLACE FUNCTION job_priority(kind text) RETURNS smallint
    LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $f$
        SELECT CASE WHEN kind IN ('sync_source', 'process_document', 'hydrate_rss_entry',
                                  'embed_ontology', 'memory_ingest', 'build_vector_index',
                                  'materialize_inferences', 'materialize_typed', 'noop',
                                  'embed_names')
                    THEN 0::smallint
                    WHEN kind IN ('resolve_time', 'prune_superseded') THEN 1::smallint
                    ELSE 2::smallint END
    $f$;
    REINDEX INDEX jobs_priority_claim_idx;
END
$$;
