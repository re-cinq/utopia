-- re:cinq fork only (RE-CINQ.md: fork migrations are numbered from 9000 and idempotent).
--
-- RC-007: a class or property carries the literal `utopia:*` display hints its ontology
-- file declared (panel, chain_first, role, tones, chain_next), as a flat object of strings.
-- Empty means the type says nothing about how it is shown and the generic panel draws it.
ALTER TABLE entity_types ADD COLUMN IF NOT EXISTS display_hints JSONB NOT NULL DEFAULT '{}'::jsonb;
ALTER TABLE relation_types ADD COLUMN IF NOT EXISTS display_hints JSONB NOT NULL DEFAULT '{}'::jsonb;
