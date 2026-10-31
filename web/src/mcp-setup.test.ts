import { describe, expect, it } from "vitest";
import {
  mcpClientConfig,
  mcpEndpoint,
  mcpHandshake,
  mcpNotification,
  mcpStream,
  mcpSubscribe,
} from "./components/McpSetup";

/**
 * The MCP box (D64) shows copy-pasteable snippets; these assertions pin the two
 * facts a client cannot guess — the endpoint's shape, and the handshake's two
 * required headers — so a refactor of the rendering cannot quietly ship a
 * snippet that a real client rejects.
 */
describe("mcp setup snippets", () => {
  it("appends the one endpoint to the host root", () => {
    expect(mcpEndpoint("https://walgit.example")).toBe("https://walgit.example/mcp");
    expect(mcpEndpoint("https://walgit.example/")).toBe("https://walgit.example/mcp");
    expect(mcpEndpoint("http://127.0.0.1:8080")).toBe("http://127.0.0.1:8080/mcp");
  });

  it("registers the endpoint as an http server with the git credential", () => {
    const config = JSON.parse(mcpClientConfig("https://walgit.example/mcp"));
    const server = config.mcpServers.walgit;
    expect(server.type).toBe("http");
    expect(server.url).toBe("https://walgit.example/mcp");
    // Braced form: the clients that expand anything expand `${VAR}`; a bare
    // `$NAME` reaches the wire as literal text.
    expect(server.headers.Authorization).toBe("Bearer ${WALGIT_TOKEN}");
  });

  it("handshakes with an initialize message and both required headers", () => {
    const curl = mcpHandshake("https://walgit.example/mcp");
    // rmcp answers 406 without `Accept: application/json, text/event-stream`.
    expect(curl).toContain("Accept: application/json, text/event-stream");
    expect(curl).toContain("Content-Type: application/json");
    const body = curl.slice(curl.indexOf("-d '") + 4, curl.lastIndexOf("'"));
    const message = JSON.parse(body);
    expect(message.method).toBe("initialize");
    expect(message.params.protocolVersion).toBe("2025-06-18");
    // rmcp rejects an initialize without a versioned `clientInfo`.
    expect(message.params.clientInfo.version).toBeTruthy();
  });

  it("subscribes with the session header the handshake returned", () => {
    const curl = mcpSubscribe("https://walgit.example/mcp", "walgit://collab/board/acme/web");
    expect(curl).toContain("mcp-session-id:");
    expect(curl).toContain("Accept: application/json, text/event-stream");
    const body = curl.slice(curl.indexOf("-d '") + 4, curl.lastIndexOf("'"));
    const message = JSON.parse(body);
    expect(message.method).toBe("resources/subscribe");
    expect(message.params.uri).toBe("walgit://collab/board/acme/web");
  });

  it("opens the stream the notifications arrive on", () => {
    // The failure this pins: a subscription that answers 200 and delivers
    // nothing because no GET /mcp is held open for the session.
    const curl = mcpStream("https://walgit.example/mcp");
    expect(curl).toContain("curl -N https://walgit.example/mcp");
    expect(curl).toContain("Accept: text/event-stream");
    expect(curl).toContain("mcp-session-id:");
  });

  it("documents the notification the stream actually carries", () => {
    const frame = mcpNotification("walgit://collab/board/acme/web");
    const data = frame.split("\n").find((l) => l.startsWith("data: "));
    const message = JSON.parse(data!.slice("data: ".length));
    expect(message.method).toBe("notifications/resources/updated");
    // The uri only — the client must re-read to learn what changed.
    expect(message.params).toEqual({ uri: "walgit://collab/board/acme/web" });
  });
});
