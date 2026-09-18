/**
 * repos.js — the browser/agent SDK for a walgit host.
 *
 * Dependency-free, script-tag loadable, a faithful map of the repository-prefix
 * API and non-repository `/api/v1` endpoints (`web/API.md`). Two ways in:
 *
 *   <script src="https://git.example.com/repos.js"></script>
 *   <script type="module">
 *     const repo = repos.repo("acme/monorepo");
 *     const { head } = await repo.refs();
 *     const tree = await repo.tree(head.sha, "areas");
 *   </script>
 *
 *   import { createClient } from "https://git.example.com/repos.mjs";
 *   const repos = createClient({ token: () => idToken });
 *
 * Lanes: an explicit `token` selects the bearer lane (`Authorization: Bearer`) and
 * same-origin pages (the bundled UI) rely on the session cookie — both call
 * `/{owner}/{repo}/api/*`; any other origin (a site listed in `server.cors_origins`)
 * uses the browser lane `/{owner}/{repo}/api-browser/*` with `credentials: "include"` —
 * on a 401 the SDK opens `/api-browser/v1/authenticate` in a popup, waits for it, and
 * retries once. The repository prefix comes first on both lanes (one edge rule routes
 * a whole repository to its host); non-repo calls go to `/api/v1/*` resp.
 * `/api-browser/v1/*`. Never proxied: browser → the walgit host.
 *
 * Long answers: endpoints that cannot answer immediately stream the SSE
 * envelope (API.md §2b). The SDK consumes it transparently and surfaces
 * `notice`/`progress`/`task` packets through `onProgress`; callers get the
 * plain JSON either way.
 */

// ---- wire types (API.md §4) ---------------------------------------------------

export interface RefInfo {
  name: string;
  sha: string;
}
/** `refs`: O(1) — only the default branch. Branch/tag lists are paged. */
export interface Refs {
  head: RefInfo | null;
}
export interface RefPage {
  refs: RefInfo[];
  more: boolean;
}
export interface RefListQuery {
  /** Path prefix under the namespace (`refs/heads/<prefix>/`). */
  prefix?: string;
  /** Case-insensitive substring filter on the short name. */
  q?: string;
  /** Name cursor: strictly greater (byte order). */
  after?: string;
  /** Page size (server default 100, max 1000). */
  n?: number;
}
// ---- D1 collaboration lane (docs/D1_COLLAB_DESIGN.md) ------------------------
/** Full-name ref pages: `refs/all` (every ref) and `refs/collab/*` (inbox/meta). */
export interface MergeBaseResult {
  from: string;
  to: string;
  /** The merge base commit, or null when the histories are unrelated. */
  merge_base: string | null;
}
export type DiffFormat = "patch" | "stat" | "name-status";
export interface DiffQuery {
  from: string;
  to: string;
  format?: DiffFormat;
}
export interface DiffNameStatus {
  status: string;
  path: string;
  old_path?: string;
}
export interface DiffStat {
  path: string;
  additions: number;
  deletions: number;
}
export interface DiffResult {
  from: string;
  to: string;
  format: DiffFormat;
  patch?: string;
  stats?: DiffStat[];
  changes?: DiffNameStatus[];
}
export interface BlameLine {
  line: number;
  commit: string;
  author: string;
  author_email: string;
  time: number;
  summary: string;
  text: string;
}
export interface BlameResult {
  sha: string;
  path: string;
  blame: BlameLine[];
}
/** A constructed collab write: the ref, the content, and the shell commands
    (run by a CLI/agent) that deliver it through receive-pack. */
export interface CollabPush {
  ref: string;
  /** Entry JSON to write, or null for a deletion (tombstone). */
  content: string | null;
  commands: string[];
}

/** D1 aggregation read answers (server: `GET /{o}/{r}/api/collab/report` and
    `/threads/{id}`; mirrors `walgit-wal::collab`) — the browser shows exactly
    what the `walgit collab` CLI computes locally. */
export interface CollabEntryRef {
  oid: string;
  principal: string;
  verified: boolean;
  entry: {
    version: number;
    kind: string;
    id: string;
    actor: string;
    ts: number;
    parent: string;
    refs?: { base?: string; head?: string };
    body: Record<string, unknown>;
    sig: string;
  };
}
export interface CollabReportThread {
  id: string;
  /** Root entry's `body.title`, "" when it has none (issue #131 — render
      `title || id` like the board cards). */
  title: string;
  entries: number;
  verified: number;
  last_ts: number;
  kinds: string[];
}
export interface CollabReview {
  actor: string;
  decision: string;
  ts: number;
  oid: string;
}
export interface CollabPr {
  id: string;
  base: string | null;
  head: string | null;
  status: string;
  reviews: CollabReview[];
  human_approvals: CollabReview[];
  unverified: string[];
}
export interface CollabMergeEval {
  allowed: boolean;
  reason: string;
  satisfied_by: string[];
}
export interface CollabReportPr {
  id: string;
  /** Root entry's `body.title`, "" when it has none (issue #131). */
  title: string;
  base: string | null;
  head: string | null;
  status: string;
  approvals: number;
  merge_allowed: boolean;
  merge_reason: string;
}
/** One CI run in the report's CI section (D1-CI §8.3): a pure-CI thread
    projected by the §7 aggregation — not a board card, but visible here. */
export interface CollabReportRun {
  id: string;
  task: string;
  repo_ref: string;
  commit: string;
  /** pending | claimed | stale | done */
  state: string;
  conclusion: string | null;
  runner: string | null;
  claims: number;
  last_ts: number;
}
export interface CollabReport {
  /** Ordered by the projection (newest activity first, id on ties); render
      arrival order — do not re-sort, the CLI renders the same order. */
  threads: CollabReportThread[];
  /** Ordered by the projection: `open` before `merged` before `closed`, then
      newest activity first, id on ties. */
  prs: CollabReportPr[];
  runs: CollabReportRun[];
  total_entries: number;
  verified_entries: number;
  unverified_entries: number;
  missing_principals: number;
  by_actor: [string, number][];
  by_kind: [string, number][];
}
export interface CollabThread {
  id: string;
  entries: CollabEntryRef[];
  pr: { pr: CollabPr; merge: CollabMergeEval } | null;
}
/** One card of the work-unit board (D1 §8): a thread flattened by the
    deterministic `build_board` projection the CLI renders too. */
