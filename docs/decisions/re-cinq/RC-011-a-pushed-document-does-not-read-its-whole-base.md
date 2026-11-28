# RC-011 · A pushed document does not read its whole base

> re:cinq fork only. Status: accepted 2026-11-28. Upstream: not yet proposed.

## Context

Two steps at the end of every document's extraction (`extraction_open::run_open`) read the whole knowledge base, whatever the size of the document:

- `phrase_alignment::bind_declared` (pushed documents only) listed the base's relation types with `ontology::relation_type_views`, which also counts each relation's live typed facts. The caller reads a relation's name, domain and range, never the count.
- `type_bindings::apply_bound` updated every entity of the base whose category word had been bound in the meantime.

On the internal cluster on 2026-11-27 (re.51), 25 samples of `pg_stat_activity` showed 507 active sessions: 193 (38%) in the relation listing and 102 (20%) in `apply_bound`, 80% of all sessions on CPU. For the `re-cinq/lore-security` base the usage count alone was a parallel sequential scan of all 15 million facts (3.0 GB heap), 297,749 blocks read from disk, 42.7 s under load. `apply_bound` read all 149,937 entities of the base with a `regexp_replace` per row. A statements extraction took 28.7 s on average, and Lore's searches waited on the same disk.

## Decision

**A document's extraction reads what the document touched.**

- `bind_declared` lists relation types with `ontology::relation_type_views_uncounted`: the same rows in the same order, `usage` 0, no read of `facts`. Every other caller keeps the counted listing.
- After a document, the catch-up runs as `type_bindings::apply_bound_to` for what that document touches: the entities the run resolved or created (`doc_entities` and the described ones) and both ends of every fact its live chunks are evidence for (`fact_evidence` of chunks not superseded). The second part covers what the run did not resolve itself: facts carried over from a pushed item's previous version, and chunks an earlier, interrupted attempt had already extracted. A memory log is caught up for the run's entities only: all of its episodes stay live, so its evidence is its whole history. The judgement itself (`type_bindings::apply`) still projects onto every entity that exists when it lands.
- The base-wide `apply_bound` still runs at the start of each category-word alignment round (`type_alignment`), which every extraction queues.
- A workspace without a chat model has no alignment round: `type_alignment` stops before its base-wide pass. There a document's extraction keeps the base-wide `apply_bound`, exactly as before this record. It still gets the uncounted listing.

## Consequences

- The cost of extracting a pushed document no longer grows with the size of its base. In a local run with a chat model set, on a base of 150,000 entities and 3.1 million facts, 405 statements extractions took 32 s of job time instead of 898 s.
- One race is now closed later than before. If a judgement's projection runs before a new entity is visible to it, and that entity's document runs its catch-up before the judgement is visible, the entity has no class until the next alignment round. Before, the next document extracted in the base would have caught it up.
- `usage` in `bind_declared`'s listing is 0. Nothing there reads it.
- A workspace without a chat model keeps one base-wide `apply_bound` per document. Making that cheap as well needs a base-wide pass that does not depend on the model, which is left open.
