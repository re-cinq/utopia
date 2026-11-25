/** Authoring desk for business rules (0021 / #277).
 *
 *  **A rule has to read like a sentence:** "Well and its subclasses, when total gas is greater than 8 and interpretation belongs to {…},
 *  yields GasBearingWell". So there is no expression input here: the structured dropdowns and number boxes are both how a rule is entered
 *  and the only way it is displayed, so the two cannot drift apart.
 *
 *  Conditions are submitted as a whole group rather than patched item by item: a conjunction is one whole, with no "half-edited" state.
 *
 *  Styling follows the five rules in web/DESIGN.md: five font-size tiers, six spacing tiers, colours only via tokens, and controls and states
 *  always come from ui/. */
import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Pencil, Play, Plus, Search, Trash2 } from "lucide-react";
import {
  api,
  type BusinessRule,
  type EntityTypeView,
  type RelationTypeView,
  type RuleCondition,
} from "../api";
import { S } from "../i18n";
import {
  Button,
  cn,
  Chip,
  DangerConfirm,
  Dropdown,
  IconButton,
  Input,
  LinkButton,
  PageHeader,
  Dialog,
  Field,
  Table,
  TBody,
  Td,
  Th,
  THead,
  Tr,
  localDate,
} from "../ui";
import { toast } from "../toast";
import { expressionText, metadataOnly, metadataPatch } from "./ruleExpressions";
import { ruleDependencies } from "./ruleDependencies";

/** op → the verb in that sentence. **Numeric and set operators are kept apart**, since their operands look different */
const OPS: {
  value: string;
  label: () => string;
  operand: "num" | "range" | "set" | "none";
}[] = [
  { value: "gt", label: () => S.ontology.ruleOpGt, operand: "num" },
  { value: "gte", label: () => S.ontology.ruleOpGte, operand: "num" },
  { value: "lt", label: () => S.ontology.ruleOpLt, operand: "num" },
  { value: "lte", label: () => S.ontology.ruleOpLte, operand: "num" },
  { value: "between", label: () => S.ontology.ruleOpBetween, operand: "range" },
  { value: "in", label: () => S.ontology.ruleOpIn, operand: "set" },
  { value: "not_in", label: () => S.ontology.ruleOpNotIn, operand: "set" },
  { value: "present", label: () => S.ontology.ruleOpPresent, operand: "none" },
];

const operandKind = (op: string) =>
  OPS.find((o) => o.value === op)?.operand ?? "num";

/** All the text a rule can be found by. **The criterion counts too**: "which rule uses Clearance"
    is the most common way of looking for a rule; searching by name alone would make you first remember what you originally called it */
function searchText(r: BusinessRule, attributes: RelationTypeView[], relations: RelationTypeView[]): string {
  return [
    r.name,
    r.description ?? "",
    r.subject_label,
    r.conclude_type_label ?? "",
    r.conclude_predicate_label ?? "",
    r.join_predicate_label ?? "",
    ...r.conditions.map((c) => c.side),
    r.conclusion === "computed" ? expressionText(r.conclude_expr, attributes, S.ontology.ruleUnknownExpression) : "",
    ...r.conditions.map(
      (c) => `${c.predicate_label} ${operandText(c.op, c.operand, attributes)}`,
    ),
  ]
    .join(" ")
    .toLowerCase();
}

/** Condition operand → the text in the input box. Reading back must use the same scheme as writing, otherwise one edit distorts it */
function operandText(op: string, operand: unknown, attributes: RelationTypeView[] = []): string {
  if (operand && typeof operand === "object" && !Array.isArray(operand)) {
    return expressionText(operand, attributes, S.ontology.ruleUnknownExpression);
  }
  const kind = operandKind(op);
  if (kind === "none") return "";
  if (kind === "set") return Array.isArray(operand) ? operand.join(", ") : "";
  if (kind === "range") return Array.isArray(operand) ? operand.join(" - ") : "";
  return operand === null || operand === undefined ? "" : String(operand);
}

/** Input box text → operand. **If it cannot be parsed, return undefined**, and the caller blocks it before saving */
export function parseOperand(op: string, text: string): unknown | undefined {
  const kind = operandKind(op);
  if (kind === "none") return undefined;
  const t = text.trim();
  if (!t) return undefined;
  if (kind === "set") {
    // **A full-width comma is a separator too**: a comma typed with a Chinese input method is a full-width one. Previously only the half-width comma was recognised,
    // so the pair "gas-test anomaly, post-gas-test anomaly" was stored as one member: "belongs to" never held,
    // and "does not belong to" held for every value. metadataOnly deliberately does not lock members containing a comma, so that
    // an old rule stored that way is repaired by opening and saving it again; the cost is that values written through the API are split too
    const set = t
      .split(/[,，]/)
      .map((s) => s.trim())
      .filter(Boolean);
    return set.length ? set : undefined;
  }
  if (kind === "range") {
    const number = "[+-]?(?:\\d+(?:\\.\\d*)?|\\.\\d+)(?:[eE][+-]?\\d+)?";
    const match = new RegExp(`^(${number})\\s*[-~]\\s*(${number})$`).exec(t);
    const parts = match ? [Number(match[1]), Number(match[2])] : [];
    return parts.length === 2 && parts.every((n) => Number.isFinite(n))
      ? parts
      : undefined;
  }
  const n = Number(t);
  return Number.isFinite(n) ? n : undefined;
}

