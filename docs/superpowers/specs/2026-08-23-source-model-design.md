# soul-audio-core — Source Model (I/O boundary)

**Date:** 2026-08-23
**Status:** Approved — implementing
**Scope:** The **source** side of the core model only. Sample/`StreamInfo`
changes, per-format decoders (WAV/FLAC/MP3/DSD/DSF), and a demuxer are explicit
future work that builds *on top of* this boundary.

## 1. Purpose

Define the abstraction decoders use to pull bytes, such that a **single generic
decoder body compiles and runs identically** on the STM32 (`no_std`, no-alloc,
sync) and on desktop (`std`), reading from any backend — in-flash slice, SD-card
file, desktop `File`, or (desktop-only, optional) a sync `TcpStream`.

The system must support **reading a file, streaming (incremental) decode, and
seeking**. On embedded, "streaming" means incremental decode from a **seekable
SD-card file** (read a chunk → decode into a caller buffer → play via DMA →
repeat), *not* network ingest. **Embedded never streams from the network**;
network, if any, is desktop-only and sync.

## 2. Fixed decisions

| Decision | Choice | Rationale |
|---|---|---|
| I/O primitive | Build on **`embedded-io`** | The `no_std` mirror of `std::io` (Read/BufRead/Seek + a no-alloc error model). Zero deps of its own, portable to desktop via `embedded-io-adapters` (`FromStd`), and the ecosystem lingua franca (SD stacks / HALs already speak it). Runs on desktop too — `no_std` ⊆ `std`. |
| API shape | **Owned super-traits** over embedded-io, blanket-impl'd | Decoders/users speak `soul_audio_core::{Source, BufSource, SeekSource}`; embedded-io stays a hidden implementation detail we can swap without breaking our public API. One home for future parsing helpers. |
| Concurrency | **Sync** | No async anywhere near core. SD + flash + desktop file/stream are all sync. (`embedded-io-async` + an async decoder API remain a possible *parallel* future addition, deferred hard.) |
| Memory | **No-alloc** | A slice is its own buffer; a wrapped source is buffered via a **caller-provided `&mut [u8]`** (std's `BufReader` heap-allocates; ours cannot). |
| Seek as capability | **Separate tier** | Seek is a *capability gate*, not a mode: forward-only decoders bind `Source` (work over anything, incl. a stream); playback seeking binds `SeekSource` (the type system forbids seeking a non-seekable source). |

## 3. The capability ladder

Each tier is the one below plus one new power. All three are **our traits**,
blanket-impl'd over the corresponding `embedded-io` trait, so anything that is
`embedded_io::Read` (etc.) automatically satisfies them.

```rust
// src/io.rs
use embedded_io::{BufRead, Read, Seek};

/// Sequential byte input — the universal baseline (flash, SD, file, socket).
pub trait Source: Read {}
impl<T: Read + ?Sized> Source for T {}

/// A `Source` you can peek without consuming (cheap header / frame-sync parsing).
pub trait BufSource: Source + BufRead {}
impl<T: Read + BufRead + ?Sized> BufSource for T {}

/// A `Source` with random access. Playback seeking binds HERE.
pub trait SeekSource: Source + Seek {}
impl<T: Read + Seek + ?Sized> SeekSource for T {}
```

- **`Source`** = `embedded_io::Read`. Forward bytes; may return partial reads.
- **`BufSource`** = `Source` + `embedded_io::BufRead` (`fill_buf` / `consume`).
- **`SeekSource`** = `Source` + `embedded_io::Seek` (byte offsets, `u64`/`i64`).

Time-based seeking (jump to time `T`) is a **per-format decoder** concern layered
on top of byte-`seek` — it is *not* part of the source model.

## 4. Concrete sources (both no-alloc)

### 4.1 `SliceSource<'a>` (evolves today's `SliceReader`)

In-memory `&[u8]`. Implements **all three tiers**: `Read`, `BufRead` (the whole
slice is the buffer — zero extra memory), and `Seek`. Used for `include_bytes!`
flash playback on firmware and for test fixtures. Reads are infallible; seeking
to a negative offset is an error (`ErrorKind::InvalidInput`); seeking at/after
end is allowed (subsequent reads return `0`). `SliceReader` is **renamed** to
`SliceSource` (no external consumers; firmware + `lib.rs` updated).

### 4.2 `BufReader<'buf, S>`

Wraps any `Source` and makes it a `BufSource` using a **caller-provided**
`&'buf mut [u8]` scratch buffer. If `S: Seek`, `BufReader` is also a
`SeekSource` (a seek discards the buffer, then seeks the inner source).

```rust
pub struct BufReader<'buf, S> {
    inner: S,
    buf: &'buf mut [u8],
    filled: usize, // valid bytes in buf
    pos: usize,    // read cursor within [0, filled)
}
```

- `Read` / `BufRead`: refill from `inner` when the cursor reaches `filled`.
- `Seek` (when `S: Seek`): reset `filled = pos = 0`, then `inner.seek(..)`.
- `Error = <S as ErrorType>::Error`.

(The large-read bypass optimization — reading straight into the caller buffer
when it exceeds the scratch buffer — is a possible later refinement, not
required for correctness.)

## 5. Backend → tier (platform-tagged)

| Backend | `Source` | `BufSource` | `SeekSource` | Where / how |
|---|:--:|:--:|:--:|---|
| slice `&[u8]` | ✅ | ✅ own buffer | ✅ | both — `include_bytes!` flash, fixtures |
| SD-card file | ✅ | ✅ via `BufReader` | ✅ | embedded — shell adapter over `embedded-sdmmc` / block device |
| `std::fs::File` | ✅ | ✅ via `BufReader` | ✅ | desktop — `embedded_io_adapters::std::FromStd` |
| sync `TcpStream` | ✅ | ✅ via `BufReader` | ❌ | desktop only, optional |

The SD adapter and the `FromStd` bridge live in the **shells**, not core. Core's
only dependency stays `embedded-io` (enforced by `architecture-tests/layering.rs`).

## 6. Decoder-side contract (guidance; decoders are future work)

Decoders bind the **minimum tier** they need:

```rust
impl<S: Source>     Decoder       for WavDecoder<S> { /* forward, streams */ }
impl<S: SeekSource> WavDecoder<S> { fn seek_to(&mut self, /* time→byte */) { } }
```

Over a `TcpStream` you get forward decode only; `seek_to` doesn't exist for a
non-seekable `S`. Over a file/slice you additionally get seeking.

## 7. Files touched (this session)

- `src/io.rs` — add `Source` / `BufSource` / `SeekSource` + blanket impls; evolve
  `SliceReader` → `SliceSource` (add `BufRead` + `Seek`); add `BufReader`.
- `src/lib.rs` — re-export `Source, BufSource, SeekSource, SliceSource, BufReader`
  (drop the `SliceReader` re-export).
- `crates/soul-audio-firmware/src/main.rs` — `SliceReader` → `SliceSource`.
- Tests — `SliceSource` read/peek/seek; `BufReader` fill/consume/refill +
  seek-invalidates-buffer; static assertions that `SliceSource: SeekSource +
  BufSource`.

## 8. Testing & invariants

- **Unit** (`src/io.rs` `#[cfg(test)]`): `SliceSource` read→EOF, `fill_buf`/
  `consume`, `Seek` (Start/Current/End + negative → error); `BufReader` read
  across refills, `fill_buf`/`consume`, and seek clearing the buffer.
- **Static assertions:** `fn _assert<S: SeekSource + BufSource>() {}` instantiated
  with `SliceSource<'static>` (compile-time proof the tiers hold).
- **Architecture (unchanged, must stay green):** `layering.rs` (allowlist =
  `embedded-io` only), `no_alloc.rs` (zero allocations — slice + caller buffer),
  and the `thumbv7em-none-eabihf --no-default-features` `no_std` build gate.

## 9. Out of scope (deferred)

Sample/`StreamInfo` model changes; per-format decoders; time-based seek API;
demuxer; async sources / `embedded-io-async`; any network on embedded; the
large-read bypass optimization in `BufReader`.

## 10. Success criteria

- `cargo test` (host) green, including new source tests + unchanged architecture
  tests.
- `cargo clippy --all-targets -D warnings` and `cargo fmt --check` clean.
- `soul-audio-core` builds for `thumbv7em-none-eabihf --no-default-features`
  (the `no_std`/no-alloc gate) and firmware builds for the target.
- Public API exposes `Source`, `BufSource`, `SeekSource`, `SliceSource`,
  `BufReader`; a decoder can bind `Source` for streaming and `SeekSource` for
  seeking with no change to the source layer.
