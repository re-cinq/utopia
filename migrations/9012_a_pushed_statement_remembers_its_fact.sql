-- A pushed statement remembers the fact it wrote: one row per (chunk, statement position), holding
-- the statement's canonical key and the fact it became, so a re-push can reuse the facts of the
-- statements it did not change, and, per named entity, the entity its entry resolved to. Running this again changes nothing.

CREATE TABLE IF NOT EXISTS pushed_statement_writes (
    chunk_id    UUID  NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    ord         INT   NOT NULL,
    key         TEXT  NOT NULL,
    fact_id     UUID  NOT NULL REFERENCES facts(id) ON DELETE CASCADE,
    PRIMARY KEY (chunk_id, ord)
);

CREATE INDEX IF NOT EXISTS pushed_statement_writes_fact ON pushed_statement_writes (fact_id);

CREATE TABLE IF NOT EXISTS pushed_entity_handles (
    chunk_id    UUID  NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    key         TEXT  NOT NULL,
    entity_id   UUID  NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    PRIMARY KEY (chunk_id, key)
);
