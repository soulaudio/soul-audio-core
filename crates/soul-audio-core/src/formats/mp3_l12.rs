//! Self-contained MPEG-1/2 **Audio Layer I and Layer II** decoder.
//!
//! Layer III (the common `.mp3`) is handled by the minimp3-derived
//! [`mp3_codec`](crate::formats::mp3_codec); Layers I and II use a completely
//! different, much simpler coding (subband PCM with per-subband bit allocation
//! and scalefactors — no MDCT, no Huffman, no bit reservoir), so they get their
//! own module rather than bending the Layer III path.
//!
//! This is an integer-only port of **kjmp2** (Martin J. Fiedler, zlib licence)
//! for Layer II, extended with the Layer I coding variant (both share the
//! polyphase synthesis filterbank and requantiser). Being fixed-point it is
//! deterministic, `no_std`, alloc-free and `forbid(unsafe_code)`-clean, and it
//! decodes **bit-exactly** to kjmp2's own output (the crate's golden vectors are
//! kjmp2-decoded). Output is interleaved `i32` in 16-bit range, matching the
//! WAV/FLAC/Layer-III convention; mono streams yield one channel (kjmp2 always
//! emits duplicated stereo — our mono output equals its left channel).
//!
//! Acknowledgement: the synthesis window `D`, the matrixing matrix `N`, and the
//! Layer II allocation/quantiser tables are from kjmp2.

// This is a fixed-point DSP port: the loops carry explicit indices that
// cross-index several 2-D/3-D arrays (`sample[ch][sb][idx]`) and appear in
// address arithmetic (`voffs + i`, `(i << 6) + j`). Rewriting them as iterators
// would obscure the correspondence with the reference, so allow index loops.
#![allow(clippy::needless_range_loop)]

// channel-mode field: 0 = stereo, 1 = joint stereo, 2 = dual, 3 = mono.
const MODE_JOINT: u32 = 1;
const MODE_MONO: u32 = 3;

/// One quantiser class: number of levels, whether three samples are grouped into
/// one codeword, and the codeword width in bits.
struct Quant {
    nlevels: i32,
    grouping: bool,
    cw_bits: u32,
}

/// Layer II quantiser classes (kjmp2 `quantizer_table`), indexed by
/// `quant_lut_step4` value minus one.
#[rustfmt::skip]
static QUANT: [Quant; 17] = [
    Quant { nlevels:     3, grouping: true,  cw_bits:  5 },
    Quant { nlevels:     5, grouping: true,  cw_bits:  7 },
    Quant { nlevels:     7, grouping: false, cw_bits:  3 },
    Quant { nlevels:     9, grouping: true,  cw_bits: 10 },
    Quant { nlevels:    15, grouping: false, cw_bits:  4 },
    Quant { nlevels:    31, grouping: false, cw_bits:  5 },
    Quant { nlevels:    63, grouping: false, cw_bits:  6 },
    Quant { nlevels:   127, grouping: false, cw_bits:  7 },
    Quant { nlevels:   255, grouping: false, cw_bits:  8 },
    Quant { nlevels:   511, grouping: false, cw_bits:  9 },
    Quant { nlevels:  1023, grouping: false, cw_bits: 10 },
    Quant { nlevels:  2047, grouping: false, cw_bits: 11 },
    Quant { nlevels:  4095, grouping: false, cw_bits: 12 },
    Quant { nlevels:  8191, grouping: false, cw_bits: 13 },
    Quant { nlevels: 16383, grouping: false, cw_bits: 14 },
    Quant { nlevels: 32767, grouping: false, cw_bits: 15 },
    Quant { nlevels: 65535, grouping: false, cw_bits: 16 },
];

/// Scalefactor base values (24-bit fixed-point) for index mod 3 (kjmp2).
const SCF_BASE: [i32; 3] = [0x0200_0000, 0x0196_5FEA, 0x0142_8A30];

// Layer II allocation-table selection (kjmp2 `quant_lut_step*`).

/// step 1: (mono?0:stereo?1) × bitrate-index-minus-1 → bitrate class.
#[rustfmt::skip]
static QUANT_LUT_STEP1: [[u8; 16]; 2] = [
    [0, 0, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 0, 0], // mono
    [0, 0, 0, 0, 0, 0, 1, 1, 1, 2, 2, 2, 2, 2, 0, 0], // stereo
];

// step 2 table codes: `sblimit | (table_idx << 6)`.
const TAB_A: u8 = 27 | 64; // high-rate, sblimit 27
const TAB_B: u8 = 30 | 64; // high-rate, sblimit 30
const TAB_C: u8 = 8; //       low-rate, sblimit 8
const TAB_D: u8 = 12; //      low-rate, sblimit 12

