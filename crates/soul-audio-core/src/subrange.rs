//! [`SubRange`]: play a **slice** of a decoder's stream as if it were a track
//! of its own.
//!
//! This is what makes a cue-sheet album playable. A single-file rip is one
//! audio file plus an index (`soul_audio_playlist::cue`); each cue track is a
//! `(start, length)` range of audio frames inside it. Wrapping the file's
//! decoder in a `SubRange` presents one such range as an ordinary
//! [`SeekableDecoder`] — position 0 is the track's first frame, the length is
//! the track's length, and the end of the range is the end of the stream — so
//! every layer above it (the [`Deck`](crate::deck::Deck), its gapless preload,
//! seeking, duration display, the DSP chain) works on cue tracks unchanged,
//! with no cue-specific code anywhere in the playback path.
//!
//! The boundaries are **sample-exact**, from two directions that agree:
//! [`decode_into`](Decoder::decode_into) never reads past the range (it reports
//! end of stream there, even though the inner decoder has more audio), and the
//! range's length is also declared through [`Decoder::playable_frames`] /
//! [`SeekableDecoder::total_frames`] — the same mechanism that trims a lossy
//! track's encoder padding — so the deck's trailing trim, its duration display
//! and its gapless preload all see the track's own length. The deck therefore
//! stops the track on its last frame and swaps to the preloaded next one with
//! no gap, no device reset and no re-seek. Concatenating a file's ranges
//! reproduces its samples exactly (asserted in
//! `soul-audio-playlist/tests/cue_playback.rs`, through a real FLAC and a real
//! cue sheet).
//!
//! It is not cue-specific: any "play this part of that file" — a long DJ set
//! split by markers, an audition range, a loop region — is the same shape.

use crate::decoder::{DecodeError, Decoder, SeekableDecoder};
use crate::model::{SampleFormat, SourceCodec, StreamInfo};

/// A sub-range of `inner`'s stream, presented as a decoder in its own right.
///
/// Construct with [`SubRange::new`], which seeks `inner` to the range's start.
pub struct SubRange<D> {
    inner: D,
    /// First frame of the range, in the inner stream's frame numbering.
    start: u64,
    /// Length of the range in frames, or `None` for "to the end of the inner
    /// stream".
    frames: Option<u64>,
    /// Frames emitted so far, i.e. the position within the range.
    pos: u64,
}

impl<D: SeekableDecoder> SubRange<D> {
    /// Present `inner`'s frames `start .. start + frames` as a whole stream,
    /// seeking it to `start`. `frames: None` runs to the end of the inner
    /// stream (the last track of a cue sheet whose file length is unknown).
    ///
    /// A `start` past the end of a stream that declares its length, or a
    /// `frames` that runs past it, is clamped — a malformed cue sheet must not
    /// be able to make playback read off the end.
    ///
    /// # Errors
    /// Propagates the inner decoder's seek error ([`DecodeError::Unsupported`]
    /// if it cannot seek at all — a range of an unseekable stream is not
    /// playable).
    pub fn new(mut inner: D, start: u64, frames: Option<u64>) -> Result<Self, DecodeError> {
        let total = inner.total_frames();
        let start = match total {
            Some(t) => start.min(t),
            None => start,
        };
        let frames = match (frames, total) {
            (Some(f), Some(t)) => Some(f.min(t.saturating_sub(start))),
            (f, _) => f,
        };
        // Seek even for start == 0: the inner decoder may have been used
        // before (a cue album opens one file per track), so the range's first
        // frame must be established rather than assumed.
        inner.seek_to(start)?;
        Ok(Self {
            inner,
            start,
            frames,
            pos: 0,
        })
    }

    /// The range's offset in the inner stream, in frames.
    #[must_use]
    pub fn start(&self) -> u64 {
        self.start
    }

    /// The range's length in frames, if bounded.
    #[must_use]
    pub fn range_frames(&self) -> Option<u64> {
        self.frames
    }

    /// Borrow the wrapped decoder (e.g. to query it for a signal-path display).
    #[must_use]
    pub fn inner(&self) -> &D {
        &self.inner
    }

    /// Unwrap, returning the inner decoder — positioned wherever the range
    /// left it.
    pub fn into_inner(self) -> D {
        self.inner
    }

    /// Frames still inside the range, or `None` when unbounded.
    fn remaining(&self) -> Option<u64> {
        self.frames.map(|f| f.saturating_sub(self.pos))
    }
}

impl<D: SeekableDecoder> Decoder for SubRange<D> {
    fn stream_info(&self) -> StreamInfo {
        self.inner.stream_info()
    }

