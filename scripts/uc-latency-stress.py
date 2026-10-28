#!/usr/bin/env python3
"""UC low-latency stress and transport-health probe.

The script assumes audio is already playing through the Fireface UC and that a
tone or music is routed to the Phones output.  It changes only the PipeWire
graph quantum and (optionally) writes harmless alternating values into one
matrix row.

Examples:
    scripts/uc-latency-stress.py --quantum 64 --seconds 30
    scripts/uc-latency-stress.py --quantum 64 --seconds 60 --writes-per-sec 150
    scripts/uc-latency-stress.py --quantum 64 --seconds 60 --load 8 --writes-per-sec 150

Requires: pw-metadata, pw-top.  Transport counters come from the built
`ui/tuxmix/target/debug/examples/fireface-events` (or amixer when present).
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import time
from pathlib import Path


def run(args: list[str], **kw) -> subprocess.CompletedProcess:
    return subprocess.run(args, capture_output=True, text=True, **kw)


ROOT = Path(__file__).resolve().parents[1]
EVENTS = ROOT / "ui/tuxmix/target/debug/examples/fireface-events"


def hwdep_stats(card: str) -> dict[str, int] | None:
    """Driver transport counters from the read-only hwdep example."""
    if not EVENTS.is_file():
        return None
    out = run([str(EVENTS), card, "0"], timeout=8)
    if out.returncode != 0:
        return None
    for line in out.stdout.splitlines():
        if not line.startswith("counters: "):
            continue
        stats: dict[str, int] = {}
        for field in line.removeprefix("counters: ").split():
            key, _, value = field.partition("=")
            try:
                stats[key] = int(value)
            except ValueError:
                return None
        return stats
    return None


def transport_stats(card: str) -> dict[str, int] | list[int] | None:
    stats = hwdep_stats(card)
    if stats is not None:
        return stats
    out = run(["amixer", "-c", card, "cget", "name=Transport Stats"])
    if out.returncode != 0:
        return None
    lines = [ln for ln in out.stdout.splitlines() if "values=" in ln]
    if not lines:
        return None
    return [int(v) for v in lines[-1].split("values=", 1)[1].split(",")]


def stats_delta(before, after):
    if isinstance(before, dict) and isinstance(after, dict):
        return {k: after[k] - before[k] for k in before if k in after}
    return [b - a for a, b in zip(before, after)]


def sink_error(q: int, name_fragment: str = "pro-output-0") -> int | None:
    out = run(["pw-top", "-b", "-n", "3"])
    if out.returncode != 0:
        return None
    for line in out.stdout.splitlines():
        if name_fragment in line and line.lstrip().startswith("R"):
            fields = line.split()
            # S ID QUANT RATE WAIT BUSY W/Q B/Q ERR FORMAT NAME...
            try:
                return int(fields[8])
            except (IndexError, ValueError):
                continue
    return None


def set_quantum(q: int) -> None:
    run(["pw-metadata", "-n", "settings", "0", "clock.force-quantum", str(q)])


def write_crosspoint(card: str, output: int, source: int, raw: int) -> None:
    vals = ["0"] * 36
    vals[source] = str(raw)
    run(
        [
            "amixer",
            "-c",
            card,
            "cset",
            f"name=Output {output} Mix Volume",
            ",".join(vals),
        ]
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--card", default="2", help="ALSA card number (default: 2)")
    ap.add_argument("--quantum", type=int, default=64)
    ap.add_argument("--seconds", type=float, default=30.0)
    ap.add_argument("--writes-per-sec", type=float, default=0.0)
    ap.add_argument("--output", type=int, default=7, help="ALSA output row for writes")
    ap.add_argument("--source", type=int, default=18, help="matrix source for writes")
    ap.add_argument("--load", type=int, default=0, help="number of CPU burner processes")
    args = ap.parse_args()

    set_quantum(args.quantum)
    time.sleep(2.0)

    stats0 = transport_stats(args.card)
    err0 = sink_error(args.quantum)
    if stats0 is None:
        print("warning: transport counters not available", file=sys.stderr)
    if err0 is None:
        print("warning: no running Fireface sink in pw-top", file=sys.stderr)

    load: list[subprocess.Popen] = []
    if args.load > 0:
        for _ in range(args.load):
            load.append(
                subprocess.Popen(
                    ["yes"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
                )
            )

    start = time.monotonic()
    writes = 0
    next_write = start
    try:
        while time.monotonic() - start < args.seconds:
            now = time.monotonic()
            if args.writes_per_sec > 0 and now >= next_write:
                raw = 8192 if (writes // 5) % 2 == 0 else 8000
                write_crosspoint(args.card, args.output, args.source, raw)
                writes += 1
                next_write += 1.0 / args.writes_per_sec
            time.sleep(0.02)
    finally:
        for p in load:
            p.terminate()
        for p in load:
            p.wait()
        # Leave the matrix at unity so the test cannot leave a faded route.
        if writes:
            write_crosspoint(args.card, args.output, args.source, 8192)

    stats1 = transport_stats(args.card)
    err1 = sink_error(args.quantum)
    print(f"quantum          {args.quantum}")
    print(f"duration         {time.monotonic() - start:.1f} s")
    print(f"matrix writes    {writes}")
    print(f"transport before {stats0}")
    print(f"transport after  {stats1}")
    if stats0 and stats1:
        print(f"transport delta  {stats_delta(stats0, stats1)}")
    print(f"pw-top ERR       {err0} -> {err1}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
