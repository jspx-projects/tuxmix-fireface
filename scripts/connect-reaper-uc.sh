#!/bin/sh
# Connect REAPER's 18 JACK ports to the Fireface UC in PipeWire Pro Audio mode.
#
# Run after REAPER has opened its JACK device with 18 input and 18 output
# channels. It is idempotent: existing links are left alone and re-creating
# one is harmless.
set -eu

SRC_NAME=$(pactl list short sources | awk '/RME.*pro-input-0/ {print $2; exit}')
SNK_NAME=$(pactl list short sinks | awk '/RME.*pro-output-0/ {print $2; exit}')
SRC_INDEX=$(pactl list short sources | awk '/RME.*pro-input-0/ {print $1; exit}')

[ -n "$SRC_NAME" ] || { echo "Fireface UC source not found" >&2; exit 1; }
[ -n "$SNK_NAME" ] || { echo "Fireface UC sink not found" >&2; exit 1; }

# Make the UC the default source so WirePlumber does not put the webcam on
# REAPER:in1 on the next client start.
wpctl set-default "$SRC_INDEX" >/dev/null 2>&1 || true

# Remove the usual webcam -> first-input auto-link if present.
WEB_NAME=$(pactl list short sources | awk '/Webcam.*mono-fallback/ {print $2; exit}')
if [ -n "$WEB_NAME" ]; then
    pw-link -d "${WEB_NAME}:capture_MONO" REAPER:in1 2>/dev/null || true
fi

# UC capture AUX0..17 -> REAPER inputs 1..18, in hardware order.
i=0
while [ "$i" -lt 18 ]; do
    pw-link "${SRC_NAME}:capture_AUX${i}" "REAPER:in$((i + 1))" 2>/dev/null || true
    pw-link "REAPER:out$((i + 1))" "${SNK_NAME}:playback_AUX${i}" 2>/dev/null || true
    i=$((i + 1))
done

echo "UC capture AUX0..17 -> REAPER in1..18"
echo "REAPER out1..18 -> UC playback AUX0..17"
echo "Reaper input 1 = AN1/mic 1, input 2 = AN2/mic 2, input 5 = AN5."
