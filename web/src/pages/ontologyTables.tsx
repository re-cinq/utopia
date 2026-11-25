// The ontology read as three tables (#498).
//
// Previously this page only spoke about one class at a time: a column of names on the left, a graph on the canvas, and a panel on the right describing the one you clicked.
// Questions that span the whole ontology had nowhere to be asked: which properties are declared unique, after importing nine hundred classes
// which have not a single instance, which relations have no domain written at all (so extraction cannot get their signature),
// and which properties this library actually has. The last one especially: properties only appear in one class's panel, one class at a time,
// so "which properties does this library have" was not answered anywhere in the interface.
//
// The other kinds of page of the same sort this month have all become tables already: tokens, data sources, members, deployment users, business rules
// (the phrase "a column of cards looks nothing like anything we just rebuilt" from #482). The ontology is the last one,
// and also the one with the most columns.
//
// **Keys are not listed**: they are machine identities (`accepted_answer`), not things a person reads. The three tables
// are for looking across the library — which classes have no instances, which properties have no domain — and the key in every row
// would only restate the name. When you need to see it (when working against a vocabulary, or writing rules), it is in the right-hand detail,
// where one thing at a time is shown, so it reads completely.
//
// **What about the hierarchy**: `subClassOf` is the skeleton of the ontology, and flattening it loses it. So by default the rows sort by hierarchy,
// with the name column indented; sorting by another column flattens it, while the parent column stays there, so flattening loses no information, it only
// changes the way of reading. That is what file managers do.

import { useMemo, useState } from "react";
import { Search, Plus } from "lucide-react";

import type { EntityTypeView, RelationTypeView } from "../api";

import { S } from "../i18n";
import {
  Button,
  Chip,
  cn,
  GroupLabel,
  Input,
  LinkButton,
  Pager,
  pageSlice,
  SkeletonTableRows,
  ROW_TRAILING,
  Segmented,
  Table,
  TBody,
  Td,
  Th,
  THead,
  Tr,
  chipLike,} from "../ui";

export type TableTab = "classes" | "properties" | "attributes";

/** How many rows to draw per page.
 *
 * The three tables have 916 / 1102 / 599 rows respectively. Previously all of them were drawn into the DOM at once — **the scrollbar could feel it**,
 * and what anyone was really looking at was always only the dozen or so rows on screen. The class tree in the left column was already paged
 * (14 per page); this just brings the same approach here. 50 is larger than 14 because a full-width table has room on one screen for more,
 * and paging too often would be annoying anyway.
 *
 * The class table pages over the **already flattened tree**, so each page is still a contiguous stretch in tree order, and parent/child relations
 * are not split across two pages (the same approach as the left column). */
const TABLE_PAGE = 50;

/** Which parent this row hangs under. **With no primary parent, fall back to the first parent**: `primary_parent` is only written when
 *  a person sets it by hand, and imported packs have none at all — zero of schema.org's 916 classes have it.
 *  Relying only on it would make every imported ontology a single flat strip in the tree, with the hierarchy entirely invisible.
 *
 *  **A class can have several parents** (`parents` is an array; 48 of schema.org's 916 are like this,
 *  and FOAF's Person is both an Agent and a SpatialThing). A row can only hang in one place,
 *  so the indentation picks one of them: the column lists all the parents, and the row marks one with a symbol saying
 *  "there are others", so that the indentation does not pass itself off as the only truth */
export function treeParent(t: EntityTypeView): string | null {
  return t.primary_parent ?? t.parents[0] ?? null;
}

/** The indent of the name column: 12px per level. Same approach as the tree in the left column */
const INDENT = 12;

/* ---------- Sorting ---------- */

type Sort = { col: string; dir: "asc" | "desc" } | null;

/** Sortable column header. **Sorting flattens the hierarchy** (see the file header) — so the name column is not sortable, since it is
 *  the hierarchy itself; clicking other columns is what enters the "flat" way of reading */
