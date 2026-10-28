#!/usr/bin/env bash
# Capture USB traffic to a pcap file (requires Wireshark/dumpcap and root).
#
# Usage: scripts/capture-usbmon.sh [bus] [out.pcap]
#        scripts/capture-usbmon.sh 1 /tmp/ucx.pcap
#
# While this runs, perform the action you want to decode (plug the device,
# change one TotalMix control in a Windows VM, etc.). Ctrl-C to stop.
set -euo pipefail

bus=${1:-1}
out=${2:-"usbmon${bus}-$(date +%Y%m%d-%H%M%S).pcap"}

if ! command -v dumpcap >/dev/null 2>&1; then
	echo "dumpcap not found. Install wireshark-cli, or use the text fallback:" >&2
	echo "  sudo modprobe usbmon && sudo cat /sys/kernel/debug/usb/usbmon/${bus}u > ${out%.pcap}.txt" >&2
	exit 1
fi

if [[ ${EUID} -ne 0 ]]; then
	echo "dumpcap needs root (or CAP_NET_RAW/CAP_NET_ADMIN): sudo $0 $*" >&2
	exit 1
fi

modprobe usbmon 2>/dev/null || true
echo "Capturing usbmon${bus} to ${out} — Ctrl-C to stop."
exec dumpcap -i "usbmon${bus}" -w "$out"
