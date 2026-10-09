//! Opus TOC (table-of-contents) byte and frame packing — RFC 6716 §3.1
//! (`parse_toc`) and §3.2 (`frames`). Pure byte bookkeeping: no DSP, no
//! allocation, no range coder — later tasks feed each `frames()` output
//! slice to the range decoder one Opus frame at a time.

use crate::decoder::DecodeError;

/// The Opus decoder mode a TOC config selects (RFC 6716 §3.1 Table 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    SilkOnly,
    Hybrid,
    CeltOnly,
}

/// Audio bandwidth a TOC config selects (RFC 6716 §2 terminology: narrowband
/// through fullband).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bandwidth {
    Nb,
    Mb,
    Wb,
    Swb,
    Fb,
}

/// Decoded fields of an Opus TOC byte's 5-bit config (RFC 6716 §3.1 Table 2)
/// plus its stereo flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub mode: Mode,
    pub bandwidth: Bandwidth,
    /// Frame duration expressed as a 48 kHz sample count (e.g. 20 ms = 960).
    pub frame_size_48k: u16,
    pub stereo: bool,
}

/// Frame sizes (48 kHz samples) for the four SILK-only durations, 10/20/40/60
/// ms, indexed by `config % 4` within the SILK config block (0-11).
const SILK_FRAME_SIZES_48K: [u16; 4] = [480, 960, 1920, 2880];

/// Frame sizes (48 kHz samples) for the two Hybrid durations, 10/20 ms,
/// indexed by `config % 2` within the Hybrid config block (12-15).
const HYBRID_FRAME_SIZES_48K: [u16; 2] = [480, 960];

/// Frame sizes (48 kHz samples) for the four CELT-only durations, 2.5/5/10/20
/// ms, indexed by `config % 4` within each CELT config block (16-31).
const CELT_FRAME_SIZES_48K: [u16; 4] = [120, 240, 480, 960];

/// Maximum number of frames a single Opus packet may carry (RFC 6716 §3.2:
/// code 3's 6-bit frame count is bounded here to keep the total packet
/// duration under 120 ms even at the smallest 2.5 ms CELT frame size).
pub const MAX_FRAMES: usize = 48;

/// Maximum total packet duration, expressed as a 48 kHz sample count (120 ms
/// = 5760 samples) — RFC 6716 §3.2's "no more than 120 ms of audio in a
/// single packet" rule, checked against `frame_count * frame_size_48k`.
const MAX_TOTAL_SAMPLES_48K: u32 = 5760;

/// Parse an Opus TOC byte's config (RFC 6716 §3.1 Table 2) and stereo flag.
/// `b0 >> 3` (5 bits) selects mode/bandwidth/frame size per this exact
/// layout:
///
/// ```text
///  0-3   SILK   NB   10/20/40/60 ms
///  4-7   SILK   MB   10/20/40/60 ms
///  8-11  SILK   WB   10/20/40/60 ms
/// 12-13  Hybrid SWB  10/20 ms
/// 14-15  Hybrid FB   10/20 ms
/// 16-19  CELT   NB   2.5/5/10/20 ms
/// 20-23  CELT   WB   2.5/5/10/20 ms
/// 24-27  CELT   SWB  2.5/5/10/20 ms
/// 28-31  CELT   FB   2.5/5/10/20 ms
/// ```
///
/// `(b0 >> 3)` is always in `0..=31` (5 bits of an 8-bit byte), so every
/// branch below is total — there is no invalid config to reject.
#[must_use]
pub fn parse_toc(b0: u8) -> Config {
    let config = b0 >> 3;
    let stereo = (b0 >> 2) & 1 == 1;

    let (mode, bandwidth, frame_size_48k) = match config {
        0..=11 => {
            let bandwidth = match config / 4 {
                0 => Bandwidth::Nb,
                1 => Bandwidth::Mb,
                _ => Bandwidth::Wb,
            };
            (
                Mode::SilkOnly,
                bandwidth,
                SILK_FRAME_SIZES_48K[(config % 4) as usize],
            )
        }
        12..=15 => {
            let bandwidth = if config < 14 {
                Bandwidth::Swb
            } else {
                Bandwidth::Fb
            };
            (
                Mode::Hybrid,
                bandwidth,
                HYBRID_FRAME_SIZES_48K[((config - 12) % 2) as usize],
            )
        }
        _ => {
            let group = (config - 16) / 4;
            let bandwidth = match group {
                0 => Bandwidth::Nb,
                1 => Bandwidth::Wb,
                2 => Bandwidth::Swb,
                _ => Bandwidth::Fb,
            };
            (
                Mode::CeltOnly,
                bandwidth,
                CELT_FRAME_SIZES_48K[((config - 16) % 4) as usize],
            )
        }
    };

    Config {
        mode,
        bandwidth,
        frame_size_48k,
        stereo,
    }
}

