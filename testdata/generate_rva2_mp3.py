#!/usr/bin/env python3
"""Generate rva2-only.mp3 — an MP3 whose only loudness tags are ID3v2.4 RVA2.

ffmpeg cannot write RVA2 (it has no mapping for relative-volume frames; `-metadata
replaygain_*` becomes TXXX), so the tag is assembled byte-for-byte here and
prepended to the untagged `tone.mp3`. The audio is unchanged — this fixture is
about the tag, and `tests/replaygain.rs` cross-checks the result against lofty.

Layout written (ID3v2.4, no unsynchronisation, no extended header):

    "ID3" 04 00 00 <synchsafe size>            10-byte header
    "RVA2" <synchsafe size> 00 00              frame header
        "track" 00                             identification
        01 <adj BE16> 10 <peak BE16>           master volume, 16-bit peak
    "RVA2" <synchsafe size> 00 00
        "album" 00
        01 <adj BE16> 00                       master volume, no peak
    "TIT2" <synchsafe size> 00 00 03 "..."     a plain UTF-8 text frame, so the
                                               fixture is not RVA2-only overall
    00 * PAD                                   padding (as real taggers leave)

`adj` is the adjustment in 1/512 dB: -7.89 dB -> round(-7.89 * 512) = -4040,
-8.52 dB -> -4362. Deliberately *no* `TXXX replaygain_*` frame: a reader that
only knows TXXX must come up empty on this file.

Run:  python testdata/generate_rva2_mp3.py
"""

import struct
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SOURCE = HERE / "tone.mp3"  # untagged MPEG Layer III, 44.1 kHz stereo
OUT = HERE / "rva2-only.mp3"

# Gains the test expects, as 1/512-dB RVA2 adjustments.
TRACK_ADJ = round(-7.89 * 512)  # -4040
ALBUM_ADJ = round(-8.52 * 512)  # -4362
TRACK_PEAK = 0xFCB9  # 16-bit peak (~0.9872 full scale); peak is parsed, not used
PADDING = 64  # trailing zero bytes inside the tag, as taggers leave room

MASTER_VOLUME = 0x01  # RVA2 channel type


def synchsafe(n: int) -> bytes:
    """ID3v2.4 synchsafe integer: 28 bits, 7 per byte, high bit always clear."""
    if not 0 <= n < (1 << 28):
        raise ValueError(f"size {n} does not fit a synchsafe integer")
    return bytes(((n >> 21) & 0x7F, (n >> 14) & 0x7F, (n >> 7) & 0x7F, n & 0x7F))


def frame(frame_id: bytes, body: bytes) -> bytes:
    """A v2.4 frame: 4-byte id, synchsafe size, 2 flag bytes, body."""
    return frame_id + synchsafe(len(body)) + b"\x00\x00" + body


def rva2_body(ident: bytes, adj: int, peak: int | None) -> bytes:
    """`identification NUL`, then the master-volume channel block."""
    body = ident + b"\x00" + bytes([MASTER_VOLUME]) + struct.pack(">h", adj)
    if peak is None:
        return body + b"\x00"  # 0 bits of peak = no peak volume
    return body + b"\x10" + struct.pack(">H", peak)  # 16 bits, then 2 bytes


def main() -> int:
    if not SOURCE.is_file():
        print(f"missing source: {SOURCE}", file=sys.stderr)
        return 1

    frames = b"".join(
        (
            frame(b"RVA2", rva2_body(b"track", TRACK_ADJ, TRACK_PEAK)),
            frame(b"RVA2", rva2_body(b"album", ALBUM_ADJ, None)),
            # Encoding 3 = UTF-8. Gives the fixture an ordinary readable field,
            # so a parser that chokes on RVA2 is distinguishable from one that
            # never reached the tag at all.
            frame(b"TIT2", b"\x03" + "RVA2-only gain".encode()),
        )
    )
    body = frames + bytes(PADDING)
    tag = b"ID3" + bytes((4, 0, 0)) + synchsafe(len(body)) + body

    OUT.write_bytes(tag + SOURCE.read_bytes())
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes; {len(tag)}-byte ID3v2.4 tag)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
