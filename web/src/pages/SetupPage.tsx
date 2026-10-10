import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { useI18n } from "../i18n";
import { ApiError, api } from "../api";
import type { StoreSettings } from "../api";

/**
 * `/setup` wears two faces:
 *
 * **First-run wizard** (issue #70, D43) — while the instance is in setup
 * state (a `memory` store without the deliberate-use flag) every other route
 * answers 503, so this page IS the whole UI until the store and the first
 * admin are configured. Plain `fetch` to the open, data-free
 * `/api/v1/setup/*` surface — no SDK lane (no repo, no session exists yet).
 * Flow: ① object storage (S3/R2 or GCS) → test connection → ② first admin
 * token → save → the server writes walgit.toml (comments preserved) and
 * exits 75 for the supervisor to restart; without one the page tells the
 * user to restart manually.
 *
 * **Admin storage editor** (issue #127) — once configured, the wizard
 * surface 404s and the page asks `GET /api/v1/store` (SDK lane, admin-only):
 * an admin gets the same form over the *current* values (credentials show as
 * presence bits — they never leave the server — and a blank credential field
 * means "keep"); everyone else lands on the "already configured" card. The
 * save rides the wizard's mechanism: compose + validate before touching the
 * file, `toml_edit` write-back, exit 75 for the supervisor.
 */

interface SetupStatus {
  needs_setup: boolean;
  backend: string;
  auth_mode: string;
  can_save: boolean;
}

interface SetupPayload {
  backend: "s3" | "gcs";
  bucket: string;
  endpoint: string;
  region: string;
  access_key: string;
  secret_key: string;
  force_path_style: boolean;
}

type Phase = "form" | "testing" | "saving" | "saved";

type Mode = "loading" | "wizard" | "edit" | "configured";

const emptyPayload: SetupPayload = {
  backend: "s3",
  bucket: "",
  endpoint: "",
  region: "auto",
  access_key: "",
  secret_key: "",
  force_path_style: true,
};

