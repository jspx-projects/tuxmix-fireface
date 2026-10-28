#!/usr/bin/env bash
# Low-rate gate for the Fireface UC transport.
#
# This is the mandatory 48 kHz regression check before any high-rate work.
# It verifies the same audio path twice:
#
#   direct   ALSA hw:<card>,0, 18 channels, with PipeWire stopped
#   pipewire PipeWire Pro Audio, 18 channels, source + sink monitor taps
#
# The test tone is software playback 1.  It is routed by the device matrix
# to output block 17 (ADAT 7/8), looped back to input 18, and measured on
# capture channel 18.  No analogue cable is required.
#
# The script writes and then clears these mixer cells:
#   matrix block 17, source 18
#   master 17
#   output volume encoder 21 (ADAT 7/8)
#   loopback index 17
# The UC mixer is write-only for these cells; the previous values are not
# restored.  Run it on the bench or with that output unused.
#
# Usage:
#   scripts/ffuc-48k-gate.sh all       # manages PipeWire user services
#   scripts/ffuc-48k-gate.sh direct    # PipeWire must already be stopped
#   scripts/ffuc-48k-gate.sh pipewire  # PipeWire must already be running
#
# Options:
#   --card N       ALSA card number (default: autodetect Fireface UC/UCX)
#   -h, --help
#
# Requirements: the fireface-ctl example, Python 3, alsa-utils, pipewire,
# wireplumber and pulseaudio-utils (pactl).  Build the control example with:
#   cargo build -p tuxmix-core --example fireface-ctl --features alsa
#
# This gate covers the UC only.  UCX and Babyface Pro FS have separate
# transport/protocol validation.
#
# Target-selection and rate-layer rules: docs/LOW-RATE-GATE.md

set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
CTL="$ROOT/ui/tuxmix/target/debug/examples/fireface-ctl"
MAKE_TONE="$ROOT/scripts/make_test_wav.py"

MODE="all"
CARD=""
PW_WAS_ACTIVE=0
PW_STOPPED_BY_US=0
PW_STARTED_BY_US=0
ORIG_FORCE_RATE=""
TMP=""
TONE=""
SRC_RAW=""
MON_RAW=""
DIRECT_WAV=""

usage() {
    sed -n '2,50p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

note() {
    printf '== %s\n' "$*"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        all|direct|pipewire) MODE="$1" ;;
        --card)
            shift
            [ "$#" -gt 0 ] || fail "--card needs a number"
            CARD="$1"
            ;;
        --card=*) CARD="${1#--card=}" ;;
        -h|--help) usage; exit 0 ;;
        *) fail "unknown argument: $1 (try --help)" ;;
    esac
    shift
done

for cmd in python3 aplay arecord pactl pw-play pw-record pw-link pw-metadata fuser; do
    command -v "$cmd" >/dev/null 2>&1 || fail "missing command: $cmd"
done
[ -x "$CTL" ] || fail "missing $CTL — run: cargo build -p tuxmix-core --example fireface-ctl --features alsa"
[ -f "$MAKE_TONE" ] || fail "missing $MAKE_TONE"

find_card() {
    local id path
    for path in /proc/asound/card*/id; do
        [ -r "$path" ] || continue
        id=$(cat "$path")
        case "$id" in
            FirefaceUC*|FirefaceUCX*)
                path=${path#/proc/asound/card}
                printf '%s\n' "${path%/id}"
                return 0
                ;;
        esac
    done
    return 1
}

if [ -z "$CARD" ]; then
    CARD=$(find_card) || fail "no Fireface UC/UCX card found; pass --card N"
fi
[ -r "/proc/asound/card$CARD/id" ] || fail "ALSA card $CARD does not exist"
CARD_ID=$(cat "/proc/asound/card$CARD/id")
case "$CARD_ID" in
    FirefaceUC*|FirefaceUCX*) ;;
    *) fail "card $CARD is '$CARD_ID', not a Fireface UC/UCX" ;;
esac

MODEL=$("$CTL" "$CARD" info 2>/dev/null | sed -n 's/.*model=\([0-9]*\).*/\1/p' || true)
[ "$MODEL" = "1" ] || fail "this gate is validated for the UC only (model=$MODEL)"
if pgrep -x tuxmix-gui >/dev/null 2>&1; then
    fail "TuxMix is running; stop it so only one writer owns the mixer"
