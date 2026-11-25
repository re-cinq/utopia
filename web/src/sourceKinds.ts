/**
 * The kinds of source: **this frontend list is written only here, once**.
 *
 * The backend copy is the `SourceKind` enum in `crates/utopia-core`; validation at creation and dispatch during sync both come from it.
 * The `utopia-store` tests read this file and check the two sides against each other: if either side has one kind more or one fewer,
 * `cargo test` goes red. Previously each side was hand-written: five connectors added a sync branch and appeared in the UI,
 * but did not make it into the creation allowlist: they could be picked but not created (#247).
 *
 * The order is the order in the add-source dialog.
 */
export const CREATABLE_SOURCE_KINDS = [
  "folder",
  "url",
  "rss",
  "github_issues",
  "github_repo",
  "gitlab_repo",
  "gitlab_issues",
  "jira_issues",
  "s3",
  "azure_blob",
  "gcs",
  "webdav",
  "notion",
  "google_drive",
  "api",
  "statements",
  "custom",
] as const;

export type CreatableSourceKind = (typeof CREATABLE_SOURCE_KINDS)[number];

/** Two kinds that cannot be created by a person can still exist in the library: `memory`, which each knowledge base has built in, and `upload`, from old data */
export type SourceKind = CreatableSourceKind | "memory" | "upload";

/**
 * The few fields that GitHub, GitLab and Jira source sync cannot do without: if they are left empty, creation still succeeds and it is stored, but every sync afterwards
 *  only reports an error. If the repo is `owner/name`, paste a whole GitHub link in and the backend likewise rejects it on every round.
 */
export function hasSyncFields(
  kind: string,
  f: { repo?: string; jiraUrl?: string; jiraProject?: string; token?: string },
): boolean {
  if (kind === "github_issues" || kind === "github_repo") {
    return /^[^/\s]+\/[^/\s]+$/.test(f.repo?.trim() ?? "");
  }
  // A GitLab project path can have several levels of subgroups: group/sub/name
  if (kind === "gitlab_repo" || kind === "gitlab_issues") {
    if (!/^[^/\s]+(\/[^/\s]+)+$/.test(f.repo?.trim() ?? "")) return false;
    // gitlab_issues notes and events return 401 without a token, so the token is required at creation. Not sent on edit: leaving it empty keeps the value already in the library
    return kind === "gitlab_repo" || f.token === undefined || f.token.trim() !== "";
  }
  if (kind === "jira_issues") return Boolean(f.jiraUrl?.trim() && f.jiraProject?.trim());
  return true;
}
