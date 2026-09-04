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


def sigma_delta_2nd(x: np.ndarray) -> np.ndarray:
    """2nd-order error-feedback modulator: x in [-1,1] -> bits (True = +1).

    NTF = (1 - z^-1)^2, so quantization noise is pushed up in frequency and the
    audio band stays clean after decimation.
    """
    bits = np.empty(x.shape[0], dtype=bool)
    e1 = 0.0
    e2 = 0.0
    for n in range(x.shape[0]):
        u = x[n] + 2.0 * e1 - e2
        y = 1.0 if u >= 0.0 else -1.0
        bits[n] = y > 0.0
        e = u - y
        e2 = e1
        e1 = e
    return bits


def main() -> None:
    out = sys.argv[1] if len(sys.argv) > 1 else "tone.dsf"
    rate = int(sys.argv[2]) if len(sys.argv) > 2 else DSD64
    n = int(DUR * rate)
    t = np.arange(n) / rate

    # One byte per 8 samples; DSF packs LSB-first.
    ch_bytes = []
    for f in FREQS:
        sig = AMP * np.sin(2.0 * np.pi * f * t)
        bits = sigma_delta_2nd(sig)
        ch_bytes.append(np.packbits(bits, bitorder="little"))

    channels = len(FREQS)
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
    print(f"wrote {out}: {total} bytes, {channels}ch {label}, {DUR}s")


if __name__ == "__main__":
    main()
