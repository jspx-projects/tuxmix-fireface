#!/bin/sh
# Install the Fireface UC transport module through DKMS.
#
# Fedora runs /usr/lib/kernel/install.d/40-dkms.install when a new kernel is
# installed.  Once this module is registered, that hook rebuilds it for the
# new kernel automatically.  The systemd unit installed here loads it after
# DKMS at boot.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DKMS_CONF="$ROOT/kernel/dkms.conf"
UNIT="$ROOT/packaging/systemd/snd-fireface-uc-load.service"
KVER=$(uname -r)

PKG=$(sed -n 's/^PACKAGE_NAME="\([^"]*\)"$/\1/p' "$DKMS_CONF")
VER=$(sed -n 's/^PACKAGE_VERSION="\([^"]*\)"$/\1/p' "$DKMS_CONF")
[ -n "$PKG" ] && [ -n "$VER" ] || {
	echo "could not read PACKAGE_NAME/PACKAGE_VERSION from $DKMS_CONF" >&2
	exit 1
}
SRC="/usr/src/$PKG-$VER"

if [ ! -d "/lib/modules/$KVER/build" ]; then
	echo "missing kernel headers for $KVER" >&2
	echo "install kernel-devel-$KVER first" >&2
	exit 1
fi

if [ "$(id -u)" -eq 0 ]; then
	SUDO=
else
	SUDO=sudo
fi

echo "Installing $PKG/$VER for $KVER"

# Refresh the registered source tree.  Build products are not source.
$SUDO dkms remove -m "$PKG" -v "$VER" --all >/dev/null 2>&1 || true
$SUDO rm -rf "$SRC"
$SUDO mkdir -p "$SRC"
$SUDO cp -a "$ROOT/kernel/." "$SRC/"
$SUDO find "$SRC" -maxdepth 1 -type f \
	\( -name '*.o' -o -name '*.ko' -o -name '*.mod' \
	   -o -name '*.mod.c' -o -name '.*.cmd' -o -name 'modules.order' \
	   -o -name 'Module.symvers' \) -delete
$SUDO rm -rf "$SRC/.tmp_versions"

$SUDO dkms add -m "$PKG" -v "$VER"
$SUDO dkms build -m "$PKG" -v "$VER" -k "$KVER"
$SUDO dkms install -m "$PKG" -v "$VER" -k "$KVER" --force
$SUDO modprobe snd-fireface-uc || true

if [ -f "$UNIT" ]; then
	$SUDO install -m 0644 "$UNIT" \
		/etc/systemd/system/snd-fireface-uc-load.service
	$SUDO systemctl daemon-reload
	$SUDO systemctl enable snd-fireface-uc-load.service
fi

# Keep Fedora's DKMS boot service enabled; it retries a build when the
# kernel-install hook ran before the matching headers were available.
if $SUDO systemctl list-unit-files dkms.service >/dev/null 2>&1; then
	$SUDO systemctl enable dkms.service
fi

echo
$SUDO dkms status
echo
echo "DKMS is active for $KVER."
echo "The boot loader unit is enabled; the installed module takes effect on"
echo "the next boot, or after stopping PipeWire/TuxMix and running modprobe."
