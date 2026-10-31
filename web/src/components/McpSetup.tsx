import { useRef, useState } from "react";
import { Box } from "./Layout";
import { useI18n } from "../i18n";

/**
 * 「MCP」box shown on the collab pages (D64): this host serves the Model
 * Context Protocol itself at `/mcp` over HTTP Streamable, so an agent client
 * needs a URL — no local `walgit mcp` process, no second contract.
 *
 * The page is a *static* explainer: the endpoint, the client-config snippet and
 * one copy-pasteable handshake. It reads no repository data; the only
 * repo-scoped text is the resource URI template for the repository whose
 * collab page this is, which comes from the route (`full`), never from a
 * request.
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

/** A client-side registration snippet (`mcpServers` shape every MCP host reads).
 *
 * `${WALGIT_TOKEN}` — braces, not a bare `$NAME`: the clients that expand
 * anything expand this form (Claude Code's `.mcp.json` `${VAR}`/`${VAR:-default}`),
 * and the ones that expand nothing show the placeholder instead of silently
 * sending the literal text as a bearer. */
export function mcpClientConfig(endpoint: string): string {
  return [
    "{",
    '  "mcpServers": {',
    '    "walgit": {',
    '      "type": "http",',
    `      "url": "${endpoint}",`,
    '      "headers": { "Authorization": "Bearer ${WALGIT_TOKEN}" }',
    "    }",
    "  }",
    "}",
  ].join("\n");
}

/** The handshake, by hand: POST one JSON-RPC message, read the answer. */
export function mcpHandshake(endpoint: string): string {
  const body = JSON.stringify({
    jsonrpc: "2.0",
    id: 1,
    method: "initialize",
    params: {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "curl", version: "1" },
    },
  });
  return [
    `curl -sS -D - ${endpoint} \\`,
    '  -H "Authorization: Bearer ${WALGIT_TOKEN}" \\',
    '  -H "Content-Type: application/json" \\',
    '  -H "Accept: application/json, text/event-stream" \\',
    `  -d '${body}'`,
  ].join("\n");
}

/** A subscription, by hand: the same POST shape, with the session header the
 * handshake returned. */
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
    '  -H "mcp-session-id: <Mcp-Session-Id from the handshake>" \\',
    `  -d '${body}'`,
  ].join("\n");
}

/** The stream the notifications arrive on: one `GET /mcp`, held open with the
 * session the handshake minted. Without it a subscription answers success and
 * delivers nothing, which is the failure this line exists to prevent. */
export function mcpStream(endpoint: string): string {
  return [
    `curl -N ${endpoint} \\`,
    '  -H "Authorization: Bearer ${WALGIT_TOKEN}" \\',
    '  -H "Accept: text/event-stream" \\',
    '  -H "mcp-session-id: <Mcp-Session-Id from the handshake>"',
  ].join("\n");
}

/** What arrives on the notification stream once the watched URI moves. */
export function mcpNotification(uri: string): string {
  return [
    "event: message",
    `data: {"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"${uri}"}}`,
  ].join("\n");
}

export function McpSetup({ full }: { full: string }) {
  const { t } = useI18n();
  const origin = typeof window === "undefined" ? "https://<host>" : window.location.origin;
  const endpoint = mcpEndpoint(origin);
  return (
    <Box title={t("mcp.title")}>
      <div className="pad">
        <p className="cmds-lead">{t("mcp.lede", { endpoint })}</p>
        <div className="box-header">{t("mcp.register")}</div>
        <CodeBlock code={mcpClientConfig(endpoint)} />
        <p className="cmds-note">{t("mcp.register.note")}</p>
        <div className="box-header">{t("mcp.handshake")}</div>
        <CodeBlock code={mcpHandshake(endpoint)} />
        <p className="cmds-note">{t("mcp.handshake.note")}</p>
        <div className="box-header">{t("mcp.resources")}</div>
        <p className="cmds-note mono" style={{ whiteSpace: "pre-wrap" }}>
          {[
            "walgit://refs/{owner}/{repo}",
            "walgit://wal/{owner}/{repo}{?from}",
            "walgit://collab/board/{owner}/{repo}",
            "walgit://collab/thread/{owner}/{repo}/{thread}",
          ].join("\n")}
        </p>
        <p className="cmds-note">{t("mcp.resources.note", { repo: full })}</p>
        <div className="box-header">{t("mcp.subscribe")}</div>
        <CodeBlock code={mcpSubscribe(endpoint, `walgit://refs/${full}`)} />
        <CodeBlock code={mcpSubscribe(endpoint, `walgit://collab/board/${full}`)} />
        <p className="cmds-note">{t("mcp.subscribe.note")}</p>
        <div className="box-header">{t("mcp.stream")}</div>
        <CodeBlock code={mcpStream(endpoint)} />
        <p className="cmds-note">{t("mcp.stream.note")}</p>
        <div className="box-header">{t("mcp.notify")}</div>
        <CodeBlock code={mcpNotification(`walgit://collab/board/${full}`)} />
        <p className="cmds-note">{t("mcp.notify.note")}</p>
        <ul className="cmds-note" style={{ margin: 0, paddingLeft: 18 }}>
          <li>{t("mcp.rule.bytes")}</li>
          <li>{t("mcp.rule.write")}</li>
          <li>{t("mcp.rule.subs")}</li>
        </ul>
      </div>
    </Box>
  );
}
