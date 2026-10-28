#!/usr/bin/env python3
"""Parse rme-probe RMEP capture files and test the Phase 0 hypotheses."""
import struct
import sys
from collections import Counter
from pathlib import Path

HDR = struct.Struct("<4sIIIIII")
REC = struct.Struct("<QII")
MAGIC = b"RMEP"


def s24le(b):
    v = b[0] | (b[1] << 8) | (b[2] << 16)
    return v - (1 << 24) if v & 0x800000 else v


def parse(path):
    data = Path(path).read_bytes()
    if len(data) < HDR.size:
        raise SystemExit(f"{path}: too short")
    magic, ver, iface, alt, ep, pkt_len, ppu = HDR.unpack_from(data, 0)
    if magic != MAGIC:
        raise SystemExit(f"{path}: bad magic {magic!r}")
    print(f"{path}: RMEP v{ver} iface={iface} alt={alt} ep=0x{ep:02x} "
          f"pkt_len={pkt_len} pkts_per_urb={ppu}")

    off = HDR.size
    recs = []
    while off + REC.size <= len(data):
        ts, ln, idx = REC.unpack_from(data, off)
        off += REC.size
        payload = data[off:off + ln]
        off += ln
        recs.append((ts, idx, payload))
    if off != len(data):
        print(f"warning: {len(data) - off} trailing bytes")

    if not recs:
        print("no records")
        return

    lens = Counter(len(p) for _, _, p in recs)
    total = sum(len(p) for _, _, p in recs)
    print(f"records={len(recs)} bytes={total} "
          f"duration={((recs[-1][0] - recs[0][0]) / 1e9):.3f}s")
    print("lengths:", ", ".join(f"{k}B x{v}" for k, v in sorted(lens.items())))

    for n, (ts, idx, p) in enumerate(recs[:4]):
        print(f"\nrecord {n}: ts={ts} idx={idx} len={len(p)}")
        print("  first 64:", " ".join(f"{b:02x}" for b in p[:64]))

    if iface == 0:
        analyze_iso(recs, pkt_len)
    elif iface == 5:
        analyze_int(recs)


def analyze_iso(recs, pkt_len):
    print("\nIF0 ISO analysis")
    print("  confirmed frame: 4-byte timestamp + N x 72 bytes "
          "(18 x float32)")
    frames = Counter()
    bad = 0
    total_frames = 0
    good = []
    for _, _, p in recs:
        if len(p) < 4 or (len(p) - 4) % 72:
            bad += 1
            continue
        n = (len(p) - 4) // 72
        if n == 0:
            bad += 1
            continue
        frames[n] += 1
        total_frames += n
        good.append(p)
    print("  frames/packet:", dict(sorted(frames.items())))
    if bad:
        print(f"  ignored {bad} non-audio record(s)")

    if not good:
        return

    print("  first timestamp words (word0, word1, word0>>5, word0&31, word1):")
    for p in good[:8]:
        w0, w1 = struct.unpack_from("<HH", p, 0)
        print(f"    {w0:5d} {w1:5d} -> sample={w0 >> 5:5d} "
              f"frac5={w0 & 31:2d} phase=0x{w1:04x}")

    for n, p in enumerate(good[:2]):
        print(f"  record {n}: {len(p)} bytes, {((len(p) - 4) // 72)} frames")
        for f in range(min(2, (len(p) - 4) // 72)):
            vals = struct.unpack_from("<18f", p, 4 + f * 72)
            print(f"    frame {f}: " +
                  " ".join(f"{v:+.6g}" for v in vals[:8]) + " ...")

    # Rough rate estimate from the capture's own timestamps.
    dur = (recs[-1][0] - recs[0][0]) / 1e9
    if dur > 0:
        print(f"  audio frames: {total_frames}, duration {dur:.3f}s, "
              f"~{total_frames / dur:.0f} Hz")


def analyze_int(recs):
    print("\nIF5 interrupt analysis (hypothesis: 18 ch packed 24-bit, 54 B/frame)")
    frames = Counter()
    for _, _, p in recs:
        if len(p) % 54 == 0:
            frames[len(p) // 54] += 1
        else:
            print(f"  transfer len {len(p)} is not a multiple of 54")
    print("  frames/transfer:", dict(sorted(frames.items())))
    p = recs[0][2]
    n = min(len(p) // 3, 18)
    print("  first frame, signed 24-bit samples:", " ".join(str(s24le(p[i * 3:i * 3 + 3])) for i in range(n)))
    print("  first 64 bytes:", " ".join(f"{b:02x}" for b in p[:64]))


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit("usage: parse_capture.py <capture.bin> [...]")
    for f in sys.argv[1:]:
        parse(f)
        print()
