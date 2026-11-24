-- re:cinq 分支专用迁移（9000 起，幂等）。
-- 排队去重（enqueue_unless_pending / enqueue_unless_queued*）按 kind + payload 查排着或跑着的任务，
-- 没有匹配的索引就会读完整张 jobs（含全部已完成的行）。
-- 索引只覆盖 queued / running 的行；用 md5 是因为 jsonb 载荷可能超过 btree 的行大小上限。
-- 不用 CONCURRENTLY：理由同 9013。
CREATE INDEX IF NOT EXISTS jobs_pending_kind_payload_idx
    ON jobs (kind, md5(payload::text))
    WHERE status IN ('queued', 'running');
