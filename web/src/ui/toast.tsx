/* Global messages (toast): a module-level singleton + <ToastHost/> (mounted once at the app root).
   Any module can show a message with `import { toast } from "./toast"`, no context wiring needed.
   Stacks in the bottom-right; success/info auto-dismiss after 3.8s, error after 6s; can be dismissed manually.
   It can carry one action (such as "Undo"): with an action it stays for 8s, and clicking the action dismisses it. */
import { useEffect, useState } from "react";
import { AlertCircle, CheckCircle2, Info, X } from "lucide-react";

type Kind = "success" | "error" | "info";
type Action = { label: string; onClick: () => void };
type Item = { id: number; kind: Kind; text: string; action?: Action };

let nextId = 1;
let items: Item[] = [];
let listener: ((items: Item[]) => void) | null = null;

function dismiss(id: number) {
  items = items.filter((t) => t.id !== id);
  listener?.(items);
}

function push(kind: Kind, text: string, action?: Action) {
  const id = nextId++;
  items = [...items, { id, kind, text, action }];
  listener?.(items);
  // Keep it up a little longer when there is an action: "Undo" needs time for the person to see it clearly before clicking
  const ttl = kind === "error" ? 6000 : action ? 8000 : 3800;
  window.setTimeout(() => dismiss(id), ttl);
}

export const toast = {
  success: (text: string, action?: Action) => push("success", text, action),
  error: (text: string) => push("error", text),
  info: (text: string, action?: Action) => push("info", text, action),
};

const ICON = { success: CheckCircle2, error: AlertCircle, info: Info } as const;
const ICON_COLOR: Record<Kind, string> = {
  success: "text-ok",
  error: "text-danger",
  info: "text-ink-2",
};

export function ToastHost() {
  const [list, setList] = useState<Item[]>(items);
  useEffect(() => {
    listener = setList;
    return () => {
      if (listener === setList) listener = null;
    };
  }, []);
  if (!list.length) return null;
  return (
    <div className="pointer-events-none fixed bottom-5 right-5 z-[100] flex flex-col items-end gap-2">
      {list.map((t) => {
        const Icon = ICON[t.kind];
        return (
          <div
            key={t.id}
            className="u-pop u-toast-in pointer-events-auto flex items-center gap-2.5 rounded-overlay py-2.5 pl-3.5 pr-2.5 text-body text-ink u-lift-strong"
          >
            <Icon size={15} className={`shrink-0 ${ICON_COLOR[t.kind]}`} />
            <span className="max-w-xs">{t.text}</span>
            {t.action && (
              <button
                onClick={() => {
                  t.action?.onClick();
                  dismiss(t.id);
                }}
                className="u-link shrink-0 text-small"
              >
                {t.action.label}
              </button>
            )}
            <button
              onClick={() => dismiss(t.id)}
              className="text-ink-2 hover:text-ink"
            >
              <X size={13} />
            </button>
          </div>
        );
      })}
    </div>
  );
}
