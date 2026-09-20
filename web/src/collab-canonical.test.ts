// @vitest-environment node
import { afterEach, describe, expect, it, vi } from "vitest";

import { ReposClient } from "../sdk/repos";

/**
 * Cross-language golden vector for the D1 canonical signature (docs/D1_PROTOCOL.md
 * §5.3): the signed bytes are the entry with `sig` present but empty. The same
 * constants are asserted by the Rust test `golden_tests` in
 * crates/walgit-wal/src/collab.rs and posted through the thin API by
 * `collab_sdk_golden_entry_verifies_end_to_end` in the server tests, so the
 * SDK, the verifier and the aggregation are pinned to one byte string.
 *
 * Regression (issue cc-ai-d1-protocol-followups P0): the SDK used to sign the
 * entry *without* the `sig` key, which differs by `,"sig":""` — every
 * browser-authored entry aggregated as unverified and browser approvals never
 * reached the merge rule.
 */
const SEED = new Uint8Array(32).fill(7);
/** PKCS#8 wrapper for an Ed25519 private key (RFC 8410), as WebCrypto expects. */
const PKCS8_PREFIX = Uint8Array.from([
  0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70,
  0x04, 0x22, 0x04, 0x20,
]);
const GOLDEN_CANONICAL =
  '{"actor":"alice","body":{"title":"golden vector"},"id":"golden","kind":"issue","parent":"","sig":"","ts":1786500000,"version":1}';
const GOLDEN_SIG_B64 =
  "VFROsCUBDR4Sj1eFoMdDI/iRfV0A0jgRSGFGjAB91MVh2oh3IwnohAxj7Mq55x+uvpyrhM2tlq6x3WYuT9f5DQ==";
const GOLDEN_PUB_B64 = "6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=";

function b64(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

function fromB64(s: string): Uint8Array<ArrayBuffer> {
  const binary = atob(s);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) out[i] = binary.charCodeAt(i);
  return out;
}

async function goldenKey(): Promise<CryptoKey> {
  const pkcs8 = new Uint8Array(PKCS8_PREFIX.length + SEED.length);
  pkcs8.set(PKCS8_PREFIX);
  pkcs8.set(SEED, PKCS8_PREFIX.length);
  return crypto.subtle.importKey("pkcs8", pkcs8, { name: "Ed25519" }, false, ["sign"]);
}

afterEach(() => {
  vi.useRealTimers();
});

describe("collab canonical signing (D1 §5.3)", () => {
  it("signs the golden Rust canonical bytes and verifies against them", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(new Date(1786500000 * 1000));
    const key = await goldenKey();
    const client = new ReposClient({ base: "http://example.invalid" });

    let signed: string | undefined;
    const entry = await client.repo("o/r").collab.buildEntry({
      principal: "alice",
      kind: "issue",
      id: "golden",
      actor: "alice",
      parent: "",
      body: { title: "golden vector" },
      sign: async (canonical) => {
        signed = canonical;
        return b64(
          new Uint8Array(
            await crypto.subtle.sign("Ed25519", key, new TextEncoder().encode(canonical)),
          ),
        );
      },
    });

    // The signed bytes include `"sig":""` — exactly the verifier's input.
    expect(signed).toBe(GOLDEN_CANONICAL);
    expect(entry.sig).toBe(`ed25519:${GOLDEN_SIG_B64}`);

    const pub = await crypto.subtle.importKey("raw", fromB64(GOLDEN_PUB_B64), { name: "Ed25519" }, false, [
      "verify",
    ]);
    const ok = await crypto.subtle.verify(
      "Ed25519",
      pub,
      fromB64(GOLDEN_SIG_B64),
      new TextEncoder().encode(GOLDEN_CANONICAL),
    );
    expect(ok).toBe(true);
  });
});
