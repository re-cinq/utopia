// Edit dialogs for the ontology: classes, relations, attributes, and "connect this class with an existing relation".
//
// The right-hand panel only displays; edits open a window. Previously three forms were embedded in the panel, and a half-edited field
// jostled in the same column as the definition being read, and saving and deleting sat right next to each other. No field changed: only the shell moved:
// every item in the form is as before, it just has its own title, its own footer and Esc.
import { useMemo, useState } from "react";
import { useMutation } from "@tanstack/react-query";
import { api, type EntityTypeView, type RelationTypeView } from "../api";
import { S } from "../i18n";
import { toast } from "../toast";
import {
  Checkbox,
  ColorPicker,
  colorForKey,
  Dropdown,
  Field,
  FormDialog,
  Input,
  MultiSearchSelect,
  SearchSelect,
  Segmented,
  Textarea,
} from "../ui";

/** Candidates for the parent-class dropdown: tree order + indentation (levels visible), excluding itself and all descendants (to prevent cycles). */
export function parentOptions(
  allTypes: EntityTypeView[],
  selfId: string | undefined,
): { value: string; label: string; indent: number }[] {
  const excluded = new Set<string>();
  if (selfId) {
    excluded.add(selfId);
    // Collect descendants: scan repeatedly until it converges (the number of classes is small, so O(n²) does not matter)
    let grew = true;
    while (grew) {
      grew = false;
      for (const t of allTypes) {
        if (t.parents.some((p) => excluded.has(p)) && !excluded.has(t.id)) {
          excluded.add(t.id);
          grew = true;
        }
      }
    }
  }
  const children = new Map<string | null, EntityTypeView[]>();
  for (const t of allTypes) {
    const p = t.primary_parent ?? null;
    if (!children.has(p)) children.set(p, []);
    children.get(p)!.push(t);
  }
  const out: { value: string; label: string; indent: number }[] = [];
  const walk = (parent: string | null, depth: number) => {
    for (const t of children.get(parent) ?? []) {
      if (excluded.has(t.id)) continue;
      out.push({ value: t.id, label: t.label, indent: depth });
      walk(t.id, depth + 1);
    }
  };
  walk(null, 0);
  return out;
}

const toggleIn = (
  set: React.Dispatch<React.SetStateAction<string[]>>,
  id: string,
) => set((v) => (v.includes(id) ? v.filter((x) => x !== id) : [...v, id]));

/* ---------- Classes ---------- */

