/* Utopia UI component library — pages use only the components here and the styles.css semantic classes; they write no color literals.
   The rules live in web/DESIGN.md, the guard in scripts/style-guard.mjs: five font-size steps, six spacing steps,
   four radius steps, colors recognize only tokens, and states (hover/focus/disabled/motion) are defined only here.
   Dialog / DangerConfirm / Tooltip / Table / Field each live in their own file and are re-exported from here. */
import { forwardRef, useEffect, useRef, useState,
  useId,} from "react";
import type {
  ButtonHTMLAttributes,
  CSSProperties,
  InputHTMLAttributes,
  ReactNode,
  TextareaHTMLAttributes,
} from "react";
import {
  ArrowUpRight,
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  CircleAlert,
  Loader2,
  Search as SearchIcon,
} from "lucide-react";
import { S } from "../i18n";
import { Button as ShadButton, buttonVariants } from "@/components/ui/button";
import { badgeVariants } from "@/components/ui/badge";
import { Checkbox as ShadCheckbox } from "@/components/ui/checkbox";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
} from "@/components/ui/select";
import {
  DropdownMenu as ShadDropdownMenu,
  DropdownMenuContent as ShadDropdownMenuContent,
  DropdownMenuLabel as ShadDropdownMenuLabel,
  DropdownMenuRadioGroup as ShadDropdownMenuRadioGroup,
  DropdownMenuRadioItem as ShadDropdownMenuRadioItem,
  DropdownMenuSeparator as ShadDropdownMenuSeparator,
  DropdownMenuTrigger as ShadDropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Popover as ShadPopover,
  PopoverContent as ShadPopoverContent,
  PopoverTrigger as ShadPopoverTrigger,
} from "@/components/ui/popover";
import {
  Command as ShadCommand,
  CommandEmpty as ShadCommandEmpty,
  CommandGroup as ShadCommandGroup,
  CommandInput as ShadCommandInput,
  CommandItem as ShadCommandItem,
  CommandList as ShadCommandList,
} from "@/components/ui/command";
import {
  RadioGroup as ShadRadioGroup,
  RadioGroupItem as ShadRadioGroupItem,
} from "@/components/ui/radio-group";
import { Input as ShadInput } from "@/components/ui/input";
import { Textarea as ShadTextarea } from "@/components/ui/textarea";
// The original table lives in ./table and is re-exported below; `SkeletonTableRows` is also used here by itself,
// so a separate alias is taken here, to avoid colliding with the same-named identifier that is re-exported
import {
  Table as SkelTable,
  TBody as SkelTBody,
  Td as SkelTd,
  Tr as SkelTr,
} from "./table";

/** The shared base for the in-app left column: width + glass surface (each page adds flex/padding on top).
    Based on the widest, Ontology (w-64): the rail holds names, so one step wider truncates less. */
export const RAIL_CLS = "w-64 shrink-0 glass-strong border-y-0 border-l-0";

/** Brand wordmark: Marcellus serif, fading in letter by letter from left to right; hover floats up ↗, click goes to the website.
    The arrow and offsets all use em, scaling with the font size where they are used (shared by the login headline and the top bar). */
export function Wordmark({ className }: { className?: string }) {
  return (
    <a
      href={S.app.siteUrl}
      target="_blank"
      rel="noreferrer"
      title="utopia.bi"
      className={cn("relative inline-flex text-ink", className)}
      style={{ fontFamily: "var(--font-brand)", letterSpacing: "0.01em" }}
    >
      {[...S.app.name].map((ch, i) => (
        <span
          key={i}
          className="u-letter"
          style={{ animationDelay: `${80 + i * 65}ms` }}
        >
          {ch}
        </span>
      ))}
      <ArrowUpRight className="u-mark-arrow" aria-hidden />
    </a>
  );
}

export function cn(...parts: (string | false | null | undefined)[]): string {
  return parts.filter(Boolean).join(" ");
}

/* ---------- Button ----------
   **The shell lives here; the look comes from shadcn** (`@/components/ui/button`, radix base / nova preset).
   The reason this layer is kept is only one: the whole app tunes buttons by these four names: primary is the one solid button at most per screen,
   secondary is the outlined default secondary action, ghost is for inline and toolbars, danger is the irreversible one.
   the names say **how heavy this action is**, while shadcn says what it looks like; the two are kept apart,
   so changing the skin does not mean revisiting thirty-two pages. */
export type ButtonVariant = "primary" | "secondary" | "ghost" | "danger";
type ButtonProps = Omit<ButtonHTMLAttributes<HTMLButtonElement>, "color"> & {
  variant?: ButtonVariant;
  size?: "sm" | "md";
  /** Icon to the left of the text (lucide). shadcn already sets the svg size and shrink-0 */
  icon?: ReactNode;
  /** Submitting: disabled, and screen readers are told */
  busy?: boolean;
};

const VARIANT = {
  primary: "default",
  // Our secondary is **outlined**; shadcn's secondary is a solid gray, which corresponds to outline
  secondary: "outline",
  ghost: "ghost",
  danger: "destructive",
} as const;

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  {
    variant = "secondary",
    size = "md",
    icon,
    busy,
    className,
    disabled,
    children,
    type = "button",
    ...props
  },
  ref,
) {
  return (
    <ShadButton
      ref={ref}
      type={type}
      variant={VARIANT[variant]}
      size={size === "sm" ? "sm" : "default"}
      className={className}
      disabled={disabled || busy}
      aria-busy={busy || undefined}
      {...props}
    >
      {icon}
      {children}
    </ShadButton>
  );
});

/* Icon-only button: square; `label` is both the aria-label and the title —
   a button with no visible text must have a name; this is the accessibility baseline */
export const IconButton = forwardRef<
  HTMLButtonElement,
  Omit<ButtonHTMLAttributes<HTMLButtonElement>, "color"> & {
    label: string;
    variant?: ButtonVariant;
    size?: "sm" | "md";
  }
>(function IconButton(
  { label, variant = "ghost", size = "md", className, type = "button", ...props },
  ref,
) {
  return (
    <ShadButton
      ref={ref}
      type={type}
      aria-label={label}
      title={label}
      variant={VARIANT[variant]}
      size={size === "sm" ? "icon-sm" : "icon"}
      className={className}
      {...props}
    />
  );
});

/** A **link** that looks like a button (router Link, external link). Used for className, not wrapped in a component:
 *  `<Link className={buttonLike("ghost")}>`; previously this was written as `u-btn u-btn-ghost`
 *  hand-tuned padding added on top; that padding is exactly why the size tiers exist */
export function buttonLike(
  variant: ButtonVariant = "ghost",
  size: "sm" | "md" = "md",
): string {
  return buttonVariants({
    variant: VARIANT[variant],
    size: size === "sm" ? "sm" : "default",
  });
}

/* ---------- Input / Textarea ----------
   Same as Button: the shell is here, the skin is in shadcn. This layer keeps two things that shadcn does not handle:
   **icon slot** (the magnifier filter box in the left column; its icon must line up with the icons of every row below, on the same
   vertical line) and **bare** (an input placed inside another surface, with no skin of its own: the find box at the top of the switcher,
   the chat input area). Sizes still follow our two tiers, mapped onto shadcn's heights. */
type InputSize = { size?: "sm" | "md" };

/** The tier with no skin of its own: no border, no background, no focus ring; left to the surface around it */
const BARE =
  "border-0 bg-transparent shadow-none focus-visible:ring-0 focus-visible:border-0 px-0 dark:bg-transparent";

/** The skin of a "box": border, radius, solid background, focus ring, exactly the same as `Input`.
 *  Controls that draw their own trigger, such as dropdowns and search selectors, use it — previously they borrowed a hand-written
 *  `.input-dark`, that class was removed along with the button/input migration, so the triggers lost their skin overnight
 *  ("why is this box without a background?"). There is only one skin; whoever needs it imports it. */
export const FIELD_SHELL =
  "flex items-center rounded-control border border-input bg-background text-body text-ink transition-colors outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:opacity-50";

/** The tier with a skin **must have a solid background**. shadcn's default is `bg-transparent`: inside a card that is right,
 *  but this interface's inputs also float over the graph canvas (entity search, filters), where the grid and lines show through a transparent box,
 *  and the text sits on the lines and looks like a mess. Use the page background: on a card it is a bit darker than the card,
 *  and on the canvas it is a solid patch; both places read as "a cell you can type in". */
