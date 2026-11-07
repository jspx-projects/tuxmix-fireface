# TotalControl GUI specification

This document defines the visible TotalControl interface for the Fireface UC
and UCX: layout, controls, panels, gestures and motion. Runtime structure,
state ownership and device I/O are in
[`../TOTALCONTROL-ARCHITECTURE.md`](../TOTALCONTROL-ARCHITECTURE.md). The
end-user manual is [`../TOTALCONTROL-Manual.md`](../TOTALCONTROL-Manual.md).

## Contents

1. [Mixer layout](#1-mixer-layout)
2. [Channel strip](#2-channel-strip)
3. [Runtime reference](#3-runtime-reference)
4. [Settings panel](#4-settings-panel)
5. [Equalizer panel](#5-equalizer-panel)
6. [Dynamics panel](#6-dynamics-panel)
7. [FX panel](#7-fx-panel)
8. [Output balance](#8-output-balance)
9. [Pan and balance](#9-pan-and-balance)
10. [Motion](#10-motion)
11. [Stereo link](#11-stereo-link)
12. [Control Room](#12-control-room)
13. [Latency display](#13-latency-display)
14. [State references](#14-state-references)
15. [Control invariants](#15-control-invariants)

## 1. Mixer layout

TotalControl presents three channel rows:

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
FX knob, group and snapshot controls, and layout. It stays in the layout as
the window narrows. Its default width is 200 px; it accepts resizing and has
its own collapse toggle.

The global row is `M`, `S`/`F`, `F`. The left button is the Mute master. The
middle button is the Solo master; in PFL mode it shows `F` while an input is
listening and `S` otherwise, and it switches the active PFL inputs off and
back on. The right `F` is the temporary fader group, not the FX panel.

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
- level fader and a meter with a coloured RMS bar, a matching peak line and
  peak hold;
- a right-hand column in fixed order: Trim, settings gear, EQ, Dynamics;
- the channel's active panel, when open.

Every full strip has the same height. A collapsed strip uses that same
height: the meter absorbs the space that the fader and controls no longer
use. A Settings, EQ, Dynamics, Trim or Route panel opens beside the strip at
that same height. The standard height is 350 px at scale 1.0.

### 1.1 Options menu

The top bar carries `OPTIONS` beside the view tabs. It opens a menu with
`Reset Mix` and `Settings`. `Reset Mix` is a submenu; choosing an entry closes
the menu, rewrites the whole mixer and pushes it in a single scene apply:

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

### 1.2 App settings

`Options > Settings`, the tray Settings entry and the `,` shortcut open the
app-level settings sheet. It holds BACKGROUND, STARTUP SYNC, SURROUND and
OSC sections.

SURROUND has `Virtual 5.1 output` and `Virtual 7.1 output` checkboxes. A
checked output creates a PipeWire sink while TotalControl runs and removes it
when the application exits. The setting is persisted. The sinks follow the
selected Fireface/MR18 Pro Audio device and bridge to the first six or eight
SW playback pairs.

OSC has an enable switch, `Classic` and `Global` protocol buttons, a
receive-port field, a send-port field and `Apply`. `Classic` is the banked
TotalMix FX 1.96–2.11 address space; `Global` is the absolute-channel TotalMix
FX 2.1+ address space. Enabling it starts the loopback UDP surface; disabling
it stops it. The switch, protocol and both ports are persisted. The command
line `--osc`, `--osc-mode`, `--osc-port` and `--osc-send-port` remain a
development override.

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
height and gap, so no symbol below moves.  The gap below the FX fader is
`theme::SPACE_LG` plus the column's own spacing (12 px at scale 1) and the
symbols are `theme::SPACE_MD` (6 px) apart; empty slots use the same heights
and gaps.

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
- An always-on coloured outline is not used.  The full-range track stays
  neutral; colour appears on an arc only while the control is live.
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
- A column of knobs shares one vertical centre line. A secondary readout such
  as the Echo BPM sits at the underside or the right of its knob without
  moving the knob off that line.
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
- The T, EQ and D tabs keep their glyph only, in orange, while their setting
  is active and the slideout is closed: trim below 0 dB, EQ enabled, or
  dynamics on. While the slideout is open the tab carries no orange; the
  panel's own on/off control is the single indication. The gear tab only
  marks the open panel.
- Outlines belong to buttons, not slideout tabs. A slideout tab never draws
  a button outline or a copy of the panel-open marker. Hover brightens the
  glyph but never uses orange.
- The T, gear, EQ and D tabs keep the same fixed slot with the slideout open
  or closed. Only the panel grows beside them; the tabs themselves never
  move.
- The glyph is centred in the tab before and after opening.

### 2.5 Control sizing

There are two sizing families, one per surface:

| Surface | Standard control height at scale 1 |
|---|---|
| Channel strip (`M`/`S`/`CUE`, tabs, collapse, submix box) | 18 px |
| Settings slideout (buttons and dropdowns) | 24 px |

Standard controls use one width per stack or panel: a single button spans it;
a pair in one row splits it with one gap. Menu rows share the menu width.
Slideout tabs (`T`, gear, `EQ`, `D`), the channel minimize button and compact
navigation and utility buttons keep their own fixed or label-sized size. A
strip's `M`/`S` pair (hardware inputs and software playback) or `M`/`CUE` pair
(hardware outputs, including Control Room channels) shares the strip's control
width; an output never shows `M` alone.

- The strip's submix box spans the strip width at the strip height, with the
  bus name vertically centred at the left and the arrow at the right.
- The dB readout under the main fader is centred on the fader track and has
  no unit suffix; the numeric scale stays in the lane beside the track. The
  collapse button sits at the right edge of the readout row.

### 2.6 Pointer gestures

Dragging a fader or knob changes it; the scroll wheel nudges it. Holding Shift
reduces the movement for fine adjustment; this applies to faders, knobs,
matrix cells and EQ handles. A press captures the pointer until release, so a
drag continues when the pointer leaves the control or the window.

A double click on a fader, knob, FX fader or EQ handle resets that control to
its default. A double click on the dB readout opens a numeric entry field. A
double click on a channel name starts a rename, on the strip background
collapses the strip, and on a Matrix cell toggles the cell between off and
unity. Ordinary push buttons act once per click and have no separate double
click.

### 2.7 Meters

Each channel meter has the two TotalMix indicators:

- an **RMS bar** with a slow time constant, coloured in three bands: green
  below -3 dB, yellow from -3 dB to 0 dB and red above 0 dB;
- a **peak line** with zero attack, held for a fixed two seconds and then
  decaying; its colour follows the same green/yellow/red bands;
- a **fixed red OVR block** at the top, which lights when a full-scale
  sample is detected.

Peak hold applies only to the peak line. The RMS bar is never held; it
continues to follow the RMS value. The meter scale runs from -60 dB at the
bottom to +6 dB at the top; below -60 dB reads as silence. About 6 dB of
headroom above 0 dB is visible as the red band before the OVR block lights.
The bar is mathematically correct RMS, so a full-scale sine reads about
-3 dB; no RMS +3 dB shift is applied.

The peak hold is fixed at two seconds. The OVR block lights on a full-scale
peak; there is no configurable hold time or OVR sample count.

Input and playback meters are pre-fader and post-trim: the trim changes the
reading, the send fader and selected submix do not. Hardware output meters
are post-fader and go to zero when the output is muted. A linked pair shows
one bar per channel; each bar measures its own channel, so linking does not
copy an input signal or meter value to its sibling.

## 3. Runtime reference

The device-service boundary, command queue, model snapshots and meter bus are
defined in [`TOTALCONTROL-ARCHITECTURE.md`](../TOTALCONTROL-ARCHITECTURE.md) §5
and §6. This specification only describes the visible result: a control shows
its local drag position immediately, and the next confirmed model snapshot
settles it.

## 4. Settings panel

The panel follows the channel type.

The layout is one vertical stack per channel type. Inputs share a nine-row
grid; a control a channel lacks leaves its row empty, so the rows below never
move and `send FX` and the phase pair sit at the same height on every input.
Empty rows keep the height of the control the grid reserves for them.

### 4.1 Input slideout grid

| Row | Inputs 1/2 | Inputs 3/4 | Inputs 5–8 | Inputs 9+ |
|---|---|---|---|---|
| 1 | `stereo` | `stereo` | `stereo` | `stereo` |
| 2 | space | reference label | reference label | space |
| 3 | `+48V` | `instr.` + reference | reference | space |
| 4 | `auto set` | `auto set` | space | space |
| 5 | gain knob | gain knob | space | space |
| 6 | `width` | `width` | `width` | `width` |
| 7 | `send FX` | `send FX` | `send FX` | `send FX` |
| 8 | `MS proc` | `MS proc` | `MS proc` | `MS proc` |
| 9 | `Ø L` `Ø R` | `Ø L` `Ø R` | `Ø L` `Ø R` | `Ø L` `Ø R` |

### 4.2 Playback and output slideout grids

| Row | Software playback | Hardware output |
|---|---|---|
| 1 | `stereo` | `stereo` |
| 2 | `width` | reference label |
| 3 | `send FX` | reference dropdown |
| 4 | `MS proc` | `return FX` |
| 5 | `Ø L` `Ø R` | `Ø L` `Ø R` |
| 6 | — | `no trim` |
| 7 | — | space |
| 8 | — | `talkback` |
| 9 | — | `loopback` |

The row heights are shared across channel types. A missing control keeps its
row so the controls below stay aligned. The Control Room output keeps its own
reference group; `talkback` is omitted on Control Room channels.

| Channel | Contents, top to bottom |
|---|---|
| Input 1/2 | Stereo, space, 48V, AutoSet, Gain, Width, Send FX, MS Proc, Ø L / R Ø |
| Input 3/4 | Stereo, `ref.`, Inst + reference dropdown, Gain, Width, Send FX, MS Proc, Ø L / R Ø |
| Inputs 5–8 | Stereo, `ref.`, reference dropdown, two empty rows, Width, Send FX, MS Proc, Ø L / R Ø |
| Inputs 9+ | Stereo, four empty rows, Width, Send FX, MS Proc, Ø L / R Ø |
| Software playback | Stereo, Width, Send FX, MS Proc, phase slot |
| Hardware output | Stereo, `ref.`, reference dropdown, Return FX, Ø L / R Ø, No Trim, space, Talkback, Loopback |

- 48 V and AutoSet are available on inputs 1/2; Inst and a gain knob on 3/4;
  a reference dropdown on 3–8; none from channel 9 on.
- The reference levels for 5–8 move together: any change to one applies to
  all four. Inputs 3 and 4 keep separate levels unless linked.
- Width is a pair control, -1.00 … 1.00: 1.00 full stereo, 0.00 mono and
  -1.00 swapped channels. It rewrites the pair's 2x2 send matrix in every
  routing that carries a live send; silent routings are untouched. Inputs and
  software playback share it.
- MS Proc is a pair button on inputs and software playback. On the UCX it
  writes `NN05` on both input channels and rewrites the same 2x2 send matrix
  with the M/S encoder (`L' = (L + R) / 2`, `R' = (L - R) / 2`); software
  playback rewrites the matrix only. A mono input pair cannot enable it.
- One control spans the panel content width; two controls share it with one
  gap, so their combined width equals a single control. `stereo`, `48V`,
  `autoset`, `no trim`, `loopback` and a phase pair therefore all end at the
  same right edge.
- Knobs keep one fixed column at the right: gain, width, send FX and return
  FX share a vertical centre line. `send`/`FX` and `return`/`FX` break over
  two lines to keep the label column narrow.
- Button labels and dropdown values are centred. Every button and dropdown
  shares one control height (24 px at scale 1).
- A standalone label such as `ref.` is centred and sits at the bottom of its
  row, directly above the control it names. It is used only where the panel
  has a reference dropdown.
- A reference dropdown reserves the handle's width, so its label clips at
  the sides and the arrow stays visible.
- Input reference values are `-10 dBV`, `+4 dBu`, `Lo Gain`; output
  reference values are `-10 dBV`, `+4 dBu`, `Hi Gain`. Reference levels
  follow register groups: the three line-output pairs share one setting and
  the Phones/Main pair has its own.
- The Talkback row is empty on Control Room channels.
- The Send FX knob mirrors the thin FX fader on the strip; the Return FX knob
  mirrors the thin fader on outputs. One value, two views.
- Every slideout fits the strip height at normal size: everything is visible
  at a glance and the panel does not scroll.
- The UCX has no PAD.

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

Phase is available on every input channel at the bottom of the Settings
panel, and on hardware outputs.  The label is the symbol `Ø`, never the word:
a linked pair shows `Ø L` and `R Ø`, a single channel shows `Ø`.  Engaged
buttons use green text and a green outline.

### 3.2 Gain

Gain is a knob in the Settings panel for the four front inputs. Inputs 1/2
use the microphone range; inputs 3/4 use the instrument/line range, in 0.5 dB
steps on the UCX. The knob writes the per-channel preamp gain and follows the
stereo link. At connect the saved scene gain is written; a state word that
arrives before the scene is applied does not override it. On the UCX the knob
then follows physical encoder moves, which the unit reports in the IF1 state
stream.

A linked input pair always shows both values, left above right, even when
they are equal. This mirrors the device, which always treats inputs 1–4 as
unlinked. An unlinked input shows one value.

### 3.3 Device sheet

The device card at the top of the sidebar (or the gear on the folded rail)
opens the device sheet. It starts with the group the unit itself owns. Where
the unit has no readback, the saved scene value is written at connect and the
sheet shows that value. A value is written when it changes there.

| Row | UC | UCX |
|---|---|---|
| Clock source | yes | yes |
| Sample rate | yes | yes, when the device owns the rate |
| Line In reference | yes | — |
| Line Out reference | yes | — |
| Phones reference | yes | — |
| Optical | yes | yes |
| SPDIF | yes | yes |
| Single speed word clock | yes | yes |
| Word clock in | — | yes |
| EQ for Record | — | yes |
| Firmware | yes | yes |
| Serial | yes | yes |

On/off options are checkboxes after their caption. `Optical` and `SPDIF` are
dropdowns (`ADAT`/`SPDIF`, `Consumer`/`Professional`). Every caption and
control shares one left edge, and every dropdown has the same width.

The options rows belong to the unit's own group. The UC offers five clock
sources, including LTC; the UCX offers four: Internal, SPDIF coax., Optical
and Word. Host-side modes (`MS Proc`, `AN 1>2`, `Input Link`) appear in the
GLOBAL grid, apart from the device-owned rows.

On the UCX the options word, the reference levels and the 48 V/Inst/Level
switches have no readback. The saved scene value is written at connect and the
sheet shows that value. Input gain is also written from the saved scene at
connect; the Gain knob follows later encoder moves.

The UC reference rows are the unit's global options word. The UCX handles
input and output sensitivity per channel instead, in per-channel registers
(`NN08` on inputs 3/4, `0x0407`–`0x0707` for inputs 5–8, `NN08` on the output
pairs, `0x1808/0x1908` for Phones). Where one control drives a group, it
writes each channel's own register. Rows a unit has no register for are absent
rather than shown inert.

Below the device group, the host engine group shows the PipeWire rate,
quantum and latency when the sound server owns the rate. `Quantum` is the
graph quantum, one ALSA period; the `Latency` line shows the resulting ring,
USB queue and device lead. It also shows the Fireface sink's host volume in
percent and dB, with a `0 dB` action. The host volume is an attenuation before
the interface, so it lowers the playback meters by the same amount; the
interface's own faders remain the monitoring level.

`Errors` lists the driver transport faults since the sheet opened or the
last `reset`, one row per type: `usb` (URB status errors), `urb` (URB resubmit
errors), `gaps` (the USB queue ran dry), `short` (short packets), `recover`
(engine recoveries) and `events` (dropped driver events). `usb`, `gaps` and
`short` show input/output as `in / out`; `urb`, `recover` and `events` are
single values. A non-zero row is a fault; the section is absent when the
backend does not expose counters.

## 5. Equalizer panel

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

## 6. Dynamics panel

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

## 7. FX panel

The FX panel is a column between the mixer rows and the right sidebar,
toggled by the `FX` knob in that sidebar and shown only on a backend with
the FX bus.  The knob lights while the column is out.
Reverb sits above Echo in one 180 px column.

The column opens and closes with the same constant-speed width tween as the
strip panels. It first reduces the mixer area until the widest row reaches its
natural width. If that still leaves no room for the column, it slides over
the rightmost part of the rows instead of forcing them to scroll further.
The overlap grows continuously as the window narrows; an open slideout keeps
its position and is covered from the right. Widening the window moves the
column back into the layout; closing FX removes it.

| Band | Height |
|---|---:|
| Header (on switch + effect name) | 36 |
| Type row | 31.5 |
| Reverb controls | 247.5 |
| Reverb type-specific band | 98 |
| Echo controls | 343 |

Knob rows are 78.75 px apart and knobs are 49.5 px across. Reverb's volume
fader occupies the right-hand column across the first two knob rows. Echo's
volume is a knob in the second column; its high-cut picker sits below it. The
Echo panel's right-hand column carries the `FX` `in`/`out` meter pair, fed by
the UCX FX bus power channels.

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

The four knobs share one vertical centre line. The delay knob shows the
delay time inside it; the BPM readout sits at its underside/right without
moving the knob. `HC` sits left of a compact high-cut box; the box is only
as wide as its value, and the value text is centred. The FX `in`/`out` meters
sit to the right of the knob column, wider than a strip meter, and show an
overload segment at the top when the peak reaches full scale.

| Control | Range |
|---|---|
| On | on/off |
| Type | Stereo Echo, Stereo Cross, Pong Echo |
| Delay time | 10..2000 ms; second readout 30..300 BPM |
| Feedback | 0..100 % |
| High cutoff | Off, 16k, 12k, 8k, 4k, 2k |
| Width | 0..100 |
| Volume | −30..+6 dB knob |

BPM is derived from the delay: it reads `60000 / delay` and writes the delay
register, `60000 / bpm`.

### 6.3 FX Return

Each hardware output has an **FX Return** knob in its Settings panel,
−65..0 dB. A linked pair writes both channels.

## 8. Output balance

Hardware output strips include a balance knob, −100..+100. The linked fader
is the louder channel (the pair level). On the UCX the knob writes the output
balance register, independent of the pair volume. Splitting converts the pair
level and balance into the two channel volumes; linking recomputes both from
those volumes. A split/re-link round trip therefore preserves the pair level
and balance. A split pair keeps two independent volumes and hides the balance
knob.

## 9. Pan and balance

A split input or software-playback strip exposes pan into the selected
submix; the knob writes the source's matrix cells for that submix. On a
linked pair the same knob is a balance derived from the two channel volumes,
and writing it changes the quieter channel's volume instead of a pan.

### 8.1 Signal model and submixes

Each hardware output is a bus. The selected output is the current submix;
every input and software-playback strip carries an independent send gain and
pan to that bus. The input/playback fader is the send level to the current
submix, not a single channel fader. The output strip's fader is the bus
master.

| Term | Normal mixing desk | TotalMix FX | TotalControl |
|---|---|---|---|
| Input gain / trim | Preamp trim before the channel fader and meter | Hardware preamp gain; the T button is Trim Gain mode, not a separate gain stage | Trim pre-gain, common to every send of the channel |
| Channel fader | One per channel, feeding the bus assignment | None | None |
| Send fader | Aux send, usually pre- or post-channel fader | Per-routing send gain; the fader edits the selected output | Per-submix send gain; the fader edits the selected output |
| Bus summing | Group, aux or matrix bus | Submix for one hardware output | Submix for one hardware output |
| Bus master | Group or aux master fader | Output strip fader | Output strip fader |
| Bus to output | Patch or assignment; one bus can feed several outputs | One bus per hardware output | One bus per hardware output |
| Input VU tap | After input gain/trim, before the channel fader | After preamp gain and optional Post FX, before the send fader; T does not change it | After trim, before the send fader |
| Output VU tap | Post-fader bus or output | Post-fader output bus | Post-fader output bus |

In TotalMix, T is the **Trim Gain** mode: it gangs the sends of one channel.
It is not a pre-gain and does not move the input VU. TotalMix therefore has
an input VU after the preamp and optional FX, not “before trim”.

In TotalControl, T is a true pre-fader trim. The send fader displays the post-trim
level, so its maximum is the trim value at 0 dB send. An analog desk changes
the meter and signal with trim but leaves the channel fader in place; TotalControl
moves the send fader so the post-trim level is visible.

For an analog input, the signal paths are:

```text
Normal desk:
input -> input stage / sensitivity -> preamp / trim -> channel VU
      -> channel fader -> pan -> bus assignment -> bus sum
      -> bus master -> output VU -> output

TotalMix FX:
input -> input stage / reference -> preamp gain (where present) -> ADC
      -> input VU (post-preamp; Post FX optional)
      -> send fader for the selected output -> bus sum
      -> output fader -> output VU -> DAC

TotalControl:
input -> input stage / reference -> preamp gain (where present) -> ADC
      -> trim -> input VU -> send fader for the selected submix
      -> bus sum -> output fader -> output VU -> DAC
```

The preamp gain is present on the UCX analog inputs 1–4. Digital inputs enter
after the preamp stage. In TotalMix, T is a send-fader mode, not a gain stage.
In TotalControl, trim is a host-side pre-fader gain after the ADC.

Control Room destinations (Main, Speaker B, Phones) are ordinary output buses
with their assignment controls. Cue/PFL to selects which monitoring output
carries the Cue signal; it is not a channel.

## 10. Motion

All strip panels open and close with a width tween at a constant 1 px/ms;
a wider panel takes longer instead of sliding faster:

- Settings, EQ, Dynamics and Trim grow and shrink beside their strip;
- the FX column grows and shrinks beside the mixer, or over its right edge
  when the window is too narrow for both;
- Route grows and shrinks as a raised picker;
- Quick view uses the same tween;
- switching Settings, EQ, Dynamics or Trim on one strip tweens directly from
  the current width to the new width.

Toggling Stereo tweens the second half of the pair between zero and full width
at the same speed. Channels to the right reflow with the tween.

Assignment add, clear and replace move at the same 1 px/ms speed. A
destination change that keeps the Control Room channel count unchanged does
not animate.

## 11. Stereo link

Linking an input pair uses the left channel as the reference for mute, solo,
phantom, Inst, Level, reference level and AutoSet. It also copies the left
channel's FX send, low cut, EQ, dynamics and AutoLevel settings to the
right. The linked gain knob moves both preamps by the same step and preserves
their offset; the knob shows both values. Phase remains per-channel.

Linking does not copy or flatten the two channel volumes: the linked fader
displays the louder channel, and moving it shifts both channels by the same
dB offset so their difference is preserved.

The linked balance is derived from the two channel volumes:

```text
balance = round((right - left) / max(left, right) * 100)
```

The knob sets the quieter side's volume and leaves the louder side at the
pair level. Splitting sets the two pans hard left and hard right; split pan
changes do not alter the balance because the balance lives in the channel
volumes. The same rule applies to software-playback and hardware-output
pairs.

Software-playback pairs link as a host grouping. Mute and solo copy from left
to right; FX send remains per-channel and MS processing stays a pair control.

Hardware-output pairs link with the output stereo words. Mute, FX return,
low cut, EQ, dynamics and AutoLevel copy from left to right. The two channel
volumes encode the pair level and balance. The output reference level is a
line-output group value, not a per-pair link value. Phase L and Phase R keep
independent states across every transition. A pair assigned to the Control
Room is always linked and never splits.

## 12. Control Room

The Control Room panel is flush with the mixer's right edge beside the FX
column. Its button stack is one channel wide; every button in the stack has
the same width and height, and its label is centred. Long labels use the next
smaller type tier. The stack runs, top to bottom:

| Group | Controls |
|---|---|
| dim / recall / speaker B | Dim, Recall, Speaker B |
| mono / talkback / ext. input / mute FX | Mono, Talkback, Ext. input, Mute FX |
| bottom | Settings, immediately above Assign |

Engaged monitoring buttons use the dark face, orange text and 1 px orange
outline. Recall has no latched state and uses the normal press feedback;
Settings stays lit while its dialog is open. `assign` is a selector, not a
monitoring button: it uses the same compact row and white drop decoration as a
strip's output-bus trigger, and its row stays lit while the menu is open.

Recall acts on the assigned Main Out pair. Dim attenuates the assigned Main
Out and Speaker B pairs; the Phones outputs are not affected. Without an
assigned Main Out both controls leave the mix unchanged and never fall back
to a Phones pair.

Dim saves the current Main Out and Speaker B levels, lowers both speaker
pairs by the configured Dim amount and engages the device Dim state. Turning
Dim off restores the saved levels. A speaker fader move while Dim is engaged
changes the audible level but not the saved pre-Dim levels; turning Dim off
returns to the saved levels. Dim attenuates both speaker pairs, Main Out and
Speaker B, and does not touch the Phones outputs. Changing the Main Dim
amount while Dim is engaged re-applies the new amount to the saved levels.
Reassigning or clearing the Main Out or Speaker B releases Dim first.

Recall sets the underlying Main Out level to the configured listening volume
once; it does not latch. When Dim is engaged, Recall updates the saved pre-Dim
levels and the current dimmed levels together.

Dim and Speaker B are runtime monitoring states. Applying a scene,
reconnecting, or a stream-start or sample-rate reapply leaves Dim off and does
not attenuate the Main Out again; Speaker B keeps its current monitor path.
The saved scene contains the level that was audible, not a pending Dim
operation. Speaker B requires assigned Main and
Speaker B outputs; its button switches the monitor routing between those
pairs, and clearing the Speaker B assignment also clears Speaker B. The
`Link to Main` setting makes Speaker B follow Main fader and Recall changes;
Dim attenuates both speaker pairs whether linked or not, while the Phones
outputs are unaffected.

Mono is a latching monitor check: it mixes the assigned Main Out and Speaker
B pairs to mono and feeds the same signal to both sides of each pair. It does
not mix the Phones outputs. Mono is stored in the device scene.

Mute FX is a latching scene control. It saves the FX-return levels of the
assigned Main Out and Speaker B pairs, writes `-65 dB`, and restores the
exact saved levels; it does not switch the Reverb or Echo processors off.
Phones is not modified.

Ext. input is a latching runtime control. It replaces the Main Out and
Speaker B submixes with the stereo hardware-input pair selected in Control
Room Settings at the configured gain, mutes those pairs' FX returns, and
sets monitor flag bit 5. Releasing restores the normal submix and the saved
FX returns. The button is disabled until a source pair is configured.
Changing the source or gain while active re-engages the path.

With `PFL replaces Solo`, each input strip's Solo button is labelled `PFL`
and routes the input pre-fader at unity to the Cue/PFL target, with the
target's other sources and FX returns suppressed. The global Solo master
shows `F` while a PFL is active and `S` otherwise; it turns active PFL inputs
off and can restore them. `Exclusive Solo/PFL` keeps one input pair active.

Control Room channels are always stereo. An assigned pair is linked and
cannot be unlinked, so the Settings panel's `STEREO` control stays engaged
and does nothing; a split pair is linked when it is assigned. `Set all
channels mono` leaves the assigned pairs linked.

### 11.1 Assign

`assign` opens a two-level menu. The first level lists the monitoring
destinations in this order, using the same names as the Control Room channels:

| Destination | Device |
|---|---|
| Main | output mode word `0xNN02` bit 1 on the chosen pair |
| Speaker B | the same bit, plus control-room state word `0x3d00 = 1` while active |
| Phones 1..4 | host-side; the unit receives nothing |
| Cue/PFL to | host-side; selects the Cue/PFL monitoring target |

The second level for an output destination lists the predetermined hardware
channel names—`None` first, then every hardware output in display order, the
current output in bold. It never shows user names. Choosing `None` clears the
destination. Both levels use the same button width, and every label is
centred.

Clicking a first-level destination opens and pins its second level. Hovering
an unpinned first-level row opens or switches the second level without
pinning it. The second level closes only when an item is chosen, a click lands
outside the menu, or `assign` is clicked.

`Cue/PFL to` is the seventh entry and stays in the Assign list. Its sublist is
`Main`, `Phones 1`, `Phones 2`, `Phones 3`; it has no `None`; the current
target is bold. Choosing a target changes only where Cue/PFL is monitored; the
same target controls PFL monitoring. It assigns no hardware output, moves no
channel and creates no Control Room slot. The target is host-side and stored
with the GUI preferences.

The menu opens immediately left of the bottom `assign` button and covers the
section's channels while it is open.

Control Room slots run left to right: `Phones 1`, `Phones 2`, `Phones 3`,
`Phones 4`, `Speaker B`, `Main`. `Main` is beside the buttons. There are no
empty slots: every hardware output is either assigned in the Control Room or
unassigned in the Hardware Outputs row.

Control Room channel names are predetermined and are exactly the first six
Assign names: `Main`, `Speaker B`, `Phones 1`–`Phones 4`. View → Names does
not change them. They are not user editable; clicking the name selects the
channel but never opens the rename field. The Assign menu uses those names in
its first level and the predetermined hardware names in its second level.

Assigning a pair moves its channel out of the Hardware Outputs row into the
Control Room section, named for its destination, and the panel widens to hold
it. Clearing the destination returns the channel to the Hardware Outputs row
in its original position. Changing an output from one destination to another
keeps the fixed slot order: when the Control Room count does not change, the
strip takes its new destination slot and its name changes without animation.

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
it is selected as current submix"). The Main output is a submix like any
other; FX Send can follow it.

Every click on a hardware output counts, not only the strip's empty surface:
its mute and cue switches, its slideout buttons (gear, EQ, D), its fader and
its name field select that output the same way, and the visible submix
switches to it. An output strip that is already part of a multi-selection
keeps the selection when one of its controls is clicked, so group edits still
apply.

### 11.2 Assignment motion

An assignment change is one combined flow. Every affected strip, slot and row
gap starts in the same frame and moves at the same constant 1 px/ms as the
other panel transitions. Parts with different travel distances finish at
different times; there is no separate Control Room timing.

Adding an assignment slides only within each row; the channel does not
travel between the Hardware Outputs row and the Control Room.

- The output strip leaves the Hardware Outputs row, and the strips to its
  right slide left by the vacated width.
- The Control Room grows by one slot; the new slot takes its place in the
  fixed order; the slots to its left slide left; the slots to its right stay.
- The channel appears in its new Control Room slot.

Clearing an assignment reverses the row-internal motion: the Control Room
slots to the left of the removed slot slide right, the panel shrinks, and the
strips to the right of the channel's original row position slide right to open
the gap. The returning channel fills that gap from its right edge as the gap
opens; it is revealed progressively rather than appearing at full width.

Replacing an occupied destination, or changing an output from one destination
to another, keeps the Control Room channel count unchanged and therefore does
not animate. The fixed slot order is recomputed immediately; the channel and
its name appear in the new destination slot.

A split output pair moves as one dual channel. If a channel has an open
slideout, the slideout travels with it and stays open. The Hardware Outputs
row makes room for the strip plus the slideout; when there is not enough room,
the rightmost channels pass under the Control Room, which always remains
visible. At the destination the usual slideout rule applies: it opens to the
right, and the Control Room shifts its own channels left by one slot.

The moving channel is drawn once, above the rows and panel, and accepts no
pointer input until it settles. No motion runs when assignments are restored
from preferences at startup.

The Main and Speaker B assignments are application configuration stored with
the GUI preferences. A scene load or reapply keeps the live assignment; a
scene's stored assignment does not move the Control Room destinations.

The front-panel volume knob follows the output selected on the unit's display,
not the Control Room Main assignment; `PH` always addresses channels 7/8.

### 11.3 Control Room Settings

`settings` opens a modal dialog titled `CONTROL ROOM SETTINGS`. It edits a
draft; the stored values change only when `OK` is pressed. `Cancel`, `Escape`
and a click outside the dialog discard the draft.

| Section | Controls |
|---|---|
| Talkback | Input, Dim |
| Listenback | Input, Dim |
| Main Out | Recall, Dim, External Input, Gain, Speaker B link |
| Solo / PFL Mode | Live Mode, PFL replaces Solo; Exclusive Solo/PFL Mode |

The input lists start with `None`, then the mono hardware inputs by their
device names. External Input lists stereo hardware-input pairs. The five
level controls have a `-65.0 … 0.0 dB` range, a `0.5 dB` step and a
one-decimal value display.

`OK` stores the settings in `gui.json` for the selected device. The settings
are not part of the mixer scene, snapshot, undo stack or OSC scene state.
`Dim`, `Speaker B`, External Input and the PFL listening set remain runtime
state. `Mono` and `Mute FX` are mixer scene state. External Input and the
Solo/PFL modes are stored as settings and applied to the running device on
`OK`.

## 13. Latency display

The Settings panel shows the application/ALSA latency budget and its terms:
the host ring, USB queue, device lead and converter. The buffer-size picker
sets the graph quantum. The formulas and the device-side model are in
[`TOTALCONTROL-ARCHITECTURE.md`](../TOTALCONTROL-ARCHITECTURE.md) §10.

The displayed line is an estimate, not a measured acoustic round trip. The
panel labels it accordingly and keeps the four terms visible.

## 14. State references

Scene contents, snapshot storage, startup mismatch, undo and the split between
mixer state and GUI preferences are defined in
[`TOTALCONTROL-ARCHITECTURE.md`](../TOTALCONTROL-ARCHITECTURE.md) §9. This
specification only names the visible surfaces: eight snapshot slots, six
layout slots, the current auto-saved mix and the GUI preference file.

The startup mismatch dialog lists the decoded differences as a bullet list.
On the UCX it appears after the asynchronous pre-connect dump has been
compared; the window itself is shown first. More than six differences are
shown in a fixed-height scrollable list box instead of letting the dialog grow
beyond the window.

## 15. Control invariants

- Every channel control has a defined effect in the mixer model and its device
  path.
- Engaged controls use their defined style: orange outline for Stereo, Inst,
  AutoSet, FX Return and Loopback; green outline for Phase; orange arc for
  gain, pan and output balance; solo colour for Dynamics.
- Settings, EQ, Dynamics, Trim and Route panels use the same height as their
  strip.
- Panel and row transitions use the constant-speed width tween.
- The right sidebar remains visible while expanded, at every window width.
- The three channel rows scroll together under one horizontal slider.
- Phase, pan, output balance, dynamics, FX and loopback survive scene recall.
- Output balance survives output-fader movement.
- Trim is a pre-fader gain that attenuates every send of a channel. It adds
  no device register; the value is applied when each send is written. `T` is
  the first icon in the strip's column on hardware inputs and software
  playbacks; it lights while trim is below 0 dB, and its tooltip states the
  applied value.
- The fader displays the post-trim level: `displayed = send × trim`. With
  the send limited to 0 dB, the fader's maximum is the trim value, and
  moving trim moves the fader cap with it. The send written to the device is
  `displayed / trim`, so the audible level remains the fader value.
- Input and playback meters are post-trim: a constant input tone falls and
  rises with the trim value. Output meters remain post-fader.
- Width is host-side and has no width register: it rewrites the pair's
  matrix cells. Playback stereo/mono, No Trim and Talkback receive are
  host-side and do not produce device writes. Mono writes the monitor flags
  `0x3f9e` and `0x3f98`.
