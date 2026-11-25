/* Citations: the superscript markers, the footer line, and the preview popover they share.
   Clicking a citation first shows a passage of the source text; the route to the source itself is at the top right. Checking a sentence usually only needs that passage,
   whereas a jump would take the person away from the answer, and on return the answer has already scrolled up. */
import { Popover as RadixPopover } from "radix-ui";
import type { ReactNode } from "react";
import { quotePieces } from "../quotes";

/** An `[n]` in the body. While streaming, a number whose source has not arrived yet is drawn static with {@link CiteMark} */
export function CiteChip({
  n,
  preview,
}: {
  n: number;
  /** Popover content; it only mounts when opened, so the requests inside are also only made when opened */
  preview: ReactNode;
}) {
  return (
    <RadixPopover.Root>
      <RadixPopover.Trigger asChild>
        <button
          type="button"
          className="u-num rounded-cell px-1 text-accent transition-colors duration-fast hover:bg-surface-2"
        >
          [{n}]
        </button>
      </RadixPopover.Trigger>
      <PreviewPortal>{preview}</PreviewPortal>
    </RadixPopover.Root>
  );
}

/** A number that cannot be clicked yet (the source has not arrived). It takes the same space, it just does not glow */
export function CiteMark({ n }: { n: number }) {
  return <span className="u-num px-1 text-ink-2">[{n}]</span>;
}

/** One line in the footer. The whole line is the trigger, and clicking it opens the same popover */
export function CiteRow({
  children,
  preview,
}: {
  children: ReactNode;
  preview: ReactNode;
}) {
  return (
    <RadixPopover.Root>
      <RadixPopover.Trigger asChild>
        <button
          type="button"
          className="block w-full px-3 py-2 text-left text-small text-ink-2 transition-colors duration-fast hover:bg-surface-2"
        >
          {children}
        </button>
      </RadixPopover.Trigger>
      <PreviewPortal align="start">{preview}</PreviewPortal>
    </RadixPopover.Root>
  );
}

function PreviewPortal({
  children,
  align = "center",
}: {
  children: ReactNode;
  align?: "start" | "center";
}) {
  return (
    <RadixPopover.Portal>
      <RadixPopover.Content
        side="top"
        align={align}
        sideOffset={6}
        collisionPadding={8}
        className="u-pop u-pop-in z-[60] rounded-panel u-lift"
      >
        {children}
      </RadixPopover.Content>
    </RadixPopover.Portal>
  );
}

/** What the popover looks like: one line for the title, a passage of source text, and one way out at the top right */
/** A passage of source text, with the quoted sentences marked in the style of a search hit (a follow-up to #968). A quote that does not match is not marked */
export function QuotedText({ text, quotes }: { text: string; quotes?: readonly string[] }) {
  return (
    <>
      {quotePieces(text, quotes).map((p, i) =>
        p.quoted ? (
          <mark key={i} className="u-mark">
            {p.text}
          </mark>
        ) : (
          <span key={i}>{p.text}</span>
        ),
      )}
    </>
  );
}

export function PreviewCard({
  title,
  meta,
  body,
  action,
}: {
  title: ReactNode;
  /** A small line under the title (the section name, where this text came from) */
  meta?: ReactNode;
  body: ReactNode;
  /** Link to the source, placed to the right of the title */
  action: ReactNode;
}) {
  return (
    <div className="w-80 max-w-[min(24rem,90vw)]">
      <div className="flex items-start gap-2 border-b border-line px-3 py-2">
        <div className="min-w-0 flex-1">
          <div className="truncate text-small text-ink">{title}</div>
          {meta && <div className="truncate text-fine text-ink-2">{meta}</div>}
        </div>
        <div className="shrink-0">{action}</div>
      </div>
      {/* The source text is laid out as-is (line breaks are part of it); it scrolls on its own when long, so the popover does not push past the screen */}
      <div className="max-h-64 overflow-auto whitespace-pre-wrap px-3 py-2 text-small leading-relaxed text-ink-2">
        {body}
      </div>
    </div>
  );
}
