// @vitest-environment node
import { describe, expect, it } from "vitest";

import { ReposClient } from "../sdk/repos";

/**
 * The two URL spaces the SDK must never confuse (D15/D20/D68): a repository's
 * own surface lives under `/{owner}/{repo}/api-browser`, and everything
 * host-level (`me`, `owners`, `store`, `instance`) lives under `/api/v1`.
 *
 * A regression here is silent and wide: the bundled UI's landing page asks for
 * `owners`, so sending it to `/api-browser/v1/owners` answers 404 and the whole
 * front page renders "not found: no route for GET owners".
 */
describe("SDK URL construction", () => {
  const client = new ReposClient({ base: "https://walgit.example" });

  it("sends host-level calls to /api/v1", () => {
    expect(client.url("owners")).toBe("https://walgit.example/api/v1/owners");
    expect(client.url("owners/acme/repos")).toBe("https://walgit.example/api/v1/owners/acme/repos");
    expect(client.url("me")).toBe("https://walgit.example/api/v1/me");
    expect(client.url("store")).toBe("https://walgit.example/api/v1/store");
    expect(client.url("instance")).toBe("https://walgit.example/api/v1/instance");
  });

  it("keeps absolute paths untouched", () => {
    expect(client.url("/services/public/ca.pem")).toBe(
      "https://walgit.example/services/public/ca.pem",
    );
  });

  it("puts a repository's own surface on the api-browser lane", () => {
    expect(client.url("owners")).not.toContain("/api-browser");
    expect(client.repo("acme/monorepo").urls.api).toBe(
      "https://walgit.example/acme/monorepo/api-browser",
    );
  });
});
