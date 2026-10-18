# RC-001 · A pushed document is not reviewed by errata

> re:cinq fork only. Status: accepted 2026-10-18. Upstream: to be proposed on [deeplethe/utopia#1296](https://github.com/deeplethe/utopia/issues/1296).

## Context

The errata agent ([0044](../0044-the-ontology-is-a-view-over-what-documents-say.md) decision 7) re-reads a document's typed facts against the document's own words and keeps, corrects or retracts them. A document pushed by a `statements` source ([0054](../0054-a-source-may-push-statements-in-the-open-contract.md)) has no words to read against: every pushed statement's `quote` is `null`, so the agent can confirm nothing.

It still ran. Every materialisation that added a row queued an `errata_review` for the base. Each review began with `documents_due`, which checks `fact_evidence` by `document_id`. No index covers that column, so on a busy deployment each call ran for 2–10 minutes, eleven at once. Then each document cost a chat-model request.

## Decision

`errata::documents_due` leaves out documents whose source is a `statements` source. Uploaded and synced documents are reviewed as before, including in a base that also receives pushes.

## Consequences

- A base that only receives pushes queues no review work and calls no chat model for errata.
- A pushed statement that alignment typed wrongly is not caught by errata. It can still be retracted on the Review page, or corrected by re-pushing under the same `external_id`.
