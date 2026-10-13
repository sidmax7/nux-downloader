#!/usr/bin/env python3
"""Writes the tiny RAR (v2.9 "RAR 4") fixtures used by the `rar`-feature
tests. The official `rar` tool is not freely available, so the archives
are assembled by hand from the documented block layout (stored, no
compression). Check them with `unrar t`. Re-run to regenerate:

    python3 make_fixtures.py

rar5-solid.rar is not made here: it is a real WinRAR archive (RAR5,
solid, -m3) copied from the `unrar` crate's test data (unrar 0.5.8,
data/solid.rar; MIT OR Apache-2.0).
"""
import struct, zlib

HELLO = b"hello from a rar archive\n"
# Deterministic ~3 KiB payload, so the two-volume set has something to split.
BIG = bytes((i * 31 + 7) % 251 for i in range(3000))

MARKER = b"Rar!\x1a\x07\x00"


def crc16(data):
    return zlib.crc32(data) & 0xFFFF


def block(btype, flags, body):
    # HEAD_CRC(2) TYPE(1) FLAGS(2) SIZE(2) body
    size = 7 + len(body)
    rest = struct.pack("<BHH", btype, flags, size) + body
    return struct.pack("<H", crc16(rest)) + rest


def main_header(flags):
    return block(0x73, flags, b"\x00\x00" + b"\x00\x00\x00\x00")


def file_header(name, data_len, unp_size, file_crc, flags):
    nb = name.encode()
    body = struct.pack(
        "<IIBIIBBHI",
        data_len,  # PACK_SIZE
        unp_size,  # UNP_SIZE
        3,  # HOST_OS: unix
        file_crc,
        0x5A3B0000,  # FTIME
        20,  # UNP_VER
        0x30,  # METHOD: store
        len(nb),
        0o100644,  # ATTR
    ) + nb
    return block(0x74, 0x8000 | flags, body)


def end_block(flags):
    return block(0x7B, 0x4000 | flags, b"")


def single(path, name, data):
    out = MARKER + main_header(0)
    out += file_header(name, len(data), len(data), zlib.crc32(data), 0) + data
    out += end_block(0)
    open(path, "wb").write(out)


def two_volumes(path1, path2, name, data):
    half = len(data) // 2
    a, b = data[:half], data[half:]
    crc = zlib.crc32(data)
    # Volume flags: 0x01 volume, 0x08 solid-less, 0x10 new numbering, 0x100 first.
    v1 = MARKER + main_header(0x0001 | 0x0010 | 0x0100)
    v1 += file_header(name, len(a), len(data), zlib.crc32(a), 0x02) + a  # SPLIT_AFTER: CRC runs over the data so far
    v1 += end_block(0x0001)  # more volumes follow
    v2 = MARKER + main_header(0x0001 | 0x0010)
    v2 += file_header(name, len(b), len(data), crc, 0x01) + b  # SPLIT_BEFORE
    v2 += end_block(0)
    open(path1, "wb").write(v1)
    open(path2, "wb").write(v2)


single("single.rar", "hello.txt", HELLO)
two_volumes("split.part1.rar", "split.part2.rar", "big.bin", BIG)
single("evil.rar", "../evil.txt", HELLO)
