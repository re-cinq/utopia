import type { SourceView } from "../api";

/** Splits a comma-separated glob into a list. Leave it empty to give undefined: the key does not appear in the JSON, and the backend filters by its default rules */
export const globList = (value: string) => {
  const list = value.split(",").map((g) => g.trim()).filter(Boolean);
  return list.length ? list : undefined;
};

/** The config submitted when editing a github_repo / gitlab_repo source. Saving replaces the whole thing, so keys not shown in the form
 *  (such as `max_file_bytes`) are carried back unchanged from the stored copy; leaving the token empty sends none, and the backend keeps the original value.
 *  GitLab stores the project path in `project`, GitHub stores it in `repo` */
export function githubRepoConfig(
  saved: NonNullable<SourceView["config"]>,
  form: { repo: string; authHeader: string; include: string; exclude: string },
  key: "repo" | "project" = "repo",
) {
  return {
    ...saved,
    [key]: form.repo.trim(),
    ...(form.authHeader.trim() ? { auth_header: form.authHeader.trim() } : {}),
    include: globList(form.include),
    exclude: globList(form.exclude),
  };
}
