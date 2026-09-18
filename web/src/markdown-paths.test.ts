import { describe, expect, it } from "vitest";
import { dirOf, normalizePath, resolveMarkdownTarget, type BlobUrls } from "./markdown-paths";

// Same shapes the SDK produces (`client.repo(full).urls`).
const urls: BlobUrls = {
  raw: (rev, path) => `/gqf2008/ventures/api/blob/${rev}/${path}?raw`,
  tree: (rev, path = "") => `/gqf2008/ventures/tree/${rev}${path ? "/" + path : ""}`,
  blob: (rev, path) => `/gqf2008/ventures/blob/${rev}/${path}`,
};
const base = { ref: "main", dir: "content", urls };

/**
 * The reported bug (`ventures@fae1111f`): a relative link in `content/README.md`
 * was resolved against the *page* URL, so the directory page's missing trailing
 * slash ate the `content/` segment and the server answered
 * `Not a valid object name <sha>:vdev-driver-ip/README.md`.
 */
describe("resolveMarkdownTarget", () => {
  it("keeps the containing directory for a relative link", () => {
    expect(resolveMarkdownTarget("vdev-driver-ip/README.md", base, "link")).toBe(
      "/gqf2008/ventures/blob/main/content/vdev-driver-ip/README.md",
    );
  });

  it("resolves ./ and ../ against the file's directory", () => {
    const nested = { ...base, dir: "content/sub" };
    expect(resolveMarkdownTarget("./x.md", nested, "link")).toBe(
      "/gqf2008/ventures/blob/main/content/sub/x.md",
    );
    expect(resolveMarkdownTarget("../y.md", nested, "link")).toBe(
      "/gqf2008/ventures/blob/main/content/y.md",
    );
  });

  it("sends a directory target to the tree route", () => {
    expect(resolveMarkdownTarget("sub/", base, "link")).toBe(
      "/gqf2008/ventures/tree/main/content/sub",
    );
  });

  it("sends relative images to the raw endpoint", () => {
    expect(resolveMarkdownTarget("pics/a.png", base, "image")).toBe(
      "/gqf2008/ventures/api/blob/main/content/pics/a.png?raw",
    );
  });

  it("keeps the fragment on links", () => {
    expect(resolveMarkdownTarget("b.md#sec", base, "link")).toBe(
      "/gqf2008/ventures/blob/main/content/b.md#sec",
    );
  });

  it("passes through anchors, absolute paths and URLs", () => {
    for (const untouched of ["#top", "/elsewhere", "//host/x", "https://x.example/a", "mailto:a@b.c"]) {
      expect(resolveMarkdownTarget(untouched, base, "link")).toBe(untouched);
    }
  });

  it("never walks above the repository root", () => {
    expect(normalizePath("content", "../../../etc/passwd")).toBe("etc/passwd");
  });

  it("sends a root-resolving directory target to the tree route, not the browser", () => {
    // `[root](../)` from `content/README.md` normalises to the repository root.
    // Returning the raw target would let the *page* URL resolve it and drop the
    // `content` segment — exactly the bug this file exists to fix.
    expect(resolveMarkdownTarget("../", base, "link")).toBe("/gqf2008/ventures/tree/main");
    expect(resolveMarkdownTarget("./", base, "link")).toBe("/gqf2008/ventures/tree/main/content");
  });

  it("joins an image query onto the raw url with &", () => {
    // `urls.raw` already ends in `?raw`; `?raw?size=1` would make the server read
    // a key called `raw?size` and hand back JSON instead of bytes.
    expect(resolveMarkdownTarget("pics/a.png?size=1#frag", base, "image")).toBe(
      "/gqf2008/ventures/api/blob/main/content/pics/a.png?raw&size=1#frag",
    );
  });

  it("dirOf gives the containing directory", () => {
    expect(dirOf("content/vdev-driver-ip/README.md")).toBe("content/vdev-driver-ip");
    expect(dirOf("README.md")).toBe("");
  });
});
