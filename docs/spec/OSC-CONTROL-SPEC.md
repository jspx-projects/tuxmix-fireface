# TotalMix FX OSC control specification

This document defines the OSC control surface used to drive TotalMix FX and
the mapping from that surface to the Fireface UCX register protocol.

The document covers protocol behaviour only. Enabling OSC in TotalMix FX and
configuring the network are operational steps and live outside this
specification.

## 1. Transport

| Property | Value |
|---|---|
| Protocol | OSC 1.0 |
| Transport | UDP |
| Encoding | big-endian |
| Values | 32-bit float, 32-bit integer, string |
| Control direction | controller to TotalMix FX |
| Feedback direction | TotalMix FX to controller |
| Fixed ports | none; both directions are configurable |
| Query request | none |
| Feedback source port | selected by the operating system |

A message is a single OSC message or an OSC bundle. Bundles contain
length-prefixed messages and may be nested.

TotalMix FX sends feedback only when state changes or when a page is selected.
A command that changes no state may produce no reply. The controller reads
state from feedback; it does not poll.

## 2. Message form

```text
/path/name ,types values
```

The type tag string is required. Control values are normally a single float:

```text
/1/micgain1 ,f 0.8
```

Display values are separate messages with a `Val` suffix:

```text
/1/micgain1Val ,s "52.0 dB"
```

A page prefix selects a control page. Sending any parameter with a page prefix
that differs from the current page makes TotalMix FX re-send the complete state
of that page.

## 3. Address model

```text
/page/control
/page/control/index
/control
```

| Prefix | Page | Purpose |
|---|---|---|
| `/1` | Mixer | Banked fader grid: inputs, software playback and hardware outputs |
| `/2` | Channel | Settings for the selected single channel |
| `/3` | FX and groups | Reverb, Echo, snapshots, mute/solo/fader groups and recorder |
| `/4` | Room EQ | Output bus Room EQ |
| `/` | None | Navigation and workspace commands |

Page 1 is a bank of faders. The UCX exposes 18 hardware inputs, 18 software
playback channels and 18 hardware outputs. Channel numbering is zero-based;
a stereo pair is addressed by its left channel.

Page 2 addresses one channel selected by `setBankStart`, `setOffsetInBank` or
page-1 navigation. The page-2 paths carry no channel index.

`setSubmix` selects the hardware output whose submix is edited. Page-1 faders
and pans are sends into that submix. Output faders are the submix masters.

## 4. Navigation

These paths have no page prefix.

| Path | Value | Effect |
|---|---|---|
| `/setSubmix` | `0 … submix_count-1` | Selects the hardware output submix |
| `/setBankStart` | `0 … channel_count-1` | Selects a page-2 channel; on page 1 sets the first channel of the bank |
| `/setOffsetInBank` | `0 … bank_size-1` | Selects the page-2 channel relative to the page-1 bank start |
| `/loadQuickWorkspace` | `1 … 30` | Loads a configured Quick Workspace |

A stereo channel is selected by its left channel number. Only valid,
remote-enabled channels are accepted.

Page 1 and page 2 also have relative navigation:

| Path | Effect |
|---|---|
| `/1/track+` | Move bank start one channel forward |
| `/1/track-` | Move bank start one channel back |
| `/1/bank+` | Move bank start one bank forward |
| `/1/bank-` | Move bank start one bank back |
| `/2/track+` | Select the next channel |
| `/2/track-` | Select the previous channel |

## 5. Value scales

| Scale | Receive | Feedback |
|---|---|---|
| `Lin01` | linear 0.0 … 1.0 | linear 0.0 … 1.0 |
| `Fader` | fader curve 0.0 … 1.0 | fader curve 0.0 … 1.0 |
| `Freq` | pseudo-logarithmic 0.0 … 1.0 | pseudo-logarithmic 0.0 … 1.0 |
| `OnOff` | 0.0 or 1.0 | 0.0 or 1.0 |
| `Toggle` | 1.0 toggles; 0.0 is ignored | resulting 0.0 or 1.0 |
| `Direct` | 1:1 value | 1:1 value |
| `Level` | meter value | meter value |
| `String` | text | text |

