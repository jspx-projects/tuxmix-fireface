# Babyface Pro control specification

This document specifies the TuxMix support for the RME Babyface Pro FS: the
channel model, the ALSA control surface, the proprietary vendor USB protocol,
the DSP EQ block format, metering, capabilities and persistence.  UC and UCX
are specified separately in `UC-CONTROL-SPEC.md` and `UCX-CONTROL-SPEC.md`.

## 1. Identification and backends

| Mode | USB ID | Backend |
|---|---|---|
| Proprietary (RME vendor mode) | `2a39:3fc0` | ALSA via the `snd-usb-babyface-pro` driver, or direct USB via the `tuxmix-usb` crate |
| Class compliant | `2a39:3fb0` | ALSA only |

- The ALSA card is found by the card-name substring `Babyface Pro`.
- The control grammar is decided once at open from a sentinel element:
  `Mic 1` means proprietary, `Mic-AN1 Gain` means class compliant.
- Both backends report the model string `Babyface Pro FS`, so scenes are
  interchangeable between them.
- The two proprietary backends are mutually exclusive on one unit: the direct
  USB backend refuses to open while the kernel driver is bound.  Auto-detect
  tries ALSA first, then USB.

## 2. Channel model

The topology is fixed.  A profile defines it for the real backends and for
the mock alike.

**Inputs — 12, fixed**

| Index | Name | Type |
|---|---|---|
| 0, 1 | `AN1`, `AN2` | Mic |
| 2, 3 | `IN3`, `IN4` | Instrument |
| 4, 5 | `AS1`, `AS2` | Line (first optical pair) |
| 6–11 | `ADAT3`–`ADAT8` | ADAT |

**Outputs — 6 stereo pairs:** `AN1/2`, `PH3/4`, `AS1/2`, `ADAT3/4`,
`ADAT5/6`, `ADAT7/8` (individual channels `OUT AN1` … `OUT ADAT8`).

**Playbacks — 6 stereo pairs:** same pair order, labels `PCM AN1` …
`PCM ADAT7/8`; the playback channels of pair *n* use ids `2n` and `2n+1`.

Pairing is structural.  Only the mono sources `AN1`–`AN4` accept a pan
value; linked pairs reject pan.  Software playback follows the output order.

## 3. ALSA backend — proprietary grammar

All elements are looked up by name at an explicit `.index`; a missing
element is a silent capability gap, never an error.

### 3.1 Routing

- 84 crosspoint elements, one per source × output pair.  Source order:
  `AN1, AN2, AN3, AN4, AS1/2, ADAT3/4, ADAT5/6, ADAT7/8, PB1…PB6`.
  Element address = source name at `.index = output*14 + src`.
- Crosspoint fader range comes from the element's TLV scale, never from
  hard-coded values.  Full scale is `0x2d41` on the crosspoint and `0x4000`
  on the master; the top of the range is +6 dB.
- Volume writes round down, so a level never lands louder than asked.

### 3.2 Pan

There is no pan register on the hardware.  A mono source's route is two
independent crosspoints; pan is encoded as "louder side keeps the volume,
quieter side is attenuated" and decoded back without loss.

### 3.3 Mute and solo

- Strip mute zeroes the strip's crosspoints into every output and restores
  them from the model on unmute.  No solo awareness in the restore.
- Strip solo zeroes every *other* strip's crosspoints; the soloed strip and
  its stereo partner are untouched.  Solo is exclusive.
- Output mute is a real hardware switch shared by the whole pair; writing
  one channel's mute mutes its sibling.
- In class compliant mode the card has no mute switch; mute drives the
  output master to zero and restores on unmute.  This does not survive a
  re-open.  Output solo does not exist.

### 3.4 Output masters

One 2-channel element per pair, named `AN1/2, PH3/4, AS1/2, ADAT3/4,
ADAT5/6, ADAT7/8` at `.index = pair`.  In class compliant mode the master is
a per-physical-channel `Main-Out <dest>` element treated as a share of
range.

### 3.5 Preamps

| Control | Inputs | Element |
|---|---|---|
| Gain | 0–3 | `Mic 1` capture volume at `.index = input` |
| 48 V | mics only | `Phantom Power Mic 1` switch |
| PAD (−20 dB) | mics only | `Pad Mic 1` switch |
| Ref level | instruments | `Instrument Ref Level` enum: `+4dBu`, `-10dBV`, `Boost` |

Gain ranges come from the card (microphones 0–65, instruments 0–18); the GUI
shows inputs 3/4 in 0.5 dB steps.  48 V, PAD and gain writes mirror across
each linked pair.