/** Conditions are split into groups, with group order ascending: consistent with the order the evaluator reads them (decision record 0029).
    A condition without a group number is group 0: rules written before 0029 were a single conjunction to begin with */
function byGroup<T extends { group?: number }>(conditions: T[]): T[][] {
  const g = (c: T) => c.group ?? 0;
  return [...new Set(conditions.map(g))]
    .sort((a, b) => a - b)
    .map((n) => conditions.filter((c) => g(c) === n));
}

/** Type views make both dropdowns readable, so the API only has to carry ids. */
const labelOptions = (items: RelationTypeView[]) =>
  items.map((item) => ({ value: item.id, label: item.label }));

/** One condition row in the form. **The text is kept as typed**: parsing happens at the moment of saving, otherwise a "1" typed halfway
    would be taken as a finished number */
type Row = { side: "x" | "y"; predicate_id: string; op: string; text: string };

type Draft = {
  /** Which rule is being edited; null when creating. **The same draft serves both purposes**: two forms would drift apart */
  id: string | null;
  name: string;
  description: string;
  subject_type_id: string;
  conclusion: "typing" | "attribute" | "relation";
  conclude_type_id: string;
  conclude_predicate_id: string;
  join_predicate_id: string;
  conclude_value: string;
  /** Each block is a conjunction, and blocks are disjoined. **An empty array is only allowed on the single block**:
      that means "no condition written yet", not "unconditionally true" (an empty conjunction is always true, and would sweep in the whole class) */
  groups: Row[][];
};

const emptyDraft = (
  classes: EntityTypeView[],
  attrs: RelationTypeView[],
  relations: RelationTypeView[],
): Draft => ({
  id: null,
  name: "",
  description: "",
  subject_type_id: classes[0]?.id ?? "",
  conclusion: "typing",
  conclude_type_id: classes[0]?.id ?? "",
  conclude_predicate_id: attrs[0]?.id ?? "",
  join_predicate_id: relations[0]?.id ?? "",
  conclude_value: "",
  groups:
    attrs[0]
      ? [[{ side: "x", predicate_id: attrs[0].id, op: "gt", text: "" }]]
      : [[]],
});

/** Existing rule → draft. **Reading back must have the same shape as writing**, otherwise one edit distorts it. */
function draftOf(r: BusinessRule): Draft {
  if (metadataOnly(r)) throw new Error(S.ontology.ruleExpressionReadOnly);
  const groups = byGroup(r.conditions).map((g) =>
    g.map((c) => ({
      side: c.side ?? "x",
      predicate_id: c.predicate_id,
      op: c.op,
      text: operandText(c.op, c.operand),
    })),
  );
  return {
    id: r.id,
    name: r.name,
    description: r.description ?? "",
    subject_type_id: r.subject_type_id,
    conclusion: r.conclusion as Draft["conclusion"],
    conclude_type_id: r.conclude_type_id ?? "",
    conclude_predicate_id: r.conclude_predicate_id ?? "",
    join_predicate_id: r.join_predicate_id ?? "",
    conclude_value:
      typeof r.conclude_value === "string"
        ? r.conclude_value
        : r.conclude_value === undefined || r.conclude_value === null
          ? ""
          : String(r.conclude_value),
    // The server does not accept a rule with no conditions at all, but the draft must hold up this cell: an empty block
    // is what says "not written yet", and that is how the interface can show "add a condition"
    groups: groups.length ? groups : [[]],
  };
}

/** What a rule is tagging right now, and on what grounds. Opening the count shows exactly this. */
function Matches({ kbId, ruleId }: { kbId: string; ruleId: string }) {
  const q = useQuery({
    queryKey: ["ruleMatches", kbId, ruleId],
    queryFn: () => api.ruleMatches(kbId, ruleId),
  });
  const rows = q.data?.matches ?? [];
  const total = q.data?.total ?? 0;
  if (!rows.length) {
    return <p className="text-small text-ink-2">{S.ontology.ruleMatchesEmpty}</p>;
  }
  return (
    <div className="space-y-2">
      {rows.map((m) => (
        <div key={m.derived_id} className="space-y-1">
          <div className="flex flex-wrap items-baseline gap-2">
            <span className="text-small text-ink">{m.entity}</span>
            {m.object_entity ? (
              <span className="text-fine text-ink-2">
                → {m.relation_predicate ?? m.concluded} {m.object_entity}
              </span>
            ) : (
              <span className="text-fine text-ink-2">→ {m.concluded}</span>
            )}
            {/* The same entity can appear several times because of readings from different periods; writing out this period is what keeps it from looking like a duplicate */}
            {m.valid_from && (
              <span className="u-num text-fine text-ink-2">
                {S.ontology.ruleMatchSpan(
                  m.valid_from.slice(0, 10),
                  m.valid_to ? m.valid_to.slice(0, 10) : null,
                )}
              </span>
            )}
          </div>
          {/* The premise is the "on what grounds": without it the list is no different from a string of unexplained judgements */}
          {m.premises.length > 0 && (
            <p className="text-fine text-ink-2">
              {S.ontology.ruleMatchBecause(m.premises.join(", "))}
            </p>
          )}
        </div>
      ))}
      {total > rows.length && (
        <p className="u-num text-fine text-ink-2">
          {S.ontology.ruleMatchesMore(rows.length, total)}
        </p>
      )}
    </div>
  );
}

