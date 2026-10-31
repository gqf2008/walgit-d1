import { useRef, useState } from "react";
import { Box } from "./Layout";
import { useI18n } from "../i18n";

/**
 * 「MCP」box on the collab pages (D64): this host serves the Model Context
 * Protocol itself at `/mcp` over HTTP Streamable.
 *
 * One paragraph, one copy button: the text names the endpoint and the
 * `walgit://` URIs this project exposes, so it can be handed to an agent as-is
 * and the agent wires up its own client. MCP is a standard — the session header,
 * the methods and the notification shape are the spec's job, not this page's.
 *
 * Static: the only repo-scoped text is built from the route, never a request.
 */

/** The host's MCP endpoint, as this browser reached it. */
export function mcpEndpoint(origin: string): string {
  return `${origin.replace(/\/+$/, "")}/mcp`;
}

/** The URIs `owner/repo` exposes: refs, the retained log, the board projection.
 * (`resources/templates/list` carries the shapes for any other repository and
 * the per-thread one.) */
export function mcpResources(full: string): string[] {
  return [`walgit://refs/${full}`, `walgit://wal/${full}`, `walgit://collab/board/${full}`];
}

export function McpSetup({ full }: { full: string }) {
  const { t } = useI18n();
  const origin = typeof window === "undefined" ? "https://<host>" : window.location.origin;
  const [refs, wal, board] = mcpResources(full);
  const brief = t("mcp.brief", {
    endpoint: mcpEndpoint(origin),
    refs: refs!,
    wal: wal!,
    board: board!,
  });

  const timer = useRef<number | undefined>(undefined);
  const [copied, setCopied] = useState(false);
  async function copy() {
    try {
      await navigator.clipboard.writeText(brief);
    } catch {
      // No clipboard (insecure context): select the paragraph so the reader
      // can copy it with the keyboard instead.
      const selection = window.getSelection();
      const el = document.getElementById("mcp-brief");
      if (selection && el) {
        const range = document.createRange();
        range.selectNodeContents(el);
        selection.removeAllRanges();
        selection.addRange(range);
      }
    }
    setCopied(true);
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setCopied(false), 1400);
  }

  return (
    <Box title={t("mcp.title")}>
      <div className="pad">
        <p id="mcp-brief" className="cmds-note">
          {brief}
        </p>
        <button type="button" className="cmds-copy" onClick={copy}>
          {copied ? t("cmds.copied") : t("cmds.copy")}
        </button>
      </div>
    </Box>
  );
}