/// step 2: bitrate class × sample-rate (44.1/48/32 kHz) → table code.
static QUANT_LUT_STEP2: [[u8; 4]; 3] = [
    [TAB_C, TAB_C, TAB_D, 0],
    [TAB_A, TAB_A, TAB_A, 0],
    [TAB_B, TAB_A, TAB_B, 0],
];

/// step 3: table × subband → `(nbal << 4) | row`. Entries past each table's
/// `sblimit` are unused (left zero).
#[rustfmt::skip]
static QUANT_LUT_STEP3: [[u8; 32]; 3] = [
    // low-rate (3-B.2c / 3-B.2d)
    [0x44, 0x44,
     0x34, 0x34, 0x34, 0x34, 0x34, 0x34, 0x34, 0x34, 0x34, 0x34,
     0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    // high-rate (3-B.2a / 3-B.2b)
    [0x43, 0x43, 0x43,
     0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x42,
     0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x31, 0x31,
     0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20,
     0, 0],
    // MPEG-2 LSR (ISO 13818-3 B.2)
    [0x45, 0x45, 0x45, 0x45,
     0x34, 0x34, 0x34, 0x34, 0x34, 0x34, 0x34,
     0x24, 0x24, 0x24, 0x24, 0x24, 0x24, 0x24, 0x24, 0x24, 0x24,
     0x24, 0x24, 0x24, 0x24, 0x24, 0x24, 0x24, 0x24, 0x24,
     0, 0],
];

/// step 4: table row × allocation code → quantiser index (0 = no allocation,
/// else index+1 into [`QUANT`]).
#[rustfmt::skip]
static QUANT_LUT_STEP4: [[u8; 16]; 6] = [
    [0, 1, 2, 17, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 2, 3, 4, 5, 6, 17, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 17],
    [0, 1, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17],
    [0, 1, 2, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 17],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
];

include!("mp3_l12_tables.rs"); // static D: [i32; 512] and N: [[i32; 32]; 64]

/// MSB-first bit reader over a byte slice; reads past the end yield zero bits
/// (frames handed here are complete, so this only pads a trailing partial byte).
struct Bits<'a> {
    data: &'a [u8],
    /// Absolute bit position from the start of `data`.
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn skip(&mut self, n: usize) {
        self.pos += n;
    }
    fn get(&mut self, n: u32) -> u32 {
        let mut r = 0u32;
        for _ in 0..n {
            let byte = self.pos >> 3;
            let bit = 7 - (self.pos & 7);
            let b = if byte < self.data.len() {
                (self.data[byte] >> bit) & 1
            } else {
                0
            };
            r = (r << 1) | u32::from(b);
            self.pos += 1;
        }
        r
    }
}

/// Persistent Layer I/II synthesis state: the two per-channel `V` delay lines and
/// the rotating write offset. Small and self-contained (8 KiB), reset on seek.
pub(crate) struct Layer12 {
    v: [[i32; 1024]; 2],
    voffs: usize,
}

impl Layer12 {
    pub(crate) fn new() -> Self {
        Self {
            v: [[0; 1024]; 2],
            voffs: 0,
        }
    }

    /// Clear the filterbank delay line (used on seek / stream restart).
    pub(crate) fn reset(&mut self) {
        self.v = [[0; 1024]; 2];
        self.voffs = 0;
    }

    /// Decode one Layer I or Layer II frame (`frame` = full MPEG frame, header
    /// included) into interleaved `i32` PCM. Returns the interleaved sample count
    /// (`samples_per_frame * channels`), or `None` if the header is malformed /
    /// unsupported.
    pub(crate) fn decode_frame(&mut self, frame: &[u8], pcm: &mut [i32]) -> Option<usize> {
        if frame.len() < 4 || frame[0] != 0xFF {
            return None;
        }
        let mpeg1 = (frame[1] >> 3) & 3 == 3;
        let layer_code = (frame[1] >> 1) & 3; // 3 = Layer I, 2 = Layer II
        let crc_present = frame[1] & 1 == 0;

        // Bitstream begins at byte 2 (sync + first header byte already consumed).
        let mut b = Bits::new(frame);
        b.skip(16);
        let bitrate_index = b.get(4);
        if bitrate_index == 0 || bitrate_index == 15 {
            return None; // free-format or reserved
        }
        let bri_m1 = (bitrate_index - 1) as usize; // 0..13
        let srate = b.get(2);
        if srate == 3 {
            return None;
        }
        let _padding = b.get(1);
        let _private = b.get(1);
        let mode = b.get(2);

        // mode_extension → joint-stereo bound.
        let bound = if mode == MODE_JOINT {
            (b.get(2) + 1) << 2
        } else {
            b.get(2);
            if mode == MODE_MONO { 0 } else { 32 }
        } as usize;
        b.get(4); // copyright / original / emphasis
        if crc_present {
            b.get(16);
        }

        let nch = if mode == MODE_MONO { 1 } else { 2 };
        match layer_code {
            3 => self.decode_layer1(&mut b, mode, bound, nch, pcm),
            2 => self.decode_layer2(&mut b, mpeg1, bri_m1, srate as usize, mode, bound, nch, pcm),
            _ => None,
        }
    }

