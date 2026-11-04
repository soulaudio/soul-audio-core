#!/usr/bin/env python3
"""Embed title/artist/album into an existing DSF (trailing ID3v2 via the DSD
chunk's metadata pointer) or DFF (a DIIN chunk with DITI/DIAR). The synthesized
DSD fixtures carry no tags; this adds them so the tag readers title them.
Usage: add_dsd_tags.py <file> <title> <artist> <album>
"""
import struct
import sys


def id3v2(title, artist, album):
    def frame(fid, text):
        data = b"\x00" + text.encode("latin-1", "replace")  # encoding 0 = latin1
        return fid + struct.pack(">I", len(data)) + b"\x00\x00" + data
    body = frame(b"TIT2", title) + frame(b"TPE1", artist) + frame(b"TALB", album)
    n = len(body)
    synchsafe = bytes([(n >> 21) & 0x7F, (n >> 14) & 0x7F, (n >> 7) & 0x7F, n & 0x7F])
    return b"ID3" + b"\x03\x00" + b"\x00" + synchsafe + body


def tag_dsf(path, title, artist, album):
    d = bytearray(open(path, "rb").read())
    # DSD chunk: "DSD "(4) size(8 le) total_file_size(8 le)@12 metadata_ptr(8 le)@20
    assert d[0:4] == b"DSD ", "not a DSF"
    tag = id3v2(title, artist, album)
    ptr = len(d)                       # ID3 appended at the old EOF
    d[20:28] = struct.pack("<Q", ptr)  # metadata pointer -> the ID3 tag
    d += tag
    d[12:20] = struct.pack("<Q", len(d))  # total file size incl. the tag
    open(path, "wb").write(d)


def big_chunk(cid, body):
    out = cid + struct.pack(">Q", len(body)) + body
    if len(body) & 1:
        out += b"\x00"  # IFF even-padding
    return out


def tag_dff(path, title, artist, album):
    d = bytearray(open(path, "rb").read())
    assert d[0:4] == b"FRM8", "not a DFF"
    # DITI/DIAR bodies: u32 BE count + ASCII.
    diti = big_chunk(b"DITI", struct.pack(">I", len(title)) + title.encode("latin-1", "replace"))
    diar = big_chunk(b"DIAR", struct.pack(">I", len(artist)) + artist.encode("latin-1", "replace"))
    diin = big_chunk(b"DIIN", diti + diar)
    # DIIN carries title + artist (no album field in DSDIFF); a trailing `ID3 `
    # chunk carries the full set, which dff.rs also reads.
    id3 = big_chunk(b"ID3 ", id3v2(title, artist, album))
    added = diin + id3
    d += added
    old = struct.unpack(">Q", d[4:12])[0]
    d[4:12] = struct.pack(">Q", old + len(added))  # grow FRM8 by the added chunks
    open(path, "wb").write(d)


def tag_aac(path, title, artist, album):
    """Prepend an ID3v2 tag to a raw ADTS .aac (the iTunes/FAAC convention)."""
    raw = open(path, "rb").read()
    if raw[:3] == b"ID3":
        return  # already tagged
    open(path, "wb").write(id3v2(title, artist, album) + raw)


def main():
    path, title, artist, album = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
    low = path.lower()
    if low.endswith(".dsf"):
        tag_dsf(path, title, artist, album)
    elif low.endswith(".dff"):
        tag_dff(path, title, artist, album)
    elif low.endswith(".aac"):
        tag_aac(path, title, artist, album)
    else:
        raise SystemExit("unknown extension")
    print(f"tagged {path}")


if __name__ == "__main__":
    main()
