#!/usr/bin/env python3
"""Generate a small DSD64 DSF test fixture (tone.dsf).

ffmpeg cannot *encode* PCM->DSD (there is no modulator), so we synthesize the
1-bit stream directly: a 2nd-order sigma-delta modulator per channel, then a
Sony DSF container. The signal matches the PCM fixtures — left 440 Hz (A4),
right 659.25 Hz (E5) — at a lower amplitude for modulator stability.

Bit order in DSF is LSB-first (earliest sample = bit 0). DSD64 = 64 * 44100 =
2,822,400 Hz, 1 bit.

Run:  python testdata/generate_dsd.py
"""
import struct
import sys

import numpy as np

DSD64 = 2_822_400
# main() reads the rate from argv[2] (Hz) if given, else DSD64.
DUR = 0.5  # seconds — DSD is 1-bit at 2.8 MHz, so keep it short
FREQS = (440.0, 659.25)  # L, R
AMP = 0.4  # headroom for sigma-delta stability
BLOCK = 4096  # DSF block size per channel (bytes)


def sigma_delta_2nd(x: np.ndarray, e1: float = 0.0, e2: float = 0.0):
    """2nd-order error-feedback modulator: x in [-1,1] -> bits (True = +1).

    NTF = (1 - z^-1)^2, so quantization noise is pushed up in frequency and the
    audio band stays clean after decimation. `e1`/`e2` carry the integrator
    state across blocks (for streaming a long input); returns `(bits, e1, e2)`.
    Iterates a Python list — far faster than per-sample numpy indexing.
    """
    xs = x.tolist()
    out = bytearray(len(xs))
    for n, xn in enumerate(xs):
        u = xn + 2.0 * e1 - e2
        if u >= 0.0:
            out[n] = 1
            e = u - 1.0
        else:
            e = u + 1.0
        e2 = e1
        e1 = e
    bits = np.frombuffer(bytes(out), dtype=np.uint8).astype(bool)
    return bits, e1, e2


def modulate_wav(path: str, rate: int):
    """Modulate a 16-bit PCM WAV to a 1-bit DSD stream per channel at `rate`
    (a multiple of the WAV's rate), so a DSD fixture carries the *real* source
    audio rather than a synthesized tone. Streams the input in blocks (bounded
    memory for the 64x-upsampled signal) into a pre-allocated per-channel bit
    array. Returns `(ch_bits, samples_per_channel)`.
    """
    import wave

    w = wave.open(path, "rb")
    channels, sw, in_rate, nframes = (
        w.getnchannels(),
        w.getsampwidth(),
        w.getframerate(),
        w.getnframes(),
    )
    if sw != 2:
        raise SystemExit(f"{path}: need 16-bit PCM")
    if rate % in_rate != 0:
        raise SystemExit(f"{path}: DSD rate {rate} is not a multiple of {in_rate}")
    oversample = rate // in_rate
    ch_bits = [np.empty(nframes * oversample, dtype=bool) for _ in range(channels)]
    state = [(0.0, 0.0)] * channels
    pos = 0
    while True:
        raw = w.readframes(in_rate)  # ~1 s per block
        if not raw:
            break
        pcm = np.frombuffer(raw, dtype="<i2").astype(np.float64).reshape(-1, channels) / 32768.0
        got = pcm.shape[0]
        for c in range(channels):
            up = np.repeat(pcm[:, c], oversample)  # zero-order-hold 64x upsample
            bits, e1, e2 = sigma_delta_2nd(up, state[c][0], state[c][1])
            state[c] = (e1, e2)
            ch_bits[c][pos * oversample : pos * oversample + bits.shape[0]] = bits
        pos += got
        print(f"  modulated {pos}/{nframes} frames", flush=True)
    w.close()
    total = pos * oversample
    return [cb[:total] for cb in ch_bits], total


def big_chunk(cid: bytes, body: bytes) -> bytes:
    """A big-endian IFF chunk: 4-byte id + u64 BE size + body (caller pads)."""
    return cid + struct.pack(">Q", len(body)) + body


