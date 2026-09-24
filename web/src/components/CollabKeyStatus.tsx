import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../api";
import { downloadKeyBackup, hasStoredKey, restoreKeyPair, type RestoreKeyError } from "../collab";
import { useI18n, type TFunc } from "../i18n";

/**
 * Shared browser-key status strip (thread cc-ai-d1-key-import): the same
 * affordance — backup, import/restore, and the recovery hint — on the thread
 * write box and the board page. The key lives in localStorage and the private
 * half never leaves the browser; importing replaces it only after the whole
 * JWK is validated, so a failed import leaves the current key untouched.
 */

function restoreErrorMessage(t: TFunc, e: unknown): string {
  if (e && typeof e === "object" && "kind" in e) {
    const err = e as RestoreKeyError;
    switch (err.kind) {
      case "not-json":
        return t("key.import.err.notJson");
      case "not-object":
        return t("key.import.err.notObject");
      case "structure":
        return t("key.import.err.structure", { detail: err.detail });
      case "invalid":
        return t("key.import.err.invalid");
      case "mismatch":
        return t("key.import.err.mismatch");
      case "unsupported":
        return t("key.import.err.unsupported");
    }
  }
  return t("key.import.err.generic", { error: e instanceof Error ? e.message : String(e) });
}

export interface CollabKeyStatusProps {
  /** The signed-in principal (the write box knows it after enabling). When
      omitted the component resolves it from the session itself. */
  principal?: string | null;
  /** Called after a successful import — the caller must re-register the new
      public key before the next write (the write box resets its ready state;
      the board re-enables on the next move anyway). */
  onImported?: () => void;
}

export function CollabKeyStatus({ principal, onImported }: CollabKeyStatusProps) {
  const { t } = useI18n();
  const fileRef = useRef<HTMLInputElement | null>(null);
  const [hasKey, setHasKey] = useState(hasStoredKey);
  const [me, setMe] = useState<string | null>(principal ?? null);
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [ok, setOk] = useState<string | null>(null);

  useEffect(() => {
    if (principal !== undefined) return;
    let alive = true;
    api
      .me()
      .then((m) => {
        if (alive) setMe(m.anonymous ? null : m.principal);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [principal]);

  const onFile = useCallback(
    async (file: File) => {
      setBusy(true);
      setError(null);
      setOk(null);
      try {
        await restoreKeyPair(await file.text());
        setHasKey(true);
        setOk(t("key.import.ok"));
        setConfirming(false);
        onImported?.();
      } catch (e) {
        setError(restoreErrorMessage(t, e));
      } finally {
        setBusy(false);
        if (fileRef.current) fileRef.current.value = "";
      }
    },
    [t, onImported],
  );

  const openPicker = useCallback(() => {
    fileRef.current?.click();
  }, []);

  return (
    <div className="key-status">
      <input
        ref={fileRef}
        type="file"
        accept="application/json,.json"
        style={{ display: "none" }}
        aria-label={t("key.import")}
        onChange={(e) => {
          const file = e.target.files?.[0];
          if (file) void onFile(file);
        }}
      />
      <div className="muted" style={{ fontSize: "0.85em" }}>{t("write.key.hint")}</div>
      <div className="row gap" style={{ alignItems: "center" }}>
        <button
          type="button"
          className="btn small"
          disabled={!hasKey}
          onClick={() => downloadKeyBackup(me ?? "key")}
        >
          {t("write.key.backup")}
        </button>
        {confirming ? (
          <>
            <span className="muted" style={{ color: "var(--danger, #f85149)" }}>{t("key.import.confirm")}</span>
            <button type="button" className="btn small" disabled={busy} onClick={openPicker}>
              {t("key.import.replace")}
            </button>
            <button type="button" className="btn small" disabled={busy} onClick={() => setConfirming(false)}>
              {t("key.import.cancel")}
            </button>
          </>
        ) : (
          <button
            type="button"
            className="btn small"
            disabled={busy}
            onClick={() => {
              if (hasKey) {
                setError(null);
                setOk(null);
                setConfirming(true);
              } else {
                openPicker();
              }
            }}
          >
            {busy ? t("key.import.busy") : t("key.import")}
          </button>
        )}
        {ok && <span className="ok">{ok}</span>}
        {error && <span className="muted" style={{ color: "var(--danger, #f85149)" }}>{error}</span>}
      </div>
    </div>
  );
}
