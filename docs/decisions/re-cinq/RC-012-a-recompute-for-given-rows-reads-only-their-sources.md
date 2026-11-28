# RC-012 · A recompute for given rows reads only their sources

> re:cinq fork only. Status: accepted 2026-11-28. Upstream: not yet proposed.

## Context

A typed row takes its attestation from its source statements (`materialize::sync_typed_attestation`, 0064 decision 5). Three callers run that sync:

- a materialization, for the whole base;
- `documents::reattest_tx`, the recompute a `reattest_facts` job runs after a document was deleted or a deletion undone (RC-010), for the facts that cited the document;
- `time_resolution::attest_statements`, once per document that states a date, for the whole base.

The statement named the given rows only on the outer `UPDATE`. Postgres aggregated every source of the base first and joined the given rows afterwards: a restriction on the updated table is not pushed into the grouped subquery. For seven facts of the `re-cinq/lore-security` base the plan on the internal cluster on 2026-11-28 (re.52) was a sequential scan of `typed_fact_sources` (4.59 million rows, all bases), a sequential scan of `facts` for the base's 7.3 million rows (the table holds 15.9 million, 11 GB), a hash join and a sort of about 2 million rows; estimated cost 1.35 million.

On that day 253 documents were deleted in the base. 87 `reattest_facts` jobs finished, 392 s on average and 9.5 hours together, with 86 more queued. A job for seven facts with 20 evidence rows took three minutes. In 20 minutes of Utopia's slow-statement log, while model-read documents were being extracted, this statement ran 25 times, 195 s on average, and was 19% of the time of all statements over one second.

## Decision

**A sync for given rows gathers only those rows' sources.**

- `materialize::typed_attestation_sql(given)` builds the statement in two forms. For given rows both branches of the source subquery carry `fact_id = ANY($2)`, so the sources are read through `typed_fact_sources_pkey` and the primary key of `implied_fact_sources`. For the whole base the statement is the one from before this record. `sync_typed_attestation` picks the form by whether rows are given.
- `time_resolution::attest_statements` no longer syncs the whole base. It takes the statements the document's live chunks are evidence for, looks up the typed rows that follow them (`materialize::typed_rows_following`: materialized from them or read from them by a rule) and syncs those rows. All of the document's statements, not only the ones this pass moved: a pass interrupted after it attested them finds nothing left to move on its retry.
- A materialization still syncs the whole base.

## Consequences

- The rows a sync of given rows moves, and the attestation each gets, are the same as before: the grouped subquery is restricted to the fact ids the outer `UPDATE` already required.
- A recompute after a deletion no longer grows with the size of the base. On a seeded base of one million statements with one typed row each and two million sources (local, Postgres 16, warm cache), the sync for 7 given rows took 1.5 ms instead of 3,062 ms, and for 500 rows 37 ms instead of 5,652 ms, with the same attestation on every row.
- A dated document's pass used to repair, as a side effect, any typed row of the base whose attestation another path had left behind. It now repairs only the rows that follow its own document's statements. A row left behind elsewhere is repaired by the base's next materialization, which syncs the whole base; the extraction of every pushed document queues one, and so does an alignment that binds a phrase.
- The whole-base form stays as expensive as it was. On 2026-11-28 `materialize_typed` ran 218 times on the internal cluster, 512 s on average and 31 hours together (for `re-cinq/lore-security` 21 runs of 49 minutes on average). Limiting a materialization to what changed is left open.
