# Fireface UCX control specification

This document defines the USB transport, register map, channel model and
device capabilities of the Fireface UCX backend.

Units and ranges are the values used by the backend. A register value is
16-bit unless stated otherwise.

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

The serial string is the stable unit identity. The ALSA card index changes
across reconnects and personality changes.

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

Each read returns 4 bytes. The payloads are available on Linux.

### 2.3 Alternate settings

The unit accepts one SET_INTERFACE at connect: interface 5, alternate
setting 1. It remains selected for streaming at 1x rates and is not reissued
for stream open/close or a 44.1↔48 kHz change.

### 2.4 Write pacing

The backend waits 5 ms after each IF1 write or burst. A full scene
(18 input and 18 output DSP blocks) exceeds the device receive FIFO at 1 ms.

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
| `0x3d03` | DIM |
| `0x3f9e` | monitor flags; bit 1 = MS processing |
| `0x0007`, `0x0107`, `0x0207`, `0x0307` | input gain 1–4, 0.1 dB |
| `0x1200..0x2300`, offset 0 | hardware-output volume, signed 0.1 dB |

Input gains are reported when the physical encoder moves. Output-volume state
is mapped to the Phones-first output order. The physical Phones encoder emits
`0x1800` and `0x1900`.

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
| `0x03` | FX Return | 0.1 dB, signed |
| `0x04` | stereo mode | 0 mono, 1 stereo |
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
| `0x3c08` | Reverb high cut | Hz |
| `0x3c0b` | Reverb smoothness | 0..100 |
| `0x3c0c` | Reverb volume | 0.1 dB, signed |
| `0x3c0d` | Reverb width | 0..100 |
| `0x3c20` | Echo enable | 0/1 |
| `0x3c21` | Echo type | 0 Stereo Echo, 1 Stereo Cross, 2 Pong Echo |
| `0x3c22` | Echo delay | 0.01 s |
| `0x3c23` | Echo feedback | % |
| `0x3c24` | Echo high cut | index |
| `0x3c25` | Echo volume | 0.1 dB, signed |
| `0x3c26` | Echo width | 0..100 |
| `0x1802` / `0x1902` | Phones/Main mode word | bit 1 = Speaker B |
| `0x3d00` | control-room state | bit 3 = Speaker B |
| `0x3d03` | DIM / Recall | 0/1 |
| `0x3f00` | heartbeat / window | sequence |
| `0x3f98` | monitor mask | — |
| `0x3f9e` | monitor flags | — |

Echo BPM is a host-side field: changing it rewrites `0x3c22`.

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
  `(dsp R, src L) = b`, `(dsp R, src R) = a`; the playback sources also
  write their `+0x180` copy. The `0x44xx` anchors are the same matrix
  family with the ADAT 7/8 submix (DSP outputs 16/17) selected.
- **MS Proc on software playback** — the playback's send words
  (`0x4020…`, `0x4120…`, `0x41A0…` families).  On hardware inputs MS Proc
  uses `NN05` and the same send recompute.
- **Talkback receive per output** and **No Trim** — host-side flags; they
  write nothing.  With a talkback source selected, enabling talkback on an
  output injects that source as ordinary send words into the output, dims
  the other sends by the Dim setting (measured -20 dB on a -20 dB setting),
  and mutes Main's FX return (`0x1803`/`0x1903` = `0xFD76`).
- **Trim** — a per-channel attenuation applied to every send (see
  `TUXMIX-SPEC.md`).

**FX send** is an absolute level in 0.1 dB (`0x0000` = 0.0 dB, `NN01`).
A fader drag writes `0x0000` to the send and restores the knob value when
the drag ends; the send value itself is the knob value and does not track
the fader.  The post-fader coupling is not expressed in `NN01`.

### 6.3 Mute, solo and stereo mode

| Control | Behaviour |
|---|---|
| Mute | `NN00 = 1`; active sends are zeroed and restored on unmute |
| Solo | host-side monitoring state |
| Stereo mode | `NN02`, 0 mono / 1 stereo |

### 6.4 Mode buttons

Stereo, 48 V, Inst, AutoSet and Phase are per-channel states. A linked pair
copies the left channel's preamp state to the right and centres both pans.
Splitting writes hard-left and hard-right pans.

## 7. Output controls

### 7.1 Volume, balance and mute

| Control | Register | Encoding |
|---|---|---|
| Volume | `NN00` | 0.1 dB, signed; −650 = −∞ |
| Balance | `NN01` | −100..+100 |
| Mute | `NN02` | 0/1 |
| Stereo mode | `NN04` | 0 mono / 1 stereo |

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

`NN03` sets the effect return for one output channel: 0.1 dB, signed. A
linked pair writes both channels.

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

### 11.4 Options word

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
exposed: request `0x10` in returns a status word. The device-owned values are
kept in the scene and written when they change.

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
| 0..159 | 20 little-endian `u64` mean-square values; first 18 used |
| 160..231 | 18 little-endian `u32` peak values |
| 232..239 | reserved |
| 240..243 | tag |

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
DSP order to the Phones-first output order.

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
stream changes.  The options word (clock source, optical format, SPDIF
professional, word-clock rate and termination, EQ+D for Record), the reference
levels and the 48 V/Inst/Level switches are held by the unit: TuxMix does not
write them at connect and has no readback for them.  The saved scene keeps the
last value written from TuxMix for display.

Input gain, front-panel output volume, DIM and MS state are read from the IF1
state stream.  A read-back gain is not overwritten by the saved scene.

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
| IF1 constants and matrix encoding | `ui/tuxmix/tuxmix-core/src/fireface/protocol.rs` |
| Device write paths, meters and scenes | `ui/tuxmix/tuxmix-core/src/fireface/device.rs` |
| Channel DSP registers | `ui/tuxmix/tuxmix-core/src/fireface/settings.rs` |
| Channel topology | `ui/tuxmix/tuxmix-core/src/profiles/ucx.rs` |
| Capability guards | `ui/tuxmix/tuxmix-core/src/fireface/models.rs` |