export interface CollabBoardCard {
  id: string;
  title: string;
  /** The root entry's human-written prose (markdown); "" for machine entries. */
  prose: string;
  actor: string;
  /** Effective work-unit status: the latest `status` entry's value. */
  status: string;
  /** Current owner from the latest status entry that names one. */
  owner: string;
  /** Current worktree from the latest status entry that names one. */
  worktree: string;
  /** Current branch from the latest status entry that names one. */
  branch: string;
  /** Current task summary (`work`, falling back to `note`). */
  work: string;
  created_ts: number;
  last_ts: number;
  entries: number;
  verified: number;
  unverified: number;
  kinds: string[];
  /** Tip of the parent chain — the `parent` a follow-up entry chains to
      (the board's move posts its `status` entry on it). */
  last_oid: string;
  /** Merge-rule verdict when the thread has a patch, else null. */
  merge: { allowed: boolean; reason: string } | null;
}
export interface CollabBoardColumn {
  name: string;
  cards: CollabBoardCard[];
}
export interface CollabBoard {
  columns: CollabBoardColumn[];
}
export interface Resolved {
  ref: string;
  sha: string;
  path: string;
  kind: "branch" | "tag" | "commit";
}
export interface Commit {
  sha: string;
  parents: string[];
  author: string;
  author_email: string;
  author_date: string;
  committer: string;
  commit_date: string;
  subject: string;
  /** Message body WITHOUT the trailer block. */
  body: string;
  /** Git trailers of the message (`Key: value` lines of the last paragraph), in order. */
  trailers: CommitTrailer[];
}
export interface CommitTrailer {
  key: string;
  value: string;
}
export interface TreeEntry {
  name: string;
  type: "blob" | "tree" | "commit";
  mode: string;
  size: number;
  sha: string;
}
export interface Tree {
  ref: string;
  sha: string;
  path: string;
  entries: TreeEntry[];
  commit?: Commit;
  readme?: { name: string; contents: string };
}
export interface Blob {
  ref: string;
  sha: string;
  path: string;
  name: string;
  size: number;
  contents?: string;
  binary?: boolean;
  too_large?: boolean;
}
export interface CommitsQuery {
  /** Full sha (immutable answer) or a ref/revision name (SWR + ETag). Default `HEAD`. */
  ref?: string;
  /** Limit to commits touching this path. */
  path?: string;
  skip?: number;
  n?: number;
}
export interface Commits {
  ref: string;
  sha: string;
  commits: Commit[];
  more: boolean;
}
export interface FileStat {
  path: string;
  additions: number;
  deletions: number;
}
export interface CommitDetail {
  commit: Commit;
  stats: FileStat[];
  patch: string;
}
export interface RepoSummary {
  owner: string;
  name: string;
  full_name: string;
  head: RefInfo | null;
  branches: number;
  tags: number;
  clone_url: string;
  html_url: string;
  api_url: string;
}
export interface Me {
  principal: string;
  write: boolean;
  /** May mutate settings/policy and the host's storage config (#127). */
  admin: boolean;
  anonymous: boolean;
}
/** The redacted storage snapshot of `GET /api/v1/store` (#127, admin). */
export interface StoreSettings {
  backend: "s3" | "gcs" | "memory";
  bucket: string;
  prefix: string;
  endpoint: string;
  region: string;
  force_path_style: boolean;
  /** Presence bits only — the credential values never leave the server. */
  has_access_key: boolean;
  has_secret_key: boolean;
  /** False when the instance does not run from a config file (nothing to save into). */
  can_save: boolean;
}
/** One edit of the storage config (the wizard's store step; #127). Blank
 *  credentials on this surface mean "keep the current value". */
export interface StoreEdit {
  backend: "s3" | "gcs";
  bucket: string;
  endpoint?: string;
  region?: string;
  access_key?: string;
  secret_key?: string;
  force_path_style?: boolean;
}
/** The answer of `POST /api/v1/store/test` (and `/api/v1/setup/test`). */
export interface StoreTestResult {
  ok: boolean;
  backend?: string;
  bucket?: string;
  message: string;
}
export interface StoreSaveResult {
  saved: boolean;
  restart: "supervisor" | "manual";
  /**
   * (issue #129, surfaced by #134) Ways the written file is not
   * self-sufficient — today: credentials supplied only by the environment,
   * which the file cannot capture. The save succeeded; a restart outside
   * that environment may fail to open the store. Empty/absent = the save
   * is durable on its own. The `/api/v1/setup/save` answer carries the
   * same field.
   */
  warnings?: string[];
  file: string;
}
export interface TaskProgress {
  label: string;
  done: number;
  total?: number;
  unit: string;
  percent?: number;
}
export interface TaskRecord {
  id: string;
  kind: string;
  repo: string;
  hostname: string;
  started: string;
  finished?: string;
  elapsed_ms: number;
  ok?: boolean;
  summary: string;
  progress?: TaskProgress;
  log_tail: string[];
  params?: Record<string, string>;
}
export interface Tasks {
  hostname: string;
  running: TaskRecord[];
  recent: TaskRecord[];
}
export interface OpSpec {
  id: string;
  label: string;
  description: string;
  params: string[];
  mutating: boolean;
}
export interface OpRecord {
  id: string;
  /** The task kind (fsck, compact, bundle, checkpoint, materialize, …) — the wire field is `kind`. */
  kind: string;
  repo: string;
  hostname: string;
  started: string;
  finished?: string;
  elapsed_ms: number;
  ok?: boolean;
  summary: string;
  log_tail: string[];
}
export type OpEvent =
  | { event: "started"; record: OpRecord }
  | { event: "log"; line: string }
  | { event: "done"; record: OpRecord; result: unknown }
  | { event: "error"; record: OpRecord; message: string };
/** `overview` is walgit-specific and large; typed loosely here, see API.md §4. */
export type Overview = Record<string, unknown> & {
  repo: string;
  clone_url: string;
  hostname: string;
  health: { status: "ok" | "degraded" | "error"; issues: string[]; deep: string };
  ops: { available: OpSpec[]; recent: OpRecord[]; bundle_strategies: string[] };
};
/** One `/policy` document (docs/POLICY.md). */
export type Policy = Record<string, unknown>;

/** An envelope packet (API.md §2b) or a task packet, surfaced via `onProgress`. */
export type Progress =
  | { kind: "notice"; text: string; url: string }
  | { kind: "progress"; label: string; done: number; total?: number; unit: string; percent?: number; url: string }
  | { kind: "task"; task: TaskRecord; url: string }
  | { kind: "done"; url: string };

