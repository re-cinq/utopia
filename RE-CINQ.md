# re-cinq/utopia

This is re:cinq's fork of [deeplethe/utopia](https://github.com/deeplethe/utopia). We run it as
Lore's content store, build features here at our own pace, and send each one upstream once it is
finished and proven. Upstream stays the source of truth: anything that can live there should end
up there.

## Branches

| Branch | What it is | Who writes it |
| --- | --- | --- |
| `main` | A mirror of upstream `main` (its release branch) | Sync only, never commit |
| `dev` | A mirror of upstream `dev` | Sync only, never commit |
| `re-dev` (default) | Upstream `dev` plus our features. Every release is cut from here | PRs into this repo |

Upstream changes reach `re-dev` by merging `dev` into it after syncing `dev` from upstream:

```sh
git fetch upstream
git push origin upstream/dev:dev upstream/main:main
git switch re-dev && git merge origin/dev
```

## Building a feature

1. Branch from `re-dev` and open a PR against `re-dev` in this repo.
2. CI runs the same checks as upstream (fmt, clippy `-D warnings`, `cargo test --workspace`, the
   migrations twice) on the PR and on every push to `re-dev`.
3. Merge when it is green and reviewed.

## Releases

Tag `re-dev` as `v0.1.0-re.N` (annotated, the next free N: check `git ls-remote --tags origin`).
The `release.yml` workflow builds and publishes `ghcr.io/re-cinq/utopia:0.1.0-re.N`. Lore pins the
tag and digest in `infra/terraform/variables.tf` (`utopia_image_tag` / `utopia_image_digest`), so a
deploy is a Lore PR followed by `terraform apply`.

Push one tag at a time: GitHub starts no workflow for a push that carries more than three tags.

## Migrations

Utopia's migrator refuses to start when a migration it has applied is missing from the code
(`ignore_missing` is off), and upstream numbers its migrations in sequence. So:

- **A migration that exists only here is numbered from `9000` up** (`9000_…sql`, `9001_…sql`), and
  keeps that number forever: once it has run in production it can never be renamed or removed.
- **It is idempotent**: `CREATE … IF NOT EXISTS`, `ADD COLUMN IF NOT EXISTS`, `CREATE INDEX IF NOT
  EXISTS`, and backfills guarded so that a second run changes nothing.
- **When the feature goes upstream**, its migration there takes upstream's next number and is
  written the same idempotent way. After the sync, `re-dev` carries both files: on an existing
  database the upstream copy runs second and does nothing, and on a fresh one our `9000` copy runs
  second and does nothing.
- `CURRENT_SCHEMA_VERSION` in `crates/utopia-cli/src/main.rs` counts migration files, so a
  fork-only migration bumps it here, and a sync that brings upstream migrations is reconciled by
  recounting.

## Decision records

A decision record for a fork-only feature goes in `docs/decisions/re-cinq/`, numbered `RC-001`,
`RC-002`… It takes upstream's next number only when its feature is agreed upstream, so the two
numberings never collide.

## Sending a feature upstream

Upstream's maintainer reviews carefully and quickly. These rules keep that sustainable:

1. Agree the shape in an upstream issue first, and wait for a yes. "Not now" means not now.
2. At most one open upstream PR per feature, and only a few in total.
3. Each PR is under 500 changed lines, branched from upstream `dev` (this repo's `dev`), one
   commit with a one-sentence message, signed off (`git commit -s`), and every test fails without
   its change.
4. Answer review within a day, every point.
5. Don't edit the same shared text (ADR 0014's decisions, the `auth.rs` route list,
   `web/src/docs/*.md`) in two open PRs.
6. Change an upstream decision record with a dated note, never in place.

Upstream PRs are opened from this repo (`re-cinq:<branch>` → `deeplethe:dev`).
