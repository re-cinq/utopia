# RC-007 · An old version keeps no vector

> re:cinq fork only. Status: accepted 2026-11-06. Upstream: not yet proposed.

## Context

`replace_chunks` supersedes a chunk the new version of its document no longer carries and keeps the row whole: the text for version replay, the vector so an as-of search still finds it ([0019](../0019-the-second-clock-can-be-rewound.md), open question ②, where upstream chose to stop clearing the vector), and the row itself because `fact_evidence`, `pending_facts` and `time_mentions` cascade on it. That reading was made for prose that is edited now and then. A `statements` source ([0054](../0054-a-source-may-push-statements-in-the-open-contract.md)) pushes everything on every pass, and every paragraph that changed leaves a superseded chunk with a 3072-dimension vector behind. In one of our bases the `chunks` table reached 14 GB, most of it superseded chunks nothing will search.

What refers to a superseded chunk, read from the schema and the code that writes it:

- A paragraph that did not change is **claimed** by the new version: the row keeps its id, and the evidence on it already points at the live version. Nothing to repoint.
- A paragraph that did change is superseded, and its evidence stays on the superseded row. That is not a leftover: it is the signal [RC-004](RC-004-a-source-closes-its-unconfirmed-facts.md) and [RC-006](RC-006-a-pushed-item-closes-what-its-new-version-stopped-stating.md) close the fact on, and the close time is the chunk's `superseded_at` (`unconfirmed::candidates`). The Review page's unconfirmed queue and the as-of reads of `record_axis::chunk_live_at` read the same column. Deleting such a chunk would cascade its evidence away, and a fact with no evidence is nowhere in the queue.
- `pending_facts.chunk_id` and `time_mentions.chunk_id` cascade the same way.
- The chunks of a deleted document are superseded, not deleted ([0022](../0022-deleting-is-an-event.md)), and come back when the deletion is reverted from `document_deletions.superseded_chunks`.
- `document_versions` holds no chunk reference, only the sha256 of each version's blob, one small row per version. It stays.

## Decision

**A chunk superseded more than seven days ago loses its vector.** A daily job, `prune_superseded`, runs once per base and sets `embedding` to NULL on every chunk of the base whose `superseded_at` is older than the cutoff, in batches of 10,000. The text stays, so version replay and the evidence quote are untouched.

**It loses its row only when nothing refers to it.** The same job deletes the chunks past the cutoff that no `fact_evidence`, `pending_facts` or `time_mentions` row references and whose document is not deleted. A chunk evidence points at keeps its row and its text, with no vector; a deleted document keeps its chunks for the revert.

**Seven days is the default, the job payload may say otherwise.** `{"kb_id": …, "days": N}`.

**The index is asked for, not rebuilt.** After a pass that changed anything the job calls `vector_index::request` for the base's live dimension, as every write of a vector does; the HNSW index sheds the pruned entries on its own. Reclaiming the disk is `VACUUM`'s business.

**One partial index.** Migration 9009 adds `chunks_superseded_idx (kb_id, superseded_at) WHERE superseded_at IS NOT NULL`, so a day's pass reads only the superseded chunks of the base. On a large base the migration holds `chunks` for the build, as 9000 did on `fact_evidence`.

## Consequences

- An as-of vector search for a moment more than seven days back no longer finds the paragraphs that were replaced since. Full-text search never held them. The record axis still answers which chunk was live when; only the nearest-neighbour question loses them. This is the trade 0019 left open, taken in one direction for this fork.
- A fact closed by RC-004 or RC-006 keeps its evidence and the quote it was read from. The evidence card opens a superseded chunk's text as before.
- In a base of mirrored statements most superseded chunks carry evidence, so the row count falls less than the disk does: the vector is the larger part of the row by far.
- A pushed statements item whose vector is never searched is a separate question: whether to embed it at all.

## Open questions

- Whether evidence on a superseded chunk of a fact that is already closed could be moved to the fact's `facts` row (a quote and a document version), so the chunk row can go too.

*Revised 2026-11-10.* Three corrections, found while proposing this upstream (deeplethe/utopia#1492):

- **A deleted document is left whole, vectors included.** Deleting a document supersedes its live chunks and restoring it makes them live again, so a vector cleared in between brought the document back unsearchable by meaning. Both halves of the job now skip a deleted document.
- **A chunk a role word points at keeps its row.** Since upstream's migration 0121 `statement_qualifiers` references `chunks` and cascades, so deleting such a chunk would silently take its role words with it.
- **The index is no longer asked for.** `vector_index::request` only queues a build when the index for a dimension is missing, and a prune cannot make it go missing.
