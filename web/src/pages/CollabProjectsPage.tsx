import { useCallback, useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { api, type CollabBoardCard, type CollabBoardColumn } from "../api";
import { useRepo } from "./RepoLayout";
import { invalidate, useData } from "../data";
import { Box } from "../components/Layout";
import { useI18n, statusLabel } from "../i18n";
import { enableCollabKey } from "../components/CollabWrite";
import { signCanonical } from "../collab";

const MOVE_STATUSES = new Set(["open", "in-progress", "needs-review", "blocked", "needs-human", "merged", "closed"]);

/** Human filters applied to the deterministic board projection. */
export function filterProjectColumns(
  columns: CollabBoardColumn[],
  status: string,
  owner: string,
  query: string,
): CollabBoardColumn[] {
  const ownerQuery = owner.trim().toLowerCase();
  const text = query.trim().toLowerCase();
  return columns
    .map((column) => ({
      ...column,
      cards: column.cards.filter(
        (card) =>
          (!status || card.status === status) &&
          (!ownerQuery || card.owner.toLowerCase().includes(ownerQuery)) &&
          (!text || card.title.toLowerCase().includes(text) || card.id.toLowerCase().includes(text)),
      ),
    }))
    .filter((column) => column.cards.length > 0);
}

function fmtTime(ts: number): string {
  return new Date(ts * 1000).toLocaleString();
}

function ProjectCard({
  full,
  card,
  dragging,
  onDragStart,
}: {
  full: string;
  card: CollabBoardCard;
  dragging: boolean;
  onDragStart: () => void;
}) {
  const { t } = useI18n();
  return (
    <div
      className={`pad project-card${dragging ? " dragging" : ""}`}
      style={{ borderBottom: "1px solid var(--border, #ddd)" }}
      draggable
      onDragStart={(e) => {
        e.dataTransfer.effectAllowed = "move";
        e.dataTransfer.setData("text/plain", card.id);
        onDragStart();
      }}
    >
      <Link to={`/${full}/collab/thread/${encodeURIComponent(card.id)}`} className="strong">
        ⠿ {card.title || card.id}
      </Link>
      <div className="muted" style={{ fontSize: "0.85em" }}>
        {statusLabel(t, card.status)} · {card.owner || t("board.card.unassigned")} · {fmtTime(card.last_ts)}
      </div>
      {card.work && <div className="muted" style={{ fontSize: "0.85em" }}>{card.work}</div>}
    </div>
  );
}

export function CollabProjectsPage() {
  const { t } = useI18n();
  const { full } = useRepo();
  const [view, setView] = useState<"board" | "table">("board");
  const [status, setStatus] = useState("");
  const [owner, setOwner] = useState("");
  const [query, setQuery] = useState("");
  const [dragId, setDragId] = useState("");
  const [overColumn, setOverColumn] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const board = useData(`collab:${full}:board`, () => api.collab(full).board());

  const columns = useMemo<CollabBoardColumn[]>(
    () => filterProjectColumns(board.columns, status, owner, query),
    [board.columns, owner, query, status],
  );
  const cards = useMemo(() => columns.flatMap((column) => column.cards), [columns]);
  const move = useCallback(
    async (card: CollabBoardCard, nextStatus: string) => {
      setBusy(true);
      setError(null);
      try {
        const principal = await enableCollabKey(full, t);
        const entry = await api.collabBuildEntry(full, {
          principal,
          kind: "status",
          id: card.id,
          actor: principal,
          parent: card.last_oid,
          body: { status: nextStatus },
          sign: signCanonical,
        });
        await api.collab(full).post(entry);
        invalidate(`collab:${full}`);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setBusy(false);
        setDragId("");
        setOverColumn("");
      }
    },
    [full, t],
  );

  return (
    <>
      <div className="pad">
        <Link to={`/${full}/collab`} className="muted">{t("back.collab")}</Link>
      </div>
      <Box title={t("projects.title")}>
        <div className="pad muted">{t("projects.explainer")}</div>
        <div className="row gap pad">
          <button className={`btn${view === "board" ? " primary" : ""}`} onClick={() => setView("board")}>
            {t("projects.view.board")}
          </button>
          <button className={`btn${view === "table" ? " primary" : ""}`} onClick={() => setView("table")}>
            {t("projects.view.table")}
          </button>
          <input
            value={status}
            onChange={(e) => setStatus(e.target.value)}
            placeholder={t("projects.filter.status")}
            aria-label={t("projects.filter.status")}
          />
          <input
            value={owner}
            onChange={(e) => setOwner(e.target.value)}
            placeholder={t("projects.filter.owner")}
            aria-label={t("projects.filter.owner")}
          />
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("projects.search")}
            aria-label={t("projects.search")}
          />
          <span className="muted">{t("projects.count", { cards: cards.length, columns: columns.length })}</span>
          <span className="spacer" />
          <Link to={`/${full}/collab/board`} className="muted">{t("projects.moveHint")}</Link>
        </div>
      </Box>
      {cards.length === 0 && <Box><div className="pad muted">{t("projects.empty")}</div></Box>}
      {view === "board" && cards.length > 0 && (
        <div className="board-grid">
          {columns.map((column) => (
            <div className="board-column" key={column.name}>
              <Box title={`${column.name} (${column.cards.length})`}>
                <div
                  className={`project-column-drop${overColumn === column.name ? " over" : ""}`}
                  onDragOver={(e) => {
                    if (!MOVE_STATUSES.has(column.name) || busy) return;
                    e.preventDefault();
                    e.dataTransfer.dropEffect = "move";
                    setOverColumn(column.name);
                  }}
                  onDragLeave={() => setOverColumn((current) => (current === column.name ? "" : current))}
                  onDrop={(e) => {
                    e.preventDefault();
                    if (!dragId || !MOVE_STATUSES.has(column.name) || busy) return;
                    const card = cards.find((item) => item.id === dragId);
                    if (card && card.status !== column.name) void move(card, column.name);
                  }}
                >
                  {column.cards.map((card) => (
                    <ProjectCard
                      key={card.id}
                      full={full}
                      card={card}
                      dragging={dragId === card.id}
                      onDragStart={() => setDragId(card.id)}
                    />
                  ))}
                </div>
              </Box>
            </div>
          ))}
        </div>
      )}
      {busy && <div className="pad muted">{t("projects.moving")}</div>}
      {error && <div className="pad muted" style={{ color: "var(--danger, #f85149)" }}>{error}</div>}
      {view === "table" && cards.length > 0 && (
        <Box title={t("projects.table")}>
          <table className="grid">
            <thead>
              <tr>
                <th>{t("discussion.th.topic")}</th>
                <th>{t("collab.th.status")}</th>
                <th>{t("projects.th.owner")}</th>
                <th>{t("projects.th.branch")}</th>
                <th>{t("discussion.th.lastActivity")}</th>
              </tr>
            </thead>
            <tbody>
              {cards.map((card) => (
                <tr key={card.id}>
                  <td>
                    <Link to={`/${full}/collab/thread/${encodeURIComponent(card.id)}`}>{card.title || card.id}</Link>
                    {card.work && <div className="muted" style={{ fontSize: "0.85em" }}>{card.work}</div>}
                  </td>
                  <td>{statusLabel(t, card.status)}</td>
                  <td>{card.owner || "—"}</td>
                  <td className="mono">{card.branch || "—"}</td>
                  <td>{fmtTime(card.last_ts)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </Box>
      )}
    </>
  );
}
