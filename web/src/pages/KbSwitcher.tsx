/* The top-bar knowledge base switcher: the same popover family as the user menu and the alert panel (shadcn Popover).
   All the KBs are listed below, with a tick on the current one. Previously each of these three places wrote its own "the capsule grows into the panel in place"
   transition, and the panel's first row had to redraw the capsule itself; once unified, that row is gone,
   and the trigger is simply the trigger.

   The panel has three sections: search, KBs, new. Once there are a dozen or more KBs, switching KB means **type first, then pick**,
   not scroll a column of names; "new" is pinned at the bottom rather than mixed into the list, because it is not a KB, it is
   the exit of this panel. */
import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { ChevronDown, Layers, Plus } from "lucide-react";
import { api, type Kb } from "../api";
import { S } from "../i18n";
import { Button, Input, Row } from "../ui";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { KbMenuList } from "./KbMenuList";
import { fetchMe } from "../authSession";

export function KbSwitcher({
  kb,
  kbs,
  onChange,
}: {
  kb: Kb | null | undefined;
  kbs: Kb[];
  onChange: (id: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const close = () => setOpen(false);
  const navigate = useNavigate();
  const name = kb?.name ?? "…";
  const [q, setQ] = useState("");
  // Creating a KB needs system administrator or workspace Admin+ (see create in api/kbs.rs): people without that permission
  // do not see the entry point, saving them from clicking through and getting a 403
  const me = useQuery({ queryKey: ["me"], queryFn: fetchMe });
  const wsRole = useQuery({
    queryKey: ["workspaceRole", kb?.workspace_id],
    queryFn: () => api.workspaceRole(kb!.workspace_id),
    enabled: !!kb?.workspace_id,
  });

  // Closing discards the search term: next time it opens you pick from scratch, not continuing the last filtered result
  const dismiss = () => {
    setQ("");
    close();
  };
  const needle = q.trim().toLowerCase();
  const shown = needle
    ? kbs.filter((k) => k.name.toLowerCase().includes(needle))
    : kbs;

  return (
    /* Knowledge base switcher. **The popover is shadcn's Popover**, the same as the other two panels in the top bar:
       previously it was a hand-written "capsule grows into the panel in place" transition, the first row had to redraw the capsule itself,
       and the trigger had to go invisible when opened. Now the trigger is the trigger, and the panel is the panel. */
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        {/* The capsule: no background or border; the icon is the same grey tier as the navigation labels, with the same size and stroke width (15 / 1.8),
            the name uses the same font size and weight as the labels (body, medium), and the colour is body colour: it is the name of the current KB,
            not secondary information */}
        <Button
          variant="ghost"
          size="sm"
          title={S.nav.kbLabel}
          /* Inner padding 12, the same figure as the rows across the site and the pl-3 of the top-bar wordmark.
             It used to be 8, with the icon sitting against the left edge of the capsule: once the capsule had a background (on hover, when open), that was visible */
          className="h-8 max-w-64 border-0 px-3"
          icon={<Layers size={15} strokeWidth={1.8} className="text-ink-2" />}
        >
          <span className="truncate text-body font-medium text-ink">{name}</span>
          <ChevronDown size={12} className="shrink-0 text-ink-2" />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        align="start"
        className="w-max min-w-64 max-w-80 overflow-hidden p-0"
      >
          {/* Search: no box of its own (bare): it is a section of the panel, not a control placed inside the panel.
              It is focused on open, so you can type as soon as you start; Esc and outside clicks are handled uniformly by Popover */}
          <div className="border-b border-line px-4 py-3">
            <Input
              bare
              autoFocus
              className="w-full text-body"
              placeholder={S.nav.findKb}
              value={q}
              onChange={(e) => setQ(e.target.value)}
            />
          </div>
          <div className="u-scroll max-h-80 overflow-y-auto">
            <KbMenuList
              kbs={shown}
              currentId={kb?.id}
              grouped={!needle}
              onPick={(id) => {
                dismiss();
                if (id !== kb?.id) onChange(id);
              }}
            />
            {shown.length === 0 && (
              <p className="px-4 py-3 text-body text-ink-2">{S.nav.noKbMatch}</p>
            )}
          </div>
          {/* "New" is pinned at the bottom, not mixed into the list: it is not a KB. It goes to "My knowledge bases" with create set,
              so it lands directly on the form, and the person does not have to find the button again once on that page */}
          {(me.data?.is_admin ||
            wsRole.data?.role === "admin" ||
            wsRole.data?.role === "owner") && (
            <Row
              density="menu"
              className="gap-3 border-t border-line px-4 py-3 text-body"
              icon={<Plus size={14} />}
              onClick={() => {
                dismiss();
                navigate({ to: "/account/kbs", search: { create: true } });
              }}
            >
              {S.settings.kbs.newKb}
            </Row>
          )}
      </PopoverContent>
    </Popover>
  );
}
