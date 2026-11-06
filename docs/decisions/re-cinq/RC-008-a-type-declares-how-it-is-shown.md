# RC-008 · A type declares how it is shown

> re:cinq fork only. Status: accepted 2026-11-06. Upstream: to be proposed as an issue first; [0031](../0031-an-event-holds-at-the-moment-it-names.md)'s `utopia:temporal` is the precedent.

## Context

Utopia's entity panel is generic: a title, the facts grouped by direction, each row a predicate and an object. For a base whose types have a shape of their own it reads as a list of rows. In Lore's base a statement is a quoted sentence, a test has a status with a tone, a file is a tree of definitions, and a statement leads to its tests, from a test to the file lines it executes, and from those lines to the history that changed them. Nothing in Utopia knows any of these words, and it must not: the UI serves every base.

The ontology file already carries one word Utopia itself defines. [0031](../0031-an-event-holds-at-the-moment-it-names.md) reads `utopia:temporal` from an object property, the export writes it back, and a value the file did not say leaves the relation alone. The same door can carry how a type is shown.

## Decision

**A class or property may carry literal-valued `utopia:*` annotations that say how it is shown.** The ontology import reads them, the store keeps them, the API serves them, and the export writes them back. The vocabulary is fixed, and every word in it names a rendering, never a domain:

| On | Key | Values | Rendering |
|---|---|---|---|
| Class | `utopia:panel` | `card` / `tree` / `quote` | header style of the panel and chain cards |
| Class | `utopia:chain_first` | `[in:\|out:]<relation key>` | first hop of the chain view from this class |
| ObjectProperty | `utopia:role` | `children` | objects nested under the subject, recursing on the same relation |
| ObjectProperty | `utopia:role` | `membership` | breadcrumb upward along the in-direction |
| ObjectProperty | `utopia:chain_next` | `[in:\|out:]<relation key>` | the hop after this one; absent = chain ends |
| DatatypeProperty | `utopia:role` | `title` / `status` / `span_start` / `span_end` / `link` / `key` / `quote` | header text / status dot + words / `L12–L30` / permalink / monospace key / quoted text |
| DatatypeProperty | `utopia:tones` | `"v1=ok,v2=danger,v3=warn"` | value → tone for the `status` role |
| Qualifier attribute | `utopia:role` | `range` | `"12-30,44-50"` rendered inline as ranges |

A direction prefix defaults to `out`. The parser keeps the five keys `panel`, `chain_first`, `role`, `tones`, `chain_next` (`HINT_KEYS` in `utopia-ingest::ontology_rdf`); a key given twice keeps its first value; any other `utopia:*` word stays reported as unprojected in the import preview, as a `utopia:temporal` with a fourth value does.

**Stored flat.** `entity_types.display_hints` and `relation_types.display_hints` (migration `9009`) hold one object of string values per type, `{"role":"status","tones":"passed=ok,failed=danger"}`, `{}` when the file says nothing. `validate_display_hints` refuses anything that is not an object of strings, on every path in: the import, the bulk insert and the ontology page's create and update bodies, which accept an optional `display_hints` and leave the column alone when it is absent, as they do with `disjoint`.

**The file is the truth about the hints, and a re-import overwrites them.** This is where the hints part from `temporal`. A `temporal` value is a judgement a person may have revised on the page, so an unannotated re-import leaves it alone. A hint is display: a file that stops saying `utopia:panel "quote"` means the panel is generic again, so the import writes what the file says, `{}` included. The one consequence for the import is that an attribute the file already brought in is now refreshed on re-import, label and description with it, as a relation already was; before this record the bulk insert only skipped it.

**The export writes every hint back** as `utopia:<key> "<value>"` beside `utopia:temporal`, so an export re-imports to the same hints.

**What a base gets from it, without the UI knowing the words.** Lore's file says `lore:Statement utopia:panel "quote" ; utopia:chain_first "out:validated_by"`, `lore:outcome utopia:role "status" ; utopia:tones "passed=ok,failed=danger"`, `lore:contains utopia:role "children"`, `lore:validated_by utopia:chain_next "out:covers"`, `lore:covers utopia:chain_next "in:changes"`. The panel then shows a quoted statement with its tests as cards, each with a status dot, and the chain view walks Statement › Tests › Files › History. A base whose file says nothing sees the panel it sees today.

## Consequences

- This record is the backend cut: parse, store, serve, export. The hinted panel and the chain view that read the hints are the next two cuts; until they land, hints arrive at the UI as a `display_hints` field nothing draws.
- A person can set hints on the ontology page's API, but the next re-import of the file replaces them. Hints belong in the file.
- The vocabulary grows only by a decision: a new key is a new row in the table above and a new entry in `HINT_KEYS`, not a free-form field.

## Dead ends

- **A second settings table keyed by type.** It would keep the hints out of the ontology columns, and it would also keep them out of the export, so a base moved by export and import would lose its panels. The file carries `temporal` already; carrying five more literals costs nothing.
- **Per-base UI configuration in the web app.** The base's shape is in its ontology; a second place to describe it drifts from the first. Lore's TTL is versioned and re-imported on every bump, and that is the one moment the hints should change.
- **Domain-named components in the UI** (`TestCard`, `SpecPanel`). They would serve one base. Every name in the vocabulary is a rendering word so that a base about contracts or sites reads the same table and gets the same panels.

## Rendering

The two cuts that read the hints live in `web/src/pages/hinted/`, and nothing in them names a domain: components, helpers, strings and fixtures speak only the vocabulary above.

- **The hinted panel.** The graph's entity panel switches its relations body to `HintedPanel` when the entity's class carries hints or one of its facts uses a hinted property (`hasHints`). Only that body changes: names, history, derived facts and the header stay as they are, and every fact row is still the panel's own `FactRow`, handed in as a `renderRow` closure so evidence, pinning and hover keep working. `layoutFor` turns the hints into a header (title, `Status` dot with words toned by `utopia:tones`, `L12–L30` span, monospace keys, a link button) styled by `utopia:panel`, a breadcrumb along `membership` (at most four up), a tree along `children` whose deeper levels are fetched on request, and the remaining facts grouped by predicate and direction, with `range` qualifiers written out under their rows.
- **The chain page.** `/kb/<id>/chain?entity=<id>` follows the start entity's `chain_first`, then each relation's `chain_next` (at most six hops, a hop never taken twice), one column per hop, filled from the previous column's entities as their facts arrive. Each entity is a row with the compact header; clicking it opens the entity in the graph. The panel's "Open chain" is the way in; the page is not a tab.