export function ClassDialog({
  kbId,
  existing,
  parentId,
  allTypes,
  onClose,
  onSaved,
  onDeleted,
  onError,
}: {
  kbId: string;
  existing: EntityTypeView | null;
  /** Parent class when creating: pre-filled when entering from the left column or the "new subclass" on the definition page; with multiple parents this is the main parent */
  parentId: string | null;
  allTypes: EntityTypeView[];
  onClose: () => void;
  /** Carries the new id when creation succeeds; undefined when editing succeeds */
  onSaved: (createdId?: string) => void;
  onDeleted: () => void;
  onError: (e: unknown) => void;
}) {
  const [key, setKey] = useState(existing?.key ?? "");
  const [label, setLabel] = useState(existing?.label ?? "");
  // When creating, the colour follows the key (the same rule as the backend color_for_key), not a fixed default.
  // The user of course may change it; but **if they do not, a manually created class and an imported one share a consistent colour scheme**
  const [color, setColor] = useState(
    existing?.color ?? colorForKey(existing?.key ?? ""),
  );
  const [colorTouched, setColorTouched] = useState(Boolean(existing?.color));
  const [shape, setShape] = useState<"circle" | "square">(
    existing?.shape ?? "circle",
  );
  const [parents, setParents] = useState<string[]>(
    existing?.parents ?? (parentId ? [parentId] : []),
  );
  const [description, setDescription] = useState(existing?.description ?? "");
  // Mutually exclusive: declares "cannot be both at once". The consistency check reports unsatisfiable classes from this (0002)
  // a class that inherits two mutually exclusive ancestors can never have an instance, and it reports no error, it just stays empty forever
  const [disjoint, setDisjoint] = useState<string[]>(existing?.disjoint ?? []);
  const options = useMemo(
    () => parentOptions(allTypes, existing?.id),
    [allTypes, existing?.id],
  );

  const save = useMutation({
    mutationFn: async (): Promise<unknown> =>
      existing
        ? api.updateEntityType(kbId, existing.id, {
            label,
            color,
            shape,
            parents,
            disjoint,
            description,
          })
        : api.createEntityType(kbId, {
            key,
            label,
            color,
            shape,
            parents,
            disjoint,
            description,
          }),
    onSuccess: (res) => {
      toast.success(existing ? S.toast.saved : S.toast.created);
      onSaved(existing ? undefined : (res as { id?: string })?.id);
    },
    onError,
  });
  const remove = useMutation({
    mutationFn: () => api.deleteEntityType(kbId, existing!.id),
    onSuccess: () => {
      toast.success(S.toast.deleted);
      onDeleted();
    },
    onError,
  });

  return (
    <FormDialog
      title={existing ? S.ontology.editClass : S.ontology.newClass}
      description={
        existing ? `${existing.key} · ${S.ontology.usage(existing.usage)}` : undefined
      }
      closeLabel={S.ui.close}
      saveLabel={S.ontology.save}
      cancelLabel={S.ontology.cancel}
      canSave={!!label.trim() && (!!existing || !!key.trim())}
      busy={save.isPending}
      onSave={() => save.mutate()}
      onCancel={onClose}
      danger={
        existing && !existing.builtin
          ? {
              label: S.ontology.delete,
              disabled: existing.usage > 0 || remove.isPending,
              title: existing.usage > 0 ? S.ontology.deleteBlocked : undefined,
              onClick: () => remove.mutate(),
            }
          : undefined
      }
    >
      {/* The key is a pure technical identifier: it is not shown once it exists, and is only typed in at creation */}
      {!existing && (
        <Field
          label={
            <>
              {S.ontology.key}{" "}
              <span className="text-ink-2">({S.ontology.keyHint})</span>
            </>
          }
        >
          <Input
            autoFocus
            value={key}
            onChange={(e) => {
              setKey(e.target.value);
              // If the user has not picked a colour themselves, let the colour follow the key: the same rule as the backend
              if (!colorTouched) setColor(colorForKey(e.target.value));
            }}
            className="w-full"
            placeholder="contract"
          />
        </Field>
      )}
      <Field label={S.ontology.label}>
        <Input
          autoFocus={!!existing}
          value={label}
          onChange={(e) => setLabel(e.target.value)}
          className="w-full"
        />
      </Field>
      <Field label={S.ontology.shapeColor}>
        <div className="flex items-center gap-2">
          <ColorPicker
            value={color}
            onChange={(c: string) => {
              setColor(c);
              setColorTouched(true);
            }}
            shape={shape}
          />
          {/* Shape: maps one-to-one to the node rendering in the graph (circle = four-layer circle / square = four-layer square) */}
          <Segmented
            size="sm"
            value={shape}
            onChange={setShape}
            options={(["circle", "square"] as const).map((sh) => ({
              value: sh,
              title: sh,
              // The icon occupies the height of one line of body text (h-4): the height of Segmented is set by its content,
              // and a bare 12px icon would make it shorter than the 32-high colour well beside it
              label: (
                <span className="flex h-4 items-center">
                  <span
                    className={`h-3 w-3 border-[1.5px] border-current ${
                      sh === "circle" ? "rounded-full" : "scale-90"
                    }`}
                  />
                </span>
              ),
            }))}
          />
        </div>
      </Field>
      {/* Multiple parents: subClassOf can have several entries. The left column draws the tree, and a class can appear only once,
          so the first one is the main parent. The interface explains this, without adding a separate "pick main parent" control */}
      <Field
        label={S.ontology.parent}
        hint={parents.length > 1 ? S.ontology.primaryParentHint : undefined}
      >
        <MultiSearchSelect
          values={parents}
          options={options}
          onToggle={(id) => toggleIn(setParents, id)}
          placeholder={S.ontology.searchTypes}
          emptyHint={S.ontology.noParent}
        />
      </Field>
      {/* Mutually exclusive: **declares "cannot be both at once"**. Placed right next to the parent class, because the two are
          two sides of the same thing: the parent says "also is", the mutual exclusion says "cannot be both at once", and the consistency check
          reports "this class can never have an instance" exactly when the two clash. Saying on the spot that a class is exclusive with its own parent
          is faster than making a person run the consistency check and discover it */}
      <Field
        label={S.ontology.disjoint}
        hint={S.ontology.disjointHint}
        error={
          disjoint.some((d) => parents.includes(d))
            ? S.ontology.disjointWithParent
            : undefined
        }
      >
        <MultiSearchSelect
          values={disjoint}
          options={options}
          onToggle={(id) => toggleIn(setDisjoint, id)}
          placeholder={S.ontology.searchTypes}
          emptyHint={S.ontology.noDisjoint}
        />
      </Field>
      {/* Semantic guidance: the whole block is injected into the extraction prompt and directly affects the quality of extraction classification */}
      <Field
        label={S.ontology.description}
        hint={S.ontology.descriptionHint}
        className="mb-0"
      >
        <Textarea
          className="w-full min-h-28 resize-y"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
        />
      </Field>
    </FormDialog>
  );
}

