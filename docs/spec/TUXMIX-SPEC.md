# TuxMix control surface specification

This document defines the TuxMix user interface for the Fireface UC and UCX,
the model behind it and the panel behaviour.

## 1. Mixer layout

TuxMix presents three channel rows:

1. Hardware Inputs
2. Software Playback
3. Hardware Outputs

Each row is a horizontal row of channel strips. Every section carries an
uppercase header — HARDWARE INPUTS, SOFTWARE PLAYBACK, HARDWARE OUTPUTS,
CONTROL ROOM — and each header spans the width of its own strips, opened by an
accent tick and closed by a matching block; the slack between sections is the
break in the line. When a row is wider than the
mixer area, one horizontal slider scrolls the three rows together. The slider
sits in the main area, immediately left of the right sidebar.

The right sidebar contains the device card, Undo/Redo, global mute/solo, the
FX switch, group and snapshot controls, and layout. It stays in the layout as
the window narrows. Its default width is 200 px; it accepts resizing and has
its own collapse toggle.

The Control Room closes the Hardware Outputs row, flush with the mixer's right
edge.

The device card reads:

```text
Internal, <sample rate>, <buffer>
```

## 2. Channel strip

A full strip contains:

- channel name and type;
- pan or balance knob;
- mute and solo; outputs use mute and cue;
- level fader and meter;
- a right-hand column in fixed order: Trim, settings gear, EQ, Dynamics;
- the channel's active panel, when open.

Every full strip has the same height. A collapsed strip uses that same
height: the meter absorbs the space that the fader and controls no longer
use. A Settings, EQ, Dynamics, Trim or Route panel opens beside the strip at
that same height. The standard height is 350 px at scale 1.0.

### 1.1 Reset Mix

The top bar carries `RESET MIX` beside the view tabs, listing the manual's
Options > Reset Mix entries. Choosing one rewrites the whole mixer and pushes
it in a single scene apply:

| Entry | Effect |
|---|---|
| Straight playback with all to Main Out | playbacks 1:1 to the outputs, each also mixed to Main |
| Straight playback | playbacks 1:1 to the outputs, nothing else routed |
| Clear all submixes | every input and playback send to zero |
| Clear channel effects | EQ, low cut, dynamics, Auto Level, Reverb, Echo and Width off |
| Set output volumes | outputs 0 dB; Main and Speaker B −10 dB |
| Reset channel names | user names cleared |
| Set all channels mono / stereo | every pair's stereo/mono mode |
| Set inputs mono, outputs stereo | the ADM layout |
| Total reset | straight playback to Main plus effects off and output volumes set |

### 2.0 FX fader

Every strip on a unit with an FX bus carries a second, thin fader at the top
of its right-hand control column.  The T / gear / EQ / D column starts
directly below that fader, as high as the strip allows; spare height stays
below the column, not between the fader and the buttons.

The symbols sit on one grid shared by all strips.  Every strip has a main
fader; the column is anchored to it, below the FX fader when the unit has an
FX bus and directly below the fader top otherwise.  The column holds the
symbols the unit has, in the fixed order `T`, settings gear, `EQ`, `D` — the
UC has `T` and the gear, the UCX has all four.  The hardware-input row is the
reference, and every other strip places its symbols at the same absolute
heights.  A channel without one of them keeps an empty slot of the same
height and gap, so no symbol below moves:

| Strip | Fader | Register | Fill |
|---|---|---|---|
| Input | FX send | `NN01`, 0.1 dB | orange |
| Software playback | FX send | `0x47A0` family, linear, `+0x41` mirror | orange |
| Hardware output | FX return | `NN03`, 0.1 dB | green |

Both faders slide in a dark slot drawn by the fader: the main fader's rail
inside a wider slot, the small one as a single thinner line scaled with its
smaller cap. Its rail fills with the send/return colour as it is raised, so an
untouched control shows no colour at all.

The main fader's value sits at the strip's bottom-left, under its scale.

Dragging a fader or knob keeps the ordinary pointer; the grab hand is not
used, since it covers the values it hovers over.

### 2.1 Knob appearance

- Every knob has the same near-black face, a step darker than the strip card,
  so its rim and arc read as colour against the card.
- Every interactive knob draws a faint full-range track and a value arc from
  the range minimum, or from top centre for pan and balance.
