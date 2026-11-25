// Ontology editor: master-detail two columns (same structure as SourcesRail in Library).
// Left column = filter + two sub-sections, Classes / Properties, plus the Unmatched entry at the bottom;
// Right side = detail panel for the selected item (display only; create, edit, delete and connect all happen in dialogs, see ontologyDialogs)
// / Unmatched-signal panel / overview.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import {
  ArrowLeft,
  ArrowLeftRight,
  ArrowRight,
  CheckCheck,
  ChevronRight,
  Inbox,
  Link2,
  Network,
  Pencil,
  Plus,
  Scale,
  Search,
  Split,
  Table as TableIcon,
  Upload,
  Wand2,
  X,
} from "lucide-react";
import {
  api,
  type EntityTypeView,
  type ImportPlan,
  type OntologyImportView,
  type AgentProposalFields,
  type CompetencyQuestion,
  type PlannedItem,
  type OntologyProposals,
  type ResolutionOutcome,
  type TypeSuggestion,
  type RelationTypeView,
  type UniquenessCandidate,
} from "../api";
import { OntologyTables, treeParent } from "./ontologyTables";
import { S } from "../i18n";
import { useKb } from "../kb";
import { toast } from "../toast";
import { OntologySchemaGraph, type SchemaSelection } from "./OntologySchemaGraph";
import { RulesPanel } from "./RulesPanel";
import { OntologyRegressionsPanel } from "./OntologyRegressionsPanel";
import {
  AttributeDialog,
  ClassDialog,
  ConnectDialog,
  PropertyDialog,
} from "./ontologyDialogs";
import {
  Button,
  Chip,
  Dialog,
  Disclosure,
  IconButton,
  Input,
  LinkButton,
  Loading,
  PageHeader,
  Pager,
  RAIL_CLS,
  ROW_TRAILING,
  RailItem,
  Row,
  Segmented,
  SkeletonRows,
  Spinner,
  Status,
  TBody,
  THead,
  Table,
  Td,
  Th,
  Tr,
  chipLike,
  cn,
  localDateTime,
  pageSlice,
  rowClass,
} from "../ui";

/** Left-column row height (py-2 + 13px text + space-y gap) and the reserved space at the bottom (new-row + pager) */
const RAIL_ROW_H = 34;
const RAIL_RESERVED = 80;
/** Whether last time the view was the table or the graph. **Per browser, not per KB**: this is a habit of how one reads the page,
 *  switching knowledge bases does not change how one reads it */
const VIEW_KEY = "utopia.ontology.view";
/** Fallback page row count (used when the height has not been measured on the first frame) */
const RAIL_PAGE = 14;
/** Rows per section when the two sections are mixed together in filter mode */

/** What the right-hand detail area is currently showing.
 *
 *  class / relation / schema / null share one workspace: the schema graph is always present as the
 *  background, and the detail panel docks on the right. Clicking a class name in the left column, clicking a node on the canvas, or selecting a relation with the schema graph's
 *  own search box: all three paths land on the same `sel`, and therefore on the same
 *  panel, with no need to draw it separately each time. The panel only displays; edits happen in the `Edit` dialog.
 *  import / refine / misses are still independent full-page views: those three are not about "some class
 *  or relation", and have no shared background with the schema graph. */
type Sel =
  | { kind: "class"; id: string }
  | { kind: "relation"; id: string }
  | { kind: "misses" }
  | { kind: "regressions" }
  | { kind: "uniqueness" }
  // The business-rules page. **focusId comes from clicking a rule edge on the schema graph**: that row is highlighted,
  // so there is no need to search again through a page of rules
  | { kind: "rules"; focusId?: string }
  | { kind: "refine" }
  | { kind: "import" }
  // Nothing selected in the schema graph: show the whole graph, with no form docked
  | { kind: "schema" }
  | null;

/** Whether this selection will dock a panel on the right side of the schema graph */
const onPanel = (s: Sel) => s?.kind === "class" || s?.kind === "relation";

/** The edit dialog that is open. The panel only displays; create, edit, delete and connect all happen in the dialog */
type Edit =
  | { kind: "class"; existing: EntityTypeView | null; parentId: string | null }
  | {
      kind: "property";
      existing: RelationTypeView | null;
      // Creating a relation from a class carries that class's id, with domain pre-filled to it
      initialDomain: string | null;
    }
  | { kind: "attribute"; typeId: string | null; existing: RelationTypeView | null }
  | { kind: "connect"; cls: EntityTypeView }
  | null;

