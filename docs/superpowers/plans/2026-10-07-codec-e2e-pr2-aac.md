# Codec E2E Completion — PR 2: AAC family Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the AAC-family gaps: bare `.aac` (ADTS) reports a duration via a
tag-layer frame-scan (#27), and AAC SBR (HE-AAC) gets committed playback-tier
e2e coverage. **PS (HE-AACv2) is deferred** to a follow-up commit on this branch
once a PS-capable encoder (fdkaac / libfdk_aac ffmpeg) is installed — `aac_mf`
only emits HE-AAC (SBR), never PS.

**Architecture:** `soul-audio-tags` has no ADTS case in `locate::sniff`, so bare
`.aac` falls to `Container::Unknown` → `Err(Unsupported)` → duration 0.0. We add
an `Adts` container (sniffed by the ADTS syncword + `layer == 00`) and a minimal
`read_adts` that frame-scans the 7-byte headers (advancing by the 13-bit
`aac_frame_length`), counting 1024 samples/frame and reading the core rate from
`sampling_frequency_index`. SBR already decodes (core goldens exist); it only
lacks a *playback* fixture — `tone-sbr.m4a` (HE-AAC via `aac_mf`), proven to
render at 439.5/659.2 Hz.

**Tech Stack:** Rust (edition 2024, `no_std`/no-alloc tags), ffmpeg `aac_mf`
(HE-AAC), `cargo xtask verify`.

**Branch:** `feat/codec-e2e-aac`, stacked on `feat/codec-e2e-dsd` (PR #45).

## Global Constraints

- **No AI trailers; GPG-signed commits** (lefthook enforces). Never `--no-gpg-sign`.
- **`soul-audio-tags` stays `no_std` + no-alloc + `#![forbid(unsafe_code)]`**, no
  new deps; thumbv7em cross-build in `cargo xtask verify` must stay green.
- **Workspace-wide clippy in pre-commit** → API + consumers commit atomically.
- **`app/src-tauri` is a separate workspace** — run its tests inside it.
- **ADTS duration semantics:** 1024 PCM samples per `raw_data_block`; duration
  seconds = `frames × 1024 / core_rate` (SBR-agnostic — the core rate halves and
  the sample count halves together, so seconds are invariant).
- **AAC sampling_frequency_index → Hz** (13 entries): `[96000, 88200, 64000,
  48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350]`.

---

## Task 1: ADTS sniff + tag-layer frame-scan (#27)

**Files:**
- Modify: `crates/soul-audio-tags/src/locate.rs` (add `Container::Adts` + sniff).
- Create: `crates/soul-audio-tags/src/adts.rs` (`read_adts` + header parse + tests).
- Modify: `crates/soul-audio-tags/src/lib.rs` (`mod adts;` + dispatch arm).

**Interfaces:**
- Produces: `pub fn read_adts<S: SeekSource>(src, scratch, sink) -> Result<StreamFacts, TagError>`
  (facts only — raw ADTS has no metadata; `scratch`/`sink` unused). `Container::Adts`.

- [ ] **Step 1: Add the `Adts` variant + sniff to `locate.rs`**

Add `Adts` to the `Container` enum (after `Dff`). In `sniff`, add before the
final `else`:

```rust
    } else if h.len() >= 2 && h[0] == 0xFF && (h[1] & 0xF6) == 0xF0 {
        // ADTS syncword 0xFFF + layer == 00 (0x06 mask rules out an MP3 frame,
        // whose layer bits are non-zero).
        Container::Adts
```

Add a sniff assertion to `sniffs_each_container_by_magic`:

```rust
        assert_eq!(sniff_bytes(b"\xFF\xF1\x50\x80"), Container::Adts);
```

- [ ] **Step 2: Run the sniff test (red → green for the new case)**

Run: `cargo test -p soul-audio-tags --lib locate`
Expected: PASS including the new ADTS case.

- [ ] **Step 3: Write `adts.rs` with the failing reader test first**

Create `crates/soul-audio-tags/src/adts.rs`:

```rust
//! Bare ADTS (`.aac`) duration facts. A raw AAC elementary stream has no
//! metadata container, so this only frame-scans the 7-byte ADTS headers —
//! advancing by the 13-bit `aac_frame_length`, counting 1024 PCM samples per
//! frame — to recover a duration the library view can show. Mirrors the
//! header fields `soul_audio_core::demux::adts` parses.

use crate::util::read_exact;
use crate::{StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

/// `sampling_frequency_index` → Hz (reserved indices `>= 13` are rejected).
const AAC_SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// Parse a 7-byte ADTS header → (core sample rate, channels, whole-frame length
/// in bytes). `Err` on a bad syncword/layer or a reserved sample-rate index.
fn parse_header(h: &[u8; 7]) -> Result<(u32, u16, usize), TagError> {
    // syncword(12)==0xFFF and layer(2)==00 (0x06 mask); layer!=00 is an MP3 frame.
    if h[0] != 0xFF || (h[1] & 0xF6) != 0xF0 {
        return Err(TagError::Malformed);
    }
    let sfi = usize::from((h[2] >> 2) & 0xF);
    let rate = *AAC_SAMPLE_RATES.get(sfi).ok_or(TagError::Malformed)?;
    let chan_cfg = ((h[2] & 0x1) << 2) | ((h[3] >> 6) & 0x3);
    let channels = match chan_cfg {
        0 => 2,         // "from PCE" — assume stereo for a facts estimate
        7 => 8,         // 7.1
        c => u16::from(c),
    };
    let frame_len = (usize::from(h[3] & 0x03) << 11)
        | (usize::from(h[4]) << 3)
        | (usize::from(h[5] >> 5) & 0x07);
    Ok((rate, channels, frame_len))
}

/// Frame-scan a raw ADTS stream for duration facts. No tags are emitted.
pub fn read_adts<S: SeekSource>(
    src: &mut S,
    _scratch: &mut [u8],
    _sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    let mut h = [0u8; 7];
    read_exact(src, &mut h)?; // at least one frame, else genuinely malformed
    let (rate, channels, first_len) = parse_header(&h)?;

    let mut frames: u64 = 0;
    let mut frame_len = first_len;
    loop {
        frames += 1;
        if frame_len < 7 {
            break; // guards the skip below; a <7 frame_len is malformed/last
        }
        // Advance from just-past-this-header to the next frame start.
        if src
            .seek(SeekFrom::Current((frame_len - 7) as i64))
            .is_err()
        {
            break;
        }
        // Read the next header; a clean EOF (0 bytes) ends the scan.
        match read_full(src, &mut h) {
            Ok(7) => match parse_header(&h) {
                Ok((_, _, len)) => frame_len = len,
                Err(_) => break, // resync garbage at the tail — stop, keep count
            },
            _ => break,
        }
    }

    Ok(StreamFacts {
        sample_rate: Some(rate),
        channels: Some(channels),
        total_frames: Some(frames * 1024),
    })
}

/// Read up to 7 bytes; returns how many were read (0 at clean EOF).
fn read_full<S: SeekSource>(src: &mut S, buf: &mut [u8; 7]) -> Result<usize, TagError> {
    let mut off = 0;
    while off < buf.len() {
        match embedded_io::Read::read(src, &mut buf[off..]).map_err(|_| TagError::Io)? {
            0 => break,
            k => off += k,
        }
    }
    Ok(off)
}

#[cfg(test)]
mod tests {
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::vec::Vec;

    /// Build `n` ADTS frames, each a 7-byte header + `payload` bytes, at 44.1 kHz
    /// stereo (sfi=4, channel_config=2). frame_len = 7 + payload.
    fn adts(n: usize, payload: usize) -> Vec<u8> {
        let frame_len = 7 + payload;
        let mut v = Vec::new();
        for _ in 0..n {
            // byte0 = 0xFF; byte1 = 0xF1 (sync + layer 00 + protection_absent=1)
            v.push(0xFF);
            v.push(0xF1);
            // byte2: profile(2)=01 LC | sfi(4)=4 | priv(1) | chan_cfg hi(1)=0
            //   = 01 0100 0 0 = 0x50
            v.push(0x50);
            // byte3: chan_cfg lo(2)=10 | orig/copy/home/copyr-id/copyr-start(... )
            //   + frame_len top 2 bits. chan_cfg=2 → low2 bits of the 3-bit field
            //   are 10 and the high bit came from byte2 bit0 (=0): so bits7-6 = 10.
            let fl = frame_len as u32;
            v.push(0x80 | ((fl >> 11) & 0x03) as u8);
            v.push(((fl >> 3) & 0xFF) as u8);
            v.push((((fl & 0x07) << 5) as u8) | 0x1F);
            v.push(0xFC);
            v.extend(core::iter::repeat(0u8).take(payload));
        }
        v
    }

    #[test]
    fn frame_scan_reports_duration_facts() {
        // 86 frames × 1024 / 44100 ≈ 1.997 s.
        let bytes = adts(86, 100);
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 16];
        let mut sink = crate::testfix::Collect::default();
        let facts = super::read_adts(&mut src, &mut scratch, &mut sink).unwrap();
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(86 * 1024));
        assert!(sink.fields.is_empty()); // raw ADTS carries no tags
    }

    #[test]
    fn empty_or_nonadts_is_malformed() {
        let mut src = SliceSource::new(b"\0\0\0\0\0\0\0");
        let mut scratch = [0u8; 16];
        let mut sink = crate::testfix::Collect::default();
        assert_eq!(
            super::read_adts(&mut src, &mut scratch, &mut sink),
            Err(crate::TagError::Malformed)
        );
    }
}
```

> **Implementer note:** verify the test's `byte3` chan_cfg bit layout against
> `parse_header` by running the test — `channel_config = ((h[2]&1)<<2) |
> ((h[3]>>6)&3)` must equal 2. If the hand-built header yields the wrong
> channels, adjust `byte2 bit0` / `byte3 bits7-6` until `channels == Some(2)`;
> the frame-count/rate assertions are the load-bearing ones.

- [ ] **Step 4: Wire `lib.rs`**

Add `mod adts;` (alphabetical, before `mod base64;` → actually after `mod adts;`
goes first). In the `read` match, add after the `Dff` arm:

```rust
        locate::Container::Adts => adts::read_adts(src, scratch, sink),
```

- [ ] **Step 5: Run the tags tests + clippy**

Run: `cargo test -p soul-audio-tags --lib adts locate` then
`cargo clippy -p soul-audio-tags --all-targets -- -D warnings`
Expected: green.

- [ ] **Step 6: Commit**

```bash
git add crates/soul-audio-tags/src/adts.rs crates/soul-audio-tags/src/locate.rs crates/soul-audio-tags/src/lib.rs
git commit -m "feat(tags): bare .aac (ADTS) duration via frame-scan (#27)"
```

---

## Task 2: SBR playback fixture

**Files:**
- Modify: `testdata/generate.sh` (add the `tone-sbr.m4a` encode line).
- Create (committed): `testdata/tone-sbr.m4a` (HE-AAC via `aac_mf`).

- [ ] **Step 1: Add the encode line to `generate.sh`**

After the Vorbis line (and near the other AAC lines), add:

```bash
# HE-AAC (SBR): aac_mf at a low bitrate emits explicit HE-AAC for MP4. Exercises
# the SBR decode path end-to-end in the playback tier (the ADTS-implicit-SBR path
# is covered by the core sbr.aac golden). NOTE: aac_mf emits plain LC for `-f
# adts`, so there is no ADTS SBR fixture here.
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a aac_mf -b:a 32k -movflags +faststart tone-sbr.m4a
```

Append `tone-sbr.m4a` to the final `ls -la` line.

- [ ] **Step 2: Generate + verify it is HE-AAC and renders in tune**

Run:
```bash
cd testdata && ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a aac_mf -b:a 32k -movflags +faststart tone-sbr.m4a
ffprobe -hide_banner -loglevel error -show_entries stream=profile -of csv=p=0 tone-sbr.m4a
```
Expected: `HE-AAC`. (Already validated: renders at 439.5 Hz / 659.2 Hz.)

- [ ] **Step 3: Commit the fixture**

```bash
git add testdata/generate.sh testdata/tone-sbr.m4a
git commit -m "test(fixtures): HE-AAC (SBR) playback fixture tone-sbr.m4a"
```

---

## Task 3: E2E wiring — ADTS duration flip + SBR coverage

**Files:**
- Modify: `app/src-tauri/tests/e2e.rs` (`per_format_durations`: `tone.aac` now
  reports ~2.0 s).
- Modify: `app/src-tauri/tests/fixtures.rs` (add `tone-sbr.m4a` decode-nonsilent).
- Modify: `crates/soul-audio-cli/tests/e2e.rs` (add `real_sbr_*` render pitch test).

- [ ] **Step 1: Flip the ADTS assertion in Tier-1 `per_format_durations`**

`tone.aac` now gets a tag-layer duration (~2.0 s), so it joins the PCM bucket and
the untagged bucket empties. Update the comment + counts:

```rust
    // ...: three DSD @ ~0.5 s (dsf, dsd128, dff), six containered/scanned-PCM
    // @ ~2.0 s (incl. tone.aac, whose duration now comes from the ADTS
    // frame-scan). No untagged fixtures remain.
    let mut secs: Vec<f64> = views.iter().map(|v| v.duration_secs).collect();
    secs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let dsd = secs.iter().filter(|d| (**d - 0.5).abs() < 0.06).count();
    let pcm = secs.iter().filter(|d| (**d - 2.0).abs() < 0.16).count();
    let untagged = secs.iter().filter(|d| **d == 0.0).count();
    assert_eq!(dsd, 3, "three DSD fixtures ~0.5s: {secs:?}");
    assert_eq!(pcm, 6, "six PCM fixtures ~2.0s (incl. ADTS): {secs:?}");
    assert_eq!(untagged, 0, "ADTS now has a frame-scan duration: {secs:?}");
```

- [ ] **Step 2: Add SBR to `fixtures.rs` decode-nonsilent**

Add `"tone-sbr.m4a"` to the `playable_fixtures_decode_nonsilent` array.

- [ ] **Step 3: Add a CLI SBR render pitch test**

In `crates/soul-audio-cli/tests/e2e.rs`, after the AAC tests:

```rust
#[test]
fn real_he_aac_sbr_renders_stereo_tones() {
    // HE-AAC (explicit SBR in MP4): decode → SBR (2× rate) → resample → file.
    // Pitch-only (lossy), like the AAC-LC render goldens.
    let out = tmp("sbr_out.pcm");
    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .args(["render"])
        .arg(fixture("tone-sbr.m4a"))
        .args(["--out"])
        .arg(&out)
        .args(["--rate", "48000", "--channels", "2"])
        .assert()
        .success();

    let pcm = read_pcm(&out);
    let n = 16_384;
    let left = analyze(&channel_window(&pcm, 2, 0, 4000, n), 48_000);
    let right = analyze(&channel_window(&pcm, 2, 1, 4000, n), 48_000);
    assert!((left.peak_hz - 440.0).abs() < 6.0, "SBR left {:.1} Hz", left.peak_hz);
    assert!((right.peak_hz - 659.25).abs() < 6.0, "SBR right {:.1} Hz", right.peak_hz);
}
```

- [ ] **Step 4: Run both workspaces' affected tests**

Run: `cargo test -p soul-audio-cli --test e2e real_he_aac`
Then (inside `app/src-tauri`): `SOUL_PLAYER_OFFLINE=1 cargo test --test e2e --test fixtures`
Expected: all green — ADTS duration now ~2.0 s (pcm=6, untagged=0); SBR renders
in tune and decodes non-silent.

- [ ] **Step 5: Commit**

```bash
git add app/src-tauri/tests/e2e.rs app/src-tauri/tests/fixtures.rs crates/soul-audio-cli/tests/e2e.rs
git commit -m "test: ADTS duration flip + HE-AAC SBR playback e2e (#27)"
```

---

## Task 4: qa-audio SBR + full verify + review + PR

- [ ] **Step 1 (optional polish): add SBR to `xtask qa-audio`**

If `qa_audio()` lists fixtures to play by ear, add `tone-sbr.m4a` with a
"should sound like the same chord (HE-AAC/SBR)" note, mirroring the `tone-aac.m4a`
entry. Skip if it complicates the list; it is a by-ear aid only.

- [ ] **Step 2: Root verify**

Run: `cargo xtask verify`
Expected: `verify: OK`.

- [ ] **Step 3: App verify**

Run (inside `app/src-tauri`): `SOUL_PLAYER_OFFLINE=1 cargo test`
Expected: all green.

- [ ] **Step 4: Review the diff, then push + open the stacked PR**

Run the code-review skill on `git diff feat/codec-e2e-dsd...HEAD`; fix findings.
Then:
```bash
git push -u origin feat/codec-e2e-aac
tea pr create --base feat/codec-e2e-dsd --head feat/codec-e2e-aac --title "AAC: ADTS duration (#27) + HE-AAC SBR e2e" --description "…"
```
PR body: ADTS frame-scan duration, SBR playback fixture + render/decode e2e, the
Tier-1 assertion flip. Note **PS deferred** (needs a PS-capable encoder; `aac_mf`
is SBR-only) — to be added as a follow-up commit on this branch. Base the PR on
`feat/codec-e2e-dsd` (stacked on #45).

---

## PS follow-up (deferred — do when a PS encoder is installed)

Once `fdkaac` or an fdk_aac-enabled ffmpeg is on PATH: encode a synthetic
`tone-ps.m4a` (HE-AACv2) + `tone-ps.ref.pcm` (ffmpeg's own PS decode); add a
committed per-channel segmental-SNR golden in `crates/soul-audio-core/tests/decode.rs`
(the 1→2 stereo-promotion path), a CLI render pitch test, and `fixtures.rs`
decode-nonsilent. Fully synthetic → no license constraint (unlike the gitignored
Coding Technologies `ps-local.*`). Commit on `feat/codec-e2e-aac`.

---

## Self-Review

**Spec coverage (AAC slice of `2026-10-07-codec-e2e-completion-design.md`):**
- #27 ADTS duration → Task 1 + Task 3 Step 1. 
- SBR playback fixture + CLI + Tier-1 → Tasks 2 + 3. 
- PS → explicitly deferred section (user chose "do SBR+ADTS now, add PS later").

**Placeholder scan:** the chan_cfg bit-layout in the test builder is flagged with
an "Implementer note" to verify-by-running (the rate/frame-count asserts are the
real gate); the PR body is author-time. No other placeholders.

**Type consistency:** `read_adts(src, scratch, sink) -> Result<StreamFacts,
TagError>` matches the `lib.rs` dispatch and the other `read_*` readers.
`Container::Adts` added to the enum consumed by `sniff` and the dispatch.