### 3.6 DSP EQ (proprietary only)

One EQ per analog input (`AN1`–`AN4`): an enable switch, three bands with
`Type` (Off/Bell/LowShelf/HighShelf), `Freq` (Hz), `Q` (raw = Q×100) and
`Gain` (raw = dB×10, ±24 dB), plus `Low Cut Freq` and `Low Cut Slope`
(6/12/18/24 dB per octave).  There is no separate low-cut enable element; a
low-cut frequency control implies enabled.

### 3.7 Monitor and strip controls (proprietary only)

| Control | Element | Encoding |
|---|---|---|
| MS processor | `MS Processor` switch | bool |
| AN 1>2 | `AN 1>2` switch | bool |
| Input link | `AN1/2 Link` switch | bool |
| Dim | `Dim` switch | absolute −20 dB cut on phones |
| Width | `Width` volume | raw = width×100, ±1.0 |
| FX send | `FX Send` volume | −65…0 dB |
| Varispeed | `Varispeed Pitch` volume | raw = percent×10, ±5 % in 0.1 % steps |
| Loopback | `Loopback` switch, one per output pair | bool |
| Phase | `<ANn> Phase` switch, AN1–4 | bool |
| Trim | `<ANn> Trim` volume, AN1–4 | dB, −65…+6 |
| Stereo split | `PBn Stereo Split` switch, PB1–6 | bool, mirrored to the pair |
| Front panel | `Front Panel In/Out` enums, `Front Panel Mix` switch | read only |

The `DIM Button Press Count` counter is read as an odd-delta toggle; acting
on it is the application's job.

### 3.8 Clock and optical

- `Optical Out Format` enum selects ADAT or S/PDIF on the optical output.
- `Sample Clock Source` is read and written in class compliant mode; the
  proprietary card does not expose it yet.
- The backend sets no sample rate; the rate belongs to the kernel driver and
  the model holds the last known value.

### 3.9 Follow-the-hardware resync

On any ALSA event the backend re-reads the whole model, then once more after
150 ms, because a front-panel wheel move notifies without carrying every
value it changed.  A re-read tolerance of 1/8192 keeps rounding from nudging
a held fader.  Crosspoints of muted and soloed strips are left alone during
resync.

## 4. Direct USB backend — vendor protocol

Implemented in the `tuxmix-usb` crate against the raw USB device
(`2a39:3fc0`, vendor-specific class).

### 4.1 Endpoint map

| Interface | Endpoint | Type | Use |
|---|---|---|---|
| 0 (endpoint 0) | — | control | all mixer control |
| 1 | `0x0A` OUT | bulk, 512 B | 64-byte EQ/FX coefficient blocks |
| 5 | `0x01` OUT, `0x82` IN | interrupt | real audio and VU metering |

The isochronous interface 0 (`0x03`/`0x84`) is not used; streaming it does
not engage the device.

### 4.2 Sample rates and frame layout

| Alt | Rates (Hz) | Frame | Channels | URB |
|---|---|---|---|---|
| 1 | 32000–88200 | 56 B | 14 | 14336 B |
| 2 | 96000, 128000 | 40 B | 10 | 10240 B |
| 3 | 176400, 192000 | 32 B | 8 | 8192 B |

A rate change is only `SET_INTERFACE(5, alt)` plus teardown/restart of the
URBs at the new frame layout; no vendor writes.  Each frame holds per
channel one marker byte `0x00` and a little-endian S24_LE sample.  On the IN
stream, channels 0–3 are AN1–4, channels 4/5 carry a fixed `0x20` marker and
channels 6–13 are ADAT/SPDIF; the OUT stream is 14 playback channels.

### 4.3 Streaming

- The stream is userspace interrupt transfers: 8 URBs in flight per
  endpoint, continuously resubmitted.  The device completes URBs only while
  both endpoints have pending URBs.
- Playback pulls a whole URB from a playback ring before resubmit
  (zero-fill on underrun); capture pushes frame-aligned chunks into a
  capture ring (drop oldest on overrun).  Both rings hold 1000 ms, share one
  eventfd wake, and expose monotonic hardware positions.
- The analog front end engages only while a valid session streams; preamp
  writes alone do not engage it.
- Start sequence: claim interface 5 → set alternate setting → streaming
  init (register clears, pitch quad, init words, preamp state) → session
  start (`0x10 0x8000`, `0x1D`) → submit URBs → arm (`0x14 0xC000`).  A
  `0x13 0xC000` write during start disarms the session and must not be
  sent.
