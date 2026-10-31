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

## 3. Settings panel

The panel follows the channel type.

The layout is one vertical stack per channel type. Inputs share a nine-row
grid; a control a channel lacks leaves its row empty, so the rows below never
move and `send FX` and the phase pair sit at the same height on every input.
Empty rows keep the height of the control the grid reserves for them.

| Channel | Contents, top to bottom |
|---|---|
| Input 1/2 | Stereo, space, 48V, AutoSet, Gain, Width, Send FX, MS Proc, Ø L / R Ø |
| Input 3/4 | Stereo, `ref.`, Inst + reference dropdown, AutoSet, Gain, Width, Send FX, MS Proc, Ø L / R Ø |
| Inputs 5–8 | Stereo, `ref.`, reference dropdown, two empty rows, Width, Send FX, MS Proc, Ø L / R Ø |
| Inputs 9+ | Stereo, four empty rows, Width, Send FX, MS Proc, Ø L / R Ø |
| Software playback | Stereo, Width, Send FX, MS Proc, phase slot |
| Hardware output | Stereo, `ref.`, reference dropdown, Return FX, Ø L / R Ø, No Trim, space, Talkback, Loopback |

- 48 V is available on inputs 1/2; AutoSet on inputs 1–4; Inst and a gain
  knob on 3/4; a reference dropdown on 3–8; none from channel 9 on.
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
  gap, so a single button and a phase pair end at the same right edge.
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
stereo link. On the UCX it also follows physical encoder moves, which the unit
reports in the IF1 state stream.

### 3.3 Device sheet

The device card at the top of the sidebar (or the gear on the folded rail)
opens the device sheet. It starts with the group the unit itself owns, read at
connect where the unit has readback and otherwise kept in the saved scene.
A value is written only when it changes there.

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
switches are held by the unit: they are not read back and not written at
connect. The sheet shows the last value written from TuxMix. Input gain is
read back from the unit and the Gain knob follows it.

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
−65..+6 dB. A linked pair writes both channels.

## 7. Output balance

Hardware output strips include a balance knob, −100..+100. The linked fader
is the louder channel (the pair level). On the UCX the knob writes the output
balance register, independent of the pair volume. Splitting converts the pair
level and balance into the two channel volumes; linking recomputes both from
those volumes. A split/re-link round trip therefore preserves the pair level
and balance. A split pair keeps two independent volumes and hides the balance
knob.

## 8. Pan and balance

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

| Term | Normal mixing desk | TotalMix FX | TuxMix |
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

In TuxMix, T is a true pre-fader trim. The send fader displays the post-trim
level, so its maximum is the trim value at 0 dB send. An analog desk changes
the meter and signal with trim but leaves the channel fader in place; TuxMix
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

TuxMix:
input -> input stage / reference -> preamp gain (where present) -> ADC
      -> trim -> input VU -> send fader for the selected submix
      -> bus sum -> output fader -> output VU -> DAC
```

The preamp gain is present on the UCX analog inputs 1–4. Digital inputs enter
after the preamp stage. In TotalMix, T is a send-fader mode, not a gain stage.
In TuxMix, trim is a host-side pre-fader gain after the ADC.

Control Room destinations (Main, Speaker B, Phones, Cue) are ordinary output
buses with their assignment controls.

## 9. Motion

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

## 10. Stereo link

Linking an input pair uses the left channel as the reference for mute, solo,
phantom, Inst, Level, reference level and AutoSet. It also copies the left
channel's FX send, low cut, EQ, dynamics and AutoLevel settings to the
right. Gain and phase remain per-channel: the linked gain control uses the
left value while the right value is retained.

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
independent states across every transition.

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

For the UCX the ring must also cover the output lead plus two URBs of
margin, rounded to a whole URB:

```text
ucx_floor = round_up(max((nurbs + 2) × frames_per_urb,
                         (nurbs + 1) × frames_per_urb + 60
                         + 2 × frames_per_urb),
                     frames_per_urb)
