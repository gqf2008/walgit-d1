import { useCallback, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { parsePatchFiles } from "@pierre/diffs";
import { FileDiff } from "@pierre/diffs/react";
import { api, type CollabEntryRef } from "../api";
import { useRepo } from "./RepoLayout";
import { invalidate, useData } from "../data";
import { Box } from "../components/Layout";
import { CollabWriteBox, enableCollabKey } from "../components/CollabWrite";
import { useI18n, kindLabel, type TFunc } from "../i18n";
import { Markdown } from "../components/Markdown";
import { fmtSize } from "../format";
import { collabEntryText } from "../collab-text";
import { signCanonical } from "../collab";

/** One attachment (`--attach`, issue #75 ④): `{filename, sha256, content_b64}`
    embedded in the signed entry body. Materialized to a Blob on demand —
    never rendered through a `data:` URL. */
type Attach = { filename: string; sha256: string; content_b64: string };

function EntryAttachments({ body }: { body: Record<string, unknown> }) {
  const { t } = useI18n();
  const raw = body.attachments;
  if (!Array.isArray(raw)) return null;
  const items = raw.filter((a): a is Attach => {
    if (a === null || typeof a !== "object") return false;
    const o = a as Record<string, unknown>;
    return typeof o.filename === "string" && typeof o.sha256 === "string" && typeof o.content_b64 === "string";
  });
  if (items.length === 0) return null;
  return (
    <div className="attachments" style={{ marginTop: 8 }}>
      <div className="muted" style={{ fontSize: "0.85em" }}>{t("entry.attachments")}</div>
      {items.map((a) => (
        <AttachmentItem key={`${a.sha256}:${a.filename}`} a={a} label={t("entry.attachment.download")} />
      ))}
    </div>
  );
}

function AttachmentItem({ a, label }: { a: Attach; label: string }) {
  const { t } = useI18n();
  const [error, setError] = useState<string | null>(null);
  // 防御性上限:CLI 侧 ≤64 KiB/文件,但条目是任意写者签名的数据——超大的
  // base64 不做渲染期解码(每次打开线程页都会跑),只显示 sha。
  const oversized = a.content_b64.length > 4 * 64 * 1024;
  const size = (() => {
    if (oversized) return 0;
    try {
      return atob(a.content_b64).length;
    } catch {
      return 0;
    }
  })();
  const download = () => {
    if (oversized) {
      setError(t("entry.attachment.big"));
      return;
    }
    try {
      const bytes = Uint8Array.from(atob(a.content_b64), (c) => c.charCodeAt(0));
      const blob = new Blob([bytes]);
      const u = URL.createObjectURL(blob);
      const el = document.createElement("a");
      el.href = u;
      el.download = a.filename;
      el.click();
      setTimeout(() => URL.revokeObjectURL(u), 10_000);
    } catch {
      setError(t("entry.attachment.bad"));
    }
  };
  return (
    <div className="row gap" style={{ alignItems: "center", fontSize: "0.85em" }}>
      <button type="button" className="btn link" title={label} onClick={download}>{a.filename}</button>
      <span className="muted mono">
        {size > 0 ? `${fmtSize(size)} · ${a.sha256.slice(0, 8)}` : a.sha256.slice(0, 8)}
      </span>
      {error && <span className="muted" style={{ color: "var(--danger, #f85149)" }}>{error}</span>}
    </div>
  );
}

function fmtTime(ts: number): string {
  return new Date(ts * 1000).toLocaleString();
}

export function CollabThreadPage() {
  const { t } = useI18n();
  const { full } = useRepo();
  const id = useParams().id ?? "";
  const thread = useData(`collab:${full}:thread:${id}`, () => api.collab(full).thread(id));
  const narration = ciNarration(thread.entries, t);
  const lastOid = thread.entries[thread.entries.length - 1]?.oid ?? "";
  const rootKind = thread.entries[0]?.entry.kind;
  const acceptedOid = (() => {
    let accepted = "";
    for (const e of thread.entries) {
      if (e.entry.kind !== "solution" || !e.verified) continue;
      const body = e.entry.body as Record<string, unknown>;
      if (body.accepted === true && typeof body.comment_oid === "string") accepted = body.comment_oid;
      else if (body.accepted === false) accepted = "";
    }
    return accepted;
  })();
  const accept = useCallback(async (commentOid: string) => {
    const principal = await enableCollabKey(full, t);
    const entry = await api.collabBuildEntry(full, {
      principal,
      kind: "solution",
      id: thread.id,
      actor: principal,
      parent: lastOid,
      body: { comment_oid: commentOid, accepted: true },
      sign: signCanonical,
    });
    await api.collab(full).post(entry);
    invalidate(`collab:${full}`);
    invalidate(`discussions:${full}`);
  }, [full, lastOid, t, thread.id]);
  return (
    <>
      <div className="pad">
        <Link to={`/${full}/collab`} className="muted">{t("back.collab")}</Link>
      </div>
      <Box title={t("thread.title", { id: thread.id })}>
        {thread.pr && (
          <div className="kv">
            <dt>{t("pr.baseHead")}</dt>
            <dd className="mono">{thread.pr.pr.base ?? "?"} → {thread.pr.pr.head ?? "?"}</dd>
            <dt>{t("pr.status")}</dt>
            <dd>{thread.pr.pr.status}</dd>
            <dt>{t("pr.reviews")}</dt>
            <dd>
              {thread.pr.pr.reviews.map((r) => `${r.actor}:${r.decision}`).join(", ") || "—"}
            </dd>
            <dt>{t("pr.approvals")}</dt>
            <dd>
              {t("pr.approvals.value", {
                human: thread.pr.pr.human_approvals.length,
                unverified: thread.pr.pr.unverified.length,
              })}
            </dd>
            <dt>{t("pr.mergeRule")}</dt>
            <dd>{thread.pr.merge.allowed ? t("collab.merge.allowed") : thread.pr.merge.reason}</dd>
          </div>
        )}
        {thread.pr && <PrDiff full={full} base={thread.pr.pr.base} head={thread.pr.pr.head} />}
        <div className="box-header" style={{ marginTop: 8 }}>{t("thread.write")}</div>
        <CollabWriteBox full={full} id={thread.id} parent={lastOid} />
      </Box>

      {narration && <div className="pad muted" style={{ borderLeft: "3px solid var(--accent, #58a6ff)" }}>{narration}</div>}
      {thread.entries.map((e, i) => (
        <EntryBox
          key={e.oid}
          e={e}
          n={i + 1}
          accepted={e.oid === acceptedOid}
          onAccept={rootKind === "discussion" && e.entry.kind === "comment" ? () => accept(e.oid) : undefined}
        />
      ))}
    </>
  );
}

/** The PR's base→head diff, rendered on demand (the diff can be large). */
function PrDiff({ full, base, head }: { full: string; base: string | null; head: string | null }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const [state, setState] = useState<{ patch: string; error?: string } | null>(null);
  const load = async () => {
    setOpen(true);
    if (state) return;
    try {
      if (!base || !head) throw new Error("base/head missing");
      const d = await api.diff(full, base, head, "patch");
      setState({ patch: d.patch ?? "" });
    } catch (e) {
      setState({ patch: "", error: e instanceof Error ? e.message : String(e) });
    }
  };
  const files = state ? parsePatchFilesSafe(state.patch, head ?? "") : [];
  return (
    <>
      <div className="box-header" style={{ marginTop: 8 }}>
        <button className="btn small" onClick={load} aria-expanded={open}>
          {open ? t("pr.diff", { base: base ?? "?", head: head ?? "?" }) : t("pr.showDiff", { base: base ?? "?", head: head ?? "?" })}
        </button>
      </div>
      {open && state && (
        <div className="pad">
          {state.error && <div className="muted" style={{ color: "var(--danger, #f85149)" }}>{state.error}</div>}
          {!state.error && files.length === 0 && <div className="muted">{t("pr.noChanges")}</div>}
          {files.map((f, i) => (
            <div key={f.name + i} className="diff-file">
              <FileDiff fileDiff={f} options={{ diffStyle: "unified", themeType: "light", overflow: "scroll" }} />
            </div>
          ))}
        </div>
      )}
    </>
  );
}

function parsePatchFilesSafe(patch: string, sha: string) {
  if (!patch) return [];
  try {
    return parsePatchFiles(patch, sha).flatMap((p) => p.files);
  } catch (e) {
    console.error(e);
    return [];
  }
}

/** CI conclusions get the only colors on this page: green/red/amber (D1-CI §8.3). */
export function ciColor(conclusion: string): string {
  return conclusion === "success" ? "var(--ok, #2ea043)"
    : conclusion === "error" ? "var(--warning, #d29922)"
    : "var(--danger, #f85149)";
}

/** Human claim TTL: 300 → "5m", 90 → "1m30s" (D1-CI §6.3 — 「认领有效期 5 分钟」). */
function fmtTtl(s: number): string {
  if (s % 60 === 0) return `${s / 60}m`;
  if (s < 60) return `${s}s`;
  return `${Math.floor(s / 60)}m${s % 60}s`;
}

/** run_id (D1-CI §5, normative): "ci-" + hex16(fnv1a64(task ∥ 0x1f ∥ ref ∥ 0x1f ∥ commit)).
    Recomputed in the browser to show the derivation is checkable by anyone. */
function fnv1a64(parts: string[]): string {
  const bytes = new TextEncoder().encode(parts.join("\x1f"));
  let h = 0xcbf29ce484222325n;
  for (const b of bytes) {
    h ^= BigInt(b);
    h = (h * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return h.toString(16).padStart(16, "0");
}

const KNOWN_KINDS = new Set(["issue", "discussion", "solution", "comment", "review", "status", "patch", "ci_claim", "ci_result"]);

/** The thread-level CI narration (issue #38): claims of the newest attempt tell
    the reader what happened without the protocol vocabulary. */
function ciNarration(entries: CollabEntryRef[], t: TFunc): string | null {
  const claims = entries.filter((e) => e.entry.kind === "ci_claim" && e.verified);
  if (claims.length === 0) return null;
  const attempts = new Set(claims.map((e) => String((e.entry.body as Record<string, unknown>).attempt ?? "?")));
  const hasResult = entries.some((e) => e.entry.kind === "ci_result" && e.verified);
  // One claim (or one claim per attempt — retries) is the ordinary path: no banner.
  if (claims.length === attempts.size) return null;
  return hasResult
    ? t("entry.claim.race", { n: claims.length })
    : t("entry.claim.pending");
}

function EntryBox({
  e,
  n,
  accepted,
  onAccept,
}: {
  e: CollabEntryRef;
  n: number;
  accepted?: boolean;
  onAccept?: () => Promise<void>;
}) {
  const { t } = useI18n();
  const [accepting, setAccepting] = useState(false);
  const [acceptError, setAcceptError] = useState<string | null>(null);
  const kind = e.entry.kind;
  const body = e.entry.body as Record<string, unknown>;
  const ciConclusion = kind === "ci_result" ? String(body.conclusion ?? "") : "";
  // 对话正文：兼容 CLI（issue body.body / review·merge_result body.note / patch
  // body.message）与 Web 写入口（issue·comment body.text / patch body.message）。
  // 除纯机器条目外都渲染 Markdown，让线程页可见 agent/人写的实际内容。
  // `collabEntryText` 与投影端 entry_prose 是同一张表（双向金标，issue #112）；
  // `discussion` 的正文走同一走法，线程页与看板卡片不会漂移。
  const proseKinds = new Set(["issue", "discussion", "comment", "review", "status", "patch"]);
  const prose = proseKinds.has(kind) ? collabEntryText(body) : "";
  const title =
    kind === "issue" || kind === "discussion" ? String(body.title ?? t("entry.issue.untitled"))
    : kind === "review" ? String(body.decision ?? "comment")
    : kind === "status" ? String(body.status ?? "status")
    : kind === "ci_claim"
      ? t("entry.claim.title", { task: String(body.task ?? "?"), attempt: String(body.attempt ?? "?") })
      : kind === "ci_result"
        ? t("entry.result.title", { task: String(body.task ?? "?"), attempt: String(body.attempt ?? "?") })
        : kind;
  return (
    <Box
      title={
        <span>
          #{n} <strong>{kindLabel(t, kind)}</strong> · {e.entry.actor} · {fmtTime(e.entry.ts)} ·{" "}
          {e.verified ? t("entry.verified") : t("entry.unverified")}
        </span>
      }
    >
      <div className="pad">
        <div className="strong">
          {title}
          {ciConclusion && (
            <span style={{ marginLeft: 8, color: ciColor(ciConclusion) }}>● {ciConclusion}</span>
          )}
        </div>
        {prose && <Markdown source={prose} />}
        {accepted && <div className="ok">{t("discussion.accepted")}</div>}
        {onAccept && (
          <div className="row gap" style={{ marginTop: 8 }}>
            <button
              className="btn small"
              disabled={accepting}
              onClick={() => {
                setAccepting(true);
                setAcceptError(null);
                onAccept()
                  .catch((err: unknown) => setAcceptError(err instanceof Error ? err.message : String(err)))
                  .finally(() => setAccepting(false));
              }}
            >
              {accepting ? t("discussion.accepting") : t("discussion.accept")}
            </button>
            {acceptError && <span className="muted" style={{ color: "var(--danger, #f85149)" }}>{acceptError}</span>}
          </div>
        )}
        <EntryAttachments body={body} />
        {kind === "ci_result" && (
          <div className="mono muted">
            {t("entry.result.meta", {
              ref: String(body.ref ?? ""),
              commit: String(body.commit ?? "").slice(0, 8),
              code: body.exit_code === null || body.exit_code === undefined ? "—" : String(body.exit_code),
              ms: String(body.duration_ms ?? "?"),
              sha: String(body.log_sha256 ?? "").slice(0, 12),
            })}
          </div>
        )}
        {kind === "ci_claim" && (
          <>
            <div className="mono muted">
              {String(body.ref ?? "")} @ {String(body.commit ?? "").slice(0, 8)}
            </div>
            <div className="muted">
              {t("entry.claim.meta", {
                ttl: fmtTtl(Number(body.ttl ?? 0)),
                runner: String(body.runner ?? "—"),
              })}
              {" · "}
              <code className="mono">{String(body.task ?? "?")}</code>
            </div>
            <details className="mono muted">
              <summary>{e.entry.id}</summary>
              {t("entry.runid.detail", {
                hex: fnv1a64([
                  String(body.task ?? ""),
                  String(body.ref ?? ""),
                  String(body.commit ?? ""),
                ]),
              })}
            </details>
          </>
        )}
        {ciConclusion && typeof body.log_summary === "string" && body.log_summary !== "" && (
          <pre className="collab-pre">{body.log_summary}</pre>
        )}
        {!ciConclusion && !KNOWN_KINDS.has(kind) && (
          <pre className="collab-pre">{JSON.stringify(e.entry.body, null, 2)}</pre>
        )}
        {(e.entry.refs?.base || e.entry.refs?.head) && (
          <div className="mono muted">
            {e.entry.refs?.base ?? "?"} → {e.entry.refs?.head ?? "?"}
          </div>
        )}
        <div className="mono muted" style={{ wordBreak: "break-all" }}>
          {e.oid} · {e.principal} · sig {e.entry.sig.slice(0, 24)}…
        </div>
      </div>
    </Box>
  );
}
