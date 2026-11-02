// @vitest-environment jsdom
import { describe, expect, it } from "vitest";

import { wantsToken } from "./components/AuthGate";

/**
 * The gate's door rule: only a credential problem asks for a token. A broken or
 * unreachable API must fall through to the app (which shows its own error), and a
 * 404 from an old server must never put a token form in front of an operator.
 */
describe("AuthGate door rule", () => {
  it("asks for a token on 401 and 403", () => {
    expect(wantsToken(401)).toBe(true);
    expect(wantsToken(403)).toBe(true);
  });

  it("lets every other answer through", () => {
    for (const status of [undefined, 0, 200, 204, 404, 409, 500, 503]) {
      expect(wantsToken(status)).toBe(false);
    }
  });
});
