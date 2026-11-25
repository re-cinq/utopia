// The KB scope layer: everything under `/kb/$kbId` belongs to this knowledge base.
//
// **Why the KB goes in the path rather than a query parameter**: the KB is a container, not a filter. The graph, search,
// the library, the ontology and review all sit under it. A path can express this containment, and the router backs you up:
// `/kb/$kbId/search` cannot even be built without an id. `?kb=` is optional, and a jump between tabs
// drops it, and it reports no error: so the data quietly shown is from another KB, and the UI looks exactly the same.
//
// **Why localStorage is still needed**: the two answer different questions. The URL answers "what does this link point
// to", localStorage answers "what was I last looking at". So if the URL has it, the URL wins,
// and only if it does not do we fall back to memory (see kb.tsx and KbRedirect below).
import { useEffect } from "react";
import { Link, Outlet, useNavigate, useParams } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";

import { ApiError, api } from "../api";
import { S } from "../i18n";
import { useKb } from "../kb";
import { buttonLike, cn } from "../ui";
import { kbStore, wsStore } from "../wsStore";

/** The landing page when the KB does not exist or there is no permission. **Must not be just an empty graph**: the most common failure of a shared link
 *  is that the other person has no permission, and a blank graph looks like "this KB is empty", which is a different thing. */
function KbNoAccess({ status }: { status: number }) {
  return (
    <div className="grid h-full place-items-center px-6">
      <div className="max-w-sm text-center">
        <h2 className="text-title text-ink">
          {status === 404 ? S.kbScope.missingTitle : S.kbScope.deniedTitle}
        </h2>
        <p className="mt-2 text-small leading-relaxed text-ink-2">
          {status === 404 ? S.kbScope.missingBody : S.kbScope.deniedBody}
        </p>
        <Link
          to="/account/kbs"
          className={cn(buttonLike("ghost"), "mt-4")}
        >
          {S.kbScope.myKbs}
        </Link>
      </div>
    </div>
  );
}

export function KbScope() {
  const { kbId } = useParams({ from: "/app/kb/$kbId" });
  // **Ask the backend directly rather than searching the list in the current workspace**: the link may point to a KB
  // in another workspace; then the list does not contain it, yet the user actually has permission, and judging from the list would wrongly reject them
  const kb = useQuery({
    queryKey: ["kbOne", kbId],
    queryFn: () => api.kbDetail(kbId),
    retry: false,
  });

  // Whichever KB is opened, "last viewed" follows that KB; the workspace is aligned as well,
  // otherwise the switcher in the top bar would still show the previous workspace
  useEffect(() => {
    if (!kb.data) return;
    kbStore.set(kb.data.id);
    wsStore.set(kb.data.workspace_id);
  }, [kb.data]);

  if (kb.isError) {
    const status = kb.error instanceof ApiError ? kb.error.status : 500;
    return <KbNoAccess status={status} />;
  }
  // While loading, draw nothing: this layer is only a scope, and flashing a spinner would look like the page is jumping
  if (!kb.data) return null;
  return <Outlet />;
}

/** Takeover of old paths (addresses without a KB, such as `/graph`): work out which KB to go to, then jump there.
 *
 *  **Cannot redirect directly in beforeLoad**: at that point the KB list has not been fetched yet,
 *  and localStorage may also hold nothing (a new device, or the cache was cleared). So it is made into a component,
 *  which waits until useKb has resolved the KB before moving on. */
export function KbRedirect({
  page,
}: {
  page:
    | "graph"
    | "search"
    | "chat"
    | "library"
    | "ontology"
    | "mappings"
    | "review"
    | "settings";
}) {
  const { kb } = useKb();
  const navigate = useNavigate();
  useEffect(() => {
    if (!kb) return;
    navigate({
      to: `/kb/$kbId/${page}`,
      params: { kbId: kb.id },
      replace: true,
    });
  }, [kb, page, navigate]);
  return null;
}
