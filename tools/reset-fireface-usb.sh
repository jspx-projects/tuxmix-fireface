#!/bin/sh
# Reset the RME Fireface USB device without physically unplugging it.
# Installed as /usr/local/sbin/reset-fireface-usb with a NOPASSWD rule.
set -eu

found=0
for id in 0424:3fc5 0424:3fc6 2a39:3fc6 0424:3fc9 2a39:3fc9; do
    if /usr/bin/lsusb -d "$id" >/dev/null 2>&1; then
        echo "resetting $id"
        /usr/bin/usbreset "$id"
        found=1
    fi
done

if [ "$found" -eq 0 ]; then
    echo "No Fireface USB device found" >&2
    exit 1
fi

# Wait for the ALSA card to be re-created.
i=0
while [ "$i" -lt 50 ]; do
    if grep -q Fireface /proc/asound/cards 2>/dev/null; then
        exit 0
    fi
    i=$((i + 1))
    sleep 0.1
done

echo "Fireface did not reappear in /proc/asound/cards" >&2
exit 1