`Toggle` is edge-triggered. One `1.0` changes the state; a second `1.0`
changes it back. `0.0` does not set the state to off.

`Lin01` values map to the destination's full range. Type selections use
evenly spaced values; an EQ type with two choices uses `0.0` and `1.0`.

The fader curve maps a normalized position `p` to dB:

```text
f = p * 1023
if f >= 649:
    db = f * 0.0320855615 - 26.8235294118
else:
    db = -(f * f) / 11033 + f * 0.1497326203 - 65
```

The inverse curve is used for feedback:

```text
if db >= -6:
    p = ((db + 26.8235294118) / 0.0320855615) / 1023
else:
    p = (826 - sqrt(-34869 - 11033 * db)) / 1023
```

The frequency curve is:

```text
p = (ln(hz) - ln(20)) / (ln(20000) - ln(20))
hz = exp(p * (ln(20000) - ln(20)) + ln(20))
```

Display companions use these suffixes:

| Suffix | Contents |
|---|---|
| `Val` | numeric value, selection name or dB string |
| none | raw normalized value |

## 6. Page 1 — mixer

Page 1 is the banked mixer. Faders, pans, mutes, solos, selects and cues use
the fixed low channel numbers and the `9`-to-`24` families for channels above
the first bank.

| Path family | Function | Scale |
|---|---|---|
| `/1/volumeN` | Fader or output volume | `Fader` |
| `/1/mastervolume` | Main Out volume | `Fader` |
| `/1/panN` | Input/playback send pan or output balance | `Lin01` |
| `/1/mute/1/N` | Channel mute | `OnOff` |
| `/1/solo/1/N` | Solo/PFL | `OnOff` |
| `/1/select/1/N` | Ad-hoc fader group selection | `OnOff` |
| `/1/cue/1/N` | Cue output | `OnOff` |
| `/1/levelNLeft`, `/1/levelNRight` | Meter level | `Level` |
| `/1/tracknameN` | Channel name | `String` |
| `/1/micgainN` | Input preamp gain | `Lin01` |
| `/1/phantom/1/N` | 48 V | `OnOff` |
| `/1/labelSubmix` | Selected submix name | `String` |

Page-1 bus and monitoring controls:

| Path | Function | Scale |
|---|---|---|
| `/1/busInput` | Show input bus data | `Toggle` |
| `/1/busPlayback` | Show playback bus data | `Toggle` |
| `/1/busOutput` | Show output bus data | `Toggle` |
| `/1/globalMute` | Global mute | `Toggle` |
| `/1/globalSolo` | Global solo | `Toggle` |
| `/1/trim` | Trim mode | `Toggle` |
| `/1/mainDim` | DIM | `Toggle` |
| `/1/mainRecall` | Recall Main Out volume | `Toggle` |
| `/1/mainSpeakerB` | Speaker B | `Toggle` |
| `/1/speakerBLinked` | Speaker B linked to Main | `Toggle` |
| `/1/mainMuteFx` | Mute FX return on Main Out | `Toggle` |
| `/1/mainMono` | Mono Main Out | `Toggle` |
| `/1/mainExtIn` | External input path | `Toggle` |
| `/1/mainTalkback` | Talkback | `Toggle` |

Snapshot, group and recorder controls are also accepted on page 1 and page 3;
see section 8.

## 7. Page 2 — selected channel

Page 2 controls the channel selected by the bank/offset commands. The same
path changes meaning with the channel row: input preamp controls apply to
hardware inputs, EQ and dynamics apply to inputs and outputs, and FX send
applies to inputs and software playback.

