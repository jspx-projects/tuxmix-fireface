#!/bin/sh
# Build the out-of-tree Fireface kernel module against the running kernel.
#
# The project path is space-free (rme-fireface-linux-driver), so M= can point
# straight at kernel/.  The running kernel needs the exact kernel-devel
# package; if it is missing, install kernel-devel-$(uname -r).
set -eu
cd "$(dirname "$0")/.."
SRC="$PWD/kernel"

if [ ! -d "/lib/modules/$(uname -r)/build" ]; then
	echo "missing kernel headers for $(uname -r)" >&2
	echo "install kernel-devel-$(uname -r) (the exact running version)" >&2
	exit 1
fi

make -C "/lib/modules/$(uname -r)/build" M="$SRC" modules
echo "built $SRC/snd-fireface-uc.ko for $(uname -r)"
