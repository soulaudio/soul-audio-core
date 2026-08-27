//! AAC-LC static tables: `sampling_frequency_index` → sample rate, the
//! per-`sfi` scalefactor-band (`swb`) count and line-offset tables for long
//! (1024-line) and short (128-line) windows, and the TNS scalefactor-band
//! cap. Transcribed verbatim from faad2 `libfaad/specrec.c` and
//! cross-checked against FFmpeg `libavcodec/aactab.c`
//! (`ff_aac_num_swb_1024/128`, `ff_swb_offset_1024/128`) — both sources
//! agree byte-for-byte on every table below.
//!
//! Self-check performed on every table at transcription time (also asserted
//! by this module's tests): each `swb_offset_*` table's **last** entry is
//! exactly `1024` (long) or `128` (short), and its entry count is
//! `num_swb + 1` for that table's `sfi` group.

use crate::decoder::DecodeError;

/// `sampling_frequency_index` (0..=12) → Hz — the canonical MPEG-4 table lives
/// in [`crate::demux`] (shared with the ADTS/MP4 demuxers); re-exported here so
/// [`resolve_sfi`] and this module's callers keep the `tables::SAMPLE_RATES`
/// path. `resolve_sfi` only ever matches against these 13 (13/14 reserved, 15
/// is the ASC-only explicit-rate escape).
pub(crate) use crate::demux::SAMPLE_RATES;

/// Number of scalefactor bands for a **long** (1024-line) window, indexed by
/// `sfi` 0..=11. `sfi == 12` (7350 Hz) is not a distinct entry here — per
/// faad2/FFmpeg's own `sfi`→table pointer arrays, 7350 Hz reuses the 8 kHz
/// (`sfi == 11`) table verbatim (there is no 13th distinct swb layout in the
/// spec), so [`num_swb_1024`]/[`swb_offset_1024`] fold `sfi == 12` into
/// index 11 rather than indexing past the end of a 12-entry table.
pub(crate) const NUM_SWB_1024: [u8; 12] = [41, 41, 47, 49, 49, 51, 47, 47, 43, 43, 43, 40];

/// Number of scalefactor bands for an **short** (128-line) window, indexed
/// by `sfi` 0..=11; see [`NUM_SWB_1024`] for the `sfi == 12` note (same
/// fold-to-11 rule applies here).
pub(crate) const NUM_SWB_128: [u8; 12] = [12, 12, 12, 14, 14, 14, 15, 15, 15, 15, 15, 15];

/// Long-window max `sfb` count for any `sfi` (the largest entry in
/// [`NUM_SWB_1024`], `sfi == 5` / 32 kHz) — bounds `sfb_cb`/`scale_factors`
/// storage in [`super::syntax::Ics`]. Short windows use at most 15 (see
/// [`NUM_SWB_128`]), comfortably under this cap.
pub(crate) const MAX_SFB: usize = 51;

/// `sample_rate` → `sampling_frequency_index`, by exact match against
/// [`SAMPLE_RATES`].
///
/// # Errors
/// [`DecodeError::Malformed`] if `sample_rate` isn't one of the 13 rates AAC
/// can express (a stream with any other rate cannot be a valid AAC-LC
/// stream, so this is the same "malformed input" bucket `adts::parse_header`
/// uses for a reserved `sampling_frequency_index`).
pub(crate) fn resolve_sfi(sample_rate: u32) -> Result<u8, DecodeError> {
    SAMPLE_RATES
        .iter()
        .position(|&r| r == sample_rate)
        .map(|i| i as u8)
        .ok_or(DecodeError::Malformed)
}

/// Safe (never-panicking) lookup into [`NUM_SWB_1024`]: `sfi >= 12` — i.e.
/// exactly `sfi == 12` (7350 Hz) for any value [`resolve_sfi`] can return,
/// but also any other out-of-table value defensively — folds to index 11 (8
/// kHz), matching faad2/FFmpeg's own `sfi`-to-table pointer aliasing.
pub(crate) fn num_swb_1024(sfi: u8) -> u8 {
    NUM_SWB_1024[(sfi as usize).min(NUM_SWB_1024.len() - 1)]
}

/// Safe (never-panicking) lookup into [`NUM_SWB_128`]; see [`num_swb_1024`].
pub(crate) fn num_swb_128(sfi: u8) -> u8 {
    NUM_SWB_128[(sfi as usize).min(NUM_SWB_128.len() - 1)]
}

