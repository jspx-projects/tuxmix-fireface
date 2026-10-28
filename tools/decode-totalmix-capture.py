#!/usr/bin/env python3
"""Decode a RME TotalMix USB capture into channel/register tables.

Usage:
    tools/decode-totalmix-capture.py capture.pcapng [--only ctl|state|bulk|summary]

The input may be a Windows USBPcap capture or a Linux usbmon capture.  The
script shells out to tshark for packet dissection and understands the three
channels that matter for TotalMix:

  * vendor control requests, especially 0x12 (mixer crosspoint / output
    master) and 0x1a (input gain / output hardware volume);
  * bulk OUT EP 0x0c, the 32-bit [parity|reg|value] register stream;
  * bulk IN EP 0x8d, the device's register-state deltas.

It is deliberately read-only: it only prints what the capture contains, so it
can be used on a capture taken during playback without touching the device.

Notes on the value scales, from the RME Fireface USB captures:

  0x1a, wIndex 0..3   input gain (preamp; UCX 1/2 use the base-3 code)
  0x1a, wIndex 4..21  output hardware volume.  wValue 0x00 = +6 dB,
                      0x06 = 0 dB, 0x38 = -50 dB, 0x3f = -inf.
  0x12, wIndex 0..647 mixer crosspoint: out = idx // 36, src = idx % 36
                      (src 0..17 = input, 18..35 = software playback)
  0x12, wIndex 992..1009 output DSP master (0..8192, 0x2000 = 0 dB)
"""

from __future__ import annotations

import argparse
import collections
import math
import subprocess
import sys
from typing import Dict, Iterable, List, Optional, Tuple

FIELDS = [
    "frame.number",
    "frame.time_relative",
    "usb.src",
    "usb.dst",
    "usb.bmRequestType",
    "usb.setup.bRequest",
    "usb.setup.wValue",
    "usb.setup.wIndex",
    "usb.endpoint_address",
    "usb.capdata",
    "usb.data_fragment",
]


def _int(text: str) -> Optional[int]:
    text = (text or "").strip()
    if not text:
        return None
    try:
        return int(text, 0)
    except ValueError:
        try:
            return int(text, 16)
        except ValueError:
            return None


def _hexbytes(text: str) -> bytes:
    text = (text or "").strip().replace(":", "").replace(" ", "")
    if not text:
        return b""
    if len(text) % 2:
        text = "0" + text
    try:
        return bytes.fromhex(text)
    except ValueError:
        return b""


def _words(data: bytes) -> Iterable[Tuple[int, int, int]]:
    """Yield (index, reg, signed value) for each 32-bit register word."""
    for off in range(0, len(data) - 3, 4):
        w = int.from_bytes(data[off:off + 4], "little")
        reg = (w >> 16) & 0x7FFF
        val = w & 0xFFFF
        if val & 0x8000:
            val -= 0x10000
        yield off // 4, reg, val


def _db_from_output_wire(wire: int) -> str:
    wire &= 0x3F
    if wire == 0x3F:
        return "-inf"
    # The wire value is 6 - dB: 0 = +6 dB, 6 = 0 dB, 0x38 = -50 dB.
    return f"{6 - wire:+d} dB"


_OUTPUT_REGS = {
    0x1200: "AN1", 0x1300: "AN2", 0x1400: "AN3", 0x1500: "AN4",
    0x1600: "AN5", 0x1700: "AN6", 0x1800: "Phones 7", 0x1900: "Phones 8",
    0x1A00: "SPDIF 9", 0x1B00: "SPDIF 10",
    0x1C00: "ADAT 1", 0x1D00: "ADAT 2", 0x1E00: "ADAT 3",
    0x1F00: "ADAT 4", 0x2000: "ADAT 5", 0x2100: "ADAT 6",
    0x2200: "ADAT 7", 0x2300: "ADAT 8",
}


