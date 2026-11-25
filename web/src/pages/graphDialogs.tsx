// Edit dialogs for the graph page: an entity (name, type) and the validity interval of a fact.
// The entity panel only displays; previously the two rename fields were embedded under the panel header and the interval form inside the fact row,
// now each opens its own window. The fields and the submit logic are moved over as they were.
import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type EntityFact, type GraphNode } from "../api";
import { S } from "../i18n";
import { toast } from "../toast";
import { fmtTime, parseDateInput } from "../time";
import { Field, FormDialog, Input, RadioGroup, SearchSelect } from "../ui";

/* ---------- Entity: extraction gives a first guess, and if it is wrong the only option used to be re-extracting the whole library ---------- */

export function EntityDialog({
  kbId,
  entityId,
  entity,
  onClose,
  onSaved,
}: {
  kbId: string;
  entityId: string;
  entity: GraphNode;
  onClose: () => void;
  onSaved: () => void;
}) {
  const qc = useQueryClient();
  const [name, setName] = useState(entity.name);
  const [typeId, setTypeId] = useState("");
  // The type dropdown needs the full ontology, not just the few that appear in the current view
  const ontology = useQuery({
    queryKey: ["ontology", kbId],
    queryFn: () => api.ontology(kbId),
  });
  const types = ontology.data?.entity_types ?? [];
  const currentId = types.find((t) => t.key === entity.type_key)?.id ?? "";
  // The ontology arrives asynchronously: when it is all here, match the type dropdown to the current type
  useEffect(() => {
    if (!typeId && currentId) setTypeId(currentId);
  }, [typeId, currentId]);

  const dirty = name.trim() !== entity.name || (!!typeId && typeId !== currentId);
  const save = useMutation({
    mutationFn: () => {
      const body: { type_id?: string; canonical_name?: string } = {};
      if (name.trim() && name.trim() !== entity.name) body.canonical_name = name.trim();
      if (typeId && typeId !== currentId) body.type_id = typeId;
      return api.updateEntity(kbId, entityId, body);
    },
    onSuccess: (r) => {
      toast.success(S.graph.editSaved);
      // After changing the type or name, the graph node and the ontology counts must move with it
      qc.invalidateQueries({ queryKey: ["entity", kbId, entityId] });
      qc.invalidateQueries({ queryKey: ["graph", kbId] });
      qc.invalidateQueries({ queryKey: ["ontology", kbId] });
      onSaved();
    },
    onError: (err: Error) => toast.error(err.message),
  });

  return (
    <FormDialog
      title={S.graph.editTitle}
      closeLabel={S.graph.close}
      saveLabel={S.graph.editSave}
      cancelLabel={S.graph.editCancel}
      canSave={dirty && !!name.trim()}
      busy={save.isPending}
      onSave={() => save.mutate()}
      onCancel={onClose}
    >
      <Field
        label={S.graph.editName}
        error={!name.trim() ? S.graph.editEmptyName : undefined}
      >
        <Input
          autoFocus
          className="w-full"
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
      </Field>
      {/* There can be thousands of classes (installing schema.org gives 1010): this field must allow typing to filter,
          so it is a SearchSelect rather than a dropdown: a dropdown is for small, bounded enumerations */}
      <Field label={S.graph.editType} className="mb-0">
        <SearchSelect
          className="w-full"
          value={typeId}
          onChange={setTypeId}
          options={types.map((t) => ({ value: t.id, label: t.label, hint: t.key }))}
        />
      </Field>
    </FormDialog>
  );
}

/* ---------- The validity interval of a fact (302) ----------
   Both ends are submitted together rather than edited one at a time: the two ends define each other, and "clear the end" has to be distinguishable from "leave the end alone this time".
   The three end options correspond one-to-one with the three ways of writing it in the ledger, so there is
   no implicit convention here where blank means "current until now": that is exactly what let valid_to IS NULL once carry two meanings
   the old problem. */

export function FactTimeDialog({
  kbId,
  fact,
  onClose,
}: {
  kbId: string;
  fact: EntityFact;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const [from, setFrom] = useState(
    fmtTime(fact.valid_from, fact.valid_from_precision) ?? "",
  );
  const [to, setTo] = useState(
    fmtTime(fact.valid_to, fact.valid_to_precision) ?? "",
  );
  const [endMode, setEndMode] = useState<"open" | "unknown" | "date">(
    fact.valid_to
      ? "date"
      : fact.valid_to_precision === "unknown"
        ? "unknown"
        : "open",
  );
  const [note, setNote] = useState("");

  const save = useMutation({
    mutationFn: () => {
      const f = from.trim() ? parseDateInput(from) : null;
      if (from.trim() && !f) throw new Error(S.graph.timeBadDate);
      const t = endMode === "date" ? parseDateInput(to) : null;
      if (endMode === "date" && !t) throw new Error(S.graph.timeBadDate);
      return api.updateFactTime(kbId, fact.id, {
        valid_from: f?.iso ?? null,
        valid_from_precision: f?.precision ?? null,
        valid_to: t?.iso ?? null,
        valid_to_precision:
          endMode === "date"
            ? (t?.precision ?? null)
            : endMode === "unknown"
              ? "unknown"
              : null,
        note: note.trim() || undefined,
      });
    },
    onSuccess: (r) => {
      // The consequences of reconciliation must be stated: changing the start may incidentally close a successor's open interval,
      // or it may create a conflict that a person has to rule on. If this is not said, the graph changes by itself and nobody knows why
      if (r.conflicts) toast.success(S.graph.timeSavedConflicts(r.conflicts));
      else if (r.closed) toast.success(S.graph.timeSavedClosed(r.closed));
      else toast.success(S.graph.timeSaved);
      qc.invalidateQueries({ queryKey: ["entity", kbId] });
      qc.invalidateQueries({ queryKey: ["graph"] });
      qc.invalidateQueries({ queryKey: ["review", kbId] });
      onClose();
    },
    onError: (e: Error) => toast.error(e.message),
  });

  const what = `${fact.predicate_label ?? S.graph.unknownPredicate} → ${
    fact.other_name ?? "?"
  }`;
  return (
    <FormDialog
      width="sm"
      title={S.graph.editTime}
      description={what}
      closeLabel={S.graph.close}
      saveLabel={S.graph.timeSave}
      cancelLabel={S.graph.timeCancel}
      busy={save.isPending}
      onSave={() => save.mutate()}
      onCancel={onClose}
    >
      <Field label={S.graph.timeStart} hint={S.graph.timeFormat}>
        <Input
          autoFocus
          value={from}
          onChange={(e) => setFrom(e.target.value)}
          placeholder={S.graph.timeFormat}
          className="u-num w-full"
        />
      </Field>
      <Field label={S.graph.timeEnd}>
        <RadioGroup
          name={`end-${fact.id}`}
          value={endMode}
          onChange={(v) => setEndMode(v)}
          options={[
            { value: "open" as const, label: S.graph.timeEndOpen },
            { value: "unknown" as const, label: S.graph.timeEndUnknown },
            {
              value: "date" as const,
              label: S.graph.timeEndDate,
              children: (
                <Input
                  size="sm"
                  value={to}
                  onChange={(e) => setTo(e.target.value)}
                  placeholder={S.graph.timeFormat}
                  className="u-num ml-1 flex-1"
                />
              ),
            },
          ]}
        />
      </Field>
      <Field label={S.graph.timeNote} className="mb-0">
        <Input
          value={note}
          onChange={(e) => setNote(e.target.value)}
          placeholder={S.graph.timeNotePlaceholder}
          className="w-full"
        />
      </Field>
    </FormDialog>
  );
}