/** Raw server-sent event. */
export interface SseEvent {
  event: string;
  data: string;
}

// ---- errors -------------------------------------------------------------------

export class ReposError extends Error {
  constructor(
    public status: number,
    message: string,
    public url = "",
  ) {
    super(message);
    this.name = "ReposError";
  }
  get notFound() {
    return this.status === 404;
  }
  get unauthorized() {
    return this.status === 401;
  }
}

// ---- client -----------------------------------------------------------------------

export type Lane = "auto" | "bearer" | "same-origin" | "browser";

export interface ClientOptions {
  /** API host origin. Default: the origin the script was loaded from (or its `data-base` attribute). */
  base?: string;
  /** Bearer token (or provider) → the bearer lane: a walgit access token (`/_auth/tokens`), a static token, or an ID token. */
  token?: string | (() => string | Promise<string>);
  /** Force a lane; `auto` (default) picks bearer if `token`, same-origin if the page is on `base`, else browser. */
  lane?: Lane;
  /** Open the sign-in popup when the browser lane has no session (default true in browsers). */
  interactive?: boolean;
  /** Global progress hook for streamed answers (per-call `onProgress` also fires). */
  onProgress?: (p: Progress) => void;
  /** Custom fetch (tests, Node). */
  fetch?: typeof fetch;
}

export interface CallOptions {
  signal?: AbortSignal;
  onProgress?: (p: Progress) => void;
  /** Extra headers (e.g. `If-None-Match`). */
  headers?: Record<string, string>;
}

// Ref pages have their own SSE dialect (API.md §4 refs/{kind}); ask for JSON only.
const JSON_ONLY: RequestInit = { headers: { Accept: "application/json" } };
const enc = (s: string) => s.split("/").map(encodeURIComponent).join("/");
const qs = (q: object) => {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== null && v !== "") p.set(k, String(v));
  const s = p.toString();
  return s ? `?${s}` : "";
};

function scriptOrigin(): string | undefined {
  try {
    const s = typeof document !== "undefined" ? (document.currentScript as HTMLScriptElement | null) : null;
    const src = s?.src;
    if (!src) return undefined;
    const base = s?.dataset?.base;
    if (base) return base.replace(/\/+$/, "");
    return new URL(src).origin;
  } catch {
    return undefined;
  }
}

/** Without a script tag or an explicit `base`: the page's own origin. */
export const DEFAULT_BASE = typeof location !== "undefined" ? location.origin : "http://127.0.0.1:8080";

