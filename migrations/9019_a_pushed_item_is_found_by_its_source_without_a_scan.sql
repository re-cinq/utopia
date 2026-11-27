-- re:cinq fork only (RE-CINQ.md: fork migrations are numbered from 9000 and idempotent).
--
-- Every push looks its document up by source before writing it: by content hash to recognise a
-- rename (documents::find_by_source_sha) and, when the external key is new, by file name among the
-- legacy documents without one (find_legacy_by_filename, and the FOR UPDATE claim in the push
-- path). No index started with source_id, so on the internal cluster on 2026-11-27 each lookup was
-- a parallel sequential scan of all documents, about a second each. Twenty pushes at once held
-- most of the 32 pool connections: the worker kept pausing its claims ("too few free database
-- connections") and Lore's reads ran into their 30 s timeout.
--
-- The file name is indexed by its md5 and the query still compares the name itself: a btree entry
-- holds at most about 2.7 kB, and an upload's file name has no length limit, so indexing the name
-- could fail this migration on an existing row or refuse a later upload.
--
-- Not CONCURRENTLY: same reason as 9013.
CREATE INDEX IF NOT EXISTS documents_source_sha_live_idx
    ON documents (source_id, sha256)
    WHERE deleted_at IS NULL AND source_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS documents_source_legacy_filename_idx
    ON documents (source_id, md5(filename))
    WHERE external_key IS NULL AND deleted_at IS NULL AND source_id IS NOT NULL;
