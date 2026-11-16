# Decoder memory externalization — shrinking `AnyDecoder` for bare metal

> Spec. Target consumer: the SoulAudio DAP (STM32H7A3ZI — 1.18 MB DMA-capable
> AXI SRAM, 128 KB DTCM, `forbid(unsafe_code)`, no-alloc). Goal: make the
> decoder-open path fit on-target by shrinking `AnyDecoder` so moving it is
> cheap. Branch: `feat/embedded-profile`.

## 1. Problem — measured, not assumed

The DAP consumer measured the **transient stack** of its open→decode→pump path
(release build, host x86_64, player off-stack ≈ board `StaticCell`), bisected
one child-process per candidate size:

| Scenario | open→decode→pump peak stack |
|---|---|
| release, player on stack | ~2.1 MB |
| **release, player off-stack (board-representative)** | **~1.44 MB** |

Format-agnostic (WAV/FLAC/MP3 identical) → the cost is **decoder construction**,
not decode scratch. **1.44 MB exceeds the H7A3's entire 1.18 MB AXI SRAM.**

Root cause: `AnyDecoder` is **262 KB** and is moved **by value** ~5× through the
open chain — `dispatch::open_seekable` (returns it) → `Engine::provide(decoder)`
→ `Deck::load(decoder)` → `build_track(decoder)` → stored. Each hop is a real
`memcpy` (the optimizer does not elide them). ~5 × 262 KB ≈ 1.44 MB.

### Per-variant sizes (measured, `embedded-profile`)

| Variant | Size | | Variant | Size |
|---|---|---|---|---|
| **AnyDecoder (enum total)** | **262,224 B** | | Alac | 124,144 B |
| **WavPack** ← ceiling | 262,224 B | | Caf | 124,032 B |
| Aac / Matroska | 241,504 B | | Opus | 87,160 B |
| OggFlac / Flac | 160,240 B | | Mp3 | 44,240 B |
| | | | Wav | 18,576 B · Aiff | 104 B |

The enum is sized by its **largest** variant, so the ceiling only drops when the
**top** variants all come down. Every large variant has the same shape: an
**inline PCM block-staging buffer** sized for (max block × max channels):
- WavPack: `pcm: [i32; 65536]` = **256 KB** (interleaved block staging)
- FLAC: `planes: [[i32; 4608]; 8]` = **147 KB** (planar; `MAX_CHANNELS=8`, ungated)
- AAC: `MAX_CHANNELS=8` filterbank/SBR/DecodedFrame buffers (ungated)
- ALAC/Caf: `MAX_FRAME=4096` stereo frame buffers; Caf embeds ALAC

## 2. Why the fix is "externalize buffers" (research + hard constraint)