#[rustfmt::skip]
static SWB_OFFSET_1024_96: [u16; 42] = [
       0,   4,   8,  12,  16,  20,  24,  28,
      32,  36,  40,  44,  48,  52,  56,  64,
      72,  80,  88,  96, 108, 120, 132, 144,
     156, 172, 188, 212, 240, 276, 320, 384,
     448, 512, 576, 640, 704, 768, 832, 896,
     960, 1024,
];

#[rustfmt::skip]
static SWB_OFFSET_1024_64: [u16; 48] = [
       0,   4,   8,  12,  16,  20,  24,  28,
      32,  36,  40,  44,  48,  52,  56,  64,
      72,  80,  88, 100, 112, 124, 140, 156,
     172, 192, 216, 240, 268, 304, 344, 384,
     424, 464, 504, 544, 584, 624, 664, 704,
     744, 784, 824, 864, 904, 944, 984, 1024,
];

#[rustfmt::skip]
static SWB_OFFSET_1024_48: [u16; 50] = [
       0,   4,   8,  12,  16,  20,  24,  28,
      32,  36,  40,  48,  56,  64,  72,  80,
      88,  96, 108, 120, 132, 144, 160, 176,
     196, 216, 240, 264, 292, 320, 352, 384,
     416, 448, 480, 512, 544, 576, 608, 640,
     672, 704, 736, 768, 800, 832, 864, 896,
     928, 1024,
];

#[rustfmt::skip]
static SWB_OFFSET_1024_32: [u16; 52] = [
       0,   4,   8,  12,  16,  20,  24,  28,
      32,  36,  40,  48,  56,  64,  72,  80,
      88,  96, 108, 120, 132, 144, 160, 176,
     196, 216, 240, 264, 292, 320, 352, 384,
     416, 448, 480, 512, 544, 576, 608, 640,
     672, 704, 736, 768, 800, 832, 864, 896,
     928, 960, 992, 1024,
];

#[rustfmt::skip]
static SWB_OFFSET_1024_24: [u16; 48] = [
       0,   4,   8,  12,  16,  20,  24,  28,
      32,  36,  40,  44,  52,  60,  68,  76,
      84,  92, 100, 108, 116, 124, 136, 148,
     160, 172, 188, 204, 220, 240, 260, 284,
     308, 336, 364, 396, 432, 468, 508, 552,
     600, 652, 704, 768, 832, 896, 960, 1024,
];

#[rustfmt::skip]
static SWB_OFFSET_1024_16: [u16; 44] = [
       0,   8,  16,  24,  32,  40,  48,  56,
      64,  72,  80,  88, 100, 112, 124, 136,
     148, 160, 172, 184, 196, 212, 228, 244,
     260, 280, 300, 320, 344, 368, 396, 424,
     456, 492, 532, 572, 616, 664, 716, 772,
     832, 896, 960, 1024,
];

#[rustfmt::skip]
static SWB_OFFSET_1024_8: [u16; 41] = [
       0,  12,  24,  36,  48,  60,  72,  84,
      96, 108, 120, 132, 144, 156, 172, 188,
     204, 220, 236, 252, 268, 288, 308, 328,
     348, 372, 396, 420, 448, 476, 508, 544,
     580, 620, 664, 712, 764, 820, 880, 944,
    1024,
];

#[rustfmt::skip]
static SWB_OFFSET_128_96: [u16; 13] = [
    0, 4, 8, 12, 16, 20, 24, 32, 40, 48, 64, 92, 128,
];

#[rustfmt::skip]
static SWB_OFFSET_128_48: [u16; 15] = [
    0, 4, 8, 12, 16, 20, 28, 36, 44, 56, 68, 80, 96, 112, 128,
];

#[rustfmt::skip]
static SWB_OFFSET_128_24: [u16; 16] = [
    0, 4, 8, 12, 16, 20, 24, 28, 36, 44, 52, 64, 76, 92, 108, 128,
];

#[rustfmt::skip]
static SWB_OFFSET_128_16: [u16; 16] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 40, 48, 60, 72, 88, 108, 128,
];

#[rustfmt::skip]
static SWB_OFFSET_128_8: [u16; 16] = [
    0, 4, 8, 12, 16, 20, 24, 28, 36, 44, 52, 60, 72, 88, 108, 128,
];