    /// Layer II frame body (after the header) → PCM. Mirrors kjmp2's
    /// `kjmp2_decode_frame` decode section.
    #[allow(clippy::too_many_arguments)]
    fn decode_layer2(
        &mut self,
        b: &mut Bits,
        mpeg1: bool,
        bri_m1: usize,
        srate: usize,
        mode: u32,
        mut bound: usize,
        nch: usize,
        pcm: &mut [i32],
    ) -> Option<usize> {
        // Table selection → allocation table index and active-subband count.
        let (table_idx, sblimit) = if mpeg1 {
            let cls = QUANT_LUT_STEP1[usize::from(mode != MODE_MONO)][bri_m1] as usize;
            let code = QUANT_LUT_STEP2[cls][srate];
            ((code >> 6) as usize, (code & 63) as usize)
        } else {
            (2usize, 30usize) // MPEG-2 / 2.5 (LSF): fixed table 2, sblimit 30
        };
        if bound > sblimit {
            bound = sblimit;
        }

        // allocation[ch][sb] = Some(index into QUANT) or None (no bits).
        let mut alloc: [[Option<usize>; 32]; 2] = [[None; 32]; 2];
        for sb in 0..bound {
            for ch in 0..2 {
                alloc[ch][sb] = read_alloc_l2(b, table_idx, sb);
            }
        }
        for sb in bound..sblimit {
            let a = read_alloc_l2(b, table_idx, sb);
            alloc[0][sb] = a;
            alloc[1][sb] = a;
        }

        // scalefactor selection information (2 bits per allocated subband/ch).
        let mut scfsi = [[0u32; 32]; 2];
        for sb in 0..sblimit {
            for ch in 0..nch {
                if alloc[ch][sb].is_some() {
                    scfsi[ch][sb] = b.get(2);
                }
            }
            if mode == MODE_MONO {
                scfsi[1][sb] = scfsi[0][sb];
            }
        }

        // scalefactors (raw 6-bit indices; resolved at requantisation).
        let mut scf = [[[0u32; 3]; 32]; 2];
        for sb in 0..sblimit {
            for ch in 0..nch {
                if alloc[ch][sb].is_some() {
                    match scfsi[ch][sb] {
                        0 => {
                            scf[ch][sb][0] = b.get(6);
                            scf[ch][sb][1] = b.get(6);
                            scf[ch][sb][2] = b.get(6);
                        }
                        1 => {
                            let s = b.get(6);
                            scf[ch][sb][0] = s;
                            scf[ch][sb][1] = s;
                            scf[ch][sb][2] = b.get(6);
                        }
                        2 => {
                            let s = b.get(6);
                            scf[ch][sb][0] = s;
                            scf[ch][sb][1] = s;
                            scf[ch][sb][2] = s;
                        }
                        _ => {
                            scf[ch][sb][0] = b.get(6);
                            let s = b.get(6);
                            scf[ch][sb][1] = s;
                            scf[ch][sb][2] = s;
                        }
                    }
                }
            }
            if mode == MODE_MONO {
                scf[1][sb] = scf[0][sb];
            }
        }

        // Samples + synthesis: 3 scalefactor parts × 4 granules, each granule
        // carrying 3 sub-block time slots per subband.
        let mut sample = [[[0i32; 3]; 32]; 2];
        let mut cur = [[0i32; 32]; 2];
        for part in 0..3 {
            for gr in 0..4 {
                for sb in 0..bound {
                    for ch in 0..2 {
                        read_samples_l2(b, alloc[ch][sb], scf[ch][sb][part], &mut sample[ch][sb]);
                    }
                }
                for sb in bound..sblimit {
                    read_samples_l2(b, alloc[0][sb], scf[0][sb][part], &mut sample[0][sb]);
                    sample[1][sb] = sample[0][sb];
                }
                for ch in 0..2 {
                    for sb in sblimit..32 {
                        sample[ch][sb] = [0; 3];
                    }
                }

                let giter = part * 4 + gr;
                for idx in 0..3 {
                    for ch in 0..nch {
                        for sb in 0..32 {
                            cur[ch][sb] = sample[ch][sb][idx];
                        }
                    }
                    let out_off = (giter * 3 + idx) * 32 * nch;
                    self.synth_subblock(&cur, nch, pcm, out_off);
                }
            }
        }
        Some(1152 * nch)
    }