/** One walgit host. */
/** Deterministic key-sorted JSON (D1 §4.2: the signed canonical form). */
function canonicalize(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalize).join(",")}]`;
  if (value !== null && typeof value === "object") {
    const o = value as Record<string, unknown>;
    // Keys are byte-sorted; `undefined` values are dropped (JSON.stringify also
    // drops them when the entry is stored), so the signed form matches the
    // stored content.
    const keys = Object.keys(o)
      .toSorted()
      .filter((k) => o[k] !== undefined);
    return `{${keys.map((k) => `${JSON.stringify(k)}:${canonicalize(o[k])}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

/** Refname-safe segment for collab refs (D1 §5): no `:`, `/`, `..`, `.lock`. */
const REF_SEGMENT = /^[A-Za-z0-9][A-Za-z0-9._@-]*$/;
function refSegment(label: string, s: string): void {
  if (!REF_SEGMENT.test(s) || s === "." || s === ".." || s.includes("..") || s.endsWith(".lock")) {
    throw new ReposError(400, `collab.${label}: ${JSON.stringify(s)} is not a refname-safe segment ([A-Za-z0-9._@-]+)`);
  }
}

/** A refname-safe per-entry segment (D1 §4.1: inbox refs are per-entry, so a
    thread can hold many entries by one principal without overwriting). */
function entrySegment(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

/** Build a signed collab entry object (the bytes the server stores, ready
    for `collab.post`): canonical form signed with `sign(canonical)` returning
    the raw Ed25519 signature as base64. Shared by `collab.entry` (which packs
    it into a receive-pack push) and `collab.buildEntry` (browser thin API). */
async function signedEntry(input: {
  principal: string;
  kind: string;
  id: string;
  actor: string;
  parent: string;
  refs?: { base?: string; head?: string };
  body: Record<string, unknown>;
  sign: (canonical: string) => Promise<string>;
}): Promise<Record<string, unknown>> {
  refSegment("entry.principal", input.principal);
  const entry: Record<string, unknown> = {
    version: 1,
    kind: input.kind,
    id: input.id,
    actor: input.actor,
    ts: Math.floor(Date.now() / 1000),
    parent: input.parent,
  };
  if (input.refs) entry.refs = input.refs;
  entry.body = input.body;
  const sig = `ed25519:${await input.sign(canonicalize(entry))}`;
  return { ...entry, sig };
}

function pushFor(ref: string, content: string | null): CollabPush {
  if (content === null) {
    return { ref, content: null, commands: [`set -e\ngit push origin ":${ref}"`] };
  }
  // A random delimiter so an entry body cannot truncate the heredoc early.
  const delim = `WALGIT_ENTRY_${Math.random().toString(36).slice(2)}`;
  const cmd = `set -e\nsha=$(git hash-object -w --stdin <<'${delim}'\n${content}\n${delim}\n)\ngit push origin "$sha:${ref}"`;
  return { ref, content, commands: [cmd] };
}

export class ReposClient {
  readonly base: string;
  private opts: ClientOptions;
  private authInFlight: Promise<boolean> | null = null;

  constructor(opts: ClientOptions = {}) {
    this.opts = { ...opts };
    this.base = (opts.base ?? scriptOrigin() ?? DEFAULT_BASE).replace(/\/+$/, "");
  }

  /** Change options in place (e.g. supply a token later). Returns `this`. */
  configure(opts: Partial<ClientOptions>): this {
    Object.assign(this.opts, opts);
    return this;
  }

  get lane(): Exclude<Lane, "auto"> {
    const l = this.opts.lane ?? "auto";
    if (l !== "auto") return l;
    if (this.opts.token) return "bearer";
    if (typeof location !== "undefined" && location.origin === this.base) return "same-origin";
    return "browser";
  }

  /**
   * Absolute URL of an API path. Repo-scoped paths are **absolute**: everything of
   * a repository under its own prefix, the lane as the segment after it —
   * `/acme/monorepo/api/refs` (bearer / same-origin session) or
   * `/acme/monorepo/api-browser/refs` (browser lane: another origin). Non-repo
   * paths (`me`, `owners`, `authenticate`) live under `/api/v1` resp.
   * `/api-browser/v1`.
   */
  url(path: string): string {
    if (path.startsWith("/")) return `${this.base}${path}`;
    return `${this.base}${this.lanePrefix}/v1/${path}`;
  }

  /** `/api` or `/api-browser` — the lane segment (D27). */
  get lanePrefix(): "/api" | "/api-browser" {
    return this.lane === "browser" ? "/api-browser" : "/api";
  }

  // ---- auth --------------------------------------------------------------------

  /** Who am I (401 → ReposError). */
  me(opts?: CallOptions) {
    return this.json<Me>("me", opts);
  }

  /**
   * Browser lane: make sure a session exists, opening the sign-in popup if
   * needed. Resolves true when signed in. Safe to call pre-emptively from a
   * click handler (popup blockers like a user gesture).
   */
  signIn(): Promise<boolean> {
    if (this.authInFlight) return this.authInFlight;
    this.authInFlight = this.openAuthPopup().finally(() => {
      this.authInFlight = null;
    });
    return this.authInFlight;
  }

  private async openAuthPopup(): Promise<boolean> {
    if (typeof window === "undefined") return false;
    const url = `${this.base}/api-browser/v1/authenticate`;
    const w = 520;
    const h = 640;
    const left = Math.max(0, (window.screen?.width ?? w) / 2 - w / 2);
    const top = Math.max(0, (window.screen?.height ?? h) / 2 - h / 2);
    const popup = window.open(url, "repos-auth", `popup,width=${w},height=${h},left=${left},top=${top}`);
    if (!popup) return false;
    return await new Promise<boolean>((resolve) => {
      let done = false;
      const finish = (ok: boolean) => {
        if (done) return;
        done = true;
        window.removeEventListener("message", onMsg);
        clearInterval(poll);
        clearTimeout(overall);
        resolve(ok);
      };
      const onMsg = (ev: MessageEvent) => {
        if (ev.origin !== this.base) return;
        const d = ev.data as { type?: string } | null;
        if (d && d.type === "repos:authenticated") finish(true);
      };
      window.addEventListener("message", onMsg);
      // The popup may close without posting (user dismissed it, or the identity
      // provider landed on a page we do not control): probe once, then give up.
      const poll = setInterval(() => {
        if (popup.closed) {
          this.probe().then(finish, () => finish(false));
        }
      }, 300);
      // A user mid-IdP sign-in must not hang the awaiting fetch forever: cap
      // the dance. Probe before ruling failure — the popup may have landed the
      // session already (its success postMessage loses the race to this cap),
      // and a probe is authoritative; the popup stays either way (a later
      // `signIn()` re-attaches to the same named window).
      const overall = setTimeout(() => {
        this.probe().then(finish, () => finish(false));
      }, 60_000);
    });
  }

  private async probe(): Promise<boolean> {
    const r = await this.rawFetch(this.url("me"), { headers: { Accept: "application/json" } });
    return r.ok;
  }

  // ---- owners / repos ----------------------------------------------------------------

  readonly owners = {
    /** Top-level namespaces. */
    list: (opts?: CallOptions) => this.json<string[]>("owners", opts),
    /** Repositories under one owner (short names). */
    repos: (owner: string, opts?: CallOptions) => this.json<string[]>(`owners/${enc(owner)}/repos`, opts),
  };

  /**
   * The instance's own object storage (#127, D43's configured-state twin):
   * admin-only; `GET` is redacted (credential presence bits only), a blank
   * credential on `test`/`save` keeps the current value, and `save` arms the
   * same exit-75 supervisor restart as the first-run wizard.
   */
  readonly store = {
    get: (opts?: CallOptions) => this.json<StoreSettings>("store", opts),
    test: (edit: StoreEdit, opts?: CallOptions) =>
      this.json<StoreTestResult>("store/test", opts, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(edit),
      }),
    save: (edit: StoreEdit, opts?: CallOptions) =>
      this.json<StoreSaveResult>("store", opts, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ store: edit }),
      }),
  };

  /** A handle on `owner/name` (no request is made). */
  repo(fullName: string): RepoClient;
  repo(owner: string, name: string): RepoClient;
  repo(a: string, b?: string): RepoClient {
    const full = b === undefined ? a.replace(/^\/+|\/+$/g, "").replace(/\.git$/, "") : `${a}/${b}`;
    const i = full.indexOf("/");
    if (i <= 0 || i === full.length - 1) throw new ReposError(400, `repos.repo(): expected "owner/name", got ${JSON.stringify(full)}`);
    return new RepoClient(this, full);
  }

  // ---- transport ------------------------------------------------------------------

  /** GET JSON (envelope-aware). */
  async json<T>(path: string, opts: CallOptions = {}, init: RequestInit = {}): Promise<T> {
    const url = this.url(path);
    const r = await this.fetch(url, {
      ...init,
      headers: { Accept: "application/json, text/event-stream", ...(init.headers as Record<string, string>), ...opts.headers },
      signal: opts.signal,
    });
    if (r.status === 204) return undefined as T;
    if (!r.ok) throw new ReposError(r.status, (await r.text()).trim() || r.statusText, url);
    const ct = r.headers.get("content-type") ?? "";
    if (!ct.startsWith("text/event-stream")) {
      return ct.startsWith("application/json") ? ((await r.json()) as T) : ((await r.text()) as unknown as T);
    }
    return this.readEnvelope<T>(r, url, opts);
  }

  /** Text body (e.g. `blob?raw`). */
  async text(path: string, opts: CallOptions = {}): Promise<string> {
    const url = this.url(path);
    const r = await this.fetch(url, { headers: { Accept: "text/plain, */*", ...opts.headers }, signal: opts.signal });
    if (!r.ok) throw new ReposError(r.status, (await r.text()).trim() || r.statusText, url);
    return r.text();
  }

  /**
   * Authenticated fetch on the lane in use: bearer header, or cookies
   * (same-origin / include). A 401 (or an `opaqueredirect` to sign-in) on the browser
   * lane triggers the popup once and retries.
   */
  async fetch(url: string, init: RequestInit = {}): Promise<Response> {
    let r = await this.rawFetch(url, init);
    const needsAuth = r.status === 401 || r.type === "opaqueredirect";
    if (needsAuth && this.lane === "browser" && (this.opts.interactive ?? typeof window !== "undefined")) {
      if (await this.signIn()) r = await this.rawFetch(url, init);
    }
    if (r.type === "opaqueredirect") throw new ReposError(401, "not signed in (the browser lane redirected to sign-in)", url);
    return r;
  }

  private async rawFetch(url: string, init: RequestInit): Promise<Response> {
    const f = this.opts.fetch ?? globalThis.fetch.bind(globalThis);
    const headers: Record<string, string> = { ...(init.headers as Record<string, string>) };
    const lane = this.lane;
    let credentials: RequestCredentials = "same-origin";
    let redirect: RequestRedirect | undefined;
    if (lane === "bearer") {
      const t = this.opts.token;
      const tok = typeof t === "function" ? await t() : t;
      if (tok) headers.Authorization = `Bearer ${tok}`;
      credentials = "omit";
    } else if (lane === "browser") {
      credentials = "include";
      // A proxy in front may answer an unauthenticated XHR with a 302 to a sign-in page;
      // `manual` turns that into an `opaqueredirect` we can act on.
      redirect = "manual";
    }
    return f(url, { ...init, headers, credentials, redirect: redirect ?? init.redirect, mode: "cors" });
  }

  /** Read an SSE stream (GET/POST) and dispatch raw events. */
  async sse(path: string, init: RequestInit, onEvent: (ev: SseEvent) => void, opts: CallOptions = {}): Promise<void> {
    const url = this.url(path);
    const r = await this.fetch(url, {
      ...init,
      headers: { ...(init.headers as Record<string, string>), Accept: "text/event-stream", ...opts.headers },
      signal: opts.signal,
    });
    if (!r.ok) throw new ReposError(r.status, (await r.text()).trim() || r.statusText, url);
    await readSse(r, onEvent);
  }

  private async readEnvelope<T>(r: Response, url: string, opts: CallOptions): Promise<T> {
    const emit = (p: Progress) => {
      opts.onProgress?.(p);
      this.opts.onProgress?.(p);
    };
    let result: { value: T } | undefined;
    let error: ReposError | undefined;
    try {
      await readSse(r, (ev) => {
        const data: unknown = ev.data ? JSON.parse(ev.data) : null;
        const o = typeof data === "object" && data !== null ? (data as Record<string, unknown>) : {};
        switch (ev.event) {
          case "notice":
            emit({ kind: "notice", text: String(o.text ?? ""), url });
            break;
          case "progress":
            emit({
              kind: "progress",
              label: String(o.label ?? ""),
              done: Number(o.done ?? 0),
              total: typeof o.total === "number" ? o.total : undefined,
              unit: String(o.unit ?? ""),
              percent: typeof o.percent === "number" ? o.percent : undefined,
              url,
            });
            break;
          case "task":
            emit({ kind: "task", task: data as TaskRecord, url });
            break;
          case "result":
            result = { value: data as T };
            break;
          case "error":
            error = new ReposError(typeof o.status === "number" ? o.status : 500, String(o.message ?? "error"), url);
            break;
          default:
            break;
        }
      });
    } finally {
      emit({ kind: "done", url });
    }
    if (error) throw error;
    if (!result) throw new ReposError(502, "stream ended without a result", url);
    return result.value;
  }
}

/** Parse a `text/event-stream` body, dispatching each event as it completes. */
export async function readSse(r: Response, onEvent: (ev: SseEvent) => void): Promise<void> {
  if (!r.body) return;
  const reader = r.body.getReader();
  const dec = new TextDecoder();
  let buf = "";
  const dispatch = (chunk: string) => {
    let event = "message";
    const data: string[] = [];
    for (const line of chunk.split("\n")) {
      if (line.startsWith("event:")) event = line.slice(6).trim();
      else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
      // `id:`, `retry:` and comments (`:`) are ignored.
    }
    if (data.length) onEvent({ event, data: data.join("\n") });
  };
  // oxlint-disable-next-line no-await-in-loop -- streaming read is inherently sequential
  for (let chunk = await reader.read(); !chunk.done; chunk = await reader.read()) {
    buf += dec.decode(chunk.value, { stream: true });
    let i: number;
    while ((i = buf.indexOf("\n\n")) >= 0) {
      dispatch(buf.slice(0, i));
      buf = buf.slice(i + 2);
    }
  }
  buf += dec.decode();
  if (buf.trim()) dispatch(buf);
}

// ---- one repository ------------------------------------------------------------------

export class RepoClient {
  readonly owner: string;
  readonly name: string;
  /** `/owner/repo/api` or `/owner/repo/api-browser` (lane = credential handling, D27). */
  private get p(): string {
    return `/${enc(this.fullName)}${this.client.lanePrefix}`;
  }

  constructor(
    readonly client: ReposClient,
    readonly fullName: string,
  ) {
    const i = fullName.indexOf("/");
    this.owner = fullName.slice(0, i);
    this.name = fullName.slice(i + 1);
  }

  /** Useful URLs (no request). */
  get urls() {
    const b = this.client.base;
    return {
      html: `${b}/${enc(this.fullName)}`,
      clone: `${b}/${enc(this.fullName)}.git`,
      api: this.client.url(this.p),
      /** Browser-navigable raw blob URL (same-origin or signed-in browser lane). */
      raw: (rev: string, path: string) => this.client.url(`${this.p}/blob/${enc(rev)}/${enc(path)}?raw`),
      /** Deep links into the UI. */
      tree: (rev: string, path = "") => `${b}/${enc(this.fullName)}/tree/${enc(rev)}${path ? "/" + enc(path) : ""}`,
      blob: (rev: string, path: string) => `${b}/${enc(this.fullName)}/blob/${enc(rev)}/${enc(path)}`,
      commit: (sha: string) => `${b}/${enc(this.fullName)}/commit/${sha}`,
    };
  }

  /** Summary: head, ref counts, URLs (SWR + ETag). */
  get(opts?: CallOptions) {
    return this.client.json<RepoSummary>(this.p, opts);
  }
  /** Create the repository (write permission). */
  async create(opts?: CallOptions): Promise<void> {
    await this.client.json<unknown>(this.p, opts, { method: "PUT" });
  }
  /** Delete the repository (admin permission). Irreversible. */
  async delete(opts?: CallOptions): Promise<void> {
    await this.client.json<unknown>(this.p, opts, { method: "DELETE" });
  }

  /** Default branch only — O(1) on any ref count. */
  refs(opts?: CallOptions) {
    return this.client.json<Refs>(`${this.p}/refs`, opts);
  }
  /** One name-sorted page of branches. */
  branches(q: RefListQuery = {}, opts?: CallOptions) {
    return this.client.json<RefPage>(`${this.p}/refs/branches${qs(q)}`, opts, JSON_ONLY);
  }
  /** One name-sorted page of tags (sha = peeled commit). */
  tags(q: RefListQuery = {}, opts?: CallOptions) {
    return this.client.json<RefPage>(`${this.p}/refs/tags${qs(q)}`, opts, JSON_ONLY);
  }
  /** Every ref as full names (`refs/…`), byte-sorted, paged (D1). */
  refsAll(q: RefListQuery = {}, opts?: CallOptions) {
    return this.client.json<RefPage>(`${this.p}/refs/all${qs(q)}`, opts, JSON_ONLY);
  }
  /** The `refs/collab/*` namespace (inbox/meta refs), full names, paged (D1). */
  refsCollab(q: RefListQuery = {}, opts?: CallOptions) {
    return this.client.json<RefPage>(`${this.p}/refs/collab${qs(q)}`, opts, JSON_ONLY);
  }
  /** Exact full-ref lookup (a collab inbox tip, a notes ref, …); 404 → ReposError. */
  refByName(name: string, opts?: CallOptions) {
    return this.client.json<RefInfo>(`${this.p}/refs/name/${enc(name)}`, opts);
  }
  /** Merge base of two revisions — the 3-dot diff base (D1 review primitive). */
  mergeBase(from: string, to: string, opts?: CallOptions) {
    return this.client.json<MergeBaseResult>(`${this.p}/merge-base?from=${enc(from)}&to=${enc(to)}`, opts);
  }
  /** Tree diff between two revisions (D1 review primitive). */
  diff(q: DiffQuery, opts?: CallOptions) {
    return this.client.json<DiffResult>(
      `${this.p}/diff?from=${enc(q.from)}&to=${enc(q.to)}${q.format ? `&format=${q.format}` : ""}`,
      opts,
    );
  }
  /** Line attribution for one file (D1 review primitive). */
  blame(rev: string, path: string, opts?: CallOptions) {
    return this.client.json<BlameResult>(`${this.p}/blame/${enc(rev)}/${enc(path)}`, opts);
  }
  /** Download a tree archive as bytes (D1 review primitive). */
  async archive(rev: string, format: "tar.gz" | "zip" = "tar.gz", opts?: CallOptions): Promise<ArrayBuffer> {
    const url = this.client.url(`${this.p}/archive/${enc(rev)}${format === "zip" ? "?format=zip" : ""}`);
    const r = await this.client.fetch(url, {
      headers: { Accept: "application/gzip, application/zip", ...opts?.headers },
      signal: opts?.signal,
    });
    if (!r.ok) throw new ReposError(r.status, (await r.text()).trim() || r.statusText, url);
    return r.arrayBuffer();
  }

  /** D1 collaboration lane: build signed entries and the git push commands that
      deliver them through receive-pack. The SDK cannot run git; a CLI or agent
      executes `commands` (docs/D1_COLLAB_DESIGN.md §4/§7). */
  readonly collab = {
    /**
     * Build a signed entry for `refs/collab/inbox/<principal>/<uuid>`.
     * `sign(canonical)` must return the Ed25519 signature over the canonical
     * JSON (WebCrypto: `crypto.subtle.sign("Ed25519", key, TextEncoder().encode(canonical))`,
     * then base64).
     */
    entry: async (input: {
      principal: string;
      kind: "issue" | "comment" | "patch" | "review" | "status" | "merge_result" | "agent_action";
      id: string;
      actor: string;
      parent: string;
      refs?: { base?: string; head?: string };
      body: Record<string, unknown>;
      sign: (canonical: string) => Promise<string>;
    }): Promise<CollabPush> => {
      const entry = await signedEntry(input);
      // Per-entry ref: a thread with many entries by one principal must not
      // overwrite a single inbox ref (D1 §4.1).
      return pushFor(`refs/collab/inbox/${input.principal}/${entrySegment()}`, JSON.stringify(entry, null, 2));
    },
    /**
     * Build a signed entry object ready for `collab.post` (the browser thin
     * API). Same signing contract as `entry()`.
     */
    buildEntry: (input: {
      principal: string;
      kind: "issue" | "comment" | "patch" | "review" | "status" | "merge_result" | "agent_action";
      id: string;
      actor: string;
      parent: string;
      refs?: { base?: string; head?: string };
      body: Record<string, unknown>;
      sign: (canonical: string) => Promise<string>;
    }): Promise<Record<string, unknown>> => signedEntry(input),
    /**
     * Post a signed entry through the thin-API write path
     * (`POST /{o}/{r}/api/collab/entries`, D1 §11): the server materializes
     * the entry as a bucket pack and publishes the inbox ref — the browser
     * write path (no git needed). The actor must be the authenticated
     * principal.
     */
    post: async (entry: Record<string, unknown>, opts?: CallOptions): Promise<{ ref: string; oid: string; seq: number }> => {
      return this.client.json<{ ref: string; oid: string; seq: number }>(`${this.p}/collab/entries`, opts, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ entry }),
      });
    },

    /** The full observability report (D1 §8): thread summaries, PR status +
        merge rule evaluation, verification health. */
    report: (opts?: CallOptions) => this.client.json<CollabReport>(`${this.p}/collab/report`, opts),
    /** One thread: parent-ordered entries with per-entry verification, plus
        the PR view + merge evaluation when the thread has a patch. */
    thread: (id: string, opts?: CallOptions) => this.client.json<CollabThread>(`${this.p}/collab/threads/${enc(id)}`, opts),
    /** The work-unit board (D1 §8): the threads projected under the board
        definition versioned at `.walgit/board.toml` (HEAD) — the same
        `build_board` the `walgit collab board` CLI renders offline. Read-only;
        moving a card posts an ordinary signed `status` entry (`buildEntry` +
        `post`). */
    board: (opts?: CallOptions) => this.client.json<CollabBoard>(`${this.p}/collab/board`, opts),
    /**
     * First-use self-registration of the authenticated principal's Ed25519
     * public key through the thin API (`POST /{o}/{r}/api/collab/principal`):
     * the token binds the principal, this ref binds the key. The browser
     * write path (no git needed).
     */
    registerPrincipal: async (input: { principal: string; publicKey: string }, opts?: CallOptions): Promise<{ ref: string; oid: string; seq: number }> => {
      refSegment("registerPrincipal", input.principal);
      return this.client.json<{ ref: string; oid: string; seq: number }>(`${this.p}/collab/principal`, opts, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ principal: input.principal, public_key: input.publicKey }),
      });
    },
    /** First-use registration of a principal's Ed25519 public key at
        `refs/collab/meta/principals/<principal>` (D1 §5: the token binds the
        principal; this ref binds the key). Content is stored as-is. */
    principal: (input: { principal: string; publicKey: string }): CollabPush => {
      refSegment("principal", input.principal);
      const content = JSON.stringify(
        { version: 1, principal: input.principal, public_key: input.publicKey, registered_at: Math.floor(Date.now() / 1000) },
        null,
        2,
      );
      return pushFor(`refs/collab/meta/principals/${input.principal}`, content);
    },
    /** Revoke a principal's key: delete the registry ref (tombstone, D1 §10). */
    revokePrincipal: (principal: string): CollabPush => {
      refSegment("revokePrincipal", principal);
      return pushFor(`refs/collab/meta/principals/${principal}`, null);
    },
  };

  /** D1 CI lane (docs/D1_CI_PROTOCOL.md): the read side of a run's stored
      bytes. The runs themselves are read through `collab.report`/the CLI;
      this is how a browser fetches what a result entry addresses. */
  readonly ci = {
    /**
     * Download one CI log/artifact object by its content address (§8.2
     * storage convention, issue #161): the bytes the runner pushed to
     * `refs/collab/ci-artifacts/<actor>/<sha256>`. The server verifies the
     * payload against the address before serving (a mismatch is a 404), caps
     * at 16 MiB, immutable + ETag. Mirrors `walgit ci log|artifacts`.
     */
    artifact: async (sha256: string, opts?: CallOptions): Promise<ArrayBuffer> => {
      if (!/^[0-9a-f]{64}$/.test(sha256)) {
        throw new ReposError(400, `ci.artifact: ${JSON.stringify(sha256)} is not a 64-char lowercase hex sha256`);
      }
      const url = this.client.url(`${this.p}/collab/ci-artifacts/${enc(sha256)}`);
      const r = await this.client.fetch(url, {
        headers: { Accept: "application/octet-stream", ...opts?.headers },
        signal: opts?.signal,
      });
      if (!r.ok) throw new ReposError(r.status, (await r.text()).trim() || r.statusText, url);
      return r.arrayBuffer();
    },
  };

  /** Streaming ref page: `onRef` per match as the server finds it; resolves `{more}`. */
  async refStream(kind: "branches" | "tags" | "all" | "collab", q: RefListQuery, onRef: (r: RefInfo) => void, opts?: CallOptions): Promise<{ more: boolean }> {
    let more = false;
    await this.client.sse(
      `${this.p}/refs/${kind}${qs(q)}`,
      {},
      (ev) => {
        const d: unknown = ev.data ? JSON.parse(ev.data) : null;
        if (typeof d !== "object" || d === null) return;
        if (ev.event === "ref" && "name" in d && "sha" in d) onRef(d as RefInfo);
        else if (ev.event === "done") more = Boolean((d as { more?: boolean }).more);
      },
      opts,
    );
    return { more };
  }

  /** Split `"{ref}/{path}"` server-side and dereference (API.md §3). Empty → default branch. */
  resolve(rest = "", opts?: CallOptions) {
    return this.client.json<Resolved>(`${this.p}/resolve${rest ? "/" + enc(rest) : ""}`, opts);
  }
  /** Directory listing (+ latest commit touching it, + README). `rev` sha → immutable. */
  tree(rev: string, path = "", opts?: CallOptions) {
    return this.client.json<Tree>(`${this.p}/tree/${enc(rev)}${path ? "/" + enc(path) : ""}`, opts);
  }
  /** File contents (text ≤ 2 MiB), or `binary`/`too_large` with size. */
  blob(rev: string, path: string, opts?: CallOptions) {
    return this.client.json<Blob>(`${this.p}/blob/${enc(rev)}/${enc(path)}`, opts);
  }
  /**
   * Raw **text** of a blob (`?raw`), decoded as UTF-8.
   *
   * The endpoint itself is a byte channel (real `Content-Type`, `Range`, up to
   * 32 MiB), so binary callers should fetch it themselves rather than go through
   * this string helper:
   * `await (await fetch(repo.urls.raw(rev, path))).arrayBuffer()`.
   */
  raw(rev: string, path: string, opts?: CallOptions) {
    return this.client.text(`${this.p}/blob/${enc(rev)}/${enc(path)}?raw`, opts);
  }

  /** The browser-navigable URL of the byte channel (for `<img>`, `<video>`, …). */
  rawUrl(rev: string, path: string) {
    return this.urls.raw(rev, path);
  }
  /** Linear history page. */
  commits(q: CommitsQuery = {}, opts?: CallOptions) {
    return this.client.json<Commits>(`${this.p}/commits${qs(q)}`, opts);
  }
  /** One commit with numstat + unified patch against the first parent. */
  commit(sha: string, opts?: CallOptions) {
    return this.client.json<CommitDetail>(`${this.p}/commit/${enc(sha)}`, opts);
  }
  /** WAL overview (walgit-specific). */
  overview(opts?: CallOptions) {
    return this.client.json<Overview>(`${this.p}/overview`, opts);
  }
  /** What is happening to this repo on the instance that answers. Never cached. */
  tasks(opts?: CallOptions) {
    return this.client.json<Tasks>(`${this.p}/tasks`, opts, { cache: "no-store" });
  }
  /** One task record (plain), or attach to its stream when `onEvent` is given. */
  async task(id: string, onEvent?: (ev: SseEvent) => void, opts?: CallOptions): Promise<TaskRecord | undefined> {
    if (!onEvent) return this.client.json<TaskRecord>(`${this.p}/tasks/${enc(id)}`, opts);
    await this.client.sse(`${this.p}/tasks/${enc(id)}`, {}, onEvent, opts);
    return undefined;
  }

  readonly ops = {
    /** Available maintenance operations + recent runs. */
    list: (opts?: CallOptions) => this.client.json<{ available: OpSpec[]; recent: OpRecord[] }>(`${this.p}/ops`, opts),
    /** Start (or attach to) a maintenance op and stream its events until done/error. */
    run: async (op: string, params: Record<string, string> = {}, onEvent: (ev: OpEvent) => void, opts?: CallOptions): Promise<void> => {
      await this.client.sse(`${this.p}/ops/${enc(op)}${qs(params)}`, { method: "POST" }, (ev) => onEvent(JSON.parse(ev.data) as OpEvent), opts);
    },
  };

  readonly policy = {
    /** The push policy document (docs/POLICY.md); missing = `{}`-equivalent allow-all. */
    get: (opts?: CallOptions) => this.client.json<Policy>(`${this.p}/policy`, opts),
    put: async (policy: Policy, opts?: CallOptions): Promise<void> => {
      await this.client.json<unknown>(`${this.p}/policy`, opts, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(policy),
      });
    },
    delete: async (opts?: CallOptions): Promise<void> => {
      await this.client.json<unknown>(`${this.p}/policy`, opts, { method: "DELETE" });
    },
    /** Parse + validate a policy document without saving it. */
    validate: (policy: unknown, opts?: CallOptions) =>
      this.client.json<PolicyValidation>(`${this.p}/policy/validate`, opts, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: typeof policy === "string" ? policy : JSON.stringify(policy),
      }),
    /** Evaluate a policy (or the saved one when `policy` is empty) against the last N pushes. */
    dryRun: (policy: unknown, last = 20, opts?: CallOptions) =>
      this.client.json<PolicyDryRun>(`${this.p}/policy/dry-run?last=${last}`, opts, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: policy === null || policy === undefined || policy === "" ? "" : typeof policy === "string" ? policy : JSON.stringify(policy),
      }),
  };

  /** D24: WAL-backed TOML overrides of [bundles], [maintenance], [compaction], [upstream], and [integrations]. */
  readonly settings = {
    /** The settings document (`revision: 0` = none). */
    get: (opts?: CallOptions) => this.client.json<RepoSettings>(`${this.p}/settings`, opts),
    /** Publish a new document (validated server-side; 400 with the reason on failure). */
    put: (toml: string, message = "", opts?: CallOptions) =>
      this.client.json<{ revision: number }>(`${this.p}/settings${message ? `?message=${enc(message)}` : ""}`, opts, {
        method: "PUT",
        headers: { "Content-Type": "application/toml" },
        body: toml,
      }),
    /** Back to the host config. */
    delete: (opts?: CallOptions) => this.client.json<{ revision: number }>(`${this.p}/settings`, opts, { method: "DELETE" }),
    /** Effective config (host ⊕ settings) as TOML text. */
    effective: (opts?: CallOptions) => this.client.text(`${this.p}/settings/effective`, opts),
    /** SETTINGS entries in the live log, oldest first. */
    history: (opts?: CallOptions) => this.client.json<SettingsHistory>(`${this.p}/settings/history`, opts),
    /** Everything the Settings tab shows: strategies with next fire, placement, fields with sources. */
    describe: (opts?: CallOptions) => this.client.json<SettingsDescribe>(`${this.p}/settings/describe`, opts),
    /** Validate a document and preview the resulting effective config, without publishing. */
    validate: (toml: string, opts?: CallOptions) =>
      this.client.json<SettingsValidation>(`${this.p}/settings/validate`, opts, {
        method: "POST",
        headers: { "Content-Type": "application/toml" },
        body: toml,
      }),
  };
}

