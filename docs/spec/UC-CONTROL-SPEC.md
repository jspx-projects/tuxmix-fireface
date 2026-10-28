# Fireface UC control specification

This specification defines the control protocol and persistent mixer state of
the Fireface UC in normal RME mode.  It is the authoritative reference for
the UC control surface implemented by TuxMix.

## 1. Device and control paths

The UC uses EP0 vendor requests for control. Two additional endpoints carry
live information:

| Endpoint | Direction | Contents |
|---|---|---|
| `0x8b` | interrupt IN | Front-panel encoder events and output-latch state |
| `0x85` | bulk IN | 324-byte transport and link state |

Mixer state is write-only.  The host model is the authoritative copy and is
written to the device on connect and on each change.

## 2. EP0 request map

| Request | Direction | Function |
|---|---|---|
| `0x10` | OUT/IN | Options word, clock source, rate family and rate commit |
| `0x11` | IN | Device state; top byte carries the rate index |
| `0x12` | OUT | Mixer crosspoints, gain/phase values and output masters |
| `0x13` | OUT | Global mute |
| `0x14` | OUT | Global unmute |
| `0x15` | OUT | Loopback bank |
| `0x16` | OUT | Channel initialization |
| `0x17` | OUT/IN | Reference levels, 48 V, PAD, Inst |
| `0x1a` | OUT | Input gain and output-channel level |
| `0x1b` | OUT | Rate-generator/pitch coefficients |
| `0x1c` | IN | Firmware revision |
| `0x20` | OUT | Global enable |

## 3. Matrix and channel words

A mixer cell is addressed by:

```text
wIndex = block * 36 + source
source = 0..17   hardware inputs
source = 18..35  software playback channels
block  = 0..17   hardware output blocks
```

The value is a signed 16-bit level.  Bits 13:0 carry the level, with the two
lowest level bits in `wIndex` bits 14:15.  A level of `0x2000` is 0 dB.
Bit 15 carries phase inversion; the inverted form of a level is its 16-bit
two's complement (`0x2000` ↔ `0xE000`).

The output masters use `wIndex = 992 + block`.  The four submix banks occupy
`wIndex` bits 14:15; an output-balance change writes the left and right
masters in each bank.

Phase is a property of the source and applies to every routed cell.
Changing phase rewrites the source's active cells with the new sign.  A
later fader, pan, matrix-cell or full-matrix write carries the same sign.

## 4. Output level and balance

Hardware output level uses request `0x1a` with the 1 dB code:

```text
code = 6 - dB
0 = +6 dB, 6 = 0 dB, 63 = -57 dB
```

Output channel indices are:

| Output pair | Channels |
|---|---|
| Phones 7/8 | 10, 11 |
| AN1/2 | 4, 5 |
| AN3/4 | 6, 7 |
| AN5/6 | 8, 9 |
| SPDIF | 12, 13 |
| ADAT 1/2 … 7/8 | 14 … 21 |

An output pair carries balance as two distinct channel levels.  The linked
fader is the pair level: moving it applies one common dB offset to both
channels.  The two channel levels are the persistent state; the balance
position follows from their difference.

A balance change writes:

- the two `0x1a` output codes, and
- the corresponding left/right `0x12` output masters in the four banks.

The mixer represents input and playback pan/balance with the two source
levels for the selected output pair.

## 5. Switches and references

Request `0x17` is a 16-bit register:

| Bits | Function |
|---|---|
| 0, 1 | 48 V on inputs 1, 2 |
| 2, 3 | PAD on inputs 4, 3 |
| 4, 5 | Inst on inputs 4, 3 |
| 6–7 | Line output reference |
| 8–9 | Line input reference |
| 10–11 | Phones reference |

A switch change reads the complete register, changes the target field and
writes the complete 16-bit value.

## 6. Routing controls

Loopback uses request `0x15`.  An index in `0..29` addresses one output
channel; value `1` feeds that output into the same-numbered input channel.
The loopback bank is live and changes the running stream.

The matrix is latched when the audio engine starts.  TuxMix writes the
complete matrix — 36 sources × 18 blocks plus 18 masters — before opening
PCM.  Subsequent cell writes update the device's stored state and apply at
the next engine start.  Loopback remains live.

Input gain uses `0x1a` with 1 dB steps for inputs 1/2 and 0.5 dB steps
(codes 0–36) for inputs 3/4.

## 7. Sample rate and clock

The host stream owns the UC sample rate.  TuxMix restores its remembered
engine rate at startup and on reconnect; `Auto` releases the override and
follows the device/graph rate.

Rate selection uses `0x10`, `wIndex 48`:

| Rate | Word |
|---|---|
| 32.0 kHz | `0x0000` |
| 44.1 kHz | `0x0010` |
| 48.0 kHz | `0x0020` |
| 64.0 kHz | `0x0000` |
| 88.2 kHz | `0x0010` |
| 96.0 kHz | `0x0020` |
| 128.0 kHz | `0x0000` |
| 176.4 kHz | `0x0010` |
| 192.0 kHz | `0x0020` |

The word selects the 32 kHz, 44.1 kHz or 48 kHz family; the stream multiplier
follows the active stream.  A selection is followed by
`0x10 / wIndex 1487 = 0x0001`.

The options word at `wIndex 1487` carries clock source (bits 1–3), word-clock
single-speed (bit 6), SPDIF professional (bit 7) and optical output format
(bit 10).

A host rate change closes the current session with
`0x13 / wValue 0x000F / wIndex 0xC000`, writes the new rate word when the
family changes, waits 200 ms, then opens the new session.  The UC keeps its
rate family across a USB reconnect.  A USB reset restores the 1× multiplier.

## 8. State ownership

The device group is read from the unit at connect: input gain, 48 V, PAD,
Inst, reference levels, clock source and optical format.

The mixer group is written from the TuxMix scene: matrix, pans, faders,
mutes, loopback, output levels and output balance.  The connect sequence
writes 30 loopback entries, 18 output-volume words, 18 output masters and
the 666 matrix words.

Front-panel gain and output-level changes arrive on `0x8b` and update the
live model.  The host scene remains the source for the next connect.

## 9. Host-side mixer operations

Trim Gains is an editing mode over a channel's send levels.  A fader move in
trim mode writes the channel's `0x12` send cells directly; the send levels
are the persistent state.

Width and MS Proc are host-computed matrix operations.  The host writes the
resulting cross-feed coefficients as `0x12` cells.

Talkback is a host-side operation.  The UC control stream carries the normal
matrix and output state produced by the host mixer.
