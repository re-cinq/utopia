/* Table: styling only. Header in fine-spaced uppercase, thin lines between rows, and the row under the pointer turns surface-2.
   Alignment and width of the data columns are decided by the page; there is no sorting and no paging here (Pager is in index.tsx). */
import type {
  HTMLAttributes,
  TableHTMLAttributes,
  TdHTMLAttributes,
  ThHTMLAttributes,
} from "react";
import { cn } from "./index";

export function Table({
  className,
  ...props
}: TableHTMLAttributes<HTMLTableElement>) {
  return (
    <div className="w-full overflow-x-auto">
      <table
        className={cn("w-full border-collapse text-body text-ink", className)}
        {...props}
      />
    </div>
  );
}

export function THead({
  className,
  ...props
}: HTMLAttributes<HTMLTableSectionElement>) {
  return <thead className={cn("text-left", className)} {...props} />;
}

export function TBody({
  className,
  ...props
}: HTMLAttributes<HTMLTableSectionElement>) {
  return <tbody className={className} {...props} />;
}

export function Tr({
  interactive,
  active,
  className,
  ...props
}: HTMLAttributes<HTMLTableRowElement> & {
  /** Clickable rows: the surface appears under the pointer and the cursor becomes a hand */
  interactive?: boolean;
  /** The selected row. **Uses `u-nav-active`, the same marker as the rows in the left column**: it is
   *  twice the hover surface level (0.09 vs 0.045) plus white text, so "what I selected" and
   *  "what I am pointing at" stay distinct; if both looked alike, moving to another row would leave two rows that look the same
   *  on screen */
  active?: boolean;
}) {
  return (
    <tr
      aria-current={active ? "true" : undefined}
      className={cn(
        "border-b border-line",
        interactive && "cursor-pointer transition-colors duration-fast hover:bg-surface-2",
        active && "u-nav-active",
        className,
      )}
      {...props}
    />
  );
}

export function Th({
  className,
  ...props
}: ThHTMLAttributes<HTMLTableCellElement>) {
  return (
    <th
      className={cn(
        "border-b border-line-strong px-3 py-2 text-fine font-medium uppercase tracking-wider text-ink-2",
        className,
      )}
      {...props}
    />
  );
}

export function Td({
  className,
  ...props
}: TdHTMLAttributes<HTMLTableCellElement>) {
  return <td className={cn("px-3 py-2 align-top", className)} {...props} />;
}
