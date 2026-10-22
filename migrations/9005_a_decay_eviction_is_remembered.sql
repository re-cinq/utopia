-- re:cinq fork only (RE-CINQ.md: fork migrations are numbered from 9000 and idempotent).
--
-- 衰减扫描淘汰的文档（RC-003）记一笔。推送方（Lore 的记忆镜像每一轮都把全部记忆重推一遍）
-- 再推同样内容的同一个身份时，不该把它从回收站里恢复出来、等下一次扫描再删：
-- 有这一行、内容没变，就答「没变」并保持已删。恢复（人或内容变了）会清掉这一行。
CREATE TABLE IF NOT EXISTS decay_evictions (
    document_id UUID PRIMARY KEY REFERENCES documents(id) ON DELETE CASCADE,
    evicted_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
