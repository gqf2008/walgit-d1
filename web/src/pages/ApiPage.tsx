import { useState, type ReactNode } from "react";
import { useSearchParams } from "react-router-dom";
import { Box } from "../components/Layout";
import { CodeSample } from "../components/CopyButton";
import { useRecipes } from "../components/CloneSetup";
import { useI18n } from "../i18n";

/**
 * The API page: one API at `/api/v1`, two lanes (bearer / browser), one
 * SDK (`/repos.js`). This page advertises it and documents the surface for
 * humans; `/api/v1` is the machine-readable discovery document and
 * web/API.md the full contract. `?repo=owner/name` pre-fills the examples
 * (the title-bar tab links here with it from any repo page).
 */
export function ApiPage() {
  const { t } = useI18n();
  const [sp] = useSearchParams();
  const [repo, setRepo] = useState(sp.get("repo") || "acme/monorepo");
  const origin = window.location.origin;
  const r = repo.trim() || "owner/repo";
  const base = `${origin}/${r}/api`;
  const curl = `curl -fsS -H "Authorization: Bearer $token" -H "Accept: application/json"`;
  const recipes = useRecipes();

  return (
    <div className="api-page">
      <h1 className="page-title">API</h1>
      <p className="lead">
        {t("apipage.lead.a")}
        <code>/api/v1</code>
        {t("apipage.lead.b")}
        <code>repos.js</code>
        {t("apipage.lead.c")}
        <code>git fetch</code>
        {t("apipage.lead.d")}
      </p>

      <div className="row gap">
        <Box title={t("apipage.browser.title")} className="grow">
          <div className="pad">
            <CodeSample
              code={`<script src="${origin}/repos.js"></script>
<script type="module">
  const r = repos.repo("${r}");
  const { head } = await r.refs();    // O(1): default branch only
  const tree = await r.tree(head.sha, "");  // by sha: immutable, cached
  render(tree.entries);
</script>`}
            />
            <div className="small muted">
              {t("apipage.sdk.picks")} <code>import {"{ createClient }"} from "{origin}/repos.mjs"</code>
              {t("apipage.dot")}
            </div>
          </div>
        </Box>
        <Box title={t("apipage.shell.title")} className="grow">
          <div className="pad">
            <label className="small strong api-repo-input">
              {t("apipage.repository")}{" "}
              <input value={repo} onChange={(e) => setRepo(e.target.value)} placeholder="owner/repo" spellCheck={false} />
            </label>
            <div className="small muted">
              {t("apipage.oncePerMachine")} <code>{recipes.install}</code>
              {recipes.token_url && (
                <>
                  {" "}
                  {t("apipage.tokens")} <a href={recipes.token_url}>{recipes.token_url.replace(/^https?:\/\//, "")}</a>
                  {t("apipage.dot")}
                </>
              )}
            </div>
            <CodeSample code={`token="$WALGIT_TOKEN"\n${curl} ${base}`} />
            <CodeSample code={`${curl} "${base}/tree/main/"`} />
            <CodeSample code={`${curl} "${base}/commits?ref=main&n=10" | jq -r '.commits[] | .sha[:8] + " " + .subject'`} />
            <div className="small muted">
              {t("apipage.signedIn.a")}{" "}
              <a href={`/${r}/api`} target="_blank" rel="noreferrer">
                {t("apipage.open")} <code>/{r}/api</code>
              </a>
              {t("apipage.signedIn.b")}{" "}
              <a href="/api/v1" target="_blank" rel="noreferrer">
                <code>/api/v1</code>
              </a>
              {t("apipage.discovery")}
            </div>
          </div>
        </Box>
      </div>

      <Box title={t("apipage.endpoints.title")}>
        <table className="api-table">
          <thead>
            <tr>
              <th>{t("apipage.th.path")}</th>
              <th>{t("apipage.th.returns")}</th>
              <th>{t("apipage.th.cache")}</th>
            </tr>
          </thead>
          <tbody>
            <Row path="/api/v1" desc={<>{t("apipage.row.discovery.a")} <code>{`{base, browser_base, sdk, auth, endpoints}`}</code>{t("apipage.dot")}</>} cache="—" />
            <Row path="/api/v1/me" desc={<><code>{`{principal, write, anonymous}`}</code> {t("apipage.row.me.a")}</>} cache="no-store" />
            <Row path="/api/v1/owners" desc={t("apipage.row.owners")} cache="SWR" />
            <Row path="/api/v1/owners/{owner}/repos" desc={t("apipage.row.ownerRepos")} cache="SWR" />
            <Row
              path={`/${r}/api`}
              desc={
                <>
                  {t("apipage.row.repo.a")} <code>{`{owner,name,full_name,head,branches,tags,clone_url,html_url}`}</code> {t("apipage.row.repo.b")} <code>PUT</code> {t("apipage.row.repo.c")} <code>DELETE</code> {t("apipage.row.repo.d")}
                </>
              }
              cache="SWR + ETag"
            />
            <Row path={`…/${r}/refs`} desc={<>{t("apipage.row.refs.a")} <code>{`{head:{name,sha}|null}`}</code> {t("apipage.row.refs.b")}</>} cache="SWR + ETag" />
            <Row
              path={`…/${r}/refs/{branches|tags}?prefix=&q=&after=&n=`}
              desc={
                <>
                  {t("apipage.row.refspage.a")} <code>{`{refs:[{name,sha}],more}`}</code>
                  {t("apipage.row.refspage.b")} <code>n</code> {t("apipage.row.refspage.c")} <code>Accept: text/event-stream</code>
                  {t("apipage.row.refspage.d")} <code>ref</code> {t("apipage.row.refspage.e")}
                </>
              }
              cache="SWR"
            />
            <Row
              path={`…/${r}/resolve/{ref/path…}`}
              desc={
                <>
                  {t("apipage.row.resolve.a")} <code>ref/path</code> {t("apipage.row.resolve.b")} <code>{`{ref,sha,path,kind}`}</code>
                  {t("apipage.row.resolve.c")}
                </>
              }
              cache="SWR + ETag"
            />
            <Row
              path={`…/${r}/tree/{rev}/{path}`}
              desc={
                <>
                  {t("apipage.row.tree.a")} <code>{`{entries:[{name,type,mode,size,sha}],commit?,readme?}`}</code>
                  {t("apipage.row.tree.b")}
                </>
              }
              cache="sha → immutable · name → SWR + ETag"
            />
            <Row
              path={`…/${r}/blob/{rev}/{path}[?raw]`}
              desc={
                <>
                  <code>{`{name,size,contents}`}</code> {t("apipage.row.blob.a")} <code>binary:true</code> / <code>too_large:true</code>; <code>?raw</code> {t("apipage.row.blob.b")} <code>{`{Content-Type}`}</code> {t("apipage.row.blob.c")}
                  {t("apipage.dot")}
                </>
              }
              cache="sha → immutable · name → SWR + ETag"
            />
            <Row
              path={`…/${r}/commits?ref=&path=&skip=&n=`}
              desc={
                <>
                  {t("apipage.row.commits.a")} <code>{`{commits:[Commit],more}`}</code>
                  {t("apipage.row.commits.b")} <code>n</code> {t("apipage.row.commits.c")} <code>skip += commits.length</code>
                  {t("apipage.dot")}
                </>
              }
              cache="sha → immutable · name → SWR + ETag"
            />
            <Row
              path={`…/${r}/commit/{sha}`}
              desc={
                <>
                  <code>{`{commit,stats:[{path,additions,deletions}],patch}`}</code> {t("apipage.row.commit.a")}
                </>
              }
              cache="full sha → immutable · else SWR + ETag"
            />
            <Row path={`…/${r}/policy`} desc={<>{t("apipage.row.policy.a")} <code>GET</code>/<code>PUT</code>/<code>DELETE</code>{t("apipage.row.policy.b")}</>} cache="no-store" />
            <Row path={`…/${r}/overview`} desc={t("apipage.row.overview")} cache="no-store" />
            <Row
              path={`…/${r}/tasks · /tasks/{id} · POST /ops/{op}`}
              desc={
                <>
                  {t("apipage.row.tasks.a")} <code>{`{hostname,running,recent}`}</code>
                  {t("apipage.row.tasks.b")}
                </>
              }
              cache="no-store"
            />
          </tbody>
        </table>
      </Box>

      <div className="row gap">
        <Box title={t("apipage.lanes.title")} className="grow">
          <ul className="api-notes">
            <li>
              <strong>{t("apipage.lanes.bearer")}</strong>: <code>Authorization: Bearer &lt;token&gt;</code>
              {t("apipage.lanes.bearer.a")} <code>/{"{owner}/{repo}"}/api/*</code> {t("apipage.lanes.bearer.b")}
            </li>
            <li>
              <strong>{t("apipage.lanes.browser")}</strong> <code>/{"{owner}/{repo}"}/api-browser/*</code>
              {t("apipage.lanes.browser.a")} <code>credentials: "include"</code>
              {t("apipage.lanes.browser.b")} <code>/api-browser/v1/authenticate</code>
              {t("apipage.lanes.browser.c")}
            </li>
            <li>
              <strong>{t("apipage.lanes.errors")}</strong>
              {t("apipage.lanes.errors.a")} <code>404</code> {t("apipage.lanes.errors.b")} <code>401</code> {t("apipage.lanes.errors.c")} <code>5xx</code> {t("apipage.lanes.errors.d")}
            </li>
            <li>
              <strong>{t("apipage.lanes.shapes")}</strong>
              {t("apipage.lanes.shapes.a")} <code>[]</code> {t("apipage.lanes.shapes.b")} <code>v1</code>
              {t("apipage.dot")}
            </li>
          </ul>
        </Box>
        <Box title={t("apipage.caching.title")} className="grow">
          <ul className="api-notes">
            <li>
              <strong>{t("apipage.caching.resolve")}</strong>
              {t("apipage.caching.resolve.a")} <code>immutable</code>
              {t("apipage.caching.resolve.b")} <code>stale-while-revalidate=60</code> {t("apipage.caching.resolve.c")} <code>ETag</code> {t("apipage.caching.resolve.d")} <code>If-None-Match</code> → 304.
            </li>
            <li>
              <strong>{t("apipage.caching.fresh")}</strong>
              {t("apipage.caching.fresh.a")}
            </li>
            <li>
              <strong>{t("apipage.caching.long")}</strong>
              {t("apipage.caching.long.a")} <code>Accept: application/json, text/event-stream</code> {t("apipage.caching.long.b")} <code>notice</code> / <code>progress</code> / <code>task</code> {t("apipage.caching.long.c")} <code>result</code> {t("apipage.caching.long.d")} <code>error</code>
              {t("apipage.caching.long.e")} <code>onProgress</code>
              {t("apipage.dot")}
            </li>
          </ul>
          <div className="pad" style={{ paddingTop: 0 }}>
            <CodeSample code={`${curl.replace("application/json", "application/json, text/event-stream")} -N "${base}/tree/main/"`} />
          </div>
        </Box>
      </div>

      <p className="small muted">
        {t("apipage.footer.a")} <code>web/API.md</code>
        {t("apipage.footer.b")} <code>web/sdk/README.md</code> {t("apipage.footer.c")}
      </p>
    </div>
  );
}

function Row({ path, desc, cache }: { path: string; desc: ReactNode; cache: string }) {
  return (
    <tr>
      <td>
        <code>{path}</code>
      </td>
      <td>{desc}</td>
      <td className="muted small">{cache}</td>
    </tr>
  );
}
