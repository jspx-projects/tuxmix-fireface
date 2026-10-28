#!/usr/bin/env python3
"""Generate a stereo 24-bit PCM WAV for UCX USB capture tests.

Left channel  = 440 Hz, right channel = 880 Hz, 0.5 full scale, with a
0.5 s lead-in and tail so the start/stop of the tone is easy to find in a
USB capture.  24-bit PCM is the most portable format for Windows players
and DAWs; the RME driver converts it to the device's float32 internally.

Examples:
  make_capture_tone.py --out ucx_48k.wav
  make_capture_tone.py --rate 44100 --out ucx_44k1.wav
"""
import argparse
import math
import struct
import wave


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rate", type=int, default=48000)
    ap.add_argument("--seconds", type=float, default=8.0)
    ap.add_argument("--lead", type=float, default=0.5)
    ap.add_argument("--tail", type=float, default=0.5)
    ap.add_argument("--freq-left", type=float, default=440.0)
    ap.add_argument("--freq-right", type=float, default=880.0)
    ap.add_argument("--amp", type=float, default=0.5)
    ap.add_argument("--out", required=True)
    ns = ap.parse_args()

    if not 0.0 < ns.amp <= 1.0:
        raise SystemExit("--amp must be 0..1")

    lead = int(ns.lead * ns.rate)
    body = int(ns.seconds * ns.rate)
    tail = int(ns.tail * ns.rate)
    total = lead + body + tail
    fade = max(1, int(0.020 * ns.rate))  # 20 ms
    full = ns.amp * 8388607.0

    w = wave.open(ns.out, "wb")
    w.setnchannels(2)
    w.setsampwidth(3)
    w.setframerate(ns.rate)

    out = bytearray()
    for n in range(total):
        if lead <= n < lead + body:
            k = n - lead
            env = 1.0
            if k < fade:
                env = k / fade
            elif k >= body - fade:
                env = (body - 1 - k) / fade
            t = k / ns.rate
            left = int(round(full * env * math.sin(2.0 * math.pi * ns.freq_left * t)))
            right = int(round(full * env * math.sin(2.0 * math.pi * ns.freq_right * t)))
        else:
            left = right = 0
        out += left.to_bytes(3, "little", signed=True)
        out += right.to_bytes(3, "little", signed=True)
    w.writeframes(out)
    w.close()
    print(f"wrote {ns.out}: 2 ch, {ns.rate} Hz, 24-bit PCM, "
          f"{total / ns.rate:.1f}s ({ns.lead}s lead, {ns.seconds}s tone, "
          f"{ns.tail}s tail), L={ns.freq_left:g} Hz R={ns.freq_right:g} Hz, "
          f"amp {ns.amp:g} FS")


if __name__ == "__main__":
    main()
