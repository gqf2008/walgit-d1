import { useRef, useState } from "react";
import { Box } from "./Layout";
import { useI18n } from "../i18n";

/**
 * 「MCP」box on the collab pages (D64): this host serves the Model Context
 * Protocol itself at `/mcp` over HTTP Streamable, so a client needs a URL and
 * the URI of the resource it wants to watch — nothing runs locally.
 *
 * Deliberately three lines: the endpoint, the two subscriptions that cover this
 * project, and the stream the notifications arrive on (a subscription without a
 * held stream answers success and delivers nothing, which is the one mistake
 * worth spending a line on). Static: the only repo-scoped text is the URI built
 * from the route, never from a request.
 */

/** One copy-on-click block (clipboard first; selection as the fallback —
 * the same two-step the `/commands` page uses). */
function CodeBlock({ code }: { code: string }) {
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
      await navigator.clipboard.writeText(code);
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
        {code}
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

/** A subscription: one POST, authorized like every other request, carrying the
 * session the `initialize` answer handed back. */
export function mcpSubscribe(endpoint: string, uri: string): string {
  const body = JSON.stringify({
    jsonrpc: "2.0",
    id: 2,
    method: "resources/subscribe",
    params: { uri },
  });
  return [
    `curl -sS ${endpoint} \\`,
    '  -H "Authorization: Bearer ${WALGIT_TOKEN}" \\',
    '  -H "Content-Type: application/json" \\',
    '  -H "Accept: application/json, text/event-stream" \\',
    '  -H "mcp-session-id: <Mcp-Session-Id from initialize>" \\',
    `  -d '${body}'`,
  ].join("\n");
}

/** The stream the notifications arrive on: one `GET /mcp`, held open with the
 * same session. Without it a subscription answers success and delivers nothing. */
export function mcpStream(endpoint: string): string {
  return [
    `curl -N ${endpoint} \\`,
    '  -H "Authorization: Bearer ${WALGIT_TOKEN}" \\',
    '  -H "Accept: text/event-stream" \\',
    '  -H "mcp-session-id: <Mcp-Session-Id from initialize>"',
  ].join("\n");
}

export function McpSetup({ full }: { full: string }) {
  const { t } = useI18n();
  const origin = typeof window === "undefined" ? "https://<host>" : window.location.origin;
  const endpoint = mcpEndpoint(origin);
  return (
    <Box title={t("mcp.title")}>
      <div className="pad">
        <div className="box-header">{t("mcp.endpoint")}</div>
        <CodeBlock code={endpoint} />
        <div className="box-header">{t("mcp.subscribe")}</div>
        <CodeBlock code={mcpSubscribe(endpoint, `walgit://refs/${full}`)} />
        <CodeBlock code={mcpSubscribe(endpoint, `walgit://collab/board/${full}`)} />
        <p className="cmds-note">{t("mcp.stream.note")}</p>
        <CodeBlock code={mcpStream(endpoint)} />
      </div>
    </Box>
  );
}
