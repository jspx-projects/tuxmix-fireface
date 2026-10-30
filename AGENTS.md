# Agent guidance

Read this file before starting work in this repository.

## Documentation rules

`docs/spec/` contains authoritative specifications. Write them as reference
documents for the current implementation, not as a work log.

- State **what something is** and how it behaves. Do not describe what it is
  not, what it used to be, or what went wrong before.
- No historical notes, retractions, changelogs, capture diaries, or "oracle
  said" narration in a specification.
- No personal identifying information. Do not put serial numbers, USB
  vendor/product IDs, hostnames, usernames, file paths with a home directory,
  or device addresses in a specification.
- Keep specifications minimal. Tables and short normative paragraphs are
  preferred over prose.
- Put open questions, TODO lists, capture requests and working notes in
  `docs/work/`. They are not part of the specification.
- The specification describes the shipped model and its device writes. When
  evidence is incomplete, state the implemented mapping positively and keep
  the open question in `docs/work/`.

Authoritative files:

- `docs/spec/UCX-CONTROL-SPEC.md` — UCX transport, registers, controls,
  metering.
- `docs/spec/TUXMIX-SPEC.md` — visible UI, panels, state and persistence.
- `docs/spec/UC-CONTROL-SPEC.md` — UC-specific protocol and controls.
- `docs/spec/BABYFACE-PRO-CONTROL-SPEC.md` — Babyface Pro FS channel model,
  ALSA surface, vendor USB protocol, DSP EQ blocks and metering.

## UI rules

- For UI layout, use current TotalMix FX screenshots when a manual figure
  shows an older layout. Manuals can lag the shipping application.

- On/off switches sit at the left of their row when the row has a label.
- Settings panels use one control width: a single control spans the content
  width; two controls share it with one gap, so their combined width equals
  a single control. `stereo`, `48V`, `autoset`, `no trim`, `loopback` and a
  phase pair therefore all end at the same right edge.
- A panel's knob rows share one vertical centre line: `gain`, `width`,
  `send FX` and `return FX` knobs are pinned to the same column. `send`/`FX`
  and `return`/`FX` use two lines so the label column stays narrow.
- Button labels are centred, and so are dropdown values.
- Every button and dropdown in a Settings panel shares one control height.
- Standalone labels in the Settings grid, such as `ref.`, are centred and
  sit at the bottom of their slot, just above the control they name.
- The strip's submix label box spans the strip width at the standard strip
  button height, with the name vertically centred at the left and the arrow
  at the right.
- The dB readout under the main fader is centred on the fader track and has
  no unit suffix; the numeric scale stays in the lane beside the track. The
  collapse button sits at the right edge of that row.
- Outside the Settings control grid, buttons are only as wide as their label
  needs. Do not stretch a button across its container by default; list and
  menu rows are the exception.
- Section headers use one style everywhere: uppercase label, accent tick and a
  trailing rule closed by a small block. A header spans its own section's
  elements, and the slack in the row is the break before the next section.
- Faders: every fader slides in one dark slot drawn by the fader itself. The
  main fader's rail sits inside a wider slot; the thin FX fader *is* the slot —
  one line, thinner, scaled with its smaller body. Both slots use the same
  dark colour.
- FX faders show colour only when raised: the rail fills with the send colour
  (orange) or return colour (green), and a fader at the bottom shows none. No
  extra marker line.
- The thin FX fader sits at the top of the strip's control column, level with
  the top of the main fader in every row. The T / gear / EQ / D column starts
  directly below it, as high as the strip allows; spare height stays below
  the column.
- Every strip has a main fader, and the symbol column is anchored to it: it
  sits below the FX fader when the unit has one, directly below the fader top
  otherwise. The column holds the symbols the unit has, in the fixed order
  T / gear / EQ / D (the UC has T and gear; the UCX has all four). Every
  strip keeps each symbol at the same absolute height as the hardware-input
  row; a channel that lacks one keeps its empty slot (same height, same gap),
  so no symbol below moves.
