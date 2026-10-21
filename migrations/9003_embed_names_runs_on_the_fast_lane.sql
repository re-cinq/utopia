-- re:cinq fork only (RE-CINQ.md: fork migrations are numbered from 9000 and idempotent).
--
-- embed_names（每个库一份、补名字向量）只调嵌入模型、不调对话模型，和 embed_ontology 一样
-- 属于不等对话模型的任务。0116 的 job_priority 没列它，它按 1 排在抽取后面，保留通道也不认领。
--
-- jobs_priority_claim_idx 是 job_priority(kind) 的表达式索引，IMMUTABLE 函数的结果在建索引时
-- 就存下了，改函数体不会让已有索引条目重算；所以函数一变就必须重建索引。
-- 只在函数还没列 embed_names 时才改并重建，第二次运行什么都不动。没有存储列，已排队的行不用补。
DO $$
BEGIN
    IF (SELECT prosrc FROM pg_proc WHERE proname = 'job_priority') LIKE '%embed_names%' THEN
        RETURN;
    END IF;
    CREATE OR REPLACE FUNCTION job_priority(kind text) RETURNS smallint
    LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $f$
        SELECT CASE WHEN kind IN ('sync_source', 'process_document', 'hydrate_rss_entry',
                                  'embed_ontology', 'memory_ingest', 'build_vector_index',
                                  'materialize_inferences', 'noop', 'embed_names')
                    THEN 0::smallint ELSE 1::smallint END
    $f$;
    DROP INDEX IF EXISTS jobs_priority_claim_idx;
    CREATE INDEX jobs_priority_claim_idx ON jobs (job_priority(kind), run_at) WHERE status = 'queued';
END
$$;
