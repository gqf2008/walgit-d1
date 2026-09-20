import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { api, refListStream, type CollabBoardCard, type CollabBoardColumn } from "../api";
import { useRepo } from "./RepoLayout";
import { invalidate, reportError, useData } from "../data";
import { Box } from "../components/Layout";
import { Markdown } from "../components/Markdown";
import { enableCollabKey } from "../components/CollabWrite";
import { AgentFlow } from "../components/AgentFlow";
import { signCanonical } from "../collab";
import { useI18n, statusLabel } from "../i18n";

/**
 * The D1 work-unit board (docs/D1_COLLAB_DESIGN.md §8): `/collab/board` — a
 * read-only renderer of the deterministic `build_board` projection the
 * `walgit collab board` CLI computes offline. The page writes nothing itself;
 * moving a card posts an ordinary signed `status` entry through the existing
 * thin API, and the projection re-derives the columns from it.
 */

/** The statuses the move-menu offers. Values are free-form on the wire (any
    `status` entry value works); these are the ones D1 names. */
const STATUSES = ["open", "in-progress", "needs-review", "blocked", "needs-human", "merged", "closed"] as const;

/** Columns shown by the default board view: non-empty only, unless asked. */
export function visibleBoardColumns(columns: CollabBoardColumn[], showEmpty: boolean): CollabBoardColumn[] {
  return showEmpty ? columns : columns.filter((col) => col.cards.length > 0);
}

function fmtTime(ts: number): string {
  return new Date(ts * 1000).toLocaleString();
}

/**
 * Live refresh over SSE: reopen the collab refs stream (`GET …/refs/collab`
 * with `Accept: text/event-stream` — refs-level reads are cheap, docs/D1_PROTOCOL.md §11) and
 * invalidate the collab views when the namespace's (name, sha) set moved.
 * Reopening with a backoff is the same reconnection semantics an EventSource
 * would apply; the first pass only seeds the digest, so mounting never
 * invalidates.
 */
function useCollabLive(full: string) {
  useEffect(() => {
    const ctl = new AbortController();
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let seen = "";
    const pass = async () => {
      const names: string[] = [];
      const { more } = await refListStream(full, "collab", { n: 1000 }, (r) => names.push(`${r.name} ${r.sha}`), ctl.signal);
      names.sort();
      const digest = `${more ? "more:" : ""}${names.join("|")}`;
      if (seen && digest !== seen) invalidate(`collab:${full}`);
      seen = digest;
    };
    const tick = () => {
      pass()
        .catch((e: unknown) => {
          if (!alive || (e as Error).name === "AbortError") return;
          reportError(e, `collab live (${full})`);
        })
        .finally(() => {
          if (alive) timer = setTimeout(tick, 5000);
        });
    };
    tick();
    return () => {
      alive = false;
      ctl.abort();
      if (timer !== undefined) clearTimeout(timer);
    };
  }, [full]);
}

/** One lane; the card menu posts the `status` entry that moves it. */
function BoardColumnView({ full, name, cards }: { full: string; name: string; cards: CollabBoardCard[] }) {
  return (
    <div className="board-column">
      <Box title={`${name} (${cards.length})`}>
        {cards.length === 0 && <div className="pad muted">—</div>}
        {cards.map((c) => (
          <BoardCard key={c.id} full={full} card={c} />
        ))}
      </Box>
    </div>
  );
}