function SortTh({
  col,
  sort,
  onSort,
  className,
  children,
}: {
  col: string;
  sort: Sort;
  onSort: (s: Sort) => void;
  className?: string;
  children: React.ReactNode;
}) {
  const active = sort?.col === col;
  return (
    <Th className={className}>
      <LinkButton
        className="inline-flex items-center gap-1"
        onClick={() =>
          onSort(
            active && sort.dir === "desc"
              ? null
              : { col, dir: active ? "desc" : "asc" },
          )
        }
      >
        {children}
        {active && <span className="u-num">{sort.dir === "asc" ? "↑" : "↓"}</span>}
      </LinkButton>
    </Th>
  );
}

function sorted<T>(rows: T[], sort: Sort, key: (r: T) => string | number) {
  if (!sort) return rows;
  const out = [...rows].sort((a, b) => {
    const x = key(a);
    const y = key(b);
    return typeof x === "number" && typeof y === "number"
      ? x - y
      : String(x).localeCompare(String(y));
  });
  return sort.dir === "asc" ? out : out.reverse();
}

/* ---------- Classes ---------- */

/** Arranged into hierarchy order by primary_parent, with each row's depth. Flattened when filtering or sorting */
function classRows(
  types: EntityTypeView[],
  flat: boolean,
): { t: EntityTypeView; depth: number }[] {
  if (flat) return types.map((t) => ({ t, depth: 0 }));
  const children = new Map<string | null, EntityTypeView[]>();
  for (const t of types) {
    const p = treeParent(t);
    if (!children.has(p)) children.set(p, []);
    children.get(p)!.push(t);
  }
  const out: { t: EntityTypeView; depth: number }[] = [];
  const seen = new Set<string>();
  const walk = (parent: string | null, depth: number) => {
    for (const t of children.get(parent) ?? []) {
      if (seen.has(t.id)) continue; // Even parents that point at each other in a cycle cannot make this loop forever
      seen.add(t.id);
      out.push({ t, depth });
      walk(t.id, depth + 1);
    }
  };
  walk(null, 0);
  // A parent that is not in this list (it was filtered out) cannot be attached to the tree, so it is appended at the end — it must not be left out of the display
  // just because it cannot be placed in the hierarchy
  for (const t of types) if (!seen.has(t.id)) out.push({ t, depth: 0 });
  return out;
}

