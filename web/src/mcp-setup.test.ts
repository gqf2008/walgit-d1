import { describe, expect, it } from "vitest";
import { mcpEndpoint, mcpStream, mcpSubscribe } from "./components/McpSetup";

/**
 * The MCP box (D64) shows three copy-pasteable things — the endpoint, a
 * subscription, and the stream notifications arrive on — so these assertions
 * pin the parts a client cannot guess and that a rendering refactor could
 * quietly break: the endpoint's shape, the session header every later request
 * carries, and the fact that a subscription is useless without a held GET.
 */
describe("mcp setup snippets", () => {
  it("appends the one endpoint to the host root", () => {
    expect(mcpEndpoint("https://walgit.example")).toBe("https://walgit.example/mcp");
    expect(mcpEndpoint("https://walgit.example/")).toBe("https://walgit.example/mcp");
    expect(mcpEndpoint("http://127.0.0.1:8080")).toBe("http://127.0.0.1:8080/mcp");
  });

  it("subscribes to a resource with the session header", () => {
    const curl = mcpSubscribe("https://walgit.example/mcp", "walgit://refs/acme/web");
    expect(curl).toContain("mcp-session-id:");
    // rmcp answers 406 unless the client accepts both, and 415 without the
    // content type — a snippet missing either cannot work at all.
    expect(curl).toContain("Accept: application/json, text/event-stream");
    expect(curl).toContain("Content-Type: application/json");
    const body = curl.slice(curl.indexOf("-d '") + 4, curl.lastIndexOf("'"));
    const message = JSON.parse(body);
    expect(message.method).toBe("resources/subscribe");
    expect(message.params.uri).toBe("walgit://refs/acme/web");
  });

  it("opens the stream the notifications arrive on", () => {
    // The failure this pins: a subscription that answers 200 and delivers
    // nothing because no GET /mcp is held open for the session.
    const curl = mcpStream("https://walgit.example/mcp");
    expect(curl).toContain("curl -N https://walgit.example/mcp");
    expect(curl).toContain("Accept: text/event-stream");
    expect(curl).toContain("mcp-session-id:");
    expect(curl).not.toContain("Content-Type");
  });
});
