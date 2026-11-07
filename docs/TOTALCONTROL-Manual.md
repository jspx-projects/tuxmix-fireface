# TotalControl Manual

TotalControl is a control surface for RME Fireface interfaces on Linux. It
replaces the vendor's mix application on Linux: routing, gains, EQ, dynamics,
FX and snapshots, driven through the kernel driver.

This manual is written for the end user. It describes installation, setup
and use. The developer specifications are in [`spec/`](spec/README.md); the
runtime architecture is in
[`TOTALCONTROL-ARCHITECTURE.md`](TOTALCONTROL-ARCHITECTURE.md).

Only the kernel driver plus hwdep path is supported for users. The legacy
ALSA backends, direct USB and capture/probe tools are development tools; see
[`DEVELOPMENT-TOOLS.md`](DEVELOPMENT-TOOLS.md).

Panels and controls appear only where the connected unit has them. Examples
in this manual assume a Fireface UCX, which has the full set: EQ, Dynamics,
FX and the Control Room. On a Fireface UC the same layout applies with the
reduced control set.

## Contents

- [Setup](#setup)
- [1. The mixer window](#1-the-mixer-window)
- [2. Views](#2-views)
- [3. Channel strips](#3-channel-strips)
- [4. How the mixer works: submixes](#4-how-the-mixer-works-submixes)
- [5. Working in stereo](#5-working-in-stereo)
- [6. The slideout panels](#6-the-slideout-panels)
- [7. Equalizer](#7-equalizer)
- [8. Dynamics](#8-dynamics)
- [9. FX: sends, reverb and echo](#9-fx-sends-reverb-and-echo)
- [10. Control Room](#10-control-room)
- [11. Gestures](#11-gestures)
- [12. Options menu](#12-options-menu)
- [13. Snapshots, layouts, undo and redo](#13-snapshots-layouts-undo-and-redo)
- [14. Latency](#14-latency)
- [15. Where TotalControl keeps its data](#15-where-totalcontrol-keeps-its-data)
- [16. Running in the background](#16-running-in-the-background)
- [17. Device settings at startup](#17-device-settings-at-startup)
- [18. The UCX front knob](#18-the-ucx-front-knob)
- [Appendix A — Device modes](#appendix-a--device-modes)
- [Appendix B — PipeWire](#appendix-b--pipewire)
- [Appendix C — OSC setup](#appendix-c--osc-setup)
- [Appendix D — Command-line options](#appendix-d--command-line-options)

## Setup

### Requirements

- A Linux system with ALSA and PipeWire.
- A supported Fireface in normal vendor mode.
- A kernel with matching headers and DKMS for the driver.

The supported devices are the Fireface UC and Fireface UCX. The Babyface Pro
FS backend is present but is not part of the supported installation path yet.

### Install the kernel driver

From the repository root:

```sh
./scripts/install-dkms.sh
```

The installer registers `snd-fireface-uc` with DKMS, builds it for the running
kernel and enables the load service. Keep the kernel headers installed so the
module is rebuilt after a kernel update.

Check the installation:

```sh
dkms status
modinfo -k "$(uname -r)" snd-fireface-uc
```

If the module is missing after a kernel update:

```sh
sudo dnf install kernel-devel-"$(uname -r)"
sudo dkms autoinstall -k "$(uname -r)"
sudo modprobe snd-fireface-uc
```

### Start TotalControl

Start the application from the desktop menu or from a terminal:

```sh
totalcontrol
```

The window opens on the Quick view. The Mixer and Matrix views are in the top
bar.

### Check the device

1. Open the device card in the sidebar.
2. Confirm the model, sample rate and buffer.
3. Confirm that the input meters move when a source is connected.

From a terminal, `aplay -l` lists the card and
`ls -l /dev/snd/hwC*D*` lists the hwdep nodes.

If no device appears, check that the unit is in normal vendor mode, that the
kernel module is loaded, and that another application is not holding the
device.

## 1. The mixer window

The mixer shows three rows of channel strips:

1. **Hardware Inputs** — the physical inputs of your interface.
2. **Software Playback** — the channels your applications play into.
3. **Hardware Outputs** — the physical outputs.

Every row carries an uppercase header. A **Control Room** section closes the
Hardware Outputs row; it holds your monitoring destinations (see
[section 10](#10-control-room)).

The right sidebar holds the device card (rate, buffer), Undo/Redo, global
mute and solo, the FX knob, group and snapshot controls and layout. It
stays visible as the window narrows and can be collapsed.

When the channels do not all fit, one slider at the lower right scrolls all
three rows together, so the routing picture stays aligned.

The device card in the sidebar reads `Internal, <sample rate>, <buffer>` and
opens the device sheet, which carries clock source, sample rate, the digital
input formats and firmware and serial information.

## 2. Views

TotalControl offers three views:

- **Quick**: one source into one destination — the fastest way to build a
  basic mix.
- **Mixer**: all strips on three rows, TotalMix-style.
- **Matrix**: the complete routing grid; double-click a cell to toggle it
  between off and unity.

These are also the tabs in the top bar, next to the `OPTIONS` menu.

## 3. Channel strips

A channel strip shows, left to right:

- the **channel name** (click to select the channel, double-click to rename);
- a **submix label box** on inputs and playback channels; it selects the bus
  being written (see [section 4](#4-how-the-mixer-works-submixes));
- **mute** and **solo** (outputs use mute and **cue**);
- a **level fader** with a meter beside it;

Mute silences a channel without moving its fader; unmuting restores the same
level. Solo silences the other inputs and software playbacks while the soloed
channel keeps its fader; releasing the last solo restores the others.
- a column of small buttons: `T` (trim), the settings gear, `EQ` and `D`
  (dynamics), when the unit provides them;
- a **thin FX fader** at the top of that column, when the unit has an FX bus.

Every strip has the same height. A collapse button in the readout row under
the fader folds a strip so only its channel name and meter remain, and the
meter grows to fill the space.

The fader value is shown in a dB readout under the fader, without a unit
suffix. Double-clicking that readout opens a numeric entry field.

Output strips also carry a **balance** knob that pans the output pair
between its two channels; it appears only while the pair is linked.

Each meter has two indicators, following TotalMix FX:

- the **RMS bar** has a deliberately slow time constant so it shows average
  loudness. It is green below -3 dB, yellow from -3 dB to 0 dB and red above
  0 dB;
- the **peak line** has zero attack so a single full-scale sample is
  visible; it is held for a fixed two seconds and then decays. Its colour
  follows the same green/yellow/red bands;
- **overload is the red OVR block** above the bar. It lights when a
  full-scale sample is detected.

Peak hold applies only to the peak line. The RMS bar is not held and keeps
following the RMS signal.

The meter scale runs from -60 dB at the bottom to +6 dB at the top, leaving
about 6 dB of headroom above 0 dB for the red band. A level below -60 dB
reads as silence and does not draw a peak line.

Input and playback meters are pre-fader and post-trim: they show the signal
after the channel trim but before its send fader, so moving a send or
changing the selected submix does not change them. Hardware output meters
are post-fader and show the finished bus.

The RMS bar is mathematically correct RMS: a full-scale sine reads about
-3 dB on it. TotalMix FX offers an optional **RMS +3 dB** display shift that
makes the RMS reading and the peak line both read 0 dB for a full-scale
sine; TotalControl does not apply that shift.

TotalControl uses a fixed two-second peak hold. TotalMix FX makes the hold
time (0.1–9.9 s) and the number of consecutive full-scale samples that
trigger OVR configurable; TotalControl uses the two-second hold and a
full-scale OVR block instead.

The meters show the level as it arrives at the interface. If the host
(PipeWire) volume is below 0 dB, a full-scale recording reaches the
interface that much lower and the playback meter reads correspondingly
low. The device sheet shows the host volume and has a `0 dB` button; set it
to unity and use the interface's own faders for monitoring. A linked pair
shows one meter bar per channel; each bar shows that channel's own signal,
so linking controls does not copy a signal from one channel to the other.

The meters are read from the interface stream independently of the rest of the
window. Moving a fader, opening a panel or scrolling does not interrupt or
freeze them. A short transient lights the peak line even when it falls
between two screen frames. When every meter has been quiet for a moment they
stop redrawing, so an idle mixer stays still; the next signal wakes them
immediately and the held peak still catches a transient shorter than a frame.

## 4. How the mixer works: submixes

The mixer is a set of buses, one per hardware output. Selecting an output
strip makes it the *current submix*. After that, every input and playback
fader edits the **send of that channel into the selected output**, and the
output's own fader is the master level of that bus.

This is the same model as TotalMix FX:

- The fader of an input or playback strip never changes what the channel
  sends to your recording or other outputs; it changes how loud that
  channel plays into the currently selected output.
- To send a channel to another output, select that output's strip and set
  the fader again. Every output remembers its own mix independently.
- The meter on an input or playback strip shows the channel after its trim
  and before the sends; the meter on an output shows the finished bus.

## 5. Working in stereo

Input and playback pairs can be linked. When linked:

- the fader shows the louder channel of the pair and moving it shifts both
  channels together, preserving their level difference;
- the pair's balance is visible and adjustable from one knob;
- the gain knob moves both preamps by the same step, preserving any offset
  between them, and shows both values;
- mute, solo, FX send, EQ, low cut and dynamics settings are shared from the
  left channel;
- panel buttons split into a **linked pair**: a phase pair shows `Ø L` and
  `R Ø`, for example.

Splitting a pair sets the two channels hard left and right for panning into
a submix and gives each channel its own fader. Re-linking restores the pair
level and balance you had before the split.

Output pairs behave the same way: the balance knob exists only while
linked, and splitting converts the pair level and balance into two
independent channel volumes. A pair assigned to the Control Room is always
stereo and never splits.

## 6. The slideout panels

Pressing a symbol button on a strip opens that channel's panel beside it.
The panel opens to the right of the strip and closes when you press the
same button again. There is no extra close button; the tabs never move.

### 6.1 Settings

The gear opens the Settings panel. Its rows follow the channel type, so
every channel of one type shows the same panel shape:

- Inputs 1/2: `stereo`, `48V`, `autoset`, `gain`, `width`, `send FX`,
  monitor processing (`MS proc`) and phase.
- Inputs 3/4: like above, with `inst` and the gain in the instrument/line
  range on the UCX, plus the level reference dropdown for these inputs.
- Inputs 5–8: `stereo`, the reference dropdown, `width`, `send FX`,
  `MS proc` and phase.
- Inputs from channel 9 on: `stereo`, `width`, `send FX`, `MS proc` and
  phase.
- Software playback: like the top inputs but without microphone features.
- Hardware outputs: `stereo`, the reference dropdown, `return FX`, phase,
  `no trim`, `talkback` receive and `loopback`.

Notes:

- **Gain** is the preamp gain of the four front inputs. At connect it is
  written from the saved scene. It follows the stereo link and follows the
  unit's physical encoder when one is turned on the hardware.
- **Width** narrows or widens the pair: `1.00` full stereo, `0.00` mono,
  `-1.00` swapped channels.
- **MS proc** encodes or decodes the pair as mid/side.
- **Phase** is the `Ø` button; engaged phase shows green.
- The **send FX** knob and the strip's thin FX fader are the same control;
  the **return FX** knob on outputs is the return level of the FX bus.
- The **reference dropdown** sets the input or output reference level. For
  inputs 5–8 the four channels move together; inputs 3/4 can be linked.
- `loopback` makes this output's signal available on its input channel, so
  recording software can capture what the output plays. `no trim` and
  `talkback` receive are host-side settings.

On/off controls sit at the left of their row. Engaged switches light orange
(`48V`, `stereo`, `autoset`, `inst`, and others) or green (phase).

### 6.2 EQ and Dynamics

The `EQ` and `D` buttons open the equalizer and the dynamics panel. See
[sections 7](#7-equalizer) and [8](#8-dynamics).

### 6.3 Trim

`T` on an input or playback strip is the pre-fader trim of the channel: it
attenuates the channel before all of its sends, whatever output it plays
into. The tab lights while trim is active, and its tooltip shows the value.

## 7. Equalizer

EQ is available on every hardware input and hardware output. The panel shows
the response plot with three coloured segments — red low, green mid, blue
high — built from the three band frequencies.

Three bands, each with gain, frequency and width:

| Parameter | Range |
|---|---|
| Gain | ±20 dB |
| Frequency | 20 Hz – 20 kHz |
| Width (Q) | 0.7–5.0 |

- Band 1 and band 3 can be a bell or a shelf; band 2 is always a bell; band
  3 can instead be a low pass (hi cut).
- The gain knob of each band carries the colour of its segment.
- Every band keeps its frequency even when the band is off, so the plot
  never jumps.

The low-cut row holds the low-cut on/off button, its frequency knob and the
slope buttons (6/12/18/24 dB per octave). Low cut is independent of the main
EQ; you can edit it while it is off, and it is only heard when enabled.

Double-clicking an EQ handle on the plot resets that handle.

## 8. Dynamics

`D` opens the dynamics panel, on hardware inputs and hardware outputs. The
graph is an input-to-output level curve: green where the expander works,
blue in the neutral range, red where the compressor limit applies.

The panel has:

- an on/off switch at the top;
- shared **gain**, **attack** (0–200 ms) and **release** (100–999 ms) used
  by both compressor and expander;
- a **compressor** column: threshold −60…0 dB, ratio 1:1…10:1 (red);
- an **expander** column: threshold −99…−30 dB, ratio 1:1…10:1 (green);
- an **auto level** row: on/off, maximum gain 0–18 dB, headroom 3–12 dB and
  rise time 0.1–9.9 s.

A change to any knob updates the whole dynamics block of that channel.

## 9. FX: sends, reverb and echo

The `FX` knob in the right sidebar opens the FX column, when the unit has an
FX bus. It lights while the column is out. Every input and playback strip can
send to it:

- the thin fader at the top of the strip (or `send FX` in the Settings
  panel) is the send level; its rail fills orange when raised;
- an output strip's thin fader (or `return FX`) is how loud the FX return
  plays on that output; its rail fills green when raised.

The column holds **Reverb** and **Echo**, each with an on/off switch and a
type menu. Both carry the controls TotalMix FX provides: predelay, room
scale, cutoffs, width and volume for reverb; delay time with a BPM readout,
feedback, high cut and width for echo. The delay and BPM readouts are two
views of one value. The echo block also shows the FX bus level meters.

## 10. Control Room

The Control Room section sits at the right of the Hardware Outputs row. It
turns ordinary hardware outputs into monitoring destinations with fixed
names: `Main`, `Speaker B` and `Phones 1–4`. Every hardware output is either
assigned in the Control Room or remains an ordinary output in the Hardware
Outputs row; there are no empty slots.

### What the destinations mean

The Control Room is not another mixer bus. It gives names and monitoring
behaviour to hardware outputs you already have:

- **Main** is normally the engineer’s monitor output: the speakers in the
  control room.
- **Phones 1–4** are normally the performers’ headphone outputs. They are
  not the engineer’s headphones unless you assign one of them that way.
- **Speaker B** is an alternative monitor output, for a second pair of
  speakers or a second listening position.
- **Cue/PFL to** chooses where the engineer’s cue or PFL signal is heard.

So the usual studio picture is: the engineer listens on Main, the performers
listen on Phones, and the Control Room decides which physical outputs play
those roles. Assigning a pair moves it out of the ordinary Hardware Outputs
row and gives it the fixed Control Room name.

### The button stack

The buttons are one channel wide, in TotalMix’s order:

- **dim**, **recall**, **speaker B**;
- **mono**, **talkback**, **ext. input**, **mute FX**;
- **settings**, immediately above **assign**.

| Button | Kind | Outputs affected | In the mixer scene |
|---|---|---|---|
| `dim` | Latching | Main Out and Speaker B | No, runtime only |
| `recall` | Momentary | Main Out; Speaker B if linked | No, momentary |
| `speaker B` | Latching | Switches monitoring between Main and Speaker B | No, runtime only |
| `mono` | Latching | Main Out and Speaker B | Yes |
| `talkback` | Toggle | Phones with Talkback receive, plus Main for Listenback | No, runtime only |
| `ext. input` | Latching | Main Out and Speaker B | No, runtime only |
| `mute FX` | Latching | Reverb and Echo returns on Main Out and Speaker B | Yes |
| `settings` | Opens the dialog | — | No |
| `assign` | Opens the menu | The monitoring assignments | No, preferences |

### Dim

Dim needs an assigned Main Out. Without one it leaves the mix unchanged and
never acts on a Phones output.

**Dim** is a latching button. Engaging it saves the current Main Out and
Speaker B levels, lowers both speaker pairs by the configured Dim amount and
engages the device Dim state. Turning Dim off restores those saved levels.
Moving a speaker fader while Dim is engaged changes the level you hear, but
not the saved pre-Dim level: turning Dim off returns to the level saved when
Dim was engaged. Changing the Main Dim amount while Dim is engaged updates
the current attenuation immediately. Reassigning or clearing the Main Out or
Speaker B turns Dim off first.

Dim affects only the speaker paths, **Main Out and Speaker B**. It does not
attenuate the Phones outputs; Talkback has its own Phones Dim amount.
`Speaker B → Link to Main` additionally makes Speaker B follow the Main fader
and Recall; Dim attenuates both speaker pairs whether or not they are linked.

Dim is runtime only. It is not stored in a scene, and applying a scene or
changing the sample rate leaves Dim off without lowering the level again.

### Recall

**Recall** sets the Main Out level to the configured listening volume. It is
momentary: it does not latch and has no lit state after release. When Dim is
engaged, Recall changes the saved pre-Dim level and the current dimmed level
together.

Recall needs an assigned Main Out and never acts on a Phones output. With
`Speaker B → Link to Main` enabled, the Speaker B pair follows the Recall
level; unlinked, it keeps its own level.

### Speaker B

**Speaker B** is a latching button that switches monitoring from Main to the
output assigned to Speaker B. Both the Main Out and Speaker B must be
assigned first. The button lights while Speaker B is the active monitor
path.

Speaker B is runtime only and is not saved in a scene. Loading a scene or
recalling a snapshot leaves the current monitor path unchanged, and clearing
the Speaker B assignment also clears Speaker B and turns the button off.

`Speaker B → Link to Main` in Control Room Settings ties the Speaker B level
to Main. Linked, the Speaker B pair follows Main fader and Recall changes;
unlinked, it keeps its own fader level. Dim attenuates both speaker pairs in
either case.

### Mono

**Mono** is a latching monitor check. It sums the assigned Main Out pair and
the Speaker B pair to mono and feeds the same signal to both sides of each
pair, which is useful for checking mono compatibility and phase problems.
The Phones outputs are not mixed by this button; they keep their own stereo
signal.

Mono is part of the mixer scene, so it is saved and restored with the scene.

### Talkback and Listenback

Talkback is the engineer’s communication path to the performers. Click
**talkback** to switch it on; the button stays lit while it is active. Click
it again to switch it off. It is a temporary monitoring state and is not
stored in a scene.

While Talkback is active:

- the selected Talkback microphone is sent to every output whose Talkback
  receive switch is enabled;
- the other signals on those outputs are attenuated by the Talkback Dim
  amount, so the performer hears the engineer without losing the cue mix
  permanently;
- the FX returns on the affected outputs are silenced, without switching the
  Reverb or Echo processors off;
- Listenback, when configured, sends the recording-room microphone to Main
  and attenuates Main by the Listenback Dim amount.

Talkback only rewrites active routes; silent sends are left alone. Switching
Talkback off restores the previous sends and FX-return levels.

The Talkback source, Talkback Dim, Listenback source and Listenback Dim are
set in Control Room Settings. The Talkback receive switch is on each output’s
Settings panel. RME’s Control Room uses both directions; TotalControl exposes
the Talkback button and the receive switches.

### Mute FX

**Mute FX** is a latching scene control. It silences the Reverb and Echo
returns on the monitored outputs without switching either effect off.
Releasing it restores the previous return levels.

Mute FX follows the assigned Main Out and Speaker B pairs: it saves their
FX-return levels, writes −65 dB, and restores the exact saved levels when the
button is released. Phones outputs are never part of this path. Mute FX is
part of the mixer scene.

### External input

**Ext. input** replaces the Main Out and Speaker B submixes with the stereo
hardware-input pair selected in Control Room Settings. It applies the
configured gain, silences the FX returns on those two monitor pairs, and
latches the button. Releasing it restores the normal submix and the saved FX
returns. The source list contains hardware inputs only.

The button is available when a source pair is configured and a Main Out is
assigned. Changing the source or gain while External Input is active
re-engages the path with the new values.

### PFL

With `Live Mode, PFL replaces Solo` enabled, the Solo button on every input
strip becomes **PFL**. PFL sends that input pre-fader and at unity to the
selected `Cue/PFL to` output, silences the other sources in that output, and
temporarily mutes that output's FX returns. Releasing the last PFL input
restores the previous submix and FX returns.

`Exclusive Solo/PFL Mode` keeps only one input pair listening at a time. The
Control Strip's global Solo master shows `F` while a PFL input is active; it
turns the active PFL inputs off and can bring them back, like the ordinary
Solo master. Playback and output Solo behave normally.

### What is stored where

| State | Stored in |
|---|---|
| Main and Speaker B assignments | TotalControl preferences |
| Phones 1–4 destinations | TotalControl preferences |
| Main Dim amount, Recall level, Talkback/Listenback settings, Speaker B link | TotalControl preferences |
| Dim, Speaker B, External Input, PFL and Talkback/Listenback active state | Runtime only |
| Mono, Mute FX and the mixer levels | Mixer scene |

### Control Room Settings

The Talkback source and Dim, Listenback source and Dim, Recall level, Main Dim,
External Input and Gain, and the Solo/PFL modes are set in the Control Room
Settings dialog. Press `settings`, immediately above `assign`.

The dialog has four sections:

- **Talkback**: source and Dim.
- **Listenback**: source and Dim.
- **Main Out**: Recall, Dim, External Input, Gain and `Speaker B → Link to Main`.
- **Solo / PFL Mode**: `Live Mode, PFL replaces Solo` and
  `Exclusive Solo/PFL Mode`.

The source lists start with `None` and then use the hardware input names. The
five level controls cover `-65.0` to `0.0 dB` in `0.5 dB` steps. The settings
are saved per device in TotalControl's own preferences, not in the mixer scene.

`OK` applies and saves the values; `Cancel` and `Escape` discard the edits.
The settings in the dialog are not written while it is open. `OK` applies
the External Input source and gain and the Solo/PFL modes to the running
device; `Cancel` and `Escape` leave the current signal paths unchanged.

### Assigning outputs

`assign` opens the destination menu. The first level lists the destinations
— the same names as the Control Room channels — plus `Cue/PFL to`, which
chooses which output carries your cue/PFL signal and never reassigns
hardware. The second level lists the hardware channels as their predetermined
names (`None`, then every hardware output, current choice in bold). Choosing
`None` clears a destination.

A hardware output can sit in only one monitoring destination. Assigning it to
a second destination moves it there instead of showing it twice. Clearing a
destination returns the output to the Hardware Outputs row.

Control Room channels are always stereo: an assigned pair is linked and
cannot be unlinked, so the `STEREO` switch in its Settings panel stays
engaged and does nothing. A split pair is linked when it is assigned. `Set
all channels mono` leaves the assigned pairs linked.

Control Room channel names are fixed: they are always `Main`, `Speaker B`
and `Phones 1`–`Phones 4`, regardless of the View → Names setting, and they
cannot be renamed. The hardware channel feeding each destination is listed
by its own hardware name in the second menu level.

The Main and Speaker B assignments are kept with your TotalControl
preferences. They are restored at startup and are not changed by loading a
scene or recalling a snapshot.

Clicking a Control Room strip — including its name, fader or switches —
selects it as the current submix, like any other output. Control Room
channels are ordinary outputs for routing, so a hardware output stays a
submix even while it is your monitoring destination.

The Control Room buttons (dim, speaker B, mono, and so on) behave like the
front-panel equivalent controls of the unit.

## 11. Gestures

- **Drag** a fader, knob, matrix cell or EQ handle to change it. The drag
  keeps working even when the pointer leaves the control or the window,
  until you release it.
- **Hold Shift** while dragging for fine adjustment. This works on faders,
  knobs, matrix cells and EQ handles.
- **Scroll** over a fader or knob nudges it.
- **Double-click** a fader, the FX fader, a knob or an EQ handle to reset
  that control; double-click a matrix cell to toggle it between off and
  unity; double-click a channel name to rename it; double-click the dB
  readout to type a value; double-click a strip's background to collapse
  it.
- Ordinary buttons act once per click.

## 12. Options menu

The `OPTIONS` menu in the top bar contains `Reset Mix` and `Settings`.

`Reset Mix` lists the standard mixes:

| Entry | Effect |
|---|---|
| Straight playback with all to Main Out | playbacks 1:1 to the outputs, each also mixed to Main |
| Straight playback | playbacks 1:1 to the outputs, nothing else routed |
| Clear all submixes | every input and playback send set to nothing |
| Clear channel effects | EQ, low cut, dynamics, auto level, reverb, echo and width off |
| Set output volumes | outputs 0 dB; Main and Speaker B −10 dB |
| Reset channel names | user names cleared |
| Set all channels mono/stereo | the stereo or mono mode of every pair |
| Set inputs mono, outputs stereo | the ADM layout |
| Total reset | straight playback to Main, effects off, output volumes set |

Choosing one rewrites the whole mixer in one go.

`Settings` opens the TotalControl settings sheet: background behaviour,
startup sync, and the virtual surround outputs described in Appendix B.

## 13. Snapshots, layouts, undo and redo

TotalControl keeps your work in three layers:

- **Snapshots** (eight slots) store a complete mix: routing, faders, names,
  trims and the device settings TotalControl manages. They are mixer-plus files —
  broader than the device's own snapshot. Recall applies them to the device
  at once. The current mix is saved every few seconds and again when
  TotalControl closes.
- **Layouts** (six slots) store only which strips are collapsed.
- **GUI state** (window, sidebar, view, zoom, names, open panels, selected
  output and link groupings) is remembered automatically between sessions.

Undo restores the scene as it was before your last mix-changing step; every
continuous drag of one control counts as a single step. Redo reapplies what
you undid. Device settings and window layout are not part of undo.

Groups (multi-select sets) exist only while TotalControl runs; they are not saved.
There is no RME-style Workspace file — TotalControl keeps the same information in
separate files instead.

## 14. Latency

The sidebar shows the round-trip latency estimate for the current sample
rate and buffer size, and the **Quantum** picker sets the PipeWire graph
period in samples. The driver and the unit add a fixed amount of
device-side latency on top of the buffer; the readout lists the estimate
and its parts (USB queue, device lead, converters and graph period) so you
can budget monitoring and recording latency.

The **Errors** section lists driver transport faults since the device sheet
opened or the last **reset**, one row per type. `usb`, `gaps` and `short`
show input and output as `in / out`; `urb`, `recover` and `events` are single
values. A non-zero row means the USB transport is dropping or mis-timing
packets; reset only clears the display baseline.

## 15. Where TotalControl keeps its data

- Snapshots: `Mix <n>.json` in the scene directory.
- The continuously saved current mix: `auto.json` in the scene directory.
- Layouts: `Layout <n>.json` in the layout directory.
- Window and GUI state: `gui.json` in the `~/.config/totalcontrol/` directory.

These are plain JSON files you can back up or copy between machines.

## 16. Running in the background

With a system tray, closing the window keeps TotalControl running, as TotalMix
does: the front-panel DIM button and the restore of your mix after a replug
are TotalControl's job while it runs. The tray icon toggles the window; its menu
has dim, recall mix 1–8, settings and quit. `totalcontrol --minimized` starts
hidden, and Settings can start TotalControl with your session. Without a tray,
closing the window quits; one TotalControl runs per session, and a second launch
tops up the first.

See [`ui/totalcontrol/docs/guide.md`](../ui/totalcontrol/docs/guide.md) for the Linux
audio stack, building from source and project status.

## 17. Device settings at startup

At connect TotalControl compares the device state with the saved scene and
lets you choose, with a dialog:

- **Use TotalControl settings** — write the saved scene to the device.
- **Use device settings** — adopt the decoded device values in TotalControl
  and keep them on the unit.
- **Always use TotalControl settings** — as above and stop asking.

On the UCX the window appears first. TotalControl then requests the pre-connect
state dump and compares input and output levels, mute, EQ, dynamics, FX
returns and matrix sends. The dialog appears when the comparison is ready, so
the startup is not held back by the dump.

The dialog lists every decoded difference. When more than six are shown, the
list gets its own scrollbar and a fixed height so the dialog does not grow
beyond the window.

The dump does not include the options word (clock source, optical format,
SPDIF professional, word-clock rate and termination, EQ+D for Record), channel
names, trims or loopback. Those stay with the saved scene. On the UCX the
`48 V`, Inst, Level and reference settings are in the dump and are compared.

## 18. The UCX front knob

The UCX has one rotary encoder with push and a two-digit LED display. A green
LED shows CHANNEL or LEVEL mode. Pushing toggles between the modes; pushing
repeatedly jumps to `PH` for the phones volume.

### Channel selection

In CHANNEL mode, turning the encoder cycles through:

| Display | Channel |
|---|---|
| `i1`–`i4` | inputs 1–4 |
| `L1`, `L3`, `L5` | line output pairs 1/2, 3/4 and 5/6 |
| `PH` | phones / line output 7/8 |
| `SP` | SPDIF output |
| `A1`, `A3`, `A5`, `A7` | ADAT output pairs 1/2 … 7/8 |
| `SU` | setup |

Press the encoder to show the selected channel's value. Turning then changes
that value.

### Input gain

Inputs 1 and 2 use the microphone range, 0–65 dB, in 1 dB steps. Inputs 3
and 4 use the instrument/line range, 0–12 dB, in 0.5 dB steps.

A long press on an input enters pair-adjust mode. The left digit gets a dot
and the encoder moves **both** channels of the pair, whether or not the pair
is linked. For example, a gain of 26 dB shows `2.6` while both channels move,
then returns to `26`.

The device has a fixed link view: inputs `i1`–`i4` are individual, and the
outputs appear only as pairs `L1`/`L3`/`L5` and `A1`/`A3`/`A5`/`A7`. The front
knob cannot link or unlink a pair; the link state is a TotalControl setting.

A linked input pair always shows both gains in the TotalControl Gain knob:
the left gain above, the right gain below, even when they are equal. Turning
that knob moves both preamps by the same step, preserving any offset between
them. This mirrors the device's pair-adjust mode, which moves both channels
whether or not they are linked. An unlinked input shows one value and moves
only that channel.

### Output level

Output levels use 1 dB steps. The range is +6 dB down to −64 dB, then mute.
A dot after the second digit marks a negative value; `0` to `+6 dB` has no
dot.

A long press on an output enters panorama mode:

| Display | Meaning |
|---|---|
| `CE` | centre |
| `L.1` / `r.1` | full left / full right |
| intermediate | balance position |

The balance range is −100 to +100 in steps of 5. The display returns to the
output fader after about one second without a change. TotalControl follows the
balance change and shows the same position on the output's balance knob.

### Dots

| Dot | Meaning |
|---|---|
| left digit | the encoder is adjusting both channels of the pair |
| right digit on `i3`/`i4` | half-dB step (`11.` = 11.5 dB); always present for a half-dB value, absent for a whole-dB value |
| right digit on an output level | negative value, shown only below 0 dB |

The left dot is not a decimal point. `2.6` on an input means 26 dB with both
channels moving, not 2.6 dB. The right dot is never shown for `i1`/`i2`
because their step is 1 dB. It is not a link marker.

### Setups

`SU` selects a stored setup:

| Display | Meaning |
|---|---|
| `1`–`6` | stored mixer setups |
| `FA` | factory preset / Total Reset |
| `Ad` | 8-channel AD/DA converter mode, only when disconnected from USB |
| `CC` | Class Compliant mode, only when disconnected from USB |

The press that enters the `SU` menu is short. The final press that loads the
selected entry must be held for about one second; a short press does not load
it. `i.o.` means loaded and `n.o.` means an empty slot. Loading a setup while
connected changes only the front-panel parameters — input gains and output
volumes — not the routing. `CC` activates after a power cycle.

The six slots are written from TotalMix FX, **Options → Store current State
into Device → Setup 1–6**. The host does not upload a scene; the unit snapshots
its current internal state, including routing, EQ, FX, references and options.
TotalControl leaves the six device slots untouched. Its own **Mix 1–8**
snapshots are host files, not the unit's setups.

If the unit reports an error at power-up and every user slot still shows
`n.o.` after TotalMix has stored it, the unit's setup memory has not passed
its self-test. `FA` may still load because it comes from the firmware. In that
case the six device slots are unavailable; keep using host scenes and Mix 1–8
and have the unit serviced.

## Appendix A — Device modes

TotalControl needs the normal vendor mode of the interface. In that mode the
kernel driver exposes the PCM device and the hwdep mixer node.

In class-compliant mode the stock `snd-usb-audio` driver carries audio, but the
vendor mixer is not available. TotalControl cannot control the mixer in that
mode. Select the normal mode on the unit before starting TotalControl. On the
UCX, switch to class-compliant mode only while the unit is disconnected from
USB; power-cycle it after the change before reconnecting.

Only one application may own the mixer at a time. Close other mixer
applications before starting TotalControl.

## Appendix B — PipeWire

TotalControl uses the ALSA PCM exposed by the kernel driver. PipeWire sees it
as a normal card. Use the Pro Audio profile when the device should expose all
channels to a DAW.

The buffer-size picker in TotalControl sets the graph quantum. The driver adds
its own USB queue and device lead; the latency display explains the result.

The WirePlumber rule in
[`ui/totalcontrol/tools/alsa/wireplumber-uc.conf`](../ui/totalcontrol/tools/alsa/wireplumber-uc.conf)
selects polled scheduling and the 18-channel Pro Audio profile.

### Surround playback

The Pro Audio profile is the right choice for a DAW, but its 18 playback
channels are presented as `AUX0…AUX17`.  Desktop players that send a 5.1 or
7.1 stream cannot map those speaker positions onto AUX channels, so PipeWire
keeps only the front pair.

For surround media, open `Options → Settings` and enable `Virtual 5.1 output`,
`Virtual 7.1 output`, or both.  The sinks exist while TotalControl runs and
follow the selected Fireface/MR18 device:

- `TotalControl Surround 5.1` → SW playback 1/2, 3/4 and 5/6
- `TotalControl Surround 7.1` → SW playback 1/2, 3/4, 5/6 and 7/8

Select the matching sink in KDE or in the player.  An already-playing stream
keeps its old device; stop and restart it, or move it to the new sink.  The
bridge is host-side: TotalControl still sees the audio on the SW playback
channels and can route it like any other playback source.

## Appendix C — OSC setup

OSC is opt-in and binds to loopback. Open `Options > Settings`, enable
`OSC control surface`, and choose `Classic` or `Global`; the receive and send
ports are in the same section and are saved with the other settings. Disabling
the switch stops the surface immediately.

`Classic` is the banked TotalMix FX 1.96–2.11 address space. `Global` is the
absolute-channel TotalMix FX 2.1+ surface: channels are addressed by their
0-based position and continuous values use native units instead of the
classic normalized scale. The selected mode is saved with the other settings.

The command line remains a development override:

```sh
totalcontrol --osc
```

| Option | Default | Meaning |
|---|---:|---|
| `--osc-mode` | classic | `classic` or `global` protocol |
| `--osc-port` | 9000 | UDP port TotalControl listens on |
| `--osc-send-port` | 9001 | UDP port for feedback |
| `--osc-host` | 127.0.0.1 | Feedback destination |

Configure the controller to send to the receive port and receive feedback on
the send port. The classic address space is defined in
[`spec/TOTALCONTROL-OSC-SPEC.md`](spec/TOTALCONTROL-OSC-SPEC.md); Global uses
RME's separate Global OSC protocol table.

For a controller on another host, set `--osc-host` to the controller's address
and allow the feedback port through the firewall. Do not run two controllers
on the same feedback port.

## Appendix D — Command-line options

| Option | Effect |
|---|---|
| `--osc` | Enable the OSC control surface |
| `--osc-mode classic|global` | Select the OSC dialect |
| `--view quick|mixer|matrix` | Start on a view |
| `--fullscreen` | Start fullscreen |
| `--minimized` | Start hidden in the tray |
| `--scale <n>` | Initial UI scale |
| `--select-out <n>` | Initial submix |
| `--mock` | Simulate a Babyface Pro FS |
| `--mock-ucx` | Simulate a Fireface UCX |
| `--mock-uc` | Simulate a Fireface UC |
| `--control-only` | Do not open the capture PCM |

The mock modes are for demonstration and development. Device backend switches
and capture tools are documented in `DEVELOPMENT-TOOLS.md` and are not
supported installation options.
