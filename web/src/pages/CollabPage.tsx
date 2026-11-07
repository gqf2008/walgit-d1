import { useState } from "react";
import { Link } from "react-router-dom";
import { api, type CollabReport } from "../api";
import { useRepo } from "./RepoLayout";
import { useData } from "../data";
import { Box } from "../components/Layout";
import { CollabWriteBox } from "../components/CollabWrite";
import { useI18n, kindLabel } from "../i18n";

function fmtTime(ts: number): string {
  return new Date(ts * 1000).toLocaleString();
}

function uuid(): string {
  return typeof crypto.randomUUID === "function"
    ? crypto.randomUUID()
    : `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

export function CollabPage() {
  const { full } = useRepo();
  const report = useData(`collab:${full}:report`, () => api.collab(full).report());
  return <CollabView full={full} report={report} />;
}

function CollabView({ full, report }: { full: string; report: CollabReport }) {
  const { t: t_ } = useI18n();
  const [newId, setNewId] = useState(() => uuid());
  return (
    <>
      <Box
        title={
          <span>
            {t_("collab.title")}
            <Link to={`/${full}/collab/guide`} className="muted" style={{ fontWeight: "normal", fontSize: "0.85em", marginLeft: 8 }}>
              {t_("guide.link")}
            </Link>
          </span>
        }
      >
        <div className="kv">
          <dt>{t_("collab.threads")}</dt>
          <dd>{report.threads.length}</dd>
          <dt>{t_("collab.prs")}</dt>
          <dd>{report.prs.length}</dd>
          <dt>{t_("collab.entries")}</dt>
          <dd>
            {t_("collab.entries.summary", {
              total: report.total_entries,
              verified: report.verified_entries,
              unverified: report.unverified_entries,
              missing: report.missing_principals,
            })}
          </dd>
          <dt>{t_("board.title")}</dt>
          <dd>
            <Link to={`/${full}/collab/board`}>{t_("collab.board.link")}</Link>
          </dd>
        </div>
        <div className="box-header" style={{ marginTop: 8 }}>{t_("collab.newThread")}</div>
        <CollabWriteBox full={full} id={newId} parent="" onPosted={() => setNewId(uuid())} />
      </Box>

      <Box title={t_("collab.threads")}>
        {report.threads.length === 0 && <div className="pad muted">{t_("collab.noThreads")}</div>}
        {report.threads.length > 0 && (
          <table className="grid">
            <thead>
              <tr>
                <th>{t_("collab.th.title")}</th>
                <th>{t_("collab.th.kinds")}</th>
                <th>{t_("collab.th.entries")}</th>
                <th>{t_("collab.th.verified")}</th>
                <th>{t_("collab.th.lastActivity")}</th>
              </tr>
            </thead>
            <tbody>
              {report.threads.map((t) => (
                <tr key={t.id}>
                  <td>
                    <Link
                      to={`/${full}/collab/thread/${encodeURIComponent(t.id)}`}
                      className="strong"
                      title={t.id}
                    >
                      {t.title || t.id}
                    </Link>
                    {t.title ? (
                      <span className="muted mono" style={{ marginLeft: 6, fontSize: "0.85em" }}>
                        {t.id}
                      </span>
                    ) : null}
                  </td>
                  <td>{t.kinds.map((k) => kindLabel(t_, k)).join(", ")}</td>
                  <td>{t.entries}</td>
                  <td>{t.verified}</td>
                  <td>{fmtTime(t.last_ts)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Box>

      <Box title={t_("collab.prs")}>
        {report.prs.length === 0 && <div className="pad muted">{t_("collab.noPrs")}</div>}
        {report.prs.length > 0 && (
          <table className="grid">
            <thead>
              <tr>
                <th>{t_("collab.th.title")}</th>
                <th>{t_("collab.th.baseHead")}</th>
                <th>{t_("collab.th.status")}</th>
                <th>{t_("collab.th.approvals")}</th>
                <th>{t_("collab.th.merge")}</th>
              </tr>
            </thead>
            <tbody>
              {report.prs.map((p) => (
                <tr key={p.id}>
                  <td>
                    <Link
                      to={`/${full}/collab/thread/${encodeURIComponent(p.id)}`}
                      className="strong"
                      title={p.id}
                    >
                      {p.title || p.id}
                    </Link>
                    {p.title ? (
                      <span className="muted mono" style={{ marginLeft: 6, fontSize: "0.85em" }}>
                        {p.id}
                      </span>
                    ) : null}
                  </td>
                  <td className="mono">{p.base ?? "?"} → {p.head ?? "?"}</td>
                  <td>{p.status}</td>
                  <td>{p.approvals}</td>
                  <td>
                    {p.merge_allowed ? <span className="ok">{t_("collab.merge.allowed")}</span> : <span className="muted">{p.merge_reason}</span>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Box>

      {(report.by_actor.length > 0 || report.by_kind.length > 0) && (
        <div className="row gap">
          <Box title={t_("collab.byActor")} className="grow">
            <table className="kv compact">
              <tbody>
                {report.by_actor.map(([a, n]) => (
                  <tr key={a}>
                    <th>{a}</th>
                    <td>{n}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </Box>
          <Box title={t_("collab.byKind")} className="grow">
            <table className="kv compact">
              <tbody>
                {report.by_kind.map(([k, n]) => (
                  <tr key={k}>
                    <th>{kindLabel(t_, k)}</th>
                    <td>{n}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </Box>
        </div>
      )}
    </>
  );
}
