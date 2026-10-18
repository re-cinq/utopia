# RC-002 · A pushed name is not recalled by containment

> re:cinq fork only. Status: accepted 2026-10-18. Upstream: the second of the three recall paths in [deeplethe/utopia#1293](https://github.com/deeplethe/utopia/issues/1293). The first, name vectors, is in [0054](../0054-a-source-may-push-statements-in-the-open-contract.md)'s revision of 2026-10-17.

## Context

When resolution creates an entity, `containment_reviews` looks for existing entities whose name contains the new one or is contained in it, and queues a review pair for each, so a short name ("Holmes") and a full one ("Sherlock Holmes") meet in review. It matches with `LIKE '%…%'` in both directions. No index can serve that, so every new name scans the base's entities and name facts.

A `statements` source pushes machine-made identities that share a prefix: `owner/repo:src/a.ts`, `owner/repo:src/a.ts#parse`, `owner/repo:src/a.ts::adds two`. Containment pairs them on every push, and they are never one thing. On Lore's deployment, with re.13 live, this search was the slowest query at a median of 12 s and a maximum of 99 s per new name. It held the database at its CPU limit, and 207 extraction jobs timed out waiting for a connection in five minutes.

## Decision

A named entity in a pushed chunk is resolved by `resolution::resolve_pushed_name`. It recalls by spelling alone: it attaches to an entity of that name or creates one, and runs neither the name-vector channel nor the containment search, so it queues neither kind of pair. Every other document is resolved by `resolve_mention`, unchanged.

## Consequences

- Pushes no longer scan the base for containing names, and the duplicates queue gets no `contains|` pairs from them.
- Two pushed names for one thing that differ beyond case and whitespace stay two entities until an `n` row or a reviewer joins them.
