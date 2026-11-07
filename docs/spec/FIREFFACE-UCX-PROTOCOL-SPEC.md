# Fireface UCX control specification

This document defines the USB transport, register map, channel model and
device capabilities of the Fireface UCX backend.

Units and ranges are the values used by the backend. A register value is
16-bit unless stated otherwise.

## Contents

- [1. Device model](#1-device-model)
- [2. USB transport](#2-usb-transport)
- [3. IF1 register words](#3-if1-register-words)
- [4. Register model](#4-register-model)
- [5. Channel topology](#5-channel-topology)
- [6. Input controls](#6-input-controls)
- [7. Output controls](#7-output-controls)
- [8. Equalizer and low cut](#8-equalizer-and-low-cut)
- [9. Dynamics and AutoLevel](#9-dynamics-and-autolevel)
- [10. FX bus](#10-fx-bus)
- [11. Clock, rate and options](#11-clock-rate-and-options)
- [12. Streaming](#12-streaming)
- [13. Metering](#13-metering)
- [14. State and scenes](#14-state-and-scenes)
- [15. Capabilities](#15-capabilities)
- [16. Code map](#16-code-map)
- [Appendix A — Requested state dump](#appendix-a--requested-state-dump)

## 1. Device model

| Property | Value |
|---|---|
| Logical inputs | 18 |
| Logical outputs | 18 |
| Software playback channels | 18 |
| 1x rates | 32, 44.1, 48 kHz |
| 2x rates | 64, 88.2, 96 kHz |
| 4x rates | 128, 176.4, 192 kHz |
| ALSA PCM channels | 18 at every rate |
| Default transport | isochronous interface 0 |
| Alternate transport | interrupt interface 5 |
| Front panel | one rotary encoder with push: selects an input or output and sets its gain or volume |

The serial string is the stable unit identity. The ALSA card index changes
across reconnects and personality changes.

### 1.1 Front panel

The front-panel control is one rotary encoder with push.  A green LED shows
CHANNEL or LEVEL mode.  Pushing toggles between them; pushing repeatedly jumps
to `PH`.

CHANNEL mode cycles through:

| Display | Channel |
|---|---|
| `i1`–`i4` | inputs 1–4 |
| `L1`, `L3`, `L5` | line output pairs 1/2, 3/4, 5/6 |
| `PH` | phones / line output 7/8 |
| `SP` | SPDIF output |
| `A1`, `A3`, `A5`, `A7` | ADAT output pairs 1/2 … 7/8 |
| `SU` | load setup |

LEVEL mode sets:

| Channel | Range | Step |
|---|---|---|
| `i1`, `i2` | 0–65 dB | 1 dB |
| `i3`, `i4` | 0–12 dB | 0.5 dB |
| `L1`, `L3`, `L5`, `PH`, `SP`, `A1`, `A3`, `A5`, `A7` | +6 to −64 dB; below 0 dB a dot after the second digit; the next step is mute | 1 dB |

Input 1/2 gain is analog; input 3/4 gain is digital; output level is digital
in the mixer.

The display has two digits, each with a dot to its right.  The left dot marks
pair-adjust mode: the encoder is moving both channels of the pair.  The right
dot is a value marker, not a link marker:

| Context | Right dot |
|---|---|
| `i1`, `i2` gain | never shown; the step is whole dB |
| `i3`, `i4` gain | marks a half-dB step, e.g. `11.` is 11.5 dB in the GUI; always present for a half-dB value, absent for a whole-dB value |
| output level | shown below 0 dB and never at 0…+6 dB; marks a negative value |

The device cannot link or unlink channels.  Its fixed view is visible in the
channel cycle: inputs `i1`–`i4` are individual, and outputs appear as pairs
`L1`/`L3`/`L5` and `A1`/`A3`/`A5`/`A7`.  The link state is a host setting.

A long press on an input enters pair-adjust mode: the left dot appears and
the knob moves both channels of the selected pair, even when the device or
the GUI treats the pair as unlinked.  The display returns to the normal value
after about one second.

A long press on an output enters panorama mode:

| Display | Meaning |
|---|---|
| `CE` | centre |
| `L.1` / `r.1` | full left / full right (the guide names them `LL` / `rr`) |
| `L9.` … `L1`, `CE`, `R1` … `R9.` | intermediate balance positions |

The balance range is −100 … +100 in steps of 5; the displayed value is the
balance value divided by 10.  The display returns to the output fader after
about one second without a change.

`SU` loads one of six user setups, `FA` the factory preset, `Ad` the 8-channel
AD/DA converter mode, and `CC` Class Compliant mode when the unit is not
connected to a computer.  `i.o.` means loaded; `n.o.` means an empty slot.
Loading a setup while online changes only the front-panel parameters (input
gains and output volumes), not routing.

Front-panel gain, output-volume and output-balance changes are reported to the
host as `NN07`, `NN00` and `NN01`.  GUI link/stereo changes use the
stereo-mode state words.

## 2. USB transport

| Interface | Purpose | Endpoints |
|---|---|---|
| IF0 | ISO audio | OUT `0x0e`, IN `0x8f` |
| IF1 | Register and state transport | OUT `0x0c`, IN `0x8d` |
| IF1 | Meter pages | IN `0x85` |
| IF2 | USB-MIDI streaming | OUT `0x07`, IN `0x86` |
| IF4 | Stream status | IN `0x8b` |
| IF5 | Interrupt audio | OUT `0x01`, IN `0x02` |

| Traffic | Endpoint or request | When |
|---|---|---|
| Live parameters | bulk OUT `0x0c`, 4-byte words; bursts of 8 or 252 bytes | always |
| State echo | bulk IN `0x8d`, 4-byte words | always |
| Meters | bulk IN `0x85`, 244 bytes | always |
| MIDI | bulk IN `0x86` / OUT `0x07`, 4-byte USB-MIDI packets | while a MIDI port is open |
| Stream status | interrupt IN `0x8b` | while a stream is open |
| Audio | ISO `0x01` / `0x82` | while streaming |
| Vendor write | EP0 `0x40` | connect, stream open/close, rate apply, dialog apply |
| Vendor read | EP0 `0xC0` (`0x1c`, `0x1e`, `0x1f`, `0x11`) | continuous poll |

### 2.1 EP0 contexts

EP0 `0x40` handles:

- connect initialisation;
- audio stream open and close;
- sample-rate family apply;
- settings-dialog apply;
- loopback (`0x15`).

Live faders, mutes, pans, preamps, EQ, dynamics and FX use bulk `0x0c`.

### 2.2 EP0 read cycle

The device is polled with a 0..15 window sequence:

```text
WRITE bulk 0x0c: 0x3f00 = n
READ  EP0 0xc0:  0x1c, wLength 8
READ  EP0 0xc0:  0x1e, wLength 8
READ  EP0 0xc0:  0x1f, wLength 8
READ  EP0 0xc0:  0x11, wLength 8
```

Each read returns 4 bytes.  The `0x1c` payload is a 32-bit firmware
revision word: bits 31..16 are the firmware revision.  The lower 16 bits are
not interpreted.

### 2.3 Alternate settings

The unit accepts one SET_INTERFACE at connect: interface 5, alternate
setting 1. It remains selected for streaming at 1x rates and is not reissued
for stream open/close or a 44.1↔48 kHz change.

### 2.4 Write pacing

IF1 words are coalesced into bursts of up to 63 words (252 bytes). The
backend waits 5 ms after a burst of up to 8 words and 16 ms after a larger
burst. A full scene is sent as 63-word bursts; closer spacing overflows the
device receive FIFO and the bulk transfer times out.

Live width, pan and volume changes use the same queue: one control event is
sent as one or more bursts rather than one transfer per register. A live
change made while a scene is being applied joins the scene batch.

### 2.5 MIDI

The unit exposes a class-compliant USB-MIDI 1.0 streaming interface on IF2 with
bulk IN `0x86` and bulk OUT `0x07`. Events are 4-byte USB-MIDI packets: cable
number in the high nibble of byte 0, code index number in the low nibble, then
one to three MIDI bytes. The wired MIDI input and output use cable 0.

The driver claims IF2 and exposes one ALSA rawmidi port. Input URBs are
submitted only while the input substream is open; output data is packed into
the same event packets and written to the bulk OUT endpoint.

## 3. IF1 register words

A word is 32-bit little-endian:

```text
bit 31      bits 30..16      bits 15..0
parity      address:15       value:16
```

The parity bit is odd parity over the address and value fields:

```c
w = (address & 0x7fff) << 16 | value;
p = w;
p ^= p >> 16; p ^= p >> 8; p ^= p >> 4;
p ^= p >> 2;  p ^= p >> 1;
if (!(p & 1))
        w |= 0x80000000;
```

Device-to-host words on `0x8d` use the same layout.

### 3.1 Heartbeat

The backend writes `0x3f00 = 0..15` every 100 ms. The heartbeat makes the unit
stream live state changes.

### 3.2 Decoded state words

| Address | Meaning |
|---|---|
| `0x3f00` | heartbeat counter |
| `0x3f01` | constant `0xffff` |
| `0x3f02` | rolling status value |
| `0x3f03` | constant `0` |
| `0x3f10` | constant `0x7070` |
| `0x3f50` | constant `0` |
| `0x3d03` | DIM |
| `0x3f9e` | monitor flags; bit 0 = Talkback active, bit 1 = Mono Main Out, bit 5 = External Input active |
| `0x0002`, `0x0102` | input stereo mode |
| `0x0202`, `0x0302` | input stereo mode, second pair |
| `0x1204`, `0x1304` | output stereo mode |
| `0x0007`, `0x0107`, `0x0207`, `0x0307` | input gain 1–4, 0.1 dB |
| `0x1200..0x2300`, offset 0 | hardware-output volume, signed 0.1 dB |
| `0x1200..0x2300`, offset 1 | hardware-output balance, signed |
| `0x12a0`, `0x22a0` | selected-submix fader record |

Input gains are reported when the physical encoder moves. The word is not
emitted at connect, so it is not a startup readback: TotalControl writes the
saved scene gain at connect and follows `NN07` afterwards. Output-volume and
balance state is mapped to the Phones-first output order. The encoder reports
the `NN00` or `NN01` of the output selected on the unit's display; `PH` emits
`0x1800`/`0x1801` and `0x1900`/`0x1901`. A balance word updates both sides of
a linked output pair.

Front-panel gain and output-volume changes are reported as `NN07` and
`NN00`. 48 V, Inst and Level are host-written; the unit stores them but emits
no state word.

While a session is running the state stream is sparse and carries only changed
values.  The full state dump is requested on demand: after the state endpoint
is active, write `0x3f99 = 0x0812`.  The unit then emits a one-shot dump
covering the 18 input blocks, the 18 output blocks, the output matrix-send
block and the global FX/monitor words.  The input blocks include 48 V, Inst,
Level and AutoSet; the output blocks include references and balance.  The
detailed field map is maintained in `docs/work/DEVICE-SETUPS.md`.

The saved scene is still the host default at connect; the requested dump makes
a real comparison possible.

### 3.3 State readback

| State | Source |
|---|---|
| Input gain 1–4 | saved scene at connect; `0xNN07` after an encoder move |
| Output volume | `0xNN00` state word; saved scene at connect |
| Output balance | `0xNN01` state word |
| DIM | `0x3d03` state word |
| Talkback | `0x3f9e` bit 0 |
| Mono Main Out | `0x3f9e` bit 1 |
| Stereo mode | `0xNN02` / `0xNN04` state word |
| Fader records | `0x12a0` family |
| 48 V, Inst, Level, reference level | saved scene, written at connect |
| Options word | saved scene, written at connect |
| EQ, dynamics, FX | saved scene, written at connect |

## 4. Register model

DSP channels:

```text
hardware inputs   0x00..0x11
hardware outputs  0x12..0x23
```

A channel register is `(channel << 8) | offset`.

### 4.1 Input channel block

| Offset | Meaning | Encoding |
|---|---|---|
| `0x00` | mute | 0/1 |
| `0x01` | FX send | 0.1 dB, signed |
| `0x02` | stereo mode | 0 mono, 1 stereo |
| `0x03` | record enable | 0/1 |
| `0x05` | MS processing | 0/1 |
| `0x06` | phase | 0/1 |
| `0x07` | preamp gain | 0.1 dB |
| `0x08` | 48 V on inputs 1/2; Level on inputs 3/4 | 0/1; 0 −10 dBV, 1 +4 dBu, 2 LoGain |
| `0x09` | Inst on inputs 3/4 | 0/1 |
| `0x0a` | AutoSet on inputs 1/2 | 0/1 |
| `0x20` | low cut enable | 0/1 |
| `0x21` | low cut frequency | Hz |
| `0x22` | low cut slope | 6/12/18/24 dB/oct |
| `0x40` | EQ enable | 0/1 |
| `0x41` | EQ band 1 type | 0 Bell, 1 Shelf |
| `0x42` | EQ band 1 gain | 0.1 dB, signed |
| `0x43` | EQ band 1 frequency | Hz |
| `0x44` | EQ band 1 Q | Q × 10 |
| `0x45` | EQ band 2 gain | 0.1 dB, signed |
| `0x46` | EQ band 2 frequency | Hz |
| `0x47` | EQ band 2 Q | Q × 10 |
| `0x48` | EQ band 3 type | 0 Bell, 1 Shelf, 2 Low Pass |
| `0x49` | EQ band 3 gain | 0.1 dB, signed |
| `0x4a` | EQ band 3 frequency | Hz |
| `0x4b` | EQ band 3 Q | Q × 10 |
| `0x60` | dynamics enable | 0/1 |
| `0x61` | CompEx gain | 0.1 dB, signed |
| `0x62` | CompEx attack | ms + 1 |
| `0x63` | CompEx release | ms + 1 |
| `0x64` | compressor threshold | 0.1 dB, signed |
| `0x65` | compressor ratio | × 10 |
| `0x66` | expander threshold | 0.1 dB, signed |
| `0x67` | expander ratio | × 10 |
| `0x80` | AutoLevel enable | 0/1 |
| `0x81` | AutoLevel max gain | 0.1 dB |
| `0x82` | AutoLevel headroom | 0.1 dB |
| `0x83` | AutoLevel rise time | 0.1 s |

### 4.2 Output channel block

Output registers are the input layout mirrored at `+0x1200`:

| Offset | Meaning | Encoding |
|---|---|---|
| `0x00` | volume | 0.1 dB, signed |
| `0x01` | balance | −100..+100 |
| `0x02` | mute | 0/1 |
| `0x03` | FX Return | 0.1 dB, signed, −65.0..0.0 dB |
| `0x04` | stereo mode | 0 mono, 1 stereo |
| `0x05` | MS processing | 0/1 |
| `0x07` | phase L, and R at `+0x100` | 0/1 |
| `0x40..0x4b` | EQ and low cut | as inputs |
| `0x60..0x83` | dynamics and AutoLevel | as inputs |

### 4.3 Matrix and fader records

| Address | Meaning |
|---|---|
| `0x4000 + dsp_output*0x40 + source` | matrix coefficient |
| `source = 0..17` | hardware input |
| `source = 0x20..0x31` | software playback channel |
| `(0x12 + dsp_output) << 8 | (0xe0 + input)` | input fader/pan record |

The matrix coefficient is 15-bit signed with a scale flag. Bit 15 selects the
coarse scale (`value / 4096`); otherwise the scale is `value / 32768`.

The fader record carries two writes on the same address:

```text
level = tenths_db << 4        (-650 .. +60)
pan   = (pan << 4) | 0x8      (-100 .. +100)
```

The fader base is submix-dependent: the selected hardware output chooses the
`0x12e0` / `0x22e0` family.

### 4.4 Global registers

| Address | Meaning | Encoding |
|---|---|---|
| `0x3c00` | Reverb enable | 0/1 |
| `0x3c01` | Reverb type | algorithm index |
| `0x3c02` | Reverb pre-delay | ms |
| `0x3c03` | Reverb low cut | Hz |
| `0x3c04` | Reverb room scale | 0.01 |
| `0x3c05` | Reverb attack | ms |
| `0x3c06` | Reverb hold | ms |
| `0x3c08` | Reverb high cut | Hz |
| `0x3c09` | Reverb time | 0..100 |
| `0x3c0a` | Reverb high damp | Hz |
| `0x3c0b` | Reverb smoothness | 0..100 |
| `0x3c0c` | Reverb volume | 0.1 dB, signed |
| `0x3c0d` | Reverb width | 0..100 |
| `0x3c20` | Echo enable | 0/1 |
| `0x3c21` | Echo type | 0 Stereo Echo, 1 Stereo Cross, 2 Pong Echo |
| `0x3c22` | Echo delay | 0.01 s |
| `0x3c23` | Echo feedback | % |
| `0x3c24` | Echo high cut | 0 Off, 1 16k, 2 12k, 3 8k, 4 4k, 5 2k |
| `0x3c25` | Echo volume | 0.1 dB, signed |
| `0x3c26` | Echo width | 0..100 |
| `NN02` | output mode word | bit 0 mute, bit 1 monitor routing |
| `0x3d00` | Speaker B active | 0/1 |
| `0x3d01` | Mute Enable flag | 0/1; writable, audio effect not measured |
| `0x3d02` | Main Dim amount | 0.1 dB, signed |
| `0x3d03` | Dim active | 0/1 |
| `0x3d04` | Main Recall level | 0.1 dB, signed |
| `0x3d20` | control-room Main pair | read-only; 0 = AN1/2 in the tested state |
| `0x3d21` | control-room Speaker B pair | read-only; 2 = AN3/4 in the normal state |
| `0x3d22` | unknown | 0 in the tested states |
| `0x3d40` | host-session/runtime flag | read-only; 0 in a Linux-only session, 1 after a TotalMix session |
| `0x3d41`, `0x3d42` | unknown | read-only, 0 in the tested states |
| `0x3d43` | unknown boolean | writable 0/1 |
| `0x3d44` | unknown index/sentinel | writable; −1, 0 and 2 accepted |
| `0x3d45`, `0x3d46` | unknown | writable words |
| `0x3f00` | heartbeat / window | sequence |
| `0x3f98` | monitor mask; `0x83ff` with Mono, `0xfc40` with Cue, `0xffff` idle | — |
| `0x3f9e` | monitor flags; bit 0 Talkback, bit 1 Mono Main Out, bit 5 External Input | — |

The requested dump emits only `0x3d00`–`0x3d04`, `0x3d20`–`0x3d22` and
`0x3d40`–`0x3d46`. `0x3d05`–`0x3d1f` and `0x3d23`–`0x3d3f` are not present.
The unknown words are preserved in the raw dump and are not written by the
backend.

Echo BPM is a host-side field: changing it rewrites `0x3c22`.

Mono Main Out writes `0x3f9e` bit 1 and `0x3f98 = 0x83ff`; switching it off
writes `0x3f9e` bit 1 clear and `0x3f98 = 0xffff`. It mixes the assigned
Main Out and Speaker B pairs; the Phones pairs keep their stereo signal.

Dim attenuates the assigned Main and Speaker B output fader values and sets
`0x3d03 = 1`. The Phones output fader values are not changed. Releasing Dim
restores the saved fader values and writes `0x3d03 = 0`.

Assigning Speaker B to an output pair writes `NN02 = 2` on that pair and
`0x3d00 = 0` while Speaker B is off. Clearing the assignment writes
`NN02 = 0` on the old pair and `0x3d00 = 0`.

Switching Speaker B on writes `NN02` bit 1 on the Main pair, clears it on the
Speaker B pair, and writes `0x3d00 = 1`. Switching it off reverses the two
mode words and writes `0x3d00 = 0`. Bit 0 is the output mute and is preserved
in both writes.

Mute FX is host-driven. It saves the FX-return levels (`NN03`) of both sides
of the assigned Main Out and Speaker B pairs, writes `0xFD76` (−65 dB) to
them, and restores the saved levels when released. Phones is not part of the
monitor-speaker pair and is not modified.

External Input replaces the Main Out and Speaker B submixes with a hardware
input pair. For each target pair it zeroes every input and playback
crosspoint, routes the selected input pair's left and right sources at the
configured gain, writes `0xFD76` to both FX returns, and sets `0x3f9e` bit 5.
Releasing restores the submix from the channel model, restores the saved
FX-return levels and clears bit 5. The source is a stereo hardware-input
pair; playback sources are not eligible.

PFL is a host-driven alternative to Solo for input channels. The selected
Cue/PFL output pair is rewritten to contain only the active PFL inputs at
unity; every other input and playback crosspoint in that pair is zeroed and
both FX returns are set to `0xFD76`. Releasing the last PFL input restores the
pair from the channel model and restores its FX returns. PFL has no device
flag; the monitor mask and other monitor flags are not changed.

### 4.5 Setup store

The unit has six setup slots. The host stores its current internal mixer
state into slot `N` (`1..6`) with one global word:

```text
0x3f97 = 0x0910 + (N - 1)
```

The host does not upload a scene. The unit snapshots the state it currently
holds, including routing, faders, EQ, FX, references and options. Slots 1 and
2 are measured; the remaining slots use the same counter.

## 5. Channel topology

### 5.1 Inputs

```text
AN1, AN2, AN3, AN4, AN5, AN6, AN7, AN8,
SPDIF L, SPDIF R,
ADAT 1..8
```

Inputs 1–4 are mono preamps. Inputs 5/6, 7/8, SPDIF and the ADAT pairs are
stereo pairs.

### 5.2 Outputs

The DSP pair order is Phones-first. The display row is TotalMix order.

| Internal pair | Display row | Name | DSP pair |
|---|---|---|---|
| 0 | 4 | Phones 7/8 | 6 |
| 1 | 1 | AN1/2 | 0 |
| 2 | 2 | AN3/4 | 2 |
| 3 | 3 | AN5/6 | 4 |
| 4 | 5 | SPDIF | 8 |
| 5 | 6 | AS 1/2 | 10 |
| 6 | 7 | ADAT 3/4 | 12 |
| 7 | 8 | ADAT 5/6 | 14 |
| 8 | 9 | ADAT 7/8 | 16 |

### 5.3 Software playback

The playback row is:

```text
AN1/2, AN3/4, AN5/6, Phones 7/8, SPDIF,
AS 1/2, ADAT 3/4, ADAT 5/6, ADAT 7/8
```

Playback channels are numbered 0..17 in that row.

## 6. Input controls

### 6.1 Preamp

| Control | Inputs | Register |
|---|---|---|
| Gain | AN1–AN4 | `NN07`, 0.1 dB |
| 48 V | AN1/2 | `NN08`, 0/1 |
| Level | AN3/4 | `NN08`, 0/1/2 |
| Inst | AN3/4 | `NN09`, 0/1 |
| AutoSet | AN1/2 | `NN0A`, 0/1 |

Level values:

| Value | Label |
|---|---|
| 0 | −10 dBV |
| 1 | +4 dBu |
| 2 | LoGain |

Gain is reported back when the physical encoder moves: the IF1 state stream
carries `NN07` in 0.1 dB. AN1/2 step in 1 dB; AN3/4 step in 0.5 dB, stored as
raw half-dB units 0..24. 48 V, Level and Inst have no observed readback.

### 6.2 Signal processing

| Control | Register | Encoding |
|---|---|---|
| Phase | `NN06` | 0/1 |
| MS processing | `NN05` | 0/1 |
| FX send | `NN01` | 0.1 dB, signed |

The unit has no register for these; the host computes them and rewrites
the affected sends:

- **Width** — the pair's 2x2 send matrix, rewritten only while the pair
  has a live send; with every send at -infinity the sweep writes nothing.
  The equal-power law is `theta = (1 - w) * pi / 4`, `a = G * cos(theta)`,
  `b = G * sin(theta)`, where `G` is the send gain of each source. One
  routing writes `(dsp L, src L) = a`, `(dsp L, src R) = b`,
  `(dsp R, src L) = b`, `(dsp R, src R) = a`. Each send occupies one
  matrix cell; no second copy is written. The `0x44xx` anchors are the
  same matrix family with the ADAT 7/8 submix (DSP outputs 16/17) selected.
- **MS Proc on software playback** — the playback's send words
  (`0x4020…`, `0x4120…`, `0x41A0…` families).  On hardware inputs MS Proc
  writes `NN05` on both channels and the same send recompute.  With source
  gains `gl`/`gr` and the width angle `theta`, the coefficients are
  `gl*(cos+sin)/2`, `gr*(sin+cos)/2`, `gl*(cos-sin)/2` and
  `gr*(sin-cos)/2` for L→L, R→L, L→R and R→R.  Mono inputs reject MS Proc.
- **Talkback receive per output** and **No Trim** — host-side flags; they
  write nothing.  With a talkback source selected, enabling talkback on an
  output injects that source as ordinary send words into the output and dims
  the other active sends by the Dim setting (measured -20 dB on a -20 dB
  setting).  It also sets the FX-return level of each affected monitored
  output to `0xFD76` (`-65.0 dB`); the Reverb and Echo processors remain
  enabled.  The activation flag is a read-modify-write of `0x3f9e` bit 0,
  preserving bit 1 (Mono Main Out).  Releasing Talkback restores the prior
  send and FX-return words.
- **Trim** — a per-channel attenuation applied to every send (see
  `TOTALCONTROL-GUI-SPEC.md`).

**FX send** is an absolute level in 0.1 dB (`0x0000` = 0.0 dB, `NN01`).
A fader drag writes `0x0000` to the send and restores the knob value when
the drag ends; the send value itself is the knob value and does not track
the fader.  The post-fader coupling is not expressed in `NN01`.

### 6.2.1 Talkback activation

Talkback is a host-driven rewrite of the monitored sends. The source route
uses a four-word block per side. For a hardware input at zero-based index
`i`, the Talkback block is:

For a receiving output pair whose DSP output is `D`, and hardware input
index `i`:

| Field | Address |
|---|---|
| source level | `((0x12 + D) << 8) \| (0xE0 + i)` |
| source pan | the same address, second word |
| source send | `0x4000 + D * 0x40 + i` |
| right-side level | `((0x13 + D) << 8) \| (0xE0 + i)` |
| right-side pan | the same address, second word |
| right-side send | `0x4000 + (D + 1) * 0x40 + i` |

The measured Phones 1 receiver had DSP output 2, so its 18 inputs occupied
`0x14e0..0x14f1`, `0x15e0..0x15f1`, `0x4080..0x4091` and
`0x40c0..0x40d1`. The addresses move with the receiving output; they are not
fixed to Phones 1. Level and pan are written as two consecutive words at the
same address; pan is `(pan << 4) | 0x8`, with `0x0008` at centre, `0xF9C8`
hard left and `0x0648` hard right.

The source send is a matrix coefficient. At source fader `0 dB` and pan
centre it is `0x8B33`; `-6 dB` is `0x2CE8`, `-20 dB` is `0x08F5`, and
`-21 dB` is `0x07FC`. The coefficient is the matrix encoding of the source
fader gain multiplied by the equal-power pan gain. At `-65 dB` the source
block is omitted. The source level word remained `0xD760` (`-65.0 dB`) in
the measured Phones 1 routing; the route gain is carried by the companion
matrix word.

The source block is followed by the attenuated active sends and the FX
returns. A normal ON/OFF burst is 60 bytes (15 words). The periodic monitor
refresh described below can add a leading `0x3f98` word to the same transfer,
making it 64 bytes. With no active sends, the burst is shorter. When
the Listenback source is the same input as the Talkback source, no source
block is written; the captured form is 36 bytes, containing only the
active sends, FX returns and `0x3f9e`.

The activation sequence also:

- attenuates active sends into the Talkback-receiving outputs by the
  Talkback Dim amount, and active sends into Main by the Listenback Dim
  amount;
- writes each affected output's FX-return level (`0x1203`, `0x1303`, …) to
  `0xFD76` (`-65.0 dB`), without disabling the Reverb or Echo processors;
- sets bit 0 of `0x3f9e` with a read-modify-write, preserving bit 1 (Mono
  Main Out).

A `0 dB` Dim writes no active-send word at all. At `-65 dB` the affected
send is `0x0000`. Listenback Dim is independent of the Listenback input:
with the input at `None`, Main sends still use the configured Listenback Dim.

Listenback with a different source adds a second source block at the Main
output's DSP addresses: `0x12e0 + i` / `0x13e0 + i` with matrix
`0x4000 + i` / `0x4040 + i` when Main is DSP 0. With the same source as
Talkback, only the first source block is present.

Deactivation restores the words captured before activation and clears bit 0.
A source change while Talkback is inactive emitted a 4-byte
`0x0001 = 0x0000` cleanup in the measured scene; a live source change also
rewrote the old and new source blocks. A `None` source emits no activation
burst.

The host rewrites the monitor mask (`0x3f98`) and the monitor flags
(`0x3f9e`) together every 1.5–1.9 s while it is connected. `0x3f98` is
`0x83ff` while Mono Main Out is active, `0xfc40` while a Cue output is
active, and `0xffff` otherwise. The refresh is independent of Talkback and
can share a USB transfer with an unrelated burst.

### 6.3 Mute, solo and stereo mode

| Control | Behaviour |
|---|---|
| Mute | `NN00 = 1`; active sends are zeroed and restored on unmute |
| Solo | host-side monitoring state; no solo register |
| Stereo mode | `NN02`, 0 mono / 1 stereo |

Solo writes no device word of its own. With active sends, solo and unsolo
rewrite the affected send cells; with no active send, the toggle produces no
host-to-device traffic.

### 6.4 Mode buttons

Stereo, 48 V, Inst, AutoSet and Phase are per-channel states. Link state is
`NN02`. The linked-pair parameter rules are defined in
[`TOTALCONTROL-OSC-SPEC.md`](TOTALCONTROL-OSC-SPEC.md) §11.

## 7. Output controls

### 7.1 Volume, balance and mute

| Control | Register | Encoding |
|---|---|---|
| Volume | `NN00` | 0.1 dB, signed; −650 = −∞ |
| Balance | `NN01` | −100..+100 |
| Mute | `NN02` | 0/1 |
| Stereo mode | `NN04` | 0 mono / 1 stereo |

The front-panel VOLUME encoder writes the `NN00` registers of the output
selected on the unit's display (`L.1`–`L.6`, `PH`, `SP`, `A.1`–`A.8`). The
selection is independent of the Control Room Main and Phones assignments;
`PH` always addresses channels 7/8.

Balance writes the same value to both channels of a linked pair. It is
independent of the pair volume.

Balance addresses:

```text
AN1/2       0x1201 / 0x1301
AN3/4       0x1401 / 0x1501
AN5/6       0x1601 / 0x1701
Phones 7/8  0x1801 / 0x1901
SPDIF       0x1a01 / 0x1b01
ADAT 1/2    0x1c01 / 0x1d01
ADAT 3/4    0x1e01 / 0x1f01
ADAT 5/6    0x2001 / 0x2101
ADAT 7/8    0x2201 / 0x2301
```

### 7.2 FX Return

`NN03` sets the effect return for one output channel: 0.1 dB, signed,
−65.0..0.0 dB. A linked pair writes both channels; writing either side
updates both registers in the state dump. Values above 0.0 dB are ignored by
the unit.

### 7.3 Loopback

Loopback uses EP0 request `0x15`:

```text
value = 1 on, 0 off
index = DSP output channel 0..17
```

A pair writes both indices. The whole 30-entry bank is available.

### 7.4 Cue

Cue is a host-side monitoring flag on an output.

### 7.5 Phase and No Trim

Each hardware output side has its own phase register: left at `base + 0x07`,
right at `base + 0x107`, with `base = 0x12 + 2 x pair`. The value is `1` for
inverted, `0` for normal.

| Pair | Ø L | Ø R |
|---|---|---|
| AN 1/2 | `0x1207` | `0x1307` |
| AN 3/4 | `0x1407` | `0x1507` |
| AN 5/6 | `0x1607` | `0x1707` |
| Phones 7/8 / Main | `0x1807` | `0x1907` |
| SPDIF | `0x1a07` | `0x1b07` |
| ADAT 1/2 … 7/8 | `0x1c07` … `0x2307` | `+0x100` |

No Trim is a host-side flag per output channel: sends into a No-Trim
destination use a trim factor of 1, so Trim Gain changes leave that
destination untouched.

## 8. Equalizer and low cut

EQ and low cut are available on every hardware input and every hardware
output.

### 8.1 EQ

| Parameter | Range | Register |
|---|---|---|
| Enable | on/off | `NN40` |
| Band 1 type | Bell, Shelf | `NN41` |
| Band 1 gain | ±20 dB | `NN42` |
| Band 1 frequency | 20 Hz..20 kHz | `NN43` |
| Band 1 Q | 0.7..5.0 | `NN44` |
| Band 2 gain | ±20 dB | `NN45` |
| Band 2 frequency | 20 Hz..20 kHz | `NN46` |
| Band 2 Q | 0.7..5.0 | `NN47` |
| Band 3 type | Bell, Shelf, Low Pass | `NN48` |
| Band 3 gain | ±20 dB | `NN49` |
| Band 3 frequency | 20 Hz..20 kHz | `NN4a` |
| Band 3 Q | 0.7..5.0 | `NN4b` |

The writer maps frequency directly in Hz and Q as Q × 10.

### 8.2 Low cut

| Parameter | Range | Register |
|---|---|---|
| Enable | on/off | `NN20` |
| Frequency | 20..500 Hz | `NN21` |
| Slope | 6/12/18/24 dB/oct | `NN22` |

The slope writer maps 6/12/18/24 to 0/1/2/3.

A linked input pair exposes one EQ/low-cut block to the host: writing either
side updates both `NN20`–`NN22` and `NN40`–`NN4b` registers, and the device
keeps the last value written. The left channel is the reference; the GUI and
the scene carry the same values on both sides.

## 9. Dynamics and AutoLevel

Dynamics are available on every hardware input and every hardware output.

| Parameter | Range | Register | Encoding |
|---|---|---|---|
| Dynamics enable | on/off | `NN60` | 0/1 |
| CompEx gain | −30..+30 dB | `NN61` | 0.1 dB, signed |
| CompEx attack | 0..200 ms | `NN62` | ms + 1 |
| CompEx release | 100..999 ms | `NN63` | ms + 1 |
| Compressor threshold | −60..0 dB | `NN64` | 0.1 dB, signed |
| Compressor ratio | 1..10 | `NN65` | × 10 |
| Expander threshold | −99..−30 dB | `NN66` | 0.1 dB, signed |
| Expander ratio | 1..10 | `NN67` | × 10 |
| AutoLevel enable | on/off | `NN80` | 0/1 |
| AutoLevel max gain | 0..18 dB | `NN81` | 0.1 dB |
| AutoLevel headroom | 3..12 dB | `NN82` | 0.1 dB |
| AutoLevel rise time | 0.1..9.9 s | `NN83` | 0.1 s |

Gain, attack and release are shared between compressor and expander.

## 10. FX bus

The Reverb and Echo registers are listed in §4.4.

- Reverb: enable, type, pre-delay, low cut, room scale, high cut, smoothness,
  volume, width.
- Echo: enable, type, delay, feedback, high cut, volume, width.
- Each output has an FX Return level (`NN03`).

The output meter pages carry two extra power channels for the FX bus and two
extra peak words at meter offsets 232..239. The FX values follow the FX send
level.

The backend applies the whole Reverb block and the whole Echo block in one
IF1 burst each.

## 11. Clock, rate and options

### 11.1 Rate family

| Change | EP0 write |
|---|---|
| 44.1 kHz | `0x10`, index 48, value `0x0010` |
| 48 kHz | `0x10`, index 48, value `0x0020` |

### 11.2 Clock source

The clock index is written with request `0x10`:

```text
value = index << 1
mask  = 0x0006
```

| Index | Source |
|---|---|
| 0 | Internal |
| 1 | SPDIF Coaxial |
| 2 | Optical |
| 3 | Word Clock |

The unit has no LTC source; the clock field is two bits.

### 11.3 Connect initialisation

1. `SET_CONFIGURATION`
2. `SET_INTERFACE(5, 1)`
3. 60 × `0x16` (indices 0..29 and 32..61)
4. 4 × `0x1b` clock quad
5. `0x10/0x05ff = 0x0028`
6. `0x17/0x0fc0 = 0x3c00`
7. the `0x10/0x3000` and `0x10/0x0800` group
8. 30 × `0x15` loopback bank
9. `0x3f99 = 0x0812`
10. 1087-word bulk scene

The `0x1b` quad is:

```text
(wValue 0xc350, wIndex 0x0000)
(0x8db9, 0x2d01)
(0x8235, 0x5502)
(0x7d00, 0x0003)
```

### 11.4 Application reconnect

When TotalMix FX opens with the USB driver already attached, it does not
reissue `SET_CONFIGURATION`, `SET_INTERFACE` or the `0x16`/`0x1b` vendor
initialisation. It writes:

1. 30 × `0x15` loopback bank, indices 0..29, value 0;
2. a 1106-word bulk scene in 18 bursts: 17 × 252 bytes and 140 bytes;
3. the normal heartbeat.

The scene begins with the fader/matrix records and ends with the global FX and
monitor words.

### 11.5 Options word

Request `0x10`, index 1487, carries the unit's options as one full word:

| Bit | Mask | Meaning |
|---|---|---|
| 3 | `0x0008` | word-clock input termination |
| 6 | `0x0040` | word-clock output single speed |
| 7 | `0x0080` | SPDIF professional |
| 8 | `0x0100` | EQ+D for Record disabled (active low) |
| 10 | `0x0400` | optical output ADAT (0 = SPDIF) |

Every option change writes the whole word, including the current clock source
in bits 1–2. Bit 10 is not an index of its own. No options-word readback is
exposed: request `0x10` in returns a status word. The saved scene value is
written at connect and after a change.

## 12. Streaming

### 12.1 ISO interface 0

| Rate class | Alt | Words/frame | Packet frames | IN max packet | OUT max packet |
|---|---|---|---|---|---|
| 1x | 1 | 18 | 8 | 508 | 532 |
| 2x | 2 | 14 | 16 | 732 | 780 |
| 4x | 3 | 12 | 32 | 1208 | 1304 |

Input packet:

- 4-byte header: bits 0..4 frame count, bits 5..17 position, bits 18..31 phase;
- block/planar 32-bit samples;
- position wraps at 3072;
- up to 8 ISO packets per URB.

Output packet:

- interleaved `lead_word + nch float32` frames;
- every lead word is written;
- the ring wraps at 3072.

### 12.2 Interrupt interface 5

- Alt 1: 48 kHz.
- Alt 2: 18 words at 1x, 14 at 2x.
- Alt 3: two endpoints, 6 words each.
- 32-bit integer samples, no ISO header.
- URB size 1024 frames.

### 12.3 Stream setup

```text
channel init 0x16 (18 inputs and 18 playback channels)
settings     0x10 value 0x0800 index 0xb800
enable       0x20 value 0x0001 index 0x0000
```

Start:

```text
settings 0x10  0x8000 / 0x8000
0x1d           0x0000 / 0x0000
unmute mask    request 0x14
```

Stop:

```text
mute mask      request 0x13
```

The playback mask covers channels 2..31. Channel bit `n` is mask bit `n+2`.

## 13. Metering

The meter endpoint is IF1 IN `0x85`. A packet is 244 bytes.

| Bytes | Contents |
|---|---|
| 0..159 | 20 little-endian `u64` mean-square values: 18 channel values, then two FX bus values |
| 160..231 | 18 little-endian `u32` channel peak values |
| 232..239 | two little-endian `u32` FX bus peak values |
| 240..243 | tag |

The two FX values are zero on input and playback pages. On output pages they
follow the FX send level and are meter data, not protocol status. An FX peak
at full scale (`2^27`) is the overload indication; there is no separate
overload flag register.

| Tag | Bank |
|---|---|
| `0x11111111` | input A |
| `0x55555555` | input B |
| `0x22222222` | software playback |
| `0x33333333` | output pre-fader |
| `0x66666666` | output post-fader |

Scales:

```text
power dBFS = 10 * log10(power / 2^55)
peak  dBFS = 20 * log10(peak  / 2^27)
power <= 31 = below range
peak == 0   = below range
```

Input meters use the larger of input A and input B. Playback meters use the
playback page. Hardware-output meters use the post-fader page, reordered from
DSP order to the Phones-first output order. TotalControl's meters use the peak
word; the power word is mean square and reads several dB below the peak for
music.

## 14. State and scenes

The host scene is authoritative.

A scene stores:

- matrix cells and pan values;
- input, playback and output faders and mutes;
- input and output EQ, low cut, dynamics and AutoLevel;
- output balance and FX Return;
- loopback flags;
- Reverb and Echo settings;
- channel names and trim values.

The backend writes the mixer scene at connect and after relevant rate or
stream changes.  The preamp group (input gain and right gain, 48 V, Inst,
Level and AutoSet), the reference levels and the options word (clock source,
optical format, SPDIF professional, word-clock rate and termination, EQ+D for
Record) are also written from the saved scene at connect.

The unit retains input gain and the main output fader across a power cycle.
The user guide also states that the unit permanently stores its configuration
and complete mixer state and reloads them at power-on.  That stored state is a
standalone feature: loading a setup while online changes only the front-panel
parameters (gains and output volumes), not routing.

The host can request the full mixer snapshot with `0x3f99 = 0x0812` while the
state endpoint is active.  The dump contains the input gain, 48 V, Inst, Level,
AutoSet, references, balance, channel DSP and matrix-send records that the
sparse running stream does not report.  TotalControl requests and retains that
dump at open.

The options word, clock source and sample rate are not part of the dump; they
remain host-written from the saved scene.  Channel names and trim are host-only.
The saved scene is still the default at connect, and the startup policy decides
which fields the host writes and which it adopts from the dump.

Front-panel output volume, DIM, MS and gain changes remain live `0x8d` words.
DIM, Talkback, Mono Main Out and stereo mode stay device-owned where they are
reported.  EQ, dynamics, FX, matrix and fader records are in the requested dump
and are also written by the host at connect.

Scenes are keyed by unit serial. Each unit keeps its own scene file.

## 15. Capabilities

| Feature | UCX |
|---|---|
| Matrix pair routing | yes |
| Per-cell mono matrix | no |
| Input/output faders, mute, solo | yes |
| Input phase | yes |
| Loopback | yes, pair-level |
| Input/output stereo split | yes |
| EQ, low cut, dynamics, AutoLevel | yes |
| 48 V | inputs 1/2 |
| Inst | inputs 3/4 |
| Input Level | inputs 3/4 |
| AutoSet | inputs 1/2 |
| PAD | no |
| Output balance | yes |
| FX Return | yes |
| Reverb and Echo | yes |
| Output reference Level | no |
| Pitch | no |
| Width | host-side |
| MS processing | register only |
| AN 1>2 | no |
| Input Link | host setting |
| EQ for Record | yes |
| Optical output format | yes |
| SPDIF professional | yes |
| Word clock output single speed | yes |
| Word clock input termination | yes |
| Clock sources | Internal, SPDIF coax., Optical, Word |

## 16. Code map

| Concern | Source |
|---|---|
| IF1 word encoding and parity | `kernel/ffuc.c` |
| hwdep ABI and events | `kernel/ffuc-hwdep.c`, `kernel/ffuc-uapi.h` |
| Stream start/stop and rate | `kernel/ffuc.c` |
| IF1 constants and matrix encoding | `ui/totalcontrol/totalcontrol-core/src/fireface/protocol.rs` |
| Device write paths, meters and scenes | `ui/totalcontrol/totalcontrol-core/src/fireface/device.rs` |
| Channel DSP registers | `ui/totalcontrol/totalcontrol-core/src/fireface/settings.rs` |
| Channel topology | `ui/totalcontrol/totalcontrol-core/src/profiles/ucx.rs` |
| Capability guards | `ui/totalcontrol/totalcontrol-core/src/fireface/models.rs` |

## Appendix A — Requested state dump

After the state endpoint is active, write `0x3f99 = 0x0812` to request a
one-shot state dump.  The UCX returns about 1,636 unique words on `0x8d`,
covering the hardware inputs, hardware outputs, their matrix-send records and
the global FX/monitor words.  The dump is not a live stream; it is a connect
snapshot.

The table is the observed layout.  “Unknown” means the address is present but
its meaning has not been confirmed.

### Input channels

Input `n` (`0..17`) uses base `n << 8`.

| Offset | Meaning | Encoding |
|---|---|---|
| `0x0000` | mute | 0/1 |
| `0x0001` | FX send | signed 0.1 dB; `0xfd76` = −65.0 dB, `0xff9c` = −10.0 dB |
| `0x0002` | stereo | 0 mono / 1 stereo |
| `0x0003` | record enable | 0/1 |
| `0x0004` | unknown | 0 observed; not the remote state |
| `0x0005` | MS processing | 0/1 |
| `0x0006` | phase, left | 0/1 |
| `0x0007` | preamp gain | 0.1 dB; `0x012c` = 30.0 dB |
| `0x0008` | 48 V (inputs 1/2) / Level (inputs 3/4) | 0/1; Level 0 −10 dBV, 1 +4 dBu, 2 LoGain |
| `0x0009` | Inst | 0/1 |
| `0x000a` | AutoSet | 0/1 |
| `0x0020` | low cut enable | 0/1 |
| `0x0021` | low cut frequency | Hz |
| `0x0022` | low cut slope | code |
| `0x0040` | EQ enable | 0/1 |
| `0x0041` | band 1 type | code |
| `0x0042` | band 1 gain | signed 0.1 dB; `0x003c` = +6.0 dB |
| `0x0043` | band 1 frequency | Hz |
| `0x0044` | band 1 Q | code |
| `0x0045` | band 2 gain | signed 0.1 dB |
| `0x0046` | band 2 frequency | Hz |
| `0x0047` | band 2 Q | code |
| `0x0048` | band 3 type | code |
| `0x0049` | band 3 gain | signed 0.1 dB |
| `0x004a` | band 3 frequency | Hz |
| `0x004b` | band 3 Q | code |
| `0x0060` | dynamics enable | 0/1 |
| `0x0061` | dynamics gain | signed 0.1 dB |
| `0x0062` | dynamics attack | ms |
| `0x0063` | dynamics release | ms |
| `0x0064` | compressor threshold | signed 0.1 dB |
| `0x0065` | compressor ratio | code |
| `0x0066` | expander threshold | signed 0.1 dB |
| `0x0067` | expander ratio | code |
| `0x0080` | AutoLevel enable | 0/1 |
| `0x0081` | AutoLevel max gain | 0.1 dB |
| `0x0082` | AutoLevel headroom | 0.1 dB |
| `0x0083` | AutoLevel rise time | 0.1 s |

Input mute also clears the input's matrix sends in the dump.  Solo has no
single flag in the dump; it is implemented by zeroing the other sends.

### Output channels

The hardware-output blocks are:

| Block | Output pair |
|---|---|
| `0x12`/`0x13` | AN 1/2 |
| `0x14`/`0x15` | AN 3/4 |
| `0x16`/`0x17` | AN 5/6 |
| `0x18`/`0x19` | Phones 7/8 |
| `0x1a`/`0x1b` | SPDIF |
| `0x1c`/`0x1d` | ADAT 1/2 |
| `0x1e`/`0x1f` | ADAT 3/4 |
| `0x20`/`0x21` | ADAT 5/6 |
| `0x22`/`0x23` | ADAT 7/8 |

Each channel uses the same offset block:

| Offset | Meaning | Encoding |
|---|---|---|
| `0x0000` | volume | signed 0.1 dB; `0xff86` = −12.2 dB |
| `0x0001` | balance | signed −100..+100 |
| `0x0002` | mute | 0/1 |
| `0x0003` | FX return | signed 0.1 dB, −65.0..0.0 dB; writing either side of a linked pair updates both |
| `0x0004` | stereo | 0/1 |
| `0x0005`, `0x0006` | unknown | 0 observed |
| `0x0007` | phase, left | 0/1 |
| `0x0008` | reference/Level code | code |
| `0x0020` | low cut enable | 0/1 |
| `0x0021` | low cut frequency | Hz |
| `0x0022` | low cut slope | code |
| `0x0040` | EQ enable | 0/1 |
| `0x0041`..`0x004b` | EQ bands | as inputs |
| `0x0060`..`0x0067` | dynamics | as inputs |
| `0x0080`..`0x0083` | AutoLevel | as inputs |

### Matrix sends

Each output block carries 18 input-send records.  A record is two words:
level and pan.

| Input index | Record address |
|---|---|
| 0–7 | `0x?a0 + 2n` |
| 8–9 | `0x?c0 + 2(n−8)` |
| 10–17 | `0x?e0 + 2(n−10)` |

Examples: input 0 to output 0 at `0x18a0`/`0x18a1`; input 2 to output 0 at
`0x18a4`/`0x18a5`.  The level word is signed tenths dB shifted left by 4
(`0xfc40` = −6.0 dB); the companion word is the signed pan value.

Software-playback sends were not observed to change any dump word; their
state is not in this snapshot.

### Globals

| Address | Meaning |
|---|---|
| `0x3c00` | Reverb enable |
| `0x3c01`..`0x3c0d` | Reverb parameters |
| `0x3c20` | Echo enable |
| `0x3c21`..`0x3c26` | Echo parameters |
| `0x3d00` | Speaker B active |
| `0x3d01` | Mute Enable flag |
| `0x3d02` | Main Dim amount |
| `0x3d03` | Dim active |
| `0x3d04` | Main Recall level |
| `0x3d20` | control-room Main pair |
| `0x3d21` | control-room Speaker B pair |
| `0x3d22` | unknown |
| `0x3d40` | host-session/runtime flag |
| `0x3d41`–`0x3d42` | unknown/read-only |
| `0x3d43`–`0x3d46` | unknown/writable |

The dump does not contain `0x3d05`–`0x3d1f` or `0x3d23`–`0x3d3f`.
`0x3f00`..`0x3f50` are heartbeat/status words, not state.
