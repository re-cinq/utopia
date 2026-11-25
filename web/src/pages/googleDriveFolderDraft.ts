import type { SourceView } from "../api";

type Folder = { id: string; name: string };
type Draft = { accountKey: string; folders: Folder[] };

export function driveFolderAccountKey(
  kbId: string,
  sourceId: string,
  { connected_at, connected_by, account_email }: NonNullable<SourceView["config"]>,
) {
  // As on the server, only a verified email is used to identify the same account; otherwise tell them apart by this authorization.
  const email = account_email?.replace(/[A-Z]/g, (letter) => letter.toLowerCase());
  const account = email ? ["email", email] : ["grant", connected_at];
  return JSON.stringify([kbId, sourceId, connected_at ? [connected_by, account] : null]);
}

export function reconcileDriveFolderDraft(
  accountKey: string,
  draft: Draft | null,
  saved: Folder[],
): Draft | null {
  if (draft === null || draft.accountKey !== accountKey) return null;
  const sameFolders =
    draft.folders.length === saved.length &&
    draft.folders.every((folder, index) => folder.id === saved[index].id);
  return sameFolders ? null : draft;
}
