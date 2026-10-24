# RC-005 · A typed fact is as confirmed as its statement

> re:cinq fork only. Status: accepted 2026-10-24. Upstream: not yet proposed.

## Context

The Review page lists a fact as unconfirmed (`review::UNCONFIRMED_FACT`) when it is live, open, has evidence, and every chunk of that evidence is superseded. Materialize builds a typed row from an open statement (`typed_fact_sources`) and copies the statement's evidence onto it at that moment. When a `statements` source pushes the item again and states the same thing, the new chunk's evidence lands on the statement, not on the typed row. The typed row keeps only the old copy, so a current fact reads as unconfirmed. In one of our bases 71,025 typed facts are such copies of statements that are live, open and backed by a live chunk. RC-004's close leaves typed rows with a statement source alone, so they never leave the queue.

## Decision

**A typed row with statement sources is judged by those statements.** It is not unconfirmed while any of its source statements is live (`invalidated_at IS NULL`), open (no `valid_to`, no `valid_to_precision`) and has evidence on a chunk that is not superseded. The rule is one more `NOT EXISTS` in `UNCONFIRMED_FACT`, read through the keys of `typed_fact_sources (fact_id, statement_id)` and `fact_evidence (fact_id, chunk_id)`.

**Every reader of the queue says the same thing.** The review counts, the review summary, RC-004's close and `graph::stale_facts`, which used to repeat the clause, all read the one fragment.

A typed row without statement sources, and an open statement, are judged as before.

## Consequences

- Once every source statement of a typed row is unconfirmed, closed or rejected, the row is unconfirmed again, and closing or rejecting the statement retires it through the recompute (RC-004).
- The typed row's own evidence is still the old copy. Search and citations that read it point at the superseded chunk. Refreshing the copy, or reading the statement's evidence there, is left for another change.
- Rows a rule implies from a statement (`implied_fact_sources`) also copy its evidence and are not covered here.