/// Read one RFC 6716 §3.2.1 self-delimited frame length starting at
/// `data[pos]`. Returns `(length, bytes_consumed)`, or `None` if `data` is
/// too short to hold the encoding at `pos` — the caller maps that to
/// [`DecodeError::Malformed`].
///
/// Encoding: a first byte `0` means length 0; `1..=251` means that value is
/// the length (1 byte total); `252..=255` means a second byte `b2` follows
/// and the length is `b + 4*b2` (2 bytes total, max `255 + 4*255 = 1275`).
fn read_frame_length(data: &[u8], pos: usize) -> Option<(usize, usize)> {
    let b = *data.get(pos)?;
    match b {
        0 => Some((0, 1)),
        1..=251 => Some((usize::from(b), 1)),
        _ => {
            let b2 = *data.get(pos + 1)?;
            Some((usize::from(b) + 4 * usize::from(b2), 2))
        }
    }
}

/// Split an Opus packet into its constituent frames (RFC 6716 §3.2, codes
/// 0-3) as borrowed subslices of `packet`, written into `out[..frame_count]`.
/// Returns the decoded TOC [`Config`] and the frame count.
///
/// `packet` must include the leading TOC byte. Handles code 0 (one frame:
/// the rest of the packet), code 1 (two equal-length frames, CBR), code 2
/// (two frames, the first's length self-delimited), and code 3 (an explicit
/// frame count with independent CBR/VBR framing and optional padding).
///
/// # Errors
/// [`DecodeError::Malformed`] for every way an attacker-controlled packet can
/// be inconsistent: empty packet (no TOC byte), odd remainder under code 1,
/// a self-delimited length that doesn't fit the remaining bytes (code 2 and
/// VBR code 3), a CBR code 1/3 remainder that doesn't divide evenly, a code 3
/// frame count of 0 or over [`MAX_FRAMES`], a truncated code-3 header
/// (missing frame-count/padding/length bytes), or a total packet duration
/// over 120 ms (`frame_count * frame_size_48k > 5760`). Never panics/indexes
/// out of bounds on malformed input — every slice access is bounds-checked.
pub fn frames<'a>(
    packet: &'a [u8],
    out: &mut [&'a [u8]; MAX_FRAMES],
) -> Result<(Config, usize), DecodeError> {
    let (&b0, rest) = packet.split_first().ok_or(DecodeError::Malformed)?;
    let config = parse_toc(b0);
    let code = b0 & 0x3;

    let frame_count = match code {
        0 => {
            out[0] = rest;
            1
        }
        1 => {
            if rest.len() % 2 != 0 {
                return Err(DecodeError::Malformed);
            }
            let half = rest.len() / 2;
            out[0] = &rest[..half];
            out[1] = &rest[half..];
            2
        }
        2 => {
            let (len, consumed) = read_frame_length(rest, 0).ok_or(DecodeError::Malformed)?;
            let body = rest.get(consumed..).ok_or(DecodeError::Malformed)?;
            if len > body.len() {
                return Err(DecodeError::Malformed);
            }
            let (first, second) = body.split_at(len);
            out[0] = first;
            out[1] = second;
            2
        }
        _ => parse_code3(rest, out)?,
    };

    // frame_count is at most MAX_FRAMES (48) on every path above (code 0/1/2
    // hardcode 1 or 2; code 3 rejects m > MAX_FRAMES in parse_code3), and
    // frame_size_48k is at most 2880, so the product fits well within u32
    // (48 * 2880 = 138_240) with no truncation or overflow.
    let total_samples = frame_count as u32 * u32::from(config.frame_size_48k);
    // frame_count == 0 is defensive/currently unreachable here: code 0/1/2
    // always yield 1 or 2 frames, and code 3 rejects m == 0 earlier in
    // parse_code3.
    if frame_count == 0 || total_samples > MAX_TOTAL_SAMPLES_48K {
        return Err(DecodeError::Malformed);
    }

    Ok((config, frame_count))
}