| Path | Function | Scale | Rows |
|---|---|---|---|
| `/2/volume` | Fader or output volume | `Fader` | all |
| `/2/pan` | Send pan or output balance | `Lin01` | all |
| `/2/mute` | Mute | `Toggle` | all |
| `/2/solo` | Solo/PFL | `Toggle` | all |
| `/2/phase` | Phase left/mono | `Toggle` | inputs, outputs |
| `/2/phaseRight` | Phase right | `Toggle` | inputs, outputs |
| `/2/stereo` | Stereo/mono link | `Toggle` | all |
| `/2/gain` | Preamp gain | `Lin01` | inputs 1–4 |
| `/2/gainRight` | Right preamp gain | `Lin01` | inputs 1–4 |
| `/2/phantom` | 48 V | `Toggle` | inputs 1/2 |
| `/2/instrument` | Instrument impedance | `Toggle` | inputs 3/4 |
| `/2/pad` | Pad | `Toggle` | where present |
| `/2/autoset` | AutoSet | `Toggle` | inputs 1/2 |
| `/2/refLevel` | Reference level | `Direct` | inputs 3/4 |
| `/2/msProc` | MS processing | `Toggle` | inputs, playback |
| `/2/width` | Stereo width | `Lin01` | inputs, playback |
| `/2/reverbSend` | FX send | `Lin01` | inputs, playback |
| `/2/reverbReturn` | FX return | `Lin01` | outputs |
| `/2/cue` | Cue output | `Toggle` | outputs |
| `/2/talkbackSel` | Include in talkback | `Toggle` | outputs |
| `/2/noTrim` | Exclude from trim | `Toggle` | outputs |
| `/2/loopback` | Loopback | `Toggle` | outputs |
| `/2/recordEnable` | Record enable | `Toggle` | all |
| `/2/playChannel` | Playback track | `Direct` | playback |
| `/2/lowcutEnable` | Low cut enable | `Toggle` | inputs, outputs |
| `/2/lowcutFreq` | Low cut corner | `Freq` | inputs, outputs |
| `/2/lowcutGrade` | Low cut slope | `Lin01` | inputs, outputs |
| `/2/eqEnable` | Parametric EQ enable | `Toggle` | inputs, outputs |
| `/2/eqType1` | Band 1 type | `Lin01` | inputs, outputs |
| `/2/eqGain1` | Band 1 gain | `Lin01` | inputs, outputs |
| `/2/eqFreq1` | Band 1 frequency | `Freq` | inputs, outputs |
| `/2/eqQ1` | Band 1 Q | `Lin01` | inputs, outputs |
| `/2/eqGain2` | Band 2 gain | `Lin01` | inputs, outputs |
| `/2/eqFreq2` | Band 2 frequency | `Freq` | inputs, outputs |
| `/2/eqQ2` | Band 2 Q | `Lin01` | inputs, outputs |
| `/2/eqType3` | Band 3 type | `Lin01` | inputs, outputs |
| `/2/eqGain3` | Band 3 gain | `Lin01` | inputs, outputs |
| `/2/eqFreq3` | Band 3 frequency | `Freq` | inputs, outputs |
| `/2/eqQ3` | Band 3 Q | `Lin01` | inputs, outputs |
| `/2/compexpEnable` | Compressor/Expander enable | `Toggle` | inputs, outputs |
| `/2/compexpGain` | Make-up gain | `Lin01` | inputs, outputs |
| `/2/compexpAttack` | Attack | `Lin01` | inputs, outputs |
| `/2/compexpRelease` | Release | `Lin01` | inputs, outputs |
| `/2/compTrsh` | Compressor threshold | `Lin01` | inputs, outputs |
| `/2/compRatio` | Compressor ratio | `Lin01` | inputs, outputs |
| `/2/expTrsh` | Expander threshold | `Lin01` | inputs, outputs |
| `/2/expRatio` | Expander ratio | `Lin01` | inputs, outputs |
| `/2/alevEnable` | AutoLevel enable | `Toggle` | inputs, outputs |
| `/2/alevMaxgain` | AutoLevel maximum gain | `Lin01` | inputs, outputs |
| `/2/alevHeadroom` | AutoLevel headroom | `Lin01` | inputs, outputs |
| `/2/alevRisetime` | AutoLevel rise time | `Lin01` | inputs, outputs |
| `/2/trackname` | Channel name | `String` | all |
| `/2/levelLeft`, `/2/levelRight` | Meter level | `Level` | all |

