#!/usr/bin/env bash
# Phase 0: read-only characterisation of the connected Fireface.
# Run after `sudo ./scripts/setup.sh`.
set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
out=captures/phase0
mkdir -p "$out"

echo "== info ==" | tee "$out/info.txt"
./rme-probe info | tee -a "$out/info.txt"

echo "== firmware ==" | tee "$out/fw.txt"
./rme-probe fw | tee -a "$out/fw.txt"

echo "== rate ==" | tee "$out/rate.txt"
./rme-probe rate | tee -a "$out/rate.txt"

echo "== status ==" | tee "$out/status.txt"
./rme-probe status | tee -a "$out/status.txt" || true

for spec in "0 1" "5 1"; do
	set -- $spec
	iface=$1
	alt=$2
	f="$out/if${iface}-alt${alt}.bin"
	log="$out/if${iface}-alt${alt}.txt"

	echo "== IF${iface} alt ${alt} (3 s) ==" | tee "$log"
	./rme-probe stream --iface "$iface" --alt "$alt" --seconds 3 \
		--out "$f" --hex 8 | tee -a "$log" || true
	if [[ -s "$f" ]]; then
		echo "-- parse --" | tee -a "$log"
		python3 tools/parse_capture.py "$f" | tee -a "$log" || true
	fi
done

echo
echo "Done. Results in $out/"