    /// Decode from the range, stopping at its end. Beyond it this reports end
    /// of stream (`Ok(0)`) even though the inner decoder has more audio — that
    /// is the whole point: the next cue track's samples belong to the next
    /// track.
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let channels = usize::from(self.stream_info().channels).max(1);
        let cap = match self.remaining() {
            Some(0) => return Ok(0),
            Some(left) => {
                // Never ask the inner decoder for more than the range holds.
                let max_samples = left.saturating_mul(channels as u64);
                (out.len() as u64).min(max_samples) as usize
            }
            None => out.len(),
        };
        // Keep whole frames: a partial frame would desynchronize the channels.
        let cap = (cap / channels) * channels;
        if cap == 0 {
            return Ok(0);
        }
        let n = self.inner.decode_into(&mut out[..cap])?;
        self.pos += (n / channels) as u64;
        Ok(n)
    }

    /// `0`: the range begins at its own first frame.
    ///
    /// A leading encoder delay is a property of the *file's* start, and the
    /// inner decoder's is forwarded only for a range that starts there — a cue
    /// track from the middle of a file is long past any priming. (Cue sheets
    /// index lossless rips in practice, where the delay is zero either way.)
    fn encoder_delay(&self) -> u64 {
        if self.start == 0 {
            self.inner.encoder_delay()
        } else {
            0
        }
    }

    /// The range's length — the mechanism that makes the boundary sample-exact
    /// and the swap gapless. For an unbounded range, whatever the inner
    /// decoder declares (its own padding trim still applies).
    fn playable_frames(&self) -> Option<u64> {
        match self.frames {
            Some(f) => Some(f),
            None => self.inner.playable_frames(),
        }
    }

    fn source_format(&self) -> SampleFormat {
        self.inner.source_format()
    }

    fn codec(&self) -> SourceCodec {
        self.inner.codec()
    }

    fn channel_layout(&self) -> Option<crate::channels::ChannelLayout> {
        self.inner.channel_layout()
    }
}

