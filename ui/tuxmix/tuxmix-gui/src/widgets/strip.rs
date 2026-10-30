//! A single channel strip: label + type tag, mute/solo, 48V/PAD, fader+VU,
//! dB readout (double-click to edit), pan readout.

use iced::advanced::text as advanced_text;
use iced::keyboard::Modifiers;
use iced::widget::canvas::{self, Canvas, Frame, Geometry};
use iced::widget::{button, column, container, mouse_area, row, text, text_input, tooltip, Stack};
use iced::{alignment, mouse, Color, Element, Length, Point, Rectangle, Renderer, Size, Theme, Vector};
use std::time::Instant;
use tuxmix_core::ChannelId;

use crate::app::{short_label, Message};
use crate::theme;
use crate::widgets::fader::{fader, vu_meter, Fader, MeterFrame};
use crate::widgets::knob::{knob, Knob};

/// Base sizes at `scale == 1.0` (`theme::SCALE_DEFAULT`) — every dimension
/// in a strip is one of these times `StripParams::scale`, so the window's
/// adaptive scale (see `app::recompute_ui_scale`) resizes strips the same
/// way it resizes text.
/// Fixed chrome (header, pan, M/S, dB row, route/collapse and padding)
/// around the fader.  The fader absorbs every extra pixel of `STRIP_H`, so
/// changing the standard channel height stretches the strip instead of
/// leaving a blank band.
const FADER_CHROME_H: f32 = 144.0;
/// Full-strip height at scale 1.0: header, pan, M/S, fader, dB and route
/// rows, plus the card padding and gaps.  Every full strip is pinned to this
/// height (inputs, playback and outputs), and the Settings/Trim/EQ slideouts
/// use the same value, so a slideout always matches the channel beside it.
pub(crate) const STRIP_H: f32 = 350.0;

/// Size of the Settings/Trim/EQ tab buttons on the strip's right flank.
/// Bigger than the T/phase icon buttons so the open-tab rectangle reads as a
/// tab attached to the slideout, not a tiny status dot.
const TAB_BTN_W: f32 = 24.0;
const TAB_BTN_H: f32 = 22.0;
/// The Settings gear is the most-used tab, so it gets a larger hit box and
/// glyph than the T/EQ tabs.  The fader's reserved right edge tracks this.
const SETTINGS_TAB_W: f32 = 28.0;
const SETTINGS_TAB_H: f32 = 26.0;
const SETTINGS_GLYPH: f32 = 19.0;
/// Width of a full channel strip: track, ruler, meter, and the side icon
/// column at `scale == 1.0`.
pub(crate) const STRIP_W: f32 = 90.0;

/// The message a Control Room strip's FX fader sends: FX Return on an
/// output, FX send on anything else.
fn fx_message(cid: ChannelId, db: f32) -> Message {
    match cid {
        ChannelId::Output(idx) => Message::FxReturnChanged(idx, db),
        _ => Message::FxSendChanged(cid, db),
    }
}

/// Linear amplitude to dB, for the FX fader's value field.
fn db_of(linear: f32) -> f32 {
    if linear <= 0.000_01 {
        -65.0
    } else {
        (20.0 * linear.log10()).clamp(-65.0, 6.0)
    }
}
/// Collapsed strips need to stand the same total height as a full strip
/// (see `collapsed_strip`'s own doc comment) despite having far fewer
/// rows — name/M/S/meter/expand vs. the full strip's
/// header/pan/M-S/fader/dB/route. The meter is what absorbs the
/// difference. Was `76.0`, re-tuned down after the name label became a
/// tall rotated canvas (see `ROTATED_LABEL_LEN`) instead of one short
/// text line — that canvas already eats most of the vertical slack the
/// old flat offset existed to add. Empirically measured the same way the
/// original number was (pixel-measuring a collapsed and an adjacent full
/// strip side by side in a live screenshot), not derived on paper.
/// Fixed chrome of a collapsed strip: outer padding, the four row gaps, the
/// rotated name, and the mute, solo and expand rows.  The meter absorbs the
/// rest so a collapsed strip and a full strip share the same `STRIP_H`.
const COLLAPSED_CHROME_H: f32 = theme::SPACE_SM * 2.0
    + theme::SPACE_HAIRLINE * 4.0
    + ROTATED_LABEL_LEN
    + ICON_BTN_H * 3.0;
const COLLAPSED_METER_H: f32 = STRIP_H - COLLAPSED_CHROME_H;
/// Collapsed strips are a glance-only readout: name, Mute/Solo (kept
/// live — see this function's own doc comment), and a VU meter; no
/// fader, no pan, no route. Width is set by the header (name + expand
/// button), not the meter, which is narrower than that on its own.
/// Deliberately the same `COLLAPSED_W` for every channel kind, output
/// strips included — collapsing is opting *out* of emphasis, so there's
/// nothing to differentiate here.
///
/// Narrowed from `60` after comparing against a live TotalMix screenshot
/// at pixel level (`Screenshot 2026-08-30 124715.png`): real collapsed
/// strips there measure ~29px against ~75px full strips — a ~0.39 ratio,
/// well under half, not the ~0.6 ours had. `44` keeps that same ratio
/// against the current (wider) `STRIP_W` while still leaving room for
/// the existing `vu_meter` widget's own fixed `METER_RULER_W` (30) plus
/// padding without clipping it — matching TotalMix's exact ~0.39 would
/// need a narrower meter widget too, not attempted this pass.
pub(crate) const COLLAPSED_W: f32 = 44.0;

/// Size of the small icon-only buttons (settings gear, collapse chevron)
/// — smaller than the strip's other buttons (M/S at 18x`btn_h`) since
/// both now share a row with the dB readout at the bottom of the strip
/// (see `full_strip`), which doesn't have much width to spare either.
const ICON_BTN_W: f32 = 15.0;
const ICON_BTN_H: f32 = 14.0;

/// Width (at `scale == 1.0`) of the Route flyout `app.rs` opens over a
/// strip's right neighbor — `pub(crate)` because `app.rs` needs it to
/// size the popover. Settings doesn't use this: it pushes the row instead
/// of overlaying it, sized to match the strip's own width (see the
/// reference design), not a fixed constant — see `FlyoutKind`.
pub(crate) const FLYOUT_W: f32 = 140.0;

/// Width (at `scale == 1.0`) of the EQ flyout — wide enough for the
/// three-column band matrix (type/gain/freq/Q) plus its row labels, and
/// the response plot above it.
pub(crate) const EQ_FLYOUT_W: f32 = 248.0;

/// Width of the Dynamics flyout — two control columns plus the Auto Level
/// block, matching the manual's panel grouping.
pub(crate) const DYN_FLYOUT_W: f32 = 230.0;

