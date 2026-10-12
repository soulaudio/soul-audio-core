//! Decode + resample **throughput** benchmark — the performance baseline and
//! regression gate for the core DSP paths. Dependency-free (no criterion): a
//! hand-rolled, auto-calibrating timer reporting, per codec, the decode speed
//! in **Msamples/s** (interleaved samples decoded per second) and the
//! **realtime factor** (×RT = decoded audio seconds ÷ wall seconds).
//!
//! Run: `cargo bench -p soul-audio-core --bench throughput` (builds optimized).
//! Decoding happens on a 16 MiB stack thread — `AnyDecoder` and the HE-AAC SBR
//! state are large by value (no-alloc), more than a default stack holds.

use std::time::{Duration, Instant};

use soul_audio_core::demux::MatroskaDemuxer;
use soul_audio_core::formats::dsf::DsfDecoder;
use soul_audio_core::formats::matroska::MatroskaVorbisOgg;
use soul_audio_core::formats::vorbis::VorbisDecoder;
use soul_audio_core::{DecimateAdapter, Decoder, Quality, Resampler, SliceSource, open};

const STACK: usize = 16 << 20;
/// Calibrate each benchmark to at least this much wall time for a stable number.
const MIN_WALL: Duration = Duration::from_millis(300);
/// Timed batches per benchmark; the fastest is reported (best-of-N, so transient
/// system load on a shared machine can't inflate the result).
const REPEAT: usize = 5;

/// Run `f` (one full decode/process, returning `(interleaved_samples,
/// sample_rate, channels)`) enough times to exceed [`MIN_WALL`], and print the
/// throughput. Executed on a big stack.
fn bench(label: &str, f: impl Fn() -> (usize, u32, u16) + Send + 'static) {
    let (samples, rate, channels, per_iter_ns) = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || {
            let (samples, rate, channels) = f(); // warm up + capture shape
            // Calibrate the iteration count so one timed batch spans ~MIN_WALL.
            let mut iters = 1u64;
            loop {
                let t = Instant::now();
                for _ in 0..iters {
                    let _ = f();
                }
                if t.elapsed() >= MIN_WALL {
                    break;
                }
                iters = iters.saturating_mul(2);
            }
            // Best-of-N: report the fastest batch. System noise can only ever
            // *add* wall time, so the minimum is the least-contended (most
            // representative) run — the standard robust throughput estimator,
            // resilient to the shared-CI jitter a single sample suffers.
            let mut per = f64::INFINITY;
            for _ in 0..REPEAT {
                let t = Instant::now();
                for _ in 0..iters {
                    let _ = f();
                }
                per = per.min(t.elapsed().as_nanos() as f64 / iters as f64);
            }
            (samples, rate, channels, per)
        })
        .expect("spawn bench thread")
        .join()
        .expect("bench thread panicked");

    let frames = samples / channels.max(1) as usize;
    let audio_s = frames as f64 / f64::from(rate.max(1));
    let wall_s = per_iter_ns / 1e9;
    let msps = samples as f64 / wall_s / 1e6;
    let rt = audio_s / wall_s;
    println!(
        "  {label:<16} {msps:>8.1} Msmp/s   {rt:>8.0}x RT   ({frames} frames @ {rate} Hz, {channels}ch)"
    );
}

fn open_decode(bytes: &'static [u8]) -> (usize, u32, u16) {
    let mut d = open(SliceSource::new(bytes)).expect("open");
    let si = d.stream_info();
    let mut total = 0usize;
    let mut buf = [0i32; 8192];
    loop {
        let n = d.decode_into(&mut buf).expect("decode");
        if n == 0 {
            break;
        }
        total += n;
    }
    (total, si.sample_rate, si.channels)
}

fn vorbis_decode(bytes: &'static [u8]) -> (usize, u32, u16) {
    let mut d = VorbisDecoder::new(SliceSource::new(bytes), vec![0u8; 128 * 1024]).expect("vorbis");
    let si = d.stream_info();
    let mut total = 0usize;
    let mut buf = [0i32; 8192];
    loop {
        let n = d.decode_into(&mut buf).expect("decode");
        if n == 0 {
            break;
        }
        total += n;
    }
    (total, si.sample_rate, si.channels)
}