/// Split one **self-delimited** Opus packet (RFC 6716 Appendix B) out of the
/// front of `buf`, as used for every stream but the last in an Opus
/// *multistream* packet (RFC 7845 §3). Identical to [`frames`] except the
/// compressed length of the **last** frame is encoded explicitly (right after
/// the per-frame VBR lengths / code-3 header), rather than inferred from the
/// remaining bytes — so the packet's exact byte length is known and the next
/// stream's packet begins immediately after. Fills `out[..count]` with frame
/// subslices and returns `(Config, count, bytes_consumed)`.
///
/// A faithful port of libopus `opus_packet_parse_impl(self_delimited=1)`.
///
/// # Errors
/// [`DecodeError::Malformed`] for any inconsistency an attacker can craft: a
/// missing TOC/length byte, a declared frame length that overruns `buf`, a
/// code-3 frame count of 0 or over [`MAX_FRAMES`], or a duration over 120 ms.
/// Never panics or indexes out of bounds.
pub fn frames_self_delimited<'a>(
    buf: &'a [u8],
    out: &mut [&'a [u8]; MAX_FRAMES],
) -> Result<(Config, usize, usize), DecodeError> {
    let (&b0, rest) = buf.split_first().ok_or(DecodeError::Malformed)?;
    let config = parse_toc(b0);
    let code = b0 & 0x3;

    let mut sizes = [0usize; MAX_FRAMES];
    let mut pos = 0usize; // cursor into `rest` (past the TOC)
    let mut padding = 0usize;

    // Code-specific header + per-frame VBR lengths for frames `0..count-1`
    // (the last frame's length is read by the self-delimited step below).
    let (count, cbr) = match code {
        0 => (1usize, true),
        1 => (2usize, true),
        2 => {
            let (len, c) = read_frame_length(rest, pos).ok_or(DecodeError::Malformed)?;
            sizes[0] = len;
            pos += c;
            (2usize, false)
        }
        _ => {
            let fc = *rest.get(pos).ok_or(DecodeError::Malformed)?;
            pos += 1;
            let vbr = fc & 0x80 != 0;
            let has_padding = fc & 0x40 != 0;
            let m = usize::from(fc & 0x3F);
            if m == 0 || m > MAX_FRAMES {
                return Err(DecodeError::Malformed);
            }
            if has_padding {
                loop {
                    let p = *rest.get(pos).ok_or(DecodeError::Malformed)?;
                    pos += 1;
                    padding = padding
                        .checked_add(if p == 255 { 254 } else { usize::from(p) })
                        .ok_or(DecodeError::Malformed)?;
                    if p != 255 {
                        break;
                    }
                }
            }
            if vbr {
                for size in sizes.iter_mut().take(m - 1) {
                    let (len, c) = read_frame_length(rest, pos).ok_or(DecodeError::Malformed)?;
                    *size = len;
                    pos += c;
                }
            }
            (m, !vbr)
        }
    };

    // Self-delimited: the last frame's length is explicit.
    let (last_len, c) = read_frame_length(rest, pos).ok_or(DecodeError::Malformed)?;
    sizes[count - 1] = last_len;
    pos += c;
    // For CBR framing (codes 0/1 and code-3 CBR) that single length sizes every
    // frame.
    if cbr {
        for size in sizes.iter_mut().take(count - 1) {
            *size = last_len;
        }
    }

    // `pos` now indexes the first frame byte within `rest`; frame data is
    // followed by `padding` trailing pad bytes.
    let body = rest.get(pos..).ok_or(DecodeError::Malformed)?;
    let frame_bytes: usize = sizes[..count].iter().sum();
    let needed = frame_bytes
        .checked_add(padding)
        .ok_or(DecodeError::Malformed)?;
    if needed > body.len() {
        return Err(DecodeError::Malformed);
    }
    let mut off = 0usize;
    for (slot, &len) in out.iter_mut().zip(sizes[..count].iter()) {
        *slot = &body[off..off + len];
        off += len;
    }

    let total_samples = count as u32 * u32::from(config.frame_size_48k);
    if total_samples > MAX_TOTAL_SAMPLES_48K {
        return Err(DecodeError::Malformed);
    }
    // TOC(1) + header/length bytes + frame data + trailing padding.
    Ok((config, count, 1 + pos + needed))
}

