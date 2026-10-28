#!/bin/sh
# Compatibility wrapper.  The DKMS installer also installs and enables the
# boot loader unit; see scripts/install-dkms.sh.
set -eu
exec "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/install-dkms.sh" "$@"
