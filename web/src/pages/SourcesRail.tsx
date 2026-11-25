/* The sources rail ("a source is a folder"): the left-side navigation shared by Library and DocViewer. */
import type { SourceKind } from "../sourceKinds";
import { useQuery } from "@tanstack/react-query";
import {
  Archive,
  BookOpen,
  Braces,
  Brain,
  Briefcase,
  CircleDot,
  Building2,
  Cloud,
  Database,
  FileText,
  FlaskConical,
  FolderOpen,
  Globe,
  HardDrive,
  Inbox,
  Library as LibraryIcon,
  Newspaper,
  Notebook,
  Plus,
  Puzzle,
  Rocket,
  Rss,
  Server,
  SquareKanban,
  Trash2,
  Upload,
  Users,
  Webhook,
  type LucideIcon,
} from "lucide-react";
import { api, type SourceView } from "../api";
import { S } from "../i18n";
import { RAIL_CLS, RailItem } from "../ui";

/** Left column selection: all / manual upload / a particular source id */
/** "deleted" = tombstone view (#268): documents that were deleted but can still be restored or purged */
export type LibrarySelection = "all" | "uploads" | "deleted" | string;

/** Optional source icons (lucide icon name → component). */
export const SOURCE_ICONS: Record<string, LucideIcon> = {
  "folder-open": FolderOpen,
  globe: Globe,
  rss: Rss,
  webhook: Webhook,
  "book-open": BookOpen,
  newspaper: Newspaper,
  "file-text": FileText,
  database: Database,
  cloud: Cloud,
  "hard-drive": HardDrive,
  inbox: Inbox,
  archive: Archive,
  briefcase: Briefcase,
  "building-2": Building2,
  "flask-conical": FlaskConical,
  notebook: Notebook,
  rocket: Rocket,
  server: Server,
  users: Users,
};

// The map is keyed fully by SourceKind: adding a kind of source without an icon turns tsc red
export const KIND_ICON: Record<SourceKind, LucideIcon> = {
  folder: FolderOpen,
  url: Globe,
  rss: Rss,
  api: Webhook,
  statements: Braces,
  custom: Puzzle,
  github_issues: CircleDot,
  github_repo: FileText,
  gitlab_repo: FileText,
  gitlab_issues: CircleDot,
  jira_issues: SquareKanban,
  s3: HardDrive,
  azure_blob: Cloud,
  gcs: Cloud,
  webdav: FolderOpen,
  notion: Notebook,
  google_drive: HardDrive,
  memory: Brain,
  upload: Upload,
};

/** Source types with pull/sync semantics (folder/api have no sync concept) */
export const SYNCING_KINDS = new Set([
  "url",
  "rss",
  "custom",
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
]);

export const SYNC_DOT: Record<SourceView["last_sync_status"], string> = {
  never: "bg-ink-2",
  queued: "bg-warn",
  running: "bg-warn animate-pulse",
  ok: "bg-ok",
  failed: "bg-danger",
};

export function sourceIcon(s: SourceView): LucideIcon {
  // Built-in type icons are fixed; only custom respects the icon the user picked
  if (s.kind === "custom" && s.icon && SOURCE_ICONS[s.icon]) return SOURCE_ICONS[s.icon];
  return KIND_ICON[s.kind] || Globe;
}

export function SourcesRail({
  kbId,
  active,
  onSelect,
  onAdd,
}: {
  kbId: string;
  active: LibrarySelection | null;
  onSelect: (sel: LibrarySelection) => void;
  /** Hide the "+" by default (e.g. on the document view page) */
  onAdd?: () => void;
}) {
  // The left column only needs two numbers: how many documents in the whole library, and how many have no source. **Each takes a page of zero rows**:
  // the counts come back with the response, so there is no need to pull documents down and count them
  const docs = useQuery({
    queryKey: ["docCount", kbId],
    queryFn: () => api.documents(kbId, { limit: 1, offset: 0 }),
  });
  const uploads = useQuery({
    queryKey: ["docCount", kbId, "uploads"],
    queryFn: () => api.documents(kbId, { source: "none", limit: 1, offset: 0 }),
  });
  const sources = useQuery({
    queryKey: ["sources", kbId],
    queryFn: () => api.sources(kbId),
  });

  const sourceList = sources.data?.sources ?? [];
  const uploadsCount = uploads.data?.total ?? 0;

  return (
    <aside className={`${RAIL_CLS} flex flex-col`}>
      {/* All documents pinned at the top as the first-level entry. **No count**: it is the total, and the rows below already say
          where the documents are; this number would be pure decoration */}
      <div className="px-2 pt-3">
        <RailItem
          active={active === "all"}
          onClick={() => onSelect("all")}
          icon={<LibraryIcon size={14} />}
        >
          {S.library.allDocs}
        </RailItem>
      </div>
      {/* Creating a new source is a row, with the same vocabulary as Chat's 'New chat' and the ontology page's 'New class':
          the plus sign sits in the icon slot; the row itself looks the same as the rows around it: this rail has only one kind of thing from start to finish.
          Pages without onAdd (the document view page) do not render this row */}
      {onAdd && (
        <div className="px-2 pt-1">
          <RailItem icon={<Plus size={14} />} onClick={onAdd}>
            {S.library.newSourceTitle}
          </RailItem>
        </div>
      )}
      <div className="u-rail-list u-scroll flex-1 overflow-y-auto px-2 pt-1 pb-3">
        {/* Uploads: a permanent default source (the default destination for uploads; cannot be deleted) */}
        <RailItem
          active={active === "uploads"}
          onClick={() => onSelect("uploads")}
          icon={<Upload size={14} />}
          count={uploadsCount}
        >
          {S.library.uploads}
        </RailItem>
        {sourceList.map((s) => {
          const Icon = sourceIcon(s);
          return (
            <RailItem
              key={s.id}
              active={active === s.id}
              onClick={() => onSelect(s.id)}
              icon={<Icon size={14} />}
              count={s.doc_count}
              dot={
                SYNCING_KINDS.has(s.kind) || s.kind === "api" || s.kind === "statements"
                  ? SYNC_DOT[s.last_sync_status]
                  : undefined
              }
            >
              {s.name}
            </RailItem>
          );
        })}
      </div>
      {/* Deleted (#268): tombstones wait here to be restored or purged. When there are none, it takes no row */}
      {((docs.data?.deleted ?? 0) > 0 || active === "deleted") && (
        <div className="px-2 pb-3">
          <RailItem
            active={active === "deleted"}
            onClick={() => onSelect("deleted")}
            icon={<Trash2 size={14} />}
            count={docs.data?.deleted ?? 0}
          >
            {S.library.deleted}
          </RailItem>
        </div>
      )}
    </aside>
  );
}
