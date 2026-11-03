import { api, type SetupRecipes } from "../api";
import { useData } from "../data";
import { CodeSample, CopyButton } from "./CopyButton";
import { useI18n } from "../i18n";

/**
 * Clone/setup recipes for this host, rendered by the server at
 * `/services/setup.json` (crates/walgit-server/src/setup.rs — the same strings
 * go into /services/public/install.sh, the overview JSON and git's auth error text).
 * The UI is a client of those recipes, never a fork: where tokens come from and
 * what the installer does are the server's to define.
 *
 * Only `/services/public/*` (the installer) is reachable without a credential;
 * everything else needs a signed-in browser or a bearer token.
 */
export function useRecipes(repo?: string): SetupRecipes {
  return useData(`setup:${repo ?? ""}`, () => api.setupRecipes(repo), Infinity);
}

/** Shared by the Clone dropdown (compact) and the WAL page. `repo` = owner/name. */
export function CloneSetup({ repo, compact = false }: { repo: string; compact?: boolean }) {
  const { t } = useI18n();
  const r = useRecipes(repo);
  const url = `${r.base_url}/${repo}.git`;
  return (
    <div className="clone-setup">
      <div className="small strong">{t("clone.runOnce")}</div>
      <div className="small muted">
        {t("clone.tokenOnce")}
        {r.token_url && (
          <>
            {t("clone.tokenCreate.a")}
            <a href={r.token_url} target="_blank" rel="noreferrer">
              {r.token_url.replace(/^https?:\/\//, "")}
            </a>
            {t("clone.tokenCreate.b")}
          </>
        )}
        {t("clone.rest")}
      </div>
      <CodeSample code={r.install} />
      <div className="small strong">{t("clone.setupDone")}</div>
      <CodeSample code={r.plain_clone} />
      <div className="small strong">{t("clone.ci")}</div>
      <CodeSample code={r.manual_clone} />
      {r.url_clone && (
        <>
          <div className="small strong">{t("clone.urlOnce")}</div>
          <div className="small muted">{t("clone.urlNote")}</div>
          {r.ca_url && <div className="small muted">{t("clone.urlSelfSigned")}</div>}
          <CodeSample code={r.url_clone} />
        </>
      )}
      <div className="clone-actions">
        <CopyButton text={() => api.installScript(repo)} label={t("clone.copyInstaller")} />
        <a className="btn btn-small" href={`/services/public/install.sh?repo=${repo}`} download="install.sh">
          {t("clone.download")}
        </a>
      </div>
      {!compact && (
        <p className="small muted">
          {t("clone.bundles.a")}
          <code>-c fetch.bundleURI=…/bundles/catchup</code>
          {t("clone.bundles.b")}
          <code>git fetch</code>
          {t("clone.bundles.c")}
          <code>transfer.bundleURI=true</code>
          {t("clone.and")}
          <code>fetch.uriProtocols=https</code>
          {t("clone.global")}
          {t("clone.bundles.d")}
        </p>
      )}
      {!compact && (
        <>
          <p className="small muted">
            <strong>{t("clone.blobless.a")}</strong>
            {t("clone.blobless.b")}
            <code>--sparse</code>
            {t("clone.blobless.c")}
            <code>git sparse-checkout add</code>
            {t("clone.dot")}
          </p>
          <CodeSample code={r.blobless_clone} />
          <p className="small muted">
            <strong>{t("clone.ciShallow.a")}</strong>
            {t("clone.ciShallow.b")}
            <code>--depth</code>, <code>--single-branch</code>
            {t("clone.ciShallow.c")}
            <code>-c transfer.bundleURI=false</code>
            {t("clone.ciShallow.d")}
          </p>
        </>
      )}
      <div className="small muted">{t("clone.cloneUrl")}</div>
      <CodeSample code={url} />
    </div>
  );
}