const FILLED = "bg-background dark:bg-background";

export const Input = forwardRef<
  HTMLInputElement,
  Omit<InputHTMLAttributes<HTMLInputElement>, "size"> &
    InputSize & {
      /** The semantic icon on the left (the filter box's magnifier). When given, className lands on the outer container */
      icon?: ReactNode;
      /** No skin of its own: placed inside another surface (the find box at the top of the switcher panel) */
      bare?: boolean;
    }
>(function Input({ className, size = "md", icon, bare, ...props }, ref) {
  const control = (
    <ShadInput
      ref={ref}
      className={cn(
        /* bare input box **does not even want the control height**. `bare` says "it is a part of the panel,
           not a control placed in the panel", but it still carries h-8 + py-1: a 32px-tall box holding a line of
           20            20px text, and the remaining 12px is not split in half, so the text sits a bit lower and the space above looks
           bigger than below. Height goes back to the line height, and vertical spacing goes back to the surrounding surface (alert panel, library switcher
           all use py-3), so top and bottom really match.
           textarea that tier is left alone: its height is already given by rows, not a control tier. */
        bare ? cn(BARE, "h-auto py-0") : cn(FILLED, size === "sm" ? "h-7" : "h-8"),
        // Icon slot: a medium icon sits 12px from the left inner edge, text starts at 34px; the small size is one step narrower (8 / 28)
        icon ? (size === "sm" ? "pl-7" : "pl-[34px]") : null,
        icon ? "w-full" : className,
      )}
      {...props}
    />
  );
  if (!icon) return control;
  return (
    /* **The outer layer must have the same corner radius as the control: with an icon, `className` lands on this layer (width,
       shadows are written at the call site), and the shadow is drawn to this box's shape: a box without rounded corners,
       gives a square shadow, and a rounded input inside floats on a square shadow, its four corners looking blurry */
    <div className={cn("relative rounded-control", className)}>
      {/* The icon is 12 from the box's left edge, the same number as the nav row's padding, so the magnifier in the left column's input
          lines up on the same vertical line as the icons of every row below (box 8 / icon 20 / text 42) */}
      <span
        className={cn(
          "pointer-events-none absolute top-1/2 -translate-y-1/2 text-ink-2",
          size === "sm" ? "left-2" : "left-3",
        )}
      >
        {icon}
      </span>
      {control}
    </div>
  );
});

export const Textarea = forwardRef<
  HTMLTextAreaElement,
  TextareaHTMLAttributes<HTMLTextAreaElement> &
    InputSize & {
      /** No skin of its own: placed in another surface (like the chat input) */
      bare?: boolean;
    }
>(function Textarea({ className, size = "md", bare, ...props }, ref) {
  return (
    <ShadTextarea
      ref={ref}
      className={cn(
        "u-scroll",
        bare ? BARE : cn(FILLED, size === "sm" ? "min-h-14" : null),
        className,
      )}
      {...props}
    />
  );
});

/* Native select: the popup cannot be themed, so it is used only where there are "two or three options, not worth a Dropdown" */
/* `NativeSelect` was removed: none are left on any page. A native select's popup is drawn by the OS,
   cannot be themed: two kinds of dropdown on one page, one our surface and one the system's gray box. Keeping one
   unused component would only let it sneak back someday. Small bounded enumerations use `Dropdown`,
   hundreds or thousands of items (ontology classes, people in a deployment) use `SearchSelect`. */

/* ---------- Dropdown (small, bounded enumerations) ----------
   **The shell is here; the popup is shadcn's DropdownMenu**. Previously this and SearchSelect each
   implemented "close on outside click, close on Esc, arrow keys to choose, focus returns to the trigger": three popups, three sets of
   behavior, each missing something: the hand-rolled entry did not even have `aria-haspopup`.
   The interface is unchanged by a single letter (value / options / onChange / icon / menuLabel / footer),
   and the pages are as before. */
export interface DropdownOption {
  value: string;
  label: ReactNode;
}

export function Dropdown({
  value,
  options,
  onChange,
  placeholder,
  className,
  size = "md",
  icon,
  menuLabel,
  footer,
}: {
  value: string;
  options: DropdownOption[];
  onChange: (v: string) => void;
  placeholder?: string;
  className?: string;
  size?: "sm" | "md";
  /** The semantic icon on the left of the trigger (says "what this level is") */
  icon?: ReactNode;
  /** The small heading at the top of the popup (also the trigger's title hint) */
  menuLabel?: string;
  /** Fixed action area at the bottom of the popup (the popup closes after a click) */
  footer?: ReactNode;
}) {
  const current = options.find((o) => o.value === value);
  return (
    <ShadDropdownMenu>
      <ShadDropdownMenuTrigger asChild>
        <button
          type="button"
          title={menuLabel}
          className={cn(
            FIELD_SHELL,
            /* **If the caller gives a width, do not add `w-full`**: the `cn` here only joins the classes,
               does no Tailwind conflict resolution: when two `w-` classes are both present, which wins depends on the generated order,
               not the order written: `className="w-40"` once had no effect at all */
            className?.includes("w-") ? null : "w-full",
            "justify-between gap-2 text-left",
            size === "sm" ? "h-7 px-2.5 text-small" : "h-8 px-3 text-body",
            className,
          )}
        >
          <span className="flex min-w-0 items-center gap-2">
            {icon && <span className="shrink-0 text-ink-2">{icon}</span>}
            <span className={cn("truncate", !current && "text-ink-2")}>
              {current?.label ?? placeholder ?? ""}
            </span>
          </span>
          <ChevronDown size={12} className="shrink-0 text-ink-2" />
        </button>
      </ShadDropdownMenuTrigger>
      <ShadDropdownMenuContent align="start" className="min-w-(--radix-dropdown-menu-trigger-width)">
        {menuLabel && <ShadDropdownMenuLabel>{menuLabel}</ShadDropdownMenuLabel>}
        <ShadDropdownMenuRadioGroup value={value} onValueChange={onChange}>
          {options.map((o) => (
            <ShadDropdownMenuRadioItem key={o.value} value={o.value}>
              {o.label}
            </ShadDropdownMenuRadioItem>
          ))}
        </ShadDropdownMenuRadioGroup>
        {footer && (
          <>
            <ShadDropdownMenuSeparator />
            {footer}
          </>
        )}
      </ShadDropdownMenuContent>
    </ShadDropdownMenu>
  );
}

/* ---------- SearchSelect (searchable selector: only for unbounded object lists — members, parent classes, data sources…)
   The trigger is the same as Dropdown; the popup is shadcn's Popover + Command: search, keyboard up/down,
   Enter to select, and the empty-result row, are all managed by Command. */
export interface SearchSelectOption {
  value: string;
  label: string;
  /** A small line after the name (email, type, path); it also takes part in search */
  hint?: string;
  /** Tree indentation level (the parent-class selector uses it) */
  indent?: number;
  /** Options of the same group are not listed under a group name while searching; they show `short`, while search still shows `label` */
  group?: string;
  short?: string;
}

