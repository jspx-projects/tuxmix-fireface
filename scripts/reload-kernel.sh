#!/bin/sh
# Reload the locally built transport module and show the card/hwdep nodes.
#
# rmmod can fail for two reasons:
#   - PipeWire/wireplumber has the ALSA control/PCM open;
#   - the ALSA card itself holds a module reference until the USB interface
#     is disconnected.
# Stop the user audio services first if possible.  If rmmod still fails, this
# script unbinds the USB interface, which calls the driver's disconnect and
# frees the card.
set -eu
cd "$(dirname "$0")/.."

if ! sudo rmmod snd_fireface_uc; then
	echo "module still in use; trying USB interface unbind..." >&2
	IFACE=$(ls -d /sys/bus/usb/drivers/snd-fireface-uc/*:*.* 2>/dev/null |
		head -1 | xargs -r -n1 basename)
	if [ -z "$IFACE" ]; then
		echo "no bound interface found." >&2
		echo "Stop the audio services: systemctl --user stop wireplumber pipewire pipewire-pulse" >&2
		echo "or unplug the Fireface, then rerun this script." >&2
		exit 1
	fi
	echo "unbinding $IFACE" >&2
	echo -n "$IFACE" | sudo tee /sys/bus/usb/drivers/snd-fireface-uc/unbind >/dev/null
	if ! sudo rmmod snd_fireface_uc; then
		echo "module is still in use; unplug the Fireface, then rerun this script." >&2
		exit 1
	fi
fi

sudo modprobe snd-pcm
# Any extra arguments are passed to insmod, e.g.
#   ./scripts/reload-kernel.sh urb_frames=48 nurbs=8 ucx_if5=Y
sudo insmod kernel/snd-fireface-uc.ko "$@"
echo '--- fireface log ---'
sudo dmesg | grep -i fireface | tail -3 || true
echo '--- card ---'
aplay -l | grep -A2 -i fireface || true
echo '--- hwdep control node ---'
# The kernel exposes raw vendor/register/meter transports here; mixer state
# lives in TuxMix (see docs/HWDEP-ABI.md).
ls -l /dev/snd/hwC*D* 2>/dev/null || true