/** The definition history of a rule (0060). Each version is read back in its current form: its criterion, conclusion, period, and how many hold on its strength right now */
function History({ kbId, ruleId, attributes }: { kbId: string; ruleId: string; attributes: RelationTypeView[] }) {
  const q = useQuery({
    queryKey: ["ruleVersions", kbId, ruleId],
    queryFn: () => api.ruleVersions(kbId, ruleId),
  });
  const versions = q.data?.versions ?? [];
  if (!versions.length) {
    return <p className="text-small text-ink-2">{S.ontology.ruleHistoryEmpty}</p>;
  }
  return (
    <div className="space-y-4">
      {versions.map((v) => {
        const label = (id: string | null) => (id ? (v.labels[id] ?? id) : "");
        // Reuses the criterion and conclusion renderers: a version in the history is what the rule looked like at the time
        const asRule = {
          id: v.id,
          name: "",
          description: "",
          enabled: true,
          version: v.seq,
          subject_type_id: v.definition.subject_type_id,
          subject_label: label(v.definition.subject_type_id),
          conclusion: v.definition.conclusion,
          conclude_type_id: v.definition.conclude_type_id,
          conclude_type_label: v.definition.conclude_type_id ? label(v.definition.conclude_type_id) : null,
          conclude_predicate_id: v.definition.conclude_predicate_id,
          conclude_predicate_label: v.definition.conclude_predicate_id ? label(v.definition.conclude_predicate_id) : null,
          conclude_value: v.definition.conclude_value,
          conclude_expr: v.definition.conclude_expr,
          join_predicate_id: v.definition.join_predicate_id,
          join_predicate_label: v.definition.join_predicate_id ? label(v.definition.join_predicate_id) : null,
          conditions: v.definition.conditions.map((c) => ({
            group: c.group,
            side: c.side,
            predicate_id: c.predicate_id,
            predicate_label: label(c.predicate_id),
            op: c.op,
            operand: c.operand,
          })),
          derived_count: v.derived_count,
          capped: 0,
        } as unknown as BusinessRule;
        return (
          <div key={v.id} className="space-y-1">
            <div className="flex flex-wrap items-baseline gap-2">
              <span className="u-num text-small text-ink">{S.ontology.ruleVersion(v.seq)}</span>
              {!v.superseded_at && <Chip tone="success">{S.ontology.ruleVersionCurrent}</Chip>}
              <span className="u-num text-fine text-ink-2">
                {S.ontology.ruleVersionSince(localDate(v.recorded_at), v.superseded_at ? localDate(v.superseded_at) : null)}
              </span>
              <span className="u-num text-fine text-ink-2">{S.ontology.ruleVersionStanding(v.derived_count)}</span>
            </div>
            <RuleCriterion rule={asRule} attributes={attributes} />
            <div className="text-small text-ink">
              <span className="text-ink-2">{asRule.subject_label} → </span>
              <RuleConclusion rule={asRule} attributes={attributes} />
            </div>
          </div>
        );
      })}
    </div>
  );
}

