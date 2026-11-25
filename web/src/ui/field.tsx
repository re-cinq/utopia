/* Form field: label + control + one hint or one error line. Spacing is set here once,
   the pages no longer have mb-3 / mt-1 each written their own way. */
import { cloneElement, isValidElement, useId, type ReactNode } from "react";
import { cn, Input, Textarea } from "./index";

export function Field({
  label,
  hint,
  error,
  htmlFor,
  className,
  children,
}: {
  label: ReactNode;
  /** A line of explanation under the control */
  hint?: ReactNode;
  /** Replaces the explanation when there is an error, in the danger color */
  error?: ReactNode;
  htmlFor?: string;
  className?: string;
  children: ReactNode;
}) {
  const generatedId = useId();
  const textControl = isValidElement<{ id?: string }>(children) &&
    (children.type === Input || children.type === Textarea || children.type === "input" || children.type === "textarea");
  // Most callers omit htmlFor; keep the visible label usable for their text control.
  const controlId = textControl ? children.props.id ?? htmlFor ?? generatedId : undefined;
  const control = textControl && !children.props.id ? cloneElement(children, { id: controlId }) : children;
  return (
    <div className={cn("mb-4", className)}>
      <label htmlFor={htmlFor ?? controlId} className="mb-1 block text-small font-medium text-ink-2">
        {label}
      </label>
      {control}
      {(error || hint) && (
        <p className={cn("mt-1 text-fine", error ? "text-danger" : "text-ink-2")}>
          {error ?? hint}
        </p>
      )}
    </div>
  );
}