- Stop cancels the URBs and releases interface 5.

### 4.4 Control transfer format

All commands are vendor control transfers with no data phase.

| Field | Write | Read |
|---|---|---|
| bmRequestType | `0x40` | `0xC0` |
| bRequest | command code | command code |
| wValue | value | `0x0000` |
| wIndex | sub-address / channel, high 2 bits = transaction counter | `0x0000` |
| wLength | 0 | 4 |

The transaction counter in the two high bits of `wIndex` cycles
`0xC000 → 0x4000 → 0x8000 → 0x0000` on fader-class writes; `0x1A` writes
(gain, 8-bit companions, mute) do not use it.  Status reads return 4 bytes,
zero-padded if short.

### 4.5 Command families

| bRequest | Meaning |
|---|---|
| `0x12` | 16-bit fader volume: crosspoints, output masters.  `0x2000` = 0 dB, `0x1000` = −6 dB |
| `0x1A` | 8-bit register: packed gain, 8-bit master companions, mute state (`0x003B`) |
| `0x17` | preamp/link/ref-level state register, followed by a `0x21` commit |
| `0x1B` | clock/pitch DDS quad (banks 0–3) |
| `0x15` | loopback flag per channel |
| `0x16` | register clear (streaming init) |
| `0x10` | settings keepalive (`0x05CF`), init words, session arm/start |
| `0x13` / `0x14` | session stop / arm |
| `0x11`, `0x1C`, `0x1E`, `0x1F` | status read registers |

### 4.6 Addresses

Source indices (L / R; mono sources reuse the L index):

| Source | L | R |
|---|---|---|
| AN1–AN4 | 0–3 | same |
| AS1/2 | 4 | 5 |
| ADAT3/4, 5/6, 7/8 | 6, 8, 10 | 7, 9, 11 |
| Playback 1–6 | 12+2n | 13+2n |

Output blocks (crosspoint block order differs from the strip order):
`PH3/4 = 0`, `AN1/2 = 1`, `AS1/2 = 2`, `ADAT3/4 = 3`, `ADAT5/6 = 4`,
`ADAT7/8 = 5`.

| Register | Address |
|---|---|
| Crosspoint L / R, standard map | `0x0034 + 0x0034·block + src_l` / `0x004E + 0x0034·block + src_r` |
| Low map (AN1/2 submix mirror) L / R | `0x0000 + src_l` / `0x001A + src_r` |
| Output master, 16-bit L / R | `0x03E0 + 2·out` / `+1` |
| Output master, 8-bit L / R | `0x0004 + 2·out` / `+1` |
| Mic gain (`0x1A`) | `0x0000 + mic` (bits 0–4 coarse 3 dB steps, bits 5–7 remainder) |
| Panel gain (`0x1A`) | `0x000A + mic` (bits 0–4) |
| Preamp state (`0x17`) | `wIndex 0x003F`: bits 0–1 = 48V mic1/2, bits 2–3 = base `0x0C` / ref level, bits 4–5 = PAD mic1/2; writes are full state followed by a `0x21` commit |
| Input link (`0x17`) | `wIndex 0x1000`: `0x0000` split, `0x0400` linked, `+0x1000` AN 1>2 copy mode |

### 4.7 Composed writes

- **MS processor** writes four crosspoints (AN2 L/R into the low map and
  standard map of output block 0); engaged = all zero.
- **Phase** writes the bitwise NOT of the fader value to the source's low
  map and AN1/2 standard crosspoint; the R side is untouched.
- **FX send** writes `wIndex 0x0138` (L) / `0x0153` (R) with the same value;
  the curve is `raw = 0x1000·2^(dB/6)`, off = `0x0000`.
- **Stereo split** on a linked pair writes `0x1000`/`0x1000` (−6 dB per
  side) when linked, `0x2000`/`0x0000` when split, into the low map and the
  AN1/2 standard crosspoints of the playback source.
- **Trim** writes all eight registers of a linked AN1/2 strip: the low map
  with the master-curve trim value and the standard map with the combined
  value.  Combined law: `standard_raw = 0x16A0 · 10^((fader_dB + trim_dB)/20)`.
- **Width** writes the low-map mirror balance pairs on AN1/AN2:
  `L = 0x2000·(1+w)/2`, `R = 0x2000 − L`; neutral width is `0x1000` both
  sides.
- **Ref level** (instruments): `0x17` at `0x003F` plus `0x21`; +4 dBu =
  state `0x000F`, −10 dBV = `0x0003`, boost = `0x0003` with commit `0x0003`.
