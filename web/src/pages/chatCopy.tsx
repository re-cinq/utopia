/* Copy in the conversation (#936): one per answer, and one per code block.
   The control is an IconButton in the shell; the copy itself goes through copyText. On deployments opened over http on the LAN,
   the clipboard API does not exist, so it falls back to the old method, and if neither works it says so. */
import { useEffect, useRef, useState, type ComponentProps } from "react";
import { Check, Copy } from "lucide-react";
import { copyText } from "../clipboard";
import { S } from "../i18n";
import { toast } from "../toast";
import { IconButton } from "../ui";

/** Copy button. `text` is only read on press: the text of a code block has to come from the rendered DOM */
export function CopyButton({ label, text }: { label: string; text: () => string }) {
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), 1500);
    return () => clearTimeout(timer);
  }, [copied]);
  return (
    <IconButton
      size="sm"
      label={copied ? S.ask.copied : label}
      onClick={async () => {
        if (await copyText(text())) setCopied(true);
        else toast.error(S.toast.copyFailed);
      }}
    >
      {copied ? <Check size={12} /> : <Copy size={12} />}
    </IconButton>
  );
}

/** A code block in an answer gets a copy button at the top right. It copies the code itself: the text inside `<pre>`,
 *  the spans split out by syntax highlighting do not affect it; the trailing newline of the fenced content is not carried over */
export function CodeBlock(props: ComponentProps<"pre">) {
  const pre = useRef<HTMLPreElement>(null);
  return (
    <div className="u-codeblock">
      <pre {...props} ref={pre} />
      <div className="absolute top-1 right-1">
        <CopyButton
          label={S.ask.copyCode}
          text={() => (pre.current?.textContent ?? "").replace(/\n$/, "")}
        />
      </div>
    </div>
  );
}