function ClassesTable({
  types,
  attributes,
  filter,
  selectedId,
  onOpen,
  onSeeInstances,
}: {
  types: EntityTypeView[];
  attributes: RelationTypeView[];
  filter: string;
  selectedId: string | null;
  onOpen: (t: EntityTypeView) => void;
  onSeeInstances: (t: EntityTypeView) => void;
}) {
  const [sort, setSort] = useState<Sort>(null);
  const [page, setPage] = useState(0);
  const nameOf = useMemo(
    () => new Map(types.map((t) => [t.id, t.label])),
    [types],
  );
  const attrCount = useMemo(() => {
    const m = new Map<string, number>();
    for (const a of attributes)
      for (const d of a.domains) m.set(d, (m.get(d) ?? 0) + 1);
    return m;
  }, [attributes]);

  const q = filter.trim().toLowerCase();
  const hit = (t: EntityTypeView) =>
    !q || t.label.toLowerCase().includes(q) || t.key.toLowerCase().includes(q);
  const shown = types.filter(hit);
  // Flattened whenever filtering or sorting: in both cases the hierarchy is no longer the order of this list
  const rows = classRows(sorted(shown, sort, (t) =>
    sort?.col === "usage" ? t.usage
    : sort?.col === "attrs" ? (attrCount.get(t.id) ?? 0)
    : sort?.col === "parent" ? (treeParent(t) ? (nameOf.get(treeParent(t)!) ?? "") : "")
    : t.label,
  ), !!sort || !!q);

  const { rows: paged, safe } = pageSlice(rows, page, TABLE_PAGE);

  return (
    <>
      <Table>
      <THead>
        <Tr>
          <Th>{S.ontology.colName}</Th>
          <SortTh col="parent" sort={sort} onSort={setSort}>
            {S.ontology.parent}
          </SortTh>
          <Th className="hidden lg:table-cell">{S.ontology.disjoint}</Th>
          <SortTh col="usage" sort={sort} onSort={setSort} className="text-right">
            {S.ontology.colInstances}
          </SortTh>
          <SortTh col="attrs" sort={sort} onSort={setSort} className="text-right">
            {S.ontology.schemaTabAttributes}
          </SortTh>
        </Tr>
      </THead>
      <TBody>
        {paged.map(({ t, depth }) => (
          <Tr
            key={t.id}
            interactive
            active={t.id === selectedId}
            onClick={() => onOpen(t)}
          >
            <Td>
              <span
                className="flex items-center gap-2"
                style={{ paddingLeft: depth * INDENT }}
              >
                <span
                  className={cn(
                    "h-2 w-2 shrink-0",
                    t.shape === "square" ? "scale-90" : "rounded-full",
                  )}
                  style={{ background: t.color }}
                />
                <span className="truncate">{t.label}</span>
                {/* Multiple parents: indentation can only hang in one place; this mark says how many others there are, and the parent column
                    lists them all */}
                {t.parents.length > 1 && (
                  <span
                    className={chipLike("neutral", "shrink-0 px-2 text-fine")}
                    title={S.ontology.multiParentHint}
                  >
                    +{t.parents.length - 1}
                  </span>
                )}
                {t.builtin && <Chip tone="neutral">{S.ontology.builtin}</Chip>}
              </span>
            </Td>
            <Td className="text-small text-ink-2">
              {t.parents.length
                ? t.parents.map((p) => nameOf.get(p) ?? p).join(", ")
                : "—"}
            </Td>
            <Td className="hidden text-small text-ink-2 lg:table-cell">
              {t.disjoint.length
                ? t.disjoint.map((d) => nameOf.get(d) ?? d).join(", ")
                : "—"}
            </Td>
            <Td className="text-right">
              {t.usage > 0 ? (
                <LinkButton
                  className="u-num"
                  onClick={(e) => {
                    e.stopPropagation();
                    onSeeInstances(t);
                  }}
                >
                  {t.usage}
                </LinkButton>
              ) : (
                <span className="u-num text-ink-2">0</span>
              )}
            </Td>
            <Td className="u-num text-right text-ink-2">
              {attrCount.get(t.id) ?? 0}
            </Td>
          </Tr>
        ))}
      </TBody>
      </Table>
      {/* The pager follows after the table; it is not a fixed bottom bar, since this block is already the scroll area,
          and pinning a bar would cut off half of the last row */}
      <Pager
        total={rows.length}
        pageSize={TABLE_PAGE}
        page={safe}
        onPage={setPage}
      />
    </>
  );
}

/* ---------- Relations ---------- */

/** Axioms are shown as a few letter chips rather than six ticks: six checkboxes in one row cannot convey "what this relation
 *  guarantees", whereas a few letters can be scanned down a column */
function Axioms({ r }: { r: RelationTypeView }) {
  const on: [boolean, string, string][] = [
    [r.functional, "F", S.ontology.functional],
    [r.inverse_functional, "IF", S.ontology.inverseFunctional],
    [r.is_transitive, "T", S.ontology.axiomTransitive],
    [r.is_symmetric, "S", S.ontology.axiomSymmetric],
    [r.is_asymmetric, "A", S.ontology.axiomAsymmetric],
    [r.is_irreflexive, "IR", S.ontology.axiomIrreflexive],
  ];
  const shown = on.filter(([v]) => v);
  if (shown.length === 0) return <span className="text-ink-2">—</span>;
  return (
    <span className="flex flex-wrap gap-1">
      {shown.map(([, mark, title]) => (
        <span key={mark} className={chipLike("neutral", "px-2 text-fine")} title={title}>
          {mark}
        </span>
      ))}
    </span>
  );
}