function BoardCard({ full, card }: { full: string; card: CollabBoardCard }) {
  const { t } = useI18n();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const move = useCallback(
    async (status: string) => {
      setBusy(true);
      setError(null);
      try {
        // Moving a card is not a board write: it is one signed `status`
        // entry, chained on the thread tip, through the same thin API every
        // other browser write uses — the verification badge on the thread
        // page proves it exactly like a CLI-written entry.
        const principal = await enableCollabKey(full, t);
        const entry = await api.collabBuildEntry(full, {
          principal,
          kind: "status",
          id: card.id,
          actor: principal,
          parent: card.last_oid,
          body: { status },
          sign: signCanonical,
        });
        await api.collab(full).post(entry);
        invalidate(`collab:${full}`);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setBusy(false);
      }
    },
    [full, card.id, card.last_oid, t],
  );
  return (
    <div className="pad" style={{ borderBottom: "1px solid var(--border, #ddd)" }}>
      <div className="row gap" style={{ alignItems: "center" }}>
        <Link to={`/${full}/collab/thread/${encodeURIComponent(card.id)}`} className="strong">
          {card.title || card.id}
        </Link>
        <span className="muted mono">{statusLabel(t, card.status)}</span>
      </div>
      <div className="muted" style={{ fontSize: "0.85em" }}>
        {t("board.card.meta", { actor: card.actor, entries: card.entries, verified: card.verified })}
        {card.unverified > 0 ? (
          <span style={{ color: "var(--danger, #f85149)" }}>{t("board.card.unverified", { n: card.unverified })}</span>
        ) : null}
        {card.merge ? (
          <span className={card.merge.allowed ? "ok" : "muted"}>
            {card.merge.allowed ? t("board.card.merge.allowed") : t("board.card.merge.blocked")}
          </span>
        ) : null}
        {" · "}
        {fmtTime(card.last_ts)}
      </div>
      <div className="board-card-context">
        <span>{card.owner ? t("board.card.owner", { owner: card.owner }) : t("board.card.unassigned")}</span>
        {card.worktree && <span>{t("board.card.worktree", { worktree: card.worktree })}</span>}
        {card.branch && <span>{t("board.card.branch", { branch: card.branch })}</span>}
      </div>
      {card.work && (
        <div className="muted card-prose board-card-work">
          <Markdown source={card.work} />
        </div>
      )}
      {card.prose && (
        <div className="muted card-prose">
          <Markdown source={card.prose} />
        </div>
      )}
      <div className="row gap" style={{ alignItems: "center", marginTop: 4 }}>
        <select
          aria-label={`Move ${card.id}`}
          disabled={busy}
          value=""
          onChange={(e) => {
            if (e.target.value) move(e.target.value);
          }}
        >
          <option value="">{busy ? t("board.moving") : t("board.moveTo")}</option>
          {STATUSES.filter((s) => s !== card.status).map((s) => (
            <option key={s} value={s}>
              {statusLabel(t, s)}
            </option>
          ))}
        </select>
        {error && <span className="muted" style={{ color: "var(--danger, #f85149)" }}>{error}</span>}
      </div>
    </div>
  );
}

export function CollabBoardPage() {
  const { t } = useI18n();
  const { full } = useRepo();
  const [showEmpty, setShowEmpty] = useState(false);
  const board = useData(`collab:${full}:board`, () => api.collab(full).board());
  useCollabLive(full);
  const nonEmpty = board.columns.filter((col) => col.cards.length > 0);
  const emptyCount = board.columns.length - nonEmpty.length;
  const visibleColumns = visibleBoardColumns(board.columns, showEmpty);
  return (
    <>
      <div className="pad">
        <Link to={`/${full}/collab`} className="muted">{t("back.collab")}</Link>
      </div>
      <Box title={t("board.title")}>
        <div className="pad muted">{t("board.explainer")}</div>
      </Box>
      <Box title={t("board.flow.title")}>
        <div className="pad muted">{t("board.flow.explainer")}</div>
        <AgentFlow columns={board.columns} />
      </Box>
      <div className="row gap board-toolbar">
        <span className="muted">
          {t("board.visible", { visible: visibleColumns.length, total: board.columns.length })}
        </span>
        <span className="spacer" />
        {emptyCount > 0 && (
          <button className="btn" aria-pressed={showEmpty} onClick={() => setShowEmpty((v) => !v)}>
            {showEmpty ? t("board.hideEmpty") : t("board.showEmpty", { n: emptyCount })}
          </button>
        )}
      </div>
      {visibleColumns.length === 0 ? (
        <Box>
          <div className="pad muted">{t("board.noCards")}</div>
        </Box>
      ) : (
        <div className="board-grid">
          {visibleColumns.map((col) => (
            <BoardColumnView key={col.name} full={full} name={col.name} cards={col.cards} />
          ))}
        </div>
      )}
    </>
  );
}