export function RulesPanel({
  kbId,
  focusId,
  classes,
  attributes,
  relations,
  onError,
}: {
  kbId: string;
  /** When a rule edge is clicked on the schema graph, that row lights up, saving a second search through a page of rules */
  focusId?: string;
  classes: EntityTypeView[];
  /** A predicate with kind='attribute': the rule only reads the entity's own literal values */
  attributes: RelationTypeView[];
  /** A predicate with kind='relation': a joined rule walks only one of its edges */
  relations: RelationTypeView[];
  onError: (e: unknown) => void;
}) {
  const qc = useQueryClient();
  const rules = useQuery({
    queryKey: ["rules", kbId],
    queryFn: () => api.rules(kbId),
  });
  const [draft, setDraft] = useState<Draft | null>(null);
  const [metadataRule, setMetadataRule] = useState<BusinessRule | null>(null);
  useEffect(() => { setDraft(null); setMetadataRule(null); }, [kbId]);
  /** The one pending deletion confirmation. Deleting a rule takes with it all the conclusions it inferred, so it is worth a pause */
  const [doomed, setDoomed] = useState<BusinessRule | null>(null);
  /** Which rule's hit list is expanded. Only one at a time: two long lists side by side cannot be read */
  const [opened, setOpened] = useState<string | null>(null);
  const [historyOf, setHistoryOf] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [dependenciesOf, setDependenciesOf] = useState<string | null>(null);
  const [navigation, setNavigation] = useState<{ id: string } | null>(null);
  useEffect(() => { setDependenciesOf(null); setNavigation(null); }, [kbId]);
  useEffect(() => {
    if (!navigation) return;
    const row = document.getElementById(`rule-${kbId}-${navigation.id}`);
    row?.scrollIntoView({ block: "center" });
    row?.focus();
  }, [kbId, navigation]);

  const invalidate = () => {
    qc.invalidateQueries({ queryKey: ["rules", kbId] });
    qc.invalidateQueries({ queryKey: ["graph"] });
    qc.invalidateQueries({ queryKey: ["entity", kbId] });
  };

  const save = useMutation({
    mutationFn: async () => {
      const d = draft!;
      // The group number is the block's position in the form. **It need not be kept as a run of consecutive numbers**: the server accepts what is sent,
      // as integers, and the evaluator sorts them itself (0029); counting from 0 here is only because it is the least effort
      const conditions: RuleCondition[] = d.groups.flatMap((g, gi) =>
        g.map((c) => {
          const operand = parseOperand(c.op, c.text);
          if (operandKind(c.op) !== "none" && operand === undefined) {
            throw new Error(S.ontology.ruleNeedsCondition);
          }
          return { group: gi, side: c.side, predicate_id: c.predicate_id, op: c.op, operand };
        }),
      );
      if (!conditions.length) throw new Error(S.ontology.ruleNeedsCondition);
      const conclusion = {
        conclusion: d.conclusion,
        conclude_type_id:
          d.conclusion === "typing" ? d.conclude_type_id : undefined,
        conclude_predicate_id:
          d.conclusion === "typing" ? undefined : d.conclude_predicate_id,
        conclude_value:
          d.conclusion === "attribute" ? d.conclude_value : undefined,
        join_predicate_id:
          d.conclusion === "relation" ? d.join_predicate_id : undefined,
      };
      // Editing an existing rule uses PATCH, and the main class stays put: changing the main class is changing a different rule,
      // so deleting and rewriting it is more honest than editing in place
      await (d.id
        ? api.updateRule(kbId, d.id, {
            name: d.name,
            description: d.description,
            conditions,
            ...conclusion,
          })
        : api.createRule(kbId, {
            name: d.name,
            description: d.description,
            subject_type_id: d.subject_type_id,
            conditions,
            ...conclusion,
          }));
    },
    onSuccess: () => {
      toast.success(S.ontology.ruleSaved);
      setDraft(null);
      invalidate();
    },
    onError: (e: Error) => toast.error(e.message),
  });

  const run = useMutation({
    mutationFn: () => api.runRules(kbId),
    onSuccess: (r) => {
      toast.success(S.ontology.ruleRunDone(r.hits, r.inserted, r.invalidated));
      // An expansion that is not complete has to be said separately: inferring fewer results looks the same as "not satisfied" in the outcome
      if (r.capped) toast.error(S.ontology.ruleRunCapped(r.capped));
      // A chain that did not run to the end is also "fewer inferred", the same kind of hint as an unfinished combination expansion (0030)
      if (r.rounds_capped) toast.error(S.ontology.ruleRunRoundsCapped(r.rounds));
      invalidate();
    },
    onError,
  });

  const toggle = useMutation({
    mutationFn: (r: BusinessRule) =>
      api.updateRule(kbId, r.id, { enabled: !r.enabled }),
    onSuccess: invalidate,
    onError,
  });

  const remove = useMutation({
    mutationFn: (id: string) => api.deleteRule(kbId, id),
    onSuccess: () => {
      toast.success(S.ontology.ruleDeleted);
      invalidate();
    },
    onError,
  });

  const all = rules.data?.rules ?? [];
  const dependencies = useMemo(() => ruleDependencies(rules.data?.rules ?? [], classes, attributes), [rules.data, classes, attributes]);
  const inspecting = all.find((r) => r.id === dependenciesOf);
  const navigateRule = (id: string) => {
    setFilter(""); setDependenciesOf(null); setNavigation({ id });
  };
  const needle = filter.trim().toLowerCase();
  const list = needle
    ? all.filter((r) => searchText(r, attributes, relations).includes(needle))
    : all;
  /** Which one the hit list is for. One at a time: two long lists side by side cannot be read */
  const opening = list.find((r) => r.id === opened) ?? null;

  return (
    <div className="space-y-4">
      {doomed && (
        <DangerConfirm
          title={S.ontology.ruleDelete}
          hint={S.ontology.ruleDeleteConfirm(doomed.name)}
          confirmLabel={S.ontology.ruleDelete}
          cancelLabel={S.graph.editCancel}
          busy={remove.isPending}
          onConfirm={() => {
            remove.mutate(doomed.id);
            setDoomed(null);
          }}
          onCancel={() => setDoomed(null)}
        />
      )}

      <PageHeader
        className="mb-2"
        title={S.ontology.rulesTitle}
        sub={S.ontology.rulesHint}
      />

      {/* Search and the two actions sit on the same row, all outside the table (DESIGN.md 6): when the filter empties the list, this row stays,
          otherwise the only entry for changing the filter would vanish along with the list. Controls do not go into the PageHeader's
          actions: that row is aligned to the title's baseline, and putting a control in would break the baseline */}
      <div className="flex flex-wrap items-center gap-2">
        <Input
          icon={<Search size={13} />}
          className="w-64"
          placeholder={S.ontology.ruleSearch}
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        <div className="ml-auto flex items-center gap-2">
          <Button
            size="sm"
            variant="ghost"
            onClick={() => run.mutate()}
            disabled={run.isPending || !all.length}
          >
            <Play size={12} />
            {run.isPending ? S.ontology.ruleRunning : S.ontology.ruleRun}
          </Button>
          {/* No threshold for entry: when an attribute is missing, the form itself says so at the missing spot */}
          <Button
            size="sm"
            variant="primary"
            onClick={() => setDraft(emptyDraft(classes, attributes, relations))}
          >
            <Plus size={12} />
            {S.ontology.ruleNew}
          </Button>
        </div>
      </div>

      {!list.length ? (
        <p className="text-small text-ink-2">
          {/* Zero rules is not the same as "all filtered out": the former should lead to creating one, the latter to changing the filter */}
          {needle ? S.ontology.rulesNoMatch : S.ontology.rulesEmpty}
        </p>
      ) : (
        /* One table, not one card per rule (DESIGN.md 6). **The first column is still the sentence**:
           all the semantics of a rule are in that sentence, and tucking it into details would hide the rule itself; the other columns
           are the answers you want at a glance: what it infers, how many hold right now, and whether it is on */
        <div className="glass overflow-hidden rounded-panel">
          <Table>
            <THead>
              <Tr>
                <Th>{S.ontology.ruleColRule}</Th>
                <Th>{S.ontology.ruleConditions}</Th>
                <Th>{S.ontology.ruleConcludes}</Th>
                <Th>{S.ontology.ruleColDerived}</Th>
                <Th>{S.ontology.ruleColStatus}</Th>
                <Th />
              </Tr>
            </THead>
            <TBody>
              {list.map((r) => (
                <Tr
                  key={r.id}
                  id={`rule-${kbId}-${r.id}`}
                  tabIndex={-1}
                  className={cn(
                    !r.enabled && "opacity-55",
                    (r.id === focusId || r.id === navigation?.id) && "u-picked bg-surface-2",
                  )}
                >
                  <Td>
                    <div className="text-body text-ink">{r.name}</div>
                    <LinkButton onClick={() => setDependenciesOf(r.id)}>{S.ontology.ruleDependencies}</LinkButton>
                    {/* Which version this is; click to open the definition history. A rule whose criterion has changed has old conclusions that rest on the old version */}
                    <LinkButton className="u-num ml-2" onClick={() => setHistoryOf(r.id)}>
                      {S.ontology.ruleVersion(r.version ?? 1)}
                    </LinkButton>
                    {r.description && (
                      <div className="text-fine text-ink-2">{r.description}</div>
                    )}
                  </Td>
                  <Td>
                    <RuleCriterion rule={r} attributes={attributes} />
                  </Td>
                  {/* **The main class is written together with the conclusion**: a rule says "such a Person is a
                      Veteran"; the main class is the left half of that sentence, not part of the criterion */}
                  <Td className="max-w-96 break-words text-small text-ink">
                    <span className="text-ink-2">{r.subject_label} → </span>
                    <RuleConclusion rule={r} attributes={attributes} />
                  </Td>
                  <Td className="whitespace-nowrap">
                    {/* The number of conclusions that hold on its strength right now. **Clickable**: twenty entities can still be opened one by one,
                        but at two hundred, only this list is usable */}
                    {r.derived_count > 0 ? (
                      <LinkButton
                        className="u-num"
                        onClick={() => setOpened(r.id)}
                      >
                        {S.ontology.ruleDerivedCount(r.derived_count)}
                      </LinkButton>
                    ) : (
                      <span className="text-small text-ink-2">—</span>
                    )}
                    {/* Combinations that cannot be fully expanded: always shown, not only in the hint right after a run. Inferring fewer results looks
                        the same as "not satisfied" in the outcome */}
                    {r.capped > 0 && (
                      <Chip
                        tone="warn"
                        className="ml-2"
                        title={S.ontology.ruleCappedHint}
                      >
                        {S.ontology.ruleCappedChip}
                      </Chip>
                    )}
                  </Td>
                  <Td>
                    {/* The switch is an action, so it is a Button; enabled or not is told apart by variant,
                        rather than using a Chip as a button, since a Chip is a status label and does not accept clicks */}
                    <Button
                      size="sm"
                      variant={r.enabled ? "secondary" : "ghost"}
                      onClick={() => toggle.mutate(r)}
                      aria-pressed={r.enabled}
                    >
                      {r.enabled ? S.ontology.ruleEnabled : S.ontology.ruleDisabled}
                    </Button>
                  </Td>
                  <Td className="whitespace-nowrap text-right">
                    <IconButton
                      label={S.ontology.ruleEdit}
                      size="sm"
                      onClick={() => metadataOnly(r) ? setMetadataRule(r) : setDraft(draftOf(r))}
                    >
                      <Pencil size={12} />
                    </IconButton>
                    <IconButton
                      label={S.ontology.ruleDelete}
                      size="sm"
                      className="ml-1"
                      onClick={() => setDoomed(r)}
                    >
                      <Trash2 size={12} />
                    </IconButton>
                  </Td>
                </Tr>
              ))}
            </TBody>
          </Table>
        </div>
      )}

      <Dialog open={!!inspecting} onOpenChange={(open) => !open && setDependenciesOf(null)}
        closeLabel={S.ui.close} title={inspecting?.name ?? ""} description={S.ontology.ruleDependenciesHint}>
        {inspecting && <div className="space-y-4">
          {dependencies.incomplete && <p className="text-small text-warn">{S.ontology.ruleDependenciesIncomplete}</p>}
          <RuleDependencyList title={S.ontology.rulePotentialProducers}
            rules={all.filter((r) => dependencies.links.get(inspecting.id)?.producers.has(r.id))} onSelect={navigateRule} />
          <RuleDependencyList title={S.ontology.rulePotentialConsumers}
            rules={all.filter((r) => dependencies.links.get(inspecting.id)?.consumers.has(r.id))} onSelect={navigateRule} />
        </div>}
      </Dialog>

      {/* Hits: which conclusions this one has inferred right now */}
      <Dialog
        open={!!opening}
        onOpenChange={(o) => !o && setOpened(null)}
        closeLabel={S.ui.close}
        title={opening?.name ?? ""}
        description={S.ontology.ruleMatchesTitle}
      >
        {opening && <Matches kbId={kbId} ruleId={opening.id} />}
      </Dialog>

      {/* Definition history: how many times this one has changed, and what each version said */}
      <Dialog
        open={!!historyOf}
        onOpenChange={(o) => !o && setHistoryOf(null)}
        closeLabel={S.ui.close}
        title={list.find((r) => r.id === historyOf)?.name ?? ""}
        description={S.ontology.ruleHistoryTitle}
      >
        {historyOf && <History kbId={kbId} ruleId={historyOf} attributes={attributes} />}
      </Dialog>

      {metadataRule && (
        <RuleMetadataDialog key={`${kbId}/${metadataRule.id}`} kbId={kbId} rule={metadataRule}
          attributes={attributes} onClose={() => setMetadataRule(null)} onSaved={invalidate} />
      )}
      {draft && (
        <RuleDialog
          draft={draft}
          setDraft={setDraft}
          classes={classes}
          attributes={attributes}
          relations={relations}
          busy={save.isPending}
          onSave={() => save.mutate()}
        />
      )}
    </div>
  );
}

