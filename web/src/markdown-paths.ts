/**
 * Markdown links and images inside a blob are **relative to the file they appear
 * in** — that is the whole point of `[vdev-driver-ip/](vdev-driver-ip/README.md)`
 * in `content/README.md`. The browser, however, resolves a plain relative href
 * against the *page* URL, and a directory page's URL (`…/tree/main/content`) has
 * no trailing slash — so `content` was read as a file name and dropped, and the
 * request went to `…/tree/main/vdev-driver-ip/README.md`:
 *
 *     not found: fatal: Not a valid object name <sha>:vdev-driver-ip/README.md
 *
 * Rewriting the target ourselves (against the containing directory) is the only
 * fix that works on both the tree page and the blob page.
 */

/** The three URLs a blob page needs; `client.repo(full).urls` satisfies it. */
export type BlobUrls = {
  raw: (rev: string, path: string) => string;
  tree: (rev: string, path?: string) => string;
  blob: (rev: string, path: string) => string;
};

/** Where the markdown being rendered lives: its ref plus its containing dir. */
export type MarkdownBase = { ref: string; dir: string; urls: BlobUrls };

// Any scheme (`https:`, `mailto:`, `data:` …) is left alone; `//host` and
// `/site/absolute` are already resolvable by the browser.
const SCHEME = /^[a-z][a-z0-9+.-]*:/i;

/**
 * `target` as it should appear in the rendered document. Anchors, absolute
 * paths and URLs pass through untouched; everything else lands on an in-app
 * route (an image reads the raw endpoint, a link goes to blob — or tree, when
 * the target names a directory).
 */
export function resolveMarkdownTarget(
  target: string,
  base: MarkdownBase,
  kind: "link" | "image",
): string {
  const trimmed = target.trim();
  if (!trimmed) return target;
  if (trimmed.startsWith("#") || trimmed.startsWith("/") || trimmed.startsWith("//")) return target;
  if (SCHEME.test(trimmed)) return target;

  const hashAt = trimmed.indexOf("#");
  const fragment = hashAt >= 0 ? trimmed.slice(hashAt) : "";
  const withoutHash = hashAt >= 0 ? trimmed.slice(0, hashAt) : trimmed;
  const queryAt = withoutHash.indexOf("?");
  const query = queryAt >= 0 ? withoutHash.slice(queryAt) : "";
  const bare = queryAt >= 0 ? withoutHash.slice(0, queryAt) : withoutHash;

  const path = normalizePath(base.dir, bare);
  // A directory target — including one that resolves to the repository root
  // (`../`, `./`) — goes to the tree route. Returning `target` unchanged here
  // would hand it back to the browser, which resolves it against the *page* URL
  // and lands somewhere else entirely.
  const isDir = bare.endsWith("/");
  if (kind === "image") {
    // `urls.raw` already carries `?raw`, so a caller query is joined with `&`
    // (`?raw?x=1` would make the server read the key as `raw?x`).
    return appendQuery(base.urls.raw(base.ref, path), query) + fragment;
  }
  const url = isDir ? base.urls.tree(base.ref, path) : base.urls.blob(base.ref, path);
  return appendQuery(url, query) + fragment;
}

/// Join `query` (already `?`-prefixed) onto `url`, using `&` when the url
/// already has a query string.
function appendQuery(url: string, query: string): string {
  if (!query) return url;
  return url.includes("?") ? url + "&" + query.slice(1) : url + query;
}

/** Join `dir` with a relative `target`, folding `.`/`..` (never above the root). */
export function normalizePath(dir: string, target: string): string {
  const out = dir ? dir.split("/").filter(Boolean) : [];
  for (const segment of target.split("/")) {
    if (segment === "" || segment === ".") continue;
    if (segment === "..") out.pop();
    else out.push(segment);
  }
  return out.join("/");
}

/** The directory part of a repo path (`a/b/c.md` → `a/b`). */
export function dirOf(path: string): string {
  return path.split("/").slice(0, -1).join("/");
}
