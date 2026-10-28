#!/bin/sh
# LEGACY: the 2026-10-25 kernel refactor creates no ALSA mixer controls.
# This demo only works with an old module.  Use TuxMix / the hwdep API.
# Demo: route rear input 5 (matrix source 4) to the phones (outputs 1/2).
# 0 dB is 0x2000 = 8192.  The other crosspoints are cleared.
set -eu
CARD="${1:-FirefaceUCX}"

vals=""
for i in $(seq 0 35); do
	if [ "$i" -eq 4 ]; then
		v=8192
	else
		v=0
	fi
	if [ -z "$vals" ]; then
		vals="$v"
	else
		vals="$vals,$v"
	fi
done

echo "card $CARD: input 5 (source 4) -> outputs 1/2 at 0 dB"
amixer -c "$CARD" cset "name=Output 1 Master Volume" 8192 >/dev/null
amixer -c "$CARD" cset "name=Output 2 Master Volume" 8192 >/dev/null
amixer -c "$CARD" cset "name=Output 1 Mix Volume" "$vals" >/dev/null
amixer -c "$CARD" cset "name=Output 2 Mix Volume" "$vals" >/dev/null
amixer -c "$CARD" sget "name=Output 1 Mix Volume" | head -4
echo "Feed a separate signal into input 5; it should appear on the phones (outputs 1/2)."
echo "Do NOT use the phones->input 5 loopback cable here: that would feed back."