/** The criterion. **This cell is the whole semantics of the rule**; there is no other place where conditions are hidden.
 *
 *  One condition per row, stacked together means "and": a column of conditions reads as "all must hold", which is the filter
 *  convention throughout, so there is no need to print a conjunction word. **The only conjunction printed is "or"**, because only it
 *  needs marking: moving between groups changes how the criterion is satisfied, not adding another requirement (0029).
 *
 *  Previously this was one run-on sentence, where the two conjunctions weighed the same and you could not tell which bound first,
 *  and that is exactly where a rule is most easily misread. */
function RuleCriterion({ rule, attributes }: { rule: BusinessRule; attributes: RelationTypeView[] }) {
  const groups = byGroup(rule.conditions);
  return (
    <div className="space-y-1">
      {groups.map((group, gi) =>
        group.map((c, i) => (
          <div key={`${gi}-${i}`} className="flex items-baseline gap-2">
            {/* "or" occupies the first column at the start of the line: only the first row of each block writes it, so a reader can glance at
                the left and see how many ways this rule can be satisfied */}
            <span className="w-6 shrink-0 text-right text-fine text-ink">
              {gi > 0 && i === 0 ? S.ontology.ruleOr : ""}
            </span>
            <span className="text-small text-ink-2">
              <span className="mr-1 u-num text-fine text-ink-2">
                {c.side === "y" ? S.ontology.ruleSideY : S.ontology.ruleSideX}
              </span>{" "}
              <span className="text-ink">{c.predicate_label}</span>{" "}
              {OPS.find((o) => o.value === c.op)?.label() ?? c.op}{" "}
              <span className="u-num text-ink">{operandText(c.op, c.operand, attributes)}</span>
            </span>
          </div>
        )),
      )}
    </div>
  );
}