    /// Layer I frame body → PCM. 32 subbands, direct 4-bit allocation, a single
    /// scalefactor per subband, and 12 time slots (no grouping, no scfsi).
    fn decode_layer1(
        &mut self,
        b: &mut Bits,
        mode: u32,
        bound: usize,
        nch: usize,
        pcm: &mut [i32],
    ) -> Option<usize> {
        // Bit allocation: 4 bits per subband/ch → codeword width (0 = no bits).
        let mut nb = [[0u32; 32]; 2];
        for sb in 0..bound {
            for ch in 0..2 {
                nb[ch][sb] = layer1_nbits(b.get(4));
            }
        }
        for sb in bound..32 {
            let n = layer1_nbits(b.get(4));
            nb[0][sb] = n;
            nb[1][sb] = n;
        }

        // One 6-bit scalefactor per allocated subband/ch.
        let mut scf = [[0u32; 32]; 2];
        for sb in 0..32 {
            for ch in 0..nch {
                if nb[ch][sb] != 0 {
                    scf[ch][sb] = b.get(6);
                }
            }
            if mode == MODE_MONO {
                scf[1][sb] = scf[0][sb];
            }
        }

        // 12 time slots, one sample per allocated subband/ch each.
        let mut cur = [[0i32; 32]; 2];
        for s in 0..12 {
            for ch in 0..2 {
                cur[ch] = [0; 32];
            }
            for sb in 0..bound {
                for ch in 0..2 {
                    cur[ch][sb] = layer1_sample(b, nb[ch][sb], scf[ch][sb]);
                }
            }
            for sb in bound..32 {
                let v = layer1_sample(b, nb[0][sb], scf[0][sb]);
                cur[0][sb] = v;
                cur[1][sb] = v;
            }
            self.synth_subblock(&cur, nch, pcm, s * 32 * nch);
        }
        Some(384 * nch)
    }

    /// Polyphase synthesis of one time slot: 32 subband samples per channel →
    /// 32 PCM samples per channel, written interleaved at `out_off`
    /// (`out[out_off + j*nch + ch]`). Integer port of kjmp2's synthesis inner
    /// loop; `self.v`/`self.voffs` carry the filterbank history across slots.
    fn synth_subblock(
        &mut self,
        samp: &[[i32; 32]; 2],
        nch: usize,
        out: &mut [i32],
        out_off: usize,
    ) {
        self.voffs = self.voffs.wrapping_sub(64) & 1023;
        let voffs = self.voffs;
        let mut u = [0i32; 512];
        for ch in 0..nch {
            // matrixing: V[voffs + i] = (sum_j N[i][j] * sample[j] + 8192) >> 14
            for i in 0..64 {
                let mut sum: i32 = 0;
                for j in 0..32 {
                    sum = sum.wrapping_add(N[i][j].wrapping_mul(samp[ch][j]));
                }
                self.v[ch][voffs + i] = (sum.wrapping_add(8192)) >> 14;
            }
            // build U from the rotated V history
            for i in 0..8 {
                for j in 0..32 {
                    u[(i << 6) + j] = self.v[ch][(voffs + (i << 7) + j) & 1023];
                    u[(i << 6) + j + 32] = self.v[ch][(voffs + (i << 7) + j + 96) & 1023];
                }
            }
            // window and 16-tap output sum → clamped 16-bit PCM
            for i in 0..512 {
                u[i] = (u[i].wrapping_mul(D[i]).wrapping_add(32)) >> 6;
            }
            for j in 0..32 {
                let mut sum: i32 = 0;
                for i in 0..16 {
                    sum = sum.wrapping_sub(u[(i << 5) + j]);
                }
                sum = (sum.wrapping_add(8)) >> 4;
                out[out_off + j * nch + ch] = sum.clamp(-32768, 32767);
            }
        }
    }
}

/// Layer I 4-bit allocation value → codeword width in bits (0 = unallocated).
/// Values 1..14 map to widths 2..15; 15 is forbidden (treated as unallocated).
fn layer1_nbits(alloc: u32) -> u32 {
    if alloc == 0 || alloc == 15 {
        0
    } else {
        alloc + 1
    }
}

