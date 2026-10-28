#!/usr/bin/env python3
"""Tiny ALSA control client for the OLD v1 Fireface UCX driver.

NOTE: since the 2026-10-25 transport-core refactor the kernel module creates
no ALSA mixer controls.  This tool only works against an old module; the
current path is the hwdep API (docs/HWDEP-ABI.md) and TuxMix.

Uses libasound.so.2 through ctypes, so alsa-lib-devel is not required.

usage:
  rme_mixer.py [-c CARD] list
  rme_mixer.py [-c CARD] get NAME [INDEX]
  rme_mixer.py [-c CARD] set NAME CHANNEL VALUE [INDEX]
  rme_mixer.py [-c CARD] demo

CARD defaults to hw:FirefaceUCX.  Names containing spaces must be quoted.
Values are raw DSP gains: 8192 = 0 dB, 0 = mute.
"""
import argparse
import ctypes
import errno
import os
import sys

IFACE_MIXER = 2
TYPE_BOOLEAN = 1
TYPE_INTEGER = 2
TYPE_ENUMERATED = 3

lib = ctypes.CDLL("libasound.so.2")

# ctl
lib.snd_ctl_open.argtypes = [ctypes.POINTER(ctypes.c_void_p),
                             ctypes.c_char_p, ctypes.c_int]
lib.snd_ctl_open.restype = ctypes.c_int
lib.snd_ctl_close.argtypes = [ctypes.c_void_p]
lib.snd_ctl_close.restype = ctypes.c_int

# elem id
lib.snd_ctl_elem_id_malloc.argtypes = [ctypes.POINTER(ctypes.c_void_p)]
lib.snd_ctl_elem_id_malloc.restype = ctypes.c_int
lib.snd_ctl_elem_id_free.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_id_set_interface.argtypes = [ctypes.c_void_p, ctypes.c_int]
lib.snd_ctl_elem_id_set_name.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
lib.snd_ctl_elem_id_set_index.argtypes = [ctypes.c_void_p, ctypes.c_uint]
lib.snd_ctl_elem_id_get_name.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_id_get_name.restype = ctypes.c_char_p
lib.snd_ctl_elem_id_get_index.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_id_get_index.restype = ctypes.c_uint

# elem info
lib.snd_ctl_elem_info_malloc.argtypes = [ctypes.POINTER(ctypes.c_void_p)]
lib.snd_ctl_elem_info_malloc.restype = ctypes.c_int
lib.snd_ctl_elem_info_free.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_info_set_id.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
lib.snd_ctl_elem_info.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
lib.snd_ctl_elem_info.restype = ctypes.c_int
lib.snd_ctl_elem_info_get_type.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_info_get_type.restype = ctypes.c_int
lib.snd_ctl_elem_info_get_count.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_info_get_count.restype = ctypes.c_uint
lib.snd_ctl_elem_info_get_min.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_info_get_min.restype = ctypes.c_long
lib.snd_ctl_elem_info_get_max.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_info_get_max.restype = ctypes.c_long
lib.snd_ctl_elem_info_get_step.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_info_get_step.restype = ctypes.c_long
lib.snd_ctl_elem_info_get_items.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_info_get_items.restype = ctypes.c_uint
lib.snd_ctl_elem_info_set_item.argtypes = [ctypes.c_void_p, ctypes.c_uint]
lib.snd_ctl_elem_info_get_item_name.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_info_get_item_name.restype = ctypes.c_char_p

# elem value
lib.snd_ctl_elem_value_malloc.argtypes = [ctypes.POINTER(ctypes.c_void_p)]
lib.snd_ctl_elem_value_malloc.restype = ctypes.c_int
lib.snd_ctl_elem_value_free.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_value_set_id.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
lib.snd_ctl_elem_read.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
lib.snd_ctl_elem_read.restype = ctypes.c_int
lib.snd_ctl_elem_write.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
lib.snd_ctl_elem_write.restype = ctypes.c_int
lib.snd_ctl_elem_value_get_integer.argtypes = [ctypes.c_void_p, ctypes.c_uint]
lib.snd_ctl_elem_value_get_integer.restype = ctypes.c_long
lib.snd_ctl_elem_value_set_integer.argtypes = [ctypes.c_void_p, ctypes.c_uint,
                                               ctypes.c_long]
lib.snd_ctl_elem_value_get_enumerated.argtypes = [ctypes.c_void_p,
                                                  ctypes.c_uint]
