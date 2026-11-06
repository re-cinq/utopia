import { useEffect, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useNavigate, useSearch } from "@tanstack/react-router";
import { Workflow } from "lucide-react";
import { api } from "../api";
import { S } from "../i18n";
import { useKbId } from "../kb";
import { EmptyState, PageHeader, Panel, Row, Skeleton } from "../ui";
import { chainColumns, frontierIds, hopsFrom, type ChainCard, type ChainColumn } from "./hinted/chain";
import { HintedHeader } from "./hinted/HintedPanel";
import { hintsOf, type Hints } from "./hinted/hints";
import { layoutFor } from "./hinted/layout";
import { useDetails } from "./hinted/useDetails";

/** Follows `chain_first` and each `chain_next` from one entity, a column per hop (RC-008) */
export function Chain() {
  const kbId = useKbId();
  const { entity: startId } = useSearch({ from: "/app/kb/$kbId/chain" });
  const navigate = useNavigate();
  const ontology = useQuery({ queryKey: ["ontology", kbId], queryFn: () => api.ontology(kbId) });
  const hints = useMemo(() => (ontology.data ? hintsOf(ontology.data) : null), [ontology.data]);
  const chain = useChain(kbId, startId ?? "", hints);
  const openInGraph = (id: string) =>
    navigate({ to: "/kb/$kbId/graph", params: { kbId }, search: { entity: id } });

  const start = chain.start;
  return (
    <div className="px-8 py-6">
      <PageHeader
        title={S.chain.title}
        sub={start ? S.chain.from(start.entity.name) : undefined}
      />
      {!startId ? (
        <EmptyState icon={<Workflow size={20} />}>{S.chain.noStart}</EmptyState>
      ) : !start || !hints ? (
        <Skeleton className="h-24" />
      ) : chain.columns.length === 0 ? (
        <EmptyState icon={<Workflow size={20} />}>{S.chain.noHints}</EmptyState>
      ) : (
        <div className="grid grid-cols-4 gap-6">
          {chain.columns.map((column, i) => (
            <ChainColumnView key={i} column={column} hints={hints} onOpen={openInGraph} />
          ))}
        </div>
      )}
    </div>
  );
}

function useChain(kbId: string, startId: string, hints: Hints | null) {
  const [ids, setIds] = useState<string[]>([]);
  const fetched = useDetails(kbId, [startId, ...ids].filter(Boolean));
  const start = fetched.get(startId);
  const columns = start && hints
    ? chainColumns(startId, hopsFrom(start.entity.type_key, hints), fetched, hints)
    : [];
  const frontier = frontierIds(columns).join(",");
  useEffect(() => {
    if (!frontier) return;
    setIds((known) => [...new Set([...known, ...frontier.split(",")])]);
  }, [frontier]);
  return { start, columns };
}

export function ChainColumnView({
  column,
  hints,
  onOpen,
}: {
  column: ChainColumn;
  hints: Hints;
  onOpen: (entityId: string) => void;
}) {
  return (
    <section className="flex min-w-0 flex-col gap-2">
      <div className="text-small font-medium text-ink-2">
        {column.hop.dir === "in" ? `← ${column.label}` : column.label}
      </div>
      {column.state === "ok" ? (
        <Panel className="py-1">
          {column.cards.map((card) => (
            <ChainCardRow key={card.id} card={card} hints={hints} onOpen={onOpen} />
          ))}
        </Panel>
      ) : (
        <EmptyState icon={<Workflow size={20} />}>
          {column.state === "none" ? S.chain.emptyNone(column.label) : S.chain.emptyUnloaded}
        </EmptyState>
      )}
    </section>
  );
}

function ChainCardRow({
  card,
  hints,
  onOpen,
}: {
  card: ChainCard;
  hints: Hints;
  onOpen: (entityId: string) => void;
}) {
  return (
    <Row title={S.chain.openInGraph} onClick={() => onOpen(card.id)}>
      {card.loaded ? (
        <HintedHeader layout={layoutFor(card.loaded.entity, card.loaded.facts, hints)} compact />
      ) : (
        card.name
      )}
    </Row>
  );
}
