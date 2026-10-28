#!/usr/bin/env python3
"""Generate a multichannel S32_LE WAV test tone.

Examples:
  make_test_wav.py --tone-channels 1,2 --out phones.wav
  make_test_wav.py --channels 18 --tone-channels 7,8 --freq 1000 --seconds 4

Channels are 1-based.  The default is 18 channels, 48 kHz, 4 s, 1 kHz at
0.1 full scale.  The output is signed 32-bit little-endian PCM, which the
driver converts to the device's native float32.
"""
import argparse
import math
import struct
import wave


def parse_channels(text):
    out = []
    for part in text.split(","):
        part = part.strip()
        if part:
            out.append(int(part) - 1)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--channels", type=int, default=18)
    ap.add_argument("--rate", type=int, default=48000)
    ap.add_argument("--seconds", type=float, default=4.0)
    ap.add_argument("--freq", type=float, default=1000.0)
    ap.add_argument("--amp", type=float, default=0.1)
    ap.add_argument("--tone-channels", default="1,2",
                    help="comma-separated 1-based channel list")
    ap.add_argument("--out", default="test-18ch.wav")
    ns = ap.parse_args()

    tone = parse_channels(ns.tone_channels)
    if any(c < 0 or c >= ns.channels for c in tone):
        raise SystemExit("tone channel outside 1..%d" % ns.channels)
    if not 0.0 <= ns.amp <= 1.0:
        raise SystemExit("--amp must be 0..1")

    frames = int(ns.rate * ns.seconds)
    full = int(ns.amp * 2147483647)

    w = wave.open(ns.out, "wb")
    w.setnchannels(ns.channels)
    w.setsampwidth(4)
    w.setframerate(ns.rate)
    for n in range(frames):
        v = int(full * math.sin(2.0 * math.pi * ns.freq * n / ns.rate))
        frame = [0] * ns.channels
        for c in tone:
            frame[c] = v
        w.writeframesraw(struct.pack("<%di" % ns.channels, *frame))
    w.close()
    print(f"wrote {ns.out}: {ns.channels} ch, {ns.rate} Hz, "
          f"{ns.seconds}s, tone on {ns.tone_channels}")


if __name__ == "__main__":
    main()
