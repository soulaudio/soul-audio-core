//! DSF (Sony DSD) container: a fixed-layout header (channel count, sample
//! rate, sample count) plus a metadata pointer to a trailing ID3v2 tag
//! (delegated to [`crate::id3v2::read_id3v2`]).
//!
//! Byte layout (ground truth: the already-shipped, spec-matching
//! `soul_audio_core::formats::dsf::DsfDecoder`, which this reader mirrors for
//! the header fields it needs):
//! - `0..4`: `"DSD "` magic
//! - `4..12`: u64 le DSD-chunk size (28)
//! - `12..20`: u64 le total file size
//! - `20..28`: u64 le metadata pointer (absolute offset of a trailing ID3v2
//!   tag; 0 if none)
//! - `28..32`: `"fmt "` magic
//! - `32..40`: u64 le fmt-chunk size (52)
//! - `40..44`: u32 le format version
//! - `44..48`: u32 le format id
//! - `48..52`: u32 le channel type
//! - `52..56`: u32 le channel count
//! - `56..60`: u32 le sample rate
//! - `60..64`: u32 le bits per sample
//! - `64..72`: u64 le sample count (= `total_frames`)

use crate::id3v2;
use crate::util::read_exact;
use crate::{StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

/// Read the DSF header's stream facts, then (if the metadata pointer is
/// non-zero) the trailing ID3v2 tag it points to.
pub fn read_dsf<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    // DSD chunk (28 bytes) + fmt chunk (52 bytes) = 80 bytes, fixed layout.
    let mut h = [0u8; 80];
    read_exact(src, &mut h)?;
    if &h[0..4] != b"DSD " || &h[28..32] != b"fmt " {
        return Err(TagError::Malformed);
    }

    let metadata_ptr = u64::from_le_bytes([h[20], h[21], h[22], h[23], h[24], h[25], h[26], h[27]]);
    let channels = u32::from_le_bytes([h[52], h[53], h[54], h[55]]);
    let sample_rate = u32::from_le_bytes([h[56], h[57], h[58], h[59]]);
    let sample_count = u64::from_le_bytes([h[64], h[65], h[66], h[67], h[68], h[69], h[70], h[71]]);

    let facts = StreamFacts {
        sample_rate: Some(sample_rate),
        channels: Some(channels as u16),
        total_frames: Some(sample_count),
    };

    if metadata_ptr != 0 {
        src.seek(SeekFrom::Start(metadata_ptr))
            .map_err(|_| TagError::Io)?;
        id3v2::read_id3v2(src, scratch, sink)?;
    }

    Ok(facts)
}

#[cfg(test)]
mod tests {
    use crate::Field;
    use crate::testfix::*;
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::vec::Vec;

    /// A minimal DSF: 28-byte DSD header (with a metadata pointer patched in
    /// after we know where the fmt chunk ends) + 52-byte fmt chunk + a
    /// trailing ID3v2.3 tag with a `TIT2` frame.
    fn build_dsf_with_id3(title: &str) -> Vec<u8> {
        let mut v = Vec::new();
        // DSD chunk (28 bytes).
        v.extend_from_slice(b"DSD ");
        v.extend_from_slice(&28u64.to_le_bytes());
        let total_size_pos = v.len();
        v.extend_from_slice(&0u64.to_le_bytes()); // total file size, patched below
        let ptr_pos = v.len();
        v.extend_from_slice(&0u64.to_le_bytes()); // metadata pointer, patched below

        // fmt chunk (52 bytes).
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&52u64.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // format version
        v.extend_from_slice(&0u32.to_le_bytes()); // format id
        v.extend_from_slice(&2u32.to_le_bytes()); // channel type: stereo
        v.extend_from_slice(&2u32.to_le_bytes()); // channel num
        v.extend_from_slice(&2_822_400u32.to_le_bytes()); // sample rate
        v.extend_from_slice(&1u32.to_le_bytes()); // bits per sample
        v.extend_from_slice(&5_644_800u64.to_le_bytes()); // sample count
        v.extend_from_slice(&4096u32.to_le_bytes()); // block size per channel
        v.extend_from_slice(&0u32.to_le_bytes()); // reserved

        let metadata_offset = v.len() as u64;
        v[ptr_pos..ptr_pos + 8].copy_from_slice(&metadata_offset.to_le_bytes());

        let id3_tag = id3v23(&[text_frame(b"TIT2", title)]);
        v.extend_from_slice(&id3_tag);

        let total_size = v.len() as u64;
        v[total_size_pos..total_size_pos + 8].copy_from_slice(&total_size.to_le_bytes());

        v
    }

    /// A minimal DSF with metadata pointer 0 (no trailing tag).
    fn build_dsf_without_id3() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"DSD ");
        v.extend_from_slice(&28u64.to_le_bytes());
        v.extend_from_slice(&80u64.to_le_bytes()); // total file size = header + fmt
        v.extend_from_slice(&0u64.to_le_bytes()); // metadata pointer: none

        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&52u64.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // format version
        v.extend_from_slice(&0u32.to_le_bytes()); // format id
        v.extend_from_slice(&1u32.to_le_bytes()); // channel type: mono
        v.extend_from_slice(&1u32.to_le_bytes()); // channel num
        v.extend_from_slice(&5_644_800u32.to_le_bytes()); // sample rate
        v.extend_from_slice(&1u32.to_le_bytes()); // bits per sample
        v.extend_from_slice(&2_822_400u64.to_le_bytes()); // sample count
        v.extend_from_slice(&4096u32.to_le_bytes()); // block size per channel
        v.extend_from_slice(&0u32.to_le_bytes()); // reserved

        v
    }

    #[test]
    fn dsf_facts_and_trailing_id3() {
        // header: "DSD " + sizes + metadata pointer -> ID3v2 with TIT2="X"
        // fmt chunk sets channels=2, sample_rate=2822400, sample_count=5644800
        let bytes = build_dsf_with_id3("X");
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = super::read_dsf(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.sample_rate, Some(2_822_400));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(5_644_800));
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::Title) && v == "X")
        );
    }

    #[test]
    fn dsf_without_metadata_pointer_returns_facts_only() {
        let bytes = build_dsf_without_id3();
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = super::read_dsf(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.sample_rate, Some(5_644_800));
        assert_eq!(facts.channels, Some(1));
        assert_eq!(facts.total_frames, Some(2_822_400));
        assert!(c.fields.is_empty());
    }
}
