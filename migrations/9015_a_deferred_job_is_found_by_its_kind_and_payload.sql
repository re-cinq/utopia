-- re:cinq 分支专用迁移（9000 起，幂等）。
-- 推迟（persist_outcome）会把 deferred_since 写进排着的任务载荷；去重比较现在忽略它，
-- 所以 9014 的索引（按整份载荷的 md5）换成按去掉 deferred_since 的载荷建，表达式须与 jobs.rs
-- 的 payload_without_deferral 逐字一致，计划器才用得上。排队与排队/在跑两种去重都走这一个索引。
-- 同时每个库只留最老的一条排着的 materialize_typed（它读当下的绑定，多的是重复），
-- 在跑的、做完的、失败的和别的种类一律不动。不用 CONCURRENTLY：理由同 9013。
DROP INDEX IF EXISTS jobs_pending_kind_payload_idx;

CREATE INDEX IF NOT EXISTS jobs_pending_kind_stripped_payload_idx
    ON jobs (kind, md5((payload - 'deferred_since')::text))
 WHERE status IN ('queued', 'running');

DELETE FROM jobs later
 USING jobs earlier
 WHERE later.kind = 'materialize_typed' AND later.status = 'queued'
   AND earlier.kind = 'materialize_typed' AND earlier.status = 'queued'
   AND earlier.payload->>'kb_id' = later.payload->>'kb_id'
   AND earlier.id < later.id;