fn matroska_vorbis_decode(bytes: &'static [u8]) -> (usize, u32, u16) {
    let demux = MatroskaDemuxer::new(SliceSource::new(bytes)).expect("mka demux");
    let adapter = MatroskaVorbisOgg::new(demux).expect("vorbis-mka adapter");
    let mut d = VorbisDecoder::new(adapter, vec![0u8; 256 * 1024]).expect("vorbis-in-mka");
    let si = d.stream_info();
    let mut total = 0usize;
    let mut buf = [0i32; 8192];
    loop {
        let n = d.decode_into(&mut buf).expect("decode");
        if n == 0 {
            break;
        }
        total += n;
    }
    (total, si.sample_rate, si.channels)
}

fn dsd_decode(bytes: &'static [u8]) -> (usize, u32, u16) {
    let dsf = DsfDecoder::new(SliceSource::new(bytes)).expect("dsf");
    let mut d = DecimateAdapter::new(dsf);
    let si = d.stream_info();
    let mut total = 0usize;
    let mut buf = [0i32; 8192];
    loop {
        let n = d.decode_into(&mut buf).expect("decode");
        if n == 0 {
            break;
        }
        total += n;
    }
    (total, si.sample_rate, si.channels)
}

/// Resample a fixed stereo block 44.1 kHz → 48 kHz for `blocks` output blocks;
/// returns the output-sample count so [`bench`] reports output Msamples/s.
fn resample_run(quality: Quality, blocks: usize) -> (usize, u32, u16) {
    const OUT: usize = 1024;
    let mut rs = Resampler::new(2, 44_100, 48_000, 1.0, OUT, quality).expect("resampler");
    let mut out = [0i32; OUT * 2];
    // A gentle stereo ramp as input (content-independent cost — the FIR runs
    // regardless of sample values).
    let mut total_out = 0usize;
    for b in 0..blocks {
        let need = rs.input_frames_next();
        let input: Vec<i32> = (0..need * 2)
            .map(|i| ((b + i) as i32 % 4096) - 2048)
            .collect();
        rs.process(&input, &mut out).expect("process");
        total_out += out.len();
    }
    (total_out, 48_000, 2)
}

fn main() {
    println!("\nsoul-audio-core — decode throughput (higher = faster):");
    // open()-dispatched codecs.
    bench("wav", || {
        open_decode(include_bytes!("../../../testdata/tone.wav"))
    });
    bench("flac", || {
        open_decode(include_bytes!("../../../testdata/tone.flac"))
    });
    bench("mp3", || {
        open_decode(include_bytes!("../../../testdata/tone.mp3"))
    });
    bench("opus", || {
        open_decode(include_bytes!("../../../testdata/tone.opus"))
    });
    bench("flac-in-ogg", || {
        open_decode(include_bytes!("../testdata/flac-in-ogg-multipage.ogg"))
    });
    bench("aac-lc", || {
        open_decode(include_bytes!("../testdata/aac.m4a"))
    });
    bench("he-aac (sbr)", || {
        open_decode(include_bytes!("../testdata/sbr.m4a"))
    });
    bench("alac", || {
        open_decode(include_bytes!("../testdata/alac.m4a"))
    });
    bench("aiff", || {
        open_decode(include_bytes!("../../../testdata/tone-aiff.aiff"))
    });
    bench("wavpack", || {
        open_decode(include_bytes!("../../../testdata/tone.wv"))
    });
    bench("caf (alac)", || {
        open_decode(include_bytes!("../../../testdata/tone-alac.caf"))
    });
    bench("mka-opus", || {
        open_decode(include_bytes!("../../../testdata/tone-opus.webm"))
    });
    bench("mka-flac", || {
        open_decode(include_bytes!("../../../testdata/tone-flac.mka"))
    });
    bench("mka-aac", || {
        open_decode(include_bytes!("../../../testdata/tone-aac.mka"))
    });
    // Consumer-dispatched codecs (not via open).
    bench("vorbis", || {
        vorbis_decode(include_bytes!("../testdata/tone-vorbis-stereo.ogg"))
    });
    bench("mka-vorbis", || {
        matroska_vorbis_decode(include_bytes!("../../../testdata/tone-vorbis.mka"))
    });
    bench("dsd (dsf→pcm)", || {
        dsd_decode(include_bytes!("../../../testdata/tone.dsf"))
    });

    println!("\nresampler 44.1→48 kHz stereo (output Msamples/s):");
    bench("resample fast", || resample_run(Quality::Fast, 256));
    bench("resample hq", || resample_run(Quality::Hq, 256));
    println!();
}