- The default arc colour is white. Semantic colours are:
  - input gain: orange;
  - EQ gain: red, green and blue for bands 1–3;
  - pan and balance: accent colour;
  - dynamics: solo colour.
- Frequency labels are compact: `20`, `120`, `1k`, `1.5k`, `10k`. There is at
  most one decimal and no unit suffix; the row label supplies the unit.
- Label size is 15 px for 1–4 characters, 11 px for 5–6, and 9.5 px for longer
  frequency labels.

### 2.2 On/off controls

An on/off switch sits at the left of its row when the row has a label. This
applies to Dynamics, Auto Level, Reverb and Echo.

### 2.3 Labels and alignment

- Related controls and values share a row or column. A control under another
  control uses the same column width and alignment.
- Ordinary nouns in slideouts are lowercase: `gain`, `attack`, `compressor`,
  `auto level`.
- The minimum gap between distinct controls is `theme::SPACE_TIGHT`
  (2 px at scale 1).
- A knob and its label use `theme::KNOB_LABEL_GAP`. The `attack` label and
  its knob are the reference distance.

### 2.4 Tab appearance

- The settings gear is a 28×26 tab with a 19 px glyph.
- Trim, EQ and Dynamics use the 24×22 tab.
- The active tab carries the panel's dark surface behind its glyph.
- The glyph is centred in the tab before and after opening.

## 3. Settings panel

The panel follows the channel type.

| Channel | Contents, top to bottom |
|---|---|
| Input 1/2 | Stereo, 48V, Gain, AutoSet, FX Send, MS Proc, Phase L/R |
| Input 3/4 | Stereo, Inst, Gain, Level, AutoSet, FX Send, MS Proc, Phase L/R |
| Inputs 5–8 | Stereo, FX Send, MS Proc, Phase L/R |
| Digital inputs | Stereo, FX Send, MS Proc, Phase L/R |
| Software playback | Stereo |
| Hardware output | Stereo, FX Return, Loopback |

48 V is available on inputs 1/2. Inst and Level are available on inputs 3/4.
The UCX has no PAD.

Control styles:

Buttons are only as wide as their label needs; list and menu rows are the
exception.

- `48V` uses the orange phantom outline.
- `STEREO`, `INST`, `AUTOSET`, `FX RETURN` and `LOOPBACK` use an orange
  text-and-outline engaged state.
- The Level button shows `-10dBV`, `+4dBu` or `LoGain` and cycles through the
  three values.
- Phase uses green text and a green outline when engaged.

### 3.1 Phase

Phase is available on input strips at the bottom of the Settings panel.

- Inputs 1/2 show a **Phase** button; a linked pair shows **Phase L** and
  **Phase R**.
- Other input channels use `Ø`, or `Ø L` and `R Ø` for a linked pair.

### 3.2 Gain

Gain is a knob in the Settings panel for the four front inputs. Inputs 1/2
use the microphone range; inputs 3/4 use the instrument/line range. The knob
writes the per-channel preamp gain and follows the stereo link.

### 3.3 Device sheet

The device card at the top of the sidebar (or the gear on the folded rail)
opens the device sheet. It starts with the group the unit itself owns, read at
connect and written only when changed there.

| Row | UC | UCX |
|---|---|---|
| Clock source | yes | yes |
| Sample rate | yes | yes, when the device owns the rate |
| Line In reference | yes | — |
| Line Out reference | yes | — |
| Phones reference | yes | — |
| Optical out: ADAT/SPDIF | yes | global grid |
| SPDIF professional | yes | — |
| Word clock out: single speed | yes | — |
| Firmware | yes | yes |
| Serial | yes | yes |

The UC reference rows are the unit's global options word. The UCX handles
input and output sensitivity per channel instead, in per-channel registers
(`NN08` on inputs 3/4, `0x0407`–`0x0707` for inputs 5–8, `NN08` on the output
pairs, `0x1808/0x1908` for Phones). Where one control drives a group, it
writes each channel's own register. Rows a unit has no register for are absent
rather than shown inert.

Below the device group, the host engine group shows the PipeWire rate, buffer
size and latency when the sound server owns the rate.

## 4. Equalizer panel

EQ and low cut are available on every hardware input and every hardware
output. The EQ trigger appears on both rows; playback strips have no EQ.

### 4.1 Response plot

