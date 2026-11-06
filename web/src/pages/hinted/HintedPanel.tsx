import { Fragment, useEffect, useMemo, useState, type ReactNode } from "react";
import { ExternalLink } from "lucide-react";
import type { EntityFact, GraphNode } from "../../api";
import { S } from "../../i18n";
import { QuotedText } from "../../ui/citation";
import { GroupLabel, IconButton, LinkButton, Row, Skeleton, Status } from "../../ui";
import { attrsOf, type Hints } from "./hints";
import { crumbTrail, layoutFor, rangeTextOf, type Crumb, type Layout } from "./layout";
import { nestBy, type NestNode } from "./nest";
import { fmtRanges, fmtSpan, parseRanges, spanOf } from "./span";
import { factsById, useDetails } from "./useDetails";

/** The panel body a type's hints ask for (RC-008); every fact row is still the caller's */
export function HintedPanel({
  kbId,
  entity,
  facts,
  hints,
  renderRow,
  onNavigate,
  onOpenChain,
}: {
  kbId: string;
  entity: GraphNode;
  facts: EntityFact[];
  hints: Hints;
  renderRow: (fact: EntityFact) => ReactNode;
  onNavigate: (entityId: string) => void;
  onOpenChain?: () => void;
}) {
  const layout = useMemo(() => layoutFor(entity, facts, hints), [entity, facts, hints]);
  return (
    <div className="flex flex-col gap-4 px-2 pb-2">
      {layout.crumbs.length > 0 && (
        <Crumbs kbId={kbId} first={layout.crumbs[0]} hints={hints} onNavigate={onNavigate} />
      )}
      <HintedHeader layout={layout} />
      {layout.chainFirst && onOpenChain && (
        <div>
          <LinkButton onClick={onOpenChain}>{S.hinted.openChain}</LinkButton>
        </div>
      )}
      {layout.childrenKey && (
        <NestedList
          kbId={kbId}
          facts={facts}
          childrenKey={layout.childrenKey}
          hints={hints}
          onNavigate={onNavigate}
        />
      )}
      {layout.groups.map((group) => (
        <section key={group.key}>
          <GroupLabel count={group.facts.length}>
            {group.direction === "in" ? `← ${group.label}` : group.label}
          </GroupLabel>
          {group.facts.map((fact) => {
            const ranges = group.ranged ? rangeTextOf(fact, hints) : null;
            return (
              <div key={fact.id}>
                {renderRow(fact)}
                {ranges && (
                  <p className="px-2 text-fine text-ink-2 u-num">
                    {S.hinted.ranges}: {fmtRanges(parseRanges(ranges))}
                  </p>
                )}
              </div>
            );
          })}
        </section>
      ))}
    </div>
  );
}

/** Title, status, span, keys and link, styled by the type's `panel` hint.
 *  `compact` keeps to inline elements so it can sit inside a row */
export function HintedHeader({ layout, compact }: { layout: Layout; compact?: boolean }) {
  const meta = (
    <>
      {layout.status && <Status tone={layout.status.tone}>{layout.status.value}</Status>}
      {layout.span && <span className="u-num">{fmtSpan(layout.span)}</span>}
      {layout.keys.map((key) => (
        <span key={key} className="font-mono">
          {key}
        </span>
      ))}
    </>
  );
  if (compact) {
    return (
      <span className="flex min-w-0 items-center gap-2">
        <span className={layout.panel === "tree" ? "truncate font-mono" : "truncate"}>
          {layout.panel === "quote" && layout.quote ? `“${layout.quote}”` : layout.title}
        </span>
        <span className="flex shrink-0 items-center gap-2 text-fine text-ink-2">{meta}</span>
      </span>
    );
  }
  return (
    <div className="flex flex-col gap-2">
      {layout.panel === "quote" ? (
        <>
          {layout.quote && layout.title !== layout.quote && (
            <div className="text-small text-ink-2">{layout.title}</div>
          )}
          <blockquote className="border-l-2 border-line-strong pl-3 text-title text-ink">
            <QuotedText text={layout.quote ?? layout.title} />
          </blockquote>
        </>
      ) : (
        <div
          className={
            layout.panel === "tree"
              ? "font-mono text-body text-ink"
              : "text-title font-medium text-ink"
          }
        >
          {layout.title}
        </div>
      )}
      <div className="flex flex-wrap items-center gap-2 text-small text-ink-2">
        {meta}
        {layout.link && (
          <IconButton
            size="sm"
            label={S.hinted.openLink}
            onClick={() => window.open(layout.link!, "_blank", "noopener")}
          >
            <ExternalLink size={14} />
          </IconButton>
        )}
      </div>
    </div>
  );
}

/** The `children` relation as a tree; a child's own children are fetched when asked for */
export function NestedList({
  kbId,
  facts,
  childrenKey,
  hints,
  onNavigate,
}: {
  kbId: string;
  facts: EntityFact[];
  childrenKey: string;
  hints: Hints;
  onNavigate: (entityId: string) => void;
}) {
  const [expanded, setExpanded] = useState<string[]>([]);
  const loaded = factsById(useDetails(kbId, expanded));
  const nodes = nestBy(facts, childrenKey, loaded);
  const expand = (id: string) => setExpanded((ids) => [...ids, id]);

  const rows = (level: NestNode[], depth: number): ReactNode =>
    level.map((node) => {
      const own = loaded.get(node.id);
      const span = own ? spanOf(attrsOf(own), hints) : null;
      return (
        <Fragment key={node.id}>
          <Row
            indent={depth}
            trailing={span ? <span className="u-num">{fmtSpan(span)}</span> : undefined}
            onClick={() => onNavigate(node.id)}
          >
            {node.name}
          </Row>
          {node.children === "unloaded" ? (
            expanded.includes(node.id) ? (
              <Skeleton className="mx-2 h-4" />
            ) : (
              <Row indent={depth + 1} onClick={() => expand(node.id)}>
                {S.hinted.loadChildren}
              </Row>
            )
          ) : (
            rows(node.children, depth + 1)
          )}
        </Fragment>
      );
    });

  return (
    <section>
      <GroupLabel>{S.hinted.children(nodes.length)}</GroupLabel>
      {nodes.length === 0 ? (
        <p className="px-2 text-small text-ink-2">{S.hinted.noNested}</p>
      ) : (
        rows(nodes, 0)
      )}
    </section>
  );
}

/** The entities this one sits within, walked upward along `membership` */
export function Crumbs({
  kbId,
  first,
  hints,
  onNavigate,
}: {
  kbId: string;
  first: Crumb;
  hints: Hints;
  onNavigate: (entityId: string) => void;
}) {
  const [ids, setIds] = useState<string[]>([first.id]);
  const loaded = factsById(useDetails(kbId, ids));
  const trail = crumbTrail(first, loaded, hints);
  const top = trail[0].id;
  useEffect(() => {
    setIds((known) => (known.includes(top) ? known : [...known, top]));
  }, [top]);
  return (
    <nav className="flex flex-wrap items-center gap-1 text-small text-ink-2">
      <span>{S.hinted.within}</span>
      {trail.map((crumb, i) => (
        <Fragment key={crumb.id}>
          {i > 0 && <span aria-hidden>›</span>}
          <LinkButton onClick={() => onNavigate(crumb.id)}>{crumb.name}</LinkButton>
        </Fragment>
      ))}
    </nav>
  );
}
