# The API: repository-prefix lanes, one SDK

Context: **the wire contract of the JSON API + SDK** (D15/D20/D26/D27), normative, for UI/SDK/site authors and for
anyone changing `crates/walgit-server/src/web/*`. Caching rules (§2a), SSE envelope (§2b), tasks (§2c) apply to
every endpoint.

The programmatic surface of walgit (D20, the *Bring Your Own Tool*
shape): everything the bundled UI, another site, an agent or a script needs.
A Git host that implements this surface (JSON over HTTP plus the hosting rule
of §1) can serve the built UI unchanged; nothing in `web/src` depends on
walgit internals except the optional `overview`/`ops`/`tasks` endpoints.

```
https://git.example.com/{owner}/{repo}/api/…           bearer lane   a bearer token or the same-origin session cookie
https://git.example.com/{owner}/{repo}/api-browser/…   browser lane  credentials: "include" (other origins; the cross-origin cookie lane)
https://git.example.com/api/v1                         non-repo      discovery, me, authenticate, owners (+ /api-browser/v1/me|authenticate for the browser lane)
https://git.example.com/repos.js                       the SDK       window.repos — permanent URL on every host
```
Lanes differ by **credential handling** and CORS (D27): the bearer is a walgit access token (`/_auth/tokens`), a
static token, or an ID token from the OIDC issuer (`GET /services/setup.json` returns the setup commands). Browser
calls carry the session cookie (`walgit_session`, minted by `/_auth/login` → the issuer → `/_auth/callback`) with
`credentials: "include"`. No lane-first forms, no aliases.

Reference implementation: `crates/walgit-server/src/web/{v1,api,ui}.rs`.
Client: **`web/sdk/repos.ts`** (the SDK; the SPA's `web/src/api.ts` is a thin
adapter over it — the dogfood rule) and `web/src/use-resolved.ts` (the
resolve-then-fetch-by-sha flow of §2a).

## 0a. First-run setup (D43, setup state only)