/** Write a rule. **Looks the same as every other form on the site**: a dialog, Field labels, and two buttons in the bottom bar.
    Previously it was a card grown inside the page, which said different things from "new token" and "register data source". */
function RuleDialog({
  draft,
  setDraft,
  classes,
  attributes,
  relations,
  busy,
  onSave,
}: {
  draft: Draft;
  setDraft: (d: Draft | null) => void;
  classes: EntityTypeView[];
  attributes: RelationTypeView[];
  relations: RelationTypeView[];
  busy: boolean;
  onSave: () => void;
}) {
  const ready =
    !!draft.name.trim() &&
    !!draft.subject_type_id &&
    (draft.conclusion !== "relation" || (!!draft.join_predicate_id && !!draft.conclude_predicate_id)) &&
    draft.groups.some((g) => g.length > 0);

  const newRow = (): Row => ({
    side: "x",
    predicate_id: attributes[0]?.id ?? "",
    op: "gt",
    text: "",
  });
  const setGroups = (groups: Row[][]) =>
    // An empty block only counts when it is the sole block ("no condition written yet"). Elsewhere, leaving an empty block
    // would be keeping an always-true conjunction, which swallows the whole main class into the conclusion
    setDraft({ ...draft, groups: groups.some((g) => g.length) ? groups.filter((g) => g.length) : [[]] });
  const editRow = (gi: number, ri: number, patch: Partial<Row>) =>
    setGroups(
      draft.groups.map((g, j) =>
        j === gi ? g.map((r, k) => (k === ri ? { ...r, ...patch } : r)) : g,
      ),
    );
  const dropRow = (gi: number, ri: number) =>
    setGroups(
      draft.groups.map((g, j) => (j === gi ? g.filter((_, k) => k !== ri) : g)),
    );
  const addRow = (gi: number) =>
    setGroups(draft.groups.map((g, j) => (j === gi ? [...g, newRow()] : g)));
  // A new block comes with one row of its own: an empty block means nothing elsewhere, so make it non-empty from birth,
  // which removes one rule of "block still empty" checking afterwards
  const addGroup = () => setGroups([...draft.groups, [newRow()]]);

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && setDraft(null)}
      width="lg"
      closeLabel={S.ui.close}
      title={draft.id ? S.ontology.ruleEditing : S.ontology.ruleNew}
      footer={
        <>
          <Button size="sm" variant="secondary" onClick={() => setDraft(null)}>
            {S.graph.editCancel}
          </Button>
          {/* The save threshold is the same as the server's: name, main class, at least one condition. An empty conjunction is always true,
              and would sweep in the whole class (business_rules.rs blocks it the same way) */}
          <Button size="sm" variant="primary" onClick={onSave} disabled={busy || !ready}>
            {S.ontology.ruleSave}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <div className="grid grid-cols-2 gap-3">
          <Field label={S.ontology.ruleName} className="mb-0">
            <Input
              autoFocus
              className="w-full"
              placeholder={S.ontology.ruleNamePlaceholder}
              value={draft.name}
              onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            />
          </Field>
          <Field
            label={S.ontology.ruleSubject}
            hint={S.ontology.ruleSubjectHint}
            className="mb-0"
          >
            {draft.id ? (
              // When editing an existing rule the main class is fixed: changing the main class is changing a different rule,
              // and the conclusions it infers all hang off the old main class
              <p className="py-1 text-body text-ink">
                {classes.find((c) => c.id === draft.subject_type_id)?.label}
              </p>
            ) : (
              <Dropdown
                className="w-full"
                value={draft.subject_type_id}
                onChange={(v) => setDraft({ ...draft, subject_type_id: v })}
                options={classes.map((c) => ({ value: c.id, label: c.label }))}
              />
            )}
          </Field>
        </div>

        <Field label={S.ontology.ruleDescription} className="mb-0">
          <Input
            className="w-full"
            value={draft.description}
            onChange={(e) => setDraft({ ...draft, description: e.target.value })}
          />
        </Field>

        {/* Conditions are written block by block: rows within a block hold together, and any one block holding is enough (0029).
            **Blocks are not boxed**: the "and" at the start of each row and the "or" between blocks already make the scope clear,
            and an extra border would turn a stretch of form fields into a panel (DESIGN.md 6) */}
        <Field
          label={S.ontology.ruleConditions}
          hint={S.ontology.ruleConditionsHint}
          className="mb-0"
        >
          <div className="space-y-2">
            {draft.groups.map((group, gi) => (
              <div key={gi} className="space-y-2">
                {/* Disjunction is not written as another line-initial conjunction: if it looked the same as "and", you could not tell which came first.
                    A line separates the two blocks, and the word sits in the same vertical column */}
                {gi > 0 && (
                  <div className="flex items-center gap-2 pt-1">
                    <span className="w-10 shrink-0 text-right text-fine text-ink">
                      {S.ontology.ruleOr}
                    </span>
                    <span className="h-px flex-1 bg-line-strong" />
                  </div>
                )}
                {group.map((c, i) => (
                  <div key={i} className="flex items-center gap-2">
                    {/* Conjunction is written at the start of the line rather than the end: the reader first learns "this must also hold",
                        and then reads what this row says */}
                    <span className="w-10 shrink-0 text-right text-fine text-ink-2">
                      {i === 0 ? "" : S.ontology.ruleAnd}
                    </span>
                    {draft.conclusion === "relation" && (
                      <Dropdown
                        className="w-16"
                        value={c.side}
                        onChange={(v) => editRow(gi, i, { side: v as Row["side"] })}
                        options={[
                          { value: "x", label: S.ontology.ruleSideX },
                          { value: "y", label: S.ontology.ruleSideY },
                        ]}
                      />
                    )}
                    <Dropdown
                      className="flex-1"
                      value={c.predicate_id}
                      onChange={(v) => editRow(gi, i, { predicate_id: v })}
                      options={labelOptions(attributes)}
                    />
                    <Dropdown
                      className="w-32"
                      value={c.op}
                      onChange={(v) => editRow(gi, i, { op: v, text: "" })}
                      options={OPS.map((o) => ({ value: o.value, label: o.label() }))}
                    />
                    {operandKind(c.op) !== "none" && (
                      <Input
                        className="w-40"
                        placeholder={S.ontology.ruleOperandPlaceholder(operandKind(c.op))}
                        value={c.text}
                        onChange={(e) => editRow(gi, i, { text: e.target.value })}
                      />
                    )}
                    <IconButton
                      label={S.ontology.ruleDropCondition}
                      size="sm"
                      onClick={() => dropRow(gi, i)}
                    >
                      <Trash2 size={11} />
                    </IconButton>
                  </div>
                ))}
                {/* The add-row button is one per block: when there are two blocks, only it can say which block "add to" refers to */}
                {attributes.length > 0 && (
                  <div className="flex items-center gap-2">
                    <span className="w-10 shrink-0" />
                    <Button size="sm" variant="ghost" onClick={() => addRow(gi)}>
                      <Plus size={11} />
                      {S.ontology.ruleAddCondition}
                    </Button>
                  </div>
                )}
              </div>
            ))}
            {/* A condition tests the value of an attribute. With no attribute, "add a condition" would only add a row of
                a condition that cannot select anything, so here we say what is missing and do not show the button */}
            {attributes.length === 0 && (
              <p className="text-small text-ink-2">
                {S.ontology.ruleNeedsAttribute}
              </p>
            )}
            {/* Adds a whole block. **Not aligned with row indentation**: indenting it would make it sit beside the previous block's "add a condition"
                as two side-by-side buttons, one adding a row and one adding a block, and it would not be clear they govern different things.
                Not shown until the first condition has been written: with no first case, "another case" has nothing to refer to */}
            {draft.groups.some((g) => g.length > 0) && (
              <div className="pt-2">
                <Button size="sm" variant="ghost" onClick={addGroup}>
                  <Plus size={11} />
                  {S.ontology.ruleAddGroup}
                </Button>
              </div>
            )}
          </div>
        </Field>

        <Field label={S.ontology.ruleConcludes} className="mb-0">
          <div className="flex flex-wrap items-center gap-2">
            <Dropdown
              className="w-40"
              value={draft.conclusion}
              onChange={(v) => {
                const conclusion = v as Draft["conclusion"];
                setDraft({
                  ...draft,
                  conclusion,
                  join_predicate_id:
                    conclusion === "relation"
                      ? draft.join_predicate_id || relations[0]?.id || ""
                      : "",
                  conclude_predicate_id:
                    conclusion === "relation"
                      ? draft.conclude_predicate_id || relations[0]?.id || ""
                      : draft.conclude_predicate_id,
                  conclude_value: conclusion === "attribute" ? draft.conclude_value : "",
                });
              }}
              options={[
                { value: "typing", label: S.ontology.ruleConcludesTyping },
                { value: "attribute", label: S.ontology.ruleConcludesAttribute },
                { value: "relation", label: S.ontology.ruleConcludesRelation },
              ]}
            />
            {draft.conclusion === "typing" ? (
              <Dropdown
                className="w-48"
                value={draft.conclude_type_id}
                onChange={(v) => setDraft({ ...draft, conclude_type_id: v })}
                options={classes.map((c) => ({ value: c.id, label: c.label }))}
              />
            ) : (
              <>
                {draft.conclusion === "relation" && (
                  <>
                    <Dropdown
                      className="w-48"
                      value={draft.join_predicate_id}
                      onChange={(v) => setDraft({ ...draft, join_predicate_id: v })}
                      options={labelOptions(relations)}
                    />
                    <Dropdown
                      className="w-48"
                      value={draft.conclude_predicate_id}
                      onChange={(v) => setDraft({ ...draft, conclude_predicate_id: v })}
                      options={labelOptions(relations)}
                    />
                  </>
                )}
                {draft.conclusion === "attribute" && (
                  <>
                    <Dropdown
                      className="w-48"
                      value={draft.conclude_predicate_id}
                      onChange={(v) => setDraft({ ...draft, conclude_predicate_id: v })}
                      options={labelOptions(attributes)}
                    />
                    <Input
                      className="w-40"
                      value={draft.conclude_value}
                      onChange={(e) =>
                        setDraft({ ...draft, conclude_value: e.target.value })
                      }
                    />
                  </>
                )}
              </>
            )}
          </div>
        </Field>
      </div>
    </Dialog>
  );
}


