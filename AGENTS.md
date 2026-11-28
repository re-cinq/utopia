# AGENTS.md

Utopia is a bitemporal enterprise knowledge base: a Rust workspace (axum server, sqlx on Postgres with pgvector, Tantivy full-text) plus a React/TypeScript web UI in `web/`. This file tells coding agents how to work in it. `CONTRIBUTING.md` is the authority where the two differ.

## Context loading order

Read these before changing anything, in this order:

1. `CLAUDE.md`, if present (owned by the maintainers; not authored here).
2. `CONTRIBUTING.md` — branch model, CI parity commands, what review sends back.
3. `README.md` — features, quick start, status.
4. `docs/decisions/` — the ADRs. Read the ones touching the area you change; they record why, including rejected approaches. Data model, ontology contract and public API changes need an ADR first.
5. `docs/design/` and `docs/pipeline.md` — subsystem design (ontology, ledger, governance, extraction, identity, chat and MCP).
6. `web/DESIGN.md` — before any UI work.
7. `.env.example` — the `UTOPIA_*` configuration surface.

## Layout

- `crates/utopia-core` — shared types and secrets handling. `utopia-store` — Postgres access and the migration runner. `utopia-server` — HTTP API (axum), serves the SPA. `utopia-ingest`, `utopia-extract`, `utopia-reason`, `utopia-search`, `utopia-llm` — ingestion, extraction, rule derivation, retrieval, model clients. `utopia-cli` — command line.
- `migrations/` — roll-forward-only SQL, numbered `NNNN_name.sql`.
- `web/` — Vite + React 19 + TanStack, pnpm, vitest. UI strings live in `web/src/i18n/en.ts` and `zh.ts`.
- `deploy/chart` — Helm chart (applied by Argo CD on merges to `re-dev`); `docker/` and `docker-compose*.yml` — images and local stack.
- `scripts/` — benchmarks and smoke scripts.

## Workflow commands

Local setup needs Docker, Rust 1.85+, Node 20+, pnpm, and Poppler (`poppler-utils poppler-data`) for the PDF fallback test.

```bash
docker compose up -d db                      # Postgres with pgvector
cargo run -p utopia-server                   # runs migrations, serves :1516
cd web && pnpm install && pnpm dev           # :5173, proxies /api

# What CI runs (checks.yml); run what your change touches
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cd web && pnpm install --frozen-lockfile && pnpm build   # style guard + tsc + vite build
helm lint deploy/chart                       # when touching the chart
```

Tests: CI runs `cargo test --workspace`, `pnpm test` (in `web/`) and `node --test scripts/bench/lib.test.mjs`. Prefer running the specific crate or test file you changed. Database-backed tests skip without `UTOPIA_DATABASE_URL`; set `UTOPIA_TEST_REQUIRE_DB=1` so a missing database fails instead of skipping, and use `utopia_store::test_db::url()` in new tests rather than reading the variable directly.

Deploy: images are published to ghcr.io by `release.yml` on `v*` tags; the Helm chart is synced by Argo CD from `re-dev`. Agents do not deploy.

## Conventions

- Comments explain why (and record the traps that were fallen into), never what.
- Tests cover only what the change introduces; one per behaviour, failing without the change. No `#[ignore]`d tests.
- UI strings go through i18n, in both languages; no hard-coded strings in components.
- Migrations: check the latest number on the base branch before adding one; duplicates break startup.
- Every workflow declares its own minimal `permissions:`.

## Commit conventions

- One English sentence stating the motivation; no long body. Skim `git log` for the register.
- Sign off every commit (`git commit -s`): the project uses the DCO, not a CLA.

## Pull requests

- Base `dev` (contributors); `re-dev` is this fork's integration branch. Never target `main`.
- Fill every section of `.github/PULL_REQUEST_TEMPLATE.md`; the description check requires `## Why`, `## What changes` and `## How it was checked`.
- One PR is one reviewable change; over 500 lines of code and tests combined needs an explanation of why it cannot be split.
- CI (`backend` and `web`) must pass. New features and dependency changes start with an issue.

## Compliance constraints

- Apache-2.0 licensed; DCO sign-off required on every commit.
- Never commit secrets: `.env`, `secret.key`, API keys, connection strings. Credentials are encrypted at rest by the app (`UTOPIA_SECRET_KEY`); keep the key out of the database and the repo.
- The decision ledger is append-only and the schema only rolls forward: do not write migrations or code that rewrite or delete ledger history.
- Report vulnerabilities per `SECURITY.md`, not in public issues.
