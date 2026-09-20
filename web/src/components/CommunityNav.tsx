import { NavLink } from "react-router-dom";
import { useI18n } from "../i18n";

export function CommunityNav({ full }: { full: string }) {
  const { t } = useI18n();
  const base = `/${full}`;
  return (
    <nav className="community-nav" aria-label={t("community.nav.aria")}>
      <NavLink to={`${base}/collab`} end>{t("community.nav.overview")}</NavLink>
      <NavLink to={`${base}/collab/discussions`}>{t("community.nav.discussions")}</NavLink>
      <NavLink to={`${base}/collab/projects`}>{t("community.nav.projects")}</NavLink>
      <NavLink to={`${base}/collab/guide`}>{t("community.nav.guide")}</NavLink>
    </nav>
  );
}
