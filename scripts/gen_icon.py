#!/usr/bin/env python3
"""Convert the master app icon (assets/icon-master.png) into the artifacts we
ship, with no third-party dependencies:

  assets/icon.png     1024x1024 squared and padded, used for the macOS .icns
                      (and copied into the Windows zip)
  src/icon_data.rs    a 32x32 RGBA byte array embedded into the binary for the
                      runtime tray icon, so the app ships no asset files

Only 8-bit RGBA / RGB / grayscale / palette, non-interlaced PNGs are supported
as input (any normal PNG from an artist tool)."""
import struct
import sys
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MASTER = ROOT / "assets" / "icon-master.png"
APP_ICON = ROOT / "assets" / "icon.png"
SRC_DATA = ROOT / "src" / "icon_data.rs"

APP_ICON_SIZE = 1024
TRAY_SIZE = 32


def png_chunks(data: bytes):
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
    off = 8
    while off + 8 <= len(data):
        (length,) = struct.unpack_from(">I", data, off)
        tag = data[off + 4 : off + 8]
        yield tag, data[off + 8 : off + 8 + length]
        off += 12 + length


def decode_png(path: Path):
    """Return (width, height, rgba bytes) for a supported PNG."""
    data = path.read_bytes()
    ihdr = None
    idat = bytearray()
    plte = b""
    trns = b""
    for tag, payload in png_chunks(data):
        if tag == b"IHDR":
            ihdr = payload
        elif tag == b"IDAT":
            idat += payload
        elif tag == b"PLTE":
            plte = payload
        elif tag == b"tRNS":
            trns = payload
        elif tag == b"IEND":
            break
    assert ihdr is not None, "no IHDR"
    w, h, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", ihdr)
    assert depth == 8, f"unsupported bit depth {depth}"
    assert interlace == 0, "interlaced PNGs are not supported"
    raw = zlib.decompress(bytes(idat))

    channels = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}[ctype]
    stride = w * channels
    px = bytearray(w * h * channels)
    prev = bytearray(stride)
    off = 0
    for y in range(h):
        flt = raw[off]
        off += 1
        line = bytearray(raw[off : off + stride])
        off += stride
        if flt == 1:  # Sub
            for i in range(channels, stride):
                line[i] = (line[i] + line[i - channels]) & 0xFF
        elif flt == 2:  # Up
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif flt == 3:  # Average
            for i in range(stride):
                a = line[i - channels] if i >= channels else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 0xFF
        elif flt == 4:  # Paeth
            for i in range(stride):
                a = line[i - channels] if i >= channels else 0
                b = prev[i]
                c = prev[i - channels] if i >= channels else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                paeth = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + paeth) & 0xFF
        elif flt != 0:
            raise ValueError(f"bad filter {flt}")
        px[y * stride : (y + 1) * stride] = line
        prev = line

    # Normalize everything to RGBA.
    rgba = bytearray(w * h * 4)
    for i in range(w * h):
        if ctype == 6:
            rgba[i * 4 : i * 4 + 4] = px[i * 4 : i * 4 + 4]
        elif ctype == 2:
            rgba[i * 4 : i * 4 + 3] = px[i * 3 : i * 3 + 3]
            rgba[i * 4 + 3] = 255
        elif ctype == 0:
            v = px[i]
            rgba[i * 4 : i * 4 + 3] = bytes((v, v, v))
            rgba[i * 4 + 3] = 255
        elif ctype == 4:
            rgba[i * 4] = rgba[i * 4 + 1] = rgba[i * 4 + 2] = px[i * 2]
            rgba[i * 4 + 3] = px[i * 2 + 1]
        elif ctype == 3:
            j = px[i] * 3
            rgba[i * 4 : i * 4 + 3] = plte[j : j + 3]
            rgba[i * 4 + 3] = trns[px[i]] if px[i] < len(trns) else 255
    return w, h, bytes(rgba)


def square_pad(w, h, rgba):
    """Center the artwork on a transparent square canvas."""
    size = max(w, h)
    out = bytearray(size * size * 4)
    for y in range(h):
        o = (y * size + (size - w) // 2) * 4
        s = y * w * 4
        out[o : o + w * 4] = rgba[s : s + w * 4]
    return size, bytes(out)


def resize(src_size, rgba, dst_size):
    """Box-filter resample of RGBA to dst_size x dst_size."""
    sx = src_size / dst_size
    dst = bytearray(dst_size * dst_size * 4)
    for dy in range(dst_size):
        y0, y1 = int(dy * sx), max(int(dy * sx) + 1, int((dy + 1) * sx))
        for dx in range(dst_size):
            x0, x1 = int(dx * sx), max(int(dx * sx) + 1, int((dx + 1) * sx))
            r = g = b = a = n = 0
            for y in range(y0, min(y1, src_size)):
                row = y * src_size
                for x in range(x0, min(x1, src_size)):
                    i = (row + x) * 4
                    r += rgba[i]
                    g += rgba[i + 1]
                    b += rgba[i + 2]
                    a += rgba[i + 3]
                    n += 1
            j = (dy * dst_size + dx) * 4
            dst[j] = round(r / n)
            dst[j + 1] = round(g / n)
            dst[j + 2] = round(b / n)
            dst[j + 3] = round(a / n)
    return bytes(dst)


def encode_png(size, rgba):
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    rows = bytearray()
    stride = size * 4
    for y in range(size):
        rows.append(0)
        rows += rgba[y * stride : (y + 1) * stride]

    def chunk(tag, payload):
        return (
            struct.pack(">I", len(payload))
            + tag
            + payload
            + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)
        )

    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(rows, level=9)) + chunk(b"IEND", b"")


def main() -> int:
    master = Path(sys.argv[1]) if len(sys.argv) > 1 else MASTER
    assert master.is_file(), f"master icon not found: {master}"
    w, h, rgba = decode_png(master)
    sq, sq_rgba = square_pad(w, h, rgba)

    APP_ICON.write_bytes(encode_png(APP_ICON_SIZE, resize(sq, sq_rgba, APP_ICON_SIZE)))

    tray = resize(sq, sq_rgba, TRAY_SIZE)
    body = ",\n".join(
        ", ".join(f"0x{tray[i]:02x}" for i in range(start, min(start + 12, len(tray))))
        for start in range(0, len(tray), 12)
    )
    SRC_DATA.write_bytes(
        "//! Generated by scripts/gen_icon.py from assets/icon-master.png.\n"
        "//! Do not edit by hand.\n"
        f"pub const TRAY_SIZE: u32 = {TRAY_SIZE};\n"
        "pub const TRAY_RGBA: [u8; (TRAY_SIZE * TRAY_SIZE * 4) as usize] = [\n"
        f"{body},\n];\n".encode()
    )
    print(f"{master} -> {APP_ICON.name} ({APP_ICON_SIZE}x{APP_ICON_SIZE}) + {SRC_DATA.name} ({TRAY_SIZE}x{TRAY_SIZE})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
