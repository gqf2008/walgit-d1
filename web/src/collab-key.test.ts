import { beforeEach, describe, expect, it } from "vitest";
import { keyBackupDataUrl, keyBackupFilename } from "./collab";

const KEY_STORE = "walgit.collab.keypair.v1";

/** The key backup is the only recovery path for a browser identity: the
    registry verifies against its *current* key, so losing or rotating the key
    makes every entry signed with it unverified. These tests pin the two pure
    pieces the download button uses. */
describe("browser key backup", () => {
  beforeEach(() => localStorage.clear());

  it("returns null when this browser has no key", () => {
    expect(keyBackupDataUrl()).toBeNull();
  });

  it("round-trips the stored JWK through a JSON data URL", () => {
    const jwk = JSON.stringify({ kty: "OKP", crv: "Ed25519", d: "seed-hex", x: "pub" });
    localStorage.setItem(KEY_STORE, jwk);
    const href = keyBackupDataUrl();
    expect(href).not.toBeNull();
    expect(href!.startsWith("data:application/json")).toBe(true);
    expect(decodeURIComponent(href!.slice(href!.indexOf(",") + 1))).toBe(jwk);
  });

  it("names the backup file after the principal", () => {
    expect(keyBackupFilename("agent-pi")).toBe("walgit-collab-key-agent-pi.json");
  });
});
