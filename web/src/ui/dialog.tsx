/* Dialog: built on Radix Dialog (focus trapping, Esc, backdrop click and aria are all inside it),
   the skin is styles.css's u-modal-*. Nearly solid rather than glass: a confirmation asks a person to read one sentence and make an
   irreversible decision, and body text showing through from below is a distraction (see the note on .u-modal-panel). */
import { Dialog as RadixDialog } from "radix-ui";
import { X } from "lucide-react";
import { useState } from "react";
import type { ReactNode } from "react";
import { Button, IconButton, Input, cn } from "./index";

export function Dialog({
  open,
  onOpenChange,
  title,
  description,
  closeLabel,
  width = "md",
  children,
  footer,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  /** A line of explanation under the title; also the description the screen reader reads out */
  description?: ReactNode;
  /** Accessible name of the close button at the top right: the page takes it from S, and the component does not know the language */
  closeLabel: string;
  width?: "sm" | "md" | "lg";
  children?: ReactNode;
  /** The button area at the bottom right. For dangerous confirmations use DangerConfirm; do not assemble them here */
  footer?: ReactNode;
}) {
  const w = { sm: "w-96", md: "w-[32rem]", lg: "w-[44rem]" }[width];
  return (
    <RadixDialog.Root open={open} onOpenChange={onOpenChange}>
      <RadixDialog.Portal>
        <RadixDialog.Overlay className="u-modal-scrim fixed inset-0 z-50 grid place-items-center overflow-y-auto p-4">
          <RadixDialog.Content
            className={cn(
              "u-modal-panel u-modal-in max-w-full rounded-overlay p-6 u-lift-strong outline-none",
              w,
            )}
          >
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <RadixDialog.Title className="u-title text-title">
                  {title}
                </RadixDialog.Title>
                {description ? (
                  <RadixDialog.Description className="mt-1 text-small text-ink-2">
                    {description}
                  </RadixDialog.Description>
                ) : (
                  // Radix warns in the console when there is no description; an empty one still needs to be present
                  <RadixDialog.Description className="sr-only">
                    {typeof title === "string" ? title : ""}
                  </RadixDialog.Description>
                )}
              </div>
              <RadixDialog.Close asChild>
                <IconButton label={closeLabel} size="sm" variant="ghost">
                  <X size={14} />
                </IconButton>
              </RadixDialog.Close>
            </div>
            {children && <div className="mt-4">{children}</div>}
            {footer && (
              <div className="mt-6 flex justify-end gap-2">{footer}</div>
            )}
          </RadixDialog.Content>
        </RadixDialog.Overlay>
      </RadixDialog.Portal>
    </RadixDialog.Root>
  );
}

/* ---------- DangerConfirm (confirmation of dangerous operations: may require typing specified text to unlock) ----------
   Same interface as before, with the skin swapped to Dialog. **Always mounted** (open is always true): callers use
   conditional rendering to control its appearance, which is as effective as the open prop of Dialog, and old code does not need to change. */
export function DangerConfirm({
  title,
  hint,
  requireText,
  confirmLabel,
  cancelLabel,
  busy,
  onConfirm,
  onCancel,
  children,
}: {
  title: string;
  hint: string;
  /** The unlock text that must be typed exactly (such as a resource name); if omitted, it can be confirmed directly */
  requireText?: string;
  confirmLabel: string;
  cancelLabel: string;
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
  /** Supplementary control under the unlock box (the reason box in the merge dialog); absent by default */
  children?: ReactNode;
}) {
  const [text, setText] = useState("");
  const unlocked = !requireText || text === requireText;
  return (
    <Dialog
      open
      onOpenChange={(o) => {
        if (!o) onCancel();
      }}
      title={<span className="text-danger">{title}</span>}
      description={hint}
      closeLabel={cancelLabel}
      width="sm"
      footer={
        <>
          <Button variant="secondary" size="sm" onClick={onCancel}>
            {cancelLabel}
          </Button>
          <Button
            variant="danger"
            size="sm"
            disabled={!unlocked || busy}
            onClick={onConfirm}
          >
            {confirmLabel}
          </Button>
        </>
      }
    >
      {requireText && (
        <Input
          autoFocus
          className="w-full"
          placeholder={requireText}
          value={text}
          onChange={(e) => setText(e.target.value)}
        />
      )}
      {children}
    </Dialog>
  );
}

/* ---------- FormDialog (edit dialog) ----------
   Changes to classes, attributes, relations, entities and fact intervals all live in this one shell: the panel only displays, and edits open a window.
   The body is wrapped in a <form>: pressing Enter in an input saves; the footer is outside the form, so cancel and delete
   are not taken as submit. Like DangerConfirm, it is **always mounted** (open is always true): callers render it conditionally.
   Delete sits at the bottom left in a secondary style: it is not a pair with save, and separating them keeps it from being clicked by accident */
type DangerAction = {
  label: string;
  disabled?: boolean;
  title?: string;
  onClick: () => void;
};

export function FormDialog({
  title,
  description,
  closeLabel,
  width = "md",
  saveLabel,
  cancelLabel,
  canSave = true,
  busy,
  onSave,
  onCancel,
  danger,
  children,
}: {
  title: ReactNode;
  description?: ReactNode;
  closeLabel: string;
  width?: "sm" | "md" | "lg";
  saveLabel: string;
  cancelLabel: string;
  /** Save cannot be clicked until everything is filled in */
  canSave?: boolean;
  busy?: boolean;
  onSave: () => void;
  onCancel: () => void;
  /** Destructive actions at the bottom left; absent by default. **One or several can be given**:
   *  a member can both be deactivated and be removed; the two have different scopes of effect, so they should not be squeezed into one button,
   *  nor should one of them be pushed into the dialog body disguised as a field. */
  danger?: DangerAction | DangerAction[];
  children: ReactNode;
}) {
  return (
    <Dialog
      open
      onOpenChange={(o) => {
        if (!o) onCancel();
      }}
      title={title}
      description={description}
      closeLabel={closeLabel}
      width={width}
      footer={
        <>
          {danger && (
            <div className="mr-auto flex items-center gap-2">
              {(Array.isArray(danger) ? danger : [danger]).map((d) => (
                <Button
                  key={d.label}
                  variant="secondary"
                  size="sm"
                  disabled={d.disabled || busy}
                  title={d.title}
                  onClick={d.onClick}
                >
                  {d.label}
                </Button>
              ))}
            </div>
          )}
          <Button variant="secondary" size="sm" onClick={onCancel}>
            {cancelLabel}
          </Button>
          <Button
            variant="primary"
            size="sm"
            disabled={!canSave}
            busy={busy}
            onClick={onSave}
          >
            {saveLabel}
          </Button>
        </>
      }
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (canSave && !busy) onSave();
        }}
      >
        {children}
      </form>
    </Dialog>
  );
}

