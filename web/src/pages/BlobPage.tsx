import { useState } from "react";
import { Link, useParams } from "react-router-dom";
import { File } from "@pierre/diffs/react";
import { api, client } from "../api";
import { useResolved } from "../use-resolved";
import { useRepo } from "./RepoLayout";
import { Box } from "../components/Layout";
import { fmtSize } from "../format";
import { RefBar } from "../components/RefBar";
import { Markdown } from "../components/Markdown";
import { useI18n } from "../i18n";
import { dirOf } from "../markdown-paths";

export function BlobPage() {
  const { full } = useRepo();
  const { t } = useI18n();
  const rest = useParams()["*"] ?? "";
  const { r, data: b } = useResolved(full, rest, (res) => api.blob(full, res.sha, res.path));
  const isMd = /\.(md|markdown)$/i.test(rest);
  const [mode, setMode] = useState<"preview" | "code">("preview");
  const lines = b.contents ? b.contents.split("\n").length - (b.contents.endsWith("\n") ? 1 : 0) : 0;
  const rawURL = client.repo(full).urls.raw(b.sha, b.path);
  return (
    <>
      <RefBar refname={r.ref} refKind={r.kind} path={r.path} page="blob" />
      <Box
        className="blob"
        title={
          <div className="blob-head">
            {isMd && b.contents !== undefined && (
              <span className="seg">
                <button className={mode === "preview" ? "active" : ""} onClick={() => setMode("preview")}>
                  {t("blob.preview")}
                </button>
                <button className={mode === "code" ? "active" : ""} onClick={() => setMode("code")}>
                  {t("blob.code")}
                </button>
              </span>
            )}
            <span className="muted small">
              {b.contents !== undefined && `${t("blob.lines", { n: lines })} · `}
              {fmtSize(b.size)}
            </span>
            <span className="spacer" />
            <a className="btn small" href={rawURL} target="_blank" rel="noreferrer">
              {t("blob.raw")}
            </a>
            <Link className="btn small" to={`/${full}/commits/${b.ref}/${b.path}`}>
              {t("blob.history")}
            </Link>
          </div>
        }
      >
        {b.too_large && <div className="pad muted">{t("blob.tooLarge", { size: fmtSize(b.size) })}</div>}
        {b.binary && <div className="pad muted">{t("blob.binary")}</div>}
        {b.contents !== undefined &&
          (isMd && mode === "preview" ? (
            <div className="pad">
              <Markdown
                source={b.contents}
                base={{ ref: b.ref, dir: dirOf(b.path), urls: client.repo(full).urls }}
              />
            </div>
          ) : (
            <File
              file={{ name: b.name, contents: b.contents.replace(/\n$/, "") }}
              options={{ disableFileHeader: true, themeType: "light", overflow: "scroll" }}
            />
          ))}
      </Box>
    </>
  );
}
