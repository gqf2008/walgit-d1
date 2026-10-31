import { describe, expect, it } from "vitest";
import { mcpEndpoint, mcpResources } from "./components/McpSetup";

/**
 * The MCP box (D64) states two facts — the endpoint and the URIs this project
 * exposes. Both are things a client cannot guess and that a rendering refactor
 * could quietly break; the protocol itself (session header, `Accept`, JSON-RPC)
 * is the spec's, so nothing here pins it.
 */
describe("mcp setup", () => {
  it("appends the one endpoint to the host root", () => {
    expect(mcpEndpoint("https://walgit.example")).toBe("https://walgit.example/mcp");
    expect(mcpEndpoint("https://walgit.example/")).toBe("https://walgit.example/mcp");
    expect(mcpEndpoint("http://127.0.0.1:8080")).toBe("http://127.0.0.1:8080/mcp");
  });

  it("names the resources of the repository in the route", () => {
    expect(mcpResources("acme/web")).toEqual([
      "walgit://refs/acme/web",
      "walgit://wal/acme/web",
      "walgit://collab/board/acme/web",
    ]);
  });
});
