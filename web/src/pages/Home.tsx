import { api } from "../api";
import { useData } from "../data";
import { Box } from "../components/Layout";
import { CodeSample } from "../components/CopyButton";
import { useI18n } from "../i18n";

/**
 * The site root "/" (redesigned 2026-10-23): one sentence that says what
 * walgit is, a paragraph leading into /SKILL.md, and five copyable role
 * prompts (leader / developer / reviewer / tester / devops) matching the
 * D59/D62 topology. The old static model essay moved out — the same story
 * is taught on a repository's live data at /<owner>/<repo>/collab/guide
 * (#41), linked from here when the host has a repository. The host-wide
 * repository index stays the top-nav「仓库」entry (/repos, #59); the CLI
 * cheat sheet is /commands (D61). An empty host shows the install slate.
 */

const ROLES = ["leader", "dev", "reviewer", "tester", "devops"] as const;

export function Home() {
  const owners = useData("owners", api.owners);
  const firstOwner = owners[0] ?? null;
  const repos = useData(firstOwner ? `repos:${firstOwner}` : "repos:none", () =>
    firstOwner ? api.repos(firstOwner) : Promise.resolve([] as string[]),
  );
  if (owners.length === 0) {
    return <BlankSlate />;
  }
  const live = firstOwner && repos.length > 0 ? { owner: firstOwner, repo: repos[0]! } : null;
  return <HomeBody live={live} />;
}

/** The presentation, split from the data layer so tests render it directly. */
export function HomeBody({ live }: { live: { owner: string; repo: string } | null }) {
  const { t } = useI18n();
  return (
    <div className="home-wrap">
      <header className="model-hero">
        <div className="eyebrow">{t("home.eyebrow")}</div>
        <h1 className="model-title">{t("home.oneline")}</h1>
        <p className="model-lede">
          {t("home.skill.para.pre")}
          <a href="/SKILL.md">SKILL.md</a>
          {t("home.skill.para.post")}{" "}
          <code className="mono">/commands</code>
          {t("home.skill.para.commands")}
        </p>
        {live && (
          <div className="model-live">
            <a className="btn btn-primary" href={`/${live.owner}/${live.repo}/collab/guide`}>
              {t("home.live")}
            </a>
          </div>
        )}
      </header>

      <section aria-labelledby="home-roles">
        <div className="model-head">
          <h2 id="home-roles">{t("home.roles.t")}</h2>
        </div>
        <p className="model-sub">{t("home.roles.lede")}</p>
        <div className="home-roles">
          {ROLES.map((role) => (
            <div key={role} className="home-role">
              <h3>{t(`home.role.${role}.t`)}</h3>
              <CodeSample code={t(`home.role.${role}.body`)} />
            </div>
          ))}
        </div>
      </section>
    </div>
  );
}

/** First repo: install.sh (this host:port) sets helper + proactiveAuth + origin. */
function BlankSlate() {
  const { t } = useI18n();
  const origin = window.location.origin;
  const host = window.location.host;
  const install = `sh -c "$(curl -fsSLk '${origin}/services/public/install.sh')" -- area/repository`;
  return (
    <div className="blankslate">
      <h1>{t("blank.title")}</h1>
      <p>
        {t("blank.intro.pre")}
        <code>area/repository</code>
        {t("blank.intro.mid1")}
        <code>http.https://{host}/.proactiveAuth=auto</code>
        {t("blank.intro.mid2")}
        <code>origin</code>
        {t("blank.intro.mid3")}
        <code>{origin}/area/repository.git</code>
        {t("blank.intro.mid4")}
        <code>area/repository.git</code>
        {t("blank.intro.post")}
      </p>
      <Box title={t("blank.once")}>
        <CodeSample code={`${install}\ngit push -u origin HEAD`} />
      </Box>
      <p className="muted small">
        <code>area</code>
        {t("blank.names.and")}
        <code>repository</code>
        {t("blank.names.rule.pre")}
        <code>[A-Za-z0-9._-]</code>
        {t("blank.names.rule.post")}
      </p>
    </div>
  );
}