/// Which flyout (at most one, and only one per strip) is open — the gear
/// icon opens `Settings` (48V/PAD/Sensitivity, moved out of the strip's
/// own vertical flow so it doesn't grow/shrink the whole card; Gain stays
/// inline instead — see `app.rs::settings_popover`'s doc comment for why),
/// the route trigger opens `Route` (which output bus), the EQ trigger
/// (analog inputs only) opens `Eq` (3-band + low-cut, see
/// `app.rs::eq_popover`), and the "T" trigger (every hardware input, see
/// `StripParams::has_trim`) opens `Trim` (see `app.rs::trim_popover`).
/// All render as the same kind of animated panel sliding out over the
/// strip's right neighbor — see `app.rs::with_flyout` (Route) and
/// `mixer_view`'s input loop (Settings/Eq/Trim, which push the row
/// instead).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FlyoutKind {
    Route,
    Settings,
    Eq,
    Dynamics,
    Trim,
}

/// The full (uncollapsed) width for a given channel — every channel kind
/// renders at the same width. Takes `cid` (unused) rather than being a
/// bare constant so `app.rs::set_collapsed`/`rendered_strip_width` don't
/// need to change if a future kind ever needs to differ again.
pub(crate) fn full_width(_cid: ChannelId) -> f32 {
    STRIP_W
}

/// Slideout speed, independent of the panel width: a 90 px Settings panel
/// still takes 90 ms, while a 248 px EQ panel takes 248 ms instead of
/// racing through the same 90 ms. One pixel per millisecond.
///
/// A width tween is a navigation aid, not an effect. A fixed duration made
/// wide panels slide visibly faster than narrow ones; a fixed speed keeps
/// the motion even.
const COLLAPSE_SPEED_PX_PER_MS: f32 = 1.0;

#[derive(Clone, Copy, Debug)]
pub struct CollapseAnim {
    pub prev: f32,
    pub value: f32,
    pub since: Instant,
}

impl CollapseAnim {
    fn duration_ms(&self) -> f32 {
        (self.value - self.prev).abs() / COLLAPSE_SPEED_PX_PER_MS
    }

    pub fn at(&self, now: Instant) -> f32 {
        let duration = self.duration_ms();
        if duration <= f32::EPSILON {
            return self.value;
        }
        let t = (now.duration_since(self.since).as_secs_f32() * 1000.0 / duration)
            .clamp(0.0, 1.0);
        self.prev + (self.value - self.prev) * t
    }

    pub fn is_settling(&self, now: Instant) -> bool {
        let duration = self.duration_ms();
        duration > f32::EPSILON
            && now.duration_since(self.since).as_secs_f32() * 1000.0 < duration
    }
}

pub struct StripParams<'a> {
    pub cid: ChannelId,
    pub output_idx: usize,
    /// Human label of the current submix/output bus (device-derived).
    pub bus_label: String,
    /// Whether the connection this strip is drawn for can drive RME's own
    /// protocol (see `RmeDevice::protocol_features`). False on the
    /// class-compliant personality, where EQ, Trim, Phase, Split and
    /// Loopback would only be able to fail: they are left out entirely,
    /// the same thing the `has_*` flags above already do for features a
    /// channel does not have.
    pub protocol_features: bool,
    /// Whether this backend can toggle a playback pair's stereo split
    /// (see `RmeDevice::stereo_split_supported`).  Separate from
    /// `protocol_features`: the UCX v1 backend supports the split through
    /// its matrix model without the full RME feature set.
    pub stereo_split_supported: bool,
    pub name: String,
    pub type_tag: Option<(&'static str, Color)>,
    pub vol: f32,
    pub pan: i8,
    /// Hardware-output balance is available for this strip (UC: unequal
    /// left/right output levels; UCX/other backends not implemented).
    pub output_balance_supported: bool,
    /// FX send (inputs and software playback) or FX Return (outputs), in dB.
    /// `None` on a backend without the FX bus. Drawn as the thin fader to
    /// the right of the main one, as TotalMix does.
    pub fx_level: Option<f32>,
    pub meter: MeterFrame,
    /// Whether `meter` is a real reading on the active backend — see
    /// `fader::draw_meter`'s doc comment.
    pub meter_available: bool,
    /// Set only when this strip represents a LINKED pair — the right
    /// channel's own meter, splitting the meter column into two thin
    /// bars (see `fader::Fader::meter2`'s own doc comment). `None` for
    /// every unlinked/mono strip.
    pub meter2: Option<MeterFrame>,
    pub meter2_available: bool,
    pub has_48v: bool,
    pub has_pad: bool,
    pub has_instrument: bool,
    pub phantom: bool,
    pub pad: bool,
    pub instrument: bool,
    /// Whether this input has a preamp gain control at all (Mic and
    /// Instrument only — Line/SPDIF/ADAT inputs have no gain knob).
    pub has_gain: bool,
    /// Raw hardware gain units, not dB — see `Message::Gain`.
    pub gain: u32,
    pub gain_max: u32,
    /// Inputs 3/4 use 0.5 dB units; display `gain / 2.0` dB.
    pub gain_half: bool,
    /// Whether this input has a sensitivity switch (Instrument only).
    pub has_sensitivity: bool,
    /// `true` = +4dBu, `false` = -10dBV.
    pub sensitivity_plus4: bool,
    /// Whether this input has the UCX three-value Level control.
    pub has_level: bool,
    /// Current Level label (`-10dBV`, `+4dBu`, `LoGain`).
    pub level: tuxmix_core::InputLevel,
    /// Whether this input has AutoSet (UCX inputs 1/2).
    pub has_autoset: bool,
    pub autoset: bool,
    /// Whether this channel has a Dynamics panel (UCX inputs/outputs).
    pub has_dynamics: bool,
    pub dynamics_on: bool,
    /// Whether this input has an EQ strip at all (the 4 analog inputs
    /// only — see `InputChannel::eq`).
    pub has_eq: bool,
    /// Whether the EQ is currently engaged — lights up the trigger button
    /// even when its flyout isn't open, like the 48V/PAD toggles.
    pub eq_enabled: bool,
    /// Whether this channel has a Trim control — every hardware input
    /// (analog and digital alike, unlike `has_gain`), `false` for
    /// Playback/Output (see `full_strip`'s "T" button doc comment for
    /// why Playback doesn't get one despite TotalMix showing it there).
    pub has_trim: bool,
    /// Current trim value in dB (-65..+6) — see `InputChannel::trim`.
    pub trim: f32,
    /// Hover text for `T`: "Show trim" at 0 dB, otherwise the applied
    /// attenuation.
    pub trim_tip: String,
    /// Whether the unit has this symbol at all.  The column reserves a slot
    /// for every symbol the unit has, so a channel that lacks it keeps the
    /// grid while a unit without the feature (no EQ on the UC) has no slot.
    pub slot_trim: bool,
    pub slot_eq: bool,
    pub slot_dyn: bool,
    /// Whether this input has a Phase Ø invert switch — the 4 analog
    /// inputs only, and only where the backend decodes phase (the UC does
    /// not yet; see `UC-TODO-06`).
    pub has_phase: bool,
    pub phase: bool,
    /// Output-only: `OutputChannel::loopback`.
    pub loopback: bool,
    /// Whether this connection can drive the loopback register (`0x15`).
    /// Separate from `protocol_features`: the UC has no EQ/FX but does
    /// have loopback.
    pub loopback_supported: bool,
    /// Whether this channel's pair is currently a linked stereo bus
    /// (`RmeDevice::{input_pair,playback,output}_linked`, whichever
    /// matches `cid`'s kind) — TotalMix's default, shown/toggled from
    /// the Settings flyout for every channel kind.
    pub stereo_linked: bool,
    /// Which flyout (if any) is currently open *for this strip* — lights
    /// up the matching trigger (gear icon for `Settings`, the route
    /// button for `Route`); `app.rs` owns the actual popover content.
    pub open_flyout: Option<FlyoutKind>,
    pub mute: bool,
    pub solo: bool,
    /// Off, but held for the sidebar's Mute / Solo master to bring back
    /// (`theme::armed_toggle_button`). Set by `app::finish_params`.
    pub mute_armed: bool,
    pub solo_armed: bool,
    /// `true` only for `ChannelId::Output` — every other channel kind
    /// has no CUE bus of its own (see `RmeDevice::set_cue`'s doc
    /// comment: CUE always monitors *some output's* dedicated
    /// playback pair through AN1/2, not an input/playback concept).
    pub has_cue: bool,
    pub cue: bool,
    pub default_vol: f32,
    pub editing: bool,
    pub edit_buf: &'a str,
    /// The name field is open for typing (`Message::RenameStart`), with
    /// what has been typed so far. Set by `app::finish_params`.
    pub renaming: bool,
    pub rename_buf: &'a str,
    pub drag_range: Option<(f32, f32)>,
    pub modifiers: Modifiers,
    pub collapsed: bool,
    pub collapse_anim: Option<CollapseAnim>,
    pub scale: f32,
    pub selected: bool,
    pub hovered: bool,
    /// This output strip is the current submix destination (TotalMix's
    /// lighter active-output card).
    pub active_surface: bool,
    /// `name` is the user's own label; do not strip a hardware "PCM "/"OUT "
    /// prefix from it, unlike the original names.
    pub name_custom: bool,
}