ring = max(2 × quantum, ceil(ucx_floor / quantum) × quantum)
```

With the default 48-frame URB this gives 336 frames at 48 kHz and 576 frames
at 96 kHz. The old 192-frame floor was smaller than the 204-frame output
lead and underran at 96 and below.

For 44.1 kHz:

| Quantum | ALSA buffer | USB queue | Endpoint | Total |
|---|---|---|---|---|
| 48 | 4.4 ms | 2.2 ms | 1.1 ms | 7.6 ms |
| 64 | 4.4 ms | 2.2 ms | 1.1 ms | 7.6 ms |
| 96 | 4.4 ms | 2.2 ms | 1.1 ms | 7.6 ms |
| 128 | 5.8 ms | 2.2 ms | 1.1 ms | 9.1 ms |

On the UCX each URB carries up to eight 1 ms ISO packets of
`ceil(rate / 8000)` frames. The queue is `nurbs + 1` URBs and the unit adds a
60-frame output lead. With the default `urb_frames = 48` and `nurbs = 2`:

| Rate | Frames/URB | USB queue | Device lead | Device side |
|---|---|---|---|---|
| 48 kHz | 48 | 144 (3.0 ms) | 60 (1.25 ms) | 204 frames (4.25 ms) |
| 96 kHz | 96 | 288 (3.0 ms) | 60 (0.63 ms) | 348 frames (3.63 ms) |
| 192 kHz | 192 | 576 (3.0 ms) | 60 (0.31 ms) | 636 frames (3.31 ms) |

The idle ring estimate is the formula above. The UC uses
`max(2 × quantum, ceil((nurbs + 2) × frames_per_urb / quantum) × quantum)`;
the UCX also includes the output lead and its two-URB margin.

The Settings **Latency** line shows the card ring buffer and, on the Fireface
models, the USB queue and device lead (the UC reports the queued URBs as an
endpoint; the UCX adds the 60-frame lead). The **Buffer size** picker shows
the graph quantum.

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

The UCX mixer scene is host-owned and applied at connect. Reverb, Echo,
dynamics and AutoLevel are host-owned. The options word, reference levels and
the 48 V/Inst/Level switches are unit-owned: the saved scene keeps the last
value written from TuxMix, and they are neither read nor written at connect.
Input gain and output volume are read from the IF1 state stream.

TuxMix restores its remembered engine rate on startup and reconnect. The UC
sample rate follows the host stream; `Auto` follows the device or graph rate.

The matrix is written before PCM opens. Loopback is written live.

### 13.1 Startup mismatch

At connect the backend drains the readable state, compares it with the saved
scene, and shows a modal when they differ. `Always use TuxMix settings` is a
preference that suppresses the comparison.

| Model | Compared | Not compared |
|---|---|---|
| UC | clock source, optical format, SPDIF professional, AN1/2 48 V, AN3/4 PAD, AN3/4 Inst, the input gains the front panel reported, the output volumes the front panel reported | matrix, masters, loopback, mute |
| UCX | the input gains and output volumes the IF1 state stream reported | options word, reference levels, 48 V/Inst/Level, PAD, matrix, faders, pans, EQ, dynamics, FX, loopback |

The UCX comparison therefore covers only the channels the state stream has
reported; a channel that has not moved can be absent.

The scene is applied before the modal is shown. The choices then correct the
readable values:

| Choice | UC | UCX |
|---|---|---|
| Use TuxMix settings | write the scene's clock, optical, SPDIF, 48 V, PAD, Inst and input gains; output volumes were already applied with the scene | write the scene's input gains; output volumes and mixer were already applied with the scene |
| Use device settings | write the device's readable clock, optical, SPDIF, 48 V, PAD, Inst, gain and output-volume values back; matrix, faders and loopback stay at the TuxMix scene | write the device's reported gains and output volumes back; options, references, 48 V/Inst/Level and the mixer stay at the TuxMix scene |
| Always use TuxMix settings | as Use TuxMix settings, and stop asking | as Use TuxMix settings, and stop asking |

The dialog never writes a value that was not compared. In particular, the UCX
options word, reference levels and 48 V/Inst/Level are not restored by
`Use device settings`; they remain at the TuxMix scene for display and are
unchanged on the unit.

The connect write is host → device for the mixer group on both models:
matrix, faders, pans, sends, EQ, dynamics, FX, loopback and output volume.
`Use TuxMix settings` adds the scene's input gain host → device.
`Use device settings` is the only device → host direction: it copies the
compared values from the device into the scene.

Not read back, so never device → host and never part of the mismatch: the UC
matrix, masters, loopback and mute; the UCX matrix, faders, pans, EQ,
dynamics, FX and loopback. They are still written host → device from the
scene at connect.

The UCX options word, reference levels and 48 V/Inst/Level switches are
unit-owned: not written at connect, not read back. Only the last value written
from TuxMix is kept in the scene for display.

### 13.2 Undo and Redo

Undo stores the scene before a mix-changing command. A continuous gesture on
one control is one step: a fader, pan, width or EQ-band drag coalesces until
the control is released, another control is used, or the edit pauses. Undo
restores the previous scene; Redo reapplies the undone scene. Device settings
and layout preferences are not captured by undo.

### 13.3 Snapshots, layouts and workspaces

TuxMix persists state in three separate layers:

| Layer | Storage | Contents |
|---|---|---|
| Snapshot | `Mix <n>.json` in the scene directory | A full `Scene` |
| Layout | `Layout <n>.json` in the layout directory | The collapsed-strip set |
| GUI state | `gui.json` in the config directory | Window, sidebar, row mode, zoom, labels, open panels, selected output and link groupings |

The current mix is `auto.json` in the scene directory.

A snapshot is a host-side file. The device stores only its current mixer
state; recalling a snapshot applies the saved scene to the device. TuxMix has
eight snapshot slots. A snapshot is a full `Scene`: mixer and routing state,
channel names, trim, link state and the saved device group. It is broader than
RME's mixer-only snapshot.

A layout has six slots and stores only which strips are collapsed. It does not
store channel visibility, remote exclusions or names.

Groups exist only at runtime. They are not written to the scene, a layout, the
GUI preferences or any other file, so restarting TuxMix clears them.

TuxMix has no Workspace file. A Workspace is the RME container for the current
mix, all eight snapshots, all layout presets, group definitions and GUI state.
TuxMix keeps those in separate files, with group state missing.

RME's corresponding scopes are:

| RME concept | Scope |
|---|---|
| Snapshot | Mixer and routing state only; no names, visibility, layout or GUI state |
| Layout Preset | Channel visibility and MIDI/OSC remote exclusions; stored in a Workspace |
| Workspace | Complete session: current mix, snapshots, Layout Presets, groups, names and GUI state |

## 14. Control invariants

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
  matrix cells. Playback stereo/mono, Mono, No Trim and Talkback receive are
  host-side and do not produce device writes.