- **Loopback** is `0x15` with the channel index 0–29 and value 1/0.
- **Pitch/varispeed** writes the `0x1B` quad:
  `DDS = round(50000·256/(1 + pitch%/100))`, bank 1 = `0.72562·DDS`,
  bank 2 = `(2/3)·DDS`, bank 3 = `0x7CFF`, then a clock keepalive.
- **Settings word** `0x10` at `0x05CF`: bit 0 = internal clock, bit 2 =
  optical clock, bit 6 = EQ for record, bit 10 = optical S/PDIF output.  It
  doubles as the ~3 s keepalive and is always recomposed as a whole.

### 4.8 Status reads and metering

- `read_status(0x17)` returns the preamp state register in byte 0 and the
  clock state in byte 2 (`0x40` internal, `0x80` optical without lock).
- Input metering folds the peak |sample| of AN1–4 from the IN interrupt
  stream as a share of full scale (2²³) and is drained reset-on-read by the
  caller, which shapes the ballistics.

## 5. DSP EQ block format

Uploaded as 64-byte blocks on bulk endpoint `0x0A`, one block per channel
(0 = left, 1 = right):

| Offset | Content |
|---|---|
| `0x00` | channel byte |
| `0x01` | low-cut slope: `2^n − 1` (6 dB→`0x01` … 24 dB→`0x0F`), 0 = off |
| `0x02` | channel byte again |
| `0x03` | `0x80` = EQ engine active |
| `0x04`, `0x14`, `0x24` | three band slots: b0, b1, b2, a1 as i32 LE |
| `0x34` | shared fifth coefficient, `0x08000000` = passthrough |
| `0x38` | low-cut frequency word, `0x04000000` = off |
| `0x3C` | zero |

Band coefficients are stored as RBJ values scaled by 2^27 in the transfer
function `H(z) = c4·(1 + c2·z⁻¹ + c3·z⁻²)/(1 + c0·z⁻¹ + c1·z⁻²)`; the device
reconstructs b0 = c4, b1 = c2·c4, b2 = c3·c4, a1 = c0, a2 = c1.  A band with
0 dB gain is the identity `[0, 0, 0, 0, 0x08000000]`.  The low-cut frequency
word is `round(11508·f/(1 + f/11656))` with slope pole-compensation factors
1.5267 (6 dB), 1.0 (12 dB), 0.8061 (18 dB) and 0.6977 (24 dB).  A block
carries the whole channel state; writing a low cut alone zeroes the band
slots, so the caller re-passes the full state.

## 6. Metering

- ALSA backend: no hardware meter register; a capture PCM (12 channels,
  S32_LE, 48 kHz requested, 1024-frame periods) is opened on a reader
  thread for pre-fader peaks.  Capture channel *n* maps to input strip *n*
  (AN1/2, IN3/4, AS1/2, ADAT3/4, ADAT5/6, ADAT7/8).  An output pair's
  loopback feeds its record bus into the capture words of its input pair.
- USB backend: AN1–4 input peaks from the IN interrupt stream (§4.8);
  playback meters come from a PipeWire sink tap instead.
- Output meters are a separate PipeWire sink tap in both cases.

## 7. Capabilities

| Capability | Proprietary | Class compliant |
|---|---|---|
| Protocol features (EQ, loopback, phase, stereo split) | yes | no |
| Input phantom, input PAD | yes | yes |
| Pitch, width, MS processor, AN 1>2, input link, optical out | yes | yes |
| Input level (3-state), autoset, dynamics, per-channel FX send | no | no |
| Matrix cells (per-cell routing) | no | no |

Capabilities that have no implemented write path are hidden by the GUI
rather than shown as dead controls: per-cell mono matrix editing, output
balance, speaker B, main-out assign, CUE, mute-FX, reverb, echo and FX
return.

## 8. State and persistence

- A scene snapshots the full model: per-strip volume, pan, mute, solo,
  preamp gain, phantom, PAD, EQ, trim, phase, stereo split, sensitivity,
  output masters and the settings set.  Scenes carry the model string, so an
  ALSA scene applies to the USB backend and vice versa.
- Applying a scene re-issues real writes: volumes first, then mute/solo for
  every strip, output mute and loopback per pair, then best-effort optional
  controls (MS processor, AN 1>2, width, FX send, clock source).  A missing
  optional control never aborts the load.
- Defaults at open: internal clock, 48 kHz, linked input pair, width 0.
  Varispeed pitch is live-only and not persisted.
- The backend follows the hardware on external changes (§3.9).