def _bulk_reg_label(reg: int) -> str:
    if reg in _OUTPUT_REGS:
        return f"hardware output {_OUTPUT_REGS[reg]}"
    if (reg & 0xFF) == 0x07 and reg < 0x400:
        return f"input gain {reg >> 8} +1"
    if 0x12E0 <= reg <= 0x12F1:
        return f"input fader position source {reg - 0x12E0}"
    if 0x13E0 <= reg <= 0x13F1:
        return f"input fader right source {reg - 0x13E0}"
    if 0x4000 <= reg <= 0x4011:
        return f"input level linear source {reg - 0x4000}"
    if 0x4040 <= reg <= 0x4051:
        return f"input level right linear source {reg - 0x4040}"
    if 0x4020 <= reg <= 0x4031:
        return f"playback level linear pair {(reg - 0x4020) // 2 + 1}"
    if 0x4060 <= reg <= 0x4071:
        return f"playback level right linear pair {(reg - 0x4061) // 2 + 1}"
    return "register"


def _decode_control(ts: str, src: str, dst: str, bm: int, req: int,
                    val: int, idx: int) -> str:
    direction = "OUT" if not (bm & 0x80) else "IN"
    if req == 0x12:
        where = f"wIndex={idx}"
        if 0 <= idx < 36 * 18:
            out, source = divmod(idx, 36)
            kind = "input" if source < 18 else "playback"
            where += f" (out {out + 1}, {kind} {source + 1 if source < 18 else source - 17})"
        elif 992 <= idx < 992 + 18:
            where += f" (output master {idx - 991})"
        return (f"{ts:>10} ctl 0x{req:02x} {direction} "
                f"wValue=0x{val:04x} {where}")
    if req == 0x1A:
        if idx < 4:
            return (f"{ts:>10} ctl 0x1a {direction} input gain ch{idx + 1} "
                    f"wValue=0x{val:04x} ({val & 0xffff})")
        wire = val & 0x3F
        return (f"{ts:>10} ctl 0x1a {direction} output ch{idx - 3} "
                f"wValue=0x{val:04x} wire=0x{wire:02x} "
                f"({_db_from_output_wire(wire)})")
    names = {0x10: "settings", 0x15: "loopback", 0x17: "levels",
             0x14: "unmute", 0x13: "mute", 0x16: "chan-init",
             0x1B: "dds", 0x20: "enable"}
    extra = names.get(req, "vendor")
    return (f"{ts:>10} ctl 0x{req:02x} {direction} {extra} "
            f"wValue=0x{val:04x} wIndex=0x{idx:04x}")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("capture")
    ap.add_argument("--only", choices=("ctl", "state", "bulk", "summary"),
                    default=None)
    args = ap.parse_args()

    cmd = ["tshark", "-r", args.capture, "-T", "fields"]
    for f in FIELDS:
        cmd += ["-e", f]
    try:
        out = subprocess.run(cmd, check=True, capture_output=True, text=True).stdout
    except FileNotFoundError:
        print("tshark not found; install wireshark-cli/tshark", file=sys.stderr)
        return 1
    except subprocess.CalledProcessError as exc:
        print(f"tshark failed: {exc.stderr.strip()}", file=sys.stderr)
        return 1

    ctl_1a: Dict[int, List[int]] = collections.defaultdict(list)
    ctl_12: Dict[int, List[int]] = collections.defaultdict(list)
    state: Dict[int, List[int]] = collections.defaultdict(list)
    bulk_regs: Dict[int, List[Tuple[str, int]]] = collections.defaultdict(list)
    bulk_out: List[str] = []
    state_lines: List[str] = []
    ctl_lines: List[str] = []

    for line in out.splitlines():
        cols = line.split("\t")
        if len(cols) < len(FIELDS):
            cols += [""] * (len(FIELDS) - len(cols))
        (frame, ts, src, dst, bm_s, req_s, val_s, idx_s,
         ep_s, cap_s, frag_s) = cols[:len(FIELDS)]
        bm = _int(bm_s)
        req = _int(req_s)
        val = _int(val_s)
        idx = _int(idx_s)
        ep = _int(ep_s)
        data = _hexbytes(cap_s) or _hexbytes(frag_s)

        if bm is not None and req is not None and val is not None and idx is not None:
            # USB control transfer.  We only care about host -> device
            # vendor requests (0x40); descriptor traffic is noise here.
            if not (bm & 0x80) and (bm & 0x60) == 0x40:
                ctl_lines.append(_decode_control(ts, src, dst, bm, req, val, idx))
                if req == 0x1A:
                    ctl_1a[idx].append(val & 0xFFFF)
                elif req == 0x12:
                    ctl_12[idx].append(val & 0xFFFF)
            continue

        if ep is None:
            continue
        ep_num = ep & 0x7F
        if ep_num == 0x0C and data:
            desc = []
            for _n, reg, value in _words(data):
                if reg == 0x3F00:
                    continue  # heartbeat, not a setting
                desc.append(f"reg 0x{reg:04x} = {value}")
                bulk_regs[reg].append((ts, value))
            if desc:
                bulk_out.append(f"{ts:>10} bulk-out 0x0c: " + "; ".join(desc))
        elif ep_num == 0x0D and data:  # 0x8d without direction bit
            for _n, reg, value in _words(data):
                state[reg].append(value)
                state_lines.append(f"{ts:>10} state     reg 0x{reg:04x} = {value}")

    def show(section: str) -> None:
        if section in (None, "ctl") and ctl_lines:
            print("== host control writes ==")
            for line in ctl_lines:
                if "wValue=0x" in line:
                    print(line)
            print()

        if section in (None, "bulk") and bulk_out:
            print("== bulk OUT 0x0c register writes ==")
            for line in bulk_out:
                print(line)
            print()

        if section in (None, "state") and state_lines:
            print("== bulk IN 0x8d state deltas ==")
            for line in state_lines:
                print(line)
            print()

        if section in (None, "summary") and (ctl_1a or ctl_12 or state or bulk_regs):
            print("== summary ==")
            if bulk_regs:
                print("0x0c host register writes:")
                for reg in sorted(bulk_regs):
                    values = bulk_regs[reg]
                    label = _bulk_reg_label(reg)
                    last = values[-1][1]
                    extra = ""
                    if "hardware output" in label or "input gain" in label:
                        extra = f" = {last / 10:+.1f} dB"
                    elif "fader position" in label:
                        extra = f" = {last / 160:+.1f} dB"
                    elif "linear" in label and last > 0:
                        extra = f" = {20 * math.log10(last / 23250):+.1f} dB"
                    print(f"  reg 0x{reg:04x} ({label}): last {last}{extra}, "
                          f"wrote {len(values)} values, {values[0][0]}..{values[-1][0]} s")
            if ctl_1a:
                print("0x1a input gain / output hardware volume:")
                for idx in sorted(ctl_1a):
                    values = ctl_1a[idx]
                    if idx < 4:
                        print(f"  input ch{idx + 1}: {values[-1]} "
                              f"(wrote {len(values)} values)")
                    else:
                        wire = values[-1] & 0x3F
                        print(f"  output ch{idx - 3}: wire 0x{wire:02x} "
                              f"({_db_from_output_wire(wire)}), "
                              f"wrote {len(values)} values")
            if ctl_12:
                print("0x12 mixer / master (non-zero only):")
                for idx in sorted(ctl_12):
                    values = ctl_12[idx]
                    if 0 <= idx < 36 * 18:
                        # Most of a TotalMix init is a zeroing pass.  Those
                        # crosspoints are uninteresting in a sweep; print the
                        # ones that carry a level.
                        if values[-1] == 0 and max(values) == 0:
                            continue
                        out, source = divmod(idx, 36)
                        where = (f"out {out + 1}, "
                                 f"{'input' if source < 18 else 'playback'} "
                                 f"{source + 1 if source < 18 else source - 17}")
                    elif 992 <= idx < 992 + 18:
                        where = f"output master {idx - 991}"
                    else:
                        where = "raw"
                    print(f"  wIndex {idx} ({where}): last 0x{values[-1]:04x}, "
                          f"wrote {len(values)} values")
            if state:
                print("0x8d state registers:")
                for reg in sorted(state):
                    values = state[reg]
                    print(f"  reg 0x{reg:04x}: last {values[-1]:+d}, "
                          f"min {min(values):+d}, max {max(values):+d}, "
                          f"changes {len(values)}")
            print()

    show(args.only)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