export function SearchSelect({
  value,
  options,
  onChange,
  placeholder,
  className,
  size = "md",
}: {
  value: string;
  options: SearchSelectOption[];
  onChange: (v: string) => void;
  placeholder?: string;
  className?: string;
  size?: "sm" | "md";
  /** Affects only the popup height; kept so that call sites do not change */
  maxVisible?: number;
}) {
  const [open, setOpen] = useState(false);
  const current = options.find((o) => o.value === value);
  return (
    <ShadPopover open={open} onOpenChange={setOpen}>
      <ShadPopoverTrigger asChild>
        <button
          type="button"
          role="combobox"
          aria-expanded={open}
          className={cn(
            FIELD_SHELL,
            /* **If the caller gives a width, do not add `w-full`**: the `cn` here only joins the classes,
               does no Tailwind conflict resolution: when two `w-` classes are both present, which wins depends on the generated order,
               not the order written: `className="w-40"` once had no effect at all */
            className?.includes("w-") ? null : "w-full",
            "justify-between gap-2 text-left",
            size === "sm" ? "h-7 px-2.5 text-small" : "h-8 px-3 text-body",
            className,
          )}
        >
          <span className={cn("truncate", !current && "text-ink-2")}>
            {current?.label ?? placeholder ?? ""}
          </span>
          <ChevronDown size={12} className="shrink-0 text-ink-2" />
        </button>
      </ShadPopoverTrigger>
      <ShadPopoverContent align="start" className="w-(--radix-popover-trigger-width) p-0">
        <ShadCommand>
          <ShadCommandInput placeholder={placeholder ?? ""} />
          <ShadCommandList>
            <ShadCommandEmpty>{S.ui.noMatches}</ShadCommandEmpty>
            <ShadCommandGroup>
              {options.map((o) => (
                <ShadCommandItem
                  key={o.value}
                  value={`${o.label} ${o.hint ?? ""}`}
                  onSelect={() => {
                    onChange(o.value);
                    setOpen(false);
                  }}
                >
                  <span className="truncate">{o.label}</span>
                  {o.hint && (
                    <span className="ml-auto truncate text-fine text-ink-2">{o.hint}</span>
                  )}
                </ShadCommandItem>
              ))}
            </ShadCommandGroup>
          </ShadCommandList>
        </ShadCommand>
      </ShadPopoverContent>
    </ShadPopover>
  );
}

/* ---------- MultiSearchSelect (multi-select version of SearchSelect) ---------- */

/**
 * Multi-select + search. The same vocabulary and keyboard operation as [`SearchSelect`]; three differences:
 *
 * - **Selected items show above the input, each with a remove button. The reason they are not shown in the dropdown is
 *   the dropdown closes and they vanish, while "what exactly have I chosen" is something to check at any time
 * - **Stays open after selecting**: multi-select usually means clicking several in a row, and refocusing every time is torture
 * -  * - Selected items carry a check in the list; clicking again deselects
 *
 * It stays usable when options number in the hundreds (a large ontology is about this size): this is exactly why it replaced the chip wall:
 * the chip wall's height grows linearly with the number of classes, while the search box does not.
 */