/* ---------- Relations ---------- */

export function PropertyDialog({
  kbId,
  existing,
  allTypes,
  allRelations,
  allAttributes,
  initialDomain,
  onClose,
  onSaved,
  onDeleted,
  onError,
}: {
  kbId: string;
  existing: RelationTypeView | null;
  allTypes: EntityTypeView[];
  /** The relations of this KB (no attributes). The dropdowns for inverse and sub-property take from here: **attributes are not among them**,
   *  because their object is a literal value, and the reverse makes no sense */
  allRelations: RelationTypeView[];
  /** The attribute definitions of this KB, candidates for "attributes on the edge". Passed in separately: previously this dropdown filtered
   *  kind=attribute from allRelations, where attributes are precisely absent, so it was always empty (#1519) */
  allAttributes: RelationTypeView[];
  /** When creating a relation from a class, domain is pre-filled with that class — **it only affects the initial value**,
   *  it is not a constraint: below it is still an editable MultiSearchSelect, so a wrong entry can be changed on the spot */
  initialDomain: string | null;
  onClose: () => void;
  onSaved: (createdId?: string) => void;
  onDeleted: () => void;
  onError: (e: unknown) => void;
}) {
  const [key, setKey] = useState(existing?.key ?? "");
  const [label, setLabel] = useState(existing?.label ?? "");
  const [temporal, setTemporal] = useState(existing?.temporal ?? "state");
  const [functional, setFunctional] = useState(existing?.functional ?? false);
  const [inverseFunctional, setInverseFunctional] = useState(
    existing?.inverse_functional ?? false,
  );
  // The other four OWL axioms. **All of the reasoner's criteria are here**. Previously they could only come in by importing OWL,
  // so a person building an ontology by hand in the interface could never switch that machine on (0002)
  const [transitive, setTransitive] = useState(existing?.is_transitive ?? false);
  const [symmetric, setSymmetric] = useState(existing?.is_symmetric ?? false);
  const [asymmetric, setAsymmetric] = useState(existing?.is_asymmetric ?? false);
  const [irreflexive, setIrreflexive] = useState(
    existing?.is_irreflexive ?? false,
  );
  // The last two of the same family have a different shape: they point at another relation. Empty string = not declared
  const [inverseOf, setInverseOf] = useState(existing?.inverse_of ?? "");
  const [subPropertyOf, setSubPropertyOf] = useState(
    existing?.sub_property_of ?? "",
  );
  const [description, setDescription] = useState(existing?.description ?? "");
  const [domains, setDomains] = useState<string[]>(
    existing?.domains ?? (initialDomain ? [initialDomain] : []),
  );
  const [ranges, setRanges] = useState<string[]>(existing?.ranges ?? []);
  // Attributes on the edge (0037): which attributes an edge of this relation can carry; the choice is the kind=attribute definitions in this KB
  const [qualifiers, setQualifiers] = useState<string[]>(existing?.qualifiers ?? []);
  // Show the display label, not the key. **The key that goes into the prompt is taken from the library by the server**, unrelated to what the interface shows
  const typeOpts = useMemo(() => parentOptions(allTypes, undefined), [allTypes]);
  // The options of the two dropdowns: the other relations of this KB. **It is not in its own list**, for both: a sub-property pointing at itself
  // is refused by the database directly (that would be a cycle); an inverse pointing at itself amounts to symmetric, and the checkbox for that is above.
  // The only exception is **when the current value is itself**: an OWL import can produce this, and leaving it out of the list
  // would make the dropdown show blank, and a blank saved would wipe out what was declared
  const linkOptions = (current: string) => [
    { value: "", label: S.ontology.noLink },
    ...(existing && current === existing.id
      ? [{ value: existing.id, label: existing.label, hint: existing.key }]
      : []),
    ...allRelations
      .filter((r) => r.id !== existing?.id)
      .map((r) => ({ value: r.id, label: r.label, hint: r.key })),
  ];
  /** The display name of the entry selected in the dropdown. If it is not found, fall back to the id: ugly is better than blank */
  const nameOf = (id: string) =>
    allRelations.find((r) => r.id === id)?.label ?? id;

  const save = useMutation({
    mutationFn: async (): Promise<unknown> => {
      const body = {
        label,
        temporal,
        functional,
        inverse_functional: inverseFunctional,
        is_transitive: transitive,
        is_symmetric: symmetric,
        is_asymmetric: asymmetric,
        is_irreflexive: irreflexive,
        // An empty string has to become null before it is sent: the server accepts `Option<Uuid>`,
        // and `""` does not parse as a UUID, giving a 422 rather than "cleared"
        inverse_of: inverseOf || null,
        sub_property_of: subPropertyOf || null,
        description,
        domains,
        ranges,
        qualifiers,
      };
      return existing
        ? api.updateRelationType(kbId, existing.id, body)
        : api.createRelationType(kbId, { key, ...body });
    },
    onSuccess: (res) => {
      toast.success(existing ? S.toast.saved : S.toast.created);
      onSaved(existing ? undefined : (res as { id?: string })?.id);
    },
    onError,
  });
  const remove = useMutation({
    mutationFn: () => api.deleteRelationType(kbId, existing!.id),
    onSuccess: () => {
      toast.success(S.toast.deleted);
      onDeleted();
    },
    onError,
  });

  return (
    <FormDialog
      width="lg"
      title={existing ? S.ontology.editProperty : S.ontology.newProperty}
      description={
        existing ? `${existing.key} · ${S.ontology.usage(existing.usage)}` : undefined
      }
      closeLabel={S.ui.close}
      saveLabel={S.ontology.save}
      cancelLabel={S.ontology.cancel}
      canSave={!!label.trim() && (!!existing || !!key.trim())}
      busy={save.isPending}
      onSave={() => save.mutate()}
      onCancel={onClose}
      danger={
        existing && !existing.builtin
          ? {
              label: S.ontology.delete,
              disabled: existing.usage > 0 || remove.isPending,
              title: existing.usage > 0 ? S.ontology.deleteBlocked : undefined,
              onClick: () => remove.mutate(),
            }
          : undefined
      }
    >
      {!existing && (
        <Field
          label={
            <>
              {S.ontology.key}{" "}
              <span className="text-ink-2">({S.ontology.keyHint})</span>
            </>
          }
        >
          <Input
            autoFocus
            value={key}
            onChange={(e) => setKey(e.target.value)}
            className="w-full"
            placeholder="signed_with"
          />
        </Field>
      )}
      <Field label={S.ontology.label}>
        <Input
          autoFocus={!!existing}
          value={label}
          onChange={(e) => setLabel(e.target.value)}
          className="w-full"
        />
      </Field>
      {/* The type signature. The interface shows labels, but what goes into the prompt is the key — that step happens on the server (0004) */}
      <Field label={S.ontology.signature} hint={S.ontology.signatureHint}>
        <div className="grid gap-2 sm:grid-cols-2">
          <div className="min-w-0">
            <div className="mb-1 text-small text-ink-2">{S.ontology.domainLabel}</div>
            <MultiSearchSelect
              values={domains}
              options={typeOpts}
              onToggle={(id) => toggleIn(setDomains, id)}
              placeholder={S.ontology.searchTypes}
              emptyHint={S.ontology.anyType}
            />
          </div>
          <div className="min-w-0">
            <div className="mb-1 text-small text-ink-2">{S.ontology.rangeLabel}</div>
            <MultiSearchSelect
              values={ranges}
              options={typeOpts}
              onToggle={(id) => toggleIn(setRanges, id)}
              placeholder={S.ontology.searchTypes}
              emptyHint={S.ontology.anyType}
            />
          </div>
          <div className="min-w-0">
            {/* Attributes on the edge (0037): the options are the attribute definitions of this KB, not classes.
                A relation cannot declare itself as its own attribute, so it is not listed in the list either */}
            <div className="mb-1 text-small text-ink-2">{S.ontology.qualifiers}</div>
            <MultiSearchSelect
              values={qualifiers}
              options={allAttributes
                .filter((r) => r.id !== existing?.id)
                .map((r) => ({ value: r.id, label: r.label, indent: 0 }))}
              onToggle={(id) => toggleIn(setQualifiers, id)}
              placeholder={S.ontology.searchTypes}
              emptyHint={S.ontology.noQualifiers}
            />
          </div>
        </div>
      </Field>
      <Field label={S.ontology.temporal}>
        <Dropdown
          value={temporal}
          onChange={setTemporal}
          className="w-full"
          options={[
            { value: "state", label: S.ontology.temporalState },
            { value: "event", label: S.ontology.temporalEvent },
            { value: "eternal", label: S.ontology.temporalEternal },
          ]}
        />
      </Field>
      {/* The six axioms are grouped together. **They are in fact one family**: the reasoner (0002) uses them as criteria.
          Under each one, write clearly "what happens if ticked": these switches are not descriptions, they are declarations that change system behaviour.
          Ticking symmetric and asymmetric together is self-contradictory (only true for an empty relation), so say so on the spot */}
      <Field
        label={S.ontology.axioms}
        hint={S.ontology.axiomsHint}
        error={symmetric && asymmetric ? S.ontology.axiomConflict : undefined}
      >
        <div className="space-y-2">
          {(
            [
              [functional, setFunctional, S.ontology.functional, S.ontology.functionalHint],
              [
                inverseFunctional,
                setInverseFunctional,
                S.ontology.inverseFunctional,
                S.ontology.inverseFunctionalHint,
              ],
              [transitive, setTransitive, S.ontology.transitive, S.ontology.transitiveHint],
              [symmetric, setSymmetric, S.ontology.symmetric, S.ontology.symmetricHint],
              [asymmetric, setAsymmetric, S.ontology.asymmetric, S.ontology.asymmetricHint],
              [
                irreflexive,
                setIrreflexive,
                S.ontology.irreflexive,
                S.ontology.irreflexiveHint,
              ],
            ] as const
          ).map(([on, set, title, hint], i) => (
            <Checkbox
              key={i}
              checked={on}
              onChange={(v) => set(v)}
              label={title}
              hint={hint}
            />
          ))}
        </div>
      </Field>
      {/* The last two of the same group differ only in shape: they point at **another relation**, so they are dropdowns rather than checkboxes.
          Once one is chosen, say the whole thing on the spot — **the facts inferred by these two do not necessarily run the same direction**: the inverse has to swap subject and object,
          whereas the sub-property does not swap; this cannot be told from the name alone, but once written out it can */}
      <div className="grid gap-2 sm:grid-cols-2">
        <Field label={S.ontology.inverseOf} hint={S.ontology.inverseOfHint}>
          <SearchSelect
            value={inverseOf}
            onChange={setInverseOf}
            options={linkOptions(inverseOf)}
            size="sm"
            className="w-full"
            placeholder={S.ontology.noLink}
          />
        </Field>
        <Field label={S.ontology.subPropertyOf} hint={S.ontology.subPropertyOfHint}>
          <SearchSelect
            value={subPropertyOf}
            onChange={setSubPropertyOf}
            options={linkOptions(subPropertyOf)}
            size="sm"
            className="w-full"
            placeholder={S.ontology.noLink}
          />
        </Field>
      </div>
      {(inverseOf || subPropertyOf) && (
        <div className="mb-4 -mt-2 space-y-1 text-fine leading-relaxed text-ink-2">
          {inverseOf && (
            <div>
              {S.ontology.linkMeansInverse(label.trim() || key || "?", nameOf(inverseOf))}
            </div>
          )}
          {subPropertyOf && (
            <div>
              {S.ontology.linkMeansSuper(label.trim() || key || "?", nameOf(subPropertyOf))}
            </div>
          )}
        </div>
      )}
      <Field
        label={S.ontology.description}
        hint={S.ontology.descriptionHint}
        className="mb-0"
      >
        <Textarea
          className="w-full min-h-28 resize-y"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
        />
      </Field>
    </FormDialog>
  );
}

