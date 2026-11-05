# RC-006 · A pushed item closes what its new version stopped stating

> re:cinq fork only. Status: accepted 2026-11-05. Upstream: deeplethe/utopia#1478, pairs with deeplethe/utopia#1477.

## Context

A `statements` item ([0054](../0054-a-source-may-push-statements-in-the-open-contract.md)) is the contract itself. When its next version no longer states a fact, the fact keeps only evidence on superseded chunks and stays open. In one of our bases the statement entity `spec.md#s0128` collected 55 `validated_by` facts from nine different texts, because nothing ever ended the ones a re-push dropped. [RC-004](RC-004-a-source-closes-its-unconfirmed-facts.md) can close them, but only when a caller sweeps the whole source.

## Decision

**When a pushed item's new version has been extracted, the facts it stopped stating are closed.** `run_open` calls `unconfirmed::close_of_document` for a pushed document, after its chunks were read and only when every chunk was (a half-read version could look as if it dropped facts it never got to).

**The scope is the document.** A fact qualifies when it is unconfirmed, every chunk that attests it belongs to this document, and it is not a `known_as` name fact. A typed row with a statement source follows its statements, as in RC-004. It shares RC-004's query, parameterised by source or document, and the same close: `temporal::close_superseded` at the latest `superseded_at` of its evidence, at `second` precision, history kept. Batches of 1000 run until nothing is left, at most 50 rounds. An open statement that closed queues one typed recompute.

**No audit row.** The close is part of the item's own push, which is already audited as an ingest.

**RC-004 stays** for the backlog that built up before this, and for sources whose pushers want to sweep.

## Consequences

- The Review queue of a pushed source empties itself.
- A fact two items state stays open until both drop it.
- Prose documents are untouched: upstream's reading that "no longer mentioned does not mean no longer true" stands for them. Only the extraction of a pushed item triggers a close.
- Between a push and its extraction nothing closes early: the close waits for the extraction of the new version. Whether a reader should call a fact stale in that gap is a separate question (deeplethe/utopia#1477).

## Note, 2026-11-05: upstream's prose reading stands

This decision narrows nothing in upstream's design. It reads a pushed statements item as the contract that it is, and leaves the reading of prose as upstream made it.
