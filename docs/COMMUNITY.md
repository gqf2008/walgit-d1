# Community features on walgit

Status: **accepted architecture; phase 1 implemented** (Discussions projection/API/SPA, Projects
board/table view layer). This is the design of record for Projects and Discussions. Code wins if it
and this document disagree; the document is updated in the same PR.

## 1. Authority model

Community features split by authority, not by page or product name:

| Surface | Authority | Write path | Read path |
|---|---|---|---|
| **Discussions** | signed `refs/collab/*` entries | `POST /api/collab/entries` or receive-pack | deterministic `build_discussions` projection |
| **Projects** | signed `status` entries + repository-versioned declarations | signed entries and reviewed commits | deterministic board/table projection over collab threads |

No feature here may create a server-side table, a mutable project record or a webhook
from the write path. Wiping every running instance must lose only caches.

## 2. Discussions

A discussion is a D1 thread whose root entry has `kind = "discussion"`:

```json
{"kind":"discussion","id":"release-notes-q3","body":{"title":"How should release notes be generated?","body":"Markdown body","category":"ideas"}}
```

Replies use `comment`. A solution is immutable and references the accepted comment:

```json
{"kind":"solution","body":{"comment_oid":"<comment oid>","accepted":true}}
```

The newest solution entry wins; `accepted:false` revokes it. Closing uses the existing `status` kind with
`{"status":"closed"}`. Pinning, locking and moderation are intentionally out of the first implementation:
they need policy semantics, not a mutable flag.

`build_discussions(entries, principals)` is a pure function beside `build_report` and `build_board`:

- title, category and author come from the root entry;
- `reply_count` counts comments after the root;
- `answered` and `solution_oid` come from the newest solution entry;
- ordering is stable `(last_ts desc, id asc)`, exposed through an opaque `(last_ts,id)` cursor;
- every entry is independently verified; a snapshot signature never substitutes for an entry signature.

The list API is `GET /{o}/{r}/api/collab/discussions?category=&state=&after=&n=`. The existing thread
endpoint is the detail page. Writes use the existing signed-entry paths and enforce
`actor == authenticated principal`, inbox ownership and Ed25519 verification.

## 3. Projects

A project is a view, not a table. A card is a collab thread; moving a card is a signed `status` entry.
Membership, columns, table grouping and roadmap dates are deterministic projections over the same entries.
No copied card or mutable project row exists.

The first implementation exposes Projects as a view layer over the existing board projection: board and
table views, filters and stable ordering all consume the same `BoardCard` set. A later `.walgit/projects.toml`
may declare multiple named projects; that declaration must be repository-versioned, fail-closed and a pure
extension of this model.

Cross-repository projects are excluded. They require a verified cross-repository index or federation; a
server-side aggregate table would violate the object-store-only rule.

All project views derive from one manifest-CAS-consistent collab read. Sorting is explicit and total: the
projection defines the sort fields and appends `id` as the final tie-break. Clients render the server order
and do not re-sort.

## 4. Wiki — deliberately not built

An earlier revision of this branch shipped a Git-backed wiki (`refs/walgit/wiki`, `walgit wiki …`,
`GET /{o}/{r}/api/wiki`, a SPA page). It was cut before merge (decision 2026-09-20): it was a
human-only browsing surface that created a second documentation home outside the reviewed branch,
and it was worse for agents (the ref is not in a default clone). Repository documents (`docs/`,
`README.md`, `SKILL.md`) remain the single home for prose; a read-only rendering of a repository
tree needs no backend and can be added later if it earns its keep.

## 5. Performance and security

- Community reads are refs-level plus one bounded entry/blob fan-out; the existing 20,000-ref collab budget
  applies until a fold reduces the tail.
- Discussion pagination uses a stable cursor; offsets are forbidden.
- No server-side cache is authoritative. Live community endpoints use SWR/ETag, never `immutable`.
- Discussion/project writes pass through collab policy and actor checks.
- The aggregator verifies each entry independently; snapshots are audit containers, not trust roots.

## 6. Acceptance and delivery order

1. Discussion projection/API/SPA: create, reply, accept and close through signed entries; CLI/API/SPA derive
   the same answer from the same refs.
2. Projects view layer: board and table show the same cards in deterministic order.
3. `collab gc` leaves discussion/project projections byte-identical.
4. Malformed declarations fail closed; auth/policy behavior matches existing writes.
5. `web/SKILL.md`, `skills/walgit/SKILL.md` and `web/API.md` are updated in the same change.

Delivery order is Discussions + Projects views first, then named projects/custom fields.