lib.snd_ctl_elem_value_get_enumerated.restype = ctypes.c_uint
lib.snd_ctl_elem_value_set_enumerated.argtypes = [ctypes.c_void_p,
                                                  ctypes.c_uint,
                                                  ctypes.c_uint]

# elem list
lib.snd_ctl_elem_list_malloc.argtypes = [ctypes.POINTER(ctypes.c_void_p)]
lib.snd_ctl_elem_list_malloc.restype = ctypes.c_int
lib.snd_ctl_elem_list_free.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_list.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
lib.snd_ctl_elem_list.restype = ctypes.c_int
lib.snd_ctl_elem_list_get_count.argtypes = [ctypes.c_void_p]
lib.snd_ctl_elem_list_get_count.restype = ctypes.c_uint
lib.snd_ctl_elem_list_alloc_space.argtypes = [ctypes.c_void_p, ctypes.c_uint]
lib.snd_ctl_elem_list_alloc_space.restype = ctypes.c_int
lib.snd_ctl_elem_list_get_id.argtypes = [ctypes.c_void_p, ctypes.c_uint,
                                         ctypes.c_void_p]
lib.snd_ctl_elem_list_get_id.restype = ctypes.c_int


def check(ret, what):
    if ret < 0:
        raise OSError(-ret, f"{what}: {os.strerror(-ret)}")
    return ret


def make_id(name, index=0):
    p = ctypes.c_void_p()
    check(lib.snd_ctl_elem_id_malloc(ctypes.byref(p)), "elem_id_malloc")
    lib.snd_ctl_elem_id_set_interface(p, IFACE_MIXER)
    lib.snd_ctl_elem_id_set_name(p, name.encode())
    lib.snd_ctl_elem_id_set_index(p, index)
    return p


def make_info(ctl, ident, what="elem_info"):
    p = ctypes.c_void_p()
    check(lib.snd_ctl_elem_info_malloc(ctypes.byref(p)), "elem_info_malloc")
    lib.snd_ctl_elem_info_set_id(p, ident)
    check(lib.snd_ctl_elem_info(ctl, p), f"elem_info {what}")
    return p


def make_value(ctl, ident):
    p = ctypes.c_void_p()
    check(lib.snd_ctl_elem_value_malloc(ctypes.byref(p)), "elem_value_malloc")
    lib.snd_ctl_elem_value_set_id(p, ident)
    check(lib.snd_ctl_elem_read(ctl, p), "elem_read")
    return p


def type_name(t):
    return {
        TYPE_INTEGER: "INTEGER",
        TYPE_BOOLEAN: "BOOLEAN",
        TYPE_ENUMERATED: "ENUMERATED",
    }.get(t, f"type {t}")


def open_card(card):
    ctl = ctypes.c_void_p()
    check(lib.snd_ctl_open(ctypes.byref(ctl), card.encode(), 0),
          f"snd_ctl_open({card})")
    return ctl


def list_controls(ctl):
    lst = ctypes.c_void_p()
    check(lib.snd_ctl_elem_list_malloc(ctypes.byref(lst)), "elem_list_malloc")
    lib.snd_ctl_elem_list(ctl, lst)
    count = lib.snd_ctl_elem_list_get_count(lst)
    check(lib.snd_ctl_elem_list_alloc_space(lst, count), "list_alloc_space")
    check(lib.snd_ctl_elem_list(ctl, lst), "elem_list")
    ident = ctypes.c_void_p()
    check(lib.snd_ctl_elem_id_malloc(ctypes.byref(ident)), "elem_id_malloc")
    for i in range(count):
        check(lib.snd_ctl_elem_list_get_id(lst, i, ident), "list_get_id")
        name = lib.snd_ctl_elem_id_get_name(ident).decode()
        index = lib.snd_ctl_elem_id_get_index(ident)
        info = make_info(ctl, ident, name)
        n = lib.snd_ctl_elem_info_get_count(info)
        t = lib.snd_ctl_elem_info_get_type(info)
        if t == TYPE_INTEGER:
            lo = lib.snd_ctl_elem_info_get_min(info)
            hi = lib.snd_ctl_elem_info_get_max(info)
            rng = f"range={lo}..{hi}"
        elif t == TYPE_BOOLEAN:
            rng = "range=0..1"
        elif t == TYPE_ENUMERATED:
            rng = f"items={lib.snd_ctl_elem_info_get_items(info)}"
        else:
            rng = ""
        print(f"{name}  index={index}  {type_name(t)}  channels={n}  {rng}")
        lib.snd_ctl_elem_info_free(info)
    lib.snd_ctl_elem_id_free(ident)
    lib.snd_ctl_elem_list_free(lst)


