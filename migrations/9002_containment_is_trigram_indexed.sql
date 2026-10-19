-- re:cinq fork only (RE-CINQ.md: fork migrations are numbered from 9000 and idempotent).
--
-- 名字包含关系（`containment_reviews`）的正向那半是 `lower(canonical_name) LIKE '%…%'`，
-- 没有三元组索引只能把整个库的实体逐行比。2026-10-19 生产上 3.9 万实体的库每次 0.5 秒、
-- 记忆抽取时 25 个并发把库钉满。pg_trgm 自 PG 13 起是受信扩展，库 owner 即可安装。
--
-- 只给实体名建：别名（known_as 事实）那半走 9001 的 `facts_live_predicate_idx` 只读本库的名字
-- 事实，比给全部现行事实建三元组索引便宜——那个索引不分库，常见词会把别的库的命中也读一遍，
-- 且每写一条事实都要更新它。
CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE INDEX IF NOT EXISTS entities_name_trgm_idx ON entities USING gin (lower(canonical_name) gin_trgm_ops)
  WHERE merged_into IS NULL;
