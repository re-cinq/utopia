-- re:cinq fork only (RE-CINQ.md: fork migrations are numbered from 9000 and idempotent).
--
-- resolve_time 是每个抽取完的文档一个、很便宜的任务，却和所有旧的抽取一起按 1 排队：
-- 内部集群上排了 39k 个，几个小时一个都没跑完。现在分三档：同步、入库这些不调对话模型的
-- 任务仍是 0（名单同 9008），resolve_time 是 1，其余全是 2。resolve_time 先于抽取排空，
-- 入库仍在它前面。快道只认 job_priority(kind) = 0，不受影响。
--
-- 函数体已经列了 resolve_time 时什么都不动。jobs_priority_claim_idx 是 IMMUTABLE 函数的
-- 表达式索引，函数一变就要重建（同 9003、9008、0123）。
DO $$
DECLARE
    body text := (SELECT prosrc FROM pg_proc WHERE oid = to_regproc('public.job_priority'));
BEGIN
    IF body LIKE '%resolve_time%' THEN
        RETURN;
    END IF;
    CREATE OR REPLACE FUNCTION job_priority(kind text) RETURNS smallint
    LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $f$
        SELECT CASE WHEN kind IN ('sync_source', 'process_document', 'hydrate_rss_entry',
                                  'embed_ontology', 'memory_ingest', 'build_vector_index',
                                  'materialize_inferences', 'materialize_typed', 'noop',
                                  'embed_names')
                    THEN 0::smallint
                    WHEN kind = 'resolve_time' THEN 1::smallint
                    ELSE 2::smallint END
    $f$;
    REINDEX INDEX jobs_priority_claim_idx;
END
$$;
