# Agent guidance

Read this file before starting work in this repository.

## What lives here

- `kernel/` — the `snd-fireface-uc` out-of-tree kernel module: USB
  transport, ALSA PCM, the `snd-hwdep` control ABI and a rawmidi port.
  The device table covers the RME Fireface UC and UCX in both USB vendor
  personalities.
- `ui/tuxmix/` — TuxMix, the mixer application, a Cargo workspace
  (`tuxmix-core`,
  `tuxmix-gui`, `tuxmix-tui`, `tuxmix-usb`, `tuxmix-sys`). It develops
  independently of the kernel. Babyface Pro FS support lives here, in
  userspace.
- `tools/` — probe and analysis sources.
- `docs/spec/` — the authoritative specifications.
- `docs/captures/` — cleaned descriptor dumps.
- `scripts/` — build, install, load and capture helpers.
- The root `Makefile` builds the `rme-probe` tool.

The only interface between driver and UI is the hwdep ABI in
`kernel/ffuc-uapi.h`, mirrored in
`ui/tuxmix/tuxmix-core/src/fireface/abi.rs`.
Changing one means updating the other in the same change.

## Build and test

- Driver: `make -C kernel`; install through DKMS with
  `scripts/install-dkms.sh`.
- UI: from `ui/tuxmix/`, `cargo test -p tuxmix-core --lib --features alsa` and
  `cargo test -p tuxmix-gui`.
- Tools: `make` in the repository root.

## Publishing rules

- No proprietary RME material may be published in this repository: manuals
  (PDFs or excerpts), drivers, firmware, or graphics copied from RME
  assets. Protocol descriptions and register maps derived from USB
  captures are fine.
- No per-unit identifiers: device serial numbers, product-string unit
  suffixes, hostnames, usernames or absolute personal paths.
- The commit history is part of the publication. Before pushing, audit the
  history for the identifiers above (`git log -S`); anything once
  committed stays recoverable from old commits.
- Local-only material (manuals, the RME Windows driver package, capture
  data, working notes) stays out of the published tree through
  `.git/info/exclude`. Do not list private paths in a committed
  `.gitignore` — the names themselves would leak.

## Documentation rules

`docs/spec/` contains authoritative specifications. Write them as reference
documents for the current implementation, not as a work log.

- State **what something is** and how it behaves. Do not describe what it
  is not, what it used to be, or what went wrong before.
- No historical notes, retractions, changelogs, capture diaries, or
  narration of individual findings in a specification.
- No personal identifying information. Do not put serial numbers, USB
  vendor/product IDs tied to a unit, hostnames, usernames, file paths with
  a home directory, or device addresses in a specification.
- Keep specifications minimal. Tables and short normative paragraphs are
  preferred over prose.
- Put open questions, TODO lists, capture requests and working notes in
  `docs/work/`. They are not part of the specification and are not
  published.
- The specification describes the shipped model and its device writes. When
  evidence is incomplete, state the implemented mapping positively and keep
  the open question in `docs/work/`.

Authoritative files:

- `docs/spec/UCX-CONTROL-SPEC.md` — UCX transport, registers, controls,
  metering.
- `docs/spec/UC-CONTROL-SPEC.md` — UC-specific protocol and controls.
- `docs/spec/BABYFACE-PRO-CONTROL-SPEC.md` — Babyface Pro FS channel model,
  ALSA surface, vendor USB protocol, DSP EQ blocks and metering.

## UI rules

- For UI layout, use current TotalMix FX screenshots when a manual figure
  shows an older layout. Manuals can lag the shipping application.
- On/off switches sit at the left of their row when the row has a label.
- Buttons are only as wide as their label needs. Do not stretch a button
  across its container by default; list and menu rows are the exception.
- Section headers use one style everywhere: uppercase label, accent tick
  and a trailing rule closed by a small block. A header spans its own
  section's elements, and the slack in the row is the break before the
  next section.
- Faders: every fader slides in one dark slot drawn by the fader itself.
  The main fader's rail sits inside a wider slot; the thin FX fader *is*
  the slot — one line, thinner, scaled with its smaller body. Both slots
  use the same dark colour.
- FX faders show colour only when raised: the rail fills with the send
  colour (orange) or return colour (green), and a fader at the bottom
  shows none. No extra marker line.
- The thin FX fader sits at the top of the strip's control column, level
  with the top of the main fader in every row. The T / gear / EQ / D column
  starts directly below it, as high as the strip allows; spare height stays
  below the column.
- Every strip has a main fader, and the symbol column is anchored to it: it
  sits below the FX fader when the unit has one, directly below the fader top
  otherwise. The column holds the symbols the unit has, in the fixed order
  T / gear / EQ / D (the UC has T and gear; the UCX has all four). Every
  strip keeps each symbol at the same absolute height as the hardware-input
  row; a channel that lacks one keeps its empty slot (same height, same gap),
  so no symbol below moves.
- The pointer never changes over a fader or a knob; the grab hand hides
  the values underneath it.
- Knobs carry a neutral full-range track; a coloured arc appears only while
  the control is live. No always-on coloured outline.
- Phase buttons use the `Ø` symbol, never the word: `Ø L` / `R Ø` for a
  linked pair, `Ø` alone otherwise.
- The Settings slideout is one vertical stack per channel type; shared
  controls keep the same height between channel types. The per-channel
  order is in `docs/spec/TUXMIX-SPEC.md` §3.
- Slideouts always open to the right of their own channel. In the Control
  Room the section shifts its own channels left by one slot to make the
  space; its left edge may overlap the channel row but never moves it.
- Slideout panels never show a scrollbar at normal size. Keep the
  scrollable wrapper in the code as a safety net while tuning, so missing
  content is reachable instead of silently clipped.
- Slideouts have no close button; the strip tab that opened the panel
  closes it.
- A knob label may overlap the unused bottom margin of its knob when space
  is tight; use the panel background behind the label.
- Align related controls and values in rows and columns whenever possible.
- Keep at least `theme::SPACE_TIGHT` (2 px at scale 1) between distinct
  controls.
- Ordinary nouns in slideouts are lowercase (`gain`, `attack`,
  `compressor`, `auto level`).
- Knob labels use the standard `theme::KNOB_LABEL_GAP`; the `attack` label
  and its knob are the reference distance.
- The channel-strip icon order is fixed: `T`, settings gear, `EQ`, `D`.
  Never reorder these.
- Colours follow the signal section: compressor red, expander green, EQ
  bands red/green/blue, shared controls neutral white.

## Working rules

- One Fireface device at a time.
- Do not kill or restart the running mixer application without letting it
  save its state; leave the unit on the user's scene after hardware tests.
- Any new UI control needs its simulation (mock) implementation in the
  same change as the real device path.
- Do not replace device-backed behaviour with host-side approximations. If
  the unit supports something and the register is unknown, add it to
  `docs/work/` and request a capture.
- Verify driver changes by reloading the module and reading the kernel
  log.
- Never run `git clean -x` in this repository: it deletes excluded
  local-only files.
