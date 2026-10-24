# RC-004 · A source closes its unconfirmed facts

> re:cinq fork only. Status: accepted 2026-10-24. Upstream: not yet proposed.

## Context

A `statements` source ([0054](../0054-a-source-may-push-statements-in-the-open-contract.md)) pushes the same items again and again. When an item stops stating a fact, because a value changed or a relation went away, the old fact keeps only evidence on superseded chunks. The Review page lists it as unconfirmed (`review::UNCONFIRMED_FACT`) until a person closes it, one at a time, with a session. Our bases hold about 1.65 million of them, and the system that knows they ended holds only a write token.

## Decision

**`POST /kbs/{kb}/sources/{source}/close-unconfirmed` closes a batch.** It needs a write token or a session, and the editor role on the base. A source of another base is `404`. `limit` is 1 to 5000, 1000 by default.

**It closes only what the source alone stopped stating.** A fact qualifies when it is unconfirmed, every chunk that attests it belongs to a document of this source, and it is not a `known_as` name fact, which Utopia writes itself. A typed row with a statement source (`typed_fact_sources`) is left out: it follows its statements.

**A close is the Review page's close.** Each fact goes through `temporal::close_superseded`, ending at the latest `superseded_at` of its evidence at `second` precision. The old row is invalidated and the closed row supersedes it, with its evidence. Open statements go first. When any was closed, a typed recompute is queued, the same way a rejected statement queues one. The recompute retires the typed row of the old statement and builds it again from the closed one, with its end. A fact whose start is after that end cannot be closed this way. The query leaves it out and counts it as `skipped`, so it cannot fill every batch.

**A closed statement stays a statement.** `rewrite_end_tx` now copies `layer` and `phrase`. Before this change, closing an open statement wrote a typed row with no predicate, and the recompute never saw it again.

**One audit row per batch** (`source.close_unconfirmed` with `source_id`, `closed`, `skipped` and the token). The answer is `{closed, skipped, remaining}`. `remaining` is a full count of what a further call would close. It runs the same query from the source's documents through `fact_evidence_document_idx`.

## Consequences

- The pusher can empty its own part of the Review queue. History is kept: nothing is rejected or deleted.
- A fact attested by two sources stays in the queue for a person.
- `rehome_tx` and the time correction in `temporal.rs` still write their rows without `layer` and `phrase`. They are left for another change.

## Note, 2026-10-24: every rewrite keeps the statement layer

`rehome_tx` (undoing a merge moves a rewritten row back to its source entity) and `correct_interval` (`PATCH /kbs/{kb}/facts/{id}`) now copy `layer` and `phrase` too. Before, either one turned an open statement into a typed row with no predicate, and the next recompute retired its typed row without building it again. These are the only rewrites that copy a row of `facts`; the other inserts write new rows. The last consequence above no longer holds.
