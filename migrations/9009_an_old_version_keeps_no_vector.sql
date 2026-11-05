-- re:cinq fork only (RE-CINQ.md: fork migrations are numbered from 9000 and idempotent).
--
-- The daily prune (RC-007, utopia_store::retention) reads a base's chunks superseded
-- before a cutoff. chunks_live_idx covers only the live ones and chunks_kb_idx would
-- walk every chunk of the base, which in a base of mirrored statements is millions of
-- rows a day. The index is partial: a live chunk never enters it.
CREATE INDEX IF NOT EXISTS chunks_superseded_idx ON chunks (kb_id, superseded_at)
    WHERE superseded_at IS NOT NULL;