export function SetupPage() {
  const { t } = useI18n();
  const [mode, setMode] = useState<Mode>("loading");
  const [status, setStatus] = useState<SetupStatus | null>(null);
  const [snapshot, setSnapshot] = useState<StoreSettings | null>(null);
  const [payload, setPayload] = useState<SetupPayload>(emptyPayload);
  const [adminToken, setAdminToken] = useState("");
  const [adminPrincipal, setAdminPrincipal] = useState("");
  const [testResult, setTestResult] = useState<{ ok: boolean; message: string } | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [restart, setRestart] = useState<"supervisor" | "manual" | null>(null);
  const [saveWarnings, setSaveWarnings] = useState<string[]>([]);
  const [phase, setPhase] = useState<Phase>("form");
  // #127 follow-up: an operator-supplied admin token for `token`-mode hosts
  // (no browser sign-in exists there — the storage editor was unreachable).
  const [storeToken, setStoreToken] = useState("");
  const [tokenError, setTokenError] = useState<string | null>(null);

  // Load once: wizard, editor, or neither.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const r = await fetch("/api/v1/setup/status");
        if (r.ok) {
          if (!cancelled) {
            setStatus((await r.json()) as SetupStatus);
            setMode("wizard");
          }
          return;
        }
      } catch {
        /* fall through to the editor probe */
      }
      // Configured: the editor answers for admins only. A 401/403 (or any
      // other answer) is not an error to show — it is simply "not yours",
      // and the card below is the truthful response.
      try {
        const s = await api.store.get();
        if (cancelled) return;
        setSnapshot(s);
        setPayload({
          backend: s.backend === "gcs" ? "gcs" : "s3",
          bucket: s.bucket,
          endpoint: s.endpoint,
          region: s.region || "auto",
          access_key: "",
          secret_key: "",
          force_path_style: s.force_path_style,
        });
        setMode("edit");
      } catch {
        if (!cancelled) setMode("configured");
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  /** Open the storage editor with an operator-supplied admin token. */
  const submitToken = async () => {
    const token = storeToken.trim();
    if (!token) return;
    setTokenError(null);
    api.setAdminToken(token);
    try {
      const s = await api.store.get();
      setSnapshot(s);
      setPayload({
        backend: s.backend === "gcs" ? "gcs" : "s3",
        bucket: s.bucket,
        endpoint: s.endpoint,
        region: s.region || "auto",
        access_key: "",
        secret_key: "",
        force_path_style: s.force_path_style,
      });
      setMode("edit");
    } catch (e) {
      // A rejected token must not stick and shadow the session lane.
      api.setAdminToken(null);
      setTokenError(e instanceof ApiError ? e.message : String(e));
    }
  };

  const set = (k: keyof SetupPayload, v: string | boolean) =>
    setPayload((p) => ({ ...p, [k]: v }));

  const showTestFailure = (e: unknown) => {
    // The 400 body is the {ok:false,message} JSON; anything else is shown raw.
    const raw = e instanceof ApiError ? e.message : String(e);
    try {
      const body = JSON.parse(raw) as { message?: string };
      setTestResult({ ok: false, message: body.message ?? raw });
    } catch {
      setTestResult({ ok: false, message: raw });
    }
  };

  const runTest = async () => {
    setPhase("testing");
    setTestResult(null);
    try {
      if (mode === "edit") {
        const body = await api.store.test(payload);
        setTestResult({ ok: true, message: body.message ?? "" });
      } else {
        const r = await fetch("/api/v1/setup/test", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(payload),
        });
        if (r.ok) {
          const body = (await r.json()) as { ok: boolean; message: string };
          setTestResult({ ok: body.ok, message: body.message });
        } else if ((r.headers.get("content-type") ?? "").includes("application/json")) {
          // The most common failure (bad credentials / unreachable endpoint) is
          // a 400 + {ok:false, message} — show the message, not the JSON.
          const body = (await r.json()) as { ok?: boolean; message?: string };
          setTestResult({ ok: false, message: body.message ?? String(body) });
        } else {
          // Validation rejections answer text/plain with the reason.
          setTestResult({ ok: false, message: await r.text() });
        }
      }
    } catch (e) {
      showTestFailure(e);
    } finally {
      setPhase("form");
    }
  };

  const runSave = async () => {
    setPhase("saving");
    setSaveError(null);
    try {
      if (mode === "edit") {
        const body = await api.store.save(payload);
        if (body.saved) {
          setRestart(body.restart ?? "manual");
          // #134 (D20): the server says when the saved file is not
          // self-sufficient (e.g. env-supplied credentials it cannot
          // capture). Render it — silent here means a surprise at the
          // next restart.
          setSaveWarnings(body.warnings ?? []);
          setPhase("saved");
          return;
        }
        setSaveError("server answered ok but did not save");
      } else {
        const r = await fetch("/api/v1/setup/save", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            store: payload,
            admin_token: adminToken,
            admin_principal: adminPrincipal,
          }),
        });
        if (r.ok) {
          const body = (await r.json()) as {
            saved?: boolean;
            restart?: "supervisor" | "manual";
            warnings?: string[];
          };
          if (body.saved) {
            setRestart(body.restart ?? "manual");
            setSaveWarnings(body.warnings ?? []);
            setPhase("saved");
            return;
          }
          setSaveError("server answered ok but did not save");
        } else {
          // The rejection paths answer text/plain with the reason.
          setSaveError(await r.text());
        }
      }
      setPhase("form");
    } catch (e) {
      setSaveError(e instanceof ApiError ? e.message : String(e));
      setPhase("form");
    }
  };

  if (mode === "loading") {
    return (
      <main className="setup">
        <div className="setup-card">
          <h1>{t("setup.title")}</h1>
          <p className="muted">…</p>
        </div>
      </main>
    );
  }

  if (mode === "configured") {
    return (
      <main className="setup">
        <div className="setup-card">
          <h1>{t("store.title")}</h1>
          <p className="muted">{t("setup.already")}</p>
          <p>{t("store.token.lede")}</p>
          <form
            className="setup-grid"
            onSubmit={(e) => {
              e.preventDefault();
              void submitToken();
            }}
          >
            <label className="setup-field">
              <span>{t("store.token.label")}</span>
              <input
                type="password"
                value={storeToken}
                onChange={(e) => setStoreToken(e.target.value)}
                autoComplete="off"
                placeholder="wgt_… / static token"
              />
              <small className="muted">{t("store.token.hint")}</small>
            </label>
            <p>
              <button className="btn primary" type="submit" disabled={!storeToken.trim()}>
                {t("store.token.submit")}
              </button>{" "}
              <Link className="btn" to="/">
                {t("setup.open")}
              </Link>
            </p>
          </form>
          {tokenError && <div className="setup-warn">{tokenError}</div>}
        </div>
      </main>
    );
  }

  const st = status;
  const snap = snapshot;
  const edit = mode === "edit";
  const canSave = edit ? (snap?.can_save ?? false) : (st?.can_save ?? false);
  const heading = edit ? t("store.title") : t("setup.title");

  if (phase === "saved") {
    return (
      <main className="setup">
        <div className="setup-card">
          <h1>{heading}</h1>
          <p className="setup-ok">
            {edit
              ? restart === "supervisor"
                ? t("store.saved")
                : t("store.saved.manual")
              : restart === "supervisor"
                ? t("setup.saved")
                : t("setup.saved.manual")}
          </p>
          {saveWarnings.length > 0 && (
            <div className="setup-warn">
              <p>{t("setup.saved.warnings")}</p>
              <ul>
                {saveWarnings.map((w, i) => (
                  <li key={i}>{w}</li>
                ))}
              </ul>
            </div>
          )}
          {edit && (
            <Link className="btn primary" to="/">
              {t("setup.open")}
            </Link>
          )}
        </div>
      </main>
    );
  }

  return (
    <main className="setup">
      <div className="setup-card">
        <h1>{heading}</h1>
        <p>{edit ? t("store.lede") : t("setup.lede")}</p>
        {!edit && <div className="setup-warn">{t("setup.memory.warn")}</div>}

        <h2>{t("setup.step.store")}</h2>
        {edit && snap && snap.prefix !== "" && (
          <p className="muted">
            {t("store.prefix")}: <code>{snap.prefix}</code>
          </p>
        )}
        <div className="setup-grid">
          <label className="setup-field">
            <span>{t("setup.backend")}</span>
            <select
              value={payload.backend}
              onChange={(e) => set("backend", e.target.value)}
            >
              <option value="s3">{t("setup.backend.s3")}</option>
              <option value="gcs">{t("setup.backend.gcs")}</option>
            </select>
          </label>
          <label className="setup-field">
            <span>{t("setup.bucket")}</span>
            <input
              value={payload.bucket}
              onChange={(e) => set("bucket", e.target.value)}
              placeholder="walgit"
            />
          </label>
          <label className="setup-field">
            <span>{t("setup.endpoint")}</span>
            <input
              value={payload.endpoint}
              onChange={(e) => set("endpoint", e.target.value)}
            />
            <small className="muted">
              {payload.backend === "s3" ? t("setup.endpoint.s3.hint") : t("setup.endpoint.gcs.hint")}
            </small>
          </label>
          {payload.backend === "s3" ? (
            <>
              <label className="setup-field">
                <span>{t("setup.region")}</span>
                <input value={payload.region} onChange={(e) => set("region", e.target.value)} />
                <small className="muted">{t("setup.region.hint")}</small>
              </label>
              <label className="setup-field">
                <span>{t("setup.access_key")}</span>
                <input
                  value={payload.access_key}
                  onChange={(e) => set("access_key", e.target.value)}
                  autoComplete="off"
                  placeholder={edit ? t("store.creds.keep") : undefined}
                />
                {edit && snap?.has_access_key && (
                  <small className="muted">{t("store.creds.set")}</small>
                )}
              </label>
              <label className="setup-field">
                <span>{t("setup.secret_key")}</span>
                <input
                  type="password"
                  value={payload.secret_key}
                  onChange={(e) => set("secret_key", e.target.value)}
                  autoComplete="new-password"
                  placeholder={edit ? t("store.creds.keep") : undefined}
                />
                <small className="muted">
                  {edit
                    ? snap?.has_secret_key
                      ? t("store.creds.set")
                      : t("store.creds.keep")
                    : t("setup.creds.hint")}
                </small>
              </label>
              <label className="setup-check">
                <input
                  type="checkbox"
                  checked={payload.force_path_style}
                  onChange={(e) => set("force_path_style", e.target.checked)}
                />
                {t("setup.force_path_style")}
              </label>
            </>
          ) : (
            <p className="muted">{t("setup.gcs.adc")}</p>
          )}
        </div>

        <p>
          <button className="btn primary" type="button" disabled={phase !== "form"} onClick={runTest}>
            {phase === "testing" ? t("setup.testing") : t("setup.test")}
          </button>
        </p>
        {testResult && (
          <div className={testResult.ok ? "setup-ok" : "setup-warn"}>
            {testResult.ok ? t("setup.test.ok") : `${t("setup.test.fail")} ${testResult.message}`}
          </div>
        )}

        {!edit && (
          <>
            <h2>{t("setup.step.admin")}</h2>
            <div className="setup-grid">
              <label className="setup-field">
                <span>{t("setup.admin.token")}</span>
                <input
                  type="password"
                  value={adminToken}
                  onChange={(e) => setAdminToken(e.target.value)}
                  autoComplete="new-password"
                />
                <small className="muted">{t("setup.admin.token.hint")}</small>
              </label>
              <label className="setup-field">
                <span>{t("setup.admin.principal")}</span>
                <input
                  value={adminPrincipal}
                  onChange={(e) => setAdminPrincipal(e.target.value)}
                  placeholder="admin"
                />
              </label>
            </div>
          </>
        )}

        <p>
          <button
            className="btn primary"
            type="button"
            disabled={phase !== "form" || !payload.bucket.trim() || !canSave}
            onClick={runSave}
          >
            {phase === "saving" ? t("setup.saving") : t("setup.save")}
          </button>
        </p>
        {!canSave && <p className="muted">{edit ? t("store.nosave") : t("setup.nosave")}</p>}
        {saveError && <div className="setup-warn">{`${t("setup.save.fail")} ${saveError}`}</div>}
      </div>
    </main>
  );
}