export function Ontology() {
  const { kb } = useKb();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const [sel, setSel] = useState<Sel>(null);
  const [edit, setEdit] = useState<Edit>(null);
  const [railTab, setRailTab] = useState<"classes" | "properties">("classes");
  /* Whether the main area shows the graph or the table (#498). **Defaults to the graph**, and **remembers the last version**: this is
     "how I habitually read this page", not "what this link points to", so it goes into localStorage rather than the
     URL (same judgment as the KB switcher, see KbScope). In private mode reads and writes both throw,
     so catch them and fall back to the default, rather than letting the whole page crash */
  const [view, setView] = useState<"table" | "diagram">(() => {
    try {
      return localStorage.getItem(VIEW_KEY) === "table" ? "table" : "diagram";
    } catch {
      return "diagram";
    }
  });
  const switchView = (v: "table" | "diagram") => {
    setView(v);
    try {
      localStorage.setItem(VIEW_KEY, v);
    } catch {
      // If it cannot be stored, it is only remembered for this session
    }
    if (!onPanel(sel)) setSel({ kind: "schema" });
  };
  /** Import / Rules / Refine / Overlaps / Unmatched take over the whole main area */
  const inWorkflow =
    sel?.kind === "import" ||
    sel?.kind === "refine" ||
    sel?.kind === "uniqueness" ||
    sel?.kind === "rules" ||
    sel?.kind === "misses" ||
    sel?.kind === "regressions";
  // Which segment the schema graph's detail panel stopped on. **Kept across selections**: going through classes one by one on instances
  // is a real way of reading, and being bounced back to the definition page on every class change would interrupt it
  const [panelTab, setPanelTab] = useState<
    "definition" | "relations" | "attributes" | "instances"
  >("definition");
  /** The selection that is leaving: the panel plays `u-dock-out` before unmounting, rather than vanishing at once */
  const [exitingSel, setExitingSel] = useState<Sel>(null);
  // **The current selection is read from a ref, not from a closure.** The effect that registers sigma events on the schema graph
  // depends only on [schema], and `clickStage` captures the onSelect of that one render; reading sel from the closure
  // would treat a long-expired selection as "the thing that is leaving", so clicking blank space would first make the panel
  // pop up and then disappear. The `selectedRef` on the graph page is for the same reason
  const selRef = useRef<Sel>(null);
  useEffect(() => {
    selRef.current = sel;
  }, [sel]);
  const closePanel = useCallback(() => {
    const cur = selRef.current;
    if (!onPanel(cur)) return;
    setExitingSel(cur);
    setSel({ kind: "schema" });
    window.setTimeout(() => setExitingSel(null), 170);
  }, []);
  const [filter, setFilter] = useState("");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  // Rows per page are computed dynamically from the actual height of the list area: the taller the window, the fuller it fills, with no scrolling and no large empty gap
  const listRef = useRef<HTMLDivElement>(null);
  const [railRows, setRailRows] = useState(RAIL_PAGE);
  useEffect(() => {
    const el = listRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      setRailRows(
        Math.max(5, Math.floor((el.clientHeight - RAIL_RESERVED) / RAIL_ROW_H)),
      );
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const data = useQuery({
    queryKey: ["ontology", kb?.id],
    queryFn: () => api.ontology(kb!.id),
    enabled: !!kb,
  });
  // Values that coexist (#341): fetched on their own, because the scan is over facts, not the ontology, and the ontology page
  // refetches on every field edit; folding this scan into it would mean scanning the whole library on every keystroke
  const uniqueness = useQuery({
    queryKey: ["uniqueness", kb?.id],
    queryFn: () => api.uniquenessCandidates(kb!.id),
    enabled: !!kb,
  });
  const overlaps = uniqueness.data?.candidates ?? [];
  /* Business rules: the schema graph has to draw them as edges, and the rules page has to list them: the same query key
     is fetched once, and both places share the cache */
  const rules = useQuery({
    queryKey: ["rules", kb?.id],
    queryFn: () => api.rules(kb!.id),
    enabled: !!kb,
  });

  const refresh = () => {
    queryClient.invalidateQueries({ queryKey: ["ontology", kb?.id] });
    // After accepting or rejecting, the two numbers for "what the ontology still lacks" also change
    queryClient.invalidateQueries({ queryKey: ["questionReport", kb?.id] });
  };
  // Errors go uniformly through the global toast, no longer through an inline error row on the page
  const onError = (e: unknown) => toast.error((e as Error).message);

  // The ontology consistency check is a synchronous pure computation (the "Run check" on the Review page uses the same endpoint),
  // no queueing and no confirmation needed: after each ontology change it runs in passing, so new contradictions are raised
  // at the moment they arise, rather than waiting for someone to remember to look on the Review page one day.
  // When it is clean it says nothing: **it only speaks when there is something new**, otherwise every save would pop up an "all good"
  // which is more annoying than not popping up at all
  const runCheck = useMutation({
    mutationFn: () => api.runConsistencyCheck(kb!.id),
    onSuccess: (r) => {
      if (r.defects_new > 0) {
        toast.info(S.ontology.schemaCheckDefects(r.defects_new), {
          label: S.ontology.schemaCheckReview,
          onClick: () =>
            navigate({ to: "/kb/$kbId/review", params: { kbId: kb!.id } }),
        });
      }
    },
    // A failure of the check itself must not cover up "saved successfully": the ontology write has already landed on disk, and silent retry is left to the next change
  });
  const afterOntologyChange = () => {
    refresh();
    runCheck.mutate();
  };

  if (!kb) return <Loading>{S.nav.loading}</Loading>;
  if (data.isError) return <Loading>{(data.error as Error).message}</Loading>;

  /* Even when the ontology has not arrived yet, **still build this page**.
     Previously a single `data.isPending` check swapped the whole page for one line of "Loading": a library with over nine hundred classes
     would take several seconds, and during those seconds the person saw a blank area, not even knowing whether they had clicked the wrong page
     — while the two groups of entries in the left column (switching view, Import / Rules / Refine…) have nothing to do with whether the ontology has been fetched,
     and they could have been there at once.
     Now only the content waits: the left-column list shows a skeleton (an indented tree; its shape is known),
     and the area on the right, not yet knowing whether it will draw a graph or a table, shows a spinner. */
  const loading = data.isPending;
  const ont = data.data;
  const entity_types = ont?.entity_types ?? [];
  const relation_types = ont?.relation_types ?? [];
  // Properties are not in the Properties list: they hang under classes and are edited in the class detail area
  const relations = relation_types.filter((r) => r.kind !== "attribute");
  // The panel has to play its exit before unmounting: unmounting the moment the selection is cleared makes it vanish instantly (the graph page does the same)
  // Switching between two classes does not go this route: the panel stays in place and swaps its content, since shaking on every node click would be noisier
  const panelSel = onPanel(sel) ? sel : exitingSel;

  const selectedClass =
    panelSel?.kind === "class"
      ? (entity_types.find((t) => t.id === panelSel.id) ?? null)
      : null;
  const selectedProp =
    panelSel?.kind === "relation"
      ? (relation_types.find((r) => r.id === panelSel.id) ?? null)
      : null;
  // The attributes attached to the selected class; the Attributes section uses it
  const classAttributes = selectedClass
    ? relation_types.filter(
        (r) => r.kind === "attribute" && r.domains.includes(selectedClass.id),
      )
    : [];

  return (
    <div className="h-full flex">
      {/* Left column: filter + two sections + Unmatched */}
      <aside className={`${RAIL_CLS} flex flex-col`}>
        {/* The same shape and the same corner as the search box on the graph page (12px in from the top-left, medium size, 232 wide):
            switching back and forth between the two tabs, the box stays where it is */}
        {/* Graph / Table: the two ways of drawing this page. **Placed above the filter box**: it decides what this page
            is, and the filter box is for finding things inside it; also, in the table tier the filter box is collapsed,
            so if the toggle sat below, its position would jump around.

            **One key, which names the version to go to**: with the two tiers side by side, you would first have to read which one is lit
            to know what you are looking at, whereas whether the main area shows a table or a graph is obvious at a glance.

            **When Import / Rules / Refine / Overlaps / Unmatched take over the main area, this is
            the way back**: at that point it shows the version you were originally looking at, and clicking it returns you. Previously this path
            was the "Schema diagram" row, and after it was replaced by the toggle, it was broken for a while: after entering Import
            there was no way at all back to the graph or the table.

            **It forms its own group**: the same approach as the group pinned at the bottom, with a line on the outer edge and py-2 separating it,
            and the row itself draws no box. It is not the same kind of thing as the filter box, and at only 4px apart it would read as a run of one thing */}
        {/* The row above is an **action** (switch to the other version), the row below is a **position** (which version is being looked at now).
            Putting two things in one row makes the key ambiguous: is it saying where I am, or where I am going;
            once split into two rows, the position row also doubles as the way back from the Import / Rules pages,
            which take over the main area entirely; previously there was no way at all back to the graph or the table.
            **It forms its own group**: the same approach as the group pinned at the bottom, with a line on the outer edge and py-2 separating it */}
        <div className="u-rail-list shrink-0 border-b border-line px-2 py-2">
          <Row
            density="nav"
            icon={<ArrowLeftRight size={14} />}
            onClick={() => switchView(view === "diagram" ? "table" : "diagram")}
          >
            {view === "diagram"
              ? S.ontology.switchToTable
              : S.ontology.switchToGraph}
          </Row>
          <Row
            density="nav"
            active={!inWorkflow}
            icon={
              view === "diagram" ? <Network size={14} /> : <TableIcon size={14} />
            }
            onClick={() => setSel({ kind: "schema" })}
          >
            {view === "diagram" ? S.ontology.viewDiagram : S.ontology.viewTable}
          </Row>
        </div>
        {/* The filter box, the two tiers and the list **belong only to the graph tier**: the table is itself a list, and a stronger one than this column
            (it has columns, can sort, and carries counts). When switching tiers they **fold up** rather than vanish instantly:
            the left column is always there, and a whole section disappearing into nothing would make people think they had jumped to another page */}
        <div
          className={cn("u-rail-fold", view === "table" && "is-folded")}
        >
          <div className="px-2 pt-3 pb-1">
            <Input
              icon={<Search size={12} />}
              placeholder={S.ontology.filter}
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
          </div>
          <div className="px-2 pb-1">
            <Segmented
              fill
              value={railTab}
              onChange={setRailTab}
              options={[
                { value: "classes", label: S.ontology.tabClasses },
                { value: "properties", label: S.ontology.tabProperties },
              ]}
            />
          </div>
        <div
          ref={listRef}
          className="flex-1 min-h-0 overflow-hidden px-2 pb-2 flex flex-col"
        >
          {/* New-row pinned to the top: creates a class / relation for the current section */}
          {/* No creating until the ontology has arrived: a new class has to pick a parent, and the parent list is empty right now */}
          {!loading && !filter.trim() && (
            <Row
              className="mb-1"
              icon={<Plus size={14} />}
              onClick={() =>
                setEdit(
                  railTab === "classes"
                    ? { kind: "class", existing: null, parentId: null }
                    : { kind: "property", existing: null, initialDomain: null },
                )
              }
            >
              {railTab === "classes"
                ? S.ontology.newClass
                : S.ontology.newProperty}
            </Row>
          )}
          {/* **Filtering also respects these two tiers.** Previously, once there was a filter term, both sections were mixed together and classes and relations were offered together,
              the reasoning being "when searching, you may not know which kind you are looking for"; the cost was that a label that stopped on Classes did not count,
              even though it was two centimetres above. A control that is selected yet ignored
              is worse than having to click one more time. Switching over to look for relations is clear, that step is explicit */}
          {loading ? (
            <SkeletonRows />
          ) : railTab === "classes" ? (
            <ClassTree
              types={entity_types}
              filter={filter}
              collapsed={collapsed}
              onToggle={(id) => {
                const next = new Set(collapsed);
                if (next.has(id)) next.delete(id);
                else next.add(id);
                setCollapsed(next);
              }}
              selectedId={selectedClass?.id ?? null}
              onSelect={(id) => setSel({ kind: "class", id })}
              pageSize={railRows}
            />
          ) : (
            <PropertyList
              relations={relations}
              filter={filter}
              selectedId={selectedProp?.id ?? null}
              onSelect={(id) => setSel({ kind: "relation", id })}
              pageSize={railRows}
            />
          )}
        </div>
        </div>
        {/* Pinned at the bottom, always present: a few entries about the ontology: taking an ontology from outside, business rules, type resolution,
            and the two kinds of signal that come back from the data. A dividing line shows that they are pinned; the rows themselves look
            the same as the rows in the list above */}
        <div className="u-rail-list shrink-0 border-t border-line px-2 py-2">
        <RailItem
          active={sel?.kind === "import"}
          icon={<Upload size={14} />}
          onClick={() => setSel({ kind: "import" })}
        >
          {S.ontology.importShort}
        </RailItem>
        {/* Business rules (0021): the ontology says "there is such a class", the rule says "what kind of entity counts as it".
            So it is filed under this ontology page rather than opened somewhere else: the criteria and the vocabulary are one contract */}
        <RailItem
          active={sel?.kind === "rules"}
          icon={<Scale size={14} />}
          onClick={() => setSel({ kind: "rules" })}
        >
          {S.ontology.rulesShort}
        </RailItem>
        {/* Type resolution: replaces a "roughly right" class with a more specific one. **Sits right next to Unmatched**:
            both are ways of handling "the ontology and the data do not line up", just in opposite directions: that one is the ontology missing something,
            while this one is the ontology having a better option that has not been used */}
        <RailItem
          active={sel?.kind === "refine"}
          icon={<Wand2 size={14} />}
          onClick={() => setSel({ kind: "refine" })}
        >
          {S.ontology.refineShort}
        </RailItem>
        {/* Values that coexist (#341): **the same kind as Unmatched** — both are data saying what the ontology is missing.
            That one says a word is missing; this one says an axiom is missing, and the cost of a missing axiom is that a successor does not close its predecessor,
            so "who is in charge in June" answers with both people */}
        <RailItem
          active={sel?.kind === "uniqueness"}
          icon={<Split size={14} />}
          count={loading ? undefined : overlaps.length}
          onClick={() => setSel({ kind: "uniqueness" })}
        >
          {S.ontology.uniquenessShort}
        </RailItem>
        <RailItem
          active={sel?.kind === "regressions"}
          icon={<CheckCheck size={14} />}
          onClick={() => setSel({ kind: "regressions" })}
        >
          {S.ontology.casesShort}
        </RailItem>
        {/* Pinned at the bottom, always present: extraction unmatched signals (with a count badge when there is a backlog) */}
        <RailItem
          active={sel?.kind === "misses"}
          icon={<Inbox size={14} />}
          onClick={() => setSel({ kind: "misses" })}
        >
          {S.ontology.missesShort}
        </RailItem>
        </div>
      </aside>

      {/* Right side: detail. class / relation / new-class / new-relation / schema / overview share
          one workspace: the schema graph is always present as background, and the form (if any) docks on the right, nearly solid
          (u-modal-panel), not glass overlay: the canvas moves, and whatever shows through would clash
          with the form field being read. Clicking a class name in the left column, clicking a node on the canvas, or selecting a relation with the schema graph's own search box:
          all three paths land on the same sel, and so on the same form —
          not drawn separately each time. import / refine / misses are still independent full-page views, and those three
          are not about "some class or relation", so there is no reason for them to share a background with the schema graph */}
      {/* **The spinner is only left for those pages that take over the main area** (Import / Rules / Refine / Overlaps /
          Unmatched): they are wholly forms and lists, so nothing can be drawn until the ontology arrives.
          The graph and the table do not take this route: each of them has much that is unrelated to the ontology that can be drawn first
          (the grid, the zoom tower, the static legend; the segmented control, the filter box), so `loading` is passed down,
          and they decide for themselves which part should wait */}
      {loading && inWorkflow ? (
        <div className="flex-1 min-w-0 grid place-items-center">
          <Spinner size={20} label={S.nav.loading} />
        </div>
      ) : sel?.kind === "import" ||
      sel?.kind === "refine" ||
      sel?.kind === "misses" ||
      sel?.kind === "uniqueness" ||
      sel?.kind === "rules" ||
      sel?.kind === "regressions" ? (
        <div className="flex-1 min-w-0 overflow-y-auto u-scroll px-8 py-6">
          <div>
            {sel.kind === "import" ? (
              <div>
                <ImportPanel kbId={kb.id} onChanged={refresh} onError={onError} />
              </div>
            ) : sel.kind === "refine" ? (
              <div>
                <RefinePanel kbId={kb.id} onChanged={refresh} onError={onError} />
              </div>
            ) : sel.kind === "uniqueness" ? (
              <div>
                <UniquenessPanel
                  kbId={kb.id}
                  candidates={overlaps}
                  relations={relation_types}
                  pending={uniqueness.isPending}
                  onChanged={() => {
                    uniqueness.refetch();
                    refresh();
                  }}
                  onError={onError}
                />
              </div>
            ) : sel.kind === "regressions" ? (
              <OntologyRegressionsPanel kbId={kb.id} />
            ) : sel.kind === "rules" ? (
              <div>
                <RulesPanel
                  kbId={kb.id}
                  focusId={sel.kind === "rules" ? sel.focusId : undefined}
                  classes={entity_types}
                  attributes={relation_types.filter((r) => r.kind === "attribute")}
                  relations={relation_types.filter((r) => r.kind === "relation")}
                  onError={onError}
                />
              </div>
            ) : (
              <div>
                <MissesPanel
                  kbId={kb.id}
                  onChanged={refresh}
                  onError={onError}
                />
                <QuestionsPanel kbId={kb.id} onError={onError} />
              </div>
            )}
          </div>
        </div>
      ) : (
        <div className="flex-1 min-w-0 relative">
          {view === "table" ? (
            <OntologyTables
              loading={loading}
              selected={
                sel?.kind === "class" || sel?.kind === "relation" ? sel : null
              }
              entityTypes={entity_types}
              relationTypes={relation_types}
              onOpenClass={(t) => setSel({ kind: "class", id: t.id })}
              onOpenProperty={(r) => setSel({ kind: "relation", id: r.id })}
              onOpenAttribute={(a) =>
                setEdit({
                  kind: "attribute",
                  typeId: a.domains[0] ?? "",
                  existing: a,
                })
              }
              onSeeInstances={(t) => {
                setPanelTab("instances");
                setSel({ kind: "class", id: t.id });
              }}
              onCreateClass={() =>
                setEdit({ kind: "class", existing: null, parentId: null })
              }
              onCreateProperty={() =>
                setEdit({
                  kind: "property",
                  existing: null,
                  initialDomain: null,
                })
              }
              onCreateAttribute={() =>
                setEdit({ kind: "attribute", typeId: null, existing: null })
              }
            />
          ) : (
            <OntologySchemaGraph
            loading={loading}
            onEmptyAction={() => setSel({ kind: "misses" })}
            entityTypes={entity_types}
            relationTypes={relation_types}
            rules={rules.data?.rules ?? []}
            selected={
              sel?.kind === "class" || sel?.kind === "relation"
                ? ({ kind: sel.kind, id: sel.id } as SchemaSelection)
                : null
            }
            // Clicking blank space to clear the selection also goes through closePanel: otherwise a panel closed by clicking the canvas gets no
            // exit animation, and clicking the canvas is the most common way of closing it
            // Clicking a rule edge: switch to the business-rules page and light up that row
            onSelect={(next) =>
              next
                ? setSel(
                    next.kind === "rule"
                      ? { kind: "rules", focusId: next.id }
                      : next,
                  )
                : closePanel()
            }
            />
          )}
          {selectedClass && (
            <DockedPanel
              onClose={closePanel}
              exiting={!onPanel(sel)}
              actions={
                <IconButton
                  size="sm"
                  label={S.ontology.editClass}
                  onClick={() =>
                    setEdit({
                      kind: "class",
                      existing: selectedClass,
                      parentId: selectedClass.primary_parent ?? null,
                    })
                  }
                >
                  <Pencil size={13} />
                </IconButton>
              }
              header={
                <PanelHeader
                  color={selectedClass.color}
                  square={selectedClass.shape === "square"}
                  title={selectedClass.label}
                  sub={`${selectedClass.key} · ${S.ontology.usage(selectedClass.usage)}`}
                  builtin={selectedClass.builtin}
                />
              }
              tabs={
                <Segmented
                  fill
                  size="sm"
                  value={panelTab}
                  onChange={setPanelTab}
                  options={[
                    {
                      value: "definition",
                      label: S.ontology.schemaTabDefinition,
                    },
                    {
                      value: "relations",
                      label: S.ontology.schemaTabRelations,
                    },
                    {
                      value: "attributes",
                      label: S.ontology.schemaTabAttributes,
                    },
                    {
                      value: "instances",
                      label: S.ontology.schemaTabInstances,
                    },
                  ]}
                />
              }
            >
              {/* The four sections are hidden with `hidden`, not unmounted: the page the instance list was on is still there when switching back */}
              <div className={panelTab === "definition" ? "" : "hidden"}>
                <ClassDefinition
                  cls={selectedClass}
                  allTypes={entity_types}
                  onSelectClass={(id) => setSel({ kind: "class", id })}
                  onNewSub={() =>
                    setEdit({ kind: "class", existing: null, parentId: selectedClass.id })
                  }
                />
              </div>
              <div className={panelTab === "relations" ? "" : "hidden"}>
                <RelationshipsCard
                  cls={selectedClass}
                  relations={relations}
                  allTypes={entity_types}
                  onSelect={(id) => setSel({ kind: "relation", id })}
                  onConnect={() => setEdit({ kind: "connect", cls: selectedClass })}
                  onAddNew={() =>
                    setEdit({
                      kind: "property",
                      existing: null,
                      initialDomain: selectedClass.id,
                    })
                  }
                />
              </div>
              <div className={panelTab === "attributes" ? "" : "hidden"}>
                <AttributesCard
                  attributes={classAttributes}
                  onEdit={(a) =>
                    setEdit({ kind: "attribute", typeId: selectedClass.id, existing: a })
                  }
                  onNew={() =>
                    setEdit({ kind: "attribute", typeId: selectedClass.id, existing: null })
                  }
                />
              </div>
              <div className={panelTab === "instances" ? "" : "hidden"}>
                <InstancesCard kbId={kb.id} type={selectedClass} />
              </div>
            </DockedPanel>
          )}
          {selectedProp && (
            <DockedPanel
              onClose={closePanel}
              exiting={!onPanel(sel)}
              actions={
                <IconButton
                  size="sm"
                  label={S.ontology.editProperty}
                  onClick={() =>
                    setEdit({ kind: "property", existing: selectedProp, initialDomain: null })
                  }
                >
                  <Pencil size={13} />
                </IconButton>
              }
              header={
                <PanelHeader
                  title={selectedProp.label}
                  sub={`${selectedProp.key} · ${S.ontology.usage(selectedProp.usage)}`}
                  builtin={selectedProp.builtin}
                />
              }
            >
              <PropertyDefinition
                rel={selectedProp}
                allTypes={entity_types}
                allRelations={relations}
              />
            </DockedPanel>
          )}
        </div>
      )}

      {/* Edit dialog: the panel only displays; create, edit, delete and connect all happen here. A newly created item is selected straight away —
          so it can be seen and carried on editing; a deleted one collapses the panel */}
      {edit?.kind === "class" && (
        <ClassDialog
          kbId={kb.id}
          existing={edit.existing}
          parentId={edit.parentId}
          allTypes={entity_types}
          onClose={() => setEdit(null)}
          onSaved={(createdId) => {
            setEdit(null);
            if (createdId) setSel({ kind: "class", id: createdId });
            afterOntologyChange();
          }}
          onDeleted={() => {
            setEdit(null);
            closePanel();
            afterOntologyChange();
          }}
          onError={onError}
        />
      )}
      {edit?.kind === "property" && (
        <PropertyDialog
          kbId={kb.id}
          existing={edit.existing}
          allTypes={entity_types}
          allRelations={relations}
          allAttributes={relation_types.filter((r) => r.kind === "attribute")}
          initialDomain={edit.initialDomain}
          onClose={() => setEdit(null)}
          onSaved={(createdId) => {
            setEdit(null);
            if (createdId) setSel({ kind: "relation", id: createdId });
            afterOntologyChange();
          }}
          onDeleted={() => {
            setEdit(null);
            closePanel();
            afterOntologyChange();
          }}
          onError={onError}
        />
      )}
      {edit?.kind === "attribute" && (
        <AttributeDialog
          kbId={kb.id}
          typeId={edit.typeId}
          allTypes={entity_types}
          existing={edit.existing}
          onClose={() => setEdit(null)}
          onSaved={() => {
            setEdit(null);
            afterOntologyChange();
          }}
          onDeleted={() => {
            setEdit(null);
            afterOntologyChange();
          }}
          onError={onError}
        />
      )}
      {edit?.kind === "connect" && (
        <ConnectDialog
          kbId={kb.id}
          cls={edit.cls}
          relations={relations}
          onClose={() => setEdit(null)}
          onConnected={(id) => {
            setEdit(null);
            afterOntologyChange();
            // After connecting, jump straight to that relation: whether the domain/range was changed, and whether it is right, is visible at a glance
            setSel({ kind: "relation", id });
          }}
          onError={onError}
        />
      )}
    </div>
  );
}

/* ---------- Docked panel: the detail shell to the right of the schema graph ----------
   Same structure as the entity panel on the graph page: **the title row is fixed, only the content below scrolls**. If the whole column scrolled together,
   the name of the class being edited, how many times it is used, and the close button would all scroll out of sight.
   The close button moves with the title and is no longer a separate row: that row both took up height at the top and made the button look
   as if it belonged to no card.

   The surface is also `glass-strong`, as on the graph page: **it solidifies as soon as the pointer is on it** (120ms in, 260ms
   out, see styles.css). The author originally used a near-solid background, because the canvas moves and the motion showing through would clash with
   the field being read — that worry holds, and the glass hover-solidify is the answer to it: while a person is using this surface it is solid,
   and only when the hand moves away does it turn see-through again, so the canvas context is not blocked by a rigid slab.

   Exit follows the graph page too: `u-dock-out` plays out before unmounting, rather than vanishing at once */

function DockedPanel({
  header,
  actions,
  tabs,
  exiting,
  onClose,
  children,
}: {
  header: React.ReactNode;
  /** The action to the left of the close button (edit): the panel only displays, and changes open a dialog from here */
  actions?: React.ReactNode;
  /** Segmented control, fixed to the top along with the title — it must stay clickable at all times */
  tabs?: React.ReactNode;
  /** Exiting: plays the animation, and during that time it no longer accepts clicks (u-dock-out carries pointer-events) */
  exiting?: boolean;
  onClose: () => void;
  children: React.ReactNode;
}) {
  return (
    <div
      // The same shell as the entity panel on the graph page: the same width (w-96), the same top starting point (to make room for the row of
      // pills on top), the same header anatomy. Viewing the two pages side by side, they are the same thing
      className={`${exiting ? "u-dock-out" : "u-dock-in"} glass-strong absolute top-3 right-3 bottom-3 w-96 z-10 rounded-overlay u-lift-strong flex flex-col`}
    >
      <div className="shrink-0 flex items-start justify-between gap-2 px-4 py-4 border-b border-line">
        <div className="min-w-0">{header}</div>
        <div className="-mr-1 -mt-1 flex shrink-0 items-center gap-1">
          {actions}
          <IconButton size="sm" label={S.ontology.schemaClosePanel} onClick={onClose}>
            <X size={15} />
          </IconButton>
        </div>
      </div>
      {tabs && <div className="shrink-0 px-4 pt-3 pb-1">{tabs}</div>}
      <div className="u-scroll flex-1 min-h-0 overflow-y-auto flex flex-col gap-3 px-4 py-3">
        {children}
      </div>
    </div>
  );
}

/* Panel title: colour dot + name + a small line of text in the second row. The same approach as the header of the entity panel on the graph page */
function PanelHeader({
  color,
  square,
  title,
  sub,
  builtin,
}: {
  color?: string;
  square?: boolean;
  title: string;
  sub?: string;
  builtin?: boolean;
}) {
  return (
    <>
      <div className="flex items-center gap-2">
        {color && (
          <span
            className={`h-2.5 w-2.5 shrink-0 ${square ? "scale-90" : "rounded-full"}`}
            style={{ background: color, boxShadow: `0 0 8px ${color}55` }}
          />
        )}
        <span
          className="truncate text-title font-semibold tracking-tight text-ink"
          style={{ fontFamily: "var(--font-display)" }}
        >
          {title}
        </span>
        {builtin && <Chip tone="neutral">{S.ontology.builtin}</Chip>}
      </div>
      {sub && <div className="mt-1 text-small text-ink-2">{sub}</div>}
    </>
  );
}

/* ---------- Instance list: the entities of the selected class (paged by the server; clicking goes to the graph) ---------- */

function InstancesCard({ kbId, type }: { kbId: string; type: EntityTypeView }) {
  const PER = 12;
  const [page, setPage] = useState(0);
  useEffect(() => setPage(0), [type.id]);
  const q = useQuery({
    queryKey: ["type-entities", kbId, type.id, page],
    queryFn: () => api.typeEntities(kbId, type.id, page, PER),
  });
  const total = q.data?.total ?? 0;
  const rows = q.data?.entities ?? [];
  // This section is itself the Instances tab: when there are no instances it says so in one line, without repeating a title of the same name
  if (!q.isPending && total === 0)
    return <p className="text-small text-ink-2">{S.ontology.schemaNoInstances}</p>;

  return (
    // Laid flat inside the panel: the panel is already a surface, so no cards are nested inside it
    <div>
      <div>
        {rows.map((e) => (
          <Link
            key={e.id}
            to="/kb/$kbId/graph"
            params={{ kbId }}
            search={{ entity: e.id }}
            className={cn(rowClass(), "-mx-2")}
          >
            <span
              className={`h-2 w-2 shrink-0 ${type.shape === "square" ? "scale-90" : "rounded-full"}`}
              style={{ background: type.color }}
            />
            <span className="truncate">{e.name}</span>
            <span className={cn("u-num", ROW_TRAILING)}>
              {S.ontology.instanceFacts(e.fact_count)}
            </span>
          </Link>
        ))}
      </div>
      <Pager total={total} pageSize={PER} page={page} onPage={setPage} />
    </div>
  );
}

/* ---------- Relation card: which relations this class connects to as subject/object + connect a new one using an existing relation ---------- */

/** "New relation" (PropertyForm) handles the case where the relation is not yet in the ontology; this card
 *  handles the other half — **which relations this class already has attached** (those leaving it, those pointing at it),
 *  and the more common need: most of the time you do not need a new relation, but want to connect this class to a
 *  relation that already exists (`works_at` already connects Person→Organization,
 *  and when creating a Contractor, the usual move is to add Contractor to works_at's domain,
 *  not create another works_at_2). The latter directly rewrites the domains/ranges of the existing relation,
 *  using the same updateRelationType that PropertyForm uses on save, just compressing "open the form,
 *  find the multi-select, add a class" into one step. */
function RelationshipsCard({
  cls,
  relations,
  allTypes,
  onSelect,
  onConnect,
  onAddNew,
}: {
  cls: EntityTypeView;
  /** Only those with kind === "relation": an attribute's object is a literal value, so there is no "connected" for it */
  relations: RelationTypeView[];
  allTypes: EntityTypeView[];
  onSelect: (relationId: string) => void;
  /** Connect this class with an existing relation: opens the dialog */
  onConnect: () => void;
  onAddNew: () => void;
}) {
  const labelOf = (id: string) =>
    allTypes.find((t) => t.id === id)?.label ?? id;
  // A reflexive relation (domain and range are both this class) counts on both sides: it does hold in both directions
  const outgoing = relations.filter((r) => r.domains.includes(cls.id));
  const incoming = relations.filter((r) => r.ranges.includes(cls.id));

  /** A collapsible group (the anatomy of #455: fold handle, header, indented body). Expanded by default —
   *  it is folded to skip over a stretch of a long list, not to hide things */
  const Group = ({
    dir,
    rows,
  }: {
    dir: "out" | "in";
    rows: RelationTypeView[];
  }) => {
    const [open, setOpen] = useState(true);
    return (
      <div>
        {/* The header is one Row: the fold handle takes the icon cell (16 wide), and the body is indented by the same 24,
            so the direction arrow of each row below sits exactly under the group header's arrow */}
        <Row
          className="-mx-2 mt-1"
          icon={
            <span className="flex w-4 justify-center">
              <ChevronRight size={12} className={cn("u-turn", open && "rotate-90")} />
            </span>
          }
          onClick={() => setOpen((v) => !v)}
        >
          <span className="flex items-center gap-2 text-small font-medium">
            {/* The **same size** as the arrow on each relation row below (12): the same mark laid out in one column,
                a header that is 2px smaller would only read as misaligned */}
            {dir === "out" ? <ArrowRight size={12} /> : <ArrowLeft size={12} />}
            <span className="truncate">
              {dir === "out" ? S.ontology.schemaOutgoing : S.ontology.schemaIncoming}
            </span>
            {rows.length > 1 && <span className="u-num">{rows.length}</span>}
          </span>
        </Row>
        {open && (
          <div className="pl-6">
            {rows.map((r) => (
              <RelationRow key={`${dir}:${r.id}`} r={r} dir={dir} />
            ))}
          </div>
        )}
      </div>
    );
  };

  const RelationRow = ({ r, dir }: { r: RelationTypeView; dir: "out" | "in" }) => (
    // Hover needs a background colour: the whole row is clickable, and only brightening the text by half a step barely shows on a dark background.
    // The background uses the tier from the left column (white/[0.05]), and the small type-name on the right brightens with it
    <Row
      className="-mx-2"
      icon={
        dir === "out" ? (
          <ArrowRight size={12} className="text-violet" />
        ) : (
          <ArrowLeft size={12} className="text-violet" />
        )
      }
      trailing={
        <span className="block max-w-32 truncate">
          {(dir === "out" ? r.ranges : r.domains).map(labelOf).join(", ") ||
            S.ontology.anyType}
        </span>
      }
      onClick={() => onSelect(r.id)}
    >
      {r.label}
    </Row>
  );

  return (
    // Laid flat inside the panel, with the same skeleton as the relation groups of the graph panel: small headings with direction arrows + rows.
    // This section is itself the Relations tab, so it does not put up a heading of the same name
    <div>
      {outgoing.length === 0 && incoming.length === 0 ? (
        <p className="mb-2 text-small text-ink-2">
          {S.ontology.schemaNoRelationships}
        </p>
      ) : (
        <div className="mb-2">
          {outgoing.length > 0 && <Group dir="out" rows={outgoing} />}
          {incoming.length > 0 && <Group dir="in" rows={incoming} />}
        </div>
      )}
      {/* Changes open a dialog: connect an existing relation, or create a new one. This section of the panel only displays.
          The two rows have the same shape as the relation rows above */}
      <Row className="-mx-2 mt-2" icon={<Link2 size={14} />} onClick={onConnect}>
        {S.ontology.connectOpen}
      </Row>
      <Row className="-mx-2" icon={<Plus size={14} />} onClick={onAddNew}>
        {S.ontology.schemaAddRelationship}
      </Row>
    </div>
  );
}

/* ---------- Attribute card: the literal-value fields of the selected class (changes open a dialog) ---------- */

function AttributesCard({
  attributes,
  onEdit,
  onNew,
}: {
  attributes: RelationTypeView[];
  onEdit: (attribute: RelationTypeView) => void;
  onNew: () => void;
}) {
  return (
    <div>
      <p className="mb-2 text-fine text-ink-2">
        {S.ontology.attributesHint}
      </p>
      <div className="divide-y divide-line">
        {attributes.map((a) => (
          <Row
            key={a.id}
            className="-mx-2"
            trailing={<span className="u-num">{S.ontology.usage(a.usage)}</span>}
            onClick={() => onEdit(a)}
          >
            <span className="flex items-center gap-2">
              <span className="truncate">{a.label}</span>
              <Chip tone="neutral">
                {S.ontology.datatypeNames[a.datatype ?? "text"]}
              </Chip>
              {a.unit && (
                <span className="shrink-0 text-small text-ink-2">{a.unit}</span>
              )}
              {a.functional && <Chip tone="info">1:1</Chip>}
            </span>
          </Row>
        ))}
      </div>
      <Row className="-mx-2 mt-2" icon={<Plus size={14} />} onClick={onNew}>
        {S.ontology.newAttribute}
      </Row>
    </div>
  );
}

/* ---------- Left-column section header ---------- */

/* ---------- Class hierarchy tree (collapsible; flattened when filtering) ---------- */

function ClassTree({
  types,
  filter,
  collapsed,
  onToggle,
  selectedId,
  onSelect,
  pageSize,
}: {
  types: EntityTypeView[];
  filter: string;
  collapsed: Set<string>;
  onToggle: (id: string) => void;
  selectedId: string | null;
  onSelect: (id: string) => void;
  pageSize: number;
}) {
  const rows = useMemo(() => {
    const q = filter.trim().toLowerCase();
    if (q) {
      // Filter mode: flattened hits (both label and key take part in matching)
      return types
        .filter(
          (t) =>
            t.label.toLowerCase().includes(q) ||
            t.key.toLowerCase().includes(q),
        )
        .map((t) => ({ t, depth: 0, hasChildren: false }));
    }
    const children = new Map<string | null, EntityTypeView[]>();
    for (const t of types) {
      const p = treeParent(t);
      if (!children.has(p)) children.set(p, []);
      children.get(p)!.push(t);
    }
    const out: { t: EntityTypeView; depth: number; hasChildren: boolean }[] =
      [];
    const walk = (parent: string | null, depth: number) => {
      for (const t of children.get(parent) ?? []) {
        const kids = children.get(t.id) ?? [];
        out.push({ t, depth, hasChildren: kids.length > 0 });
        if (!collapsed.has(t.id)) walk(t.id, depth + 1);
      }
    };
    walk(null, 0);
    return out;
  }, [types, filter, collapsed]);

  // Half-screen paging: a change in the filter term returns to page one
  const [page, setPage] = useState(0);
  useEffect(() => setPage(0), [filter]);
  const { rows: paged, safe } = pageSlice(rows, page, pageSize);

  return (
    <div className="u-rail-list">
      {paged.map(({ t, depth, hasChildren }) => (
        <Row
          key={t.id}
          indent={depth}
          active={selectedId === t.id}
          onClick={() => onSelect(t.id)}
          icon={
            hasChildren ? (
              // Fold handle: rendered only when there are subclasses; clicking it does not select. It is a span rather than a button, because a button cannot nest another button inside it
              <span
                onClick={(e) => {
                  e.stopPropagation();
                  onToggle(t.id);
                }}
                className="u-toggle"
              >
                <ChevronRight
                  size={12}
                  className={cn("u-turn", !collapsed.has(t.id) && "rotate-90")}
                />
              </span>
            ) : (
              <span className="block w-3" />
            )
          }
        >
          <span className="flex items-center gap-2">
            {/* Squares are right-angled, to set them apart from circles (the same logic applies to graph nodes) */}
            <span
              className={`h-2.5 w-2.5 shrink-0 ${t.shape === "square" ? "scale-90" : "rounded-full"}`}
              style={{ background: t.color }}
            />
            {/* No per-item usage readout in the list: once the count grows, both the statistics and the rendering become a burden; usage is checked in the form */}
            <span className="truncate">{t.label}</span>
          </span>
        </Row>
      ))}
      <Pager
        total={rows.length}
        pageSize={pageSize}
        page={safe}
        onPage={setPage}
      />
    </div>
  );
}

/* ---------- Relation list ---------- */

function PropertyList({
  relations,
  filter,
  selectedId,
  onSelect,
  pageSize,
}: {
  relations: RelationTypeView[];
  filter: string;
  selectedId: string | null;
  onSelect: (id: string) => void;
  pageSize: number;
}) {
  const q = filter.trim().toLowerCase();
  const rows = q
    ? relations.filter(
        (r) =>
          r.label.toLowerCase().includes(q) || r.key.toLowerCase().includes(q),
      )
    : relations;
  // Half-screen paging: a change in the filter term returns to page one
  const [page, setPage] = useState(0);
  useEffect(() => setPage(0), [filter]);
  const { rows: paged, safe } = pageSlice(rows, page, pageSize);
  return (
    <div className="u-rail-list">
      {paged.map((r) => (
        <Row
          key={r.id}
          active={selectedId === r.id}
          onClick={() => onSelect(r.id)}
          // The leading area keeps only the fold-handle slot, so the text starts aligned with the left edge of the "identifier dot" of the class rows
          icon={<span className="block w-3" />}
        >
          <span className="flex items-center gap-2">
            <span className="truncate">{r.label}</span>
            {r.functional && <Chip tone="info">1:1</Chip>}
          </span>
        </Row>
      ))}
      <Pager
        total={rows.length}
        pageSize={pageSize}
        page={safe}
        onPage={setPage}
      />
    </div>
  );
}

/* ---------- Definition (read-only): the panel displays it, changes open a dialog ---------- */

/** One row of "label / value"; if the value is empty, write a placeholder rather than leaving it blank */
function Def({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="border-b border-line py-2 last:border-0">
      <div className="text-small text-ink-2">{label}</div>
      <div className="mt-1 break-words text-body text-ink">{children}</div>
    </div>
  );
}

function Description({ text }: { text: string | null | undefined }) {
  return text?.trim() ? (
    <span className="whitespace-pre-wrap">{text}</span>
  ) : (
    <span className="text-ink-2">{S.ontology.noDescription}</span>
  );
}

export function ClassDefinition({
  cls,
  allTypes,
  onSelectClass,
  onNewSub,
}: {
  cls: EntityTypeView;
  allTypes: EntityTypeView[];
  onSelectClass: (id: string) => void;
  /** Create a subclass with the current class as parent: opens the dialog */
  onNewSub: () => void;
}) {
  const nameOf = (id: string) => allTypes.find((t) => t.id === id)?.label ?? id;
  const classLinks = (ids: string[]) =>
    ids.map((id, index) => (
      <span key={id}>
        {index > 0 && ", "}
        <LinkButton onClick={() => onSelectClass(id)}>{nameOf(id)}</LinkButton>
      </span>
    ));
  const subclasses = allTypes.filter((type) => type.parents.includes(cls.id));
  return (
    <div>
      <Def label={S.ontology.parent}>
        {cls.parents.length > 0 ? classLinks(cls.parents) : S.ontology.noParent}
      </Def>
      <Def label={S.ontology.subclasses}>
        {subclasses.length > 0
          ? classLinks(subclasses.map((type) => type.id))
          : S.ontology.noSubclasses}
      </Def>
      <Def label={S.ontology.disjoint}>
        {cls.disjoint.length > 0 ? cls.disjoint.map(nameOf).join(", ") : S.ontology.noDisjoint}
      </Def>
      <Def label={S.ontology.description}>
        <Description text={cls.description} />
      </Def>
      {/* The add entry has the same shape as the rows in the list (the "New class" in the left column is also a row like this):
          the icon sits on the left edge of the text, rather than using a button with inner padding to line up */}
      <Row className="-mx-2 mt-2" icon={<Plus size={14} />} onClick={onNewSub}>
        {S.ontology.newSubClass}
      </Row>
    </div>
  );
}

function PropertyDefinition({
  rel,
  allTypes,
  allRelations,
}: {
  rel: RelationTypeView;
  allTypes: EntityTypeView[];
  allRelations: RelationTypeView[];
}) {
  const typeName = (id: string) => allTypes.find((t) => t.id === id)?.label ?? id;
  const relName = (id: string) => allRelations.find((r) => r.id === id)?.label ?? id;
  const temporal =
    (
      {
        state: S.ontology.temporalState,
        event: S.ontology.temporalEvent,
        eternal: S.ontology.temporalEternal,
      } as Record<string, string>
    )[rel.temporal] ?? rel.temporal;
  // Only the axioms that are ticked are listed: unticked ones carry no information
  const axioms = (
    [
      [rel.functional, S.ontology.functional],
      [rel.inverse_functional, S.ontology.inverseFunctional],
      [rel.is_transitive, S.ontology.transitive],
      [rel.is_symmetric, S.ontology.symmetric],
      [rel.is_asymmetric, S.ontology.asymmetric],
      [rel.is_irreflexive, S.ontology.irreflexive],
    ] as const
  )
    .filter(([on]) => on)
    .map(([, name]) => name);
  return (
    <div>
      <Def label={S.ontology.domainLabel}>
        {rel.domains.length > 0 ? rel.domains.map(typeName).join(", ") : S.ontology.anyType}
      </Def>
      <Def label={S.ontology.rangeLabel}>
        {rel.ranges.length > 0 ? rel.ranges.map(typeName).join(", ") : S.ontology.anyType}
      </Def>
      {/* Attributes on the edge (0037): which attributes an edge of this relation can carry, listed by the attribute's label */}
      <Def label={S.ontology.qualifiers}>
        {/* `?? []`: this cell was added later (0037), and **older versions of the backend do not send it**.
            Losing one cell should not bring the whole panel down — clicking a relation and getting a blank screen is exactly how that happened */}
        {(rel.qualifiers ?? []).length > 0
          ? (rel.qualifiers ?? []).map(typeName).join(", ")
          : S.ontology.noQualifiers}
      </Def>
      <Def label={S.ontology.temporal}>{temporal}</Def>
      <Def label={S.ontology.axioms}>
        {axioms.length > 0 ? (
          <span className="flex flex-wrap gap-1">
            {axioms.map((a) => (
              <Chip key={a} tone="info">
                {a}
              </Chip>
            ))}
          </span>
        ) : (
          <span className="text-ink-2">{S.ontology.axiomsNone}</span>
        )}
      </Def>
      {rel.inverse_of && (
        <Def label={S.ontology.inverseOf}>{relName(rel.inverse_of)}</Def>
      )}
      {rel.sub_property_of && (
        <Def label={S.ontology.subPropertyOf}>{relName(rel.sub_property_of)}</Def>
      )}
      <Def label={S.ontology.description}>
        <Description text={rel.description} />
      </Def>
    </div>
  );
}

/* ---------- Unmatched signals + AI suggestions ---------- */


/** Type resolution: replaces a "roughly right" class with a more specific one.
 *
 * **Two steps, the same shape as ontology import**: first look at what would happen, then decide whether to land it. Here there is one more
 * layer of reasoning: a change of class does not enter the timeline, and unlike fact rewrites it does not show itself in the entity history, so
 * "look first" is the only moment at which it can be seen.
 *
 * After running, it splits into three tiers, each with its own disposition: the ones changed automatically (can be undone in one batch), the ones that cross the classification axis and are left to a person,
 * and the ones the adjudicator said were "none of these". **The last tier carries its reason**: this step rests on "none of the choices is a
 * decent answer", and without a recorded reason the biggest tier would be opaque.
 */
function RefinePanel({
  kbId,
  onChanged,
  onError,
}: {
  kbId: string;
  onChanged: () => void;
  onError: (e: Error) => void;
}) {
  const [preview, setPreview] = useState<TypeSuggestion[] | null>(null);
  const [outcome, setOutcome] = useState<ResolutionOutcome | null>(null);

  const look = useMutation({
    mutationFn: () => api.typeResolutionPreview(kbId),
    onSuccess: (d) => {
      setPreview(d.items);
      setOutcome(null);
    },
    onError,
  });
  const run = useMutation({
    mutationFn: () => api.typeResolutionApply(kbId),
    onSuccess: (d) => {
      setOutcome(d);
      setPreview(null);
      onChanged();
    },
    onError,
  });
  const approve = useMutation({
    mutationFn: (v: {
      from_type_id: string;
      to_type_id: string;
      entity_ids: string[];
    }) => api.approveRefinement(kbId, v),
    onSuccess: () => {
      toast.success(S.toast.saved);
      onChanged();
    },
    onError,
  });
  const undo = useMutation({
    mutationFn: (batch: string) => api.typeResolutionUndo(kbId, batch),
    onSuccess: (d) => {
      toast.success(S.ontology.refineUndone(d.reverted));
      setOutcome(null);
      onChanged();
    },
    onError,
  });

  const busy = look.isPending || run.isPending;

  return (
    <div className="space-y-4">
      <PageHeader className="mb-2" title={S.ontology.refineTitle} sub={S.ontology.refineHint} />

      <div className="flex gap-2">
        <Button variant="secondary" size="sm" disabled={busy} onClick={() => look.mutate()}>
          {look.isPending ? S.ontology.refineLooking : S.ontology.refinePreview}
        </Button>
        <Button variant="primary" size="sm" disabled={busy} onClick={() => run.mutate()}>
          {run.isPending ? S.ontology.refineRunning : S.ontology.refineRun}
        </Button>
      </div>

      {/* ---- The step that only computes and does not write */}
      {preview && (
        <div className="space-y-2">
          <p className="text-small text-ink-2">
            {preview.length === 0
              ? S.ontology.refineNothing
              : S.ontology.refineCandidates(preview.length)}
          </p>
          {preview.map((s) => (
            <div key={s.entity_id} className="glass rounded-panel p-3">
              <div className="flex items-baseline gap-2 flex-wrap">
                <span className="text-body text-ink">{s.name}</span>
                <span className="text-fine text-ink-2">
                  {s.coarse ?? S.graph.untyped}
                </span>
                {s.specific_type && (
                  <span className="text-fine text-warn">
                    {S.ontology.refineModelSays(s.specific_type)}
                  </span>
                )}
                <span className="ml-auto u-num text-fine text-ink-2">
                  {S.review.factsCount(s.fact_count)}
                </span>
              </div>
              {/* **Show the text that was sent to retrieval**: when nothing is found, the first thing to check is
                  what we searched with, rather than guessing whether the problem is the profile or the class description */}
              <p className="mt-1 text-fine text-ink-2 line-clamp-2">
                {s.profile}
              </p>
              <div className="mt-2 flex flex-wrap gap-1">
                {s.candidates.slice(0, 6).map((c) => (
                  <span
                    key={c.id}
                    title={c.description}
                    className={chipLike("neutral", "u-num text-fine")}
                  >
                    {c.label} {c.distance.toFixed(2)}
                  </span>
                ))}
                {s.candidates.length === 0 && (
                  <span className="text-fine text-ink-2">
                    {S.ontology.refineNoCandidates}
                  </span>
                )}
              </div>
            </div>
          ))}
        </div>
      )}

      {/* ---- The three tiers after it has been written to the library */}
      {outcome && (
        <div className="space-y-3">
          <div className="flex items-center gap-3 flex-wrap">
            <span className="text-small text-ink-2">
              {S.ontology.refineRetyped(outcome.retyped)}
            </span>
            {outcome.batch && outcome.retyped > 0 && (
              <Button
                variant="secondary"
                size="sm"
                disabled={undo.isPending}
                onClick={() => undo.mutate(outcome.batch!)}
              >
                {S.ontology.refineUndo}
              </Button>
            )}
          </div>

          {outcome.for_review.length > 0 && (
            <div className="space-y-2">
              <p className="text-small text-ink-2">
                {S.ontology.refineForReview(outcome.for_review.length)}
              </p>
              {outcome.for_review.map((r) => (
                <div key={r.entity_id} className="glass rounded-panel p-3">
                  <div className="flex items-baseline gap-2 flex-wrap">
                    <span className="text-body text-ink">{r.name}</span>
                    <span className="text-fine text-ink-2">
                      {r.coarse ?? S.graph.untyped} → {r.choice}
                    </span>
                    {r.crosses_axis && (
                      <Status tone="warn" className="text-fine">
                        {S.ontology.refineCrossesAxis}
                      </Status>
                    )}
                    <span className="ml-auto u-num text-fine text-ink-2">
                      {Math.round(r.confidence * 100)}%
                    </span>
                  </div>
                  {r.reason && (
                    <p className="mt-1 text-fine text-ink-2">
                      {r.reason}
                    </p>
                  )}
                  {/* **What is approved is this pair of classes, not this one entity.** Approved once,
                      after that the same pair no longer goes to manual review — that is the cause of most entries in this tier */}
                  {r.from_type_id && (
                    <Button
                      variant="primary"
                      size="sm"
                      className="mt-2"
                      disabled={approve.isPending}
                      onClick={() =>
                        approve.mutate({
                          from_type_id: r.from_type_id!,
                          to_type_id: r.to_type_id,
                          entity_ids: [r.entity_id],
                        })
                      }
                    >
                      {S.ontology.refineApprovePair}
                    </Button>
                  )}
                </div>
              ))}
            </div>
          )}

          {outcome.left_alone.length > 0 && (
            <div className="space-y-2">
              <p className="text-small text-ink-2">
                {S.ontology.refineLeftAlone(outcome.left_alone.length)}
              </p>
              {outcome.left_alone.map((d, i) => (
                <div key={i} className="glass rounded-panel px-3 py-2">
                  <div className="flex items-baseline gap-2 flex-wrap">
                    <span className="text-body text-ink">
                      {d.name}
                    </span>
                    <span className="text-fine text-ink-2">
                      {d.coarse ?? S.graph.untyped}
                    </span>
                  </div>
                  {/* The reason is given together with the first candidate: when the reason does not make sense, looking at the candidates shows whether
                      retrieval found nothing or the adjudicator did not like any of them */}
                  {d.reason && (
                    <p className="mt-1 text-fine text-ink-2">
                      {d.reason}
                    </p>
                  )}
                  {d.top_candidate && (
                    <p className="mt-1 text-fine text-ink-2">
                      {S.ontology.refineTopCandidate(d.top_candidate)}
                    </p>
                  )}
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
/** Predicates with more than one open value at one end (#341).
 *
 *  Automatic closing relies on `functional` / `inverse_functional`, and in a library the ontology grew by itself
 *  nobody has declared them — and the engine does not infer them for people either (that would let a guessed axiom rewrite the ledger).
 *  So a successor does not close its predecessor: two `leads` both stay open, and asking who is in charge in June, both answer.
 *
 *  **This tier does not make the decision for people; it only lays out the evidence.** Who holds which values, how many rows the reconciliation would close,
 *  and how many of those are uncertain and must go to human review — all written on the face of the button before pressing it, because closing one row that has been open since 2023
 *  is a real change to the ledger. */
function UniquenessPanel({
  kbId,
  candidates,
  relations,
  pending,
  onChanged,
  onError,
}: {
  kbId: string;
  candidates: UniquenessCandidate[];
  relations: RelationTypeView[];
  pending: boolean;
  onChanged: () => void;
  onError: (e: unknown) => void;
}) {
  // The item just completed leaves one line of result: the panel refetches, and the card disappears with it,
  // without a note the person only sees the thing gone, and does not know how many were closed
  const [done, setDone] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<string | null>(null);

  const apply = async (c: UniquenessCandidate) => {
    setBusy(c.predicate_id);
    try {
      // Two steps: first add the declarations (skip those already declared), then reconcile. Declaring is the ontology's business,
      // reconciling is the ledger's business — the backend's reconcile refuses to run when there is no declaration.
      //
      // **Send the whole thing together.** This endpoint is a whole replacement: a missing field equals clearing it (the backend wrote out why
      // on `RelationTypeReq`: a half overwrite, half preserve would make "I removed the inverse" look the same as
      // "I did not touch the inverse"). Sending only one axiom would wipe out the other five along with inverse_of,
      // and that is the hardest part of the ontology to rebuild */
      if (!c.declared) {
        const rel = relations.find((r) => r.id === c.predicate_id);
        if (!rel) throw new Error(c.label);
        await api.updateRelationType(kbId, c.predicate_id, {
          label: rel.label,
          temporal: rel.temporal,
          functional: c.axiom === "functional" ? true : rel.functional,
          inverse_functional:
            c.axiom === "inverse_functional" ? true : rel.inverse_functional,
          is_transitive: rel.is_transitive,
          is_symmetric: rel.is_symmetric,
          is_asymmetric: rel.is_asymmetric,
          is_irreflexive: rel.is_irreflexive,
          inverse_of: rel.inverse_of,
          sub_property_of: rel.sub_property_of,
          description: rel.description,
          domains: rel.domains,
          ranges: rel.ranges,
          qualifiers: rel.qualifiers,
        });
      }
      const r = await api.reconcileRelationType(kbId, c.predicate_id);
      setDone((d) => ({
        ...d,
        [c.predicate_id]: S.ontology.uniquenessDone(r.corrected, r.conflicts),
      }));
      onChanged();
    } catch (e) {
      onError(e);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="space-y-4">
      <PageHeader className="mb-2" title={S.ontology.uniqueness} sub={S.ontology.uniquenessHint} />

      {pending ? (
        <p className="text-small text-ink-2">{S.nav.loading}</p>
      ) : candidates.length === 0 ? (
        <p className="text-small text-ink-2">{S.ontology.uniquenessEmpty}</p>
      ) : (
        <div className="space-y-2">
          {candidates.map((c) => (
            <div
              key={`${c.predicate_id}-${c.side}`}
              className="rounded-panel border border-line bg-surface px-3 py-3"
            >
              <div className="flex items-center gap-2">
                <span className="text-body text-ink">{c.label}</span>
                <span className="font-mono text-fine text-ink-2">
                  {c.key}
                </span>
                {c.declared && (
                  <Chip tone="neutral">{S.ontology.uniquenessDeclared}</Chip>
                )}
                <span className="ml-auto">
                  {done[c.predicate_id] ? (
                    <span className="text-small text-ink-2">
                      {done[c.predicate_id]}
                    </span>
                  ) : (
                    <Button variant="primary"
                      size="sm"
                      onClick={() => apply(c)}
                      disabled={busy === c.predicate_id}
                    >
                      {busy === c.predicate_id
                        ? S.ontology.uniquenessBusy
                        : c.declared
                          ? S.ontology.uniquenessReconcile
                          : S.ontology.uniquenessDeclare}
                    </Button>
                  )}
                </span>
              </div>

              <p className="mt-1 text-small text-ink-2">
                {c.side === "subject"
                  ? S.ontology.uniquenessSubject(c.holders)
                  : S.ontology.uniquenessObject(c.holders)}
                {" · "}
                {S.ontology.uniquenessEffect(c.would_close, c.would_review)}
              </p>

              {/* Evidence: who holds which values. **This is the basis for a person's judgment**, not decoration —
                  only by seeing "Zhang San leads Phoenix / Li Si leads Phoenix" does one know whether it should be declared */}
              {c.examples.length > 0 && (
                <div className="mt-2 space-y-1 border-t border-line pt-2">
                  {c.examples.slice(0, 3).map((ex) => (
                    <div
                      key={ex.holder}
                      className="flex items-baseline gap-2 text-small"
                    >
                      <span className="shrink-0 text-ink-2">
                        {ex.holder}
                      </span>
                      <span className="flex flex-wrap gap-x-3 gap-y-1 text-ink-2">
                        {ex.values.map((v) => (
                          <span key={v.fact_id}>
                            {v.name ?? "—"}
                            {v.valid_from && (
                              <span className="u-num ml-1 text-ink-2">
                                {S.ontology.uniquenessSince(
                                  v.valid_from.slice(0, 10),
                                )}
                              </span>
                            )}
                          </span>
                        ))}
                      </span>
                    </div>
                  ))}
                </div>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** How many proposals are still waiting for a person's response in the library: the criterion for waiting until the agent has answered, and also whether there is anything to look at */
function proposalCount(d: OntologyProposals | null | undefined): number {
  if (!d) return 0;
  return (
    (d.map_to?.length ?? 0) +
    d.entity_types.length +
    d.relation_types.length +
    (d.attribute_types?.length ?? 0)
  );
}

/** The extra columns an agent proposal carries (0061): the definition, the shapes it would bind, the category words, the questions it serves.
    A Suggest entry lacks proposed_by, so it draws nothing */
function AgentMeta({
  p,
}: {
  p: AgentProposalFields & { description?: string; forms?: string[] };
}) {
  if (p.proposed_by !== "agent") return null;
  const shapes = p.signatures?.phrases ?? [];
  const shapeText = shapes
    .map(
      (sh) =>
        `${sh.subject ?? "?"} — ${sh.phrase} → ${sh.value ? "value" : (sh.object ?? "?")}` +
        (sh.direction === "reverse" ? " (reverse)" : ""),
    )
    .join("\n");
  const examples = (p.examples ?? []).join("\n");
  return (
    <>
      <Chip tone="success" title={S.ontology.askAgentHint}>
        {S.ontology.agentChip}
      </Chip>
      {p.description && (
        <span className="text-small text-ink-2 truncate" title={p.description}>
          {p.description}
        </span>
      )}
      {shapes.length > 0 && (
        <span
          className="text-small text-accent whitespace-nowrap"
          title={examples ? `${shapeText}\n\n${examples}` : shapeText}
        >
          {S.ontology.bindsShapes(shapes.length)}
        </span>
      )}
      {!!p.kind_words?.length && (
        <span
          className="text-small text-accent whitespace-nowrap"
          title={p.kind_words.join(" · ")}
        >
          {S.ontology.bindsKindWords(p.kind_words.length)}
        </span>
      )}
      {!!p.serves?.length && (
        <Chip tone="info">{S.ontology.servesQuestions(p.serves.length)}</Chip>
      )}
      {!!p.closest?.length && (
        <span
          className="text-small text-ink-2 whitespace-nowrap"
          title={p.closest
            .map((c) => `${c.key} · ${c.label} · ${c.distance.toFixed(2)}`)
            .join("\n")}
        >
          {S.ontology.closestExisting(p.closest[0].key, p.closest[0].label)}
        </span>
      )}
    </>
  );
}

/** Capability question (0061 decision 1): a question the ontology should be able to answer. Written by a person it is accepted; retired ones are kept to look at */
function QuestionsPanel({
  kbId,
  onError,
}: {
  kbId: string;
  onError: (e: unknown) => void;
}) {
  const qc = useQueryClient();
  // After calling the agent, wait for it to ask questions: look again every few seconds, and stop once a proposed one appears
  const [awaiting, setAwaiting] = useState<{ since: number; had: number } | null>(null);
  const questions = useQuery({
    queryKey: ["questions", kbId],
    queryFn: () => api.listQuestions(kbId),
    refetchInterval: awaiting ? 4000 : false,
  });
  const report = useQuery({
    queryKey: ["questionReport", kbId],
    queryFn: () => api.questionReport(kbId),
  });
  useEffect(() => {
    if (!awaiting || !questions.data) return;
    const now = questions.data.filter((q) => q.status === "proposed").length;
    if (now > awaiting.had || Date.now() - awaiting.since > 120_000) setAwaiting(null);
  }, [awaiting, questions.data]);
  const [draft, setDraft] = useState("");
  const refresh = () => {
    qc.invalidateQueries({ queryKey: ["questions", kbId] });
    qc.invalidateQueries({ queryKey: ["questionReport", kbId] });
  };
  const askForQuestions = useMutation({
    mutationFn: () => api.proposeQuestions(kbId),
    onSuccess: () => {
      toast.info(S.ontology.questionsQueued);
      setAwaiting({
        since: Date.now(),
        had: (questions.data ?? []).filter((q) => q.status === "proposed").length,
      });
    },
    onError,
  });
  const decide = useMutation({
    mutationFn: ({ q, status }: { q: CompetencyQuestion; status: "accepted" | "rejected" }) =>
      api.updateQuestion(kbId, q.id, { status }),
    onSuccess: refresh,
    onError,
  });
  const add = useMutation({
    mutationFn: (question: string) => api.createQuestion(kbId, { question }),
    onSuccess: () => {
      toast.success(S.ontology.questionAdded);
      setDraft("");
      refresh();
    },
    onError,
  });
  const retire = useMutation({
    mutationFn: (q: CompetencyQuestion) =>
      api.updateQuestion(kbId, q.id, { status: "retired" }),
    onSuccess: () => {
      toast.success(S.ontology.questionRetired);
      refresh();
    },
    onError,
  });
  const remove = useMutation({
    mutationFn: (q: CompetencyQuestion) => api.deleteQuestion(kbId, q.id),
    onSuccess: refresh,
    onError,
  });
  const rows = (questions.data ?? []).filter((q) => q.status !== "rejected");
  const submit = () => {
    const text = draft.trim();
    if (text) add.mutate(text);
  };
  return (
    <div className="mt-6 border-t border-line pt-3">
      <div className="flex items-center gap-2">
        <h4 className="text-small font-semibold text-ink-2">{S.ontology.questions}</h4>
        {report.data && report.data.questions.checked > 0 && (
          <Chip tone="info" title={S.ontology.questionsScoreHint}>
            {S.ontology.questionsScore(
              report.data.questions.answered,
              report.data.questions.checked,
            )}
          </Chip>
        )}
        {report.data && report.data.questions.answered > 0 && (
          <Chip tone="neutral" title={S.ontology.viaGraphHint}>
            {S.ontology.viaGraph(
              report.data.questions.answered_with_graph,
              report.data.questions.answered_graph_only,
            )}
          </Chip>
        )}
        {report.data && report.data.proposals.decided > 0 && (
          <Chip tone="neutral" title={S.ontology.proposalsChangedHint}>
            {S.ontology.proposalsChanged(
              report.data.proposals.changed,
              report.data.proposals.decided,
            )}
          </Chip>
        )}
        <Button
          size="sm"
          variant="secondary"
          className="ml-auto"
          title={S.ontology.askQuestionsHint}
          onClick={() => askForQuestions.mutate()}
          disabled={askForQuestions.isPending || awaiting !== null}
        >
          {awaiting ? S.ontology.agentWaiting : S.ontology.askQuestions}
        </Button>
      </div>
      <p className="mb-2 text-small text-ink-2">{S.ontology.questionsHint}</p>
      <div className="mb-2 flex items-center gap-2">
        <Input
          size="sm"
          className="flex-1"
          value={draft}
          placeholder={S.ontology.questionPlaceholder}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") submit();
          }}
        />
        <Button
          size="sm"
          variant="secondary"
          onClick={submit}
          disabled={add.isPending || !draft.trim()}
        >
          {S.ontology.addQuestion}
        </Button>
      </div>
      {rows.length === 0 ? (
        <p className="text-body text-ink-2">{S.ontology.noQuestions}</p>
      ) : (
        <div className="space-y-1">
          {rows.map((q) => (
            <div key={q.id} className="flex items-center gap-2 text-body">
              <span
                className={cn(
                  "flex-1 min-w-0 truncate",
                  q.status === "retired" ? "text-ink-2 line-through" : "text-ink",
                )}
                title={q.question}
              >
                {q.question}
              </span>
              {q.origin === "agent" && (
                <Chip tone="success">{S.ontology.agentChip}</Chip>
              )}
              {q.last_result != null &&
                typeof q.last_result === "object" &&
                "answered" in q.last_result && (
                  <Chip
                    tone={(q.last_result as { answered?: boolean }).answered ? "success" : "warn"}
                  >
                    {(q.last_result as { answered?: boolean }).answered
                      ? S.ontology.questionAnswered
                      : S.ontology.questionUnanswered}
                    {S.ontology.questionVia[(q.last_result as { via?: string }).via ?? ""] &&
                      ` · ${S.ontology.questionVia[(q.last_result as { via?: string }).via ?? ""]}`}
                  </Chip>
                )}
              {q.status === "proposed" && (
                <>
                  <Button
                    size="sm"
                    variant="primary"
                    onClick={() => decide.mutate({ q, status: "accepted" })}
                    disabled={decide.isPending}
                  >
                    {S.ontology.acceptQuestion}
                  </Button>
                  <Button
                    size="sm"
                    variant="secondary"
                    onClick={() => decide.mutate({ q, status: "rejected" })}
                    disabled={decide.isPending}
                  >
                    {S.ontology.rejectProposal}
                  </Button>
                </>
              )}
              {q.status === "accepted" && (
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => retire.mutate(q)}
                  disabled={retire.isPending}
                >
                  {S.ontology.retireQuestion}
                </Button>
              )}
              <IconButton
                size="sm"
                label={S.ontology.deleteQuestion}
                onClick={() => remove.mutate(q)}
                disabled={remove.isPending}
              >
                <X size={13} />
              </IconButton>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function MissesPanel({
  kbId,
  onChanged,
  onError,
}: {
  kbId: string;
  onChanged: () => void;
  onError: (e: unknown) => void;
}) {
  // How much the documents say that the ontology still cannot hold (the two things the agent reads): an empty ontology does not mean "the corpus is covered"
  const uncovered = useQuery({
    queryKey: ["questionReport", kbId],
    queryFn: () => api.questionReport(kbId),
  }).data?.uncovered;
  const [proposals, setProposals] = useState<OntologyProposals | null>(null);
  // After calling the agent, wait for it: the task runs in the background, so look at the library every few seconds for new proposals (0061).
  // What is recorded is the count of proposals at the moment it was called — once there are more, it has finished answering; if there is no movement for two minutes, stop waiting
  const [awaitingAgent, setAwaitingAgent] = useState<{
    since: number;
    had: number;
  } | null>(null);
  // Proposals not yet responded to are stored in the library (0049): page refresh, or switching away and back, both pull them from here
  const storedProposals = useQuery({
    queryKey: ["storedProposals", kbId],
    queryFn: () => api.storedProposals(kbId),
    refetchInterval: awaitingAgent ? 4000 : false,
  });
  useEffect(() => {
    if (!awaitingAgent || !storedProposals.data) return;
    const d = storedProposals.data;
    if (
      proposalCount(d) > awaitingAgent.had ||
      Date.now() - awaitingAgent.since > 120_000
    ) {
      setProposals(d);
      setAwaitingAgent(null);
    }
  }, [awaitingAgent, storedProposals.data]);
  useEffect(() => {
    // Backfill only when there is no local result yet: a batch just responded to should not be overwritten by the old snapshot from the library
    if (proposals === null && storedProposals.data) {
      if (proposalCount(storedProposals.data) > 0) setProposals(storedProposals.data);
    }
  }, [storedProposals.data, proposals]);

  const askAgent = useMutation({
    mutationFn: () => api.proposeOntology(kbId),
    onSuccess: () => {
      toast.info(S.ontology.agentQueued);
      setAwaitingAgent({
        since: Date.now(),
        had: proposalCount(storedProposals.data ?? proposals),
      });
    },
    onError,
  });
  type Section = "map_to" | "entity_types" | "relation_types" | "attribute_types";
  const drop = (section: Section, key: string) =>
    setProposals(
      (prev) =>
        prev && {
          ...prev,
          [section]: (prev[section] ?? []).filter((x) => x.key !== key),
        },
    );
  // Rejecting, even with a reason: changing the status does not delete the row, and the reason is something the next round of the agent can read (0061)
  const rejectProposal = useMutation({
    mutationFn: ({
      section,
      key,
      reason,
    }: {
      section: Section;
      key: string;
      reason?: string;
    }) => api.decideProposal(kbId, section, key, "rejected", reason),
    onSuccess: (_d, v) => {
      toast.success(S.ontology.rejectedProposal);
      drop(v.section, v.key);
    },
    onError,
  });
  const reject = (section: Section, key: string) => {
    const reason = window.prompt(S.ontology.rejectReasonPrompt) ?? "";
    rejectProposal.mutate({ section, key, reason: reason.trim() || undefined });
  };
  // Adopting goes through one endpoint on the server: create the class or attribute (or write the person's binding), mark the proposal, queue alignment (0061 decision 3)
  const adopt = useMutation({
    mutationFn: ({ section, key }: { section: Section; key: string }) =>
      api.adoptProposal(kbId, section, key),
    onSuccess: (d, v) => {
      // When reusing, anything a person decided differently after the agent read it is kept as the person left it and not written: say so, otherwise once the card goes it looks as if everything was written
      if (d.skipped.length > 0) toast.info(S.ontology.adoptKept(d.skipped.length));
      else toast.success(v.section === "map_to" ? S.toast.saved : S.toast.added);
      drop(v.section, v.key);
      onChanged();
    },
    onError,
  });
  const actions = (section: Section, key: string, label: string) => (
    <>
      <Button
        variant="primary"
        size="sm"
        className="ml-auto"
        onClick={() => adopt.mutate({ section, key })}
        disabled={adopt.isPending}
      >
        {label}
      </Button>
      <Button
        size="sm"
        variant="secondary"
        onClick={() => reject(section, key)}
        disabled={rejectProposal.isPending}
      >
        {S.ontology.rejectProposal}
      </Button>
    </>
  );

  return (
    // The full-page view no longer wraps content in a card: the title is the page title, and the body is laid flat (the same look as Refine / Rules)
    <div>
      <PageHeader
        title={S.ontology.misses}
        sub={S.ontology.missesHint}
        actions={
          <Button
            size="sm"
            variant="secondary"
            title={S.ontology.askAgentHint}
            onClick={() => askAgent.mutate()}
            disabled={askAgent.isPending || awaitingAgent !== null}
          >
            {awaitingAgent ? S.ontology.agentWaiting : S.ontology.askAgent}
          </Button>
        }
      />

      {/* While the two numbers have not come back yet, say nothing: saying "nothing waiting" and then correcting it is worse than leaving it empty */}
      {uncovered && (
        <p className="text-body text-ink-2">
          {uncovered.kind_words + uncovered.phrases > 0
            ? S.ontology.uncovered(uncovered.kind_words, uncovered.phrases)
            : S.ontology.noMisses}
        </p>
      )}
      {proposals && (
        <div className="mt-4 border-t border-line pt-3">
          <h4 className="mb-2 text-small font-semibold text-ink-2">
            {S.ontology.proposals}
          </h4>
          <div className="space-y-2">
            {/* Placed first: it says "the ontology already has it", and that is the sentence most worth seeing first.
                If it sat after "create new", a person clicking straight down the list would end up creating duplicates */}
            {(proposals.map_to ?? []).map((p) => (
              <div key={`map-${p.key}`} className="flex items-center gap-2 text-body">
                <Chip tone="success">=</Chip>
                <span className="font-mono text-ink-2">{p.key}</span>
                {p.label && <span className="text-ink">{p.label}</span>}
                {!!p.forms?.length && (
                  <span
                    className="text-small text-ink-2 truncate"
                    title={p.forms.join(" · ")}
                  >
                    {p.forms.join(" · ")}
                  </span>
                )}
                {p.reason && (
                  <span className="text-small text-ink-2 truncate">{p.reason}</span>
                )}
                <AgentMeta p={p} />
                {actions("map_to", p.key, S.ontology.mapOver)}
              </div>
            ))}
            {proposals.entity_types.map((p) => (
              <div key={p.key} className="flex items-center gap-2 text-body">
                <Chip tone="info">C</Chip>
                <span className="font-mono text-ink-2">{p.key}</span>
                <span className="text-ink">{p.label}</span>
                {p.reason && (
                  <span className="text-small text-ink-2 truncate">{p.reason}</span>
                )}
                <AgentMeta p={p} />
                {actions("entity_types", p.key, S.ontology.approve)}
              </div>
            ))}
            {proposals.relation_types.map((p) => (
              <div key={p.key} className="flex items-center gap-2 text-body">
                <Chip tone="violet">P</Chip>
                <span className="font-mono text-ink-2">{p.key}</span>
                <span className="text-ink">{p.label}</span>
                {p.temporal && <Chip tone="neutral">{p.temporal}</Chip>}
                {p.reason && (
                  <span className="text-small text-ink-2 truncate">{p.reason}</span>
                )}
                <AgentMeta p={p} />
                {actions("relation_types", p.key, S.ontology.approve)}
              </div>
            ))}
            {(proposals.attribute_types ?? []).map((p) => (
              <div key={`attr-${p.key}`} className="flex items-center gap-2 text-body">
                {/* A rather than P: the literal-value tier is a different thing from relations, and the interface keeps them apart */}
                <Chip tone="warn">A</Chip>
                <span className="font-mono text-ink-2">{p.key}</span>
                <span className="text-ink">{p.label}</span>
                <Chip tone="neutral">{p.datatype ?? "text"}</Chip>
                {p.unit && <Chip tone="neutral">{p.unit}</Chip>}
                {p.reason && (
                  <span className="text-small text-ink-2 truncate">{p.reason}</span>
                )}
                <AgentMeta p={p} />
                {actions("attribute_types", p.key, S.ontology.approve)}
              </div>
            ))}
            {proposalCount(proposals) === 0 && (
              <p className="text-body text-ink-2">—</p>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

/* ---------- Ontology import: upload → preview plan → confirm and write ---------- */
/* Preview and commit share the same plan from the server. This panel's entire job is to put
   **three things that can bite** in front of the person before they confirm: functional relations (a wrong uniqueness declaration creates a crowd of
   false conflicts), classes without a description (description goes verbatim into the extraction prompt, so if it is missing, extraction silently goes wrong), and
   key collisions (reported, not resolved — an automatic rename would make the next re-import unable to tell which one it created last time). */

function ImportPanel({
  kbId,
  onChanged,
  onError,
}: {
  kbId: string;
  onChanged: () => void;
  onError: (e: unknown) => void;
}) {
  const [file, setFile] = useState<File | null>(null);
  const pick = useRef<HTMLInputElement>(null);
  const queryClient = useQueryClient();

  const preview = useMutation({
    mutationFn: (f: File) => api.previewOntologyImport(kbId, f),
    onError: (e) => {
      setFile(null);
      onError(e);
    },
  });

  const apply = useMutation({
    mutationFn: (f: File) => api.applyOntologyImport(kbId, f),
    onSuccess: (res) => {
      const p = res.plan;
      toast.success(
        S.ontology.importDone(
          p.classes.filter((c) => c.disposition === "create").length,
          p.classes.filter((c) => c.disposition === "update").length,
        ),
      );
      setFile(null);
      preview.reset();
      queryClient.invalidateQueries({ queryKey: ["ontology-imports", kbId] });
      onChanged();
    },
    onError,
  });

  const choose = (f: File | undefined) => {
    if (!f) return;
    setFile(f);
    preview.mutate(f);
  };

  const plan = preview.data?.plan ?? null;
  const busy = preview.isPending || apply.isPending;
  const empty =
    plan &&
    plan.classes.length === 0 &&
    plan.relations.length === 0 &&
    plan.attributes.length === 0;

  return (
    <div>
      <PageHeader title={S.ontology.importTitle} sub={S.ontology.importHint} />

      <input
        ref={pick}
        type="file"
        accept=".owl,.rdf,.ttl,.xml,.n3"
        className="hidden"
        onChange={(e) => {
          choose(e.target.files?.[0]);
          e.target.value = "";
        }}
      />
      <div className="flex items-center gap-2">
        <Button
          size="sm"
          variant="secondary"
          disabled={busy}
          onClick={() => pick.current?.click()}
        >
          {file ? S.ontology.importChange : S.ontology.importPick}
        </Button>
        {file && (
          <span className="text-small text-ink-2 truncate">
            <span className="font-mono">{file.name}</span>
            <span className="text-ink-2">
              {" "}
              · {S.ontology.importSize(file.size)}
            </span>
          </span>
        )}
        {preview.isPending && (
          <span className="text-small text-ink-2">
            {S.ontology.importReading}
          </span>
        )}
      </div>

      {plan && (
        <div className="mt-4">
          <p className="u-num text-fine text-ink-2">
            {S.ontology.importParsed(plan.format, plan.triples)}
          </p>

          {empty ? (
            <p className="mt-2 text-body text-ink-2">
              {S.ontology.importNothing}
            </p>
          ) : (
            <>
              {/* The three warnings come before the counts: people only read the first screen */}
              <Warning
                show={plan.functional_relations > 0}
                tone="warn"
                title={S.ontology.warnFunctional(plan.functional_relations)}
                body={S.ontology.warnFunctionalBody}
                items={plan.relations
                  .filter((r) => r.functional)
                  .map((r) => r.key)}
              />
              <Warning
                show={plan.classes_without_description > 0}
                tone="warn"
                title={S.ontology.warnNoDescription(
                  plan.classes_without_description,
                )}
                body={S.ontology.warnNoDescriptionBody}
                items={plan.classes
                  .filter((c) => !c.has_description)
                  .map((c) => c.key)}
              />
              <Warning
                show={takenCount(plan) > 0}
                tone="danger"
                title={S.ontology.warnKeyTaken(takenCount(plan))}
                body={S.ontology.warnKeyTakenBody}
                items={[...plan.classes, ...plan.relations, ...plan.attributes]
                  .filter((i) => i.disposition === "key_taken")
                  .map(
                    (i) =>
                      `${i.key} — ${S.ontology.importTakenBy(i.conflict_with ?? null)}`,
                  )}
              />

              <div className="mt-3 grid gap-2">
                <PlanRow
                  label={S.ontology.importClasses}
                  items={plan.classes}
                />
                <PlanRow
                  label={S.ontology.importRelations}
                  items={plan.relations}
                />
                <PlanRow
                  label={S.ontology.importAttributes}
                  items={plan.attributes}
                  note={
                    plan.attributes.length > 0
                      ? S.ontology.importAttributesLater
                      : undefined
                  }
                />
              </div>

              {plan.unprojected.length > 0 && (
                <Disclosure
                  className="mt-3"
                  summary={
                    <>
                      {S.ontology.importUnprojected} ({plan.unprojected.length})
                    </>
                  }
                >
                  <p className="mt-2 text-fine text-ink-2">
                    {S.ontology.importUnprojectedBody}
                  </p>
                  <ul className="mt-2 space-y-1">
                    {plan.unprojected.map(([iri, n]) => (
                      <li key={iri} className="flex gap-2 text-fine">
                        <span
                          className="font-mono text-ink-2 truncate"
                          title={iri}
                        >
                          {shortIri(iri)}
                        </span>
                        <span className="u-num text-ink-2 shrink-0">
                          ×{n}
                        </span>
                      </li>
                    ))}
                  </ul>
                </Disclosure>
              )}

              <div className="mt-4 flex items-center gap-2">
                <Button variant="primary"
                  size="sm"
                  disabled={busy}
                  onClick={() => file && apply.mutate(file)}
                >
                  {apply.isPending
                    ? S.ontology.importApplying
                    : S.ontology.importApply}
                </Button>
                <Button
                  size="sm"
                  variant="secondary"
                  disabled={busy}
                  onClick={() => {
                    setFile(null);
                    preview.reset();
                  }}
                >
                  {S.ontology.importCancel}
                </Button>
              </div>
            </>
          )}
        </div>
      )}

      {/* Who touched the ontology, when, with which file, and what actually came in that time */}
      <ImportHistory kbId={kbId} />
    </div>
  );
}

/** Import history: **the server records a whole ledger for every import** — how many classes were created, how many updated,
 *  how many keys were taken, how many inverse properties were linked, how many attributes were skipped, how many triples, and which IRIs were not projected
 *  down. Previously this section only printed "file name · size · who on which day", and everything else fell on the floor:
 *  after importing, there was nowhere on the interface that could say "what actually came in".
 *
 *  So this is a table (one row per import's ledger), with the detailed ledger in the details at the end of the row: those numbers are only read
 *  when something goes wrong, and they should not take up table width. */
function ImportHistory({ kbId }: { kbId: string }) {
  const [detail, setDetail] = useState<OntologyImportView | null>(null);
  const history = useQuery({
    queryKey: ["ontology-imports", kbId],
    queryFn: () => api.ontologyImports(kbId),
  });
  const rows = history.data?.imports ?? [];

  return (
    <div className="mt-6">
      <h4 className="mb-2 text-small font-medium text-ink-2">
        {S.ontology.importHistory}
      </h4>
      {/* While still fetching, it must not say "never imported" — that statement is false, and it looks exactly like
          the true case of never having imported, so the reader cannot tell which one they are seeing */}
      {history.isLoading ? (
        <p className="text-small text-ink-2">{S.nav.loading}</p>
      ) : !rows.length ? (
        <p className="text-small text-ink-2">{S.ontology.importNoHistory}</p>
      ) : (
        <div className="glass overflow-hidden rounded-panel">
          <Table>
            <THead>
              <Tr>
                <Th>{S.ontology.importColFile}</Th>
                <Th>{S.ontology.importColFormat}</Th>
                <Th>{S.ontology.importColSize}</Th>
                <Th>{S.ontology.importClasses}</Th>
                <Th>{S.ontology.importRelations}</Th>
                <Th>{S.ontology.importAttributes}</Th>
                <Th>{S.ontology.importColTriples}</Th>
                <Th>{S.ontology.importColWhen}</Th>
                <Th />
              </Tr>
            </THead>
            <TBody>
              {rows.map((im) => {
                const s = im.summary ?? {};
                return (
                  <Tr key={im.id}>
                    {/* One fact per cell, one import per row. **Format and size each take a column**, rather than
                        sitting under the file name — stacking would make every row two lines tall while the right side stayed half-empty.
                        The file name does not need monospace: monospace is for keys, ids, code and URLs
                        (DESIGN.md 1); a file name in a column of Geist just looks out of place */}
                    <Td>
                      <div className="max-w-64 truncate text-small text-ink" title={im.filename}>
                        {im.filename}
                      </div>
                    </Td>
                    <Td className="text-small text-ink-2">{im.format}</Td>
                    <Td className="u-num whitespace-nowrap text-small text-ink-2">
                      {S.ontology.importSize(im.byte_size)}
                    </Td>
                    <Td className="text-small text-ink-2">
                      <Counts
                        created={s.classes_created}
                        updated={s.classes_updated}
                        taken={s.classes_key_taken}
                      />
                    </Td>
                    <Td className="text-small text-ink-2">
                      <Counts created={s.relations_created} updated={s.relations_updated} />
                    </Td>
                    <Td className="text-small text-ink-2">
                      <Counts
                        created={s.attributes_created}
                        skipped={sumCounts(s.attributes_skipped)}
                      />
                    </Td>
                    <Td className="u-num text-small text-ink-2">{s.triples ?? "—"}</Td>
                    <Td className="text-small text-ink-2 whitespace-nowrap">
                      {S.ontology.importBy(
                        im.imported_by_name ?? "—",
                        localDateTime(im.imported_at).slice(0, 16),
                      )}
                    </Td>
                    <Td className="text-right">
                      <LinkButton onClick={() => setDetail(im)}>
                        {S.ontology.importDetail}
                      </LinkButton>
                    </Td>
                  </Tr>
                );
              })}
            </TBody>
          </Table>
        </div>
      )}

      {/* Detailed ledger: only read when something goes wrong, so it is folded away here rather than laid out on the table */}
      <Dialog
        open={!!detail}
        onOpenChange={(o) => !o && setDetail(null)}
        width="lg"
        closeLabel={S.ui.close}
        title={detail?.filename ?? ""}
        description={
          detail
            ? S.ontology.importBy(
                detail.imported_by_name ?? "—",
                localDateTime(detail.imported_at),
              )
            : undefined
        }
      >
        {detail && <ImportDetail im={detail} />}
      </Dialog>
    </div>
  );
}

/** `{no_domain: 3, unknown_domain: 1}` → 4. Missing means 0 */
function sumCounts(m: Record<string, number> | undefined): number {
  return Object.values(m ?? {}).reduce((a, b) => a + b, 0);
}

/** One cell in the table: how many were created / updated / keys taken / skipped. All zeros show as a dash */
function Counts({
  created,
  updated,
  taken,
  skipped,
}: {
  created?: number;
  updated?: number;
  taken?: number;
  skipped?: number;
}) {
  const parts = [
    created ? S.ontology.importCreatedN(created) : null,
    updated ? S.ontology.importUpdatedN(updated) : null,
    skipped ? S.ontology.importSkippedN(skipped) : null,
    taken ? S.ontology.importTakenN(taken) : null,
  ].filter(Boolean);
  return <>{parts.length ? parts.join(" · ") : "—"}</>;
}

function ImportDetail({ im }: { im: OntologyImportView }) {
  const s = im.summary ?? {};
  const stat = (label: string, n: number | undefined) =>
    n === undefined ? null : (
      <div key={label} className="flex items-baseline justify-between gap-3 py-1">
        <span className="text-small text-ink-2">{label}</span>
        <span className="u-num text-small text-ink">{n}</span>
      </div>
    );
  const L = S.ontology;
  return (
    <div className="space-y-4">
      <div className="grid grid-cols-2 gap-x-6">
        <div className="divide-y divide-line">
          {stat(L.statClassesCreated, s.classes_created)}
          {stat(L.statClassesUpdated, s.classes_updated)}
          {stat(L.statClassesTaken, s.classes_key_taken)}
          {stat(L.statClassesNoDesc, s.classes_without_description)}
          {stat(L.statTriples, s.triples)}
        </div>
        <div className="divide-y divide-line">
          {stat(L.statRelationsSeen, s.relations_seen)}
          {stat(L.statRelationsCreated, s.relations_created)}
          {stat(L.statRelationsUpdated, s.relations_updated)}
          {stat(L.statFunctional, s.functional_relations)}
          {stat(L.statInverseLinked, s.inverse_linked)}
          {stat(L.statSubPropertyLinked, s.sub_property_linked)}
          {stat(L.statAttributesSeen, s.attributes_seen)}
          {stat(L.statAttributesCreated, s.attributes_created)}
          {/* Skipped properties **grouped by reason**: a number only says "this many fewer", while the reason says
              where in the ontology to change next (no domain written, domain not in this library, range unusable…) */}
          {Object.entries(s.attributes_skipped ?? {}).map(([reason, n]) =>
            stat(
              `${L.statAttributesSkipped} · ${S.ontology.skipReason[reason] ?? reason}`,
              n,
            ),
          )}
        </div>
      </div>

      {/* IRIs not projected: referencing external vocabularies is normal, but "how many links were missed" should be stated */}
      {!!s.unprojected?.length && (
        <div>
          <p className="mb-1 text-small font-medium text-ink-2">
            {S.ontology.importUnprojected} ({s.unprojected.length})
          </p>
          <p className="mb-2 text-fine leading-relaxed text-ink-2">
            {S.ontology.importUnprojectedBody}
          </p>
          <ul className="u-scroll max-h-48 space-y-1 overflow-y-auto">
            {s.unprojected.map(([iri, n]) => (
              <li key={iri} className="flex gap-2 text-fine">
                <span className="min-w-0 truncate font-mono text-ink-2" title={iri}>
                  {shortIri(iri)}
                </span>
                <span className="u-num shrink-0 text-ink-2">×{n}</span>
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}

function takenCount(p: ImportPlan) {
  return [...p.classes, ...p.relations, ...p.attributes].filter(
    (i) => i.disposition === "key_taken",
  ).length;
}

/** The tail of an IRI is the part people recognise; the prefix only takes up width in the list */
function shortIri(iri: string) {
  const i = Math.max(iri.lastIndexOf("#"), iri.lastIndexOf("/"));
  return i < 0 ? iri : iri.slice(i + 1);
}

/** One warning: the title gives the number, one sentence in the body gives the consequence, and the entries fold in details */
function Warning({
  show,
  tone,
  title,
  body,
  items,
}: {
  show: boolean;
  tone: "warn" | "danger";
  title: string;
  body: string;
  items: string[];
}) {
  if (!show) return null;
  return (
    <div
      className={cn(
        "mt-3 rounded-panel border px-3 py-3",
        tone === "danger"
          ? "border-danger/25 bg-danger/[0.06]"
          : "border-warn/25 bg-warn/[0.06]",
      )}
    >
      <p className="text-small text-ink">{title}</p>
      <p className="mt-1 text-fine text-ink-2">{body}</p>
      {items.length > 0 && (
        <Disclosure
          className="mt-2"
          summary={items.length > 1 ? `${items.length} items` : "1 item"}
        >
          <ul className="mt-1 space-y-1">
            {items.map((it) => (
              <li key={it} className="font-mono text-fine text-ink-2">
                {it}
              </li>
            ))}
          </ul>
        </Disclosure>
      )}
    </div>
  );
}

/** Destination count for a section: created / updated / skipped, zeros not shown */
function PlanRow({
  label,
  items,
  note,
}: {
  label: string;
  items: PlannedItem[];
  note?: string;
}) {
  if (items.length === 0) return null;
  const n = (d: PlannedItem["disposition"]) =>
    items.filter((i) => i.disposition === d).length;
  return (
    <div className="rounded-panel bg-surface px-3 py-2">
      <div className="flex items-center gap-2">
        <span className="text-small text-ink-2">{label}</span>
        <span className="ml-auto flex items-center gap-2">
          {n("create") > 0 && (
            <Chip tone="success">
              {S.ontology.importWillCreate(n("create"))}
            </Chip>
          )}
          {n("update") > 0 && (
            <Chip tone="info">{S.ontology.importWillUpdate(n("update"))}</Chip>
          )}
          {n("key_taken") > 0 && (
            <Chip tone="neutral">
              {S.ontology.importKeyTaken(n("key_taken"))}
            </Chip>
          )}
        </span>
      </div>
      {note && <p className="mt-1 text-fine text-ink-2">{note}</p>}
    </div>
  );
}