function PropertiesTable({
  relations,
  types,
  filter,
  selectedId,
  onOpen,
}: {
  relations: RelationTypeView[];
  types: EntityTypeView[];
  filter: string;
  selectedId: string | null;
  onOpen: (r: RelationTypeView) => void;
}) {
  const [sort, setSort] = useState<Sort>(null);
  const [page, setPage] = useState(0);
  const nameOf = useMemo(
    () => new Map(types.map((t) => [t.id, t.label])),
    [types],
  );
  const relName = useMemo(
    () => new Map(relations.map((r) => [r.id, r.label])),
    [relations],
  );
  const q = filter.trim().toLowerCase();
  const shown = relations.filter(
    (r) =>
      !q || r.label.toLowerCase().includes(q) || r.key.toLowerCase().includes(q),
  );
  const side = (ids: string[]) =>
    ids.length ? ids.map((i) => nameOf.get(i) ?? i).join(", ") : S.ontology.anyType;
  const rows = sorted(shown, sort, (r) =>
    sort?.col === "usage" ? r.usage
    : sort?.col === "temporal" ? r.temporal
    : r.label,
  );

  const { rows: paged, safe } = pageSlice(rows, page, TABLE_PAGE);

  return (
    <>
      <Table>
      <THead>
        <Tr>
          <Th>{S.ontology.colName}</Th>
          <Th>{S.ontology.colSignature}</Th>
          <SortTh col="temporal" sort={sort} onSort={setSort}>
            {S.ontology.temporal}
          </SortTh>
          <Th>{S.ontology.axioms}</Th>
          <Th className="hidden lg:table-cell">{S.ontology.inverseOf}</Th>
          <SortTh col="usage" sort={sort} onSort={setSort} className="text-right">
            {S.ontology.colFacts}
          </SortTh>
        </Tr>
      </THead>
      <TBody>
        {paged.map((r) => (
          <Tr
            key={r.id}
            interactive
            active={r.id === selectedId}
            onClick={() => onOpen(r)}
          >
            <Td>
              <span className="flex items-center gap-2">
                <span className="truncate">{r.label}</span>
                {r.builtin && <Chip tone="neutral">{S.ontology.builtin}</Chip>}
              </span>
            </Td>
            <Td className="text-small text-ink-2">
              {side(r.domains)} → {side(r.ranges)}
            </Td>
            <Td className="text-small text-ink-2">
              {r.temporal === "event"
                ? S.ontology.temporalEvent
                : r.temporal === "eternal"
                  ? S.ontology.temporalEternal
                  : S.ontology.temporalState}
            </Td>
            <Td>
              <Axioms r={r} />
            </Td>
            <Td className="hidden text-small text-ink-2 lg:table-cell">
              {r.inverse_of ? (relName.get(r.inverse_of) ?? "—") : "—"}
            </Td>
            <Td className="u-num text-right text-ink-2">{r.usage}</Td>
          </Tr>
        ))}
      </TBody>
      </Table>
      {/* The pager follows after the table; it is not a fixed bottom bar, since this block is already the scroll area,
          and pinning a bar would cut off half of the last row */}
      <Pager
        total={rows.length}
        pageSize={TABLE_PAGE}
        page={safe}
        onPage={setPage}
      />
    </>
  );
}

/* ---------- Properties ---------- */

/** Properties get a table of their own, the most telling of the three: previously properties only surfaced in one class's panel,
 *  one class at a time, so "which properties this library has" was not answered anywhere in the interface */
