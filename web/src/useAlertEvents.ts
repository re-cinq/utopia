// Alert event subscription. **Global, not per base**: the top-bar badge spans bases, and system-level alerts have no base at all.
//
// The message the server pushes carries no data and does not check permissions (see alerts_routes::stream): on receipt it refetches,
// and who can see what is decided by the list query. So nothing here needs to know which base is current.
//
// A page with a base open does not need this: the base event stream carries alerts too (`useKbEvents`), so a page holds only one connection
// (#1028). When `active` is false, nothing is subscribed.
import { useEffect } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { subscribeEvents } from "./eventStream";

export function useAlertEvents(active = true) {
  const queryClient = useQueryClient();
  useEffect(() => {
    if (!active) return;
    // Alerts are not replayed during a disconnect: on recovery, refresh the badge and the list together.
    const refresh = () => queryClient.invalidateQueries({ queryKey: ["alerts"] });
    return subscribeEvents("/api/v1/alerts/events", { alert: refresh }, refresh);
  }, [active, queryClient]);
}
