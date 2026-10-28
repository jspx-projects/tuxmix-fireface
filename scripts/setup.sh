#!/usr/bin/env bash
# Install the udev rule and enable usbmon. Run once with sudo.
set -euo pipefail

if [[ ${EUID} -ne 0 ]]; then
	echo "Run with sudo: sudo $0" >&2
	exit 1
fi

here=$(cd "$(dirname "$0")/.." && pwd)
rule=/etc/udev/rules.d/99-rme-fireface.rules

install -m 0644 "$here/udev/99-rme-fireface.rules" "$rule"
udevadm control --reload-rules
udevadm trigger --subsystem-match=usb --action=change
modprobe usbmon 2>/dev/null || true

echo "Installed $rule"
echo
echo "Matching devices:"
for d in /dev/bus/usb/*/*; do
	[[ -e "$d" ]] || continue
	if udevadm info -q property -n "$d" 2>/dev/null | grep -qE '^ID_VENDOR_ID=(0424|2a39)$'; then
		ls -l "$d"
	fi
done
echo
echo "Now run: ./rme-probe info"
