# `snd-fireface-uc` — Fireface UC/UCX transport module

The kernel module is a transport core:

- USB enumeration and a per-device descriptor table;
- alternate-setting and sample-rate selection;
- interrupt and isochronous URB engines;
- the ALSA PCM device;
- one `snd-hwdep` control node per card;
- the UC output-latch watchdog and engine recovery.

Mixer state, routing, preamps, EQ, meters and scenes live in TuxMix
userspace.  See [../docs/DRIVER-REFACTOR.md](../docs/DRIVER-REFACTOR.md),
[../docs/PROTOCOL-COMPLETE.md](../docs/PROTOCOL-COMPLETE.md) and
[../docs/HWDEP-ABI.md](../docs/HWDEP-ABI.md).

## Devices

| Model | USB ID | Transport | Interface |
|---|---|---|---|
| Fireface UC PC | `0424:3fc5` | interrupt IF0 | 0 |
| Fireface UC AP | `0424:3fc6` / `2a39:3fc6` | isochronous IF0 | 0 |
| Fireface UCX | `0424:3fc9` / `2a39:3fc9` | isochronous IF0 (default) | 0 |
| Fireface UCX | `0424:3fc9` / `2a39:3fc9` | interrupt IF5 (`ucx_if5=1`) | 5 |

One module instance handles several physical devices; each gets its own card,
PCM device and hwdep node.

## Build

The project directory is space-free, so the module builds in place:

```sh
./scripts/build-kernel.sh
```

The script uses the `kernel-devel-$(uname -r)` package matching the running
kernel.

The result is `kernel/snd-fireface-uc.ko`.

## Load

```sh
sudo modprobe snd-pcm
sudo insmod kernel/snd-fireface-uc.ko
dmesg | tail
aplay -l
ls -l /dev/snd/hwC*D*
```

Unload with `sudo rmmod snd_fireface_uc`, or use
`./scripts/reload-kernel.sh`.

Module parameters:

| Parameter | Default | Meaning |
|---|---|---|
| `index[]`, `id[]`, `enable[]` | ALSA defaults | card slot, id and enable |
| `urb_frames` | 48 | frames per URB at 1× on the UC interrupt path |
| `nurbs` | 2 | URBs in flight per endpoint; 96 frames queued at 1× |
| `ucx_if5` | 0 | UCX audio interface: 0 = isochronous IF0, 1 = interrupt IF5 |
| `ucx_if5_frames` | 1024 | frames per URB at 1× on the UCX interrupt path |
| `fixed_rate` | 0 | restrict the PCM rate |

The ALSA ring is at least `(nurbs + 2) × urb_frames`, rounded up to whole
periods: 192 frames at 1×, which is four periods at 48 samples, three at 64,
and two at 96 and above.  This covers the queued USB URBs and their margin.

## Userspace

```sh
cd ../ui/tuxmix
cargo run -p tuxmix-core --example fireface-scan --features alsa
cargo run -p tuxmix-gui
```

The hwdep ABI is versioned (`FFUC_ABI_VERSION`); the Rust binding checks it
at open.

## DKMS

```sh
./scripts/install-dkms.sh
```

This registers `kernel/` with DKMS, builds it for the running kernel, and
installs the boot loader unit.  Fedora's
`/usr/lib/kernel/install.d/40-dkms.install` hook then rebuilds it for each
new kernel.  Keep `kernel-devel` installed so the matching headers are
available for the update.

`scripts/install-kernel.sh` is a compatibility wrapper for this installer.

## Current scope

- UC interrupt and isochronous transports.
- UCX isochronous and interrupt transports.
- UC EP0 vendor requests, UCX IF1 register words, and raw meter/state/status
  events.
- UC output-latch detection and engine restart.
- Suspend, resume and hotplug.