/// Long-window (1024-line) scalefactor-band offsets for `sfi`. The rate
/// groupings (`sfi` 0/1 share the 96 kHz table, 3/4 share 48 kHz, 6/7 share
/// 24 kHz, 8/9/10 share 16 kHz, and 11/12 share 8 kHz) are faad2/FFmpeg's own
/// `sfi`→table pointer aliasing, not an approximation. Never panics: any
/// `sfi` outside `0..=12` defensively falls through to the 8 kHz table.
pub(crate) fn swb_offset_1024(sfi: u8) -> &'static [u16] {
    match sfi {
        0 | 1 => &SWB_OFFSET_1024_96,
        2 => &SWB_OFFSET_1024_64,
        3 | 4 => &SWB_OFFSET_1024_48,
        5 => &SWB_OFFSET_1024_32,
        6 | 7 => &SWB_OFFSET_1024_24,
        8..=10 => &SWB_OFFSET_1024_16,
        _ => &SWB_OFFSET_1024_8, // 11, 12 (7350 Hz), and any defensive fallback
    }
}

/// Short-window (128-line) scalefactor-band offsets for `sfi`. Unlike the
/// long-window grouping, `sfi` 0/1/2 (96/88.2/64 kHz) share one table and
/// 3/4/5 (48/44.1/32 kHz) share another — verified against both faad2
/// `specrec.c` and FFmpeg's `ff_swb_offset_128[]` pointer table, which alias
/// identically. Never panics; see [`swb_offset_1024`].
pub(crate) fn swb_offset_128(sfi: u8) -> &'static [u16] {
    match sfi {
        0..=2 => &SWB_OFFSET_128_96,
        3..=5 => &SWB_OFFSET_128_48,
        6 | 7 => &SWB_OFFSET_128_24,
        8..=10 => &SWB_OFFSET_128_16,
        _ => &SWB_OFFSET_128_8, // 11, 12, and any defensive fallback
    }
}

/// `max_tns_sfb`, long-window, by `sfi` (`aac-reference.md` §9: `{0/1:31,
/// 2:34, 3:40, 4:42, 5:51, 6/7:46, 8-10:42, 11:39, 12:39}`). TNS isn't
/// *applied* until Task 6, but the parser (Task 4) still bounds every
/// `tns_data()` field it reads against this cap.
const MAX_TNS_SFB_LONG: [u8; 13] = [31, 31, 34, 40, 42, 51, 46, 46, 42, 42, 42, 39, 39];

/// `max_tns_sfb`, short-window, by `sfi` (`aac-reference.md` §9: `{0/1:9,
/// 2:10, 3-11:14}`, extended to `sfi 12` at the same 14 as `sfi 11`).
const MAX_TNS_SFB_SHORT: [u8; 13] = [9, 9, 10, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14];

/// TNS scalefactor-band cap for `sfi`/window type. Never panics: an
/// out-of-range `sfi` defensively clamps to the last (lowest-rate) entry.
pub(crate) fn max_tns_sfb(sfi: u8, short: bool) -> u8 {
    let table = if short {
        &MAX_TNS_SFB_SHORT
    } else {
        &MAX_TNS_SFB_LONG
    };
    table[(sfi as usize).min(table.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-check (per the task brief): every `swb_offset_1024_*` table's
    /// last entry is 1024 and its length is `num_swb + 1`, for every `sfi`
    /// this module resolves to that table.
    #[test]
    fn swb_offset_1024_tables_end_at_1024_with_correct_length() {
        for sfi in 0u8..=12 {
            let table = swb_offset_1024(sfi);
            assert_eq!(*table.last().unwrap(), 1024, "sfi {sfi}");
            assert_eq!(table.len(), usize::from(num_swb_1024(sfi)) + 1, "sfi {sfi}");
        }
    }

    /// Same self-check for the short-window (128-line) tables.
    #[test]
    fn swb_offset_128_tables_end_at_128_with_correct_length() {
        for sfi in 0u8..=12 {
            let table = swb_offset_128(sfi);
            assert_eq!(*table.last().unwrap(), 128, "sfi {sfi}");
            assert_eq!(table.len(), usize::from(num_swb_128(sfi)) + 1, "sfi {sfi}");
        }
    }

    #[test]
    fn resolve_sfi_matches_all_known_rates() {
        for (i, &rate) in SAMPLE_RATES.iter().enumerate() {
            assert_eq!(resolve_sfi(rate).unwrap(), i as u8);
        }
    }

    #[test]
    fn resolve_sfi_rejects_unknown_rate() {
        assert_eq!(resolve_sfi(44_101), Err(DecodeError::Malformed));
        assert_eq!(resolve_sfi(0), Err(DecodeError::Malformed));
    }

    #[test]
    fn table_lookups_never_panic_on_out_of_range_sfi() {
        for sfi in 0..=255u8 {
            let _ = swb_offset_1024(sfi);
            let _ = swb_offset_128(sfi);
            let _ = num_swb_1024(sfi);
            let _ = num_swb_128(sfi);
            let _ = max_tns_sfb(sfi, false);
            let _ = max_tns_sfb(sfi, true);
        }
    }
}