### 7.1 Toggle commands

`mute`, `solo`, `phase`, `phaseRight`, `phantom`, `instrument`, `pad`,
`autoset`, `msProc`, `stereo`, `lowcutEnable`, `eqEnable`, `compexpEnable`,
`alevEnable`, `loopback`, `cue` and `talkbackSel` are toggles. Send `1.0` to
change the state. `0.0` is not an off command.

`phantom` also accepts `0.0` as a direct off command on the UCX.

### 7.2 Stereo button

`/2/stereo` toggles the pair. A linked pair writes the stereo-mode register on
both sides. Splitting writes the mono value on both sides.

There is no separate link register. The link state is the stereo-mode value.

## 8. Page 3 — FX, snapshots and groups

| Path | Function | Scale |
|---|---|---|
| `/3/reverbEnable` | Reverb enable | `Toggle` |
| `/3/reverbType` | Reverb type | `Lin01` |
| `/3/reverbPredelay` | Pre-delay | `Lin01` |
| `/3/reverbLowcut` | Low cut | `Freq` |
| `/3/reverbHighcut` | High cut | `Freq` |
| `/3/reverbRoomscale` | Room scale | `Lin01` |
| `/3/reverbAttack` | Envelope attack | `Lin01` |
| `/3/reverbHold` | Envelope hold | `Lin01` |
| `/3/reverbRelease` | Envelope release | `Lin01` |
| `/3/reverbTime` | Space reverb time | `Lin01` |
| `/3/reverbHighdamp` | Space high damp | `Freq` |
| `/3/reverbSmooth` | Smoothness | `Lin01` |
| `/3/reverbWidth` | Stereo width | `Lin01` |
| `/3/reverbVolume` | Reverb volume | `Lin01` |
| `/3/echoEnable` | Echo enable | `Toggle` |
| `/3/echoType` | Echo type | `Lin01` |
| `/3/echoDelaytime` | Delay time | `Lin01` |
| `/3/echoFeedback` | Feedback | `Lin01` |
| `/3/echoWidth` | Stereo width | `Lin01` |
| `/3/echoVolume` | Echo volume | `Lin01` |
| `/3/undo` | Undo | none |
| `/3/redo` | Redo | none |

Snapshot and group paths:

```text
/3/snapshots/8/1 … /3/snapshots/1/1
/3/muteGroups/4/1 … /3/muteGroups/1/1
/3/soloGroups/4/1 … /3/soloGroups/1/1
/3/faderGroups/4/1 … /3/faderGroups/1/1
```

The snapshot number and group number are encoded in the path. Send `1.0` to
load or enable.

Recorder paths:

| Path | Function | Scale |
|---|---|---|
| `/3/recordRecordStart` | Start recording | `Toggle` |
| `/3/recordPlayPause` | Play/pause | `Toggle` |
| `/3/recordStop` | Stop | `Toggle` |
| `/3/recordTime` | Current time | `String` |
| `/3/recordState` | Recorder state | `String` |

## 9. Page 4 — Room EQ

Selecting page 4 selects the output bus. `/4/track+` and `/4/track-` move the
selected output channel; page 2 then edits that output. Room EQ is a TotalMix
FX host feature; it has no UCX register mapping.

| Path | Function | Scale |
|---|---|---|
| `/4/leftChannel` | Select left channel | `Toggle` |
| `/4/rightChannel` | Select right channel | `Toggle` |
| `/4/reqEnable` | Room EQ enable | `Toggle` |
| `/4/reqDelay` | Delay | `Lin01` |
| `/4/reqVolumeCorr` | Volume correction | `Lin01` |
| `/4/reqTypeN` | Band N type | `Lin01` |
| `/4/reqGainN` | Band N gain | `Lin01` |
| `/4/reqFreqN` | Band N frequency | `Freq` |
| `/4/reqQN` | Band N Q | `Lin01` |
| `/4/trackname` | Channel name | `String` |

