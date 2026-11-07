# walgit — decisions in force (D1–D70)

> Split out of `AGENTS.md` on 2026-11-05. **Append, never silently change:** a
> decision that no longer holds is superseded by a later entry that says so in
> its own text; entries are never deleted, reworded in place or renumbered.
> Decision identifiers are stable; gaps in the numbering are intentional.
>
> In this file, a bare `§N.M` reference is `docs/ARCHITECTURE.md` unless another
> document is named; `§N.M` after `docs/D1_PROTOCOL.md` / `web/SKILL.md` /
> `web/API.md` is that document's own numbering.

- **D1** Rust, tokio + axum, HTTP/2 (h2c or ALPN), streaming both ways, gzip request bodies.
- **D2** gix in-process where it is correct and measured; upstream `git` for delta-compressing repack/bitmaps,
  bundle creation, and normal upload-pack. **`git.upload_pack_engine = "auto"` selects stock git wherever it can
  read the packs** (local copies or mount-linked bases); gix handles only a remote-served base. **The gix engine
  never emits thin packs** (`upload_gix.rs`): gix-pack's thin-pack path builds the base pack's whole (offset → oid)
  table per chunk per thread (60 M entries × 44 threads ⇒ 178 GB RSS) and is the only place gix writes an object id
  into a pack (a mid-pack refresh once paired offsets with another pack's table ⇒ a wrong id). The gix pack source
  is a frozen snapshot (`frozen_pack_source`). Reproducer: `walgit-git/tests/upload_gix_scale.rs`.
- **D3** `ObjectStore` trait with CAS version tokens, conditional GET, range, compose; gcs/s3/memory backends.
  `compose` is native on GCS and a multipart `UploadPartCopy` on S3 (`compose_is_native` tells callers which);
  `accel_target` gives an edge a URL (+ bearer on GCS, presigned on S3) to fetch an object itself. Above
  `store.multipart_threshold` an S3/R2 `Create` PUT (packs and their side files) is a HEAD pre-check plus
  multipart — `CreateMultipartUpload` has no `If-None-Match`, so large-create atomicity is best effort; the keys
  are content-addressed, so racing writers upload the same bytes (`s3.rs` module docs).
- **D4** protobuf on the wire and in the bucket; schema versioned, append-only.
- **D5** Repo identity `<owner>/<repo>[.git]`, prefix `repos/<o>/<r>/`, creation = CAS create of the manifest.
- **D6** Manifest CAS is the only commit point. **D7** No node identity, no elections; leases for exclusivity.
- **D8** `walgit.toml` only (+ `WALGIT__` env overrides). **D9** One binary, roles by config (`serve`,
  `maintain`; `maintain` includes compaction and bundles).
- **D10** One static-serving code path for every immutable byte (ETag/304/If-Range/Range/416/HEAD/immutable;
  UI assets precompressed at build; store objects never compressed at request time).
- **D11** Too-large repos are served, not refused: remote reader for the web API; clones via bundle-uri; refs from
  the WAL. Object work returns 503 when remote objects are disabled or the repository is excluded from this host's
  serving placement (D30).
- **D12** Auth is `none` | `token` | `oidc` (§1.3). `oidc` is generic OpenID Connect through discovery; the
  walgit-issued access token (`wgt_…`, HMAC, stateless, `/_auth/tokens`) is the credential git uses, so no client
  needs a vendor CLI to mint tokens. An edge that wants to do auth itself uses `auth_request /_auth/check`
  (`deploy/nginx.conf.example`).
- **D13** Long work is a task and is narrated (§2.7); no endpoint may block silently.
- **D14** Web toolchain: pnpm + Vite only. The build fails unless the optimized SPA artefacts are present.
- **D15** Repo-scoped API lives at `/{o}/{r}/api/…` (refs, resolve, tree, blob, commits, commit, overview, ops,
  tasks, policy, settings) so the repository prefix determines placement. The browser lane is
  `/{o}/{r}/api-browser/…`; `/api/v1` is non-repository discovery, identity and owner listing. No aliases.
- **D16** Push authorization is a per-repo **rule language** at `repos/<o>/<r>/policy.json` (`docs/POLICY.md`).
  Envelope `version` + `groups` + `rules`; `protect` = AND. Empty/missing = anyone with write may move any ref.
- **D17** `bundles.require` refuses only **unbounded** zero-have fetches (no `deepen*`, no `filter`): that is a
  full clone and belongs to bundle-uri. Bounded zero-have fetches (CI's `--depth`/`--filter`) go to upload-pack.
  git never retries a failed bundle download and then falls back to a zero-have fetch; a principal that fetched
  the repo's `bundles/list` within the hour gets **one upload-pack full clone per 6 h** with a loud band-2
  WARNING; everyone else is refused with the truthful message.
- **D18** **History pack**: a tier-2 base is published with a derived `pack-objects --filter=blob:none` pack
  (commits + trees; `PackRef.kind = HISTORY`, `derived_from = <base>`) that every instance keeps as a real local
  pack; blob bytes are all that cross the mount / range reads. Superseded with its base.
- **D19** **The serving runtime is untouchable.** (1) control-plane store objects (manifest, log, checkpoints,
  leases, bundle list, policy, render cache) never share a transport or a permit with bulk bytes (packs, idx,
  side-files, bundles, LFS, ranged reads: their own pools under `store.gcs.bulk_concurrency`); (2) pack
  materialization never runs on the serving runtime (`sync::on_bulk_runtime`) and never queues as a writer on
  `RepoHandle::rw` — the refs phase needs only `sync_mutex`, pack removal is `try_write()` (a queued writer on a
  tokio RwLock blocks every new reader; one 24-minute clone once starved every info/refs for minutes).
- **D20** **One API, two lanes, one SDK**: `/{o}/{r}/api/…` accepts a bearer or the same-origin session;
  `/{o}/{r}/api-browser/…` is the cross-origin browser lane (`credentials: "include"`, CORS only for
  `server.cors_origins`, sign-in popup at `/api-browser/v1/authenticate`). The SDK **`repos.js`**
  (`web/sdk/repos.ts`; IIFE registers `window.repos`, ESM `repos.mjs`) maps the whole surface, picks the lane,
  does the popup dance and unwraps the SSE envelope; served at `/repos.js` (open, so a `<script>` tag can load it).
  **Dogfood rule**: the bundled UI is built on the SDK (`web/src/api.ts` is an adapter); fix the SDK, never fork.
- **D21** **No monthly bundle layer.** git's `creationToken` heuristic never walks past a full bundle; two layers
  of incrementals (daily, hourly) under one full (weekly) is the shape. Incrementals without `chain` list only
  their 2 newest per strategy (the second so a client that read the list a slot ago never 404s mid-chain);
  **`chain = true`** cuts a slot on the strategy's own previous bundle while that is newer than the newest base
  bundle — every slot since the base is wanted and kept, each exactly its delta. **Dailies are chained by
  default**; hourlies stay on the newest daily with 2 kept. `slots::base_for_incremental` is the one place that
  knows either rule.
- **D22** **Bundles are cut on calendar slots, main-only, as-of-slot, with a contiguous chain**
  (`docs/BUNDLE_URI_DESIGN.md` §3/§4): a 6-field UTC cron `schedule` whose fire time *is* the slot; missing full
  slots backfilled oldest-first (`backfill_max`); content is main as of the highest WAL seq with `created_at ≤
  slot`; `creationToken = slot epoch`; prerequisites = tips of the newest `base` bundle at or before the slot;
  retention = `keep` fulls + the chain under them; refs default to `HEAD` + `refs/heads/main` for
  `bundles.main_only` repos (+ `extra_refs`).
- **D23** **An edge is optional and load-bearing only for bytes.** An nginx in front may terminate TLS, cache one
  `auth_request` verdict per credential, route by `/<owner>/<repo>`, and offload static bytes by `X-Accel-Redirect`
  — only when it injects `X-Walgit-Capabilities: accel-redirect`; the app's answer supplies `X-Walgit-Store-Url` /
  `-Authorization` / `-Key` and the edge slices and caches. `deploy/nginx.conf.example` is the reference; nothing
  in `crates/` knows a hostname.
- **D24** **Per-repo settings live in the WAL.** `RepoSettings {toml, revision, author, updated_at, message}` — a
  TOML document restricted to `[bundles]`, `[maintenance]`, `[compaction]`, `[upstream]` (≤ 16 KiB) — is published
  as a `SETTINGS` log entry **and inline on `manifest.pb`** (every refs-level sync sees the effective config at no
  extra round trip). Effective config = host `walgit.toml` ⊕ env ⊕ settings (`Config::with_settings`). Validated at
  publish; invalid = 400, nothing published. Surface: `GET|PUT|DELETE /{o}/{r}/api/settings`, `…/effective`,
  `…/history`; `walgit repo settings show|set|clear|history`. Not in settings: auth, store, server, wal, cache,
  `upstream.token_env` (host-only). `GET …/effective` returns only those sections. PUT/DELETE of settings and of
  `policy.json` require an **admin** principal (`tokens[].admin`, or oidc `admin_emails`/`admin_domains`;
  `mode = none` is admin on loopback). Write is push, not admin.
- **D25** **Two cache modes.** `cache.mode = "budget" | "disk" | "auto"` (auto = disk when `maintenance.disk =
  "ssd"`); in **disk** mode `cache_budget_bytes()` is 0 = unlimited — every repo fully local, never the
  too-large/remote/link path, eviction only under disk pressure (`cache.disk_high_watermark`, idle-oldest first).
- **D26** **Routing is by repo prefix, nothing else.** Everything whose path starts with `/<owner>/<repo>` (git
  smart HTTP, `.git` suffix, bundles, LFS, `/{o}/{r}/api*`, `/{o}/{r}/settings`, the SPA pages) is one routing
  unit; an edge maps `^/<o>/<r>[./?]` → a host. "Which machine serves a repo" is decidable from the first path
  segments alone (the `Server` header shows it). **D27** Lanes are a segment *after* the repo prefix
  (`/api`, `/api-browser`); the prefix is still the only routing key.
- **D28** **The host that maintains a repo is its writer.** Forwarding receive-pack to a broker
  (`wal.push_broker_url` + `push_broker_token`) is for hosts that maintain nothing. A `maintain` host refuses a push
  for a repo it does not `assigned()` (band-2 line + `ng`). Publish is CAS-safe, so two writer hosts for two
  disjoint repo sets is correct by construction; the broker is an optimisation.
- **D29** **An edge's fallback for a placed repo is read-only and refs-level.** GETs (info/refs, API, UI, bundle
  list, bundles) may fall back to another host; git RPC and LFS writes have no fallback and get **503 +
  `Retry-After: 15`** — a replayed POST either hangs or warms the wrong host while the client sees nothing.
- **D30** **Placement is configured, not inferred.** `[placement] serve / serve_exclude / maintain /
  maintain_exclude` (globs). A host that does not **serve** a repo answers its object work with **503 +
  `Retry-After: 15`** and a pkt-line `ERR walgit: <repo> is served by <host>; retry shortly` *before* any sync;
  refs-level reads stay available everywhere. **maintain** gates the maintainer loop.
  Test: `host_excluded_from_serving_a_repo_refuses_object_work_with_503`.
- **D31** **Draining must not stop serving.** *Phase 1* (SIGTERM): the maintenance loop starts no new unit and
  the running unit is **interrupted at once** (D22 redoes it; a unit too expensive to redo is made resumable —
  `BaseRebuild`) while the instance serves everything normally, `/readyz` 200; bounded 30 s. *Phase 2*:
  `/readyz` 503 + Retry-After, new fetch/push/LFS refused with 503 before any work, in-flight requests get
  `server.drain_timeout`, exit. Test `tests/drain.rs`.
- **D32** *(Superseded 2026-09-16 by D46: the events bridge, its `events` role, `[events]` config and `docs/EVENTS.md` were removed. The text below is kept as history.)*
  **Events are produced from the WAL by one small service, never by the push path** (`docs/EVENTS.md`).
  The **events bridge** (`roles=["events"]`) tails each repo's log from a durable per-repo cursor
  (`events/cursor.json`), converts committed PUSH/REF_UPDATE entries to `ref` events, POSTs each batch to
  `events.webhook_url` (JSON array; `X-Walgit-Delivery` = sha1 of the body; `X-Walgit-Signature: sha256=<HMAC>`
  when `webhook_secret` is set), advances the cursor: published iff durable, a crash loses nothing, lag =
  `head_seq − cursor`. Writers and the WAL crate contain no event code. Wake-ups: `POST /_events/notify` from a
  bucket notification (at-least-once) + a periodic sweep as backstop. Dedup key `(repo, seq, ref_name)`.
- **D33** **A repository's refs can follow an upstream git host, through the WAL, by its maintainer.**
  `[upstream] follow = ["refs/heads/main"]` (needs `upstream.git`; `upstream.token_env` names the env var with the
  token) makes the maintaining host fetch the delta every `maintenance.follow_interval` and publish it as an
  ordinary PUSH entry (`principal = upstream`), fast-forward only; policy is not evaluated (follow is
  configuration). Its own loop, never a unit of the priority loop. `walgit mirror` is the same operation from a
  host without the repo local, pushing over HTTP.
- **D39** **walgit is a standalone program; any deployment is packaging.** `walgit-server --config x.toml` (a thin
  bin = `walgit serve`; `walgit` with no subcommand serves too) works on one machine against one bucket with
  nothing in front of it. (1) **TLS in-process** — `[server.tls] mode = off | self_signed | files`; self-signed
  certs are generated once under `<cache.dir>/tls/`, published at `/services/public/ca.pem`, pinned for git by the
  installer. (2) **Everything an upstream might take over is announced by the upstream, per request, in
  `X-Walgit-Capabilities`** — never assumed. (3) `cache.dir` defaults to `temp/walgit` (`/tmp` honouring TMPDIR,
  `%TEMP%` elsewhere). (4) A missing `--config` file is fatal (exit 2); `--config /dev/null` is the explicit
  defaults+env form (`NUL` plays that role on Windows). (5) The credential helper and
  token file are host-derived (`<host>-credential-helper`, `<host>-token`) so two walgit hosts coexist on one machine.
- **D41** **Bundles: chained dailies, a clone list and a catch-up list, the chain through the weekly.** Dailies
  are cut on their predecessor (`chain = true`, default), hourlies on the newest daily (2 kept); at the tie between
  Sunday's daily and the weekly the chain continues through its own link, so Monday's daily has the weekly's tips
  as prerequisites without being cut on it; retention keeps the chain under every kept full. `bundles/list` (fulls
  + chain) is for clones, **`bundles/catchup`** (no fulls) is what recipes record in `fetch.bundleURI`. Measured: a
  catch-up is exactly the slots missed; for a client fetching several times a day, upload-pack's thin pack is
  smaller than an hourly bundle — bundles pay off for fresh clones and far-behind clients.

- **D42** **The host-wide repo index is the page route `/repos`, and the AI-agent guide is `/SKILL.md`
  (2026-09-03, #59/#60).** The top-nav「仓库」entry lands on `/repos`; `/` stays the landing page (it does not
  repeat the repo index). Data comes from the existing `/api/v1/owners*` surface — no new endpoints.
  `/SKILL.md` serves the repository source `web/SKILL.md` verbatim (data-free, open like `/repos.js`, strong
  ETag): the agent-facing guide to git/API/auth on this host. Both are single-segment static routes, so each
  reserves its owner name for the SPA exactly as `/api` reserves "api" (the `routing_prefix` allow-list
  records them); git operations for such owners are unaffected — only the single-segment owner page is
  shadowed.

- **D43** **The first-run setup wizard (2026-09-06, #70).** A `memory` store without
  `[store] memory_backend_intentional = true` means "unconfigured" (the installer's initial config ships exactly
  that shape): the instance enters **setup state** — loopback-listen only (refused at startup otherwise,
  fail-closed like §1.3), serving just `/setup` (a client route of the SPA shell), its `/_ui/*` assets,
  `/api/v1/setup/*` (data-free, open like `/_auth/*`; answers 404 once configured), `/healthz`/`/readyz` and a
  `/` → `/setup` redirect; everything else is 503 with a pointer. The wizard (① S3/R2 or GCS + bucket, ② test
  connection — a HEAD probe of the bucket, ③ first admin token) writes back into the running `walgit.toml`
  through `toml_edit` — comments and unrelated structure survive — after validating the composed file as a
  whole. S3 credentials persist as literals (`[store.s3] access_key`/`secret_key`, take precedence over the
  `*_env` names; 0600 the file); GCS rides ADC. After save the CLI serve path exits 75 and the tray's watcher
  (its supervisor) respawns — bounded (5 consecutive 75-exits stop the loop); without a supervisor the wizard
  says so. `InstanceInfo.store_backend` (`memory` | `s3` | `gcs`) is the shared runtime-warning judgement for
  #73. The bundled SPA page lives outside the authed `Layout`.

- **D44** **The storage editor: configured-state admin surface (2026-09-07, #127).**
  D43's wizard owns only the unconfigured state; once configured, `GET|PUT
  /api/v1/store` and `POST /api/v1/store/test` (admin, §1.3/D24's challenge
  shape; 404 in setup state) are the same write mechanism hung a second time:
  compose + validate before touching the file, `toml_edit` write-back
  (comments survive), exit-75 supervisor handoff. Credentials never travel on
  the read side (`has_access_key`/`has_secret_key` presence bits), so a blank
  credential field on this surface means **keep**, where the wizard's blank
  meant omit (`BlankCreds::Keep`/`Omit`); the edit-state test overlays the
  submission on the *running* config, never `Config::default()`. `/api/v1/me`
  carries `admin` so the SPA can gate its entry; `/setup` renders the double
  face. One mechanism, two mounts (`crates/walgit-server/src/store_settings.rs`).
  Hardened by #129: both saves write the file **atomically** (same-directory
  temp + rename, 0600 — `walgit.toml` is the literal credentials' only
  carrier and a torn write would strand the instance); `can_save` requires
  the `--config` path to be a **regular file** (`/dev/null`/`NUL`/fifo
  accept writes and discard them — refused like a missing file); and a save
  whose file holds no literal credential while the environment supplies one
  answers `warnings: [...]` instead of a silent "kept". Concurrent PUTs are
  last-writer-wins (admin-only surface, tiny file).

- **D45** **Collab entries fold into a signed snapshot; every aggregation
  reads snapshot ∪ inbox tail (2026-09-10, #160).** The append-only
  `refs/collab/inbox/*` namespace hits two walls as it grows: the 20k-ref
  per-request aggregation budget (`collab_load`) and the clone/fetch ref
  advertisement size. The fold is the WAL checkpoint's shape applied to the
  collab namespace (normative: `docs/D1_PROTOCOL.md` §9): one
  CAS-moved ref `refs/collab/meta/snapshot` points at a signed JSON blob
  that carries every folded entry verbatim (`oid` + inbox `principal` + raw
  signed bytes — the digest manifest that keeps the per-entry signature
  chain verifiable after pruning; the oid is recomputable from the bytes).
  The fold unit is the CLI (`walgit collab gc`), never the maintainer —
  the folder must hold a collab principal and signing key, and the write
  goes through receive-pack like any other: snapshot ref lands first as a
  **CAS from the baseline the gc actually read** (`--force-with-lease=refs/collab/meta/snapshot:<baseline>`;
  no snapshot means an empty `<baseline>`, i.e. `refs/collab/meta/snapshot:`, not a zero oid —
  never a `+` refspec, which would silently short-circuit the lease; a
  concurrent fold that moved the ref fails the push and the gc retries),
  even a prune-only fold revalidates that baseline before deleting, and the
  pruned inbox refs are deleted after, in batches with per-ref OID leases.
  Record integrity is checked in the repository's object format (SHA-1 or
  SHA-256). Readers dedup by oid, so aggregation
  before and after a fold is byte-identical — that equality is the
  acceptance test. Server and CLI parse the snapshot through the same
  `walgit-wal::collab` code; the server rejects a snapshot over 64 MiB before
  materializing its body on both local and remote paths, and counts only the
  unfolded tail against the 20k budget.

- **D46** **The events bridge is removed; refs-level watching is the only way to observe the WAL
  (2026-09-16).** D32's bridge is deleted in full: the `events` role, the `[events]` section
  (`webhook_url` / `webhook_secret` / `sweep_interval`), the per-repo cursor `events/cursor.json`,
  the `POST /_events/notify` wake route and its sweeper, `docs/EVENTS.md`, `deploy/events/*`, the
  `walgit-ci` wake listener (`ci_wake.rs`, `walgit ci run --listen` / `--webhook-secret`), and the
  `--test events` suite. Nothing took its place in-process: the server no longer pushes ref events
  anywhere. Consumers observe the facts they need directly and cheaply — refs-level polling
  (`git ls-remote`, O(1), no pack), `walgit collab watch`, or the SSE envelope the web API streams
  to any endpoint that accepts `text/event-stream`. Every one of them revalidates against the WAL,
  so correctness still depends on the facts, never on a push: the WAL remains the sole event source
  (principle III), and this only removes one *consumer* that happened to ship inside the same
  binary. Breaking, pre-1.0 ("no backwards compatibility"): a `walgit.toml` still carrying `[events]`
  or `roles = ["events"]` is now a hard parse error (`deny_unknown_fields` / unknown enum variant) —
  delete the section and the role. Supersedes D32.

- **D47** **The ops skill ships with the binary and installs from the public lane (2026-09-17,
  cc-ai-ship-skill).** The agent *operator* guide (`skills/walgit/SKILL.md`) is compiled into the
  server (`include_str!`) and served on the data-free lane: `GET /services/public/skill/SKILL.md`
  verbatim (strong ETag), `…/manifest.json` (`name` / `version` == `instance::build_version()` /
  `sha256` / `bytes` / URLs) and `…/install.sh` — a POSIX-sh, idempotent one-liner with the sha256
  and base URL baked in, writing `${WALGIT_SKILL_DIR:-$HOME/.agents/skills/walgit}/SKILL.md` and
  re-verifying on every run. `web/SKILL.md` (D42, the *consumer* guide, still at `/SKILL.md`) gains
  the install one-liner, so an agent reads one public document and can pull the matching operator
  skill. The skill is versioned with the build, not the bucket: re-run the installer after a host
  upgrade to refresh, and it is a no-op when the bytes already match. Still data-free — nothing
  credential-bearing or repo-scoped may join the lane.

- **D48** **On Windows the Task Scheduler owns the server process; nothing else may (2026-09-17,
  cc-ai-win-task-service).** `walgit service start|stop|status` drives one named task
  (`schtasks /Create /XML /TN walgit`, `MultipleInstancesPolicy=IgnoreNew`,
  `ExecutionTimeLimit=PT0S`, `LogonType=InteractiveToken`) instead of spawning a detached child and
  writing *its* pid. The pidfile is a macOS/Linux mechanism only: it carries no meaning once a
  supervisor can hand out a pid it does not own, and a stale one must never be read as "not
  running" (the CLI now probes `/healthz` even on the stale branch and says so instead). `start`
  asks `/healthz` before it does anything, and `status` reports the version the port *answers*
  with next to this binary's build — "already running" is not evidence that the *right* build is
  running. `stop` kills the **port's owner** first and re-checks the port, and only then `/End`s
  the task and confirms it is no longer `Running`: measured on the CI runner, `/End` alone ends the
  `cmd /c` wrapper but not the `walgit.exe` child holding the socket, and a task left `Running`
  would make the next `start` a silent no-op under `IgnoreNew`. A listener is killed only when its
  image is exactly `walgit.exe` or `walgit-server.exe` — a prefix match would take
  `walgit-backup.exe`, and a row counts as ours only when its **local address** matches the
  configured `listen` (same address, the `::1` twin of a loopback bind, or a wildcard): a socket on
  another address of the same port number is somebody else's, and reading it as "the owner of our
  port" made `stop` refuse to stop a healthy server (cc-ai-win-port-owner-scope, 2026-09-19). A
  matching `netstat` row whose pid cannot be read is an **error**, never "the port is free". The task
  action is `walgit-service-host.exe -EncodedCommand <base64>`: a **GUI-subsystem** launcher
  (`crates/walgit-cli/src/bin/walgit-service-host.rs`) that runs `cmd /d /s /c "<exe>" serve
  --config <cfg> >> <log> 2>&1` with `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP` and waits. A
  scheduled `Exec` action is *always* given a console, and with Windows Terminal as the default
  terminal that console becomes an on-screen window (or a taskbar button that restores one);
  `powershell -WindowStyle Hidden` only minimised it, and `DETACHED_PROCESS` is wrong too — with no
  console to inherit, the console-subsystem server allocates a fresh one and the window is back
  (cc-ai-win-service-console, 2026-09-19: measured on a real machine). The redirect
  stays `cmd`'s job — a redirect, not a supervisor, because the scheduler gives an `Exec` action no
  stdout to log to — and the definition records **absolute** paths, because the scheduler starts it
  in `WorkingDirectory` rather than the caller's. The
  Windows tray stops spawning and supervising anything: it calls `walgit service`, exactly like
  macOS (Linux keeps its tray-side supervisor — no equivalent scheduler there). The installer ends
  the task, sweeps the *port owner* (not a pidfile) plus both program directories before it
  replaces files, and deletes the task on uninstall; the windows CI leg type-checks `installer.iss`
  against dummy payloads and runs a real start-twice-stop smoke, so neither half waits for a
  release tag to be exercised.

- **D49** *(Superseded 2026-10-30 by D64: the host serves MCP itself at `/mcp` and `walgit mcp` is
  deleted. What carries over unchanged: the surface is read-only by default, the one/two writing
  tools need an explicit opt-in and a key, and destructive operations are advertised nowhere.)*
  **MCP is a client-side adapter (`walgit mcp`), never a server endpoint (2026-09-18,
  cc-ai-mcp-client).** An agent host spawns it over stdio; it speaks newline-delimited JSON-RPC 2.0
  (`initialize` / `tools/list` / `tools/call` / `ping`) and its tools **are** the CLI — every call
  runs this same binary's subcommand, so there is exactly one implementation and no second contract
  to drift from `web/API.md`, the CLI or the SDK. The default surface is **read-only**; the one
  writing tool (`collab_entry`) is registered only under `--allow-write` and needs `--key`.
  Destructive operations (`gc`/`reclaim`, `compact`, `import`, per-repo settings/policy writes, ref
  deletion, force-push) appear in **neither** the tool table nor the dispatch match: an agent must
  not be able to start a bucket-GC pass by naming a tool, and `tools/call` cannot reach what
  `tools/list` hides. Bytes never travel over MCP — clone/fetch/push stay git + bundle-uri.
  *(Amended 2026-10-20, design-card-claim: a second writing tool, `collab_claim`, joins
  `collab_entry` under `--allow-write --key`. It is `walgit collab claim` — read → publish →
  re-read — so an MCP-only agent can take part in the card-claim convergence
  (`docs/D1_PROTOCOL.md` §8.4) in one call. Still append-only and non-destructive; the excluded
  destructive list is unchanged.)*
  *(Amended 2026-10-27: both writing tools refuse an `actor` the process `--key` is not registered
  under. The adapter passes `actor` and `--key` through independently, and a mismatched pair
  produces an entry that does not verify until that actor is rotated to the signing key; the
  append-only log cannot be edited, so the entry cannot be deleted or repaired in place (observed:
  worker actors signed with a coordinator key). The guard sits in the CLI write
  path (`ensure_actor_key_binding`), so `collab entry`/`collab claim`/`ci run` and every other
  caller get it too, not just MCP.)*

- **D50** **The blob viewer embeds by type; the PDF case is an *accepted* risk (2026-09-18,
  cc-ai-blob-viewers).** `?raw` is the byte channel — content type by extension, a single `Range` →
  206, `X-Content-Type-Options: nosniff`, `Content-Encoding: identity`, 32 MiB cap → 413, strong
  hashed ETag (immutable included) — and the four types a browser can treat as a *program*
  (`text/html`, `application/xhtml+xml`, `image/svg+xml`, `application/xml`) additionally carry
  `Content-Security-Policy: sandbox; default-src 'none'; …`, with HTML framed at `sandbox=""`. The
  viewer embeds images (`<img>`), media (`<video>`/`<audio>`), HTML in that sandboxed frame, and
  **PDF in `<object type="application/pdf">` with no CSP**. The PDF choice is deliberate and is an
  accepted risk, not a verified property: `sandbox=""` also disables the browser's own PDF viewer,
  and the built-in viewers are expected not to execute embedded PDF JavaScript or to expose the
  embedding document — but that expectation has no browser matrix behind it yet. The follow-up is
  that matrix (or download-only); until then this decision, not a claim of proof, is what the code
  rests on. A PDF stays otherwise inert on our origin: `application/pdf` + `nosniff`, and it is not
  in the active-content set for the CSP clause.
- **D51** **Windows tray upgrades run a copied helper, not an in-place self-update (2026-09-19,
  cc-ai-win-tray-upgrade).** An installed Windows tray detects updates only when it runs from the
  installer directory (`%LOCALAPPDATA%\Programs\walgit`, with `%USERPROFILE%\walgit` recognized for
  the installer's legacy migration, or the `.walgit-install` marker written by the installer for a
  custom `{app}`) and compares `walgit.exe --version` with GitHub latest. The only
  accepted asset is `walgit-setup-<version>-x64.exe`, selected by exact release version and exact
  GitHub `sha256` digest; no digest means no install. The tray downloads both the new and the current
  release's installer, copies `walgit-upgrade-helper.exe` into `%USERPROFILE%\.walgit\update\<pid>`
  so the running image is outside the directory Inno Setup replaces, then exits. The helper waits for
  the tray PID, runs `walgit service stop` (D48), invokes the installer with
  `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART`, checks the installed `walgit.exe --version`, starts the
  service, and requires `/healthz` to report the target version. Any failure runs the old installer,
  verifies the old version and health, and relaunches the old tray; the sequence and rollback are
  exercised with fake installers in `tray-rs` tests.

- **D52** *(Superseded 2026-10-30 by D64 for the MCP lane's shape: the transport and the poller now
  live in the server, so "adapter-side" became "server-side". The substance below is unchanged and
  still binding — subscriptions poll, they are best-effort and per-instance, and they are never a
  durable push channel.)*
  **MCP resource subscriptions are adapter-side polling, never a server event surface
  (2026-09-19, cc-ai-mcp-subscribe).** `walgit mcp` adds
  `resources/list` / `resources/read` / `resources/subscribe` / `resources/unsubscribe` for
  `walgit://refs/…`, `walgit://wal/…?from=…`, `walgit://collab/board/…` and
  `walgit://collab/thread/…`. A subscription runs a bounded, cancellable, per-process poller over
  the existing pull primitives: refs use a streaming `git ls-remote --refs` digest, WAL uses the
  refs-only manifest head seq, and collab resources share one session-level ref-level puller for
  `refs/collab/*`; board/thread versions are recomputed only after that shared ref signal changes.
  *(Amended 2026-10-20, design-card-claim: the **board** version is also recomputed once it is 60 s
  old — the board projection is a function of the wall clock too, because a `card_claim` TTL expiring
  moves no ref (`docs/D1_PROTOCOL.md` §8.4). Everything else in this decision stands.)*
  The poller computes only a stable version probe; `resources/read` remains the client's opt-in
  content read, and refs reads return a bounded digest sample rather than the whole ref list. A version change
  emits `notifications/resources/updated` (URI only), disappearance emits
  `notifications/resources/list_changed`, and repeated failures back off then cancel with a logging
  notification. This is `per-instance` and `best-effort`, matching the SSE/pull-lane semantics of
  D46: durable consumers still keep their own cursor. No server HTTP/SSE endpoint, no bridge,
  no new write path.
- **D53** **A Windows scheduled task must not launch a console program (2026-09-21,
  win-sched-task-console).** The Task Scheduler's `Exec` action always creates a console for a
  console-subsystem action; in an interactive session Windows Terminal shows it (or leaves a
  restorable taskbar button) before `-WindowStyle Hidden` can hide it — measured 2026-09-20: a
  60-second mirror task flashed a window on every run. Every periodic task (mirror, sync,
  inspection) launches a **GUI-subsystem launcher** instead, which spawns the real command with
  `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP` and waits for it; `DETACHED_PROCESS` is not used
  (with no console to inherit, a console-subsystem child allocates a fresh one — a new window under
  Windows Terminal);
  *(the GitHub mirror is no longer one of those periodic tasks — it is on-demand, D57; the rule
  still governs any scheduler that remains)*
  `walgit-service-host.exe` (`crates/walgit-cli/src/bin/walgit-service-host.rs`) is the reference
  implementation, `deploy/windows/README.md` carries the copy-paste registration template, and
  `deploy/windows/task-action-check.ps1` is the post-registration assertion (PE subsystem == GUI,
  no name allowlist). `powershell -WindowStyle Hidden` is not acceptable.
- **D54** **Agent names are agent-side bookkeeping; walgit manages only keys (2026-09-24,
  agent-names-agent-side).** The D1 registry stores exactly one fact per principal — the
  `principal → public key` binding (`refs/collab/meta/principals/<principal>`, host-level
  `host/principals/<principal>`; `docs/D1_PROTOCOL.md` §4.3/§4.4) — and signature verification
  checks only that binding. walgit maintains no roster of who *should* be on a team and does
  not gate principal names: the team list (who is in, what they are called) is the agents'
  own bookkeeping, formed by one self-registration each; roster changes are new registrations
  or revocations by the agents, never edits to a central list. Private keys stay agent-side
  at `~/.walgit/keys/<principal>.ed25519` (`0600`); the registry carries only public keys.
  *(Key location amended by D58/D59: project-local and per-worktree at
  `<git-dir>/walgit/keys/<principal>.ed25519`; the binding rule above stands.)*
  Consumer-facing statement in `web/SKILL.md` §0/§0a (the automatic first-contact routine)
  and `skills/walgit/SKILL.md` §5.
- **D55** **There is no wait / long-poll lane; refs-level polling and adapter-side subscriptions are
  the observation surface (2026-09-30, cc-ai-no-longpoll-decision).** A held "wait until `head_seq`
  or a `refs/collab/*` ref changes, else timeout" request was considered as the middle rung between
  push and fixed-interval polling, and **rejected**. It cannot buy correctness — the WAL is the only
  event source and nothing is pushed (D46) — and it buys latency only: the wait's own probe *is* a
  conditional GET of `manifest.pb` at 15–18 ms (`docs/ROUNDTRIPS.md` §1), `collab watch --interval`
  already runs at 1 s (its practical floor — lower values only spin harder), and the dominant latency
  of an unattended loop is the hook → worker → agent chain, not the refs check. It is also hostile to
  the deployment assumptions: a held request is exactly the between-requests window §1.1 says may be
  CPU-throttled on serverless platforms, and because instances are shared-nothing a waiter on
  instance A cannot be woken by a push published on instance B — the waiter would degenerate into
  that server polling the bucket on the client's behalf, i.e. the same round trips moved, not removed.
  The sanctioned lanes stay refs-level polling (`walgit collab watch` — at-least-once with the
  client's own cursor, *given a supervisor that restarts it or `--once` under a timer*, `web/SKILL.md`
  §0c; `git ls-remote` tips; `walgit wal ls` for the retained log), MCP resource
  subscriptions (adapter-side polling, best-effort, D52) and the SSE envelope as live narration for
  a request's *own* work (never a durable subscription). Do not re-open this as a missing feature:
  its absence is the decision.
- **D56** **Windows installs the detected release unattended; macOS and source checkouts still need
  the click (2026-09-30, cc-ai-win-auto-upgrade).** D51's Windows flow is unchanged in *what* it
  installs — the exact `walgit-setup-<version>-x64.exe` selected by release version plus GitHub
  `sha256` digest, the copied helper, silent install, `walgit.exe --version` + `/healthz` checks,
  rollback on any failure — and changed in *who starts it*: a release-managed Windows install runs
  the same pipeline **without a menu click**, 90 s after detection (long enough that a logon-time
  detection cannot pull the tray out from under the user), then exits so the helper can replace it.
  macOS keeps the click (its swap replaces the App Bundle) and a source checkout keeps the click (it
  would rebuild). `WALGIT_AUTO_UPGRADE=0` pins one machine to manual; `=1` is the test/CI seam.
  **The unattended path is fenced against retry loops:** when the helper rolls back it records the
  target version in `<state>/update/auto-upgrade-failed`, and the tray never auto-installs a fenced
  version again (the menu still can) — without that, "install → rollback → the helper relaunches the
  old tray → it detects the same release" would repeat forever. Consumer-facing statement in
  `web/SKILL.md` (Host upgrades), `skills/walgit/SKILL.md` and `deploy/tray/README.md`.
- **D57** **Release refs reach GitHub by hand; the mirror is never a resident loop (2026-09-30,
  cc-ai-mirror-on-demand, amended by cc-ai-release-push-github).** The release recipe has one home — "Where
  this repository lives" above. This entry is the incident that made it worth stating: a launchd job
  (`com.sqb.walgit-sync-github`, `RunAtLoad` + `KeepAlive`) mirrored continuously from 2026-09-29, and on
  2026-09-30 two releases reached GitHub without anyone deciding to publish them; the job was booted out and
  its plist moved to `~/.walgit/removed-launchagents/` (kept, not deleted). The 2026-09-23 rule (f7635f9) had
  already said the mirror is an on-demand command, not a loop, and it was violated in practice — so: push
  `main` by hand whenever it moves (`git push github main`; the reviewer of this very change caught that
  main had been pushed three times in ten minutes) and the tag at release, and never install the mirror
  under a scheduler.
- **D58** **Collaboration keys are project-local; walgit stores only the principal→key binding
  (2026-10-10, cc-ai-project-identity).** A collaborator's private key belongs to the project, not
  to a host-wide pile next to the deployment: `walgit collab join --principal <p>` writes the
  Ed25519 seed at `<git-common-dir>/walgit/keys/<p>.ed25519` (`0600`), the identity pointer at
  `<git-common-dir>/walgit/identity`, and registers `refs/collab/meta/principals/<p>`. The git
  common dir is shared by every worktree of the clone, is never tracked, and is never removed by
  `git clean -fdx`; one clone therefore carries one project identity (separate clones, or an
  explicit `--principal`/`--key`, for a second one). `collab entry` / `principal-register` / `gc`
  resolve `--principal`/`--actor` and `--key` from that identity, so a project only ever references
  its own. Each collaborator keeps only its own key: never pile several principals' keys in one
  directory — the old `~/.walgit/keys/` convention and the `web/SKILL.md` example that had one
  operator mint the whole team's keys there are removed. walgit does not constrain the key path
  (`--key` still takes any file), and the host-wide registry of `docs/D1_PROTOCOL.md` §4.4 is out of
  scope here. Normative statement in `docs/D1_PROTOCOL.md` §4.1.
  *(Amended by D59 (2026-10-10): the identity is **per worktree**, not per clone — a clone's main
  checkout and each linked worktree carry their own `<git-dir>/walgit/` identity, which is what
  lets one project run several background sub-agents with different principals. The rest of D58
  stands.)*
- **D59** **One console per project; the human assigns and observes, sub-agents implement and
  accept, clean work merges itself (2026-10-10, cc-ai-autonomous-collab).** Parallel work is not
  N human-driven consoles: it is **one human console per project** plus background sub-agents the
  console's orchestrator spawns. The human does two ends only — files the task card(s) (an `issue`
  with one owner and machine-checkable acceptance; batch units as a checklist) and observes the
  board — and is pulled in only by a `needs-human` card. Everything between is automated: the
  **orchestrator** (a client-side loop, e.g. driven by `walgit collab watch`; never a server
  endpoint, no new durable state — D46/D49/D55) dispatches each unowned card to a **worker
  sub-agent** in **its own worktree with its own identity** (identity is per worktree, D58
  amended), and, on `needs-review`, to an **acceptance sub-agent** with a **different principal**
  that independently judges the diff against the card's machine-checkable acceptance and posts a
  `review` with severity-tagged findings. **No Critical/Important finding and all acceptance green
  ⇒ the orchestrator merges itself** (local merge + push + one `merge_result` + `status: closed`):
  the gate is the distinct non-author verified `approve` that `merge_rule_eval` already enforces,
  so no human click is needed. Blockers route back to the worker (new `status: in-progress`) or a
  fixer sub-agent; genuine ambiguity/authorization/external input goes to `needs-human`; Minor does
  not block. Sub-agents are short-lived background processes started with the host agent's own
  capability (walgit does not prescribe the runtime), supervised by the
  orchestrator (launch / timeout / budget / restart / log; the worktree-tenure cleanup of the
  original text is superseded by D62 — worktrees are never removed); one card, one
  owner, one worktree, never a shared checkout or key. Durable truth stays in `refs/collab/*`;
  process state is local and disposable. Normative statement in `web/SKILL.md` §0d (and §0/§0b)
  and `skills/walgit/SKILL.md` §5b.
  *(Amended 2026-10-30: **there are two roles, leader and worker** — the dedicated acceptance
  sub-agent of the original text is folded into the leader. The leader reviews the worker's
  patch itself: it is by construction a different principal than the worker (per-worktree
  identity, D58/D62), so `merge_rule_eval`'s gate — a verified non-author `approve` — is
  unchanged and no human click is added. A leader still does not review a commit it wrote: that
  one card's review goes to another worker's principal, or the work is filed so a second
  principal exists. The UI's role-prompt block (`web/src/pages/Home.tsx`) and both SKILL.md
  files carry exactly these two prompts; the reviewer/tester/devops prompts are gone.)*
- **D60** **The bundled UI can carry an operator-supplied admin token in the bearer lane
  (2026-10-10, cc-ai-token-mode-store-ui).** A `token`-mode instance has no browser sign-in
  (only oidc mints a session cookie — `auth.rs`), but the first-run wizard *produces* token
  mode, so the admin storage editor and every admin surface were unreachable from the UI:
  `GET /api/v1/store` answered 401, `/setup` fell back to its "already configured" card, and
  the top-bar「存储配置」entry was hidden behind `me.admin`. That is the reported failure — an
  operator whose bucket credential was rotated had no UI to fix it and hand-edited
  `walgit.toml`. Fix: the top-bar entry is always shown; `/setup`'s configured face asks for an
  admin credential when the store read is 401/403, and the SPA carries it in the **bearer**
  lane (`Authorization: Bearer`, the credential git already uses) kept in `sessionStorage`
  (dies with the tab; never `localStorage`). No server auth path changes — `require_admin`
  already accepts bearer, and oidc keeps its session cookie and sign-in popup. Only `/setup`
  self-selects; no other page changes lanes.
- **D61** **The everyday CLI set ships in the bundled UI as `/commands` (2026-10-21,
  cc-ai-commands-page).** The agent-facing cheat sheet — first contact, reading the board,
  claiming a card and writing entries, repository reads, `collab watch` / MCP, CI, diagnostics —
  is a static SPA page at `/commands`, linked from the top nav as「命令」/「指令」, next to
  `/repos` and `/api`. It is data-free: a projection of the CLI surface (`walgit --help`), not
  of any bucket state, and every command block copies on click. Like `/repos` and `/api` the
  single-segment route shadows the owner page for an owner literally named `commands` (git
  operations are unaffected). The page carries no second contract: the normative text stays in
  `web/SKILL.md` §0a–§0e and `skills/walgit/SKILL.md` §5, and the page is updated with the CLI
  in the same change. `page_routes_serve_index_without_cache` covers `/commands`.

- **D62** **The worktree is the collaboration identity's home; the coordinator holds `main`
  and never removes a worker worktree (2026-10-23, cc-ai-worktree-tenure).** D58/D59 put each
  collaborator's key in its own worktree's git dir; this decision draws the consequence: the set
  of surviving worktrees **is** the team's physical roster, so worktree deletion is not cleanup —
  it is amputating an identity. The shape:
  (1) the **coordinator/leader** works in the main checkout, on `main` — it decomposes the goal
  into cards, reviews (itself, or through the acceptance sub-agent — either way a principal other
  than the patch author), merges worker branches, and records `merge_result`;
  (2) each **worker** owns one worktree and creates its **dev branch inside it** per card
  (`git worktree add .worktrees/<unit> -b feat/<thread> <base>`), claims before work, and never
  works in a shared checkout;
  (3) the coordinator **never deletes a worker worktree** — not after a merge, not after a
  crash, not to "tidy up". A merged **branch** may be pruned once the card is closed; the
  worktree outlives the card. Retiring a principal is `walgit collab principal-revoke` (a
  signed registry act), never `git worktree remove`. The pre-D62 texts that told the
  orchestrator to "remove the worktree/branch after the card is closed" (web/SKILL.md §0d/§0e,
  skills/walgit/SKILL.md §5b) are corrected in place; D59's "worktree cleanup" is superseded.
  *(Amended 2026-10-23, same decision: **a worker reports every problem to its card** — an
  error, a red test, a missing dependency, an ambiguity — as a signed `comment` with the
  evidence, or `status: blocked` when it halts the work. The card is the worker's only report
  channel: never the console alone, never a side chat.)*

- **D63** **The macOS tray is a menu-bar agent — no Dock icon (2026-10-28,
  cc-ai-tray-no-dock).** Superseding #197's "regular app with a Dock fallback": the bundle
  declares `LSUIElement=true` and the runtime activation policy is `Accessory`, so the app
  lives only in the menu bar — no Dock icon, no Cmd+Tab entry — and a bare `cargo run` (no
  `Info.plist`) behaves the same way. The status item is the entry point; the **only**
  remaining programmatic one is `open -a walgit-tray`, which still delivers #200's
  `applicationShouldHandleReopen:hasVisibleWindows:` to the delegate class the tray injects
  its hook into (that hook, its `WALGIT_REOPEN_SELFTEST` seam and its fixture are unchanged
  apart from the log prefix). Accepted consequence, stated so nobody re-litigates it as a
  bug: when the status item is occluded (full menu bar, the notch, a full-screen app) there
  is no clickable entry point — `open -a` from a terminal is what remains. `walgit.icns`
  stays in the bundle (Finder, notifications); it is not a Dock icon.
  `deploy/tray/macos/test.sh` asserts the key is `true`, so a hand-edited `Info.plist`
  cannot silently bring the Dock icon back.

- **D64** **MCP is served by the host itself at `/mcp`; the stdio adapter is gone
  (2026-10-30, cc-ai-mcp-server).** Every instance exposes the Model Context Protocol on
  **Streamable HTTP** at `POST|GET|DELETE /mcp` (`crates/walgit-server/src/mcp/`, on `rmcp`'s
  tower transport — `server`, `transport-streamable-http-server`, `tower` features). It is the
  same data the browser and git see, so it is the same lane: `POST` carries JSON-RPC and answers
  one `application/json` **or** `text/event-stream` message (the transport requires
  `Accept: application/json, text/event-stream` and `Content-Type: application/json`, answers
  `406`/`415` on the HTTP line when they are missing, `404` for an unknown `Mcp-Session-Id`),
  `GET` with the session id holds the notification stream, `DELETE` ends the session (`204`;
  rmcp answers `202`, which the adapter rewrites). Sessions are in memory — a restart costs a
  client one `initialize`, nothing durable.
  **Auth is the existing one, unchanged**: `st.auth.require_read` on the caller's credential
  (session cookie, `wgt_…` token, or a static bearer), a real `401` with a **`Bearer`-only**
  challenge (a `Basic` challenge on a browser-reachable surface pops the native password
  dialog, §1.3), and an `Origin` that is either allowed by `server.cors_origins` or this
  request's own. **The tools are the host's own JSON API**, dispatched in-process
  (`AppState::inner_router`, a `tower::ServiceExt::oneshot` on the same `Router`) with the
  caller's `Authorization`/`Cookie`/`Host`/`User-Agent` replayed — so placement (D30's 503),
  freshness (§2.3), `actor == caller` and every response shape are the code that already
  answers a browser, never a second contract to drift from. Read tools are always advertised
  (`repo_*`, `collab_board/thread/report`, `ci_status`, `wal_ls`); **the one write tool is
  `collab_publish`**,
  which takes an entry the client **already signed** with its own principal key and forwards it
  to `POST /{o}/{r}/api/collab/entries` — the host refuses an `actor` that is not the
  authenticated caller and **holds no key and signs nothing** (signing stays where the identity
  is, D58/D62). The **signature is not checked on write**: it is verified when entries are
  aggregated (`is_verified`), because an entry and the `principal-register` naming its key may
  land in either order and the git-push lane cannot check it at all — an entry that does not
  verify lands in the log and never counts, and cannot be repaired there (append-only). Policy and the done-transition gate stay
  single-sourced on that endpoint. There is no server identity and no destructive operation on
  the surface (D49's rule, kept).
  **Resources** are `walgit://refs/<o>/<r>`, `walgit://wal/<o>/<r>?from=<seq>`,
  `walgit://collab/board/<o>/<r>` and `walgit://collab/thread/<o>/<r>/<id>`:
  `resources/list` (one store listing, bounded), `resources/templates/list`, `resources/read`,
  and `resources/subscribe` — a **server-side bounded poll** of a refs-level read (one
  conditional `manifest.pb` GET per tick) emitting `notifications/resources/updated` with the
  `uri` only, `notifications/resources/list_changed` when the resource disappears. Bytes never
  travel over MCP; clone/fetch/push stay git + bundle-uri. Config is host-level and outside
  D24's per-repo settings: `[mcp] subscribe_interval` (>= 1s), `max_subscriptions`,
  `session_ttl`. **Supersedes D49 and the adapter half of D52**: `walgit mcp` (the stdio
  adapter that shelled out to this CLI) is deleted — keeping it would be a second
  implementation of the same surface — while D52's substance stands: subscriptions are
  adapter-side (now server-side) **polling, best-effort, per-instance**, never a durable push
  channel and never a held request (D55).
  *(Amended 2026-10-31: the surface is a **projection of the JSON API, nothing more** —
  `mcp.key_dir`, the server-side signing path (`mcp/signing.rs`, `write_entry`/`write_claim`)
  and the two tool names were deleted in favour of the single pass-through `collab_publish`.
  The earlier shape had the host hold collaborators' seeds to sign for them; that custody was
  a second mechanism the HTTP surface does not need, and it made the server a signing oracle.)*
  *(Amended 2026-10-31, adversarial review: `DELETE /mcp` **reaps** the terminated session's
  pollers (`Mcp::forget_session`) instead of leaving them reading the bucket forever and counting
  against `mcp.max_subscriptions` until the process dies, and the poller probes its peer's liveness
  every tick, not only when the version moved (rmcp drops the send result, so an idle repository
  never reports a dead session). The subscription budget is **per caller across its sessions** —
  each session's notifications travel down its own stream, so each needs its own poller. Inner
  calls now forward `X-Walgit-Principal` from the *outer verdict* (never from the request), so a
  caller cannot inherit a forwarder's (often admin) identity through `Call`. `resources/read`
  answers `_meta.version` for the board as `seq:<n>:t:<minutes>`, because a `card_claim` TTL
  expiring moves no ref but does move the projection (D52's amendment). `/mcp` is a single CORS
  policy, not two: it is in `web/v1.rs::is_cors_api_path` with `Mcp-Session-Id` exposed, and the
  endpoint has no preflight of its own.)*
  *(Amended 2026-11-01, alignment pass: the surface is **complete over the read API** and
  audited against it — 23 tools, `web/API.md` §5a is the mapping table, and every endpoint
  without a tool is listed there with its reason (bytes: `archive` and CI artifacts; a stream:
  `tasks/{id}`; operator/admin acts: `ops/{op}` start, policy/settings writes, repository
  create/delete). Added the reads that were missing (`repo_merge_base`, `repo_commit`,
  `repo_overview`, `repo_blame`, `repo_tasks`, `repo_ops`, `repo_policy`, `repo_settings` —
  the last answering the *effective* TOML, the one endpoint that is not JSON) and a second
  write, `collab_register` (the caller's own public key), because an agent with no checkout
  could not name its key at all. The claim convergence the deleted adapter implemented with a
  tool is now a documented **three-step recipe** in `INSTRUCTIONS` and both skill files (read
  the board → publish `card_claim` → re-read and stand down if someone else holds it): the
  server keeps no lock, the TTL is the lock, and re-adding `collab_claim` would have been a
  second mechanism. Host validation moved out of rmcp's exact-name matcher into
  `host_allowed` — `walgit.localhost` (the name `walgit.standalone.toml` advertises, and an IPv6
  literal is matched *before* the port split, with `:authority` read when a client sends no
  `Host`) and every
  `127.x` alias are names that can only mean this machine, RFC 6761 §6.3, and rmcp refused
  them unless `server.public_url` happened to name the port; a `Host` that is none of the above
  is a 403 that says which config key fixes it.)*
  *(Amended 2026-11-02: `wal_ls` and the `walgit://wal` resource answer the window they were asked for —
  `from`/`to` echo the request, `to` is a bound rather than a hint, `more` means "newer entries exist than
  this window" (`to < head_seq`), and `folded_through` names the seq a checkpoint folded, so an empty page
  says *why* it is empty (retention reclaimed those segment objects) instead of reporting a moving window
  with `more: true` forever. Reproduced on `gqf2008/abb` (head 5748): every window answered `entries: []`
  because the checkpoint sat at head; the fix is the `log_reader` fallback above.)*
  *(Amended 2026-11-01, D67 tightened: the agent guides present **git + `/mcp` only**. The
  `walgit collab …` forms lost their recipes in `web/SKILL.md` (identity is a seed plus
  `collab_register`, a card is a signed entry plus `collab_publish`, watching is
  `resources/subscribe` + re-read), and `walgit repo …` / `walgit ci …` were demoted with them —
  the CLI section is now "Host operations": the binary runs the host, edits its config and holds
  keys, and its collaboration subcommands are the *scripting* forms of the same two lanes, listed
  by name and documented by `--help`. Same boundary as the HTTP API: still supported, still
  tested, no longer taught as a way to talk to the host. `scripts/check-skill-covers-cli.sh`
  keeps its floor by name-matching, which is exactly the "one naming line, no recipe" shape.)*
  The bundled UI teaches the endpoint where an agent arrives: a **MCP box**
  (`web/src/components/McpSetup.tsx`) on every repository's `/collab` page and on the board's
  empty state is **one paragraph with one copy button** — it states the endpoint and the
  `walgit://` URIs this project exposes for `resources/read` and `resources/subscribe` (refs,
  the retained log, the board projection; the shapes for any other repository and the
  per-thread URI come from `resources/templates/list`), so a reader copies it into an agent and
  the agent configures its own client. No curl, no client config, no protocol reminders: MCP is
  a standard, so the session header and the methods are the spec's job.
  All data-free — the only repo-scoped text is built from the route, never a request.

  *(Amended 2026-11-01, `walgit-mcp-resource-version-churn-20261101`: a resource version must move
  **iff that resource's body would**.)* The first shape used the repository's WAL seq for every
  URI, so every subscriber of every thread woke on every ref write (a `ci-artifacts` blob landing
  woke card watchers), and a `card_claim` TTL lapsing — which moves no ref but does move the
  board's owner column — never woke anyone. The patch for that half carried `now / 60` and woke
  every board subscriber once a minute forever: measured on a 4104-inbox-entry repository, seq
  frozen, 1440 wakes/day carrying a 183 KiB body. So versions are now computed from the
  projection, never from the clock and never from the repository seq: a **thread**'s version is
  its own tip oid (`h:<tip>`), a **board**'s is a fingerprint of the projected cards plus the
  earliest claim that is still live (`b:<witness>[:claim:<expires_at>]`, so a lapse is exactly one
  more move and a board with no claim carries no clock at all), and `refs`/`wal` stay on `seq:<n>`
  because they *are* the repository-wide view — subscribing to them is watching the whole
  repository. The projection is built from the board document itself (so the version and the body
  a read returns cannot disagree), cached per repository and bounded
  (`MCP_PROJECTIONS_MAX = 32`), and rebuilt only when the fingerprint of
  `refs/collab/{inbox,meta}` changes — the `ci-artifacts` namespace is excluded on purpose, so a
  busy CI lane costs a subscriber nothing. `_meta.version` and the notification use one function,
  not two spellings of it (`crates/walgit-server/src/mcp/resources.rs`).
- **D65** **One program file per install; a `PATH` entry is a symlink, never a copy (2026-11-01,
  cc-ai-cli-entry-owner).** Program files live where an updater owns them (D51: the macOS app
  bundle, `/usr/bin` from the `.deb`, `%USERPROFILE%\walgit` from Inno Setup); `~/.walgit` is user
  state only. The tray's `install_cli_link` (`deploy/tray/tray-rs/src/bootstrap.rs`) therefore
  creates the terminal entry — `/usr/local/bin/walgit`, falling back to `~/.local/bin/walgit` —
  as a **symlink into the app bundle** and re-points it on every start, so an app upgrade moves
  the CLI with it. Until this decision it **refused to touch a regular file** at that path and only
  logged a line: a hand-copied CLI is a second program file no upgrade can reach, and that is how
  a v0.5-era copy of `~/.local/bin/walgit` (2026-10-27) kept serving `walgit mcp` — and an
  MCP surface whose tools had been deleted upstream — weeks after the release that removed them,
  while the same machine's `/healthz` answered `v0.11.0`. The rule now: a regular file that
  **answers `--version` as a walgit** (behind a ≥1 MiB size gate, so a wrapper script is never
  executed) is renamed aside as `walgit.copy-<unix-ts>` — never deleted, it is the user's file —
  and the symlink takes its place; a note lands at `<state>/cli-link-healed.json` for the UI to
  report. Anything that is not a walgit binary (a wrapper, another tool, a symlink the user made)
  is left alone. Corollary, and the other half of the bug: **no configuration may pin a program's
  absolute path** (an agent's `mcpServers` entry did, `command: "/Users/…/.local/bin/walgit"`),
  because that is a second owner that no upgrade follows — `command: "walgit"` resolves through
  the one entry.

  *(Amended 2026-11-01, every platform: the entry rule lives in
  `deploy/tray/tray-rs/src/cli_entry.rs` and runs on **every** tray start, not just macOS. It no
  longer guesses two fixed paths: it asks where `walgit` must be findable — the process `PATH`
  **and** the login shell's (`sh -lc`, because a tray launched by launchd/Explorer has a much
  narrower `PATH` than a terminal) — then picks the first candidate that is both writable and
  already on that `PATH` (`~/.local/bin` first, then `/opt/homebrew/bin`, `/usr/local/bin`;
  Linux the same minus Homebrew). Nothing on the `PATH` is writable ⇒ the entry is still created
  and the log says `on_path = false` with the line the user has to add: walgit edits no shell rc
  file, anywhere. A candidate that **is** the program (`/usr/bin/walgit` from the `.deb`) is
  never "healed" — it would be renamed aside and symlinked to itself. Windows has no symlink
  lane: the equivalent act is appending the install directory to `HKCU\Environment\Path`
  (`REG_EXPAND_SZ`, idempotent, the same judgement as `deploy/windows/user-path.iss`), because
  the installer's one-time write cannot heal a PATH that was reset later. Rule stated once, used
  by all three platforms: after the tray starts, a new shell resolves `walgit`.)*

- **D66** **The collab inbox folds itself; the fold is a maintainer unit, not an operator's chore
  (2026-11-01, cc-ai-collab-fold-unit).** Every collab lane budgets by **ref count**
  (`COLLAB_MAX_ENTRIES`, §9.4 of `docs/D1_PROTOCOL.md`), and folding is a pure function of the inbox
  (`walgit_wal::collab::build_snapshot*` — aggregation over snapshot ∪ inbox is byte-identical to the
  unfolded ledger, `fold_and_snapshot_plus_tail_are_byte_identical_to_the_unfolded_inbox`). Leaving that
  to whoever remembers `walgit collab gc` means a busy repository's board stops aggregating *silently*,
  so the folded state is now the maintainer's own housekeeping: `Unit::CollabFold`, triggered by
  `maintenance.collab_fold_refs` (default 5 000 inbox refs, 0 = off) instead of a clock, refs-level so it
  runs wherever the repo is assigned (`crates/walgit-server/src/collab_fold.rs`, op `collab-fold`).
  **Why it needs a key at all:** the snapshot document is *signed* (`sign_snapshot`) and a snapshot
  nobody can verify strands every reader — so D45 handed the fold to an operator principal. The
  maintainer stands in for one only when `maintenance.collab_fold_principal` **and**
  `maintenance.collab_fold_key` name a registered principal and its seed (host-only config, never
  per-repo settings, D24); with neither, nothing is planned and the CLI remains the manual form. A key
  the registry does not name is refused **before** anything is published. The unit differs from the CLI
  in one deliberate way: it lands the snapshot and prunes the inbox in **one atomic ref transaction**
  (snapshot CAS baseline + per-ref OID lease + delete), so the two-phase window the CLI has between
  "push snapshot" and "delete inbox" does not exist; a losing CAS abandons the whole transaction and the
  next pass re-reads. It prunes only refs the *new* snapshot carries (an unparseable blob stays put, as
  in the CLI), reuses the baseline snapshot for a prune-only fold, and truncates oldest-first with
  `complete=false` plus a warning when the ledger is over 64 MiB — an automatic path cannot "refuse to
  publish" its way back to serviceability. Test: `crates/walgit-server/tests/collab_fold.rs`.

  *(Amended 2026-11-01, review: **a truncated snapshot names what it dropped** —
  `build_capped` returns the dropped oids, and the fold prunes only the refs the
  new snapshot carries. Before, the 64 MiB truncation path computed a count and
  then deleted every folded inbox ref, i.e. the entries the snapshot left out
  vanished from an append-only ledger with no retention window to restore them.
  `FoldOutcome.dropped` reports the real number now, and the truncation path has
  a unit test. The fold identity (`maintenance.collab_fold_principal` /
  `…_key`) is genuinely host-only too: `public_settings_toml` strips both and
  `with_settings` refuses them per repository — the pair names this host's key,
  and a repository admin could otherwise re-point the maintainer's identity and
  use the key-path validation as a filesystem probe.)*
  *(Amended 2026-11-01, same decision, cc-ai-collab-fold-unit: the **CI control plane** is the same
  projection. `ci_status` (MCP) → `GET /{o}/{r}/api/collab/ci[?id=]`, which serializes
  `walgit_wal::ci::collect_runs` — the aggregation `walgit ci status` and the report's CI section
  already share, so there is no third answer. Claiming and reporting are **entries**
  (`kind: ci_claim` / `ci_result`) published through `collab_publish`; the runner half — clone, build,
  artifacts — stays git + the artifact endpoints, because bytes do not travel over MCP. `walgit ci`
  remains the local runner; nothing about CI is CLI-only any more, and nothing is MCP-only either.)*

- **D68** **One HTTP lane and MCP for agents: both collaboration subcommands and the `/{o}/{r}/api`
  lane are closed to the outside (2026-11-01, cc-ai-mcp-single-surface).** D67 stopped *describing*
  the HTTP API to agents; this closes it. Three surfaces move behind one door:
  (1) **`/{owner}/{repo}/api/…` is not routable any more** — `REPO_API_BASES` has exactly one entry
  (`/{owner}/{repo}/api-browser`), so the old lane answers a plain 404. The MCP tools keep calling the
  same handlers: `crates/walgit-server/src/mcp/api.rs::Call::lane` rewrites a tool's natural
  `/{o}/{r}/api/…` path to `/{o}/{r}/api-browser/…` in the one place that decides it, and the call
  still goes through `tower::oneshot` on this process's own router — so placement, freshness and
  `actor == caller` stay the code that already answers a browser (D64). Credential handling was
  never the lane's job: a bearer token works on `api-browser` exactly as it did on the old lane, and
  the SDK's `lanePrefix` now returns `/api-browser` for every lane while `lane` still decides how the
  credential travels. The bundled UI needed no change beyond the segment (`repoApi`), because the
  browser lane is the same handlers.
  (2) **`walgit collab`, `walgit ci`, `walgit principal` are hidden and deprecated.** They are not
  deleted — they are the same protocol code, and an operator needs them for a script or an
  emergency — but they are `#[command(hide = true)]` and every run prints one deprecation line on
  stderr naming the MCP tools that replace them. There is **no switch** (2026-11-02: the
  `WALGIT_LOCAL_COMMANDS` door was removed): a notice is not a permission, and a migration that
  depends on an environment variable has not started. They behave exactly as before until the next
  major release, which deletes them. The `/{o}/{r}/api/…` module is in the same stage — `/api` stays
  404, `api-browser` is the one lane, and `web/api.rs` is marked deprecated in its own docs because
  MCP has taken over as the agent interface.
  (3) **What stays**: `/{o}/{r}/api-browser/…` (the UI, the SDK, a script that needs bytes such as
  `archive` and `ci-artifacts`), `/api/v1` (discovery, `me`, owners, the setup wizard and storage
  editor), `/mcp` (the agent interface), git, LFS and the static lanes. The repo **create/delete/admin**
  route moved with the lane (`PUT|GET|DELETE /{o}/{r}/api-browser`), since it was mounted per lane.
  Tests: `crates/walgit-server/tests/web_api.rs::the_repository_api_lane_is_not_served` (five gone
  paths 404, two live ones 200, `/api/v1` unaffected), `walgit-cli`
  `local_command_tests::{only_an_explicit_yes_opens_the_local_door, the_collaboration_groups_are_hidden_from_help}`.
  Amends D15/D20/D26/D27: the lane split keeps its shape, one lane is retired. `web/API.md` states the
  rule (and that agents use `/mcp`, D67); `docs/D1_CI_PROTOCOL.md` carries the moved paths.


  *(Amended 2026-11-02, cc-ai-shell-for-token-hosts: **a page navigation gets the shell; data lanes
  never do**.)* Retiring a lane exposed a hole that had nothing to do with lanes: on a `token`-mode
  host with `anonymous_read = false` a browser could not get *in*. Token mode has no sign-in page
  (only `oidc` mints a session), the token lives in the page's `sessionStorage`, and the page itself
  was gated — a door with the handle on the inside. The server now serves the **static shell** to a
  page navigation and nothing else: `crates/walgit-server/src/web/mod.rs::shell_request` passes a
  `GET` whose `Accept` looks like a browser (`is_browser`) through the auth gate **unless**
  `is_data_lane(path)` — `/api/…`, `/services/api/…`, `/_auth/…`, `/mcp`, `/metrics`, any
  `/{o}/{r}/api…` or `api-browser…` segment, and git smart HTTP — plus the compiled assets
  (`/_ui/…`, `/favicon.ico`, `/index.html`), whose requests never carry `text/html`. A host with
  app-owned sign-in (`browser_login_enabled()`, i.e. `oidc` with an OAuth client) keeps its
  redirect to `/_auth/login` instead: the shell would replace the one path a browser has to a
  session. The shell carries no repository data and the page's own reads are the same gated
  calls as before, so no read path is added: the 401 is merely made *reachable*. `web/src/components/AuthGate.tsx` then
  asks for the token, keeps it in `sessionStorage` for the tab and switches the SDK to the bearer
  lane — the credential git uses; a host that allows anonymous reads answers `me` and the gate steps
  aside. Tests: `crates/walgit-server/tests/web_ui.rs::
  a_token_only_host_still_serves_the_shell_so_a_browser_can_enter_a_token` and
  `web/src/auth-gate.test.ts`.

- **D69** **The collaboration command families move to the standalone `wgl`
  client (Zig); walgit's CLI carries them no more (2026-11-06, cc-ai-wgl-zig).**
  D68 hid `walgit collab` / `walgit ci` / `walgit principal` behind a deprecation
  notice and scheduled them for deletion at the next major release. This is that
  deletion, with the replacement landing first: the whole command surface now
  lives in **`wgl/`**, a standalone Zig module (zig pinned `0.17.0`) that speaks
  the same D1 protocol over the same lanes — reads aggregate the local
  `refs/collab/*` offline (§3.2), writes are local refs pushed over
  receive-pack, and the host registry commands talk to `/api/v1/principals`.
  The server keeps everything it had: the MCP collab tools, the thin API, the
  browser collaboration surface, the fold maintainer unit (D66) — nothing on
  the wire changed, and the canonical form is the same contract (wgl pins the
  §5.3.1 golden vectors).
  - **Byte-compat is the acceptance, and it holds**: `wgl collab board
    --format json` and `--format hash` are byte-identical to the retired Rust
    CLI on the same refs, thread output included; the transport e2e
    (`scripts/wgl-e2e.sh`, the lane the retired suites owned) ends by
    cross-checking the board hash between wgl and the Rust aggregation on the
    folded state. The D45 fold equality (§9) holds across the boundary: wgl's
    gc folds, and the pre/post-fold hashes are identical.
  - **What moved**: `collab_cmd.rs`, `ci_cmd.rs` (the local CI runner —
    validate/run/status/log/artifacts, the §8.1 worktree sandbox, the cron
    sweep with D1-CI §4.3 due-slot coalescing), `principal_cmd.rs`, and their
    integration suites (`collab_e2e.rs`, `ci_e2e.rs`) — replaced by wgl's unit
    tests (golden vectors, §16 anchors ported from `walgit-wal::collab`) and
    `scripts/wgl-e2e.sh` (lanes `just wgl-test` / `just wgl-e2e`; the
    suite-guard anchors them). `proc_group.rs` went with the runner; the
    server's maintainer keeps its own kill paths.
  - **What stayed**: `walgit-wal`'s collab/ci aggregation libraries (the MCP
    face and the thin API still serve them — one aggregation, every client),
    the host registry, and every server-side surface from D64/D66/D68.
  - Agent-facing notes: the tracker's scripting lane is now `wgl collab …`
    (§6 of the charter updated in the same change); the exit contract of
    `wgl collab claim` is unchanged (0 won / 2 stand-down / 1 error). Amends
    D68's "next major release" schedule (done early, replacement first) and
    D62's retire-a-principal pointer (`wgl collab principal-revoke`).

- **D70** **The collaboration MCP face moves from the host's `/mcp` to `wgl mcp`
  (stdio); the host keeps `repo_*` and `wal_ls` (2026-11-06, cc-ai-wgl-mcp).**
  This is the agent surface D64 described, split by what each side can honestly
  read. `wgl mcp --repo <checkout> [--remote origin]` serves JSON-RPC over
  **stdio** (one message per line; stdout carries nothing else, logs go to
  stderr, stdin EOF exits 0) with:
  - tools `collab_board`, `collab_thread` (`{id, entries:[{oid, principal,
    verified, entry, broken_refs}], pr}`), `collab_report`, `ci_status`
    (`{repo, runs}` with optional `id`), the one write tool `collab_publish`
    (an entry the caller **already signed**; parsed like the server's serde —
    unknown fields ignored — with `actor` required to be this worktree's
    identity principal; when given as JSON text its **exact bytes** become the
    blob, so the oid matches `wgl collab entry`, pushed to
    `refs/collab/inbox/<actor>/<uuid>`) and `collab_register` (`force?`: this
    worktree identity's public key to `refs/collab/meta/principals/<p>`; a
    different existing key is refused unless forced — the §4.3 rotation guard,
    since rotation makes every past entry unverified);
  - resources `walgit://collab/board` and `walgit://collab/thread/<id>`, with
    `resources/list|templates/list|read|subscribe|unsubscribe`. Subscriptions
    are in-process polls (`--subscribe-interval`, default 5 s, `--max-subscriptions`,
    default 32), never a held request (D55).
  **Freshness is the point of the split.** Every read fetches
  `refs/collab/{inbox,meta}` first (`git fetch --prune`, exactly `wgl collab
  claim` step 1) and **fails closed** when that fails; `fresh:false` (per call)
  reads the cached refs, and `--no-fetch` (process) is a **hard gate** that no
  per-call `fresh:true` can override; answers report `_meta.fetched_at`.
  Without this, a worker that had not fetched could see no owner and duplicate
  the work §8.4 exists to prevent. The MCP fetch and ref pushes are bounded
  (120 s); a timed-out transfer is the same fail-closed tool error, never a hung
  stdio loop. Other CLI writers of big blobs (`collab gc`'s ≤64 MiB snapshot,
  CI ≤16 MiB artifacts) scale the bound with the payload (`gitx.pushTimeoutSeconds`)
  instead of inheriting the ref bound. One request line may be 2 MiB (entry cap
  plus envelope/escaping); a longer line is drained, answered once with
  `-32700`, and the process keeps serving.
  **Caller and write checks.** The caller is `<git-dir>/walgit/identity` —
  one identity per worktree (D58/D59/D62). `collab_publish` parses the entry
  through the same structural rules as the server's write path (`refs` shape,
  `version` range, unknown fields ignored) and additionally requires an
  `ed25519:` + 64-byte signature, refuses `actor != caller`, and enforces the
  `status=done` gate locally after a fetch (the same rule as `wgl collab entry`
  and the thin API). **It does not verify
  the signature on write**: an entry and the registration naming its key may
  land in either order, and the git lane cannot check it — aggregation verifies
  (`docs/D1_PROTOCOL.md` §4.5). A raw `git push` bypasses every local check;
  these are discipline, not a security boundary (§7.5, §14).
  **Order (no dual authority).** `wgl mcp` landed and passed shape acceptance
  first, then the host `/mcp` lost its D1 tools and `walgit://collab/*`
  resources in the same change; the MCP-only projection/board-version cache was
  deleted rather than merely unadvertised. `repo_*` and `wal_ls` remain because
  they need host knowledge (registry, placement, remote reader, freshness).
  **D64 relationship (controlled exception).** D64's target was a *second
  implementation of the same data path* — the retired stdio adapter proxied the
  CLI/host. `wgl mcp` is not that: reads aggregate the checkout's own
  `refs/collab/*` offline and writes are receive-pack pushes, while `/mcp`
  projects the host's HTTP API. With the old face gone, the agent MCP face has
  one implementation again. The thin API and the SPA still aggregate
  server-side, so "double read" disappears only on the agent MCP face, not
  globally — the client and server deliberately cross-check each other in
  `scripts/wgl-e2e.sh`.
  **Supersedes** D69's sentence that "the server keeps … the MCP collab tools"
  and D64's D1 tool/resource list; the rest of D69 stands. D64's auth, transport
  and subscription rules still govern the remaining host surface.

- **D71** **The collaboration client is renamed `wgl` → `wg`; this supersedes the
  name in D69/D70, not their decisions — the command surface, protocol and
  behaviour are unchanged (2026-11-06, cc-ai-wg-rename).** The standalone Zig
  client is the same binary and the same contract under a shorter name. The
  project directory is `wg/` (`build.zig.zon` `.name = .wg` with the refreshed
  `.fingerprint`, `build.zig` `.name = "wg"`), the transport e2e is
  `scripts/wg-e2e.sh`, the justfile lanes are `just wg-test` / `just wg-e2e`
  (the suite-guard anchors moved with them), and the release assets are
  `wg-<arch>-<os>.tar.gz|zip` (build dir `wg-dist/`, artifact names
  `wg-binaries*`; the mac zips still contain plain `wg` / `wg.exe`, signed and
  notarized as before). Deployment scripts read `WG_BIN`; the old `WGL_BIN`
  spelling is deleted with the rest of the old name (pre-1.0: no aliases). The
  commands are the same — `wg collab …`, `wg principal …`, `wg ci …`,
  `wg mcp` — and the entry schema, `refs/collab/*` layout, signatures, exit
  codes, MCP tool/resource shapes and error-prefix discipline (`wg: `) do not
  change. The operator skill moves to `skills/wg/SKILL.md` (`name: wg`). The
  rename lands before the first release that would have carried `wgl`, so no
  published asset or persisted identifier is broken; `WALGIT_*` variables,
  `.walgit/` paths and `refs/walgit/*` keep the product name.

- **D72** **The host no longer serves MCP: repository reads are plain git, and
  collaboration is `wg mcp` (2026-11-07, cc-ai-rm-host-mcp).** D64 put an MCP
  endpoint on the host (`POST|GET|DELETE /mcp`, `crates/walgit-server/src/mcp/`,
  a Streamable-HTTP projection of the host's own JSON read API) and D70 kept
  `repo_*`/`wal_ls` there after moving collaboration to `wgl mcp` (since D71,
  `wg mcp`). This removes that endpoint and the whole `[mcp]` configuration
  (`subscribe_interval`, `max_subscriptions`, `session_ttl`): the host is a
  server, and a read interface for agents does not need to be re-implemented on
  top of it when git already is the read interface.
  - **Repository reads are git.** `git ls-remote --heads --tags`, `fetch`,
    `clone --filter=blob:none`, `cat-file --batch`, `log`, `diff` and `blame`
    cover discovery, refs, single blobs, history and metadata with no second
    contract to drift. A host API exists for the bundled UI and the SDK
    (`/{owner}/{repo}/api-browser/…`), not for agents.
  - **Collaboration stays where D70 put it.** `wg mcp` (stdio, against a
    checkout's own `refs/collab/*`) and `wg collab` are unchanged; their tools,
    resources, freshness and signing rules are untouched by this decision.
  - **Host maintenance views stay.** `ops`, `tasks`, `settings` and `policy`
    remain on the existing HTTP API and the `walgit` CLI: they are host facts,
    not repository reads, and D70 never moved them.
  - **Upgrade compatibility.** A `walgit.toml` written before D72 may still
    carry `[mcp]`. The loader accepts the table, drops it and prints one clear
    warning on stderr (and reports `WALGIT__MCP__*` overrides as ignored), so
    an existing host still boots; the section can then be deleted. The old path
    answers 404 for every method — a tombstone route shadows the SPA's
    `/{owner}` page route, which would otherwise answer it with the shell — and
    CORS no longer treats it as an API path.
  - **Tests pin the removal**: `POST|GET|DELETE` on the old path are 404, a
    preflight/response carries no CORS grant for it, the `/api/v1` discovery
    index does not advertise MCP, and a residual `[mcp]` section (file or
    environment) parses and validates. The MCP suite and this surface's
    projection tests were deleted with the feature; they are not a weakening
    of coverage, and the non-MCP store-error classification tests that lived
    in that file moved to `tests/e2e.rs` and `tests/web_api.rs`.
  - **Supersedes** the host-endpoint half of D64 (its auth/transport/
    subscription rules no longer have a host surface to govern; `wg mcp`'s own
    transport rules in D70/D71 stand) and the sentence in D70 that "the host
    keeps `repo_*` and `wal_ls`". The rest of D64, D67, D70 and D71 stands:
    in particular, agents are still told one interface family per job — git for
    repository reads, `wg mcp` for collaboration.