/* ---------- Attributes (literal-value fields of a class) ---------- */

export function AttributeDialog({
  kbId,
  typeId,
  allTypes,
  existing,
  onClose,
  onSaved,
  onDeleted,
  onError,
}: {
  kbId: string;
  /** null = not opened from a class (the attribute page of the ontology table): the dialog itself chooses which class it hangs on */
  typeId: string | null;
  allTypes: EntityTypeView[];
  existing: RelationTypeView | null;
  onClose: () => void;
  onSaved: () => void;
  onDeleted: () => void;
  onError: (e: unknown) => void;
}) {
  const [key, setKey] = useState(existing?.key ?? "");
  const [label, setLabel] = useState(existing?.label ?? "");
  const [datatype, setDatatype] = useState(existing?.datatype ?? "text");
  const [unit, setUnit] = useState(existing?.unit ?? "");
  // Single-valued = functional: a new value closes the old one through the temporal engine (the source of attribute history). Most attributes are like this, default on
  const [single, setSingle] = useState(existing?.functional ?? true);
  const [description, setDescription] = useState(existing?.description ?? "");
  const [host, setHost] = useState(typeId ?? "");
  const hostOpts = useMemo(() => parentOptions(allTypes, undefined), [allTypes]);

  const save = useMutation({
    mutationFn: async (): Promise<unknown> =>
      existing
        ? api.updateRelationType(kbId, existing.id, {
            label,
            temporal: existing.temporal,
            functional: single,
            inverse_functional: false,
            description,
            datatype,
            unit,
          })
        : api.createRelationType(kbId, {
            key,
            label,
            kind: "attribute",
            domains: [host],
            temporal: "state",
            functional: single,
            inverse_functional: false,
            description,
            datatype,
            unit,
          }),
    onSuccess: () => {
      toast.success(existing ? S.toast.saved : S.toast.created);
      onSaved();
    },
    onError,
  });
  const remove = useMutation({
    mutationFn: () => api.deleteRelationType(kbId, existing!.id),
    onSuccess: () => {
      toast.success(S.toast.deleted);
      onDeleted();
    },
    onError,
  });

  return (
    <FormDialog
      title={existing ? S.ontology.editAttribute : S.ontology.newAttribute}
      description={
        existing ? `${existing.key} · ${S.ontology.usage(existing.usage)}` : undefined
      }
      closeLabel={S.ui.close}
      saveLabel={S.ontology.save}
      cancelLabel={S.ontology.cancel}
      canSave={!!label.trim() && (!!existing || (!!key.trim() && !!host))}
      busy={save.isPending}
      onSave={() => save.mutate()}
      onCancel={onClose}
      danger={
        existing
          ? {
              label: S.ontology.delete,
              disabled: existing.usage > 0 || remove.isPending,
              title: existing.usage > 0 ? S.ontology.deleteBlocked : undefined,
              onClick: () => remove.mutate(),
            }
          : undefined
      }
    >
      {!existing && typeId === null && (
        <Field label={S.ontology.attributeHost}>
          <SearchSelect
            value={host}
            onChange={setHost}
            options={hostOpts}
            className="w-full"
            placeholder={S.ontology.searchTypes}
          />
        </Field>
      )}
      {existing ? (
        <Field label={S.ontology.label}>
          <Input
            autoFocus
            value={label}
            onChange={(e) => setLabel(e.target.value)}
            className="w-full"
          />
        </Field>
      ) : (
        <div className="grid grid-cols-2 gap-4">
          <Field label={S.ontology.key}>
            <Input
              autoFocus
              value={key}
              onChange={(e) => setKey(e.target.value)}
              className="w-full"
              placeholder="salary"
            />
          </Field>
          <Field label={S.ontology.label}>
            <Input
              value={label}
              onChange={(e) => setLabel(e.target.value)}
              className="w-full"
            />
          </Field>
        </div>
      )}
      <div className="grid grid-cols-2 gap-4">
        <Field label={S.ontology.attrDatatype}>
          <Dropdown
            value={datatype}
            onChange={(v) => setDatatype(v as typeof datatype)}
            className="w-full"
            options={(["text", "number", "date", "bool"] as const).map((d) => ({
              value: d,
              label: S.ontology.datatypeNames[d],
            }))}
          />
        </Field>
        <Field
          label={
            <>
              {S.ontology.attrUnit}{" "}
              <span className="text-ink-2">({S.ontology.attrUnitHint})</span>
            </>
          }
        >
          <Input
            value={unit}
            onChange={(e) => setUnit(e.target.value)}
            className="w-full"
          />
        </Field>
      </div>
      <div className="mb-4">
        <Checkbox
          checked={single}
          onChange={(v) => setSingle(v)}
          label={S.ontology.attrSingle}
        />
      </div>
      <Field label={S.ontology.description} className="mb-0">
        <Textarea
          className="w-full min-h-24 resize-y"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
        />
      </Field>
    </FormDialog>
  );
}