def get_control(ctl, name, index=0):
    ident = make_id(name, index)
    info = make_info(ctl, ident, name)
    n = lib.snd_ctl_elem_info_get_count(info)
    t = lib.snd_ctl_elem_info_get_type(info)
    value = make_value(ctl, ident)
    if t == TYPE_ENUMERATED:
        vals = [lib.snd_ctl_elem_value_get_enumerated(value, i)
                for i in range(n)]
    else:
        vals = [lib.snd_ctl_elem_value_get_integer(value, i)
                for i in range(n)]
    print(f"{name}: {type_name(t)} channels={n}")
    print("  " + " ".join(str(v) for v in vals))
    lib.snd_ctl_elem_value_free(value)
    lib.snd_ctl_elem_info_free(info)
    lib.snd_ctl_elem_id_free(ident)


def set_control(ctl, name, channel, value, index=0, zero_others=False):
    ident = make_id(name, index)
    info = make_info(ctl, ident, name)
    n = lib.snd_ctl_elem_info_get_count(info)
    t = lib.snd_ctl_elem_info_get_type(info)
    if t == TYPE_INTEGER:
        lo = lib.snd_ctl_elem_info_get_min(info)
        hi = lib.snd_ctl_elem_info_get_max(info)
    elif t == TYPE_BOOLEAN:
        lo, hi = 0, 1
    elif t == TYPE_ENUMERATED:
        lo, hi = 0, lib.snd_ctl_elem_info_get_items(info) - 1
    else:
        raise SystemExit(f"{name}: unsupported control type {t}")
    if not 0 <= channel < n:
        raise SystemExit(f"{name}: channel {channel} outside 0..{n - 1}")
    if not lo <= value <= hi:
        raise SystemExit(f"{name}: value {value} outside {lo}..{hi}")
    val = make_value(ctl, ident)
    if t == TYPE_ENUMERATED:
        if zero_others:
            for i in range(n):
                lib.snd_ctl_elem_value_set_enumerated(val, i, 0)
        lib.snd_ctl_elem_value_set_enumerated(val, channel, value)
    else:
        if zero_others:
            for i in range(n):
                lib.snd_ctl_elem_value_set_integer(val, i, 0)
        lib.snd_ctl_elem_value_set_integer(val, channel, value)
    check(lib.snd_ctl_elem_write(ctl, val), f"elem_write {name}")
    lib.snd_ctl_elem_value_free(val)
    lib.snd_ctl_elem_info_free(info)
    lib.snd_ctl_elem_id_free(ident)
    print(f"{name}[{channel}] = {value}")


def demo(ctl):
    # Output 1/2 are the phones in the confirmed Phones-first block order.
    for out in (1, 2):
        set_control(ctl, f"Output {out} Master Volume", 0, 8192)
        # Source 4 is hardware input 5.  Clear the row and route only input 5.
        set_control(ctl, f"Output {out} Mix Volume", 4, 8192,
                    zero_others=True)
    print("input 5 -> phones (outputs 1/2) at 0 dB")
    print("Feed a separate signal into input 5; do not use the "
          "phones->input 5 loopback cable (feedback).")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-c", "--card", default="hw:FirefaceUCX")
    ap.add_argument("cmd", choices=["list", "get", "set", "demo"])
    ap.add_argument("args", nargs="*")
    ns = ap.parse_args()
    ctl = open_card(ns.card)
    try:
        if ns.cmd == "list":
            list_controls(ctl)
        elif ns.cmd == "get":
            if not ns.args:
                raise SystemExit("get needs NAME [INDEX]")
            name = ns.args[0]
            index = int(ns.args[1]) if len(ns.args) > 1 else 0
            get_control(ctl, name, index)
        elif ns.cmd == "set":
            if len(ns.args) < 3:
                raise SystemExit("set needs NAME CHANNEL VALUE [INDEX]")
            name = ns.args[0]
            channel = int(ns.args[1])
            value = int(ns.args[2], 0)
            index = int(ns.args[3]) if len(ns.args) > 3 else 0
            set_control(ctl, name, channel, value, index)
        elif ns.cmd == "demo":
            demo(ctl)
    except OSError as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)
    finally:
        lib.snd_ctl_close(ctl)


if __name__ == "__main__":
    main()