/// A button's own padding-based centering isn't reliable across glyphs of
/// different intrinsic width (e.g. "S" sat visibly left of center while "M"
/// looked fine) — force it explicitly instead of trusting the default. The
/// default 1.2x line-height also reserves descender space these glyphs
/// (M, S, no descenders) never use, which reads as "sitting too high" once
/// centered — tightening it to 1:1 removes that residual vertical bias.
pub(crate) fn centered_label<'a>(s: &'a str, size: f32) -> Element<'a, Message> {
    container(
        text(s)
            .size(size)
            .line_height(iced::widget::text::LineHeight::Absolute(iced::Pixels(
                size,
            ))),
    )
    .center(Length::Fill)
    .into()
}

/// Wraps a control in a hover tooltip — for the abbreviations (M, S, 48V,
/// PAD) that read as pro-audio jargon to anyone not already fluent in it.
/// A short delay so it doesn't flash on every incidental mouse-over while
/// moving across the strip toward something else.
pub(crate) fn hint<'a>(
    content: impl Into<Element<'a, Message>>,
    label: impl Into<std::borrow::Cow<'a, str>>,
    scale: f32,
) -> Element<'a, Message> {
    let label: std::borrow::Cow<'a, str> = label.into();
    tooltip(
        content,
        container(text(label).size(theme::TEXT_XS * scale).color(theme::TEXT_PRIMARY))
            .padding(theme::SPACE_SM * scale)
            .style(theme::panel),
        tooltip::Position::Top,
    )
    .gap(4.0 * scale)
    .delay(std::time::Duration::from_millis(400))
    .into()
}

/// The active-tab background: a rounded rectangle 6 px wider on each side
/// than the icon box, so the icon stays put and centred while the rectangle
/// reaches the strip's right padding and touches the slideout.  Drawn as a
/// Canvas underlay so it can overflow the icon box without affecting layout.
struct TabUnderlay {
    active: bool,
    scale: f32,
    w: f32,
    h: f32,
}

impl<Message> canvas::Program<Message> for TabUnderlay {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        if !self.active {
            return Vec::new();
        }
        let w = (self.w + 12.0) * self.scale;
        let h = self.h * self.scale;
        let mut frame = Frame::new(renderer, Size::new(w, h));
        let path = canvas::Path::new(|p| {
            p.rounded_rectangle(
                Point::new(-6.0 * self.scale, 0.0),
                Size::new(w, h),
                (3.0 * self.scale).into(),
            )
        });
        frame.fill(&path, theme::SURFACE);
        let _ = bounds;
        vec![frame.into_geometry()]
    }
}

/// Settings/Trim/EQ tab.  The underlay is 6 px wider on each side than the
/// button, so the dark rectangle reaches the strip edge without moving the
/// icon or the channel content.  The rectangle is only drawn while that
/// slideout is open.  Settings passes the larger `SETTINGS_TAB_*` size.
fn tab_button<'a>(
    label: &'static str,
    active: bool,
    lit: bool,
    message: Message,
    tip: impl Into<std::borrow::Cow<'a, str>>,
    scale: f32,
) -> Element<'a, Message> {
    if active {
        // Open: the underlay joins the tab to its panel.  No accent here:
        // the panel itself shows the on/off state.
        return tab_button_sized(
            label,
            true,
            false,
            message,
            tip,
            scale,
            TAB_BTN_W,
            TAB_BTN_H,
            theme::TEXT_MD + 2.0,
        );
    }
    // Closed: an active setting gets the Stereo button's accent outline,
    // sized to the text and centred in the tab's fixed column slot.  An
    // inactive setting stays a bare glyph.
    let glyph = || centered_label(label, (theme::TEXT_MD + 2.0) * scale);
    let face = if lit {
        button(glyph())
            .padding([1.0 * scale, 3.0 * scale])
            .style(theme::stereo_button(true))
            .on_press(message)
    } else {
        button(glyph())
            .padding([1.0 * scale, 3.0 * scale])
            .style(theme::panel_tab(false, false))
            .on_press(message)
    };
    hint(
        container(face)
            .width(Length::Fixed(TAB_BTN_W * scale))
            .height(Length::Fixed(TAB_BTN_H * scale))
            .align_x(iced::Alignment::Center)
            .align_y(iced::Alignment::Center),
        tip,
        scale,
    )
}

#[allow(clippy::too_many_arguments)]
fn tab_button_sized<'a>(
    label: &'static str,
    active: bool,
    lit: bool,
    message: Message,
    tip: impl Into<std::borrow::Cow<'a, str>>,
    scale: f32,
    w: f32,
    h: f32,
    glyph_size: f32,
) -> Element<'a, Message> {
    let underlay = Canvas::new(TabUnderlay {
        active,
        scale,
        w,
        h,
    })
    .width(Length::Fixed(w * scale))
    .height(Length::Fixed(h * scale));
    // The underlay draws 6 px to the left of its frame, but that part is
    // clipped, so the visible rectangle is centred 3 px right of the glyph.
    // A 6 px left content padding moves the centred glyph right by 3 px;
    // the glyph now sits in the middle of the visible back rectangle.
    let face = button(centered_label(label, glyph_size * scale))
        .padding(iced::Padding {
            left: 6.0 * scale,
            ..iced::Padding::ZERO
        })
        .width(Length::Fixed(w * scale))
        .height(Length::Fixed(h * scale))
        .style(theme::panel_tab(false, lit))
        .on_press(message);
    hint(Stack::new().push(underlay).push(face), tip, scale)
}