export interface RepoSettings {
  revision: number;
  toml: string;
  author: string;
  updated_at: string | null;
  message: string;
}
export interface SettingsHistory {
  min_seq: number;
  entries: { seq: number; revision: number; author: string; message: string; at: string | null; toml: string }[];
}
export interface StrategyInfo {
  name: string;
  kind: "full" | "incremental";
  base: string | null;
  schedule: string;
  schedule_human: string;
  next: string | null;
  keep: number;
  backfill_max: number;
  min_commits: number;
  refs: string[];
  /** Incrementals: cut on this strategy's previous bundle (chained) instead of the base's newest. */
  chain: boolean;
  filter: string | null;
}
export interface SettingsField {
  key: string;
  value: unknown;
  host_value: unknown;
  source: "host" | "setting";
}
export interface SettingsDescribe {
  repo: string;
  settings: RepoSettings | { revision: 0; toml: "" };
  sections: string[];
  strategies: StrategyInfo[];
  bundles: { enabled: boolean; min_commits: number; main_only: boolean };
  maintenance: {
    checkpoints: boolean;
    interval_secs: number;
    this_host: { name: string; serves: boolean; maintains: boolean; disk: string; max_pack_bytes: number; cache_budget_bytes: number; roles: string[] };
  };
  compaction: { enabled: boolean; trigger_packs: number; trigger_bytes: number };
  /** D33: what this repository follows (`[upstream] follow`) and the last round on the answering instance. */
  upstream: {
    git: string | null;
    lfs: string | null;
    token_env: boolean;
    follow: string[];
    follow_interval_secs: number;
    last_round: FollowStatus | null;
  };
  fields: SettingsField[];
  head_seq: number;
}
export interface FollowStatus {
  at: string;
  outcome: "in-sync" | "published" | "refused" | "failed";
  detail: string;
  /** ref → oid upstream had this round. */
  upstream: Record<string, string>;
  /** ref → oid the WAL had before the round. */
  ours: Record<string, string>;
}
export type SettingsValidation = ({ ok: true; errors: [] } & SettingsDescribe) | { ok: false; errors: string[] };
export interface PolicyValidation {
  ok: boolean;
  errors: string[];
  rules?: number;
  groups?: number;
  protect?: boolean;
}
export interface PolicyDryRun {
  pushes: number;
  allowed: number;
  denied: number;
  results: { seq: number; at: string | null; principal: string; atomic: boolean; refs: { name: string; ok: boolean; reason: string | null; force: boolean }[] }[];
}

// ---- module surface ----------------------------------------------------------------

/** Build a client. */
export function createClient(opts: ClientOptions = {}): ReposClient {
  return new ReposClient(opts);
}

export const version = 1;

/** The default client, bound to the host the script came from (or the page's origin). */
export const repos: ReposClient & { createClient: typeof createClient; ReposError: typeof ReposError; version: number } = Object.assign(
  createClient(),
  { createClient, ReposError, version },
);

export default repos;

// Script-tag registration: `window.repos`.
declare global {
  interface Window {
    repos?: typeof repos;
  }
}
if (typeof window !== "undefined" && !window.repos) {
  window.repos = repos;
}
