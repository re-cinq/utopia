-- re:cinq fork only (RE-CINQ.md: fork migrations are numbered from 9000 and idempotent).
--
-- 上游 0123 把 materialize_typed 列进 job_priority 的快道，9003 把 embed_names 列进去；两份
-- 函数体各自只认自己那个。新库按 0123、9003 的顺序跑，留下的是 9003 的函数（没有
-- materialize_typed）；已经跑过 9003 的库再跑 0123，留下的是上游的函数（没有 embed_names）。
-- 这里把两个都列上，两种顺序到头都一样。
--
-- 函数体已经两个都有时什么都不动。jobs_priority_claim_idx 是 IMMUTABLE 函数的表达式索引，
-- 函数一变就要重建（同 9003、0123）。
DO $$
DECLARE
    body text := (SELECT prosrc FROM pg_proc WHERE proname = 'job_priority');
BEGIN
    IF body LIKE '%embed_names%' AND body LIKE '%materialize_typed%' THEN
        RETURN;
    END IF;
    CREATE OR REPLACE FUNCTION job_priority(kind text) RETURNS smallint
    LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $f$
        SELECT CASE WHEN kind IN ('sync_source', 'process_document', 'hydrate_rss_entry',
                                  'embed_ontology', 'memory_ingest', 'build_vector_index',
                                  'materialize_inferences', 'materialize_typed', 'noop',
                                  'embed_names')
                    THEN 0::smallint ELSE 1::smallint END
    $f$;
    REINDEX INDEX jobs_priority_claim_idx;
END
$$;