/// How long (in canvas-local x, *before* the -90° rotation — i.e. how
/// tall the finished label actually stands once rotated) `rotated_label`
/// reserves for the name — sized for the longest real channel name this
/// app ever shows ("ADAT7/8 PB" and friends), not just the common case,
/// since unlike a plain `text()` widget this canvas has no shrink-to-fit:
/// anything longer than this would silently draw past its own bounds and
/// overlap the Mute button below it.
const ROTATED_LABEL_LEN: f32 = 64.0;

/// A read-only text canvas rotated -90° (reads bottom-to-top) — the
/// collapsed strip's own name label. Confirmed against a live TotalMix
/// screenshot: collapsed buses are narrow enough (see `COLLAPSED_W`'s own
/// doc comment) that a normal horizontal name doesn't fit at all, so
/// TotalMix rotates it instead of truncating or wrapping it. iced's plain
/// `text()` widget has no rotation of its own, hence a small dedicated
/// canvas rather than reusing `centered_label`.
struct RotatedLabel {
    text: String,
    size: f32,
}

impl<Message> canvas::Program<Message> for RotatedLabel {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let center = frame.center();
        frame.with_save(|frame| {
            frame.translate(Vector::new(center.x, center.y));
            // -90°: in this canvas's clockwise-from-+x, y-down convention
            // (matching `iced_graphics::geometry::path::Arc` and
            // `widgets::knob`'s own `angle_of`), this maps local "reading
            // direction" (+x) to global "up" — i.e. the text climbs the
            // strip bottom-to-top, matching the reference exactly rather
            // than guessed-and-checked.
            frame.rotate(-std::f32::consts::FRAC_PI_2);
            frame.fill_text(canvas::Text {
                content: self.text.clone(),
                position: Point::ORIGIN,
                color: theme::TEXT_PRIMARY,
                size: self.size.into(),
                align_x: advanced_text::Alignment::Center,
                align_y: alignment::Vertical::Center,
                ..canvas::Text::default()
            });
        });
        vec![frame.into_geometry()]
    }
}

fn rotated_label<'a, Msg: 'a + Clone>(text: String, size: f32, scale: f32) -> Element<'a, Msg> {
    Canvas::new(RotatedLabel { text, size })
        .width(Length::Fill)
        .height(Length::Fixed(ROTATED_LABEL_LEN * scale))
        .into()
}

/// Full-strip header — just the name (+ type tag), centered, TotalMix-style
/// (see the reference design: no icons in the header at rest). See
/// `collapsed_header_row` for the collapsed strip's own header, which
/// still needs its chevron.
///
/// Double-clicking the name renames the strip (`Message::RenameStart`):
/// while `renaming`, the name is a text field, Enter keeps the typed name
/// and Escape drops it. The name's own `mouse_area` captures its clicks
/// (iced's `mouse_area` captures every press once it has a double-click
/// handler), so it forwards single clicks as `StripClicked` itself; that
/// keeps Ctrl/Shift-click selection working on the name, and stops a
/// double-click there from also collapsing the strip.
fn header_row<'a>(
    cid: ChannelId,
    name: &str,
    name_custom: bool,
    type_tag: Option<(&'static str, Color)>,
    renaming: bool,
    rename_buf: &'a str,
    scale: f32,
) -> Element<'a, Message> {
    let shown_name = if name_custom {
        name
    } else {
        short_label(name)
    };
    if renaming {
        return text_input(shown_name, rename_buf)
            .id(crate::app::RENAME_INPUT_ID)
            .on_input(Message::RenameChanged)
            .on_submit(Message::RenameCommit)
            .style(theme::text_input)
            .size(theme::TEXT_SM * scale)
            .padding([theme::SPACE_TIGHT * scale, theme::SPACE_SM * scale])
            .width(Length::Fill)
            .into();
    }
    // `.wrapping(Wrapping::None)` — without it, a long name ("Instr. 3/4",
    // "ADAT7/8") wraps to two lines partway through the collapse/expand
    // width animation, once the shrinking card gets narrower than the
    // name's natural width, growing the whole strip's *height* for the
    // ~90ms the animation is mid-flight before it settles back down —
    // user report: "pendant que ça se minimise le bus augmente de hauteur
    // un peu." No text in this card is meant to ever wrap (every label
    // here is a single short line by design), so this is a correctness
    // fix for the whole card, not just the header specifically — see the
    // matching `.wrapping(None)` added to every other `text()` in
    // `full_strip` below for the same reason.
    let mut header = row![text(shown_name.to_string())
        .color(theme::TEXT_PRIMARY)
        .size(theme::TEXT_MD * scale)
        .wrapping(advanced_text::Wrapping::None)]
    .spacing(theme::SPACE_HAIRLINE);
    if let Some((tag, color)) = type_tag {
        header = header.push(
            text(tag)
                .color(color)
                .size(theme::TEXT_XS * scale)
                .wrapping(advanced_text::Wrapping::None),
        );
    }
    let header = mouse_area(header)
        .on_press(Message::StripClicked(cid))
        .on_double_click(Message::RenameStart(cid))
        .interaction(mouse::Interaction::Text);
    container(hint(header, "Double-click to rename", scale))
        .width(Length::Fill)
        .center_x(Length::Fill)
        .align_y(iced::Alignment::Center)
        .into()
}

