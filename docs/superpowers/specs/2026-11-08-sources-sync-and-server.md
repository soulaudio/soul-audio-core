# Sources, transports, mutable state, and the server

Status: **design — committed (see [`ARCHITECTURE.md`](../../ARCHITECTURE.md) §2);
server/sync parts are seam-reserved, built in a later phase.**
Date: 2026-11-08
Builds on: [`shared-catalog-index-architecture`](2026-11-08-shared-catalog-index-architecture.md),
[`dap-export-and-blobs`](2026-11-08-dap-export-and-blobs.md).

This spec covers everything above the single-source `.soulidx` reader: multiple
sources, the three transports, the mutable-state op-log, the three sync flows,
and the server. The shared `no_std` core is unchanged by any of it — these are
adapters.

## 1. Sources & `MultiCatalog`

A **source** is a `Catalog` adapter. The desktop aggregates many; the DAP has
one.

- **Desktop library** = `{ watched dir, … } ∪ { remote server, … }`.
- **DAP library** = `{ one SD card }` — the `N = 1` case; no aggregation code.

**`MultiCatalog`** (std, desktop) is itself a `Catalog` that composes `N` member
catalogs:
- Members are each **pre-sorted**, so aggregate browse is a **k-way merge** of
  `N` sorted order streams (a small `N`-head cursor; the desktop has the RAM) —
  never a re-sort.
- **Identity is compound `(source_idx, local_id)`.** Packed into the `u32` id
  for reasonable counts, or widened at the `MultiCatalog` layer. Every id the UI
  holds is compound; the backend routes each read/op to the owning source.
- Search/filter fan out to members and merge results.

Source kinds, each a `Catalog` adapter:

| Source | Backing | `SeekSource` | Persistence |
| --- | --- | --- | --- |
| local directory | scan → **local `.soulidx` cache** → read; re-bake on change | file | yes (the cache) |
| remote server | server-baked `.soulidx` | HTTP Range | n/a (remote) |
| DAP SD card | desktop-baked `.soulidx` | SD | yes (card) |

The desktop dogfoods `.soulidx` as its own persistence (committed decision §7) —
baking a local cache is the *same code* as DAP export and server bake.

## 2. Transports — one reader, three `SeekSource`s

`IndexReader<S: SeekSource>` reads `.soulidx` over any seekable byte source; the
decoder reads audio the same way. Adapters:

- **file** (`std::fs::File` via `embedded-io-adapters`) — desktop local.
- **SD** (`embedded-sdmmc`) — DAP.
- **HTTP Range** (`soul-audio-remote`, std, reqwest) — remote. Browse/query touch
  only the pages they need (sql.js-httpvfs pattern: tiny transfers against a
  large index because lookups are indexed, not scanned).

Consequences (committed §4, §8, §11):
- **Local stays in-process** (no local HTTP server). Remote is a *source*.
- **Remote audio streams** over a Range `SeekSource` — same decode path.
- **At scale/remote, bake indices, don't scan** — a scan is many round-trips.
  Dynamic search gets a baked **term index** section (sorted terms + postings →
  binary search, no-alloc to read); the linear scan is the small-card fallback.

## 3. Three query mechanisms (how each feature stays shared)

1. **Pre-baked** — desktop computes once, both read: album/artist/folder/genre
   browse (order tables + facet directories). 1:1 identical.
2. **Shared runtime logic** (`no_std`, no-alloc, runs on both): navigation
   (walk/jump/facet-subrange), a search/filter baseline, playlist edit ops.
3. **Platform override** (same port, richer adapter): desktop's larger in-RAM /
   baked search index; the e-ink art baker; (later) a server FTS.

Faceted filter = sub-range nav over a baked facet directory (shared). Dynamic
predicate filter = no-alloc scan (baseline) + optional baked secondary index
(desktop). Free-text search = baked term index + no-alloc baseline behind a
`Search` port; quality degrades gracefully across platforms, the interface is
one.

## 4. Mutable state — the op-log (`MutableStore`, one port many backings)

Immutable `.soulidx` is read-only; **mutable state** = favorites, play-counts,
ratings, last-position, user playlists. Modeled as an **append-only operation
log** with CRDT merge — *sync intent, not state*:

- **Merge semantics:** LWW-register (favorite, rating), grow-only **counter**
  (play-count), **OR-set + fractional index** (playlist membership + order),
  LWW-map for scalar fields.
- **`no_std`-feasible:** a tiny typed op set (favorite / rate / play++ / add /
  remove / move / set-position), hand-rolled with these merge rules — no heavy
  dep on the DAP's critical path. (`crdt-kit`, ~50 KB `no_std`, is the escape
  hatch if we later want generic collaborative editing.)
- **One `MutableStore` port, many backings:** desktop = embedded op-log file;
  DAP = a small writable card op-file (the one thing it writes); server = DB +
  op-log. All speak the same op vocabulary so they converge.
- **Ops reference compound `(source, track)` ids**, so mutable state spans
  sources and reserves the multi-user seam.

## 5. Three sync flows

| Flow | Direction | Mechanism | Phase |
| --- | --- | --- | --- |
| **1 Catalog read** | server → client | `.soulidx` snapshot over HTTP Range | with remote adapter |
| **2 Mutable-state sync** | client ↔ server | op-log / CRDT, two-message (`since`/vector-clock) exchange | server phase |
| **3 Content upload** | client → server | content-hash–keyed upload, dedup, server re-index | server phase |

Flow 2 is multi-writer by construction (CRDTs converge regardless of order/
author) — so "multiple desktops/users on one server" is handled for metadata.
Flow 3 ("add a song locally, sync it to the server") is a content-addressed
transfer: upload keyed by `quick_hash`/`content_hash` → server dedups → stores →
re-indexes; the desktop records provenance `(local id ↔ server id)`.

## 6. The server (baker + socket) — CQRS-shaped

```
   writes (upload, ops)                    reads (range)
desktops ─────────────────►  SERVER  ◄─────────────────── clients
                               │
        ┌──────────────────────┴─────────────────────┐
  authoritative MUTABLE store              baked IMMUTABLE snapshot
  (catalog + users + perms + op-log)  ──►   .soulidx + blobs
  — a real DB is fine HERE (adapter)        (atomic swap; readers never torn)
```

- The server runs the **same import/bake crates** over its library, keeps an
  **authoritative mutable store** (DB — the *only* place SQLite/Postgres may
  live, as a server adapter), and **bakes immutable `.soulidx` snapshots** it
  serves. Ingestion/re-bake swaps a snapshot atomically; concurrent readers are
  lock-free against a frozen file.
- Clients are **unchanged** — they read `.soulidx` over their transport. The DAP
  is untouched.
- **Multi-user** = users + permissions + per-user mutable streams. Scoping
  (shared vs. private vs. shared-catalog-with-private-favorites) is an authz
  decision on the server adapter, not a format/core change.
- **Interop (optional, later):** an OpenSubsonic facade as a server adapter for
  the existing client ecosystem; native `.soulidx`-over-range is the primary,
  lossless path.

## 7. Crates this introduces (all std adapters, tier = Std adapter)

| Crate | Role | Phase |
| --- | --- | --- |
| `soul-audio-remote` | HTTP-Range `SeekSource` + remote `Catalog` adapter | remote phase |
| `soul-audio-multicatalog` | aggregate `N` sources (k-way merge, compound ids) | multi-source phase |
| `soul-audio-mutable` | the op-log `MutableStore` (+ CRDT merge); `no_std`-able core, std backing | mutation phase |
| `soul-audio-artbake` | decode + 1-bit dither → `art.blob` bytes (isolates `image` dep) | export phase |
| `soul-audio-server` | Axum: serve `.soulidx` + audio + sync; DB authority | server phase |

All are reserved in the tier manifest (`tiers.rs`) so, once added, they are
forced into the Std-adapter tier and may never be pulled in by the core or the
DAP firmware.

## 8. Reserved / deferred

Content ingest + dedup + resumable upload; auth + permission scoping; server DB
authority + snapshot baker; OpenSubsonic facade; cross-source dedup; managed-
library sync-to-device; incremental re-bake; enrichment-sourced extended data.
See [`ARCHITECTURE.md`](../../ARCHITECTURE.md) §9.