- The response line is drawn in three segments: red low, green mid, blue high.
- The boundaries are `sqrt(f1*f2)` and `sqrt(f2*f3)`, where the three band
  frequencies are sorted.
- The three band frequencies are always present, including when a band is off.
- The three gain knobs carry their band colour on the arc. Frequency and Q
  knobs use the white default.

### 4.2 Bands

| Parameter | Range |
|---|---|
| Gain | ±20 dB |
| Frequency | 20 Hz..20 kHz |
| Q | 0.7..5.0 |

Band 1 and band 3 can be Bell or Shelf. Band 2 is Bell. Band 3 can also be a
Low Pass (Hi Cut).

The UI model uses Hz and Q × 10. That mapping is written to the device.

### 4.3 Low cut

The low-cut row is one centred line:

- the Low Cut on/off button on the left;
- the frequency knob in the middle;
- the 6/12/18/24 dB/oct buttons on the right.

The full word is used when the panel is wide enough; `LC` is the narrow
fallback. The dB/oct unit is in the slope tooltips. Low cut is independent of
the main EQ bypass. Frequency and slope remain editable while it is off, and
the plotted low-cut response is omitted while off.

### 4.4 Panel geometry

Inline slideouts keep at least 8 px between the panel edge and content
(`theme::FLYOUT_PAD`). This applies to Settings, EQ, Dynamics and Trim.

A slideout never shows a scrollbar at normal size. The panels are sized to the
strip height and their contents are compacted to fit. The scrollable wrapper
stays in the code so content remains reachable at extreme zoom.

A slideout has no close button; the strip tab that opened it closes it again.
When vertical space is tight, a knob label may sit over the unused bottom
margin of its knob, with the panel background behind the label.

## 5. Dynamics panel

A **D** tab opens on every hardware input and every hardware output. Playback
strips have no Dynamics.

| Row | Controls |
|---|---|
| Header | On button at the left, then the Dynamics label |
| Shared | Gain −30..+30 dB, Attack 0..200 ms, Release 100..999 ms |
| Compressor | Threshold −60..0 dB, Ratio 1..10 |
| Expander | Threshold −99..−30 dB, Ratio 1..10 |
| Auto Level | On/off, Max. Gain 0..18 dB, Headroom 3..12 dB, Rise 0.1..9.9 s |

Gain, attack and release are shared between compressor and expander.

The panel follows the new TotalMix FX layout: the amplitude graph spans the
top, the shared Gain/Attack/Release knobs sit below it, then the
Compressor/Expander columns and the Auto Level row. Compressor aligns with
Attack and Expander with Release. Threshold and ratio are grouped as one
tightly spaced block to show that they belong together; the ratio row is
labelled `ratio 1:` on one line.

The graph has input level on the horizontal axis and output level on the
vertical axis. The curve has three coloured segments:

- green below the expander threshold;
- blue between the expander and compressor thresholds;
- red above the compressor threshold.

The compressor knobs and labels use red, the expander knobs and labels use
green. The shared Gain/Attack/Release and Auto Level knobs use white.

Each knob writes the complete dynamics block for its channel.

## 6. FX panel

The FX panel is a column between the mixer rows and the sidebar, toggled by
the `FX` button in the top bar and shown only on a backend with the FX bus.
Reverb sits above Echo in one 180 px column.

| Band | Height |
|---|---:|
| Header (on switch + effect name) | 36 |
| Type row | 31.5 |
| Reverb controls | 247.5 |
| Reverb type-specific band | 98 |
| Echo controls | 343 |

Knob rows are 78.75 px apart and knobs are 49.5 px across. Reverb's volume
fader occupies the right-hand column across the first two knob rows; Echo's
occupies the second column.

### 6.1 Reverb

| Control | Range |
|---|---|
| On | on/off |
| Type | Small Room, Medium Room, Large Room, Walls, Shorty, Attack, Swagger, Old School, Echoistic, 8plus9, Grand Wide, Thicker, Envelope, Gated, Space |
| PreDelay | 0..999 ms |
| Room scale | 0.50..3.00 |
| Volume | −30..+6 dB, vertical fader |
| Low cutoff | 20..500 Hz |
| High cutoff | 5..20 kHz |
| Smooth | 0..100 |
| Width | 0..100 |

The lower band holds the type-specific parameters, in the same knobs:

| Type | Controls |
|---|---|
| Envelope (12) | attack 5..400 ms, hold 5..400 ms, release 5..500 ms |
| Gated (13) | hold 5..400 ms, release 5..500 ms |
| Space (14) | reverb time 0.1..9.9 s, high damp 200..20000 Hz |

Other types leave the band empty.

### 6.2 Echo

| Control | Range |
|---|---|
| On | on/off |
| Type | Stereo Echo, Stereo Cross, Pong Echo |
| Delay | 10..2000 ms |
| Delay BPM | 30..300 |
| Feedback | 0..100 % |
| High cutoff | 0..20 |
| Width | 0..100 |
| Volume | −30..+6 dB, vertical fader |

BPM is derived from the delay: it reads `60000 / delay` and writes the delay
register, `60000 / bpm`.

### 6.3 FX Return

Each hardware output has an **FX Return** knob in its Settings panel,
−65..+6 dB. A linked pair writes both channels.

## 7. Output balance

Hardware output strips include a balance knob, −100..+100. The knob writes the
output balance register and is independent of the pair volume. The fader is the
pair level and does not change balance. A split pair keeps two independent
volumes.

## 8. Pan and balance

Input and software-playback strips expose pan into the selected submix. The
knob writes the source's matrix cells for that submix.

## 9. Motion

All strip panels open and close with a 90 ms width tween:

- Settings, EQ, Dynamics and Trim grow and shrink beside their strip;
- Route grows and shrinks as a raised picker;
- Quick view uses the same tween;
- switching Settings, EQ, Dynamics or Trim on one strip tweens directly from
  the current width to the new width.

Toggling Stereo tweens the second half of the pair between zero and full width
over the same 90 ms. Channels to the right reflow with the tween.

## 10. Stereo link

Linking an input pair uses the left channel as the reference. Volume, mute,
solo, phantom, Inst, Level, AutoSet and gain copy to the right channel; pans
centre. Splitting sets the left and right pans hard left and hard right.

Phase L and Phase R keep independent states across the transition.

Software-playback pairs use the same reference-and-centre rule. Output pairs
link at centre balance.

## 11. Control Room

The Control Room panel closes the mixer, flush with its right edge beside the
FX column. Its buttons sit in TotalMix's background groups:

| Group | Control | Behaviour |
|---|---|---|
| assign | Assign | Picks the Main Out from the hardware outputs (output mode word `0xNN02` bit 1). Assigning another pair clears the previous one. The button reads `assign` with the list closed |
| dim / recall | Dim | Lowers the Main Out by 20 dB and sets the device Dim flag |
| | Recall | Restores the pre-Dim level and clears the Dim flag |
| mono / speaker B / ext. input / mute FX | Mono | Host-side monitoring option |
| | Speaker B | Switches the main monitoring output to Speaker B |
| | Ext. input | Switches monitoring to the external stereo input. Inert: the level register is unmeasured |
| | Mute FX | Host-side effect-return option |
| talkback | Talkback | Dims the Phones outputs and sends the talkback source. Inert: the source assignment is unmeasured |

Dim, Recall and Speaker B use the UCX control-room registers. Mono and
Mute FX are stored in the device scene.

### 11.1 Assign

`assign` opens a two-level menu. The first level lists the monitoring
destinations, each with the output it currently holds:

| Destination | Device |
|---|---|
| Main Out | output mode word `0xNN02` bit 1 on the chosen pair |
| Main Out B | the same bit, plus control-room state word `0x3d00 = 3` |
| Phones 1..4 | host-side; the unit receives nothing |
| Cue/PFL to | host-side; the unit receives nothing |

The menu opens immediately left of the `assign` button and covers the
section's channels while it is open.

Hovering or clicking a destination opens the second level beside it: every
hardware output in order, `None` at the top, and the currently assigned output
in bold. Choosing `None` clears that destination. The list closes when an
output is chosen or a click lands outside it.

The section carries its own `control room` title in the same line as the
HARDWARE OUTPUTS label. The title starts at the section's leftmost channel,
and a gap separates the two titles when the row has room to spare for it.

A Control Room channel's slideout opens to its right like any other. The
section shifts its own channels left by one slot to make the space, and its
left edge may overlap the channel row, which never moves.