While the store is `memory` without `memory_backend_intentional` (the
installer's initial config), the instance serves **only** the wizard: `/setup`
(SPA shell), its `/_ui/*` assets, `/healthz`, `/readyz`, and the data-free
`/api/v1/setup/*` surface below — open like `/_auth/*`, no bearer, no repo
data; `/` redirects to `/setup`; everything else answers 503 with a pointer.
Once configured (backend != memory) every `/api/v1/setup/*` path answers 404;
the configured-state entry point for the same settings is the admin
`/api/v1/store` surface (§4, #127).

```
GET  /api/v1/setup/status  → 200 {needs_setup:true, backend:"memory", auth_mode, can_save}; 404 once configured
POST /api/v1/setup/test    → {backend:"s3"|"gcs", bucket, endpoint, region, access_key, secret_key, force_path_style}
                              → 200 {ok:true, ...} when the bucket is reachable; 400 {ok:false, message} otherwise (nothing persisted)
POST /api/v1/setup/save    → {store:{...same}, admin_token?, admin_principal?}
                              → 200 {saved:true, restart:"supervisor"|"manual", warnings, file} — the config file is
                                rewritten (comments preserved); the CLI serve path then exits 75 for the supervisor
                              → 400/503/500 with the reason; nothing written on refusal
```

Setup state requires a loopback listen (startup refusal otherwise). S3
credentials persist as literals (`[store.s3] access_key`/`secret_key`, they
win over `*_env`; 0600 the file); GCS rides ADC. `can_save` on both this
surface and `/api/v1/store` is true only when the `--config` path is a
regular file — `/dev/null`/`NUL` instances refuse the save with the 503
(#129); successful saves write atomically (same-dir temp + rename, #129).

## 0. Lanes and auth (D27)

| Lane | Path | Who | Credentials |
|---|---|---|---|
| **bearer / same-origin** | `/{owner}/{repo}/api/…` (+ `/api/v1/…` for non-repo) | git tooling, CI, agents, scripts, and the bundled same-origin UI | `Authorization: Bearer <token>`, or the same-origin session cookie |
| **browser** | `/{owner}/{repo}/api-browser/…` (+ `/api-browser/v1/me\|authenticate`) | other allowed origins loading `repos.js` | `fetch(…, {credentials: "include"})`; the session cookie from walgit's own sign-in (`/_auth/login`) |

**The repository prefix comes first on every lane** — it is the only routing key (D26: one edge rule sends
`/acme/monorepo*` to its host). Both lanes hit the same handlers; they differ by credential handling and CORS,
never by a rewrite. There are no other forms: the lane-first `/api/v1/repos/…`, `/api-browser/v1/repos/…` and the
pre-D15 `/services/api/{owner}/{repo}/…` are gone (prototyping phase — no aliases, AGENTS banner).
Every request authenticates (§1.3 of AGENTS.md); a missing/expired browser
session answers `401` and the SDK opens **`/api-browser/v1/authenticate`** in a popup —
walgit's own sign-in (`/_auth/login`) runs before the application page `postMessage`s
`{type: "repos:authenticated"}` to its opener and closes — then retries once.
`GET /api/v1/me` → `{principal, write, admin, anonymous}` (`no-store`).

CORS: origins listed in `server.cors_origins` (exact, or one leading `*.`,
e.g. `https://*.docs.example.com`) get `Access-Control-Allow-Origin: <origin>`
+ `Allow-Credentials: true` + `Expose-Headers: ETag, …` + `Vary: Origin` on
`/{owner}/{repo}/api[-browser]/*` and `/api*`; preflights are answered without auth; a state-changing request from
any other origin is refused (403) before reaching a handler. No CORS headers
anywhere else. Empty list (default) = no cross-origin lane at all.

Versioning is app-owned: additive changes ship as they are; a breaking change of the repo surface gets a new
lane segment (e.g. `/{owner}/{repo}/api2/…`) without infrastructure work.

## 1. Hosting contract

| What | Rule |
|---|---|
| Asset base | Built assets are served under **`/_ui/`** (`vite.config.ts` `base: "/_ui/"`, content-hashed filenames, `public, max-age=31536000, immutable`, strong ETag, brotli/gzip precompressed; `index.html` is `no-cache` + ETag and carries the import map; see `web/README.md`). |
| Page routes | Every UI route below MUST return `web/dist/index.html` (`text/html; charset=utf-8`, `Cache-Control: no-cache`) so deep links and reloads work: `/`, `/repos` (host-wide repo index, the top-nav「仓库」entry), `/{owner}`, `/{owner}/{repo}`, `/{owner}/{repo}/tree/*`, `/{owner}/{repo}/blob/*`, `/{owner}/{repo}/commits`, `/{owner}/{repo}/commits/*`, `/{owner}/{repo}/commit/*`, `/{owner}/{repo}/wal`, `/{owner}/{repo}/settings`, and `/api` (the "API" docs page in the UI; `?repo=owner/name` pre-fills its examples; `/api/v1` itself is the JSON discovery document). |
| API base | **`/{owner}/{repo}/api`** (bearer / same-origin cookie) and **`/{owner}/{repo}/api-browser`** (cross-origin browser), D15/D26/D27: one path prefix per repository, so the edge routes e.g. `acme/monorepo` to its host. Non-repo: `/api/v1` (discovery, `me`, `authenticate`, `owners`), `/services/api/owners*`, `/services/api/instance`. The bundled UI fetches same-origin with the session cookie and `Accept: application/json, text/event-stream`. No aliases or application sessions. |
| Public lane | **`/services/public/*`** — the one open prefix besides health and the SDK (nginx skips `auth_request` there; the app never authenticates here and never reads repo data). Today exactly `/services/public/install.sh[?repo=owner/name]` (`text/x-shellscript`, `Cache-Control: public, max-age=300`); `/services/public/ca.pem` (the certificate this process presents when it terminates TLS itself, `application/x-pem-file`, D39; 404 behind an edge); the shipped ops skill (D47) — `/services/public/skill/SKILL.md` (`text/markdown`, strong ETag), `/services/public/skill/manifest.json` (`{name, version, sha256, bytes, skill_url, install_url}`, `no-cache`) and `/services/public/skill/install.sh` (POSIX sh, `no-cache`, one-line installer); anything else under it is 404. |
| Setup | `/services/setup.json[?repo=owner/name]` — the clone/setup recipes (`setup::Recipes`: `token_url`, `install`, `install_url`, `plain_clone`, `blobless_clone`, `bundle_list`, `manual_clone`, `setup_text`, and `ca_url` + `trust` when the host terminates self-signed TLS itself — D39), `no-cache`; `/services/public/install.sh[?repo=]` — the one-time installer (open lane, AGENTS §1.3) (POSIX sh). The Clone menu and the API page render these, never their own copies. |
| Auth | `/_auth/login?next=`, `/_auth/callback`, `/_auth/logout`, `/_auth/me`, `/_auth/check` (an `auth_request` target for an edge), `/_auth/tokens` (GET: the token page; POST: mint a walgit access token for the signed-in principal, `{token, principal, write, expires_at}`, same-origin only) — in-app OIDC sign-in + session cookie. Off until `session_secret` + the OAuth client are set. |
| SDK | `/repos.js` (IIFE, registers `window.repos`) and `/repos.mjs` (ESM), built from `web/sdk/repos.ts` into `web/dist/` by `pnpm run build`; `no-cache` + strong ETag, precompressed. `/SKILL.md` (D42) serves `web/SKILL.md` verbatim (`text/markdown`, open, strong ETag) — the AI-agent guide to using this host; it carries the one-liner that installs the ops skill (D47, `/services/public/skill/*`). These data-free routes are open at the application. |
| Dev | `vite dev` proxies `/api/`, `/api-browser/`, `/services/api/` and `/{owner}/{repo}/api[-browser]/…` (plus git/bundle paths) to `$WALGIT_URL` (default `http://127.0.0.1:8080`). |

Path segments in URLs are `encodeURIComponent`-encoded per segment by the
client (`enc()` in `api.ts`); `/` between segments is literal. Servers must
decode each segment.

## 2. Conventions

- **Success**: `200`, `Content-Type: application/json`, plus the cache
  headers in §2a. The UI relies on the browser HTTP cache — it does not
  cache in JS — so the headers are the contract. The server must still
  return a **consistent, up-to-date** view on every revalidation (walgit
  reconciles the warm repo against the WAL before every read; another host
  must offer the same "read your own push" guarantee or the UI will show
  stale refs after a push).
- **Errors**: non-2xx with a **plain-text body** (the message is shown
  verbatim in the UI's error box). `404` for unknown owner/repo/ref/path/
  sha (walgit maps git's "Not a valid object name", "not a tree object",
  "unknown revision", "bad revision", "does not exist" to 404); `5xx` for
  server faults. No JSON error envelope.
- **Null safety**: every array field MUST be `[]` when empty, never `null`
  (the UI calls `.length`/`.map` directly).
- **Timestamps**: RFC 3339 strings (`%aI`/`%cI`), e.g.
  `2026-08-19T11:55:42-04:00`.
- **SHAs**: full 40-hex (SHA-1) strings. The UI abbreviates for display and
  uses full SHAs in links.
- **Sizes**: bytes, integers.

### 2a. Caching — the central design rule

Two classes of URL, and the client is built so that almost every request
falls in the first:

| Class | URLs | Headers |
|---|---|---|
| **Sha-addressed, immutable** | `tree/{sha}/…`, `blob/{sha}/…`, `commits?ref={sha}`, `commit/{sha}` where `{sha}` is a full 40-hex sha | `Cache-Control: private, max-age=31536000, immutable`. Content can never change; browsers never re-ask. |
| **Ref-dependent** | `owners`, `owners/{o}`, `refs`, `refs/branches`, `refs/tags`, `resolve/…`, and any tree/blob/commits/commit addressed by a *name* | `Cache-Control: private, max-age=0, stale-while-revalidate=60` and, where there is a natural one, `ETag: "<resolved sha>"` with `If-None-Match` → `304`. |

Flow per navigation: one ref-dependent call (`resolve`, SWR — paints
instantly from cache, revalidates in the background; a `304` costs the
server only the freshness check it would do anyway), then one
sha-addressed call (immutable — usually a cache hit on revisits, back
button, ref switch that lands on the same commit). `refs` is fetched once
per repository visit.

**Guidance for implementers** (this is where a monorepo-scale host wins or
loses):

- Make `resolve`, `refs` and the ref lists fast under *any* ref count: O(k)
  exact lookups for resolve (k = path segments), O(1) for `refs`, bounded
  output for the lists. Never "load all refs, then filter" per request.
- Keep an in-process **LRU of resolved ref → sha** (and of ref-list pages
  keyed by `(repo, kind, prefix, q, after, n)`) invalidated by the repo's
  ref-state version (walgit: the WAL manifest version; elsewhere: the refs
  ETag / packed-refs mtime / reftable generation). Serve `304` straight from
  that cache when `If-None-Match` matches.
- Honour **stale-while-revalidate** server-side too: if you front the API
  with a cache (CDN, nginx, Varnish) let it serve stale ref-dependent
  responses while one request refreshes; the UI is designed to tolerate
  ≤60 s-old ref data because the second, sha-addressed step is always exact.
- Sha-addressed responses are safe to cache **anywhere, forever** (shared
  caches included, if your auth model allows; walgit says `private` because
  a cloud IAM proxy sits in front). An LRU of rendered tree/commit JSON keyed
  by `(sha, path)` turns repeat views into memory reads.
- Use `private` unless every caller is allowed to see every repo; then
  `public` lets a shared cache absorb the load.

### 2b. Long answers — the SSE envelope

Some answers need work that takes seconds or minutes on a cold instance:
downloading a repo's packs (materialize), downloading pack **indexes** for a
repo too large to materialize (objects are then read from the store by
range), reading objects remotely, running a maintenance task. The UI must
never stare at a silent spinner, so every JSON endpoint accepts

```
Accept: application/json, text/event-stream
```

and answers in one of two ways, chosen by the server:

* **Plain JSON** (with the §2a cache headers) whenever it can answer
  immediately — cached render, packs on disk. The browser HTTP cache keeps
  working; this is the normal case.
* **`text/event-stream`** otherwise: a sequence of packets, then exactly one
  terminal packet. Standard packet names (shared by every streaming endpoint,
  including `…/tasks/{id}` and `POST …/ops/{op}`):

| `event:` | `data:` (JSON) | Meaning |
|---|---|---|
| `notice` | `{"text": "Downloading 1 pack index (2.1 GB)"}` | What the server is doing now. Show it. |
| `progress` | `{"label","done","total"?,"unit","percent"?}` | A bar; `total` absent = indeterminate. The latest packet per `label` wins. |
| `task` | `{TaskRecord}` (see §4 Tasks) | A background task this request depends on started/changed/finished. |
| `result` | *exactly the JSON body the plain endpoint returns* | Terminal. |
| `error` | `{"status": 503, "message": "…"}` | Terminal. Same text the plain endpoint would send. |

Comment lines (`: keepalive`) may appear at any time. A client treats the
stream as: render notices/progress live, resolve with `result`, reject with
`error`; if the stream ends without either, that is an error too. Streamed
results are not HTTP-cacheable, but the server keeps them in its immutable
render cache (and, for remotely served repos, in the object store), so the
*next* request for the same sha is plain JSON.

`…/blob/…?raw` is a page navigation and never streams.

### 2c. Tasks

Anything long-running is a **task** with a unique id, discoverable per repo:

* `GET …/tasks` → `{"hostname","running":[TaskRecord],"recent":[TaskRecord]}`
  (`Cache-Control: no-store`). `running` is what is happening to this repo on
  the instance that answered; the UI shows them as a pill in the repo header
  (spinner + job name + percent, click for the full list) and polls while any
  is running. Because routing is random, a task that vanishes from `running`
  counts as finished only when the same instance answered or `recent` lists it
  with a result — not when a different instance answered.
* `GET …/tasks/{id}` with `Accept: text/event-stream` → attach: `task` packet,
  replay of the packets so far, then live, then `result`
  (`{"task": TaskRecord, "value": …}`) or `error`. Without SSE accept → the
  `TaskRecord` as JSON. `404` if unknown here (tasks are per instance; the
  record says `hostname`).
* `POST …/ops/{op}?params` (write permission) → starts a maintenance task and
  returns the same attach stream; if the same `(repo, kind)` is already
  running, the response attaches to it instead of starting a second one.

`TaskRecord`: `{id, kind, repo, hostname, started, finished?, elapsed_ms,
ok?: bool, summary, progress?: {label,done,total?,unit,percent?},
log_tail: [string], params?: {…}}`; `ok` absent = running. Kinds today:
`materialize`, `remote-index`, `fsck`, `compact`, `bundle`, `checkpoint`,
`sync`, `rematerialize`.

## 3. Ref + path resolution (`{rest...}` routes)

Page URLs are GitHub-shaped: `/{ref}/{path}` with no delimiter, so
`feature/x/src/main.go` is ambiguous. The **server** resolves it (the client
never has the ref list) via `GET …/resolve/{rest}`:

1. For `rest = s1/s2/…/sk`, the candidates are the k prefixes `s1`,
   `s1/s2`, … as branch names and as tag names — **2k exact ref lookups**
   (walgit: one `for-each-ref` with those 2k exact patterns; a packed-refs or
   reftable host does k binary searches). Do **not** scan all refs.
2. The **longest** candidate that exists wins; on equal length a branch
   beats a tag. The remainder (leading `/` trimmed) is the path.
3. No match → the **first segment** must be a revision git can resolve to a
   commit (full/abbreviated sha; `HEAD`); `kind: "commit"`. Otherwise `404`.
4. Empty `rest` → the default branch (`refs.head`); `404` if unborn.

Tags resolve to the **peeled** commit. Tree/blob/commits endpoints also
accept a name in `{ref}` and apply the same rule server-side, but the UI
always sends the sha it got from `resolve` so those requests are immutable.
Responses echo `ref`, `sha` and `path` so the client never re-splits.

## 4. Endpoints

### `GET /api/v1/owners`

Top-level namespaces.

```json
["demo", "jane"]
```

Sorted, `[]` if none. Must come from the authoritative repo list (walgit
lists the object store, not local disk, so a cold node sees everything).
Cache: SWR (§2a).

### `GET /api/v1/owners/{owner}/repos`

Repositories under one owner, short names only.

```json
["hello", "walgit"]
```

Sorted, `[]` for an unknown/empty owner (200, not 404). Cache: SWR.

### `GET /api/v1/me`

```json
{ "principal": "jane@example.com", "write": true, "admin": true, "anonymous": false }
```

`401` without credentials. `Cache-Control: no-store`. `admin` (§1.3/D24's
judgement) gates the admin surfaces — `PUT/DELETE` of settings and
`policy.json`, and the storage editor below; `mode = none` on loopback is
admin for everyone.

### `GET|PUT /api/v1/store` · `POST /api/v1/store/test` — storage editor (#127, admin)

The configured-state twin of the §0a wizard: one mechanism (the wizard's
compose + validate + `toml_edit` write-back + exit-75 supervisor handoff),
hung twice. **404 while the instance is in setup state** (the wizard owns
that; this surface passes the gate precisely to say so). Admin-gated with
§1.3's channel semantics: missing credential `401` (Bearer-only challenge),
authenticated non-admin `403`.

```
GET  /api/v1/store  → 200 {backend,bucket,prefix,endpoint,region,force_path_style,
                            has_access_key,has_secret_key,can_save}  (`no-store`)
POST /api/v1/store/test → {backend:"s3"|"gcs",bucket,endpoint,region,access_key,secret_key,force_path_style}
                            → 200 {ok:true,…} / 400 {ok:false,message}; nothing persisted
PUT  /api/v1/store  → {store:{…same fields…}}
                            → 200 {saved:true,restart:"supervisor"|"manual",warnings:[…],file}; 400/503 on refusal
```

Two contracts distinguish this from the wizard's surface:

* **Credentials never leave the server.** `GET` answers them as presence
  bits only (`has_access_key` / `has_secret_key`); the secret values exist
  in `walgit.toml` and nowhere on the wire.
* **A blank credential field means “keep the current value”** (the wizard's
  blank meant “omit”). Test and save overlay the submitted parameters on a
  clone of the *running* config, so an unmentioned field behaves as the
  instance runs today, and blank fields in the write-back leave the file's
  existing `access_key`/`secret_key` untouched. A submitted value always
  overwrites.
* `PUT` body is exactly `{store: {...}}` — the wizard's first-admin fields
  (`admin_token`, `admin_principal`) are refused with a `400` here. Validation
  happens **before** the file is touched: a `400` writes nothing.
* `can_save: false` ⇒ `PUT` answers `503` with the reason. False when the
  instance was not started from a config file **or the `--config` path is not
  a regular file** — `--config /dev/null` (D39's explicit defaults+env form,
  `NUL` on Windows) accepts writes and discards them, so saving into it is
  refused (#129).
* `warnings` (issue #129): a successful save reports, in addition to
  `saved`, the ways the written file is not self-sufficient. Today one case:
  the file holds no literal `access_key`/`secret_key` while the running
  instance gets it from the environment (a `WALGIT__STORE__S3__*` override,
  or the variable named by `access_key_env`/`secret_key_env`). Such an
  instance boots today and fails to open the store on a restart outside that
  environment — the editor's "blank = keep" kept only what the *file* can
  see, so the response says so instead of lying silently. `[]` when the save
  is durable on its own. The write-back is an atomic same-directory
  temp-file + rename (#129), so a crash or ENOSPC mid-save cannot destroy
  the credentials' only carrier; concurrent PUTs are last-writer-wins.

The SPA renders this as `/setup`'s admin face (pre-filled form, the top-bar
「存储配置」entry for `me.admin`); the SDK maps all three
(`client.store.get/test/save`, D20). The save-success phase of the same page
lists `warnings` in a warning box (`StoreSaveResult.warnings`, #134) — on
both faces, the wizard's `POST /api/v1/setup/save` and the editor's `PUT` —
so an env-supplied credential never waits for the next restart to be found.

### `GET /api/v1/principals` · `PUT|DELETE /api/v1/principals/{principal}`

The host-global principal registry (issue #76; cross-repo identity — one
registration verifies in every repository of this host; repo-local
`refs/collab/meta/principals/*` remains authoritative when present):

- `GET /api/v1/principals` → `{ "alice": "<base64 ed25519 public key>", … }`
  (self-describing map; the collab read paths consult it per request through a
  TTL cache — issue #104 — so reads never LIST the store hot).
- `PUT /api/v1/principals/{principal}` with `{ "public_key": … }` —
  register/rotate this principal's key; **the authenticated principal must
  match the path** (self-registration), else `403`.
- `DELETE /api/v1/principals/{principal}` — revoke; self only for now.

Writes invalidate the cache immediately on this instance (a revoke is
effective within the TTL — 60 s — on every other instance). `GET
/api/v1/principals` requires read auth; PUT/DELETE write auth (§1.3).

### `GET /{owner}/{repo}/api`

```json
{ "owner": "acme", "name": "monorepo", "full_name": "acme/monorepo",
  "head": { "name": "main", "sha": "807d45a6…" }, "branches": 12, "tags": 3,
  "clone_url": "https://git.example.com/acme/monorepo.git",
  "html_url": "https://git.example.com/acme/monorepo",
  "api_url": "https://git.example.com/acme/monorepo/api" }
```

Ref-level summary: head (`null` when unborn), O(1) ref counts from the ref
index, URLs. `404` for an unknown repo. Cache: SWR + `ETag: "<head sha>"`.
`PUT` creates the repository (write permission; `201`/`200`), `DELETE`
removes it (admin permission) — the same handlers as `PUT|DELETE /{owner}/{repo}`.
`GET|PUT|DELETE …/policy` is the push policy document (`docs/POLICY.md`).

`GET|PUT|DELETE /{o}/{r}/api/settings` (D24, 2026-08-21) is the repository's **settings in the WAL**: a TOML document
restricted to `[bundles]`, `[maintenance]`, `[compaction]`, `[upstream]`, and `[integrations]`, merged over the
host's config (`effective config`).
`GET` → `{revision, author, updated_at, message, toml}` (`revision: 0` = none). `PUT` body = the TOML
(`?message=` optional), validated against the serving host's build — 400 with the reason and nothing published
on failure; 200 `{revision}`. `DELETE` publishes an empty document. `GET …/settings/effective` → the effective
`[bundles]`/`[maintenance]`/`[compaction]`/`[upstream]` as TOML (`application/toml`; no host secrets,
no `token_env`); `GET …/settings/history` → `{min_seq, entries:[{seq,revision,author,message,
at,toml}]}` from the live log (older changes are folded into checkpoints). All `no-store`; PUT/DELETE need
**admin** (`tokens[].admin` or oidc `admin_emails`/`admin_domains`; `mode = none` is admin on loopback).
Every instance sees a new revision on its next refs-level sync (no extra round trip: the document rides
inline on `manifest.pb`). CLI: `walgit repo settings show|set|clear|history`.

Settings tab helpers (`/{o}/{r}/api/settings…`, all `no-store`): `GET …/settings/describe` → `{settings, sections,
strategies:[{name,kind,base,schedule,schedule_human,next,keep,backfill_max,min_commits,refs}], bundles, maintenance:
{checkpoints,interval_secs,this_host:{name,serves,maintains,disk,max_pack_bytes,cache_budget_bytes,roles}}, compaction,
upstream:{git,lfs,token_env:bool,follow:[refs],follow_interval_secs,last_round:{at,outcome: in-sync|published|refused|failed,
detail,upstream:{ref:oid},ours:{ref:oid}}|null} (D33; last_round = this instance's last follow round),
fields:[{key,value,host_value,source: host|setting}], head_seq}`; `POST …/settings/validate` (body TOML) → the same
shape for the *would-be* effective config with `ok: true`, or `{ok: false, errors[]}` (nothing published);
`POST …/policy/validate` (body policy JSON) → `{ok, errors[], rules, groups, protect}`; `POST …/policy/dry-run?last=N`
(body policy JSON, empty = the saved policy) → the policy evaluated against the last N PUSH entries of the live log
(`{pushes, allowed, denied, results:[{seq,at,principal,atomic,refs:[{name,ok,reason,force}]}]}`; force = non-ancestor
update when objects are local). SDK: `repo.settings.{get,put,delete,effective,history,describe,validate}`,
`repo.policy.{validate,dryRun}`.

### `GET /{owner}/{repo}/api/refs`

```json
{ "head": { "name": "main", "sha": "807d45a6…" } }
```

**O(1) by design**: only the default branch (`HEAD` symref target and the
commit it points at). `"head": null` when HEAD is unborn → the UI shows the
"Setup" empty-repo box. No branch/tag arrays here — a repo with a
million refs must answer this as fast as one with three. Fetched once per
repository visit. Cache: SWR + `ETag: "<head sha>"` → `304`.

### `GET /{owner}/{repo}/api/refs/{branches|tags}?prefix=&q=&after=&n=`

One **name-sorted page** of one namespace, for the branch/tag picker.

```json
{ "refs": [ { "name": "2-5-stable", "sha": "46b7fd29…" }, … ], "more": true }
```

- `prefix`: path prefix under the namespace (`refs/heads/<prefix>/`), lets a
  server use a pattern instead of a scan. `q`: case-insensitive substring
  filter on the short name (the picker sends this per keystroke,
  debounced). `after`: name cursor — return names strictly greater (byte
  order) than it. `n`: page size (walgit: default 100, max 1000).
- Sorted by name ascending (byte order — `git for-each-ref --sort=refname`);
  tag `sha` is the peeled commit. `more` = at least one further match
  exists. `[]` when none. Unknown namespace → `404`.
- **SSE option**: with `Accept: text/event-stream` the same page streams as
  `event: ref` / `data: {"name","sha"}` per ref, terminated by
  `event: done` / `data: {"more":bool}` (this predates §2b and keeps its own
  packet names), so a client can paint progressively while the server is
  still walking refs. The bundled UI's branch/tag picker uses this SSE form
  (painting matches as they arrive, aborting on the next keystroke); walgit
  writes each event as soon as it is found (no buffering, `X-Accel-Buffering:
  no`, never compressed). Either way the server must stop walking after `n`
  matches.
- Cache: SWR (same ETag rule as `refs` if you can produce one cheaply; walgit
  sends SWR without ETag here).
- Implementers: cache pages in an LRU keyed by the full query + the repo's
  ref-state version; the picker's typical traffic is the same handful of
  queries repeated.

### `GET /{owner}/{repo}/api/refs/{all|collab}?prefix=&q=&after=&n=`

Full-name pages for the collaboration layer (D1) and any other tooling that
namespaces refs: `all` is every ref in the repository, `collab` is the
`refs/collab/*` namespace (the decentralized-collaboration inbox/meta refs;
a convenience equivalent to `all?prefix=refs/collab`).

```json
{ "refs": [ { "name": "refs/collab/inbox/alice@example.com/1", "sha": "807d45a6…" }, … ], "more": true }
```

- Same pagination contract as `refs/{branches|tags}`: `prefix`, `q`,
  `after`, `n`, byte-sorted ascending, `more`, SSE option (`event: ref` /
  `event: done`), SWR cache.
- **Names are full ref names** (unlike `branches`/`tags`, which trim their
  namespace): `prefix` therefore matches against `refs/…` (e.g.
  `prefix=refs/collab/inbox` on `all` — a bare `prefix=inbox` matches
  nothing). `[]` when the namespace is empty. Unknown namespace (anything
  other than `branches|tags|all|collab`) → `404`.

### `GET /{owner}/{repo}/api/refs/name/{rest...}`

Exact full-ref lookup by name — the tip of one D1 inbox, a notes ref, any
ref. `rest` is the full name with slashes (`refs/collab/inbox/alice@example.com/1`).

```json
{ "name": "refs/collab/inbox/alice@example.com/1", "sha": "807d45a6…" }
```

- `sha` is the raw oid the ref points at (no peeling). `404` when the ref
  does not exist. Cache: SWR + `ETag: "<sha>"` → `304` (the ref's oid is a
  strong version for its value).

### `GET /{owner}/{repo}/api/merge-base?from=&to=`

The merge base of two revisions — the diff base for 3-dot PR comparisons (D1
review primitive).

```json
{ "from": "807d45a6…", "to": "5ed435d9…", "merge_base": "46b7fd29…" }
```

- `from` / `to`: branch name, tag name, or commit-ish (same resolution as
  `resolve`). The response echoes the **resolved commit shas**.
- Local packs run `git merge-base`; a remote-served base uses a **bounded
  bidirectional walk** over the pack set (`Remote::merge_base`), which for the
  branch-from-trunk shape returns the branch point — criss-cross histories may
  differ from git's preferred base. The walk is narrated (SSE) and gives up
  with a clear error past the budget (paths too far apart for a remote reader).
- **Remote revision resolution** (applies to `merge-base`/`diff`/`blame`):
  on a remote-served base `from`/`to` resolve by branch, tag or full/unique
  hex sha — revision expressions like `HEAD~1` or `main^` need local packs
  and are `404` on remote.
- `merge_base` is `null` when the histories are unrelated. In a deep
  repository two unrelated histories can exhaust the walk budget first and
  surface as a `503` ("too far apart (or unrelated)"). Unknown revision →
  `404`. Cache: SWR (depends on ref state, not immutable).

### `GET /{owner}/{repo}/api/diff?from=&to=&format=`

The tree diff between two revisions — the file list / stat / patch a PR
review (human or agent) renders. `format` is `patch` (default), `stat` or
`name-status`.

```json
{ "from": "807d45a6…", "to": "5ed435d9…", "format": "name-status",
  "changes": [ { "status": "M", "path": "src/inner/x.txt" },
               { "status": "R", "path": "src/app.rs", "old_path": "src/main.rs" } ] }
```

- `from` / `to` resolve like `resolve` (branch, tag, commit-ish); the response
  echoes the resolved shas. `stat` returns the `stats` array of the `commit`
  endpoint (`--numstat -M`, renames as new path); `patch` returns the raw
  unified diff text.
- Local packs run `git diff` directly; a remote-served base faults exactly the
  trees/blobs the diff touches into the loose store first (`Remote::fault_diff`,
  level-parallel, narrated on SSE, bounded by `MAX_DIFF_OBJECTS`) and then
  runs the same `git diff` — the two paths are byte-identical.
- Unknown format → `404`; `from == to` → empty changes / empty patch. Cache:
  SWR (ref-state dependent, not immutable).

### `GET /{owner}/{repo}/api/blame/{rev}/{path}`

Line attribution for one file — who last changed each line — for review
context and agent lookups. `rev` resolves like `resolve`; `path` is the file.

```json
{ "sha": "807d45a6…", "path": "src/app.rs",
  "blame": [ { "line": 1, "commit": "5ed435d9…", "author": "alice",
               "author_email": "alice@example.com", "time": 1710000000,
               "summary": "move main into app", "text": "fn main() {}" } ] }
```

- The line order matches the file; `text` is the line content (self-contained,
  no separate blob fetch). Local packs run `git blame --porcelain`; a
  remote-served base faults the path history first (`Remote::fault_blame`,
  bounded by `BLAME_BUDGET` commits, narrated on SSE, path boundaries and
  merge parents covered) and then runs the same git — same objects, same
  answer.
- **Remote limits**: a file whose *ancestry* is deeper than the budget → `503`
  (deep-history files need a host with the packs or a bundle — the budget
  counts every ancestor the walk must fault, not just lines that changed).
  Renames **are followed on remote** when the content did not change with the
  move: at a boundary the walk faults both sides' tree skeletons (bounded by
  `BLAME_TREE_BUDGET` tree objects, `503` past it) and continues along the
  exact content predecessor — the same candidate git blame's own rename
  detection picks first, so local and remote agree. Renames that also change
  the content (similarity detection) are still not followed; they need a host
  with the packs or a bundle.
- `blame` of a directory or a missing path → `404`. Cache: SWR.

### `GET /{owner}/{repo}/api/archive/{rev}?format=`

A tree archive at one revision as a binary download — `format=tar.gz`
(default) or `zip`. The "download this repo" button; small/medium repos get a
real archive, large ones a clear 503 pointing at bundle-uri (where big-repo
bytes belong anyway).

- Local packs run `git archive` directly; a remote-served base faults the
  whole tree first (`Remote::fault_tree_all`, level-parallel, deduped, bounded
  by `ARCHIVE_OBJECT_BUDGET` objects → `503` past it).
- `Content-Type`: `application/gzip` / `application/zip`; `ETag: "<sha>"` +
  SWR. Unknown format or revision → `404`. Never the SSE envelope (binary).

### `GET /{owner}/{repo}/api/resolve/{rest...}`

```json
{ "ref": "feature/x", "sha": "5ed435d9…", "path": "src/main.go", "kind": "branch" }
```

The ref/path split of §3 plus dereference. `kind` is `branch | tag |
commit`. `404` (plain text) if nothing resolves. Cache: SWR + `ETag:
"<sha>"` → `304`. This is the one request the UI makes per navigation
that depends on ref state; make it cheap and cache it hard.

### `GET /{owner}/{repo}/api/tree/{ref}/{path}`

Directory listing (one round trip for the repo home).

```json
{
  "ref": "main",
  "sha": "807d45a6…",
  "path": "proto",
  "entries": [
    { "name": "pb", "type": "tree", "mode": "040000", "size": -1, "sha": "…" },
    { "name": "gitwal.proto", "type": "blob", "mode": "100644", "size": 4821, "sha": "…" }
  ],
  "commit": { …Commit… },
  "readme": { "name": "README.md", "contents": "# …" }
}
```

- `path` is `""` for the root. Trailing/leading slashes ignored.
- `entries`: sorted **directories first**, then by name (byte order).
  `type` is `blob | tree | commit` (submodule). `mode` is the git mode
  string. `size` is `-1` for trees/submodules. `[]` for an empty tree.
- `commit` (optional): the newest commit touching `path` on `ref`
  (`git log -1 ref -- path`); shown in the tree header. Omit if unknown.
- `readme` (optional): contents of the first entry named (case-insensitive)
  `README`, `README.md`, `README.markdown`, `README.txt`, `README.rst`, only
  if valid UTF-8. Rendered as Markdown under the listing.
- `404` if `ref` is unknown or `path` is not a tree (e.g. a blob path).
- `{ref}` may be a name (resolved per §3; response SWR + ETag) or a full sha
  (response **immutable**). The UI always sends the sha. `sha` in the
  response is the resolved commit.

### `GET /{owner}/{repo}/api/blob/{ref}/{path}[?raw]`

```json
{ "ref": "main", "sha": "807d45a6…", "path": "src/main.go", "name": "main.go", "size": 1234, "contents": "package main\n…" }
```

- Exactly one of: `contents` (UTF-8 text, no NUL bytes), `binary: true`,
  or `too_large: true` (walgit's limit is 2 MiB; the limit is the server's
  choice, the UI just shows "too large"/"binary" with `size`).
- `?raw`: the **byte channel** (the "Raw" link, and what the blob page's
  image/video/audio/PDF viewers fetch). Any file type, with its real
  `Content-Type` by extension (images, `video/*`, `audio/*`, `application/pdf`,
  `text/html`, …; unknown extensions are `application/octet-stream`) plus
  `X-Content-Type-Options: nosniff` and `Content-Encoding: identity` (the byte
  channel is never re-encoded). A single `Range` is served as `206` +
  `Content-Range` + `Accept-Ranges: bytes` — media seeking and PDF partial loads
  rely on it. Untrusted active content (`text/html`, `application/xhtml+xml`,
  `image/svg+xml`, `application/xml`) additionally carries
  `Content-Security-Policy: sandbox; default-src 'none'; …` so it can never run
  on the app's origin. The complete object is faulted to serve it, so the channel
  is capped at 32 MiB and answers `413` beyond that (the JSON lane keeps its own
  2 MiB cap). Strong `ETag` (hashed over the *resolved* revision + path) on every response, immutable
  or not; `If-None-Match` → `304`.
- Markdown (`.md`, `.markdown`) and README blobs get a Preview/Code toggle
  client-side; no server involvement.
- `404` if unknown ref or path. Cache: as tree (`{ref}` sha → immutable;
  name → SWR + ETag). `?raw` follows the same rule.

### `GET /{owner}/{repo}/api/commits?ref=&path=&skip=&n=`

Linear history page.

```json
{ "ref": "main", "sha": "807d45a6…", "commits": [ …Commit… ], "more": true }
```

- `ref`: a full sha (what the UI sends; response **immutable**) or a
  branch/tag/revision name (resolved per §3; response SWR + ETag); default
  `HEAD`. `path`: optional, limits to commits
  touching it (`git log ref -- path`); `""`/absent = whole history.
- `skip` (default 0) and `n` (default 35, server may cap; walgit caps at
  200). `more` is true when at least one more commit exists after this page
  (walgit asks for `n+1`). Pagination in the UI is "Older" → `skip +=
  commits.length`.
- Order: `git log` default (reverse chronological, topo where needed).
- `commits` is `[]` (never null) for an empty result; unknown ref → 404.

`Commit`:

```json
{
  "sha": "33419c76…",
  "parents": ["f6ef950…"],
  "author": "Jane Doe", "author_email": "jane@…", "author_date": "2026-08-19T11:55:42-04:00",
  "committer": "Jane Doe", "commit_date": "2026-08-19T11:55:42-04:00",
  "subject": "first line",
  "body": "rest of the message WITHOUT the trailer block, trailing newlines trimmed, \"\" if none",
  "trailers": [ { "key": "Merge-Queue-Pull-Request", "value": "42" }, { "key": "Co-authored-by", "value": "Jane <jane@…>" } ]
}
```

`parents` is `[]` for a root commit. `trailers` are the git trailers of the message
(`git interpret-trailers --parse` rules: the `Key: value` lines of the last paragraph,
continuation lines folded, in order; `[]` when the last paragraph is prose) — `body` is
the message with that block removed. Repo-agnostic; the UI knows a small table of keys
(merge-queue CI / people) for grouping and linking. The UI groups the
list by `commit_date` day and shows `subject` + `author`.

### `GET /{owner}/{repo}/api/commit/{sha}`

```json
{
  "commit": { …Commit… },
  "stats": [ { "path": "server.go", "additions": 12, "deletions": 3 } ],
  "patch": "diff --git a/server.go b/server.go\n…"
}
```

- `{sha}` may be any revision git resolves to a commit (full/short sha,
  ref). `404` if not found. Full 40-hex sha → **immutable**; anything else
  → SWR + `ETag: "<full sha>"`.
- `stats`: `git show --numstat` order; `additions == deletions == -1` for
  binary files. Renamed files appear once (`-M`) with the new path. `[]`
  for an empty commit.
- `patch`: the **unified diff** of the commit against its first parent
  (against the empty tree for root commits: `--root`), `--no-color`,
  `--no-ext-diff`, rename detection on, default context. Parsed
  client-side by `@pierre/diffs` `parsePatchFiles(patch, sha)` and rendered
  unified/split; the `diff --git a/… b/…` headers must be present so file
  boundaries and names are detected. The UI anchors files by `stats[].path`
  matching the parsed file name, so use the same paths in both.
- Merge commits: diff against the **first parent** (GitHub semantics;
  walgit passes `--diff-merges=first-parent`). Do not return a combined
  (`diff --cc`) patch or an empty patch with non-empty `stats`.

### D1 collaboration lane (`/{owner}/{repo}/api/collab/…`) — walgit-specific

The decentralized collaboration layer (`docs/D1_COLLAB_DESIGN.md`) writes to
`refs/collab/*` (inbox entries + principals registry) and aggregates them
deterministically; these endpoints back the Collab tab and mirror what the
`walgit collab` CLI computes locally. Since D45 (issue #160) the aggregation
input is **snapshot ∪ inbox tail**: a signed fold at
`refs/collab/meta/snapshot` (written by `walgit collab gc`, which then deletes
the folded inbox refs) plus every inbox ref not yet folded — deduped by oid,
so a fold never changes any answer below. Auth: same as every repo-scoped write
(write tokens / signed-in principal; `actor == principal` is enforced).

#### `POST /{owner}/{repo}/api/collab/entries`

```json
{ "entry": { "version": 1, "kind": "comment", "id": "t1", "actor": "alice", "ts": 1786500000, "parent": "…", "body": {}, "sig": "ed25519:…" } }
```

Materializes the entry as a one-object bucket pack and publishes
`refs/collab/inbox/<actor>/<uuid>` through the WAL — the browser write path
(no git). `200` → `{ "ref", "oid", "seq" }`. The server does **not** verify
the signature (client-side); it enforces `actor == authenticated principal`
(`403` otherwise) and that the actor is a refname-safe segment. The
repository's `policy.json` gates this write **exactly as it gates
receive-pack** — a rule protecting or freezing `refs/collab/*` denies the
browser path too (`403`, the reason is logged server-side). No credential
→ `401`/`403` per lane. Signature verification happens in the aggregation.

#### `POST /{owner}/{repo}/api/collab/principal`

```json
{ "principal": "alice", "public_key": "base64(Ed25519 pub)" }
```

First-use self-registration: publishes the public key at
`refs/collab/meta/principals/<principal>` (D1 §5; re-registering **updates**
the ref — CAS against its current value, so a concurrent re-registration
loses and retries; the tombstone is a git deletion). `principal ==
authenticated principal` is enforced, as is `policy.json` (same gate as
receive-pack). `200` → `{ "ref", "oid", "seq" }`.

#### `GET /{owner}/{repo}/api/collab/report`

The full observability report (D1 §8) — thread summaries, PR status + merge
rule evaluation, the CI runs section (D1-CI §8.3: pure-CI threads are not
board cards but are projected here by the same aggregation as
`walgit ci status`), verification health, per-actor/per-kind activity. Shape
is `walgit-wal::collab::Report`:

```json
{
  "threads": [ { "id": "t1", "title": "hi", "entries": 4, "verified": 3, "last_ts": 1786500000, "kinds": ["comment","issue","patch","review"] } ],
  "prs": [ { "id": "t1", "title": "hi", "base": "refs/heads/main", "head": "refs/heads/topic", "status": "open", "approvals": 1, "merge_allowed": true, "merge_reason": "…" } ],
  "runs": [ { "id": "ci-9a1b…", "task": "test", "repo_ref": "refs/heads/main", "commit": "c0ffee…", "state": "done", "conclusion": "success", "runner": "ci-runner-a", "claims": 2, "last_ts": 1786500100 } ],
  "total_entries": 4,
  "verified_entries": 3,
  "unverified_entries": 1,
  "missing_principals": 1,
  "by_actor": [["alice", 3], ["bob", 1]],
  "by_kind": [["comment", 1], ["issue", 1], ["patch", 1], ["review", 1]]
}
```

`title` is the root entry's `body.title`, `""` when it has none — lists render
`title || id` (issue #131, same rule as board cards). The list order is part
of the projection and clients render it as-is: `threads` newest activity
first (`last_ts` desc, id asc on ties); `prs` `open` before `merged` before
`closed`, then newest activity first (id asc on ties).

`state` ∈ `pending | claimed | stale | done` — the attempt state machine of
`docs/D1_CI_PROTOCOL.md` §7.3, evaluated at request time (claims expire).
`conclusion` is the effective result's conclusion on the latest attempt.
`claims` counts every claim on the run: a run with more claims than attempts
saw a race (both executed, one result is effective — that is the rule, not a
bug).

`verified` requires a registered key for the actor **and** that the entry sits
in the actor's own inbox (the D1 inbox model). Merge rules come from
`refs/collab/meta/rules` when present, else defaults (nothing protected).

#### `GET /{owner}/{repo}/api/collab/threads/{id}`

One thread: parent-ordered entries with per-entry verification, plus the PR
view + merge evaluation when the thread has a `patch`. `404` for an unknown
thread id.

```json
{
  "id": "t1",
  "entries": [ { "oid": "…", "principal": "alice", "verified": true, "entry": { "kind": "issue", "actor": "alice", "ts": 1786500000, "parent": "", "refs": null, "body": {}, "sig": "ed25519:…" } } ],
  "pr": { "pr": { "id": "t1", "base": "refs/heads/main", "head": "refs/heads/topic", "status": "open", "reviews": [], "human_approvals": [], "unverified": [] }, "merge": { "allowed": true, "reason": "base is not protected", "satisfied_by": [] } }
}
```

#### `GET /{owner}/{repo}/api/collab/board`

The work-unit board (D1 §8): every thread projected under the declarative
column rules of `.walgit/board.toml` **at HEAD**. The board is a *pure
function of the collab refs and the definition* — `build_board` in
`walgit-wal::collab` — and there is no board state anywhere: moving a card is
an ordinary signed `status` entry (POST to `…/collab/entries` above) and the
columns re-derive. The SPA board page and `walgit collab board` render from
this same shape; the CLI's `--format json` writes these exact bytes, which is
what the cross-client acceptance test asserts.

Definition: absent → the built-in default (`open` by status, `merged`,
`closed`, `other` catch-all); **present but invalid → `400`** (fail closed —
a typo'd column rule must never silently fold cards into the wrong lane).

```json
{
  "columns": [
    {
      "name": "review",
      "cards": [
        {
          "id": "t1",
          "title": "add the thing",
          "actor": "alice",
          "status": "needs-review",
          "owner": "agent-mendel",
          "worktree": "prod-release",
          "branch": "feat/prod-release",
          "work": "review release preflight",
          "created_ts": 1786500000,
          "last_ts": 1786500123,
          "entries": 2,
          "verified": 2,
          "unverified": 0,
          "kinds": ["issue", "status"],
          "last_oid": "…",
          "merge": null
        }
      ]
    }
  ]
}
```

A card's `status` is decided in thread order and the last match wins:
`status` entries set their `body.status`, a `merge_result` with
`merged = true` sets `merged` (a later `status` entry still overrides it);
default `open`. `last_oid` — the parent-chain tip — is what a follow-up
entry chains to.
Column order is declaration order, **first matching column wins**, cards
matching no column are omitted, and within a column cards sort by the
definition's `sort` (default `last_ts` descending, card id as the total-order
tie-break, so the bytes are deterministic). Cards without a `patch` never
match a `merge = "allowed"|"blocked"` column (there is no verdict).

All three read endpoints answer from the synced WAL view (`Need::Objects`): entry
blobs are read locally when packs fit, faulted through the remote reader
otherwise (one batched fault, one `git cat-file --batch` — never one read or
one process per entry). The namespace is budgeted at 20 000 refs per request:
past it the answer is a `503` pointing at the `walgit collab` CLI, which
aggregates offline. SWR caching (stale-while-revalidate=60), never
immutable — collab state changes with every push.

#### `GET /{owner}/{repo}/api/collab/ci-artifacts/{sha256}`

The D1-CI §8.2 storage convention's HTTP read side (issue #161): a CI run's
full log and its declared artifacts live as plain git blobs at
`refs/collab/ci-artifacts/<actor>/<sha256>`, pushed through receive-pack by
the runner (the CLI side is `walgit ci log` / `walgit ci artifacts`; the SDK
side is `repo.ci.artifact(sha256)`). `200` → the exact bytes as
`application/octet-stream`, `Cache-Control: immutable` + ETag = the quoted
sha256 (content-addressed: the bytes behind an address never change). The
server verifies the payload against the address before serving. Candidates are
restricted to the `ci-artifacts` ref prefix and size-probed before any body
read, so an earlier same-sha ref cannot shadow the valid publisher and an
oversized object is refused without materialization. A ref whose blob does not
hash to its name answers `404`, as does an unknown address; a malformed address
(not 64 lowercase hex) is `400`; a blob over the 16 MiB convention cap is
`413`.

`HEAD` on the same route is the CLI's preflight. With `?actor=<principal>` it
is scoped to the exact publisher ref the CLI will fetch: `200` means the object
exists and fits the cap, `404` means that actor's object is absent, and `413`
means it exceeds the cap. The preflight never reads the blob body; remotes
without this HTTP metadata channel are refused before `git fetch`.

### `GET /{owner}/{repo}/api/overview` — optional, walgit-specific

Backs the "WAL" tab. Not needed by Code/Commits pages; a host without a
WAL should return `404` (the tab then shows the error text). Shape is in
`api.ts#Overview` / `overview.go`: `repo`, `clone_url`, `hostname`,
`health{status: ok|degraded|error, issues[], deep, suggestions[{op, params?, reason, auto?}]}` — `deep` is the
last connectivity audit as recorded in the store (`fsck.pb`, any maintainer), `auto` says how/when the
maintainer loop performs a suggestion by itself (absent = a human must) — `manifest{version,
next_seq, min_seq, segments[], tail_entries, entries, checkpoint?,
packset?, advertised_bundle_uri?, last_push?}`, `local{version, next_seq,
bootstrap, reconciled, size_bytes}`, `packs{live, live_bytes, pushes}`,
`bundles[{sha, size, at_seq, created, uri, strategy, kind, base_id, creation_token, filter, tips}]` (the chain:
`base_id` is the bundle whose tips are this one's prerequisites, `""` for a full), `bundle_plan{slots[{strategy,
kind, slot, status: built|missing|pending|blocked|unavailable|too-small|skipped|wrong-host, detail, bundle_id}],
upcoming[], maintainers[{host, disk, max_pack_bytes, last_pass_age_secs, alive, passes, last_unit}], orphaned}`,
`compactions[]`, `node{…counters}`. Arrays `[]` when empty.

## 5. Consistency and cost expectations

- **Every screen is O(1) requests** — exactly two: `resolve` (tiny, SWR +
  ETag, the only ref-dependent call) plus one **sha-addressed, immutable**
  endpoint — tree, blob, or commits page; commit detail is one immutable
  call. `refs` is one extra tiny call per repository visit; owner and repo
  lists are one request each. No per-row fetches: the tree response carries
  its latest commit and README, the commit response carries numstat and
  patch, the commits page carries its own `more` flag. The branch picker
  asks for one server-filtered page per keystroke, never the whole list.
- **Cost must not scale with ref count** anywhere on the hot path: `refs`
  is O(1), `resolve` is O(path segments), ref lists are O(page). The only
  O(refs) work allowed is a bounded scan inside one ref-list page, and
  even that should hide behind an LRU.
- A server should answer each endpoint in a handful of batched git
  invocations (tree = `ls-tree` + `log -1` + maybe `cat-file`; commit =
  `log -1` + `show`), never one process per entry — and should cache
  sha-addressed JSON in an LRU, since it can never go stale.
- Reads must be as fresh as a `git fetch` from the same host would be:
  after a push is acknowledged, the next API call (any node) reflects it.
- Writes on the JSON surface are admin only: `PUT|DELETE /{o}/{r}/api`,
  `PUT|DELETE …/policy`, `POST …/ops/{op}`. Content moves over git
  (`git-receive-pack`) and LFS, never through JSON.

## 6. Minimal conformance checklist

```
GET /api/v1                                     → 200 {version:1, base, browser_base=/api/v1, sdk, auth, endpoints}
GET /api/v1/me                                  → 200 {principal,write,anonymous} | 401; no-store
GET /api/v1/owners                              → 200 [..]   ([] when empty)
GET /api/v1/owners/nobody/repos                 → 200 []
GET /o/r/api                                    → 200 {owner,name,full_name,head,branches,tags,clone_url,html_url,api_url}; SWR + ETag "<head sha>"
GET /o/r/api-browser/refs                           → same handlers (browser lane)
OPTIONS /api/v1/… (Origin ∈ cors_origins)       → 204 + Access-Control-Allow-{Origin,Credentials,Methods,Headers}
GET /api/v1/… (Origin ∉ cors_origins)           → 200, no CORS headers; DELETE/PUT/POST → 403
GET /repos.js                                   → 200 text/javascript; ETag; no-cache; unauthenticated
GET /o/r/api/refs                               → 200 {head:{name,sha}|null}; ETag; If-None-Match → 304   (browser lane: /o/r/api-browser/refs)
GET /o/r/api/refs/branches?n=3                  → 200 {refs:[3],more}; SWR
GET /o/r/api/refs/branches?q=stab&after=1-x&n=2 → filtered, after cursor
GET /o/r/api/refs/tags   (Accept: text/event-stream) → event: ref … event: done
GET /o/r/api/resolve/feature/x/dir              → 200 {ref:"feature/x",sha,path:"dir",kind:"branch"}; SWR+ETag
GET /o/r/api/resolve/nope/x                     → 404 text/plain
GET /o/r/api/tree/<sha>                         → 200 {ref,sha,path:"",entries,commit?,readme?}; immutable
GET /o/r/api/tree/main                          → 200 same shape; SWR + ETag "<sha>"
GET /o/r/api/tree/<sha>/README.md               → 404 (blob, not tree)
GET /o/r/api/blob/<sha>/README.md               → 200 {contents}; immutable
GET /o/r/api/blob/<sha>/README.md?raw           → 200 text/plain; immutable
GET /o/r/api/commits?ref=<sha>&path=&skip=0     → 200 {ref,sha,commits,more}; immutable
GET /o/r/api/commit/<sha>                       → 200 {commit,stats,patch}; immutable
GET /o/r/api/commit/<sha[:8]>                   → 200; SWR + ETag "<full sha>"
GET /o/r/api/commit/deadbeef                    → 404 text/plain
POST /o/r/api/collab/entries                    → 200 {ref,oid,seq} | 403 (actor≠principal, or policy denial) | 401 (no credential)
POST /o/r/api/collab/principal                  → 200 {ref,oid,seq} | 403 (registering another principal, or policy denial)
GET /o/r/api/collab/report                      → 200 CollabReport; SWR | 503 (namespace past the 20k-ref budget)
GET /o/r/api/collab/board                       → 200 {columns[{name,cards}]}; SWR | 400 (HEAD `.walgit/board.toml` unparseable) | 503 (budget)
GET /o/r/api/collab/threads/t1                  → 200 {id,entries,pr?} | 404 unknown thread
GET /o/r/tree/main/anything                              → 200 index.html
GET /o/r/settings                                        → 200 index.html  (JSON is /o/r/api/settings)
```