`N` is 1 through 9. When both `leftChannel` and `rightChannel` are active the
left channel's values are displayed and sent to both sides.

## 10. Feedback

Feedback uses the same paths as control. A value message may be accompanied by
a `Val` message:

```text
/1/micgain1 ,f 0.8
/1/micgain1Val ,s "52.0 dB"
```

| Companion | Contents |
|---|---|
| `volumeVal` | dB or `-oo` |
| `panVal` | pan string |
| `eqTypeNVal` | type name |
| `lowcutGradeVal` | slope name |
| `recordState` | recorder state string |
| `trackname` | channel name |

Feedback is event-driven. Selecting a new page sends the complete state of
that page. Bundles may contain several paths in one datagram; the controller
must not assume one message per datagram or a fixed ordering.

Peak-level feedback is optional. When enabled, TotalMix FX sends the level
paths for the visible channels:

```text
/1/level1Left, /1/level1Right, ...
/2/levelLeft, /2/levelRight
```

The level paths use the `Level` scale. Peak hold keeps the displayed maximum
at each path. These messages are feedback only; they have no control
direction and do not change any UCX register.

## 11. Channel rows and link rules

The three rows are hardware inputs, software playback and hardware outputs.
Page-2 settings apply to the selected row.

### 11.1 Input pairs

Linking an input pair writes `NN02 = 1` to both sides. The left channel is
the reference for the shared controls.

| Parameter | On link | On split |
|---|---|---|
| Volume | Pair level is the louder channel; both keep their difference | Both channel volumes remain |
| Pan | Pan values are retained; linked control is a balance derived from the two volumes | Left and right pans are written hard left and hard right |
| Mute, solo | Left value copied to right | Per-channel |
| 48 V, Inst, Level, refLevel, AutoSet | Left value copied to right | Per-channel |
| Gain | Left value is the linked control; right value retained | Per-channel |
| FX send | Left value copied to right | Per-channel |
| Low cut, EQ, dynamics, AutoLevel | Left values copied to right | Per-channel |
| Phase L, phase R | Independent | Independent |

### 11.2 Playback pairs

Playback pairs use a host-side link. No stereo register is written. Volume,
pan and balance follow the same pair rule as inputs; mute and solo copy from
left to right when linking. Splitting writes hard-left and hard-right pans.
The playback FX send remains per-channel; linking rewrites the pair matrix
without copying the left send value. Width and MS processing remain
host-side pair controls.

### 11.3 Output pairs

Linking an output pair writes `NN04 = 1` to both sides. Volume and balance
are encoded by the two channel volumes; the linked fader shows the pair level
and the balance knob attenuates the quieter side. Mute copies from left to
right. FX return, low cut, EQ, dynamics and AutoLevel copy from left to right
when linking. Phase L and phase R remain independent. Splitting leaves the
two channel volumes and pans available as separate controls.

The output reference level is a line-output group control, not a per-pair
link value. Writing `NN08` on one line output updates the line-output group
registers `0x1208..0x1708` together.

## 12. UCX register mapping

The following table maps OSC controls to UCX IF1 words. `NN` is the channel
register base:

```text
input channel   NN = channel << 8             channel 0..17
output channel  NN = (0x12 + channel) << 8    channel 0..17
right side      + 0x100
```

Inputs use `0x00..0x11`; outputs use `0x12..0x23`. Register values are 16-bit
unless stated otherwise. `0x8000` in the address field is the odd-parity bit,
not part of the address.

### 12.1 Page 1 and page 2 channel controls