Control Room channels are ordinary hardware outputs for routing too: clicking
one selects that output's submix and it takes the same brighter surface as any
other output does, which is how the manual describes every hardware output
("click on the Hardware Output channel ... This channel turns brighter, means
it is selected as current submix"). The Main Out is a submix like any other;
FX Send can follow it.

Every click on a hardware output counts, not only the strip's empty surface:
its mute and cue switches, its slideout buttons (gear, EQ, D), its fader and
its name field select that output the same way, and the visible submix
switches to it.  An output strip that is already part of a multi-selection
keeps the selection when one of its controls is clicked, so group edits still
apply.

Assigning a pair moves its channel out of the Hardware Outputs row into the
Control Room section, renamed for the destination — `Main`, `Speaker B`,
`Phones 1`–`Phones 4`, `Cue` — and the panel widens to hold it. Assigning a
different output, or `None`, returns the previous channel to the Hardware
Outputs row in its original position.

The manual (§25.4) describes the move: assigning moves the channel "from the
Hardware Outputs into the Control Room section, and renamed Main. The same
happens when assigning Main Out B or the Phones." Its `Names` view option is
what lets the original name be shown again; TuxMix keeps the destination name
while the channel is in the Control Room.

Whether the front-panel volume knob follows a reassigned Main is unmeasured.

## 12. Latency chain

The Buffer size picker sets the PipeWire graph quantum, one ALSA period. The
driver keeps a ring of at least the two queued USB URBs plus two URBs of
margin. At 1x rates that is 192 frames: four periods at 48 samples, three at
64, and two at 96. At 128 samples the two periods give 256 frames.

```text
ring = max(2 × quantum, ceil(192 / quantum) × quantum)
output latency ≈ ring + 2 × 48 + 48 frames
```

For 44.1 kHz:

| Quantum | ALSA buffer | USB queue | Endpoint | Total |
|---|---|---|---|---|
| 48 | 4.4 ms | 2.2 ms | 1.1 ms | 7.6 ms |
| 64 | 4.4 ms | 2.2 ms | 1.1 ms | 7.6 ms |
| 96 | 4.4 ms | 2.2 ms | 1.1 ms | 7.6 ms |
| 128 | 5.8 ms | 2.2 ms | 1.1 ms | 9.1 ms |

The Settings **Latency** line shows the card ring buffer, USB queue, endpoint
and total. The **Buffer size** picker shows the graph quantum.

The RME Pro Audio nodes use polled scheduling, a graph-owned quantum and at
least two ALSA periods. TuxMix's graph-quantum override reaches the card at
48, 64, 96 and 128 samples. The low-latency rule is
`ui/tuxmix/tools/alsa/wireplumber-uc.conf`.

## 13. State and persistence

A scene stores:

- matrix cells and pan values;
- input, playback and output faders and mutes;
- input and output EQ, low cut, dynamics and AutoLevel;
- output balance and FX Return;
- loopback flags;
- Reverb and Echo settings;
- channel names and trim values.

The UC device group is read at connect: input gain, 48 V, PAD, Inst, reference
levels, clock source and optical format.

The UCX host scene is authoritative. Reverb, Echo, dynamics and AutoLevel are
host-owned and re-applied from the scene.

TuxMix restores its remembered engine rate on startup and reconnect. The UC
sample rate follows the host stream; `Auto` follows the device or graph rate.

The matrix is written before PCM opens. Loopback is written live.

## 14. Control invariants

- Every channel control has a defined effect in the mixer model and its device
  path.
- Engaged controls use their defined style: orange outline for Stereo, Inst,
  AutoSet, FX Return and Loopback; green outline for Phase; orange arc for
  gain, pan and output balance; solo colour for Dynamics.
- Settings, EQ, Dynamics, Trim and Route panels use the same height as their
  strip.
- Panel and row transitions use the 90 ms width tween.
- The right sidebar remains visible while expanded, at every window width.
- The three channel rows scroll together under one horizontal slider.
- Phase, pan, output balance, dynamics, FX and loopback survive scene recall.
- Output balance survives output-fader movement.
- Trim attenuates a channel's sends in the mixer: the value is applied when
  each send is written, so it adds no device register.  `T` is the first
  icon in the strip's column on hardware inputs and software playbacks; it
  lights while trim is below 0 dB, and its tooltip states the applied value.
- Width, playback stereo/mono, Mono, No Trim and Talkback receive are
  host-side and do not produce device writes.
