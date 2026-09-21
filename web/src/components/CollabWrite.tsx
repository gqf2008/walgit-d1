import { useCallback, useState } from "react";
import { api } from "../api";
import { invalidate } from "../data";
import { ed25519Supported, publicKeyB64, signCanonical } from "../collab";
import { useI18n, kindLabel, decisionLabel, statusLabel, type TFunc } from "../i18n";

/**
 * D1 browser write box: sign in as the session principal, self-register the
 * browser Ed25519 public key through the thin API, and post signed entries
 * (issue / comment / review / status / patch). Entries are verified by the
 * aggregation exactly like CLI/agent writes.
 */

export interface CollabWriteProps {
  full: string;
  /** Thread id this entry joins (new threads use a fresh uuid). */
  id: string;
  /** Previous entry oid in the thread ("" for a root entry). */
  parent: string;
  onPosted?: () => void;
}

/**
 * Shared D1 browser-write setup, used by the write box and the board's
 * move-card action: WebCrypto Ed25519 available, session signed in, and the
 * public key self-registered through the thin API. Resolves the principal or
 * throws with the reason it could not.
 */
export async function enableCollabKey(full: string, t?: TFunc): Promise<string> {
  if (!(await ed25519Supported())) {
    throw new Error(t ? t("write.err.noWebCrypto") : "This browser has no WebCrypto Ed25519 support — use the walgit collab CLI to sign entries.");
  }
  const me = await api.me();
  if (me.anonymous) {
    throw new Error(t ? t("write.err.signedOut") : "Signed out — sign in to participate in the collaboration layer.");
  }
  const publicKey = await publicKeyB64();
  // Self-registration is idempotent in effect (re-registering the same key
  // just appends a log entry; the aggregation reads the latest key).
  await api.collab(full).registerPrincipal(me.principal, publicKey);
  invalidate(`collab:${full}`);
  return me.principal;
}

type Kind = "issue" | "comment" | "review" | "status" | "patch";

export function CollabWriteBox({ full, id, parent, onPosted }: CollabWriteProps) {
  const { t } = useI18n();
  const [ready, setReady] = useState<string | null>(null); // principal when the browser key is registered
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [kind, setKind] = useState<Kind>(parent === "" ? "issue" : "comment");
  const [text, setText] = useState("");
  const [decision, setDecision] = useState<"approve" | "request_changes" | "comment">("approve");
  const [status, setStatus] = useState<"open" | "closed" | "merged">("closed");
  const [base, setBase] = useState("refs/heads/main");
  const [head, setHead] = useState("");

  const enable = useCallback(async (): Promise<string | null> => {
    setBusy(true);
    setError(null);
    try {
      const principal = await enableCollabKey(full, t);
      setReady(principal);
      return principal;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return null;
    } finally {
      setBusy(false);
    }
  }, [full, t]);

  const post = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const principal = ready ?? (await enable());
      if (!principal) return; // enable() set the error
      const body: Record<string, unknown> =
        kind === "issue"
          ? { title: text.split("\n")[0] || t("write.err.untitled"), text }
          : kind === "comment"
            ? { text }
            : kind === "review"
              ? text.trim() !== ""
                ? { decision, note: text }
                : { decision }
              : kind === "status"
                ? text.trim() !== ""
                  ? { status, note: text }
                  : { status }
                : { message: text };
      const refs = kind === "patch" ? { base, head } : undefined;
      const entry = await api.collabBuildEntry(full, {
        principal,
        kind,
        id,
        actor: principal,
        parent,
        refs,
        body,
        sign: signCanonical,
      });
      await api.collab(full).post(entry);
      invalidate(`collab:${full}`);
      setText("");
      onPosted?.();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }, [ready, enable, kind, text, decision, status, base, head, full, id, parent, onPosted, t]);

  if (!ready) {
    return (
      <div className="pad">
        {error && <div className="muted" style={{ color: "var(--danger, #f85149)" }}>{error}</div>}
        <button className="btn" disabled={busy} onClick={enable}>
          {busy ? t("write.enabling") : t("write.enable")}
        </button>
        <span className="muted">{t("write.enable.hint")}</span>
      </div>
    );
  }
  return (
    <div className="pad">
      <div className="row gap" style={{ alignItems: "center" }}>
        <strong>{ready}</strong>
        <select value={kind} onChange={(e) => setKind(e.target.value as Kind)}>
          <option value="issue">{kindLabel(t, "issue")}</option>
          <option value="comment">{kindLabel(t, "comment")}</option>
          <option value="review">{kindLabel(t, "review")}</option>
          <option value="status">{kindLabel(t, "status")}</option>
          <option value="patch">{kindLabel(t, "patch")}</option>
        </select>
        {kind === "review" && (
          <select value={decision} onChange={(e) => setDecision(e.target.value as typeof decision)}>
            <option value="approve">{decisionLabel(t, "approve")}</option>
            <option value="request_changes">{decisionLabel(t, "request_changes")}</option>
            <option value="comment">{decisionLabel(t, "comment")}</option>
          </select>
        )}
        {kind === "status" && (
          <select value={status} onChange={(e) => setStatus(e.target.value as typeof status)}>
            <option value="closed">{statusLabel(t, "closed")}</option>
            <option value="merged">{statusLabel(t, "merged")}</option>
            <option value="open">{statusLabel(t, "open")}</option>
          </select>
        )}
      </div>
      {kind === "patch" && (
        <div className="row gap">
          <input value={base} onChange={(e) => setBase(e.target.value)} placeholder={t("write.baseRef")} />
          <input value={head} onChange={(e) => setHead(e.target.value)} placeholder={t("write.headRef")} />
        </div>
      )}
      <textarea
        className="collab-body"
        value={text}
        onChange={(e) => setText(e.target.value)}
        placeholder={kind === "issue" ? t("write.ph.issue") : kind === "review" || kind === "status" ? t("write.ph.note") : t("write.ph.write")}
        rows={4}
      />
      <div className="row gap">
        <button className="btn primary" disabled={busy || (kind === "patch" && !head)} onClick={post}>
          {busy ? t("write.posting") : t("write.post", { kind: kindLabel(t, kind) })}
        </button>
        {error && <span className="muted" style={{ color: "var(--danger, #f85149)" }}>{error}</span>}
      </div>
    </div>
  );
}
