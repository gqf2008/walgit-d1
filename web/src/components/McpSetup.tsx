import { useRef, useState } from "react";
import { Box } from "./Layout";
import { useI18n } from "../i18n";

/**
 * 「MCP」box on the collab pages (D64): this host serves the Model Context
 * Protocol itself at `/mcp` over HTTP Streamable.
 *
 * Two things and nothing else — the endpoint, and the `walgit://` URIs this
 * project exposes for `resources/read` and `resources/subscribe`. MCP is a
 * standard: how to call it (session header, `Accept`, JSON-RPC) is the spec's
 * job, not this page's, and every client already knows it.
 *
 * Static: the only repo-scoped text is built from the route, never a request.
 */

/** One copy-on-click line (clipboard first; selection as the fallback — the
 * same two-step the `/commands` page uses). */
function CopyLine({ value }: { value: string }) {
  const { t } = useI18n();
  const node = useRef<HTMLElement>(null);
  const timer = useRef<number | undefined>(undefined);
  const [state, setState] = useState<"idle" | "copied" | "selected">("idle");

  function flash(next: "copied" | "selected") {
    setState(next);
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setState("idle"), 1400);
  }

  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      flash("copied");
    } catch {
      const el = node.current;
      const selection = window.getSelection();
      if (el && selection) {
        const range = document.createRange();
        range.selectNodeContents(el);
        selection.removeAllRanges();
        selection.addRange(range);
      }
      flash("selected");
    }
  }

  const label =
    state === "copied" ? t("cmds.copied") : state === "selected" ? t("cmds.selected") : t("cmds.copy");
  return (
    <div className="cmds-row">
      <code ref={node} className="mono">
        {value}
      </code>
      <button type="button" className="cmds-copy" onClick={copy} aria-label={`${t("cmds.copy")}: ${label}`}>
        {label}
      </button>
    </div>
  );
}

/** The host's MCP endpoint, as this browser reached it. */
export function mcpEndpoint(origin: string): string {
  return `${origin.replace(/\/+$/, "")}/mcp`;
}

/** The resources `owner/repo` exposes: refs, the retained log, and the board
 * projection. `resources/templates/list` carries the shapes for any other
 * repository (and the per-thread one). */
export function mcpResources(full: string): string[] {
  return [
    `walgit://refs/${full}`,
    `walgit://wal/${full}`,
    `walgit://collab/board/${full}`,
  ];
}

export function McpSetup({ full }: { full: string }) {
  const { t } = useI18n();
  const origin = typeof window === "undefined" ? "https://<host>" : window.location.origin;
  return (
    <Box title={t("mcp.title")}>
      <div className="pad">
        <div className="box-header">{t("mcp.endpoint")}</div>
        <CopyLine value={mcpEndpoint(origin)} />
        <div className="box-header">{t("mcp.resources")}</div>
        {mcpResources(full).map((uri) => (
          <CopyLine key={uri} value={uri} />
        ))}
      </div>
    </Box>
  );
}