function AttributesTable({
  attributes,
  types,
  filter,
  onOpen,
}: {
  attributes: RelationTypeView[];
  types: EntityTypeView[];
  filter: string;
  onOpen: (a: RelationTypeView) => void;
}) {
  const [sort, setSort] = useState<Sort>(null);
  const [page, setPage] = useState(0);
  const nameOf = useMemo(
    () => new Map(types.map((t) => [t.id, t.label])),
    [types],
  );
  const q = filter.trim().toLowerCase();
  const shown = attributes.filter(
    (a) =>
      !q || a.label.toLowerCase().includes(q) || a.key.toLowerCase().includes(q),
  );
  const rows = sorted(shown, sort, (a) =>
    sort?.col === "usage" ? a.usage
    : sort?.col === "datatype" ? (a.datatype ?? "")
    : sort?.col === "domain" ? (a.domains[0] ? (nameOf.get(a.domains[0]) ?? "") : "")
    : a.label,
  );

  const { rows: paged, safe } = pageSlice(rows, page, TABLE_PAGE);

  return (
    <>
      <Table>
      <THead>
        <Tr>
          <Th>{S.ontology.colName}</Th>
          <SortTh col="domain" sort={sort} onSort={setSort}>
            {S.ontology.colOnClass}
          </SortTh>
          <SortTh col="datatype" sort={sort} onSort={setSort}>
            {S.ontology.colDatatype}
          </SortTh>
          <Th>{S.ontology.colUnit}</Th>
          <Th>{S.ontology.colSingleValued}</Th>
          <SortTh col="usage" sort={sort} onSort={setSort} className="text-right">
            {S.ontology.colFacts}
          </SortTh>
        </Tr>
      </THead>
      <TBody>
        {paged.map((a) => (
          <Tr key={a.id} interactive onClick={() => onOpen(a)}>
            <Td className="truncate">{a.label}</Td>
            <Td className="text-small text-ink-2">
              {a.domains.length
                ? a.domains.map((d) => nameOf.get(d) ?? d).join(", ")
                : "—"}
            </Td>
            <Td className="text-small text-ink-2">{a.datatype ?? "—"}</Td>
            <Td className="text-small text-ink-2">{a.unit || "—"}</Td>
            <Td className="text-small text-ink-2">
              {a.functional ? S.ontology.yes : "—"}
            </Td>
            <Td className="u-num text-right text-ink-2">{a.usage}</Td>
          </Tr>
        ))}
      </TBody>
      </Table>
      {/* The pager follows after the table; it is not a fixed bottom bar, since this block is already the scroll area,
          and pinning a bar would cut off half of the last row */}
      <Pager
        total={rows.length}
        pageSize={TABLE_PAGE}
        page={safe}
        onPage={setPage}
      />
    </>
  );
}

/* ---------- The three tables ---------- */