/// A collapsed strip is a glance-only readout, but per the reference
/// design keeps Mute/Solo live (unlike the fader/pan/route, still traded
/// away for space) — collapsing is for strips you're not actively
/// adjusting the level of, not ones you'd never want to silence in a
/// hurry. Only ever rendered fully settled (see `strip()`'s dispatch), so
/// `w` is always `COLLAPSED_W`, but it's threaded through rather than
/// hardcoded to keep this in lockstep with `full_strip`'s signature.
fn collapsed_strip<'a>(p: StripParams<'a>, w: f32) -> Element<'a, Message> {
    let cid = p.cid;
    let scale = p.scale;
    let btn_h = ICON_BTN_H * scale;

    let mute_btn = hint(
        button(centered_label("M", theme::TEXT_XS * scale))
            .width(Length::Fill)
            .height(btn_h)
            .style(theme::armed_toggle_button(p.mute, p.mute_armed, theme::MUTE_COLOR))
            .on_press(Message::Mute(cid, !p.mute)),
        if p.mute_armed {
            "Mute (switched off by the M master, which brings it back)"
        } else {
            "Mute"
        },
        scale,
    );
    let solo_btn = hint(
        button(centered_label("S", theme::TEXT_XS * scale))
            .width(Length::Fill)
            .height(btn_h)
            .style(theme::armed_toggle_button(p.solo, p.solo_armed, theme::SOLO_COLOR))
            .on_press(Message::Solo(cid, !p.solo)),
        if p.solo_armed {
            "Solo (switched off by the S master, which brings it back)"
        } else {
            "Solo"
        },
        scale,
    );
    // "+" rather than a chevron — the reference puts the expand trigger
    // at the *bottom* of the collapsed card, not a top corner (which is
    // also where the full strip's own gear/chevron column sits now, at
    // the fader's mid-height on the right — matching that position here
    // would put "expand" somewhere a collapsed card, with no fader, has
    // nothing to anchor to).
    let expand_btn = hint(
        button(centered_label("+", theme::TEXT_SM * scale))
            .padding(0)
            .width(Length::Fill)
            .height(btn_h)
            .style(theme::toggle_button(false, theme::ACCENT))
            .on_press(Message::ToggleCollapse(cid)),
        "Expand",
        scale,
    );

    // No solo on outputs (see `full_strip`); an empty slot keeps the
    // collapsed cards' meters aligned across the row.
    let solo_slot: Element<'a, Message> = if matches!(cid, ChannelId::Output(_)) {
        iced::widget::Space::new().height(btn_h).into()
    } else {
        solo_btn.into()
    };
    let rows = column![
        rotated_label(
            if p.name_custom {
                p.name.to_string()
            } else {
                short_label(&p.name).to_string()
            },
            theme::TEXT_SM * scale,
            scale,
        ),
        mute_btn,
        solo_slot,
        container(vu_meter(
            p.meter,
            COLLAPSED_METER_H * scale,
            scale,
            p.meter_available,
        ))
            .width(Length::Fill)
            .center_x(Length::Fill),
        expand_btn,
    ]
    .spacing(theme::SPACE_HAIRLINE)
    .width(Length::Fill)
    .align_x(iced::Alignment::Center);

    mouse_area(
        container(rows)
            .style(theme::strip_panel(
                p.selected,
                p.hovered,
                p.type_tag.map(|(_, c)| c),
                p.active_surface,
            ))
            .padding([theme::SPACE_SM * p.scale, theme::SPACE_MD * p.scale])
            .width(Length::Fixed(w * p.scale))
            .height(Length::Fixed(STRIP_H * p.scale))
            .clip(true),
    )
    .on_press(Message::StripClicked(cid))
    .on_double_click(Message::ToggleCollapse(cid))
    .on_enter(Message::StripHovered(Some(cid)))
    .on_exit(Message::StripHovered(None))
    .into()
}

/// Picks between the two strip layouts and, while a collapse/expand
/// animation is in flight, the width the outer card should be drawn at
/// this frame. The full (uncollapsed) content is shown not just when
/// resting expanded but for the whole transition in *either* direction —
/// shrinking, it's the thing visibly getting clipped down to
/// `COLLAPSED_W`; growing, it's what's being revealed. Only once a
/// collapse has fully settled does rendering switch to the lighter,
/// control-free `collapsed_strip`.
pub fn strip<'a>(p: StripParams<'a>) -> Element<'a, Message> {
    let now = Instant::now();
    let (w, show_full) = match &p.collapse_anim {
        Some(a) => (a.at(now), a.is_settling(now) || !p.collapsed),
        None => {
            if p.collapsed {
                (COLLAPSED_W, false)
            } else {
                (full_width(p.cid), true)
            }
        }
    };

    if show_full {
        full_strip(p, w)
    } else {
        collapsed_strip(p, w)
    }
}

