//! Container identification by peeking magic bytes.
//!
//! [`probe`] binds [`BufSource`] and *consumes nothing* — it looks at the
//! buffered prefix via `fill_buf`, so the decoder chosen afterwards still sees
//! byte 0. That peek-without-rewind is exactly what `BufSource` is for.

use crate::decoder::DecodeError;
use crate::io::BufSource;
use crate::model::Format;

/// Identify the container of `src` from its leading bytes, without consuming
/// them.
///
/// # Errors
/// - [`DecodeError::Io`] if the source read fails.
/// - [`DecodeError::Unsupported`] if the magic matches no known container (or
///   too few bytes are buffered to decide).
pub fn probe<S: BufSource>(src: &mut S) -> Result<Format, DecodeError> {
    let head = src.fill_buf().map_err(|_| DecodeError::Io)?;
    identify(head).ok_or(DecodeError::Unsupported)
}

/// Match a leading byte window against known container magic. Returns `None`
/// when nothing matches or the window is too short to decide.
fn identify(head: &[u8]) -> Option<Format> {
    // WAV: "RIFF" at 0 and "WAVE" at 8.
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WAVE" {
        return Some(Format::Wav);
    }
    // AIFF / AIFF-C: "FORM" at 0 and "AIFF" or "AIFC" at 8.
    if head.len() >= 12 && &head[0..4] == b"FORM" && matches!(&head[8..12], b"AIFF" | b"AIFC") {
        return Some(Format::Aiff);
    }
    // Matroska / WebM: the EBML header magic 0x1A45DFA3.
    if head.len() >= 4 && head[0..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some(Format::Matroska);
    }
    // WavPack: "wvpk" block magic.
    if head.len() >= 4 && &head[0..4] == b"wvpk" {
        return Some(Format::WavPack);
    }
    // Apple CAF: "caff" header magic.
    if head.len() >= 4 && &head[0..4] == b"caff" {
        return Some(Format::Caf);
    }
    if head.len() >= 4 {
        match &head[0..4] {
            b"fLaC" => return Some(Format::Flac),
            b"DSD " => return Some(Format::Dsf),
            b"FRM8" => return Some(Format::Dff),
            // Ogg: FLAC-in-Ogg, Vorbis, and Opus are recognized — their BOS
            // page body begins with the `\x7FFLAC` mapping header, the Vorbis
            // identification header's `\x01vorbis` prefix, or the Opus
            // identification header's `OpusHead` magic, respectively (all
            // within the first page).
            b"OggS" => {
                let window = &head[..head.len().min(64)];
                if window.windows(5).any(|w| w == b"\x7FFLAC") {
                    return Some(Format::OggFlac);
                }
                if window.windows(7).any(|w| w == b"\x01vorbis") {
                    return Some(Format::Vorbis);
                }
                if window.windows(8).any(|w| w == b"OpusHead") {
                    return Some(Format::Opus);
                }
                return None; // other Ogg mappings are not supported
            }
            _ => {}
        }
    }
    // ID3v2-prefixed elementary stream: MP3 *or* raw AAC/ADTS (taggers add an
    // ID3 tag to `.aac` elementary streams too, not just `.mp3`). Skip the tag
    // and classify by the first real frame's sync: an ADTS syncword with
    // layer == 00 is raw AAC; anything else — including a tag that runs past the
    // buffered window — stays MP3 (the historical default).
    if head.len() >= 3 && &head[0..3] == b"ID3" {
        if head.len() >= 10 {
            let size = (u32::from(head[6] & 0x7F) << 21)
                | (u32::from(head[7] & 0x7F) << 14)
                | (u32::from(head[8] & 0x7F) << 7)
                | u32::from(head[9] & 0x7F);
            let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
            let tag_end = 10usize + size as usize + footer as usize;
            if let Some(s) = head.get(tag_end..tag_end + 2) {
                if s[0] == 0xFF && (s[1] & 0xF0) == 0xF0 && (s[1] & 0x06) == 0 {
                    return Some(Format::Adts); // ADTS sync + layer == 00 => raw AAC
                }
            }
        }
        return Some(Format::Mp3); // MPEG-audio, or the tag runs past the buffer
    }
    // ADTS (raw .aac elementary stream): syncword 0xFFF + layer == 00 —
    // checked *before* the MP3 raw-sync fallback below, whose looser 3-bit
    // tail mask (0xE0) would otherwise also match every ADTS header (an ADTS
    // top nibble of 0xF0 satisfies MP3's 0xE0 mask too); the layer field is
    // what actually disambiguates the two, so it must be checked first here.
    if head.len() >= 2 && head[0] == 0xFF && (head[1] & 0xF0) == 0xF0 && (head[1] & 0x06) == 0 {
        return Some(Format::Adts);
    }
    if head.len() >= 2 && head[0] == 0xFF && (head[1] & 0xE0) == 0xE0 {
        return Some(Format::Mp3);
    }
    // MP4/M4A: "ftyp" box type at offset 4 (after the 4-byte box size).
    if head.len() >= 8 && &head[4..8] == b"ftyp" {
        return Some(Format::Mp4);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;
    use embedded_io::Read;

    fn probe_bytes(bytes: &[u8]) -> Result<Format, DecodeError> {
        probe(&mut SliceSource::new(bytes))
    }

    #[test]
    fn identifies_each_container() {
        assert_eq!(probe_bytes(b"RIFF\0\0\0\0WAVEfmt ").unwrap(), Format::Wav);
        assert_eq!(probe_bytes(b"FORM\0\0\0\0AIFFCOMM").unwrap(), Format::Aiff);
        assert_eq!(probe_bytes(b"FORM\0\0\0\0AIFCFVER").unwrap(), Format::Aiff);
        assert_eq!(
            probe_bytes(&[0x1A, 0x45, 0xDF, 0xA3, 0x01, 0x00]).unwrap(),
            Format::Matroska
        );
        assert_eq!(probe_bytes(b"wvpk\0\0\0\0").unwrap(), Format::WavPack);
        assert_eq!(probe_bytes(b"caff\0\x01\0\0").unwrap(), Format::Caf);
        assert_eq!(probe_bytes(b"fLaC\0\0\0\0").unwrap(), Format::Flac);
        assert_eq!(probe_bytes(b"DSD \0\0\0\0").unwrap(), Format::Dsf);
        assert_eq!(probe_bytes(b"FRM8\0\0\0\0").unwrap(), Format::Dff);
        assert_eq!(probe_bytes(b"ID3\x04\0\0\0").unwrap(), Format::Mp3);
        assert_eq!(probe_bytes(&[0xFF, 0xFB, 0x90, 0x00]).unwrap(), Format::Mp3);
        // A real ADTS header (layer == 00) must probe as Adts, not Mp3 — its
        // top nibble (0xF1 & 0xF0 == 0xF0) would otherwise satisfy the MP3
        // raw-sync fallback's looser 0xE0 mask too.
        assert_eq!(
            probe_bytes(&[0xFF, 0xF1, 0x50, 0x80, 0x2C, 0xDF, 0xFC]).unwrap(),
            Format::Adts
        );
    }

    #[test]
    fn id3_prefixed_adts_probes_as_aac_not_mp3() {
        // A tagger may wrap a raw `.aac` elementary stream in an ID3v2 tag. The
        // probe must skip the tag and classify by the first real frame's sync:
        // an ADTS syncword with layer == 00 is raw AAC, not MP3. 10-byte ID3v2.3
        // header (size 0, no footer) immediately followed by an ADTS header.
        let bytes = [
            0x49, 0x44, 0x33, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // ID3 hdr
            0xFF, 0xF1, 0x50, 0x80, // ADTS sync, layer 00
        ];
        assert_eq!(probe_bytes(&bytes).unwrap(), Format::Adts);
    }

    #[test]
    fn id3_prefixed_mp3_still_probes_as_mp3() {
        // The common case must not regress: an ID3v2-tagged MP3 (MPEG-1 Layer III
        // sync 0xFF 0xFB, layer != 00) still routes to MP3.
        let bytes = [
            0x49, 0x44, 0x33, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // ID3 hdr
            0xFF, 0xFB, 0x90, 0x00, // MPEG-1 Layer III sync
        ];
        assert_eq!(probe_bytes(&bytes).unwrap(), Format::Mp3);
    }

    /// Build a minimal, probe-realistic Ogg BOS page: a 27-byte page header +
    /// a 1-entry lacing table + `body`. Not a fully valid page (checksum is
    /// unchecked, like every Ogg reader in this crate) — just enough for
    /// [`identify`]'s byte-window sniff, which never parses page structure.
    fn ogg_bos_page(body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"OggS");
        v.push(0); // version
        v.push(0x02); // BOS
        v.extend_from_slice(&0u64.to_le_bytes()); // granule
        v.extend_from_slice(&1u32.to_le_bytes()); // serial
        v.extend_from_slice(&0u32.to_le_bytes()); // sequence
        v.extend_from_slice(&[0, 0, 0, 0]); // checksum (unchecked)
        v.push(1); // one lacing entry
        v.push(body.len() as u8);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn identifies_ogg_vorbis() {
        // A realistic id-header packet prefix: packet_type=1, then the
        // literal "vorbis" magic (both byte-aligned, so no bit-packing needed
        // to construct this fixture) — the rest of the fixed header's exact
        // contents don't matter to `identify`, which only sniffs the prefix.
        let mut packet = vec![1u8];
        packet.extend_from_slice(b"vorbis");
        packet.extend_from_slice(&[0u8; 23]); // pad to a realistic 30-byte packet
        let bytes = ogg_bos_page(&packet);
        assert_eq!(probe_bytes(&bytes).unwrap(), Format::Vorbis);
    }

    #[test]
    fn identifies_ogg_flac_still_works_alongside_vorbis() {
        // Guards against the new Vorbis check accidentally shadowing the
        // pre-existing FLAC-in-Ogg detection.
        let mut packet = b"\x7FFLAC".to_vec();
        packet.extend_from_slice(&[0u8; 46]); // pad to a realistic mapping-header size
        let bytes = ogg_bos_page(&packet);
        assert_eq!(probe_bytes(&bytes).unwrap(), Format::OggFlac);
    }

    #[test]
    fn ogg_without_a_known_mapping_is_still_unsupported() {
        // Anything else in Ogg: no magic matches, so this must stay
        // `Unsupported`, not silently become `Vorbis`/`OggFlac`/`Opus`.
        let bytes = ogg_bos_page(&[0u8; 40]);
        assert_eq!(probe_bytes(&bytes).unwrap_err(), DecodeError::Unsupported);
    }

    #[test]
    fn identifies_ogg_opus() {
        // A realistic `OpusHead` id-header prefix (RFC 7845 §5.1): the exact
        // trailing fields don't matter to `identify`, which only sniffs the
        // magic.
        let mut packet = b"OpusHead".to_vec();
        packet.extend_from_slice(&[1, 2]); // version, channels
        packet.extend_from_slice(&[0u8; 9]); // pre_skip + rate + gain + family
        let bytes = ogg_bos_page(&packet);
        assert_eq!(probe_bytes(&bytes).unwrap(), Format::Opus);
    }

    #[test]
    fn identifies_ogg_flac_and_vorbis_still_work_alongside_opus() {
        // Guards against the new Opus check accidentally shadowing the
        // pre-existing FLAC-in-Ogg/Vorbis detection.
        let mut flac_packet = b"\x7FFLAC".to_vec();
        flac_packet.extend_from_slice(&[0u8; 46]);
        assert_eq!(
            probe_bytes(&ogg_bos_page(&flac_packet)).unwrap(),
            Format::OggFlac
        );

        let mut vorbis_packet = vec![1u8];
        vorbis_packet.extend_from_slice(b"vorbis");
        vorbis_packet.extend_from_slice(&[0u8; 23]);
        assert_eq!(
            probe_bytes(&ogg_bos_page(&vorbis_packet)).unwrap(),
            Format::Vorbis
        );
    }

    #[test]
    fn identifies_mp4_ftyp() {
        // 4-byte box size (0x18) then the "ftyp" box type at offset 4 -> MP4/M4A.
        assert_eq!(
            probe_bytes(&[
                0, 0, 0, 0x18, b'f', b't', b'y', b'p', b'M', b'4', b'A', b' '
            ])
            .unwrap(),
            Format::Mp4
        );
    }

    #[test]
    fn unknown_magic_is_unsupported() {
        assert_eq!(
            probe_bytes(b"XXXXYYYY").unwrap_err(),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn riff_without_wave_is_not_wav() {
        // RIFF/AVI etc. — RIFF present but not a WAVE payload.
        assert_eq!(
            probe_bytes(b"RIFF\0\0\0\0AVI LIST").unwrap_err(),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn probe_consumes_nothing() {
        // After probing, a read still sees byte 0 (the magic).
        let bytes = b"fLaC\x00\x22";
        let mut src = SliceSource::new(bytes);
        assert_eq!(probe(&mut src).unwrap(), Format::Flac);
        let mut out = [0u8; 4];
        assert_eq!(src.read(&mut out).unwrap(), 4);
        assert_eq!(&out, b"fLaC");
    }
}