/// Code-3 frame packing (RFC 6716 §3.2): frame-count byte (VBR flag,
/// padding flag, 6-bit count `M`), optional padding-length bytes, then
/// either `M` equal CBR frames or `M-1` self-delimited VBR lengths followed
/// by a final remainder frame. Returns the frame count `M` on success.
fn parse_code3<'a>(rest: &'a [u8], out: &mut [&'a [u8]; MAX_FRAMES]) -> Result<usize, DecodeError> {
    let (&frame_count_byte, after_header) = rest.split_first().ok_or(DecodeError::Malformed)?;
    let vbr = frame_count_byte & 0x80 != 0;
    let has_padding = frame_count_byte & 0x40 != 0;
    let m = usize::from(frame_count_byte & 0x3F);
    if m == 0 || m > MAX_FRAMES {
        return Err(DecodeError::Malformed);
    }

    // `pos` walks forward from `after_header` through the padding-length
    // bytes and (VBR only) the frame-length bytes; everything from `pos`
    // onward is frame data followed by `padding_total` trailing pad bytes.
    let mut pos = 0usize;
    let mut padding_total = 0usize;
    if has_padding {
        loop {
            let p = *after_header.get(pos).ok_or(DecodeError::Malformed)?;
            pos += 1;
            if p == 255 {
                padding_total = padding_total
                    .checked_add(254)
                    .ok_or(DecodeError::Malformed)?;
            } else {
                padding_total = padding_total
                    .checked_add(usize::from(p))
                    .ok_or(DecodeError::Malformed)?;
                break;
            }
        }
    }

    if vbr {
        let mut lengths = [0usize; MAX_FRAMES];
        for length in lengths.iter_mut().take(m - 1) {
            let (len, consumed) =
                read_frame_length(after_header, pos).ok_or(DecodeError::Malformed)?;
            *length = len;
            pos += consumed;
        }

        let after_lengths = after_header.get(pos..).ok_or(DecodeError::Malformed)?;
        let audio_len = after_lengths
            .len()
            .checked_sub(padding_total)
            .ok_or(DecodeError::Malformed)?;
        let declared: usize = lengths[..m - 1].iter().sum();
        if declared > audio_len {
            return Err(DecodeError::Malformed);
        }
        let last_len = audio_len - declared;

        let mut offset = 0usize;
        for (i, &len) in lengths[..m - 1].iter().enumerate() {
            out[i] = after_lengths
                .get(offset..offset + len)
                .ok_or(DecodeError::Malformed)?;
            offset += len;
        }
        out[m - 1] = after_lengths
            .get(offset..offset + last_len)
            .ok_or(DecodeError::Malformed)?;
    } else {
        let after_lengths = after_header.get(pos..).ok_or(DecodeError::Malformed)?;
        let audio_len = after_lengths
            .len()
            .checked_sub(padding_total)
            .ok_or(DecodeError::Malformed)?;
        if audio_len % m != 0 {
            return Err(DecodeError::Malformed);
        }
        let each = audio_len / m;
        let mut offset = 0usize;
        for slot in out.iter_mut().take(m) {
            *slot = after_lengths
                .get(offset..offset + each)
                .ok_or(DecodeError::Malformed)?;
            offset += each;
        }
    }

    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- §3.1 TOC config table: all 32 configs -----------------------

    #[test]
    fn config_table_matches_rfc_6716_table_2() {
        // (config, mode, bandwidth, frame_size_48k)
        let expected: [(u8, Mode, Bandwidth, u16); 32] = [
            (0, Mode::SilkOnly, Bandwidth::Nb, 480),
            (1, Mode::SilkOnly, Bandwidth::Nb, 960),
            (2, Mode::SilkOnly, Bandwidth::Nb, 1920),
            (3, Mode::SilkOnly, Bandwidth::Nb, 2880),
            (4, Mode::SilkOnly, Bandwidth::Mb, 480),
            (5, Mode::SilkOnly, Bandwidth::Mb, 960),
            (6, Mode::SilkOnly, Bandwidth::Mb, 1920),
            (7, Mode::SilkOnly, Bandwidth::Mb, 2880),
            (8, Mode::SilkOnly, Bandwidth::Wb, 480),
            (9, Mode::SilkOnly, Bandwidth::Wb, 960),
            (10, Mode::SilkOnly, Bandwidth::Wb, 1920),
            (11, Mode::SilkOnly, Bandwidth::Wb, 2880),
            (12, Mode::Hybrid, Bandwidth::Swb, 480),
            (13, Mode::Hybrid, Bandwidth::Swb, 960),
            (14, Mode::Hybrid, Bandwidth::Fb, 480),
            (15, Mode::Hybrid, Bandwidth::Fb, 960),
            (16, Mode::CeltOnly, Bandwidth::Nb, 120),
            (17, Mode::CeltOnly, Bandwidth::Nb, 240),
            (18, Mode::CeltOnly, Bandwidth::Nb, 480),
            (19, Mode::CeltOnly, Bandwidth::Nb, 960),
            (20, Mode::CeltOnly, Bandwidth::Wb, 120),
            (21, Mode::CeltOnly, Bandwidth::Wb, 240),
            (22, Mode::CeltOnly, Bandwidth::Wb, 480),
            (23, Mode::CeltOnly, Bandwidth::Wb, 960),
            (24, Mode::CeltOnly, Bandwidth::Swb, 120),
            (25, Mode::CeltOnly, Bandwidth::Swb, 240),
            (26, Mode::CeltOnly, Bandwidth::Swb, 480),
            (27, Mode::CeltOnly, Bandwidth::Swb, 960),
            (28, Mode::CeltOnly, Bandwidth::Fb, 120),
            (29, Mode::CeltOnly, Bandwidth::Fb, 240),
            (30, Mode::CeltOnly, Bandwidth::Fb, 480),
            (31, Mode::CeltOnly, Bandwidth::Fb, 960),
        ];

        for (config, mode, bandwidth, frame_size_48k) in expected {
            let b0 = config << 3; // stereo=0, code=0
            let got = parse_toc(b0);
            assert_eq!(got.mode, mode, "config {config} mode");
            assert_eq!(got.bandwidth, bandwidth, "config {config} bandwidth");
            assert_eq!(
                got.frame_size_48k, frame_size_48k,
                "config {config} frame_size_48k"
            );
            assert!(!got.stereo, "config {config} stereo bit unset");
        }
    }

    #[test]
    fn stereo_bit_is_bit_2() {
        assert!(!parse_toc(0b0000_0000).stereo);
        assert!(parse_toc(0b0000_0100).stereo);
    }

    // ---- §3.2 frame packing -------------------------------------------

    /// A packet with the given config/stereo/code TOC byte followed by
    /// `payload`.
    fn packet(config: u8, stereo: bool, code: u8, payload: &[u8]) -> Vec<u8> {
        let mut b0 = config << 3;
        if stereo {
            b0 |= 0b0000_0100;
        }
        b0 |= code;
        let mut v = vec![b0];
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn code0_is_one_frame_of_the_entire_remainder() {
        let pkt = packet(16, false, 0, &[1, 2, 3, 4]);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (config, n) = frames(&pkt, &mut out).unwrap();
        assert_eq!(n, 1);
        assert_eq!(out[0], &[1, 2, 3, 4][..]);
        assert_eq!(config.mode, Mode::CeltOnly);
    }

    // ---- Appendix B self-delimiting framing (multistream) -------------

    #[test]
    fn self_delimited_code0_reads_explicit_length() {
        // TOC(code 0) | len=3 | 3 frame bytes | (a trailing byte NOT consumed).
        let pkt = packet(16, false, 0, &[3, 0xAA, 0xBB, 0xCC, 0x99]);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (config, n, used) = frames_self_delimited(&pkt, &mut out).unwrap();
        assert_eq!(n, 1);
        assert_eq!(out[0], &[0xAA, 0xBB, 0xCC][..]);
        assert_eq!(used, 1 + 1 + 3); // TOC + length byte + frame
        assert_eq!(config.mode, Mode::CeltOnly);
    }

    #[test]
    fn self_delimited_code2_reads_both_lengths() {
        // TOC(code 2) | size0=2 | size1=3 | frame0(2) | frame1(3).
        let pkt = packet(16, false, 2, &[2, 3, 0x10, 0x11, 0x20, 0x21, 0x22]);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (_, n, used) = frames_self_delimited(&pkt, &mut out).unwrap();
        assert_eq!(n, 2);
        assert_eq!(out[0], &[0x10, 0x11][..]);
        assert_eq!(out[1], &[0x20, 0x21, 0x22][..]);
        assert_eq!(used, pkt.len());
    }

    #[test]
    fn self_delimited_boundary_splits_a_two_stream_packet() {
        // Stream 0 (self-delimited, one 1-byte frame) then stream 1 (standard
        // framing, 4-byte frame) — the multistream layout: all but the last
        // stream are self-delimited, the last uses `frames`.
        let mut buf = packet(16, false, 0, &[1, 0xAA]); // self-delim stream 0
        buf.extend_from_slice(&packet(16, false, 0, &[0xBB, 0xBB, 0xBB, 0xBB]));
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (_, n0, used) = frames_self_delimited(&buf, &mut out).unwrap();
        assert_eq!(n0, 1);
        assert_eq!(out[0], &[0xAA][..]);
        assert_eq!(used, 3); // TOC + len + 1 byte
        // The remainder parses as an ordinary (last-stream) packet.
        let (_, n1) = frames(&buf[used..], &mut out).unwrap();
        assert_eq!(n1, 1);
        assert_eq!(out[0], &[0xBB, 0xBB, 0xBB, 0xBB][..]);
    }

    #[test]
    fn code1_splits_into_two_equal_halves() {
        let pkt = packet(16, false, 1, &[1, 2, 3, 4]);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (_, n) = frames(&pkt, &mut out).unwrap();
        assert_eq!(n, 2);
        assert_eq!(out[0], &[1, 2][..]);
        assert_eq!(out[1], &[3, 4][..]);
    }

    #[test]
    fn code1_odd_remainder_is_malformed() {
        let pkt = packet(16, false, 1, &[1, 2, 3]);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        assert_eq!(frames(&pkt, &mut out).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn code2_splits_by_a_self_delimited_length() {
        // Frame-length byte 3 -> frame 1 = 3 bytes, frame 2 = the rest (2).
        let pkt = packet(16, false, 2, &[3, 10, 11, 12, 20, 21]);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (_, n) = frames(&pkt, &mut out).unwrap();
        assert_eq!(n, 2);
        assert_eq!(out[0], &[10, 11, 12][..]);
        assert_eq!(out[1], &[20, 21][..]);
    }

    #[test]
    fn code2_two_byte_length_encoding() {
        // 252..=255 first byte -> second byte b2, length = b + 4*b2.
        // b=252, b2=1 -> length 256.
        let mut payload = vec![252u8, 1];
        let frame1: Vec<u8> = (0..256).map(|i| (i % 256) as u8).collect();
        payload.extend_from_slice(&frame1);
        payload.extend_from_slice(&[9, 9, 9]); // frame 2 = remainder (3 bytes)
        let pkt = packet(16, false, 2, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (_, n) = frames(&pkt, &mut out).unwrap();
        assert_eq!(n, 2);
        assert_eq!(out[0].len(), 256);
        assert_eq!(out[0], &frame1[..]);
        assert_eq!(out[1], &[9, 9, 9][..]);
    }

    #[test]
    fn code2_length_overrunning_the_packet_is_malformed() {
        // Length byte says 10, but only 2 bytes remain.
        let pkt = packet(16, false, 2, &[10, 1, 2]);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        assert_eq!(frames(&pkt, &mut out).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn code2_truncated_two_byte_length_is_malformed() {
        // First length byte in 252..=255 signals a second length byte
        // follows, but the packet ends right after the first byte.
        let pkt = packet(16, false, 2, &[252]);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        assert_eq!(frames(&pkt, &mut out).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn code2_zero_length_first_frame_is_valid() {
        let pkt = packet(16, false, 2, &[0, 1, 2, 3]);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (_, n) = frames(&pkt, &mut out).unwrap();
        assert_eq!(n, 2);
        assert_eq!(out[0], &[][..]);
        assert_eq!(out[1], &[1, 2, 3][..]);
    }

    #[test]
    fn code3_cbr_splits_into_m_equal_frames() {
        // frame-count byte: v=0, p=0, M=4 -> 8 bytes split into 4x2.
        let mut payload = vec![0b0000_0100u8];
        payload.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let pkt = packet(16, false, 3, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (_, n) = frames(&pkt, &mut out).unwrap();
        assert_eq!(n, 4);
        assert_eq!(out[0], &[1, 2][..]);
        assert_eq!(out[1], &[3, 4][..]);
        assert_eq!(out[2], &[5, 6][..]);
        assert_eq!(out[3], &[7, 8][..]);
    }

    #[test]
    fn code3_cbr_uneven_remainder_is_malformed() {
        // M=3 but 8 bytes doesn't divide evenly by 3.
        let mut payload = vec![0b0000_0011u8];
        payload.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let pkt = packet(16, false, 3, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        assert_eq!(frames(&pkt, &mut out).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn code3_cbr_with_padding_strips_trailing_padding_bytes() {
        // v=0, p=1, M=2. Padding-length byte = 3 (one byte, <255 -> stop).
        // After header: 4 bytes of frame data (2 each) + 3 bytes padding.
        let mut payload = vec![0b0100_0010u8, 3];
        payload.extend_from_slice(&[1, 2, 3, 4]); // frame data
        payload.extend_from_slice(&[0xAA, 0xAA, 0xAA]); // padding
        let pkt = packet(16, false, 3, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (_, n) = frames(&pkt, &mut out).unwrap();
        assert_eq!(n, 2);
        assert_eq!(out[0], &[1, 2][..]);
        assert_eq!(out[1], &[3, 4][..]);
    }

    #[test]
    fn code3_padding_length_255_continuation_byte() {
        // Padding-length sequence [255, 2] -> total padding = 254 + 2 = 256.
        // v=0, p=1, M=1. After header: 4 bytes frame data + 256 padding bytes.
        let mut payload = vec![0b0100_0001u8, 255, 2];
        payload.extend_from_slice(&[9, 9, 9, 9]); // the one frame
        payload.extend(core::iter::repeat_n(0xAAu8, 256)); // padding
        let pkt = packet(16, false, 3, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (_, n) = frames(&pkt, &mut out).unwrap();
        assert_eq!(n, 1);
        assert_eq!(out[0], &[9, 9, 9, 9][..]);
    }

    #[test]
    fn code3_vbr_reads_m_minus_1_lengths_then_a_remainder_frame() {
        // v=1, p=0, M=3. Two self-delimited lengths (2, 3), then remainder.
        let mut payload = vec![0b1000_0011u8, 2, 3];
        payload.extend_from_slice(&[1, 2]); // frame 1 (len 2)
        payload.extend_from_slice(&[3, 4, 5]); // frame 2 (len 3)
        payload.extend_from_slice(&[6, 7]); // frame 3 (remainder, len 2)
        let pkt = packet(16, false, 3, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (_, n) = frames(&pkt, &mut out).unwrap();
        assert_eq!(n, 3);
        assert_eq!(out[0], &[1, 2][..]);
        assert_eq!(out[1], &[3, 4, 5][..]);
        assert_eq!(out[2], &[6, 7][..]);
    }

    #[test]
    fn code3_vbr_with_padding() {
        // v=1, p=1, M=2. Padding-length byte = 1. One self-delimited length
        // (len 2) for frame 1, remainder (minus padding) is frame 2.
        let mut payload = vec![0b1100_0010u8, 1, 2];
        payload.extend_from_slice(&[1, 2]); // frame 1 (len 2)
        payload.extend_from_slice(&[3, 4, 5]); // frame 2 (remainder, len 3)
        payload.extend_from_slice(&[0xAA]); // padding (1 byte)
        let pkt = packet(16, false, 3, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (_, n) = frames(&pkt, &mut out).unwrap();
        assert_eq!(n, 2);
        assert_eq!(out[0], &[1, 2][..]);
        assert_eq!(out[1], &[3, 4, 5][..]);
    }

    #[test]
    fn code3_vbr_length_overrun_is_malformed() {
        // v=1, p=0, M=2. Declared length (10) exceeds what remains.
        let mut payload = vec![0b1000_0010u8, 10];
        payload.extend_from_slice(&[1, 2, 3]);
        let pkt = packet(16, false, 3, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        assert_eq!(frames(&pkt, &mut out).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn code3_frame_count_zero_is_malformed() {
        let payload = vec![0b0000_0000u8];
        let pkt = packet(16, false, 3, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        assert_eq!(frames(&pkt, &mut out).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn code3_frame_count_over_max_frames_is_malformed() {
        // 6-bit frame count max is 63; MAX_FRAMES is 48.
        let payload = vec![0b0011_0001u8]; // M = 0b11_0001 = 49
        let pkt = packet(16, false, 3, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        assert_eq!(frames(&pkt, &mut out).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn code3_truncated_header_is_malformed_not_a_panic() {
        // Frame-count byte claims padding but the packet ends right there.
        let payload = vec![0b0100_0001u8];
        let pkt = packet(16, false, 3, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        assert_eq!(frames(&pkt, &mut out).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn code3_missing_frame_count_byte_is_malformed() {
        // Code 3 but no bytes at all follow the TOC byte.
        let pkt = packet(16, false, 3, &[]);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        assert_eq!(frames(&pkt, &mut out).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn code3_padding_length_exceeds_remaining_data_is_malformed() {
        // v=0, p=1, M=1. Padding-length byte claims 5 bytes of padding, but
        // only 2 bytes remain after it.
        let mut payload = vec![0b0100_0001u8, 5];
        payload.extend_from_slice(&[9, 9]);
        let pkt = packet(16, false, 3, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        assert_eq!(frames(&pkt, &mut out).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn empty_packet_is_malformed() {
        let pkt: Vec<u8> = vec![];
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        assert_eq!(frames(&pkt, &mut out).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn total_duration_over_120ms_is_malformed() {
        // config 17 = CELT NB, 5 ms/frame (240 samples @ 48k). code 3 CBR,
        // M=48 -> 48*240 = 11520 samples = 240 ms > 5760 (120 ms) -> Malformed.
        let mut payload = vec![48u8]; // v=0, p=0, M=48
        payload.extend(core::iter::repeat_n(0u8, 48)); // 1 byte/frame, CBR
        let pkt = packet(17, false, 3, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        assert_eq!(frames(&pkt, &mut out).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn total_duration_at_exactly_120ms_is_ok() {
        // config 16 = CELT NB, 2.5 ms/frame (120 samples @ 48k). code 3 CBR,
        // M=48 -> 48*120 = 5760 samples = exactly 120 ms -> Ok.
        let mut payload = vec![48u8]; // v=0, p=0, M=48
        payload.extend(core::iter::repeat_n(0u8, 48)); // 1 byte/frame, CBR
        let pkt = packet(16, false, 3, &payload);
        let mut out: [&[u8]; MAX_FRAMES] = [&[]; MAX_FRAMES];
        let (_, n) = frames(&pkt, &mut out).unwrap();
        assert_eq!(n, 48);
    }
}
