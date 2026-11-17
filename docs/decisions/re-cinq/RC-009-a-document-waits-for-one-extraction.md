# RC-009 · A document waits for one extraction

> re:cinq fork only. Status: accepted 2026-11-17. Upstream: proposed in [deeplethe/utopia#1514](https://github.com/deeplethe/utopia/issues/1514).

## Context

`extract_document` carries a document id and nothing that says which version to read: the extraction reads the document's current chunks when it runs. Nothing checked for a job already waiting when one was enqueued, so a document that changed three times while the queue was behind got three jobs, and the second and third read exactly what the first read.

On our instance on 2026-11-17, 29,493 `extract_document` jobs were queued for 12,986 documents. A mirror re-pushes its statements items after every merge, and each extraction there costs about 75 s of entity matching, so the repeats held new uploads about two days behind.

Two places enqueue the job, `documents::finish_processing_if_current` with `{document_id}` and the memory re-embed in `pipeline.rs` with `{document_id, proposed_by, proposed_token}`. `enqueue_unless_queued` compares the whole payload, so it would not see one as a repeat of the other.

## Decision

**A document has at most one queued `extract_document` job.** `jobs::enqueue_extraction` (and `_tx`) inserts the job only when no job of that kind with the same `payload->>'document_id'` is `queued`, and both sites use it. A `running` job does not count: it may have read the chunks before the new version was written, so a change while it runs still queues one more.

**The first queued job keeps its payload.** A later push folded into it does not replace the proposer it names. Every push to one base comes through one source token on both sites today, so no attribution is lost there.

**The check has an index.** Migration `9011` adds `jobs_queued_extraction_document`, a partial index on `(payload->>'document_id') WHERE kind = 'extract_document' AND status = 'queued'`, so the check reads one entry however long the queue is.

**The repeats already queued are removed once.** The same migration deletes every queued `extract_document` job that has an older queued job for the same document, keeping the oldest, so each document keeps its place in the queue. Running it again deletes nothing.

## Consequences

- A busy queue does each document's extraction once for the version current when it runs, instead of once per change.
- A job can no longer be queued for a document "on top" of a waiting one to force a second read. Re-reading the same chunks would change nothing, so nothing needs that.
- Other job kinds keep their own enqueue rules; this record covers `extract_document` only.
