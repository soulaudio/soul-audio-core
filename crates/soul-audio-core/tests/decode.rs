//! Integration tests exercising the public decode API.
//!
//! TEMPLATE: decoders are stubs, so this asserts the current `Unsupported`
//! contract. Once you implement a decoder, replace this with a golden test:
//! decode a committed `testdata/<clip>.wav` through [`WavDecoder`] and assert
//! the interleaved PCM output byte-matches a `testdata/<clip>.pcm` reference
//! that was generated *independently* of `soul-audio-core`.

use soul_audio_core::formats::wav::WavDecoder;
use soul_audio_core::{DecodeError, SliceReader};

#[test]
fn wav_decoder_stub_reports_unsupported() {
    let bytes: [u8; 0] = [];
    let result = WavDecoder::new(SliceReader::new(&bytes)).map(|_| ());
    assert_eq!(result, Err(DecodeError::Unsupported));
}