fi

TMP=$(mktemp -d "${TMPDIR:-/tmp}/ffuc-48k-gate.XXXXXX")
TONE="$TMP/tone-18ch-48k.wav"
SRC_RAW="$TMP/source-18ch-48k.raw"
MON_RAW="$TMP/monitor-18ch-48k.raw"
DIRECT_WAV="$TMP/direct-18ch-48k.wav"

python3 "$MAKE_TONE" --channels 18 --rate 48000 --seconds 5 \
    --tone-channels 1 --out "$TONE" >/dev/null

# ---------------------------------------------------------------------------
# mixer helpers
# ---------------------------------------------------------------------------

set_route() {
    # matrix source 18 is software playback 1; block 17 is ADAT 7/8.
    "$CTL" "$CARD" loopback 17 0 >/dev/null
    "$CTL" "$CARD" mix 17 18 1.0 >/dev/null
    "$CTL" "$CARD" master 17 1.0 >/dev/null
    "$CTL" "$CARD" output 21 1.0 >/dev/null
}

enable_loopback() {
    # Enable while playback is already running.  This is the verified
    # lifecycle for the gate; do not enable before the stream starts here.
    "$CTL" "$CARD" loopback 17 1 >/dev/null
}

clear_route() {
    "$CTL" "$CARD" loopback 17 0 >/dev/null 2>&1 || true
    "$CTL" "$CARD" mix 17 18 0.0 >/dev/null 2>&1 || true
    "$CTL" "$CARD" master 17 0.0 >/dev/null 2>&1 || true
    "$CTL" "$CARD" output 21 0.0 >/dev/null 2>&1 || true
}

# ---------------------------------------------------------------------------
# PipeWire service helpers (used only by "all")
# ---------------------------------------------------------------------------

pw_stop() {
    systemctl --user stop \
        pipewire.socket pipewire-pulse.socket pipewire.service \
        wireplumber.service pipewire-pulse.service >/dev/null 2>&1 || true
    PW_STOPPED_BY_US=1
}

pw_start() {
    systemctl --user start \
        pipewire.socket pipewire-pulse.socket pipewire.service \
        wireplumber.service pipewire-pulse.service >/dev/null 2>&1
    PW_STARTED_BY_US=1
    local i
    for i in $(seq 1 50); do
        if pactl list short sources 2>/dev/null | grep -q 'RME.*pro-input-0'; then
            return 0
        fi
        sleep 0.1
    done
    fail "PipeWire did not expose the RME Pro Audio source"
}

restore_pipewire() {
    if [ "$PW_STOPPED_BY_US" = 1 ] && [ "$PW_STARTED_BY_US" = 0 ] && [ "$PW_WAS_ACTIVE" = 1 ]; then
        pw_start >/dev/null 2>&1 || true
    fi
    if [ "$PW_WAS_ACTIVE" = 0 ] && [ "$PW_STARTED_BY_US" = 1 ]; then
        pw_stop >/dev/null 2>&1 || true
    fi
}

