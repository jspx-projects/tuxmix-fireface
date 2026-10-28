#!/bin/sh
# Apply the confirmed UCX init/mixer state used by the Phase 0 playback probe.
#
#   0x16 init for channels 0..17 and 0x20..0x31
#   0x10 settings word, 0x20 global enable
#   output masters 992 + o at 0 dB (0x2000)
#   playback 1/2 routed to every output (crosspoint source + 36*output)
#
# The probe's play command sends the 0x14 unmute and, by default, uses the
# confirmed 436-byte OUT layout (--format p) with a 396-frame live position
# lead. Run this script once after plugging in / power-cycling the UCX.
set -eu
cd "$(dirname "$0")/.."
P=./rme-probe

for ch in $(seq 0 17); do
	$P write 0x16 1 "$ch" --yes >/dev/null
	$P write 0x16 1 $((ch + 0x20)) --yes >/dev/null
done
$P write 0x10 0x0800 0xb800 --yes >/dev/null
$P write 0x20 1 0 --yes >/dev/null

for o in $(seq 0 17); do
	$P write 0x12 0x2000 $((992 + o)) --yes >/dev/null
done

for o in $(seq 0 17); do
	$P write 0x12 0x2000 $((36 * o + 18)) --yes >/dev/null
	$P write 0x12 0x2000 $((36 * o + 19)) --yes >/dev/null
done

echo "UCX init + output masters (0 dB) + playback 1/2 -> all outputs applied."
