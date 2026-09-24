import { beforeEach, describe, expect, it } from "vitest";
import { keyBackupDataUrl, keyBackupFilename, publicKeyB64, restoreKeyPair, signCanonical } from "./collab";

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

/** The import/restore path (thread cc-ai-d1-key-import): a backup JSON is
    validated end-to-end before it may replace the stored key, and a failed
    import must leave the current key untouched — an unchecked write would
    destroy the only copy of the old identity. */
describe("browser key restore", () => {
  beforeEach(() => localStorage.clear());

  it("rejects non-JSON and leaves the current key untouched", async () => {
    localStorage.setItem(KEY_STORE, "existing");
    await expect(restoreKeyPair("{nope")).rejects.toMatchObject({ kind: "not-json" });
    expect(localStorage.getItem(KEY_STORE)).toBe("existing");
  });

  it("rejects JSON that is not an object", async () => {
    await Promise.all(
      ["null", "[]", "\"x\""].map((bad) =>
        expect(restoreKeyPair(bad)).rejects.toMatchObject({ kind: "not-object" }),
      ),
    );
  });

  it("names the missing or wrong JWK members", async () => {
    await expect(restoreKeyPair(JSON.stringify({ kty: "OKP", crv: "Ed25519", x: "AAAA" }))).rejects.toMatchObject({
      kind: "structure",
      detail: "d",
    });
    await expect(restoreKeyPair(JSON.stringify({ kty: "RSA", x: "AAAA" }))).rejects.toMatchObject({
      kind: "structure",
      detail: "kty,crv,d",
    });
  });

  it("rejects a structurally valid JWK whose material is not an Ed25519 key", async () => {
    const bad = JSON.stringify({
      kty: "OKP",
      crv: "Ed25519",
      x: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
      d: "AAAA", // 3 bytes, not a 32-byte seed
    });
    await expect(restoreKeyPair(bad)).rejects.toMatchObject({ kind: "invalid" });
  });

  it("rejects a JWK whose private and public halves come from different keys", async () => {
    const a = await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"]);
    const b = await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"]);
    const ja = (await crypto.subtle.exportKey("jwk", a.privateKey)) as JsonWebKey;
    const jb = (await crypto.subtle.exportKey("jwk", b.privateKey)) as JsonWebKey;
    localStorage.setItem(KEY_STORE, "existing");
    // Engines that validate d against x at import answer `invalid`; engines
    // that derive the public half from d hit the explicit `mismatch` check.
    await expect(restoreKeyPair(JSON.stringify({ kty: "OKP", crv: "Ed25519", d: ja.d, x: jb.x }))).rejects.toMatchObject({
      kind: expect.stringMatching(/^(mismatch|invalid)$/),
    });
    expect(localStorage.getItem(KEY_STORE)).toBe("existing");
  });

  it("restores a backup and signs verifiably with it immediately", async () => {
    const pair = await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"]);
    const jwk = (await crypto.subtle.exportKey("jwk", pair.privateKey)) as JsonWebKey;
    const rawPub = await crypto.subtle.exportKey("raw", pair.publicKey);
    const expectedPubB64 = btoa(String.fromCharCode(...new Uint8Array(rawPub)));

    await restoreKeyPair(JSON.stringify(jwk));

    // The restored identity is the original one: same public key, and a
    // signature made with the restored private key verifies against it.
    expect(await publicKeyB64()).toBe(expectedPubB64);
    const sig = await signCanonical("hello");
    const pub = await crypto.subtle.importKey("raw", rawPub, { name: "Ed25519" }, false, ["verify"]);
    const ok = await crypto.subtle.verify(
      "Ed25519",
      pub,
      Uint8Array.from(atob(sig), (c) => c.charCodeAt(0)),
      new TextEncoder().encode("hello"),
    );
    expect(ok).toBe(true);

    // The stored form is the canonical re-export, not the pasted text.
    const stored = JSON.parse(localStorage.getItem(KEY_STORE) as string) as JsonWebKey;
    expect(stored.d).toBe(jwk.d);
    expect(stored.x).toBe(jwk.x);
  });
});