impl<D: SeekableDecoder> SeekableDecoder for SubRange<D> {
    /// Seek **within the range**: frame `0` is the range's start. A target past
    /// the end lands on the end (an empty remainder), never outside it — a
    /// scrub to the end of a cue track must not spill into the next one.
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        let target = match self.frames {
            Some(f) => sample.min(f),
            None => sample,
        };
        let landed = self.inner.seek_to(self.start + target)?;
        // Trust the inner decoder's landing point (a frame-granular format can
        // land earlier), but never outside the range.
        self.pos = landed.saturating_sub(self.start);
        if let Some(f) = self.frames {
            self.pos = self.pos.min(f);
        }
        Ok(self.pos)
    }

    /// The range's length, so a duration display shows the *track's* length
    /// rather than the whole album file's.
    fn total_frames(&self) -> Option<u64> {
        match self.frames {
            Some(f) => Some(f),
            None => self
                .inner
                .total_frames()
                .map(|t| t.saturating_sub(self.start)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stereo ramp decoder: frame `k` holds `(k*2, k*2+1)`, so a test can
    /// read back exactly which frames of the inner stream were emitted.
    struct Ramp {
        pos: u64,
        frames: u64,
        delay: u64,
        playable: Option<u64>,
    }

    impl Ramp {
        fn new(frames: u64) -> Self {
            Self {
                pos: 0,
                frames,
                delay: 0,
                playable: None,
            }
        }
    }

    impl Decoder for Ramp {
        fn stream_info(&self) -> StreamInfo {
            StreamInfo {
                sample_rate: 44_100,
                channels: 2,
                format: SampleFormat::Int { bits: 24 },
            }
        }
        fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
            let want = ((out.len() / 2) as u64).min(self.frames - self.pos) as usize;
            for f in 0..want {
                let k = self.pos + f as u64;
                out[f * 2] = (k * 2) as i32;
                out[f * 2 + 1] = (k * 2 + 1) as i32;
            }
            self.pos += want as u64;
            Ok(want * 2)
        }
        fn encoder_delay(&self) -> u64 {
            self.delay
        }
        fn playable_frames(&self) -> Option<u64> {
            self.playable
        }
    }

    impl SeekableDecoder for Ramp {
        fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
            self.pos = sample.min(self.frames);
            Ok(self.pos)
        }
        fn total_frames(&self) -> Option<u64> {
            Some(self.frames)
        }
    }

    /// Decode a whole decoder to exhaustion in small chunks (a chunk size that
    /// is not a multiple of the range length, to catch off-by-one at the end).
    fn drain(d: &mut impl Decoder) -> Vec<i32> {
        let mut out = Vec::new();
        let mut buf = [0i32; 14];
        loop {
            let n = d.decode_into(&mut buf).expect("decode");
            if n == 0 {
                return out;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    #[test]
    fn a_range_emits_exactly_its_own_frames() {
        let mut r = SubRange::new(Ramp::new(1_000), 100, Some(50)).expect("range");
        assert_eq!(
            r.total_frames(),
            Some(50),
            "the track's length, not the file's"
        );
        assert_eq!(r.playable_frames(), Some(50));
        let out = drain(&mut r);
        assert_eq!(out.len(), 50 * 2, "exactly the range's frames");
        // The first emitted frame is the inner stream's frame 100.
        assert_eq!(&out[..2], &[200, 201]);
        // The last is frame 149 — not 150: the next track's first frame.
        assert_eq!(&out[out.len() - 2..], &[149 * 2, 149 * 2 + 1]);
    }

    /// The property cue playback rests on: consecutive ranges reproduce the
    /// file exactly — every frame once, in order, nothing dropped or repeated
    /// at a boundary.
    #[test]
    fn consecutive_ranges_tile_the_stream_exactly() {
        let total = 500u64;
        let whole = drain(&mut Ramp::new(total));
        let mut joined = Vec::new();
        // Deliberately uneven splits, including a 1-frame track.
        for (start, len) in [(0u64, 137u64), (137, 1), (138, 200), (338, 162)] {
            let mut r = SubRange::new(Ramp::new(total), start, Some(len)).expect("range");
            joined.extend(drain(&mut r));
        }
        assert_eq!(joined, whole, "the ranges must tile the stream exactly");
    }

    #[test]
    fn an_unbounded_range_runs_to_the_end_of_the_stream() {
        let mut r = SubRange::new(Ramp::new(300), 250, None).expect("range");
        assert_eq!(r.total_frames(), Some(50), "to the end of the inner stream");
        assert_eq!(r.playable_frames(), None, "no declared trim");
        let out = drain(&mut r);
        assert_eq!(out.len(), 50 * 2);
        assert_eq!(&out[..2], &[500, 501]);
    }

    #[test]
    fn a_start_or_length_past_the_end_is_clamped() {
        // A start past the end gives an empty, harmless track.
        let mut r = SubRange::new(Ramp::new(100), 500, Some(50)).expect("range");
        assert_eq!(r.start(), 100);
        assert_eq!(r.range_frames(), Some(0));
        assert_eq!(drain(&mut r), Vec::<i32>::new());
        // A length past the end stops at the end.
        let mut r = SubRange::new(Ramp::new(100), 80, Some(500)).expect("range");
        assert_eq!(r.range_frames(), Some(20));
        assert_eq!(drain(&mut r).len(), 20 * 2);
    }

    #[test]
    fn seeking_is_relative_to_the_range_and_cannot_leave_it() {
        let mut r = SubRange::new(Ramp::new(1_000), 100, Some(50)).expect("range");
        // Frame 0 of the range is the inner stream's frame 100.
        assert_eq!(r.seek_to(0).expect("seek"), 0);
        let mut buf = [0i32; 2];
        r.decode_into(&mut buf).expect("decode");
        assert_eq!(buf, [200, 201]);
        // Mid-range.
        assert_eq!(r.seek_to(10).expect("seek"), 10);
        r.decode_into(&mut buf).expect("decode");
        assert_eq!(buf, [220, 221]);
        // Past the end lands on the end and emits nothing — it must not spill
        // into the following track's audio.
        assert_eq!(r.seek_to(10_000).expect("seek"), 50);
        assert_eq!(r.decode_into(&mut buf).expect("decode"), 0);
        // And a seek back into the range resumes it.
        assert_eq!(r.seek_to(49).expect("seek"), 49);
        assert_eq!(r.decode_into(&mut buf).expect("decode"), 2);
        assert_eq!(buf, [298, 299]);
    }

    #[test]
    fn the_encoder_delay_belongs_to_the_start_of_the_file_only() {
        let delayed = |start: u64| {
            let inner = Ramp {
                pos: 0,
                frames: 1_000,
                delay: 1_024,
                playable: Some(900),
            };
            SubRange::new(inner, start, Some(100)).expect("range")
        };
        // A range at the file's start keeps the file's priming skip...
        assert_eq!(delayed(0).encoder_delay(), 1_024);
        // ...and one from the middle has nothing to skip.
        assert_eq!(delayed(500).encoder_delay(), 0);
        // The range's own length always wins over the inner playable length.
        assert_eq!(delayed(500).playable_frames(), Some(100));
    }

    #[test]
    fn stream_facts_and_the_signal_path_pass_through() {
        let r = SubRange::new(Ramp::new(100), 10, Some(10)).expect("range");
        assert_eq!(r.stream_info().sample_rate, 44_100);
        assert_eq!(r.stream_info().channels, 2);
        assert_eq!(r.source_format(), SampleFormat::Int { bits: 24 });
        assert_eq!(r.codec(), SourceCodec::Pcm);
        assert_eq!(r.channel_layout(), None);
        assert_eq!(r.inner().total_frames(), Some(100), "the file's own length");
    }

    #[test]
    fn a_partial_frame_is_never_emitted() {
        // An odd-sized output buffer must not split a stereo frame.
        let mut r = SubRange::new(Ramp::new(100), 0, Some(10)).expect("range");
        let mut buf = [0i32; 7];
        let n = r.decode_into(&mut buf).expect("decode");
        assert_eq!(n % 2, 0, "whole frames only, got {n}");
        assert_eq!(n, 6);
    }
}