export function OntologyTables({
  entityTypes,
  relationTypes,
  onOpenClass,
  onOpenProperty,
  onOpenAttribute,
  onSeeInstances,
  onCreateClass,
  onCreateProperty,
  onCreateAttribute,
  selected,
  loading = false,
}: {
  entityTypes: EntityTypeView[];
  relationTypes: RelationTypeView[];
  onOpenClass: (t: EntityTypeView) => void;
  onOpenProperty: (r: RelationTypeView) => void;
  onOpenAttribute: (a: RelationTypeView) => void;
  onSeeInstances: (t: EntityTypeView) => void;
  /** Create a top-level class. The "New class" row in the left column only expands in the graph tier, so the table needs its own route */
  onCreateClass: () => void;
  /** Create a relation without a domain, likewise */
  onCreateProperty: () => void;
  /** A property must hang on a class: when created from here, the dialog itself asks which class it hangs on */
  onCreateAttribute: () => void;
  /** The currently selected class or property. The table must mark it — **clicking a row opens the panel on the right,
   *  but if the row itself leaves no trace, after paging two pages you no longer know which one you are looking at**.
   *  Only these two tiers are collected: the page's `Sel` also includes import / rules / schema and a few others,
   *  which have no correspondence to the rows in the table, so narrowing is more honest than passing the whole thing down */
  selected:
    | { kind: "class"; id: string }
    | { kind: "relation"; id: string }
    | null;
  /** The ontology has not arrived yet. Controls render as usual and the table body shows a skeleton, **counts and row numbers are never reported** —
   *  at this point they would only be 0, and 0 would be a conclusion, not "not known yet" */
  loading?: boolean;
}) {
  const [tab, setTab] = useState<TableTab>("classes");
  const [filter, setFilter] = useState("");
  const relations = relationTypes.filter((r) => r.kind === "relation");
  const attributes = relationTypes.filter((r) => r.kind === "attribute");
  const counts: Record<TableTab, number> = {
    classes: entityTypes.length,
    properties: relations.length,
    attributes: attributes.length,
  };

  return (
    <div className="flex h-full min-w-0 flex-col">
      {/* Controls that act on the table's contents sit **outside** the table (rule 6): once a filter clears, the table turns into an empty state,
          and putting the filter box inside would hide the only thing that can change it back as well */}
      <div className="flex shrink-0 items-center gap-4 px-8 pt-6 pb-3">
        <Segmented<TableTab>
          value={tab}
          onChange={setTab}
          options={(["classes", "properties", "attributes"] as const).map(
            (v) => ({
              value: v,
              label:
                v === "classes"
                  ? S.ontology.tabClasses
                  : v === "properties"
                    ? S.ontology.schemaTabRelations
                    : S.ontology.schemaTabAttributes,
              count: loading ? undefined : counts[v],
            }),
          )}
        />
        <Input
          // Previously this was a `/`, which by convention is a "press slash to focus" hint — **but this page does not bind
          // that key (only the docs page does), so it hinted at a feature that does not exist. And the left-column filter box
          // uses a magnifier, which is the same on this page — two icons would make no sense
          icon={<Search size={12} />}
          className="w-58"
          placeholder={S.ontology.filter}
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        <GroupLabel className={cn(ROW_TRAILING, "shrink-0")}>
          {loading
            ? null
            : S.ontology.rowsShown(
                tab === "classes"
                  ? counts.classes
                  : tab === "properties"
                    ? counts.properties
                    : counts.attributes,
              )}
        </GroupLabel>
        {/* Creating follows the current tier (the same thing in the graph tier is the first row of the left column). A property has to hang on
            a class: when opened from here the dialog asks which class it hangs on; when opened from a class's panel it is that class */}
        <Button
          variant="primary"
          size="sm"
          icon={<Plus size={12} />}
          // No creating until the ontology has arrived: a new class has to pick a parent, and the parent list is empty right now
          disabled={loading}
          onClick={
            tab === "classes"
              ? onCreateClass
              : tab === "properties"
                ? onCreateProperty
                : onCreateAttribute
          }
        >
          {tab === "classes"
            ? S.ontology.newClass
            : tab === "properties"
              ? S.ontology.newProperty
              : S.ontology.newAttribute}
        </Button>
      </div>
      <div className="u-scroll min-h-0 flex-1 overflow-y-auto px-8 pb-6">
        {/* The table body shows a skeleton, but **the header is not drawn**: what the columns are is not yet decided (the three tables have different columns),
            and drawing a row of grey bars to pass as column names would be making things up. The segmented control, filter box and scroll area are already there */}
        {loading && <SkeletonTableRows rows={10} />}
        {!loading && tab === "classes" && (
          <ClassesTable
            types={entityTypes}
            attributes={attributes}
            filter={filter}
            selectedId={
              selected?.kind === "class" ? selected.id : null
            }
            onOpen={onOpenClass}
            onSeeInstances={onSeeInstances}
          />
        )}
        {!loading && tab === "properties" && (
          <PropertiesTable
            relations={relations}
            types={entityTypes}
            filter={filter}
            selectedId={
              selected?.kind === "relation" ? selected.id : null
            }
            onOpen={onOpenProperty}
          />
        )}
        {!loading && tab === "attributes" && (
          <AttributesTable
            attributes={attributes}
            types={entityTypes}
            filter={filter}
            onOpen={onOpenAttribute}
          />
        )}
      </div>
    </div>
  );
}
