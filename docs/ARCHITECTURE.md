# Soul Audio — committed architecture

> **This document is the committed architecture. It is the single source of
> truth.** Changing anything here is a deliberate act: it requires a discussion
> and an update to this document *and* the architecture tests that enforce it.
> Code, specs, and agents must conform to this; if reality and this doc
> disagree, that is a bug in one of them — reconcile, don't drift.
>
> The architecture is **machine-enforced** (see §7). The tests are the
> guardrail; this document is the explanation.

## 1. Essence

**One `no_std` brain and one on-disk format (`.soulidx`), consumed through a
small set of ports by three bodies — a desktop app, an e-ink DAP, and a server —
where the only things that differ are the transport, the rendering, and how much
you optimize on top.**

The organizing principle: **the build target is the arbiter.** If code compiles
under `no_std` + no-alloc + `forbid(unsafe_code)` + `thumbv7em-none-eabihf`, it
belongs in the shared layer. If it cannot, it is *by definition* a platform
adapter. Placement is not a matter of taste — it is a test (§7).

## 2. Committed decisions

These are settled. Each is enforced by docs + (where mechanizable) tests.

1. **Shared, platform-agnostic core is `no_std`.** Playback (engine/control/
   queue/tags) and the data layer (catalog/index-reader) build for the STM32
   DAP. Changing a crate's `no_std`-ness is an architecture change.
2. **Hexagonal ports & adapters.** The core defines the traits it needs
   (`SeekSource`, `Catalog`, `Search`, `MutableStore`, `ArtStore`) and never
   imports an implementation. Every platform/transport is a swappable adapter.
3. **One on-disk format, `.soulidx`**, for desktop cache, DAP SD card, and
   server. Fixed-width records → O(1) seek; desktop-baked pre-sorted order
   tables → zero on-device sort; string heap; in-index playlists; `extended.blob`
   (Tier-2/3) and `art.blob` (1-bit e-ink). Spec:
   [`shared-catalog-index-architecture`](superpowers/specs/2026-11-08-shared-catalog-index-architecture.md),
   [`dap-export-and-blobs`](superpowers/specs/2026-11-08-dap-export-and-blobs.md).
4. **Transport is a port (`SeekSource`), one reader, three transports:** local
   file, SD card, **HTTP Range**. The same `IndexReader` reads `.soulidx`, and
   the same decoder reads audio, over any of them.
5. **Compact integer ids** (record position = id). Not UUID strings. Multi-source
   identity is a **compound `(source, id)`** ref.
6. **Desktop = multi-source aggregator** (`MultiCatalog`) over `{ local dirs… } ∪
   { remote servers… }`; **DAP = single SD source**. The DAP's simplicity is the
   `N = 1` case, not a special path.
7. **Local directories persist as local `.soulidx` caches** (dogfood the format,
   avoid re-scanning big libraries; re-bake incrementally on change).
8. **Tauri is a desktop app, never a website. Local is in-process** (Tauri IPC →
   library → file; audio direct). **No local HTTP server** (Tauri + local-first
   both call it an anti-pattern: latency, an open port, attack surface).
   "Remote" is a *source the backend reaches over HTTP Range*, not a UI
   transport.
9. **Immutable catalog vs. mutable state are split.** `.soulidx` is baked,
   read-only. Favorites / play-counts / ratings / last-position / user playlists
   live in an **append-only op-log with CRDT merge semantics** behind one
   `MutableStore` port (one port, many backings). "Sync intent, not state."
10. **The server is "a baker with a socket"**: same import/bake crates, serving
    `.soulidx` + audio + sync over HTTP Range. It keeps an **authoritative
    mutable store** (a real DB is fine *here*) and bakes **immutable `.soulidx`
    snapshots** (atomic swap) for clients. **SQLite/Postgres may appear only as a
    server-side adapter — never in the core or on the DAP.**
11. **Browse is pre-baked; search/filter at scale is a baked index, not a scan**
    (scans are the DAP-card fallback only — remote/large needs few round-trips).
12. **Multi-user / server write-sync is deferred but seam-reserved**: the op-log
    is multi-writer by construction; content upload + auth + permissions are a
    server-phase adapter, not a core change.

Full design discussion lives in the specs (§8). Deviating from any of the above
requires editing this section first.

## 3. Tier model (what §7 enforces)

Dependencies flow **downward only**. A crate may depend on its own tier or lower,
never higher. Every workspace crate must be classified in the tier manifest
(`crates/architecture-tests/tests/tiers.rs`); an unclassified crate fails the
build.

| Tier | Constraint | Members |
| --- | --- | --- |
| **T0 — kernel** | `no_std`, no-alloc, `forbid(unsafe)`; normal deps ⊆ {`embedded-io`} ∪ T0 | `soul-audio-core`, `soul-audio-tags`, `soul-audio-catalog`, `soul-audio-index` (reader surface) |
| **T1 — no_std + alloc** | `no_std`; may use `alloc`; no `std`-only dep; deps ⊆ {`embedded-io`} ∪ T0 ∪ T1 | `soul-audio-queue`, `soul-audio-engine`, `soul-audio-control` |
| **Firmware** | `no_std` binary; may depend on T0/T1 + embedded HAL; **never** a std-adapter | `soul-audio-firmware` |
| **Std adapter** | `std`; may depend on T0/T1 + std-adapters | `soul-audio-desktop`, `soul-audio-service`, `soul-audio-cli`, `soul-audio-import`; *planned:* `soul-audio-artbake`, `soul-audio-remote`, `soul-audio-server`, `soul-audio-multicatalog`, `soul-audio-mutable` |
| **Tests** | may depend on anything; nothing depends on it | `architecture-tests` |

