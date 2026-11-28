# Utopia — System Specification

Describes the system as it exists today (v0.1, release candidates), derived from the README, workspace layout, migrations and `docs/`.

## Overview

Utopia is an open, self-hostable enterprise knowledge base built around a bitemporal knowledge graph and an editable ontology. Documents and other sources are ingested, extracted into entities and facts that follow the ontology, and queried through search, chat and MCP. Every fact records when it held and when the system learned it, and corrections close earlier versions instead of overwriting them. It ships as one Rust binary plus one Postgres (pgvector); it can run offline against any OpenAI-compatible model endpoint.

## Key capabilities

- **Ingest**: PDF, DOCX, PPTX, XLSX/XLS/ODS, CSV/TSV, Markdown, HTML and text; OCR and transcription via external services; web pages, RSS and Google Drive sources; pushed observations in the extraction contract.
- **Search and chat**: Tantivy full-text (CJK-aware) and pgvector vectors fused with RRF; streaming answers with citations; an agent that searches, walks the graph and queries mounted databases.
- **MCP**: a per-knowledge-base MCP server exposing read-only typed tools; writes arrive as proposals for human review.
- **Ontology**: packs (schema.org, W3C Org, PROV-O, FOAF, IOF Core) as cold start, OWL/RDF import, editable types, relations and axioms.
- **Bitemporal graph**: valid-time and learned-time on every fact, with provenance and versioned corrections.
- **Resolution and review**: exact, embedding, then model-adjudicated entity resolution; undoable merges; a review queue for doubtful cases; conflict detection with explicit resolutions.
- **Reasoning**: axioms compile to forward-chaining rules (transitivity, symmetry, inverses, hierarchy); derived facts are marked and carry premises. Off by default.
- **Ontology-driven querying**: mounted Postgres, MySQL-protocol engines, ClickHouse, Trino, Databricks and Snowflake queried alongside documents.
- **Governance**: an append-only decision ledger, alerts, audit trail, source decay.
- **Deployment**: Docker Compose, container images (amd64/arm64) on ghcr.io, a Helm chart under `deploy/`.

## Core data model

Postgres schema evolved by roll-forward-only migrations in `migrations/`:

- Organizations, users, knowledge bases (bases), members and roles; personal and push tokens.
- Sources, documents, versions, chunks (with embeddings) and a Postgres-backed job queue.
- Entities, aliases, facts (typed, with valid-time and learned-time, evidence and supersession links), relations.
- Ontology: types, relations, axioms, packs, mappings to data-source tables.
- Resolution candidates and review items; reasoning rules and derived facts; axiom violations.
- Conversations and messages; alerts; the decision ledger and audit records.
- Encrypted credentials (LLM keys, connection strings, source tokens).

## User roles

- **System administrator**: the first registered account; manages users, models and deployment settings.
- **Knowledge-base owner / admin / editor / viewer**: per-base roles. Open bases are readable by all users; restricted bases by invitation only.
- **Agents**: connect over MCP with a token scoped to a base; read-only, writes are proposals.
- **Auditors / reviewers**: read the ledger and work the review queue.

## Business rules

- Corrections never overwrite: the old fact version is closed and linked to its replacement.
- Facts always carry provenance (document, passage) and both time axes.
- Uncertain extractions, suspected duplicates and cardinality conflicts go to human review; merges are reversible.
- The ontology is a contract: extraction and rules follow it; axiom violations are surfaced with explicit choices (retract, relax, accept).
- Derivation is opt-in; derived facts are distinguishable from asserted ones.
- The ledger is append-only and outlives its objects.
- Credentials are encrypted at rest with a key kept outside the database.
- Migrations only roll forward; version numbers must be unique.
- Language follows the reader of each text (UI strings per person; extracted data quoted verbatim).

## Success metrics

- Temporal question answering accuracy on the bundled benchmark corpora (`scripts/bench`, e.g. the contracts corpus).
- Every answer and fact traceable to evidence; every review decision recorded in the ledger.
- Works air-gapped: install and first answer with a self-hosted model endpoint and no external services beyond Postgres.
- CI green on fmt, clippy, tests (including database-backed tests), migrations replayed twice on a fresh database, chart lint/render, and web build.
