import { useEffect, useState, type ReactNode } from "react";

import { api, client } from "../api";
import { useI18n } from "../i18n";

/**
 * The door for a `token`-mode host that refuses anonymous reads.
 *
 * A browser has nowhere to put a credential on such a host: there is no
 * sign-in page (only `oidc` mints a session), and every read answers 401 — so
 * the page it would enter the token on never loads. The server therefore serves
 * the static shell to a page navigation (`web::mod::shell_request`) and this
 * gate asks for the token the shell is missing: it holds it in
 * `sessionStorage` for the tab and switches the SDK to the bearer lane, exactly
 * what `git` does with the same string.
 *
 * A host that allows anonymous reads answers `me` with `anonymous: true` and the
 * gate steps out of the way, so the ordinary UI is unchanged.
 */
export function AuthGate({ children }: { children: ReactNode }) {
  const { t } = useI18n();
  const [needsToken, setNeedsToken] = useState(false);
  const [ready, setReady] = useState(false);
  const [token, setToken] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);

  useEffect(() => {
    let alive = true;
    // `client.me()` rather than `api.me()`: the latter reloads on 401, and a
    // reload here is the loop this gate exists to break.
    client.me().then(
      () => alive && setReady(true),
      (e: unknown) => {
        if (!alive) return;
        const status = (e as { status?: number }).status;
        if (status === 401 || status === 403) setNeedsToken(true);
        else setReady(true); // a broken API is not a credential problem
      },
    );
    return () => {
      alive = false;
    };
  }, []);

  if (ready) return <>{children}</>;
  if (!needsToken) {
    return (
      <main className="setup">
        <div className="setup-card">
          <p className="muted">{t("gate.checking")}</p>
        </div>
      </main>
    );
  }

  const submit = async () => {
    const value = token.trim();
    if (!value) return;
    setChecking(true);
    setError(null);
    api.setAdminToken(value);
    try {
      await client.me();
      window.location.reload();
    } catch (e) {
      const status = (e as { status?: number }).status;
      api.setAdminToken(null);
      setError(status === 403 ? t("gate.forbidden") : t("gate.bad"));
    } finally {
      setChecking(false);
    }
  };

  return (
    <main className="setup">
      <form
        className="setup-card"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <h1>{t("gate.title")}</h1>
        <p className="muted">{t("gate.lede")}</p>
        <label className="setup-field">
          <span>{t("gate.label")}</span>
          <input
            type="password"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            placeholder="wgt_… / <bearer token>"
            autoComplete="off"
            autoFocus
          />
          <small className="muted">{t("gate.hint")}</small>
        </label>
        {error && <div className="setup-warn">{error}</div>}
        <p>
          <button className="btn primary" type="submit" disabled={checking || !token.trim()}>
            {checking ? t("gate.checking") : t("gate.submit")}
          </button>
        </p>
        <p className="muted">{t("gate.note")}</p>
      </form>
    </main>
  );
}

/** Whether a status means "this host wants a credential" (exported for tests). */
export function wantsToken(status: number | undefined): boolean {
  return status === 401 || status === 403;
}
