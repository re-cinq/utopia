-- re:cinq fork only (RE-CINQ.md: fork migrations are numbered from 9000 and idempotent).
--
-- 这个部署上的 statements 源都是 Lore 的镜像：推来的名字（owner/repo:path#symbol）本身就是唯一
-- 标识。0071 让源用 {"names": "identity"} 声明这一点；从前 fork 对所有推送名字一律跳过召回，
-- 换成上游的按源开关之后，要在这里替现有的镜像源把开关打开，行为才不倒退。
--
-- 只补还没有 names 键的 statements 源：人手设过的不动，第二次运行什么都不改。
-- 之后新建的源要由创建方自己带上这个键。
UPDATE sources
   SET config = config || '{"names": "identity"}'::jsonb
 WHERE kind = 'statements'
   AND NOT config ? 'names';
