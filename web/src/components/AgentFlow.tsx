import type { CollabBoardCard, CollabBoardColumn } from "../api";
import { useI18n, statusLabel } from "../i18n";

export interface AgentFlowCard {
  card: CollabBoardCard;
  lane: string;
}

export interface AgentFlowGroup {
  owner: string;
  cards: AgentFlowCard[];
}

const ARCHIVED = new Set(["closed", "merged", "done"]);
export const MAX_FLOW_CARDS = 60;
const MAX_ANIMATED_EDGES = 30;

/** Group current board cards by their explicit owner; empty owner is unassigned. */
export function agentFlowGroups(columns: CollabBoardColumn[]): AgentFlowGroup[] {
  // One card may be listed twice: once in an `unverified` alert lane and once in
  // its workflow lane (`build_board` alert lanes never consume a card). The
  // projection always emits an alert lane *before* the workflow lane that
  // places the card, so the last column listing a card id is the placement
  // lane — render only that occurrence, keeping card counts, React keys and
  // flow edges unique.
  const placement = new Map<string, CollabBoardColumn>();
  for (const col of columns) {
    for (const card of col.cards) {
      if (!ARCHIVED.has(card.status)) placement.set(card.id, col);
    }
  }
  const groups = new Map<string, AgentFlowGroup>();
  for (const col of columns) {
    for (const card of col.cards) {
      if (placement.get(card.id) !== col) continue;
      const owner = card.owner.trim() || "unassigned";
      const group = groups.get(owner) ?? { owner, cards: [] };
      group.cards.push({ card, lane: col.name });
      groups.set(owner, group);
    }
  }
  return [...groups.values()].toSorted((a, b) => a.owner.localeCompare(b.owner));
}

/** Keep the SVG bounded; callers show a truncation notice when this drops cards. */
export function limitAgentFlowGroups(groups: AgentFlowGroup[], maxCards: number): AgentFlowGroup[] {
  const limited: AgentFlowGroup[] = [];
  let remaining = maxCards;
  for (const group of groups) {
    if (remaining <= 0) break;
    const cards = group.cards.slice(0, remaining);
    if (cards.length > 0) limited.push({ owner: group.owner, cards });
    remaining -= cards.length;
  }
  return limited;
}

function short(s: string, n: number): string {
  return s.length > n ? `${s.slice(0, n - 1)}…` : s;
}

function statusClass(status: string): string {
  return `status-${status.replace(/[^a-z0-9_-]/gi, "-").toLowerCase()}`;
}

/**
 * A live projection of the board's owner→issue relations. It is deliberately
 * SVG-only: no layout engine, no backend state, and the existing board live
 * refresh drives updates.
 */
export function AgentFlow({ columns }: { columns: CollabBoardColumn[] }) {
  const { t } = useI18n();
  const allGroups = agentFlowGroups(columns);
  const totalCards = allGroups.reduce((n, group) => n + group.cards.length, 0);
  const groups = limitAgentFlowGroups(allGroups, MAX_FLOW_CARDS);
  const cards = groups.flatMap((g) => g.cards);
  if (cards.length === 0) return <div className="agent-flow-empty">{t("board.flow.empty")}</div>;

  const width = 760;
  const left = 20;
  const agentWidth = 170;
  const issueLeft = 390;
  const issueWidth = 350;
  const top = 40;
  const row = 62;
  const height = Math.max(180, top * 2 + cards.length * row);
  const cardY = new Map<string, number>();
  let rowIndex = 0;
  for (const group of groups) {
    for (const item of group.cards) cardY.set(item.card.id, top + rowIndex++ * row);
  }
  const agentY = new Map<string, number>();
  for (const group of groups) {
    const ys = group.cards.map((item) => cardY.get(item.card.id) ?? top);
    agentY.set(group.owner, ys.reduce((a, b) => a + b, 0) / ys.length);
  }

  return (
    <div className={`agent-flow-wrap ${totalCards > MAX_ANIMATED_EDGES ? "agent-flow-static" : ""}`}>
      {totalCards > MAX_FLOW_CARDS && (
        <div className="agent-flow-truncated">
          {t("board.flow.truncated", { shown: MAX_FLOW_CARDS, total: totalCards })}
        </div>
      )}
      <svg className="agent-flow-svg" viewBox={`0 0 ${width} ${height}`} role="img" aria-label={t("board.flow.aria")}>
        {groups.flatMap((group) =>
          group.cards.map((item) => {
            const ay = agentY.get(group.owner) ?? top;
            const by = cardY.get(item.card.id) ?? top;
            return (
              <path
                key={`${group.owner}-${item.card.id}`}
                className={`agent-flow-edge ${statusClass(item.card.status)}`}
                d={`M ${left + agentWidth} ${ay} C ${left + agentWidth + 70} ${ay}, ${issueLeft - 70} ${by}, ${issueLeft} ${by}`}
              />
            );
          }),
        )}

        {groups.map((group) => {
          const y = agentY.get(group.owner) ?? top;
          const label = group.owner === "unassigned" ? t("board.flow.unassigned") : group.owner;
          return (
            <g key={group.owner} className="agent-flow-node agent-flow-agent">
              <title>{label}</title>
              <rect x={left} y={y - 23} width={agentWidth} height={46} rx={10} />
              <text x={left + 12} y={y - 3}>{short(label, 24)}</text>
              <text className="agent-flow-muted" x={left + 12} y={y + 14}>
                {t("board.flow.issues", { n: group.cards.length })}
              </text>
            </g>
          );
        })}

        {groups.flatMap((group) =>
          group.cards.map((item) => {
            const y = cardY.get(item.card.id) ?? top;
            const title = item.card.title || item.card.id;
            return (
              <g key={item.card.id} className={`agent-flow-node agent-flow-issue ${statusClass(item.card.status)}`}>
                <title>{`${title}\n${item.card.work || item.card.status}`}</title>
                <rect x={issueLeft} y={y - 23} width={issueWidth} height={46} rx={10} />
                <text x={issueLeft + 12} y={y - 3}>{short(title, 42)}</text>
                <text className="agent-flow-muted" x={issueLeft + 12} y={y + 14}>
                  {statusLabel(t, item.card.status)} · {short(item.card.work || item.card.id, 38)}
                </text>
              </g>
            );
          }),
        )}
      </svg>
    </div>
  );
}
