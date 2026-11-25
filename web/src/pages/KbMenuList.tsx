/* The knowledge-base column of the menu, shared by the top-bar switcher and the scope chip of the ask box. Knowledge bases whose name contains `/` are grouped
   into one group (`re-cinq ›`): hovering over it or clicking it pops out the knowledge bases in the group to the right (#1249).

   The submenu goes through the Popover's Portal and is mounted on body, so it is **not** in the DOM of the host menu.
   If the host decides for itself that "a click outside closes it", it must let through elements carrying `data-kb-submenu`; otherwise
   mousedown closes the whole menu first, and the click falls through.

   Keyboard: on the group row, Enter / space / → opens it and sends focus to the first item; ↑↓ move within the submenu;
   Esc / ← closes it and returns focus to that row (kbMenuKeys). DropdownMenuSub is not used: it can only live inside DropdownMenu,
   whereas one host is a Popover with a search box (the menu's typeahead and focus management would fight the search box for keys),
   and the other is a hand-written div. */
import { useEffect, useRef, useState, useSyncExternalStore, type KeyboardEvent } from "react";
import { Check, ChevronRight } from "lucide-react";
import type { Kb } from "../api";
import { Row, cn } from "../ui";
import { Popover, PopoverAnchor, PopoverContent } from "@/components/ui/popover";
import { kbGroups, type KbGroupItem } from "./kbGroups";
import { groupRowKey, submenuKey } from "./kbMenuKeys";

const ROW = "gap-3 px-4 py-2 text-body";
/** The pointer has to cross a gap when moving from the group row to the submenu; closing too fast makes it unreachable */
const CLOSE_DELAY_MS = 150;

const NARROW = "(max-width: 639px)";

/** The screen is too narrow to fit a side submenu (portrait phone) */
function useNarrow() {
  return useSyncExternalStore(
    (changed) => {
      const query = window.matchMedia(NARROW);
      query.addEventListener("change", changed);
      return () => query.removeEventListener("change", changed);
    },
    () => window.matchMedia(NARROW).matches,
  );
}

export function KbMenuList({
  kbs,
  currentId,
  onPick,
  grouped = true,
}: {
  kbs: Kb[];
  currentId: string | undefined;
  onPick: (id: string) => void;
  /** When searching, pass false: flatten by full name, so `re-cinq/lo` is still found */
  grouped?: boolean;
}) {
  if (!grouped)
    return kbs.map((k) => (
      <KbRow key={k.id} label={k.name} current={k.id === currentId} onClick={() => onPick(k.id)} />
    ));
  return kbGroups(kbs).map((entry) =>
    entry.kind === "kb" ? (
      <KbRow
        key={entry.kb.id}
        label={entry.kb.name}
        current={entry.kb.id === currentId}
        onClick={() => onPick(entry.kb.id)}
      />
    ) : (
      <KbGroupRow
        key={`group-${entry.name}`}
        name={entry.name}
        items={entry.items}
        currentId={currentId}
        onPick={onPick}
      />
    ),
  );
}

function KbRow({
  label,
  current,
  onClick,
  indent,
}: {
  label: string;
  current: boolean;
  onClick: () => void;
  indent?: number;
}) {
  return (
    /* The tick goes in the `trailing` slot. **An svg stuffed into children drops onto a second line**:
       Row wraps the whole of children in a `flex-1 truncate` span,
       and that span is not a flex container, while preflight sets svg to block level,
       so the tick takes up a line of its own and the whole row grows taller */
    <Row
      density="menu"
      className={ROW}
      active={current}
      indent={indent}
      trailing={current ? <Check size={13} className="text-ink-2" /> : undefined}
      onClick={onClick}
    >
      <span className="truncate">{label}</span>
    </Row>
  );
}