export function MultiSearchSelect({
  values,
  options,
  onToggle,
  placeholder,
  emptyHint,
  className,
  maxVisible = 8,
}: {
  values: string[];
  options: SearchSelectOption[];
  onToggle: (v: string) => void;
  placeholder?: string;
  /** The text shown when nothing is selected. Leaving a multi-select empty often means something ("no limit"), not that it was not filled in */
  emptyHint?: string;
  className?: string;
  maxVisible?: number;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  const q = query.trim().toLowerCase();
  const matches = q
    ? options.filter((o) =>
        `${o.label} ${o.hint ?? ""}`.toLowerCase().includes(q),
      )
    : options;
  const visible = matches.slice(0, maxVisible);
  const hidden = matches.length - visible.length;
  const picked = values
    .map((v) => options.find((o) => o.value === v))
    .filter((o): o is SearchSelectOption => !!o);

  const toggle = (v: string) => {
    onToggle(v);
    setQuery("");
    setActive(0);
    inputRef.current?.focus();
  };

  return (
    <div className={cn("relative", className)}>
      {picked.length > 0 && (
        <div className="mb-1 flex flex-wrap gap-1">
          {picked.map((o) => (
            <button
              key={o.value}
              type="button"
              onClick={() => onToggle(o.value)}
              className="group flex items-center gap-1 rounded-cell bg-surface-2 px-2 py-0.5 text-fine text-ink transition-colors duration-fast hover:bg-surface-3"
              title={o.hint ?? o.label}
            >
              {o.label}
              <span className="text-ink-2 group-hover:text-ink">
                ✕
              </span>
            </button>
          ))}
        </div>
      )}
      {picked.length === 0 && emptyHint && (
        <p className="mb-1 text-fine text-ink-2">{emptyHint}</p>
      )}
      {/* The icon is positioned relative to the input only. Previously it was centered on the whole component, and with the input above it
          there is a row of selected items or an empty-state hint, so "half height" landed above the input (#288) */}
      <div className="relative">
        <SearchIcon
          size={11}
          className="absolute left-2.5 top-1/2 -translate-y-1/2 text-ink-2 pointer-events-none"
        />
      <input
          ref={inputRef}
          className={cn(FIELD_SHELL, "w-full pl-7 pr-2.5 py-1 text-small")}
          value={query}
          placeholder={placeholder}
          onFocus={() => {
            setOpen(true);
            setActive(0);
          }}
          onBlur={() => setOpen(false)}
          onChange={(e) => {
            setQuery(e.target.value);
            setActive(0);
          }}
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              setOpen(false);
              inputRef.current?.blur();
            } else if (e.key === "ArrowDown") {
              e.preventDefault();
              setActive((a) => Math.min(a + 1, visible.length - 1));
            } else if (e.key === "ArrowUp") {
              e.preventDefault();
              setActive((a) => Math.max(a - 1, 0));
            } else if (e.key === "Enter" && visible[active]) {
              e.preventDefault();
              toggle(visible[active].value);
            } else if (e.key === "Backspace" && !query && picked.length) {
              // Backspace on empty input removes the last one, consistent with other token inputs
              onToggle(picked[picked.length - 1].value);
            }
          }}
        />
      </div>
      {open && (
        <div className="u-menu-glass u-pop-in u-pop-in-tl absolute z-50 mt-1 w-full rounded-overlay u-lift-strong overflow-hidden">
          {(q
            ? visible.map((option, index) => ({ kind: "option" as const, option, index }))
            : withGroupHeadings(visible)
          ).map((row) => {
            if (row.kind === "heading")
              return (
                <div
                  key={`heading-${row.name}`}
                  className="px-2.5 pt-2 pb-0.5 text-fine font-medium text-ink-2"
                >
                  {row.name}
                </div>
              );
            const { option: o, index: i } = row;
            const indent = q ? 0 : (o.indent ?? 0) + (o.group ? 1 : 0);
            return (
            <button
              key={o.value}
              type="button"
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => toggle(o.value)}
              onMouseEnter={() => setActive(i)}
              className={cn(
                "w-full flex items-center gap-2 text-left px-2.5 py-1 text-small",
                i === active
                  ? "bg-surface-3 text-ink"
                  : "text-ink-2",
              )}
            >
              {indent > 0 && (
                <span className="shrink-0" style={{ width: indent * 14 }} />
              )}
              <span className="min-w-0 flex-1 truncate">
                {!q && o.short ? o.short : o.label}
                {o.hint && (
                  <span className="ml-2 text-ink-2">{o.hint}</span>
                )}
              </span>
              {values.includes(o.value) && (
                <Check size={12} className="shrink-0 text-ink-2" />
              )}
            </button>
            );
          })}
          {visible.length === 0 && (
            <p className="px-2.5 py-1 text-small text-ink-2">
              {S.ui.noMatches}
            </p>
          )}
          {hidden > 0 && (
            <div className="px-2.5 py-1 text-fine text-ink-2 border-t border-line">
              {S.ui.keepTyping(hidden)}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

import { ENTITY_PALETTE } from "../palette";
import { withGroupHeadings } from "./withGroupHeadings";
export { ENTITY_PALETTE, colorForKey } from "../palette";

export function ColorPicker({
  value,
  onChange,
  shape,
}: {
  value: string;
  onChange: (v: string) => void;
  /** When given, the color well renders "shape + color" instead of a full fill (a square has right angles, consistent with graph nodes) */
  shape?: "circle" | "square";
}) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const valid = /^#[0-9a-fA-F]{6}$/.test(value);

  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (!rootRef.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div ref={rootRef} className="relative inline-block">
      {/* Trigger: the current color block (Figma-style color well); with shape it renders shape + color */}
      {shape ? (
        <button
          type="button"
          title={value}
          onClick={() => setOpen(!open)}
          className="h-8 w-10 rounded-control border border-line-strong hover:border-line-strong transition-colors bg-surface grid place-items-center"
        >
          <span
            className={cn("h-3.5 w-3.5", shape === "circle" ? "rounded-full" : "scale-90")}
            style={{ background: valid ? value : ENTITY_PALETTE[0] }}
          />
        </button>
      ) : (
        <button
          type="button"
          title={value}
          onClick={() => setOpen(!open)}
          className="h-8 w-14 rounded-control border border-line-strong hover:border-line-strong transition-colors"
          style={{ background: valid ? value : ENTITY_PALETTE[0] }}
        />
      )}
      {open && (
        // Explicit width: the shrink width of absolute positioning would be pinned by the container block of the inline-block trigger
        <div className="u-menu-glass u-pop-in u-pop-in-tl absolute z-50 left-0 top-full mt-2 w-56 rounded-overlay p-3 u-lift-strong">
          <div className="grid grid-cols-8 gap-1.5 mb-2.5">
            {ENTITY_PALETTE.map((c) => (
              <button
                key={c}
                type="button"
                title={c}
                onClick={() => {
                  onChange(c);
                  setOpen(false);
                }}
                className={cn(
                  "h-5 w-5 rounded-full transition-transform hover:scale-110",
                  value.toLowerCase() === c &&
                    "outline outline-2 outline-ring outline-offset-1",
                )}
                style={{ background: c }}
              />
            ))}
          </div>
          <input
            value={value}
            onChange={(e) => onChange(e.target.value)}
            placeholder={ENTITY_PALETTE[0]}
            className={cn(
              FIELD_SHELL,
              "w-full px-2 py-1 text-small font-mono",
              !valid && "!border-danger",
            )}
          />
        </div>
      )}
    </div>
  );
}

/* ---------- Pager (list pagination bar: automatically hidden when there is less than one page) ---------- */
export function Pager({
  total,
  pageSize,
  page,
  onPage,
  always,
  /** Overrides the default top margin. The default `mt-3` suits following a list;
      pass `""` to remove it when placed in a footer bar that already has padding */
  className = "mt-3",
}: {
  total: number;
  pageSize: number;
  page: number;
  onPage: (p: number) => void;
  /** Shown even with only one page. This is for the **fixed footer**: that bar is always there,
      and if the pager is hidden, it becomes an empty edge with no content */
  always?: boolean;
  className?: string;
}) {
  const pageCount = Math.max(1, Math.ceil(total / pageSize));
  const safe = Math.min(page, pageCount - 1);
  if (total <= pageSize && !always) return null;
  return (
    <div className={cn("flex items-center justify-end gap-2 text-small text-ink-2", className)}>
      <span className="u-num">
        {S.library.pageOf(
          safe * pageSize + 1,
          Math.min((safe + 1) * pageSize, total),
          total,
        )}
      </span>
      <button
        onClick={() => onPage(safe - 1)}
        disabled={safe === 0}
        className="u-btn u-btn-ghost h-7 w-7 grid place-items-center"
      >
        <ChevronLeft size={13} />
      </button>
      <button
        onClick={() => onPage(safe + 1)}
        disabled={safe >= pageCount - 1}
        className="u-btn u-btn-ghost h-7 w-7 grid place-items-center"
      >
        <ChevronRight size={13} />
      </button>
    </div>
  );
}

/** Pagination slice helper: returns the current page's data and a safe page number. */
export function pageSlice<T>(
  items: T[],
  page: number,
  pageSize: number,
): { rows: T[]; safe: number } {
  const pageCount = Math.max(1, Math.ceil(items.length / pageSize));
  const safe = Math.min(page, pageCount - 1);
  return { rows: items.slice(safe * pageSize, (safe + 1) * pageSize), safe };
}

/* ---------- Panel (glass panel) ---------- */
export function Panel({
  strong = false,
  className,
  children,
}: {
  strong?: boolean;
  className?: string;
  children: ReactNode;
}) {
  return (
    <div
      className={cn(strong ? "glass-strong" : "glass", "rounded-panel", className)}
    >
      {children}
    </div>
  );
}

/* ---------- SettingsCard (one save unit) ----------
   A card in the settings page: title, one line of explanation, fields, and a horizontal bar below: on the left the constraint or cost,
   on the right this card's own save. **The border marks what this button covers**: rename, press save,
   and the four toggles below should not be sent along (DESIGN.md 6).
   Save uses secondary: at most one primary per screen (rule 5), yet every card on the settings page has one. */
export function SettingsCard({
  title,
  /** A line under the title: what this setting is */
  hint,
  /** A line on the left of the footer: constraint, cost, when it takes effect */
  note,
  /** On the right of the footer: usually this card's save button */
  action,
  className,
  children,
}: {
  title: ReactNode;
  hint?: ReactNode;
  note?: ReactNode;
  action?: ReactNode;
  className?: string;
  children?: ReactNode;
}) {
  return (
    <section className={cn("glass overflow-hidden rounded-panel", className)}>
      <div className="p-6">
        <h2 className="text-title text-ink">{title}</h2>
        {hint && <p className="mt-1 text-small leading-relaxed text-ink-2">{hint}</p>}
        {children && <div className="mt-4">{children}</div>}
      </div>
      {(note || action) && (
        /* The footer is separated from the above only by a line, with no extra surface layer: the three surface states are rest / hover / selected,
           using the hover state as the resting background would make this bar look like it is always being pointed at */
        <div className="flex items-center justify-between gap-4 border-t border-line px-6 py-3">
          <div className="min-w-0 flex-1 text-small leading-relaxed text-ink-2">{note}</div>
          <div className="flex shrink-0 items-center gap-3">{action}</div>
        </div>
      )}
    </section>
  );
}

/* ---------- StatusCell (a status cell in a table) ----------
   **Success is plain text; color appears only when something goes wrong.** When every row in a column carries a colored pill,
   color no longer indicates anything: twelve "Ready" and one "Failed" look equally heavy,
   and the eye has to read row by row to find the bad one. So the status word is a sentence in the secondary color,
   with a red exclamation mark in front of failures: **the red is the symbol, not the whole sentence**,
   so scanning down a column only a few red dots stand out.
   Given an onClick, it is clickable (opens the original error text). */
export function StatusCell({
  danger,
  onClick,
  title,
  children,
}: {
  danger?: boolean;
  onClick?: () => void;
  title?: string;
  children: ReactNode;
}) {
  const body = (
    <>
      {danger && <CircleAlert size={12} className="shrink-0 text-danger" />}
      {children}
    </>
  );
  const cls = "inline-flex items-center gap-1.5 text-small text-ink-2";
  if (!onClick) {
    return (
      <span className={cls} title={title}>
        {body}
      </span>
    );
  }
  return (
    <button
      type="button"
      title={title}
      onClick={onClick}
      className={cn(cls, "u-linkbtn")}
    >
      {body}
    </button>
  );
}

/* ---------- Status (one status) ----------
   **Color goes only on the dot; the text is not colored.** Previously a status was a filled pill (green background with green text
   "Confirmed", amber background with amber text for "Pending"): a dozen filled blocks in one table column,
   the eye first sees a field of color, not what that column is saying. A dot takes only the small space it should take,
   the colors still tell apart done from not done, and waiting on someone or not, while the text returns to the body gray.

   `Chip` is kept for things that are **not statuses**: counts, library names, axiom names, markers like "derived" —
   they are labels stuck on content and should have a box to frame themselves. */
export function Status({
  tone = "neutral",
  pulse,
  className,
  title,
  children,
}: {
  tone?: ChipTone;
  /** This thing is **in progress**: the dot breathes. Used for live states like "the agent is adjudicating this pair" */
  pulse?: boolean;
  className?: string;
  title?: string;
  children: ReactNode;
}) {
  return (
    <span
      className={cn("inline-flex items-center gap-1.5 text-small text-ink-2", className)}
      title={title}
    >
      <span
        className={cn(
          "h-1.5 w-1.5 shrink-0 rounded-full",
          STATUS_DOT[tone],
          pulse && "animate-pulse",
        )}
      />
      {children}
    </span>
  );
}

const STATUS_DOT: Record<ChipTone, string> = {
  neutral: "bg-ink-2",
  success: "bg-ok",
  warn: "bg-warn",
  danger: "bg-danger",
  info: "bg-violet",
  violet: "bg-violet",
  contest: "bg-contest",
};

/* ---------- Chip (status pill) ---------- */
export type ChipTone =
  "neutral" | "info" | "success" | "warn" | "danger" | "violet" | "contest";

export function Chip({
  tone = "neutral",
  className,
  title,
  onClick,
  children,
}: {
  tone?: ChipTone;
  className?: string;
  title?: string;
  /** Given, it is a button (like the one that opens the original failure text) */
  onClick?: () => void;
  children: ReactNode;
}) {
  /* The shape follows shadcn's Badge, **the color is still our semantic color**: Badge only has
     default / secondary / destructive tiers, while in this vocabulary a chip says
     a status — ready, 3 dropped, contested, derived each have their own color; they are tokens,
     not variants. So the base uses outline's skeleton, and the color is applied by tone. */
  const cls = cn(
    badgeVariants({ variant: "outline" }),
    /* **Square corners, not a pill**: shadcn's badge base is `rounded-4xl`, and a single-digit count
       shrinks to a dot. In this vocabulary a chip says a status (Ready, 3 dropped,
       contested), a label plate, not a bean, so it uses the cell tier (rule 3).
       the geometry (height 20, padding 8, font size xs) still comes from shadcn */
    "rounded-cell border-transparent",
    CHIP_TONE[tone],
    onClick && "cursor-pointer",
    className,
  );
  if (onClick) {
    return (
      <button type="button" onClick={onClick} title={title} className={cls}>
        {children}
      </button>
    );
  }
  return (
    <span className={cls} title={title}>
      {children}
    </span>
  );
}

/** chip look, for places that cannot be a component: the inline span with `role="link"`
 *  (the outside is already a clickable row, and a button cannot be nested inside a button) */
export function chipLike(tone: ChipTone = "neutral", className?: string): string {
  return cn(
    badgeVariants({ variant: "outline" }),
    "rounded-cell border-transparent",
    CHIP_TONE[tone],
    className,
  );
}

const CHIP_TONE: Record<ChipTone, string> = {
  neutral: "bg-surface-2 text-ink-2",
  success: "bg-ok/12 text-ok",
  warn: "bg-warn/12 text-warn",
  danger: "bg-danger/12 text-danger",
  info: "bg-violet/12 text-violet",
  contest: "bg-contest/12 text-contest",
  violet: "bg-violet/12 text-violet",
};

/* ---------- LinkButton (an action that looks like a sentence: "re-extract" and "delete" at the end of a table row) ---------- */
export function LinkButton({
  tone = "default",
  underline,
  className,
  type = "button",
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & {
  tone?: "default" | "danger";
  /** With an underline (like u-link) */
  underline?: boolean;
}) {
  return (
    <button
      type={type}
      className={cn(
        "u-linkbtn",
        tone === "danger" && "is-danger",
        underline && "u-link",
        className,
      )}
      {...props}
    />
  );
}

/* ---------- ChoiceCard (selectable cards with the checkbox hidden: the several side-by-side cards like the ontology packs) ---------- */
export function ChoiceCard({
  checked,
  onChange,
  label,
  className,
  children,
}: {
  checked: boolean;
  onChange: () => void;
  /** The name screen readers read out */
  label: string;
  className?: string;
  children: ReactNode;
}) {
  return (
    <label className={cn("u-choice", checked && "is-on", className)}>
      <input
        type="checkbox"
        className="sr-only"
        aria-label={label}
        checked={checked}
        onChange={onChange}
      />
      {children}
    </label>
  );
}

/* ---------- PageTitle ---------- */
export function PageTitle({
  className,
  children,
}: {
  className?: string;
  children: ReactNode;
}) {
  return <h2 className={cn("u-title text-title", className)}>{children}</h2>;
}

/** The shape of the row of actions under a card. **There is only one**: the review page has seven kinds of card (duplicate pairs, facts awaiting confirmation,
 *  temporal conflicts, stale assertions, ontology defects, axiom conflicts, facts a person raised); previously they were placed three different ways:
 *  three at the bottom right, three at the bottom left, and two hung at the right end of a row. A person moving from one card to
 *  the next had to find "where is the decision" again every time.
 *
 *  Two rules: **actions sit at the bottom left of the card**, on the same left edge as each line of text in the card, so down a column of cards
 *  the buttons fall on one vertical line and the pointer barely moves sideways; **they wrap**, because some cards have five or six
 *  options, and right-aligned lists would go ragged at the left edge once they wrap.
 *
 *  There is one more rule, not in this class but just as firm: **the dangerous one always goes last**. After left alignment,
 *  the far left is where the pointer reaches first, and it should not be "reject". */
export const CARD_ACTIONS = "mt-3 flex flex-wrap items-center gap-2";

/* ---------- EmptyState ---------- */
export function EmptyState({
  icon,
  children,
}: {
  icon: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="text-center">
      <div className="glass mx-auto mb-4 h-14 w-14 rounded-panel grid place-items-center text-title font-bold text-ink-2">
        {icon}
      </div>
      <div className="text-body text-ink-2 whitespace-pre-line">
        {children}
      </div>
    </div>
  );
}

/* ---------- Loading / Skeleton / Spinner / ErrorText ---------- */
export function Loading({ children }: { children: ReactNode }) {
  return <div className="p-8 text-body text-ink-2">{children}</div>;
}

/** A skeleton bar. **Use it for things whose shape is known**: a title row, a color dot, a column of tree.
 *
 * The caller gives the size (height from the font-size tiers, width via `style`; the bar's length is set to follow
 * the real content's length distribution, not arbitrarily fixed, since equal widths in a column look like a progress bar rather than a list). The corner radius is fixed to cell:
 * it stands in for one cell of content, not a surface. */
export function Skeleton({
  className,
  style,
}: {
  className?: string;
  style?: CSSProperties;
}) {
  return (
    <div className={cn("u-skel rounded-cell", className)} style={style} aria-hidden />
  );
}

/** Rows of a column that have not arrived yet. The left column, lists: any place that will be rows one by one uses it.
 *
 * Each row is just one full-width gray bar: **no icon slot, no right end, no simulated indentation**. Those are all things
 * not yet known, and drawing them would be making things up: an extra gray square, and readers would take it for a checkbox or color dot;
 * faking one level of indentation, and if the tree that arrives has a different shape, that jolt is worse than no indentation. What a skeleton can honestly say
 * is only two things: **here it will be rows one by one, and how tall each row is**.
 *
 * The row itself is the real `Row`, so row height, padding and corner radius match the real list pixel for pixel, and the content
 * does not jump a row when it lands. `density` must match the tier the host list gives.
 *
 * (It sits here rather than after `Row` so that the three waiting primitives sit together; function declarations are hoisted,
 * so referencing `Row` later is fine.) */
export function SkeletonRows({
  rows = 8,
  density = "list",
  className,
}: {
  /** How many rows to draw. **Enough to show "this will be a column here"**; no need to fill the space: the spinner on the right already
   *  says it is loading; a column of gray bars running all the way down is just a striped wall. */
  rows?: number;
  density?: RowDensity;
  className?: string;
}) {
  return (
    <div className={cn("u-rail-list", className)} aria-hidden>
      {Array.from({ length: rows }, (_, i) => (
        /* The gray bar **is directly the row's content**, with no extra shell that "fills the row height". Wrapped like that, a 30px row
           has only 10px of bar and 10px empty above and below, so a column looks much looser than the real list. Now a row
           is 4 + 16 + 4: the bar is as thick as a line of text, and the line spacing is back to what it should be */
        <Row key={i} density={density} flush disabled>
          <Skeleton className="h-4 w-full" />
        </Row>
      ))}
    </div>
  );
}

/** The waiting mark in the middle of the canvas. **It sits over what has already been drawn; it does not replace it**: the grid, the zoom tower,
 *  and the static legends are all independent of data and drawn first; this layer only says the middle part is still on its way.
 *
 * One step bigger than the inline spinner (28): it must be seen at a glance in the exact middle of a whole screen of canvas, not squeezed
 * next to a line of text. `pointer-events-none` lets the controls underneath still be clicked: zooming and
 * resetting the view are still possible while waiting.
 *
 * The host must have `relative` (both canvases are `h-full relative`). */
export function CanvasLoading({ size = 28 }: { size?: number }) {
  return (
    <div className="absolute inset-0 grid place-items-center pointer-events-none">
      <Spinner size={size} label={S.nav.loading} />
    </div>
  );
}

/** The table version of a skeleton row.
 *
 * Same reasoning as `SkeletonRows`, only the row's shape is a table's: it is built from **the real `Tr` / `Td`**,
 * so the row height is guaranteed by construction (8 + 22 + 8 = 38), not by a hard-coded number.
 *
 * This one was learned the hard way: the table skeleton once used the left column's row tier (24px), which is
 * a third shorter than real table rows; a column of them looked short and dense, and it was obvious at a glance that it was not what that table would look like.
 * A skeleton says "what shape is being waited for"; if the shape is wrong, it is better to say nothing. */
export function SkeletonTableRows({ rows = 8 }: { rows?: number }) {
  return (
    <SkelTable>
      <SkelTBody>
        {Array.from({ length: rows }, (_, i) => (
          <SkelTr key={i}>
            <SkelTd>
              {/* This layer holds one row height (see .u-skel-line in styles.css):
                  a real cell holds a line of text; without it the whole table is shorter */}
              <span className="u-skel-line">
                <Skeleton className="h-4 w-full" />
              </span>
            </SkelTd>
          </SkelTr>
        ))}
      </SkelTBody>
    </SkelTable>
  );
}

/** A spinner. **Use it for things whose shape is unknown**: a whole region that does not yet know whether it will draw a chart, a table or be empty.
 *  Do not use it where the shape is known: there `Skeleton` can say one more thing about "what is being waited for". */
export function Spinner({
  size = 16,
  label,
  className,
}: {
  size?: number;
  /** For screen readers; if not given, the whole thing is hidden as decoration */
  label?: string;
  className?: string;
}) {
  return (
    <Loader2
      size={size}
      className={cn("animate-spin text-ink-2", className)}
      role={label ? "status" : undefined}
      aria-label={label}
      aria-hidden={label ? undefined : true}
    />
  );
}

export function ErrorText({ children }: { children: ReactNode }) {
  return <p className="text-body text-danger">{children}</p>;
}

/* ---------- GithubMark (lucide has no brand icons, so the official mark is inlined) ---------- */
export function GithubMark({ size = 16 }: { size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="currentColor"
      aria-hidden
    >
      <path d="M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12" />
    </svg>
  );
}

/* ---------- PageHeader (page-level title for the right content area) ----------
   display font size (rule 1: page titles, and only page titles) + one line of subtitle + actions at the right end.
   Every page takes its title from here, so font size, subtitle color and distance to the body are not each written their own way.
   className when given, replaces the default mb-6 (where the outer layer already spaces with space-y, pass mb-2 or the like). */
export function PageHeader({
  title,
  sub,
  actions,
  className,
}: {
  title: ReactNode;
  sub?: ReactNode;
  actions?: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn(className ?? "mb-6")}>
      {/* Actions follow **the title row**, not the whole "title + subtitle" block: if the whole block were centered,
          with a subtitle the button would float between two lines, looking as if it belonged to neither title nor explanation.
          items-baseline: the text on the button sits on the same baseline as the title */}
      <div className="flex items-baseline justify-between gap-4">
        <h1 className="u-title min-w-0 text-display break-words">{title}</h1>
        {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
      </div>
      {sub && <p className="mt-1 text-body text-ink-2">{sub}</p>}
    </div>
  );
}

/* ---------- SectionMark (section wordmark: Docs / account layer etc., letters enter one by one, clicking returns to the app) ---------- */
import { Link as RouterLink } from "@tanstack/react-router";
export function SectionMark({
  text,
  title,
  className,
}: {
  text: string;
  title: string;
  className?: string;
}) {
  return (
    <RouterLink
      to="/"
      title={title}
      className={cn("u-wordmark-top relative inline-flex text-ink", className)}
      style={{ fontFamily: "var(--font-brand)", letterSpacing: "0.01em" }}
    >
      {[...text].map((ch, i) => (
        <span
          key={i}
          className="u-letter"
          style={{ animationDelay: `${80 + i * 45}ms` }}
        >
          {/* inline-flex collapses spans of pure spaces — use a space that does not collapse */}
          {ch === " " ? " " : ch}
        </span>
      ))}
    </RouterLink>
  );
}

/* Instants are shown in the viewer's timezone. **Only for instants**: values like recorded_at, created_at, upload time,
   "when it happened" values. Calendar dates written in a document ("May 2019") have no timezone, so they take an ISO slice,
   converting to local would make a UTC-5 reader see the previous day. The same test in EntityHistory applies */
export function localDate(iso: string): string {
  const d = new Date(iso);
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${d.getFullYear()}-${m}-${day}`;
}
export function localDateTime(iso: string): string {
  const d = new Date(iso);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  return `${localDate(iso)} ${hh}:${mm}`;
}

/* ---------- Row (a clickable row: left-column nav items, class tree, relation/attribute lists) ----------
   The whole row is clickable; the pointer hovering changes the surface, and selection inverts. Rows in lists and left-column nav items are the same thing,
   differing only in density: nav is a bit taller and has an icon; list is shorter and can be indented. */
/** The class of a row: Row uses it itself; rows on a page that must be a <Link> (instance rows that jump to the graph) also use it */
export type RowDensity = "nav" | "list" | "menu";
/** The semantic color of a row: danger = a row that deletes things; warn = an entry leading to the danger zone — it only leads there,
 *  nothing has been done yet, so it uses the warning color rather than the danger color */
export type RowTone = "danger" | "warn";
export function rowClass(
  active?: boolean,
  density: RowDensity = "list",
  tone?: RowTone,
  /** Secondary: the items under a parent item in the left column. The text shrinks to the position of the parent row's label
      (padding 12 + icon 14 + gap 8 = 34), while the background still fills the whole row —
      it is the text that shrinks, not the cell */
  sub?: boolean,
): string {
  return cn(
    "group flex w-full items-center gap-2 text-left transition-colors duration-fast",
    density === "menu" ? "rounded-none" : "rounded-cell",
    sub && "pl-[34px]",
    // Left-column nav items are 32 high (py 6): 36 looks loose in a column of a dozen items
    density === "nav"
      ? "px-3 py-1.5 text-body font-medium"
      : density === "menu"
        ? "px-3 py-2 text-small"
        : "px-2 py-1 text-body",
    active
      ? "u-nav-active"
      : tone === "danger"
        ? "text-danger hover:bg-surface-2"
        : tone === "warn"
          ? "text-warn hover:bg-surface-2"
          : "text-ink-2 hover:bg-surface-2 hover:text-ink",
  );
}
/** Small text at the row's right end: one step smaller and dimmer; it brightens along when the whole row is pointed at */
export const ROW_TRAILING = "ml-auto shrink-0 text-fine text-ink-2 group-hover:text-ink";
/** A value that follows right after the label: same color as `ROW_TRAILING`, but **not pushed to the right**.
 *  One on the left and one on the right suits two-column reading like "name …… count"; "predicate object" is a sentence, and separating them with one
 *  whole row of blank space would make it unreadable as a sentence */
export const ROW_VALUE =
  "min-w-0 flex-1 truncate text-fine text-ink-2 group-hover:text-ink";

export function Row({
  active,
  danger,
  tone,
  density = "list",
  indent = 0,
  icon,
  flush,
  trailing,
  className,
  children,
  type = "button",
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & {
  active?: boolean;
  /** A dangerous row (delete in a menu); = tone="danger" */
  danger?: boolean;
  tone?: RowTone;
  /** nav = left-column nav item; list = list row; menu = an item in a popup menu (full-width, no rounded corners) */
  density?: RowDensity;
  /** Tree indentation level */
  indent?: number;
  icon?: ReactNode;
  /** Drops the icon cell: when no row in the group has an icon, that cell aligns to nothing and only pushes the title right */
  flush?: boolean;
  /** Things at the right end: counts, small type labels */
  trailing?: ReactNode;
}) {
  return (
    <button
      type={type}
      aria-current={active ? "true" : undefined}
      style={indent ? { paddingLeft: `${8 + indent * 14}px` } : undefined}
      className={cn(rowClass(active, density, danger ? "danger" : tone), className)}
      {...props}
    >
      {/* The icon matches the text color: when selected it turns white, when warning it turns orange, together. Nav items with no icon still reserve
          the icon cell, so text lines up in a column whether or not it has an icon, unless the whole group has none (flush) */}
      {icon ? (
        <span className="shrink-0">{icon}</span>
      ) : density === "nav" && !flush ? (
        <span className="w-3.5 shrink-0" aria-hidden />
      ) : null}
      <span className="min-w-0 flex-1 truncate">{children}</span>
      {trailing && <span className={ROW_TRAILING}>{trailing}</span>}
    </button>
  );
}

/* An entry in the left column: Library's sources, Review's queues, and the ones pinned at the bottom of Ontology
   are all this. **Same look as rows in lists** (Row density="nav", rounded, indented) —
   the separator line on the outer side of the bottom pins shows "these are always present", without making the row itself square.
   A count is shown when given, and 0 is shown too (one gray step dimmer): an empty queue is a meaningful fact. */
/** Where a conversation stands right now, drawn in the icon cell of a left-column row (14 wide, the same cell as icons elsewhere,
 *  so the title still lands on the same vertical line; see the note on rowClass).
 *
 *  Three tiers: `rest` is a 6px thin-line hollow circle — it is itself a marker, saying every row in this column is a conversation;
 *  `live` three dots jumping; `unread` is the same circle filled solid blue: it finished and you have not come back to look.
 *  **The shape never changes**, only the inside and the color do: changing state does not change shape, so it is recognized at a glance as
 *  the same thing. Small enough to act only as a bullet, not competing with the title (earlier 12-square blocks were too heavy) */
export function ConvMark({ state }: { state: "rest" | "live" | "unread" }) {
  return (
    // `flex` not `inline-flex`: Row wraps the icon in a span, and an inline box would sit
    // on the baseline of the line of text, with line height underneath, so the whole mark sits low (the three dots have no text,
    // so the baseline is the bottom edge and the offset is most visible). A block box has no line box, so Row's items-center centers it
    <span
      className="flex h-3.5 w-3.5 shrink-0 items-center justify-center"
      aria-hidden
    >
      {state === "live" ? (
        <span className="u-dots">
          <span />
          <span />
          <span />
        </span>
      ) : (
        <span
          className={cn(
            "h-1.5 w-1.5 rounded-full border",
            state === "unread" ? "border-unread bg-unread" : "border-ink-2",
          )}
        />
      )}
    </span>
  );
}

export function RailItem({
  active,
  icon,
  count,
  dot,
  external,
  className,
  children,
  ...props
}: Omit<ButtonHTMLAttributes<HTMLButtonElement>, "type"> & {
  active?: boolean;
  icon?: ReactNode;
  count?: number;
  /** The status dot on the right (syncing / failed kinds), a class name for the background color */
  dot?: string;
  /** This entry is not handled on this page: add a destination marker, so that clicking does not look like the page did nothing */
  external?: boolean;
}) {
  return (
    <Row
      density="nav"
      active={active}
      icon={icon}
      className={className}
      // Count appears only when there is something, as a chip; 0 is not written, a column of gray zeros is only noise
      trailing={
        count !== undefined && count > 0 ? (
          <span className={chipLike("neutral", "u-num")}>{count}</span>
        ) : undefined
      }
      {...props}
    >
      <span className="flex min-w-0 items-center gap-2">
        <span className="truncate">{children}</span>
        {dot && <span className={cn("h-1.5 w-1.5 shrink-0 rounded-full", dot)} />}
        {external && <ArrowUpRight size={11} className="shrink-0 opacity-50" />}
      </span>
    </Row>
  );
}

/** A row in a menu: "label + editable value". **The whole row is the trigger; the popup lands on the value end**,
 *  this row reads like "Language: English", and what is changed is the cell after the colon, so the popup should appear
 *  on that cell, like a fill-in-the-blank. Spreading it below would make the menu longer and longer; throwing it to the right would cover the panel beside it.
 *  The row rhythm matches the other rows in the menu (px-4 py-2, body size, icon gap 3). */
const MENU_ROW = "gap-3 rounded-none px-4 py-2 text-body";

export function MenuSelect({
  icon,
  label,
  value,
  onChange,
  options,
}: {
  icon: ReactNode;
  label: string;
  value: string;
  onChange: (v: string) => void;
  /** An option's own icon is **optional**: the three theme tiers each have a widely recognized symbol (moon / sun /
   *  a screen), and icons separate the three options at a glance. Languages have no such symbol: a flag is not a language,
   *  since Chinese is spoken in more than one place, that tier is text only, and the check mark says which one is selected. */
  options: { value: string; label: string; icon?: ReactNode }[];
}) {
  return (
    <Select value={value} onValueChange={onChange}>
      <SelectTrigger
        className={cn(
          MENU_ROW,
          // Row appearance belongs to the menu: remove Select's own frame and background, leaving "a row"
          "h-auto w-full border-0 bg-transparent shadow-none focus-visible:ring-0 dark:bg-transparent",
          "justify-start text-ink",
        )}
      >
        <span className="text-ink-2">{icon}</span>
        <span>{label}</span>
        <span className="ml-auto pl-2 text-fine text-ink-2">
          {options.find((o) => o.value === value)?.label ?? value}
        </span>
      </SelectTrigger>
      {/* **popper positioning, not item-aligned.** By default Select lays the "selected item" over
          the trigger (the look of a native select); in this menu the position it computes is
          (0, 900)——the bottom-left corner outside the viewport, so the popup opens but is not visible (`data-state=open`,
          the content is attached too, it is just not on screen). The item-aligned approach measures the trigger, and the trigger
          sits inside a portal layer; popper uses floating-ui, following the trigger frame by frame, and only then honors align. */}
      <SelectContent
        position="popper"
        side="bottom"
        align="end"
        sideOffset={4}
        /* **The popup is sized to content, not to the trigger width.** The trigger is the whole row, while shadcn's viewport in
           popper mode is hard-coded to `min-w-(--radix-select-trigger-width)`, so the popup follows
           fill the whole menu, looking as if the menu grew two rows by itself. It should land on the value cell, like a fill-in-the-blank.
           What is changed is `--radix-popper-anchor-width` rather than `--radix-select-trigger-width`:
           the latter is written by Radix into the content element's **inline style** (`var(--radix-popper-anchor-width)`),
           class a class cannot override inline styles, so it can only change the cell it refers to. */
        className="[--radix-popper-anchor-width:auto]"
      >
        {options.map((o) => (
          <SelectItem key={o.value} value={o.value}>
            {o.icon && <span className="text-ink-2">{o.icon}</span>}
            {o.label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

/* ---------- Segmented (segmented toggle) ----------
   Two or three mutually exclusive views or values. fill = each cell is equal width and fills (the class/attribute toggle in the left column);
   otherwise sized to content (small toggles like connection direction and shape). */
export function Segmented<T extends string>({
  value,
  options,
  onChange,
  size = "md",
  fill,
  disabled,
  className,
}: {
  value: T;
  options: { value: T; label: ReactNode; count?: number; title?: string }[];
  onChange: (v: T) => void;
  size?: "sm" | "md";
  fill?: boolean;
  disabled?: boolean;
  className?: string;
}) {
  return (
    <div
      role="tablist"
      className={cn(
        "flex gap-1 rounded-control bg-surface p-1",
        fill && "w-full",
        disabled && "opacity-40",
        className,
      )}
    >
      {options.map((o) => {
        const active = o.value === value;
        return (
          <button
            key={o.value}
            type="button"
            role="tab"
            aria-selected={active}
            title={o.title}
            disabled={disabled}
            onClick={() => onChange(o.value)}
            className={cn(
              "flex items-center justify-center gap-1 rounded-control font-medium transition-colors duration-fast",
              size === "sm" ? "px-2 py-1 text-fine" : "px-3 py-1 text-small",
              fill && "flex-1",
              active
                ? "bg-surface-3 text-ink"
                : "text-ink-2 hover:bg-surface-2 hover:text-ink",
            )}
          >
            {o.label}
            {o.count !== undefined && o.count > 0 && (
              <span className="u-num">{o.count}</span>
            )}
          </button>
        );
      })}
    </div>
  );
}

/* ---------- Checkbox ---------- */
export function Checkbox({
  label,
  hint,
  className,
  checked,
  onChange,
  disabled,
  id,
}: {
  label: ReactNode;
  hint?: ReactNode;
  className?: string;
  checked?: boolean;
  /** **It takes a boolean, not an event**: underneath it is no longer a native input (Radix's button +
   *  a hidden input), so `target.checked` on the event object has no meaning here */
  onChange?: (checked: boolean) => void;
  disabled?: boolean;
  id?: string;
}) {
  const auto = useId();
  const inputId = id ?? auto;
  return (
    <div className={cn("flex items-start gap-2", className)}>
      <ShadCheckbox
        id={inputId}
        checked={checked}
        disabled={disabled}
        onCheckedChange={(v) => onChange?.(v === true)}
        className="mt-0.5"
      />
      <label htmlFor={inputId} className="min-w-0 cursor-pointer">
        <span className="block text-body text-ink">{label}</span>
        {hint && <span className="block text-fine text-ink-2">{hint}</span>}
      </label>
    </div>
  );
}

/* ---------- Disclosure (collapsible section: a clickable summary row + expanded content) ---------- */
export function Disclosure({
  summary,
  defaultOpen,
  className,
  children,
}: {
  summary: ReactNode;
  defaultOpen?: boolean;
  className?: string;
  children: ReactNode;
}) {
  return (
    <details open={defaultOpen} className={className}>
      <summary className="cursor-pointer select-none text-small text-ink-2 transition-colors duration-fast hover:text-ink">
        {summary}
      </summary>
      <div className="mt-2">{children}</div>
    </details>
  );
}

/* ---------- ToolTower (vertical tool tower on the canvas) ----------
   Icons stay visible; the names expand together when the whole group is hovered or reached by keyboard (u-tower in styles.css).
   A group is one semantic unit: derive / layout / camera. */
export function ToolTower({
  className,
  children,
}: {
  className?: string;
  children: ReactNode;
}) {
  return (
    <div
      className={cn(
        "u-tower group glass-strong flex flex-col overflow-hidden rounded-panel u-lift-strong",
        className,
      )}
    >
      {children}
    </div>
  );
}

export const ToolButton = forwardRef<
  HTMLButtonElement,
  ButtonHTMLAttributes<HTMLButtonElement> & {
    active?: boolean;
    /** The name shown when expanded; also the title and accessible name */
    label: string;
    icon: ReactNode;
  }
>(function ToolButton({ active, label, icon, className, type = "button", ...props }, ref) {
  return (
    <button
      ref={ref}
      type={type}
      title={label}
      aria-label={label}
      className={cn("u-tool", active && "is-on", className)}
      {...props}
    >
      {icon}
      <span className="u-tower-label">{label}</span>
    </button>
  );
});

export function ToolDivider() {
  return <div className="mx-2 h-px bg-line-strong" />;
}

/* ---------- GroupLabel (small heading for a group of content) ----------
   Small size, medium weight, sentence case. **Do not use all caps with letter spacing**: such section headings look like
   a different type system, which users explicitly do not want. The sections in the docked panel, the extraction bar, the three sections of the overview,
   and the groups in the left column are all this. A count can sit on the right, a small icon on the left (the direction arrow in the graph panel). */
export function GroupLabel({
  icon,
  count,
  tone = "default",
  className,
  children,
}: {
  icon?: ReactNode;
  count?: ReactNode;
  /** contest: a group that is blocked, uses the dispute color */
  tone?: "default" | "contest";
  className?: string;
  children: ReactNode;
}) {
  return (
    <div
      className={cn(
        "flex items-center gap-2 text-small font-medium",
        tone === "contest" ? "text-contest" : "text-ink-2",
        className,
      )}
    >
      {icon}
      <span className="min-w-0 truncate">{children}</span>
      {count !== undefined && <span className="u-num">{count}</span>}
    </div>
  );
}

/* ---------- Pill (glass pill: legend, and small toggles floating on the canvas such as "+N classes") ---------- */
export const Pill = forwardRef<
  HTMLButtonElement,
  ButtonHTMLAttributes<HTMLButtonElement> & {
    active?: boolean;
    /** The off state: dimmed to 35% */
    dim?: boolean;
  }
>(function Pill({ active, dim, className, type = "button", ...props }, ref) {
  return (
    <button
      ref={ref}
      type={type}
      className={cn("u-pill", active && "is-on", dim && "is-dim", className)}
      {...props}
    />
  );
});

/* ---------- Radio ---------- */
/* ---------- RadioGroup (a group of mutually exclusive options) ----------
   **A group is one component, not a pile of independent Radios**: arrow keys move within the group, focus lands only
   on the selected one, and screen readers read them as one group; these are properties of a "group" that Radix's
   RadioGroup handles; previously each Radio was a bare input, grouped by a shared `name`,
   and the keyboard part could only rely on the browser's default behavior.
   `children` is what follows the label once selected (such as a date box). */
export function RadioGroup<T extends string>({
  value,
  onChange,
  options,
  className,
  name,
}: {
  value: T;
  onChange: (v: T) => void;
  options: { value: T; label: ReactNode; children?: ReactNode }[];
  className?: string;
  name?: string;
}) {
  return (
    <ShadRadioGroup
      value={value}
      onValueChange={(v) => onChange(v as T)}
      name={name}
      className={cn("gap-2", className)}
    >
      {options.map((o) => (
        <div key={o.value} className="flex items-center gap-2 text-small text-ink-2">
          <ShadRadioGroupItem value={o.value} id={`${name ?? "radio"}-${o.value}`} />
          <label htmlFor={`${name ?? "radio"}-${o.value}`} className="cursor-pointer">
            {o.label}
          </label>
          {value === o.value && o.children}
        </div>
      ))}
    </ShadRadioGroup>
  );
}

/* ---------- ExpandCard (an expandable item: facts, derived items, timeline entries) ----------
   The collapsed row has only one look: a fixed chevron cell on the left (rotates 90° when open), and the header on the right: one line
   or two lines of text. Expanded content is indented under the text (pl-7 = padding 8 + chevron 12 + gap 8).
   the header is a whole clickable button; inside it there can be a span with role="link" (to look at the other end),
   but it cannot contain a button, since a button cannot nest inside a button. */
export function ExpandCard({
  open,
  onToggle,
  dim,
  title,
  className,
  header,
  children,
}: {
  open: boolean;
  onToggle: () => void;
  /** The stale kind: the whole item dimmed */
  dim?: boolean;
  title?: string;
  className?: string;
  header: ReactNode;
  children?: ReactNode;
}) {
  return (
    <div
      className={cn("u-card-row group", open && "is-open", dim && "opacity-55", className)}
      title={title}
    >
      <button
        type="button"
        onClick={onToggle}
        className="flex w-full items-start gap-2 px-2 py-1.5 text-left"
      >
        <ChevronRight
          size={12}
          className={cn("mt-1 shrink-0 text-ink-2 u-turn", open && "rotate-90")}
        />
        <div className="min-w-0 flex-1">{header}</div>
      </button>
      {children && <div className="pb-2 pl-7 pr-2">{children}</div>}
    </div>
  );
}

/** Hover for a row inside a panel. **Gives state only, not layout**: each kind of list has its own row height and padding,
    but hover is the same thing. The panel itself does not respond to the pointer (DESIGN.md 6); hover belongs to the row. */
export const ROW_HOVER = "transition-colors duration-fast hover:bg-surface-2";

/** The shell of a compound row: when a row has two buttons it cannot be a Row (buttons cannot nest inside buttons),
    the outer div uses it to get hover and group */
export const HOVER_ROW =
  "group flex items-center gap-2 rounded-cell px-2 py-1 transition-colors duration-fast hover:bg-surface-2";
/** Things that appear only when the pointer is on the row (.group); add is-on to keep them shown */
export const REVEAL = "u-reveal";
/** A word in a row that can be pointed at and clicked on its own (the predicate and object of a fact row). The row's background color says "this one is pointed at",
 *  and this word's underline says "this particular item is pointed at"; the two layers must be kept apart, so no background color is used */
export const POINT_WORD =
  "cursor-pointer rounded-cell underline-offset-4 hover:text-ink hover:underline";

/* Components that each live in their own file are exported together from here; pages only recognize the one entry point "../ui" */
export { Dialog, DangerConfirm, FormDialog } from "./dialog";
export { Tooltip } from "./tooltip";
export { Table, THead, TBody, Tr, Th, Td } from "./table";
export { Field } from "./field";
