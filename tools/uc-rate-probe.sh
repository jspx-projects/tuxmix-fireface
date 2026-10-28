#!/usr/bin/env bash
# UC live sample-rate probe (AN5 hardware capture).
#
# Plays a long 18-channel tone on PCM 1/2 and captures the analogue input
# source around live rate changes.  Channel 5 (0-based 4) is the measurement
# cable input; all captures are analysed per channel.
#
# Usage:
#   tools/uc-rate-probe.sh [--reset] [--seconds N] [--tone-seconds N]
#                          [--out DIR] [--sequence a|b]
#
#   --reset        virtual USB unplug/plug first (default when --reset given)
#   --seconds N    capture length per step (default 4)
#   --tone-seconds N  length of the continuous tone (default 90)
#   --out DIR      output directory (default /tmp/opencode/uc-rate-<stamp>)
#   --sequence a   48 -> 44.1 -> 44.1 -> 48 -> 44.1   (default)
#   --sequence b   48 -> 44.1 (fresh player start after switch)
#
# The tone is kept running for the whole sequence so the rate change is a
# real in-flight change, matching the successful 12:54 test.

set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
STAMP=$(date +%Y%m%dT%H%M%S)
OUT="/tmp/opencode/uc-rate-$STAMP"
SECONDS_PER_STEP=4
TONE_SECONDS=90
DO_RESET=0
SEQUENCE=a

while [ $# -gt 0 ]; do
    case "$1" in
        --reset) DO_RESET=1 ;;
        --seconds) SECONDS_PER_STEP=$2; shift ;;
        --tone-seconds) TONE_SECONDS=$2; shift ;;
        --out) OUT=$2; shift ;;
        --sequence) SEQUENCE=$2; shift ;;
        -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

mkdir -p "$OUT"
log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/probe.log"; }

sink_name() {
    pactl list short sinks | awk '/RME.*pro-output/{print $2; exit}'
}
source_index() {
    pactl list short sources | awk '/RME.*pro-input/{print $2; exit}'
}
node_rate() {
    pactl list short sinks | awk '/RME.*pro-output/{print $6; exit}' | tr -dc '0-9'
}

set_rate() {
    local rate=$1
    log "force-rate $rate"
    pw-metadata -n settings 0 clock.force-rate "$rate" >/dev/null
}

wait_rate() {
    local want=$1 i
    for i in $(seq 1 50); do
        if [ "$(node_rate)" = "$want" ]; then return 0; fi
        sleep 0.1
    done
    log "WARN: node did not reach $want (is $(node_rate))"
    return 0
}

capture() {
    local label=$1 rate
    rate=$(node_rate)
    [ -n "$rate" ] || { log "no RME sink"; return 1; }
    log "capture $label at ${rate}Hz (${SECONDS_PER_STEP}s)"
    timeout 30 pw-record --target "$(source_index)" \
        --rate "$rate" --channels 18 --format s32 --raw \
        -n $((rate * SECONDS_PER_STEP)) "$OUT/$label.raw" \
        >"$OUT/$label.rec.log" 2>&1 || true
    printf '%s\n' "$rate" >"$OUT/$label.rate"
    journalctl -k -n 80 --no-pager 2>/dev/null | grep -E 'usb 1-7: (engine|UC |hw_params)' \
        >>"$OUT/$label.klog" || true
}

if [ "$DO_RESET" -eq 1 ]; then
    log "virtual USB reset"
    sudo -n /usr/local/sbin/reset-fireface-usb | tee -a "$OUT/probe.log"
    sleep 1
fi

log "start: sink=$(sink_name) source=$(source_index) rate=$(node_rate)"
set_rate 48000
wait_rate 48000

TONE="$OUT/tone-${TONE_SECONDS}s-48k.wav"
if [ ! -f "$TONE" ]; then
    python3 "$ROOT/scripts/make_test_wav.py" --channels 18 --rate 48000 \
        --seconds "$TONE_SECONDS" --freq 1000 --amp 0.2 \
        --tone-channels 1,2 --out "$TONE" >/dev/null
fi

SINK=$(sink_name)
log "start continuous tone -> $SINK"
pw-play --target "$SINK" "$TONE" >"$OUT/play.log" 2>&1 &
PLAY_PID=$!
sleep 2

capture 01-48-baseline
if [ "$SEQUENCE" = b ]; then
    set_rate 44100; wait_rate 44100; sleep 2
    kill "$PLAY_PID" 2>/dev/null || true
    wait "$PLAY_PID" 2>/dev/null || true
    sleep 1
    log "restart tone after switch (fresh player open)"
    pw-play --target "$SINK" "$TONE" >"$OUT/play2.log" 2>&1 &
    PLAY_PID=$!
    sleep 2
    capture 02-44-first-open
else
    set_rate 44100; wait_rate 44100; sleep 2
    capture 02-44-first-open
    sleep 1
    capture 03-44-second-open
    set_rate 48000; wait_rate 48000; sleep 2
    capture 04-48-after
    set_rate 44100; wait_rate 44100; sleep 2
    capture 05-44-again
fi

kill "$PLAY_PID" 2>/dev/null || true
wait "$PLAY_PID" 2>/dev/null || true

python3 - "$OUT" <<'PY'
import sys, glob, os
import numpy as np
out = sys.argv[1]
print(f"{'capture':22s} {'rate':>6s} {'dur':>5s} {'ch5 rms':>8s} {'ch5 pk':>7s} "
      f"{'best ch':>8s} {'best rms':>8s} {'windows (dBFS, 0.5s)'}")
for p in sorted(glob.glob(os.path.join(out, '*.raw'))):
    label = os.path.basename(p)[:-4]
    rp = p[:-4] + '.rate'
    rate = int(open(rp).read().strip()) if os.path.exists(rp) else 0
    a = np.fromfile(p, dtype='<i4')
    if a.size == 0:
        print(f"{label:22s} EMPTY"); continue
    a = a[:a.size // 18 * 18].reshape(-1, 18).astype(np.float64) / 2**31
    r = a[:, 4]
    rms = 20*np.log10(np.sqrt(np.mean(r*r)) + 1e-12)
    pk = 20*np.log10(np.max(np.abs(r)) + 1e-12)
    rms_all = [20*np.log10(np.sqrt(np.mean(a[:,c]**2)) + 1e-12) for c in range(18)]
    best = int(np.argmax(rms_all))
    w = max(1, (rate or 48000)//2)
    wins = [20*np.log10(np.sqrt(np.mean(r[i:i+w]**2)) + 1e-12)
            for i in range(0, len(r)-w+1, w)]
    dur = len(r)/(rate or 48000)
    wstr = ' '.join(f'{v:.1f}' for v in wins[:12])
    print(f"{label:22s} {rate:6d} {dur:5.1f} {rms:8.2f} {pk:7.2f} "
          f"{best+1:8d} {rms_all[best]:8.2f} {wstr}")
PY

log "done: $OUT"
