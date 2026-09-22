/**
 * Browser-side D1 collaboration identity: a per-origin Ed25519 keypair
 * persisted as a JWK in localStorage. The private key never leaves this
 * browser; the public key is self-registered through the thin API into
 * `refs/collab/meta/principals/<principal>` so the aggregation can verify
 * entries signed here.
 */

const KEY_STORE = "walgit.collab.keypair.v1";

function b64(bytes: ArrayBuffer | Uint8Array): string {
  const u = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  return btoa(Array.from(u, (c) => String.fromCharCode(c)).join(""));
}

/** WebCrypto Ed25519 exists in current Chromium/Safari/Firefox. Feature
    detection must ATTEMPT the operation: `SubtleCrypto` exposes no
    algorithm-named properties, so `"Ed25519" in crypto.subtle` is always
    false and a property check silently disables the whole browser path.
    Older browsers reject generateKey — we surface that as the message. */
export async function ed25519Supported(): Promise<boolean> {
  try {
    if (typeof crypto === "undefined" || typeof crypto.subtle === "undefined") return false;
    await crypto.subtle.generateKey({ name: "Ed25519" }, false, ["sign", "verify"]);
    return true;
  } catch {
    return false;
  }
}

export async function loadKeyPair(): Promise<CryptoKeyPair | null> {
  const jwk = localStorage.getItem(KEY_STORE);
  if (!jwk) return null;
  try {
    const priv = JSON.parse(jwk) as JsonWebKey;
    const privateKey = await crypto.subtle.importKey("jwk", priv, { name: "Ed25519" }, true, ["sign"]);
    const publicKey = await crypto.subtle.importKey(
      "jwk",
      { kty: priv.kty, crv: priv.crv, x: priv.x, ext: true },
      { name: "Ed25519" },
      true,
      ["verify"],
    );
    return { privateKey, publicKey };
  } catch {
    localStorage.removeItem(KEY_STORE);
    return null;
  }
}

export async function ensureKeyPair(): Promise<CryptoKeyPair> {
  const existing = await loadKeyPair();
  if (existing) return existing;
  const pair = await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"]);
  const privJwk = await crypto.subtle.exportKey("jwk", pair.privateKey);
  localStorage.setItem(KEY_STORE, JSON.stringify(privJwk));
  return pair;
}

/** The public key, raw bytes → base64 (what the principals registry stores). */
export async function publicKeyB64(): Promise<string> {
  const pair = await ensureKeyPair();
  const raw = await crypto.subtle.exportKey("raw", pair.publicKey);
  return b64(raw);
}

/** Sign the canonical form of an entry; returns the raw signature as base64
    (the SDK wraps it in `ed25519:`). */
export async function signCanonical(canonical: string): Promise<string> {
  const pair = await ensureKeyPair();
  const sig = await crypto.subtle.sign("Ed25519", pair.privateKey, new TextEncoder().encode(canonical));
  return b64(sig);
}

/** The stored private JWK as a JSON data URL for a user-requested backup, or
    `null` when this browser has no key yet. Exporting is deliberate: the
    registry verifies against its *current* key, so losing or rotating this key
    makes every entry signed with it unverified — the backup is the only way
    back. */
export function keyBackupDataUrl(): string | null {
  const jwk = localStorage.getItem(KEY_STORE);
  return jwk === null ? null : `data:application/json;charset=utf-8,${encodeURIComponent(jwk)}`;
}

export function keyBackupFilename(principal: string): string {
  return `walgit-collab-key-${principal}.json`;
}

/** Download the stored key. Returns `false` when there is nothing to back up. */
export function downloadKeyBackup(principal: string): boolean {
  const href = keyBackupDataUrl();
  if (href === null) return false;
  const a = document.createElement("a");
  a.href = href;
  a.download = keyBackupFilename(principal);
  a.click();
  return true;
}
