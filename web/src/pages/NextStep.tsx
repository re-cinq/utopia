/* The "next step" in an empty state (#313).

   The four pages each used to assume they were "already ready": the graph page unconditionally sent people to configure a model, even while documents were being
   extracted; the chat page showed its greeting as usual even when no model was configured at all, so the user hit the wall only after asking the first question.
   This gathers "how far this base has got" into one judgement, shared by the four pages.

   **Approach: the state in one sentence, the action on the button.** No explanation of the mechanics: those belong in the docs, and a person staring at an
   empty page wants to know where to click next. */
import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { api, type Readiness } from "../api";
import { S } from "../i18n";

export function useReadiness(kbId: string | undefined) {
  return useQuery({
    queryKey: ["readiness", kbId],
    queryFn: () => api.readiness(kbId!),
    enabled: !!kbId,
    // Uploads and extraction both change it, but there is no need to poll: refetching when the page is switched back to is enough
    staleTime: 10_000,
  });
}

/** One sentence plus at most one action. Without an action only the sentence remains (someone without permission should not see a button). */
export function NextStep({
  line,
  action,
}: {
  line: string;
  action?: { label: string; to: string; params?: Record<string, string>; search?: Record<string, unknown> };
}) {
  return (
    <div className="text-center text-body text-ink-2 max-w-xs">
      {line}
      {action && (
        <Link
          // The route table is a literal union type; the target here is computed at runtime
          to={action.to as never}
          params={action.params as never}
          search={(action.search ?? {}) as never}
          className="u-link mt-3 block"
        >
          {action.label}
        </Link>
      )}
    </div>
  );
}

/** The next step for this base. `canUpload` is handed in by the caller, since it knows its own role. */
export function nextStep(
  r: Readiness | undefined,
  opts: { kbId: string; isAdmin: boolean; canUpload: boolean },
): { line: string; action?: { label: string; to: string; params?: Record<string, string>; search?: Record<string, unknown> } } | null {
  if (!r) return null;
  // Order is priority: without a model, uploading does not yield anything extractable; without documents, there is nothing being processed
  if (!r.has_chat_model) {
    return opts.isAdmin
      ? {
          line: S.steps.noModel,
          action: {
            label: S.steps.configureModel,
            to: "/admin",
            search: { tab: "models" },
          },
        }
      : { line: S.steps.noModelAsk };
  }
  if (r.documents === 0) {
    return opts.canUpload
      ? {
          line: S.steps.noDocs,
          action: {
            label: S.steps.upload,
            to: "/kb/$kbId/library",
            params: { kbId: opts.kbId },
          },
        }
      : { line: S.steps.noDocs };
  }
  if (r.processing > 0) {
    return {
      line: S.steps.processing(r.processing),
      action: {
        label: S.steps.viewProgress,
        to: "/kb/$kbId/library",
        params: { kbId: opts.kbId },
      },
    };
  }
  // Processing finished but not one failure was fixed: worth saying so, otherwise "nothing was extracted" would be taken as the product being broken
  if (r.failed > 0 && r.entities === 0) {
    return {
      line: S.steps.someFailed(r.failed),
      action: {
        label: S.steps.openLibrary,
        to: "/kb/$kbId/library",
        params: { kbId: opts.kbId },
      },
    };
  }
  return null;
}
