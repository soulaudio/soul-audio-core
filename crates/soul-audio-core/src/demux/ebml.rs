//! EBML primitives (the byte grammar under Matroska / WebM): variable-length
//! element IDs and sizes, and fixed-width unsigned-int / float / string reads.
//!
//! An EBML *variable-size integer* (vint) is length-prefixed like UTF-8: the
//! position of the most-significant set bit in the first byte gives the total
//! byte length (1–8). For **element IDs** the marker bit is part of the value
//! (kept); for **sizes** and numeric vints the marker is a descriptor only
//! (stripped). A size whose data bits are all `1` means "unknown length"
//! (streamed) and is returned as `None`.
//!
//! All reads are forward-only over a [`Source`]; no seeking. Shared by
//! [`crate::demux::matroska`].

use super::read_exact;
use crate::decoder::DecodeError;
use crate::io::Source;

/// Read an EBML element ID (1–4 bytes, marker bit **kept**). Returns the ID as a
/// `u32` (the raw bytes, big-endian, including the length-descriptor bits — so
/// e.g. the Segment ID reads back as `0x1853_8067`) and the byte length.
pub(crate) fn read_id<S: Source>(src: &mut S) -> Result<(u32, u8), DecodeError> {
    let mut b0 = [0u8; 1];
    read_exact(src, &mut b0)?;
    let len = b0[0].leading_zeros() as u8 + 1;
    if b0[0] == 0 || len > 4 {
        return Err(DecodeError::Malformed);
    }
    let mut id = u32::from(b0[0]);
    for _ in 1..len {
        let mut b = [0u8; 1];
        read_exact(src, &mut b)?;
        id = (id << 8) | u32::from(b[0]);
    }
    Ok((id, len))
}

/// Read an EBML size / numeric vint (1–8 bytes, marker bit **stripped**).
/// Returns `(None, len)` when all data bits are `1` (unknown/streamed size).
pub(crate) fn read_vint<S: Source>(src: &mut S) -> Result<(Option<u64>, u8), DecodeError> {
    let mut b0 = [0u8; 1];
    read_exact(src, &mut b0)?;
    let len = b0[0].leading_zeros() as u8 + 1;
    if b0[0] == 0 || len > 8 {
        return Err(DecodeError::Malformed);
    }
    // Clear the marker bit from the first byte. For an 8-byte vint the first
    // byte is all descriptor (0x01) with no data bits, and `0xFF >> 8` would
    // overflow a `u8` shift, so mask to 0 there.
    let data_mask = if len >= 8 { 0u8 } else { 0xFFu8 >> len };
    let mut val = u64::from(b0[0] & data_mask);
    for _ in 1..len {
        let mut b = [0u8; 1];
        read_exact(src, &mut b)?;
        val = (val << 8) | u64::from(b[0]);
    }
    // All data bits set → unknown size. The field has `7*len` data bits.
    let all_ones = if len == 8 {
        // 56 data bits; (1<<56)-1.
        (1u64 << 56) - 1
    } else {
        (1u64 << (7 * u32::from(len))) - 1
    };
    Ok((if val == all_ones { None } else { Some(val) }, len))
}

/// Read a big-endian unsigned integer of `size` bytes (0–8). EBML allows a
/// 0-length integer (value 0).
pub(crate) fn read_uint<S: Source>(src: &mut S, size: u64) -> Result<u64, DecodeError> {
    if size > 8 {
        return Err(DecodeError::Malformed);
    }
    let mut v = 0u64;
    for _ in 0..size {
        let mut b = [0u8; 1];
        read_exact(src, &mut b)?;
        v = (v << 8) | u64::from(b[0]);
    }
    Ok(v)
}

/// Read an EBML float of `size` bytes: 4 (`f32`) or 8 (`f64`), returned as `f64`.
/// A 0-length float is `0.0` (EBML default). Any other width is malformed.
pub(crate) fn read_float<S: Source>(src: &mut S, size: u64) -> Result<f64, DecodeError> {
    match size {
        0 => Ok(0.0),
        4 => {
            let mut b = [0u8; 4];
            read_exact(src, &mut b)?;
            Ok(f64::from(f32::from_be_bytes(b)))
        }
        8 => {
            let mut b = [0u8; 8];
            read_exact(src, &mut b)?;
            Ok(f64::from_be_bytes(b))
        }
        _ => Err(DecodeError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    #[test]
    fn reads_ids_of_each_length() {
        // SimpleBlock 0xA3 (1 byte), TrackEntry 0xAE, Segment 0x18538067 (4).
        assert_eq!(read_id(&mut SliceSource::new(&[0xA3])).unwrap(), (0xA3, 1));
        assert_eq!(read_id(&mut SliceSource::new(&[0xAE])).unwrap(), (0xAE, 1));
        assert_eq!(
            read_id(&mut SliceSource::new(&[0x18, 0x53, 0x80, 0x67])).unwrap(),
            (0x1853_8067, 4)
        );
        // EBML header magic 0x1A45DFA3.
        assert_eq!(
            read_id(&mut SliceSource::new(&[0x1A, 0x45, 0xDF, 0xA3])).unwrap(),
            (0x1A45_DFA3, 4)
        );
    }

    #[test]
    fn reads_sizes_stripping_the_marker() {
        // 1-byte size 0x81 → 1. 0xA3 → 0x23 = 35.
        assert_eq!(
            read_vint(&mut SliceSource::new(&[0x81])).unwrap(),
            (Some(1), 1)
        );
        assert_eq!(
            read_vint(&mut SliceSource::new(&[0xA3])).unwrap(),
            (Some(0x23), 1)
        );
        // 2-byte size 0x40 0x02 → 2.
        assert_eq!(
            read_vint(&mut SliceSource::new(&[0x40, 0x02])).unwrap(),
            (Some(2), 2)
        );
        // 3-byte 0x20 0x00 0xFF → 255.
        assert_eq!(
            read_vint(&mut SliceSource::new(&[0x20, 0x00, 0xFF])).unwrap(),
            (Some(255), 3)
        );
    }

    #[test]
    fn detects_unknown_size() {
        // 1-byte all-ones (0xFF) = unknown.
        assert_eq!(
            read_vint(&mut SliceSource::new(&[0xFF])).unwrap(),
            (None, 1)
        );
        // 8-byte 0x01 FF*7 = unknown (56 data bits all set).
        assert_eq!(
            read_vint(&mut SliceSource::new(&[
                0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF
            ]))
            .unwrap(),
            (None, 8)
        );
        // 2-byte 0x7F 0xFF = unknown (14 data bits all set).
        assert_eq!(
            read_vint(&mut SliceSource::new(&[0x7F, 0xFF])).unwrap(),
            (None, 2)
        );
    }

    #[test]
    fn reads_uint_and_float() {
        assert_eq!(
            read_uint(&mut SliceSource::new(&[0x01, 0x00]), 2).unwrap(),
            256
        );
        assert_eq!(read_uint(&mut SliceSource::new(&[]), 0).unwrap(), 0);
        // 48000.0 as f32 big-endian = 0x47 0x3B 0x80 0x00.
        assert_eq!(
            read_float(&mut SliceSource::new(&[0x47, 0x3B, 0x80, 0x00]), 4).unwrap(),
            48_000.0
        );
    }

    #[test]
    fn rejects_zero_first_byte() {
        assert!(read_id(&mut SliceSource::new(&[0x00, 0x01])).is_err());
        assert!(read_vint(&mut SliceSource::new(&[0x00])).is_err());
    }
}