def write_dff(out: str, rate: int, ch_bits: list) -> None:
    """Write a raw-DSD DSDIFF (.dff): MSB-first, channel-interleaved."""
    channels = len(ch_bits)
    ch_bytes = [np.packbits(b, bitorder="big") for b in ch_bits]
    nbytes = ch_bytes[0].shape[0]
    inter = np.empty(nbytes * channels, dtype=np.uint8)
    for c, cb in enumerate(ch_bytes):
        inter[c::channels] = cb

    chnl = struct.pack(">H", channels) + b"".join(
        (b"SLFT" if c == 0 else b"SRGT") for c in range(channels)
    )
    name = b"raw"
    cmpr = b"DSD " + bytes([len(name)]) + name  # 4 + 1 + 3 = 8 (even)
    prop_body = (
        b"SND "
        + big_chunk(b"FS  ", struct.pack(">I", rate))
        + big_chunk(b"CHNL", chnl)
        + big_chunk(b"CMPR", cmpr)
    )
    body = big_chunk(b"PROP", prop_body) + big_chunk(b"DSD ", inter.tobytes())
    frm8 = b"FRM8" + struct.pack(">Q", 4 + len(body)) + b"DSD " + body
    with open(out, "wb") as fh:
        fh.write(frm8)
    print(f"wrote {out}: {len(frm8)} bytes, {channels}ch raw-DSD (.dff)")


def write_dsf(out: str, rate: int, n: int, ch_bits: list) -> None:
    """Write a Sony DSF (.dsf): LSB-first, block-planar."""
    channels = len(ch_bits)
    ch_bytes = [np.packbits(b, bitorder="little") for b in ch_bits]
    valid_bpc = ch_bytes[0].shape[0]  # bytes per channel (n // 8)
    blocks = (valid_bpc + BLOCK - 1) // BLOCK
    padded_bpc = blocks * BLOCK
    padded = [np.zeros(padded_bpc, dtype=np.uint8) for _ in range(channels)]
    for c in range(channels):
        padded[c][: ch_bytes[c].shape[0]] = ch_bytes[c]

    # Data: block-interleaved by channel (block0 ch0, block0 ch1, block1 ch0, ...).
    data = bytearray()
    for b in range(blocks):
        for c in range(channels):
            data += padded[c][b * BLOCK : (b + 1) * BLOCK].tobytes()

    data_chunk = b"data" + struct.pack("<Q", 12 + len(data)) + bytes(data)
    fmt_chunk = (
        b"fmt "
        + struct.pack("<Q", 52)
        + struct.pack("<I", 1)  # format version
        + struct.pack("<I", 0)  # format id: 0 = DSD raw
        + struct.pack("<I", 2)  # channel type: 2 = stereo (L/R)
        + struct.pack("<I", channels)
        + struct.pack("<I", rate)
        + struct.pack("<I", 1)  # bits per sample
        + struct.pack("<Q", n)  # sample count per channel
        + struct.pack("<I", BLOCK)
        + struct.pack("<I", 0)  # reserved
    )
    total = 28 + len(fmt_chunk) + len(data_chunk)
    dsd_chunk = b"DSD " + struct.pack("<Q", 28) + struct.pack("<Q", total) + struct.pack("<Q", 0)

    with open(out, "wb") as fh:
        fh.write(dsd_chunk)
        fh.write(fmt_chunk)
        fh.write(data_chunk)
    label = f"DSD{64 * rate // DSD64}" if rate % DSD64 == 0 else f"{rate}Hz"
    print(f"wrote {out}: {total} bytes, {channels}ch {label} (.dsf)")


def main() -> None:
    out = sys.argv[1] if len(sys.argv) > 1 else "tone.dsf"
    rate = int(sys.argv[2]) if len(sys.argv) > 2 else DSD64
    # Third positional: a `.wav` to modulate (DSD of the real track), else a
    # duration in seconds (synthesized tone, the small committed fixtures).
    arg3 = sys.argv[3] if len(sys.argv) > 3 else None
    if arg3 and arg3.lower().endswith(".wav"):
        ch_bits, n = modulate_wav(arg3, rate)
    else:
        dur = float(arg3) if arg3 else DUR
        n = int(dur * rate)
        t = np.arange(n) / rate
        # 1-bit stream per channel (True = +1); packing/layout is per-container.
        ch_bits = [sigma_delta_2nd(AMP * np.sin(2.0 * np.pi * f * t))[0] for f in FREQS]

    if out.endswith(".dff"):
        write_dff(out, rate, ch_bits)
    else:
        write_dsf(out, rate, n, ch_bits)


if __name__ == "__main__":
    main()