Web research on the canonical fixes for "moving a large enum = full `memcpy`"
([rust#47490](https://github.com/rust-lang/rust/issues/47490)):

1. **Box the large variant** — needs an allocator; and construction *still*
   copies into the box unless placement-new'd. ✗ (no-alloc).
2. **Placement-new via `MaybeUninit` / `pinned-init`** — guaranteed no stack
   copy, no_std — but expands to **`unsafe`**. ✗ (`forbid(unsafe_code)`).
3. **Pass large buffers by reference** (caller-provided / borrowed scratch). ✓

`forbid(unsafe_code)` + no-alloc **rules out (1) and (2)** — safe Rust has no
placement-new. The only constraint-compatible fix is **(3): externalize the
decoder's large buffers** so `AnyDecoder` becomes small and cheap to move.

This is already an **established pattern in this crate**: `formats/vorbis/arena.rs`
(caller-provided bump arena; decoder stores **offsets**, not references, to avoid
the self-reference trap under `forbid(unsafe_code)`), and Vorbis is deliberately
kept **out of `AnyDecoder`** and dispatched per-platform because it needs that
arena. The author already observed the stack blow-up ("8 channels overflowed
even a single decoder") and built the **profile-knob** mitigation
(`MAX_CHANNELS` gated by feature: desktop=8, embedded=2).

## 3. The self-reference constraint (the crux)

A `Deck` cannot own both a buffer *and* a decoder that borrows it — that's a
self-referential struct, impossible in safe Rust. Two escape routes, both used
here:
- **Owned generic storage + offsets** (Vorbis model): the decoder owns storage
  `P` generically; embedded supplies external storage (`&'static mut [i32]` from
  a board `StaticCell`), desktop supplies `Vec`. The decoder owns a *fat pointer*
  (16 B), not an inline array → small, cheap to move. Storage is owned by board
  `main`'s static (outlives the `Deck`), so no self-reference.
- **Pool-by-parameter**: the decoder stores no buffer; `decode(&mut self, pcm:
  &mut [i32])` receives staging each call from a `Deck`-owned pool. No stored
  borrow → no self-reference. More invasive to each decode loop.

This spec uses **owned generic storage** (matches the existing Vorbis seam).

## 4. Design — three combined levers

The ceiling drops fastest by combining levers, not one mega-refactor:

### Lever A — channel-profile gating (safe, high-leverage, mechanical)
Gate every channel-scaled inline buffer to **stereo** under `embedded-profile`
(FLAC `MAX_CHANNELS` 8→2, AAC 8→2, and audit the rest). Correct for the DAP:
it is **stereo-only** (`Deck` rejects non-stereo; surround is downmixed to stereo
at DAP import — see the dap-index doc), so >2-ch inline buffers are pure waste;
a >2-ch file is already a graceful `DecodeError::Unsupported`. Desktop keeps 8.
- FLAC 160 KB → ~50 KB; AAC 241 KB → TBD (measure); Matroska (embeds AAC/FLAC)
  shrinks with them.

### Lever B — externalize the block-staging buffers (the architectural core)
Some ceilings are **block-size-driven**, not channel-driven, and cannot be
profile-shrunk without rejecting valid files — **WavPack**'s `[i32; 65536]` can't
go below ~44,100 (ffmpeg stereo blocks) → ~176 KB floor. These must be
**externalized** via owned generic storage `P` (§3). Start with **WavPack** (the
ceiling, a single `pcm` field — the simplest proof of the pattern + the open-path
API change), measure, then extend to the other still-large variants (ALAC/Caf,
FLAC planes, AAC) as needed to hit the target.

### Lever C — reduce the move count (cheap multiplier win)
Collapse/inline `open_seekable → provide → load → build_track` so the decoder is
constructed closer to its final slot (fewer `memcpy` hops). Combined with A+B a
smaller `AnyDecoder` × fewer hops compounds. Verify with the release stack probe.

## 5. Acceptance gates (data-driven, each phase)
- **Size probe**: a throwaway test prints per-variant `size_of` (the §1.3 table)
  — each phase must show the targeted variants shrink; desktop sizes unchanged.
- **Release stack probe**: the DAP's `examples/stackprobe.rs` (release, boxed) —
  the open transient must fall from ~1.44 MB toward the target (**open transient
  ≤ ~256 KB**, so it fits an AXI-SRAM stack region alongside the shrunk audio
  path + framebuffers; ideally approach DTCM-fittable).
- **Correctness**: `cargo test` (desktop) AND `cargo test --features
  embedded-profile` both green; a >2-ch file is a clean `Unsupported`, never a
  panic; multichannel tests cfg-gated off under `embedded-profile` (as surround
  already is).
- **Zero desktop regression**: desktop decoders keep full channel/block caps and
  owned `Vec` storage; behavior identical.

## 6. Rollout (incremental, tests green each step)
- **Phase 1 — Lever A**: channel-gate FLAC + AAC (+ audit Matroska/others) to
  stereo under `embedded-profile`. Measure. Low risk; mechanical.
- **Phase 2 — Lever B on WavPack**: introduce the owned-generic-storage seam +
  the open-path storage-provisioning API; externalize WavPack `pcm`. Measure the
  ceiling drop + the stack-probe drop. Establishes the pattern + API.
- **Phase 3 — Lever B on the rest**: ALAC/Caf, FLAC planes, AAC staging as
  needed to reach the §5 target; Lever C move-count collapse.
- **Phase 4 — DAP side**: wire the board's `StaticCell` storage pool into
  `AudioPlayer`/`Deck` open; update `dap-arch-tests` budgets + the
  `stack_highwater.rs` conclusion to the real release numbers; re-run the probe.

Commits `sebastianstupak`, Conventional Commits, no AI trailers, hooks enforced.
