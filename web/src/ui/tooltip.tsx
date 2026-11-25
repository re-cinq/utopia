/* Tooltip: Radix Tooltip. For things with no visible text (icon buttons, truncated names, a number);
   buttons with a visible label do not need it. The native title waits a second and its style follows the system; this one does not. */
import { Tooltip as RadixTooltip } from "radix-ui";
import type { ReactNode } from "react";

export function Tooltip({
  content,
  side = "top",
  children,
}: {
  content: ReactNode;
  side?: "top" | "bottom" | "left" | "right";
  /** The trigger element; it must accept a ref and events (Button / IconButton / native elements all work) */
  children: ReactNode;
}) {
  return (
    <RadixTooltip.Provider delayDuration={300} skipDelayDuration={200}>
      <RadixTooltip.Root>
        <RadixTooltip.Trigger asChild>{children}</RadixTooltip.Trigger>
        <RadixTooltip.Portal>
          <RadixTooltip.Content
            side={side}
            sideOffset={6}
            collisionPadding={8}
            className="u-pop u-pop-in z-[60] max-w-xs rounded-cell px-2 py-1 text-fine text-ink-2 u-lift"
          >
            {content}
          </RadixTooltip.Content>
        </RadixTooltip.Portal>
      </RadixTooltip.Root>
    </RadixTooltip.Provider>
  );
}
