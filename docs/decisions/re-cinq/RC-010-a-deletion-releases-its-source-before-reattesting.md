# RC-010 · A deletion releases its source before reattesting

> re:cinq fork only. Status: accepted 2026-11-17. Upstream: to be proposed as an issue first.

## Context

`documents::delete` and `restore_tx` open with `lock_document_source_tx`, which locks the document's `sources` row (`FOR UPDATE`), the ordering that keeps them from crossing a sync of the same source. In the same transaction they then recompute the attestation of every fact the document cites (`reattest_tx`) and tidy the timelines those facts sit on. For a document that cites many facts, that recompute takes tens of seconds.

Every push to a statements source writes the same `sources` row (`start_run`, `finish_sync`). On our instance on 2026-11-17, a mirror's `POST …/missing/cleanup` was deleting the items it had tombstoned, one 34-second delete held the row, ten pushes to that source waited behind it, and the mirror's requests timed out.

## Decision

**The recompute leaves the transaction that holds the source row.** `delete` and `restore_tx` still invalidate or restore facts and lock the cited timelines as before, but where they recomputed they now enqueue one `reattest_facts` job, `{kb_id, fact_ids}`, committed with the invalidation, so a process that stops right after the commit still leaves the work queued.

**`documents::reattest_facts` does the work in a transaction of its own.** It finds the facts' timelines, locks them in the usual order, recomputes attestation (`reattest_tx`), tidies the timelines, and commits. It never touches the source row. The tidy moves with the recompute because timelines are ordered by the attested dates.

**A caller that owns its transaction runs it right after the commit.** `delete` and `restore` call it as soon as the source row is released and then remove the queued job if no worker has claimed it, so when they return, attestation and timelines are as current as they were before this record. A worker that claimed the job first runs it again, which writes nothing new. `restore_tx` inside a caller's transaction (an upload that meets a tombstone, a sync that revives an entry) leaves the job to the worker.

## Consequences

- A push no longer waits on a deletion's recompute; it waits only on the short invalidation.
- Between a deletion's commit and its recompute (milliseconds when `delete` runs it, one worker pass otherwise), a fact can still carry the date of the evidence just deleted, and a timeline can still show a predecessor closed by a row now invalidated.
- The job is idempotent: it computes from the evidence as it stands when it runs.