- The pointer never changes over a fader or a knob; the grab hand hides the
  values underneath it.
- Knobs carry a neutral full-range track; a coloured arc appears only while
  the control is live. No always-on coloured outline.
- Phase buttons use the `Ø` symbol, never the word: `Ø L` / `R Ø` for a
  linked pair, `Ø` alone otherwise.
- The Settings slideout is one fixed row grid per channel type; a control a
  channel lacks leaves its row empty, so nothing below it moves. The
  per-channel grid is in `docs/spec/TUXMIX-SPEC.md` §3.
- Every slideout fits the strip height at normal size: everything visible at
  a glance, no scrolling.
- Slideouts always open to the right of their own channel. In the Control Room
  the section shifts its own channels left by one slot to make the space; its
  left edge may overlap the channel row but never moves it.
- The FX column is a slideout: it pushes the rows until the widest row reaches
  its natural width, then overlays the rightmost part of the rows. The overlap
  grows continuously; the Control Room and any open channel slideout keep
  their position and are covered from the right. Never switch the main width
  between push and overlay in one step. Widening the window or closing FX
  clears it.
- Slideout panels never show a scrollbar at normal size. Keep the scrollable
  wrapper in the code as a safety net while tuning, so missing content is
  reachable instead of silently clipped.
- Slideout speed is constant in pixels per millisecond; a wider panel takes
  longer instead of moving faster.
- Outline only for buttons, never for slideout tabs. T, EQ and D use orange
  text only while their setting is active and the slideout is closed; while
  the slideout is open they carry no orange. Hover brightens the glyph but
  never uses orange. The panel's own on/off control is the single
  indication. The gear only marks the open panel.
- A channel button that opens a slideout keeps the same fixed slot whether
  the slideout is open or closed; only the panel grows beside it.
- A fader, knob, matrix cell or EQ handle captures the pointer for the drag;
  the drag continues outside the control and ends on release.
- Linking an input pair restores the pan captured when it was split; the
  split itself sets the two channels hard left and hard right.
- Splitting an output pair converts its pair level and balance into two
  channel volumes; linking recomputes both from those volumes. The round
  trip must preserve the original values. The balance knob is linked-only.
- Slideouts have no close button; the strip tab that opened the panel closes
  it.
- A knob label may overlap the unused bottom margin of its knob when space
  is tight; use the panel background behind the label.
- Align related controls and values in rows and columns whenever possible.
- Keep at least `theme::SPACE_TIGHT` (2 px at scale 1) between distinct
  controls.
- Ordinary nouns in slideouts are lowercase (`gain`, `attack`, `compressor`,
  `auto level`).
- Knob labels use the standard `theme::KNOB_LABEL_GAP`; the `attack` label
  and its knob are the reference distance.
- The channel-strip icon order is fixed: `T`, settings gear, `EQ`, `D`.
  Never reorder these.
- Colours follow the signal section: compressor red, expander green,
  EQ bands red/green/blue, shared controls neutral white.

## Working rules

- Use the NAS mailbox in `/media/nas_data/John/Electronica/rme/coordination/`
  for Windows/oracle questions and acknowledgements. Linux owns
  `LINUX-STATUS.md`, `LINUX-REQUESTS.md`, `DECISIONS.md`, `analysis/*` and
  `coordination/inbox/windows/*`.
- Push only to our own repository, `jspx-projects/tuxmix-fireface`.
- The UI is part of this repository. Do not add an external TuxMix development
  remote; the imported upstream history is on `archive/tuxmix-upstream`.
- Preserve the user scene and `~/.config/tuxmix/gui.json`. Leave the device on
  the user's scene after hardware tests.
- One Fireface at a time.
- Verify with `cargo test -p tuxmix-core --lib --features alsa` and
  `cargo test -p tuxmix-gui`.
- Do not replace device-backed behaviour with host-side approximations. If the
  unit supports something and the register is unknown, add it to
  `docs/work/` and request a capture.