| OSC path | Row | UCX target | Value |
|---|---|---|---|
| `/1/volumeN`, `/2/volume` | input/playback | fader record for selected submix | level, tenths dB × 16 |
| `/1/panN`, `/2/pan` | input/playback | fader record for selected submix | `(pan << 4) \| 8` |
| `/1/volumeN`, `/2/volume` | output | `NN00` | 0.1 dB, signed |
| `/1/panN`, `/2/pan` | output | `NN01` | −100 … +100 |
| `/1/mute/1/N` | input/playback | none | host monitoring |
| `/2/mute` | input | `NN00` | 0/1 |
| `/2/mute` | output | `NN02` | 0/1 |
| `/2/solo` | all | none | host monitoring |
| `/1/micgainN` | input 1–4 | `NN07` | 0.1 dB |
| `/2/gain` | input 1–4 | `NN07` | 0.1 dB |
| `/2/gainRight` | input 1–4 | `NN07 + 0x100` | 0.1 dB |
| `/1/phantom/1/N`, `/2/phantom` | input 1/2 | `NN08` | 0/1 |
| `/2/refLevel` | input 3/4 | `NN08` | 0 −10 dBV, 1 +4 dBu, 2 LoGain |
| `/2/refLevel` | output | `0x1208..0x1708` line group | reference index; line outputs write together |
| `/2/instrument` | input 3/4 | `NN09` | 0/1 |
| `/2/pad` | input | none | not exposed on the UCX |
| `/2/autoset` | input 1/2 | `NN0A` | 0/1 |
| `/2/phase` | input/output | `NN06` / output `NN07` | 0/1 |
| `/2/phaseRight` | input/output | `NN06 + 0x100` / output `NN07 + 0x100` | 0/1 |
| `/2/msProc` | input | `NN05` | 0/1 |
| `/2/msProc` | playback | matrix rewrite | host-side |
| `/2/stereo` | input | `NN02` | 0 mono, 1 stereo |
| `/2/stereo` | output | `NN04` | 0 mono, 1 stereo |
| `/2/lowcutEnable` | input/output | `NN20` | 0/1 |
| `/2/lowcutFreq` | input/output | `NN21` | Hz |
| `/2/lowcutGrade` | input/output | `NN22` | 0/1/2/3 = 6/12/18/24 dB/oct |
| `/2/eqEnable` | input/output | `NN40` | 0/1 |
| `/2/eqType1` | input/output | `NN41` | 0 Bell, 1 Shelf |
| `/2/eqGain1` | input/output | `NN42` | 0.1 dB, signed |
| `/2/eqFreq1` | input/output | `NN43` | Hz |
| `/2/eqQ1` | input/output | `NN44` | Q × 10 |
| `/2/eqGain2` | input/output | `NN45` | 0.1 dB, signed |
| `/2/eqFreq2` | input/output | `NN46` | Hz |
| `/2/eqQ2` | input/output | `NN47` | Q × 10 |
| `/2/eqType3` | input/output | `NN48` | 0 Bell, 1 Shelf, 2 Low pass |
| `/2/eqGain3` | input/output | `NN49` | 0.1 dB, signed |
| `/2/eqFreq3` | input/output | `NN4A` | Hz |
| `/2/eqQ3` | input/output | `NN4B` | Q × 10 |
| `/2/compexpEnable` | input/output | `NN60` | 0/1 |
| `/2/compexpGain` | input/output | `NN61` | 0.1 dB, signed |
| `/2/compexpAttack` | input/output | `NN62` | ms + 1 |
| `/2/compexpRelease` | input/output | `NN63` | ms + 1 |
| `/2/compTrsh` | input/output | `NN64` | 0.1 dB, signed |
| `/2/compRatio` | input/output | `NN65` | × 10 |
| `/2/expTrsh` | input/output | `NN66` | 0.1 dB, signed |
| `/2/expRatio` | input/output | `NN67` | × 10 |
| `/2/alevEnable` | input/output | `NN80` | 0/1 |
| `/2/alevMaxgain` | input/output | `NN81` | 0.1 dB |
| `/2/alevHeadroom` | input/output | `NN82` | 0.1 dB |
| `/2/alevRisetime` | input/output | `NN83` | 0.1 s |
| `/2/reverbSend` | input | `NN01` | 0.1 dB, signed |
| `/2/reverbSend` | playback | `0x47A0 + 2 × channel` and `+0x41` | linear matrix |
| `/2/reverbReturn` | output | `NN03` | 0.1 dB, signed |
| `/2/width` | input/playback | matrix rewrite | host-side |
| `/2/cue`, `/2/talkbackSel`, `/2/noTrim`, `/2/loopback` | output | none except loopback | host-side; loopback uses EP0 `0x15` |
| `/2/playChannel` | playback | none | host-side |
| `/2/recordEnable` | all | none | host-side |
| `/2/stereo` | playback | none | host-side |

