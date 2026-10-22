-- re:cinq fork only (RE-CINQ.md: fork migrations are numbered from 9000 and idempotent).
--
-- 一个源可以让自己的文档衰减（RC-003）。`sources.decay` 为 NULL 就是关着：今天的行为，
-- 什么都不多写。打开之后形如 {"max_items":500,"half_life_days":60,"stale_after_days":30,
-- "min_age_days":30}，缺的字段取 Lore 的默认值。
ALTER TABLE sources
    ADD COLUMN IF NOT EXISTS decay JSONB CHECK (decay IS NULL OR jsonb_typeof(decay) = 'object');

-- 文档被检索的记录：只给打开了衰减的源的文档写，边表而不是文档上的列，
-- 因为每次检索都要写，文档行不该被读搅动。半衰期每次检索 +2 天，上限 365。
CREATE TABLE IF NOT EXISTS document_salience (
    document_id       UUID PRIMARY KEY REFERENCES documents(id) ON DELETE CASCADE,
    retrieval_count   INTEGER NOT NULL DEFAULT 0 CHECK (retrieval_count >= 0),
    last_retrieved_at TIMESTAMPTZ,
    half_life_days    REAL NOT NULL CHECK (half_life_days >= 1 AND half_life_days <= 365)
);