cleanup() {
    clear_route >/dev/null 2>&1 || true
    if [ -n "${ORIG_FORCE_RATE:-}" ] && command -v pw-metadata >/dev/null 2>&1; then
        pw-metadata -n settings 0 clock.force-rate "$ORIG_FORCE_RATE" >/dev/null 2>&1 || true
    fi
    restore_pipewire >/dev/null 2>&1 || true
    [ -n "$TMP" ] && rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# analysis
# ---------------------------------------------------------------------------

analyze() {
    python3 - "$@" <<'PY'
import array
import math
import sys
import wave

def db(x):
    return 20.0 * math.log10(max(x, 1e-12))

def wav_channel(path, ch):
    w = wave.open(path, "rb")
    try:
        if w.getnchannels() != 18 or w.getframerate() != 48000:
            raise SystemExit(
                f"FAIL: {path}: {w.getnchannels()} ch at {w.getframerate()} Hz, "
                "expected 18 ch at 48000 Hz"
            )
        raw = w.readframes(w.getnframes())
    finally:
        w.close()
    a = array.array("i")
    a.frombytes(raw)
    if sys.byteorder != "little":
        a.byteswap()
    frames = len(a) // 18
    return [a[i * 18 + ch] / 2147483648.0 for i in range(frames)]

def raw_channel(path, ch):
    a = array.array("i")
    with open(path, "rb") as f:
        a.frombytes(f.read())
    if sys.byteorder != "little":
        a.byteswap()
    frames = len(a) // 18
    if frames == 0:
        raise SystemExit(f"FAIL: {path}: no complete 18-channel frames")
    return [a[i * 18 + ch] / 2147483648.0 for i in range(frames)]

def rms_db(v, start, end):
    a = int(start * 48000)
    b = min(len(v), int(end * 48000))
    if b <= a:
        raise SystemExit(f"FAIL: empty analysis window {start}..{end}")
    n = b - a
    return db(math.sqrt(sum(x * x for x in v[a:b]) / n))

kind, path = sys.argv[1], sys.argv[2]
get = wav_channel if kind == "wav" else raw_channel

if sys.argv[3] == "direct":
    ch18 = get(path, 17)
    pre = rms_db(ch18, 0.0, 0.6)
    post = rms_db(ch18, 1.5, 4.5)
    print(f"direct input18: pre-loopback {pre:.1f} dBFS, after-loopback {post:.1f} dBFS")
    if pre > -70.0:
        raise SystemExit("FAIL: input 18 carried signal before loopback was enabled")
    if post < -50.0:
        raise SystemExit("FAIL: input 18 did not receive the looped-back tone")
elif sys.argv[3] == "pipewire-source":
    ch18 = get(path, 17)
    pre = rms_db(ch18, 0.0, 0.6)
    post = rms_db(ch18, 1.5, 4.5)
    print(f"PipeWire source input18: pre-loopback {pre:.1f} dBFS, after-loopback {post:.1f} dBFS")
    if pre > -70.0:
        raise SystemExit("FAIL: source input 18 carried signal before loopback was enabled")
    if post < -50.0:
        raise SystemExit("FAIL: source input 18 did not receive the looped-back tone")
elif sys.argv[3] == "pipewire-monitor":
    ch1 = get(path, 0)
    level = rms_db(ch1, 0.2, 4.5)
    print(f"PipeWire sink monitor channel 1: {level:.1f} dBFS")
    if level < -50.0:
        raise SystemExit("FAIL: sink monitor did not carry the playback tone")
else:
    raise SystemExit(f"FAIL: unknown analysis kind {sys.argv[3]}")
PY
}

wait_hw_params() {
    local pcm="$1" rate channels i
    for i in $(seq 1 40); do
        if [ -r "/proc/asound/card$CARD/$pcm/sub0/hw_params" ]; then
            rate=$(sed -n 's/^rate: \([0-9]*\).*/\1/p' \
                "/proc/asound/card$CARD/$pcm/sub0/hw_params" 2>/dev/null || true)
            channels=$(sed -n 's/^channels: \([0-9]*\).*/\1/p' \
                "/proc/asound/card$CARD/$pcm/sub0/hw_params" 2>/dev/null || true)
            if [ "$rate" = "48000" ] && [ "$channels" = "18" ]; then
                return 0
            fi
        fi
        sleep 0.05
    done
    fail "$pcm did not open at 18 channels / 48000 Hz"
}

# ---------------------------------------------------------------------------
# direct ALSA phase
# ---------------------------------------------------------------------------

run_direct() {
    note "direct ALSA: 18 channels at 48 kHz"
    if fuser -s \
        "/dev/snd/pcmC${CARD}D0p" "/dev/snd/pcmC${CARD}D0c" 2>/dev/null; then
        fail "PipeWire (or another process) is holding the PCM; stop PipeWire first"
    fi

    set_route
    arecord -D "hw:$CARD,0" -f S32_LE -c 18 -r 48000 \
        --period-size 128 --buffer-size 512 -d 6 "$DIRECT_WAV" \
        >/dev/null 2>&1 &
    local rec=$!
    sleep 0.2
    aplay -D "hw:$CARD,0" -f S32_LE -c 18 -r 48000 \
        --period-size 128 --buffer-size 512 "$TONE" >/dev/null 2>&1 &
    local play=$!
    sleep 1.0
    wait_hw_params pcm0p
    wait_hw_params pcm0c
    enable_loopback
    wait "$play"
    wait "$rec" || fail "arecord failed"
    clear_route
    analyze wav "$DIRECT_WAV" direct
}

# ---------------------------------------------------------------------------
# PipeWire phase
# ---------------------------------------------------------------------------

pw_save_force_rate() {
    ORIG_FORCE_RATE=$(pw-metadata -n settings 0 2>/dev/null |
        sed -n "s/.*key:'clock.force-rate' value:'\\([0-9]*\\)'.*/\\1/p" | head -1)
    [ -n "$ORIG_FORCE_RATE" ] || ORIG_FORCE_RATE=0
}

pw_require_nodes() {
    local i
    for i in $(seq 1 50); do
        if pactl list short sources 2>/dev/null | grep -q 'RME.*pro-input-0' &&
           pactl list short sinks 2>/dev/null | grep -q 'RME.*pro-output-0'; then
            return 0
        fi
        sleep 0.1
    done
    fail "PipeWire has no RME Pro Audio source/sink; select the pro-audio profile"
}

run_pipewire() {
    note "PipeWire Pro Audio: 18 channels at 48 kHz"
    pw_require_nodes
    pw_save_force_rate
    pw-metadata -n settings 0 clock.force-rate 48000 >/dev/null
    sleep 0.5

    local sink_name sink_index source_name source_index
    sink_name=$(pactl list short sinks | awk '/RME.*pro-output-0/ {print $2; exit}')
    sink_index=$(pactl list short sinks | awk '/RME.*pro-output-0/ {print $1; exit}')
    source_name=$(pactl list short sources | awk '/RME.*pro-input-0/ {print $2; exit}')
    source_index=$(pactl list short sources | awk '/RME.*pro-input-0/ {print $1; exit}')
    [ -n "$sink_name" ] && [ -n "$sink_index" ] || fail "RME Pro Audio sink not found"
    [ -n "$source_name" ] && [ -n "$source_index" ] || fail "RME Pro Audio source not found"

    set_route
    rm -f "$SRC_RAW" "$MON_RAW"
    # Source: exact Pulse source index.  Monitor: sink index plus
    # stream.capture.sink=true.  Never use a monitor name here: a missing
    # target silently falls back to the default source.
    pw-record --target "$source_index" --format s32 --rate 48000 --channels 18 \
        --raw "$SRC_RAW" >/dev/null 2>&1 &
    local src_pid=$!
    pw-record --target "$sink_index" --properties stream.capture.sink=true \
        --format s32 --rate 48000 --channels 18 \
        --raw "$MON_RAW" >/dev/null 2>&1 &
    local mon_pid=$!
    sleep 0.4

    pw-play --target "$sink_name" --rate 48000 --channels 18 "$TONE" >/dev/null 2>&1 &
    local play_pid=$!
    sleep 1.0
    wait_hw_params pcm0p
    wait_hw_params pcm0c

    local links
    links=$(pw-link -l 2>/dev/null || true)
    printf '%s\n' "$links" | grep -q 'capture_AUX17' ||
        fail "source tap is not linked to capture_AUX17"
    printf '%s\n' "$links" | grep -q 'monitor_AUX17' ||
        fail "monitor tap is not linked to monitor_AUX17"

    enable_loopback
    wait "$play_pid"
    sleep 0.3
    kill "$src_pid" "$mon_pid" 2>/dev/null || true
    wait "$src_pid" 2>/dev/null || true
    wait "$mon_pid" 2>/dev/null || true
    clear_route

    analyze raw "$SRC_RAW" pipewire-source
    analyze raw "$MON_RAW" pipewire-monitor
}

# ---------------------------------------------------------------------------

case "$MODE" in
    direct)
        run_direct
        ;;
    pipewire)
        run_pipewire
        ;;
    all)
        command -v systemctl >/dev/null 2>&1 || fail "all needs systemctl --user; use direct/pipewire manually"
        if systemctl --user is-active --quiet pipewire.service; then
            PW_WAS_ACTIVE=1
            pw_stop
            sleep 1
        fi
        run_direct
        pw_start
        run_pipewire
        if [ "$PW_WAS_ACTIVE" = 0 ]; then
            pw_stop
            PW_STARTED_BY_US=0
        fi
        ;;
esac

note "PASS: Fireface UC 48 kHz gate"
