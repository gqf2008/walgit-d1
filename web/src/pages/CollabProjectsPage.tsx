import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { api, type CollabBoardCard, type CollabBoardColumn } from "../api";
import { useRepo } from "./RepoLayout";
import { useData } from "../data";
import { Box } from "../components/Layout";
import { useI18n, statusLabel } from "../i18n";

function fmtTime(ts: number): string {
  return new Date(ts * 1000).toLocaleString();
}

function ProjectCard({ full, card }: { full: string; card: CollabBoardCard }) {
  const { t } = useI18n();
  return (
    <div className="pad" style={{ borderBottom: "1px solid var(--border, #ddd)" }}>
      <Link to={`/${full}/collab/thread/${encodeURIComponent(card.id)}`} className="strong">
        {card.title || card.id}
      </Link>
      <div className="muted" style={{ fontSize: "0.85em" }}>
        {card.status} · {card.owner || t("board.card.unassigned")} · {fmtTime(card.last_ts)}
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
  const board = useData(`collab:${full}:board`, () => api.collab(full).board());

  const columns = useMemo<CollabBoardColumn[]>(() => {
    const ownerQuery = owner.trim().toLowerCase();
    return board.columns
      .map((column) => ({
        ...column,
        cards: column.cards.filter(
          (card) =>
            (!status || card.status === status) &&
            (!ownerQuery || card.owner.toLowerCase().includes(ownerQuery)),
        ),
      }))
      .filter((column) => column.cards.length > 0);
  }, [board.columns, owner, status]);
  const cards = useMemo(() => columns.flatMap((column) => column.cards), [columns]);

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
                {column.cards.map((card) => <ProjectCard key={card.id} full={full} card={card} />)}
              </Box>
            </div>
          ))}
        </div>
      )}
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