function RuleConclusion({ rule: r, attributes }: { rule: BusinessRule; attributes: RelationTypeView[] }) {
  if (r.conclusion === "typing") return <>{r.conclude_type_label}</>;
  if (r.conclusion === "computed") return <>{r.conclude_predicate_label} = {expressionText(r.conclude_expr, attributes, S.ontology.ruleUnknownExpression)}</>;
  if (r.conclusion === "attribute") return <>{r.conclude_predicate_label} = {JSON.stringify(r.conclude_value)}</>;
  if (r.conclusion === "relation")
    return (
      <>
        {S.ontology.ruleConcludesRelationText(
          r.join_predicate_label ?? r.join_predicate_id ?? "",
          r.conclude_predicate_label ?? r.conclude_predicate_id ?? "",
        )}
      </>
    );
  return <>{S.ontology.ruleUnknownExpression}</>;
}

function RuleMetadataDialog({ kbId, rule, attributes, onClose, onSaved }: {
  kbId: string; rule: BusinessRule; attributes: RelationTypeView[]; onClose: () => void; onSaved: () => void;
}) {
  const [name, setName] = useState(rule.name);
  const [description, setDescription] = useState(rule.description ?? "");
  const save = useMutation({
    mutationFn: () => api.updateRule(kbId, rule.id, metadataPatch(name, description)),
    onSuccess: () => { toast.success(S.ontology.ruleSaved); onSaved(); onClose(); },
  });
  return <Dialog open onOpenChange={(open) => !open && onClose()} title={S.ontology.ruleEditing}
    closeLabel={S.ui.close} width="lg" footer={<>
      <Button size="sm" variant="secondary" onClick={onClose}>{S.graph.editCancel}</Button>
      <Button size="sm" variant="primary" disabled={save.isPending || !name.trim()} onClick={() => save.mutate()}>{S.ontology.ruleSave}</Button>
    </>}>
    <div className="space-y-4">
      <Field label={S.ontology.ruleName}><Input autoFocus className="w-full" value={name} onChange={(e) => setName(e.target.value)} /></Field>
      <Field label={S.ontology.ruleDescription}><Input className="w-full" value={description} onChange={(e) => setDescription(e.target.value)} /></Field>
      <p className="text-small text-ink-2">{S.ontology.ruleExpressionReadOnly}</p>
      <Field label={S.ontology.ruleConditions}><RuleCriterion rule={rule} attributes={attributes} /></Field>
      <Field label={S.ontology.ruleConcludes}><p className="break-words text-body text-ink">{rule.subject_label} → <RuleConclusion rule={rule} attributes={attributes} /></p></Field>
      {save.error && <p role="alert" className="text-small text-danger">{(save.error as Error).message}</p>}
    </div>
  </Dialog>;
}

export function RuleDependencyList({ title, rules, onSelect }: {
  title: string; rules: BusinessRule[]; onSelect: (id: string) => void;
}) {
  return <section className="space-y-2">
    <h3 className="text-body font-medium text-ink">{title}</h3>
    {rules.length ? rules.map((r) => <div key={r.id} className="flex flex-wrap items-baseline gap-2">
      <LinkButton onClick={() => onSelect(r.id)}>{r.name}</LinkButton>
      <span className="text-fine text-ink-2">{r.subject_label} · {r.enabled ? S.ontology.ruleEnabled : S.ontology.ruleDisabled}</span>
    </div>) : <p className="text-small text-ink-2">{S.ontology.ruleDependenciesEmpty}</p>}
  </section>;
}