/// Read and requantise one Layer I sample.
fn layer1_sample(b: &mut Bits, nb: u32, scf_index: u32) -> i32 {
    if nb == 0 {
        return 0;
    }
    let nlevels = (1i32 << nb) - 1;
    let code = b.get(nb) as i32;
    requantise(nlevels, code, resolve_scalefactor(scf_index))
}

/// Read one allocation field for Layer II subband `sb` under `table_idx`.
/// Returns `Some(index into QUANT)` or `None` (no bits allocated).
fn read_alloc_l2(b: &mut Bits, table_idx: usize, sb: usize) -> Option<usize> {
    let entry = QUANT_LUT_STEP3[table_idx][sb];
    let nbal = u32::from(entry >> 4);
    let row = usize::from(entry & 15);
    let code = b.get(nbal) as usize;
    let qi = QUANT_LUT_STEP4[row][code];
    if qi == 0 {
        None
    } else {
        Some((qi - 1) as usize)
    }
}

/// Read and requantise the three Layer II samples of one subband granule into
/// `out`. `alloc` selects the quantiser class (`None` → three zeros).
fn read_samples_l2(b: &mut Bits, alloc: Option<usize>, scf_index: u32, out: &mut [i32; 3]) {
    let Some(qi) = alloc else {
        *out = [0; 3];
        return;
    };
    let q = &QUANT[qi];
    let mult = resolve_scalefactor(scf_index);
    let nlevels = q.nlevels;
    let mut codes = [0i32; 3];
    if q.grouping {
        let mut v = b.get(q.cw_bits) as i32;
        codes[0] = v % nlevels;
        v /= nlevels;
        codes[1] = v % nlevels;
        codes[2] = v / nlevels;
    } else {
        for c in &mut codes {
            *c = b.get(q.cw_bits) as i32;
        }
    }
    for i in 0..3 {
        out[i] = requantise(nlevels, codes[i], mult);
    }
}

/// Resolve a 6-bit scalefactor index to its fixed-point multiplier (kjmp2).
fn resolve_scalefactor(index: u32) -> i32 {
    if index == 63 {
        return 0;
    }
    let adj = (index / 3) as i32;
    (SCF_BASE[(index % 3) as usize] + ((1 << adj) >> 1)) >> adj
}

/// Requantise a subband code (0..nlevels-1) to a scaled subband sample. Renormal-
/// ises to a symmetric range, then applies the fixed-point scalefactor multiplier
/// in two 12-bit halves (kjmp2 `read_samples`).
fn requantise(nlevels: i32, code: i32, scalefactor: i32) -> i32 {
    let scale = 65536 / (nlevels + 1);
    let adj = ((nlevels + 1) >> 1) - 1;
    let val = (adj - code).wrapping_mul(scale);
    (val.wrapping_mul(scalefactor >> 12) + ((val.wrapping_mul(scalefactor & 4095) + 2048) >> 12))
        >> 12
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_short_or_unsynced_frames() {
        let mut d = Layer12::new();
        let mut pcm = [0i32; 2304];
        assert_eq!(d.decode_frame(&[], &mut pcm), None);
        assert_eq!(d.decode_frame(&[0xFF, 0xFD], &mut pcm), None); // too short
        assert_eq!(d.decode_frame(&[0x00, 0x00, 0x00, 0x00], &mut pcm), None); // no sync
    }

    #[test]
    fn rejects_free_format_and_reserved_bitrate() {
        let mut d = Layer12::new();
        let mut pcm = [0i32; 2304];
        // 0xFFFD = MPEG-1 Layer II; byte 2 high nibble = bitrate index.
        let free = [0xFF, 0xFD, 0x00, 0x04]; // bitrate index 0 (free format)
        let bad = [0xFF, 0xFD, 0xF0, 0x04]; // bitrate index 15 (reserved)
        assert_eq!(d.decode_frame(&free, &mut pcm), None);
        assert_eq!(d.decode_frame(&bad, &mut pcm), None);
    }

    #[test]
    fn scalefactor_index_63_is_silence() {
        // Index 63 is the reserved "illegal" scalefactor → zero multiplier.
        assert_eq!(resolve_scalefactor(63), 0);
        // Index 0 is the full-scale multiplier (SCF_BASE[0]).
        assert_eq!(resolve_scalefactor(0), SCF_BASE[0]);
    }

    #[test]
    fn layer1_allocation_widths() {
        assert_eq!(layer1_nbits(0), 0); // unallocated
        assert_eq!(layer1_nbits(1), 2); // 2-bit samples
        assert_eq!(layer1_nbits(14), 15); // widest
        assert_eq!(layer1_nbits(15), 0); // forbidden → treated as unallocated
    }
}
