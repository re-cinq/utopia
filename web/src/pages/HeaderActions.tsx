/* The group on the right side of the top bar: the first link (Docs inside the App, back to the App on the Docs / account pages) +
   the GitHub·version capsule + the alert bell + the user menu. All three top bars share this one copy: previously each assembled its own,
   and the inner padding and gaps differed by a few pixels, so the top-right corner jumped on page changes. */
import { Link } from "@tanstack/react-router";
import type { ReactNode } from "react";
import type { User } from "../api";
import { S } from "../i18n";
import { GithubMark } from "../ui";
import { AlertBell } from "./AlertBell";
import { UserMenu } from "./UserMenu";
import { versionLabel } from "./versionLabel";

export function HeaderActions({
  link,
  version,
  user,
  signedOut,
}: {
  /** The leftmost link: where it goes and what it says */
  link: { to: "/" | "/docs"; label: string };
  version?: string;
  user: User | null | undefined;
  /** What sits in the bell and user-menu position when not logged in (the "Log in" on the Docs page); by default nothing is shown */
  signedOut?: ReactNode;
}) {
  return (
    <div className="ml-auto flex items-center gap-3">
      {/* The project entry is a pair (link + GitHub·version), set closer together than the gap between groups */}
      <div className="flex items-center gap-2">
        <Link to={link.to} className="u-navlink">
          {link.label}
        </Link>
        <a
          href={S.login.githubUrl}
          target="_blank"
          rel="noreferrer"
          title="GitHub"
          className="u-pill"
        >
          <GithubMark size={13} />
          {version && <span className="u-num text-fine">{versionLabel(version)}</span>}
        </a>
      </div>
      {user ? (
        <>
          {/* The alert badge follows the person, not the page: the knowledge base keeps running while reading docs or editing the account */}
          <AlertBell />
          <UserMenu user={user} />
        </>
      ) : (
        signedOut
      )}
    </div>
  );
}