/* ---------- Connect this class with an existing relation ---------- */

export function ConnectDialog({
  kbId,
  cls,
  relations,
  onClose,
  onConnected,
  onError,
}: {
  kbId: string;
  cls: EntityTypeView;
  /** Those with kind === "relation" */
  relations: RelationTypeView[];
  onClose: () => void;
  /** After connecting, carries the id of that relation: the caller jumps there, and whether the domain/range was changed is visible at a glance */
  onConnected: (relationId: string) => void;
  onError: (e: unknown) => void;
}) {
  const [relationId, setRelationId] = useState("");
  const [side, setSide] = useState<"domain" | "range">("domain");
  const connect = useMutation({
    mutationFn: () => {
      const rel = relations.find((r) => r.id === relationId);
      if (!rel) return Promise.reject(new Error("relation not found"));
      // Only add, never remove: the side already connected is kept as is, and only the other side may be appended.
      // Set de-duplicates — picking a relation that is already connected is a harmless no-op
      const domains =
        side === "domain" ? [...new Set([...rel.domains, cls.id])] : rel.domains;
      const ranges =
        side === "range" ? [...new Set([...rel.ranges, cls.id])] : rel.ranges;
      return api.updateRelationType(kbId, rel.id, {
        label: rel.label,
        temporal: rel.temporal,
        functional: rel.functional,
        inverse_functional: rel.inverse_functional,
        is_transitive: rel.is_transitive,
        is_symmetric: rel.is_symmetric,
        is_asymmetric: rel.is_asymmetric,
        is_irreflexive: rel.is_irreflexive,
        inverse_of: rel.inverse_of,
        sub_property_of: rel.sub_property_of,
        description: rel.description,
        domains,
        ranges,
      });
    },
    onSuccess: () => {
      const rel = relations.find((r) => r.id === relationId);
      toast.success(S.ontology.schemaConnected(rel?.label ?? ""));
      onConnected(relationId);
    },
    onError,
  });
  return (
    <FormDialog
      width="sm"
      title={S.ontology.connectTitle}
      description={S.ontology.schemaConnectHint}
      closeLabel={S.ui.close}
      saveLabel={S.ontology.schemaConnect}
      cancelLabel={S.ontology.cancel}
      canSave={!!relationId}
      busy={connect.isPending}
      onSave={() => connect.mutate()}
      onCancel={onClose}
    >
      <Field label={S.ontology.tabProperties}>
        <SearchSelect
          value={relationId}
          onChange={setRelationId}
          options={relations.map((r) => ({ value: r.id, label: r.label, hint: r.key }))}
          className="w-full"
          placeholder={S.ontology.schemaConnectPlaceholder}
        />
      </Field>
      <Field label={`${S.ontology.schemaConnectAs} ${cls.label}`} className="mb-0">
        <Segmented
          size="sm"
          value={side}
          onChange={setSide}
          options={[
            { value: "domain", label: S.ontology.domainLabel },
            { value: "range", label: S.ontology.rangeLabel },
          ]}
        />
      </Field>
    </FormDialog>
  );
}
