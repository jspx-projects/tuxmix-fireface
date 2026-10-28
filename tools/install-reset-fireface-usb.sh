#!/bin/sh
# Install the passwordless USB-reset helper.  Run once with root, e.g.:
#   pkexec /bin/sh tools/install-reset-fireface-usb.sh
set -eu

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
target=${SUDO_USER:-}
if [ -z "$target" ] && [ -n "${PKEXEC_UID:-}" ]; then
    target=$(getent passwd "$PKEXEC_UID" | cut -d: -f1)
fi
[ -n "$target" ] || { echo "cannot determine target user" >&2; exit 1; }

install -o root -g root -m 0755 \
    "$here/reset-fireface-usb.sh" /usr/local/sbin/reset-fireface-usb

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
printf '%s ALL=(root) NOPASSWD: /usr/local/sbin/reset-fireface-usb\n' \
    "$target" >"$tmp"
chmod 0440 "$tmp"
visudo -cf "$tmp" >/dev/null
install -o root -g root -m 0440 "$tmp" /etc/sudoers.d/90-fireface-reset

echo "installed /usr/local/sbin/reset-fireface-usb for $target"