### 12.2 Page 1 global and monitoring controls

| OSC path | UCX target | Value |
|---|---|---|
| `/1/busInput`, `/1/busPlayback`, `/1/busOutput` | none | host view |
| `/1/track+`, `/1/track-`, `/1/bank+`, `/1/bank-` | none | host view |
| `/1/globalMute`, `/1/globalSolo` | none | host-side |
| `/1/trim` | none | host-side |
| `/1/mainDim` | `0x3d03` | 0/1 |
| `/1/mainMono` | `0x3f9e` bit 1 | host/device monitor flags |
| `/1/mainTalkback` | `0x3f9e` bit 0 | host/device monitor flags |
| `/1/mainSpeakerB`, `/1/speakerBLinked` | output assign words | host/output assignment |
| `/1/mainMuteFx` | control-room `NN03` | FX return level |
| `/1/mastervolume` | Main Out `NN00` | 0.1 dB, signed |

### 12.3 Page 3 global FX

| OSC path | UCX target | Value |
|---|---|---|
| `/3/reverbEnable` | `0x3c00` | 0/1 |
| `/3/reverbType` | `0x3c01` | algorithm index |
| `/3/reverbPredelay` | `0x3c02` | ms |
| `/3/reverbLowcut` | `0x3c03` | Hz |
| `/3/reverbRoomscale` | `0x3c04` | 0.01 |
| `/3/reverbHighcut` | `0x3c08` | Hz |
| `/3/reverbSmooth` | `0x3c0b` | 0 … 100 |
| `/3/reverbVolume` | `0x3c0c` | 0.1 dB, signed |
| `/3/reverbWidth` | `0x3c0d` | 0 … 100 |
| `/3/echoEnable` | `0x3c20` | 0/1 |
| `/3/echoType` | `0x3c21` | 0 Stereo Echo, 1 Stereo Cross, 2 Pong Echo |
| `/3/echoDelaytime` | `0x3c22` | 0.01 s |
| `/3/echoFeedback` | `0x3c23` | % |
| `/3/echoWidth` | `0x3c26` | 0 … 100 |
| `/3/echoVolume` | `0x3c25` | 0.1 dB, signed |

Snapshot, group, recorder, undo and redo operate on TotalMix FX state and do
not write UCX registers.

### 12.4 Fader and matrix records

Input and playback faders are sends into the selected hardware output:

```text
base = (0x12 + dsp_output) << 8 | (0xe0 + input)
level = tenths_db << 4
pan = (pan << 4) | 8
```

The level and pan are two writes to the same address. The base is
submix-dependent; the selected hardware output chooses the `0x12e0` or
`0x22e0` family.

Matrix cells are:

```text
0x4000 + dsp_output * 0x40 + source
```

`source` is 0..17 for hardware inputs and `0x20..0x31` for software playback.
The value is 15-bit signed with a coarse-scale flag in bit 15.

Playback sends use the `0x4020` and `0x41A0` families. The playback FX send
uses `0x47A0 + 2 × channel` and its `+0x41` mirror.

### 12.5 IF1 word

Each IF1 word is a 32-bit little-endian value:

```text
bits 31      bits 30..16      bits 15..0
parity       address:15       value:16
```

The parity bit is odd parity over the address and value. Words are coalesced
into bursts; a burst carries complete 4-byte words. A 4-byte word is
`[value16 LE][address16 LE]` after stripping the parity bit.

The UCX state stream on endpoint `0x8d` uses the same word layout.
