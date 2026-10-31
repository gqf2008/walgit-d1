import { describe, expect, it } from "vitest";
import { mcpEndpoint, mcpResources } from "./components/McpSetup";

/**
 * The MCP box (D64) is one paragraph a reader can copy and hand to an agent, so
 * these pin the two facts it carries — the endpoint's shape and the URIs this
 * project exposes. The protocol itself (session header, methods, notification
 * shape) is the spec's, so nothing here pins it.
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
