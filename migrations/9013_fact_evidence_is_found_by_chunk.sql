-- re:cinq 分支专用迁移（9000 起，幂等）。
-- 按 chunk_id 把 fact_evidence 与 chunks 连接的查询（例如 documents.rs 删除文档时
-- 读取被引用的事实）会扫描整张 fact_evidence，因为主键 (fact_id, chunk_id) 以 fact_id 开头。
-- 不用 CONCURRENTLY：理由同 0122；并发建失败会留下无效索引，下次 IF NOT EXISTS 会把它跳过。
CREATE INDEX IF NOT EXISTS fact_evidence_chunk_idx ON fact_evidence (chunk_id);
