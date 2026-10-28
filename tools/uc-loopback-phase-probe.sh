#!/usr/bin/env bash
# UC digital-loopback polarity probe.
#
# Routes PCM 1 -> AN3 and PCM 2 -> AN4, enables the UC's internal
# loopback on AN3/4 (0x15 indices 2/3), plays a stereo cancellation signal
# (L = +tone, R = -tone) and captures inputs AN3/4.  With no phase
# inversion the two captured channels cancel; flipping either channel's
# phase turns the sum into a strong signal.
#
# This is a DIGITAL loopback test, not an analog cable test.  TuxMix must
# be stopped (one mixer writer).  Restart TuxMix afterwards to restore the
# saved scene; the probe leaves the hardware matrix in its test state.
#
# Usage:
#   tools/uc-loopback-phase-probe.sh [--reset] [--seconds N] [--out DIR]
#                                    [--run-cmd 'shell command']
#
#   --reset       virtual USB reset first (default: no reset)
#   --seconds N   capture length (default 3)
#   --out DIR     output directory (default /tmp/opencode/uc-phase-<stamp>)
#   --run-cmd C   run C between the baseline and the after capture
#
# Requires: fireface-ctl (cargo build -p tuxmix-core --example fireface-ctl
# --features alsa), pactl, pw-play, pw-record, python3+numpy.

set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
CTL="$ROOT/ui/tuxmix/target/debug/examples/fireface-ctl"
CARD=2
STAMP=$(date +%Y%m%dT%H%M%S)
OUT="/tmp/opencode/uc-phase-$STAMP"
SECS=3
RESET=0
RUN_CMD=""

while [ $# -gt 0 ]; do
    case "$1" in
        --reset) RESET=1 ;;
        --seconds) SECS=$2; shift ;;
        --out) OUT=$2; shift ;;
        --run-cmd) RUN_CMD=$2; shift ;;
        -h|--help) sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

[ -x "$CTL" ] || { echo "missing $CTL; build the fireface-ctl example" >&2; exit 1; }
if pgrep -x tuxmix-gui >/dev/null 2>&1; then
    echo "TuxMix is running; stop it first (one mixer writer)" >&2
    exit 1
fi
mkdir -p "$OUT"

if [ "$RESET" -eq 1 ]; then
    sudo -n /usr/local/sbin/reset-fireface-usb >/dev/null
    sleep 2
fi

SINK=$(pactl list short sinks | awk '/RME.*pro-output/{print $2; exit}')
SRC=$(pactl list short sources | awk '/RME.*pro-input/{print $2; exit}')
[ -n "$SINK" ] && [ -n "$SRC" ] || { echo "RME sink/source not found" >&2; exit 1; }

# One graph rate.
pw-metadata -n settings 0 clock.force-rate 48000 >/dev/null
sleep 1

# Stereo cancellation WAV: 18ch, 48 kHz, 1 kHz, +amp on ch1, -amp on ch2.
WAV="$OUT/cancel-1k-18ch.wav"
python3 - "$WAV" <<'PY'
import math, struct, sys, wave
out = sys.argv[1]
rate, secs, amp, chans = 48000, 6, 0.2, 18
full = int(amp * 2147483647)
w = wave.open(out, "wb")
w.setnchannels(chans); w.setsampwidth(4); w.setframerate(rate)
for n in range(rate * secs):
    v = int(full * math.sin(2.0 * math.pi * 1000.0 * n / rate))
    frame = [0] * chans
    frame[0] = v
    frame[1] = -v
    w.writeframesraw(struct.pack("<%di" % chans, *frame))
w.close()
PY

# Clean route: only PCM1 -> AN3 and PCM2 -> AN4.  The mixer matrix uses
# raw output blocks 2/3 for AN3/4 (the UI output ids 4/5 are labels; the
# matrix block is the hardware output channel).
for block in 2 3; do
    for src in $(seq 0 35); do
        "$CTL" "$CARD" mix "$block" "$src" 0.0 >/dev/null
    done
    "$CTL" "$CARD" master "$block" 1.0 >/dev/null
done
"$CTL" "$CARD" mix 2 18 1.0 >/dev/null   # raw block 2 (AN3) <- PCM1
"$CTL" "$CARD" mix 3 19 1.0 >/dev/null   # raw block 3 (AN4) <- PCM2
# Output attenuators for blocks 2/3 (encoder channel 4 + block).
"$CTL" "$CARD" output 6 1.0 >/dev/null
"$CTL" "$CARD" output 7 1.0 >/dev/null
# Only AN3/4 loopback.
for i in $(seq 0 29); do "$CTL" "$CARD" raw 0x15 0 "$i" >/dev/null; done
"$CTL" "$CARD" raw 0x15 1 2 >/dev/null
"$CTL" "$CARD" raw 0x15 1 3 >/dev/null

capture() {
    local label=$1
    pw-play --target "$SINK" "$WAV" >/dev/null 2>&1 &
    local play=$!
    sleep 1
    pw-record --target "$SRC" --rate 48000 --channels 18 --format s32 --raw \
        -n $((48000 * SECS)) "$OUT/$label.raw" >/dev/null 2>&1 || true
    kill "$play" 2>/dev/null || true
    wait "$play" 2>/dev/null || true
}

capture before
if [ -n "$RUN_CMD" ]; then
    echo "run: $RUN_CMD"
    sh -c "$RUN_CMD"
    sleep 0.5
    capture after
fi

python3 - "$OUT" <<'PY'
import glob, os, sys
import numpy as np
out = sys.argv[1]
for p in sorted(glob.glob(os.path.join(out, "*.raw"))):
    a = np.fromfile(p, dtype='<i4')
    a = a[:a.size // 18 * 18].reshape(-1, 18).astype(np.float64) / 2**31
    l, r = a[:, 2], a[:, 3]
    rms = lambda x: 20*np.log10(np.sqrt(np.mean(x*x)) + 1e-12)
    if l.std() < 1e-9 or r.std() < 1e-9:
        corr = float('nan')
    else:
        corr = float(np.corrcoef(l, r)[0, 1])
    s = l + r
    d = l - r
    print(f"{os.path.basename(p)[:-4]:18s} L={rms(l):7.1f} R={rms(r):7.1f} "
          f"corr={corr:+.3f} sum={rms(s):7.1f} diff={rms(d):7.1f} dBFS")
PY

echo "captures in $OUT"
