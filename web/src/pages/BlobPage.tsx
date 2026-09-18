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
import { blobKind } from "../blob-kind";

export function BlobPage() {
  const { full } = useRepo();
  const { t } = useI18n();
  const rest = useParams()["*"] ?? "";
  const { r, data: b } = useResolved(full, rest, (res) => api.blob(full, res.sha, res.path));
  // Presentation only: the API tells us whether the bytes are text, `?raw`
  // serves every type — this just picks the viewer.
  const kind = blobKind(b.path);
  const isMd = kind === "markdown";
  const isViewer =
    kind === "image" || kind === "video" || kind === "audio" || kind === "pdf" || kind === "html";
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
        {isViewer && (
          <div className="pad">
            {kind === "image" && <img className="blob-image" src={rawURL} alt={b.name} />}
            {kind === "video" && <video className="blob-media" src={rawURL} controls preload="metadata" />}
            {kind === "audio" && <audio className="blob-media" src={rawURL} controls preload="metadata" />}
            {/* `<object>`, not a sandboxed iframe: `sandbox=""` also disables the
                browser's own PDF viewer, and the built-in viewers do not execute
                embedded PDF JavaScript or expose the embedding document. That is
                an **accepted risk** pending a browser matrix (D50) — the API
                sends `application/pdf` + nosniff and nothing else. The fallback
                child covers browsers with no inline viewer. */}
            {kind === "pdf" && (
              <object className="blob-frame" data={rawURL} type="application/pdf" title={b.name}>
                <p className="muted small">{t("blob.pdfNoViewer")}</p>
              </object>
            )}
            {kind === "html" && (
              <>
                <p className="muted small">{t("blob.htmlSandboxed")}</p>
                {/* sandbox="" — no scripts, no same-origin: repository HTML is
                    untrusted input and must never reach the app's origin. */}
                <iframe className="blob-frame" sandbox="" src={rawURL} title={b.name} />
              </>
            )}
          </div>
        )}
        {!isViewer && b.too_large && (
          <div className="pad muted">{t("blob.tooLarge", { size: fmtSize(b.size) })}</div>
        )}
        {!isViewer && !b.too_large && b.binary && <div className="pad muted">{t("blob.binary")}</div>}
        {!isViewer && b.contents !== undefined &&
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