> `soul-audio-index`'s **`build` feature is `std`** (the baker); its default
> surface (the reader) is T0. The tier is the *default* surface.

## 4. Ports (the seams)

| Port | Crate | Read/Write | Adapters |
| --- | --- | --- | --- |
| `SeekSource` | `soul-audio-core` | I/O | file (std) · SD (firmware) · HTTP-Range (remote) |
| `Catalog` / `Search` | `soul-audio-catalog` | read | local-scan · local-`.soulidx` · remote-`.soulidx` · `MultiCatalog` · (server DB) |
| `MutableStore` | `soul-audio-catalog` *(planned)* | write | op-log file (desktop) · card op-file (DAP) · DB+log (server) |
| `ArtStore` | `soul-audio-catalog` | read | 1-bit blob (DAP) · originals (desktop) |

Ports are defined by the consumer tier (T0/T1); adapters live in their platform
tier. A port must never force an owned collection across it (keeps the DAP
no-alloc and maps onto paged/range reads).

## 5. Deployment topology — three bodies, one core

| | Transport | Catalog | Audio | Render | Mutation |
| --- | --- | --- | --- | --- | --- |
| **Desktop** (Tauri) | in-process; HTTP Range for remote sources | `MultiCatalog` over local caches + remote | direct (local) / range (remote) | Svelte | op-log file |
| **DAP** (firmware) | SD | one SD `.soulidx` | direct from SD | e-ink 1-bit | card op-file |
| **Server** | serves HTTP Range | DB authority → baked `.soulidx` snapshot | range | — | DB + op-log sync |

## 6. Lifecycle, query mechanisms, sync (summary — depth in the specs)

- **Lifecycle:** scan (std) → catalog → **bake** (std) → `.soulidx` (+ blobs) →
  **read** (no-alloc) over file/SD/range → browse / filter / search / locate /
  decode / sink → **mutate** (op-log).
- **Three query mechanisms:** (1) **pre-baked** (orders, facet directories —
  albums/artists/folders/genres), (2) **shared runtime logic** (navigation, a
  no-alloc search/filter baseline, playlist ops), (3) **platform override**
  (desktop richer search index, the e-ink art baker).
- **Three sync flows:** (1) catalog read (server→client, range), (2) mutable
  state (client↔server, CRDT op-log), (3) content upload (client→server, hash-
  deduped, re-index). Flows 2–3 + multi-user are detailed in
  [`sources-sync-and-server`](superpowers/specs/2026-11-08-sources-sync-and-server.md).

## 7. Architecture tests (the guardrail)

Invariant → test. All in `crates/architecture-tests/` + the `xtask` embedded
gate. Adding a crate or a dep that violates these fails CI.

| Invariant (from §2/§3) | Enforced by |
| --- | --- |
| T0/T1 build `no_std` for the DAP | `cargo build --target thumbv7em-none-eabihf --no-default-features` (xtask gate) |
| The reader path never allocates | `no_alloc.rs` (counting `GlobalAlloc` probe) |
| Every crate is classified; deps flow one way; no_std tiers carry no std dep; firmware excludes std adapters | `tiers.rs` (manifest-driven, `cargo_metadata`) |
| Core/catalog/index stay lean (exact allowlists) | `layering.rs` |
| `.soulidx` round-trips (bake → read) | `soul-audio-index/tests/roundtrip.rs` |
| Desktop pipeline (scan → bake → read) | `soul-audio-import/tests/` |
| *(planned)* on-disk export round-trip over a `File` SeekSource | `soul-audio-import` export test |

**Reserved rules:** `tiers.rs` lists the planned std-adapter crates
(`artbake`/`remote`/`server`/`multicatalog`/`mutable`) with their tier; the test
skips crates not yet in the workspace and enforces the moment they appear — so a
new crate can only land in its documented tier.

## 8. Specs (depth)

- [`2026-11-08-shared-catalog-index-architecture`](superpowers/specs/2026-11-08-shared-catalog-index-architecture.md) — the shared layer, ports, `.soulidx`, test-first plan.
- [`2026-11-08-dap-export-and-blobs`](superpowers/specs/2026-11-08-dap-export-and-blobs.md) — export, `art.blob`/`extended.blob`, track→file location.
- [`2026-11-08-sources-sync-and-server`](superpowers/specs/2026-11-08-sources-sync-and-server.md) — sources/`MultiCatalog`, transports, op-log/CRDT, the three sync flows, the server.
- Playback/codec/engine design docs: see `docs/superpowers/specs/2026-08-*`…`2026-10-*`.

## 9. Deferred (seams reserved, not built)

Multi-user auth + permissions; content upload/ingest; server DB authority +
snapshot baker; OpenSubsonic facade; managed-library (sync-to-device, file
organization); cross-source dedup; incremental re-bake; enrichment backend. Each
is a new adapter or field behind an existing port — none changes the core, the
format, or the DAP.

## 10. Change process

To change the architecture: (1) discuss it; (2) edit §2/§3 of this document and
the relevant spec; (3) update the architecture tests to match; (4) only then
change code. A PR that alters tiers, ports, or the format without touching this
document and `tiers.rs`/`layering.rs` is wrong by construction.
