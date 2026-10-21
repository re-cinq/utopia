# RC-003 · A source may decay

> re:cinq fork only. Status: accepted 2026-10-21, in progress. Upstream: to be proposed on [deeplethe/utopia#1277](https://github.com/deeplethe/utopia/issues/1277), where it narrows the unnumbered draft "Use decays; the ledger does not" to a per-source option.

## Context

Lore keeps agent memory with a decay model: every memory has a score from 0 to 10 that falls as it goes unread, rises when it is read, and decides which memories go when an agent holds more than 500. Lore is moving its stores into Utopia, and Utopia keeps everything with the same weight forever. A source that pushes memories (0054) grows without bound, and a note nobody has read in a year ranks the same as one read yesterday.

Not every source wants this. A folder of specifications must not forget. So decay is a choice a source makes.

## Decision

**Decay is an option of a source, off by default.** `sources.decay` is NULL until someone sets it, and a source with NULL behaves as it does today and writes nothing extra. When set it is an object with `max_items` (500), `half_life_days` (60), `stale_after_days` (30) and `min_age_days` (30); a missing field takes Lore's value, so `{}` is Lore's behaviour. A field this record does not name is refused.

**The score is Lore's, unchanged.** `decay::score_importance` returns an integer from 0 to 10: `round(10 · 0.5^(age / half-life))`, where age counts from the last retrieval and from creation if there is none, plus adjustments for the length of the text (under 50 UTF-16 units −2, over 500 +1), the key prefixes `auto-curation/` and `session-summary/` (−1 each), the key topics `gotcha`, `decision`, `convention` and `pattern` (+2 each), the retrieval count (5 or more +1, 20 or more +2) and staleness (−1). A document is a Lore memory: its name is the key and its text the value. The test compares the port with scores computed by Lore's own `scoreImportance`, so the two cannot drift apart unnoticed.

**A retrieval is recorded beside the document.** `document_salience` holds a retrieval count, the time of the last retrieval and the half-life, one row per document, and only for documents of a source that decays. Each retrieval adds 2 days to the half-life up to 365. It is a side table because the document row should not change on every read.

**Stale is read, not written.** Lore marks an unread fact `stale` in its confidence column. Here `stale` is worked out when it is read, from the last retrieval and the source's `stale_after_days`, and `confidence` is never touched. It says how sure the source was, [0011](../0011-a-mapping-is-not-a-fact.md) removed the one update in place the ledger ever had, and the review queue reads that column.

## What is built in which step

1. This record, the `sources.decay` column, the `document_salience` table and the score.
2. Setting the option on a source.
3. Recording retrievals.
4. A daily sweep that takes a source over `max_items` back to it: from the documents older than `min_age_days`, the `excess` with the lowest score are deleted the way a person deletes them, so they can be restored.
5. Stale documents rank at 0.6 of their search score, and a result says its score.

## Consequences

- A base where no source sets the option stores nothing new and ranks as before.
- The sweep needs a source to be a bounded list; a source with no cap in its option never loses a document.
- The fact purge Lore runs on facts invalidated more than 30 days ago is not part of this. The ledger is append-only.

## Open questions

- Whether a chunk of the memory log (`memory.rs`) should decay on its own, since it is one document of many chunks.
- Whether feedback (a merged pull request raises a half-life by 5, a rejected one lowers it by 3 down to 7) is reachable from outside, as an API.