function KbGroupRow({
  name,
  items,
  currentId,
  onPick,
}: {
  name: string;
  items: KbGroupItem[];
  currentId: string | undefined;
  onPick: (id: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const narrow = useNarrow();
  const closing = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const anchor = useRef<HTMLDivElement>(null);
  const submenu = useRef<HTMLDivElement>(null);
  /** Only moves focus into the submenu when opened by keyboard; a pointer hover does not steal it (the search box in the top bar is still being typed in) */
  const focusInside = useRef(false);
  const show = (withFocus: boolean) => {
    clearTimeout(closing.current);
    focusInside.current = withFocus;
    setOpen(true);
  };
  const leave = () => {
    clearTimeout(closing.current);
    closing.current = setTimeout(() => setOpen(false), CLOSE_DELAY_MS);
  };
  /* Only the mouse opens and closes it on hover. A touch tap first simulates an enter and then a click:
     if click toggled, it would open and close straight away */
  const byMouse = (run: () => void) => (e: { pointerType: string }) => {
    if (e.pointerType === "mouse") run();
  };
  const onRowKey = (e: KeyboardEvent) => {
    if (groupRowKey(e.key) !== "open") return;
    e.preventDefault();
    // The pointer has already opened it: onOpenAutoFocus will not come again, so focus moves in by itself
    if (open) submenu.current?.querySelector("button")?.focus();
    else show(true);
  };
  useEffect(() => {
    if (!narrow || !open || !focusInside.current) return;
    submenu.current?.querySelector("button")?.focus();
    focusInside.current = false;
  }, [narrow, open]);
  const onSubmenuKey = (e: KeyboardEvent<HTMLDivElement>) => {
    const action = submenuKey(e.key);
    if (action === undefined) return;
    e.preventDefault();
    // Do not let the Esc of the ask-box host (attached to document) close the whole menu as well
    e.stopPropagation();
    if (action === "close") {
      setOpen(false);
      anchor.current?.querySelector("button")?.focus();
      return;
    }
    const buttons = [...e.currentTarget.querySelectorAll("button")];
    const at = buttons.indexOf(document.activeElement as HTMLButtonElement);
    buttons[(at + action + buttons.length) % buttons.length]?.focus();
  };
  const holdsCurrent = items.some((i) => i.kb.id === currentId);
  const trailing = (
    <span className="flex items-center gap-2">
      {holdsCurrent && <Check size={13} className="text-ink-2" />}
      <ChevronRight
        size={13}
        className={cn("u-turn text-ink-2", narrow && open && "rotate-90")}
      />
    </span>
  );
  /* On a narrow screen (phone) the menu takes almost the full width, so the submenu fits neither to the left nor to the right: Radix ends up
     pushing it off-screen. Here it is changed to expand in place: the knowledge bases are listed indented one level under the group */
  if (narrow)
    return (
      <>
        <div ref={anchor}>
          <Row
            density="menu"
            className={ROW}
            aria-expanded={open}
            trailing={trailing}
            onClick={() => setOpen((v) => !v)}
            onKeyDown={onRowKey}
          >
            <span className="truncate">{name}</span>
          </Row>
        </div>
        {open &&
          <div ref={submenu} onKeyDown={onSubmenuKey}>
            {items.map((item) => (
              <KbRow
                key={item.kb.id}
                label={item.label}
                indent={1}
                current={item.kb.id === currentId}
                onClick={() => {
                  setOpen(false);
                  onPick(item.kb.id);
                }}
              />
            ))}
          </div>
        }
      </>
    );
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverAnchor asChild>
        <div ref={anchor} onPointerEnter={byMouse(() => show(false))} onPointerLeave={byMouse(leave)}>
          <Row
            density="menu"
            className={ROW}
            aria-haspopup="menu"
            aria-expanded={open}
            trailing={trailing}
            onClick={() => show(false)}
            onKeyDown={onRowKey}
          >
            <span className="truncate">{name}</span>
          </Row>
        </div>
      </PopoverAnchor>
      <PopoverContent
        data-kb-submenu
        side="right"
        align="start"
        sideOffset={2}
        collisionPadding={8}
        onOpenAutoFocus={(e) => {
          if (!focusInside.current) e.preventDefault();
        }}
        // When focus is in the submenu, Esc is handed to onSubmenuKey (which also returns focus to that row);
        // when focus is still in the search box, it closes as usual
        onEscapeKeyDown={(e) => {
          if (submenu.current?.contains(document.activeElement)) e.preventDefault();
        }}
        // A press on the group row does not count as outside: otherwise press closes it and the click on release reopens it, a flicker
        onInteractOutside={(e) => {
          if (anchor.current?.contains(e.target as Node)) e.preventDefault();
        }}
        onKeyDown={onSubmenuKey}
        onPointerEnter={byMouse(() => show(false))}
        onPointerLeave={byMouse(leave)}
        className="w-max min-w-48 max-w-80 overflow-hidden p-0"
      >
        <div ref={submenu} className="u-scroll max-h-80 overflow-y-auto">
          {items.map((item) => (
            <KbRow
              key={item.kb.id}
              label={item.label}
              current={item.kb.id === currentId}
              onClick={() => {
                setOpen(false);
                onPick(item.kb.id);
              }}
            />
          ))}
        </div>
      </PopoverContent>
    </Popover>
  );
}
