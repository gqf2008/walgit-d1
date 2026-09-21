import { useCallback, useState } from "react";
import { Link } from "react-router-dom";
import { api } from "../api";
import { useRepo } from "./RepoLayout";
import { invalidate, useData } from "../data";
import { Box } from "../components/Layout";
import { enableCollabKey } from "../components/CollabWrite";
import { signCanonical } from "../collab";
import { useI18n } from "../i18n";

function fmtTime(ts: number): string {
  return new Date(ts * 1000).toLocaleString();
}

function newDiscussionId(): string {
  return typeof crypto.randomUUID === "function"
    ? `discussion-${crypto.randomUUID()}`
    : `discussion-${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

function DiscussionComposer({ full }: { full: string }) {
  const { t } = useI18n();
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [category, setCategory] = useState("general");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const post = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      if (!title.trim()) throw new Error(t("discussion.err.title"));
      const principal = await enableCollabKey(full, t);
      const entry = await api.collabBuildEntry(full, {
        principal,
        kind: "discussion",
        id: newDiscussionId(),
        actor: principal,
        parent: "",
        body: { title: title.trim(), body, category: category.trim() || "general" },
        sign: signCanonical,
      });
      await api.collab(full).post(entry);
      setTitle("");
      setBody("");
      invalidate(`collab:${full}`);
      invalidate(`discussions:${full}`);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }, [body, category, full, t, title]);

  return (
    <div className="pad">
      <div className="row gap">
        <input
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          placeholder={t("discussion.ph.title")}
          aria-label={t("discussion.ph.title")}
        />
        <select value={category} onChange={(e) => setCategory(e.target.value)} aria-label={t("discussion.category")}>
          <option value="general">{t("discussion.category.general")}</option>
          <option value="q-and-a">{t("discussion.category.qAndA")}</option>
          <option value="ideas">{t("discussion.category.ideas")}</option>
          <option value="announcements">{t("discussion.category.announcements")}</option>
          <option value="show-and-tell">{t("discussion.category.showAndTell")}</option>
        </select>
      </div>
      <textarea
        className="collab-body"
        value={body}
        onChange={(e) => setBody(e.target.value)}
        placeholder={t("discussion.ph.body")}
        rows={4}
      />
      <div className="row gap">
        <button className="btn primary" disabled={busy || !title.trim()} onClick={post}>
          {busy ? t("discussion.posting") : t("discussion.post")}
        </button>
        {error && <span className="muted" style={{ color: "var(--danger, #f85149)" }}>{error}</span>}
      </div>
    </div>
  );
}

export function CollabDiscussionsPage() {
  const { t } = useI18n();
  const { full } = useRepo();
  const [state, setState] = useState<"all" | "open" | "closed" | "answered">("all");
  const [category, setCategory] = useState("");
  const page = useData(`discussions:${full}:${state}:${category}`, () =>
    api.collab(full).discussions({ state, category: category || undefined, n: 100 }),
  );

  return (
    <>
      <div className="pad">
        <Link to={`/${full}/collab`} className="muted">{t("back.collab")}</Link>
      </div>
      <Box title={t("discussion.title")}>
        <div className="pad muted">{t("discussion.explainer")}</div>
        <DiscussionComposer full={full} />
      </Box>
      <div className="row gap" style={{ padding: "0 1rem" }}>
        <select value={state} onChange={(e) => setState(e.target.value as typeof state)} aria-label={t("discussion.state")}>
          <option value="all">{t("discussion.state.all")}</option>
          <option value="open">{t("discussion.state.open")}</option>
          <option value="closed">{t("discussion.state.closed")}</option>
          <option value="answered">{t("discussion.state.answered")}</option>
        </select>
        <select value={category} onChange={(e) => setCategory(e.target.value)} aria-label={t("discussion.category")}>
          <option value="">{t("discussion.category.all")}</option>
          <option value="general">{t("discussion.category.general")}</option>
          <option value="q-and-a">{t("discussion.category.qAndA")}</option>
          <option value="ideas">{t("discussion.category.ideas")}</option>
          <option value="announcements">{t("discussion.category.announcements")}</option>
          <option value="show-and-tell">{t("discussion.category.showAndTell")}</option>
        </select>
      </div>
      <Box title={t("discussion.list")}>
        {page.discussions.length === 0 && <div className="pad muted">{t("discussion.empty")}</div>}
        {page.discussions.length > 0 && (
          <table className="grid">
            <thead>
              <tr>
                <th>{t("discussion.th.topic")}</th>
                <th>{t("discussion.th.category")}</th>
                <th>{t("discussion.th.replies")}</th>
                <th>{t("discussion.th.state")}</th>
                <th>{t("discussion.th.lastActivity")}</th>
              </tr>
            </thead>
            <tbody>
              {page.discussions.map((d) => (
                <tr key={d.id}>
                  <td>
                    <Link to={`/${full}/collab/thread/${encodeURIComponent(d.id)}`} className="strong">
                      {d.title || d.id}
                    </Link>
                    {d.body && <div className="muted" style={{ fontSize: "0.85em" }}>{d.body}</div>}
                  </td>
                  <td>{d.category}</td>
                  <td>{d.reply_count}</td>
                  <td>
                    {d.closed ? t("discussion.state.closed") : d.answered ? t("discussion.state.answered") : t("discussion.state.open")}
                  </td>
                  <td>{fmtTime(d.last_ts)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Box>
    </>
  );
}
