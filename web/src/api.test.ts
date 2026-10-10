import { beforeEach, describe, expect, it } from "vitest";
import { api, client } from "./api";

describe("api.setAdminToken (#127 follow-up)", () => {
  beforeEach(() => {
    try {
      sessionStorage.clear();
    } catch {
      /* jsdom always has it; keep the test robust regardless */
    }
    api.setAdminToken(null);
  });

  it("carries the token in the bearer lane and persists it in sessionStorage", () => {
    api.setAdminToken("wgt_test");
    expect(client.lane).toBe("bearer");
    expect(sessionStorage.getItem("walgit.admin_token")).toBe("wgt_test");

    api.setAdminToken(null);
    expect(client.lane).toBe("same-origin");
    expect(sessionStorage.getItem("walgit.admin_token")).toBeNull();
  });
});