fn full_strip<'a>(p: StripParams<'a>, w: f32) -> Element<'a, Message> {
    let cid = p.cid;
    let out = p.output_idx;
    let scale = p.scale;
    let btn_h = 18.0 * scale;

    // Every channel kind has at least the STEREO link/split toggle now
    // (see `app.rs::settings_popover`) — 48V/PAD/Sensitivity/Gain are
    // just the input-only additions to the same panel.
    let has_settings = true;
    let header = header_row(
        cid,
        &p.name,
        p.name_custom,
        p.type_tag,
        p.renaming,
        p.rename_buf,
        scale,
    );

    let mute_btn = hint(
        button(centered_label("M", theme::TEXT_SM * scale))
            .width(Length::Fill)
            .height(btn_h)
            .style(theme::armed_toggle_button(p.mute, p.mute_armed, theme::MUTE_COLOR))
            .on_press(Message::Mute(cid, !p.mute)),
        if p.mute_armed {
            "Mute (switched off by the M master, which brings it back)"
        } else {
            "Mute"
        },
        scale,
    );
    let solo_btn = hint(
        button(centered_label("S", theme::TEXT_SM * scale))
            .width(Length::Fill)
            .height(btn_h)
            .style(theme::armed_toggle_button(p.solo, p.solo_armed, theme::SOLO_COLOR))
            .on_press(Message::Solo(cid, !p.solo)),
        if p.solo_armed {
            "Solo (switched off by the S master, which brings it back)"
        } else {
            "Solo"
        },
        scale,
    );
    // Fixed-width buttons left dead space flanking them whenever the card
    // was sized for a wider sibling row (48V/PAD, or just a long channel
    // name) — filling the row makes every row use the card's full width
    // instead of only the widest one.
    // Outputs have no solo: TotalMix has none, and neither backend that
    // drives the hardware does anything with one (`set_solo` returns early
    // for outputs), so an output's S lit only in the simulation.
    let mut ms_row = if matches!(cid, ChannelId::Output(_)) {
        row![mute_btn]
    } else {
        row![mute_btn, solo_btn]
    }
    .spacing(theme::SPACE_TIGHT)
    .width(Length::Fill);
    if p.has_cue {
        ms_row = ms_row.push(hint(
            button(centered_label("C", theme::TEXT_SM * scale))
                .width(Length::Fill)
                .height(btn_h)
                .style(theme::toggle_button(p.cue, theme::ACCENT))
                .on_press(Message::CueChanged(cid, !p.cue)),
            "CUE — exclusively monitor this output's own playback feed through AN1/2",
            scale,
        ));
    }

    let mut rows = column![header].spacing(theme::SPACE_HAIRLINE);

    // Pan sits directly under the header, TotalMix-style, rather than
    // trailing at the very bottom — with pan above the fader and the
    // route picker (below) after it, the fader ends up vertically
    // centered in the strip's own flow instead of front-loaded.
    //
    // Outputs get this row too.  On the UC it is a real output balance:
    // the two physical channels carry different 0x1a levels, and the knob
    // is derived from their difference.  Backends without a balance path
    // keep the knob dimmed (the same footprint, so every strip stays the
    // same height).
    let pan_str = match p.pan.cmp(&0) {
        std::cmp::Ordering::Less => format!("L{}", -p.pan),
        std::cmp::Ordering::Greater => format!("R{}", p.pan),
        std::cmp::Ordering::Equal => "C".to_string(),
    };
    let is_output = matches!(cid, ChannelId::Output(_));
    let balance_available = !is_output || p.output_balance_supported;
    let pan_knob = Knob {
        value: if balance_available { p.pan as f32 } else { 0.0 },
        range: (-100.0, 100.0),
        label: if balance_available {
            pan_str
        } else {
            "C".to_string()
        },
        arc_from_center: true,
        level_arc_color: Some(theme::PHANTOM),
        interactive: balance_available,
        log_scale: false,
        modifiers: p.modifiers,
        scale,
        on_change: if balance_available {
            Box::new(move |v| Message::PanChanged(cid, out, v.round() as i8))
        } else {
            Box::new(|_| unreachable!("balance unavailable suppresses all input"))
        },
        on_reset: if balance_available {
            Box::new(move || Message::PanReset(cid, out))
        } else {
            Box::new(|| unreachable!("balance unavailable suppresses all input"))
        },
    };
    let pan_hint = if is_output {
        if p.output_balance_supported {
            "Output balance — drag, scroll, Shift: fine, double-click to reset"
        } else {
            "Output balance — not implemented for this output/backend"
        }
    } else {
        "Pan — drag, scroll, Shift: fine, double-click to reset"
    };
    rows = rows.push(
        container(hint(knob(pan_knob), pan_hint, scale))
            .width(Length::Fill)
            .center_x(Length::Fill),
    );

    rows = rows.push(ms_row);
    // 48V/PAD/Sensitivity/Gain all live in the gear flyout now (see
    // `app.rs::settings_popover`), not inline here — keeps every strip's
    // own height fixed regardless of channel type, and matches the route
    // panel's own "opens beside the strip" shape instead of an inline
    // accordion that used to grow/shrink the card itself. Gain used to be
    // the one holdout (a `Knob` placed inside the old Stack-based flyout
    // broke click handling for the whole row), but the flyout isn't a
    // `Stack` anymore — it pushes the row instead of overlaying it — so
    // that failure mode doesn't apply and Gain moved in with the rest.

    let default_vol = p.default_vol;
    let fader_h = (STRIP_H - FADER_CHROME_H) * scale;
    let fader_widget = fader(Fader {
        value: p.vol,
        range: p.drag_range.unwrap_or((0.0, 2.0)),
        default_value: default_vol,
        meter: p.meter,
        meter_available: p.meter_available,
        meter2: p.meter2,
        meter2_available: p.meter2_available,
        height: fader_h,
        show_meter: true,
        fill: theme::FADER,
        // The icon column (`icon_col`, built below) is this canvas's
        // `row!` sibling — same width regardless of how many buttons it
        // holds (T/gear/EQ all share `ICON_BTN_W`), plus the row's own
        // spacing between them. See `Fader::reserved_right`'s own doc
        // comment for why the canvas needs to know this at all.
        // `ICON_BTN_W` scales with the strip (every icon button's width
        // does), but the row's own `.spacing(theme::SPACE_TIGHT)` below
        // — like several other spacings in this file — doesn't scale;
        // matching that exactly (not just approximately) is the point,
        // since this value feeds a pixel-level centering fix.
        reserved_right: SETTINGS_TAB_W * scale + theme::SPACE_TIGHT,
        modifiers: p.modifiers,
        scale,
        show_track: true,
        label: None,
        label_color: theme::TEXT_SEC,
        // Unused: only matters when `show_meter: false` (see its own
        // doc comment) — this strip fader is never that.
        compact_width: 0.0,
        on_press: Box::new(move |v, range| Message::FaderPressed(cid, out, v, range)),
        on_drag: Box::new(move |v| Message::VolumeChanged(cid, out, v)),
        on_release: Box::new(move || Message::RangeCleared(cid)),
        on_reset: Box::new(move || Message::Reset(cid, out, default_vol)),
    });

    // Settings gear and EQ stacked on the strip's right flank, beside the
    // fader — confirmed against a live TotalMix screenshot at high zoom
    // (an earlier "put them in a row below the fader instead" call in
    // this file was a misreading of a narrower crop; a full-height zoom
    // on a real strip shows the gear/EQ column sitting to the right of
    // the meter, roughly spanning the fader's lower half, not underneath
    // it — the user caught this directly). `STRIP_W` was widened (see
    // its own doc comment) specifically so this column has room without
    // pushing the meter/ruler group past the fader canvas's own bounds.
    let mut icon_col = column![].spacing(theme::SPACE_MD).align_x(iced::Alignment::Center);
    // Trim ("T") sits above the gear icon — every hardware input has one
    // (confirmed via the reference: ADAT/AS strips show "T" same as the
    // analog ones, unlike Gain which only exists on Mic/Instrument), but
    // not Playback or Output. TotalMix *does* show a "T" on Software
    // Playback strips too, but per the user's own call once told
    // `RmeDevice::set_trim` only maps to real hardware input sources
    // (`tuxmix_usb::usb.rs::input_source`, not playback indices): ship
    // the real, working control on Hardware Inputs, skip it on Playback
    // rather than either faking one there or blocking on new protocol
    // work.
    // The four symbols keep one grid across every strip: a channel that
    // lacks one keeps an empty slot of the same height, so the symbols
    // below never move (hardware outputs have no T, playbacks no EQ/D).
    let blank_slot = || -> Element<'a, Message> {
        iced::widget::Space::new()
            .height(Length::Fixed(ICON_BTN_H * scale))
            .into()
    };
    if p.has_trim {
        let trim_open = p.open_flyout == Some(FlyoutKind::Trim);
        // Lights up whenever trim is off unity too, not just while its
        // flyout is open — same "shows live state at a glance" idea as the
        // EQ trigger's own `eq_open || eq_enabled`.
        icon_col = icon_col.push(tab_button(
            "T",
            trim_open,
            p.trim < 0.0,
            Message::ToggleFlyout(cid, FlyoutKind::Trim),
            if trim_open {
                std::borrow::Cow::Borrowed("Hide trim")
            } else {
                std::borrow::Cow::Owned(p.trim_tip.clone())
            },
            scale,
        ));
    } else if p.slot_trim {
        icon_col = icon_col.push(blank_slot());
    }
    // Phase now lives in the Settings flyout, at the bottom of the manual's
    // Input row, rather than as an extra glyph in this narrow column.
    // No "SP" button here any more: a playback pair's stereo split IS its
    // link state (`RmeDevice::playback_linked` is `!split`), so it was the
    // STEREO switch of the settings flyout a second time, lit under
    // another name.
    if has_settings {
        let settings_open = p.open_flyout == Some(FlyoutKind::Settings);
        icon_col = icon_col.push(tab_button_sized(
            "⚙",
            settings_open,
            false,
            Message::ToggleFlyout(cid, FlyoutKind::Settings),
            if settings_open { "Hide settings" } else { "Show settings (48V/PAD/Sensitivity)" },
            scale,
            SETTINGS_TAB_W,
            SETTINGS_TAB_H,
            SETTINGS_GLYPH,
        ));
    }
    if p.has_eq && p.protocol_features {
        let eq_open = p.open_flyout == Some(FlyoutKind::Eq);
        icon_col = icon_col.push(tab_button(
            "EQ",
            eq_open,
            p.eq_enabled,
            Message::ToggleFlyout(cid, FlyoutKind::Eq),
            if eq_open { "Hide EQ" } else { "Show EQ (3-band + low cut)" },
            scale,
        ));
    } else if p.slot_eq {
        icon_col = icon_col.push(blank_slot());
    }
    if p.has_dynamics && p.protocol_features {
        let dyn_open = p.open_flyout == Some(FlyoutKind::Dynamics);
        icon_col = icon_col.push(tab_button(
            "D",
            dyn_open,
            p.dynamics_on,
            Message::ToggleFlyout(cid, FlyoutKind::Dynamics),
            if dyn_open { "Hide Dynamics" } else { "Show Dynamics (Compressor, Expander, Auto Level)" },
            scale,
        ));
    } else if p.slot_dyn {
        icon_col = icon_col.push(blank_slot());
    }
    // The FX fader: send on inputs and playbacks, return on outputs. A thin
    // track beside the main fader, no meter, its own value drag — measured
    // from the TotalMix strip (small cap, no scale of its own).
    // The FX fader: send on inputs and playbacks, return on outputs. A thin
    // track to the right of the main fader, starting at the same height,
    // with a short line under it — orange for a send, green for a return.
    let fx_fader: Option<Element<'a, Message>> = p.fx_level.map(move |db| {
        let value = 10f32.powf(db / 20.0).clamp(0.0, 2.0);
        let inner = fader(Fader {
            value,
            range: (0.0, 1.0),
            default_value: 10f32.powf(-65.0 / 20.0),
            meter: MeterFrame::still(0.0),
            meter_available: false,
            meter2: None,
            meter2_available: false,
            // Leaves the T / gear / EQ / D buttons their room at the
            // bottom of the column.
            // 20 % taller than the earlier track: the FX send/return is
            // the only small fader on the strip, and the extra travel
            // matches the main fader's own feel.  Every strip on an FX
            // unit grows by the same amount, so the symbol grid below it
            // keeps its shared height.
            height: ((fader_h - 150.0 * scale).max(48.0 * scale) * 1.2),
            show_meter: false,
            // Same dark slot as the main fader, but a smaller body.
                fill: match cid {
                ChannelId::Output(_) => theme::MGREEN,
                _ => theme::PHANTOM,
            },
            reserved_right: 0.0,
            modifiers: p.modifiers,
            // A smaller body: the cap and rail scale down with it.
            scale: scale * 0.62,
            show_track: true,
            label: None,
            label_color: theme::TEXT_SEC,
            compact_width: 26.0,
            on_press: Box::new(move |v, _| fx_message(cid, db_of(v))),
            on_drag: Box::new(move |v| fx_message(cid, db_of(v))),
            on_release: Box::new(move || Message::RangeCleared(cid)),
            on_reset: Box::new(move || fx_message(cid, -65.0)),
        });
        // The fader's own coloured fill says everything; the dark channel
        // behind it is what marks the slot above the cap.
        column![container(inner).width(Length::Fixed(26.0 * 0.62 * scale))]
            .align_x(iced::Alignment::Center)
            .into()
    });

    // The FX fader sits at the top of the strip's right-hand control column,
    // above T / gear / EQ / D, so the main fader keeps the full height.
    // The FX fader is pinned to the top of the row — the same line as the
    // big fader's top in every row — and the symbol column starts directly
    // below it.  The slack stays under the column, so the four symbols share
    // one absolute height across all strips.
    // A little air under the FX fader (and under the fader top on units
    // without one), so the symbols do not crowd the slider above them.
    let top_gap: Element<'a, Message> = iced::widget::Space::new()
        .height(Length::Fixed(theme::SPACE_LG * scale))
        .into();
    let icon_col = column![
        fx_fader.unwrap_or_else(|| iced::widget::Space::new().height(Length::Fixed(0.0)).into()),
        top_gap,
        icon_col,
        iced::widget::Space::new().height(Length::Fill),
    ]
    .spacing(theme::SPACE_TIGHT)
    .align_x(iced::Alignment::Center);
    rows = rows.push(
        row![fader_widget, icon_col]
            .spacing(theme::SPACE_TIGHT)
            .align_y(iced::Alignment::Start)
            .width(Length::Fill),
    );

    let db_display: Element<'a, Message> = if p.editing {
        text_input("", p.edit_buf)
            .id(crate::app::DB_INPUT_ID)
            .on_input(Message::EditChanged)
            .on_submit(Message::EditCommit)
            .style(theme::text_input)
            .size(theme::TEXT_SM * scale)
            .width(Length::Fixed(64.0 * scale))
            .into()
    } else {
        let initial = if p.vol > 0.0 {
            format!("{:.1}", 20.0 * p.vol.log10())
        } else {
            "-inf".into()
        };
        mouse_area(
            text(if p.vol > 0.0 {
                format!("{:.1}", 20.0 * p.vol.log10())
            } else {
                "-∞".into()
            })
                .color(theme::TEXT_PRIMARY)
                .size(theme::TEXT_XS * scale)
                .wrapping(advanced_text::Wrapping::None),
        )
        .on_double_click(Message::EditStart(cid, initial))
        .into()
    };
    // Collapse trigger — a small pill next to the dB readout, TotalMix-
    // style (confirmed via the reference: "0.0" then a "-" pill on the
    // same row, at the bottom of the strip, not up near the fader/gear
    // column). Moved out of `icon_col` at the user's request; plain "-"
    // rather than the em dash it briefly was — now that it's sitting in
    // the exact spot and shape the reference uses, matching that glyph
    // too seemed like the better default over inventing a new one.
    //
    // Plain `text(...)` here, not `centered_label` — `centered_label`
    // wraps its text in a `container(...).center(Length::Fill)`, which
    // needs a *bounded* parent to fill. Every other `centered_label`
    // button in this file pins that down with an explicit `.width()`/
    // `.height()` on the button; this one doesn't (it's meant to size to
    // its own content, pill-style, like the route/LOOP buttons below).
    // Skipping that pin left a `Length::Fill` with nothing to fill,
    // which didn't just mis-size this one button — it broke layout for
    // the *entire* strip list (every strip rendered as a blank card,
    // confirmed live and bisected line-by-line before landing on this).
    //
    // `.width(ICON_BTN_W * scale)` added back on below (unlike the
    // "skip it" lesson above, this one's deliberate) so this button has
    // a known, fixed footprint — needed to build a *symmetric* row: a
    // same-width blank spacer on the left, this button on the right,
    // with `db_display` centered in what's left between them. That
    // symmetry is what makes `db_display` land exactly under the
    // fader's own center rather than the meter's — see the row below.
    //
    // 2026-10-20: same box, glyph and style as the icon column above it
    // (`toggle_button` at `ICON_BTN_W` x `ICON_BTN_H`, a true minus sign
    // centered in it) instead of a `plain_button` of its own height, so
    // the strip's small controls are one family.
    // A little wider than the other icon buttons when the strip has the
    // room; the left spacer matches it exactly so the dB readout stays
    // centred on the fader.
    let collapse_w = (ICON_BTN_W + 4.0) * scale;
    let collapse_btn = hint(
        button(centered_label("−", theme::TEXT_SM * scale))
            .padding(0)
            .width(Length::Fixed(collapse_w))
            .height(ICON_BTN_H * scale)
            .style(theme::toggle_button(false, theme::ACCENT))
            .on_press(Message::ToggleCollapse(cid)),
        "Collapse",
        scale,
    );
    // Centered on the fader, not the meter — the fader's own row above
    // is centered on the *full* row width (`Fader::reserved_right`
    // accounts for `icon_col` so the track lands at exactly
    // `content_width / 2`), so centering `db_display` in a container
    // spanning that same full width lines the two up automatically.
    // A plain `row![db_display, Space::Fill, collapse_btn]` (the
    // previous shape) instead centered it in the *meter's* half of the
    // row — TotalMix-accurate positionally (matches the reference), but
    // not what the user asked for this time, which was explicitly
    // "under the fader." The left spacer matches `collapse_btn`'s own
    // fixed width exactly so the centered container's *available* width
    // stays symmetric around the row's true center, rather than losing
    // width asymmetrically to a right-hand sibling the way the fader
    // row itself used to (see `Fader::layout_x`'s doc comment).
    rows = rows.push(
        row![
            iced::widget::Space::new().width(Length::Fixed(collapse_w)),
            container(db_display)
                .width(Length::Fill)
                .center_x(Length::Fill),
            collapse_btn,
        ]
        .spacing(theme::SPACE_TIGHT)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill),
    );

    // TotalMix-style per-strip route: a compact trigger, centered on the
    // strip, opening `app.rs::route_popover` — a flyout that slides out
    // over the strip to the right rather than a plain dropdown (see the
    // route-panel plan). Drives the same `state.sel_out` the top bar's own
    // Submix picker does — a shortcut for it, not an independent
    // per-channel destination (TuxMix's model has exactly one "current
    // bus" concept, not TotalMix's separate "free" per-channel routing
    // mode). Centering it (rather than right-aligning) is purely visual —
    // the flyout itself still opens from the strip's right edge, unrelated
    // to where this trigger sits within the strip.
    if !matches!(cid, ChannelId::Output(_)) {
        rows = rows.push(
            container(hint(
                button(
                    row![
                        text(p.bus_label.clone())
                            .size(theme::TEXT_XS * scale)
                            .wrapping(advanced_text::Wrapping::None),
                        iced::widget::Space::new().width(Length::Fill),
                        text("▸").size(theme::TEXT_XS * scale),
                    ]
                    .spacing(theme::SPACE_TIGHT)
                    .align_y(iced::Alignment::Center)
                    .width(Length::Fill)
                    .height(Length::Fill),
                )
                .width(Length::Fill)
                .height(Length::Fixed(btn_h))
                .padding([0.0, theme::SPACE_SM * scale])
                .style(theme::toggle_button(p.open_flyout == Some(FlyoutKind::Route), theme::ACCENT))
                .on_press(Message::ToggleFlyout(cid, FlyoutKind::Route)),
                "Change output bus",
                scale,
            ))
            .padding(iced::Padding {
                top: theme::SPACE_MD * scale,
                ..iced::Padding::ZERO
            })
            .width(Length::Fill)
            .center_x(Length::Fill),
        );
    }

    // Double-click anywhere on the card that isn't already claimed by a
    // specific control (the fader/pan canvases capture their own
    // double-click for reset-to-default, the dB readout for its edit
    // field, buttons for their own press) collapses the strip — a bigger,
    // more discoverable target than the tiny "-" button alone. A plain
    // click there is a no-op; Ctrl/Shift+click toggles multi-selection
    // (see `Message::StripClicked`) — mute/solo/collapse on any selected
    // strip then apply to the whole selection at once.
    mouse_area(
        container(
            rows.width(Length::Fill)
                .align_x(iced::Alignment::Center),
        )
        .style(theme::strip_panel(
            p.selected,
            p.hovered,
            p.type_tag.map(|(_, c)| c),
            p.active_surface,
        ))
        // Bottom padding wider than the top three sides — the route
        // trigger (the card's last row) otherwise sat almost flush against
        // the card's own bottom edge, reading as cramped next to the dB
        // readout right above it.
        .padding(iced::Padding {
            top: theme::SPACE_SM * scale,
            // Keep the strip's content stationary when a slideout opens;
            // the panel overlaps this right padding instead (see app.rs).
            right: theme::SPACE_MD * scale,
            bottom: theme::SPACE_XL * scale,
            left: theme::SPACE_MD * scale,
        })
        .width(Length::Fixed(w * scale))
        .height(Length::Fixed(STRIP_H * scale))
        .align_y(iced::Alignment::Start)
        .clip(true),
    )
    .on_press(Message::StripClicked(cid))
    .on_double_click(Message::ToggleCollapse(cid))
    .on_enter(Message::StripHovered(Some(cid)))
    .on_exit(Message::StripHovered(None))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    #[test]
    fn collapsed_strip_fills_the_standard_strip_height() {
        assert!((COLLAPSED_METER_H + COLLAPSED_CHROME_H - STRIP_H).abs() < f32::EPSILON);
        assert!(COLLAPSED_METER_H > 0.0);
    }

    #[test]
    fn settled_anim_is_never_settling() {
        let a = CollapseAnim { prev: STRIP_W, value: STRIP_W, since: Instant::now() };
        assert!(!a.is_settling(a.since));
        assert!(!a.is_settling(a.since + Duration::from_millis(10)));
    }

    #[test]
    fn transitioning_anim_settles_after_its_width_based_duration() {
        let a = CollapseAnim { prev: STRIP_W, value: COLLAPSED_W, since: Instant::now() };
        let duration = Duration::from_millis(a.duration_ms() as u64);
        assert!(a.is_settling(a.since), "just started — should still be settling");
        assert!(
            a.is_settling(a.since + duration / 2),
            "mid-transition — should still be settling"
        );
        assert!(
            !a.is_settling(a.since + duration + Duration::from_millis(1)),
            "past the width-based window — should have stopped requesting redraws"
        );
    }

    #[test]
    fn at_interpolates_linearly_from_prev_to_value() {
        let a = CollapseAnim { prev: STRIP_W, value: COLLAPSED_W, since: Instant::now() };
        let duration = Duration::from_millis(a.duration_ms() as u64);
        assert_eq!(a.at(a.since), STRIP_W);
        assert_eq!(a.at(a.since + duration), COLLAPSED_W);
        let mid = a.at(a.since + duration / 2);
        let expected_mid = (STRIP_W + COLLAPSED_W) / 2.0;
        assert!(
            (mid - expected_mid).abs() < 0.5,
            "expected ~{expected_mid} at the midpoint, got {mid}"
        );
    }

    #[test]
    fn slideout_speed_does_not_depend_on_panel_width() {
        let small = CollapseAnim { prev: 0.0, value: 90.0, since: Instant::now() };
        let large = CollapseAnim { prev: 0.0, value: 180.0, since: Instant::now() };
        assert!(
            (large.duration_ms() / small.duration_ms() - 2.0).abs() < 1e-4,
            "a panel twice as wide must take twice as long"
        );
    }
}
