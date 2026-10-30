import { describe, expect, it } from "vitest";
import { mcpClientConfig, mcpEndpoint, mcpHandshake } from "./components/McpSetup";

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
    expect(server.headers.Authorization).toBe("Bearer $WALGIT_TOKEN");
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
});
