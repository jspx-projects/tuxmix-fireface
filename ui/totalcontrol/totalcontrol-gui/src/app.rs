use iced::futures::channel::mpsc;
use iced::keyboard::{self, Key};
use iced::widget::{
    button, checkbox, column, container, mouse_area, opaque, pick_list, progress_bar, responsive,
    row, scrollable, slider, stack, text, text_input,
};
use iced::{window, Background, Color, Element, Length, Subscription, Task};
use rosc::OscType;

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use totalcontrol_core::channel::EqBandType;
use totalcontrol_core::fireface::UcxDump;
#[cfg(feature = "alsa")]
use totalcontrol_core::{BabyfacePro, Fireface, FirefaceModel, UcxAlsa};
use totalcontrol_core::{
    BabyfaceProUsb, ChannelId, ChannelType, Dynamics, Echo, InputLevel, MockBabyfacePro, RmeDevice,
    Reverb, Scene, Sensitivity, TalkbackConfig,
};

use crate::about;
use crate::matrix;
use crate::osc::{self, OscConfig, OscOutbound};
use crate::prefs::{ControlRoomSettings, SyncDirection};
use totalcontrol_core::pw_clock::{self, CardStream, PwClock};
use totalcontrol_core::pw_meter;
use crate::scenes::{load_scene_file, save_scene_file};
use crate::sidebar::{self, Group};
use crate::sync_dialog;
use crate::theme;
use crate::widgets::dynamics_plot::dynamics_plot;
use crate::widgets::eq_plot::{eq_plot, EqPlotActions};
use crate::widgets::fader;
use crate::widgets::knob::{knob, Knob};
use crate::widgets::strip::{self, hint};

/// Strips redundant prefixes baked into a channel's stored name before it's
/// shown in a strip header — "PCM " for playback channels, "OUT " for
/// output bus pairs (`profile.rs`'s `format!("OUT {}", pair.left)`). Both
/// are already conveyed by the strip's own type tag badge right next to
/// the name, so keeping them in the text just wastes width for no extra
/// information — on an output like "OUT ADAT3" that was enough to push the
/// collapse button (`header_row`) almost entirely off the card.
pub fn short_label(name: &str) -> &str {
    name.strip_prefix("PCM ")
        .or_else(|| name.strip_prefix("OUT "))
        .unwrap_or(name)
}

/// Combines a linked output pair's two per-channel names ("AN1"/"AN2",
/// "PH3"/"PH4", "ADAT7"/"ADAT8") into the historical combined bus label
/// ("AN1/2", "PH3/4", "ADAT7/8") — matches what `totalcontrol-usb`'s output
/// list used before it became per-channel (see `usb.rs`'s `open()`),
/// and what TotalMix itself shows for a linked stereo bus. Falls back to
/// `"{left}/{right}"` unmodified if `right` doesn't end in digits (not
/// expected for this device's own channel names, but harmless).
pub(crate) fn pair_bus_label(left: &str, right: &str) -> String {
    // A digital pair has one RME name, not L/R suffixes: both channels are
    // called "SPDIF", so show that once rather than "SPDIF/SPDIF".
    if left == right {
        return left.to_string();
    }
    let right_num: String = right.chars().skip_while(|c| !c.is_ascii_digit()).collect();
    if right_num.is_empty() {
        format!("{left}/{right}")
    } else {
        format!("{left}/{right_num}")
    }
}

pub fn type_tag(t: ChannelType) -> (&'static str, iced::Color) {
    // The muted type family: quiet enough to live in a graphite UI
    // without fighting the white "engaged" states for attention, but
    // still separable hues for scanning a long strip row. Mic, line and
    // instrument keep their console associations (blue, neutral, amber);
    // the two digital ports share a violet.
    match t {
        ChannelType::Mic => ("MIC", iced::Color::from_rgb8(0x7f, 0xb3, 0xd5)),
        ChannelType::Instrument => ("INST", iced::Color::from_rgb8(0xd9, 0xa8, 0x5c)),
        ChannelType::Line => ("LINE", iced::Color::from_rgb8(0x9d, 0xa3, 0xac)),
        ChannelType::SPDIF => ("SPDIF", iced::Color::from_rgb8(0xa2, 0x92, 0xc6)),
        ChannelType::ADAT => ("ADAT", iced::Color::from_rgb8(0xa2, 0x92, 0xc6)),
    }
}

/// Tag colors for the bus rows (not real `ChannelType`s, so not part of
/// `type_tag`) — kept distinct from the input-type palette above so PB/OUT
/// don't just blend into the secondary-text gray everything else uses.
pub const PB_TAG: iced::Color = iced::Color::from_rgb8(0x5f, 0xb3, 0xa8);

pub fn parse_db_input(s: &str) -> Option<f32> {
    let raw = s.trim().to_lowercase();
    if raw.is_empty() || raw == "-inf" || raw == "-\u{221e}" {
        return Some(0.0);
    }
    raw.replace(',', ".")
        .parse::<f32>()
        .ok()
        .map(|db| (10f32.powf(db / 20.0)).clamp(0.0, 2.0))
}

/// Compact frequency label for knob faces: 1000 -> "1k",
/// 1500 -> "1.5k", 120 -> "120".  The row label already says Hz.
fn freq_text(hz: u16) -> String {
    if hz >= 1_000 {
        let k = hz as f32 / 1_000.0;
        if (k - k.round()).abs() < 0.05 {
            format!("{}k", k.round() as u32)
        } else {
            format!("{k:.1}k")
        }
    } else {
        hz.to_string()
    }
}

// ── Device enum ──────────────────────────────────────────────────


/// A user-selectable mixer backend/device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    /// First Fireface visible through the kernel hwdep ABI (UC or UCX).
    Fireface,
    Ucx,
    Uc,
    Babyface,
    /// The in-memory device, as a Babyface Pro FS.
    Mock,
    /// The in-memory device, as a Fireface UCX: EQ, dynamics, FX and the
    /// Control Room, all without hardware.
    MockUcx,
    /// The in-memory device, as a Fireface UC: the UC's gated feature set.
    MockUc,
}

/// Choose the startup backend from the devices that are actually present.
///
/// Policy:
/// - an explicitly preferred device wins while it is present;
/// - otherwise, if exactly one real device is present, select it — even when
///   the saved preference names a different model;
/// - with several devices and no matching preference, keep the auto entry;
/// - a saved Simulation preference gives way whenever a real device is
///   present; the simulation is the fallback for "nothing connected".
#[cfg(test)]
fn choose_startup_kind(
    available: &[DeviceKind],
    preferred: Option<DeviceKind>,
) -> Option<DeviceKind> {
    // A saved simulation is only used while nothing real is attached.
    if matches!(
        preferred,
        Some(DeviceKind::Mock) | Some(DeviceKind::MockUcx) | Some(DeviceKind::MockUc)
    ) {
        if let Some(real) = available.iter().copied().find(|k| {
            !matches!(
                k,
                DeviceKind::Mock | DeviceKind::MockUcx | DeviceKind::MockUc
            )
        }) {
            return Some(real);
        }
        return preferred;
    }
    if let Some(p) = preferred {
        if available.is_empty() || available.contains(&p) {
            return Some(p);
        }
    }
    if available.len() == 1 {
        return available.first().copied();
    }
    if preferred == Some(DeviceKind::Fireface) {
        return Some(DeviceKind::Fireface);
    }
    // A fresh profile with nothing attached opens the UCX simulation: it is
    // the reference layout (three rows, EQ/dynamics, FX) and the device the
    // control sections are specified against.
    let only_simulations = !available.is_empty()
        && available.iter().all(|k| {
            matches!(
                k,
                DeviceKind::Mock | DeviceKind::MockUcx | DeviceKind::MockUc
            )
        });
    if preferred.is_none() && only_simulations && available.contains(&DeviceKind::MockUcx) {
        return Some(DeviceKind::MockUcx);
    }
    available.first().copied().or(preferred)
}

/// One selectable physical device: a model plus the serial of the unit.
/// The serial is what survives a card-index change or a USB reconnect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceTarget {
    pub kind: DeviceKind,
    pub serial: Option<String>,
}

impl DeviceTarget {
    pub fn kind_only(kind: DeviceKind) -> Self {
        Self { kind, serial: None }
    }

    pub fn from_identity(identity: totalcontrol_core::FirefaceIdentity) -> Self {
        let kind = match identity.model {
            totalcontrol_core::FirefaceModel::Uc => DeviceKind::Uc,
            totalcontrol_core::FirefaceModel::Ucx => DeviceKind::Ucx,
        };
        Self {
            kind,
            serial: (!identity.serial.is_empty()).then_some(identity.serial),
        }
    }

    pub fn label(&self) -> String {
        match &self.serial {
            Some(serial) => format!("{} ({serial})", self.kind.label()),
            None => self.kind.label().to_string(),
        }
    }
}

/// Choose a physical unit, preferring the remembered serial over the
/// model-wide kind.  A serial survives a card-index change and a USB
/// reconnect; the kind is only a fallback for an old preference file.
fn choose_startup_target(
    available: &[DeviceTarget],
    preferred: Option<DeviceKind>,
    preferred_serial: Option<&str>,
) -> Option<DeviceTarget> {
    if let Some(serial) = preferred_serial {
        if let Some(target) = available.iter().find(|t| t.serial.as_deref() == Some(serial)) {
            return Some(target.clone());
        }
    }
    if let Some(kind) = preferred {
        if !matches!(
            kind,
            DeviceKind::Mock | DeviceKind::MockUcx | DeviceKind::MockUc
        ) {
            if let Some(target) = available.iter().find(|t| t.kind == kind) {
                return Some(target.clone());
            }
        }
    }
    if available.len() == 1 {
        return available.first().cloned();
    }
    if preferred == Some(DeviceKind::Fireface) {
        return available
            .iter()
            .find(|t| t.kind == DeviceKind::Fireface)
            .cloned()
            .or_else(|| available.first().cloned());
    }
    available.first().cloned().or_else(|| preferred.map(DeviceTarget::kind_only))
}

impl DeviceKind {
    pub const ALL: [DeviceKind; 7] = [
        DeviceKind::Fireface,
        DeviceKind::Ucx,
        DeviceKind::Uc,
        DeviceKind::Babyface,
        DeviceKind::Mock,
        DeviceKind::MockUcx,
        DeviceKind::MockUc,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DeviceKind::Fireface => "Fireface (auto)",
            DeviceKind::Ucx => "Fireface UCX",
            DeviceKind::Uc => "Fireface UC",
            DeviceKind::Babyface => "Babyface Pro FS",
            DeviceKind::Mock => "Babyface Pro FS (simulated)",
            DeviceKind::MockUcx => "Fireface UCX (simulated)",
            DeviceKind::MockUc => "Fireface UC (simulated)",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.label() == label)
    }
}

pub enum DeviceHandle {
    #[cfg(feature = "alsa")]
    Real(BabyfacePro),
    #[cfg(feature = "alsa")]
    Ucx(UcxAlsa),
    /// The Fireface UC: same ALSA control surface as the UCX, ISO/IF5
    /// streaming, no EQ/FX.
    #[cfg(feature = "alsa")]
    Uc(UcxAlsa),
    /// Fireface UC/UCX through the kernel hwdep transport.  This is the
    /// refactored path: the kernel owns USB/PCM, TotalControl owns the model.
    #[cfg(feature = "alsa")]
    Fireface(Fireface),
    Mock(MockBabyfacePro),
    /// The proprietary USB backend (the TotalMix protocol). On Linux
    /// with the device in proprietary mode this is the path that
    /// actually works.
    Usb(BabyfaceProUsb),
    /// A UI-side copy of a device owned by the device service.
    Mirror(crate::device_service::Mirror),
}

impl DeviceHandle {
    /// Point the *mock* at the class-compliant personality's feature set.
    /// Only tests ever need this: every real backend answers for itself,
    /// and `TOTALCONTROL_MOCK_CC` covers looking at the gated UI by hand.
    #[cfg(test)]
    fn set_protocol_features(&mut self, on: bool) {
        if let DeviceHandle::Mock(dev) = self {
            dev.set_protocol_features(on);
        }
    }

    /// Point the *mock* at "unplugged", or back. Tests only; every real
    /// backend answers for itself, and `TOTALCONTROL_MOCK_DISCONNECT` covers
    /// looking at the disconnected chip by hand.
    #[cfg(test)]
    fn set_connected(&mut self, on: bool) {
        if let DeviceHandle::Mock(dev) = self {
            dev.set_connected(on);
        }
    }

    /// Ignore queued/early UC front-panel output events during the connect
    /// window.  Only the hwdep Fireface path needs this; the USB backend has
    /// no such event stream.
    pub fn suppress_front_panel_for(&mut self, duration: Duration) {
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(dev) = self {
            dev.suppress_front_panel_for(duration);
        }
        let _ = duration;
    }

    pub fn set_talkback_receive(
        &mut self,
        output: usize,
        on: bool,
    ) -> Result<(), totalcontrol_core::Error> {
        if let DeviceHandle::Mirror(d) = self {
            d.set_talkback_receive(output, on);
            return Ok(());
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.set_talkback_receive(output, on);
        }
        if let Some(out) = self.outputs_mut().get_mut(output) {
            out.talkback = on;
        }
        Ok(())
    }
}

macro_rules! delegate {
    ($self:expr, $method:ident($($arg:expr),*)) => { match $self {
        DeviceHandle::Mock(d) => d.$method($($arg),*),
        DeviceHandle::Usb(d) => d.$method($($arg),*),
        DeviceHandle::Mirror(d) => d.$method($($arg),*),
        #[cfg(feature = "alsa")]
        DeviceHandle::Real(d) => d.$method($($arg),*),
        #[cfg(feature = "alsa")]
        DeviceHandle::Ucx(d) => d.$method($($arg),*),
        #[cfg(feature = "alsa")]
        DeviceHandle::Uc(d) => d.$method($($arg),*),
        #[cfg(feature = "alsa")]
        DeviceHandle::Fireface(d) => d.$method($($arg),*),
    } };
    ($self:expr, $method:ident) => { match $self {
        DeviceHandle::Mock(d) => d.$method(),
        DeviceHandle::Usb(d) => d.$method(),
        DeviceHandle::Mirror(d) => d.$method(),
        #[cfg(feature = "alsa")]
        DeviceHandle::Real(d) => d.$method(),
        #[cfg(feature = "alsa")]
        DeviceHandle::Ucx(d) => d.$method(),
        #[cfg(feature = "alsa")]
        DeviceHandle::Uc(d) => d.$method(),
        #[cfg(feature = "alsa")]
        DeviceHandle::Fireface(d) => d.$method(),
    } };
}

impl RmeDevice for DeviceHandle {
    fn model_name(&self) -> &str {
        delegate!(self, model_name)
    }
    fn capabilities(&self) -> totalcontrol_core::DeviceCapabilities {
        delegate!(self, capabilities)
    }
    fn protocol_features(&self) -> bool {
        delegate!(self, protocol_features)
    }
    fn loopback_supported(&self) -> bool {
        delegate!(self, loopback_supported)
    }
    fn phase_supported(&self) -> bool {
        delegate!(self, phase_supported)
    }
    fn output_balance_supported(&self) -> bool {
        delegate!(self, output_balance_supported)
    }
    fn stereo_split_supported(&self) -> bool {
        delegate!(self, stereo_split_supported)
    }
    fn class_compliant(&self) -> bool {
        delegate!(self, class_compliant)
    }
    fn is_connected(&self) -> bool {
        delegate!(self, is_connected)
    }
    fn output_pair_count(&self) -> usize {
        delegate!(self, output_pair_count)
    }
    fn output_display_order(&self) -> Vec<usize> {
        delegate!(self, output_display_order)
    }
    fn matrix_uses_display_order(&self) -> bool {
        delegate!(self, matrix_uses_display_order)
    }
    fn open() -> Result<Self, totalcontrol_core::Error> {
        unreachable!()
    }
    fn inputs(&self) -> &[totalcontrol_core::InputChannel] {
        delegate!(self, inputs)
    }
    fn inputs_mut(&mut self) -> &mut [totalcontrol_core::InputChannel] {
        delegate!(self, inputs_mut)
    }
    fn playbacks(&self) -> &[totalcontrol_core::PlaybackChannel] {
        delegate!(self, playbacks)
    }
    fn playbacks_mut(&mut self) -> &mut [totalcontrol_core::PlaybackChannel] {
        delegate!(self, playbacks_mut)
    }
    fn outputs(&self) -> &[totalcontrol_core::OutputChannel] {
        delegate!(self, outputs)
    }
    fn outputs_mut(&mut self) -> &mut [totalcontrol_core::OutputChannel] {
        delegate!(self, outputs_mut)
    }
    fn settings(&self) -> &totalcontrol_core::DeviceSettings {
        delegate!(self, settings)
    }
    fn settings_mut(&mut self) -> &mut totalcontrol_core::DeviceSettings {
        delegate!(self, settings_mut)
    }
    /// The UC defers its input meter tap until the startup scene is latched
    /// (`TOTALCONTROL_DEFER_METER_TAP`).  The trait default is a no-op, so without
    /// this forward the Fireface backend's tap is never started and the AN5
    /// meter stays flat even though the hardware capture is live.
    fn begin_metering(&mut self) {
        delegate!(self, begin_metering)
    }
    fn set_volume(&mut self, ch: ChannelId, out: usize, v: f32) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_volume(ch, out, v))
    }
    fn volume(&self, ch: ChannelId, out: usize) -> Result<f32, totalcontrol_core::Error> {
        delegate!(self, volume(ch, out))
    }
    fn set_pan(&mut self, ch: ChannelId, out: usize, p: i8) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_pan(ch, out, p))
    }
    fn pan(&self, ch: ChannelId, out: usize) -> Result<i8, totalcontrol_core::Error> {
        delegate!(self, pan(ch, out))
    }
    fn set_mute(&mut self, ch: ChannelId, m: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_mute(ch, m))
    }
    fn mute(&self, ch: ChannelId) -> Result<bool, totalcontrol_core::Error> {
        delegate!(self, mute(ch))
    }
    fn set_solo(&mut self, ch: ChannelId, s: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_solo(ch, s))
    }
    fn solo(&self, ch: ChannelId) -> Result<bool, totalcontrol_core::Error> {
        delegate!(self, solo(ch))
    }
    fn set_phantom(&mut self, idx: usize, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_phantom(idx, on))
    }
    fn set_pad(&mut self, idx: usize, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_pad(idx, on))
    }
    fn set_instrument(&mut self, idx: usize, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_instrument(idx, on))
    }
    fn set_gain(&mut self, idx: usize, gain: u32) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_gain(idx, gain))
    }
    fn set_input_gain_right(
        &mut self,
        idx: usize,
        gain: u32,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_input_gain_right(idx, gain))
    }
    fn set_input_level(
        &mut self,
        idx: usize,
        level: totalcontrol_core::InputLevel,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_input_level(idx, level))
    }
    fn set_autoset(&mut self, idx: usize, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_autoset(idx, on))
    }
    fn set_dynamics(
        &mut self,
        ch: ChannelId,
        dynamics: &totalcontrol_core::Dynamics,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_dynamics(ch, dynamics))
    }
    fn set_reverb(&mut self, reverb: &totalcontrol_core::Reverb) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_reverb(reverb))
    }
    fn set_echo(&mut self, echo: &totalcontrol_core::Echo) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_echo(echo))
    }
    fn set_fx_return(&mut self, output: usize, db: f32) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_fx_return(output, db))
    }
    fn set_input_pair_linked(
        &mut self,
        pair: usize,
        linked: bool,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_input_pair_linked(pair, linked))
    }
    fn set_output_linked(
        &mut self,
        pair: usize,
        linked: bool,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_output_linked(pair, linked))
    }
    fn set_pitch(&mut self, pitch_percent: f32) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_pitch(pitch_percent))
    }
    fn set_sensitivity(
        &mut self,
        idx: usize,
        sensitivity: totalcontrol_core::Sensitivity,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_sensitivity(idx, sensitivity))
    }
    fn set_clock_source(&mut self, source: &str) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_clock_source(source))
    }
    fn set_sample_rate(&mut self, rate: u32) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_sample_rate(rate))
    }
    fn set_loopback(&mut self, out: usize, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_loopback(out, on))
    }
    fn set_ms_proc(&mut self, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_ms_proc(on))
    }
    fn set_an12(&mut self, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_an12(on))
    }
    fn set_dim(&mut self, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_dim(on))
    }
    fn set_dim_with(&mut self, on: bool, amount_db: f32) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_dim_with(on, amount_db))
    }
    fn set_main_recall(&mut self, db: f32) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_main_recall(db))
    }
    fn set_speaker_b(&mut self, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_speaker_b(on))
    }
    fn set_mono(&mut self, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_mono(on))
    }
    fn set_speaker_b_linked(&mut self, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_speaker_b_linked(on))
    }
    fn set_main_out(&mut self, pair: Option<usize>) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_main_out(pair))
    }
    fn set_main_out_b(&mut self, pair: Option<usize>) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_main_out_b(pair))
    }
    fn set_mute_fx(&mut self, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_mute_fx(on))
    }
    fn set_external_input(
        &mut self,
        on: bool,
        source: Option<usize>,
        gain_db: f32,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_external_input(on, source, gain_db))
    }
    fn set_cue_target(&mut self, pair: Option<usize>) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_cue_target(pair))
    }
    fn set_pfl(&mut self, channel: ChannelId, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_pfl(channel, on))
    }
    fn set_pfl_mode(&mut self, pfl: bool, exclusive: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_pfl_mode(pfl, exclusive))
    }
    fn set_channel_fx_send(
        &mut self,
        channel: ChannelId,
        db: f32,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_channel_fx_send(channel, db))
    }
    fn set_eq_for_record(&mut self, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_eq_for_record(on))
    }
    fn set_optical_out_format(&mut self, spdif: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_optical_out_format(spdif))
    }
    fn set_cue(&mut self, idx: usize, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_cue(idx, on))
    }
    fn set_input_link(&mut self, linked: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_input_link(linked))
    }
    fn set_trim(&mut self, channel: ChannelId, db: f32) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_trim(channel, db))
    }
    fn set_phase(&mut self, idx: usize, invert: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_phase(idx, invert))
    }
    fn set_output_phase(&mut self, idx: usize, invert: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_output_phase(idx, invert))
    }
    fn set_no_trim(&mut self, idx: usize, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_no_trim(idx, on))
    }
    fn set_fx_send(&mut self, db: f32) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_fx_send(db))
    }
    fn set_stereo_split(&mut self, pb: usize, split: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_stereo_split(pb, split))
    }
    fn set_width(&mut self, width: f32) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_width(width))
    }
    fn set_channel_width(
        &mut self,
        channel: ChannelId,
        width: f32,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_channel_width(channel, width))
    }
    fn set_ref_level(&mut self, idx: usize, raw: u16) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_ref_level(idx, raw))
    }
    fn set_eq_enabled(&mut self, idx: usize, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_eq_enabled(idx, on))
    }
    fn set_eq_band_type(
        &mut self,
        idx: usize,
        band: usize,
        band_type: totalcontrol_core::channel::EqBandType,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_eq_band_type(idx, band, band_type))
    }
    fn set_eq_band_freq(
        &mut self,
        idx: usize,
        band: usize,
        freq_hz: u16,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_eq_band_freq(idx, band, freq_hz))
    }
    fn set_eq_band_q(&mut self, idx: usize, band: usize, q: f32) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_eq_band_q(idx, band, q))
    }
    fn set_eq_band_gain(
        &mut self,
        idx: usize,
        band: usize,
        gain_db: f32,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_eq_band_gain(idx, band, gain_db))
    }
    fn set_eq_low_cut_enabled(&mut self, idx: usize, on: bool) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_eq_low_cut_enabled(idx, on))
    }
    fn set_eq_low_cut_freq(&mut self, idx: usize, freq_hz: u16) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_eq_low_cut_freq(idx, freq_hz))
    }
    fn set_eq_low_cut_slope(
        &mut self,
        idx: usize,
        slope_db_oct: u8,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_eq_low_cut_slope(idx, slope_db_oct))
    }
    fn set_output_eq(
        &mut self,
        idx: usize,
        eq: &totalcontrol_core::channel::InputEq,
    ) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, set_output_eq(idx, eq))
    }
    fn capture_scene(&self) -> Scene {
        delegate!(self, capture_scene)
    }
    fn apply_scene(&mut self, s: &Scene) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, apply_scene(s))
    }
    fn poll_events(&mut self) -> Result<(), totalcontrol_core::Error> {
        delegate!(self, poll_events)
    }
}

impl DeviceHandle {
    /// Opens the real hardware. `backend` forces a specific path
    /// (`"alsa"` or `"usb"`) — anything else (including `None`) auto-
    /// detects: ALSA (the kernel driver) first, falling back to the
    /// direct USB backend if the kernel driver isn't loaded. The USB
    /// backend refuses to open while the kernel driver owns the device
    /// (see `totalcontrol-usb`), so the two paths are mutually exclusive —
    /// auto-detect never double-opens, it just wasn't ever *said* which
    /// one won. Every path now logs that at `info` level so a failure
    /// to find the device isn't a silent "which one did it even try?".
    pub fn open_real(backend: Option<&str>) -> Option<Self> {
        #[cfg_attr(not(feature = "alsa"), allow(unused_variables))]
        let want_alsa = !matches!(backend, Some("usb"));
        let want_usb = !matches!(backend, Some("alsa"));

        #[cfg(feature = "alsa")]
        for c in totalcontrol_core::fireface::class_compliant_cards() {
            log::warn!(
                "{} (card {}) is in Class Compliant mode: the RME mixer is not \
                 available. Use the stock ALSA/PipeWire device; switch the unit \
                 back to normal RME mode to use TotalControl's matrix.",
                c.name, c.card
            );
        }

        #[cfg(feature = "alsa")]
        if want_alsa {
            // Refactored path first: the kernel module exposes one hwdep
            // control node per card and PCM only.  This works for UC and
            // UCX and for several units at once; `open()` picks the first.
            match Fireface::open() {
                Ok(d) => {
                    log::info!(
                        "Device backend: hwdep ({} on card {})",
                        d.model_name(),
                        d.info().card
                    );
                    return Some(DeviceHandle::Fireface(d));
                }
                Err(e) => log::debug!("Fireface hwdep open not available: {e:?}"),
            }
            match UcxAlsa::open() {
                Ok(d) => {
                    log::info!("Device backend: ALSA (Fireface UCX)");
                    return Some(DeviceHandle::Ucx(d));
                }
                Err(e) => log::debug!("Fireface UCX ALSA open not available: {e:?}"),
            }
            match UcxAlsa::open_uc() {
                Ok(d) => {
                    log::info!("Device backend: ALSA (Fireface UC)");
                    return Some(DeviceHandle::Uc(d));
                }
                Err(e) => log::debug!("Fireface UC ALSA open not available: {e:?}"),
            }
            match BabyfacePro::open() {
                Ok(d) => {
                    log::info!("Device backend: ALSA (kernel driver)");
                    return Some(DeviceHandle::Real(d));
                }
                Err(e) if backend == Some("alsa") => {
                    log::warn!("--backend alsa requested but ALSA open failed: {e:?}");
                    return None;
                }
                // The card was found, it's just in a mode this backend
                // can't drive — user-actionable, so say it on stderr
                // instead of falling through to the generic (and here
                // simply wrong) "No device found".
                Err(e @ totalcontrol_core::Error::UnsupportedDeviceMode { .. }) => {
                    eprintln!("{e}");
                }
                Err(_) => {}
            }
        }
        #[cfg(not(feature = "alsa"))]
        if backend == Some("alsa") {
            log::warn!("--backend alsa requested but this build has no `alsa` feature");
            return None;
        }

        if want_usb {
            match BabyfaceProUsb::open() {
                Ok(d) => {
                    log::info!("Device backend: USB (libusb)");
                    return Some(DeviceHandle::Usb(d));
                }
                Err(e) if backend == Some("usb") => {
                    log::warn!("--backend usb requested but USB open failed: {e:?}");
                }
                Err(_) => {}
            }
        }
        None
    }
    /// The UCX simulation, same in-memory device with the UCX's topology and
    /// capability set.
    pub fn open_mock_ucx() -> Self {
        DeviceHandle::Mock(MockBabyfacePro::open_ucx().expect("mock opens"))
    }

    /// The UC simulation: the UC's topology and gated feature set.
    pub fn open_mock_uc() -> Self {
        DeviceHandle::Mock(MockBabyfacePro::open_uc().expect("mock opens"))
    }

    pub fn open_mock() -> Self {
        let device = match std::env::var("TOTALCONTROL_MOCK_MODEL").ok().as_deref() {
            Some("ucx") => MockBabyfacePro::open_ucx(),
            Some("uc") => MockBabyfacePro::open_uc(),
            _ => MockBabyfacePro::open(),
        };
        DeviceHandle::Mock(device.expect("mock opens"))
    }

    pub fn kind(&self) -> DeviceKind {
        match self {
            DeviceHandle::Mock(d) => {
                if d.is_ucx() {
                    DeviceKind::MockUcx
                } else if d.is_uc() {
                    DeviceKind::MockUc
                } else {
                    DeviceKind::Mock
                }
            }
            DeviceHandle::Usb(_) => DeviceKind::Babyface,
            DeviceHandle::Mirror(d) => d.info().kind,
            #[cfg(feature = "alsa")]
            DeviceHandle::Real(_) => DeviceKind::Babyface,
            #[cfg(feature = "alsa")]
            DeviceHandle::Ucx(_) => DeviceKind::Ucx,
            #[cfg(feature = "alsa")]
            DeviceHandle::Uc(_) => DeviceKind::Uc,
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(d) => match d.model() {
                FirefaceModel::Ucx => DeviceKind::Ucx,
                FirefaceModel::Uc => DeviceKind::Uc,
            },
        }
    }

    /// ALSA card id used by `/proc/asound/card*/id`; must match the
    /// kernel driver's `snd_card_set_id`, not the display name.
    pub fn card_id(&self) -> &'static str {
        match self {
            DeviceHandle::Mock(_) | DeviceHandle::Usb(_) => "BabyfacePro",
            DeviceHandle::Mirror(d) => d.info().card_id,
            #[cfg(feature = "alsa")]
            DeviceHandle::Real(_) => "BabyfacePro",
            #[cfg(feature = "alsa")]
            DeviceHandle::Ucx(_) => "FirefaceUCX",
            #[cfg(feature = "alsa")]
            DeviceHandle::Uc(_) => "FirefaceUC",
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(d) => d.spec().card_id,
        }
    }

    pub fn open_kind(kind: DeviceKind) -> Option<Self> {
        match kind {
            DeviceKind::Mock => Some(Self::open_mock()),
            DeviceKind::MockUcx => Some(Self::open_mock_ucx()),
            DeviceKind::MockUc => Some(Self::open_mock_uc()),
            #[cfg(feature = "alsa")]
            DeviceKind::Fireface => Fireface::open().ok().map(DeviceHandle::Fireface),
            #[cfg(not(feature = "alsa"))]
            DeviceKind::Fireface => None,
            #[cfg(feature = "alsa")]
            DeviceKind::Ucx => Fireface::open_ucx()
                .ok()
                .map(DeviceHandle::Fireface)
                .or_else(|| UcxAlsa::open().ok().map(DeviceHandle::Ucx)),
            #[cfg(feature = "alsa")]
            DeviceKind::Uc => Fireface::open_uc()
                .ok()
                .map(DeviceHandle::Fireface)
                .or_else(|| UcxAlsa::open_uc().ok().map(DeviceHandle::Uc)),
            #[cfg(feature = "alsa")]
            DeviceKind::Babyface => BabyfacePro::open()
                .ok()
                .map(DeviceHandle::Real)
                .or_else(|| BabyfaceProUsb::open().ok().map(DeviceHandle::Usb)),
            #[cfg(not(feature = "alsa"))]
            DeviceKind::Babyface => BabyfaceProUsb::open().ok().map(DeviceHandle::Usb),
            #[cfg(not(feature = "alsa"))]
            _ => None,
        }
    }
    /// All physical hwdep devices, one entry per unit, with stable serial.
    pub fn available_targets() -> Vec<DeviceTarget> {
        #[cfg(feature = "alsa")]
        {
            let mut out = Vec::new();
            for info in Fireface::enumerate() {
                let target = DeviceTarget::from_identity(info.identity());
                if !out.contains(&target) {
                    out.push(target);
                }
            }
            // Both simulations are always offered, so the interface can be
            // explored (or a GUI change checked) without the hardware.
            out.push(DeviceTarget::kind_only(DeviceKind::Mock));
            out.push(DeviceTarget::kind_only(DeviceKind::MockUcx));
            out.push(DeviceTarget::kind_only(DeviceKind::MockUc));
            out
        }
        #[cfg(not(feature = "alsa"))]
        {
            Vec::new()
        }
    }

    /// Stable target for the currently open device.
    pub fn target(&self) -> DeviceTarget {
        match self {
            DeviceHandle::Mock(d) => DeviceTarget::kind_only(if d.is_ucx() {
                DeviceKind::MockUcx
            } else if d.is_uc() {
                DeviceKind::MockUc
            } else {
                DeviceKind::Mock
            }),
            DeviceHandle::Usb(_) | DeviceHandle::Real(_) => {
                DeviceTarget::kind_only(DeviceKind::Babyface)
            }
            DeviceHandle::Mirror(d) => d.info().target.clone(),
            #[cfg(feature = "alsa")]
            DeviceHandle::Ucx(_) => DeviceTarget::kind_only(DeviceKind::Ucx),
            #[cfg(feature = "alsa")]
            DeviceHandle::Uc(_) => DeviceTarget::kind_only(DeviceKind::Uc),
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(d) => DeviceTarget::from_identity(d.identity()),
        }
    }

    /// Open a specific unit, preferring its stable serial when present.
    pub fn open_target(target: &DeviceTarget) -> Option<Self> {
        #[cfg(feature = "alsa")]
        if let Some(serial) = &target.serial {
            let model = match target.kind {
                DeviceKind::Uc => Some(totalcontrol_core::FirefaceModel::Uc),
                DeviceKind::Ucx => Some(totalcontrol_core::FirefaceModel::Ucx),
                _ => None,
            };
            if let Some(model) = model {
                if let Ok(d) = Fireface::open_identity(&totalcontrol_core::FirefaceIdentity {
                    model,
                    serial: serial.clone(),
                }) {
                    return Some(DeviceHandle::Fireface(d));
                }
            }
        }
        Self::open_kind(target.kind)
    }

    /// All input meter levels in one call (the USB backend's
    /// `meters()` is draining — call once per tick, not per channel).
    pub fn input_meters(&self) -> Vec<f32> {
        let n = self.inputs().len();
        match self {
            DeviceHandle::Mock(d) => (0..n).map(|i| d.input_meter(i)).collect(),
            DeviceHandle::Mirror(d) => d.meters().inputs.to_vec(),
            DeviceHandle::Usb(d) => d.meters().unwrap_or_else(|| vec![0.0; n]),
            #[cfg(feature = "alsa")]
            DeviceHandle::Real(d) => d.meters().unwrap_or_else(|| vec![0.0; n]),
            #[cfg(feature = "alsa")]
            DeviceHandle::Ucx(d) => d.meters().unwrap_or_else(|| vec![0.0; n]),
            #[cfg(feature = "alsa")]
            DeviceHandle::Uc(d) => d.meters().unwrap_or_else(|| vec![0.0; n]),
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(d) => d.meters().unwrap_or_else(|| vec![0.0; n]),
        }
    }
    pub fn playback_meters(&self) -> Vec<f32> {
        let n = self.playbacks().len();
        match self {
            DeviceHandle::Mock(d) => (0..n).map(|i| d.playback_meter(i)).collect(),
            DeviceHandle::Mirror(d) => d.meters().playbacks.to_vec(),
            // Playback meters come from the OUT stream — not wired yet.
            #[cfg(feature = "alsa")]
            DeviceHandle::Real(_) => vec![0.0; n],
            #[cfg(feature = "alsa")]
            DeviceHandle::Ucx(_) => vec![0.0; n],
            #[cfg(feature = "alsa")]
            DeviceHandle::Uc(_) => vec![0.0; n],
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(d) => d
                .playback_meters()
                .unwrap_or_else(|| vec![0.0; n]),
            DeviceHandle::Usb(_) => vec![0.0; n],
        }
    }

    /// Device-side SW-playback meters (UCX IF1 0x85), if available.
    pub fn device_playback_meters(&self) -> Option<Vec<f32>> {
        match self {
            DeviceHandle::Mirror(d) if d.info().device_playback_meters => {
                Some(d.meters().playbacks.to_vec())
            }
            DeviceHandle::Mirror(_) => None,
            #[cfg(feature = "alsa")]
            DeviceHandle::Ucx(d) => d.device_playback_meters(),
            #[cfg(feature = "alsa")]
            DeviceHandle::Uc(d) => d.device_playback_meters(),
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(d) => d.playback_meters(),
            _ => None,
        }
    }

    /// Peak words for the same channels as `input_meters`.  `None` when
    /// the backend only reports mean power.
    pub fn input_peaks(&self) -> Option<Vec<f32>> {
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.input_peaks();
        }
        None
    }

    pub fn device_playback_peaks(&self) -> Option<Vec<f32>> {
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.playback_peaks();
        }
        None
    }

    pub fn device_output_peaks(&self) -> Option<Vec<f32>> {
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.output_peaks_internal(true);
        }
        None
    }

    pub fn fx_peaks(&self) -> Option<[f32; 2]> {
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.fx_peaks();
        }
        None
    }

    /// UCX FX bus meters: `[send, return]`.
    pub fn fx_meters(&self) -> Option<[f32; 2]> {
        if let DeviceHandle::Mirror(d) = self {
            return Some(d.meters().fx);
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.fx_meters();
        }
        None
    }

    /// UCX FX bus overload flags: `[send, return]`.
    pub fn fx_overload(&self) -> Option<[bool; 2]> {
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.fx_overload();
        }
        None
    }

    /// Device-supplied hardware-output meters, already mapped to TotalControl's
    /// internal Phones-first output order.  `None` means the backend has
    /// no output meter stream and the GUI must compute them host-side.
    /// Driver transport health, when the device service or a direct
    /// Fireface handle can provide it.
    pub fn transport_counters(&self) -> Option<totalcontrol_core::fireface::abi::HwdepCounters> {
        match self {
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(d) => d.transport_counters(),
            DeviceHandle::Mirror(d) => d.transport_counters(),
            _ => None,
        }
    }

    pub fn device_output_meters(&self) -> Option<Vec<f32>> {
        match self {
            DeviceHandle::Mirror(d) => Some(d.meters().outputs.to_vec()),
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(d) => d.output_meters_internal(true),
            _ => None,
        }
    }

    /// Release/reopen the input capture meter at a new rate.  Needed
    /// because capture and playback share the device clock.
    pub fn set_meter_rate(&mut self, rate: u32) {
        #[cfg(feature = "alsa")]
        match self {
            DeviceHandle::Ucx(d) | DeviceHandle::Uc(d) => d.set_meter_rate(rate),
            DeviceHandle::Fireface(d) => {
                let _ = d;
                // UCX meters come from the driver's 0x85 events; the UC
                // capture fallback reopens on demand.
            }
            _ => {}
        }
        #[cfg(not(feature = "alsa"))]
        let _ = rate;
    }

    /// True when the backend has its own playback meter source, so the
    /// PipeWire sink tap is unnecessary.
    pub fn has_device_playback_meters(&self) -> bool {
        if let DeviceHandle::Mirror(d) = self {
            return d.info().device_playback_meters;
        }
        #[cfg(feature = "alsa")]
        if matches!(self, DeviceHandle::Ucx(_) | DeviceHandle::Uc(_)) {
            return true;
        }
        if matches!(self, DeviceHandle::Fireface(d) if d.model() == FirefaceModel::Ucx) {
            return true;
        }
        false
    }

    /// Per-channel availability for the input strips. Mock is real
    /// everywhere; the USB backend's ISO IN stream meters AN1-4 only
    /// (`totalcontrol-usb`'s `input_peaks()` returns four channels); the
    /// ALSA/kernel-driver backend meters all twelve inputs in the
    /// device's own order (AN1-4, AS1/2, ADAT3-8) with the v7 driver,
    /// see `capture_meter`'s module doc comment), provided the PCM
    /// opened (`BabyfacePro::meters_available`). An older driver puts
    /// AS1/2 at 10/11 instead, so TotalControl expects the updated driver.
    /// Each capture pair is also the matching
    /// output pair's record bus, so enabling that output's loopback
    /// makes the input meter read the output bus too.
    ///
    /// The UCX backend uses the same capture-meter path with all 18
    /// inputs (AN1-8, SPDIF, ADAT) and an identity channel map. These
    /// are the raw pre-fader input words, so the meter stays live while
    /// the channel is faded down or muted.
    pub fn has_input_meter(&self, idx: usize) -> bool {
        match self {
            DeviceHandle::Mock(_) => true,
            DeviceHandle::Mirror(d) => d.info().input_meters.get(idx).copied().unwrap_or(false),
            DeviceHandle::Usb(_) => idx < 4,
            #[cfg(feature = "alsa")]
            DeviceHandle::Real(d) => idx < d.inputs().len() && d.meters_available(),
            #[cfg(feature = "alsa")]
            DeviceHandle::Ucx(d) => idx < d.inputs().len() && d.meters_available(),
            #[cfg(feature = "alsa")]
            DeviceHandle::Uc(d) => idx < d.inputs().len() && d.meters_available(),
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(d) => idx < d.inputs().len(),
        }
    }
    /// Whether `playback_meters()` is real — true only for Mock. The USB
    /// backend runs its ISO OUT stream in meter-only (silence) mode, so it
    /// never sees real playback audio to compute a level from even though
    /// it technically owns the stream — and only one process can hold that
    /// stream at a time (see `tools/alsa/README.md`'s "Known limits"), so
    /// real playback audio from another app never reaches it either.
    pub fn has_playback_meters(&self) -> bool {
        match self {
            DeviceHandle::Mock(_) => true,
            DeviceHandle::Mirror(d) => d.info().device_playback_meters || d.info().is_mock,
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(d) => d.model() == FirefaceModel::Ucx,
            _ => false,
        }
    }
    /// Output meters, computed host-side like TotalMix from levels the
    /// caller already drained this tick: each output's level is the
    /// power sum of every routed source (inputs + playbacks) scaled by
    /// that source's fader into the output. Passing the levels in
    /// matters: the previous `output_meters()` called `input_meters()`/
    /// `playback_meters()` itself, and those drain, so on USB and ALSA
    /// the sum was computed from the near-empty second drain. See
    /// `power_sum_output_meters`'s own doc comment for the math.
    pub fn output_meters_from(&self, input_meters: &[f32], playback_meters: &[f32]) -> Vec<f32> {
        let mut input_vols: Vec<Vec<f32>> =
            self.inputs().iter().map(|c| c.volumes.clone()).collect();
        let mut playback_vols: Vec<Vec<f32>> =
            self.playbacks().iter().map(|c| c.volumes.clone()).collect();
        if self.matrix_uses_display_order() {
            let order = self.output_display_order();
            reorder_pair_volumes(&mut input_vols, &order);
            reorder_pair_volumes(&mut playback_vols, &order);
        }
        let mut out = power_sum_output_meters(
            &input_vols,
            input_meters,
            &playback_vols,
            playback_meters,
            self.outputs().len(),
        );
        // TotalMix's hardware-output meters are post-fader: the bottom-row
        // fader ("total level of the output") must move its own VU too.
        for (i, level) in out.iter_mut().enumerate() {
            if let Some(ch) = self.outputs().get(i) {
                *level = if ch.mute {
                    0.0
                } else {
                    *level * ch.volume.clamp(0.0, 1.0)
                };
            }
        }
        out
    }
    pub fn is_mock(&self) -> bool {
        match self {
            DeviceHandle::Mock(_) => true,
            DeviceHandle::Mirror(d) => d.info().is_mock,
            _ => false,
        }
    }
    /// UC parameters that can be read back at connect.  `None` for
    /// backends without this readback path.
    pub fn readable_state(&self) -> Option<totalcontrol_core::fireface::ReadableState> {
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.readable_state();
        }
        None
    }

    pub fn begin_startup_state_dump(
        &mut self,
        timeout: Duration,
    ) -> Result<(), totalcontrol_core::Error> {
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.begin_startup_state_dump(timeout);
        }
        Ok(())
    }

    pub fn startup_dump_pending(&self) -> bool {
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.startup_dump_pending();
        }
        false
    }

    pub fn startup_dump_complete(&self) -> bool {
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.startup_dump_complete();
        }
        true
    }

    pub fn pre_connect_words(&self) -> Option<std::collections::HashMap<u16, u16>> {
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return Some(d.pre_connect_words().clone());
        }
        None
    }

    pub fn load_scene_model(&mut self, scene: &Scene) -> Result<(), totalcontrol_core::Error> {
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.load_scene_model(scene);
        }
        self.apply_scene(scene)
    }

    pub fn firmware(&self) -> Option<u32> {
        if let DeviceHandle::Mirror(d) = self {
            return d.info().firmware;
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return Some(d.firmware());
        }
        None
    }

    pub fn serial(&self) -> Option<&str> {
        if let DeviceHandle::Mirror(d) = self {
            return d.info().serial.as_deref();
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return Some(d.serial());
        }
        None
    }

    pub fn set_line_in_ref(&mut self, code: u16) -> Result<(), totalcontrol_core::Error> {
        if let DeviceHandle::Mirror(d) = self {
            d.set_line_in_ref(code);
            return Ok(());
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.set_line_in_ref(code);
        }
        Ok(())
    }

    pub fn set_line_out_ref(&mut self, code: u16) -> Result<(), totalcontrol_core::Error> {
        if let DeviceHandle::Mirror(d) = self {
            d.set_line_out_ref(code);
            return Ok(());
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.set_line_out_ref(code);
        }
        Ok(())
    }

    pub fn set_phones_ref(&mut self, code: u16) -> Result<(), totalcontrol_core::Error> {
        if let DeviceHandle::Mirror(d) = self {
            d.set_phones_ref(code);
            return Ok(());
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.set_phones_ref(code);
        }
        Ok(())
    }

    pub fn set_line_in_ref_58(&mut self, code: u16) -> Result<(), totalcontrol_core::Error> {
        if let DeviceHandle::Mirror(d) = self {
            d.set_line_in_ref_58(code);
            return Ok(());
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.set_line_in_ref_58(code);
        }
        let _ = code;
        Ok(())
    }

    pub fn set_spdif_professional(&mut self, on: bool) -> Result<(), totalcontrol_core::Error> {
        if let DeviceHandle::Mirror(d) = self {
            d.set_spdif_professional(on);
            return Ok(());
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.set_spdif_professional(on);
        }
        if let DeviceHandle::Mock(d) = self {
            return d.set_spdif_professional(on);
        }
        Ok(())
    }

    pub fn set_word_clock_single_speed(&mut self, on: bool) -> Result<(), totalcontrol_core::Error> {
        if let DeviceHandle::Mirror(d) = self {
            d.set_word_clock_single_speed(on);
            return Ok(());
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.set_word_clock_single_speed(on);
        }
        if let DeviceHandle::Mock(d) = self {
            return d.set_word_clock_single_speed(on);
        }
        Ok(())
    }

    pub fn set_word_clock_termination(&mut self, on: bool) -> Result<(), totalcontrol_core::Error> {
        if let DeviceHandle::Mirror(d) = self {
            d.set_word_clock_termination(on);
            return Ok(());
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.set_word_clock_termination(on);
        }
        if let DeviceHandle::Mock(d) = self {
            return d.set_word_clock_termination(on);
        }
        Ok(())
    }

    pub fn set_input_ms_proc(&mut self, idx: usize, on: bool) -> Result<(), totalcontrol_core::Error> {
        if let DeviceHandle::Mirror(d) = self {
            d.set_input_ms_proc(idx, on);
            return Ok(());
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.set_input_ms_proc(idx, on);
        }
        if let DeviceHandle::Mock(d) = self {
            return d.set_input_ms_proc(idx, on);
        }
        Ok(())
    }

    pub fn set_playback_ms_proc(
        &mut self,
        idx: usize,
        on: bool,
    ) -> Result<(), totalcontrol_core::Error> {
        if let DeviceHandle::Mirror(d) = self {
            d.set_playback_ms_proc(idx, on);
            return Ok(());
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.set_playback_ms_proc(idx, on);
        }
        if let DeviceHandle::Mock(d) = self {
            return d.set_playback_ms_proc(idx, on);
        }
        Ok(())
    }

    pub fn set_talkback_with(
        &mut self,
        on: bool,
        config: &TalkbackConfig,
    ) -> Result<(), totalcontrol_core::Error> {
        if let DeviceHandle::Mirror(d) = self {
            d.set_talkback_with(on, config);
            return Ok(());
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.set_talkback_with(on, config);
        }
        if let DeviceHandle::Mock(_) = self {
            return Ok(());
        }
        Ok(())
    }

    /// Apply the readable/queued device values after the user chooses
    /// "Device -> TotalControl".  Unreadable state stays on the TotalControl scene.
    pub fn restore_readable(
        &mut self,
        state: &totalcontrol_core::fireface::ReadableState,
    ) -> Result<(), totalcontrol_core::Error> {
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Fireface(d) = self {
            return d.restore_readable(state);
        }
        let _ = state;
        Ok(())
    }

    /// Send a host-only settings edit to the device service.  No-op on the
    /// direct backends, which already hold the edit.
    pub fn sync_settings_to_device(&self) {
        if let DeviceHandle::Mirror(d) = self {
            d.sync_settings();
        }
    }

    /// Physical Phones hardware volume (linear 0..1), if the backend has a
    /// real front-panel readback.  Only the UCX IF1 reader provides it.
    pub fn hardware_volume(&self) -> Option<f32> {
        if let DeviceHandle::Mirror(d) = self {
            return d.hardware_volume();
        }
        #[cfg(feature = "alsa")]
        if let DeviceHandle::Ucx(d) | DeviceHandle::Uc(d) = self {
            return d.hardware_volume();
        }
        if let DeviceHandle::Fireface(d) = self {
            return d.hardware_volume();
        }
        None
    }
    /// Convert an internal output-pair index to its display/submix index.
    pub fn display_pair_index(&self, internal: usize) -> usize {
        self.output_display_order()
            .iter()
            .position(|&p| p == internal)
            .unwrap_or(internal)
    }

    /// The hwdep Fireface path has no on-demand mixer readback.  Like
    /// TotalMix, TotalControl restores its saved model at startup and writes it
    /// to the unit, so the unit and the displayed state start identical.
    /// The older ALSA/USB backends read the live matrix on open and do not
    /// need this.
    pub fn needs_scene_restore(&self) -> bool {
        match self {
            DeviceHandle::Usb(_) => true,
            DeviceHandle::Mirror(_) => true,
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(_) => true,
            _ => false,
        }
    }
    /// The hwdep Fireface path treats TotalControl's saved state as the source
    /// of truth because most of the UC mixer is write-only.  Live
    /// front-panel gain/volume events are still applied on top (see
    /// `Fireface::uc_status_event`).
    pub fn device_state_is_authoritative(&self) -> bool {
        false
    }
    /// Whether the sound server, not this app, sets the card's sample rate
    /// and buffer size: true for the kernel driver (PipeWire opens the
    /// card), false for the USB backend, which streams the device itself.
    pub fn rate_follows_sound_server(&self) -> bool {
        if let DeviceHandle::Mirror(d) = self {
            return d.info().rate_follows;
        }
        #[cfg(feature = "alsa")]
        if matches!(
            self,
            DeviceHandle::Real(_)
                | DeviceHandle::Ucx(_)
                | DeviceHandle::Uc(_)
                | DeviceHandle::Fireface(_)
        ) {
            return true;
        }
        false
    }
    /// Whether this backend actually implements `set_sensitivity` —
    /// real for Mock and the USB/libusb backend (wired to the same
    /// hardware-verified ref-level mechanism `set_ref_level` already
    /// used for Instr 3/4's +4dBu/-10dBV/Boost switch). **2026-09-06**:
    /// now real for the ALSA/kernel-driver backend too, once
    /// `babyface-pro-linux` gained its own "Instrument Ref Level"
    /// control — previously this backend had no such control at all.
    pub fn has_sensitivity_control(&self) -> bool {
        match self {
            DeviceHandle::Mock(_) | DeviceHandle::Usb(_) => true,
            DeviceHandle::Mirror(d) => d.info().has_sensitivity,
            #[cfg(feature = "alsa")]
            DeviceHandle::Real(_) => true,
            #[cfg(feature = "alsa")]
            DeviceHandle::Ucx(_) => false,
            #[cfg(feature = "alsa")]
            DeviceHandle::Uc(_) => false,
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(_) => false,
        }
    }
    /// True when the backend lays out outputs as ONE channel per submix
    /// pair (the proprietary USB path: `outputs().len() ==
    /// output_pair_count()`) rather than two channels per pair (the
    /// ALSA/profile `build_outputs` layout). Callers that map an output
    /// strip click back to the submix pair index need this.
    pub fn outputs_one_per_pair(&self) -> bool {
        self.outputs().len() == self.output_pair_count()
    }
    /// The current front-panel state (MIX engaged, IN sel, OUT sel) —
    /// only the proprietary USB backend has a panel.
    pub fn panel_selection(&self) -> Option<(bool, usize, usize)> {
        match self {
            DeviceHandle::Mock(_) => None,
            DeviceHandle::Mirror(d) => d.panel_selection(),
            DeviceHandle::Usb(d) => Some(d.panel_selection()),
            #[cfg(feature = "alsa")]
            DeviceHandle::Real(d) => d.panel_selection(),
            #[cfg(feature = "alsa")]
            DeviceHandle::Ucx(_) => None,
            #[cfg(feature = "alsa")]
            DeviceHandle::Uc(_) => None,
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(_) => None,
        }
    }

    /// True for a Fireface UC model regardless of which backend drives it.
    pub fn is_uc_model(&self) -> bool {
        match self {
            DeviceHandle::Mock(d) => d.is_uc(),
            DeviceHandle::Mirror(d) => d.info().is_uc,
            #[cfg(feature = "alsa")]
            DeviceHandle::Uc(_) => true,
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(d) => d.model() == FirefaceModel::Uc,
            _ => false,
        }
    }

    /// True for a Fireface UCX model regardless of which backend drives it.
    pub fn is_ucx_model(&self) -> bool {
        match self {
            DeviceHandle::Mock(d) => d.is_ucx(),
            DeviceHandle::Mirror(d) => d.info().is_ucx,
            #[cfg(feature = "alsa")]
            DeviceHandle::Ucx(_) => true,
            #[cfg(feature = "alsa")]
            DeviceHandle::Fireface(d) => d.model() == FirefaceModel::Ucx,
            _ => false,
        }
    }

    /// True for the UC/UCX family (the shared 18-channel Fireface node),
    /// as opposed to a Babyface Pro.
    pub fn is_uc_family(&self) -> bool {
        match self {
            DeviceHandle::Mock(d) => d.is_uc() || d.is_ucx(),
            DeviceHandle::Mirror(d) => d.info().is_uc_family,
            #[cfg(feature = "alsa")]
            DeviceHandle::Uc(_) | DeviceHandle::Ucx(_) | DeviceHandle::Fireface(_) => true,
            _ => false,
        }
    }
}

/// Re-index every channel's per-output-pair volumes from matrix/display
/// order into internal output order.  The channel list itself stays put:
/// it is the software-playback source order, not an output dimension.
fn reorder_pair_volumes(channels: &mut [Vec<f32>], order: &[usize]) {
    for vols in channels.iter_mut() {
        let mut by_internal = vec![0.0f32; order.len()];
        for (display, &internal) in order.iter().enumerate() {
            if let Some(v) = vols.get(display) {
                by_internal[internal] = *v;
            }
        }
        *vols = by_internal;
    }
}

/// The actual math behind `DeviceHandle::output_meters_from` — each output
/// *channel*'s level is the power sum of every input/playback source
/// routed into it, scaled by that source's live meter reading. Pulled
/// out as a free, pure function (rather than left inline) so it's
/// testable with deterministic meter values — the mock backend's own
/// `input_meter`/`playback_meter` are randomized, so a test going
/// through `DeviceHandle` itself could never assert an exact number.
///
/// `input_volumes`/`playback_volumes` are indexed by *output pair*
/// (`output_pair_count()` entries — one crosspoint per pair, not per
/// individual physical channel) while `n_out` is the individual output
/// *channel* count (2 per pair) — the pair a channel `o` belongs to is
/// `o / 2`, the same convention `set_channel_volume` and friends use
/// everywhere else. Indexing the volumes arrays with the raw channel
/// index instead of `o / 2` used to silently read the *wrong* pair's
/// crosspoint for every odd channel, and always read nothing at all
/// for every channel whose pair index was past the volumes arrays'
/// own (pair-counted) length — every output pair past the first three
/// read a permanent, wrong zero level regardless of actual audio.
fn power_sum_output_meters(
    input_volumes: &[Vec<f32>],
    input_meters: &[f32],
    playback_volumes: &[Vec<f32>],
    playback_meters: &[f32],
    n_out: usize,
) -> Vec<f32> {
    let mut out = vec![0.0f32; n_out];
    for (o, slot) in out.iter_mut().enumerate() {
        let pair = o / 2;
        let mut p = 0.0f32;
        for (i, vols) in input_volumes.iter().enumerate() {
            let v = vols.get(pair).copied().unwrap_or(0.0);
            let m = input_meters.get(i).copied().unwrap_or(0.0);
            p += (m * v) * (m * v);
        }
        for (c, vols) in playback_volumes.iter().enumerate() {
            let v = vols.get(pair).copied().unwrap_or(0.0);
            let m = playback_meters.get(c).copied().unwrap_or(0.0);
            p += (m * v) * (m * v);
        }
        *slot = p.sqrt().min(1.0);
    }
    out
}

/// Which top-level page is showing. `Quick` is the default — a focused
/// "pick a source, adjust it, pick a destination, adjust it" pair of big
/// strips, for the podcaster/streamer/remote-work case that never touches
/// the full matrix. `Mixer`/`Matrix` are the existing dense views for
/// users who need them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum View {
    Quick,
    Mixer,
    Matrix,
}

// ── Messages ─────────────────────────────────────────────────────

/// Options > Reset Mix entries (manual 25.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMix {
    StraightToMain,
    StraightPlayback,
    ClearSubmixes,
    ClearEffects,
    SetOutputVolumes,
    ResetNames,
    AllMono,
    AllStereo,
    InputsMonoOutputsStereo,
    TotalReset,
}

/// The top-bar Options menu.  Reset Mix is a submenu; Settings opens the
/// app-level sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptionsMenu {
    #[default]
    Closed,
    Root,
    Reset,
}

pub const RESET_MIX: [(&str, ResetMix); 10] = [
    ("Straight playback with all to Main Out", ResetMix::StraightToMain),
    ("Straight playback", ResetMix::StraightPlayback),
    ("Clear all submixes", ResetMix::ClearSubmixes),
    ("Clear channel effects", ResetMix::ClearEffects),
    ("Set output volumes", ResetMix::SetOutputVolumes),
    ("Reset channel names", ResetMix::ResetNames),
    ("Set all channels mono", ResetMix::AllMono),
    ("Set all channels stereo", ResetMix::AllStereo),
    ("Set inputs mono, outputs stereo", ResetMix::InputsMonoOutputsStereo),
    ("Total reset", ResetMix::TotalReset),
];

/// Which level of the Assign menu is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignMenu {
    Closed,
    Destinations,
    Outputs(usize),
    CueTargets,
}

/// Control Room Assign destinations, in the menu order. The first six are
/// output-pair assignments; the seventh is the Cue/PFL target selector.
pub const OUTPUT_DESTS: usize = 6;
pub const CUE_DEST: usize = 6;
pub const ASSIGN_DESTS: [&str; 7] = [
    "Main",
    "Speaker B",
    "Phones 1",
    "Phones 2",
    "Phones 3",
    "Phones 4",
    "Cue/PFL to",
];

/// Monitoring outputs the Cue/PFL signal can follow. Phones 4 is selectable
/// as a Control Room channel but is not a Cue target on the UCX.
pub const CUE_TARGETS: [usize; 4] = [0, 2, 3, 4];

/// One add or clear transition between the Hardware Outputs row and the
/// Control Room. A same-count change (destination move or replace) does not
/// animate.
#[derive(Debug, Clone)]
pub struct AssignAnim {
    pub since: Instant,
    pub old: [Option<usize>; OUTPUT_DESTS],
    pub new: [Option<usize>; OUTPUT_DESTS],
    pub max_ms: f32,
}

impl AssignAnim {
    fn elapsed_ms(&self, now: Instant) -> f32 {
        now.duration_since(self.since).as_secs_f32() * 1000.0
    }

    fn moving(&self) -> Option<(usize, usize)> {
        let old_count = self.old.iter().flatten().count();
        let new_count = self.new.iter().flatten().count();
        if new_count > old_count {
            self.new
                .iter()
                .enumerate()
                .find_map(|(dest, pair)| match (pair, self.old[dest]) {
                    (Some(pair), None) => Some((dest, *pair)),
                    _ => None,
                })
        } else if new_count < old_count {
            self.old
                .iter()
                .enumerate()
                .find_map(|(dest, pair)| match (pair, self.new[dest]) {
                    (Some(pair), None) => Some((dest, *pair)),
                    _ => None,
                })
        } else {
            None
        }
    }
}



#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    /// A model snapshot is waiting on the device service channel.  The
    /// subscription fires only when the service actually published one, so
    /// an idle mixer is not rebuilt at the meter rate.
    DeviceUpdated,
    /// Meter activity started after silence.  The message carries nothing;
    /// it only asks for the frame in which the canvases see `active` again
    /// and resume scheduling their own 30 fps frames.
    MeterWake,
    SetView(View),
    /// The source channel picked in the Quick Control view — always an
    /// `Input`/`Playback` id, never `Output` (see `quick_channel_options`,
    /// the only place that builds the pick-list this feeds).
    QuickChannelSelected(ChannelId),
    /// Switch to another detected mixer backend/device.
    SelectDevice(DeviceTarget),
    SelectOutput(usize),
    ModifiersChanged(keyboard::Modifiers),
    /// F11: toggle the window between windowed and fullscreen. The id
    /// comes from the global event handler, the only place that has it.
    ToggleFullscreen(window::Id),
    TabPressed,
    EscapePressed,
    /// The OS window was resized (or just opened) — tracks the current
    /// width in `TotalControl::window_width` (coalesced, `RESIZE_THROTTLE`) so
    /// `responsive_row` can decide whether a strip row fits or scrolls.
    /// This no longer rescales anything: strips keep their current
    /// `ui_scale` (changed only by the manual `Zoom*` / `WheelZoom`
    /// messages), so a window drag is pure scroll/clip.
    WindowResized(f32),
    /// The window's full size, kept for the preferences (`window_size`).
    WindowSize(f32, f32),

    /// Ctrl+wheel — zooms the UI in/out (see `zoom`). Ignored unless Ctrl
    /// is held (`TotalControl::modifiers`); plain wheel scrolls as normal.
    WheelZoom(f32),
    /// Discrete manual zoom (Ctrl+= / Ctrl+- / Ctrl+0).
    ZoomIn,
    ZoomOut,
    ZoomReset,
    /// The monitor's logical size, asked once at the first launch to pick
    /// a sensible starting zoom (`initial_zoom_for`).
    MonitorSize(Option<iced::Size>),
    /// The window's close button (or a close request from the system):
    /// hides the window when the app runs in the tray, quits otherwise.
    CloseRequested,
    /// A window went away without our asking (the compositor closed it).
    WindowClosed(window::Id),
    /// The notification-area icon, or a second launch (`tray`).
    Tray(crate::tray::TrayEvent),
    /// "Keep running in the tray when I close the window".
    SetKeepInTray(bool),
    /// "Start with the session" (an autostart entry, hidden in the tray).
    SetAutostart(bool),
    /// Startup mismatch: use the saved TotalControl scene (already applied).
    MismatchUseTotalcontrol,
    /// Startup mismatch: re-apply the readable/queued device values.
    MismatchUseDevice,
    /// Startup mismatch: use TotalControl now and suppress future prompts.
    MismatchAlwaysTotalcontrol,
    /// Settings toggle for RME's "Always init DSP devices with TotalMix FX
    /// settings".
    SetAlwaysInitDevice(bool),
    /// Speaker B follows Main Out level changes (`/1/speakerBLinked`).
    SpeakerBLinked(bool),

    Mute(ChannelId, bool),
    Solo(ChannelId, bool),
    /// The sidebar's global "M" button (`sidebar::msf_row`), TotalMix's
    /// Mute Master: if any channel is muted, unmutes exactly those and
    /// remembers them (`TotalControl::last_cleared_mutes`); if none is, mutes
    /// the remembered set again. The same shape as `GlobalSoloToggle`.
    /// RME's manual: "As soon as any Mute button is pressed, the Mute
    /// Master button lights up ... With this button all selected mutes can
    /// be switched off and on again." It used to mute *every* channel
    /// instead, which is a panic button TotalMix does not have, and left
    /// the button unlit while a single channel was muted.
    GlobalMuteToggle,
    /// The sidebar's global "S" button — a real toggle, symmetric with
    /// `GlobalMuteToggle`: if anything is soloed, clears every solo and
    /// remembers which channels those were (`TotalControl::last_cleared_solos`);
    /// if nothing is soloed and there's a remembered set, restores solo
    /// on exactly those channels instead. There's no sensible "solo
    /// everything" (unlike mute), so restoring the prior set is the
    /// toggle's other direction rather than that.
    GlobalSoloToggle,
    Phantom(usize, bool),
    Pad(usize, bool),
    Instrument(usize, bool),
    /// UCX three-value input Level (`-10 dBV` / `+4 dBu` / `LoGain`).
    LevelChanged(usize, InputLevel),
    /// UCX AutoSet on inputs 1/2.
    Autoset(usize, bool),
    /// Raw preamp gain units (0..=`InputChannel::gain_max`), not dB —
    /// the hardware's own scale isn't a fixed dB curve we can label.
    Gain(usize, u32),
    /// `true` = +4dBu, `false` = -10dBV.
    Sensitivity(usize, bool),
    /// Trim in dB (-65..+6, see `RmeDevice::set_trim`) — `usize` is the
    /// input index, same scale as every other Hardware Input, unlike
    /// `Gain` which is raw per-model hardware units.
    TrimChanged(ChannelId, f32),
    TrimReset(ChannelId),

    /// Hardware 3-band + low-cut EQ (analog inputs only) — `usize` is
    /// always the input index; the `Eq*Band*` variants carry the band
    /// index (0-2) as their second field. See `totalcontrol_core::channel::InputEq`.
    EqEnabled(ChannelId, bool),
    EqBandType(ChannelId, usize, EqBandType),
    EqBandFreq(ChannelId, usize, u16),
    EqBandQ(ChannelId, usize, f32),
    EqBandGain(ChannelId, usize, f32),
    /// A drag in the EQ plot: frequency and gain move together, so the
    /// device gets one update instead of two.
    EqBandPoint(ChannelId, usize, u16, f32),
    /// Double-click on a band handle: back to the configured reset point.
    EqBandReset(ChannelId, usize),
    EqLowCutEnabled(ChannelId, bool),
    EqLowCutFreq(ChannelId, u16),
    EqLowCutSlope(ChannelId, u8),

    /// Dynamics panel: the shared on/off button and the complete state
    /// (each knob sends the full struct so the backend writes one block).
    DynamicsEnabled(ChannelId, bool),
    DynamicsChanged(ChannelId, Dynamics),
    /// Global FX panel.
    ReverbChanged(Reverb),
    EchoChanged(Echo),
    /// A TotalMix FX page-3 parameter applied to the current Reverb/Echo.
    OscRmeFx { echo: bool, field: &'static str, value: f32 },
    /// Per-output FX Return level in dB.
    FxReturnChanged(usize, f32),
    /// Per-channel FX send level in dB.
    FxSendChanged(ChannelId, f32),
    /// Options > Reset Mix.
    ResetMix(ResetMix),

    /// Output-channel index (not the submix pair — converted in the
    /// handler via `DeviceHandle::outputs_one_per_pair`).
    LoopbackChanged(usize, bool),
    /// Toggles the stereo link/split state of `cid`'s pair — dispatches
    /// to `set_input_pair_linked`/`set_playback_linked`/
    /// `set_output_linked` depending on which kind `cid` is.
    StereoLinkChanged(ChannelId, bool),
    /// Settings panel content, all in the `device_panel` drawer.
    PitchChanged(f32),
    WidthChanged(f32),
    /// Per-channel stereo width (inputs and playbacks), -1..+1.
    ChannelWidthChanged(ChannelId, f32),
    MsProcChanged(bool),
    InputMsProcChanged(usize, bool),
    PlaybackMsProcChanged(usize, bool),
    An12Changed(bool),
    InputLinkChanged(bool),
    EqForRecordChanged(bool),
    OpticalOutFormatChanged(bool),
    LineInRefSelected(u16),
    /// Line In 5-8 reference (one shared group on the UCX).
    LineInRef58Selected(u16),
    LineOutRefSelected(u16),
    PhonesRefSelected(u16),
    SpdifProfessionalChanged(bool),
    WordClockSingleSpeedChanged(bool),
    WordClockTerminationChanged(bool),
    /// The CUE button on an Output strip — exclusive across every
    /// output (see `RmeDevice::set_cue`'s own doc comment), so this
    /// always carries the *target* state, not a relative toggle.
    CueChanged(ChannelId, bool),
    /// Phase Ø invert — Hardware Input strips, the 4 analog inputs only.
    PhaseChanged(ChannelId, bool),
    /// Phase invert on the right side of a split input pair.
    InputPhaseRight(usize, bool),
    /// Phase invert on one hardware output side (`Ø L` / `R Ø`).
    OutputPhaseChanged(usize, bool),
    /// Exclude one hardware output from Trim Gain changes.
    NoTrimChanged(usize, bool),
    TalkbackReceiveChanged(usize, bool),

    VolumeChanged(ChannelId, usize, f32),
    /// One Matrix cell: source channel and physical output block (0-17).
    /// Unlike `VolumeChanged`, this carries the exact column so Mono Mode
    /// can write one cell instead of the stereo pair.
    MatrixCellChanged(ChannelId, usize, f32),
    /// Toggle Matrix Mono Mode (one mono cell vs the stereo pair).
    ToggleMatrixMono,
    /// Cursor-key fader nudge, in dB (+ = louder).
    NudgeVolume(f32),
    FaderPressed(ChannelId, usize, f32, Option<(f32, f32)>),
    RangeCleared(ChannelId),
    Reset(ChannelId, usize, f32),

    PanChanged(ChannelId, usize, i8),
    PanReset(ChannelId, usize),
    ToggleCollapse(ChannelId),
    /// Opens/closes one strip's flyout (route bus picker or settings —
    /// see `strip::FlyoutKind`). Opening one closes whichever other was
    /// open, of either kind, since at most one is ever shown at a time —
    /// a display preference, deliberately not propagated across a multi-
    /// selection the way `ToggleCollapse` is.
    ToggleFlyout(ChannelId, strip::FlyoutKind),
    /// Closes the open flyout — the click-outside catcher.
    CloseFlyout,
    /// Fires only while `collapse_anim` is non-empty (see `subscription`) —
    /// exists purely to trigger a redraw at a much higher rate than the
    /// normal 50ms `Tick` so the width tween looks smooth, and to prune
    /// entries once they've settled so that faster timer can shut off.
    CollapseTick,
    /// Window close requested — save the mixer state NOW so the next UI
    /// (GUI or TUI, shared `auto.json`) doesn't restore a stale snapshot.
    SaveNow,

    EditStart(ChannelId, String),
    EditChanged(String),
    EditCommit,

    /// Ctrl/Shift+click on a strip's non-control area toggles its
    /// selection membership; a plain click there is a no-op (so
    /// double-click-to-collapse on a selected strip isn't disrupted by an
    /// intervening deselect on the first press).
    StripClicked(ChannelId),
    /// Plain click on genuinely empty page background clears the
    /// selection — see `page()`.
    ClearSelection,
    /// The cursor just entered (`Some`) or left (`None`) a strip's card —
    /// drives the hover border/glow. See `TotalControl::hovered_strip`.
    StripHovered(Option<ChannelId>),

    /// The OSC worker (see `osc.rs`) just started — carries the sender
    /// used to push outgoing feedback packets back out over UDP. Only
    /// fires if `--osc` was passed; `state.osc_tx` stays `None` otherwise.
    OscReady(mpsc::Sender<OscOutbound>),
    /// One raw classic RME OSC message.  Parsed on the UI thread so the
    /// parser can resolve `Toggle` against the confirmed mixer model.
    OscRmeIn(String, Vec<OscType>),
    /// One decoded Global OSC command.  The transport is absolute and
    /// value-native, so no page or bank state is involved.
    OscGlobalIn(crate::osc_global::GlobalCommand),
    /// A `Toggle` control whose direction depends on the current model.
    RmeToggle(crate::osc_rme::RmeToggle),
    /// `/1/mastervolume`: the Main Out pair fader.
    RmeMasterVolume(f32),
    /// `/2/gainRight`: the right side of an input pair.
    RmeGainRight(usize, u32),
    /// `/1/select/1/N`: ad-hoc fader-group selection.
    RmeSelect(ChannelId, bool),
    /// `/1/tracknameN` or `/2/trackname`.
    RmeRename(ChannelId, String),
    /// One page-2 dynamics field, resolved against the live `Dynamics`.
    RmeDynamicsField {
        channel: ChannelId,
        field: &'static str,
        value: f32,
    },
    /// One formatted line for the OSC debug log panel — sent for every
    /// message crossing the bridge in either direction, regardless of
    /// whether the panel is currently open (see `osc::worker`).
    OscLog(String),
    /// Toggles the OSC debug log panel (top-bar button, only shown when
    /// `--osc` is active).
    ToggleOscLog,
    ClearOscLog,
    /// Settings sheet: start/stop the OSC surface.
    SetOscEnabled(bool),
    /// Settings sheet: choose the classic banked or Global OSC dialect.
    SetOscMode(crate::osc::OscDialect),
    OscRecvPortChanged(String),
    OscSendPortChanged(String),
    /// Parse both port fields and restart the worker if they changed.
    ApplyOscPorts,
    /// Toggles the Settings panel (clock source, SPDIF,
    /// sample rate).
    ToggleDevicePanel,
    /// Shows or hides the FX column beside the mixer.
    ToggleFxPanel,
    /// Opens the Assign menu at its first level, or closes it.
    ToggleAssign,
    /// Opens the top-bar Options menu, or closes it.
    ToggleOptions,
    /// Opens the Reset Mix submenu.
    OptionsReset,
    /// Returns from Reset Mix to the Options root.
    OptionsBack,
    /// Opens the app-level Settings sheet from Options.
    OptionsSettings,
    /// Closes the Options menu.
    CloseOptions,
    /// Start/stop the virtual 5.1 and 7.1 outputs.
    SetSurround51(bool),
    SetSurround71(bool),
    /// First level: open and pin that destination's second level.
    AssignDestinations(usize),
    /// Pointer entered a first-level destination: switch an unpinned
    /// second level, but never close it.
    AssignHover(usize),
    /// Second level: assign an output to a destination, `None` clears it.
    AssignOutput(usize, Option<usize>),
    /// Choose the monitoring output that carries Cue/PFL.
    AssignCueTarget(usize),
    /// Toggles the About modal (version, icon, links; see `about`).
    ToggleAbout,
    /// The TotalControl settings popover (the app's own, not the device's).
    ToggleSettings,
    /// Opens the repository in the desktop's browser, from the About
    /// drawer's own link.
    OpenRepoUrl,
    ClockSourceSelected(String),
    SampleRateSelected(u32),

    // ── Right sidebar ("control strip") ─────────────────────────────
    /// Pops `undo_stack`, pushes the current state onto `redo_stack`,
    /// applies the popped one. No-op if `undo_stack` is empty.
    Undo,
    /// Mirror of `Undo` against `redo_stack`.
    Redo,
    /// Expands/collapses one of the 4 sidebar panels (each has its own
    /// "−"/"+" header, like the reference).
    ToggleSidebarPanel(sidebar::Panel),
    /// Expands/collapses the *whole* right sidebar — distinct from
    /// `ToggleSidebarPanel`, see `TotalControl::sidebar_open`'s own doc comment.
    ToggleSidebar,
    /// One horizontal drag step on the sidebar's own resize strip, in
    /// logical pixels (negative = the pointer moved left, which *widens*
    /// the sidebar) — see `sidebar::resize_strip`. Clamped to
    /// `sidebar::MIN_WIDTH`/`MAX_WIDTH` in `update`. UI-only: never
    /// pushed on `undo_stack`, never persisted.
    SidebarResize(f32),
    /// The Options panel's mixer layout (TotalMix's "2 Row"), see
    /// `RowMode`. UI-only.
    SetRowMode(RowMode),
    /// First-connect direction; see `SyncDirection`.
    SetSyncDirection(SyncDirection),
    /// Flips the hardware Dim (`RmeDevice::set_dim`), the same switch the
    /// front-panel DIM button toggles.
    DimToggle,
    /// Target-state dim (OSC); `DimToggle` remains the UI's relative toggle.
    Dim(bool),
    /// Control-room Speaker B switch.
    SpeakerB(bool),
    /// Restore the pre-Dim Phones level.
    ControlRoomRecall,
    /// Control-room Mono monitoring option (host-side).
    ControlRoomMono(bool),
    /// Control-room Mute FX option (host-side).
    ControlRoomMuteFx(bool),
    /// Control-room External Input engaged (device-backed).
    ExternalInput(bool),
    /// Control-room Talkback on/off.
    TalkbackChanged(bool),
    /// Control Room Settings dialog.
    OpenControlRoomSettings,
    CancelControlRoomSettings,
    ApplyControlRoomSettings,
    /// Restore the draft dialog values to their defaults without closing it.
    ResetControlRoomSettings,
    ControlRoomTalkbackInput(Option<usize>),
    ControlRoomTalkbackDim(f32),
    ControlRoomListenbackInput(Option<usize>),
    ControlRoomListenbackDim(f32),
    ControlRoomRecallDb(f32),
    ControlRoomMainDim(f32),
    ControlRoomSpeakerBLinked(bool),
    ControlRoomExternalInput(Option<usize>),
    ControlRoomExternalGain(f32),
    ControlRoomPflReplacesSolo(bool),
    ControlRoomExclusiveSoloPfl(bool),
    /// Recalls "Mix `u8`" (1-8) and marks it as the slot `SnapshotStore`
    /// will save into.
    SnapshotClicked(u8),
    SnapshotStore,
    /// Enters/exits group-assignment mode — while active, clicking a
    /// group's mute/solo/fader cell assigns the current multi-selection
    /// (`state.selected`) as that group's membership.
    GroupEditToggle,
    GroupLinkToggle(usize, sidebar::GroupLink),
    /// Clears the group currently being edited (or all four, if none
    /// is — see the handler).
    GroupClear,
    /// Recalls "Layout `u8`" (1-6) and marks it as the slot
    /// `LayoutStore` will save into.
    LayoutClicked(u8),
    LayoutStore,
    /// Double-click on a strip's name: start typing a new one for `cid`.
    RenameStart(ChannelId),
    RenameChanged(String),
    /// Enter: keep the typed name (an empty one restores the original).
    RenameCommit,
    /// The Options panel's "names" row, TotalMix's "Show Names": show the
    /// original channel names even where the user renamed one.
    SetShowOriginalNames(bool),
    /// The pointer entered (`true`) or left a Matrix cell: the source row,
    /// and the physical output column. Drives the crosshair labels and
    /// the view's readout.
    MatrixHover(ChannelId, usize, bool),
    /// Any left press in the window (from the global event listener).
    /// While a text field is open, it asks the field whether it still has
    /// focus, see `Message::FieldFocus`.
    PointerPressed,
    /// The answer to that question: `false` means the click landed outside
    /// the field (iced unfocuses a text input then), so its text is
    /// accepted, the way a click away commits an edit in any desktop app.
    /// Carries which field and which strip were open when asked, so a late
    /// answer cannot commit a different, newer edit.
    FieldFocus(EditField, ChannelId, bool),
    /// Force PipeWire's graph rate (Hz), `0` = back to its default. See
    /// `totalcontrol_core::pw_clock`: system-wide, until PipeWire restarts.
    EngineRate(u32),
    /// Force PipeWire's quantum (buffer size in samples), `0` = default.
    EngineQuantum(u32),
    /// Zero the transport-error display against the driver's current counters.
    ResetTransportErrors,
    /// Periodic re-read of the PipeWire clock (another app may change it).
    EnginePoll,
    /// The graph clock, the card's own open stream (`None` = idle), whether
    /// a Fireface playback sink is running, and the host sink's volume.
    EngineRead(
        Option<PwClock>,
        Option<CardStream>,
        bool,
        Option<(u32, f32)>,
    ),
    /// Set the Fireface PipeWire sink to unity (0 dB, 100 %).
    HostVolumeUnity,
}

/// The two inline text fields a strip can open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditField {
    Rename,
    Db,
}

/// How the Mixer lays its three sections out.  TotalMix offers the
/// default 3-row inline layout and the optional "2 Row" layout (Hardware
/// Inputs and Software Playback side by side, Hardware Outputs underneath).
/// `Auto` is a retired TotalControl-only width-dependent mode kept only so old
/// preference files deserialize; it is treated as `Three` and is no longer
/// offered in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum RowMode {
    Auto,
    #[default]
    Three,
    Two,
}

/// The strip-rename field's widget id, so `RenameStart` can focus it.
pub(crate) const RENAME_INPUT_ID: &str = "strip-rename";
/// The typed-dB field's widget id, for the same reason.
pub(crate) const DB_INPUT_ID: &str = "strip-db";

// ── App state ────────────────────────────────────────────────────

/// A startup difference between the values the UC can read back and the
/// saved TotalControl scene.  Only the readable/queued subset is listed; see
/// `docs/UC-PARAMETER-SYNC.md`.
#[derive(Clone)]
pub struct StartupMismatch {
    pub items: Vec<String>,
    pub device_state: totalcontrol_core::fireface::ReadableState,
    /// The saved scene, for "Use TotalControl settings": the connect write keeps
    /// the unit's readable values, so choosing TotalControl has to write them.
    pub totalcontrol_scene: Scene,
    /// The scene's input gains after the UCX half-dB migration.
    pub totalcontrol_gain: Vec<(usize, u32)>,
    /// Full decoded UCX dump, when the mismatch came from the dump.
    pub ucx_dump: Option<UcxDump>,
    /// Saved scene with all decoded device values applied, for "Use device".
    pub device_scene: Option<Scene>,
}

pub struct TotalControl {
    pub device: DeviceHandle,
    /// Stable identity of the selected unit, when the backend exposes one.
    /// The card index is deliberately not part of this.
    pub device_target: Option<DeviceTarget>,
    pub sel_out: usize,
    /// Last front-panel OUT selection seen by `Message::Tick` — when it
    /// changes, `sel_out` follows (TotalMix highlights the panel's
    /// current submix). `None` = never seen (no panel / first tick).
    pub last_panel_out: Option<usize>,
    pub view: View,
    /// The channel shown in the Quick Control view's source block — an
    /// `Input`/`Playback` id, defaulting to the first input. Independent of
    /// `selected` (multi-select in the Mixer/Matrix views) — Quick Control
    /// always focuses exactly one channel.
    pub quick_channel: ChannelId,
    pub editing: Option<ChannelId>,
    pub edit_buf: String,
    pub drag_range: Option<(ChannelId, f32, f32)>,
    pub modifiers: keyboard::Modifiers,
    /// Ballistics-smoothed meter values shown in the UI — the raw values
    /// from `device.input_meter`/`playback_meter` jump straight to their new
    /// reading every tick, which reads as flickery rather than a real meter
    /// needle. Smoothed here instead of at the device layer so it applies
    /// uniformly regardless of data source (mock or real hardware).
    pub input_meters: Vec<MeterAnim>,
    pub playback_meters: Vec<MeterAnim>,
    /// Host-side playback tap from PipeWire's sink monitor. Disabled in
    /// mock mode, where the mock's own levels are authoritative.
    pub playback_tap: pw_meter::PlaybackTap,
    /// Monitor channel count the tap currently has (0 = not running):
    /// 2 in the stereo profile (PB1 only), 12 in Pro Audio (all pairs).
    pub playback_tap_channels: usize,
    /// Host-computed output levels (power sum of routed sources ×
    /// faders) — same ballistics as the input/playback meters.
    pub output_meters: Vec<MeterAnim>,
    /// UCX FX bus meters: send and return.
    pub fx_meters: [MeterAnim; 2],
    /// UCX FX bus overload indicators: send and return.
    pub fx_overload: [bool; 2],
    /// Strips the user has collapsed to save horizontal space — presence in
    /// the set means collapsed. This is the *target* state; the strip may
    /// still be mid-transition, tracked separately in `collapse_anim`.
    pub collapsed: HashSet<ChannelId>,
    /// In-flight collapse/expand width animations, keyed by strip — only
    /// holds an entry while a strip is actually transitioning; pruned once
    /// settled (see `Message::CollapseTick`), which also lets the extra
    /// high-frequency redraw timer in `subscription()` shut itself off.
    pub collapse_anim: HashMap<ChannelId, strip::CollapseAnim>,
    /// Every strip+flyout currently open — the route bus picker or the
    /// 48V/PAD/Sensitivity settings panel, EQ, or Trim (see
    /// `strip::FlyoutKind`, `route_popover`, `settings_popover`,
    /// `Message::ToggleFlyout`). Any number can be open at once, on
    /// different strips or different kinds on the same strip — matches
    /// real TotalMix, which doesn't close one panel just because another
    /// opened (user-caught: TotalControl used to force a single global
    /// `Option`, closing whatever was open the moment a new gear icon
    /// was clicked). **Exception**: `Route` stays exclusive with itself
    /// (opening a new one still closes any other Route flyout) — its
    /// Stack-based overlay (`with_flyout`) only ever positions/renders
    /// one popover at a time, but both the old and new overlay now have
    /// their own width tween.  Switching Settings/EQ/Trim on the same
    /// strip queues the new panel until the old shrink finishes, so both
    /// directions always animate.
    pub flyout_open: HashSet<(ChannelId, strip::FlyoutKind)>,
    /// Width tween for the inline Settings/EQ/Trim panels, keyed like
    /// `flyout_open`.  Opening grows the panel from 0 to its natural width,
    /// pushing the following strips right; closing shrinks it again.  The
    /// animation is kept after `flyout_open` is cleared so the shrink can
    /// finish, then pruned by `CollapseTick`.
    pub flyout_anim: HashMap<(ChannelId, strip::FlyoutKind), strip::CollapseAnim>,
    /// Width tween for the second half of a stereo pair when Stereo is
    /// toggled.  Keyed by the pair's left `ChannelId`; the animated value
    /// is the right strip's width (0 when linked, full when split), so the
    /// whole rest of the row shifts at the fold-out speed.
    pub stereo_anim: HashMap<ChannelId, strip::CollapseAnim>,
    /// A different inline panel requested on the same strip while the old
    /// one is still sliding out.  It opens after that shrink finishes, so
    /// switching Settings ↔ EQ ↔ Trim never snaps the old panel away.
    pending_flyout: Option<(ChannelId, strip::FlyoutKind)>,
    /// Manual UI zoom for the mixer/matrix views — every text size and
    /// widget dimension there is multiplied by this. Changed only by
    /// `ZoomIn`/`ZoomOut`/`ZoomReset`/Ctrl+molette (`zoom`); it is NOT tied
    /// to window size, so resizing never rescales anything — strips keep
    /// fixed geometry and rows/pages scroll. `SCALE_DEFAULT` is the default.
    pub ui_scale: f32,
    /// F11 state: the window is currently fullscreen. Kept in sync with
    /// `Message::ToggleFullscreen`; the WM can still change the mode on
    /// its own, in which case this only affects the next F11 toggle.
    pub fullscreen: bool,
    /// Current window width in logical pixels, kept in sync via
    /// `Message::WindowResized` — lets `mixer_view` decide whether a
    /// strip row fits without scrolling (see `responsive_row`).
    /// `Length::Fill` alone can't answer that inside a horizontal
    /// `Scrollable`: iced compresses Fill back down to the content's
    /// natural size on the scroll axis, so centering has to be done by
    /// skipping the scrollable entirely when content already fits —
    /// which needs the real window width tracked in state.
    pub window_width: f32,
    /// The window's size, saved at close so the next launch opens the
    /// same (`prefs::Prefs::window_size`). Not written on every resize
    /// step: a drag would rewrite the file 20 times a second.
    pub window_size: Option<(f32, f32)>,
    /// The last time anything on screen changed or the user acted: while
    /// it is recent the UI ticks at `TICK_ACTIVE`, afterwards at
    /// `TICK_IDLE` (see `tick_interval`). An idle mixer used to redraw 20
    /// times a second for nothing: ~12 ms a frame, a quarter of a core.
    pub last_activity: Instant,
    /// Last hardware Phones volume read from the driver; a change switches
    /// the UI to the fast tick for a moment, so an idle mixer doesn't redraw
    /// at 30 Hz just to poll the physical knob.
    pub last_hw_volume: Option<f32>,
    /// A requested UC rate waiting for the card's ALSA stream to confirm
    /// it; lets the UI report a rate the unit actually accepted.
    pub pending_device_rate: Option<(u32, Instant)>,
    /// One-shot reapply of the startup scene after the UC meter taps have
    /// opened the card.  The UC can reload its stored mixer state on stream
    /// start, so the initial scene write can be lost.
    pub startup_scene_due: Option<Instant>,
    /// The host-owned scene used for autosave/save/exit and snapshot
    /// persistence.  Front-panel changes update the live device model but
    /// not this scene, matching Windows TotalMix: the saved host mix wins at
    /// the next connect.
    pub host_scene: Option<Scene>,
    /// A host mix change whose confirming service snapshot has not arrived
    /// yet.  The mirror only changes when that snapshot lands, so the host
    /// scene merge waits for it instead of comparing two identical
    /// captures (which recorded nothing).
    pub pending_host_merge: Option<(Scene, Instant)>,
    /// The host scene to reapply after the UC meter tap has opened the
    /// card.  Reapplying `capture_scene()` here was wrong: a stale
    /// front-panel `0x8b` event could already have replaced the output
    /// fader in the model, and the reapply then persisted the device value
    /// instead of the host scene.
    pub startup_scene: Option<Scene>,
    /// Previous RME sink running state, for the IDLE -> RUNNING reapply.
    pub last_sink_running: bool,
    /// Last non-idle card rate seen, kept across a brief stream close during
    /// a rate switch.  A rate change can happen while the sink never stops
    /// reporting RUNNING, so the sink transition alone is not enough to
    /// detect it.
    pub last_card_rate: Option<u32>,
    /// The notification-area icon is up (`tray::start` found a service):
    /// only then may the window be hidden, there is a way back.
    pub tray_active: bool,
    /// Closing the window hides it instead of quitting (needs `tray_active`).
    pub keep_in_tray: bool,
    /// The window is hidden in the tray right now.
    pub window_hidden: bool,
    /// The last failure worth telling the user about (a device write that
    /// was refused), shown for `NOTICE_FOR`.
    pub notice: Option<(String, Instant)>,
    /// The open window, if any: none while hidden in the tray.
    pub window_id: Option<window::Id>,
    /// An autostart entry exists (`tray::autostart_enabled`).
    pub autostart: bool,
    /// The first launch has no saved zoom: pick one from the monitor.
    pub initial_zoom_pending: bool,
    /// When the last resize was actually applied to `window_width`/
    /// `ui_scale` (`None` = never) — the resize coalescing throttle (see
    /// `RESIZE_THROTTLE`) compares against this so a stream of `Resized`
    /// events doesn't rebuild the whole view on every single one.
    last_resize_applied: Option<Instant>,
    /// The most recent resize width still waiting to be applied (it arrived
    /// inside `RESIZE_THROTTLE` of the last applied one) — flushed by the
    /// next due resize or the next `Message::Tick`. See `apply_pending_resize`.
    pending_resize_width: Option<f32>,
    /// Multi-selected strips — Ctrl+click toggles just the clicked strip,
    /// Shift+click selects the whole range from `select_anchor` (standard
    /// file-manager convention), click empty background to clear.
    /// Mute/solo/collapse/volume/pan applied to any selected strip apply
    /// to the whole selection at once.
    pub selected: HashSet<ChannelId>,
    /// The strip a plain click last focused.  Cursor Up/Down nudge this
    /// channel (or all of `selected` when a multi-selection exists), the
    /// way TotalMix's keyboard control works.  Kept separate from
    /// `selected` so a plain click does not turn into a multi-selection.
    pub focus_channel: Option<ChannelId>,
    /// The strip a future Shift+click's range is measured from — the
    /// most recent strip explicitly Ctrl- or Shift-clicked. Not moved by
    /// Shift+click itself, so repeated Shift+clicks pivot around the same
    /// point (letting you grow/shrink a range interactively) the way
    /// Explorer/Finder do.
    pub select_anchor: Option<ChannelId>,
    /// Which strip the cursor is currently over, if any — drives the hover
    /// border/glow in `theme::strip_panel`. Plain `container::Style`
    /// closures don't get a hover `Status` the way `button`'s do, so this
    /// has to be tracked explicitly via `mouse_area::on_enter`/`on_exit`
    /// (see `widgets/strip.rs`) rather than read off the widget itself.
    pub hovered_strip: Option<ChannelId>,
    /// Port config for the OSC control surface — `None` unless `--osc` was
    /// passed, in which case `subscription()` starts the worker.
    pub osc_config: Option<OscConfig>,
    /// Whether the OSC surface is enabled, independent of `osc_config` so
    /// the settings sheet can start and stop it at runtime.
    pub osc_enabled: bool,
    pub osc_recv_port: u16,
    pub osc_send_port: u16,
    pub osc_dialect: crate::osc::OscDialect,
    pub osc_recv_text: String,
    pub osc_send_text: String,
    /// `--osc` was passed: the command line overrides the saved switch.
    pub osc_cli: bool,
    /// Classic RME OSC navigation state: bus, bank, selected channel and
    /// page.  Kept on the UI thread so feedback and `Toggle` resolution see
    /// the same state as the parser.
    pub rme: crate::osc_rme::RmeState,
    /// Sender for outgoing OSC feedback packets, handed back by the worker
    /// via `Message::OscReady` once it's actually bound and listening.
    /// `None` until then (or always, if OSC isn't enabled) — `notify_osc`
    /// is a no-op in that case.
    pub osc_tx: Option<mpsc::Sender<OscOutbound>>,
    /// Ring buffer of formatted OSC traffic lines (see `Message::OscLog`),
    /// newest first — capped at `OSC_LOG_MAX` so a busy controller can't
    /// grow this unboundedly. Populated regardless of `show_osc_log`, so
    /// opening the panel after a burst of activity isn't a blank page.
    pub osc_log: VecDeque<String>,
    pub show_osc_log: bool,
    /// Docked Settings panel (device group, clock source, SPDIF
    /// flags) — same pattern as `show_osc_log`, see `device_panel`.
    pub show_device_panel: bool,
    /// Talkback is engaged.  Performance state, not part of the scene.
    pub talkback_on: bool,
    /// Persisted Control Room monitoring settings.
    pub control_room: ControlRoomSettings,
    /// Draft values while the Control Room Settings dialog is open.
    pub control_room_draft: ControlRoomSettings,
    pub control_room_settings_open: bool,
    /// The FX column beside the mixer: Reverb and Echo, in TotalMix's own
    /// place for them. Only meaningful when the backend reports `fx`.
    pub show_fx_panel: bool,
    /// Width tween for the FX column. `None` once settled; closing keeps
    /// the column in the layout until this reaches zero.
    pub fx_anim: Option<strip::CollapseAnim>,
    /// Control Room Assign: the output pair chosen for each of the first six
    /// destinations, then the Cue/PFL target index in the seventh slot. An
    /// assigned pair leaves the hardware-output row and shows up in the
    /// Control Room section instead.
    pub assign: [Option<usize>; 7],
    /// Which level of the Assign menu is open.
    pub assign_menu: AssignMenu,
    /// A click on a first-level destination pins its second level open.
    /// Hovering switches an unpinned second level; it never closes it.
    pub assign_pinned: bool,
    /// In-flight add or clear between the Hardware Outputs row and the
    /// Control Room.
    pub assign_anim: Option<AssignAnim>,
    /// Whether the About modal is open: the version, the icon and the
    /// links, layered above the whole window (see `about::about_modal`).
    pub show_about: bool,
    pub show_settings: bool,
    /// Top-bar Options menu state.
    pub options_menu: OptionsMenu,
    /// Persisted intent for the virtual surround outputs.
    pub surround_51: bool,
    pub surround_71: bool,
    /// Whether the helper processes are currently running.
    pub surround_51_running: bool,
    pub surround_71_running: bool,
    /// RME-style startup mismatch dialog.  `None` = no prompt.
    pub startup_mismatch: Option<StartupMismatch>,
    /// UCX startup dump still in flight; compare once it completes.
    pub startup_compare_pending: bool,
    /// Saved scene used for the deferred UCX comparison.
    pub startup_compare_scene: Option<Scene>,
    /// Suppress future mismatch prompts and write the saved TotalControl scene.
    pub always_init_device: bool,
    /// Whether the device this handle was opened on is still there, as of
    /// the last `Message::Tick` probe (`RmeDevice::is_connected`): the
    /// handle keeps answering for a card that is long gone, so the chip
    /// and About read this, not the handle's mere existence.
    pub connected: bool,
    /// How the device was opened (`None` = auto-detect), kept so a
    /// reconnect can use the same path (see `DeviceHandle::open_real`).
    backend: Option<String>,
    /// `--mock` was passed: never auto-reconnect, there is no hardware to
    /// come back and the mock was asked for.
    explicit_mock: bool,
    /// When the connection was last probed. The throttle for the Tick
    /// probe; `None` = never, so the first tick probes.
    last_conn_check: Option<Instant>,
    /// Debounce timer for the auto-saved mixer state (see
    /// `Message::Tick` — the device has no readback, so this is how the
    /// UI restores the gains/volumes/48V on the next open).
    pub last_auto_save: Instant,
    /// Last JSON written to the "auto" scene, to skip redundant writes.
    pub last_saved_json: Option<String>,

    // ── Right sidebar ────────────────────────────────────────────────
    /// Coarse, `Scene`-snapshot-based undo — see `Message::Undo`'s own
    /// doc comment for the granularity trade-off. Capped at
    /// `UNDO_STACK_CAP`.
    pub undo_stack: Vec<Scene>,
    pub redo_stack: Vec<Scene>,
    pub groups: [Group; 4],
    /// Whether the "edit" button is engaged — while `true`, clicking a
    /// group's mute/solo/fader cell assigns `selected` as that group's
    /// membership (see `Message::GroupLinkToggle`); while `false`, the
    /// same click just flips that link type on/off for the group's
    /// existing membership.
    pub group_editing: bool,
    /// `None` = that slot has never been stored to.
    pub layouts: [Option<HashSet<ChannelId>>; 6],
    pub active_layout: Option<usize>,
    pub active_snapshot: Option<usize>,
    /// Which "Mix n" slots have a stored scene on disk, so the sidebar can
    /// tell an empty slot from a stored one. Read once at startup, kept in
    /// step by `SnapshotStore`.
    pub stored_snapshots: [bool; 8],
    /// The simulated device's snapshots. A mock session never touches the
    /// real `Mix n` files (as its auto-save never does): storing there
    /// would overwrite the user's real snapshots with simulated data, and
    /// recalling a real one would push hardware state into the simulation.
    pub mock_snapshots: [Option<Scene>; 8],
    /// Which channels `Message::GlobalSoloToggle` most recently cleared
    /// — restored on the next press if nothing's been soloed again in
    /// the meantime. Empty when there's nothing to restore.
    pub last_cleared_solos: Vec<ChannelId>,
    /// Which channels `Message::GlobalMuteToggle` most recently
    /// unmuted, muted again on the next press if nothing has been muted in
    /// the meantime. Empty when there's nothing to restore.
    pub last_cleared_mutes: Vec<ChannelId>,
    /// Whether the mix has changed since `active_snapshot` was recalled
    /// or stored: TotalMix flashes the slot ("As soon as the mixer state
    /// is changed the button starts flashing"), the sidebar marks it.
    pub snapshot_dirty: bool,
    /// See `RowMode`.
    pub row_mode: RowMode,
    /// First-connect direction, persisted in the GUI prefs.  The default
    /// is device -> GUI, so launching never writes a stale GUI state over
    /// the live device.
    pub sync_direction: SyncDirection,
    pub sidebar_panels_open: sidebar::PanelsOpen,
    /// Whether the whole right sidebar is expanded — distinct from
    /// `sidebar_panels_open`, which collapses individual panels *within*
    /// an expanded sidebar. `false` reclaims its width for the mixer,
    /// leaving only a thin rail with the button to bring it back.
    pub sidebar_open: bool,
    /// Draggable width of the whole sidebar, in logical pixels — set by
    /// `Message::SidebarResize`, starting at `sidebar::DEFAULT_WIDTH` and
    /// clamped to `sidebar::MIN_WIDTH`/`MAX_WIDTH`. Like `sidebar_open`
    /// and `sidebar_panels_open`, session state: nothing persists it.
    pub sidebar_width: f32,
    /// User channel names, by the strip they were typed on (see
    /// `Message::RenameStart`). A GUI label only: the device model, OSC
    /// addresses and scenes keep the hardware names.
    pub labels: HashMap<ChannelId, String>,
    /// See `Message::SetShowOriginalNames`.
    pub show_original_names: bool,
    /// The strip whose name is being edited, and the text typed so far.
    pub renaming: Option<ChannelId>,
    pub rename_buf: String,
    /// Where `prefs::Prefs` is written. Set by `main` only, so tests (which
    /// build state through `new`) never touch the user's config.
    pub prefs_path: Option<std::path::PathBuf>,
    /// A preference changed since the last write; flushed on `Tick` and
    /// on window close (`SaveNow`).
    pub prefs_dirty: bool,
    /// The Matrix cell under the pointer (see `Message::MatrixHover`).
    pub matrix_hover: Option<(ChannelId, usize)>,
    /// Matrix Mono Mode: on = a crosspoint edit writes one mono cell;
    /// off = the cell edit follows the stereo pair (TotalMix's default).
    pub matrix_mono: bool,
    /// The continuous edit the last undo step was opened for, and when it
    /// last moved (see `undo_gesture`).
    pub undo_gesture: Option<(UndoGesture, Instant)>,
    /// Set when a text field opens. The press that opened it (the second
    /// click of the double-click) also reaches `PointerPressed`, after the
    /// widget's own message; this makes that one press not count as a
    /// click away.
    pub field_opening: bool,
    /// PipeWire's graph clock (rate, buffer), when the sound server owns
    /// it (`rate_follows_sound_server`); simulated for the mock, which
    /// never touches the real PipeWire; `None` for the USB backend.
    pub engine: Option<PwClock>,
    /// The rate / quantum the user forced here (`0` = PipeWire's own),
    /// kept in the preferences and applied again at launch.
    pub engine_force_rate: u32,
    pub engine_force_quantum: u32,
    /// What the card itself runs at while audio plays (kernel driver only):
    /// its rate and hardware period, which the sound server may pin
    /// regardless of the quantum. `None` while the card is idle.
    pub card_stream: Option<CardStream>,
    /// Driver transport counters as last read, and the baseline they are
    /// shown relative to (set on connect or by the reset button).
    pub transport_counters: Option<totalcontrol_core::fireface::abi::HwdepCounters>,
    pub transport_baseline: Option<totalcontrol_core::fireface::abi::HwdepCounters>,
    /// The Fireface PipeWire sink's volume (percent, dB), polled with the
    /// engine.  It is an attenuation *before* the device, so a sink below
    /// 0 dB makes the playback meters read low even for a full-scale file.
    pub host_volume: Option<(u32, f32)>,
    /// The device service, when the split is enabled.
    pub device_service: Option<crate::device_service::DeviceService>,
    pub device_updates:
        Option<std::sync::mpsc::Receiver<crate::device_service::Snapshot>>,
    /// Shared meter values for the canvases (stage 3).
    pub meter_bus:
        Option<std::sync::Arc<std::sync::Mutex<crate::device_service::MeterSnapshot>>>,
    /// The kernel driver's period floor (`urb_frames` for the UC), read at
    /// launch: which buffer sizes the card can actually run. `None` when
    /// unknown (no kernel driver), taken as the default 32.
    pub driver_floor: Option<u32>,
    /// The UC USB queue: `(frames per URB, URBs in flight)`, for the
    /// actual latency readout.
    pub driver_queue: Option<(u32, u32)>,
}

impl TotalControl {
    /// Bus labels for the current device, in internal pair order.
    ///
    /// The Babyface profile has six pairs; the UCX has nine.  `sel_out`
    /// and the matrix use these internal indices; the output strips use
    /// `display_pair_labels()` instead.
    pub fn pair_labels(&self) -> Vec<String> {
        let outs = self.device.outputs();
        (0..self.device.output_pair_count())
            .map(|p| {
                let left = outs
                    .get(p * 2)
                    .map(|o| short_label(o.name.as_str()))
                    .unwrap_or("");
                let right = outs
                    .get(p * 2 + 1)
                    .map(|o| short_label(o.name.as_str()))
                    .unwrap_or("");
                pair_bus_label(left, right)
            })
            .collect()
    }

    /// Labels in TotalMix's hardware-output display order.  The returned
    /// positions are display positions, not the internal pair indices used
    /// by the matrix and `sel_out`.
    pub fn display_pair_labels(&self) -> Vec<String> {
        let labels = self.pair_labels();
        self.device
            .output_display_order()
            .into_iter()
            .map(|pair| labels.get(pair).cloned().unwrap_or_default())
            .collect()
    }

    /// The output pairs assigned to the Control Room. The seventh `assign`
    /// slot is the Cue/PFL target, not an output pair, so it is excluded.
    pub fn assigned_pairs(&self) -> impl Iterator<Item = usize> + '_ {
        self.assign[..OUTPUT_DESTS].iter().flatten().copied()
    }

    pub fn is_output_assigned(&self, pair: usize) -> bool {
        self.assigned_pairs().any(|p| p == pair)
    }

    /// The monitoring output that carries Cue/PFL. `0` is Main.
    pub fn cue_target(&self) -> usize {
        self.assign[CUE_DEST].unwrap_or(0)
    }
}

/// The mock's stand-in for PipeWire's clock: what the real card runs with
/// nothing forced, 48 kHz and the 256-sample default the WirePlumber rule
/// requests (`node.latency`).
const SIMULATED_ENGINE: PwClock = PwClock {
    rate: 48_000,
    quantum: 256,
    force_rate: 0,
    force_quantum: 0,
};

/// What a continuous control is editing. A drag, a wheel spin or a knob
/// turn sends many messages; all of them on the same control, without a
/// pause longer than `UNDO_COALESCE`, are one undo step instead of one
/// per message (a knob turn used to need dozens of undos, and fader wheel
/// spins and Matrix edits, sent as `VolumeChanged`, were never undoable at
/// all).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UndoGesture {
    Volume(ChannelId, usize),
    Pan(ChannelId, usize),
    Gain(usize),
    Trim(ChannelId),
    EqBand(ChannelId, usize),
    LowCut(ChannelId),
    Dynamics(ChannelId),
    Reverb,
    Echo,
    Pitch,
    Width,
    ChannelWidth(ChannelId),
}

/// See `UndoGesture`. Long enough to bridge the gaps inside one wheel spin
/// or knob turn, short enough that two deliberate edits stay two steps.
const UNDO_COALESCE: Duration = Duration::from_millis(700);

fn undo_gesture(message: &Message) -> Option<UndoGesture> {
    Some(match message {
        Message::VolumeChanged(cid, out, _) | Message::FaderPressed(cid, out, ..) => {
            UndoGesture::Volume(*cid, *out)
        }
        Message::PanChanged(cid, out, _) => UndoGesture::Pan(*cid, *out),
        Message::Gain(idx, _) => UndoGesture::Gain(*idx),
        Message::TrimChanged(cid, _) => UndoGesture::Trim(*cid),
        Message::EqBandFreq(cid, band, _)
        | Message::EqBandQ(cid, band, _)
        | Message::EqBandGain(cid, band, _)
        | Message::EqBandPoint(cid, band, ..) => UndoGesture::EqBand(*cid, *band),
        Message::EqLowCutFreq(cid, _) => UndoGesture::LowCut(*cid),
        Message::EqLowCutEnabled(cid, _) => UndoGesture::LowCut(*cid),
        Message::DynamicsChanged(cid, _) | Message::DynamicsEnabled(cid, _) => {
            UndoGesture::Dynamics(*cid)
        }
        Message::ReverbChanged(_) => UndoGesture::Reverb,
        Message::EchoChanged(_) => UndoGesture::Echo,
        Message::PitchChanged(_) => UndoGesture::Pitch,
        Message::WidthChanged(_) => UndoGesture::Width,
        Message::ChannelWidthChanged(cid, _) => UndoGesture::ChannelWidth(*cid),
        _ => return None,
    })
}

/// Matches the "Max lines" default in oscmix's own OSC debug log — enough
/// history to catch a burst of activity without the panel scrolling
/// forever.
const OSC_LOG_MAX: usize = 500;

/// Matches the `Tick` subscription interval below — the release curve is
/// timed in real milliseconds rather than "per tick" so it stays correct if
/// that interval ever changes.
const METER_TICK_MS: f32 = 50.0;
/// Fast rise — a meter should jump to a new peak almost instantly so
/// transients don't feel muted.
const METER_ATTACK: f32 = 0.7;
/// Fast fall: more than four times quicker than the old 300 ms ease-out.
const METER_RELEASE: f32 = 0.9;
/// TotalMix's default peak-hold time.  The visible peak marker stays put
/// for this long after the last peak, then falls with a short ease-out
/// (the bar itself falls fast).  The 260 ms tail is deliberately a little
/// quicker than the original 300 ms so held peaks do not linger.
const METER_PEAK_HOLD_MS: f32 = 2000.0;
const PEAK_RELEASE_START: f32 = 0.26;
const PEAK_RELEASE_END: f32 = 0.06;
const PEAK_RELEASE_MS: f32 = 260.0;

/// Per-channel VU ballistics state.
#[derive(Clone, Copy, Debug)]
pub struct MeterAnim {
    /// Value as of the *previous* `step` — the start of the current
    /// keyframe transition `MeterFrame` interpolates from.
    prev_value: f32,
    value: f32,
    /// Held peak, drawn as a separate marker.
    peak: f32,
    /// Time since the peak last rose, for the 2 s hold.
    peak_hold_ms: f32,
    /// Time since the held peak started falling (after the hold).
    peak_release_ms: f32,
    /// When `value` was last computed — the display layer (`MeterFrame`)
    /// uses this to interpolate a smooth in-between value at full display
    /// refresh rate instead of jumping once per `Tick`.
    last_step_at: Instant,
}

impl MeterAnim {
    fn new() -> Self {
        Self {
            prev_value: 0.0,
            value: 0.0,
            peak: 0.0,
            peak_hold_ms: 0.0,
            peak_release_ms: 0.0,
            last_step_at: Instant::now(),
        }
    }

    pub fn frame(&self) -> fader::MeterFrame {
        fader::MeterFrame {
            prev: self.prev_value,
            value: self.value,
            peak: self.peak,
            peak_since: self.last_step_at,
            since: self.last_step_at,
        }
    }

    /// One nominal tick (`METER_TICK_MS`): what the ballistics were tuned
    /// with, and what the tests step by.
    #[cfg(test)]
    fn step(&mut self, target: f32) {
        self.step_for(target, METER_TICK_MS);
    }

    /// Steps by the time really elapsed since the last step. The attack
    /// and release factors are per nominal tick; a tick that is faster or
    /// slower (the UI ticks at 30 Hz with a signal, 2 Hz at rest) must not
    /// make a meter rise or fall faster or slower, so the factor is raised
    /// to the elapsed/nominal ratio.
    fn step_elapsed(&mut self, target: f32) {
        let dt = self.last_step_at.elapsed().as_secs_f32() * 1000.0;
        self.step_for(target, dt.clamp(1.0, 250.0));
    }

    fn step_for(&mut self, target: f32, dt_ms: f32) {
        self.prev_value = self.value;
        let ratio = dt_ms / METER_TICK_MS;
        let eff = |alpha: f32| 1.0 - (1.0 - alpha).powf(ratio);
        if target >= self.value {
            self.value += (target - self.value) * eff(METER_ATTACK);
        } else {
            self.value += (target - self.value) * eff(METER_RELEASE);
        }

        if target >= self.peak {
            self.peak = target;
            self.peak_hold_ms = 0.0;
            self.peak_release_ms = 0.0;
        } else {
            self.peak_hold_ms += dt_ms;
            if self.peak_hold_ms >= METER_PEAK_HOLD_MS {
                // The old ease-out is deliberately kept for the peak
                // marker: the bar snaps down fast, while the little peak
                // line glides down like an analog peak-hold.
                self.peak_release_ms =
                    (self.peak_release_ms + dt_ms).min(PEAK_RELEASE_MS);
                let t = self.peak_release_ms / PEAK_RELEASE_MS;
                let alpha = PEAK_RELEASE_END
                    + (PEAK_RELEASE_START - PEAK_RELEASE_END) * (1.0 - t) * (1.0 - t);
                self.peak += (target - self.peak) * eff(alpha);
                if self.peak < target + 0.001 {
                    self.peak = target;
                }
            }
        }
        self.last_step_at = Instant::now();
    }
}

#[cfg(test)]
pub fn new(mock: bool, osc_config: Option<OscConfig>, backend: Option<String>) -> TotalControl {
    new_with_device(mock, osc_config, backend, None, None)
}

/// Like [`new`], but honours the device selected in the UI/prefs at
/// startup.  Falls back to auto-detection when that device is not present.
pub fn new_with_device(
    mock: bool,
    osc_config: Option<OscConfig>,
    backend: Option<String>,
    preferred: Option<DeviceKind>,
    preferred_serial: Option<String>,
) -> TotalControl {
    let device = if mock {
        DeviceHandle::open_mock()
    } else {
        let available = DeviceHandle::available_targets();
        let selected = choose_startup_target(
            &available,
            preferred,
            preferred_serial.as_deref(),
        );
        if available.len() == 1 && selected.as_ref().map(|t| t.kind) != preferred {
            log::info!(
                "Auto-selected {}: it is the only Fireface visible",
                selected.as_ref().map(DeviceTarget::label).unwrap_or_else(|| "device".into())
            );
        }
        let preferred = selected.as_ref().and_then(DeviceHandle::open_target);
        match preferred.or_else(|| DeviceHandle::open_real(backend.as_deref())) {
            Some(device) => device,
            None if backend.is_some() => {
                eprintln!(
                    "No device found for --backend {}; refusing to start in simulation mode. \
                     Use --mock explicitly for the simulation.",
                    backend.as_deref().unwrap_or("")
                );
                std::process::exit(1);
            }
            None => {
                eprintln!("No device found. Use --mock for simulation.");
                DeviceHandle::open_mock()
            }
        }
    };
    // The saved scene is loaded in `apply_startup_sync`, after the
    // preferences are known, so the UC mismatch prompt and the
    // "Always init DSP devices" preference can run first.
    // USB has no readback: its only meaningful direction is GUI -> device.
    // ALSA/UCX starts in the safe Device -> GUI direction.
    let sync_direction = if device.needs_scene_restore() {
        SyncDirection::GuiToDevice
    } else {
        SyncDirection::DeviceToGui
    };
    let n_inputs = device.inputs().len();
    let n_playbacks = device.playbacks().len();
    let n_outputs = device.outputs().len();
    // Probed before the literal: `device` moves into the struct, so it
    // cannot be asked afterwards.
    let connected = device.is_connected();
    let simulated = device.is_mock();
    // A fallback mock is only a placeholder while no hardware is present;
    // leave its target empty so the reconnect probe searches for a real
    // unit instead of reopening the mock.
    let device_target = if simulated && !mock {
        None
    } else {
        Some(device.target())
    };
    let persist_target = device_target
        .as_ref()
        .is_some_and(|target| target.serial.is_some());
    // Mock generates its own playback levels; the UCX has the driver's
    // IF5 OUT peaks.  The other real backends still need the PipeWire
    // sink monitor (see `pw_meter`).
    let playback_tap = if device.is_mock() || device.has_device_playback_meters() {
        pw_meter::PlaybackTap::disabled()
    } else {
        pw_meter::PlaybackTap::start()
    };
    // `--osc` is a development override: it starts the surface regardless
    // of the saved preference.  Without it the settings sheet owns the
    // switch and the two ports.
    let osc_cli = osc_config.is_some();
    let osc_recv_port = osc_config.map(|c| c.recv_port).unwrap_or(9000);
    let osc_send_port = osc_config.map(|c| c.send_port).unwrap_or(9001);
    let osc_dialect = osc_config
        .map(|c| c.dialect)
        .unwrap_or(crate::osc::OscDialect::Classic);
    let osc_enabled = osc_cli;

    let state = TotalControl {
        device,
        device_target,
        connected,
        sel_out: 0,
        last_panel_out: None,
        view: View::Quick,
        quick_channel: ChannelId::Input(0),
        editing: None,
        edit_buf: String::new(),
        drag_range: None,
        modifiers: keyboard::Modifiers::default(),
        input_meters: vec![MeterAnim::new(); n_inputs],
        playback_meters: vec![MeterAnim::new(); n_playbacks],
        playback_tap,
        playback_tap_channels: 0,
        output_meters: vec![MeterAnim::new(); n_outputs],
        fx_meters: [MeterAnim::new(), MeterAnim::new()],
        fx_overload: [false, false],
        collapsed: HashSet::new(),
        collapse_anim: HashMap::new(),
        flyout_open: HashSet::new(),
        flyout_anim: HashMap::new(),
        stereo_anim: HashMap::new(),
        pending_flyout: None,
        ui_scale: theme::SCALE_DEFAULT,
        fullscreen: false,
        // Matches `window::Settings::size` in main.rs — updated for real
        // as soon as the first `Opened`/`Resized` event arrives.
        window_size: None,
        last_activity: Instant::now(),
        last_hw_volume: None,
        pending_device_rate: None,
        startup_scene_due: None,
        startup_scene: None,
        host_scene: None,
        pending_host_merge: None,
        last_sink_running: false,
        last_card_rate: None,
        tray_active: false,
        keep_in_tray: true,
        window_hidden: false,
        notice: None,
        window_id: None,
        autostart: crate::tray::autostart_enabled(),
        initial_zoom_pending: false,
        window_width: 1280.0,
        last_resize_applied: None,
        pending_resize_width: None,
        selected: HashSet::new(),
        focus_channel: None,
        select_anchor: None,
        hovered_strip: None,
        osc_config,
        osc_enabled,
        osc_recv_port,
        osc_send_port,
        osc_dialect,
        osc_recv_text: osc_recv_port.to_string(),
        osc_send_text: osc_send_port.to_string(),
        osc_cli,
        rme: crate::osc_rme::RmeState::default(),
        osc_tx: None,
        osc_log: VecDeque::new(),
        show_osc_log: false,
        show_device_panel: false,
        talkback_on: false,
        control_room: ControlRoomSettings::default(),
        control_room_draft: ControlRoomSettings::default(),
        control_room_settings_open: false,
        show_fx_panel: true,
        fx_anim: None,
        assign: {
            let mut assign = [None; 7];
            // The seventh slot is the Cue/PFL target; Main is the default.
            assign[CUE_DEST] = Some(0);
            assign
        },
        assign_menu: AssignMenu::Closed,
        assign_pinned: false,
        assign_anim: None,
        show_about: false,
        show_settings: false,
        options_menu: OptionsMenu::Closed,
        surround_51: false,
        surround_71: false,
        surround_51_running: false,
        surround_71_running: false,
        startup_mismatch: None,
        startup_compare_pending: false,
        startup_compare_scene: None,
        always_init_device: false,
        backend,
        explicit_mock: mock,
        last_conn_check: None,
        // State persistence (the device has no gain/volume readback —
        // TotalMix re-applies its saved state; this is our equivalent).
        last_auto_save: Instant::now(),
        last_saved_json: None,
        undo_stack: Vec::new(),
        redo_stack: Vec::new(),
        groups: Default::default(),
        group_editing: false,
        layouts: std::array::from_fn(|i| crate::layouts::load_layout_file((i + 1) as u8)),
        active_layout: None,
        active_snapshot: None,
        stored_snapshots: std::array::from_fn(|i| {
            !simulated
                && crate::scenes::scenes_dir()
                    .join(format!("Mix {}.json", i + 1))
                    .exists()
        }),
        mock_snapshots: Default::default(),
        last_cleared_solos: Vec::new(),
        last_cleared_mutes: Vec::new(),
        snapshot_dirty: false,
        row_mode: RowMode::Three,
        sync_direction,
        sidebar_panels_open: sidebar::PanelsOpen::default(),
        sidebar_open: true,
        sidebar_width: sidebar::DEFAULT_WIDTH,
        labels: HashMap::new(),
        show_original_names: false,
        renaming: None,
        rename_buf: String::new(),
        prefs_path: None,
        // Persist a resolved hardware serial even on a first launch: the
        // old preference file may only have the model-level label.
        prefs_dirty: persist_target,
        matrix_hover: None,
        matrix_mono: false,
        undo_gesture: None,
        field_opening: false,
        engine: simulated.then_some(SIMULATED_ENGINE),
        engine_force_rate: 0,
        engine_force_quantum: 0,
        card_stream: None,
        transport_counters: None,
        transport_baseline: None,
        host_volume: None,
        device_service: None,
        device_updates: None,
        meter_bus: None,
        // The mock stands for the stock driver.
        driver_floor: simulated.then_some(32),
        driver_queue: simulated.then_some((48, 2)),
    };

    state
}

/// Attach the service for the running application.  Kept separate from
/// `new_with_device` because startup sync (scene readback, mismatch check,
/// meter tap) must run on the direct handle first.
pub(crate) fn maybe_attach_device_service(state: &mut TotalControl) {
    if crate::device_service::requested() {
        attach_device_service(state);
    }
}

/// Move the device onto the service thread and leave a mirror in its
/// place.  Split from `new_with_device` so the boundary can be exercised
/// without touching the process environment.
pub(crate) fn attach_device_service(state: &mut TotalControl) {
    if state.device_service.is_some() {
        return;
    }
    // The mock placeholder is dropped immediately; it only satisfies the
    // type until the service has taken the real handle.
    let placeholder = DeviceHandle::Mock(
        MockBabyfacePro::open().expect("placeholder mock"),
    );
    let device = std::mem::replace(&mut state.device, placeholder);
    let mut service = crate::device_service::DeviceService::spawn(device);
    let updates = service.take_updates();
    let snapshot = service.snapshot();
    let mirror = crate::device_service::Mirror::new(
        snapshot,
        service.command_sender(),
        service.meter_bus(),
    );
    state.meter_bus = Some(service.meter_bus());
    state.device = DeviceHandle::Mirror(mirror);
    state.device_updates = updates;
    state.device_service = Some(service);
}

/// Rebuild the effective OSC config from the settings switch and ports.
/// The subscription identifies a running worker by this value, so changing
/// it stops the old worker and starts a new one on the next frame.
fn rebuild_osc_config(state: &mut TotalControl) {
    if state.osc_enabled {
        state.osc_config = Some(OscConfig {
            recv_port: state.osc_recv_port,
            send_port: state.osc_send_port,
            send_host: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            dialect: state.osc_dialect,
        });
    } else {
        state.osc_config = None;
        state.osc_tx = None;
    }
    state.osc_recv_text = state.osc_recv_port.to_string();
    state.osc_send_text = state.osc_send_port.to_string();
}

/// The persisted subset of `state` (see `prefs.rs`).
pub fn prefs_of(state: &TotalControl) -> crate::prefs::Prefs {
    let p = &state.sidebar_panels_open;
    let mut labels: Vec<(ChannelId, String)> =
        state.labels.iter().map(|(c, l)| (*c, l.clone())).collect();
    // Stable order, so an unchanged state writes an unchanged file.
    labels.sort_by_key(|(c, _)| format!("{c:?}"));
    let mut collapsed: Vec<ChannelId> = state.collapsed.iter().copied().collect();
    collapsed.sort_by_key(|c| format!("{c:?}"));
    // Route is a transient bus picker, not a channel control panel; do not
    // reopen it on the next launch.
    // Channel panels are transient; reopening them on the next launch
    // would widen a strip row without the user asking for it.
    let flyouts: Vec<(ChannelId, strip::FlyoutKind)> = Vec::new();
    crate::prefs::Prefs {
        view: Some(state.view),
        device: Some(state.device.kind().label().to_string()),
        device_serial: state
            .device_target
            .as_ref()
            .and_then(|target| target.serial.clone()),
        sidebar_open: Some(state.sidebar_open),
        fx_panel: Some(state.show_fx_panel),
        assign: state.assign,
        surround_51: state.surround_51,
        surround_71: state.surround_71,
        control_room: state.control_room.clone(),
        sidebar_width: Some(state.sidebar_width),
        panels_open: Some([p.options, p.snapshots, p.groups, p.layout]),
        two_row: None,
        row_mode: Some(state.row_mode),
        ui_scale: Some(state.ui_scale),
        labels,
        show_original_names: state.show_original_names,
        window_size: state.window_size,
        // Only a choice to leave the default is worth writing down.
        keep_in_tray: (!state.keep_in_tray).then_some(false),
        engine_force_rate: Some(state.engine_force_rate),
        engine_force_quantum: Some(state.engine_force_quantum),
        collapsed,
        flyouts,
        sel_out: Some(state.sel_out),
        sync_direction: Some(state.sync_direction),
        always_init_device: state.always_init_device,
        osc_enabled: state.osc_enabled,
        osc_mode: state.osc_dialect,
        osc_recv_port: state.osc_recv_port,
        osc_send_port: state.osc_send_port,
        input_pair_link: state.device.settings().input_pair_link.clone(),
        output_link: state.device.settings().output_link.clone(),
    }
}

/// Restores what `prefs_of` saved. Values are clamped to the same bounds
/// the live controls use, so a hand-edited file cannot open the app in a
/// state the UI could never reach.
pub fn apply_prefs(state: &mut TotalControl, prefs: crate::prefs::Prefs) {
    if let Some(v) = prefs.view {
        state.view = v;
    }
    if let Some(open) = prefs.sidebar_open {
        state.sidebar_open = open;
    }
    if let Some(open) = prefs.fx_panel {
        state.show_fx_panel = open;
    }
    state.assign = prefs.assign;
    state.surround_51 = prefs.surround_51;
    state.surround_71 = prefs.surround_71;
    state.control_room = prefs.control_room;
    state.control_room_draft = state.control_room.clone();
    // Older files stored an output pair in the seventh slot. It cannot be
    // interpreted as a monitoring target, so anything outside the Cue
    // target list falls back to Main.
    let cue = state.assign[CUE_DEST]
        .filter(|target| CUE_TARGETS.contains(target))
        .unwrap_or(0);
    state.assign[CUE_DEST] = Some(cue);
    if let Some(w) = prefs.sidebar_width {
        // Move installs still on the old default to the new, slightly
        // narrower default; an explicitly chosen width is left alone.
        let w = if (w - sidebar::OLD_DEFAULT_WIDTH).abs() < 0.5 {
            sidebar::DEFAULT_WIDTH
        } else {
            w
        };
        state.sidebar_width = w.clamp(sidebar::MIN_WIDTH, sidebar::MAX_WIDTH);
    }
    if let Some([options, snapshots, groups, layout]) = prefs.panels_open {
        state.sidebar_panels_open = sidebar::PanelsOpen {
            options,
            snapshots,
            groups,
            layout,
        };
    }
    // TotalMix has two supported layouts: 3 rows (default) and 2 rows.
    // `Auto` was a TotalControl-only width-dependent mode and is deliberately
    // retired: an old preference containing it opens as the 3-row default.
    state.row_mode = match (prefs.row_mode, prefs.two_row) {
        (Some(RowMode::Auto), _) => RowMode::Three,
        (Some(mode), _) => mode,
        (None, Some(true)) => RowMode::Two,
        _ => RowMode::Three,
    };
    if let Some(z) = prefs.ui_scale {
        state.ui_scale = z.clamp(theme::SCALE_MIN, theme::SCALE_MAX);
    } else {
        // Never saved: the first launch, see `Message::MonitorSize`.
        state.initial_zoom_pending = true;
    }
    state.labels = prefs
        .labels
        .into_iter()
        .filter(|(_, l)| !l.trim().is_empty())
        .collect();
    state.show_original_names = prefs.show_original_names;
    state.window_size = prefs.window_size;
    state.keep_in_tray = prefs.keep_in_tray.unwrap_or(true);
    state.engine_force_rate = prefs.engine_force_rate.unwrap_or(0);
    state.engine_force_quantum = prefs.engine_force_quantum.unwrap_or(0);
    state.always_init_device = prefs.always_init_device;
    if !state.osc_cli {
        state.osc_recv_port = if prefs.osc_recv_port == 0 {
            9000
        } else {
            prefs.osc_recv_port
        };
        state.osc_send_port = if prefs.osc_send_port == 0 {
            9001
        } else {
            prefs.osc_send_port
        };
        state.osc_enabled = prefs.osc_enabled;
        state.osc_dialect = prefs.osc_mode;
        rebuild_osc_config(state);
    }

    // Apply the link groupings before the per-channel UI state, so a saved
    // flyout/collapse on a now-linked right channel is moved to the one
    // strip the row actually shows.
    if !prefs.input_pair_link.is_empty() {
        state.device.settings_mut().input_pair_link = prefs.input_pair_link;
    }
    if !prefs.output_link.is_empty() {
        state.device.settings_mut().output_link = prefs.output_link;
    }

    // Per-channel GUI state.  Filter out ids the current device does not
    // have (a prefs file from another model) and the transient Route bus
    // picker.  These are pure GUI state and never touch the device.
    state.collapsed = prefs
        .collapsed
        .into_iter()
        .map(|cid| normalize_channel(state, cid))
        .filter(|&cid| channel_exists(state, cid))
        .collect();
    // Flyouts are transient UI state.  Keep old preference files readable,
    // but start with every panel closed so a saved Settings panel cannot
    // push a channel row past the window edge.
    state.flyout_open.clear();
    if let Some(out) = prefs.sel_out {
        let pairs = state.device.output_pair_count().max(1);
        state.sel_out = out.min(pairs - 1);
    }

    // USB has no readback; it must always restore GUI -> device.  The UCX
    // honours the user's choice, defaulting to the safe Device -> GUI.
    state.sync_direction = if state.device.needs_scene_restore() {
        SyncDirection::GuiToDevice
    } else if state.device.device_state_is_authoritative() {
        SyncDirection::DeviceToGui
    } else {
        prefs.sync_direction.unwrap_or(SyncDirection::DeviceToGui)
    };
    state.device.sync_settings_to_device();
}

/// Push the Control Room Main and Speaker B assignments from the saved
/// preferences into the open device.  The visual assignment map and the
/// device's `main_out`/`main_b_out` are separate records; Dim and Recall
/// read the device model, so the two must agree after startup, a device
/// switch or a scene load.
/// Output pair currently selected as the Cue/PFL target.
fn control_room_cue_pair(state: &TotalControl) -> Option<usize> {
    let dest = state.cue_target();
    state.assign.get(dest).and_then(|slot| *slot)
}

/// Push the runtime-only Control Room state (Cue target, PFL mode, External
/// Input) into the device.  Safe to call after connects, scene loads and
/// settings changes.
fn sync_control_room_runtime(state: &mut TotalControl) {
    if let Err(e) = state.device.set_cue_target(control_room_cue_pair(state)) {
        log::warn!("Could not set the Cue/PFL target: {e}");
    }
    if let Err(e) = state.device.set_pfl_mode(
        state.control_room.pfl_replaces_solo,
        state.control_room.exclusive_solo_pfl,
    ) {
        log::warn!("Could not set PFL mode: {e}");
    }
    let active = state.device.settings().external_input;
    if active {
        if let Err(e) = state.device.set_external_input(false, None, 0.0) {
            log::warn!("Could not release External Input: {e}");
        }
        if let Some(source) = state.control_room.external_input {
            if let Err(e) = state.device.set_external_input(
                true,
                Some(source),
                state.control_room.external_gain_db,
            ) {
                log::warn!("Could not re-engage External Input: {e}");
            }
        }
    }
}

pub fn apply_control_room_assignments(
    state: &mut TotalControl,
) -> Result<(), totalcontrol_core::Error> {
    // Preferences can outlive the device that wrote them.  Drop destinations
    // this unit cannot address and keep a hardware pair in one slot only.
    let pairs = state.device.output_pair_count();
    let mut seen = HashSet::new();
    for slot in state.assign.iter_mut().take(OUTPUT_DESTS) {
        if let Some(pair) = *slot {
            if pair >= pairs || !seen.insert(pair) {
                *slot = None;
            }
        }
    }
    let main = state.assign.first().copied().flatten();
    if state.device.settings().main_out != main {
        state.device.set_main_out(main)?;
    }
    // Include `None`: clearing the Speaker B destination must clear the
    // device assignment too, otherwise the Speaker B button can still latch
    // a pair the Control Room no longer shows.
    let main_b = state.assign.get(1).copied().flatten();
    if state.device.settings().main_b_out != main_b {
        state.device.set_main_out_b(main_b)?;
    }
    state
        .device
        .set_speaker_b_linked(state.control_room.speaker_b_linked)?;
    enforce_control_room_stereo(state);
    sync_control_room_runtime(state);
    Ok(())
}

/// Locate the surround helper in a development checkout, through the
/// environment, or on PATH.
fn surround_script_path() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("TOTALCONTROL_SURROUND_SCRIPT") {
        let path = std::path::PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        for dir in exe.ancestors().skip(1) {
            let candidate = dir.join("scripts/totalcontrol-surround.sh");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("totalcontrol-surround");
            if candidate.is_file() {
                return Some(candidate);
            }
            let candidate = dir.join("totalcontrol-surround.sh");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn run_surround_command(command: &str, target: Option<&str>) -> Result<(), String> {
    let script = surround_script_path()
        .ok_or_else(|| "surround helper not found".to_string())?;
    let mut process = std::process::Command::new("bash");
    process.arg(script).arg(command);
    if let Some(target) = target {
        process.arg(target);
    }
    let status = process.status().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("helper exited with {status}"))
    }
}

/// Start or stop the virtual surround outputs according to the settings.
pub fn sync_surround_bridge(state: &mut TotalControl) {
    if state.device.is_mock() {
        state.surround_51_running = false;
        state.surround_71_running = false;
        return;
    }
    for (wanted, running, kind) in [
        (state.surround_51, &mut state.surround_51_running, "51"),
        (state.surround_71, &mut state.surround_71_running, "71"),
    ] {
        if wanted == *running {
            continue;
        }
        let command = if wanted {
            format!("start{kind}")
        } else {
            format!("stop{kind}")
        };
        let target = if wanted {
            pw_clock::fireface_sink_name()
        } else {
            None
        };
        match run_surround_command(&command, target.as_deref()) {
            Ok(()) => *running = wanted,
            Err(e) => {
                state.notice = Some((
                    format!("Could not {} virtual {kind}: {e}", if wanted { "start" } else { "stop" }),
                    Instant::now(),
                ));
            }
        }
    }
}

/// Stop active sinks when TotalControl is really quitting.  Hiding to the
/// tray keeps the application running, so it keeps the sinks too.
pub fn stop_surround_bridge(state: &mut TotalControl) {
    if state.surround_51_running {
        let _ = run_surround_command("stop51", None);
        state.surround_51_running = false;
    }
    if state.surround_71_running {
        let _ = run_surround_command("stop71", None);
        state.surround_71_running = false;
    }
}

/// Restart active sinks after a device switch or reconnect.
pub fn restart_surround_bridge(state: &mut TotalControl) {
    stop_surround_bridge(state);
    sync_surround_bridge(state);
}

/// A linked pair is represented by its left/even strip.  Move a saved
/// per-channel UI entry on the hidden right channel to that representative
/// so Settings/EQ/Trim panels and collapsed state cannot get stranded.
fn normalize_channel(state: &TotalControl, cid: ChannelId) -> ChannelId {
    match cid {
        ChannelId::Input(i) if state.device.input_pair_linked(i / 2) => {
            ChannelId::Input(i & !1)
        }
        ChannelId::Playback(i) if state.device.playback_linked(i / 2) => {
            ChannelId::Playback(i & !1)
        }
        ChannelId::Output(i) if state.device.output_linked(i / 2) => {
            ChannelId::Output(i & !1)
        }
        _ => cid,
    }
}

/// Whether `cid` names a strip the currently open device actually has.
fn channel_exists(state: &TotalControl, cid: ChannelId) -> bool {
    match cid {
        ChannelId::Input(i) => i < state.device.inputs().len(),
        ChannelId::Playback(i) => i < state.device.playbacks().len(),
        ChannelId::Output(i) => i < state.device.outputs().len(),
    }
}

/// Open channel panels named on the command line before the first frame.
///
/// The spec is a comma-separated list of `kind:channel:index`, where `kind`
/// is `settings`, `eq`, `dyn`, `trim` or `route` and `channel` is `input`,
/// `playback` or `output` (short forms `in`, `pb`, `out` also work).  This
/// exists for screenshots and UI checks: it avoids hunting for a gear icon
/// through a synthetic pointer.
pub fn open_startup_flyouts(state: &mut TotalControl, spec: &str) {
    for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let mut fields = part.split(':');
        let kind = match fields.next() {
            Some("settings") => strip::FlyoutKind::Settings,
            Some("eq") => strip::FlyoutKind::Eq,
            Some("dyn" | "dynamics") => strip::FlyoutKind::Dynamics,
            Some("trim") => strip::FlyoutKind::Trim,
            Some("route") => strip::FlyoutKind::Route,
            other => {
                eprintln!("Unknown --flyouts panel {other:?} (use settings|eq|dyn|trim|route)");
                continue;
            }
        };
        let channel = match fields.next() {
            Some("input" | "in") => "input",
            Some("playback" | "pb") => "playback",
            Some("output" | "out") => "output",
            other => {
                eprintln!("Unknown --flyouts channel {other:?} (use input|playback|output)");
                continue;
            }
        };
        let Some(index) = fields.next().and_then(|s| s.parse::<usize>().ok()) else {
            eprintln!("Missing or invalid --flyouts channel index in {part:?}");
            continue;
        };
        let cid = match channel {
            "input" => ChannelId::Input(index),
            "playback" => ChannelId::Playback(index),
            _ => ChannelId::Output(index),
        };
        if !channel_exists(state, cid) {
            eprintln!("--flyouts: {part:?} does not exist on this device");
            continue;
        }
        state.flyout_open.insert((cid, kind));
    }
}

/// Scene key for the current physical unit.  The model keeps old files and
/// the TUI fallback working; the serial separates two units of the same model.
fn scene_key(state: &TotalControl) -> String {
    let model = state.device.canonical_model();
    match state.device_target.as_ref().and_then(|t| t.serial.as_deref()) {
        Some(serial) => format!("{model}-{serial}"),
        None => model.to_string(),
    }
}

fn load_scene_for_current(state: &TotalControl) -> Option<Scene> {
    // The hwdep Fireface backends use per-unit (UC/UCX with serial) or
    // per-model (UC without a serial) files.  The older USB/ALSA backends
    // and the TUI still share auto.json; preserve that path for them.
    if state.device.is_uc_model()
        || state
            .device_target
            .as_ref()
            .is_some_and(|target| target.serial.is_some())
    {
        let key = scene_key(state);
        let model = state.device.canonical_model();
        totalcontrol_core::scene::load_auto_scene_for(&key)
            .or_else(|| {
                if key != model {
                    totalcontrol_core::scene::load_auto_scene_for(model)
                } else {
                    None
                }
            })
            // Migration from the pre-serial shared auto.json.  This is only
            // used until the first per-unit save exists.
            .or_else(totalcontrol_core::scene::load_auto_scene)
    } else {
        totalcontrol_core::scene::load_auto_scene()
    }
}

fn save_scene_for_current(state: &TotalControl, scene: &Scene) -> Result<(), String> {
    if state.device.is_uc_model()
        || state
            .device_target
            .as_ref()
            .is_some_and(|target| target.serial.is_some())
    {
        totalcontrol_core::scene::save_auto_scene_for(&scene_key(state), scene)
    } else {
        totalcontrol_core::scene::save_auto_scene(scene)
    }
}

/// Legacy backends and single-unit UCX sessions without a serial still use
/// the shared auto.json that the TUI also writes.
fn uses_shared_auto_scene(state: &TotalControl) -> bool {
    !state.device.is_uc_model()
        && !state
            .device_target
            .as_ref()
            .is_some_and(|target| target.serial.is_some())
}

/// First-connect sync, after the preferences are known.
///
/// Backends without a full mixer readback restore their saved scene.  For
/// the UC, TotalControl compares the readable/queued device values first and
/// raises the RME-style mismatch prompt unless "Always init DSP devices
/// with TotalMix FX settings" is enabled.  The scene is still the default
/// choice, so the unit and the model agree when the dialog is answered.
pub fn apply_startup_sync(state: &mut TotalControl) {
    if state.device.is_mock() {
        return;
    }
    if state.host_scene.is_none() {
        state.host_scene = Some(state.device.capture_scene());
    }
    if state.device.needs_scene_restore() {
        let model = state.device.canonical_model().to_string();
        // The UCX state stream starts with the unit's current gain, output
        // volumes and monitor flags.  Drain it before applying the scene so
        // the read-back gain wins over the saved value.  The UC path keeps
        // its startup suppression and is not drained here.
        if state.device.is_ucx_model() {
            let _ = state.device.poll_events();
        }
        let scene = load_scene_for_current(state);
        let Some(scene) = scene else {
            log::warn!("no auto scene found for {model}");
            state.device.begin_metering();
            return;
        };
        log::info!(
            "loaded startup scene for {model}: pb1={:?} out4(vol/loop)={:?}/{:?}",
            scene.playbacks.first().map(|p| p.volumes.clone()),
            scene.outputs.get(4).map(|o| o.volume),
            scene.outputs.get(4).map(|o| o.loopback),
        );
        state.host_scene = Some(scene.clone());
        if state.device.is_ucx_model() && !state.always_init_device {
            if let Err(e) =
                state
                    .device
                    .begin_startup_state_dump(Duration::from_millis(3000))
            {
                log::warn!("UCX startup state dump request failed: {e:?}");
            }
        }
        if state.device.is_ucx_model()
            && state.device.startup_dump_pending()
            && !state.always_init_device
        {
            // Show the saved scene immediately, but do not touch the
            // device until the pre-connect dump has been decoded.  Writing
            // now would interleave scene echoes with the dump stream.
            if let Err(e) = state.device.load_scene_model(&scene) {
                log::warn!("could not load the UCX scene model: {e:?}");
            }
            state.host_scene = Some(state.device.capture_scene());
            state.startup_compare_scene = Some(scene);
            state.startup_compare_pending = true;
            log::info!("deferred UCX startup sync until the state dump completes");
            return;
        }
        // RME's UC behavior is unconditional: on connect TotalMix writes
        // its host state over the unit, with no mismatch dialog (verified
        // on TotalMix FX 2.10 / driver 1.277).  Live front-panel changes
        // are still observed through 0x8b after the connect.
        if !state.device.is_uc_model() && !state.always_init_device {
            if let Some(device_state) = state.device.readable_state() {
                let items = device_state.differences(&scene);
                if !items.is_empty() {
                    let totalcontrol_gain =
                        scene_gain_pairs(&scene, state.device.is_ucx_model());
                    state.startup_mismatch = Some(StartupMismatch {
                        items,
                        device_state,
                        totalcontrol_scene: scene.clone(),
                        totalcontrol_gain,
                        ucx_dump: None,
                        device_scene: None,
                    });
                }
            }
        }
        match state.device.apply_scene(&scene) {
            Ok(()) => {
                // `apply_scene` normalizes old scenes into the live channel
                // topology.  Keep that normalized view as the host scene so
                // the next auto-save cannot write stale names/order back.
                enforce_control_room_stereo(state);
                state.host_scene = Some(state.device.capture_scene());
                if state.always_init_device {
                    // "Always use TotalControl settings" skips the dialog, so the
                    // same readable values it would write are pushed here.
                    let totalcontrol_gain =
                        scene_gain_pairs(&scene, state.device.is_ucx_model());
                    apply_totalcontrol_mismatch(
                        state,
                        &StartupMismatch {
                            items: Vec::new(),
                            device_state: Default::default(),
                            totalcontrol_scene: scene.clone(),
                            totalcontrol_gain,
                            ucx_dump: None,
                            device_scene: None,
                        },
                    );
                }
                if state.device.is_uc_model() {
                    // Reapply this exact host scene after the engine has
                    // started.  Do not capture the model at that point: a
                    // queued front-panel 0x8b can already have replaced the
                    // output fader, and capturing it would persist the
                    // device value instead of the host scene.
                    state.startup_scene = Some(scene.clone());
                    state.startup_scene_due =
                        Some(Instant::now() + Duration::from_secs(1));
                    // The UC can deliver a front-panel move made while the
                    // app was not running after `begin_metering()` starts
                    // the engine.  TotalMix's connect write is
                    // authoritative, so ignore that stale output event
                    // through the reapply.
                    state
                        .device
                        .suppress_front_panel_for(Duration::from_secs(2));
                }
            }
            Err(e) => eprintln!("auto scene load failed: {e:?}"),
        }
        // Only now may the meter tap open the capture PCM and start the
        // engine; the scene above is latched at that point.
        state.device.begin_metering();
        return;
    }
    if state.device.device_state_is_authoritative() {
        state.device.begin_metering();
        return;
    }
    if state.sync_direction != SyncDirection::GuiToDevice {
        state.device.begin_metering();
        return;
    }
    if let Some(scene) = totalcontrol_core::scene::load_auto_scene() {
        state.host_scene = Some(scene.clone());
        if let Err(e) = state.device.apply_scene(&scene) {
            eprintln!("auto scene load failed: {e:?}");
        } else {
            enforce_control_room_stereo(state);
            state.host_scene = Some(state.device.capture_scene());
        }
    }
    state.device.begin_metering();
}

/// Schedule a reapply of the **current model** after the UC has reloaded its
/// stored mixer (playback start or a live sample-rate change).
///
/// The startup path has its own saved scene; if that reapply is still
/// pending, keep it instead of overwriting it with a possibly device-updated
/// model.
/// Finish the parts of startup that must follow the scene write.
fn finish_startup_setup(state: &mut TotalControl) {
    if let Err(e) = apply_control_room_assignments(state) {
        log::debug!("Control Room assignments not restored: {e}");
    }
    sync_surround_bridge(state);
    restore_engine(state);
    state.device.begin_metering();
    maybe_attach_device_service(state);
}

fn apply_startup_scene(state: &mut TotalControl, scene: &Scene) {
    match state.device.apply_scene(scene) {
        Ok(()) => {
            enforce_control_room_stereo(state);
            state.host_scene = Some(state.device.capture_scene());
        }
        Err(e) => log::warn!("startup scene apply failed: {e:?}"),
    }
    finish_startup_setup(state);
}

/// Finish the deferred UCX startup comparison after the asynchronous
/// pre-connect dump has been snapshotted.
fn poll_startup_compare(state: &mut TotalControl) {
    if !state.startup_compare_pending || !state.device.startup_dump_complete() {
        return;
    }
    state.startup_compare_pending = false;
    let Some(scene) = state.startup_compare_scene.take() else {
        finish_startup_setup(state);
        return;
    };
    let dump = UcxDump::new(state.device.pre_connect_words().unwrap_or_default());
    let mut device_scene = scene.clone();
    let items = dump.differences_into(&mut device_scene);
    log::info!(
        "UCX startup comparison: {} difference(s): {:?}",
        items.len(),
        items
    );
    if items.is_empty() {
        apply_startup_scene(state, &scene);
        return;
    }
    let totalcontrol_gain = scene_gain_pairs(&scene, true);
    state.startup_mismatch = Some(StartupMismatch {
        items,
        device_state: Default::default(),
        totalcontrol_scene: scene,
        totalcontrol_gain,
        ucx_dump: Some(dump),
        device_scene: Some(device_scene),
    });
}

fn schedule_mixer_reapply(state: &mut TotalControl, delay: Duration, reason: &str) {
    if state.startup_scene.is_some() {
        log::debug!("{reason}: a startup scene reapply is already pending");
        return;
    }
    state.startup_scene = Some(state.device.capture_scene());
    state.startup_scene_due = Some(Instant::now() + delay);
    log::info!("{reason}; reapplying the Fireface mixer scene");
}

/// A change between two observed card rates.  `None` means "no open stream";
/// keep the last known rate across that gap so a rate switch made while the
/// stream was briefly closed is still detected when it reopens.
fn card_rate_changed(last: Option<u32>, current: Option<u32>) -> bool {
    matches!((last, current), (Some(old), Some(new)) if old != new)
}

/// Whether the first observed card rate differs from the rate the device
/// reported when TotalControl opened it.  This is the common startup case: the
/// scene is applied while the card is at its old rate, then PipeWire opens
/// it at a different rate and the UC reloads its stored mixer.
fn first_rate_needs_reapply(last: Option<u32>, current: Option<u32>, model_rate: u32) -> bool {
    last.is_none() && current.is_some_and(|rate| rate != model_rate)
}

fn rate_needs_mixer_reapply(
    last: Option<u32>,
    current: Option<u32>,
    model_rate: u32,
) -> bool {
    card_rate_changed(last, current) || first_rate_needs_reapply(last, current, model_rate)
}

/// Brings the audio engine in line with the preferences at launch: the
/// rate / buffer forced here last time are forced again (PipeWire forgets
/// them when it restarts), then the clock is read. A choice of "Auto"
/// (`0`) is not re-sent, so launching TotalControl never lifts an override some
/// other tool set. The mock applies them to its simulated clock only.
pub fn restore_engine(state: &mut TotalControl) {
    if state.device.is_mock() {
        // The simulated clock starts at its defaults: the remembered
        // choices are the real PipeWire's.
        return;
    }
    if !state.device.rate_follows_sound_server() {
        return;
    }
    // The UC sample rate follows the host's stream, as it does on
    // Windows: the device keeps its last rate while powered, but the host's
    // remembered rate is what opens the next stream.  Restore the saved
    // choice; "Auto" (0) releases the override so the graph/device rate is
    // used instead.
    if state.device.is_uc_model() {
        state.pending_device_rate = None;
        if state.engine_force_rate > 0 {
            if let Err(e) = pw_clock::force_rate(state.engine_force_rate) {
                log::warn!("Restoring the PipeWire rate failed: {e}");
            }
            state.device.set_meter_rate(state.engine_force_rate);
        } else if let Err(e) = pw_clock::force_rate(0) {
            log::warn!("Releasing the PipeWire rate override failed: {e}");
        }
    } else if state.engine_force_rate > 0 {
        if let Err(e) = pw_clock::force_rate(state.engine_force_rate) {
            log::warn!("Restoring the PipeWire rate failed: {e}");
        }
        // The input capture PCM shares the device clock; release it so
        // the kernel driver can move the card to the requested rate.
        state.device.set_meter_rate(state.engine_force_rate);
    }
    if state.engine_force_quantum > 0 {
        if let Err(e) = pw_clock::force_quantum(state.engine_force_quantum) {
            log::warn!("Restoring the PipeWire buffer size failed: {e}");
        }
    }
    state.engine = pw_clock::read();
    state.card_stream = pw_clock::card_stream(state.device.card_id());
    // The scene was just written at this rate/stream state.  Seed the
    // detectors so the first EnginePoll only reacts to a change made after
    // the write; otherwise a sink that was already running at launch causes
    // a second full scene push.
    state.last_card_rate = state.card_stream.map(|stream| stream.rate);
    state.last_sink_running = !state.device.is_mock() && pw_clock::fireface_sink_running();
    state.driver_floor = pw_clock::driver_period_floor();
    state.driver_queue = pw_clock::driver_urb_queue();
}

/// Writes the preferences if something changed (and a path is set).
fn flush_prefs(state: &mut TotalControl) {
    if !state.prefs_dirty {
        return;
    }
    state.prefs_dirty = false;
    if let Some(path) = state.prefs_path.clone() {
        if let Err(e) = crate::prefs::save(&path, &prefs_of(state)) {
            log::warn!("Failed to save GUI preferences: {e}");
        }
    }
}

/// Whether `message` changes something `prefs_of` persists.
fn changes_prefs(message: &Message) -> bool {
    matches!(
        message,
        Message::SetView(_)
            | Message::TabPressed
            | Message::ToggleSidebar
            | Message::ToggleFxPanel
            | Message::AssignOutput(..)
            | Message::AssignCueTarget(_)
            | Message::SidebarResize(_)
            | Message::ToggleSidebarPanel(_)
            | Message::SetRowMode(_)
            | Message::SetSyncDirection(_)
            | Message::ToggleCollapse(_)
            | Message::ToggleFlyout(..)
            | Message::StripClicked(_)
            | Message::StereoLinkChanged(..)
            | Message::WheelZoom(_)
            | Message::ZoomIn
            | Message::ZoomOut
            | Message::ZoomReset
            | Message::SetKeepInTray(_)
            | Message::MonitorSize(_)
            | Message::RenameCommit
            | Message::SetShowOriginalNames(_)
            | Message::SetSurround51(_)
            | Message::SetSurround71(_)
            | Message::EngineRate(_)
            | Message::EngineQuantum(_)
            | Message::SelectDevice(_)
            | Message::ApplyControlRoomSettings
    )
}

/// The user's name for `cid`'s strip, unless "show original names" is on.
/// Keyed by the strip's own channel (a linked pair's left channel), which
/// is also what the Quick picker and the Matrix row groups look up.
pub(crate) fn user_label(state: &TotalControl, cid: ChannelId) -> Option<&str> {
    if state.show_original_names {
        return None;
    }
    state.labels.get(&cid).map(String::as_str)
}

/// Whether `cid` is rendered by the Control Room. Its name is fixed by the
/// destination; it may not be renamed and never takes a user label.
fn is_control_room_cid(state: &TotalControl, cid: ChannelId) -> bool {
    let ChannelId::Output(i) = cid else {
        return false;
    };
    state.is_output_assigned(i / 2)
}

/// Control Room pairs cannot be unlinked. Every assigned pair is linked
/// before a message is handled and after a scene is applied, so no
/// interaction, recall or external client can leave one split.
fn enforce_control_room_stereo(state: &mut TotalControl) {
    let pairs = state.device.output_pair_count();
    let assigned: Vec<usize> = state.assigned_pairs().collect();
    for pair in assigned {
        if pair < pairs && !state.device.output_linked(pair) {
            let _ = state.device.set_output_linked(pair, true);
        }
    }
}

/// The name a strip shows: the user's label unless "show original names"
/// is on, else the hardware-derived one the caller built. Also hands the
/// strip its rename-field state. Every `strip::strip` call goes through
/// this, after any pair-label joining, so a label replaces the whole
/// strip name ("AN1/2" -> "Vocals"), not one half of it.
fn finish_params<'a>(state: &'a TotalControl, mut p: strip::StripParams<'a>) -> strip::StripParams<'a> {
    if !p.name_locked {
        if let Some(label) = user_label(state, p.cid) {
            p.name = label.to_string();
            p.name_custom = true;
        }
    }
    p.renaming = p.renameable && state.renaming == Some(p.cid);
    p.mute_armed = mute_armed(state, p.cid);
    p.solo_armed = solo_armed(state, p.cid);
    p.rename_buf = &state.rename_buf;
    p
}

/// The window title: which device this window drives, and whether it is
/// there. That is what the taskbar and the window switcher show, where
/// "TotalControl - RME Mixer" said the same thing for every state.
pub fn title(state: &TotalControl, _window: window::Id) -> String {
    let model = state
        .device
        .model_name()
        .strip_suffix(" (mock)")
        .unwrap_or_else(|| state.device.model_name());
    if !state.connected {
        format!("TotalControl \u{2014} {model} (disconnected)")
    } else if state.device.is_mock() {
        format!("TotalControl \u{2014} {model} (simulated)")
    } else {
        format!("TotalControl \u{2014} {model}")
    }
}

/// Floor used when converting silence (linear 0.0) to dB for group-delta
/// math — an actual `f32::NEG_INFINITY` would turn one dragged-to-zero
/// channel into an infinite delta that snaps every other selected channel
/// to 0.0 or 1.0 depending on direction. A large-but-finite floor keeps
/// the swing dramatic (as it should be) without the infinity/NaN edge
/// case.
const GROUP_SILENCE_DB: f32 = -100.0;

fn vol_to_db(v: f32) -> f32 {
    if v <= 0.0 {
        GROUP_SILENCE_DB
    } else {
        (20.0 * v.log10()).max(GROUP_SILENCE_DB)
    }
}

fn db_to_vol(db: f32) -> f32 {
    if db <= GROUP_SILENCE_DB {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

/// Every selectable channel, in the same order they're laid out on
/// screen (Hardware Inputs, then Software Playback, then Hardware
/// Outputs, each in index order) — the linear ordering Shift+click range
/// selection is measured against, so a range visually spans exactly the
/// strips between two clicks the way it would in a file manager.
fn channel_order(state: &TotalControl) -> Vec<ChannelId> {
    let d = &state.device;
    (0..d.inputs().len())
        .map(ChannelId::Input)
        .chain((0..d.playbacks().len()).map(ChannelId::Playback))
        .chain((0..d.outputs().len()).map(ChannelId::Output))
        .collect()
}

/// Sets `cid`'s volume to `v`. If `cid` is part of an active multi-selection,
/// every other selected channel moves by the same *relative* amount instead
/// of jumping to the same absolute level — preserving the balance between
/// them, the way dragging one fader in a DAW's multi-track selection moves
/// the whole group together rather than flattening it to one value.
///
/// The delta is computed in dB, not raw linear amplitude — the fader's own
/// travel is dB-tapered, so an equal *linear* delta applied to channels
/// sitting at different points on that curve produces wildly different dB
/// swings (a channel near the bottom barely moves while one near unity
/// swings hard). dB delta is what actually reads as "moving together."
/// Routes a volume write through the output-pair link logic
/// (`RmeDevice::set_output_volume`) when `cid` is an Output channel —
/// so a linked pair always moves both channels together regardless of
/// which path triggered the write (fader drag, reset, typed dB, OSC,
/// grouped multi-select) — otherwise a plain `set_volume`. The single
/// choke point every output-channel volume write should go through.
/// Gain factor of a trim value: 1.0 at 0 dB, smaller when attenuated.
fn trim_factor(db: f32) -> f32 {
    10f32.powf(db.clamp(-65.0, 0.0) / 20.0)
}

/// Apply each channel's pre-fader trim to its meter reading. The meters
/// show the signal after trim, so a constant tone moves with the trim.
fn trim_meter_levels(levels: &[f32], trims: &[f32]) -> Vec<f32> {
    levels
        .iter()
        .enumerate()
        .map(|(i, level)| level * trim_factor(trims.get(i).copied().unwrap_or(0.0)))
        .collect()
}

/// The trim value of a channel in dB.
fn channel_trim(state: &TotalControl, cid: ChannelId) -> f32 {
    match cid {
        ChannelId::Input(i) => state.device.inputs().get(i).map(|c| c.trim).unwrap_or(0.0),
        ChannelId::Playback(i) => state
            .device
            .playbacks()
            .get(i)
            .map(|c| c.trim)
            .unwrap_or(0.0),
        ChannelId::Output(_) => 0.0,
    }
}

/// Hover text for the T button: 0 dB invites opening the panel, an
/// applied trim states its value.
fn trim_tip(db: f32) -> String {
    if db < 0.0 {
        format!("Trim {db:.1} dB")
    } else {
        "Show trim".to_string()
    }
}

/// Gain factor of a channel's trim, for views that show the level after
/// the trim (the Matrix).
pub(crate) fn channel_trim_factor(state: &TotalControl, cid: ChannelId) -> f32 {
    trim_factor(channel_trim(state, cid))
}

/// Turn a level the user sees (after trim) into the stored send gain.
fn effective_to_send(state: &TotalControl, cid: ChannelId, level: f32) -> f32 {
    (level / trim_factor(channel_trim(state, cid))).clamp(0.0, 2.0)
}

/// Apply a trim value to a channel.  A linked stereo pair shares it.
fn apply_trim(state: &mut TotalControl, cid: ChannelId, db: f32) {
    let db = db.clamp(-65.0, 0.0);
    let targets = match cid {
        ChannelId::Input(i) if state.device.input_pair_linked(i / 2) => {
            vec![ChannelId::Input(i & !1), ChannelId::Input(i | 1)]
        }
        ChannelId::Playback(i) if state.device.playback_linked(i / 2) => {
            vec![ChannelId::Playback(i & !1), ChannelId::Playback(i | 1)]
        }
        ChannelId::Input(_) | ChannelId::Playback(_) => vec![cid],
        ChannelId::Output(_) => return,
    };
    for target in targets {
        if let Err(e) = state.device.set_trim(target, db) {
            fail(state, "set_trim", &e);
        }
    }
}

fn set_channel_volume(state: &mut TotalControl, cid: ChannelId, out: usize, v: f32) {
    // The caller passes the level shown next to the fader; the model
    // stores the send before trim.
    let send = effective_to_send(state, cid, v);
    let _ = match cid {
        ChannelId::Input(ch) => state.device.set_input_volume(ch / 2, ch % 2, out, send),
        ChannelId::Playback(ch) => state.device.set_playback_volume(ch / 2, ch % 2, out, send),
        ChannelId::Output(ch) => state.device.set_output_volume(ch / 2, ch % 2, send),
    };
}

/// Same idea as `set_channel_volume`, for mute.
fn set_channel_mute(state: &mut TotalControl, cid: ChannelId, m: bool) {
    let _ = match cid {
        ChannelId::Input(ch) => state.device.set_input_mute(ch / 2, ch % 2, m),
        ChannelId::Playback(ch) => state.device.set_playback_mute(ch / 2, ch % 2, m),
        ChannelId::Output(ch) => state.device.set_output_mute(ch / 2, ch % 2, m),
    };
}

/// The balance shown for a linked pair, derived from its two channel
/// volumes. Split channels use their own pan.
fn channel_pan(state: &TotalControl, cid: ChannelId, out: usize) -> i8 {
    match cid {
        ChannelId::Input(i) if state.device.input_pair_linked(i / 2) => {
            let left = state
                .device
                .inputs()
                .get(i & !1)
                .and_then(|c| c.volumes.get(out))
                .copied()
                .unwrap_or(0.0);
            let right = state
                .device
                .inputs()
                .get(i | 1)
                .and_then(|c| c.volumes.get(out))
                .copied()
                .unwrap_or(0.0);
            output_balance_from_volumes(left, right)
        }
        ChannelId::Playback(i) if state.device.playback_linked(i / 2) => {
            let left = state
                .device
                .playbacks()
                .get(i & !1)
                .and_then(|c| c.volumes.get(out))
                .copied()
                .unwrap_or(0.0);
            let right = state
                .device
                .playbacks()
                .get(i | 1)
                .and_then(|c| c.volumes.get(out))
                .copied()
                .unwrap_or(0.0);
            output_balance_from_volumes(left, right)
        }
        _ => state.device.pan(cid, out).unwrap_or(0),
    }
}

/// Apply a pan/balance move. A linked pair has no separate balance
/// parameter: its balance is the difference between the two channel
/// volumes, so the knob sets the quieter side's level.
fn set_channel_pan(state: &mut TotalControl, cid: ChannelId, out: usize, pan: i8) {
    let result = match cid {
        ChannelId::Input(i) if state.device.input_pair_linked(i / 2) => {
            state.device.set_input_balance(i / 2, out, pan)
        }
        ChannelId::Playback(i) if state.device.playback_linked(i / 2) => {
            state.device.set_playback_balance(i / 2, out, pan)
        }
        _ => state.device.set_pan(cid, out, pan),
    };
    if let Err(e) = result {
        fail(state, "set_pan", &e);
    }
}

/// Same idea as `set_channel_volume`, for solo.
fn set_channel_solo(state: &mut TotalControl, cid: ChannelId, s: bool) {
    if state.control_room.pfl_replaces_solo {
        if let ChannelId::Input(ch) = cid {
            let pair = ch / 2;
            let (left, right) = (pair * 2, pair * 2 + 1);
            let _ = if state.device.input_pair_linked(pair) {
                state
                    .device
                    .set_pfl(ChannelId::Input(left), s)
                    .and_then(|_| state.device.set_pfl(ChannelId::Input(right), s))
            } else {
                state.device.set_pfl(ChannelId::Input(ch), s)
            };
            return;
        }
    }
    let _ = match cid {
        ChannelId::Input(ch) => state.device.set_input_solo(ch / 2, ch % 2, s),
        ChannelId::Playback(ch) => state.device.set_playback_solo(ch / 2, ch % 2, s),
        ChannelId::Output(ch) => state.device.set_output_solo(ch / 2, ch % 2, s),
    };
}

fn channel_is_muted(state: &TotalControl, cid: ChannelId) -> bool {
    match cid {
        ChannelId::Input(ch) => state.device.inputs()[ch].mute,
        ChannelId::Playback(ch) => state.device.playbacks()[ch].mute,
        ChannelId::Output(ch) => state.device.outputs()[ch].mute,
    }
}

fn channel_is_soloed(state: &TotalControl, cid: ChannelId) -> bool {
    match cid {
        ChannelId::Input(ch) => state.device.inputs()[ch].solo,
        ChannelId::Playback(ch) => state.device.playbacks()[ch].solo,
        ChannelId::Output(ch) => state.device.outputs()[ch].solo,
    }
}

/// Every channel the device currently exposes, across all three types
/// — what the sidebar's global "M"/"S" buttons
/// (`Message::GlobalMuteToggle`/`GlobalSoloToggle`) act on.
fn all_channel_ids(state: &TotalControl) -> Vec<ChannelId> {
    (0..state.device.inputs().len())
        .map(ChannelId::Input)
        .chain((0..state.device.playbacks().len()).map(ChannelId::Playback))
        .chain((0..state.device.outputs().len()).map(ChannelId::Output))
        .collect()
}

/// Whether at least one channel is currently muted — drives the global
/// "M" button's toggle direction and its lit state (`sidebar::msf_row`
/// reads this too, hence `pub(crate)`).
pub(crate) fn any_channel_muted(state: &TotalControl) -> bool {
    all_channel_ids(state).into_iter().any(|cid| channel_is_muted(state, cid))
}

/// Whether `cid`'s mute is held off by the Mute master: the master's last
/// press cleared it, and nothing has been muted since (a new mute makes the
/// master's next press clear that one instead, so the old set is no longer
/// what it would bring back).
pub(crate) fn mute_armed(state: &TotalControl, cid: ChannelId) -> bool {
    state.last_cleared_mutes.contains(&cid)
        && !channel_is_muted(state, cid)
        && !any_channel_muted(state)
}

/// The same for the Solo master and `cid`'s solo.
pub(crate) fn solo_armed(state: &TotalControl, cid: ChannelId) -> bool {
    state.last_cleared_solos.contains(&cid)
        && !channel_is_soloed(state, cid)
        && !any_channel_soloed(state)
}

/// Whether at least one channel is currently soloed — drives the
/// global "S" button's lit state (there's something for it to clear).
pub(crate) fn any_channel_soloed(state: &TotalControl) -> bool {
    all_channel_ids(state).into_iter().any(|cid| channel_is_soloed(state, cid))
}

/// Every *other* channel that should move/mute/solo alongside `cid`
/// right now: the active multi-selection (if `cid` is part of one, same
/// as before the sidebar's Groups panel existed) unioned with the
/// membership of any of `state.groups` whose `link` is engaged and that
/// `cid` belongs to. `cid` itself is never included, so callers can
/// `is_empty()`-check this directly instead of re-deriving "is anything
/// else supposed to move." A `HashSet` (not `Vec`) since a channel can
/// be both multi-selected *and* in a linked group — the union must
/// de-duplicate, not propagate to the same channel twice.
fn propagation_set(
    state: &TotalControl,
    cid: ChannelId,
    link: sidebar::GroupLink,
) -> HashSet<ChannelId> {
    let mut set = HashSet::new();
    if state.selected.len() > 1 && state.selected.contains(&cid) {
        set.extend(state.selected.iter().copied());
    }
    for g in &state.groups {
        let linked = match link {
            sidebar::GroupLink::Mute => g.mute_linked,
            sidebar::GroupLink::Solo => g.solo_linked,
            sidebar::GroupLink::Fader => g.fader_linked,
        };
        if linked && g.members.contains(&cid) {
            set.extend(g.members.iter().copied());
        }
    }
    set.remove(&cid);
    set
}

fn apply_grouped_volume(state: &mut TotalControl, cid: ChannelId, out: usize, v: f32) {
    let others = propagation_set(state, cid, sidebar::GroupLink::Fader);
    if others.is_empty() {
        set_channel_volume(state, cid, out, v);
        notify_osc(state, OscOutbound::Volume(cid, out, v));
        return;
    }
    let old = state.device.volume(cid, out).unwrap_or(v)
        * trim_factor(channel_trim(state, cid));
    let delta_db = vol_to_db(v) - vol_to_db(old);
    set_channel_volume(state, cid, out, v);
    notify_osc(state, OscOutbound::Volume(cid, out, v));
    for sel in others {
        let cur = state.device.volume(sel, out).unwrap_or(0.0)
            * trim_factor(channel_trim(state, sel));
        let new_vol = db_to_vol(vol_to_db(cur) + delta_db).clamp(0.0, 2.0);
        set_channel_volume(state, sel, out, new_vol);
        notify_osc(state, OscOutbound::Volume(sel, out, new_vol));
    }
}

/// Same relative-delta grouping as `apply_grouped_volume`, for pan.
fn apply_grouped_pan(state: &mut TotalControl, cid: ChannelId, out: usize, pan: i8) {
    let others = propagation_set(state, cid, sidebar::GroupLink::Fader);
    if others.is_empty() {
        set_channel_pan(state, cid, out, pan);
        notify_osc(state, OscOutbound::Pan(cid, out, pan));
        return;
    }
    let old = i16::from(channel_pan(state, cid, out));
    let delta = i16::from(pan) - old;
    set_channel_pan(state, cid, out, pan);
    notify_osc(state, OscOutbound::Pan(cid, out, pan));
    for sel in others {
        let cur = i16::from(channel_pan(state, sel, out));
        let new = (cur + delta).clamp(-100, 100) as i8;
        set_channel_pan(state, sel, out, new);
        notify_osc(state, OscOutbound::Pan(sel, out, new));
    }
}

/// Apply one field of an output EQ by reading the live output model,
/// mutating it, and writing the whole DSP block in one UCX transaction.
fn update_output_eq(
    state: &mut TotalControl,
    idx: usize,
    change: impl FnOnce(&mut totalcontrol_core::channel::InputEq),
) -> Result<(), totalcontrol_core::Error> {
    let mut eq = state
        .device
        .outputs()
        .get(idx)
        .and_then(|o| o.eq)
        .unwrap_or_default();
    change(&mut eq);
    state.device.set_output_eq(idx, &eq)
}

/// Shared EQ action dispatch for input and output channels.
fn set_eq_enabled_for(
    state: &mut TotalControl,
    cid: ChannelId,
    on: bool,
) -> Result<(), totalcontrol_core::Error> {
    match cid {
        ChannelId::Input(idx) => state.device.set_eq_enabled(idx, on),
        ChannelId::Output(idx) => update_output_eq(state, idx, |e| e.enabled = on),
        ChannelId::Playback(_) => Ok(()),
    }
}

fn set_eq_band_type_for(
    state: &mut TotalControl,
    cid: ChannelId,
    band: usize,
    t: EqBandType,
) -> Result<(), totalcontrol_core::Error> {
    match cid {
        ChannelId::Input(idx) => state.device.set_eq_band_type(idx, band, t),
        ChannelId::Output(idx) => update_output_eq(state, idx, |e| e.bands[band].band_type = t),
        ChannelId::Playback(_) => Ok(()),
    }
}

fn set_eq_band_freq_for(
    state: &mut TotalControl,
    cid: ChannelId,
    band: usize,
    freq: u16,
) -> Result<(), totalcontrol_core::Error> {
    match cid {
        ChannelId::Input(idx) => state.device.set_eq_band_freq(idx, band, freq),
        ChannelId::Output(idx) => {
            update_output_eq(state, idx, |e| e.bands[band].freq_hz = freq)
        }
        ChannelId::Playback(_) => Ok(()),
    }
}

fn set_eq_band_q_for(
    state: &mut TotalControl,
    cid: ChannelId,
    band: usize,
    q: f32,
) -> Result<(), totalcontrol_core::Error> {
    match cid {
        ChannelId::Input(idx) => state.device.set_eq_band_q(idx, band, q),
        ChannelId::Output(idx) => update_output_eq(state, idx, |e| e.bands[band].q = q),
        ChannelId::Playback(_) => Ok(()),
    }
}

fn set_eq_band_gain_for(
    state: &mut TotalControl,
    cid: ChannelId,
    band: usize,
    gain: f32,
) -> Result<(), totalcontrol_core::Error> {
    match cid {
        ChannelId::Input(idx) => state.device.set_eq_band_gain(idx, band, gain),
        ChannelId::Output(idx) => update_output_eq(state, idx, |e| e.bands[band].gain_db = gain),
        ChannelId::Playback(_) => Ok(()),
    }
}

fn set_low_cut_enabled_for(
    state: &mut TotalControl,
    cid: ChannelId,
    on: bool,
) -> Result<(), totalcontrol_core::Error> {
    match cid {
        ChannelId::Input(idx) => state.device.set_eq_low_cut_enabled(idx, on),
        ChannelId::Output(idx) => update_output_eq(state, idx, |e| e.low_cut_enabled = on),
        ChannelId::Playback(_) => Ok(()),
    }
}

fn set_low_cut_freq_for(
    state: &mut TotalControl,
    cid: ChannelId,
    freq: u16,
) -> Result<(), totalcontrol_core::Error> {
    match cid {
        ChannelId::Input(idx) => state.device.set_eq_low_cut_freq(idx, freq),
        ChannelId::Output(idx) => update_output_eq(state, idx, |e| e.low_cut_freq_hz = freq),
        ChannelId::Playback(_) => Ok(()),
    }
}

fn set_low_cut_slope_for(
    state: &mut TotalControl,
    cid: ChannelId,
    slope: u8,
) -> Result<(), totalcontrol_core::Error> {
    match cid {
        ChannelId::Input(idx) => state.device.set_eq_low_cut_slope(idx, slope),
        ChannelId::Output(idx) => {
            update_output_eq(state, idx, |e| e.low_cut_slope_db_oct = slope)
        }
        ChannelId::Playback(_) => Ok(()),
    }
}

/// Current dynamics of one hardware input/output, or the defaults if the
/// backend has no state yet.
fn dynamics_for(state: &TotalControl, cid: ChannelId) -> Dynamics {
    match cid {
        ChannelId::Input(idx) => state
            .device
            .inputs()
            .get(idx)
            .and_then(|c| c.dynamics)
            .unwrap_or_default(),
        ChannelId::Output(idx) => state
            .device
            .outputs()
            .get(idx)
            .and_then(|c| c.dynamics)
            .unwrap_or_default(),
        ChannelId::Playback(_) => Dynamics::default(),
    }
}
fn pair_linked(state: &TotalControl, cid: ChannelId) -> bool {
    match cid {
        ChannelId::Input(i) => state.device.input_pair_linked(i / 2),
        ChannelId::Playback(i) => state.device.playback_linked(i / 2),
        ChannelId::Output(i) => state.device.output_linked(i / 2),
    }
}

fn channel_eq_on(state: &TotalControl, cid: ChannelId) -> bool {
    match cid {
        ChannelId::Input(i) => state
            .device
            .inputs()
            .get(i)
            .and_then(|c| c.eq)
            .is_some_and(|eq| eq.enabled),
        ChannelId::Output(i) => state
            .device
            .outputs()
            .get(i)
            .and_then(|c| c.eq)
            .is_some_and(|eq| eq.enabled),
        ChannelId::Playback(_) => false,
    }
}

fn channel_phase(state: &TotalControl, cid: ChannelId, right: bool) -> bool {
    match cid {
        ChannelId::Input(i) => state
            .device
            .inputs()
            .get(i)
            .is_some_and(|c| if right { c.phase_r } else { c.phase }),
        ChannelId::Output(i) => state
            .device
            .outputs()
            .get(i)
            .is_some_and(|c| if right { c.phase_r } else { c.phase_l }),
        ChannelId::Playback(_) => false,
    }
}

fn channel_low_cut_on(state: &TotalControl, cid: ChannelId) -> bool {
    match cid {
        ChannelId::Input(i) => state
            .device
            .inputs()
            .get(i)
            .and_then(|c| c.eq)
            .is_some_and(|eq| eq.low_cut_enabled),
        ChannelId::Output(i) => state
            .device
            .outputs()
            .get(i)
            .and_then(|c| c.eq)
            .is_some_and(|eq| eq.low_cut_enabled),
        ChannelId::Playback(_) => false,
    }
}

/// Resolve one edge-triggered RME `Toggle` against the confirmed model and
/// return the ordinary GUI message that performs the opposite state.
fn rme_toggle_message(
    state: &TotalControl,
    target: crate::osc_rme::RmeToggle,
) -> Option<Message> {
    use crate::osc_rme::RmeToggle as T;
    Some(match target {
        T::Mute(cid) => Message::Mute(cid, !channel_is_muted(state, cid)),
        T::Solo(cid) => Message::Solo(cid, !channel_is_soloed(state, cid)),
        T::Phase(cid) => match cid {
            ChannelId::Input(_) => Message::PhaseChanged(cid, !channel_phase(state, cid, false)),
            ChannelId::Output(i) => Message::OutputPhaseChanged(i, !channel_phase(state, cid, false)),
            ChannelId::Playback(_) => return None,
        },
        T::PhaseRight(cid) => match cid {
            ChannelId::Input(i) => Message::InputPhaseRight(i, !channel_phase(state, cid, true)),
            ChannelId::Output(i) => Message::OutputPhaseChanged(i, !channel_phase(state, cid, true)),
            ChannelId::Playback(_) => return None,
        },
        T::Phantom(i) => Message::Phantom(i, !state.device.inputs().get(i)?.phantom),
        T::Instrument(i) => Message::Instrument(i, !state.device.inputs().get(i)?.instrument),
        T::Pad(i) => Message::Pad(i, !state.device.inputs().get(i)?.pad),
        T::Autoset(i) => Message::Autoset(i, !state.device.inputs().get(i)?.autoset),
        T::MsProc(cid) => match cid {
            ChannelId::Input(i) => Message::InputMsProcChanged(i, !state.device.inputs().get(i)?.ms_proc),
            ChannelId::Playback(i) => {
                Message::PlaybackMsProcChanged(i, !state.device.playbacks().get(i)?.ms_proc)
            }
            ChannelId::Output(_) => return None,
        },
        T::Stereo(cid) => Message::StereoLinkChanged(cid, !pair_linked(state, cid)),
        T::Cue(i) => Message::CueChanged(
            ChannelId::Output(i),
            !state.device.outputs().get(i)?.cue,
        ),
        T::TalkbackSel(i) => Message::TalkbackReceiveChanged(
            i,
            !state.device.outputs().get(i)?.talkback,
        ),
        T::NoTrim(i) => Message::NoTrimChanged(i, !state.device.outputs().get(i)?.no_trim),
        T::Loopback(i) => Message::LoopbackChanged(i, !state.device.outputs().get(i)?.loopback),
        T::LowCutEnable(cid) => Message::EqLowCutEnabled(cid, !channel_low_cut_on(state, cid)),
        T::EqEnable(cid) => Message::EqEnabled(cid, !channel_eq_on(state, cid)),
        T::CompexpEnable(cid) => Message::DynamicsEnabled(cid, !dynamics_for(state, cid).on),
        T::AlevEnable(cid) => {
            let mut d = dynamics_for(state, cid);
            d.auto_level = !d.auto_level;
            Message::DynamicsChanged(cid, d)
        }
        T::GlobalMute => Message::GlobalMuteToggle,
        T::GlobalSolo => Message::GlobalSoloToggle,
        T::MainDim => Message::Dim(!state.device.settings().dim),
        T::MainSpeakerB => Message::SpeakerB(!state.device.settings().speaker_b),
        T::MainMuteFx => Message::ControlRoomMuteFx(!state.device.settings().mute_fx),
        T::MainMono => Message::ControlRoomMono(!state.device.settings().mono),
        T::MainTalkback => Message::TalkbackChanged(!state.talkback_on),
        T::SpeakerBLinked => Message::SpeakerBLinked(!state.control_room.speaker_b_linked),
        T::ReverbEnable => Message::OscRmeFx {
            echo: false,
            field: "on",
            value: if state.device.settings().reverb.unwrap_or_default().on {
                0.0
            } else {
                1.0
            },
        },
        T::EchoEnable => Message::OscRmeFx {
            echo: true,
            field: "on",
            value: if state.device.settings().echo.unwrap_or_default().on {
                0.0
            } else {
                1.0
            },
        },
    })
}

fn global_channel(
    state: &TotalControl,
    bus: crate::osc_global::Bus,
    index: usize,
) -> Option<ChannelId> {
    let count = match bus {
        crate::osc_global::Bus::Input => state.device.inputs().len(),
        crate::osc_global::Bus::Playback => state.device.playbacks().len(),
        crate::osc_global::Bus::Output => state.device.outputs().len(),
    };
    (index < count).then_some(match bus {
        crate::osc_global::Bus::Input => ChannelId::Input(index),
        crate::osc_global::Bus::Playback => ChannelId::Playback(index),
        crate::osc_global::Bus::Output => ChannelId::Output(index),
    })
}

fn global_gain(state: &TotalControl, index: usize, db: f32) -> Option<u32> {
    let input = state.device.inputs().get(index)?;
    let max = input
        .gain_max
        .unwrap_or(if index < 2 { 65 } else { state.rme.gain_max_34 });
    // Global OSC sends the preamp value in dB.  Inputs 1/2 use whole dB,
    // inputs 3/4 use half-dB steps in the device model.
    let raw = if index < 2 {
        db.round()
    } else {
        (db * 2.0).round()
    };
    Some(raw.clamp(0.0, max as f32) as u32)
}

fn global_input_level(value: usize) -> Option<InputLevel> {
    InputLevel::from_code(value as u16)
}

fn global_pan(value: f32) -> i8 {
    (value * 100.0).round().clamp(-100.0, 100.0) as i8
}

fn global_eq_type(band: usize, kind: usize) -> Option<EqBandType> {
    match band {
        0 => match kind {
            0 => Some(EqBandType::Bell),
            1 => Some(EqBandType::LowShelf),
            _ => None,
        },
        1 => (kind == 0).then_some(EqBandType::Bell),
        2 => match kind {
            0 => Some(EqBandType::Bell),
            1 => Some(EqBandType::HighShelf),
            2 => Some(EqBandType::LowPass),
            _ => None,
        },
        _ => None,
    }
}

fn global_lowcut_messages(
    cid: ChannelId,
    control: &crate::osc_global::LowCutControl,
) -> Vec<Message> {
    match control {
        crate::osc_global::LowCutControl::Enable(on) => {
            vec![Message::EqLowCutEnabled(cid, *on)]
        }
        crate::osc_global::LowCutControl::Freq(hz) => {
            vec![Message::EqLowCutFreq(cid, hz.round().clamp(0.0, 20_000.0) as u16)]
        }
        crate::osc_global::LowCutControl::Slope(slope) => {
            vec![Message::EqLowCutSlope(cid, slope.round().clamp(6.0, 24.0) as u8)]
        }
        crate::osc_global::LowCutControl::LoadPreset => Vec::new(),
    }
}

fn global_eq_messages(cid: ChannelId, control: &crate::osc_global::EqControl) -> Vec<Message> {
    match control {
        crate::osc_global::EqControl::Enable(on) => vec![Message::EqEnabled(cid, *on)],
        crate::osc_global::EqControl::BandGain { band, db } if (1..=3).contains(band) => {
            vec![Message::EqBandGain(cid, band - 1, db.clamp(-24.0, 24.0))]
        }
        crate::osc_global::EqControl::BandFreq { band, hz } if (1..=3).contains(band) => {
            vec![Message::EqBandFreq(
                cid,
                band - 1,
                hz.round().clamp(20.0, 20_000.0) as u16,
            )]
        }
        crate::osc_global::EqControl::BandQ { band, q } if (1..=3).contains(band) => {
            vec![Message::EqBandQ(cid, band - 1, q.clamp(0.05, 10.0))]
        }
        crate::osc_global::EqControl::BandType { band, kind } if (1..=3).contains(band) => {
            global_eq_type(band - 1, *kind)
                .map(|ty| vec![Message::EqBandType(cid, band - 1, ty)])
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

fn global_dynamics_messages(
    state: &TotalControl,
    cid: ChannelId,
    control: &crate::osc_global::DynamicsControl,
) -> Vec<Message> {
    if matches!(cid, ChannelId::Playback(_)) {
        return Vec::new();
    }
    if let crate::osc_global::DynamicsControl::Enable(on) = control {
        return vec![Message::DynamicsEnabled(cid, *on)];
    }
    let mut d = dynamics_for(state, cid);
    match control {
        crate::osc_global::DynamicsControl::Gain(v) => d.gain_db = *v,
        crate::osc_global::DynamicsControl::Attack(v) => {
            d.attack_ms = v.round().clamp(0.0, 200.0) as u16
        }
        crate::osc_global::DynamicsControl::Release(v) => {
            d.release_ms = v.round().clamp(100.0, 999.0) as u16
        }
        crate::osc_global::DynamicsControl::CompThreshold(v) => d.comp_threshold_db = *v,
        crate::osc_global::DynamicsControl::CompRatio(v) => d.comp_ratio = *v,
        crate::osc_global::DynamicsControl::ExpThreshold(v) => d.exp_threshold_db = *v,
        crate::osc_global::DynamicsControl::ExpRatio(v) => d.exp_ratio = *v,
        crate::osc_global::DynamicsControl::LoadPreset => return Vec::new(),
        crate::osc_global::DynamicsControl::Enable(_) => unreachable!(),
    }
    vec![Message::DynamicsChanged(cid, d.clamped())]
}

fn global_autolevel_messages(
    state: &TotalControl,
    cid: ChannelId,
    control: &crate::osc_global::AutoLevelControl,
) -> Vec<Message> {
    if matches!(cid, ChannelId::Playback(_)) {
        return Vec::new();
    }
    let mut d = dynamics_for(state, cid);
    match control {
        crate::osc_global::AutoLevelControl::Enable(on) => d.auto_level = *on,
        crate::osc_global::AutoLevelControl::MaxGain(v) => d.auto_max_gain_db = *v,
        crate::osc_global::AutoLevelControl::Headroom(v) => d.auto_headroom_db = *v,
        crate::osc_global::AutoLevelControl::RiseTime(v) => d.auto_rise_s = *v,
        crate::osc_global::AutoLevelControl::LoadPreset => return Vec::new(),
    }
    vec![Message::DynamicsChanged(cid, d.clamped())]
}

fn global_channel_messages(
    state: &TotalControl,
    bus: crate::osc_global::Bus,
    channel: usize,
    cid: ChannelId,
    control: &crate::osc_global::ChannelControl,
) -> Vec<Message> {
    use crate::osc_global::ChannelControl;
    match control {
        ChannelControl::Mute(on) => vec![Message::Mute(cid, *on)],
        ChannelControl::Stereo(on) => vec![Message::StereoLinkChanged(cid, *on)],
        ChannelControl::Name(name) => vec![Message::RmeRename(cid, name.clone())],
        ChannelControl::MsProc(on) => match bus {
            crate::osc_global::Bus::Input => vec![Message::InputMsProcChanged(channel, *on)],
            crate::osc_global::Bus::Playback => {
                vec![Message::PlaybackMsProcChanged(channel, *on)]
            }
            crate::osc_global::Bus::Output => Vec::new(),
        },
        ChannelControl::Phase(on) => match bus {
            crate::osc_global::Bus::Input => vec![Message::PhaseChanged(cid, *on)],
            crate::osc_global::Bus::Output => {
                vec![Message::OutputPhaseChanged(channel, *on)]
            }
            crate::osc_global::Bus::Playback => Vec::new(),
        },
        ChannelControl::Width(width) => match bus {
            crate::osc_global::Bus::Input | crate::osc_global::Bus::Playback => {
                vec![Message::ChannelWidthChanged(cid, width.clamp(0.0, 1.0))]
            }
            crate::osc_global::Bus::Output => Vec::new(),
        },
        ChannelControl::Gain(db) if bus == crate::osc_global::Bus::Input => {
            global_gain(state, channel, *db)
                .map(|raw| vec![Message::Gain(channel, raw)])
                .unwrap_or_default()
        }
        ChannelControl::RefLevel(code) if bus == crate::osc_global::Bus::Input => {
            global_input_level(*code)
                .map(|level| vec![Message::LevelChanged(channel, level)])
                .unwrap_or_default()
        }
        ChannelControl::Phantom(on) if bus == crate::osc_global::Bus::Input => {
            vec![Message::Phantom(channel, *on)]
        }
        ChannelControl::Instrument(on) if bus == crate::osc_global::Bus::Input => {
            vec![Message::Instrument(channel, *on)]
        }
        ChannelControl::Pad(on) if bus == crate::osc_global::Bus::Input => {
            vec![Message::Pad(channel, *on)]
        }
        ChannelControl::Autoset(on) if bus == crate::osc_global::Bus::Input => {
            vec![Message::Autoset(channel, *on)]
        }
        ChannelControl::Volume(db) if bus == crate::osc_global::Bus::Output => {
            vec![Message::VolumeChanged(
                cid,
                channel,
                db_to_vol(*db).clamp(0.0, 2.0),
            )]
        }
        ChannelControl::VolumeLin(norm) if bus == crate::osc_global::Bus::Output => {
            let gain = crate::osc_rme::fader_gain(&OscType::Float(*norm)).unwrap_or(0.0);
            vec![Message::VolumeChanged(cid, channel, gain)]
        }
        ChannelControl::Balance(balance) if bus == crate::osc_global::Bus::Output => {
            vec![Message::PanChanged(cid, channel, global_pan(*balance))]
        }
        ChannelControl::FxSend(db) if bus != crate::osc_global::Bus::Output => {
            vec![Message::FxSendChanged(cid, *db)]
        }
        ChannelControl::FxReturn(db) if bus == crate::osc_global::Bus::Output => {
            vec![Message::FxReturnChanged(channel, *db)]
        }
        ChannelControl::Loopback(on) if bus == crate::osc_global::Bus::Output => {
            vec![Message::LoopbackChanged(channel, *on)]
        }
        ChannelControl::TalkbackSel(on) if bus == crate::osc_global::Bus::Output => {
            vec![Message::TalkbackReceiveChanged(channel, *on)]
        }
        ChannelControl::Pfl(on)
            if bus == crate::osc_global::Bus::Input
                && state.control_room.pfl_replaces_solo =>
        {
            vec![Message::Solo(cid, *on)]
        }
        ChannelControl::LowCut(control) if bus != crate::osc_global::Bus::Playback => {
            global_lowcut_messages(cid, control)
        }
        ChannelControl::Eq(control) if bus != crate::osc_global::Bus::Playback => {
            global_eq_messages(cid, control)
        }
        ChannelControl::Dynamics(control) => global_dynamics_messages(state, cid, control),
        ChannelControl::AutoLevel(control) => global_autolevel_messages(state, cid, control),
        _ => Vec::new(),
    }
}

fn global_mix_messages(
    state: &TotalControl,
    source: crate::osc_global::Bus,
    channel: usize,
    submix: usize,
    control: &crate::osc_global::MixControl,
) -> Vec<Message> {
    let Some(cid) = global_channel(state, source, channel) else {
        return Vec::new();
    };
    match control {
        crate::osc_global::MixControl::Fader(db) => {
            vec![Message::MatrixCellChanged(cid, submix, db_to_vol(*db).clamp(0.0, 2.0))]
        }
        crate::osc_global::MixControl::FaderLin(norm) => {
            let gain = crate::osc_rme::fader_gain(&OscType::Float(*norm)).unwrap_or(0.0);
            vec![Message::MatrixCellChanged(cid, submix, gain)]
        }
        crate::osc_global::MixControl::Balance(balance) => {
            vec![Message::PanChanged(cid, submix, global_pan(*balance))]
        }
        _ => Vec::new(),
    }
}

fn global_control_room_messages(
    state: &TotalControl,
    control: &crate::osc_global::ControlRoomControl,
) -> Vec<Message> {
    use crate::osc_global::ControlRoomControl;
    match control {
        ControlRoomControl::MainOut(channel) => {
            vec![Message::AssignOutput(0, Some(channel / 2))]
        }
        ControlRoomControl::MainOutB(channel) => {
            vec![Message::AssignOutput(1, Some(channel / 2))]
        }
        ControlRoomControl::Phones { slot, channel } => {
            vec![Message::AssignOutput(1 + slot, Some(channel / 2))]
        }
        ControlRoomControl::MainMono(on) => vec![Message::ControlRoomMono(*on)],
        ControlRoomControl::Dim(on) => vec![Message::Dim(*on)],
        ControlRoomControl::SpeakerB(on) => vec![Message::SpeakerB(*on)],
        ControlRoomControl::MuteFx(on) => vec![Message::ControlRoomMuteFx(*on)],
        ControlRoomControl::ExternalIn(on) => vec![Message::ExternalInput(*on)],
        ControlRoomControl::ExtInChannel(channel) => {
            vec![Message::ControlRoomExternalInput(Some(channel / 2))]
        }
        ControlRoomControl::ExtInGain(db) => vec![Message::ControlRoomExternalGain(*db)],
        ControlRoomControl::Talkback(on) => vec![Message::TalkbackChanged(*on)],
        ControlRoomControl::LinkAB(on) => vec![Message::SpeakerBLinked(*on)],
        ControlRoomControl::Recall => vec![Message::ControlRoomRecall],
        ControlRoomControl::CueChannel(channel) => {
            let pair = channel / 2;
            state
                .assign
                .iter()
                .take(OUTPUT_DESTS)
                .position(|slot| *slot == Some(pair))
                .filter(|dest| CUE_TARGETS.contains(dest))
                .map(|dest| vec![Message::AssignCueTarget(dest)])
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

fn global_feedback_is_typed(command: &crate::osc_global::GlobalCommand) -> bool {
    use crate::osc_global::{Bus, ChannelControl, GlobalCommand, MixControl};
    match command {
        GlobalCommand::Channel { bus, control, .. } => match control {
            ChannelControl::Mute(_) => true,
            ChannelControl::Volume(_) | ChannelControl::VolumeLin(_) | ChannelControl::Balance(_) => {
                *bus == Bus::Output
            }
            _ => false,
        },
        GlobalCommand::Mix { control, .. } => matches!(
            control,
            MixControl::Fader(_) | MixControl::FaderLin(_) | MixControl::Balance(_)
        ),
        _ => false,
    }
}

fn global_messages(state: &TotalControl, command: &crate::osc_global::GlobalCommand) -> Vec<Message> {
    use crate::osc_global::GlobalCommand;
    match command {
        GlobalCommand::Channel {
            bus,
            channel,
            control,
        } => global_channel(state, *bus, *channel)
            .map(|cid| global_channel_messages(state, *bus, *channel, cid, control))
            .unwrap_or_default(),
        GlobalCommand::Mix {
            source,
            channel,
            submix,
            control,
        } => global_mix_messages(state, *source, *channel, *submix, control),
        GlobalCommand::ControlRoom(control) => global_control_room_messages(state, control),
        GlobalCommand::Reverb(crate::osc_global::FxControl::Enable(on)) => {
            vec![Message::OscRmeFx {
                echo: false,
                field: "on",
                value: if *on { 1.0 } else { 0.0 },
            }]
        }
        GlobalCommand::Echo(crate::osc_global::FxControl::Enable(on)) => {
            vec![Message::OscRmeFx {
                echo: true,
                field: "on",
                value: if *on { 1.0 } else { 0.0 },
            }]
        }
        GlobalCommand::MuteGroup { .. }
        | GlobalCommand::SoloGroup { .. }
        | GlobalCommand::FaderGroup { .. }
        | GlobalCommand::Reverb(_)
        | GlobalCommand::Echo(_)
        | GlobalCommand::Durec(_)
        | GlobalCommand::Send(_)
        | GlobalCommand::ShowWindow(_)
        | GlobalCommand::Level { .. }
        | GlobalCommand::Status(_) => Vec::new(),
        GlobalCommand::GlobalMute(on) => {
            let current = all_channel_ids(state)
                .into_iter()
                .any(|cid| channel_is_muted(state, cid));
            (current != *on)
                .then_some(vec![Message::GlobalMuteToggle])
                .unwrap_or_default()
        }
        GlobalCommand::GlobalSolo(on) => {
            let current = all_channel_ids(state)
                .into_iter()
                .any(|cid| channel_is_soloed(state, cid));
            (current != *on)
                .then_some(vec![Message::GlobalSoloToggle])
                .unwrap_or_default()
        }
        GlobalCommand::Undo => vec![Message::Undo],
        GlobalCommand::Redo => vec![Message::Redo],
        GlobalCommand::SnapshotLoad { number, .. } if (1..=8).contains(number) => {
            vec![Message::SnapshotClicked(*number as u8)]
        }
        GlobalCommand::LayoutLoad(number) if (1..=6).contains(number) => {
            vec![Message::LayoutClicked(*number as u8)]
        }
        GlobalCommand::SnapshotLoad { .. } | GlobalCommand::LayoutLoad(_) => Vec::new(),
        GlobalCommand::SnapshotSave => vec![Message::SnapshotStore],
    }
}

fn global_gain_db(gain: f32) -> f32 {
    if gain <= 0.0 {
        -300.0
    } else {
        20.0 * gain.log10()
    }
}

fn global_eq_controls(eq: &totalcontrol_core::channel::InputEq) -> Vec<crate::osc_global::ChannelControl> {
    use crate::osc_global::{ChannelControl, EqControl, LowCutControl};
    let mut out = vec![
        ChannelControl::LowCut(LowCutControl::Enable(eq.low_cut_enabled)),
        ChannelControl::LowCut(LowCutControl::Freq(f32::from(eq.low_cut_freq_hz))),
        ChannelControl::LowCut(LowCutControl::Slope(f32::from(eq.low_cut_slope_db_oct))),
        ChannelControl::Eq(EqControl::Enable(eq.enabled)),
    ];
    for (i, band) in eq.bands.iter().take(3).enumerate() {
        let number = i + 1;
        out.push(ChannelControl::Eq(EqControl::BandGain {
            band: number,
            db: band.gain_db,
        }));
        out.push(ChannelControl::Eq(EqControl::BandFreq {
            band: number,
            hz: f32::from(band.freq_hz),
        }));
        out.push(ChannelControl::Eq(EqControl::BandQ {
            band: number,
            q: band.q,
        }));
        let kind = match (i, band.band_type) {
            (0, EqBandType::Bell) | (1, EqBandType::Bell) | (2, EqBandType::Bell) => Some(0),
            (0, EqBandType::LowShelf) => Some(1),
            (2, EqBandType::HighShelf) => Some(1),
            (2, EqBandType::LowPass) => Some(2),
            _ => None,
        };
        if let Some(kind) = kind {
            out.push(ChannelControl::Eq(EqControl::BandType {
                band: number,
                kind,
            }));
        }
    }
    out
}

fn global_dynamics_controls(d: &Dynamics) -> Vec<crate::osc_global::ChannelControl> {
    use crate::osc_global::{AutoLevelControl, ChannelControl, DynamicsControl};
    vec![
        ChannelControl::Dynamics(DynamicsControl::Enable(d.on)),
        ChannelControl::Dynamics(DynamicsControl::Gain(d.gain_db)),
        ChannelControl::Dynamics(DynamicsControl::Attack(f32::from(d.attack_ms))),
        ChannelControl::Dynamics(DynamicsControl::Release(f32::from(d.release_ms))),
        ChannelControl::Dynamics(DynamicsControl::CompThreshold(d.comp_threshold_db)),
        ChannelControl::Dynamics(DynamicsControl::CompRatio(d.comp_ratio)),
        ChannelControl::Dynamics(DynamicsControl::ExpThreshold(d.exp_threshold_db)),
        ChannelControl::Dynamics(DynamicsControl::ExpRatio(d.exp_ratio)),
        ChannelControl::AutoLevel(AutoLevelControl::Enable(d.auto_level)),
        ChannelControl::AutoLevel(AutoLevelControl::MaxGain(d.auto_max_gain_db)),
        ChannelControl::AutoLevel(AutoLevelControl::Headroom(d.auto_headroom_db)),
        ChannelControl::AutoLevel(AutoLevelControl::RiseTime(d.auto_rise_s)),
    ]
}

fn global_channel_state(
    state: &TotalControl,
    bus: crate::osc_global::Bus,
    channel: usize,
) -> Vec<crate::osc_global::GlobalCommand> {
    use crate::osc_global::{Bus, ChannelControl, GlobalCommand};
    let Some(cid) = global_channel(state, bus, channel) else {
        return Vec::new();
    };
    let command = |control| GlobalCommand::Channel {
        bus,
        channel,
        control,
    };
    let mut out = Vec::new();
    match bus {
        Bus::Input => {
            let Some(c) = state.device.inputs().get(channel) else {
                return out;
            };
            out.push(command(ChannelControl::Mute(c.mute)));
            out.push(command(ChannelControl::Name(rme_name(state, cid))));
            out.push(command(ChannelControl::Stereo(
                state.device.input_pair_linked(channel / 2),
            )));
            out.push(command(ChannelControl::MsProc(c.ms_proc)));
            out.push(command(ChannelControl::Phase(c.phase)));
            out.push(command(ChannelControl::Width(c.width.clamp(0.0, 1.0))));
            if let Some(raw) = c.gain {
                out.push(command(ChannelControl::Gain(rme_gain_db(
                    state, channel, raw,
                ))));
            }
            if let Some(level) = c.level {
                out.push(command(ChannelControl::RefLevel(level.code() as usize)));
            }
            out.push(command(ChannelControl::Phantom(c.phantom)));
            out.push(command(ChannelControl::Instrument(c.instrument)));
            out.push(command(ChannelControl::Pad(c.pad)));
            out.push(command(ChannelControl::Autoset(c.autoset)));
            out.push(command(ChannelControl::FxSend(c.fx_send_db)));
            if let Some(eq) = c.eq.as_ref() {
                out.extend(global_eq_controls(eq).into_iter().map(command));
            }
            if let Some(d) = c.dynamics.as_ref() {
                out.extend(global_dynamics_controls(d).into_iter().map(command));
            }
        }
        Bus::Playback => {
            let Some(c) = state.device.playbacks().get(channel) else {
                return out;
            };
            out.push(command(ChannelControl::Mute(c.mute)));
            out.push(command(ChannelControl::Name(rme_name(state, cid))));
            out.push(command(ChannelControl::Stereo(!c.split)));
            out.push(command(ChannelControl::MsProc(c.ms_proc)));
            out.push(command(ChannelControl::Width(c.width.clamp(0.0, 1.0))));
            out.push(command(ChannelControl::FxSend(c.fx_send_db)));
        }
        Bus::Output => {
            let Some(c) = state.device.outputs().get(channel) else {
                return out;
            };
            out.push(command(ChannelControl::Mute(c.mute)));
            out.push(command(ChannelControl::Name(rme_name(state, cid))));
            out.push(command(ChannelControl::Stereo(
                state.device.output_linked(channel / 2),
            )));
            out.push(command(ChannelControl::Phase(c.phase_l)));
            out.push(command(ChannelControl::Volume(global_gain_db(c.volume))));
            out.push(command(ChannelControl::VolumeLin(
                crate::osc_rme::gain_to_fader(c.volume),
            )));
            out.push(command(ChannelControl::Balance(
                f32::from(c.balance) / 100.0,
            )));
            out.push(command(ChannelControl::FxReturn(c.fx_return_db)));
            out.push(command(ChannelControl::Loopback(c.loopback)));
            out.push(command(ChannelControl::TalkbackSel(c.talkback)));
            if let Some(eq) = c.eq.as_ref() {
                out.extend(global_eq_controls(eq).into_iter().map(command));
            }
            if let Some(d) = c.dynamics.as_ref() {
                out.extend(global_dynamics_controls(d).into_iter().map(command));
            }
        }
    }
    out
}

fn global_matrix_state(
    state: &TotalControl,
    mode: u8,
    only_pair: Option<usize>,
) -> Vec<crate::osc_global::GlobalCommand> {
    use crate::osc_global::{Bus, GlobalCommand, MixControl};
    let pairs = state.device.output_pair_count();
    let mut out = Vec::new();
    for (bus, count) in [
        (Bus::Input, state.device.inputs().len()),
        (Bus::Playback, state.device.playbacks().len()),
    ] {
        for channel in 0..count {
            let Some(cid) = global_channel(state, bus, channel) else {
                continue;
            };
            for pair in 0..pairs {
                if only_pair.is_some_and(|only| only != pair) {
                    continue;
                }
                let Ok(volume) = state.device.volume(cid, pair) else {
                    continue;
                };
                let db = global_gain_db(volume);
                if mode == 2 && db <= -65.0 {
                    continue;
                }
                let submix = pair * 2;
                out.push(GlobalCommand::Mix {
                    source: bus,
                    channel,
                    submix,
                    control: MixControl::Fader(db),
                });
                out.push(GlobalCommand::Mix {
                    source: bus,
                    channel,
                    submix,
                    control: MixControl::Balance(
                        f32::from(channel_pan(state, cid, pair)) / 100.0,
                    ),
                });
            }
        }
    }
    out
}

fn global_control_room_state(state: &TotalControl) -> Vec<crate::osc_global::GlobalCommand> {
    use crate::osc_global::{ControlRoomControl, GlobalCommand};
    let mut out = Vec::new();
    for (dest, pair) in state.assign.iter().take(OUTPUT_DESTS).enumerate() {
        let Some(pair) = *pair else { continue };
        let channel = pair * 2;
        out.push(GlobalCommand::ControlRoom(match dest {
            0 => ControlRoomControl::MainOut(channel),
            1 => ControlRoomControl::MainOutB(channel),
            slot => ControlRoomControl::Phones {
                slot: slot - 1,
                channel,
            },
        }));
    }
    out.push(GlobalCommand::ControlRoom(ControlRoomControl::MainMono(
        state.device.settings().mono,
    )));
    out.push(GlobalCommand::ControlRoom(ControlRoomControl::DimReduction(
        state.control_room.main_dim_db,
    )));
    out.push(GlobalCommand::ControlRoom(ControlRoomControl::Dim(
        state.device.settings().dim,
    )));
    out.push(GlobalCommand::ControlRoom(ControlRoomControl::RecallVolume(
        state.control_room.recall_db,
    )));
    out.push(GlobalCommand::ControlRoom(ControlRoomControl::Talkback(
        state.talkback_on,
    )));
    out.push(GlobalCommand::ControlRoom(ControlRoomControl::LinkAB(
        state.control_room.speaker_b_linked,
    )));
    out.push(GlobalCommand::ControlRoom(ControlRoomControl::SpeakerB(
        state.device.settings().speaker_b,
    )));
    out.push(GlobalCommand::ControlRoom(ControlRoomControl::MuteFx(
        state.device.settings().mute_fx,
    )));
    if let Some(input) = state.control_room.talkback_input {
        out.push(GlobalCommand::ControlRoom(ControlRoomControl::TalkChannel(
            input,
        )));
    }
    out.push(GlobalCommand::ControlRoom(ControlRoomControl::ExternalIn(
        state.device.settings().external_input,
    )));
    if let Some(pair) = state.control_room.external_input {
        out.push(GlobalCommand::ControlRoom(ControlRoomControl::ExtInChannel(
            pair * 2,
        )));
        out.push(GlobalCommand::ControlRoom(ControlRoomControl::ExtInGain(
            state.control_room.external_gain_db,
        )));
    }
    if let Some(dest) = state.assign[CUE_DEST] {
        if let Some(pair) = state.assign.get(dest).and_then(|slot| *slot) {
            out.push(GlobalCommand::ControlRoom(ControlRoomControl::CueChannel(
                pair * 2,
            )));
        }
    }
    out
}

fn global_fx_state(state: &TotalControl) -> Vec<crate::osc_global::GlobalCommand> {
    let settings = state.device.settings();
    vec![
        crate::osc_global::GlobalCommand::Reverb(crate::osc_global::FxControl::Enable(
            settings.reverb.unwrap_or_default().on,
        )),
        crate::osc_global::GlobalCommand::Echo(crate::osc_global::FxControl::Enable(
            settings.echo.unwrap_or_default().on,
        )),
    ]
}

fn global_status_state(state: &TotalControl) -> Vec<crate::osc_global::GlobalCommand> {
    use crate::osc_global::{GlobalCommand, StatusControl};
    let mut out = vec![
        GlobalCommand::Status(StatusControl::Device(
            state.device.kind().label().to_string(),
        )),
        GlobalCommand::Status(StatusControl::Connection(
            state.device.is_connected(),
        )),
        GlobalCommand::GlobalMute(any_channel_muted(state)),
        GlobalCommand::GlobalSolo(any_channel_soloed(state)),
    ];
    for number in 1..=8 {
        let value = if state.active_snapshot == Some(number) {
            if state.snapshot_dirty {
                3.0
            } else {
                2.0
            }
        } else {
            0.0
        };
        out.push(GlobalCommand::SnapshotLoad {
            number,
            state: value,
        });
    }
    out
}

fn global_state_commands(
    state: &TotalControl,
    control: &crate::osc_global::SendControl,
) -> Vec<crate::osc_global::GlobalCommand> {
    use crate::osc_global::{Bus, GlobalCommand, SendControl};
    match control {
        SendControl::SendAll(mode) => {
            let mut out = Vec::new();
            for channel in 0..state.device.inputs().len() {
                out.extend(global_channel_state(state, Bus::Input, channel));
            }
            for channel in 0..state.device.playbacks().len() {
                out.extend(global_channel_state(state, Bus::Playback, channel));
            }
            for channel in 0..state.device.outputs().len() {
                out.extend(global_channel_state(state, Bus::Output, channel));
            }
            out.extend(global_matrix_state(state, *mode, None));
            out.extend(global_control_room_state(state));
            out.extend(global_fx_state(state));
            out.extend(global_status_state(state));
            out
        }
        SendControl::SendSettings => {
            let mut out = global_control_room_state(state);
            out.extend(global_fx_state(state));
            out.push(GlobalCommand::GlobalMute(any_channel_muted(state)));
            out.push(GlobalCommand::GlobalSolo(any_channel_soloed(state)));
            out
        }
        SendControl::SendChannel { bus, channel } => global_channel_state(state, *bus, *channel),
        SendControl::SendMix(mode) => global_matrix_state(state, *mode, None),
        SendControl::SendSubmix { submix, mode } => {
            global_matrix_state(state, *mode, Some(submix / 2))
        }
        SendControl::SendState => global_status_state(state),
    }
}

fn send_global_state(state: &mut TotalControl, control: &crate::osc_global::SendControl) {
    let commands = global_state_commands(state, control);
    log::debug!("OSC global: sending {} state messages", commands.len());
    for command in commands {
        send_global(state, command);
    }
}

/// Low-level feedback send.  Everything that reaches a controller goes
/// through here with an already-classic RME address.
fn send_osc(state: &mut TotalControl, addr: String, args: Vec<OscType>) {
    // The classic feedback builder is also used by the device mirror.  Do
    // not leak classic page addresses into a Global OSC connection.
    if state.osc_dialect != crate::osc::OscDialect::Classic {
        return;
    }
    if let Some(tx) = state.osc_tx.as_mut() {
        let _ = tx.try_send(OscOutbound::Raw { addr, args });
    }
}

fn global_feedback(msg: &OscOutbound) -> Option<crate::osc_global::GlobalCommand> {
    use crate::osc_global::{Bus, ChannelControl, GlobalCommand, MixControl};
    match msg {
        OscOutbound::Volume(cid, out, gain) => {
            let db = if *gain <= 0.0 {
                -300.0
            } else {
                20.0 * gain.log10()
            };
            Some(match cid {
                ChannelId::Output(i) => GlobalCommand::Channel {
                    bus: Bus::Output,
                    channel: *i,
                    control: ChannelControl::Volume(db),
                },
                ChannelId::Input(i) => GlobalCommand::Mix {
                    source: Bus::Input,
                    channel: *i,
                    submix: out * 2,
                    control: MixControl::Fader(db),
                },
                ChannelId::Playback(i) => GlobalCommand::Mix {
                    source: Bus::Playback,
                    channel: *i,
                    submix: out * 2,
                    control: MixControl::Fader(db),
                },
            })
        }
        OscOutbound::Pan(cid, out, pan) => {
            let value = f32::from(*pan) / 100.0;
            Some(match cid {
                ChannelId::Output(i) => GlobalCommand::Channel {
                    bus: Bus::Output,
                    channel: *i,
                    control: ChannelControl::Balance(value),
                },
                ChannelId::Input(i) => GlobalCommand::Mix {
                    source: Bus::Input,
                    channel: *i,
                    submix: out * 2,
                    control: MixControl::Balance(value),
                },
                ChannelId::Playback(i) => GlobalCommand::Mix {
                    source: Bus::Playback,
                    channel: *i,
                    submix: out * 2,
                    control: MixControl::Balance(value),
                },
            })
        }
        OscOutbound::Mute(cid, on) => Some(match cid {
            ChannelId::Input(i) => GlobalCommand::Channel {
                bus: Bus::Input,
                channel: *i,
                control: ChannelControl::Mute(*on),
            },
            ChannelId::Playback(i) => GlobalCommand::Channel {
                bus: Bus::Playback,
                channel: *i,
                control: ChannelControl::Mute(*on),
            },
            ChannelId::Output(i) => GlobalCommand::Channel {
                bus: Bus::Output,
                channel: *i,
                control: ChannelControl::Mute(*on),
            },
        }),
        OscOutbound::Solo(..) | OscOutbound::Raw { .. } | OscOutbound::Global(_) => None,
    }
}

fn send_global(state: &mut TotalControl, command: crate::osc_global::GlobalCommand) {
    if let Some(tx) = state.osc_tx.as_mut() {
        let _ = tx.try_send(OscOutbound::Global(command));
    }
}

fn notify_osc(state: &mut TotalControl, msg: OscOutbound) {
    match msg {
        OscOutbound::Global(command) => {
            if state.osc_dialect == crate::osc::OscDialect::Global {
                send_global(state, command);
            }
        }
        other => {
            if state.osc_dialect == crate::osc::OscDialect::Global {
                if let Some(command) = global_feedback(&other) {
                    send_global(state, command);
                }
                return;
            }
            match other {
                OscOutbound::Volume(cid, _, _)
                | OscOutbound::Pan(cid, _, _)
                | OscOutbound::Mute(cid, _)
                | OscOutbound::Solo(cid, _) => notify_rme_channel(state, cid),
                OscOutbound::Raw { addr, args } => notify_rme_native(state, &addr, &args),
                OscOutbound::Global(_) => unreachable!(),
            }
        }
    }
}

fn notify_osc_raw(state: &mut TotalControl, addr: impl Into<String>, args: Vec<OscType>) {
    notify_osc(
        state,
        OscOutbound::Raw {
            addr: addr.into(),
            args,
        },
    );
}

fn osc_bool(on: bool) -> OscType {
    OscType::Float(if on { 1.0 } else { 0.0 })
}

fn notify_osc_bool(state: &mut TotalControl, addr: impl Into<String>, on: bool) {
    notify_osc_raw(state, addr, vec![osc_bool(on)]);
}

fn channel_slug(cid: ChannelId) -> (&'static str, usize) {
    match cid {
        ChannelId::Input(i) => ("input", i),
        ChannelId::Playback(i) => ("playback", i),
        ChannelId::Output(i) => ("output", i),
    }
}

fn rme_cid_row(cid: ChannelId) -> (crate::osc_rme::Bus, usize) {
    match cid {
        ChannelId::Input(i) => (crate::osc_rme::Bus::Input, i),
        ChannelId::Playback(i) => (crate::osc_rme::Bus::Playback, i),
        ChannelId::Output(i) => (crate::osc_rme::Bus::Output, i),
    }
}

fn rme_page1_index(state: &TotalControl, cid: ChannelId) -> Option<usize> {
    let (bus, idx) = rme_cid_row(cid);
    if bus != state.rme.bus {
        return None;
    }
    let end = (state.rme.bank + state.rme.bank_size.max(1)).min(crate::osc_rme::CHANNELS);
    (idx >= state.rme.bank && idx < end).then_some(idx - state.rme.bank + 1)
}

fn rme_is_selected(state: &TotalControl, cid: ChannelId) -> bool {
    let (bus, idx) = rme_cid_row(cid);
    bus == state.rme.bus && idx == state.rme.channel
}

fn rme_name(state: &TotalControl, cid: ChannelId) -> String {
    if let Some(label) = user_label(state, cid) {
        return label.to_string();
    }
    match cid {
        ChannelId::Input(i) => state.device.inputs().get(i).map(|c| c.name.clone()),
        ChannelId::Playback(i) => state.device.playbacks().get(i).map(|c| c.name.clone()),
        ChannelId::Output(i) => state.device.outputs().get(i).map(|c| c.name.clone()),
    }
    .unwrap_or_default()
}

fn rme_db_string(gain: f32) -> String {
    if gain <= 0.0 {
        "-oo".into()
    } else {
        format!("{:.1} dB", 20.0 * gain.log10())
    }
}

fn rme_pan_string(pan: i8) -> String {
    match pan {
        0 => "C".into(),
        p if p < 0 => format!("L{}", -p),
        p => format!("R{p}"),
    }
}

fn rme_gain_db(_state: &TotalControl, idx: usize, raw: u32) -> f32 {
    if idx < 2 {
        raw as f32
    } else {
        raw as f32 * 0.5
    }
}

fn rme_gain_norm(state: &TotalControl, idx: usize, raw: u32) -> f32 {
    let max = state
        .device
        .inputs()
        .get(idx)
        .and_then(|c| c.gain_max)
        .unwrap_or(if idx < 2 { 65 } else { state.rme.gain_max_34 });
    (raw as f32 / max.max(1) as f32).clamp(0.0, 1.0)
}

fn rme_channel_volume(state: &TotalControl, cid: ChannelId) -> (f32, usize) {
    match cid {
        ChannelId::Output(i) => (
            state.device.outputs().get(i).map(|o| o.volume).unwrap_or(0.0),
            i,
        ),
        ChannelId::Input(i) => (
            state
                .device
                .inputs()
                .get(i)
                .and_then(|c| c.volumes.get(state.rme.submix))
                .copied()
                .unwrap_or(0.0)
                * trim_factor(state.device.inputs().get(i).map(|c| c.trim).unwrap_or(0.0)),
            state.rme.submix,
        ),
        ChannelId::Playback(i) => (
            state
                .device
                .playbacks()
                .get(i)
                .and_then(|c| c.volumes.get(state.rme.submix))
                .copied()
                .unwrap_or(0.0)
                * trim_factor(state.device.playbacks().get(i).map(|c| c.trim).unwrap_or(0.0)),
            state.rme.submix,
        ),
    }
}

fn rme_channel_pan(state: &TotalControl, cid: ChannelId) -> i8 {
    let (_, out) = rme_channel_volume(state, cid);
    channel_pan(state, cid, out)
}

fn rme_value(state: &mut TotalControl, addr: String, value: f32) {
    send_osc(state, addr, vec![OscType::Float(value)]);
}

fn rme_string(state: &mut TotalControl, addr: String, value: String) {
    send_osc(state, addr, vec![OscType::String(value)]);
}

fn rme_bool(state: &mut TotalControl, addr: String, on: bool) {
    send_osc(state, addr, vec![osc_bool(on)]);
}

fn rme_page1_channel(state: &mut TotalControl, cid: ChannelId, n: usize) {
    let (gain, _) = rme_channel_volume(state, cid);
    let pan = rme_channel_pan(state, cid);
    let muted = channel_is_muted(state, cid);
    let soloed = channel_is_soloed(state, cid);
    let fader = crate::osc_rme::gain_to_fader(gain);
    rme_value(state, format!("/1/volume{n}"), fader);
    rme_string(state, format!("/1/volume{n}Val"), rme_db_string(gain));
    rme_value(
        state,
        format!("/1/pan{n}"),
        crate::osc_rme::pan_norm(pan),
    );
    rme_bool(state, format!("/1/mute/1/{n}"), muted);
    rme_bool(state, format!("/1/solo/1/{n}"), soloed);
    rme_bool(
        state,
        format!("/1/select/1/{n}"),
        state.selected.contains(&cid),
    );
    match cid {
        ChannelId::Input(i) => {
            if let Some(ch) = state.device.inputs().get(i).cloned() {
                if let Some(raw) = ch.gain {
                    let norm = rme_gain_norm(state, i, raw);
                    let db = rme_gain_db(state, i, raw);
                    rme_value(state, format!("/1/micgain{n}"), norm);
                    rme_string(
                        state,
                        format!("/1/micgain{n}Val"),
                        format!("{db:.1} dB"),
                    );
                }
                rme_bool(state, format!("/1/phantom/1/{n}"), ch.phantom);
            }
        }
        ChannelId::Output(i) => {
            if let Some(ch) = state.device.outputs().get(i).cloned() {
                rme_bool(state, format!("/1/cue/1/{n}"), ch.cue);
            }
        }
        ChannelId::Playback(_) => {}
    }
    let name = rme_name(state, cid);
    rme_string(state, format!("/1/trackname{n}"), name);
}

fn rme_channel_phase(state: &TotalControl, cid: ChannelId, right: bool) -> Option<bool> {
    match cid {
        ChannelId::Input(i) => state
            .device
            .inputs()
            .get(i)
            .map(|c| if right { c.phase_r } else { c.phase }),
        ChannelId::Output(i) => state
            .device
            .outputs()
            .get(i)
            .map(|c| if right { c.phase_r } else { c.phase_l }),
        ChannelId::Playback(_) => None,
    }
}

fn rme_channel_low_cut(state: &TotalControl, cid: ChannelId) -> Option<bool> {
    match cid {
        ChannelId::Input(i) => state.device.inputs().get(i).and_then(|c| c.eq).map(|e| e.low_cut_enabled),
        ChannelId::Output(i) => state.device.outputs().get(i).and_then(|c| c.eq).map(|e| e.low_cut_enabled),
        ChannelId::Playback(_) => None,
    }
}

fn rme_channel_eq(state: &TotalControl, cid: ChannelId) -> Option<totalcontrol_core::channel::InputEq> {
    match cid {
        ChannelId::Input(i) => state.device.inputs().get(i).and_then(|c| c.eq),
        ChannelId::Output(i) => state.device.outputs().get(i).and_then(|c| c.eq),
        ChannelId::Playback(_) => None,
    }
}

fn rme_eq_type_value(band: usize, ty: EqBandType) -> f32 {
    match (band, ty) {
        (0, EqBandType::LowShelf) => 1.0,
        (0, _) => 0.0,
        (2, EqBandType::HighShelf) => 0.5,
        (2, EqBandType::LowPass) => 1.0,
        (2, _) => 0.0,
        _ => 0.0,
    }
}

fn rme_eq_type_name(band: usize, ty: EqBandType) -> String {
    match (band, ty) {
        (0, EqBandType::LowShelf) => "Low Shelf".into(),
        (2, EqBandType::HighShelf) => "High Shelf".into(),
        (2, EqBandType::LowPass) => "Low Pass".into(),
        _ => "Bell".into(),
    }
}

fn rme_page2_channel(state: &mut TotalControl, cid: ChannelId) {
    let (gain, _) = rme_channel_volume(state, cid);
    let pan = rme_channel_pan(state, cid);
    let muted = channel_is_muted(state, cid);
    let soloed = channel_is_soloed(state, cid);
    let phase = rme_channel_phase(state, cid, false);
    let phase_right = rme_channel_phase(state, cid, true);
    let linked = pair_linked(state, cid);

    rme_value(state, "/2/volume".into(), crate::osc_rme::gain_to_fader(gain));
    rme_string(state, "/2/volumeVal".into(), rme_db_string(gain));
    rme_value(state, "/2/pan".into(), crate::osc_rme::pan_norm(pan));
    rme_string(state, "/2/panVal".into(), rme_pan_string(pan));
    rme_bool(state, "/2/mute".into(), muted);
    rme_bool(state, "/2/solo".into(), soloed);
    if let Some(phase) = phase {
        rme_bool(state, "/2/phase".into(), phase);
    }
    if let Some(phase) = phase_right {
        rme_bool(state, "/2/phaseRight".into(), phase);
    }
    rme_bool(state, "/2/stereo".into(), linked);

    let input = match cid {
        ChannelId::Input(i) => state.device.inputs().get(i).cloned(),
        _ => None,
    };
    if let Some(ch) = input {
        if let Some(raw) = ch.gain {
            let norm = rme_gain_norm(state, ch.id, raw);
            let db = rme_gain_db(state, ch.id, raw);
            rme_value(state, "/2/gain".into(), norm);
            rme_string(state, "/2/gainVal".into(), format!("{db:.1} dB"));
        }
        if let Some(raw) = ch.gain_right {
            let norm = rme_gain_norm(state, ch.id, raw);
            let db = rme_gain_db(state, ch.id, raw);
            rme_value(state, "/2/gainRight".into(), norm);
            rme_string(state, "/2/gainRightVal".into(), format!("{db:.1} dB"));
        }
        rme_bool(state, "/2/phantom".into(), ch.phantom);
        rme_bool(state, "/2/instrument".into(), ch.instrument);
        rme_bool(state, "/2/pad".into(), ch.pad);
        rme_bool(state, "/2/autoset".into(), ch.autoset);
        let level = ch
            .level
            .or_else(|| totalcontrol_core::channel::InputLevel::from_code(ch.ref_level));
        if let Some(level) = level {
            send_osc(state, "/2/refLevel".into(), vec![OscType::Int(level.code() as i32)]);
        }
        rme_bool(state, "/2/msProc".into(), ch.ms_proc);
        rme_value(state, "/2/width".into(), ch.width.clamp(0.0, 1.0));
        let send = (1.0 + ch.fx_send_db / 65.0).clamp(0.0, 1.0);
        rme_value(state, "/2/reverbSend".into(), send);
    }

    if let Some(ch) = match cid {
        ChannelId::Playback(i) => state.device.playbacks().get(i).cloned(),
        _ => None,
    } {
        rme_bool(state, "/2/msProc".into(), ch.ms_proc);
        rme_value(state, "/2/width".into(), ch.width.clamp(0.0, 1.0));
        let send = (1.0 + ch.fx_send_db / 65.0).clamp(0.0, 1.0);
        rme_value(state, "/2/reverbSend".into(), send);
    }

    if let Some(ch) = match cid {
        ChannelId::Output(i) => state.device.outputs().get(i).cloned(),
        _ => None,
    } {
        let ret = (1.0 + ch.fx_return_db / 65.0).clamp(0.0, 1.0);
        rme_value(state, "/2/reverbReturn".into(), ret);
        rme_bool(state, "/2/cue".into(), ch.cue);
        rme_bool(state, "/2/talkbackSel".into(), ch.talkback);
        rme_bool(state, "/2/noTrim".into(), ch.no_trim);
        rme_bool(state, "/2/loopback".into(), ch.loopback);
    }

    let low_cut = rme_channel_low_cut(state, cid);
    if let Some(on) = low_cut {
        rme_bool(state, "/2/lowcutEnable".into(), on);
    }
    let eq = rme_channel_eq(state, cid);
    if let Some(eq) = eq {
        rme_bool(state, "/2/lowcutEnable".into(), eq.low_cut_enabled);
        rme_value(
            state,
            "/2/lowcutFreq".into(),
            crate::osc_rme::hz_freq(eq.low_cut_freq_hz as f32, 20.0, 500.0),
        );
        rme_string(
            state,
            "/2/lowcutFreqVal".into(),
            format!("{} Hz", eq.low_cut_freq_hz),
        );
        let grade = ((eq.low_cut_slope_db_oct.saturating_sub(6)) as f32 / 18.0).clamp(0.0, 1.0);
        rme_value(state, "/2/lowcutGrade".into(), grade);
        rme_string(
            state,
            "/2/lowcutGradeVal".into(),
            format!("{} dB/oct", eq.low_cut_slope_db_oct),
        );
        rme_bool(state, "/2/eqEnable".into(), eq.enabled);
        for (band, b) in eq.bands.iter().enumerate() {
            rme_value(
                state,
                format!("/2/eqType{}", band + 1),
                rme_eq_type_value(band, b.band_type),
            );
            rme_string(
                state,
                format!("/2/eqType{}Val", band + 1),
                rme_eq_type_name(band, b.band_type),
            );
            rme_value(
                state,
                format!("/2/eqGain{}", band + 1),
                (b.gain_db + 24.0) / 48.0,
            );
            rme_string(
                state,
                format!("/2/eqGain{}Val", band + 1),
                format!("{:.1} dB", b.gain_db),
            );
            rme_value(
                state,
                format!("/2/eqFreq{}", band + 1),
                crate::osc_rme::hz_freq(b.freq_hz as f32, 20.0, 20_000.0),
            );
            rme_string(
                state,
                format!("/2/eqFreq{}Val", band + 1),
                format!("{} Hz", b.freq_hz),
            );
            let q = ((b.q.max(0.05).log10() + 1.0) / 2.0).clamp(0.0, 1.0);
            rme_value(state, format!("/2/eqQ{}", band + 1), q);
            rme_string(
                state,
                format!("/2/eqQ{}Val", band + 1),
                format!("{:.2}", b.q),
            );
        }
    }

    let dynamics = match cid {
        ChannelId::Input(i) => state.device.inputs().get(i).and_then(|c| c.dynamics),
        ChannelId::Output(i) => state.device.outputs().get(i).and_then(|c| c.dynamics),
        ChannelId::Playback(_) => None,
    };
    if let Some(d) = dynamics {
        rme_bool(state, "/2/compexpEnable".into(), d.on);
        rme_value(state, "/2/compexpGain".into(), (d.gain_db + 30.0) / 60.0);
        rme_value(state, "/2/compexpAttack".into(), d.attack_ms as f32 / 200.0);
        rme_value(
            state,
            "/2/compexpRelease".into(),
            ((d.release_ms as f32 - 100.0) / 899.0).clamp(0.0, 1.0),
        );
        rme_value(
            state,
            "/2/compTrsh".into(),
            ((d.comp_threshold_db + 60.0) / 60.0).clamp(0.0, 1.0),
        );
        rme_value(
            state,
            "/2/compRatio".into(),
            ((d.comp_ratio - 1.0) / 9.0).clamp(0.0, 1.0),
        );
        rme_value(
            state,
            "/2/expTrsh".into(),
            ((d.exp_threshold_db + 99.0) / 69.0).clamp(0.0, 1.0),
        );
        rme_value(
            state,
            "/2/expRatio".into(),
            ((d.exp_ratio - 1.0) / 9.0).clamp(0.0, 1.0),
        );
        rme_bool(state, "/2/alevEnable".into(), d.auto_level);
        rme_value(
            state,
            "/2/alevMaxgain".into(),
            (d.auto_max_gain_db / 18.0).clamp(0.0, 1.0),
        );
        rme_value(
            state,
            "/2/alevHeadroom".into(),
            ((d.auto_headroom_db - 3.0) / 9.0).clamp(0.0, 1.0),
        );
        rme_value(
            state,
            "/2/alevRisetime".into(),
            ((d.auto_rise_s - 0.1) / 9.8).clamp(0.0, 1.0),
        );
    }
    let name = rme_name(state, cid);
    rme_string(state, "/2/trackname".into(), name);
}

fn rme_selected_meter(state: &TotalControl) -> f32 {
    let idx = state.rme.channel;
    let meter = match state.rme.bus {
        crate::osc_rme::Bus::Input => state.input_meters.get(idx),
        crate::osc_rme::Bus::Playback => state.playback_meters.get(idx),
        crate::osc_rme::Bus::Output => state.output_meters.get(idx),
    };
    meter.map(|m| m.frame().value).unwrap_or(0.0)
}

fn rme_page1_meter(state: &TotalControl, n: usize, idx: usize) -> f32 {
    let meter = match state.rme.bus {
        crate::osc_rme::Bus::Input => state.input_meters.get(idx),
        crate::osc_rme::Bus::Playback => state.playback_meters.get(idx),
        crate::osc_rme::Bus::Output => state.output_meters.get(idx),
    };
    let _ = n;
    meter.map(|m| m.frame().value).unwrap_or(0.0)
}

fn notify_rme_meters(state: &mut TotalControl) {
    let end = (state.rme.bank + state.rme.bank_size.max(1)).min(crate::osc_rme::CHANNELS);
    for idx in state.rme.bank..end {
        let n = idx - state.rme.bank + 1;
        let value = rme_page1_meter(state, n, idx);
        rme_value(state, format!("/1/level{n}Left"), value);
        rme_value(state, format!("/1/level{n}Right"), value);
    }
    let selected = rme_selected_meter(state);
    rme_value(state, "/2/levelLeft".into(), selected);
    rme_value(state, "/2/levelRight".into(), selected);
}

fn notify_rme_page1(state: &mut TotalControl) {
    let end = (state.rme.bank + state.rme.bank_size.max(1)).min(crate::osc_rme::CHANNELS);
    let ids: Vec<ChannelId> = (state.rme.bank..end)
        .filter_map(|idx| match state.rme.bus {
            crate::osc_rme::Bus::Input => Some(ChannelId::Input(idx)),
            crate::osc_rme::Bus::Playback => Some(ChannelId::Playback(idx)),
            crate::osc_rme::Bus::Output => Some(ChannelId::Output(idx)),
        })
        .collect();
    for (n, cid) in ids.into_iter().enumerate() {
        rme_page1_channel(state, cid, n + 1);
    }
    if let Some(pair) = state.device.settings().main_out {
        let idx = pair * 2;
        if let Some(volume) = state.device.outputs().get(idx).map(|ch| ch.volume) {
            rme_value(state, "/1/mastervolume".into(), crate::osc_rme::gain_to_fader(volume));
            rme_string(state, "/1/mastervolumeVal".into(), rme_db_string(volume));
        }
    }
    if let Some(name) = state
        .device
        .outputs()
        .get(state.rme.submix * 2)
        .map(|out| out.name.clone())
    {
        rme_string(state, "/1/labelSubmix".into(), name);
    }
    let s = state.device.settings().clone();
    let speaker_b_linked = state.control_room.speaker_b_linked;
    let talkback = state.talkback_on;
    let global_mute = any_channel_muted(state);
    let global_solo = any_channel_soloed(state);
    rme_bool(state, "/1/mainDim".into(), s.dim);
    rme_bool(state, "/1/mainSpeakerB".into(), s.speaker_b);
    rme_bool(state, "/1/speakerBLinked".into(), speaker_b_linked);
    rme_bool(state, "/1/mainMuteFx".into(), s.mute_fx);
    rme_bool(state, "/1/mainMono".into(), s.mono);
    rme_bool(state, "/1/mainTalkback".into(), talkback);
    rme_bool(state, "/1/globalMute".into(), global_mute);
    rme_bool(state, "/1/globalSolo".into(), global_solo);
}

fn notify_rme_page2(state: &mut TotalControl) {
    let idx = state.rme.channel;
    let cid = match state.rme.bus {
        crate::osc_rme::Bus::Input => ChannelId::Input(idx),
        crate::osc_rme::Bus::Playback => ChannelId::Playback(idx),
        crate::osc_rme::Bus::Output => ChannelId::Output(idx),
    };
    rme_page2_channel(state, cid);
    let value = rme_selected_meter(state);
    rme_value(state, "/2/levelLeft".into(), value);
    rme_value(state, "/2/levelRight".into(), value);
    if let Some(out) = state.device.outputs().get(state.rme.submix * 2) {
        rme_string(state, "/2/labelSubmix".into(), out.name.clone());
    }
}

fn notify_rme_page3(state: &mut TotalControl) {
    let r = state.device.settings().reverb.unwrap_or_default();
    rme_bool(state, "/3/reverbEnable".into(), r.on);
    rme_value(state, "/3/reverbType".into(), r.kind as f32);
    rme_value(state, "/3/reverbPredelay".into(), r.pre_delay_ms as f32 / 999.0);
    rme_value(
        state,
        "/3/reverbLowcut".into(),
        crate::osc_rme::hz_freq(r.low_cut_hz as f32, 20.0, 500.0),
    );
    rme_value(
        state,
        "/3/reverbHighcut".into(),
        crate::osc_rme::hz_freq(r.high_cut_hz as f32, 5000.0, 20_000.0),
    );
    rme_value(state, "/3/reverbRoomscale".into(), ((r.room_scale - 0.5) / 2.5).clamp(0.0, 1.0));
    rme_value(state, "/3/reverbAttack".into(), ((r.attack_ms as f32 - 5.0) / 395.0).clamp(0.0, 1.0));
    rme_value(state, "/3/reverbHold".into(), ((r.hold_ms as f32 - 5.0) / 395.0).clamp(0.0, 1.0));
    rme_value(state, "/3/reverbRelease".into(), ((r.release_ms as f32 - 5.0) / 495.0).clamp(0.0, 1.0));
    rme_value(state, "/3/reverbTime".into(), ((r.reverb_time_ds as f32 - 1.0) / 49.0).clamp(0.0, 1.0));
    rme_value(
        state,
        "/3/reverbHighdamp".into(),
        crate::osc_rme::hz_freq(r.high_damp_hz as f32, 1000.0, 20_000.0),
    );
    rme_value(state, "/3/reverbSmooth".into(), r.smoothness as f32 / 100.0);
    rme_value(state, "/3/reverbWidth".into(), r.width as f32 / 100.0);
    rme_value(state, "/3/reverbVolume".into(), ((r.volume_db + 65.0) / 65.0).clamp(0.0, 1.0));

    let e = state.device.settings().echo.unwrap_or_default();
    rme_bool(state, "/3/echoEnable".into(), e.on);
    rme_value(state, "/3/echoType".into(), e.kind as f32 / 2.0);
    rme_value(state, "/3/echoDelaytime".into(), e.delay_ms as f32 / 2000.0);
    rme_value(state, "/3/echoFeedback".into(), e.feedback as f32 / 100.0);
    rme_value(state, "/3/echoWidth".into(), e.width as f32 / 100.0);
    rme_value(state, "/3/echoVolume".into(), ((e.volume_db + 65.0) / 65.0).clamp(0.0, 1.0));

    rme_bool(state, "/3/globalMute".into(), any_channel_muted(state));
    rme_bool(state, "/3/globalSolo".into(), any_channel_soloed(state));
    for n in 1..=8 {
        let active = state.active_snapshot == Some(n);
        rme_bool(state, format!("/3/snapshots/{}/1", 9 - n), active);
    }
    for idx in 0..4 {
        let g = state.groups[idx].clone();
        rme_bool(state, format!("/3/muteGroups/{}/1", 4 - idx), g.mute_linked);
        rme_bool(state, format!("/3/soloGroups/{}/1", 4 - idx), g.solo_linked);
        rme_bool(state, format!("/3/faderGroups/{}/1", 4 - idx), g.fader_linked);
    }
}

fn notify_rme_page(state: &mut TotalControl, page: u8) {
    match page {
        1 => notify_rme_page1(state),
        2 => notify_rme_page2(state),
        3 => notify_rme_page3(state),
        _ => {}
    }
}

/// Feedback for a change that arrived as a native TotalControl path.  Only
/// the RME surface is sent; the native address space is not echoed.
fn notify_rme_native(state: &mut TotalControl, addr: &str, _args: &[OscType]) {
    let parts: Vec<&str> = addr.split('/').filter(|s| !s.is_empty()).collect();
    match parts.as_slice() {
        [kind, id, ..] if matches!(*kind, "input" | "playback" | "output") => {
            let Ok(id) = id.parse::<usize>() else {
                return;
            };
            let cid = match *kind {
                "input" => ChannelId::Input(id),
                "playback" => ChannelId::Playback(id),
                _ => ChannelId::Output(id),
            };
            notify_rme_channel(state, cid);
        }
        ["fx", "reverb", ..] | ["fx", "echo", ..] => notify_rme_page3(state),
        ["dim"] => notify_rme_page1(state),
        ["speaker_b"] | ["mono"] | ["mute_fx"] | ["talkback"] => notify_rme_page1(state),
        ["snapshot", "active"] | ["snapshot", _ , "stored"] => notify_rme_page3(state),
        ["submix"] => notify_rme_page1(state),
        _ => {}
    }
}

fn notify_rme_channel(state: &mut TotalControl, cid: ChannelId) {
    if let Some(n) = rme_page1_index(state, cid) {
        rme_page1_channel(state, cid, n);
    }
    if rme_is_selected(state, cid) {
        rme_page2_channel(state, cid);
    }
}

/// Flips `cid`'s collapsed/expanded target and kicks off (or redirects, if
/// one was already in flight — e.g. double-clicking again before it
/// settles) the width animation that plays it out. A no-op if `cid` is
/// already at `target`, so re-applying the same target to a whole
/// selection doesn't restart every strip's animation from scratch.
fn set_collapsed(state: &mut TotalControl, cid: ChannelId, target: bool) {
    if state.collapsed.contains(&cid) == target {
        return;
    }
    let now = Instant::now();
    let current_w = state.collapse_anim.get(&cid).map(|a| a.at(now)).unwrap_or(
        if state.collapsed.contains(&cid) {
            strip::COLLAPSED_W
        } else {
            strip::full_width(cid)
        },
    );
    let target_w = if target {
        strip::COLLAPSED_W
    } else {
        strip::full_width(cid)
    };
    state.collapse_anim.insert(
        cid,
        strip::CollapseAnim {
            prev: current_w,
            value: target_w,
            since: now,
        },
    );
    if target {
        state.collapsed.insert(cid);
    } else {
        state.collapsed.remove(&cid);
    }
}

/// Opens `(cid, kind)` — adds it to the open set without disturbing any
/// OTHER strip's flyout, matching real TotalMix (see `TotalControl::
/// flyout_open`'s own doc comment). Two exceptions, both real rendering
/// constraints rather than a design choice:
/// - `Route` closes any other open `Route` first (its Stack overlay only
///   positions one popover at a time).
/// - Settings/Eq/Trim close any OTHER Settings/Eq/Trim already open on
///   the SAME `cid` first — the inline-push render loop only handles one
///   panel per strip (an `if`/`else if` chain), a second entry for the
///   same strip would just sit in the set unrendered.
fn begin_flyout_close(
    state: &mut TotalControl,
    cid: ChannelId,
    kind: strip::FlyoutKind,
    now: Instant,
) {
    if !state.flyout_open.remove(&(cid, kind)) {
        return;
    }
    let from = state
        .flyout_anim
        .get(&(cid, kind))
        .map(|a| a.at(now))
        .unwrap_or_else(|| flyout_width(state, cid, kind));
    state.flyout_anim.insert(
        (cid, kind),
        strip::CollapseAnim {
            prev: from,
            value: 0.0,
            since: now,
        },
    );
}

fn open_flyout_now(
    state: &mut TotalControl,
    cid: ChannelId,
    kind: strip::FlyoutKind,
    now: Instant,
) {
    state.flyout_open.insert((cid, kind));
    let target = flyout_width(state, cid, kind);
    let from = state
        .flyout_anim
        .get(&(cid, kind))
        .map(|a| a.at(now))
        .unwrap_or(0.0);
    state.flyout_anim.insert(
        (cid, kind),
        strip::CollapseAnim {
            prev: from,
            value: target,
            since: now,
        },
    );
}

/// Opens `(cid, kind)`; returns false when the request was queued until an
/// existing same-strip panel has finished its shrink.
fn open_flyout(
    state: &mut TotalControl,
    cid: ChannelId,
    kind: strip::FlyoutKind,
    now: Instant,
) -> bool {
    if kind == strip::FlyoutKind::Route {
        let old_routes: Vec<_> = state
            .flyout_open
            .iter()
            .filter(|&&(_, k)| k == strip::FlyoutKind::Route)
            .copied()
            .collect();
        for (c, k) in old_routes {
            begin_flyout_close(state, c, k, now);
        }
        open_flyout_now(state, cid, kind, now);
        return true;
    }

    let other_open: Vec<_> = state
        .flyout_open
        .iter()
        .filter(|&&(c, k)| c == cid && k != strip::FlyoutKind::Route)
        .copied()
        .collect();
    // A different inline panel on the same strip morphs the current width
    // straight to the new panel's width.  Do not run the old panel down to
    // zero first: that full slide-out/in is visually disturbing and wastes
    // the animation time when only the difference between the two panel
    // widths is needed.
    let closing_width = state
        .flyout_anim
        .iter()
        .filter(|(&(c, k), _)| c == cid && k != strip::FlyoutKind::Route)
        .map(|(_, a)| a.at(now))
        .fold(0.0f32, f32::max);
    if !other_open.is_empty() || closing_width > 0.0 {
        let from = other_open
            .iter()
            .map(|&(c, k)| {
                state
                    .flyout_anim
                    .get(&(c, k))
                    .map(|a| a.at(now))
                    .unwrap_or_else(|| flyout_width(state, c, k))
            })
            .fold(closing_width, f32::max);
        for (c, k) in other_open {
            state.flyout_open.remove(&(c, k));
        }
        state.flyout_anim.retain(|&(c, k), _| {
            c != cid || k == strip::FlyoutKind::Route
        });
        let target = flyout_width(state, cid, kind);
        state.flyout_open.insert((cid, kind));
        state.flyout_anim.insert(
            (cid, kind),
            strip::CollapseAnim {
                prev: from.max(0.0),
                value: target,
                since: now,
            },
        );
        state.pending_flyout = None;
        return true;
    }

    open_flyout_now(state, cid, kind, now);
    state.pending_flyout = None;
    true
}

/// Closes every currently-open `Route` flyout — the only kind
/// `Message::CloseFlyout` is ever sent for (the click-outside catcher in
/// `with_flyout`, `Route`'s own Stack overlay); Settings/Eq/Trim have no
/// such catcher, they're plain inline layout with their own explicit
/// close (re-pressing the same gear/EQ/T icon).
fn close_route_flyout(state: &mut TotalControl) {
    let now = Instant::now();
    let routes: Vec<_> = state
        .flyout_open
        .iter()
        .filter(|&&(_, k)| k == strip::FlyoutKind::Route)
        .copied()
        .collect();
    for (cid, kind) in routes {
        begin_flyout_close(state, cid, kind, now);
    }
}

/// Cap on `TotalControl::undo_stack`/`redo_stack` — unbounded growth over a
/// long session would otherwise hold one full `Scene` (every channel's
/// full state) per mutating action forever.
const UNDO_STACK_CAP: usize = 50;

/// Whether `message` should push a pre-action snapshot onto
/// `undo_stack` before being handled — a blacklist of messages that
/// either don't touch device state at all (view/selection/animation/
/// text-in-progress UI) or would flood the stack with one entry per
/// event if included (`VolumeChanged` fires continuously during a fader
/// drag; the drag's *start*, `FaderPressed`, is what actually captures
/// the "before" state — deliberately not blacklisted). Defaults to
/// "capture" for anything not explicitly listed, so a future `Message`
/// variant that *does* mutate device state is undoable by default
/// rather than silently missed.
///
/// Known, accepted gap: knob-driven values (`PanChanged`, `Gain`,
/// `TrimChanged`, the EQ band params) have no distinct "drag start"
/// message the way faders do, so a knob drag pushes one snapshot per
/// tick rather than one per gesture — coarser undo than fader moves,
/// not worth a `Knob` widget API change in this pass (see the sidebar
/// implementation plan).
///
/// Also known: `undo_stack`/`redo_stack` hold `Scene`s, which only
/// capture *device* state (`RmeDevice::capture_scene`) — sidebar-only
/// concepts (group membership, which layout/snapshot slot is selected,
/// panel collapse state) aren't part of a `Scene` at all, so those
/// messages are excluded below not just to avoid noise but because
/// capturing around them would be a no-op anyway.
fn is_undoable(message: &Message) -> bool {
    !matches!(
        message,
        Message::Tick
            | Message::ResetTransportErrors
            | Message::DeviceUpdated
            | Message::MeterWake
            | Message::SetView(_)
            | Message::ToggleMatrixMono
            | Message::QuickChannelSelected(_)
            | Message::SelectOutput(_)
            | Message::ModifiersChanged(_)
            | Message::TabPressed
            | Message::EscapePressed
            | Message::WindowResized(_)
            | Message::WindowSize(..)
            | Message::WheelZoom(_)
            | Message::ZoomIn
            | Message::ZoomOut
            | Message::ZoomReset
            | Message::MonitorSize(_)
            | Message::CloseRequested
            | Message::WindowClosed(_)
            | Message::Tray(_)
            | Message::SetKeepInTray(_)
            | Message::SetAutostart(_)
            | Message::MismatchUseTotalcontrol
            | Message::MismatchUseDevice
            | Message::MismatchAlwaysTotalcontrol
            | Message::SetAlwaysInitDevice(_)
            | Message::RangeCleared(_)
            | Message::ToggleFxPanel
            | Message::ToggleAssign
            | Message::AssignDestinations(_)
            | Message::AssignOutput(..)
            | Message::AssignCueTarget(_)
            | Message::ToggleCollapse(_)
            | Message::ToggleFlyout(..)
            | Message::CloseFlyout
            | Message::CollapseTick
            | Message::SaveNow
            | Message::EditStart(..)
            | Message::EditChanged(_)
            | Message::StripClicked(_)
            | Message::ClearSelection
            | Message::StripHovered(_)
            | Message::OscReady(_)

            | Message::OscLog(_)
            | Message::ToggleOscLog
            | Message::ClearOscLog
            | Message::ToggleDevicePanel
            | Message::ToggleAbout
            | Message::ToggleSettings
            | Message::ToggleOptions
            | Message::OptionsReset
            | Message::OptionsBack
            | Message::OptionsSettings
            | Message::CloseOptions
            | Message::SetSurround51(_)
            | Message::SetSurround71(_)
            | Message::LineInRefSelected(_)
            | Message::LineInRef58Selected(_)
            | Message::LineOutRefSelected(_)
            | Message::PhonesRefSelected(_)
            | Message::SpdifProfessionalChanged(_)
            | Message::WordClockSingleSpeedChanged(_)
            | Message::WordClockTerminationChanged(_)
            | Message::OpenRepoUrl
            | Message::Undo
            | Message::Redo
            | Message::ToggleSidebarPanel(_)
            | Message::ToggleSidebar
            | Message::SidebarResize(_)
            | Message::SetRowMode(_)
            | Message::DimToggle
            | Message::Dim(_)
            | Message::TalkbackChanged(_)
            | Message::OpenControlRoomSettings
            | Message::CancelControlRoomSettings
            | Message::ApplyControlRoomSettings
            | Message::ResetControlRoomSettings
            | Message::ControlRoomTalkbackInput(_)
            | Message::ControlRoomTalkbackDim(_)
            | Message::ControlRoomListenbackInput(_)
            | Message::ControlRoomListenbackDim(_)
            | Message::ControlRoomRecallDb(_)
            | Message::ControlRoomMainDim(_)
            | Message::ControlRoomSpeakerBLinked(_)
            | Message::ControlRoomExternalInput(_)
            | Message::ControlRoomExternalGain(_)
            | Message::ControlRoomPflReplacesSolo(_)
            | Message::ControlRoomExclusiveSoloPfl(_)
            | Message::MatrixHover(..)
            | Message::PointerPressed
            | Message::FieldFocus(..)
            | Message::EngineRate(_)
            | Message::EngineQuantum(_)
            | Message::EnginePoll
            | Message::EngineRead(..)
            | Message::RenameStart(_)
            | Message::RenameChanged(_)
            | Message::RenameCommit
            | Message::SetShowOriginalNames(_)
            | Message::SnapshotStore
            | Message::GroupEditToggle
            | Message::GroupLinkToggle(..)
            | Message::GroupClear
            | Message::LayoutClicked(_)
            | Message::LayoutStore
    )
}

/// Copy the non-undoable device group from the current saved scene into a
/// scene that is about to be applied.  Undo/redo scenes are captured around
/// mixer edits, so they can predate a later device-settings change; applying
/// them must not roll that change back in the model without writing it.
fn keep_device_settings(scene: &mut Scene, current: &Scene) {
    let d = &current.settings;
    let s = &mut scene.settings;
    s.clock_source = d.clock_source.clone();
    s.clock_sources = d.clock_sources.clone();
    s.optical_out_spdif = d.optical_out_spdif;
    s.spdif_professional = d.spdif_professional;
    s.word_clock_single_speed = d.word_clock_single_speed;
    s.word_clock_termination = d.word_clock_termination;
    s.eq_for_record = d.eq_for_record;
    s.line_in_ref = d.line_in_ref;
    s.line_in_ref_58 = d.line_in_ref_58;
    s.line_out_ref = d.line_out_ref;
    s.phones_ref = d.phones_ref;
    s.sample_rate = d.sample_rate;
}

/// The scene's input gains for "Use TotalControl", migrating the old UCX 3/4
/// whole-dB scale (`gain_max = 12`) to raw half-dB units.
fn scene_gain_pairs(scene: &Scene, ucx: bool) -> Vec<(usize, u32)> {
    scene
        .inputs
        .iter()
        .enumerate()
        .filter_map(|(idx, input)| {
            input.gain.map(|gain| {
                let gain = if ucx && input.gain_max == Some(12) {
                    (gain * 2).min(24)
                } else {
                    gain
                };
                (idx, gain)
            })
        })
        .collect()
}

/// "Use TotalControl settings" on the startup mismatch: write the saved scene's
/// readable values.  The connect write keeps the device's readable values,
/// so the scene has to be pushed for the fields the dialog compared.
fn apply_totalcontrol_mismatch(state: &mut TotalControl, mismatch: &StartupMismatch) {
    if state.device.is_ucx_model() {
        for &(idx, gain) in &mismatch.totalcontrol_gain {
            if let Err(e) = state.device.set_gain(idx, gain) {
                fail(state, "use TotalControl gain", &e);
            }
        }
    } else {
        let scene = &mismatch.totalcontrol_scene;
        if let Err(e) = state.device.set_clock_source(&scene.settings.clock_source) {
            fail(state, "set_clock_source", &e);
        }
        if let Err(e) = state
            .device
            .set_optical_out_format(scene.settings.optical_out_spdif)
        {
            fail(state, "set_optical_out_format", &e);
        }
        if let Err(e) = state
            .device
            .set_spdif_professional(scene.settings.spdif_professional)
        {
            fail(state, "set_spdif_professional", &e);
        }
        for i in 0..2 {
            if let Some(input) = scene.inputs.get(i) {
                if let Err(e) = state.device.set_phantom(i, input.phantom) {
                    fail(state, "set_phantom", &e);
                }
            }
        }
        for i in 2..4 {
            if let Some(input) = scene.inputs.get(i) {
                if let Err(e) = state.device.set_pad(i, input.pad) {
                    fail(state, "set_pad", &e);
                }
                if let Err(e) = state.device.set_instrument(i, input.instrument) {
                    fail(state, "set_instrument", &e);
                }
            }
        }
        for &(idx, gain) in &mismatch.totalcontrol_gain {
            if let Err(e) = state.device.set_gain(idx, gain) {
                fail(state, "use TotalControl gain", &e);
            }
        }
    }
    state.host_scene = Some(state.device.capture_scene());
}

/// Whether `message` changes the mix away from the recalled snapshot:
/// everything that would push an undo step, plus the steps of a fader
/// drag (`VolumeChanged`, kept off the undo stack, see `is_undoable`) and
/// undo/redo themselves. Recalling a snapshot is the one undoable message
/// that does the opposite, so it is left out; its handler clears the flag.
fn changes_mix(message: &Message) -> bool {
    match message {
        Message::SnapshotClicked(_) => false,
        Message::VolumeChanged(..) | Message::MatrixCellChanged(..) | Message::Undo | Message::Redo => true,
        // Device-owned and host-mode settings that live in the saved scene
        // but are not undo steps.  No UCX options-word readback is exposed,
        // so its values reach the scene only through this merge.
        Message::ClockSourceSelected(_)
        | Message::MsProcChanged(_)
        | Message::An12Changed(_)
        | Message::InputLinkChanged(_)
        | Message::EqForRecordChanged(_)
        | Message::OpticalOutFormatChanged(_)
        | Message::LineInRefSelected(_)
        | Message::LineInRef58Selected(_)
        | Message::LineOutRefSelected(_)
        | Message::PhonesRefSelected(_)
        | Message::SpdifProfessionalChanged(_)
        | Message::WordClockSingleSpeedChanged(_)
        | Message::WordClockTerminationChanged(_) => true,
        other => is_undoable(other),
    }
}

/// Whether `message` can only do something on a connection that speaks
/// RME's own protocol: the setters behind `require_proprietary` in the ALSA
/// backend (`babyface.rs`), plus `set_eq_for_record`, which that backend
/// does not implement at all. Everything else in the app (routing, gains,
/// preamps, clock, meters, scenes, the sidebar's own state) works in either
/// personality, so this is deliberately not a general "may the UI do this"
/// list. The renderers hide the controls these carry, and `update` drops the
/// messages, so a stale key binding or an OSC message cannot slip past a
/// hidden button.
fn needs_protocol_features(message: &Message) -> bool {
    matches!(
        message,
        Message::LevelChanged(..)
            | Message::Autoset(..)
            | Message::DynamicsEnabled(..)
            | Message::DynamicsChanged(..)
            | Message::SpeakerB(..)
            | Message::TalkbackChanged(_)
            | Message::ReverbChanged(..)
            | Message::EchoChanged(..)
            | Message::FxReturnChanged(..)
            | Message::FxSendChanged(..)
            | Message::ResetMix(_)
            | Message::PhaseChanged(..)
            | Message::OutputPhaseChanged(..)
            | Message::NoTrimChanged(..)
            | Message::PitchChanged(_)
            | Message::WidthChanged(_)
            | Message::ChannelWidthChanged(..)
            | Message::MsProcChanged(_)
            | Message::InputMsProcChanged(..)
            | Message::PlaybackMsProcChanged(..)
            | Message::An12Changed(_)
            | Message::InputLinkChanged(_)
            | Message::EqForRecordChanged(_)
            | Message::DimToggle
            | Message::EqEnabled(..)
            | Message::EqBandType(..)
            | Message::EqBandFreq(..)
            | Message::EqBandQ(..)
            | Message::EqBandGain(..)
            | Message::EqBandPoint(..)
            | Message::EqBandReset(..)
            | Message::EqLowCutEnabled(..)
            | Message::EqLowCutFreq(..)
            | Message::EqLowCutSlope(..)
    )
}

/// Select a strip the way a plain click does.  The strip's own controls
/// (mute, solo, the flyout buttons, the fader, the knobs) call this too, so
/// a click anywhere on a channel selects it; on a hardware output it also
/// shows that output's submix.  A strip that is already part of a
/// multi-selection keeps the selection, so group edits still work.
fn select_strip(state: &mut TotalControl, cid: ChannelId) {
    state.focus_channel = Some(cid);
    if !state.selected.contains(&cid) {
        state.selected.clear();
        state.selected.insert(cid);
    }
    state.select_anchor = Some(cid);
    if let ChannelId::Output(i) = cid {
        state.sel_out = if state.device.outputs_one_per_pair() {
            i
        } else {
            i / 2
        };
    }
}

pub fn update(state: &mut TotalControl, message: Message) -> Task<Message> {
    // A host change whose confirming snapshot never arrived (a rejected
    // command, or a stopped service) must not capture a later snapshot.
    if state
        .pending_host_merge
        .as_ref()
        .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(1))
    {
        state.pending_host_merge = None;
    }
    // A Control Room pair can never be unlinked; correct the model before
    // the message below acts on it.
    enforce_control_room_stereo(state);
    // The Assign cards close on an explicit close or an actual click
    // elsewhere.  Hover messages, the periodic engine poll and the collapse
    // animation tick must not close them: moving from the assign button to
    // the card crosses the strip row and fires `StripHovered`, which used to
    // collapse the menu before the pointer reached it.  `PointerPressed` is
    // published for every left-button press by the global subscription, so it
    // arrives for presses on the menu itself too; closing on it would destroy
    // the pressed button before its release could send the selection.
    if !matches!(state.assign_menu, AssignMenu::Closed)
        && !matches!(
            message,
            Message::ToggleAssign
                | Message::AssignDestinations(_)
                | Message::AssignHover(_)
                | Message::AssignOutput(..)
                | Message::AssignCueTarget(_)
                | Message::PointerPressed
                | Message::Tick
                | Message::DeviceUpdated
                | Message::MeterWake
                | Message::ModifiersChanged(_)
                | Message::StripHovered(_)
                | Message::CollapseTick
                | Message::EnginePoll
                | Message::EngineRead(..)
                | Message::OscLog(_)
                | Message::OscRmeIn(_, _)
                | Message::OscGlobalIn(_)
                | Message::WindowResized(_)
                | Message::WindowSize(..)
        )
    {
        state.assign_menu = AssignMenu::Closed;
        state.assign_pinned = false;
    }

    // Classic RME OSC: parse on this thread so `Toggle` can read the
    // confirmed model, dispatch the resulting ordinary messages, then
    // re-send the page when the bank/bus/channel selection changed.
    if let Message::OscRmeIn(addr, args) = &message {
        let before = state.rme.nav();
        state.rme.gain_max_34 = state
            .device
            .inputs()
            .get(2)
            .and_then(|c| c.gain_max)
            .unwrap_or(24);
        state.rme.submix_count = state.device.output_pair_count().max(1);
        state.rme.counts = [
            state.device.inputs().len().min(crate::osc_rme::CHANNELS),
            state.device.playbacks().len().min(crate::osc_rme::CHANNELS),
            state.device.outputs().len().min(crate::osc_rme::CHANNELS),
        ];
        let parsed = crate::osc_rme::parse(addr, args, &mut state.rme);
        let page = state.rme.page;
        let changed = state.rme.nav() != before;
        match parsed {
            Some(messages) => {
                for message in messages {
                    let _ = update(state, message);
                }
            }
            None => log::debug!("OSC: unsupported classic path {addr} {args:?}"),
        }
        if changed {
            notify_rme_page(state, page);
        }
        return Task::none();
    }
    // Global OSC: absolute paths map straight to the ordinary mixer
    // messages, then the applied value is echoed in the same dialect.  The
    // classic feedback builder is suppressed in Global mode by `send_osc`.
    if let Message::OscGlobalIn(command) = &message {
        if let crate::osc_global::GlobalCommand::Send(control) = command {
            send_global_state(state, control);
            return Task::none();
        }
        let messages = global_messages(state, command);
        if messages.is_empty() {
            log::debug!("OSC global: no mapping for {command:?}");
            return Task::none();
        }
        let echo = !matches!(
            command,
            crate::osc_global::GlobalCommand::Send(_)
                | crate::osc_global::GlobalCommand::Level { .. }
                | crate::osc_global::GlobalCommand::Status(_)
        ) && !global_feedback_is_typed(command);
        for message in messages {
            let _ = update(state, message);
        }
        if echo {
            notify_osc(state, OscOutbound::Global(command.clone()));
        }
        return Task::none();
    }
    // UC startup: the first scene write happens before the meter taps open
    // the card, and the unit can reload its stored mixer on stream start.
    // Reapply the (possibly user-edited) model once the card has settled.
    if let Some(due) = state.startup_scene_due {
        if Instant::now() >= due {
            state.startup_scene_due = None;
            // Reapply the scene that was loaded from disk, not the current
            // model.  The UC may have delivered a stale front-panel event
            // after the first write; capturing the model here would make
            // the device value permanent.
            if let Some(scene) = state.startup_scene.take() {
                if let Err(e) = state.device.apply_scene(&scene) {
                    log::warn!("scene reapply failed: {e:?}");
                } else {
                    log::info!("reapplied the Fireface mixer scene");
                }
            }
        }
    }
    // Anything but the background polls is the user doing something, or
    // something the user must see: keep the tick fast for a while.
    if !matches!(
        message,
        Message::Tick | Message::EnginePoll | Message::EngineRead(..)
    ) {
        state.last_activity = Instant::now();
    }
    // The class-compliant personality drives routing, preamps and clock and
    // nothing else (see `RmeDevice::protocol_features`). The UI already
    // hides the controls; this is the other half of that, so nothing built
    // by hand reaches a setter that can only fail.
    if needs_protocol_features(&message) && !state.device.protocol_features() {
        // Phase is the one exception: the UC has no EQ/FX DSP, so the
        // coarse protocol_features flag is false there, but it does store
        // phase in the matrix sign bits.
        let phase_allowed =
            matches!(&message, Message::PhaseChanged(..)) && state.device.phase_supported();
        if !phase_allowed {
            log::debug!("ignored {message:?}: this connection has no RME-protocol features");
            return Task::none();
        }
    }
    let mix_changed = changes_mix(&message);
    if mix_changed {
        state.snapshot_dirty = true;
    }
    // Capture the live model before a host-originated mix change.  At the
    // end of this update only the fields that actually changed are merged
    // into the saved host scene; an earlier front-panel change is present
    // in both captures and is therefore not persisted.
    let host_before = mix_changed.then(|| state.device.capture_scene());
    if changes_prefs(&message) {
        state.prefs_dirty = true;
    }
    if is_undoable(&message) {
        // A continuous edit opens one undo step for the whole gesture.
        let now = Instant::now();
        let gesture = undo_gesture(&message);
        let continues = matches!(
            (gesture, state.undo_gesture),
            (Some(g), Some((last, at))) if g == last && now.duration_since(at) < UNDO_COALESCE
        );
        state.undo_gesture = gesture.map(|g| (g, now));
        if !continues {
            let scene = state
                .host_scene
                .clone()
                .unwrap_or_else(|| state.device.capture_scene());
            state.undo_stack.push(scene);
            if state.undo_stack.len() > UNDO_STACK_CAP {
                state.undo_stack.remove(0);
            }
            state.redo_stack.clear();
        }
    }
    match message {
        Message::ToggleFullscreen(id) => {
            state.fullscreen = !state.fullscreen;
            let mode = if state.fullscreen {
                window::Mode::Fullscreen
            } else {
                window::Mode::Windowed
            };
            return iced::window::set_mode(id, mode);
        }
        Message::DeviceUpdated => {
            if let (Some(rx), DeviceHandle::Mirror(m)) =
                (&mut state.device_updates, &mut state.device)
            {
                while let Ok(snapshot) = rx.try_recv() {
                    m.update(snapshot);
                }
            }
            // The mirror now holds the confirmed values.  Merge the host
            // change that was waiting for them; keep waiting if this was a
            // snapshot published before the command reached the service.
            if let Some((before, at)) = state.pending_host_merge.take() {
                let after = state.device.capture_scene();
                if after != before {
                    merge_host_changes(state, before);
                } else if at.elapsed() < Duration::from_secs(1) {
                    state.pending_host_merge = Some((before, at));
                }
            }
        }
        // No model change: the message itself produces the redraw in which
        // the canvases read the now-active snapshot and schedule a frame.
        Message::MeterWake => {}
        Message::Tick => {
            // Driver transport health: cumulative counters, shown relative
            // to the first reading of this connection (or the reset button).
            let counters = state.device.transport_counters();
            if counters.is_some() && state.transport_baseline.is_none() {
                state.transport_baseline = counters;
            }
            if counters.is_none() {
                state.transport_baseline = None;
            }
            state.transport_counters = counters;
            if state.notice.as_ref().is_some_and(|(_, at)| at.elapsed() > NOTICE_FOR) {
                state.notice = None;
            }
            if state.tray_active {
                crate::tray::set_status(crate::tray::Status {
                    window_visible: !state.window_hidden,
                    dim: state.device.settings().dim,
                });
            }
            flush_prefs(state);
            // Resize coalescing: if the last `Resized` of a drag landed
            // inside the throttle window, flush it here so the width always
            // settles (even if no further `Resized` arrives). 50 ms is well
            // past `RESIZE_THROTTLE`, so this only ever catches the tail end
            // of a drag.
            apply_pending_resize(state);
            // Connection probe, throttled: the handle keeps answering
            // after the card is unplugged (and after a personality switch
            // re-enumerates it), so this is the only thing that can tell
            // the chip the truth. When the card is back: reopen through
            // the same path the app started with, and re-apply the
            // auto-saved scene exactly like `new` does, so the UI and the
            // hardware agree again.
            let conn_due = state
                .last_conn_check
                .map_or(true, |t| t.elapsed() >= Duration::from_millis(CONN_CHECK_MS));
            if conn_due {
                state.last_conn_check = Some(Instant::now());
                state.connected = state.device.is_connected();
                if !state.connected {
                    state.show_device_panel = false;
                    clear_meters(state);
                }
                // A fallback mock (no device found at launch) must keep
                // probing for the real card.  Its `is_connected()` is true,
                // so the old `!connected` gate would leave the Babyface mock
                // on screen even after the UC appeared.
                let fallback_mock = state.device.is_mock() && !state.explicit_mock;
                if (!state.connected || fallback_mock) && !state.explicit_mock {
                    // A running simulation must give way to a real unit that
                    // appeared; only then fall back to the stored target.
                    let fresh = if fallback_mock {
                        DeviceHandle::open_real(state.backend.as_deref())
                            .or_else(|| {
                                state
                                    .device_target
                                    .as_ref()
                                    .and_then(DeviceHandle::open_target)
                            })
                    } else {
                        state
                            .device_target
                            .as_ref()
                            .and_then(DeviceHandle::open_target)
                            .or_else(|| DeviceHandle::open_real(state.backend.as_deref()))
                    };
                    if let Some(fresh) = fresh {
                        log::info!("Device connected: {}", fresh.target().label());
                        state.prefs_dirty = true;
                        replace_device_handle(state, fresh);
                        clear_meters(state);
                        state.device_target = Some(state.device.target());
                        state.connected = state.device.is_connected();
                        state.show_device_panel = false;
                        if state.device.needs_scene_restore()
                            || state.sync_direction == SyncDirection::GuiToDevice
                        {
                            let scene = load_scene_for_current(state);
                            if let Some(scene) = scene {
                                state.host_scene = Some(scene.clone());
                                if let Err(e) = state.device.apply_scene(&scene) {
                                    fail(state, "apply_scene", &e);
                                } else {
                                    enforce_control_room_stereo(state);
                                    state.host_scene = Some(state.device.capture_scene());
                                }
                                if state.device.is_uc_model() {
                                    // The reconnect scene is written while
                                    // the new stream may still be closed.
                                    // Reapply it once the stream and the
                                    // UC's reloaded mixer have settled.
                                    state.startup_scene = Some(scene);
                                    state.startup_scene_due =
                                        Some(Instant::now() + Duration::from_secs(1));
                                    state
                                        .device
                                        .suppress_front_panel_for(Duration::from_secs(2));
                                }
                            }
                        }
                        if state.host_scene.is_none() {
                            state.host_scene = Some(state.device.capture_scene());
                        }
                        if let Err(e) = apply_control_room_assignments(state) {
                            log::debug!("Control Room assignments not restored: {e}");
                        }
                        restart_surround_bridge(state);
                        restore_engine(state);
                        // `fresh` starts with the deferred meter tap (the
                        // global TOTALCONTROL_DEFER_METER_TAP), exactly like the
                        // first launch.  Start it now that the scene and the
                        // remembered rate are in place; otherwise the input
                        // meters stay dead after a reconnect.
                        state.device.begin_metering();
                        // Force the next poll to treat the reopened card as
                        // a fresh stream: schedule the post-start reapply
                        // even if the previous state already said RUNNING.
                        state.last_sink_running = false;
                        state.last_card_rate = None;
                    }
                }
            }
            let before = panel_fingerprint(state);
            let _ = state.device.poll_events();
            poll_startup_compare(state);
            if panel_fingerprint(state) != before {
                // The front panel moved a level, a mute or the dim: show
                // it at the fast rate while it keeps moving.
                state.last_activity = Instant::now();
            }
            // The physical UCX encoder changes the IF1-derived Phones
            // hardware volume.  Detect that at the 100 ms idle poll and
            // switch to the 33 ms tick only while it is moving; polling it
            // at 30 Hz forever costs ~45% of a core in iced redraws.
            let hw = state.device.hardware_volume();
            if hw != state.last_hw_volume {
                state.last_hw_volume = hw;
                state.last_activity = Instant::now();
            }
            // Follow the front panel's OUT selection (TotalMix
            // highlights the panel's current submix) — only when it
            // CHANGES on the panel, so a GUI click on the submix picker
            // keeps winning until the user touches OUT. Panel OUT
            // selection (0=Ch1/2, 1=Phones, 2=Opt) → submix pair index
            // (OUT_LABELS: AN1/2=0, PH3/4=1, … A7/A8=5).
            if let Some((_, _, out_sel)) = state.device.panel_selection() {
                let out = match out_sel {
                    1 => 1,
                    2 => 5, // Opt = the optical output (A7/A8)
                    _ => 0,
                };
                if state.last_panel_out != Some(out_sel) {
                    state.last_panel_out = Some(out_sel);
                    state.sel_out = state.device.display_pair_index(out);
                }
            }
            let input_trims: Vec<f32> =
                state.device.inputs().iter().map(|c| c.trim).collect();
            let raw_input = if state.connected {
                state
                    .device
                    .input_peaks()
                    .unwrap_or_else(|| state.device.input_meters())
            } else {
                vec![0.0; state.device.inputs().len()]
            };
            let in_levels = trim_meter_levels(&raw_input, &input_trims);
            for (i, m) in state.input_meters.iter_mut().enumerate() {
                m.step_elapsed(in_levels.get(i).copied().unwrap_or(0.0));
            }
            // UCX: use the device's own 0x85 meter words for inputs and
            // playback.  Hardware-output post-fader meters come from the
            // device too when it supplies them; otherwise fall back to the
            // host-side matrix sum used by the ALSA/mock backends.
            let (pb_levels, device_output) = if state.connected {
                if let Some(pb) = state.device.device_playback_peaks() {
                    state.playback_tap_channels = state.device.playbacks().len();
                    (pb, state.device.device_output_peaks())
                } else if let Some(pb) = state.device.device_playback_meters() {
                    state.playback_tap_channels = state.device.playbacks().len();
                    (pb, state.device.device_output_meters())
                } else {
                    let (tap_levels, tap_channels) = state.playback_tap.drain();
                    state.playback_tap_channels = tap_channels;
                    let pb = if state.device.is_mock() {
                        state.device.playback_meters()
                    } else {
                        tap_levels.to_vec()
                    };
                    (pb, state.device.device_output_meters())
                }
            } else {
                (
                    vec![0.0; state.device.playbacks().len()],
                    Some(vec![0.0; state.device.outputs().len()]),
                )
            };
            let playback_trims: Vec<f32> =
                state.device.playbacks().iter().map(|c| c.trim).collect();
            let pb_levels = trim_meter_levels(&pb_levels, &playback_trims);
            let mut out_levels = device_output
                .unwrap_or_else(|| state.device.output_meters_from(&in_levels, &pb_levels));
            // A muted output must read zero even if a backend's post-fader
            // packet is taken before its mute stage.
            for (i, ch) in state.device.outputs().iter().enumerate() {
                if ch.mute {
                    if let Some(level) = out_levels.get_mut(i) {
                        *level = 0.0;
                    }
                }
            }
            for (i, m) in state.playback_meters.iter_mut().enumerate() {
                m.step_elapsed(pb_levels.get(i).copied().unwrap_or(0.0));
            }
            for (i, m) in state.output_meters.iter_mut().enumerate() {
                m.step_elapsed(out_levels.get(i).copied().unwrap_or(0.0));
            }
            let fx_levels = if state.connected {
                state
                    .device
                    .fx_peaks()
                    .or_else(|| state.device.fx_meters())
                    .unwrap_or([0.0, 0.0])
            } else {
                [0.0, 0.0]
            };
            for (i, m) in state.fx_meters.iter_mut().enumerate() {
                m.step_elapsed(fx_levels[i]);
            }
            state.fx_overload = state.device.fx_overload().unwrap_or([false, false]);
            // Meter motion does not need the whole view rebuilt: the
            // fader canvases interpolate between these keyframes and ask
            // for their own redraws (see `widgets::fader`).  The idle tick
            // therefore keeps polling levels while the UI stays at rest.
            // Persist the mixer state (debounced 3 s) for the backend that
            // has no gain/volume readback (USB); this is its restore source
            // for the next open (loaded in `new()`). ALSA backends read the
            // live matrix and skip this path entirely. Shared with the TUI.
            if state.last_auto_save.elapsed() >= Duration::from_secs(3)
                && state.startup_scene.is_none()
            {
                state.last_auto_save = Instant::now();
                // The mock has no hardware and its state would pollute
                // the SHARED auto.json with a "(mock)" model (making the
                // real device reject it on load) — never persist it.
                if state.device.is_mock() {
                    state.last_saved_json = None;
                    return Task::none();
                }
                // USB always needs the shared auto scene.  An ALSA/UCX
                // session in GUI -> device mode also saves its state so the
                // next launch can apply it; Device -> GUI never writes the
                // stale scene over the live device.
                let auto_save = state.device.needs_scene_restore()
                    || state.sync_direction == SyncDirection::GuiToDevice;
                if auto_save {
                    if uses_shared_auto_scene(state) {
                        // GUI/TUI sync: if the OTHER UI wrote auto.json since
                        // our last save, re-apply its state first so we don't
                        // clobber it with our own (possibly stale) copy.
                        if let Some(their) = totalcontrol_core::scene::auto_scene_written_by_other(
                            state.last_saved_json.as_deref(),
                        ) {
                            state.host_scene = Some(their.clone());
                            if let Err(e) = state.device.apply_scene(&their) {
                                fail(state, "apply_scene", &e);
                            }
                        }
                    }
                    let scene = state
                        .host_scene
                        .clone()
                        .unwrap_or_else(|| state.device.capture_scene());
                    if let Ok(json) = scene.to_json() {
                        if state.last_saved_json.as_deref() != Some(json.as_str()) {
                            let saved = save_scene_for_current(state, &scene);
                            if saved.is_ok() {
                                state.last_saved_json = Some(json);
                            }
                        }
                    }
                }
            }
            if state.osc_tx.is_some() {
                notify_rme_meters(state);
            }
        }
        Message::WindowClosed(id) => {
            // Our own hide already forgot the id; this is a close we did not
            // ask for. With a tray the app lives on hidden, without one a
            // daemon with no window would run unseen: quit.
            if state.window_id == Some(id) {
                state.window_id = None;
                if state.tray_active && state.keep_in_tray {
                    state.window_hidden = true;
                } else {
                    let saved = update(state, Message::SaveNow);
                    stop_surround_bridge(state);
                    return Task::batch([saved, iced::exit()]);
                }
            }
        }
        Message::CloseRequested => {
            let saved = update(state, Message::SaveNow);
            if state.tray_active && state.keep_in_tray {
                // Keep running: the DIM button and the saved mix's restore
                // after a replug are this app's job (see `tray`).
                return Task::batch([saved, hide_window(state)]);
            }
            stop_surround_bridge(state);
            return Task::batch([saved, iced::exit()]);
        }
        Message::Tray(event) => match event {
            crate::tray::TrayEvent::Show => return show_window(state),
            crate::tray::TrayEvent::Toggle => {
                return if state.window_hidden {
                    show_window(state)
                } else {
                    hide_window(state)
                };
            }
            crate::tray::TrayEvent::Settings => {
                state.show_settings = true;
                state.show_device_panel = false;
                return show_window(state);
            }
            crate::tray::TrayEvent::Dim => return update(state, Message::DimToggle),
            crate::tray::TrayEvent::Snapshot(n) => {
                return update(state, Message::SnapshotClicked(n))
            }
            crate::tray::TrayEvent::Quit => {
                let saved = update(state, Message::SaveNow);
                stop_surround_bridge(state);
                return Task::batch([saved, iced::exit()]);
            }
        },
        Message::SetKeepInTray(on) => state.keep_in_tray = on,
        Message::SetAutostart(on) => match crate::tray::set_autostart(on) {
            Ok(()) => state.autostart = crate::tray::autostart_enabled(),
            Err(e) => log::warn!("Could not change the autostart entry: {e}"),
        },
        Message::MismatchUseTotalcontrol => {
            if let Some(mismatch) = state.startup_mismatch.take() {
                if mismatch.ucx_dump.is_some() {
                    apply_startup_scene(state, &mismatch.totalcontrol_scene);
                } else {
                    apply_totalcontrol_mismatch(state, &mismatch);
                }
            }
            state.sync_direction = SyncDirection::GuiToDevice;
        }
        Message::MismatchUseDevice => {
            if let Some(mismatch) = state.startup_mismatch.take() {
                if let Some(scene) = mismatch.device_scene.as_ref() {
                    apply_startup_scene(state, scene);
                } else {
                    if let Err(e) = state.device.restore_readable(&mismatch.device_state) {
                        fail(state, "restore device settings", &e);
                    }
                    state.host_scene = Some(state.device.capture_scene());
                }
            }
            state.sync_direction = SyncDirection::GuiToDevice;
        }
        Message::MismatchAlwaysTotalcontrol => {
            if let Some(mismatch) = state.startup_mismatch.take() {
                if mismatch.ucx_dump.is_some() {
                    apply_startup_scene(state, &mismatch.totalcontrol_scene);
                } else {
                    apply_totalcontrol_mismatch(state, &mismatch);
                }
            }
            state.always_init_device = true;
            state.prefs_dirty = true;
            state.sync_direction = SyncDirection::GuiToDevice;
        }
        Message::SetAlwaysInitDevice(on) => {
            state.always_init_device = on;
            state.prefs_dirty = true;
        }
        // Immediate save (window close) — the debounce above would lose
        // the last few seconds of changes. Sync with the other UI first
        // (see the 3 s auto-save in `Message::Tick`). The mock never
        // persists (same reasoning as the auto-save).
        Message::SaveNow => {
            // Talkback is a performance state; never leave the unit talking
            // back after TotalControl closes.
            if state.talkback_on {
                let _ = state.device.set_talkback_with(false, &talkback_config(state));
                state.talkback_on = false;
            }
            // Closing writes the window size, which no resize step does.
            state.prefs_dirty = true;
            flush_prefs(state);
            if !state.device.is_mock() && state.device.needs_scene_restore() {
                if uses_shared_auto_scene(state) {
                    if let Some(their) = totalcontrol_core::scene::auto_scene_written_by_other(
                        state.last_saved_json.as_deref(),
                    ) {
                        state.host_scene = Some(their.clone());
                        if let Err(e) = state.device.apply_scene(&their) {
                            fail(state, "apply_scene", &e);
                        }
                    }
                }
                // If the startup reapply is still pending, save the host
                // scene that was loaded.  Otherwise save the host-owned
                // scene, never the live model (front-panel changes must not
                // become part of the saved mix).
                let scene = state
                    .startup_scene
                    .clone()
                    .or_else(|| state.host_scene.clone())
                    .unwrap_or_else(|| state.device.capture_scene());
                if let Ok(json) = scene.to_json() {
                    let saved = save_scene_for_current(state, &scene);
                    if saved.is_ok() {
                        state.last_saved_json = Some(json);
                    }
                }
            }
        }
        Message::TabPressed => {
            // Cycles all three views — previously skipped Quick (Matrix and
            // Mixer only), which read as broken since the tab bar itself
            // shows three tabs, not two.
            state.view = match state.view {
                View::Quick => View::Mixer,
                View::Mixer => View::Matrix,
                View::Matrix => View::Quick,
            };
        }
        Message::SetView(v) => {
            state.view = v;
            state.matrix_hover = None;
        }
        Message::SelectDevice(target) => {
            if let Err(e) = switch_device(state, target) {
                fail(state, "switch device", &e);
            }
        }
        Message::WheelZoom(y) => {
            // Only zoom on Ctrl+wheel; plain wheel keeps scrolling the
            // hovered scrollable (which iced handles on its own).
            if state.modifiers.contains(keyboard::Modifiers::CTRL) {
                zoom(state, if y > 0.0 { ZOOM_STEP } else { -ZOOM_STEP });
            }
        }
        Message::ZoomIn => zoom(state, ZOOM_STEP),
        Message::ZoomOut => zoom(state, -ZOOM_STEP),
        Message::ZoomReset => state.ui_scale = theme::SCALE_DEFAULT,
        Message::MonitorSize(size) => {
            if std::mem::take(&mut state.initial_zoom_pending) {
                if let Some(size) = size {
                    state.ui_scale = initial_zoom_for(size.height);
                    state.prefs_dirty = true;
                }
            }
        }
        Message::QuickChannelSelected(cid) => state.quick_channel = cid,
        Message::SelectOutput(i) => {
            // A click or OSC message can outlive the device it came from
            // (an open submix list, a delayed controller update). Keep the
            // remembered submix inside the current unit's output pairs; the
            // Quick view derives an output channel from it.
            let sel_out = i.min(state.device.output_pair_count().saturating_sub(1));
            state.sel_out = sel_out;
            // Selecting a bus dismisses the Route flyout — it did its
            // job. Doesn't touch any other (Settings/Eq/Trim) flyout
            // that happens to be open elsewhere.
            close_route_flyout(state);
            notify_osc_raw(state, "/submix", vec![OscType::Int(sel_out as i32)]);
        }
        Message::ModifiersChanged(m) => state.modifiers = m,
        Message::WindowSize(w, h) => {
            // The right-hand control strip is never folded by the window
            // width.  A narrow window keeps it visible and the mixer rows
            // scroll horizontally underneath their own slider, which sits
            // immediately to the left of the panel.  Only the user's own
            // ToggleSidebar may collapse it.
            state.window_size = Some((w, h));
            return update(state, Message::WindowResized(w));
        }
        Message::WindowResized(width) => {
            // Coalesce: a live window drag fires `Resized` faster than the
            // display can present. Stash the width and only rebuild/recompute
            // when `RESIZE_THROTTLE` has elapsed since the last applied one,
            // so the UI doesn't do N full rebuilds for N intermediate sizes
            // that were never even rendered. The throttle is flushed by the
            // next due resize or `Message::Tick` (above).
            state.pending_resize_width = Some(width);
            apply_pending_resize(state);
        }
        Message::EscapePressed => {
            // Topmost thing first: the modal outranks an in-place edit.
            if state.show_about {
                state.show_about = false;
            } else if state.control_room_settings_open {
                state.control_room_settings_open = false;
                state.control_room_draft = state.control_room.clone();
            } else if state.show_device_panel {
                state.show_device_panel = false;
            } else if state.show_settings {
                state.show_settings = false;
            } else if state.renaming.is_some() {
                state.renaming = None;
                state.rename_buf.clear();
            } else if state.editing.is_some() {
                state.editing = None;
            }
        }
        Message::Mute(cid, m) => {
            select_strip(state, cid);
            // `propagation_set` covers both the pre-existing multi-select
            // case and (new) any mute-linked group `cid` belongs to.
            set_channel_mute(state, cid, m);
            notify_osc(state, OscOutbound::Mute(cid, m));
            for other in propagation_set(state, cid, sidebar::GroupLink::Mute) {
                set_channel_mute(state, other, m);
                notify_osc(state, OscOutbound::Mute(other, m));
            }
        }
        Message::Solo(cid, s) => {
            select_strip(state, cid);
            set_channel_solo(state, cid, s);
            notify_osc(state, OscOutbound::Solo(cid, s));
            for other in propagation_set(state, cid, sidebar::GroupLink::Solo) {
                set_channel_solo(state, other, s);
                notify_osc(state, OscOutbound::Solo(other, s));
            }
        }
        Message::GlobalMuteToggle => {
            let muted: Vec<ChannelId> =
                all_channel_ids(state).into_iter().filter(|&cid| channel_is_muted(state, cid)).collect();
            if muted.is_empty() {
                for cid in std::mem::take(&mut state.last_cleared_mutes) {
                    set_channel_mute(state, cid, true);
                    notify_osc(state, OscOutbound::Mute(cid, true));
                }
            } else {
                for &cid in &muted {
                    set_channel_mute(state, cid, false);
                    notify_osc(state, OscOutbound::Mute(cid, false));
                }
                state.last_cleared_mutes = muted;
            }
        }
        Message::GlobalSoloToggle => {
            let soloed: Vec<ChannelId> =
                all_channel_ids(state).into_iter().filter(|&cid| channel_is_soloed(state, cid)).collect();
            if soloed.is_empty() {
                for cid in std::mem::take(&mut state.last_cleared_solos) {
                    set_channel_solo(state, cid, true);
                    notify_osc(state, OscOutbound::Solo(cid, true));
                }
            } else {
                for &cid in &soloed {
                    set_channel_solo(state, cid, false);
                    notify_osc(state, OscOutbound::Solo(cid, false));
                }
                state.last_cleared_solos = soloed;
            }
        }
        Message::Phantom(idx, p) => {            if let Err(e) = state.device.set_input_phantom(idx, p) {
                fail(state, "set_input_phantom", &e);
            }
            notify_osc_bool(state, format!("/input/{idx}/phantom"), p);
        }
        Message::Pad(idx, p) => {            if let Err(e) = state.device.set_input_pad(idx, p) {
                fail(state, "set_input_pad", &e);
            }
            notify_osc_bool(state, format!("/input/{idx}/pad"), p);
        }
        Message::Instrument(idx, p) => {
            if let Err(e) = state.device.set_input_instrument(idx, p) {
                fail(state, "set_input_instrument", &e);
            }
            notify_osc_bool(state, format!("/input/{idx}/instrument"), p);
        }
        Message::LevelChanged(idx, level) => {
            if let Err(e) = state.device.set_input_level(idx, level) {
                fail(state, "set_input_level", &e);
            }
            notify_osc_raw(
                state,
                format!("/input/{idx}/level"),
                vec![OscType::Int(level.code() as i32)],
            );
        }
        Message::Autoset(idx, on) => {            if let Err(e) = state.device.set_autoset(idx, on) {
                fail(state, "set_autoset", &e);
            }
            notify_osc_bool(state, format!("/input/{idx}/autoset"), on);
        }
        Message::Gain(idx, g) => {
            // A linked pair's gain control moves both preamps by the same
            // step, preserving any offset between them.  The device still
            // stores the two gains independently, so the right side goes
            // through its own register; backends without one fall back to
            // the odd channel's normal gain setter.
            // The linked gain knob is the left strip's control (even index).
            // An odd index is the right side addressed directly, e.g. by
            // `/2/gainRight` or a Global OSC input path; leave that side
            // independent.
            let linked = idx % 2 == 0 && state.device.input_pair_linked(idx / 2);
            let max = state
                .device
                .inputs()
                .get(idx)
                .and_then(|c| c.gain_max)
                .unwrap_or(65);
            let old_left = state
                .device
                .inputs()
                .get(idx)
                .and_then(|c| c.gain)
                .unwrap_or(g);
            let old_right = state
                .device
                .inputs()
                .get(idx)
                .and_then(|c| c.gain_right)
                .or_else(|| state.device.inputs().get(idx + 1).and_then(|c| c.gain))
                .unwrap_or(old_left);
            if let Err(e) = state.device.set_input_gain(idx, g) {
                fail(state, "set_input_gain", &e);
            }
            if linked {
                let delta = g as i64 - old_left as i64;
                let right = (old_right as i64 + delta).clamp(0, max as i64) as u32;
                if state.device.set_input_gain_right(idx, right).is_err() {
                    if let Err(e) = state.device.set_input_gain(idx + 1, right) {
                        fail(state, "set_input_gain_right", &e);
                    }
                }
            }
            let actual = state
                .device
                .inputs()
                .get(idx)
                .and_then(|c| c.gain)
                .unwrap_or(g);
            notify_osc_raw(
                state,
                format!("/input/{idx}/gain"),
                vec![OscType::Float(actual as f32)],
            );
        }
        Message::RmeToggle(target) => {
            if let Some(message) = rme_toggle_message(state, target) {
                return update(state, message);
            }
        }
        Message::RmeGainRight(idx, gain) => {
            // The right side is independent even while the pair is linked;
            // `/2/gain` only moves the left side.
            if let Err(e) = state.device.set_input_gain_right(idx, gain) {
                fail(state, "set_input_gain_right", &e);
            }
        }
        Message::RmeMasterVolume(value) => {
            if let Some(pair) = state.device.settings().main_out {
                let left = pair * 2;
                if left < state.device.outputs().len() {
                    set_channel_volume(state, ChannelId::Output(left), left, value);
                    notify_rme_channel(state, ChannelId::Output(left));
                }
            }
        }
        Message::RmeSelect(cid, on) => {
            if on {
                state.selected.insert(cid);
                state.focus_channel = Some(cid);
                state.select_anchor = Some(cid);
            } else {
                state.selected.remove(&cid);
                if state.focus_channel == Some(cid) {
                    state.focus_channel = state.selected.iter().next().copied();
                }
            }
        }
        Message::RmeRename(cid, name) => {
            let name = name.trim().to_string();
            if name.is_empty() {
                state.labels.remove(&cid);
            } else {
                state.labels.insert(cid, name);
            }
            state.prefs_dirty = true;
        }
        Message::RmeDynamicsField {
            channel,
            field,
            value,
        } => {
            let mut d = dynamics_for(state, channel);
            match field {
                "gain" => d.gain_db = value * 60.0 - 30.0,
                "attack" => d.attack_ms = (value * 200.0).round() as u16,
                "release" => d.release_ms = (100.0 + value * 899.0).round() as u16,
                "comp_threshold" => d.comp_threshold_db = -60.0 + value * 60.0,
                "comp_ratio" => d.comp_ratio = 1.0 + value * 9.0,
                "exp_threshold" => d.exp_threshold_db = -99.0 + value * 69.0,
                "exp_ratio" => d.exp_ratio = 1.0 + value * 9.0,
                "auto_max_gain" => d.auto_max_gain_db = value * 18.0,
                "auto_headroom" => d.auto_headroom_db = 3.0 + value * 9.0,
                "auto_rise" => d.auto_rise_s = 0.1 + value * 9.8,
                _ => return Task::none(),
            }
            return update(state, Message::DynamicsChanged(channel, d.clamped()));
        }
        Message::InputPhaseRight(idx, invert) => {
            if let Err(e) = state.device.set_input_phase_right(idx, invert) {
                fail(state, "set_input_phase_right", &e);
            }
        }
        Message::SpeakerBLinked(on) => {
            state.control_room.speaker_b_linked = on;
            if let Err(e) = state.device.set_speaker_b_linked(on) {
                fail(state, "set_speaker_b_linked", &e);
            }
            state.prefs_dirty = true;
        }
        Message::Sensitivity(idx, plus4) => {
            let s = if plus4 {
                totalcontrol_core::Sensitivity::Plus4dBu
            } else {
                totalcontrol_core::Sensitivity::Minus10dBV
            };
            if let Err(e) = state.device.set_sensitivity(idx, s) {
                fail(state, "set_sensitivity", &e);
            }
            notify_osc_bool(state, format!("/input/{idx}/sensitivity"), plus4);
        }
        Message::TrimChanged(cid, db) => {
            let db = db.clamp(-65.0, 0.0);
            apply_trim(state, cid, db);
            let (kind, id) = channel_slug(cid);
            notify_osc_raw(state, format!("/{kind}/{id}/trim"), vec![OscType::Float(db)]);
        }
        Message::TrimReset(cid) => {
            apply_trim(state, cid, 0.0);
            let (kind, id) = channel_slug(cid);
            notify_osc_raw(state, format!("/{kind}/{id}/trim"), vec![OscType::Float(0.0)]);
        }
        Message::EqEnabled(cid, on) => {            if let Err(e) = set_eq_enabled_for(state, cid, on) {
                fail(state, "set_eq_enabled", &e);
            }
            let (kind, id) = channel_slug(cid);
            notify_osc_bool(state, format!("/{kind}/{id}/eq"), on);
        }
        Message::EqBandType(cid, band, t) => {            if let Err(e) = set_eq_band_type_for(state, cid, band, t) {
                fail(state, "set_eq_band_type", &e);
            }
            let code = match t {
                EqBandType::Off => 0,
                EqBandType::Bell => 1,
                EqBandType::LowShelf => 2,
                EqBandType::HighShelf => 3,
                        EqBandType::LowPass => 4,
};
            let (kind, id) = channel_slug(cid);
            notify_osc_raw(
                state,
                format!("/{kind}/{id}/eq/{band}/type"),
                vec![OscType::Int(code)],
            );
        }
        Message::EqBandFreq(cid, band, freq) => {            if let Err(e) = set_eq_band_freq_for(state, cid, band, freq) {
                fail(state, "set_eq_band_freq", &e);
            }
            let (kind, id) = channel_slug(cid);
            notify_osc_raw(
                state,
                format!("/{kind}/{id}/eq/{band}/freq"),
                vec![OscType::Float(freq as f32)],
            );
        }
        Message::EqBandQ(cid, band, q) => {            if let Err(e) = set_eq_band_q_for(state, cid, band, q) {
                fail(state, "set_eq_band_q", &e);
            }
            let (kind, id) = channel_slug(cid);
            notify_osc_raw(
                state,
                format!("/{kind}/{id}/eq/{band}/q"),
                vec![OscType::Float(q)],
            );
        }
        Message::EqBandGain(cid, band, gain) => {            if let Err(e) = set_eq_band_gain_for(state, cid, band, gain) {
                fail(state, "set_eq_band_gain", &e);
            }
            let (kind, id) = channel_slug(cid);
            notify_osc_raw(
                state,
                format!("/{kind}/{id}/eq/{band}/gain"),
                vec![OscType::Float(gain)],
            );
        }
        Message::EqBandPoint(cid, band, freq, gain) => {            if let Err(e) = set_eq_band_freq_for(state, cid, band, freq) {
                fail(state, "set_eq_band_freq", &e);
            }
            if let Err(e) = set_eq_band_gain_for(state, cid, band, gain) {
                fail(state, "set_eq_band_gain", &e);
            }
            let (kind, id) = channel_slug(cid);
            notify_osc_raw(
                state,
                format!("/{kind}/{id}/eq/{band}/freq"),
                vec![OscType::Float(freq as f32)],
            );
            notify_osc_raw(
                state,
                format!("/{kind}/{id}/eq/{band}/gain"),
                vec![OscType::Float(gain)],
            );
        }
        Message::EqBandReset(cid, band) => {            let reset = match cid {
                ChannelId::Input(idx) => {
                    let mut result = state.device.set_eq_band_freq(idx, band, 1000);
                    if result.is_ok() {
                        result = state.device.set_eq_band_q(idx, band, 0.7);
                    }
                    if result.is_ok() {
                        result = state.device.set_eq_band_gain(idx, band, 0.0);
                    }
                    result
                }
                ChannelId::Output(idx) => update_output_eq(state, idx, |e| {
                    e.bands[band].freq_hz = 1000;
                    e.bands[band].q = 0.7;
                    e.bands[band].gain_db = 0.0;
                }),
                ChannelId::Playback(_) => Ok(()),
            };
            if let Err(e) = reset {
                fail(state, "set_eq_band_reset", &e);
            }
            let (kind, id) = channel_slug(cid);
            for (suffix, value) in [("freq", 1000.0), ("q", 0.7), ("gain", 0.0)] {
                notify_osc_raw(
                    state,
                    format!("/{kind}/{id}/eq/{band}/{suffix}"),
                    vec![OscType::Float(value)],
                );
            }
        }
        Message::EqLowCutEnabled(cid, on) => {            if let Err(e) = set_low_cut_enabled_for(state, cid, on) {
                fail(state, "set_eq_low_cut_enabled", &e);
            }
            let (kind, id) = channel_slug(cid);
            notify_osc_bool(state, format!("/{kind}/{id}/lowcut"), on);
        }
        Message::EqLowCutFreq(cid, freq) => {            if let Err(e) = set_low_cut_freq_for(state, cid, freq) {
                fail(state, "set_eq_low_cut_freq", &e);
            }
            let (kind, id) = channel_slug(cid);
            notify_osc_raw(
                state,
                format!("/{kind}/{id}/lowcut/freq"),
                vec![OscType::Float(freq as f32)],
            );
        }
        Message::EqLowCutSlope(cid, slope) => {            if let Err(e) = set_low_cut_slope_for(state, cid, slope) {
                fail(state, "set_eq_low_cut_slope", &e);
            }
            let (kind, id) = channel_slug(cid);
            notify_osc_raw(
                state,
                format!("/{kind}/{id}/lowcut/slope"),
                vec![OscType::Int(slope as i32)],
            );
        }
        Message::DynamicsEnabled(cid, on) => {            let mut d = dynamics_for(state, cid);
            d.on = on;
            if let Err(e) = state.device.set_dynamics(cid, &d) {
                fail(state, "set_dynamics", &e);
            }
            let (kind, id) = channel_slug(cid);
            notify_osc_bool(state, format!("/{kind}/{id}/dynamics"), on);
        }
        Message::DynamicsChanged(cid, d) => {            if let Err(e) = state.device.set_dynamics(cid, &d) {
                fail(state, "set_dynamics", &e);
            }
            let (kind, id) = channel_slug(cid);
            let base = format!("/{kind}/{id}/dynamics");
            notify_osc_bool(state, format!("{base}/on"), d.on);
            notify_osc_raw(state, format!("{base}/gain"), vec![OscType::Float(d.gain_db)]);
            notify_osc_raw(
                state,
                format!("{base}/attack"),
                vec![OscType::Int(d.attack_ms as i32)],
            );
            notify_osc_raw(
                state,
                format!("{base}/release"),
                vec![OscType::Int(d.release_ms as i32)],
            );
            notify_osc_raw(
                state,
                format!("{base}/comp/threshold"),
                vec![OscType::Float(d.comp_threshold_db)],
            );
            notify_osc_raw(
                state,
                format!("{base}/comp/ratio"),
                vec![OscType::Float(d.comp_ratio)],
            );
            notify_osc_raw(
                state,
                format!("{base}/exp/threshold"),
                vec![OscType::Float(d.exp_threshold_db)],
            );
            notify_osc_raw(
                state,
                format!("{base}/exp/ratio"),
                vec![OscType::Float(d.exp_ratio)],
            );
            notify_osc_bool(state, format!("{base}/auto"), d.auto_level);
            notify_osc_raw(
                state,
                format!("{base}/auto/max"),
                vec![OscType::Float(d.auto_max_gain_db)],
            );
            notify_osc_raw(
                state,
                format!("{base}/auto/headroom"),
                vec![OscType::Float(d.auto_headroom_db)],
            );
            notify_osc_raw(
                state,
                format!("{base}/auto/rise"),
                vec![OscType::Float(d.auto_rise_s)],
            );
        }
        Message::OscRmeFx { echo, field, value } => {
            if echo {
                let mut e = state.device.settings().echo.unwrap_or_default();
                match field {
                    "on" => e.on = value >= 0.5,
                    "type" => e.kind = (value * 2.0).round().clamp(0.0, 2.0) as u8,
                    "delay" => e.delay_ms = (value * 2000.0).round() as u16,
                    "feedback" => e.feedback = (value * 100.0).round().clamp(0.0, 100.0) as u8,
                    "highcut" => e.high_cut = (value * 5.0).round().clamp(0.0, 5.0) as u8,
                    "volume" => e.volume_db = value * 65.0 - 65.0,
                    "width" => e.width = (value * 100.0).round().clamp(0.0, 100.0),
                    _ => {}
                }
                if let Err(err) = state.device.set_echo(&e) {
                    fail(state, "set_echo", &err);
                }
            } else {
                let mut r = state.device.settings().reverb.unwrap_or_default();
                match field {
                    "on" => r.on = value >= 0.5,
                    "type" => r.kind = value.round().clamp(0.0, 9.0) as u8,
                    "predelay" => r.pre_delay_ms = (value * 999.0).round() as u16,
                    "lowcut" => r.low_cut_hz = (20.0 * 25.0f32.powf(value.clamp(0.0, 1.0))).round() as u16,
                    "highcut" => r.high_cut_hz = (5000.0 * 4.0f32.powf(value.clamp(0.0, 1.0))).round() as u16,
                    "roomscale" => r.room_scale = 0.5 + value.clamp(0.0, 1.0) * 2.5,
                    "attack" => r.attack_ms = (5.0 + value.clamp(0.0, 1.0) * 395.0).round() as u16,
                    "hold" => r.hold_ms = (5.0 + value.clamp(0.0, 1.0) * 395.0).round() as u16,
                    "release" => r.release_ms = (5.0 + value.clamp(0.0, 1.0) * 495.0).round() as u16,
                    "time" => r.reverb_time_ds = (1.0 + value.clamp(0.0, 1.0) * 49.0).round() as u16,
                    "highdamp" => r.high_damp_hz = (1000.0 * 20.0f32.powf(value.clamp(0.0, 1.0))).round() as u16,
                    "smooth" => r.smoothness = (value * 100.0).round().clamp(0.0, 100.0) as u8,
                    "width" => r.width = (value * 100.0).round().clamp(0.0, 100.0),
                    "volume" => r.volume_db = value * 65.0 - 65.0,
                    _ => {}
                }
                if let Err(err) = state.device.set_reverb(&r) {
                    fail(state, "set_reverb", &err);
                }
            }
            notify_rme_page3(state);
        }
        Message::ReverbChanged(r) => {
            if let Err(e) = state.device.set_reverb(&r) {
                fail(state, "set_reverb", &e);
            }
            notify_osc_bool(state, "/fx/reverb/on", r.on);
            notify_osc_raw(state, "/fx/reverb/type", vec![OscType::Int(r.kind as i32)]);
            notify_osc_raw(
                state,
                "/fx/reverb/predelay",
                vec![OscType::Int(r.pre_delay_ms as i32)],
            );
            notify_osc_raw(
                state,
                "/fx/reverb/lowcut",
                vec![OscType::Int(r.low_cut_hz as i32)],
            );
            notify_osc_raw(
                state,
                "/fx/reverb/highcut",
                vec![OscType::Int(r.high_cut_hz as i32)],
            );
            notify_osc_raw(
                state,
                "/fx/reverb/room",
                vec![OscType::Float(r.room_scale)],
            );
            notify_osc_raw(
                state,
                "/fx/reverb/smooth",
                vec![OscType::Int(r.smoothness as i32)],
            );
            notify_osc_raw(
                state,
                "/fx/reverb/volume",
                vec![OscType::Float(r.volume_db)],
            );
            notify_osc_raw(
                state,
                "/fx/reverb/width",
                vec![OscType::Float(r.width)],
            );
        }
        Message::EchoChanged(echo) => {
            if let Err(e) = state.device.set_echo(&echo) {
                fail(state, "set_echo", &e);
            }
            notify_osc_bool(state, "/fx/echo/on", echo.on);
            notify_osc_raw(state, "/fx/echo/type", vec![OscType::Int(echo.kind as i32)]);
            notify_osc_raw(
                state,
                "/fx/echo/delay",
                vec![OscType::Int(echo.delay_ms as i32)],
            );
            notify_osc_raw(
                state,
                "/fx/echo/feedback",
                vec![OscType::Int(echo.feedback as i32)],
            );
            notify_osc_raw(
                state,
                "/fx/echo/highcut",
                vec![OscType::Int(echo.high_cut as i32)],
            );
            notify_osc_raw(
                state,
                "/fx/echo/volume",
                vec![OscType::Float(echo.volume_db)],
            );
            notify_osc_raw(state, "/fx/echo/width", vec![OscType::Float(echo.width)]);
        }
        Message::ResetMix(kind) => {
            state.options_menu = OptionsMenu::Closed;
            apply_reset_mix(state, kind);
        }
        Message::FxSendChanged(cid, db) => {            if let Err(e) = state.device.set_channel_fx_send(cid, db) {
                fail(state, "set_channel_fx_send", &e);
            }
            let (kind, id) = channel_slug(cid);
            notify_osc_raw(state, format!("/{kind}/{id}/fx_send"), vec![OscType::Float(db)]);
        }
        Message::FxReturnChanged(out, db) => {
            select_strip(state, ChannelId::Output(out));
            if let Err(e) = state.device.set_fx_return(out, db) {
                fail(state, "set_fx_return", &e);
            }
            notify_osc_raw(
                state,
                format!("/output/{out}/fxreturn"),
                vec![OscType::Float(db)],
            );
        }
        Message::LoopbackChanged(i, on) => {
            if let Err(e) = state.device.set_loopback_channel(i, on) {
                fail(state, "set_loopback", &e);
            }
            notify_osc_bool(state, format!("/output/{i}/loopback"), on);
        }
        Message::StereoLinkChanged(cid, linked) => {
            // A Control Room pair never splits, whichever client asks.
            let linked = if is_control_room_cid(state, cid) {
                true
            } else {
                linked
            };
            let _ = match cid {
                ChannelId::Input(i) => state.device.set_input_pair_linked(i / 2, linked),
                ChannelId::Playback(i) => state.device.set_playback_linked(i / 2, linked),
                ChannelId::Output(i) => state.device.set_output_linked(i / 2, linked),
            };
            start_stereo_anim(state, cid, linked);
            let (kind, id) = channel_slug(cid);
            notify_osc_bool(state, format!("/{kind}/{id}/stereo"), linked);
        }
        Message::PitchChanged(v) => {
            if let Err(e) = state.device.set_pitch(v) {
                fail(state, "set_pitch", &e);
            }
            notify_osc_raw(state, "/pitch", vec![OscType::Float(v)]);
        }
        Message::WidthChanged(v) => {
            if let Err(e) = state.device.set_width(v) {
                fail(state, "set_width", &e);
            }
            notify_osc_raw(state, "/width", vec![OscType::Float(v)]);
        }
        Message::ChannelWidthChanged(cid, v) => {
            if let Err(e) = state.device.set_channel_width(cid, v) {
                fail(state, "set_channel_width", &e);
            }
            let (kind, id) = channel_slug(cid);
            notify_osc_raw(
                state,
                format!("/{kind}/{id}/width"),
                vec![OscType::Float(v)],
            );
        }
        Message::MsProcChanged(on) => {
            if let Err(e) = state.device.set_ms_proc(on) {
                fail(state, "set_ms_proc", &e);
            }
            notify_osc_bool(state, "/ms_proc", on);
        }
        Message::InputMsProcChanged(idx, on) => {
            if let Err(e) = state.device.set_input_ms_proc(idx, on) {
                fail(state, "set_input_ms_proc", &e);
            }
            notify_osc_bool(state, format!("/input/{idx}/ms_proc"), on);
        }
        Message::PlaybackMsProcChanged(idx, on) => {
            if let Err(e) = state.device.set_playback_ms_proc(idx, on) {
                fail(state, "set_playback_ms_proc", &e);
            }
            notify_osc_bool(state, format!("/playback/{idx}/ms_proc"), on);
        }
        Message::An12Changed(on) => {
            if let Err(e) = state.device.set_an12(on) {
                fail(state, "set_an12", &e);
            }
            notify_osc_bool(state, "/an12", on);
        }
        Message::InputLinkChanged(on) => {
            if let Err(e) = state.device.set_input_link(on) {
                fail(state, "set_input_link", &e);
            }
            notify_osc_bool(state, "/input_link", on);
        }
        Message::EqForRecordChanged(on) => {
            if let Err(e) = state.device.set_eq_for_record(on) {
                fail(state, "set_eq_for_record", &e);
            }
            notify_osc_bool(state, "/eq_for_record", on);
        }
        Message::OpticalOutFormatChanged(spdif) => {
            if let Err(e) = state.device.set_optical_out_format(spdif) {
                fail(state, "set_optical_out_format", &e);
            }
            notify_osc_bool(state, "/optical_out_spdif", spdif);
        }
        Message::LineInRefSelected(code) => {
            if let Err(e) = state.device.set_line_in_ref(code) {
                fail(state, "set_line_in_ref", &e);
            }
            notify_osc_raw(state, "/line_in_ref", vec![OscType::Int(code as i32)]);
        }
        Message::LineInRef58Selected(code) => {
            if let Err(e) = state.device.set_line_in_ref_58(code) {
                fail(state, "set_line_in_ref_58", &e);
            }
            notify_osc_raw(state, "/line_in_ref_58", vec![OscType::Int(code as i32)]);
        }
        Message::LineOutRefSelected(code) => {
            if let Err(e) = state.device.set_line_out_ref(code) {
                fail(state, "set_line_out_ref", &e);
            }
            notify_osc_raw(state, "/line_out_ref", vec![OscType::Int(code as i32)]);
        }
        Message::PhonesRefSelected(code) => {
            if let Err(e) = state.device.set_phones_ref(code) {
                fail(state, "set_phones_ref", &e);
            }
            notify_osc_raw(state, "/phones_ref", vec![OscType::Int(code as i32)]);
        }
        Message::SpdifProfessionalChanged(on) => {
            if let Err(e) = state.device.set_spdif_professional(on) {
                fail(state, "set_spdif_professional", &e);
            }
            notify_osc_bool(state, "/spdif_professional", on);
        }
        Message::WordClockSingleSpeedChanged(on) => {
            if let Err(e) = state.device.set_word_clock_single_speed(on) {
                fail(state, "set_word_clock_single_speed", &e);
            }
            notify_osc_bool(state, "/word_clock_single_speed", on);
        }
        Message::WordClockTerminationChanged(on) => {
            if let Err(e) = state.device.set_word_clock_termination(on) {
                fail(state, "set_word_clock_termination", &e);
            }
            notify_osc_bool(state, "/word_clock_termination", on);
        }
        Message::CueChanged(cid, on) => {
            select_strip(state, cid);
            if let ChannelId::Output(idx) = cid {
                if let Err(e) = state.device.set_cue(idx / 2, on) {
                    fail(state, "set_cue", &e);
                }
                notify_osc_bool(state, format!("/output/{idx}/cue"), on);
            }
        }
        Message::PhaseChanged(cid, invert) => {            if let ChannelId::Input(idx) = cid {
                if let Err(e) = state.device.set_phase(idx, invert) {
                    fail(state, "set_phase", &e);
                }
                notify_osc_bool(state, format!("/input/{idx}/phase"), invert);
            }
        }
        Message::OutputPhaseChanged(idx, invert) => {
            if let Err(e) = state.device.set_output_phase(idx, invert) {
                fail(state, "set_output_phase", &e);
            }
            notify_osc_bool(state, format!("/output/{idx}/phase"), invert);
        }
        Message::NoTrimChanged(idx, on) => {
            if let Err(e) = state.device.set_no_trim(idx, on) {
                fail(state, "set_no_trim", &e);
            }
            notify_osc_bool(state, format!("/output/{idx}/no_trim"), on);
        }
        Message::TalkbackReceiveChanged(idx, on) => {
            if let Err(e) = state.device.set_talkback_receive(idx, on) {
                fail(state, "set_talkback_receive", &e);
            }
            notify_osc_bool(state, format!("/output/{idx}/talkback"), on);
        }
        Message::VolumeChanged(cid, out, v) => {
            select_strip(state, cid);
            apply_grouped_volume(state, cid, out, v);
        }
        Message::MatrixCellChanged(cid, block, v) => {
            if state.matrix_mono {
                let send = effective_to_send(state, cid, v);
                if let Err(e) = state.device.set_matrix_cell(cid, block, send) {
                    fail(state, "set_matrix_cell", &e);
                }
            } else {
                apply_grouped_volume(state, cid, block / 2, v);
            }
        }
        Message::ToggleMatrixMono => {
            state.matrix_mono = !state.matrix_mono;
            state.last_activity = Instant::now();
        }
        Message::NudgeVolume(delta_db) => {
            let targets: Vec<ChannelId> = if !state.selected.is_empty() {
                state.selected.iter().copied().collect()
            } else if let Some(cid) = state.focus_channel {
                vec![cid]
            } else {
                Vec::new()
            };
            for cid in targets {
                let out = match cid {
                    ChannelId::Output(_) => 0,
                    _ => state.sel_out,
                };
                let cur = state.device.volume(cid, out).unwrap_or(0.0)
                    * trim_factor(channel_trim(state, cid));
                let new_vol = db_to_vol(vol_to_db(cur) + delta_db).clamp(0.0, 2.0);
                set_channel_volume(state, cid, out, new_vol);
                notify_osc(state, OscOutbound::Volume(cid, out, new_vol));
            }
            state.last_activity = Instant::now();
        }
        Message::FaderPressed(cid, out, v, range) => {
            select_strip(state, cid);
            if let Some((lo, hi)) = range {
                state.drag_range = Some((cid, lo, hi));
            }
            apply_grouped_volume(state, cid, out, v);
        }
        Message::RangeCleared(cid) => {
            // Letting go ends the gesture: the next drag is its own step.
            state.undo_gesture = None;
            if state.drag_range.is_some_and(|(dc, _, _)| dc == cid) {
                state.drag_range = None;
            }
        }
        Message::Reset(cid, out, default_vol) => {
            select_strip(state, cid);
            if state.selected.len() > 1 && state.selected.contains(&cid) {
                // Reset means "back to default" for the whole group, not a
                // relative move — every selected fader snaps to the same
                // absolute value, unlike a drag which preserves balance.
                for sel in state.selected.clone() {
                    set_channel_volume(state, sel, out, default_vol);
                    notify_osc(state, OscOutbound::Volume(sel, out, default_vol));
                }
            } else {
                set_channel_volume(state, cid, out, default_vol);
                notify_osc(state, OscOutbound::Volume(cid, out, default_vol));
            }
            if state.drag_range.is_some_and(|(dc, _, _)| dc == cid) {
                state.drag_range = None;
            }
        }
        Message::PanChanged(cid, out, pan) => {            apply_grouped_pan(state, cid, out, pan);
        }
        Message::PanReset(cid, out) => {            if state.selected.len() > 1 && state.selected.contains(&cid) {
                for sel in state.selected.clone() {
                    set_channel_pan(state, sel, out, 0);
                    notify_osc(state, OscOutbound::Pan(sel, out, 0));
                }
            } else {
                set_channel_pan(state, cid, out, 0);
                notify_osc(state, OscOutbound::Pan(cid, out, 0));
            }
        }
        Message::ToggleCollapse(cid) => {
            select_strip(state, cid);
            let target = !state.collapsed.contains(&cid);
            if state.selected.len() > 1 && state.selected.contains(&cid) {
                // Uniform target for the whole group — the opposite of
                // what the clicked strip currently is — rather than each
                // toggling its own state independently, which would leave
                // them out of sync with each other.
                for sel in state.selected.clone() {
                    set_collapsed(state, sel, target);
                }
            } else {
                set_collapsed(state, cid, target);
            }
        }
        Message::ToggleFlyout(cid, kind) => {
            select_strip(state, cid);
            let now = Instant::now();
            if state.flyout_open.contains(&(cid, kind)) {
                begin_flyout_close(state, cid, kind, now);
                state.pending_flyout = None;
            } else {
                open_flyout(state, cid, kind, now);
            }
        }
        Message::CloseFlyout => {
            close_route_flyout(state);
        }
        Message::CollapseTick => {
            let now = Instant::now();
            state.collapse_anim.retain(|_, a| a.is_settling(now));
            state.flyout_anim.retain(|_, a| a.is_settling(now));
            state.stereo_anim.retain(|_, a| a.is_settling(now));
            if state.fx_anim.is_some_and(|a| !a.is_settling(now)) {
                state.fx_anim = None;
            }
            if state
                .assign_anim
                .as_ref()
                .is_some_and(|a| a.elapsed_ms(now) >= a.max_ms)
            {
                state.assign_anim = None;
            }
            if let Some((cid, kind)) = state.pending_flyout {
                let still_closing = state.flyout_anim.iter().any(|(&(c, k), a)| {
                    c == cid && k != strip::FlyoutKind::Route && a.is_settling(now)
                });
                if !still_closing {
                    state.pending_flyout = None;
                    open_flyout_now(state, cid, kind, now);
                }
            }
        }
        Message::StripClicked(cid) => {
            state.focus_channel = Some(cid);
            if state.modifiers.control() {
                // Toggle just this one strip, leaving the rest of the
                // selection untouched — the standard Ctrl+click
                // convention. Becomes the pivot for the next Shift+click.
                if !state.selected.remove(&cid) {
                    state.selected.insert(cid);
                }
                state.select_anchor = Some(cid);
            } else if state.modifiers.shift() {
                // Select the whole visual range from the anchor through
                // cid, replacing the current selection — the standard
                // Shift+click convention. Doesn't move the anchor, so a
                // second Shift+click elsewhere re-measures from the same
                // start rather than the last endpoint.
                let order = channel_order(state);
                let anchor = state.select_anchor.unwrap_or(cid);
                if let (Some(from), Some(to)) = (
                    order.iter().position(|&c| c == anchor),
                    order.iter().position(|&c| c == cid),
                ) {
                    let (lo, hi) = (from.min(to), from.max(to));
                    state.selected = order[lo..=hi].iter().copied().collect();
                }
                if state.select_anchor.is_none() {
                    state.select_anchor = Some(cid);
                }
            } else {
                // Plain click: this strip becomes the selection.
                select_strip(state, cid);
            }
        }
        Message::ClearSelection => {
            state.selected.clear();
            state.select_anchor = None;
        }
        Message::StripHovered(h) => state.hovered_strip = h,
        Message::EditStart(cid, buf) => {
            select_strip(state, cid);
            state.editing = Some(cid);
            state.edit_buf = buf;
            state.field_opening = true;
            // Ready to type over, like the rename field: it used to open
            // unfocused, so typing did nothing until it was clicked again.
            return Task::batch([
                iced::widget::operation::focus(DB_INPUT_ID),
                iced::widget::operation::select_all(DB_INPUT_ID),
            ]);
        }
        Message::EditChanged(s) => state.edit_buf = s,
        Message::EditCommit => {
            if let Some(cid) = state.editing {
                if let Some(v) = parse_db_input(&state.edit_buf) {
                    // An output strip's typed value is its master — use
                    // the channel index, not the selected submix.
                    let out = match cid {
                        ChannelId::Output(i) => i,
                        _ => state.sel_out,
                    };
                    set_channel_volume(state, cid, out, v);
                }
                state.editing = None;
            }
        }
        Message::OscReady(tx) => {
            state.osc_tx = Some(tx);
            // A controller connecting after startup shouldn't stay blind
            // until the next manual change — snapshot everything once.
            if state.osc_dialect == crate::osc::OscDialect::Global {
                send_global_state(
                    state,
                    &crate::osc_global::SendControl::SendState,
                );
            } else {
                notify_rme_page(state, state.rme.page);
            }
        }
        Message::OscLog(line) => {
            state.osc_log.push_front(line);
            state.osc_log.truncate(OSC_LOG_MAX);
        }
        // Handled at the top of `update`, before this match.
        Message::OscRmeIn(_, _) | Message::OscGlobalIn(_) => {}
        Message::ToggleOscLog => state.show_osc_log = !state.show_osc_log,
        Message::ClearOscLog => state.osc_log.clear(),
        Message::SetOscEnabled(on) => {
            state.osc_enabled = on;
            state.prefs_dirty = true;
            rebuild_osc_config(state);
        }
        Message::SetOscMode(mode) => {
            state.osc_dialect = mode;
            state.prefs_dirty = true;
            rebuild_osc_config(state);
        }
        Message::OscRecvPortChanged(text) => state.osc_recv_text = text,
        Message::OscSendPortChanged(text) => state.osc_send_text = text,
        Message::ApplyOscPorts => {
            if let Ok(port) = state.osc_recv_text.trim().parse::<u16>() {
                if port >= 1024 {
                    state.osc_recv_port = port;
                }
            }
            if let Ok(port) = state.osc_send_text.trim().parse::<u16>() {
                if port >= 1024 {
                    state.osc_send_port = port;
                }
            }
            state.prefs_dirty = true;
            rebuild_osc_config(state);
        }
        Message::ToggleDevicePanel => {
            if !state.connected || state.device.is_mock() {
                state.show_device_panel = false;
                state.notice = Some((
                    "Connect a Fireface to open settings.".into(),
                    Instant::now(),
                ));
                return Task::none();
            }
            state.show_device_panel = !state.show_device_panel;
            state.show_settings = false;
        }
        Message::ToggleFxPanel => {
            let now = Instant::now();
            let full = FX_W * state.ui_scale;
            let current = state
                .fx_anim
                .map(|a| a.at(now))
                .unwrap_or(if state.show_fx_panel { full } else { 0.0 });
            state.show_fx_panel = !state.show_fx_panel;
            state.fx_anim = Some(strip::CollapseAnim {
                prev: current,
                value: if state.show_fx_panel { full } else { 0.0 },
                since: now,
            });
        }
        Message::ToggleAssign => {
            let opening = matches!(state.assign_menu, AssignMenu::Closed);
            state.assign_menu = if opening {
                AssignMenu::Destinations
            } else {
                AssignMenu::Closed
            };
            state.assign_pinned = false;
        }
        Message::AssignDestinations(dest) => {
            if dest == CUE_DEST {
                state.assign_menu = AssignMenu::CueTargets;
                state.assign_pinned = true;
            } else if dest < OUTPUT_DESTS {
                state.assign_menu = AssignMenu::Outputs(dest);
                state.assign_pinned = true;
            }
        }
        Message::AssignHover(dest) => {
            if !state.assign_pinned && !matches!(state.assign_menu, AssignMenu::Closed) {
                if dest == CUE_DEST {
                    state.assign_menu = AssignMenu::CueTargets;
                } else if dest < OUTPUT_DESTS {
                    state.assign_menu = AssignMenu::Outputs(dest);
                }
            }
        }
        Message::AssignCueTarget(target) => {
            state.assign_menu = AssignMenu::Closed;
            state.assign_pinned = false;
            if CUE_TARGETS.contains(&target) {
                state.assign[CUE_DEST] = Some(target);
            }
            sync_control_room_runtime(state);
        }
        Message::AssignOutput(dest, pair) => {
            // Choosing an output ends the menu, like TotalMix's.
            state.assign_menu = AssignMenu::Closed;
            state.assign_pinned = false;
            if dest >= OUTPUT_DESTS {
                return Task::none();
            }
            let old_assign = state.assign;
            // A hardware output can only sit in one monitoring destination.
            // Move it rather than showing the same channel in two Control
            // Room slots; this is also what lets `None` return it to the
            // Hardware Outputs row. The seventh slot is the Cue target, not
            // an output assignment, so it is not part of this scan.
            if let Some(pair) = pair {
                for (i, slot) in state.assign.iter_mut().take(OUTPUT_DESTS).enumerate() {
                    if i != dest && *slot == Some(pair) {
                        *slot = None;
                    }
                }
            }
            state.assign[dest] = pair;
            // A Control Room channel is always stereo: linking here, before
            // the slot animation measures the pair, makes a split pair join
            // as the single stereo strip it will be in the Control Room.
            if let Some(pair) = pair {
                if let Err(e) = state.device.set_output_linked(pair, true) {
                    fail(state, "assign stereo", &e);
                }
            }
            let old_outputs: [Option<usize>; OUTPUT_DESTS] =
                std::array::from_fn(|i| old_assign[i]);
            let new_outputs: [Option<usize>; OUTPUT_DESTS] =
                std::array::from_fn(|i| state.assign[i]);
            let old_count = old_outputs.iter().flatten().count();
            let new_count = new_outputs.iter().flatten().count();
            // Only a change in the number of Control Room channels animates.
            // Destination moves and replacements keep the count and snap.
            state.assign_anim = (old_count != new_count).then_some(AssignAnim {
                since: Instant::now(),
                old: old_outputs,
                new: new_outputs,
                max_ms: (state.window_width + 200.0).clamp(300.0, 1600.0),
            });
            // Sync both hardware-backed destinations, not only the slot
            // just changed: moving a Main or Speaker B pair to a Phones
            // slot clears the old slot above and must clear its device
            // assignment too.
            if let Err(e) = apply_control_room_assignments(state) {
                fail(state, "assign", &e);
            }
        }
        Message::ToggleSettings => {
            state.show_settings = !state.show_settings;
            state.show_device_panel = false;
        }
        Message::ToggleOptions => {
            state.options_menu = match state.options_menu {
                OptionsMenu::Closed => OptionsMenu::Root,
                _ => OptionsMenu::Closed,
            };
        }
        Message::OptionsReset => state.options_menu = OptionsMenu::Reset,
        Message::OptionsBack => state.options_menu = OptionsMenu::Root,
        Message::OptionsSettings => {
            state.options_menu = OptionsMenu::Closed;
            state.show_settings = true;
            state.show_device_panel = false;
        }
        Message::CloseOptions => state.options_menu = OptionsMenu::Closed,
        Message::SetSurround51(on) => {
            state.surround_51 = on;
            state.prefs_dirty = true;
            sync_surround_bridge(state);
        }
        Message::SetSurround71(on) => {
            state.surround_71 = on;
            state.prefs_dirty = true;
            sync_surround_bridge(state);
        }
        Message::ToggleAbout => state.show_about = !state.show_about,
        Message::OpenRepoUrl => {
            // Fire and forget: the browser is not our child process, and a
            // minimal session may have no `xdg-open` to hand it to at all.
            if let Err(e) = std::process::Command::new("xdg-open")
                .arg(about::REPO_URL)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                log::warn!("Could not open {}: {e}", about::REPO_URL);
            }
        }
        Message::ClockSourceSelected(source) => {
            if let Err(e) = state.device.set_clock_source(&source) {
                fail(state, "set_clock_source", &e);
            }
            notify_osc_raw(state, "/clock_source", vec![OscType::String(source)]);
        }
        Message::SampleRateSelected(rate) => {
            // The UC's kernel PCM rate is the PipeWire graph rate.  Reuse
            // the engine-rate path so this settings-sheet picker actually
            // changes the card instead of only recording the model value.
            // Whenever the sound server owns the card rate (UCX included),
            // use the engine-rate path so the OSC/selector rate is real.
            if state.device.rate_follows_sound_server() {
                return update(state, Message::EngineRate(rate));
            }
            if let Err(e) = state.device.set_sample_rate(rate) {
                fail(state, "set_sample_rate", &e);
            }
            notify_osc_raw(state, "/sample_rate", vec![OscType::Int(rate as i32)]);
            // On a backend where this setter reaches the card, the UC can
            // reload its stored mixer at the new speed.  Reapply the host
            // scene once the new stream has settled.
            schedule_mixer_reapply(
                state,
                Duration::from_millis(1_500),
                "device sample-rate change",
            );
        }
        // ── Right sidebar ────────────────────────────────────────────
        Message::Undo => {
            state.undo_gesture = None;
            if let Some(mut scene) = state.undo_stack.pop() {
                let current = state
                    .host_scene
                    .clone()
                    .unwrap_or_else(|| state.device.capture_scene());
                state.redo_stack.push(current.clone());
                keep_device_settings(&mut scene, &current);
                if let Err(e) = state.device.apply_scene(&scene) {
                    fail(state, "undo", &e);
                } else {
                    enforce_control_room_stereo(state);
                    state.host_scene = Some(state.device.capture_scene());
                }
            }
        }
        Message::Redo => {
            state.undo_gesture = None;
            if let Some(mut scene) = state.redo_stack.pop() {
                let current = state
                    .host_scene
                    .clone()
                    .unwrap_or_else(|| state.device.capture_scene());
                state.undo_stack.push(current.clone());
                keep_device_settings(&mut scene, &current);
                if let Err(e) = state.device.apply_scene(&scene) {
                    fail(state, "redo", &e);
                } else {
                    enforce_control_room_stereo(state);
                    state.host_scene = Some(state.device.capture_scene());
                }
            }
        }
        Message::ToggleSidebarPanel(panel) => state.sidebar_panels_open.toggle(panel),
        Message::ToggleSidebar => {
            state.sidebar_open = !state.sidebar_open;
        }
        Message::SidebarResize(dx) => {
            state.sidebar_width =
                (state.sidebar_width - dx).clamp(sidebar::MIN_WIDTH, sidebar::MAX_WIDTH);
        }
        Message::SetRowMode(mode) => state.row_mode = mode,
        Message::SetSyncDirection(dir) => {
            state.sync_direction = if state.device.needs_scene_restore() {
                SyncDirection::GuiToDevice
            } else {
                dir
            };
        }
        Message::RenameStart(cid) => {
            select_strip(state, cid);
            // Control Room names are fixed by their destination.
            if is_control_room_cid(state, cid) {
                return Task::none();
            }
            // Opening another strip's name accepts the one being typed.
            if state.renaming.is_some_and(|open| open != cid) {
                let _ = update(state, Message::RenameCommit);
            }
            state.rename_buf = state.labels.get(&cid).cloned().unwrap_or_default();
            state.renaming = Some(cid);
            state.field_opening = true;
            return Task::batch([
                iced::widget::operation::focus(RENAME_INPUT_ID),
                iced::widget::operation::select_all(RENAME_INPUT_ID),
            ]);
        }
        Message::RenameChanged(s) => state.rename_buf = s,
        Message::RenameCommit => {
            if let Some(cid) = state.renaming.take() {
                let label = state.rename_buf.trim().to_string();
                if label.is_empty() {
                    state.labels.remove(&cid);
                } else {
                    state.labels.insert(cid, label);
                }
                state.rename_buf.clear();
            }
        }
        Message::SetShowOriginalNames(on) => state.show_original_names = on,
        Message::ResetTransportErrors => {
            state.transport_baseline = state.transport_counters;
        }
        Message::EngineRate(rate) => {
            // A simulated session changes its simulated clock only, and
            // never the remembered choice: that is applied to the real
            // PipeWire at the next real launch.
            if !state.device.is_mock() {
                state.engine_force_rate = rate;
            }
            if state.device.is_mock() {
                if let Some(e) = state.engine.as_mut() {
                    e.force_rate = rate;
                }
            } else if state.device.rate_follows_sound_server() {
                let uc = state.device.is_uc_model();
                // The UC latches matrix routing at engine start.  A scene
                // written while the engine is running only updates its
                // shadow state; the next engine start applies it.
                //
                // Before a rate change, close the capture side so the
                // kernel no longer locks the playback rate to the old
                // value.  Keep playback running, write the full scene into
                // the shadow, then change the rate.  PipeWire restarts the
                // playback stream at the new rate, and that restarted
                // engine latches the scene we just wrote.
                let suspended_source = if uc && rate > 0 {
                    let source = pw_clock::fireface_source_name();
                    if let Some(source) = &source {
                        if pw_clock::set_source_suspended(source, true) {
                            // Let the kernel clear the capture hw_params
                            // (the rate lock) before touching the graph.
                            std::thread::sleep(Duration::from_millis(120));
                        } else {
                            log::warn!(
                                "Could not suspend the RME source for the rate change"
                            );
                        }
                    }
                    source
                } else {
                    None
                };
                if uc && rate > 0 {
                    // Shadow write before the rate change.  Playback is
                    // still running, so this does not disturb the current
                    // audio; the rate-change engine restart latches it.
                    let scene = state.device.capture_scene();
                    if let Err(e) = state.device.apply_scene(&scene) {
                        log::warn!("Scene shadow write before rate change failed: {e:?}");
                    }
                }
                if let Err(e) = pw_clock::force_rate(rate) {
                    log::warn!("Setting the PipeWire rate failed: {e}");
                }
                if rate > 0 {
                    state.device.set_meter_rate(rate);
                    // Only wait for confirmation when a stream is open.  An
                    // idle card closes its PCM; the forced graph rate will
                    // be used when the next stream opens, and a timeout
                    // notice there would be a false failure.
                    state.pending_device_rate = state
                        .card_stream
                        .is_some()
                        .then_some((rate, Instant::now()));
                    if let Some(source) = suspended_source {
                        pw_clock::set_source_suspended(&source, false);
                    }
                    // Fallback for a backend/card where the restart did not
                    // latch the shadow state.
                    schedule_mixer_reapply(
                        state,
                        Duration::from_millis(1_500),
                        "host sample-rate change",
                    );
                } else {
                    state.pending_device_rate = None;
                }
                state.engine = pw_clock::read();
                state.card_stream = pw_clock::card_stream(state.device.card_id());
            }
            notify_osc_raw(state, "/sample_rate", vec![OscType::Int(rate as i32)]);
        }
        Message::EngineQuantum(quantum) => {
            if !state.device.is_mock() {
                state.engine_force_quantum = quantum;
            }
            if state.device.is_mock() {
                if let Some(e) = state.engine.as_mut() {
                    e.force_quantum = quantum;
                }
            } else if state.device.rate_follows_sound_server() {
                if let Err(e) = pw_clock::force_quantum(quantum) {
                    log::warn!("Setting the PipeWire buffer size failed: {e}");
                }
                state.engine = pw_clock::read();
                state.card_stream = pw_clock::card_stream(state.device.card_id());
            }
        }
        Message::HostVolumeUnity => {
            if pw_clock::set_fireface_sink_unity() {
                state.host_volume = pw_clock::fireface_sink_volume();
                state.notice =
                    Some(("Host volume set to 0 dB".into(), Instant::now()));
            }
        }
        Message::EnginePoll => {
            // Off the UI thread: both the graph read and the `pactl`
            // probe spawn processes (~10 ms).  Capture what the async
            // block needs before it; the device handle itself cannot
            // cross the task boundary.
            let card_id = state.device.card_id();
            let follows = state.device.rate_follows_sound_server();
            let is_mock = state.device.is_mock();
            return Task::perform(
                async move {
                    let sink_running =
                        follows && !is_mock && pw_clock::fireface_sink_running();
                    let host_volume = if follows && !is_mock {
                        pw_clock::fireface_sink_volume()
                    } else {
                        None
                    };
                    (
                        pw_clock::read(),
                        pw_clock::card_stream(card_id),
                        sink_running,
                        host_volume,
                    )
                },
                |(clock, stream, sink_running, host_volume)| {
                    Message::EngineRead(clock, stream, sink_running, host_volume)
                },
            );
        }
        Message::EngineRead(clock, stream, sink_running, host_volume) => {
            if host_volume.is_some() {
                state.host_volume = host_volume;
            }
            if state.device.rate_follows_sound_server() {
                let sink_started = sink_running && !state.last_sink_running;
                let new_rate = stream.map(|s| s.rate);
                let model_rate = state.device.settings().sample_rate;
                let rate_changed = rate_needs_mixer_reapply(
                    state.last_card_rate,
                    new_rate,
                    model_rate,
                );
                if let Some(rate) = new_rate {
                    if rate_changed {
                        // Keep the scene/model in step with the rate the
                        // card actually opened at, whether TotalControl, the
                        // player or PipeWire initiated the switch.
                        let _ = state.device.set_sample_rate(rate);
                    }
                    state.last_card_rate = Some(rate);
                }
                if sink_started || rate_changed {
                    // The UC reloads its stored mixer when a playback stream
                    // starts or the device speed changes.  Reapply the
                    // current host scene after the new stream has settled.
                    let reason = if rate_changed {
                        "Fireface card rate changed"
                    } else {
                        "Fireface playback started"
                    };
                    schedule_mixer_reapply(
                        state,
                        Duration::from_secs(1),
                        reason,
                    );
                }
                state.last_sink_running = sink_running;
                state.engine = clock;
                state.card_stream = stream;
                if let Some((wanted, since)) = state.pending_device_rate {
                    let actual = stream.map(|s| s.rate);
                    if actual == Some(wanted) {
                        state.pending_device_rate = None;
                    } else if since.elapsed() >= Duration::from_secs(3) {
                        state.pending_device_rate = None;
                        state.notice = Some((
                            format!(
                                "The device did not accept {}; it is still at {}.",
                                rate_label(wanted),
                                actual.map(rate_label).unwrap_or_else(|| "its current rate".into())
                            ),
                            Instant::now(),
                        ));
                    }
                }
            }
        }
        Message::PointerPressed => {
            if std::mem::take(&mut state.field_opening) {
                return Task::none();
            }
            let mut checks = Vec::new();
            if let Some(cid) = state.renaming {
                checks.push(
                    iced::widget::operation::is_focused(RENAME_INPUT_ID)
                        .map(move |f| Message::FieldFocus(EditField::Rename, cid, f)),
                );
            }
            if let Some(cid) = state.editing {
                checks.push(
                    iced::widget::operation::is_focused(DB_INPUT_ID)
                        .map(move |f| Message::FieldFocus(EditField::Db, cid, f)),
                );
            }
            return Task::batch(checks);
        }
        Message::FieldFocus(field, cid, focused) => {
            if !focused {
                match field {
                    EditField::Rename if state.renaming == Some(cid) => {
                        return update(state, Message::RenameCommit);
                    }
                    EditField::Db if state.editing == Some(cid) => {
                        return update(state, Message::EditCommit);
                    }
                    _ => {}
                }
            }
        }
        Message::MatrixHover(cid, col, over) => {
            if over {
                state.matrix_hover = Some((cid, col));
            } else if state.matrix_hover == Some((cid, col)) {
                // Only the cell that is hovered may clear it: moving from
                // one cell to the next can deliver the new enter first.
                state.matrix_hover = None;
            }
        }
        Message::DimToggle => {
            let on = !state.device.settings().dim;
            return update(state, Message::Dim(on));
        }
        Message::Dim(on) => {
            if let Err(e) = state
                .device
                .set_dim_with(on, state.control_room.main_dim_db)
            {
                fail(state, "dim", &e);
            }
            notify_osc_bool(state, "/dim", state.device.settings().dim);
        }
        Message::SpeakerB(on) => {
            if let Err(e) = state.device.set_speaker_b(on) {
                fail(state, "speaker_b", &e);
            }
            notify_osc_bool(state, "/speaker_b", state.device.settings().speaker_b);
        }
        Message::ControlRoomRecall => {
            if let Err(e) = state.device.set_main_recall(state.control_room.recall_db) {
                fail(state, "recall", &e);
            }
        }
        Message::ControlRoomMono(on) => {
            state.device.settings_mut().mono = on;
            state.device.sync_settings_to_device();
            if let Err(e) = state.device.set_mono(on) {
                fail(state, "mono", &e);
            }
            notify_osc_bool(state, "/mono", state.device.settings().mono);
        }
        Message::ControlRoomMuteFx(on) => {
            if let Err(e) = state.device.set_mute_fx(on) {
                fail(state, "set_mute_fx", &e);
            }
            state.device.settings_mut().mute_fx = on;
            notify_osc_bool(state, "/mute_fx", on);
        }
        Message::ExternalInput(on) => {
            let source = state.control_room.external_input;
            if on && source.is_none() {
                return Task::none();
            }
            if let Err(e) = state.device.set_external_input(
                on,
                source,
                state.control_room.external_gain_db,
            ) {
                fail(state, "external input", &e);
            }
            state.device.settings_mut().external_input = on;
            notify_osc_bool(state, "/external_input", on);
        }
        Message::TalkbackChanged(on) => {
            if let Err(e) = state.device.set_talkback_with(on, &talkback_config(state)) {
                fail(state, "talkback", &e);
            }
            state.talkback_on = on;
            notify_osc_bool(state, "/talkback", on);
        }
        Message::OpenControlRoomSettings => {
            state.control_room_draft = state.control_room.clone();
            state.control_room_settings_open = true;
        }
        Message::CancelControlRoomSettings => {
            state.control_room_settings_open = false;
            state.control_room_draft = state.control_room.clone();
        }
        Message::ApplyControlRoomSettings => {
            state.control_room = state.control_room_draft.clone();
            state.control_room_settings_open = false;
            state.prefs_dirty = true;
            flush_prefs(state);
            if state.device.settings().dim {
                if let Err(e) = state
                    .device
                    .set_dim_with(true, state.control_room.main_dim_db)
                {
                    fail(state, "dim", &e);
                }
            }
            if let Err(e) = state
                .device
                .set_speaker_b_linked(state.control_room.speaker_b_linked)
            {
                fail(state, "speaker B link", &e);
            }
            sync_control_room_runtime(state);
        }
        Message::ResetControlRoomSettings => {
            state.control_room_draft = ControlRoomSettings::default();
        }
        Message::ControlRoomTalkbackInput(input) => {
            state.control_room_draft.talkback_input = input;
        }
        Message::ControlRoomTalkbackDim(db) => {
            state.control_room_draft.talkback_dim_db = round_half_db(db);
        }
        Message::ControlRoomListenbackInput(input) => {
            state.control_room_draft.listenback_input = input;
        }
        Message::ControlRoomListenbackDim(db) => {
            state.control_room_draft.listenback_dim_db = round_half_db(db);
        }
        Message::ControlRoomRecallDb(db) => {
            state.control_room_draft.recall_db = round_half_db(db);
        }
        Message::ControlRoomMainDim(db) => {
            state.control_room_draft.main_dim_db = round_half_db(db);
        }
        Message::ControlRoomSpeakerBLinked(on) => {
            state.control_room_draft.speaker_b_linked = on;
        }
        Message::ControlRoomExternalInput(pair) => {
            state.control_room_draft.external_input = pair;
            if !state.control_room_settings_open {
                state.control_room.external_input = pair;
                sync_control_room_runtime(state);
            }
        }
        Message::ControlRoomExternalGain(db) => {
            let db = round_half_db(db);
            state.control_room_draft.external_gain_db = db;
            if !state.control_room_settings_open {
                state.control_room.external_gain_db = db;
                sync_control_room_runtime(state);
            }
        }
        Message::ControlRoomPflReplacesSolo(on) => {
            state.control_room_draft.pfl_replaces_solo = on;
        }
        Message::ControlRoomExclusiveSoloPfl(on) => {
            state.control_room_draft.exclusive_solo_pfl = on;
        }
        Message::SnapshotClicked(n) => {
            state.active_snapshot = Some(n as usize);
            state.snapshot_dirty = false;
            let scene = if state.device.is_mock() {
                state.mock_snapshots.get(n as usize - 1).cloned().flatten()
            } else {
                load_scene_file(&format!("Mix {n}"))
            };
            if let Some(scene) = scene {
                if let Err(e) = state.device.apply_scene(&scene) {
                    fail(state, "snapshot", &e);
                } else {
                    enforce_control_room_stereo(state);
                    state.host_scene = Some(state.device.capture_scene());
                }
            }
            notify_osc_raw(state, "/snapshot/active", vec![OscType::Int(n as i32)]);
        }
        Message::SnapshotStore => {
            if let Some(n) = state.active_snapshot {
                let scene = state
                    .host_scene
                    .clone()
                    .unwrap_or_else(|| state.device.capture_scene());
                let saved = if state.device.is_mock() {
                    if let Some(slot) = state.mock_snapshots.get_mut(n - 1) {
                        *slot = Some(scene);
                    }
                    Ok(())
                } else {
                    save_scene_file(&format!("Mix {n}"), &scene)
                };
                if let Err(e) = saved {
                    log::warn!("Failed to store snapshot 'Mix {n}': {e}");
                } else {
                    state.snapshot_dirty = false;
                    if let Some(slot) = state.stored_snapshots.get_mut(n - 1) {
                        *slot = true;
                    }
                    notify_osc_bool(state, format!("/snapshot/{n}/stored"), true);
                }
            }
        }
        Message::GroupEditToggle => state.group_editing = !state.group_editing,
        Message::GroupLinkToggle(idx, link) => {
            if let Some(g) = state.groups.get_mut(idx) {
                if state.group_editing {
                    // Assign the current multi-selection as this
                    // group's membership and engage this link type —
                    // the existing Ctrl/Shift-click selection is the
                    // whole "picker UI" here, deliberately, rather than
                    // a bespoke channel-assignment dialog.
                    g.members = state.selected.iter().copied().collect();
                    match link {
                        sidebar::GroupLink::Mute => g.mute_linked = true,
                        sidebar::GroupLink::Solo => g.solo_linked = true,
                        sidebar::GroupLink::Fader => g.fader_linked = true,
                    }
                } else {
                    match link {
                        sidebar::GroupLink::Mute => g.mute_linked = !g.mute_linked,
                        sidebar::GroupLink::Solo => g.solo_linked = !g.solo_linked,
                        sidebar::GroupLink::Fader => g.fader_linked = !g.fader_linked,
                    }
                }
            }
            notify_rme_page3(state);
        }
        Message::GroupClear => {
            state.groups = Default::default();
            state.group_editing = false;
            notify_rme_page3(state);
        }
        Message::LayoutClicked(n) => {
            state.active_layout = Some(n as usize);
            if let Some(set) = state.layouts.get((n - 1) as usize).cloned().flatten() {
                state.collapsed = set;
                // Bulk recall snaps instantly (matching Scene recall,
                // which doesn't animate faders either) — clear any
                // in-flight collapse animations so a stale one can't
                // fight the new target state.
                state.collapse_anim.clear();
            }
        }
        Message::LayoutStore => {
            if let Some(n) = state.active_layout {
                state.layouts[(n - 1) as usize] = Some(state.collapsed.clone());
                if let Err(e) = crate::layouts::save_layout_file(n as u8, &state.collapsed) {
                    log::warn!("Failed to store layout {n}: {e}");
                }
            }
        }
    }
    if let Some(before) = host_before {
        if state.device_service.is_some() {
            // The command only reaches the device thread; the mirror still
            // shows the old value.  Wait for the confirming snapshot.
            state.pending_host_merge = Some((before, Instant::now()));
        } else {
            merge_host_changes(state, before);
        }
    }
    Task::none()
}

/// Merge one host-originated change into the saved scene.  Only the fields
/// that differ between the two captures are taken, so a front-panel change
/// present in both is not persisted.
fn merge_host_changes(state: &mut TotalControl, before: Scene) {
    let after = state.device.capture_scene();
    // The UCX options word, references and 48 V/Inst/Level switches have no
    // readback, so a host change is the only record.  Gain is read back from
    // the state stream.  The UC reads its device group again at connect and
    // keeps the unit's values.
    let include_device = state.device.is_ucx_model();
    if let Some(host) = state.host_scene.as_mut() {
        host.merge_host_changes_with_device(&before, &after, include_device);
    } else {
        state.host_scene = Some(after);
    }
}

/// Swap in a newly opened device.  When the device service owns the
/// handle, stop the old thread, clear its bus and start a service for the
/// new unit before any caller touches the model.
fn clear_meters(state: &mut TotalControl) {
    for m in &mut state.input_meters {
        *m = MeterAnim::new();
    }
    for m in &mut state.playback_meters {
        *m = MeterAnim::new();
    }
    for m in &mut state.output_meters {
        *m = MeterAnim::new();
    }
    for m in &mut state.fx_meters {
        *m = MeterAnim::new();
    }
    state.fx_overload = [false, false];
}

fn replace_device_handle(state: &mut TotalControl, new: DeviceHandle) {
    let use_service = state.device_service.is_some() || crate::device_service::requested();
    state.device_service = None;
    state.device_updates = None;
    state.meter_bus = None;
    state.device = new;
    if use_service {
        attach_device_service(state);
    }
}

/// How long a failure stays on screen.
const NOTICE_FOR: Duration = Duration::from_secs(5);

/// Swap the whole backend at runtime: open the chosen device, rebuild the
/// meter vectors/tap for its channel counts, and carry the GUI prefs
/// across.  ALSA devices keep their own live matrix; the USB backend
/// restores its per-model auto scene.
fn switch_device(state: &mut TotalControl, target: DeviceTarget) -> Result<(), String> {
    if state
        .device_target
        .as_ref()
        .is_some_and(|current| *current == target)
        || (state.device_target.is_none() && state.device.kind() == target.kind)
    {
        return Ok(());
    }
    let old_has_serial = state
        .device_target
        .as_ref()
        .is_some_and(|target| target.serial.is_some());
    let old_is_uc = state.device.is_uc_model();
    let old_key = scene_key(state);
    if state.device.needs_scene_restore() {
        let scene = state.device.capture_scene();
        if old_has_serial || old_is_uc {
            let _ = totalcontrol_core::scene::save_auto_scene_for(&old_key, &scene);
        } else {
            let _ = totalcontrol_core::scene::save_auto_scene(&scene);
        }
    }
    let new = DeviceHandle::open_target(&target)
        .ok_or_else(|| format!("{} is not connected", target.label()))?;
    let prefs = prefs_of(state);
    replace_device_handle(state, new);
    state.device_target = Some(state.device.target());
    // The new unit has its own layout, topology and scene; never carry the
    // old device's host model into it.
    state.host_scene = None;
    // Drop every transient channel id from the old topology before the new
    // device is rendered.  A selected strip, open panel or queued pick-list
    // action can otherwise name e.g. UCX output 12 on a 12-output Babyface
    // and make the first frame index past the new output list.
    state.selected.clear();
    state.focus_channel = None;
    state.select_anchor = None;
    state.hovered_strip = None;
    state.matrix_hover = None;
    state.pending_flyout = None;
    state.flyout_anim.clear();
    state.collapse_anim.clear();
    state.stereo_anim.clear();
    state.assign_anim = None;
    state.assign_menu = AssignMenu::Closed;
    state.assign_pinned = false;
    state.control_room_settings_open = false;
    state.startup_mismatch = None;
    state.startup_compare_pending = false;
    state.startup_compare_scene = None;
    state.pending_host_merge = None;
    state.undo_stack.clear();
    state.redo_stack.clear();
    state.connected = state.device.is_connected();
    state.input_meters = vec![MeterAnim::new(); state.device.inputs().len()];
    state.playback_meters = vec![MeterAnim::new(); state.device.playbacks().len()];
    state.output_meters = vec![MeterAnim::new(); state.device.outputs().len()];
    state.fx_meters = [MeterAnim::new(), MeterAnim::new()];
    state.fx_overload = [false, false];
    state.playback_tap = if state.device.is_mock() || state.device.has_device_playback_meters() {
        pw_meter::PlaybackTap::disabled()
    } else {
        pw_meter::PlaybackTap::start()
    };
    state.playback_tap_channels = 0;
    state.sel_out = 0;
    state.quick_channel = ChannelId::Input(0);
    state.last_panel_out = None;
    state.renaming = None;
    state.editing = None;
    state.drag_range = None;
    state.last_saved_json = None;
    state.last_activity = Instant::now();
    state.startup_scene = None;
    state.startup_scene_due = None;
    state.last_sink_running = false;
    state.last_card_rate = None;
    apply_prefs(state, prefs);
    enforce_control_room_stereo(state);
    state.sel_out = state
        .sel_out
        .min(state.device.output_pair_count().saturating_sub(1));
    if state.device.needs_scene_restore() {
        if let Some(scene) = load_scene_for_current(state) {
            state.host_scene = Some(scene.clone());
            if state.device.apply_scene(&scene).is_ok() {
                enforce_control_room_stereo(state);
                state.host_scene = Some(state.device.capture_scene());
            }
        }
    }
    if state.host_scene.is_none() {
        state.host_scene = Some(state.device.capture_scene());
    }
    if let Err(e) = apply_control_room_assignments(state) {
        log::debug!("Control Room assignments not restored: {e}");
    }
    restart_surround_bridge(state);
    // The UC's input meters come from a PipeWire capture tap that is opened
    // only after the scene is applied; without this a live switch to the UC
    // leaves every input VU at zero.  The UCX uses the device meter endpoint,
    // for which this is a no-op.
    state.device.begin_metering();
    if state.engine_force_rate > 0 {
        state.device.set_meter_rate(state.engine_force_rate);
    }
    state.notice = Some((format!("Switched to {}", target.label()), Instant::now()));
    Ok(())
}

/// Reports a refused device write: logged, and shown briefly so a fader that
/// moves without effect is not silent. A drag repeats the same failure many
/// times a second; the same text only refreshes the timer.
fn fail(state: &mut TotalControl, what: &str, error: &dyn std::fmt::Display) {
    let what = what.trim_start_matches("set_").replace('_', " ");
    let text = format!("Could not apply {what}: {error}");
    log::warn!("{text}");
    state.notice = Some((text, Instant::now()));
}

/// Hides the window in the tray by closing it: the process, and with it the
/// ticks, the front panel's DIM and the reconnect logic, keeps running.
fn hide_window(state: &mut TotalControl) -> Task<Message> {
    state.window_hidden = true;
    match state.window_id.take() {
        Some(id) => window::close(id),
        None => Task::none(),
    }
}

/// Brings the window back: opens it again if it was hidden, else raises it.
fn show_window(state: &mut TotalControl) -> Task<Message> {
    state.window_hidden = false;
    state.last_activity = Instant::now();
    if let Some(id) = state.window_id {
        return Task::batch([window::minimize(id, false), window::gain_focus(id)]);
    }
    let size = state
        .window_size
        .map(|(w, h)| iced::Size::new(w.max(MIN_WINDOW.0), h.max(MIN_WINDOW.1)))
        .unwrap_or(iced::Size::new(1280.0, 800.0));
    let (id, open) = window::open(crate::window_settings(size, state.fullscreen));
    state.window_id = Some(id);
    Task::batch([open.discard(), window::gain_focus(id)])
}

/// A cheap summary of what the front panel can change behind the app's back
/// (output levels, mutes, the dim): when it differs after a poll, the panel
/// was touched. Not a hash of the whole mix: just enough to notice.
fn panel_fingerprint(state: &TotalControl) -> f64 {
    state
        .device
        .outputs()
        .iter()
        .enumerate()
        .map(|(i, o)| {
            (o.volume as f64 + if o.mute { 1000.0 } else { 0.0 }) * (1.0 + i as f64 * 0.01)
        })
        .sum::<f64>()
        + if state.device.settings().dim { 7777.0 } else { 0.0 }
}

/// The tick while something is being dragged or animated: 30 Hz.
const TICK_ACTIVE: Duration = Duration::from_millis(33);
/// The tick while only live meters are moving: 25 Hz.  The full view is
/// rebuilt on every tick, so this is the difference between ~37 % and ~25 %
/// of a core on a busy mixer; 25 Hz still reads as smooth VU motion.
const TICK_METERS: Duration = Duration::from_millis(40);
/// The tick while the window is hidden in the tray: 4 Hz.  Nothing on
/// screen can change, but the front panel, connection probe and autosave
/// still need to run.
const TICK_HIDDEN: Duration = Duration::from_millis(250);
/// The tick at rest. Every tick rebuilds and redraws the whole view, so at
/// rest it only has to keep the slow chores alive (connection probe,
/// auto-save, the front panel); anything that changes the screen switches
/// back to a faster tick at once.
const TICK_IDLE: Duration = Duration::from_millis(100);
/// Housekeeping cadence once the device service owns the meters: the
/// canvases redraw from the shared bus themselves, so the view only needs
/// an occasional pass for connection/autosave/OSC and non-canvas readouts.
const TICK_SERVICE: Duration = Duration::from_secs(1);
/// How long after the last change the tick stays fast: longer than the
/// meters' release, so a fading peak is drawn to the end.
const ACTIVE_FOR: Duration = Duration::from_millis(1500);

/// A drag or a layout animation needs the 30 Hz tick; live meters only
/// need the meter tick.  Keeping these separate stops a mixer that is
/// merely showing levels from paying the interaction frame rate.
fn needs_fast_redraw(state: &TotalControl) -> bool {
    state.pending_resize_width.is_some()
        || !state.collapse_anim.is_empty()
        || !state.flyout_anim.is_empty()
        || !state.stereo_anim.is_empty()
        || state.fx_anim.is_some()
        || state.drag_range.is_some()
}

/// Whether the screen has anything moving, i.e. the tick should be faster
/// than idle.
fn is_active(state: &TotalControl) -> bool {
    state.last_activity.elapsed() < ACTIVE_FOR || state.notice.is_some()
}

pub fn tick_interval(state: &TotalControl) -> Duration {
    if state.window_hidden {
        TICK_HIDDEN
    } else if needs_fast_redraw(state) {
        TICK_ACTIVE
    } else if is_active(state) {
        TICK_METERS
    } else if state.meter_bus.is_some()
        && (state.device.has_device_playback_meters() || state.device.is_mock())
        && state.osc_config.is_none()
    {
        TICK_SERVICE
    } else {
        TICK_IDLE
    }
}

pub fn subscription(state: &TotalControl) -> Subscription<Message> {
    let mut subs = vec![
        iced::time::every(tick_interval(state)).map(|_| Message::Tick),
        iced::event::listen_with(handle_global_event),
    ];
    // The device service publishes model snapshots only when something
    // changed.  This stream is the wake-up edge; the snapshots themselves
    // are drained on `Message::DeviceUpdated`.  Without it the tick would
    // have to poll the channel, which would rebuild the view for meters.
    if state.device_service.is_some() {
        subs.push(
            Subscription::run(crate::device_service::update_stream)
                .map(|_| Message::DeviceUpdated),
        );
        // Meter canvases schedule their own frames while a signal is
        // visible.  When every level and held peak has fallen, they stop;
        // this edge is what starts them again on the next transient.
        subs.push(
            Subscription::run(crate::device_service::meter_stream)
                .map(|_| Message::MeterWake),
        );
    }
    // Another app (or `pw-metadata` by hand) can change the graph clock;
    // re-read it now and then so the device card never shows a stale rate.
    if state.device.rate_follows_sound_server() {
        subs.push(iced::time::every(Duration::from_secs(3)).map(|_| Message::EnginePoll));
    }
    // Only running while a collapse/expand transition is actually in
    // flight — plain `column`/`container` widgets can't self-request a
    // redraw the way the canvas-based fader/meter animations do, so a
    // much faster timer stands in for that during the ~160ms transition,
    // then switches itself back off once `collapse_anim` empties out.
    if !state.collapse_anim.is_empty()
        || !state.flyout_anim.is_empty()
        || !state.stereo_anim.is_empty()
        || state.fx_anim.is_some()
        || state.assign_anim.is_some()
    {
        subs.push(iced::time::every(Duration::from_millis(8)).map(|_| Message::CollapseTick));
    }
    // The notification-area icon and second launches, see `tray`.
    subs.push(crate::tray::subscription().map(Message::Tray));
    // Only running when `--osc` was passed — see `osc.rs`.
    if let Some(config) = &state.osc_config {
        subs.push(Subscription::run_with(*config, osc::worker));
    }
    Subscription::batch(subs)
}

fn handle_global_event(
    event: iced::Event,
    status: iced::event::Status,
    id: window::Id,
) -> Option<Message> {
    match event {
        iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            // Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y. Left to a focused text field
            // (a rename or a typed dB value), which has its own undo.
            if modifiers.command() && status == iced::event::Status::Ignored {
                if let Key::Character(c) = &key {
                    match c.to_lowercase().as_str() {
                        "z" if modifiers.shift() => return Some(Message::Redo),
                        "z" => return Some(Message::Undo),
                        "y" => return Some(Message::Redo),
                        _ => {}
                    }
                }
            }
            // Ctrl+= / Ctrl+- / Ctrl+0 — manual zoom. Accept "=" and "+"
            // (shifted/unshifted) and "0"; requires Ctrl.
            if modifiers.contains(keyboard::Modifiers::CTRL) {
                if let Key::Character(c) = &key {
                    let zoom = match c.as_str() {
                        "=" | "+" => Some(Message::ZoomIn),
                        "-" | "_" => Some(Message::ZoomOut),
                        "0" => Some(Message::ZoomReset),
                        "," => Some(Message::ToggleSettings),
                        _ => None,
                    };
                    if zoom.is_some() {
                        return zoom;
                    }
                }
            }
            match key {
                Key::Named(keyboard::key::Named::ArrowUp)
                    if status == iced::event::Status::Ignored =>
                {
                    Some(Message::NudgeVolume(1.0))
                }
                Key::Named(keyboard::key::Named::ArrowDown)
                    if status == iced::event::Status::Ignored =>
                {
                    Some(Message::NudgeVolume(-1.0))
                }
                Key::Named(keyboard::key::Named::F11) => {
                    Some(Message::ToggleFullscreen(id))
                }
                Key::Named(keyboard::key::Named::Tab) => Some(Message::TabPressed),
                Key::Named(keyboard::key::Named::Escape) => Some(Message::EscapePressed),
                _ => None,
            }
        }
        iced::Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => {
            Some(Message::ModifiersChanged(m))
        }
        iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)) => {
            Some(Message::PointerPressed)
        }
        iced::Event::Mouse(iced::mouse::Event::WheelScrolled { delta, .. }) => {
            // Reported every wheel tick; `update` decides whether it's a
            // zoom (Ctrl held) or leaves it alone (plain scroll handled by
            // the widget tree).
            let y = match delta {
                iced::mouse::ScrollDelta::Lines { y, .. } => y,
                iced::mouse::ScrollDelta::Pixels { y, .. } => y,
            };
            Some(Message::WheelZoom(y))
        }
        iced::Event::Window(window::Event::Resized(size)) => {
            Some(Message::WindowSize(size.width, size.height))
        }
        iced::Event::Window(window::Event::CloseRequested) => Some(Message::CloseRequested),
        iced::Event::Window(window::Event::Closed) => Some(Message::WindowClosed(id)),
        iced::Event::Window(window::Event::Opened { size, .. }) => {
            Some(Message::WindowSize(size.width, size.height))
        }
        _ => None,
    }
}

// ── View ─────────────────────────────────────────────────────────

pub fn view(state: &TotalControl, _window: window::Id) -> Element<'_, Message> {
    let top = top_bar(state);
    let now = Instant::now();
    let fx_width = fx_slot(state, now);
    let (content, fx_overlay) = match state.view {
        View::Quick => (quick_view(state), false),
        View::Mixer => mixer_view(state, fx_width),
        View::Matrix => (matrix_view(state), false),
    };

    // Explicit Fill — a Shrink parent doesn't actually grant a Fill-sized
    // child the real window height for layout/hit-testing, even though
    // the raw window clear color visually fills the gap identically to
    // our own background (same near-black), making a real empty area
    // indistinguishable on screen from a genuinely non-interactive one.
    // That's what made `page()`'s click-to-clear-selection silently miss
    // every click below the shortest section's natural content height.
    // The sidebar runs the window's full height; the top bar, banner,
    // view and OSC drawer stack in the column beside it. (The bar used to
    // span the whole window above both, which left its right-hand tools
    // floating over the sidebar's own title, aligned with neither.)
    let mut main = column![top];
    if !state.connected {
        main = main.push(disconnected_banner(state));
    }
    if fx_overlay {
        let width = fx_width.unwrap_or(0.0);
        let panel = container(
            container(fx_column(state, width))
                .width(Length::Fixed(width))
                .height(Length::Fill)
                .style(theme::root),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::Alignment::End)
        .align_y(iced::Alignment::Start);
        main = main.push(
            stack(vec![content, panel.into()])
                .width(Length::Fill)
                .height(Length::Fill),
        );
    } else {
        main = main.push(content);
    }
    if state.show_osc_log {
        main = main.push(osc_log_panel(state));
    }
    let main = main.width(Length::Fill).height(Length::Fill);
    // TotalMix's own order across the window: mixer, FX column, control
    // strip. When the mixer cannot shrink far enough, the FX column is
    // drawn over the rightmost part of the mixer instead of squeezing it.
    let mut parts = row![main];
    if let Some(width) = fx_width {
        if !fx_overlay {
            parts = parts.push(fx_column(state, width));
        }
    }
    let mut base: Element<'_, Message> = parts
        .push(sidebar::sidebar(state))
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
    if state.show_device_panel {
        base = stack(vec![base, device_panel(state)])
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }
    if let Some((text, _)) = &state.notice {
        base = stack(vec![base, notice_toast(text, state.ui_scale)])
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }
    if state.options_menu != OptionsMenu::Closed {
        base = stack(vec![base, options_menu_layer(state)])
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }
    if state.show_settings {
        base = stack(vec![base, settings_panel(state)])
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }
    if state.control_room_settings_open {
        base = stack(vec![base, control_room_settings_modal(state)])
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }
    if state.show_about {
        // The About dialog rides above the whole window, drawers included:
        // it is a modal now, not one more docked panel (see
        // `about::about_modal`).
        base = stack(vec![base, about::about_modal(state)])
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }
    if state.startup_mismatch.is_some() {
        // RME's mismatch prompt is the topmost modal: it must be answered
        // before the mixer is used.
        stack(vec![base, sync_dialog::mismatch_modal(state)])
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    } else {
        base
    }
}

/// Shown under the top bar while the device is gone. The sidebar chip
/// already says "Disconnected", but that is a small word at the window's
/// edge, while the mixer underneath keeps looking live; a full-width line
/// says it where the eye is, and says what happens next.
fn disconnected_banner(state: &TotalControl) -> Element<'_, Message> {
    let scale = state.ui_scale;
    let model = state
        .device
        .model_name()
        .strip_suffix(" (mock)")
        .unwrap_or_else(|| state.device.model_name())
        .to_string();
    container(
        row![
            text("\u{25CF}").size(theme::TEXT_XS * scale).color(theme::MRED),
            text(format!(
                "{model} is disconnected. TotalControl reconnects on its own and restores the mix when it is back."
            ))
            .size(theme::TEXT_SM * scale)
            .color(theme::TEXT_PRIMARY),
        ]
        .spacing(theme::SPACE_MD * scale)
        .align_y(iced::Alignment::Center),
    )
    .padding([theme::SPACE_SM * scale, theme::SPACE_XXL * scale])
    .width(Length::Fill)
    .style(|_theme: &iced::Theme| container::Style {
        background: Some(iced::Background::Color(theme::blend(
            theme::SURFACE,
            theme::MRED,
            0.22,
        ))),
        ..container::Style::default()
    })
    .into()
}

/// A fixed-height drawer docked under the main view, listing raw OSC
/// traffic in both directions — the same idea as oscmix's own OSC debug
/// log window, just docked into the single-window layout instead of a
/// separate floating one (`totalcontrol-gui` doesn't use multi-window at all
/// elsewhere, so this stays consistent rather than being the one exception).
/// Newest line first, so the most recent activity is always visible at the
/// top without needing scroll-follow logic.
fn osc_log_panel(state: &TotalControl) -> Element<'_, Message> {
    let scale = state.ui_scale;

    let header = row![
        text("OSC DEBUG LOG")
            .color(theme::TEXT_PRIMARY)
            .size(theme::TEXT_SM * scale),
        text(format!("{} lines", state.osc_log.len()))
            .color(theme::TEXT_SEC)
            .size(theme::TEXT_XS * scale),
        iced::widget::Space::new().width(Length::Fill),
        iced::widget::button(text("Clear").size(theme::TEXT_XS * scale))
            .padding([theme::SPACE_SM * scale, theme::SPACE_MD * scale])
            .style(theme::plain_button)
            .on_press(Message::ClearOscLog),
        iced::widget::button(text("Close").size(theme::TEXT_XS * scale))
            .padding([theme::SPACE_SM * scale, theme::SPACE_MD * scale])
            .style(theme::plain_button)
            .on_press(Message::ToggleOscLog),
    ]
    .spacing(theme::SPACE_MD * scale)
    .align_y(iced::Alignment::Center);

    let body: Element<'_, Message> = if state.osc_log.is_empty() {
        text("No OSC traffic yet.")
            .color(theme::TEXT_SEC)
            .size(theme::TEXT_XS * scale)
            .into()
    } else {
        let mut lines = column![].spacing(1.0);
        for line in &state.osc_log {
            lines = lines.push(
                text(line.clone())
                    .font(iced::Font::MONOSPACE)
                    .color(theme::TEXT_SEC)
                    .size(theme::TEXT_XS * scale),
            );
        }
        scrollable(lines)
            .direction(scrollable::Direction::Vertical(theme::thin_scrollbar()))
            .height(Length::Fill)
            .style(theme::scrollable)
            .into()
    };

    container(
        column![header, body]
            .spacing(theme::SPACE_SM * scale)
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .style(theme::top_bar)
    .padding(theme::SPACE_MD * scale)
    .width(Length::Fill)
    .height(Length::Fixed(180.0 * scale))
    .into()
}

/// Settings that have no per-channel strip to live on (clock, sample
/// rate, pitch/width, the global protocol flags), as a popover card
/// beside the sidebar's device card, which opens it (or the folded rail's
/// gear). It was opened from a top-bar field captioned "Clock" first,
/// where nobody would look for the sample rate.
///
/// It used to be a full-width drawer docked under the main view: a strip
/// of loose rows pinned to the window's bottom-left corner, as far as the
/// window allows from the field that opened it. A card under that field,
/// closed by a click anywhere outside it (or by the field / Close), is
/// what the field promises, and sections give the flags a structure.
/// Reverb algorithm names by the measured index (`0x3c01`, capture H81).
const REVERB_TYPES: [&str; 15] = [
    "Small Room",
    "Medium Room",
    "Large Room",
    "Walls",
    "Shorty",
    "Attack",
    "Swagger",
    "Old School",
    "Echoistic",
    "8plus9",
    "Grand Wide",
    "Thicker",
    "Envelope",
    "Gated",
    "Space",
];

fn reverb_type_name(kind: u8) -> &'static str {
    REVERB_TYPES[(kind as usize) % REVERB_TYPES.len()]
}

const ECHO_TYPES: [&str; 3] = ["Stereo Echo", "Stereo Cross", "Pong Echo"];
const ECHO_HIGH_CUTS: [&str; 6] = ["Off", "16k", "12k", "8k", "4k", "2k"];

fn device_panel(state: &TotalControl) -> Element<'_, Message> {
    let scale = state.ui_scale;
    // Match Control Room Settings: 16 pt at the current UI scale.
    let text_size = 16.0 * scale;
    let settings = state.device.settings();
    let modifiers = state.modifiers;

    // Caption column shared by every labelled row, so the controls line up.
    // Wide enough for the longest device label, "Single speed word clock".
    const LABEL_W: f32 = 180.0;
    let section = |label: &'static str| -> Element<'static, Message> {
        row![
            text(label)
                .color(theme::TEXT_SEC)
                .size(text_size),
            iced::widget::rule::horizontal(1),
        ]
        .spacing(theme::SPACE_MD * scale)
        .align_y(iced::Alignment::Center)
        .into()
    };

    let header = row![
        text("Settings")
            .color(theme::TEXT_PRIMARY)
            .size(text_size),
        iced::widget::Space::new().width(Length::Fill),
        iced::widget::button(text("Close").size(text_size))
            .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
            .style(theme::ghost_button)
            .on_press(Message::ToggleDevicePanel),
    ]
    .align_y(iced::Alignment::Center);

    let mut body = column![header]
        .spacing(theme::SPACE_XL * scale)
        .width(Length::Fill);

    // One device-owned group for every model: identity, clock, sample rate
    // and the format switches the unit itself holds. `device_group` leaves
    // out the rows a model has no register for.
    body = body.push(device_group(state, LABEL_W));

    if state.device.rate_follows_sound_server() || state.device.is_mock() {
        body = body.push(engine_section(state, LABEL_W));
    }

    // NOTE: there used to be a "SPDIF: Enabled / Emphasis / Professional"
    // row here. Removed 2026-09-12: those were never Babyface Pro
    // features. They are generic IEC958 controls that `snd-usb-audio`
    // synthesizes for any class-compliant device with an S/PDIF
    // endpoint — RME's own Fireface USB Settings dialog for this card
    // offers no such options (its SPDIF settings are "Optical Out
    // ADAT/SPDIF" and "SPDIF In TMS"; see PROTOCOL.md's captured UI
    // reference). They reached TotalControl through the CC-mode ALSA surface,
    // and once `open()` started refusing CC-mode cards they were three
    // buttons that could not do anything on any backend. The real
    // optical-format control lives in the Global grid below.

    let caps = state.device.capabilities();
    if caps.protocol_features && !state.device.is_uc_model() {
        let knob_cell = |label: &'static str, k: Element<'static, Message>| -> Element<'static, Message> {
            column![
                k,
                text(label)
                    .color(theme::TEXT_SEC)
                    .size(text_size),
            ]
            .spacing(theme::SPACE_SM * scale)
            .align_x(iced::Alignment::Center)
            .width(Length::Fill)
            .into()
        };
        let mut output_row = row![].spacing(theme::SPACE_MD * scale);
        if caps.pitch {
            output_row = output_row.push(knob_cell(
                "Pitch",
                knob(Knob {
                    value: settings.pitch_percent,
                    range: (-5.0, 5.0),
                    label: format!("{:+.1}%", settings.pitch_percent),
                    arc_from_center: true,
                    level_arc_color: Some(theme::TEXT_PRIMARY),
                    interactive: true,
                    log_scale: false,
                    modifiers,
                    scale,
                    on_change: Box::new(|v| Message::PitchChanged(v.clamp(-5.0, 5.0))),
                    on_reset: Box::new(|| Message::PitchChanged(0.0)),
                }),
            ));
        }
        if caps.width {
            output_row = output_row.push(knob_cell(
                "Width",
                knob(Knob {
                    value: settings.width,
                    range: (-1.0, 1.0),
                    label: format!("{:+.2}", settings.width),
                    arc_from_center: true,
                    level_arc_color: Some(theme::TEXT_PRIMARY),
                    interactive: true,
                    log_scale: false,
                    modifiers,
                    scale,
                    on_change: Box::new(|v| Message::WidthChanged(v.clamp(-1.0, 1.0))),
                    on_reset: Box::new(|| Message::WidthChanged(0.0)),
                }),
            ));
        }
        if caps.pitch || caps.width {
            body = body.push(
                column![section("OUTPUT"), output_row].spacing(theme::SPACE_MD * scale),
            );
        }

        // The global flags as a two-column grid of equal toggles, the same
        // family as the sidebar's group cells.  Only include a toggle when
        // the backend has a path that reaches the device or a real host
        // mode; otherwise a press would only change cached state.
        let mut flags: Vec<(&'static str, bool, fn(bool) -> Message)> = Vec::new();
        if caps.ms_proc {
            flags.push(("MS Proc", settings.ms_proc, Message::MsProcChanged));
        }
        if caps.an12 {
            flags.push(("AN 1>2", settings.an12, Message::An12Changed));
        }
        if caps.input_link {
            flags.push(("Input Link", settings.input_link, Message::InputLinkChanged));
        }
        // The Fireface options word lives in the device group above; its
        // host-side modes stay here.
        if caps.eq_for_record && !state.device.is_uc_family() {
            flags.push(("EQ for Record", settings.eq_for_record, Message::EqForRecordChanged));
        }
        if caps.optical_out && !state.device.is_uc_family() {
            flags.push(("Opt Out: SPDIF", settings.optical_out_spdif, Message::OpticalOutFormatChanged));
        }
        if !flags.is_empty() {
            let mut grid = column![section("GLOBAL")].spacing(theme::SPACE_SM * scale);
            for pair in flags.chunks(2) {
                let mut line = row![].spacing(theme::SPACE_SM * scale);
                for &(label, active, on_toggle) in pair {
                    line = line.push(
                        iced::widget::button(text(label).size(text_size).center())
                            .padding([theme::SPACE_SM * scale, theme::SPACE_MD * scale])
                            .width(Length::Fill)
                            .style(theme::toggle_button(active, theme::ACCENT))
                            .on_press(on_toggle(!active)),
                    );
                }
                if pair.len() == 1 {
                    line = line.push(iced::widget::Space::new().width(Length::Fill));
                }
                grid = grid.push(line);
            }
            body = body.push(grid);
        }
    } else if !state.device.is_uc_model() {
        // Say why the card is shorter rather than leaving two knobs and
        // five toggles silently missing: on the class-compliant personality
        // everything below the sample rate lives in RME's own protocol.
        body = body.push(
            text(
                "Class Compliant mode: routing, preamps, gains and clock only. \
                 Pitch, Width, MS Proc, AN 1>2, Input Link, EQ for Record and \
                 the optical-out format are RME-protocol features, and the \
                 strips drop EQ, Trim, Phase \u{00d8}, Split and Loopback for the \
                 same reason.",
            )
            .color(theme::TEXT_SEC)
            .size(text_size),
        );
    }
    popover(state, body.into(), Message::ToggleDevicePanel, 520.0)
}

/// The card of a popover docked beside the sidebar, closing on `close` when
/// a click lands outside it.
fn popover<'a>(
    state: &'a TotalControl,
    body: Element<'a, Message>,
    close: Message,
    width: f32,
) -> Element<'a, Message> {
    let scale = state.ui_scale;
    let card = opaque(
        container(body)
            .style(theme::dialog)
            .padding(theme::SPACE_XL * scale)
            .width(Length::Fixed(width * scale)),
    );

    // One layer, not a catcher stacked under the card: in a `stack`, the
    // top layer's full-window container reports a mouse interaction, which
    // makes iced levitate the cursor for every layer below it, so a
    // catcher underneath never saw a click. Here the card (`opaque`)
    // captures its own clicks first, and `mouse_area` only acts on the
    // uncaptured ones: a click anywhere outside the card closes it, like a
    // menu. No dimming: it is a popover, not a modal.
    opaque(
        mouse_area(
            container(card)
                .align_x(iced::Alignment::End)
                .padding(iced::Padding {
                    top: POPOVER_TOP * scale,
                    // Beside the device card that opens it, just off the
                    // sidebar's edge.
                    right: theme::SPACE_MD * scale + sidebar_px(state),
                    bottom: 0.0,
                    left: 0.0,
                })
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .on_press(close),
    )
}

/// A refused device write, as a small card at the foot of the window. It
/// takes no clicks (a `Space`-sized layer would levitate the cursor for the
/// mixer below): the card alone is as big as its text.
fn notice_toast(message: &str, scale: f32) -> Element<'_, Message> {
    container(
        container(
            text(message.to_string())
                .size(theme::TEXT_SM * scale)
                .color(theme::TEXT_PRIMARY),
        )
        .padding([theme::SPACE_SM * scale, theme::SPACE_LG * scale])
        .style(theme::dialog),
    )
    .padding(theme::SPACE_XL * scale)
    .center_x(Length::Fill)
    .align_bottom(Length::Fill)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// The top-bar Options menu.  Root holds Reset Mix and Settings; Reset Mix
/// expands into the reset actions.
fn options_menu_layer(state: &TotalControl) -> Element<'_, Message> {
    let scale = state.ui_scale;
    let ts = scale * 1.15;
    let item = |label: String, message: Message| {
        iced::widget::button(text(label).size(theme::TEXT_XS * ts))
            .padding([theme::SPACE_SM * scale, theme::SPACE_MD * scale])
            .width(Length::Fill)
            .style(theme::ghost_button)
            .on_press(message)
    };
    let mut card = column![].spacing(0.0).width(Length::Fixed(250.0 * scale));
    match state.options_menu {
        OptionsMenu::Root => {
            card = card
                .push(item("Reset Mix  ▸".into(), Message::OptionsReset))
                .push(item("Settings…".into(), Message::OptionsSettings));
        }
        OptionsMenu::Reset => {
            card = card.push(item("‹  Reset Mix".into(), Message::OptionsBack));
            for (name, kind) in RESET_MIX {
                card = card.push(item(name.to_string(), Message::ResetMix(kind)));
            }
        }
        OptionsMenu::Closed => {}
    }
    let card = opaque(
        container(card)
            .style(theme::dialog)
            .padding(theme::SPACE_SM * scale),
    );
    opaque(
        mouse_area(
            container(card)
                .align_x(iced::Alignment::Start)
                .padding(iced::Padding {
                    top: theme::TOP_BAR_H * scale,
                    // Under the Options button: brand + view tabs + the
                    // fixed gaps before it in the top bar.
                    left: 390.0 * scale,
                    right: 0.0,
                    bottom: 0.0,
                })
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .on_press(Message::CloseOptions),
    )
}

/// The app's own settings (as opposed to the device's): how TotalControl behaves
/// around its window, not what the card does.
fn settings_panel(state: &TotalControl) -> Element<'_, Message> {
    let scale = state.ui_scale;
    let text_size = 16.0 * scale;
    let header = row![
        text("TotalControl settings")
            .color(theme::TEXT_PRIMARY)
            .size(text_size),
        iced::widget::Space::new().width(Length::Fill),
        iced::widget::button(text("Close").size(text_size))
            .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
            .style(theme::ghost_button)
            .on_press(Message::ToggleSettings),
    ]
    .align_y(iced::Alignment::Center);
    let section = row![
        text("BACKGROUND")
            .color(theme::TEXT_SEC)
            .size(text_size),
        iced::widget::rule::horizontal(1),
    ]
    .spacing(theme::SPACE_MD * scale)
    .align_y(iced::Alignment::Center);
    let toggle = |label: &'static str, active: bool, message: Message| {
        iced::widget::button(text(label).size(text_size).center())
            .padding([theme::SPACE_SM * scale, theme::SPACE_MD * scale])
            .width(Length::Fill)
            .style(theme::toggle_button(active, theme::ACCENT))
            .on_press(message)
    };
    let mut background = column![section].spacing(theme::SPACE_SM * scale);
    if state.tray_active {
        background = background.push(toggle(
            "Keep running in the tray when closed",
            state.keep_in_tray,
            Message::SetKeepInTray(!state.keep_in_tray),
        ));
    } else {
        background = background.push(
            text("No system tray found: closing the window quits.")
                .color(theme::TEXT_SEC)
                .size(text_size),
        );
    }
    background = background.push(toggle(
        "Start with the session, hidden in the tray",
        state.autostart,
        Message::SetAutostart(!state.autostart),
    ));
    let sync_section = row![
        text("STARTUP SYNC")
            .color(theme::TEXT_SEC)
            .size(text_size),
        iced::widget::rule::horizontal(1),
    ]
    .spacing(theme::SPACE_MD * scale)
    .align_y(iced::Alignment::Center);
    let surround_section = row![
        text("SURROUND")
            .color(theme::TEXT_SEC)
            .size(text_size),
        iced::widget::rule::horizontal(1),
    ]
    .spacing(theme::SPACE_MD * scale)
    .align_y(iced::Alignment::Center);
    let surround = column![
        surround_section,
        toggle(
            "Virtual 5.1 output",
            state.surround_51,
            Message::SetSurround51(!state.surround_51),
        ),
        toggle(
            "Virtual 7.1 output",
            state.surround_71,
            Message::SetSurround71(!state.surround_71),
        ),
        text("Active while TotalControl runs. Select the device in KDE or the player.")
            .color(theme::TEXT_SEC)
            .size(text_size),
    ]
    .spacing(theme::SPACE_SM * scale);
    let sync_help = text(if state.device.is_uc_model() {
        "The UC keeps input gain, reference levels, clock and rate. TotalControl writes only \
         its mixer scene (matrix, faders, mutes, loopback) on connect."
    } else {
        "On first connect: read the device where it has readback, compare it with the \
         saved TotalControl scene, and choose when they differ."
    })
    .color(theme::TEXT_SEC)
    .size(text_size);
    let sync_buttons = row![
        toggle(
            "Device → GUI",
            state.sync_direction == SyncDirection::DeviceToGui,
            Message::SetSyncDirection(SyncDirection::DeviceToGui),
        ),
        toggle(
            "GUI → device",
            state.sync_direction == SyncDirection::GuiToDevice,
            Message::SetSyncDirection(SyncDirection::GuiToDevice),
        ),
    ]
    .spacing(theme::SPACE_SM * scale);
    let always_init = toggle(
        "Always init the device with TotalControl settings",
        state.always_init_device,
        Message::SetAlwaysInitDevice(!state.always_init_device),
    );
    let sync = if state.device.is_uc_model() {
        column![sync_section, sync_help].spacing(theme::SPACE_SM * scale)
    } else {
        column![sync_section, sync_help, sync_buttons, always_init]
            .spacing(theme::SPACE_SM * scale)
    };
    let osc_section = row![
        text("OSC")
            .color(theme::TEXT_SEC)
            .size(text_size),
        iced::widget::rule::horizontal(1),
    ]
    .spacing(theme::SPACE_MD * scale)
    .align_y(iced::Alignment::Center);
    let osc_mode = row![
        text("Protocol")
            .color(theme::TEXT_SEC)
            .size(text_size),
        toggle(
            "Classic",
            state.osc_dialect == crate::osc::OscDialect::Classic,
            Message::SetOscMode(crate::osc::OscDialect::Classic),
        ),
        toggle(
            "Global",
            state.osc_dialect == crate::osc::OscDialect::Global,
            Message::SetOscMode(crate::osc::OscDialect::Global),
        ),
    ]
    .spacing(theme::SPACE_SM * scale)
    .align_y(iced::Alignment::Center);
    let osc_ports = row![
        text("Receive")
            .color(theme::TEXT_SEC)
            .size(text_size),
        text_input("9000", &state.osc_recv_text)
            .on_input(Message::OscRecvPortChanged)
            .width(Length::Fixed(90.0 * scale))
            .size(text_size),
        text("Send")
            .color(theme::TEXT_SEC)
            .size(text_size),
        text_input("9001", &state.osc_send_text)
            .on_input(Message::OscSendPortChanged)
            .width(Length::Fixed(90.0 * scale))
            .size(text_size),
        iced::widget::button(text("Apply").size(text_size))
            .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
            .style(theme::ghost_button)
            .on_press(Message::ApplyOscPorts),
    ]
    .spacing(theme::SPACE_SM * scale)
    .align_y(iced::Alignment::Center);
    let osc = column![
        osc_section,
        toggle(
            "Enable OSC control surface",
            state.osc_enabled,
            Message::SetOscEnabled(!state.osc_enabled),
        ),
        osc_mode,
        osc_ports,
        text("Loopback only. Classic uses the banked 1.96–2.11 address space; Global uses absolute channels and native values (TotalMix FX 2.1+).")
            .color(theme::TEXT_SEC)
            .size(text_size),
    ]
    .spacing(theme::SPACE_SM * scale);
    let body = column![header, background, sync, surround, osc]
        .spacing(theme::SPACE_XL * scale)
        .width(Length::Fill);
    popover(state, body.into(), Message::ToggleSettings, 520.0)
}

/// A sample rate as people say it: "48 kHz", "44.1 kHz", "176.4 kHz".
pub(crate) fn rate_label(hz: u32) -> String {
    if hz % 1000 == 0 {
        format!("{} kHz", hz / 1000)
    } else {
        format!("{:.1} kHz", hz as f32 / 1000.0)
    }
}

/// The sidebar's current width on screen, open or folded.
pub(crate) fn sidebar_px(state: &TotalControl) -> f32 {
    let logical = if state.sidebar_open {
        state.sidebar_width
    } else {
        sidebar::COLLAPSED_WIDTH
    };
    logical * sidebar_scale(state)
}

/// The device-owned group: identity, clock, sample rate and the format
/// switches the unit itself holds. Shown for every model that speaks RME's
/// protocol, so the Settings sheet has the same shape whichever unit is
/// attached.
///
/// Rows are the unit's own, not the app's: a model with no register for one
/// is left without it rather than shown inert. The UC keeps its Gain-tab
/// reference levels (`0x17`) and its options word; the UCX has no global
/// reference register (they are per channel) and its options word carries
/// the optical format, SPDIF professional, word-clock output rate and
/// termination, and EQ+D for Record (`docs/spec/UCX-CONTROL-SPEC.md` §11).
fn device_group(state: &TotalControl, label_w: f32) -> Element<'_, Message> {
    let scale = state.ui_scale;
    let text_size = 16.0 * scale;
    let settings = state.device.settings();
    let uc = state.device.is_uc_model();
    let ucx = state.device.is_ucx_model();
    let section = |label: &'static str| -> Element<'static, Message> {
        row![
            text(label)
                .color(theme::TEXT_SEC)
                .size(text_size),
            iced::widget::rule::horizontal(1),
        ]
        .spacing(theme::SPACE_MD * scale)
        .align_y(iced::Alignment::Center)
        .into()
    };
    let caption = |label: &'static str| {
        text(label)
            .color(theme::TEXT_SEC)
            .size(text_size)
            .width(Length::Fixed(label_w * scale))
    };
    let field = |p: iced::widget::PickList<'static, String, Vec<String>, String, Message>| {
        p.style(theme::pick_list)
            .menu_style(theme::menu)
            .padding([theme::SPACE_SM * scale, theme::SPACE_MD * scale])
            .text_size(text_size)
            .width(Length::Fill)
    };
    let ref_row = |label: &'static str,
                   options: [&'static str; 3],
                   codes: [u16; 3],
                   current: u16,
                   message: fn(u16) -> Message|
     -> Element<'static, Message> {
        let opts: Vec<String> = options.iter().map(|s| s.to_string()).collect();
        let selected = options
            .iter()
            .zip(codes)
            .find(|(_, c)| *c == current)
            .map(|(s, _)| s.to_string())
            .unwrap_or_else(|| options[0].to_string());
        let opts_for_pick = opts.clone();
        row![
            caption(label),
            field(pick_list(opts, Some(selected), move |s| {
                let idx = opts_for_pick.iter().position(|o| *o == s).unwrap_or(0);
                message(codes[idx])
            })),
        ]
        .spacing(theme::SPACE_MD * scale)
        .align_y(iced::Alignment::Center)
        .into()
    };
    let checkbox = |on: bool, message: Message| {
        iced::widget::button(
            text(if on { "\u{2713}" } else { "" })
                .size(text_size)
                .center(),
        )
        .padding(0)
        .width(Length::Fixed(SETTINGS_CONTROL_H * scale))
        .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
        .style(theme::toggle_button(on, theme::ACCENT))
        .on_press(message)
    };
    let check_row = |label: &'static str, on: bool, message: Message| {
        row![caption(label), checkbox(on, message)]
            .spacing(theme::SPACE_MD * scale)
            .align_y(iced::Alignment::Center)
    };
    let choice = |options: [&'static str; 2], current: bool, message: fn(bool) -> Message| {
        let list: Vec<String> = options.iter().map(|s| s.to_string()).collect();
        let selected = options[if current { 1 } else { 0 }].to_string();
        let second = options[1];
        field(pick_list(list, Some(selected), move |s| message(s == second)))
    };
    let choice_row =
        |label: &'static str, options: [&'static str; 2], current: bool, message: fn(bool) -> Message| {
            row![caption(label), choice(options, current, message)]
                .spacing(theme::SPACE_MD * scale)
                .align_y(iced::Alignment::Center)
        };

    let firmware = state
        .device
        .firmware()
        .map(|f| f.to_string())
        .unwrap_or_else(|| "\u{2014}".into());
    let serial = state
        .device
        .serial()
        .map(str::to_string)
        .unwrap_or_else(|| "\u{2014}".into());
    // The card's open stream is the device rate; while it is idle, show the
    // requested graph rate so the single picker does not snap back after a
    // selection.
    let device_rate = state
        .card_stream
        .map(|c| c.rate)
        .or_else(|| {
            state
                .engine
                .map(|e| if e.force_rate > 0 { e.force_rate } else { e.effective_rate() })
        })
        .unwrap_or(settings.sample_rate);

    // Every rate class the UC/UCX hardware provides.  The kernel keeps a
    // constant 18-channel ALSA node at all rates; at 2x/4x the USB packet
    // carries fewer words and the unused ADAT channels stay silent.
    const UC_RATES: [u32; 9] = [
        32_000, 44_100, 48_000, 64_000, 88_200, 96_000, 128_000, 176_400, 192_000,
    ];
    let rate_opts: Vec<String> = UC_RATES.iter().map(|r| rate_label(*r)).collect();
    let rate_for_pick = UC_RATES;
    let engine_rate_sel = if device_rate == 0 {
        rate_label(48_000)
    } else {
        rate_label(device_rate)
    };
    let engine_rate_row = row![
        caption("Sample rate"),
        field(pick_list(rate_opts, Some(engine_rate_sel), move |label| {
            let idx = UC_RATES
                .iter()
                .position(|r| rate_label(*r) == label)
                .unwrap_or(2);
            Message::EngineRate(rate_for_pick[idx])
        })),
    ]
    .spacing(theme::SPACE_MD * scale)
    .align_y(iced::Alignment::Center);
    // The UCX's own rate class list, used when the device owns the rate.
    let device_rate_row = row![
        caption("Sample rate"),
        pick_list(
            UC_RATES.to_vec(),
            Some(settings.sample_rate),
            Message::SampleRateSelected,
        )
        .style(theme::pick_list)
        .menu_style(theme::menu)
        .padding([theme::SPACE_SM * scale, theme::SPACE_MD * scale])
        .text_size(text_size)
        .width(Length::Fill),
    ]
    .spacing(theme::SPACE_MD * scale)
    .align_y(iced::Alignment::Center);

    let mut body = column![
        section("DEVICE"),
        text(if ucx {
            "Kept in the saved scene; written only when changed here."
        } else {
            "Read from the unit at connect; written only when changed here."
        })
        .color(theme::TEXT_SEC)
        .size(text_size),
        row![
            caption("Clock source"),
            field(pick_list(
                settings.clock_sources.clone(),
                Some(settings.clock_source.clone()),
                Message::ClockSourceSelected,
            )),
        ]
        .spacing(theme::SPACE_MD * scale)
        .align_y(iced::Alignment::Center),
    ];
    if uc {
        body = body.push(engine_rate_row);
    } else if !state.device.rate_follows_sound_server() && !state.device.is_mock() {
        body = body.push(device_rate_row);
    }
    if state.device.is_uc_family() {
        body = body.push(
            text(
                "32-192 kHz use the same 18-channel PipeWire node. Above 48 kHz the \
                 ADAT channels fall away (14 at 64-96 kHz, 12 at 128-192 kHz) and \
                 the remaining logical channels stay silent.",
            )
            .color(theme::TEXT_SEC)
            .size(text_size),
        );
    }
    if uc {
        // UC-only: the Gain tab's reference levels are one word (`0x17`).
        // The UCX sets them per channel, in the channel panels.
        body = body
            .push(ref_row("Line In", ["Lo Gain", "+4 dBu", "-10 dBV"], [0, 1, 3], settings.line_in_ref, Message::LineInRefSelected))
            .push(ref_row("Line Out", ["Hi Gain", "+4 dBu", "-10 dBV"], [2, 3, 1], settings.line_out_ref, Message::LineOutRefSelected))
            .push(ref_row("Phones", ["Hi Gain", "+4 dBu", "-10 dBV"], [1, 3, 2], settings.phones_ref, Message::PhonesRefSelected));
    }
    if uc || ucx {
        // Both options words carry the output format.  The UCX adds
        // word-clock input termination and EQ+D for Record (H67/H68/H75,
        // H71/H101).
        body = body
            .push(check_row(
                "Single speed word clock",
                settings.word_clock_single_speed,
                Message::WordClockSingleSpeedChanged(!settings.word_clock_single_speed),
            ))
            .push(choice_row(
                "Optical",
                ["ADAT", "SPDIF"],
                settings.optical_out_spdif,
                Message::OpticalOutFormatChanged,
            ))
            .push(choice_row(
                "SPDIF",
                ["Consumer", "Professional"],
                settings.spdif_professional,
                Message::SpdifProfessionalChanged,
            ));
        if ucx {
            body = body
                .push(check_row(
                    "Word clock in",
                    settings.word_clock_termination,
                    Message::WordClockTerminationChanged(!settings.word_clock_termination),
                ))
                .push(check_row(
                    "EQ for Record",
                    settings.eq_for_record,
                    Message::EqForRecordChanged(!settings.eq_for_record),
                ));
        }
    }
    if state.device.firmware().is_some() {
        body = body.push(
            row![
                caption("Firmware"),
                text(firmware)
                    .color(theme::TEXT_PRIMARY)
                    .size(text_size),
            ]
            .spacing(theme::SPACE_MD * scale)
            .align_y(iced::Alignment::Center),
        );
    }
    if state.device.serial().is_some() {
        body = body.push(
            row![
                caption("Serial"),
                text(serial)
                    .color(theme::TEXT_PRIMARY)
                    .size(text_size),
            ]
            .spacing(theme::SPACE_MD * scale)
            .align_y(iced::Alignment::Center),
        );
    }
    body.spacing(theme::SPACE_MD * scale)
        .width(Length::Fill)
        .into()
}

fn engine_section(state: &TotalControl, label_w: f32) -> Element<'_, Message> {
    let scale = state.ui_scale;
    let text_size = 16.0 * scale;
    let caption = |label: &'static str| {
        text(label)
            .color(theme::TEXT_SEC)
            .size(text_size)
            .width(Length::Fixed(label_w * scale))
    };
    let heading = row![
        text(if state.device.is_mock() {
            "AUDIO ENGINE \u{00b7} PIPEWIRE (SIMULATED)"
        } else {
            "AUDIO ENGINE \u{00b7} PIPEWIRE"
        })
        .color(theme::TEXT_SEC)
        .size(text_size),
        iced::widget::rule::horizontal(1),
    ]
    .spacing(theme::SPACE_MD * scale)
    .align_y(iced::Alignment::Center);

    let Some(clock) = state.engine else {
        return column![
            heading,
            text(
                "PipeWire is not running: the sample rate and buffer size are \
                 set by whatever opens the card.",
            )
            .color(theme::TEXT_SEC)
            .size(text_size),
        ]
        .spacing(theme::SPACE_MD * scale)
        .width(Length::Fill)
        .into();
    };

    // "Auto" is PipeWire's own value (it may differ per graph); naming it
    // keeps the default visible next to the overrides.
    const RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
    // 48 and 96 are 1 and 2 ms at 48 kHz, whole USB packets; PipeWire takes
    // quanta that are not powers of two. 16 and 24 only when the driver can
    // run them: its period floor is one URB, `urb_frames_min` (32 unless
    // the module is loaded lower). Measured 2026-10-21 (the driver's
    // KERNEL-DRIVER.md): 10-minute real-time soaks at 16 and 24 ran clean
    // once loaded with `urb_frames_min=8`; 8 still loses capture buffers,
    // so it is never offered.
    const ALL_QUANTA: [u32; 11] = [16, 24, 32, 48, 64, 96, 128, 256, 512, 1024, 2048];
    let floor = state.driver_floor.unwrap_or(32);
    let quanta: Vec<u32> = ALL_QUANTA.iter().copied().filter(|q| *q >= floor).collect();
    let rate_opts: Vec<String> = std::iter::once(format!("Auto ({})", rate_label(clock.rate)))
        .chain(RATES.iter().map(|r| rate_label(*r)))
        .collect();
    let rate_sel = if clock.force_rate == 0 {
        rate_opts[0].clone()
    } else {
        rate_label(clock.force_rate)
    };
    // "Auto" without a number: the card follows the graph's quantum.
    // The latency line below shows the resulting hardware buffer and chain.
    let quantum_opts: Vec<String> = std::iter::once("Auto".to_string())
        .chain(quanta.iter().map(|q| format!("{q} samples")))
        .collect();
    let quantum_sel = if clock.force_quantum == 0 {
        quantum_opts[0].clone()
    } else {
        format!("{} samples", clock.force_quantum)
    };
    let field = |p: iced::widget::PickList<'static, String, Vec<String>, String, Message>| {
        p.style(theme::pick_list)
            .menu_style(theme::menu)
            .padding([theme::SPACE_SM * scale, theme::SPACE_MD * scale])
            .text_size(text_size)
            .width(Length::Fill)
    };
    let rate_list = rate_opts.clone();
    let rate_pick = field(pick_list(rate_opts, Some(rate_sel), move |label| {
        let idx = rate_list.iter().position(|l| *l == label).unwrap_or(0);
        Message::EngineRate(if idx == 0 { 0 } else { RATES[idx - 1] })
    }));
    let quantum_list = quantum_opts.clone();
    let quanta_for_pick = quanta.clone();
    let quantum_pick = field(pick_list(quantum_opts, Some(quantum_sel), move |label| {
        let idx = quantum_list.iter().position(|l| *l == label).unwrap_or(0);
        Message::EngineQuantum(if idx == 0 { 0 } else { quanta_for_pick[idx - 1] })
    }));

    let mut body = column![heading];
    // The UC's single sample-rate control lives in the DEVICE group above.
    // This section is the host/PipeWire graph: buffer and latency only.
    if !state.device.is_uc_model() {
        body = body.push(
            row![caption("Sample rate"), rate_pick]
                .spacing(theme::SPACE_MD * scale)
                .align_y(iced::Alignment::Center),
        );
    }
    body = body.push(
        row![caption("Quantum"), quantum_pick]
            .spacing(theme::SPACE_MD * scale)
            .align_y(iced::Alignment::Center),
    );
    body = body.push(
        row![caption("Latency"), latency_readout(state, clock)]
            .spacing(theme::SPACE_MD * scale)
            .align_y(iced::Alignment::Center),
    );
    if let (Some(now), Some(base)) = (state.transport_counters, state.transport_baseline) {
        let delta = transport_delta(&now, &base);
        body = body.push(
            row![
                caption("Errors"),
                iced::widget::Space::new().width(Length::Fill),
                button(text("reset").size(theme::TEXT_XS * scale))
                    .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                    .style(theme::ghost_button)
                    .on_press(Message::ResetTransportErrors),
            ]
            .spacing(theme::SPACE_MD * scale)
            .align_y(iced::Alignment::Center),
        );
        // One row per fault type, named for the transport stage.  "xruns"
        // is the host/PipeWire side and needs its own driver counter before
        // it can be shown here; it is not inferred from these.
        for (label, value, bad) in [
            (
                "usb",
                format!("{} / {}", delta.in_status_errors, delta.out_status_errors),
                delta.in_status_errors + delta.out_status_errors,
            ),
            ("urb", delta.urb_errors.to_string(), delta.urb_errors),
            (
                "gaps",
                format!("{} / {}", delta.in_gaps, delta.out_gaps),
                delta.in_gaps + delta.out_gaps,
            ),
            (
                "short",
                format!("{} / {}", delta.in_shorts, delta.out_shorts),
                delta.in_shorts + delta.out_shorts,
            ),
            ("recover", delta.recoveries.to_string(), delta.recoveries),
            ("events", delta.events_dropped.to_string(), delta.events_dropped),
        ] {
            let colour = if bad == 0 {
                theme::TEXT_SEC
            } else {
                Color::from_rgb8(0xe0, 0x5a, 0x5a)
            };
            body = body.push(
                row![
                    text(label).size(text_size).color(theme::TEXT_SEC),
                    iced::widget::Space::new().width(Length::Fill),
                    text(value).size(text_size).color(colour),
                ]
                .spacing(theme::SPACE_MD * scale)
                .align_y(iced::Alignment::Center),
            );
        }
    }
    if let Some((percent, db)) = state.host_volume {
        let colour = if db < -0.5 {
            theme::YSIM
        } else {
            theme::TEXT_PRIMARY
        };
        body = body.push(
            row![
                caption("Host volume"),
                text(format!("{percent}% ({db:.1} dB)"))
                    .size(text_size)
                    .color(colour),
                iced::widget::Space::new().width(Length::Fill),
                button(text("0 dB").size(theme::TEXT_XS * scale))
                    .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                    .style(theme::ghost_button)
                    .on_press(Message::HostVolumeUnity),
            ]
            .spacing(theme::SPACE_MD * scale)
            .align_y(iced::Alignment::Center),
        );
    }
    body.spacing(theme::SPACE_MD * scale)
    .width(Length::Fill)
    .into()
}

/// Per-field difference of two cumulative driver counter readings.
fn transport_delta(
    now: &totalcontrol_core::fireface::abi::HwdepCounters,
    base: &totalcontrol_core::fireface::abi::HwdepCounters,
) -> totalcontrol_core::fireface::abi::HwdepCounters {
    use totalcontrol_core::fireface::abi::HwdepCounters;
    HwdepCounters {
        out_urbs: now.out_urbs.saturating_sub(base.out_urbs),
        out_gaps: now.out_gaps.saturating_sub(base.out_gaps),
        out_shorts: now.out_shorts.saturating_sub(base.out_shorts),
        out_status_errors: now
            .out_status_errors
            .saturating_sub(base.out_status_errors),
        urb_errors: now.urb_errors.saturating_sub(base.urb_errors),
        recoveries: now.recoveries.saturating_sub(base.recoveries),
        events_dropped: now.events_dropped.saturating_sub(base.events_dropped),
        meter_urbs: now.meter_urbs.saturating_sub(base.meter_urbs),
        state_urbs: now.state_urbs.saturating_sub(base.state_urbs),
        status_urbs: now.status_urbs.saturating_sub(base.status_urbs),
        in_urbs: now.in_urbs.saturating_sub(base.in_urbs),
        in_gaps: now.in_gaps.saturating_sub(base.in_gaps),
        in_shorts: now.in_shorts.saturating_sub(base.in_shorts),
        in_status_errors: now.in_status_errors.saturating_sub(base.in_status_errors),
    }
}


/// The latency line.  While the card plays, it uses the card's actual period
/// and the USB queue from the module parameters.  The UCX line is a
/// round-trip estimate; the UC line is a one-way output budget.
fn latency_readout(state: &TotalControl, clock: PwClock) -> Element<'_, Message> {
    let scale = state.ui_scale;
    let line = |s: String, c: Color| text(s).color(c).size(theme::TEXT_XS * scale);
    match state.card_stream {
        Some(card) => {
            let mut col = column![].spacing(theme::SPACE_TIGHT * scale);
            if state.device.is_uc_model() {
                if let Some(queue) = state.driver_queue {
                    let (usb, endpoint) = uc_usb_frames(card.rate, queue);
                    let chain = LatencyChain {
                        ring: card.buffer,
                        period: card.period,
                        usb,
                        device: endpoint,
                        converter: converter_frames(card.rate),
                        rate: card.rate,
                        device_label: "endpoint",
                    };
                    col = col.push(line(chain.summary(), theme::TEXT_PRIMARY));
                    col = col.push(line(chain.breakdown(), theme::TEXT_SEC));
                } else {
                    col = col.push(line(
                        ring_text(card.buffer, card.period, card.rate),
                        theme::TEXT_PRIMARY,
                    ));
                }
            } else if state.device.is_ucx_model() {
                if let Some(queue) = state.driver_queue {
                    let (usb, lead) = ucx_usb_frames(card.rate, queue);
                    let rtl = RtlEstimate {
                        usb,
                        lead,
                        converter: converter_frames(card.rate),
                        graph: card.period,
                        rate: card.rate,
                    };
                    col = col.push(line(rtl.summary(), theme::TEXT_PRIMARY));
                    col = col.push(line(rtl.breakdown(), theme::TEXT_SEC));
                } else {
                    col = col.push(line(
                        ring_text(card.buffer, card.period, card.rate),
                        theme::TEXT_PRIMARY,
                    ));
                }
            } else {
                col = col.push(line(
                    ring_text(card.buffer, card.period, card.rate),
                    theme::TEXT_PRIMARY,
                ));
            }
            // Only when the user asked for a size and the card ignores it
            // (a fixed period in a WirePlumber rule); "Auto" follows clients.
            if clock.force_quantum != 0 && card.period != clock.effective_quantum() {
                col = col.push(line(
                    "The card keeps its own fixed period.".into(),
                    theme::YSIM,
                ));
            }
            col.into()
        }
        None => {
            let rate = clock.effective_rate();
            let quantum = clock.effective_quantum().max(1);
            let minimum = state
                .driver_queue
                .map(|queue| {
                    if state.device.is_uc_model() {
                        uc_min_buffer_frames(rate, queue)
                    } else if state.device.is_ucx_model() {
                        ucx_min_buffer_frames(rate, queue)
                    } else {
                        0
                    }
                })
                .unwrap_or(0);
            let periods = if minimum > 0 {
                minimum.div_ceil(quantum).max(2)
            } else {
                2
            };
            let buffer = quantum.saturating_mul(periods);
            if state.device.is_uc_model() {
                if let Some(queue) = state.driver_queue {
                    let (usb, endpoint) = uc_usb_frames(rate, queue);
                    let chain = LatencyChain {
                        ring: buffer,
                        period: quantum,
                        usb,
                        device: endpoint,
                        converter: converter_frames(rate),
                        rate,
                        device_label: "endpoint",
                    };
                    return column![
                        line(chain.summary(), theme::TEXT_PRIMARY),
                        line(chain.breakdown(), theme::TEXT_SEC),
                    ]
                    .spacing(theme::SPACE_TIGHT * scale)
                    .into();
                }
                return line(ring_text(buffer, quantum, rate), theme::TEXT_PRIMARY).into();
            }
            if state.device.is_ucx_model() {
                if let Some(queue) = state.driver_queue {
                    let (usb, lead) = ucx_usb_frames(rate, queue);
                    let rtl = RtlEstimate {
                        usb,
                        lead,
                        converter: converter_frames(rate),
                        graph: quantum,
                        rate,
                    };
                    return column![
                        line(rtl.summary(), theme::TEXT_PRIMARY),
                        line(rtl.breakdown(), theme::TEXT_SEC),
                    ]
                    .spacing(theme::SPACE_TIGHT * scale)
                    .into();
                }
                return line(ring_text(buffer, quantum, rate), theme::TEXT_PRIMARY).into();
            }
            line(
                format!("{:.1} ms per buffer", clock.latency_ms()),
                theme::TEXT_PRIMARY,
            )
            .into()
        }
    }
}

/// One-way output latency: ALSA ring, USB queue, device stage and output
/// converter.  The ring total is the period count times the selected
/// quantum, so a 128-sample quantum is a 256-frame ring at two periods.
struct LatencyChain {
    ring: u32,
    period: u32,
    usb: u32,
    device: u32,
    converter: u32,
    rate: u32,
    device_label: &'static str,
}

impl LatencyChain {
    fn total(&self) -> u32 {
        self.ring + self.usb + self.device + self.converter
    }

    fn summary(&self) -> String {
        format!(
            "{:.1} ms one-way output latency",
            frames_ms(self.total(), self.rate)
        )
    }

    fn breakdown(&self) -> String {
        format!(
            "{} + {:.1} ms USB + {:.1} ms {} + {:.1} ms converter",
            ring_text(self.ring, self.period, self.rate),
            frames_ms(self.usb, self.rate),
            frames_ms(self.device, self.rate),
            self.device_label,
            frames_ms(self.converter, self.rate)
        )
    }
}

/// Round-trip estimate for the UCX: USB queue, device lead, converter and one
/// graph period.  The measured values are in the latency work note; this
/// estimate is conservative by about 1.2 ms.
struct RtlEstimate {
    usb: u32,
    lead: u32,
    converter: u32,
    graph: u32,
    rate: u32,
}

impl RtlEstimate {
    fn total(&self) -> u32 {
        self.usb + self.lead + self.converter + self.graph
    }

    fn summary(&self) -> String {
        format!(
            "{:.1} ms RTL estimate (±1.2 ms)",
            frames_ms(self.total(), self.rate)
        )
    }

    fn breakdown(&self) -> String {
        format!(
            "{:.1} ms USB + {:.1} ms device lead + {:.1} ms converter + {:.1} ms graph",
            frames_ms(self.usb, self.rate),
            frames_ms(self.lead, self.rate),
            frames_ms(self.converter, self.rate),
            frames_ms(self.graph, self.rate)
        )
    }
}

/// The ALSA ring as time and frames, with the period composition that
/// explains a total larger than the selected quantum.
fn ring_text(frames: u32, period: u32, rate: u32) -> String {
    let periods = if period > 0 { frames / period } else { 0 };
    if periods >= 2 && periods * period == frames {
        format!(
            "{:.1} ms ring ({} frames = {} × {})",
            frames_ms(frames, rate),
            frames,
            periods,
            period
        )
    } else {
        format!("{:.1} ms ring ({} frames)", frames_ms(frames, rate), frames)
    }
}

/// Frames covered by the UC's host queue and endpoint at the current rate.
fn uc_usb_frames(rate: u32, (frames, urbs): (u32, u32)) -> (u32, u32) {
    let factor = if rate > 128_000 {
        4
    } else if rate > 64_000 {
        2
    } else {
        1
    };
    let per_urb = frames * factor;
    (per_urb * urbs, per_urb)
}

/// Frames the ALSA ring needs: the USB queue plus two URBs of margin.
fn uc_min_buffer_frames(rate: u32, queue: (u32, u32)) -> u32 {
    let (usb, endpoint) = uc_usb_frames(rate, queue);
    usb + 2 * endpoint
}

/// Frames in one UCX USB URB: up to eight 1 ms ISO packets of
/// `ceil(rate / 8000)` frames each.
fn ucx_urb_frames(rate: u32, urb_frames: u32) -> u32 {
    let iso_packets = 8.min(urb_frames.max(1).div_ceil(6));
    iso_packets * rate.max(1).div_ceil(8000)
}

/// Frames covered by the UCX's host queue and device lead at the current
/// rate.  The queued depth is `nurbs + 1` URBs (`fb_delay`), and the unit
/// adds a 60-frame output lead.
fn ucx_usb_frames(rate: u32, (urb_frames, urbs): (u32, u32)) -> (u32, u32) {
    ((urbs + 1) * ucx_urb_frames(rate, urb_frames), 60)
}

/// Frames the UCX's ALSA ring needs: `nurbs + 2` URBs of margin.
fn ucx_min_buffer_frames(rate: u32, (urb_frames, urbs): (u32, u32)) -> u32 {
    (urbs + 2) * ucx_urb_frames(rate, urb_frames)
}

/// Output converter delay in frames: half of the measured converter
/// round-trip (25 frames at 1x and 2x, 23 at 4x).  The value is measured on
/// the UCX and assumed to be the same on the UC.
fn converter_frames(rate: u32) -> u32 {
    let round_trip: u32 = if rate > 128_000 { 23 } else { 25 };
    round_trip.div_ceil(2)
}

fn frames_ms(frames: u32, rate: u32) -> f32 {
    frames as f32 * 1000.0 / rate.max(1) as f32
}

/// Where the device popover's card starts, from the window top at scale 1:
/// level with the sidebar's device card, which opens it.
const POPOVER_TOP: f32 = theme::TOP_BAR_H;

/// A section label (HARDWARE INPUTS, SOFTWARE PLAYBACK, ...) with an accent
/// tick and a rule trailing off to the right, instead of bare gray text that
/// blends into the background.
fn section_header(label: &str, scale: f32) -> Element<'_, Message> {
    // The tick opens the line, the block closes it: the pair marks exactly
    // which channels the section holds.
    let block = || {
        container(
            iced::widget::Space::new()
                .width(3.0 * scale)
                .height(12.0 * scale),
        )
        .style(theme::accent_bar)
    };
    row![
        block(),
        text(label)
            .color(theme::TEXT_PRIMARY)
            .size(theme::TEXT_MD * scale),
        iced::widget::rule::horizontal(1),
        block(),
    ]
    .spacing(theme::SPACE_LG)
    .align_y(iced::Alignment::Center)
    .into()
}

/// Wraps a view's body in the root background, filling the window, with a
/// vertical scrollbar for when the stacked sections (or the matrix grid)
/// don't fit the window height.
fn page<'a>(body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    mouse_area(
        container(
            // One scroll area for the whole view.  A narrow window must not
            // squeeze any row out of the layout: the three mixer rows scroll
            // horizontally together, with the scrollbar at the bottom, and
            // vertically when the content is taller than the window.
            scrollable(body)
                .direction(scrollable::Direction::Both {
                    vertical: theme::thin_scrollbar(),
                    horizontal: theme::thin_scrollbar(),
                })
                .width(Length::Fill)
                // Fill the window, not the content height: this keeps the
                // horizontal scrollbar pinned to the bottom of the window
                // instead of sitting under the last mixer row.
                .height(Length::Fill)
                .style(theme::scrollable),
        )
        .style(theme::root)
        .padding([theme::SPACE_LG, theme::SPACE_XL])
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .on_press(Message::ClearSelection)
    .into()
}

fn top_bar(state: &TotalControl) -> Element<'_, Message> {
    let scale = state.ui_scale;

    // The device identity chip (name + connected/simulated status) used
    // to live here — removed as a straight duplicate now that the
    // sidebar's own device chip shows the same thing (see
    // `sidebar::device_chip`, which absorbed the status dot/label this
    // one used to carry). Everything else in this bar (view tabs, the
    // Scene/Submix/Clock session tools) has no sidebar equivalent, so it
    // stays — TotalMix's own control strip doesn't replace *this* bar's
    // job, and neither does ours.

    // View switch: a plain segmented toggle, not a chip — it's navigation,
    // not a status readout, so it shouldn't carry the same visual weight
    // as the identity chip. Both labels are always visible and clickable
    // (previously only the active view's name showed, with no click
    // target — Tab-key was the only way to switch).
    let tab_toggle = row![
        iced::widget::button(text("QUICK").size(theme::TEXT_MD * scale))
            .padding([theme::SPACE_SM * scale, theme::SPACE_XL * scale])
            .style(theme::tab_toggle(state.view == View::Quick))
            .on_press(Message::SetView(View::Quick)),
        iced::widget::button(text("MIXER").size(theme::TEXT_MD * scale))
            .padding([theme::SPACE_SM * scale, theme::SPACE_XL * scale])
            .style(theme::tab_toggle(state.view == View::Mixer))
            .on_press(Message::SetView(View::Mixer)),
        iced::widget::button(text("MATRIX").size(theme::TEXT_MD * scale))
            .padding([theme::SPACE_SM * scale, theme::SPACE_XL * scale])
            .style(theme::tab_toggle(state.view == View::Matrix))
            .on_press(Message::SetView(View::Matrix)),
    ]
    .spacing(theme::SPACE_TIGHT);

    // Secondary session tools: submix / clock. Scene save/load used to
    // live here too — moved into the sidebar's Snapshots panel (see
    // `sidebar::snapshots_panel`'s own comment), since it's the exact
    // same `save_scene_file`/`load_scene_file` mechanism as the Mix 1-8
    // slots there, just with free-form names instead of fixed ones —
    // no reason to keep two separate homes for one feature.
    // Flat fields, like everything else in the bar: this was the bar's
    // only boxed element (a `chip`), and read as dropped into the corner.
    let out_labels = state.display_pair_labels();
    let out_labels_pick = out_labels.clone();
    let display_to_internal = state.device.output_display_order();
    let selected_pair_label = state
        .pair_labels()
        .get(state.sel_out)
        .cloned()
        .unwrap_or_default();
    let current_target = state
        .device_target
        .clone()
        .unwrap_or_else(|| state.device.target());
    let mut targets = DeviceHandle::available_targets();
    if !targets.contains(&current_target) {
        targets.push(current_target.clone());
    }
    let device_labels: Vec<String> = targets.iter().map(DeviceTarget::label).collect();
    let targets_for_pick = targets.clone();
    let session = row![
            text("Device")
                .color(theme::TEXT_SEC)
                .size(theme::TEXT_XS * scale),
            pick_list(
                device_labels,
                Some(current_target.label()),
                move |label| {
                    let target = targets_for_pick
                        .iter()
                        .find(|target| target.label() == label)
                        .cloned()
                        .unwrap_or_else(|| current_target.clone());
                    Message::SelectDevice(target)
                },
            )
            .style(theme::pick_list)
            .menu_style(theme::menu)
            .padding([theme::SPACE_SM * scale, theme::SPACE_MD * scale])
            .text_size(theme::TEXT_MD * scale),
            text("Submix")
                .color(theme::TEXT_SEC)
                .size(theme::TEXT_XS * scale),
            pick_list(
                out_labels,
                Some(selected_pair_label.clone()),
                move |label| {
                    let display = out_labels_pick
                        .iter()
                        .position(|l| *l == label)
                        .unwrap_or(0);
                    Message::SelectOutput(display_to_internal.get(display).copied().unwrap_or(display))
                },
            )
            .style(theme::pick_list)
            .menu_style(theme::menu)
            .padding([theme::SPACE_SM * scale, theme::SPACE_MD * scale])
            .text_size(theme::TEXT_MD * scale),

    ]
    .spacing(theme::SPACE_SM * scale)
    .align_y(iced::Alignment::Center);

    // The mark and the wordmark as one lockup, the same artwork the window
    // icon wears (see `icon::mark`), flat white like every other engaged
    // state and sized off the wordmark's own height. A missing asset just
    // falls back to the plain wordmark.
    let brand_row: Element<'_, Message> = match crate::icon::mark() {
        Some((handle, ratio)) => {
            let mark_h = 20.0 * scale;
            row![
                iced::widget::image(handle)
                    .width(mark_h * ratio)
                    .height(mark_h)
                    .filter_method(iced::widget::image::FilterMethod::Linear),
                text("TotalControl")
                    .color(theme::ACCENT)
                    .size(theme::TEXT_XL * scale),
            ]
            .spacing(theme::SPACE_SM * scale)
            .align_y(iced::Alignment::Center)
            .into()
        }
        None => text("TotalControl")
            .color(theme::ACCENT)
            .size(theme::TEXT_XL * scale)
            .into(),
    };

    // The lockup is the way into About: the product's face opens the
    // product's own dialog, which is also why the separate "About" button
    // that used to sit at the far right of this bar is gone. A ghost
    // button keeps it looking like the wordmark at rest and gives it a
    // hover target and a pointer cursor.
    let brand = iced::widget::button(brand_row)
        .padding([theme::SPACE_TIGHT * scale, theme::SPACE_SM * scale])
        .style(theme::ghost_button)
        .on_press(Message::ToggleAbout);

    // Options > Reset Mix and Settings.  Reset Mix is a submenu; Settings
    // opens the app-level sheet.
    let options = iced::widget::button(
        text(if state.options_menu == OptionsMenu::Closed {
            "OPTIONS"
        } else {
            "OPTIONS ▴"
        })
        .size(theme::TEXT_MD * scale),
    )
    .padding([theme::SPACE_SM * scale, theme::SPACE_XL * scale])
    .style(theme::tab_toggle(state.options_menu != OptionsMenu::Closed))
    .on_press(Message::ToggleOptions);

    let mut bar = row![
        brand,
        tab_toggle,
        options,
        // Everything right of here is the tool side of the bar: the
        // flexible space sits in the middle, so the session chip stays
        // against the right edge instead of floating at whatever width it
        // used to claim for itself.
        iced::widget::Space::new().width(Length::Fill),
    ]
    .spacing(theme::SPACE_XXL)
    .align_y(iced::Alignment::Center);

    // Only when `--osc` is actually running — otherwise there's nothing
    // for the log to show, and the button would just be dead weight.
    if state.osc_config.is_some() {
        bar = bar.push(
            iced::widget::button(text("OSC LOG").size(theme::TEXT_MD * scale))
                .padding([theme::SPACE_SM * scale, theme::SPACE_XL * scale])
                .style(theme::tab_toggle(state.show_osc_log))
                .on_press(Message::ToggleOscLog),
        );
    }
    // The session chip hugs its own content (Submix + Clock) at the right
    // edge: it used to be stretched across `FillPortion(20)` inside a
    // scrollable, which painted a mostly-empty recessed pill across the
    // middle of the window. Content-sized, it reads as the bar's
    // right-hand tool cluster. The scrollable went with the stretch: at
    // the chip's real width there is nothing left to overflow.
    bar = bar.push(session);

    // A fixed height (`theme::TOP_BAR_H`) the sidebar's title row is
    // aligned to: the bar spans the mixer's width only, and the sidebar
    // runs the window's full height beside it, its title on the same line.
    container(bar)
        .style(theme::top_bar)
        .padding([0.0, theme::SPACE_XXL * scale])
        .height(Length::Fixed(theme::TOP_BAR_H * scale))
        .align_y(iced::Alignment::Center)
        .width(Length::Fill)
        .into()
}

/// Whether playback channel `idx` has a real level. Mock has its own
/// generated meters; real backends get them from the PipeWire monitor,
/// whose channel count depends on the card profile (2 = PB1 only,
/// 12 = Babyface six pairs, 18 = UCX nine pairs). See `pw_meter`.
fn playback_meter_available(state: &TotalControl, idx: usize) -> bool {
    if state.device.is_mock() {
        state.device.has_playback_meters()
    } else if state.device.has_device_playback_meters() {
        idx < state.device.playbacks().len()
    } else {
        idx < state.playback_tap_channels
    }
}

/// Whether an output's host-computed meter is complete: every source
/// routed into that output pair must itself have a real meter. Inputs
/// are all real with the updated ALSA driver; playbacks depend on the
/// PipeWire tap's channel count (2 = PB1 only, 12 = Babyface six pairs,
/// 18 = UCX nine pairs). An output with an unmetered nonzero crosspoint
/// stays N/A instead of showing a partial sum.
fn output_meter_available(state: &TotalControl, out_idx: usize) -> bool {
    let pair = if state.device.outputs_one_per_pair() {
        out_idx
    } else {
        out_idx / 2
    };
    let inputs_ok = state.device.inputs().iter().enumerate().all(|(i, ch)| {
        ch.volumes.get(pair).copied().unwrap_or(0.0) == 0.0
            || state.device.has_input_meter(i)
    });
    let playbacks_ok = state
        .device
        .playbacks()
        .iter()
        .enumerate()
        .all(|(i, ch)| {
            ch.volumes.get(pair).copied().unwrap_or(0.0) == 0.0
                || playback_meter_available(state, i)
        });
    inputs_ok && playbacks_ok
}

/// Builds the full `StripParams` for any channel — the field-by-field
/// logic (which meter buffer, which overrides, which default level, pan
/// vs. no-pan for outputs) used to be duplicated once per channel kind
/// inline in `mixer_view`'s three loops; factored out so `quick_view` can
/// render the exact same strip (just at a bigger `scale`) without a fourth
/// copy of it.
fn strip_params<'a>(
    state: &'a TotalControl,
    cid: ChannelId,
    output_idx: usize,
) -> strip::StripParams<'a> {
    let drag_range = state
        .drag_range
        .and_then(|(dc, lo, hi)| (dc == cid).then_some((lo, hi)));
    // An OUTPUT strip's fader IS that output's master — the strip's
    // `out` (used in the VolumeChanged/Reset messages) must be the
    // output channel index, NOT the selected submix `output_idx`
    // (which is only meaningful for input/playback crosspoints).
    // `set_volume(Output(i), out, v)` writes `output_for(out)` — so
    // out must equal i or the wrong master moves.
    let output_idx = match cid {
        ChannelId::Output(i) => i,
        _ => output_idx,
    };
    // FX send on inputs and software playbacks, FX Return on outputs: the
    // thin fader TotalMix draws beside the main one.
    let fx_level = if state.device.capabilities().fx {
        match cid {
            ChannelId::Input(i) => state.device.inputs().get(i).map(|c| c.fx_send_db),
            ChannelId::Playback(i) => state.device.playbacks().get(i).map(|c| c.fx_send_db),
            ChannelId::Output(i) => state.device.outputs().get(i).map(|c| c.fx_return_db),
        }
    } else {
        None
    };

    // The shared bus carries device meters for every row.  A backend that
    // only has a PipeWire tap for playback stays on the view-fed path.
    let bus_ok = state.device.has_device_playback_meters() || state.device.is_mock();
    let meter_bus = if bus_ok {
        state.meter_bus.clone()
    } else {
        None
    };
    let meter_slot = meter_bus.as_ref().map(|_| match cid {
        ChannelId::Input(i) => fader::BusSlot::Input(i),
        ChannelId::Playback(i) => fader::BusSlot::Playback(i),
        ChannelId::Output(i) => fader::BusSlot::Output(i),
    });

    let base = strip::StripParams {
        cid,
        output_idx,
        bus_label: state
            .pair_labels()
            .get(output_idx)
            .cloned()
            .unwrap_or_default(),
        // The three per-kind literals below spread this one, so the
        // capability is read once. See `RmeDevice::protocol_features`.
        protocol_features: state.device.protocol_features(),
        stereo_split_supported: state.device.stereo_split_supported(),
        name: String::new(),
        type_tag: None,
        vol: 0.0,
        pan: 0,
        output_balance_supported: false,
        fx_level,
        meter: fader::MeterFrame::still(0.0),
        meter_available: false,
        meter2: None,
        meter2_available: false,
        meter_bus,
        meter_slot,
        meter_trim: 0.0,
        has_48v: false,
        has_pad: false,
        has_instrument: false,
        phantom: false,
        pad: false,
        instrument: false,
        has_gain: false,
        gain: 0,
        gain_right: None,
        gain_max: 0,
        gain_half: false,
        has_sensitivity: false,
        sensitivity_plus4: false,
        has_level: false,
        level: InputLevel::Plus4dBu,
        has_autoset: false,
        autoset: false,
        has_dynamics: false,
        dynamics_on: false,
        has_eq: false,
        eq_enabled: false,
        has_trim: false,
        trim: 0.0,
        trim_tip: String::new(),
        slot_trim: true,
        slot_eq: false,
        slot_dyn: false,
        has_phase: false,
        phase: false,
        loopback: false,
        loopback_supported: state.device.loopback_supported(),
        stereo_linked: false,
        open_flyout: state
            .flyout_open
            .iter()
            .find(|(c, _)| *c == cid)
            .map(|(_, k)| *k),
        mute: false,
        pfl: state.control_room.pfl_replaces_solo
            && matches!(cid, ChannelId::Input(_)),
        solo: false,
        mute_armed: false,
        solo_armed: false,

        cue: false,
        default_vol: 1.0,
        editing: state.editing == Some(cid),
        edit_buf: &state.edit_buf,
        renaming: false,
        rename_buf: "",
        drag_range,
        modifiers: state.modifiers,
        collapsed: state.collapsed.contains(&cid),
        collapse_anim: state.collapse_anim.get(&cid).copied(),
        scale: state.ui_scale,
        selected: state.selected.contains(&cid),
        hovered: state.hovered_strip == Some(cid),
        active_surface: false,
        name_custom: false,
        name_locked: false,
        renameable: true,
    };

    // A device switch can leave one stale id in the frame while the next
    // update is queued.  Never index a channel array with an id the open
    // device does not have; draw an empty strip until the switch handler
    // has dropped it.
    if !channel_exists(state, cid) {
        return base;
    }

    let caps = state.device.capabilities();
    match cid {
        ChannelId::Input(i) => {
            let ch = &state.device.inputs()[i];
            let has_48v = caps.input_phantom && ch.channel_type == ChannelType::Mic;
            let linked = state.device.input_pair_linked(i / 2);
            let gain_right = ch.gain_right.or_else(|| {
                if linked && i % 2 == 0 {
                    state.device.inputs().get(i + 1).and_then(|right| right.gain)
                } else {
                    None
                }
            });
            let (volume, pan) = if linked {
                let left = i & !1;
                let right = left + 1;
                let left_volume = state
                    .device
                    .inputs()
                    .get(left)
                    .and_then(|c| c.volumes.get(output_idx))
                    .copied()
                    .unwrap_or(0.0);
                let right_volume = state
                    .device
                    .inputs()
                    .get(right)
                    .and_then(|c| c.volumes.get(output_idx))
                    .copied()
                    .unwrap_or(0.0);
                (
                    left_volume.max(right_volume),
                    output_balance_from_volumes(left_volume, right_volume),
                )
            } else {
                (ch.volumes[output_idx], ch.pans[output_idx])
            };
            strip::StripParams {
                name: ch.name.clone(),
                // RME TotalMix identifies the row and the channel name; a
                // second "MIC"/"LINE"/"SPDIF" badge next to every name is
                // redundant.  Keep the channel-type colour for the Matrix
                // and the settings panel only.
                type_tag: None,
                // Input strips always use the lighter active-source
                // surface; the current output strip uses the same tone.
                active_surface: true,
                vol: volume * trim_factor(ch.trim),
                pan,
                meter: state
                    .input_meters
                    .get(i)
                    .map(MeterAnim::frame)
                    .unwrap_or_else(|| fader::MeterFrame::still(0.0)),
                meter_available: state.device.has_input_meter(i),
                meter_trim: ch.trim,
                has_48v,
                has_pad: caps.input_pad && ch.has_pad,
                has_instrument: ch.has_instrument,
                phantom: ch.phantom,
                pad: ch.pad,
                instrument: ch.instrument,
                has_gain: ch.gain_max.is_some(),
                gain: ch.gain.unwrap_or(0),
                gain_right,
                gain_max: ch.gain_max.unwrap_or(0),
                // The driver's new UCX control is 0..24 in 0.5 dB units;
                // the older whole-dB control was 0..18 (or 0..12).
                gain_half: matches!(ch.gain_max, Some(24 | 36)),
                has_sensitivity: ch.sensitivity.is_some(),
                sensitivity_plus4: ch.sensitivity == Some(Sensitivity::Plus4dBu),
                has_level: caps.input_level && (2..4).contains(&i),
                level: ch.level.unwrap_or(InputLevel::Plus4dBu),
                has_autoset: caps.input_autoset && i < 2,
                autoset: ch.autoset,
                has_dynamics: caps.dynamics && ch.dynamics.is_some(),
                dynamics_on: ch.dynamics.is_some_and(|d| d.on),
                has_eq: ch.eq.is_some(),
                eq_enabled: ch.eq.is_some_and(|e| e.enabled),
                has_trim: true,
                trim: ch.trim,
                trim_tip: trim_tip(ch.trim),
                // The UC now stores phase as the sign of every routed
                // matrix cell; it has no EQ, so the old UCX EQ test hid it.
                has_phase: caps.phase && (ch.eq.is_some() || caps.phase_without_eq),
                phase: ch.phase,
                stereo_linked: state.device.input_pair_linked(i / 2),
                mute: ch.mute,
                solo: ch.solo,
                default_vol: 1.0,
                ..base
            }
        }
        ChannelId::Playback(i) => {
            let ch = &state.device.playbacks()[i];
            let linked = state.device.playback_linked(i / 2);
            let (volume, pan) = if linked {
                let left = i & !1;
                let right = left + 1;
                let left_volume = state
                    .device
                    .playbacks()
                    .get(left)
                    .and_then(|c| c.volumes.get(output_idx))
                    .copied()
                    .unwrap_or(0.0);
                let right_volume = state
                    .device
                    .playbacks()
                    .get(right)
                    .and_then(|c| c.volumes.get(output_idx))
                    .copied()
                    .unwrap_or(0.0);
                (
                    left_volume.max(right_volume),
                    output_balance_from_volumes(left_volume, right_volume),
                )
            } else {
                (ch.volumes[output_idx], ch.pans[output_idx])
            };
            strip::StripParams {
                name: ch.name.clone(),
                // The software-playback row already says what these are;
                // no redundant "PB" tag next to every name (TotalMix).
                type_tag: None,
                active_surface: true,
                vol: volume * trim_factor(ch.trim),
                pan,
                has_trim: true,
                trim: ch.trim,
                trim_tip: trim_tip(ch.trim),
                meter: state
                    .playback_meters
                    .get(i)
                    .map(MeterAnim::frame)
                    .unwrap_or_else(|| fader::MeterFrame::still(0.0)),
                meter_available: playback_meter_available(state, i),
                meter_trim: ch.trim,
                stereo_linked: state.device.playback_linked(i / 2),
                mute: ch.mute,
                solo: ch.solo,
                default_vol: 1.0,
                ..base
            }
        }
        ChannelId::Output(i) => {
            let ch = &state.device.outputs()[i];
            // A Control Room channel is always stereo, whatever the stored
            // link state says; every assigned pair is linked on assignment.
            let linked = state.device.output_linked(i / 2) || is_control_room_cid(state, cid);
            let left = i & !1;
            let right = left + 1;
            // Output balance is implicit in the two channel levels.  The
            // linked fader displays the louder side (the pair level), so
            // moving it preserves the balance.
            let pair_level = if linked {
                let l = state.device.outputs()[left].volume;
                let r = state.device.outputs()[right].volume;
                l.max(r)
            } else {
                ch.volume
            };
            // The UCX stores balance in its own register (`NN01`), so the
            // knob must read that; the UC derives it from the two volumes.
            let balance_register =
                state.device.output_balance_supported() && !state.device.is_uc_model();
            let pan: i8 = if balance_register {
                ch.balance
            } else if linked && state.device.output_balance_supported() {
                output_balance_from_volumes(
                    state.device.outputs()[left].volume,
                    state.device.outputs()[right].volume,
                )
            } else {
                0
            };
            // Device block pair 0 is Phones 7/8, wherever the display
            // order puts it.  The unit's physical encoder writes the IF1
            // 0x1800/0x1900 hardware volume, which the driver exposes
            // separately from the DSP master.  Show that value on the
            // Phones strip so the slider follows the unit.
            let hw = if i / 2 == 0 {
                state.device.hardware_volume()
            } else {
                None
            };
            strip::StripParams {
                name: ch.name.clone(),
                // The hardware-output row already says what these are; no
                // redundant "OUT" tag next to every name (TotalMix).
                type_tag: None,
                vol: hw.unwrap_or(pair_level),
                pan,
                output_balance_supported: state.device.output_balance_supported() && linked,
                meter: state
                    .output_meters
                    .get(i)
                    .map(MeterAnim::frame)
                    .unwrap_or_else(|| fader::MeterFrame::still(0.0)),
                // Host-computed from routed sources: available only
                // when every nonzero crosspoint feeding this pair has
                // a real meter (see `output_meter_available`).
                meter_available: output_meter_available(state, i),
                has_eq: ch.eq.is_some(),
                eq_enabled: ch.eq.is_some_and(|e| e.enabled),
                has_dynamics: caps.dynamics && ch.dynamics.is_some(),
                dynamics_on: ch.dynamics.is_some_and(|d| d.on),
                loopback: ch.loopback,
                stereo_linked: linked,
                mute: ch.mute,
                solo: ch.solo,

                cue: ch.cue,
                default_vol: 1.0,
                active_surface: if state.device.outputs_one_per_pair() {
                    i == state.sel_out
                } else {
                    i / 2 == state.sel_out
                },
                ..base
            }
        }
    }
}

/// Minimum interval between two applied window-resize updates. A live window
/// drag fires `Resized` events at pointer/mouse rate — often several between
/// two display frames — and each one that mutates state forces a full view
/// rebuild. Throttling to ~one per frame stops the UI from doing N rebuilds
/// for N intermediate widths that most of them never even got shown; the
/// latest width is stashed and applied when the window elapses (see
/// `apply_pending_resize`). ~16 ms keeps it at or just under display-refresh
/// cadence without feeling laggy.
const RESIZE_THROTTLE: Duration = Duration::from_millis(16);

/// How often `Message::Tick` probes whether the device is still there
/// (`RmeDevice::is_connected`): the tick itself runs at 20 Hz, and a card
/// does not vanish between two frames. Doubles as the reconnect retry
/// cadence while disconnected.
const CONN_CHECK_MS: u64 = 1000;

/// Apply the stashed resize width to `window_width`/`ui_scale`, but at most
/// once per `RESIZE_THROTTLE` — see `Message::WindowResized` and the const
/// above for why. No-op when there's nothing pending or the throttle window
/// hasn't elapsed yet.
fn apply_pending_resize(state: &mut TotalControl) {
    let Some(width) = state.pending_resize_width else {
        return;
    };
    let due = match state.last_resize_applied {
        Some(t) => t.elapsed() >= RESIZE_THROTTLE,
        None => true,
    };
    if !due {
        return;
    }
    state.pending_resize_width = None;
    state.last_resize_applied = Some(Instant::now());
    state.window_width = width;
}

// ── Scale: initial zoom, fit, sidebar ────────────────────────────

/// The smallest window the app opens at (logical px). Width: a strip row at
/// the minimum zoom plus the always-visible right sidebar and the top bar's
/// tools; height: one row of strips under the top bar (the rest scrolls).
/// It was 960x600, a round number rather than a measured one, and it kept a
/// window that fits everything at 80 % from being any smaller.
pub(crate) const MIN_WINDOW: (f32, f32) = (640.0, 420.0);

/// The sidebar scales with the zoom only up to this: it is a column of
/// small controls, and at the strips' 175 % it was as wide as half a
/// small window for no gain.
const SIDEBAR_SCALE_MAX: f32 = 1.25;
/// A starting zoom for a monitor of `height` logical pixels (the OS scale
/// factor is already in it): 100 % at 1080p and below, more on bigger
/// screens, where the 10-13 px text is small. Only used at the first
/// launch; the user's choice is remembered after that.
pub(crate) fn initial_zoom_for(height: f32) -> f32 {
    if height >= 2000.0 {
        1.5
    } else if height >= 1300.0 {
        1.25
    } else if height >= 1100.0 {
        1.1
    } else {
        theme::SCALE_DEFAULT
    }
}

/// The scale the sidebar draws at: the zoom, capped (`SIDEBAR_SCALE_MAX`).
pub(crate) fn sidebar_scale(state: &TotalControl) -> f32 {
    state.ui_scale.min(SIDEBAR_SCALE_MAX)
}

/// How much one zoom step changes `ui_scale` (a Ctrl+molette notch, or one
/// Ctrl+= / Ctrl+- press). The scale is decoupled from window size — fixed
/// geometry, rows and pages scroll — so this is the only thing that moves it.
const ZOOM_STEP: f32 = 0.1;

/// Moves `ui_scale` by `delta`, clamped to the manual-zoom range
/// (`SCALE_MIN`..`SCALE_MAX`). `ZoomReset` sets `SCALE_DEFAULT` directly.
fn zoom(state: &mut TotalControl, delta: f32) {
    state.ui_scale = (state.ui_scale + delta).clamp(theme::SCALE_MIN, theme::SCALE_MAX);
}

/// A strip's on-screen width at the current zoom, matching whatever
/// `strip::strip()` actually renders (collapsed vs. full) — used to
/// decide if a row fits without scrolling, see `responsive_row`.
fn rendered_strip_width(state: &TotalControl, cid: ChannelId) -> f32 {
    let w = if state.collapsed.contains(&cid) {
        strip::COLLAPSED_W
    } else {
        strip::full_width(cid)
    };
    w * state.ui_scale
}

/// Derive a stereo balance from the two hardware-output levels.  Output
/// balance is not stored anywhere: the per-channel volumes are the state,
/// exactly as on the UC wire (two unequal `0x1a` codes).
fn output_balance_from_volumes(left: f32, right: f32) -> i8 {
    let peak = left.max(right);
    if peak <= 0.0 {
        0
    } else {
        (((right - left) / peak) * 100.0)
            .round()
            .clamp(-100.0, 100.0) as i8
    }
}

/// The left/right channel ids of the pair containing `cid`.
fn stereo_pair(cid: ChannelId) -> (ChannelId, ChannelId) {
    match cid {
        ChannelId::Input(i) => (ChannelId::Input(i & !1), ChannelId::Input(i | 1)),
        ChannelId::Playback(i) => {
            (ChannelId::Playback(i & !1), ChannelId::Playback(i | 1))
        }
        ChannelId::Output(i) => (ChannelId::Output(i & !1), ChannelId::Output(i | 1)),
    }
}

/// Width of the second strip of a pair: 0 while linked, its full width
/// while split.  During a Stereo toggle it tweens between the two, which is
/// what makes the rest of the row slide at the fold-out speed.
fn stereo_extra_width(
    state: &TotalControl,
    left: ChannelId,
    linked: bool,
    full: f32,
    now: Instant,
) -> f32 {
    state
        .stereo_anim
        .get(&left)
        .map(|a| a.at(now).clamp(0.0, full))
        .unwrap_or(if linked { 0.0 } else { full })
}

fn start_stereo_anim(state: &mut TotalControl, cid: ChannelId, linked: bool) {
    let (left, right) = stereo_pair(cid);
    let full = rendered_strip_width(state, right);
    let now = Instant::now();
    let prev = state
        .stereo_anim
        .get(&left)
        .map(|a| a.at(now))
        .unwrap_or(if linked { full } else { 0.0 });
    let target = if linked { 0.0 } else { full };
    if (prev - target).abs() < 0.5 {
        state.stereo_anim.remove(&left);
    } else {
        state.stereo_anim.insert(
            left,
            strip::CollapseAnim {
                prev,
                value: target,
                since: now,
            },
        );
    }
}

/// Natural width of an inline panel, matching the width the render loops
/// pass to its popover function.
fn flyout_width(state: &TotalControl, cid: ChannelId, kind: strip::FlyoutKind) -> f32 {
    match kind {
        strip::FlyoutKind::Settings => rendered_strip_width(state, cid),
        strip::FlyoutKind::Eq => strip::EQ_FLYOUT_W * state.ui_scale,
        strip::FlyoutKind::Dynamics => strip::DYN_FLYOUT_W * state.ui_scale,
        strip::FlyoutKind::Trim => strip::FLYOUT_W,
        strip::FlyoutKind::Route => strip::FLYOUT_W,
    }
}

/// Current animated width of an inline panel, or `None` when it is fully
/// closed and should not occupy a row slot.  `full_width` is the strip's
/// current width (settings panels match the strip).
fn flyout_slot(
    state: &TotalControl,
    cid: ChannelId,
    kind: strip::FlyoutKind,
    full_width: f32,
    now: Instant,
) -> Option<f32> {
    let open = state.flyout_open.contains(&(cid, kind));
    match state.flyout_anim.get(&(cid, kind)) {
        Some(anim) => {
            let w = anim.at(now);
            if !open && !anim.is_settling(now) {
                None
            } else {
                Some(w.max(0.0))
            }
        }
        None => open.then_some(full_width),
    }
}

/// Current animated FX-column width, or `None` when it is fully closed and
/// should not occupy the layout.
fn fx_slot(state: &TotalControl, now: Instant) -> Option<f32> {
    if !state.device.capabilities().fx {
        return None;
    }
    match state.fx_anim {
        Some(anim) => {
            let width = anim.at(now);
            if !state.show_fx_panel && !anim.is_settling(now) {
                None
            } else {
                Some(width.max(0.0))
            }
        }
        None => state.show_fx_panel.then_some(FX_W * state.ui_scale),
    }
}

/// How much of the main area the FX column reserves. It grows to the
/// column's full extent while the mixer has slack, stays pinned at the
/// mixer's natural width while the column slides over it, and reaches zero
/// once the mixer itself has to scroll.
fn fx_reserve(main_space: f32, natural_width: f32, fx_extent: f32) -> f32 {
    (main_space - natural_width).clamp(0.0, fx_extent)
}

/// Route is rendered as a Stack overlay, so its row needs the strip's
/// right edge while it is open *or* still sliding out.
fn route_visible(state: &TotalControl, cid: ChannelId, now: Instant) -> bool {
    state
        .flyout_open
        .contains(&(cid, strip::FlyoutKind::Route))
        || state
            .flyout_anim
            .get(&(cid, strip::FlyoutKind::Route))
            .is_some_and(|a| a.is_settling(now))
}

// Rows are plain content now: the whole page is the one bidirectional
// scroll area (`page()`), so all three rows scroll together and no row
// can be squeezed out by a narrow window.

/// The route flyout's content — one button per output bus, current
/// selection highlighted. `width` is the caller's animated tween value;
/// `.clip(true)` keeps the list from spilling out past it mid-animation.
fn route_popover(state: &TotalControl, width: f32, scale: f32) -> Element<'_, Message> {
    let mut list = column![].spacing(theme::SPACE_TIGHT);
    let labels = state.display_pair_labels();
    let display_to_internal = state.device.output_display_order();
    let selected_display = state.device.display_pair_index(state.sel_out);
    for (display, label) in labels.iter().enumerate() {
        let internal = display_to_internal.get(display).copied().unwrap_or(display);
        list = list.push(
            button(text(label.clone()).size(theme::TEXT_SM * scale))
                .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                .width(Length::Fill)
                // The bus a strip routes to is a position, not an "on"
                // state: the graphite thumb, not the white fill.
                .style(theme::segment_button(display == selected_display))
                .on_press(Message::SelectOutput(internal)),
        );
    }
    // Floats over the next strips, so it is a raised card with a shadow,
    // like the device popover; it used to be the top bar's flat fill and
    // read as painted onto the strips under it.
    container(list)
        .padding(theme::SPACE_SM * scale)
        .width(Length::Fixed(width))
        .clip(true)
        .style(theme::dialog)
        .into()
}

/// The card a strip's settings / trim / EQ panel sits in: the strip's own
/// card style, so the panel reads as the strip opened up rather than as
/// loose controls on the page. These used to take the top bar's flat fill
/// (no border, a shade off the page), and only the controls showed.
fn flyout_card<'a>(
    content: impl Into<Element<'a, Message>>,
    width: f32,
    scale: f32,
) -> Element<'a, Message> {
    container(content)
        .padding(theme::FLYOUT_PAD * scale)
        .width(Length::Fixed(width))
        // A fixed full-strip height, not the content's natural height: every
        // slideout must occupy the same row height (Settings is short, Trim
        // is short, EQ is tall).  `Length::Fill` resolves to zero inside the
        // page-level horizontal scrollable, so this is the measured full
        // strip height (`strip::STRIP_H`), not a fill.
        .height(Length::Fixed(strip::STRIP_H * scale))
        .align_y(iced::Alignment::Start)
        .style(theme::panel_attached)
        .clip(true)
        .into()
}

/// Row heights of the Settings grid (logical pixels at scale 1).  Every
/// channel type uses the same slot heights so a control a channel lacks
/// leaves a hole of the same size and nothing below it moves.
const SETTINGS_ROW_TEXT: f32 = 18.0;
const SETTINGS_ROW_CTRL: f32 = 24.0;
const SETTINGS_ROW_PICK: f32 = 26.0;
const SETTINGS_ROW_KNOB: f32 = 54.0;
/// Height shared by every button and dropdown in a Settings panel.
const SETTINGS_CONTROL_H: f32 = 24.0;

/// A dB value as the knob face shows it: the floor is infinity, not a
/// literal -65.0.
fn knob_db_label(db: f32) -> String {
    if db <= -64.95 {
        "-∞".to_string()
    } else {
        format!("{db:+.1}")
    }
}

/// Gap between two controls sharing one Settings row.  Two half-width
/// controls plus this gap equal one full-width control, so a single button
/// and a pair of phase buttons end at the same right edge.
const SETTINGS_CTRL_GAP: f32 = theme::SPACE_TIGHT;

/// Two controls on one Settings row, each taking half the content width.
fn settings_pair<'a>(
    first: Element<'a, Message>,
    second: Element<'a, Message>,
    scale: f32,
) -> Element<'a, Message> {
    row![first, second]
        .spacing(SETTINGS_CTRL_GAP * scale)
        .width(Length::Fill)
        .into()
}

/// One reference-level dropdown for the channel panels.  The option order
/// is the panel's, the codes are the register's.
fn settings_reference_pick(
    options: [&'static str; 3],
    codes: [u16; 3],
    current: u16,
    scale: f32,
    message: impl Fn(u16) -> Message + 'static,
) -> Element<'static, Message> {
    let opts: Vec<String> = options.iter().map(|s| s.to_string()).collect();
    let selected = options
        .iter()
        .zip(codes)
        .find(|(_, c)| *c == current)
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| options[0].to_string());
    let opts_for_pick = opts.clone();
    let selected_label = selected.clone();
    let pick = pick_list(opts, Some(selected), move |s| {
        let idx = opts_for_pick.iter().position(|o| *o == s).unwrap_or(0);
        message(codes[idx])
    })
    .style(theme::pick_list_no_text)
    .menu_style(theme::menu)
    // Reserve the handle's width in the text bounds; the centred label is
    // drawn below and clips at both sides in the reserved area.
    .padding(iced::Padding {
        top: 1.0 * scale,
        right: 10.0 * scale,
        bottom: 1.0 * scale,
        left: 1.0 * scale,
    })
    .text_size(theme::TEXT_SM * scale)
    .text_line_height(iced::widget::text::LineHeight::Absolute(
        iced::Pixels((SETTINGS_CONTROL_H - 2.0) * scale),
    ))
    .width(Length::Fill)
    .handle(iced::widget::pick_list::Handle::None);
    let label = container(
        text(selected_label)
            .size(theme::TEXT_SM * scale)
            .color(theme::TEXT_PRIMARY),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(iced::Alignment::Center)
    .align_y(iced::Alignment::Center)
    .clip(true)
    .padding(iced::Padding {
        right: 10.0 * scale,
        left: 1.0 * scale,
        ..iced::Padding::ZERO
    });
    let arrow = container(
        text("▾")
            .size(theme::TEXT_XS * scale)
            .color(theme::TEXT_SEC),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(iced::Alignment::End)
    .align_y(iced::Alignment::Center)
    .padding(iced::Padding {
        right: theme::SPACE_SM * scale,
        ..iced::Padding::ZERO
    });
    iced::widget::stack![pick, label, arrow].into()
}

/// The Settings card.  Its right padding is a little smaller than the
/// shared flyout padding, so the knob column sits slightly closer to the
/// panel edge and the label column keeps a few more pixels.
fn settings_card<'a>(
    content: impl Into<Element<'a, Message>>,
    width: f32,
    scale: f32,
) -> Element<'a, Message> {
    container(content)
        .padding(iced::Padding {
            top: theme::FLYOUT_PAD * scale,
            right: (theme::FLYOUT_PAD - 4.0) * scale,
            bottom: theme::FLYOUT_PAD * scale,
            left: theme::FLYOUT_PAD * scale,
        })
        .width(Length::Fixed(width))
        .height(Length::Fixed(strip::STRIP_H * scale))
        .align_y(iced::Alignment::Start)
        .style(theme::panel_attached)
        .clip(true)
        .into()
}

/// One fixed slot of the Settings grid.  An empty slot keeps its height, so
/// the shared rows (`width`, `send FX`, `MS proc`, phase) line up on every
/// channel.
fn settings_slot<'a>(
    content: Option<Element<'a, Message>>,
    height: f32,
    scale: f32,
) -> Element<'a, Message> {
    container(content.unwrap_or_else(|| iced::widget::Space::new().into()))
        .height(Length::Fixed(height * scale))
        .width(Length::Fill)
        .align_y(iced::Alignment::Center)
        .into()
}

/// Format one input gain for the Gain knob.  `half` means raw half-dB units.
fn input_gain_label(raw: u32, half: bool) -> String {
    if half {
        format!("{:.1}", raw as f32 / 2.0)
    } else {
        raw.to_string()
    }
}

/// The Gain knob label for a linked left input.  The device always treats
/// inputs 1-4 as unlinked, so a paired input always shows both values, even
/// when they are currently equal.
fn input_gain_knob_label(
    gain: u32,
    gain_right: Option<u32>,
    half: bool,
    linked_left: bool,
) -> String {
    let left = input_gain_label(gain, half);
    if linked_left {
        let right = gain_right.unwrap_or(gain);
        return format!("{left}\n{}", input_gain_label(right, half));
    }
    left
}

/// One label plus one knob.  The knob is a fixed box pinned to the right
/// edge, so gain, width, send FX and return FX sit at the same x on every
/// channel type.
fn settings_knob_row<'a>(
    label: Element<'a, Message>,
    knob: Element<'a, Message>,
) -> Element<'a, Message> {
    row![
        label,
        iced::widget::Space::new().width(Length::Fill),
        knob,
    ]
    .align_y(iced::Alignment::Center)
    .width(Length::Fill)
    .into()
}

/// A standalone label row.  The text sits at the bottom of its slot, just
/// above the control below it, without moving the shared grid rows.
fn settings_label_slot<'a>(
    content: Option<Element<'a, Message>>,
    height: f32,
    scale: f32,
) -> Element<'a, Message> {
    container(content.unwrap_or_else(|| iced::widget::Space::new().into()))
        .height(Length::Fixed(height * scale))
        .width(Length::Fill)
        .align_y(iced::Alignment::End)
        .into()
}

fn settings_popover(state: &TotalControl, cid: ChannelId, width: f32, scale: f32) -> Element<'_, Message> {
    let p = strip_params(state, cid, state.sel_out);
    let caps = state.device.capabilities();
    // The reference dropdowns in the channel panels are UCX registers; the
    // UC keeps its reference levels in the device sheet's options word.
    let ucx = state.device.is_uc_family() && !state.device.is_uc_model();

    // Stereo link/split — every channel kind has this (TotalMix shows the
    // button on every strip, not just AN1/2 or hardware outputs; see
    // `RmeDevice::{input_pair,playback,output}_linked`'s doc comments for
    // why this is a pure display/control-grouping concept, not tied to a
    // specific hardware register).
    // A playback pair's link is its hardware stereo split (the pair goes
    // split-mono into AN1/2), an RME-protocol register: in Class Compliant
    // mode the switch is shown but inert, saying why.
    // A Control Room channel is always stereo: its pair cannot be split, so
    // the control stays engaged and inert (the locked style keeps the lit
    // outline rather than the disabled dimming).
    let cr = is_control_room_cid(state, cid);
    let can_toggle =
        !cr && (!matches!(cid, ChannelId::Playback(_)) || p.stereo_split_supported);
    let tip = if cr {
        "Control Room channels are always stereo"
    } else {
        match (cid, can_toggle) {
            (ChannelId::Playback(_), false) if state.device.class_compliant() => {
                "Splitting a playback pair needs the RME protocol (not in Class Compliant mode)"
            }
            (ChannelId::Playback(_), false) => {
                "Splitting a playback pair is not supported by this backend"
            }
            (ChannelId::Playback(_), true) => {
                "Stereo: one strip for the pair. Off: two mono strips, split L/R into AN1/2"
            }
            _ => "Stereo: one strip for the pair. Off: the two channels as separate strips",
        }
    };
    let stereo = hint(
        button(strip::centered_label("STEREO", theme::TEXT_SM * scale))
            .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
            .width(Length::Fill)
            .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
            .style(theme::stereo_button_state(p.stereo_linked || cr, cr))
            .on_press_maybe(
                can_toggle.then_some(Message::StereoLinkChanged(cid, !p.stereo_linked)),
            ),
        tip,
        scale,
    );

    let send_knob = |send: f32| -> Element<'_, Message> {
        let modifiers = state.modifiers;
        knob(Knob {
            value: send,
            range: (-65.0, 0.0),
            label: knob_db_label(send),
            arc_from_center: false,
            level_arc_color: Some(theme::PHANTOM),
            interactive: true,
            log_scale: false,
            modifiers,
            scale,
            on_change: Box::new(move |v| Message::FxSendChanged(cid, v)),
            on_reset: Box::new(move || Message::FxSendChanged(cid, -65.0)),
        })
        .into()
    };

    if let ChannelId::Input(idx) = cid {
        let mut col = column![].spacing(theme::SPACE_TIGHT * scale).width(Length::Fill);
        col = col.push(settings_slot(Some(stereo), SETTINGS_ROW_CTRL, scale));

        // Row 2 — the reference readout for the channels whose dropdown is
        // not preceded by a preamp button.
        let ref_label = (2..=7).contains(&idx).then_some("ref.");
        col = col.push(settings_label_slot(
            ref_label.map(|label| {
                container(
                    text(label)
                        .size(theme::TEXT_SM * scale)
                        .color(theme::TEXT_SEC),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::Center)
                .into()
            }),
            SETTINGS_ROW_TEXT,
            scale,
        ));

        // Row 3 — 48 V/PAD/Inst and the reference dropdown.  A button
        // alone takes the whole row; when a dropdown shares it, the button
        // shrinks to its label and the dropdown gets the rest.
        let has_dropdown = ucx && (p.has_level || (4..=7).contains(&idx));
        let button_width = if has_dropdown {
            Length::Shrink
        } else {
            Length::Fill
        };
        let mut pre = row![].spacing(SETTINGS_CTRL_GAP * scale).width(Length::Fill);
        let mut pre_ctrl = false;
        if p.has_48v {
            pre = pre.push(
                button(strip::centered_label("48V", theme::TEXT_SM * scale))
                    .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                    .width(button_width)
                .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
                    .style(theme::phantom_button(p.phantom))
                    .on_press(Message::Phantom(idx, !p.phantom)),
            );
            pre_ctrl = true;
        }
        if p.has_pad {
            pre = pre.push(
                button(strip::centered_label("PAD", theme::TEXT_SM * scale))
                    .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                    .width(button_width)
                .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
                    .style(theme::stereo_button(p.pad))
                    .on_press(Message::Pad(idx, !p.pad)),
            );
            pre_ctrl = true;
        }
        if p.has_instrument {
            pre = pre.push(
                button(strip::centered_label("INST", theme::TEXT_SM * scale))
                    .padding([theme::SPACE_TIGHT * scale, theme::SPACE_SM * scale])
                    .width(button_width)
                .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
                    .style(theme::stereo_button(p.instrument))
                    .on_press(Message::Instrument(idx, !p.instrument)),
            );
            pre_ctrl = true;
        }
        if ucx {
            if p.has_level {
                let options = ["-10dBV", "+4dBu", "LoGain"];
                let codes = [0u16, 1, 2];
                pre = pre.push(settings_reference_pick(options, codes, p.level.code(), scale, move |code| {
                    Message::LevelChanged(
                        idx,
                        InputLevel::from_code(code).unwrap_or(InputLevel::Plus4dBu),
                    )
                }));
                pre_ctrl = true;
            } else if (4..=7).contains(&idx) {
                pre = pre.push(settings_reference_pick(
                    ["-10dBV", "+4dBu", "LoGain"],
                    [0, 1, 2],
                    state.device.settings().line_in_ref_58,
                    scale,
                    Message::LineInRef58Selected,
                ));
                pre_ctrl = true;
            }
        }
        col = col.push(settings_slot(
            pre_ctrl.then_some(pre.into()),
            SETTINGS_ROW_PICK,
            scale,
        ));

        // Row 4 — AutoSet (and the Babyface sensitivity switch).
        let mut auto = row![].spacing(SETTINGS_CTRL_GAP * scale).width(Length::Fill);
        let mut auto_ctrl = false;
        if p.has_autoset {
            auto = auto.push(hint(
                button(strip::centered_label("AUTOSET", theme::TEXT_SM * scale))
                    .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                    .width(Length::Fill)
                .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
                    .style(theme::stereo_button(p.autoset))
                    .on_press(Message::Autoset(idx, !p.autoset)),
                "AutoSet — automatic gain reduction to prevent overloads",
                scale,
            ));
            auto_ctrl = true;
        }
        if p.has_sensitivity && state.device.has_sensitivity_control() {
            let label = if p.sensitivity_plus4 {
                "+4dBu"
            } else {
                "-10dBV"
            };
            auto = auto.push(
                button(strip::centered_label(label, theme::TEXT_SM * scale))
                    .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                    .width(Length::Fill)
                .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
                    .style(theme::plain_button)
                    .on_press(Message::Sensitivity(idx, !p.sensitivity_plus4)),
            );
            auto_ctrl = true;
        }
        col = col.push(settings_slot(
            auto_ctrl.then_some(auto.into()),
            SETTINGS_ROW_CTRL,
            scale,
        ));

        // Row 5 — gain.
        let gain = p.has_gain.then(|| {
            let gain_max = p.gain_max;
            let label = input_gain_knob_label(
                p.gain,
                p.gain_right,
                p.gain_half,
                p.stereo_linked && idx % 2 == 0,
            );
            settings_knob_row(
                column![
                    text("gain")
                        .size(theme::TEXT_SM * scale)
                        .color(theme::TEXT_SEC),
                    text("dB")
                        .size(theme::TEXT_XS * scale)
                        .color(theme::TEXT_SEC),
                ]
                .spacing(0.0)
                .into(),
                knob(Knob {
                    value: p.gain as f32,
                    range: (0.0, gain_max as f32),
                    // Gain is tracked in raw device units (whole dB on
                    // inputs 1/2, half-dB units on 3/4).  A linked pair with
                    // different left/right gains shows both values, left
                    // above right; equal values collapse to one number.
                    label,
                    arc_from_center: false,
                    level_arc_color: Some(theme::PHANTOM),
                    interactive: true,
                    log_scale: false,
                    modifiers: p.modifiers,
                    scale,
                    on_change: Box::new(move |v| Message::Gain(idx, (v.round() as u32).min(gain_max))),
                    on_reset: Box::new(move || Message::Gain(idx, 0)),
                })
                .into(),
            )
        });
        col = col.push(settings_slot(gain, SETTINGS_ROW_KNOB, scale));

        // Row 6 — width.
        let width_knob = caps.channel_width.then(|| {
            let modifiers = state.modifiers;
            let value = state.device.inputs()[idx].width;
            settings_knob_row(
                text("width")
                    .size(theme::TEXT_SM * scale)
                    .color(theme::TEXT_SEC)
                    .into(),
                knob(Knob {
                    value,
                    range: (-1.0, 1.0),
                    label: format!("{value:.2}"),
                    arc_from_center: true,
                    level_arc_color: Some(theme::PHANTOM),
                    interactive: true,
                    log_scale: false,
                    modifiers,
                    scale,
                    on_change: Box::new(move |v| Message::ChannelWidthChanged(cid, v)),
                    on_reset: Box::new(move || Message::ChannelWidthChanged(cid, 1.0)),
                })
                .into(),
            )
        });
        col = col.push(settings_slot(width_knob, SETTINGS_ROW_KNOB, scale));

        // Row 7 — send FX.
        let send = caps.fx.then(|| {
            settings_knob_row(
                column![
                    text("send")
                        .size(theme::TEXT_SM * scale)
                        .color(theme::TEXT_SEC),
                    text("FX")
                        .size(theme::TEXT_SM * scale)
                        .color(theme::TEXT_SEC),
                ]
                .spacing(0.0)
                .into(),
                send_knob(state.device.inputs()[idx].fx_send_db),
            )
        });
        col = col.push(settings_slot(send, SETTINGS_ROW_KNOB, scale));

        // Row 8 — MS proc.  A mono input pair cannot process M/S, so the
        // button is inert until the pair is linked.  Stereo inputs (5-8,
        // SPDIF, ADAT) are always a pair.
        let ms_on = state.device.inputs()[idx].ms_proc;
        let can_ms = idx >= 4 || p.stereo_linked;
        let ms = ucx.then(|| {
            hint(
                button(strip::centered_label("MS Proc", theme::TEXT_SM * scale))
                    .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                    .width(Length::Fill)
                    .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
                    .style(theme::stereo_button(ms_on))
                    .on_press_maybe(
                        can_ms.then_some(Message::InputMsProcChanged(idx, !ms_on)),
                    ),
                "Mid/side processing on the stereo pair",
                scale,
            )
        });
        col = col.push(settings_slot(ms, SETTINGS_ROW_CTRL, scale));

        // Row 9 — the phase pair.
        let phase = p.has_phase.then(|| {
            let phase_button =
                |label: &'static str, channel: usize, on: bool| -> Element<'_, Message> {
                    hint(
                        button(strip::centered_label(label, theme::TEXT_SM * scale))
                            .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                            .width(Length::Fill)
                .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
                            .style(theme::phase_button(on))
                            .on_press(Message::PhaseChanged(ChannelId::Input(channel), !on)),
                        if on { "Phase inverted" } else { "Phase normal" },
                        scale,
                    )
                };
            if p.stereo_linked {
                let left = idx & !1;
                let right = left + 1;
                let left_on = state.device.inputs().get(left).is_some_and(|c| c.phase);
                let right_on = state
                    .device
                    .inputs()
                    .get(right)
                    .is_some_and(|c| c.phase);
                settings_pair(
                    phase_button("Ø L", left, left_on),
                    phase_button("R Ø", right, right_on),
                    scale,
                )
            } else {
                row![phase_button("Ø", idx, p.phase)]
                    .width(Length::Fill)
                    .into()
            }
        });
        col = col.push(settings_slot(phase, SETTINGS_ROW_CTRL, scale));

        return settings_card(col, width, scale);
    }

    if let ChannelId::Playback(i) = cid {
        let mut col = column![].spacing(theme::SPACE_TIGHT * scale).width(Length::Fill);
        col = col.push(settings_slot(Some(stereo), SETTINGS_ROW_CTRL, scale));
        let width_knob = caps.channel_width.then(|| {
            let modifiers = state.modifiers;
            let value = state.device.playbacks()[i].width;
            settings_knob_row(
                text("width")
                    .size(theme::TEXT_SM * scale)
                    .color(theme::TEXT_SEC)
                    .into(),
                knob(Knob {
                    value,
                    range: (-1.0, 1.0),
                    label: format!("{value:.2}"),
                    arc_from_center: true,
                    level_arc_color: Some(theme::PHANTOM),
                    interactive: true,
                    log_scale: false,
                    modifiers,
                    scale,
                    on_change: Box::new(move |v| Message::ChannelWidthChanged(cid, v)),
                    on_reset: Box::new(move || Message::ChannelWidthChanged(cid, 1.0)),
                })
                .into(),
            )
        });
        col = col.push(settings_slot(width_knob, SETTINGS_ROW_KNOB, scale));
        let send = caps.fx.then(|| {
            settings_knob_row(
                column![
                    text("send")
                        .size(theme::TEXT_SM * scale)
                        .color(theme::TEXT_SEC),
                    text("FX")
                        .size(theme::TEXT_SM * scale)
                        .color(theme::TEXT_SEC),
                ]
                .spacing(0.0)
                .into(),
                send_knob(
                    state
                        .device
                        .playbacks()
                        .get(i)
                        .map(|c| c.fx_send_db)
                        .unwrap_or(-65.0),
                ),
            )
        });
        col = col.push(settings_slot(send, SETTINGS_ROW_KNOB, scale));
        // Row 4 — MS proc.  The transform is per stereo pair, so a split
        // playback pair keeps the button inert.
        let ms_on = state.device.playbacks()[i].ms_proc;
        let ms = ucx.then(|| {
            hint(
                button(strip::centered_label("MS Proc", theme::TEXT_SM * scale))
                    .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                    .width(Length::Fill)
                    .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
                    .style(theme::stereo_button(ms_on))
                    .on_press_maybe(
                        p.stereo_linked
                            .then_some(Message::PlaybackMsProcChanged(i, !ms_on)),
                    ),
                "Mid/side processing on the stereo pair",
                scale,
            )
        });
        col = col.push(settings_slot(ms, SETTINGS_ROW_CTRL, scale));
        // Row 5 — playback has no phase.
        col = col.push(settings_slot(None, SETTINGS_ROW_CTRL, scale));
        return settings_card(col, width, scale);
    }

    let ChannelId::Output(out) = cid else {
        return settings_card(column![], width, scale);
    };
    let pair = out / 2;
    let ref_group = match pair {
        0 => Some((state.device.settings().phones_ref, true)),
        1..=3 => Some((state.device.settings().line_out_ref, false)),
        _ => None,
    };
    let mut col = column![].spacing(theme::SPACE_TIGHT * scale).width(Length::Fill);
    col = col.push(settings_slot(Some(stereo), SETTINGS_ROW_CTRL, scale));
    col = col.push(settings_label_slot(
        ref_group.map(|_| {
            container(
                text("ref.")
                    .size(theme::TEXT_SM * scale)
                    .color(theme::TEXT_SEC),
            )
            .width(Length::Fill)
            .align_x(iced::Alignment::Center)
            .into()
        }),
        SETTINGS_ROW_TEXT,
        scale,
    ));
    col = col.push(settings_slot(
        ref_group
            .filter(|_| ucx)
            .map(|(current, phones)| {
                settings_reference_pick(
                    ["-10dBV", "+4dBu", "HiGain"],
                    [0, 1, 2],
                    current,
                    scale,
                    if phones {
                        Message::PhonesRefSelected
                    } else {
                        Message::LineOutRefSelected
                    },
                )
            }),
        SETTINGS_ROW_PICK,
        scale,
    ));

    // Row 4 — return FX.
    let fx = caps.fx.then(|| {
        let fx = state
            .device
            .outputs()
            .get(out)
            .map(|o| o.fx_return_db)
            .unwrap_or(-65.0);
        let modifiers = state.modifiers;
        settings_knob_row(
            column![
                text("return")
                    .size(theme::TEXT_SM * scale)
                    .color(theme::TEXT_SEC),
                text("FX")
                    .size(theme::TEXT_SM * scale)
                    .color(theme::TEXT_SEC),
            ]
            .spacing(0.0)
            .into(),
            knob(Knob {
                value: fx,
                range: (-65.0, 0.0),
                label: knob_db_label(fx),
                arc_from_center: false,
                level_arc_color: Some(theme::PHANTOM),
                interactive: true,
                log_scale: false,
                modifiers,
                scale,
                on_change: Box::new(move |v| Message::FxReturnChanged(out, v)),
                on_reset: Box::new(move || Message::FxReturnChanged(out, -65.0)),
            })
            .into(),
        )
    });
    col = col.push(settings_slot(fx, SETTINGS_ROW_KNOB, scale));

    // Row 5 — output phase pair.
    let phase = caps.output_phase.then(|| {
        let phase_button = |label: &'static str, idx: usize, on: bool| -> Element<'_, Message> {
            hint(
                button(strip::centered_label(label, theme::TEXT_SM * scale))
                    .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                    .width(Length::Fill)
                .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
                    .style(theme::phase_button(on))
                    .on_press(Message::OutputPhaseChanged(idx, !on)),
                if on { "Phase inverted" } else { "Phase normal" },
                scale,
            )
        };
        if p.stereo_linked || cr {
            let left = out & !1;
            let right = left + 1;
            let left_on = state
                .device
                .outputs()
                .get(left)
                .is_some_and(|o| o.phase_l);
            let right_on = state
                .device
                .outputs()
                .get(right)
                .is_some_and(|o| o.phase_r);
            settings_pair(
                phase_button("Ø L", left, left_on),
                phase_button("R Ø", right, right_on),
                scale,
            )
        } else {
            let on = if out % 2 == 0 {
                state.device.outputs().get(out).is_some_and(|o| o.phase_l)
            } else {
                state.device.outputs().get(out).is_some_and(|o| o.phase_r)
            };
            row![phase_button("Ø", out, on)]
                .width(Length::Fill)
                .into()
        }
    });
    col = col.push(settings_slot(phase, SETTINGS_ROW_CTRL, scale));

    // Row 6 — No Trim.
    let no_trim_on = if p.stereo_linked {
        state
            .device
            .outputs()
            .get(out & !1)
            .is_some_and(|o| o.no_trim)
    } else {
        state
            .device
            .outputs()
            .get(out)
            .is_some_and(|o| o.no_trim)
    };
    let no_trim = caps.fx.then(|| {
        hint(
            button(strip::centered_label("no trim", theme::TEXT_SM * scale))
                .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                .width(Length::Fill)
                .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
                .style(theme::stereo_button(no_trim_on))
                .on_press(Message::NoTrimChanged(out, !no_trim_on)),
            "Excludes this output from Trim Gain changes",
            scale,
        )
    });
    col = col.push(settings_slot(no_trim, SETTINGS_ROW_CTRL, scale));

    // Row 7 — reserved for the Talkback source/level rows.
    col = col.push(settings_slot(None, SETTINGS_ROW_TEXT, scale));

    // Row 8 — Talkback receive.
    let talkback_on = if p.stereo_linked {
        state
            .device
            .outputs()
            .get(out & !1)
            .is_some_and(|o| o.talkback)
    } else {
        state
            .device
            .outputs()
            .get(out)
            .is_some_and(|o| o.talkback)
    };
    let talkback = caps.fx.then(|| {
        hint(
            button(strip::centered_label("talkback", theme::TEXT_SM * scale))
                .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                .width(Length::Fill)
                .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
                .style(theme::stereo_button(talkback_on))
                .on_press(Message::TalkbackReceiveChanged(out, !talkback_on)),
            "Receives the Talkback source while Talkback is active",
            scale,
        )
    });
    col = col.push(settings_slot(talkback, SETTINGS_ROW_CTRL, scale));

    // Row 9 — loopback.
    let loopback = p.loopback_supported.then(|| {
        hint(
            button(strip::centered_label("loopback", theme::TEXT_SM * scale))
                .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                .width(Length::Fill)
                .height(Length::Fixed(SETTINGS_CONTROL_H * scale))
                .style(theme::stereo_button(p.loopback))
                .on_press(Message::LoopbackChanged(out, !p.loopback)),
            "Records this output on the same-numbered input channel",
            scale,
        )
    });
    col = col.push(settings_slot(loopback, SETTINGS_ROW_CTRL, scale));

    flyout_card(col, width, scale)
}

/// The Dynamics flyout: Compressor, Expander, the shared CompEx gain/attack/
/// release and Auto Level, in the order of the UCX manual (§25.3.3).
fn dynamics_popover(
    state: &TotalControl,
    cid: ChannelId,
    width: f32,
    scale: f32,
) -> Element<'_, Message> {
    let d = dynamics_for(state, cid);
    let modifiers = state.modifiers;
    // Vertical proportions follow the measured TotalMix FX 2.x panel:
    // header 21, graph 71, gap 3, gain row 60, gap 10, comp/exp 96.4,
    // gap 3, auto level 62.  These land on the measured line centres.
    let ds = scale * 0.76;
    let mut col = column![].spacing(0.0);

    // One knob cell.  `label_gap` is the vertical distance from the knob
    // box to its label; the gain row uses the larger RME spacing, Auto
    // Level uses the closer spacing.
    let cell = move |label: Option<&'static str>,
                     label_gap: f32,
                     overlap: bool,
                     colour: Color,
                     value: f32,
                     range: (f32, f32),
                     value_text: String,
                     reset: f32,
                     mutate: fn(&mut Dynamics, f32)|
     -> Element<'static, Message> {
        let d = d;
        let knob_box = 54.0 * ds;
        let face = container(knob(Knob {
            value,
            range,
            label: value_text,
            arc_from_center: range.0 < 0.0,
            level_arc_color: Some(colour),
            interactive: true,
            log_scale: false,
            modifiers,
            scale: ds,
            on_change: Box::new(move |v| {
                let mut next = d;
                mutate(&mut next, v);
                Message::DynamicsChanged(cid, next)
            }),
            on_reset: Box::new(move || {
                let mut next = d;
                mutate(&mut next, reset);
                Message::DynamicsChanged(cid, next)
            }),
        }))
        .width(Length::Fill)
        .center_x(Length::Fill);
        match label {
            Some(label) => {
                let label_text = text(label)
                    .size(theme::TEXT_XS * scale)
                    .color(colour)
                    .width(Length::Fill)
                    .center();
                if overlap {
                    let chip = container(label_text)
                        .padding([0.0, theme::SPACE_TIGHT * scale])
                        .style(theme::label_backdrop);
                    // Keep the label in the same bottom position but give the
                    // knob 4 px more room above it, reducing the overlap.
                    let stack_h = knob_box + 8.0 * scale;
                    iced::widget::Stack::new()
                        .width(Length::Fill)
                        .height(Length::Fixed(stack_h))
                        .push(face)
                        .push(
                            container(chip)
                                .width(Length::Fill)
                                .height(Length::Fixed(stack_h))
                                .align_y(iced::Alignment::End)
                                .center_x(Length::Fill),
                        )
                        .into()
                } else {
                    column![face, label_text]
                        .spacing(label_gap * scale)
                        .align_x(iced::Alignment::Center)
                        .width(Length::Fill)
                        .into()
                }
            }
            None => face.into(),
        }
    };

    let toggle = |on: bool, msg: Message| -> Element<'static, Message> {
        button(text(if on { "ON" } else { "OFF" }).size(theme::TEXT_XS * scale))
            .padding([0.0, theme::SPACE_SM * scale])
            .style(theme::toggle_button(on, theme::ACCENT))
            .on_press(msg)
            .into()
    };

    col = col.push(
        container(
            row![
                toggle(d.on, Message::DynamicsEnabled(cid, !d.on)),
                text("dynamics").size(theme::TEXT_SM * scale),
                iced::widget::Space::new().width(Length::Fill),
            ]
            .spacing(theme::SPACE_SM * scale)
            .align_y(iced::Alignment::Center)
            .width(Length::Fill),
        )
        .height(Length::Fixed(21.0 * scale))
        .align_y(iced::Alignment::Center),
    );

    col = col.push(dynamics_plot(d, 75.0 * scale, scale));
    col = col.push(iced::widget::Space::new().height(Length::Fixed(3.0 * scale)));

    col = col.push(
        row![
            cell(
                Some("gain"),
                theme::KNOB_LABEL_GAP,
                false,
                theme::TEXT_PRIMARY,
                d.gain_db,
                (-30.0, 30.0),
                format!("{:+.1}", d.gain_db),
                0.0,
                |d, v| d.gain_db = v,
            ),
            cell(
                Some("attack"),
                theme::KNOB_LABEL_GAP,
                false,
                theme::TEXT_PRIMARY,
                d.attack_ms as f32,
                (0.0, 200.0),
                format!("{}", d.attack_ms),
                10.0,
                |d, v| d.attack_ms = v.round().clamp(0.0, 200.0) as u16,
            ),
            cell(
                Some("release"),
                theme::KNOB_LABEL_GAP,
                false,
                theme::TEXT_PRIMARY,
                d.release_ms as f32,
                (100.0, 999.0),
                format!("{}", d.release_ms),
                300.0,
                |d, v| d.release_ms = v.round().clamp(100.0, 999.0) as u16,
            ),
        ]
        .spacing(theme::SPACE_TIGHT * scale)
        .width(Length::Fill),
    );
    col = col.push(iced::widget::Space::new().height(Length::Fixed(10.0 * scale)));

    let mut comp_rows = column![].spacing(3.4 * scale);
    let mut comp_exp = column![].spacing(theme::KNOB_LABEL_GAP * scale);
    let blank = || -> Element<'static, Message> {
        container(iced::widget::Space::new())
            .width(Length::Fill)
            .into()
    };
    let heading = |label: &'static str, colour: Color| -> Element<'static, Message> {
        container(
            text(label)
                .size(theme::TEXT_XS * scale)
                .color(colour)
                .width(Length::Fill)
                .center(),
        )
        .width(Length::Fill)
        .into()
    };
    comp_exp = comp_exp.push(
        row![
            blank(),
            heading("compressor", theme::MRED),
            heading("expander", theme::MGREEN),
        ]
        .spacing(theme::SPACE_TIGHT * scale)
        .width(Length::Fill),
    );
    let row_label = |top: &'static str, bottom: &'static str| -> Element<'static, Message> {
        container(
            column![
                text(top).size(theme::TEXT_XS * scale).color(theme::TEXT_SEC),
                text(bottom).size(theme::TEXT_XS * scale).color(theme::TEXT_SEC),
            ]
            .width(Length::Fill),
        )
        .width(Length::Fill)
        .align_x(iced::Alignment::Start)
        .into()
    };
    comp_rows = comp_rows.push(
        row![
            row_label("threshold", "dB"),
            cell(
                None,
                0.0,
                false,
                theme::MRED,
                d.comp_threshold_db,
                (-60.0, 0.0),
                format!("{:.1}", d.comp_threshold_db),
                -30.0,
                |d, v| d.comp_threshold_db = v,
            ),
            cell(
                None,
                0.0,
                false,
                theme::MGREEN,
                d.exp_threshold_db,
                (-99.0, -30.0),
                format!("{:.1}", d.exp_threshold_db),
                -60.0,
                |d, v| d.exp_threshold_db = v,
            ),
        ]
        .spacing(theme::SPACE_TIGHT * scale)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill),
    );
    comp_rows = comp_rows.push(
        row![
            container(
                text("ratio 1:")
                    .size(theme::TEXT_XS * scale)
                    .color(theme::TEXT_SEC),
            )
            .width(Length::Fill)
            .align_x(iced::Alignment::Start),
            cell(
                None,
                0.0,
                false,
                theme::MRED,
                d.comp_ratio,
                (1.0, 10.0),
                format!("{:.1}", d.comp_ratio),
                1.0,
                |d, v| d.comp_ratio = v,
            ),
            cell(
                None,
                0.0,
                false,
                theme::MGREEN,
                d.exp_ratio,
                (1.0, 10.0),
                format!("{:.1}", d.exp_ratio),
                1.0,
                |d, v| d.exp_ratio = v,
            ),
        ]
        .spacing(theme::SPACE_TIGHT * scale)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill),
    );
    comp_exp = comp_exp.push(comp_rows);
    col = col.push(comp_exp);
    col = col.push(iced::widget::Space::new().height(Length::Fixed(3.0 * scale)));

    let mut auto = column![].spacing(theme::KNOB_LABEL_GAP * scale);
    auto = auto.push(
        row![
            toggle(d.auto_level, Message::DynamicsChanged(
                cid,
                Dynamics { auto_level: !d.auto_level, ..d }
            )),
            text("auto level").size(theme::TEXT_XS * scale).color(theme::TEXT_SEC),
            iced::widget::Space::new().width(Length::Fill),
        ]
        .spacing(theme::SPACE_SM * scale)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill),
    );
    auto = auto.push(
        row![
            cell(
                Some("max gain"),
                0.0,
                true,
                theme::TEXT_PRIMARY,
                d.auto_max_gain_db,
                (0.0, 18.0),
                format!("{:.1}", d.auto_max_gain_db),
                6.0,
                |d, v| d.auto_max_gain_db = v,
            ),
            cell(
                Some("headroom"),
                0.0,
                true,
                theme::TEXT_PRIMARY,
                d.auto_headroom_db,
                (3.0, 12.0),
                format!("{:.1}", d.auto_headroom_db),
                6.0,
                |d, v| d.auto_headroom_db = v,
            ),
            cell(
                Some("rise time"),
                0.0,
                true,
                theme::TEXT_PRIMARY,
                d.auto_rise_s,
                (0.1, 9.9),
                format!("{:.1}", d.auto_rise_s),
                5.0,
                |d, v| d.auto_rise_s = v,
            ),
        ]
        .spacing(theme::SPACE_TIGHT * scale)
        .width(Length::Fill),
    );
    col = col.push(auto);

    // Keep the scrollable wrapper as a safety net; at normal size the
    // content fits the panel and no scrollbar is shown.
    flyout_card(
        scrollable(col)
            .direction(scrollable::Direction::Vertical(theme::thin_scrollbar()))
            .style(theme::scrollable)
            .width(Length::Fill)
            .height(Length::Fill),
        width,
        scale,
    )
}

/// The Trim flyout's content: a single knob, -65..+6 dB on the fader's
/// own master curve (see `RmeDevice::set_trim`). Every hardware input
/// gets this trigger (`StripParams::has_trim`, unconditional — unlike
/// Gain, which is Mic/Instrument-only), so unlike `eq_popover` there's
/// no `Option`-shaped hardware gate to check here, just the channel kind.
/// Same "pushes the row" shape as `settings_popover`/`eq_popover`.
fn trim_popover(state: &TotalControl, cid: ChannelId, width: f32, scale: f32) -> Element<'_, Message> {
    if matches!(cid, ChannelId::Output(_)) {
        return container(iced::widget::Space::new()).into();
    }
    let modifiers = state.modifiers;
    let trim = channel_trim(state, cid);

    let col = column![
        text("Trim").size(theme::TEXT_SM * scale).color(theme::TEXT_SEC),
        container(knob(Knob {
            value: trim,
            range: (-65.0, 0.0),
            label: knob_db_label(trim),
            arc_from_center: false,
            level_arc_color: Some(theme::TEXT_PRIMARY),
            interactive: true,
            log_scale: false,
            modifiers,
            scale,
            on_change: Box::new(move |v| Message::TrimChanged(cid, v)),
            on_reset: Box::new(move || Message::TrimReset(cid)),
        }))
        .width(Length::Fill)
        .center_x(Length::Fill),
    ]
    .spacing(theme::SPACE_SM * scale);

    settings_card(col, width, scale)
}

/// The EQ flyout's content: the interactive response plot, then a
/// three-column band matrix (one column per band) with type, gain,
/// frequency and Q rows, and the low-cut slope/frequency row below.
/// Laid out like TotalMix's own EQ panel, with TotalControl's knobs and
/// palette. Analog inputs only (`ch.eq: Some(_)`).
///
/// The panel is height-constrained to the strip so opening it does not
/// stretch the row; its controls scroll when the window is too short to
/// show them all. The frequency knobs are log-scaled, matching the plot's
/// axis.
fn eq_popover(state: &TotalControl, cid: ChannelId, width: f32, scale: f32) -> Element<'_, Message> {
    let eq = match cid {
        ChannelId::Input(idx) => state.device.inputs().get(idx).and_then(|c| c.eq),
        ChannelId::Output(idx) => state.device.outputs().get(idx).and_then(|c| c.eq),
        ChannelId::Playback(_) => None,
    };
    let Some(eq) = eq else {
        return container(iced::widget::Space::new()).into();
    };
    let low_cut_supported = state.device.capabilities().low_cut;
    let modifiers = state.modifiers;
    let label_w = 42.0 * scale;
    let gap = theme::SPACE_TIGHT;
    // The EQ matrix has to fit inside one strip's height. Smaller knobs
    // here (the strip's own gain/pan knobs keep their full size) are what
    // makes the three control rows and the low cut fit without scrolling.
    let eq_scale = scale * 0.9;

    let mut col = column![].spacing(gap);

    col = col.push(
        row![
            button(text("EQ").size(theme::TEXT_XS * scale))
                .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                .style(theme::toggle_button(eq.enabled, theme::ACCENT))
                .on_press(Message::EqEnabled(cid, !eq.enabled)),
            text(if eq.enabled { "enabled" } else { "bypassed" })
                .size(theme::TEXT_XS * scale)
                .color(theme::TEXT_SEC),
        ]
        .spacing(theme::SPACE_SM * scale)
        .align_y(iced::Alignment::Center),
    );

    col = col.push(eq_plot(
        eq,
        state.device.settings().sample_rate,
        scale,
        modifiers,
        EqPlotActions {
            on_band: Box::new(move |band, freq, gain| {
                Message::EqBandPoint(cid, band, freq, gain)
            }),
            on_band_reset: Box::new(move |band| Message::EqBandReset(cid, band)),
            on_band_q: Box::new(move |band, q| Message::EqBandQ(cid, band, q)),
            on_low_cut: Box::new(move |freq| Message::EqLowCutFreq(cid, freq)),
        },
    ));

    let label = |content: &'static str| {
        container(
            text(content)
                .size(theme::TEXT_XS * scale)
                .color(theme::TEXT_SEC),
        )
        .width(Length::Fixed(label_w))
    };

    let mut type_row = row![label("type")]
        .spacing(gap)
        .align_y(iced::Alignment::Center);
    for band in 0..3 {
        let b = eq.bands[band];
        // UCX band 2 is fixed to Bell (manual §25.3.2).  Band 1 is
        // Bell/Low Shelf and band 3 is Bell/High Shelf/Hi Cut.  The cycle
        // never offers a type the unit does not have.
        let (short, full, next_type) = match (band, b.band_type) {
            (_, EqBandType::Off) => ("Off", "Off (click for Bell)", EqBandType::Bell),
            (1, EqBandType::Bell) => ("Bell", "Bell (click for Off)", EqBandType::Off),
            (1, _) => ("Bell", "Bell (fixed for band 2)", EqBandType::Bell),
            (0, EqBandType::Bell) => (
                "Bell",
                "Bell (click for Low Shelf)",
                EqBandType::LowShelf,
            ),
            (0, EqBandType::LowShelf) => {
                ("Low", "Low Shelf (click for Off)", EqBandType::Off)
            }
            (0, _) => ("Bell", "Bell (band 1 has no shelf on this side)", EqBandType::Bell),
            (2, EqBandType::Bell) => (
                "Bell",
                "Bell (click for High Shelf)",
                EqBandType::HighShelf,
            ),
            (2, EqBandType::HighShelf) => (
                "High",
                "High Shelf (click for Hi Cut)",
                EqBandType::LowPass,
            ),
            (2, EqBandType::LowPass) => {
                ("Hi Cut", "Hi Cut, low pass (click for Off)", EqBandType::Off)
            }
            (2, _) => ("Bell", "Bell (band 3 has no low shelf)", EqBandType::Bell),
            _ => ("Off", "Off", EqBandType::Bell),
        };
        let number_color = if b.band_type == EqBandType::Off {
            theme::EQ_BAND[band]
        } else {
            theme::ON_ACTIVE
        };
        type_row = type_row.push(
            container(hint(
                button(
                    row![
                        text(format!("{}", band + 1))
                            .size(theme::TEXT_XS * scale)
                            .color(number_color),
                        text(short).size(theme::TEXT_XS * scale),
                    ]
                    .spacing(theme::SPACE_TIGHT)
                    .align_y(iced::Alignment::Center),
                )
                .padding([theme::SPACE_TIGHT * scale, theme::SPACE_TIGHT * scale])
                .width(Length::Fill)
                .style(theme::toggle_button(
                    b.band_type != EqBandType::Off,
                    theme::EQ_BAND[band],
                ))
                .on_press(Message::EqBandType(cid, band, next_type)),
                full,
                scale,
            ))
            .width(Length::Fill)
            .center_x(Length::Fill),
        );
    }
    col = col.push(type_row);

    let mut gain_row = row![label("gain dB")]
        .spacing(gap)
        .align_y(iced::Alignment::Center);
    for band in 0..3 {
        let b = eq.bands[band];
        gain_row = gain_row.push(
            container(hint(
                knob(Knob {
                    value: b.gain_db,
                    range: (-20.0, 20.0),
                    label: format!("{:+.1}", b.gain_db),
                    arc_from_center: true,
                    level_arc_color: Some(theme::EQ_BAND[band]),
                    interactive: true,
                    log_scale: false,
                    modifiers,
                    scale: eq_scale,
                    on_change: Box::new(move |v| {
                        Message::EqBandGain(cid, band, v.clamp(-20.0, 20.0))
                    }),
                    on_reset: Box::new(move || Message::EqBandGain(cid, band, 0.0)),
                }),
                "Band gain",
                scale,
            ))
            .width(Length::Fill)
            .center_x(Length::Fill),
        );
    }
    col = col.push(gain_row);

    let mut freq_row = row![label("freq Hz")]
        .spacing(gap)
        .align_y(iced::Alignment::Center);
    for band in 0..3 {
        let b = eq.bands[band];
        freq_row = freq_row.push(
            container(hint(
                knob(Knob {
                    value: b.freq_hz as f32,
                    range: (20.0, 20_000.0),
                    label: freq_text(b.freq_hz),
                    arc_from_center: false,
                    level_arc_color: Some(theme::TEXT_PRIMARY),
                    interactive: true,
                    log_scale: true,
                    modifiers,
                    scale: eq_scale,
                    on_change: Box::new(move |v| {
                        Message::EqBandFreq(cid, band, v.round().clamp(20.0, 20_000.0) as u16)
                    }),
                    on_reset: Box::new(move || Message::EqBandFreq(cid, band, 1000)),
                }),
                "Band frequency",
                scale,
            ))
            .width(Length::Fill)
            .center_x(Length::Fill),
        );
    }
    col = col.push(freq_row);

    let mut q_row = row![label("Q")]
        .spacing(gap)
        .align_y(iced::Alignment::Center);
    for band in 0..3 {
        let b = eq.bands[band];
        q_row = q_row.push(
            container(hint(
                knob(Knob {
                    value: b.q,
                    range: (0.7, 5.0),
                    label: format!("{:.2}", b.q),
                    arc_from_center: false,
                    level_arc_color: Some(theme::TEXT_PRIMARY),
                    interactive: true,
                    log_scale: false,
                    modifiers,
                    scale: eq_scale,
                    on_change: Box::new(move |v| {
                        Message::EqBandQ(cid, band, v.clamp(0.7, 5.0))
                    }),
                    on_reset: Box::new(move || Message::EqBandQ(cid, band, 0.7)),
                }),
                "Band Q",
                scale,
            ))
            .width(Length::Fill)
            .center_x(Length::Fill),
        );
    }
    col = col.push(q_row);

    let lc_label = if width / scale >= 230.0 {
        "Low Cut"
    } else {
        "LC"
    };
    let lc_button: Option<Element<'_, Message>> = if low_cut_supported {
        Some(
            container(hint(
                button(text(lc_label).size(theme::TEXT_XS * scale))
                    .padding([theme::SPACE_TIGHT * scale, theme::SPACE_TIGHT * scale])
                    .width(Length::Fill)
                    .style(theme::toggle_button(eq.low_cut_enabled, theme::ACCENT))
                    .on_press(Message::EqLowCutEnabled(cid, !eq.low_cut_enabled)),
                "Low cut on/off (independent of the main EQ bypass)",
                scale,
            ))
            .width(Length::Fixed(58.0 * scale))
            .into(),
        )
    } else {
        None
    };
    let mut slope_row = row![].spacing(gap).width(Length::Fill);
    for slope in [6u8, 12, 18, 24] {
        slope_row = slope_row.push(
            container(hint(
                button(text(format!("{slope}")).size(theme::TEXT_XS * scale))
                    .padding([theme::SPACE_TIGHT * scale, theme::SPACE_TIGHT * scale])
                    .width(Length::Fill)
                    .style(theme::toggle_button(
                        eq.low_cut_slope_db_oct == slope,
                        theme::ACCENT,
                    ))
                    .on_press(Message::EqLowCutSlope(cid, slope)),
                "Low-cut slope (dB/oct)",
                scale,
            ))
            .width(Length::Fill),
        );
    }

    let freq_col = container(hint(
        knob(Knob {
            value: eq.low_cut_freq_hz as f32,
            range: (20.0, 500.0),
            label: freq_text(eq.low_cut_freq_hz),
            arc_from_center: false,
            level_arc_color: Some(theme::TEXT_PRIMARY),
            interactive: true,
            log_scale: true,
            modifiers,
            scale: eq_scale,
            on_change: Box::new(move |v| {
                Message::EqLowCutFreq(cid, v.round().clamp(20.0, 500.0) as u16)
            }),
            on_reset: Box::new(move || Message::EqLowCutFreq(cid, 20)),
        }),
        "Low-cut frequency",
        scale,
    ))
    .width(Length::Fill)
    .center_x(Length::Fill);

    let mut low_cut_row = row![].spacing(gap).align_y(iced::Alignment::Center);
    if let Some(lc) = lc_button {
        low_cut_row = low_cut_row.push(lc);
    }
    col = col.push(
        low_cut_row
            .push(freq_col)
            .push(slope_row),
    );

    container(
        scrollable(col)
            .direction(scrollable::Direction::Vertical(theme::hidden_scrollbar()))
            .style(theme::scrollable)
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .padding(theme::FLYOUT_PAD * scale)
    .width(Length::Fixed(width))
    .height(Length::Fill)
    .style(theme::panel)
    .clip(true)
    .into()
}

/// Layers the open route flyout on top of a strip row, positioned by
/// left-padding computed from `open_x` (the open strip's right edge within
/// the row — see `mixer_view`). `None` (nothing open, or the open flyout
/// is `Settings` — see below) skips the `Stack` entirely — the common
/// case, so most frames don't pay for it.
///
/// Settings doesn't come through here at all: unlike Route, which
/// deliberately slides out *over* the strip's right neighbor, the
/// reference design (`Bus+settings_opened.png`) shows the gear panel
/// pushing the row — growing the strip's own footprint rather than
/// floating above whatever's next to it. `mixer_view`'s input loop
/// handles that directly, by widening the row item itself when a strip's
/// Settings flyout is open, which naturally pushes every strip after it
/// over — ordinary layout, no `Stack`/`opaque` overlay machinery needed.
///
/// Positioning assumes the row isn't horizontally scrolled — a known v1
/// gap (no per-row scroll-offset tracking yet); the panel can land at a
/// slightly wrong x if it is.
fn with_flyout<'a>(
    state: &'a TotalControl,
    row_element: Element<'a, Message>,
    route: Option<(ChannelId, f32)>,
) -> Element<'a, Message> {
    let now = Instant::now();
    let Some((cid, x)) = route else {
        return row_element;
    };
    let open = state
        .flyout_open
        .contains(&(cid, strip::FlyoutKind::Route));
    let Some(width) = flyout_slot(
        state,
        cid,
        strip::FlyoutKind::Route,
        strip::FLYOUT_W,
        now,
    ) else {
        return row_element;
    };
    if !open && width <= 0.0 {
        return row_element;
    }
    let content = route_popover(state, width, state.ui_scale);
    // `opaque` captures clicks across its *own* widget's bounds — those
    // have to be just the small popover itself (its natural, tightly-fit
    // size), not the Length::Fill positioning container around it. Get
    // that backwards (opaque wrapping the Fill container) and it silently
    // swallows every click anywhere in the row, popover visible there or
    // not, and the click-outside-closes catcher below never sees a thing.
    // So: outer Fill container (plain, not opaque — a click landing on
    // its empty padding is free to fall through to the catcher) positions
    // an inner `opaque(...)` (small, only as big as the panel actually
    // is) via padding + bottom alignment.
    stack(vec![
        row_element,
        mouse_area(
            iced::widget::Space::new()
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .on_press(Message::CloseFlyout)
        .into(),
        container(opaque(content))
            .padding(iced::Padding {
                left: x,
                ..iced::Padding::ZERO
            })
            .width(Length::Fill)
            .height(Length::Fill)
            .align_bottom(Length::Fill)
            .into(),
    ])
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

// ── FX column (Reverb / Echo) ─────────────────────────────────────
//
// TotalMix's own place for the FX bus: a column between the mixer and the
// control strip, Reverb above Echo. Every height below is measured from the
// RME panel (`docs/work/UCX-FX-PANEL-METRICS.md`) and scaled by 1.125, the
// ratio between TotalControl's 90 px strip and the 80 px strip in the same
// screenshot, so a 160 px column becomes 180 px.

/// Content width: 160 px at RME scale.
const FX_W: f32 = 180.0;
/// Effect name and its on switch.
const FX_HEADER_H: f32 = 36.0;
/// `type` caption and the algorithm pick list.
const FX_TYPE_H: f32 = 31.5;
/// Reverb controls: three rows of two knob columns plus the volume fader,
/// which spans the first two rows.
const FX_REV_CONTROLS_H: f32 = 247.5;
/// The Reverb panel's tail. This is the type-specific parameter band:
/// `Envelope` puts attack, hold and release here, `Gated` hold and release,
/// the Space types reverb time and high damp. It stays reserved and empty
/// until those registers are known (`docs/work/UCX-OPEN-ITEMS.md`).
const FX_REV_TAIL_H: f32 = 98.0;
/// Echo controls: four rows plus the high-cut picker.
const FX_ECHO_CONTROLS_H: f32 = 360.0;
/// Row pitch: 70 px at RME scale. Each row owns a whole pitch, so the
/// rhythm holds whatever the label's font metrics turn out to be.
const FX_ROW_H: f32 = 78.75;
/// FX knobs use the house knob size — the same 42 px face as a channel's
/// balance knob. They are only scaled down where a row needs more of them
/// than the width holds.
const FX_KNOB: f32 = 1.0;
/// Knob-area width and fader-column width inside the content box. The
/// measured column centres land at 34, 94 and 152 px.
const FX_KNOBS_W: f32 = 120.0;
const FX_FADER_W: f32 = 56.0;

fn fx_column(state: &TotalControl, width: f32) -> Element<'_, Message> {
    let scale = state.ui_scale;
    let full = FX_W * scale;
    let body = column![reverb_panel(state, scale), echo_panel(state, scale)]
        .spacing(theme::SPACE_SM * scale);
    // The two panels together are 840 px tall, short enough for the default
    // window; the scrollable is the safety net for a squeezed one.
    let column = container(
        scrollable(body)
            .direction(scrollable::Direction::Vertical(theme::thin_scrollbar()))
            .style(theme::scrollable)
            .width(Length::Fixed(full))
            .height(Length::Fill),
    )
    .width(Length::Fixed(full))
    .height(Length::Fill)
    .padding(iced::Padding {
        top: theme::SPACE_MD * scale,
        ..iced::Padding::ZERO
    });
    container(column)
        .width(Length::Fixed(width.max(0.0)))
        .height(Length::Fill)
        .clip(true)
        .into()
}

/// One FX panel's title bar: the on switch at the left, then the effect
/// name — lowercase, like every ordinary noun in a panel.
fn fx_header_row(
    name: &'static str,
    on: bool,
    toggle: Message,
    scale: f32,
) -> Element<'static, Message> {
    container(
        row![
            iced::widget::button(text(if on { "ON" } else { "OFF" }).size(theme::TEXT_XS * scale))
                .padding([0.0, theme::SPACE_MD * scale])
                .style(theme::toggle_button(on, theme::ACCENT))
                .on_press(toggle),
            text(name).size(theme::TEXT_SM * scale).color(theme::TEXT_SEC),
            iced::widget::Space::new().width(Length::Fill),
        ]
        .spacing(theme::SPACE_MD * scale)
        .align_y(iced::Alignment::Center),
    )
    .height(Length::Fixed(FX_HEADER_H * scale))
    .align_y(iced::Alignment::Center)
    .padding([0.0, theme::SPACE_SM * scale])
    .width(Length::Fill)
    .style(theme::fx_header)
    .into()
}

/// The `type` row: a caption at the left edge, then the algorithm pick list
/// filling the rest of the row.
fn fx_centered_pick(
    options: Vec<String>,
    selected: String,
    on_select: impl Fn(String) -> Message + 'static,
    height: f32,
    scale: f32,
) -> Element<'static, Message> {
    let selected_label = selected.clone();
    let pick = pick_list(options, Some(selected), on_select)
        .style(theme::pick_list_no_text)
        .menu_style(theme::menu)
        .padding(iced::Padding {
            top: 1.0 * scale,
            right: 10.0 * scale,
            bottom: 1.0 * scale,
            left: 1.0 * scale,
        })
        .text_size(theme::TEXT_XS * scale)
        .text_line_height(iced::widget::text::LineHeight::Absolute(
            iced::Pixels((height - 2.0) * scale),
        ))
        .width(Length::Fill)
        .handle(iced::widget::pick_list::Handle::None);
    let label = container(
        text(selected_label)
            .size(theme::TEXT_XS * scale)
            .color(theme::TEXT_PRIMARY),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(iced::Alignment::Center)
    .align_y(iced::Alignment::Center)
    .clip(true)
    .padding(iced::Padding {
        right: 10.0 * scale,
        left: 1.0 * scale,
        ..iced::Padding::ZERO
    });
    let arrow = container(
        text("▾")
            .size(theme::TEXT_XS * scale)
            .color(theme::TEXT_SEC),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(iced::Alignment::End)
    .align_y(iced::Alignment::Center)
    .padding(iced::Padding {
        right: theme::SPACE_SM * scale,
        ..iced::Padding::ZERO
    });
    iced::widget::stack![pick, label, arrow]
        .height(Length::Fixed(height * scale))
        .into()
}

fn fx_type_row(picker: Element<'static, Message>, scale: f32) -> Element<'static, Message> {
    container(
        row![
            text("type")
                .size(theme::TEXT_XS * scale)
                .color(theme::TEXT_SEC),
            picker,
        ]
        .spacing(theme::SPACE_SM * scale)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill),
    )
    .height(Length::Fixed(FX_TYPE_H * scale))
    .align_y(iced::Alignment::Center)
    .padding([0.0, theme::SPACE_SM * scale])
    .width(Length::Fill)
    .into()
}

/// One knob with its label underneath, the label centred on the knob. The
/// two function pointers build the bus message, so Reverb and Echo share
/// this instead of keeping a copy each.
#[allow(clippy::too_many_arguments)]
fn fx_knob_cell<'a, T: Copy + 'static>(
    label: &'static str,
    value: f32,
    range: (f32, f32),
    value_text: String,
    colour: Color,
    log: bool,
    reset: f32,
    base: T,
    commit: fn(T) -> Message,
    mutate: fn(&mut T, f32),
    modifiers: iced::keyboard::Modifiers,
    scale: f32,
) -> Element<'a, Message> {
    let control: Element<'a, Message> = knob(Knob {
        value,
        range,
        label: value_text,
        arc_from_center: range.0 < 0.0,
        level_arc_color: Some(colour),
        interactive: true,
        log_scale: log,
        modifiers,
        scale: FX_KNOB * scale,
        on_change: Box::new(move |v| {
            let mut next = base;
            mutate(&mut next, v);
            commit(next)
        }),
        on_reset: Box::new(move || {
            let mut next = base;
            mutate(&mut next, reset);
            commit(next)
        }),
    })
    .into();
    column![
        container(control)
            .width(Length::Fill)
            .center_x(Length::Fill),
        text(label)
            .size(theme::TEXT_XS * scale)
            .color(theme::TEXT_SEC)
            .width(Length::Fill)
            .center(),
    ]
    .spacing(theme::KNOB_LABEL_GAP * scale)
    .align_x(iced::Alignment::Center)
    .width(Length::Fill)
    .into()
}

fn fx_delay_cell(
    echo: Echo,
    scale: f32,
    modifiers: iced::keyboard::Modifiers,
) -> Element<'static, Message> {
    let delay_s = echo.delay_ms as f32 / 1000.0;
    let bpm = 60_000.0 / echo.delay_ms.max(1) as f32;
    let knob = fx_knob_cell(
        "delay time",
        delay_s,
        (0.01, 2.0),
        format!("{delay_s:.2}"),
        theme::KNOB_GREEN,
        true,
        0.6,
        echo,
        Message::EchoChanged,
        |e, v| e.delay_ms = (v * 1000.0).round().clamp(10.0, 2_000.0) as u16,
        modifiers,
        scale,
    );
    iced::widget::stack![
        container(knob).width(Length::Fill),
        container(
            text(format!("{bpm:.0}"))
                .size(theme::TEXT_XS * scale)
                .color(theme::TEXT_SEC)
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::Alignment::End)
        .align_y(iced::Alignment::End)
        .padding(iced::Padding {
            right: theme::SPACE_TIGHT * scale,
            bottom: 18.0 * scale,
            ..iced::Padding::ZERO
        }),
    ]
    .height(Length::Fixed(FX_ROW_H * scale))
    .width(Length::Fill)
    .into()
}

fn fx_meter_column(state: &TotalControl, scale: f32) -> Element<'static, Message> {
    let bar_h = 230.0 * scale;
    let bar_w = 14.0 * scale;
    let bus = state.meter_bus.clone();
    let bar = move |i: usize| -> Element<'static, Message> {
        // Service path: the canvas reads the shared bus at display rate.
        // Direct path: keep the view-fed progress bar.
        if let Some(bus) = &bus {
            let side = if i == 0 {
                fader::FxSide::In
            } else {
                fader::FxSide::Out
            };
            return fader::fx_meter(Some((bus.clone(), side)), bar_h, scale);
        }
        let meter = progress_bar(0.0..=1.0, state.fx_meters[i].frame().value)
            .vertical()
            .length(Length::Fixed(bar_h))
            .girth(Length::Fixed(bar_w))
            .style(theme::fx_meter);
        if state.fx_overload[i] {
            iced::widget::stack![
                meter,
                container(iced::widget::Space::new().height(Length::Fixed(4.0 * scale)))
                    .width(Length::Fixed(bar_w))
                    .style(theme::overload)
            ]
            .into()
        } else {
            meter.into()
        }
    };
    let bars = row![bar(0), bar(1)]
        .spacing(theme::SPACE_SM * scale)
        .align_y(iced::Alignment::End);
    let captions = row![
        text("In")
            .size(theme::TEXT_XS * scale)
            .color(theme::TEXT_SEC),
        text("Out")
            .size(theme::TEXT_XS * scale)
            .color(theme::TEXT_SEC),
    ]
    .spacing(theme::SPACE_MD * scale);
    container(
        column![
            text("FX")
                .size(theme::TEXT_XS * scale)
                .color(theme::TEXT_SEC),
            captions,
            bars
        ]
        .spacing(theme::SPACE_SM * scale)
        .align_x(iced::Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fixed(FX_ECHO_CONTROLS_H * scale))
    .align_y(iced::Alignment::Start)
    .into()
}

/// One fixed-pitch row of the controls band.
fn fx_row<'a>(
    left: Element<'a, Message>,
    right: Element<'a, Message>,
    scale: f32,
) -> Element<'a, Message> {
    container(
        row![left, right]
            .width(Length::Fill)
            .height(Length::Fixed(FX_ROW_H * scale)),
    )
    .height(Length::Fixed(FX_ROW_H * scale))
    .align_y(iced::Alignment::Start)
    .width(Length::Fill)
    .into()
}

fn fx_controls(
    rows: Element<'static, Message>,
    fader_col: Element<'static, Message>,
    right_col: Option<Element<'static, Message>>,
    knobs_w: f32,
    height: f32,
    top_pad: f32,
    scale: f32,
) -> Element<'static, Message> {
    let filler = FX_W - theme::SPACE_SM - knobs_w - FX_FADER_W;
    let mut band = row![
        container(rows).width(Length::Fixed(knobs_w * scale)),
        container(fader_col).width(Length::Fixed(FX_FADER_W * scale)),
    ];
    if let Some(extra) = right_col {
        band = band.push(container(extra).width(Length::Fixed(filler * scale)));
    } else if filler > 0.0 {
        band = band.push(iced::widget::Space::new().width(Length::Fixed(filler * scale)));
    }
    container(
        band
        // Inset on the left only: the columns add up to the panel width.
        .padding(iced::Padding {
            top: top_pad * scale,
            left: theme::SPACE_SM * scale,
            right: 0.0,
            bottom: 0.0,
        }),
    )
    .height(Length::Fixed(height * scale))
    .align_y(iced::Alignment::Start)
    .width(Length::Fill)
    .style(theme::fx_controls)
    .into()
}

/// A fixed-pitch row holding a single knob, for the panels whose fader sits
/// in the next column over.
fn fx_row_one<'a>(
    cell: Element<'a, Message>,
    scale: f32,
) -> Element<'a, Message> {
    container(
        container(cell)
            .width(Length::Fill)
            .height(Length::Fixed(FX_ROW_H * scale)),
    )
    .height(Length::Fixed(FX_ROW_H * scale))
    .align_y(iced::Alignment::Start)
    .width(Length::Fill)
    .into()
}

/// The selected Reverb type's own parameters, side by side in the tail
/// band under the knob rows.
fn fx_param_row<'a>(cells: Vec<Element<'a, Message>>, scale: f32) -> Element<'a, Message> {
    let mut r = row![].spacing(0.0).width(Length::Fill);
    for cell in cells {
        r = r.push(cell);
    }
    container(r)
        .height(Length::Fixed(FX_REV_TAIL_H * scale))
        .align_y(iced::Alignment::Start)
        .padding([theme::SPACE_SM * scale, theme::SPACE_SM * scale])
        .width(Length::Fill)
        .into()
}

fn reverb_panel(state: &TotalControl, scale: f32) -> Element<'_, Message> {
    let r = state.device.settings().reverb.unwrap_or_default();
    let modifiers = state.modifiers;
    let colour = theme::KNOB_BLUE;

    let header = fx_header_row(
        "reverb",
        r.on,
        Message::ReverbChanged(Reverb { on: !r.on, ..r }),
        scale,
    );
    let type_row = fx_type_row(
        pick_list(
            REVERB_TYPES.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
            Some(reverb_type_name(r.kind).to_string()),
            move |name| {
                let kind = REVERB_TYPES
                    .iter()
                    .position(|n| *n == name)
                    .unwrap_or(0) as u8;
                Message::ReverbChanged(Reverb { kind, ..r })
            },
        )
        .style(theme::pick_list)
        .menu_style(theme::menu)
        .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
        .text_size(theme::TEXT_XS * scale)
        .width(Length::Fill)
        .into(),
        scale,
    );

    let row1 = fx_row(
        fx_knob_cell(
            "pre delay",
            r.pre_delay_ms as f32,
            (0.0, 999.0),
            format!("{}", r.pre_delay_ms),
            colour,
            false,
            22.0,
            r,
            Message::ReverbChanged,
            |r, v| r.pre_delay_ms = v.round() as u16,
            modifiers,
            scale,
        ),
        fx_knob_cell(
            "room scale",
            r.room_scale,
            (0.5, 3.0),
            format!("{:.2}", r.room_scale),
            colour,
            false,
            1.5,
            r,
            Message::ReverbChanged,
            |r, v| r.room_scale = v,
            modifiers,
            scale,
        ),
        scale,
    );
    let row2 = fx_row(
        fx_knob_cell(
            "low cut",
            r.low_cut_hz as f32,
            (20.0, 500.0),
            format!("{}", r.low_cut_hz),
            colour,
            true,
            49.0,
            r,
            Message::ReverbChanged,
            |r, v| r.low_cut_hz = v.round() as u16,
            modifiers,
            scale,
        ),
        fx_knob_cell(
            "high cut",
            r.high_cut_hz as f32,
            (5_000.0, 20_000.0),
            format!("{}", r.high_cut_hz),
            colour,
            true,
            11_900.0,
            r,
            Message::ReverbChanged,
            |r, v| r.high_cut_hz = v.round() as u16,
            modifiers,
            scale,
        ),
        scale,
    );
    let row3 = fx_row(
        fx_knob_cell(
            "smooth",
            r.smoothness as f32,
            (0.0, 100.0),
            format!("{}", r.smoothness),
            colour,
            false,
            93.0,
            r,
            Message::ReverbChanged,
            |r, v| r.smoothness = v.round() as u8,
            modifiers,
            scale,
        ),
        fx_knob_cell(
            "width",
            r.width,
            (0.0, 100.0),
            format!("{:.0}", r.width),
            colour,
            false,
            52.0,
            r,
            Message::ReverbChanged,
            |r, v| r.width = v,
            modifiers,
            scale,
        ),
        scale,
    );

    let volume = fx_row_one(
        fx_knob_cell(
            "volume",
            r.volume_db,
            (-30.0, 6.0),
            format!("{:+.1}", r.volume_db),
            colour,
            false,
            -15.8,
            r,
            Message::ReverbChanged,
            |r, v| r.volume_db = v,
            modifiers,
            scale,
        ),
        scale,
    );
    let volume_column = column![
        iced::widget::Space::new().height(Length::Fill),
        volume
    ]
    .height(Length::Fixed(FX_REV_CONTROLS_H * scale))
    .width(Length::Fill);

    let rows = column![row1, row2, row3].spacing(0.0).width(Length::Fill);
    let controls = fx_controls(
        rows.into(),
        volume_column.into(),
        None,
        FX_KNOBS_W,
        FX_REV_CONTROLS_H,
        3.5,
        scale,
    );

    // The band under the knob rows carries the selected type's own
    // parameters: Attack/Hold/Release for Envelope (`0x3c05..0x3c07`),
    // Hold/Release for Gated, Reverb Time/High Damp for Space
    // (`0x3c09`, `0x3c0a`).
    let tail: Element<'static, Message> = match r.kind {
        12 => fx_param_row(
            vec![
                fx_knob_cell(
                    "attack",
                    r.attack_ms as f32,
                    (5.0, 400.0),
                    format!("{}", r.attack_ms),
                    colour,
                    false,
                    20.0,
                    r,
                    Message::ReverbChanged,
                    |r, v| r.attack_ms = v.round().clamp(5.0, 400.0) as u16,
                    modifiers,
                    scale,
                ),
                fx_knob_cell(
                    "hold",
                    r.hold_ms as f32,
                    (5.0, 400.0),
                    format!("{}", r.hold_ms),
                    colour,
                    false,
                    40.0,
                    r,
                    Message::ReverbChanged,
                    |r, v| r.hold_ms = v.round().clamp(5.0, 400.0) as u16,
                    modifiers,
                    scale,
                ),
                fx_knob_cell(
                    "release",
                    r.release_ms as f32,
                    (5.0, 500.0),
                    format!("{}", r.release_ms),
                    colour,
                    false,
                    100.0,
                    r,
                    Message::ReverbChanged,
                    |r, v| r.release_ms = v.round().clamp(5.0, 500.0) as u16,
                    modifiers,
                    scale,
                ),
            ],
            scale,
        ),
        13 => fx_param_row(
            vec![
                fx_knob_cell(
                    "hold",
                    r.hold_ms as f32,
                    (5.0, 400.0),
                    format!("{}", r.hold_ms),
                    colour,
                    false,
                    40.0,
                    r,
                    Message::ReverbChanged,
                    |r, v| r.hold_ms = v.round().clamp(5.0, 400.0) as u16,
                    modifiers,
                    scale,
                ),
                fx_knob_cell(
                    "release",
                    r.release_ms as f32,
                    (5.0, 500.0),
                    format!("{}", r.release_ms),
                    colour,
                    false,
                    100.0,
                    r,
                    Message::ReverbChanged,
                    |r, v| r.release_ms = v.round().clamp(5.0, 500.0) as u16,
                    modifiers,
                    scale,
                ),
            ],
            scale,
        ),
        14 => fx_param_row(
            vec![
                fx_knob_cell(
                    "reverb time",
                    r.reverb_time_ds as f32 / 10.0,
                    (0.1, 9.9),
                    format!("{:.1}", r.reverb_time_ds as f32 / 10.0),
                    colour,
                    false,
                    2.0,
                    r,
                    Message::ReverbChanged,
                    |r, v| r.reverb_time_ds = (v * 10.0).round().clamp(1.0, 99.0) as u16,
                    modifiers,
                    scale,
                ),
                fx_knob_cell(
                    "high damp",
                    r.high_damp_hz as f32,
                    (200.0, 20_000.0),
                    format!("{}", r.high_damp_hz),
                    colour,
                    true,
                    12_000.0,
                    r,
                    Message::ReverbChanged,
                    |r, v| r.high_damp_hz = v.round().clamp(200.0, 20_000.0) as u16,
                    modifiers,
                    scale,
                ),
            ],
            scale,
        ),
        _ => container(iced::widget::Space::new())
            .height(Length::Fixed(FX_REV_TAIL_H * scale))
            .width(Length::Fill)
            .into(),
    };

    container(column![header, type_row, controls, tail].spacing(0.0).width(Length::Fill))
        .width(Length::Fixed(FX_W * scale))
        .style(theme::panel)
        .clip(true)
        .into()
}

fn echo_panel(state: &TotalControl, scale: f32) -> Element<'_, Message> {
    let e = state.device.settings().echo.unwrap_or_default();
    let modifiers = state.modifiers;
    let colour = theme::KNOB_GREEN;

    let header = fx_header_row(
        "echo",
        e.on,
        Message::EchoChanged(Echo { on: !e.on, ..e }),
        scale,
    );
    let echo_type = match e.kind {
        0 => "Stereo Echo",
        1 => "Stereo Cross",
        _ => "Pong Echo",
    };
    let type_row = fx_type_row(
        fx_centered_pick(
            ECHO_TYPES.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
            echo_type.to_string(),
            move |name| {
                let kind = ECHO_TYPES
                    .iter()
                    .position(|n| *n == name)
                    .unwrap_or(0) as u8;
                Message::EchoChanged(Echo { kind, ..e })
            },
            24.0,
            scale,
        ),
        scale,
    );

    let row1 = fx_delay_cell(e, scale, modifiers);
    let row2 = fx_row_one(
        fx_knob_cell(
            "feedback",
            e.feedback as f32,
            (0.0, 100.0),
            format!("{}", e.feedback),
            colour,
            false,
            20.0,
            e,
            Message::EchoChanged,
            |e, v| e.feedback = v.round() as u8,
            modifiers,
            scale,
        ),
        scale,
    );
    let row3 = fx_row_one(
        fx_knob_cell(
            "width",
            e.width,
            (0.0, 100.0),
            format!("{:.0}", e.width),
            colour,
            false,
            100.0,
            e,
            Message::EchoChanged,
            |e, v| e.width = v,
            modifiers,
            scale,
        ),
        scale,
    );
    let high_cut_name = ECHO_HIGH_CUTS
        .get(e.high_cut as usize)
        .copied()
        .unwrap_or("Off");
    let row4 = fx_row_one(
        fx_knob_cell(
            "vol.",
            e.volume_db,
            (-30.0, 6.0),
            format!("{:+.1}", e.volume_db),
            colour,
            false,
            -10.0,
            e,
            Message::EchoChanged,
            |e, v| e.volume_db = v,
            modifiers,
            scale,
        ),
        scale,
    );
    let high_cut = container(
        row![
            text("HC")
                .size(theme::TEXT_XS * scale)
                .color(theme::TEXT_SEC),
            container(fx_centered_pick(
                ECHO_HIGH_CUTS
                    .iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>(),
                high_cut_name.to_string(),
                move |value: String| Message::EchoChanged(Echo {
                    high_cut: ECHO_HIGH_CUTS
                        .iter()
                        .position(|name| *name == value)
                        .unwrap_or(0) as u8,
                    ..e
                }),
                22.0,
                scale,
            ))
            .width(Length::Fixed(54.0 * scale)),
        ]
        .spacing(theme::SPACE_TIGHT * scale)
        .align_y(iced::Alignment::Center),
    )
    .height(Length::Fixed(24.0 * scale))
    .width(Length::Fill)
    .align_x(iced::Alignment::Center)
    .padding([0.0, theme::SPACE_SM * scale]);

    let knobs = column![row1, row2, row3, row4, high_cut]
        .spacing(0.0)
        .width(Length::Fixed(100.0 * scale));
    let meters = container(fx_meter_column(state, scale))
        .width(Length::Fixed(60.0 * scale));
    let controls = container(
        row![knobs, meters]
            .spacing(theme::SPACE_SM * scale)
            .padding(iced::Padding {
                top: 4.0 * scale,
                left: theme::SPACE_SM * scale,
                right: 0.0,
                bottom: 0.0,
            }),
    )
    .height(Length::Fixed(FX_ECHO_CONTROLS_H * scale))
    .width(Length::Fill)
    .style(theme::fx_controls);

    container(column![header, type_row, controls].spacing(0.0).width(Length::Fill))
        .width(Length::Fixed(FX_W * scale))
        .style(theme::panel)
        .clip(true)
        .into()
}

/// The Assign menu: a card anchored at the Control Room panel's left edge,
/// with the destination list on the right and the selected destination's
/// output list beside it. Opened by the `assign` button, closed by choosing
/// an output or a click anywhere else.
fn assign_menu_layer(state: &TotalControl) -> Element<'_, Message> {
    let scale = state.ui_scale;
    let gap = theme::SPACE_SM * scale;
    let bold = iced::Font {
        weight: iced::font::Weight::Bold,
        ..iced::Font::DEFAULT
    };
    let entry_w = (ASSIGN_MENU_W - 2.0 * theme::SPACE_SM) * scale;
    let entry = |label: String, strong: bool, message: Message| -> Element<'static, Message> {
        iced::widget::button(
            container(
                text(label)
                    .size(theme::TEXT_MD * scale)
                    .font(if strong { bold } else { iced::Font::DEFAULT })
                    .center(),
            )
            .width(Length::Fill)
            .align_x(iced::Alignment::Center),
        )
        .width(Length::Fixed(entry_w))
        .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
        .style(theme::ghost_button)
        .on_press(message)
        .into()
    };

    let mut cards = row![].spacing(gap).align_y(iced::Alignment::End);

    // Second level first: it sits to the left of the destination list.
    match state.assign_menu {
        AssignMenu::Outputs(dest) => {
            let labels = state.display_pair_labels();
            let order = state.device.output_display_order();
            let current = state.assign.get(dest).copied().flatten();
            let mut col = column![].spacing(0.0).width(Length::Fill);
            col = col.push(entry(
                "None".to_string(),
                current.is_none(),
                Message::AssignOutput(dest, None),
            ));
            for (display, label) in labels.iter().enumerate() {
                let internal = order.get(display).copied().unwrap_or(display);
                col = col.push(entry(
                    label.clone(),
                    current == Some(internal),
                    Message::AssignOutput(dest, Some(internal)),
                ));
            }
            cards = cards.push(opaque(
                container(col)
                    .padding(theme::SPACE_SM * scale)
                    .width(Length::Fixed(ASSIGN_MENU_W * scale))
                    .style(theme::dialog),
            ));
        }
        AssignMenu::CueTargets => {
            let current = state.cue_target();
            let mut col = column![].spacing(0.0).width(Length::Fill);
            for target in CUE_TARGETS {
                col = col.push(entry(
                    ASSIGN_DESTS[target].to_string(),
                    current == target,
                    Message::AssignCueTarget(target),
                ));
            }
            cards = cards.push(opaque(
                container(col)
                    .padding(theme::SPACE_SM * scale)
                    .width(Length::Fixed(ASSIGN_MENU_W * scale))
                    .style(theme::dialog),
            ));
        }
        _ => {}
    }

    // First level: the destinations. Choosing one opens the matching list;
    // the active destination is highlighted so the second level clearly
    // belongs to it. The row is a plain button, not a hover area: rebuilding
    // a hovered row under the pointer used to lose the click.
    let mut col = column![].spacing(0.0).width(Length::Fill);
    for (dest, name) in ASSIGN_DESTS.iter().enumerate() {
        let active = matches!(state.assign_menu, AssignMenu::Outputs(d) if d == dest)
            || (dest == CUE_DEST && matches!(state.assign_menu, AssignMenu::CueTargets));
        col = col.push(
            iced::widget::mouse_area(
                iced::widget::button(
                    container(
                        text(*name)
                            .size(theme::TEXT_MD * scale)
                            .center(),
                    )
                    .width(Length::Fill)
                    .align_x(iced::Alignment::Center),
                )
                .width(Length::Fixed(entry_w))
                .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                .style(theme::toggle_button(active, theme::ACCENT))
                .on_press(Message::AssignDestinations(dest)),
            )
            .on_enter(Message::AssignHover(dest)),
        );
    }
    cards = cards.push(opaque(
        container(col)
            .padding(theme::SPACE_SM * scale)
            .width(Length::Fixed(ASSIGN_MENU_W * scale))
            .style(theme::dialog),
    ));

    // The cards open immediately left of the assign button, which sits at
    // the bottom of the Control Room button stack. Each card is opaque on
    // its own; there is no background catcher, so a click outside reaches
    // the page or a strip and the update loop closes the menu.
    container(cards)
        .align_x(iced::Alignment::End)
        .align_y(iced::Alignment::End)
        .padding(iced::Padding {
            top: 0.0,
            // Inside the panel's own padding, the assign column's left edge.
            right: theme::SPACE_SM * scale + CONTROL_ROOM_W * scale + gap,
            // Panel padding, body padding and group padding sit below the
            // bottom Assign button.
            bottom: 3.0 * theme::SPACE_SM * scale,
            left: 0.0,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Width of the Control Room section: its button column plus one strip per
/// assigned monitoring output.
fn control_room_width(state: &TotalControl, scale: f32) -> f32 {
    let assigned = state.assigned_pairs().count() as f32;
    CONTROL_ROOM_W * scale
        + assigned * (strip::STRIP_W + theme::SPACE_MD) * scale
        + 2.0 * theme::SPACE_SM * scale
}

/// Control Room width including the slideout currently open beside one of its
/// channels. The header and the page width follow this, so the section grows
/// left with the panel instead of leaving the header behind.
fn control_room_visual_width(
    state: &TotalControl,
    scale: f32,
    now: Instant,
) -> f32 {
    let mut extra = 0.0f32;
    for dest in [2usize, 3, 4, 5, 1, 0] {
        let Some(pair) = state.assign[dest] else {
            continue;
        };
        if pair * 2 + 1 >= state.device.outputs().len() {
            continue;
        }
        let cid = ChannelId::Output(pair * 2);
        for (kind, width) in [
            (strip::FlyoutKind::Settings, strip::STRIP_W * scale),
            (strip::FlyoutKind::Eq, strip::EQ_FLYOUT_W * scale),
            (strip::FlyoutKind::Dynamics, strip::DYN_FLYOUT_W * scale),
        ] {
            if let Some(w) = flyout_slot(state, cid, kind, width, now) {
                extra = extra.max(w);
            }
        }
    }
    control_room_width(state, scale) + extra
}

/// The width an output pair occupies in the Hardware Outputs row. A linked
/// pair is one strip; a split pair is two strips plus the gap between them.
fn rendered_pair_width(state: &TotalControl, pair: usize, scale: f32) -> f32 {
    let left = ChannelId::Output(pair * 2);
    let left_w = rendered_strip_width(state, left);
    if state.device.output_linked(pair) {
        left_w
    } else {
        let right = ChannelId::Output(pair * 2 + 1);
        left_w + rendered_strip_width(state, right) + theme::SPACE_MD * scale
    }
}

/// Slot index of a Control Room destination, in the fixed left-to-right
/// order, for one assignment map.
fn cr_slot_index(assign: &[Option<usize>; OUTPUT_DESTS], dest: usize) -> Option<usize> {
    let mut index = 0usize;
    for d in [2usize, 3, 4, 5, 1, 0] {
        if d == dest {
            return assign[d].map(|_| index);
        }
        if assign[d].is_some() {
            index += 1;
        }
    }
    None
}

/// The returning pair's normal output-row strips, clipped to `clip_w`. The
/// clip grows during a clear, so the channel fills its slot from the right
/// instead of appearing all at once when the gap reaches full width.
fn output_row_clipped_pair(
    state: &TotalControl,
    pair: usize,
    clip_w: f32,
    scale: f32,
) -> Element<'_, Message> {
    let left = pair * 2;
    let linked = state.device.output_linked(pair);
    let mut items = row![].spacing(theme::SPACE_MD * scale);
    let channels: Vec<usize> = if linked {
        vec![left]
    } else {
        vec![left, left + 1]
    };
    for ch_idx in channels {
        let cid = ChannelId::Output(ch_idx);
        let mut params = strip_params(state, cid, state.sel_out);
        if linked && ch_idx == left {
            if let Some(right) = state.device.outputs().get(left + 1) {
                params.name = pair_bus_label(&params.name, &right.name);
            }
            params.meter2 = state.output_meters.get(left + 1).map(MeterAnim::frame);
            params.meter2_available = output_meter_available(state, left + 1);
        }
        items = items.push(strip::strip(finish_params(state, params)));
    }
    container(items)
        .width(Length::Fixed(clip_w))
        .height(Length::Fixed(strip::STRIP_H * scale))
        .align_x(iced::Alignment::End)
        .clip(true)
        .into()
}

/// Width of the temporary gap in the Hardware Outputs row during an add or
/// clear. The gap is at the moving pair's display position.
fn assign_gap_width(
    state: &TotalControl,
    anim: &AssignAnim,
    pair: usize,
    scale: f32,
    now: Instant,
) -> f32 {
    let pair_w = rendered_pair_width(state, pair, scale);
    let old_count = anim.old.iter().flatten().count();
    let new_count = anim.new.iter().flatten().count();
    let duration = (pair_w / 1.0).max(1.0);
    let t = (anim.elapsed_ms(now) / duration).clamp(0.0, 1.0);
    if new_count > old_count {
        pair_w * (1.0 - t)
    } else {
        pair_w * t
    }
}


/// Options > Reset Mix (manual 25.10): rewrite the captured scene, then push
/// it in one go. Every entry writes the whole mixer, which is what makes this
/// cheaper than clicking a matrix cell at a time.
fn apply_reset_mix(state: &mut TotalControl, kind: ResetMix) {
    if kind == ResetMix::ResetNames {
        state.labels.clear();
        state.editing = None;
        state.notice = Some(("Channel names reset".into(), std::time::Instant::now()));
        return;
    }
    let mut scene = state.device.capture_scene();
    let main_pair = scene.settings.main_out.unwrap_or(0);
    let b_pair = scene.settings.main_b_out;
    let pairs = (state.device.outputs().len() / 2).max(1);

    let silence = |vols: &mut Vec<f32>| vols.iter_mut().for_each(|v| *v = 0.0);

    match kind {
        ResetMix::StraightPlayback | ResetMix::StraightToMain | ResetMix::TotalReset => {
            for (i, pb) in scene.playbacks.iter_mut().enumerate() {
                silence(&mut pb.volumes);
                let pair = (i / 2).min(pairs - 1);
                if let Some(v) = pb.volumes.get_mut(pair) {
                    *v = 1.0;
                }
            }
            if kind != ResetMix::StraightPlayback {
                for pb in scene.playbacks.iter_mut() {
                    if let Some(v) = pb.volumes.get_mut(main_pair) {
                        *v = 1.0;
                    }
                }
            }
            if kind == ResetMix::TotalReset {
                clear_effects(&mut scene);
                set_output_volumes(&mut scene, main_pair, b_pair);
            }
        }
        ResetMix::ClearSubmixes => {
            for pb in scene.playbacks.iter_mut() {
                silence(&mut pb.volumes);
            }
            for inp in scene.inputs.iter_mut() {
                silence(&mut inp.volumes);
            }
            scene.matrix_overrides.clear();
        }
        ResetMix::ClearEffects => clear_effects(&mut scene),
        ResetMix::SetOutputVolumes => set_output_volumes(&mut scene, main_pair, b_pair),
        ResetMix::AllMono | ResetMix::AllStereo | ResetMix::InputsMonoOutputsStereo => {
            let (inputs_linked, outputs_linked) = match kind {
                ResetMix::AllMono => (false, false),
                ResetMix::AllStereo => (true, true),
                _ => (false, true),
            };
            for pair in 0..(state.device.inputs().len() / 2).max(1) {
                let _ = state.device.set_input_pair_linked(pair, inputs_linked);
            }
            for pair in 0..pairs {
                // Control Room channels are always stereo, so "Set all
                // channels mono" leaves the assigned pairs linked.
                let linked = outputs_linked || state.is_output_assigned(pair);
                let _ = state.device.set_output_linked(pair, linked);
            }
            state.notice = Some((
                "Channel stereo/mono reset".into(),
                std::time::Instant::now(),
            ));
            return;
        }
        ResetMix::ResetNames => return,
    }

    if let Err(e) = state.device.apply_scene(&scene) {
        fail(state, "reset_mix", &e);
        return;
    }
    state.notice = Some((
        RESET_MIX
            .iter()
            .find(|(_, k)| *k == kind)
            .map(|(name, _)| format!("Reset Mix: {name}"))
            .unwrap_or_else(|| "Reset Mix".into()),
        std::time::Instant::now(),
    ));
}

fn clear_effects(scene: &mut totalcontrol_core::Scene) {
    for ch in scene.inputs.iter_mut() {
        off_eq(&mut ch.eq);
        off_dynamics(&mut ch.dynamics);
    }
    for ch in scene.outputs.iter_mut() {
        off_eq(&mut ch.eq);
        off_dynamics(&mut ch.dynamics);
    }
    if let Some(r) = scene.settings.reverb.as_mut() {
        r.on = false;
    }
    if let Some(e) = scene.settings.echo.as_mut() {
        e.on = false;
    }
    scene.settings.width = 0.0;
}

fn off_eq(eq: &mut Option<totalcontrol_core::channel::InputEq>) {
    if let Some(eq) = eq.as_mut() {
        eq.enabled = false;
        eq.low_cut_enabled = false;
        for band in eq.bands.iter_mut() {
            band.band_type = totalcontrol_core::channel::EqBandType::Off;
        }
    }
}

fn off_dynamics(d: &mut Option<totalcontrol_core::Dynamics>) {
    if let Some(d) = d.as_mut() {
        d.on = false;
        d.auto_level = false;
    }
}

fn set_output_volumes(scene: &mut totalcontrol_core::Scene, main_pair: usize, b_pair: Option<usize>) {
    for (i, out) in scene.outputs.iter_mut().enumerate() {
        let pair = i / 2;
        out.volume = if pair == main_pair || Some(pair) == b_pair {
            0.316   // -10 dB
        } else {
            1.0
        };
    }
}

/// TotalMix's Control Room, beside the output strips it monitors (manual
/// 25.4): Assign picks the Main Out and Speaker B, and the monitoring
/// buttons act on those speaker pairs or, for Talkback, the Phones outputs.
///
/// `ext. input` is drawn without a press handler, which iced renders
/// dimmed: its runtime signal path is not implemented.  `assign` and
/// `talkback` open their menus or toggle the device path directly.
/// The buttons carry TotalMix's own order and background groups.
/// One channel wide: the button stack is exactly a strip wide, and every
/// Control Room button shares that width.
const CONTROL_ROOM_W: f32 = strip::STRIP_W;

/// Both Assign menu levels use this width so the cards align.
const ASSIGN_MENU_W: f32 = 150.0;

fn control_room_panel(
    state: &TotalControl,
    scale: f32,
    anim_now: std::time::Instant,
) -> Element<'_, Message> {
    let settings = state.device.settings();
    // Monitoring outputs that are assigned live here rather than in the
    // hardware-output row, renamed for their destination (manual 25.4:
    // "the channel will be shifted from the Hardware Outputs into the
    // Control Room section, and renamed Main").
    let mut strip_items: Vec<(f32, Element<'_, Message>)> = Vec::new();
    let mut slot_index = 0usize;
    let slot = (strip::STRIP_W + theme::SPACE_MD) * scale;
    for dest in [2usize, 3, 4, 5, 1, 0] {
        let Some(pair) = state.assign.get(dest).copied().flatten() else {
            continue;
        };
        if pair * 2 + 1 >= state.device.outputs().len() {
            continue;
        }
        let cid = ChannelId::Output(pair * 2);
        let mut params = strip_params(state, cid, state.sel_out);
        // The Control Room name is always one of the first six Assign
        // names. View → Names does not change it.
        params.name = ASSIGN_DESTS[dest].to_string();
        params.name_custom = true;
        params.name_locked = true;
        params.renameable = false;
        // A linked output pair shows both channels' meters, like the
        // Hardware Outputs row does.
        if state.device.output_linked(pair) {
            params.meter2 = state.output_meters.get(pair * 2 + 1).map(MeterAnim::frame);
            params.meter2_available = output_meter_available(state, pair * 2 + 1);
        }
        let strip_w = strip::STRIP_W * scale;
        let mut item: Element<'_, Message> = strip::strip(finish_params(state, params));
        // A slideout always opens to the right of its channel. The Control
        // Room is pinned to the window's right edge, so it shifts its own
        // channels left by one slot to make room; its left edge may
        // overlap the channel row, which never moves.
        for (kind, width) in [
            (strip::FlyoutKind::Settings, strip_w),
            (strip::FlyoutKind::Eq, strip::EQ_FLYOUT_W * scale),
            (strip::FlyoutKind::Dynamics, strip::DYN_FLYOUT_W * scale),
        ] {
            let Some(panel_w) = flyout_slot(state, cid, kind, width, anim_now) else {
                continue;
            };
            let panel = match kind {
                strip::FlyoutKind::Settings => settings_popover(state, cid, panel_w, scale),
                strip::FlyoutKind::Eq => eq_popover(state, cid, panel_w, scale),
                strip::FlyoutKind::Dynamics => dynamics_popover(state, cid, panel_w, scale),
                _ => continue,
            };
            item = row![item, panel].spacing(0.0).into();
            break;
        }
        // The final slot position; an add or clear slides the existing
        // slots between their old and new index at the same 1 px/ms speed.
        let final_x = slot_index as f32 * slot;
        let x = if let Some(anim) = state.assign_anim.as_ref() {
            if let Some(old_index) = cr_slot_index(&anim.old, dest) {
                let old_count = anim.old.iter().flatten().count();
                let new_count = anim.new.iter().flatten().count();
                let adding = new_count > old_count;
                let old_x = (old_index as f32 + if adding { 1.0 } else { -1.0 }) * slot;
                let dx = final_x - old_x;
                let distance = dx.abs();
                let t = if distance <= f32::EPSILON {
                    1.0
                } else {
                    (anim.elapsed_ms(anim_now) / distance).clamp(0.0, 1.0)
                };
                old_x + dx * t
            } else {
                final_x
            }
        } else {
            final_x
        };
        strip_items.push((x, item));
        slot_index += 1;
    }
    let count = slot_index as f32;
    let strips_w = if count > 0.0 {
        count * (strip::STRIP_W + theme::SPACE_MD) * scale - theme::SPACE_MD * scale
    } else {
        0.0
    };
    // The row layout is the settled state and the only one that accounts for
    // open slideouts' extra width. During an add/clear, absolute x positions
    // let the existing slots slide to their new indexes.
    let strips: Element<'_, Message> = if state.assign_anim.is_some() {
        let mut layers: Vec<Element<'_, Message>> =
            vec![iced::widget::Space::new().width(Length::Fill).height(Length::Fill).into()];
        for (x, item) in strip_items {
            layers.push(
                container(item)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(iced::Alignment::Start)
                    .align_y(iced::Alignment::Start)
                    .padding(iced::Padding {
                        left: x,
                        top: 0.0,
                        right: 0.0,
                        bottom: 0.0,
                    })
                    .into(),
            );
        }
        iced::widget::Stack::with_children(layers)
            .width(Length::Fixed(strips_w))
            .height(Length::Fixed(strip::STRIP_H * scale))
            .into()
    } else {
        let mut row = row![].spacing(theme::SPACE_MD * scale);
        for (_, item) in strip_items {
            row = row.push(item);
        }
        row.into()
    };

    // The stack is one channel wide. The body and each group add one
    // SPACE_SM inset on both sides, so the button itself is four insets
    // narrower than the channel.
    let btn_w = (CONTROL_ROOM_W - 4.0 * theme::SPACE_SM) * scale;
    let btn_h = 24.0 * scale;
    let cell = |label: &'static str,
                size: f32,
                active: bool,
                msg: Option<Message>|
     -> Element<'static, Message> {
        button(
            container(text(label).size(size * scale).center())
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::Alignment::Center)
                .align_y(iced::Alignment::Center),
        )
        .width(Length::Fixed(btn_w))
        .height(Length::Fixed(btn_h))
        .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
        .style(theme::phantom_button(active))
        .on_press_maybe(msg)
        .into()
    };
    // The Assign button opens the menu card beside the panel. It uses the
    // same compact selector decoration as a strip's output-bus trigger.
    let assign = iced::widget::button(
        row![
            text("assign").size(theme::TEXT_MD * scale),
            iced::widget::Space::new().width(Length::Fill),
            text("\u{25be}").size(theme::TEXT_MD * scale),
        ]
        .spacing(theme::SPACE_TIGHT * scale)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .width(Length::Fixed(btn_w))
    .height(Length::Fixed(btn_h))
    .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
    .style(theme::toggle_button(
        !matches!(state.assign_menu, AssignMenu::Closed),
        theme::ACCENT,
    ))
    // The menu writes the UC/UCX Main Out bit; a unit without that path
    // shows the button dimmed rather than opening a menu that cannot apply.
    .on_press_maybe(state.device.is_uc_family().then_some(Message::ToggleAssign));

    let group = |cells: Vec<Element<'static, Message>>| -> Element<'static, Message> {
        container(
            column(cells)
                .spacing(theme::SPACE_SM * scale)
                .align_x(iced::Alignment::Start)
                .width(Length::Fill),
        )
        .padding(theme::SPACE_SM * scale)
        .width(Length::Fixed((CONTROL_ROOM_W - 2.0 * theme::SPACE_SM) * scale))
        .style(theme::fx_controls)
        .into()
    };

    // RME order: Dim, Recall, Speaker B; Mono, Talkback, Ext. Input,
    // Mute FX; then a gap and Assign at the bottom.
    let body = column![
        group(vec![
            cell("dim", theme::TEXT_MD, settings.dim, Some(Message::Dim(!settings.dim))),
            cell("recall", theme::TEXT_MD, false, Some(Message::ControlRoomRecall)),
            cell(
                "speaker B",
                theme::TEXT_SM,
                settings.speaker_b,
                // Speaker B is a UC/UCX register; other units show it
                // dimmed instead of failing on press.
                state
                    .device
                    .is_uc_family()
                    .then_some(Message::SpeakerB(!settings.speaker_b)),
            ),
        ]),
        group(vec![
            cell(
                "mono",
                theme::TEXT_MD,
                settings.mono,
                Some(Message::ControlRoomMono(!settings.mono)),
            ),
            cell(
                "talkback",
                theme::TEXT_MD,
                state.talkback_on,
                state
                    .device
                    .is_ucx_model()
                    .then_some(Message::TalkbackChanged(!state.talkback_on)),
            ),
            cell(
                "ext. input",
                theme::TEXT_SM,
                settings.external_input,
                state
                    .control_room
                    .external_input
                    .map(|_| Message::ExternalInput(!settings.external_input)),
            ),
            cell(
                "mute FX",
                theme::TEXT_MD,
                settings.mute_fx,
                state
                    .device
                    .is_uc_family()
                    .then_some(Message::ControlRoomMuteFx(!settings.mute_fx)),
            ),
        ]),
        iced::widget::Space::new().height(Length::Fill),
        group(vec![
            cell(
                "settings",
                theme::TEXT_MD,
                state.control_room_settings_open,
                Some(Message::OpenControlRoomSettings),
            ),
            assign.into(),
        ]),
    ]
    .spacing(theme::SPACE_MD * scale)
    .padding(theme::SPACE_SM * scale)
    .width(Length::Fill)
    .height(Length::Fill);

    let buttons = container(body)
        .width(Length::Fixed(CONTROL_ROOM_W * scale))
        .height(Length::Fixed(strip::STRIP_H * scale));

    container(row![strips, buttons].spacing(theme::SPACE_MD * scale))
        .padding(theme::SPACE_SM * scale)
        .style(theme::panel)
        .into()
}

/// The persistent Control Room values in the form the UCX Talkback
/// generator consumes.
fn talkback_config(state: &TotalControl) -> TalkbackConfig {
    TalkbackConfig {
        source: state.control_room.talkback_input,
        dim_db: state.control_room.talkback_dim_db,
        listenback: state.control_room.listenback_input,
        listenback_dim_db: state.control_room.listenback_dim_db,
    }
}

/// Round a slider value to the Control Room's half-dB grid.
fn round_half_db(db: f32) -> f32 {
    (db.clamp(-65.0, 0.0) * 2.0).round() / 2.0
}

fn mono_input_choices(state: &TotalControl) -> Vec<String> {
    let mut choices = Vec::with_capacity(state.device.inputs().len() + 1);
    choices.push("None".to_string());
    choices.extend(state.device.inputs().iter().map(|input| input.name.clone()));
    choices
}

fn stereo_input_choices(state: &TotalControl) -> Vec<String> {
    let mut choices = vec!["None".to_string()];
    for pair in state.device.inputs().chunks(2) {
        let label = match pair {
            [left, right] => format!("{} / {}", left.name, right.name),
            [only] => only.name.clone(),
            _ => unreachable!(),
        };
        choices.push(label);
    }
    choices
}

fn choice_label(choices: &[String], selected: Option<usize>) -> String {
    selected
        .and_then(|index| choices.get(index + 1).cloned())
        .unwrap_or_else(|| choices[0].clone())
}

fn choice_index(choices: &[String], label: &str) -> Option<usize> {
    choices
        .iter()
        .position(|choice| choice == label)
        .and_then(|index| index.checked_sub(1))
}

fn db_control(value: f32, on_change: fn(f32) -> Message, scale: f32) -> Element<'static, Message> {
    let wheel = move |delta: iced::mouse::ScrollDelta| {
        let dy = match delta {
            iced::mouse::ScrollDelta::Lines { y, .. } => y,
            iced::mouse::ScrollDelta::Pixels { y, .. } => y,
        };
        let step = if dy >= 0.0 { 0.5 } else { -0.5 };
        on_change(round_half_db(value + step))
    };
    let row = row![
        slider(-65.0..=0.0, value, move |v| on_change(round_half_db(v)))
            .step(0.5_f32)
            .style(theme::db_slider)
            .width(Length::Fixed(250.0 * scale))
            .height(24.0 * scale),
        text(format!("{value:+.1} dB"))
            .color(theme::TEXT_PRIMARY)
            .size(16.0 * scale)
            .width(Length::Fixed(84.0 * scale)),
    ]
    .spacing(theme::SPACE_MD * scale)
    .align_y(iced::Alignment::Center);
    // The whole row, including its vertical padding, is the wheel target.
    mouse_area(
        container(row)
            .padding([theme::SPACE_SM * scale, 0.0])
            .width(Length::Fill),
    )
    .on_scroll(wheel)
    .into()
}

/// The Control Room Settings dialog. It edits a draft only; the live
/// settings change when `OK` is pressed, and the snapshot is saved there.
fn control_room_settings_modal(state: &TotalControl) -> Element<'_, Message> {
    let scale = state.ui_scale;
    let draft = &state.control_room_draft;
    let text_size = 16.0 * scale;
    let content_w = 500.0 * scale;

    let section = |label: &'static str| -> Element<'static, Message> {
        container(
            row![
                text(label)
                    .color(theme::TEXT_PRIMARY)
                    .size(text_size),
                iced::widget::rule::horizontal(1),
            ]
            .spacing(theme::SPACE_MD * scale)
            .align_y(iced::Alignment::Center),
        )
        .padding(iced::Padding {
            top: (theme::SPACE_XXL + theme::SPACE_SM) * scale,
            ..iced::Padding::ZERO
        })
        .width(Length::Fill)
        .into()
    };
    let label = |text_value: &'static str| -> Element<'static, Message> {
        text(text_value)
            .color(theme::TEXT_PRIMARY)
            .size(text_size)
            .width(Length::Fixed(150.0 * scale))
            .into()
    };
    let row_with = |label_text: &'static str, control: Element<'static, Message>| {
        row![label(label_text), control]
            .spacing(theme::SPACE_MD * scale)
            .align_y(iced::Alignment::Center)
            .width(Length::Fill)
    };

    let mono_choices = mono_input_choices(state);
    let ext_choices = stereo_input_choices(state);
    let longest = mono_choices
        .iter()
        .chain(ext_choices.iter())
        .map(|choice| choice.chars().count())
        .max()
        .unwrap_or(0);
    let pick_w = (longest as f32 * 9.0 + 40.0) * scale;

    let tb_choices = mono_choices.clone();
    let tb_pick = pick_list(
        tb_choices.clone(),
        Some(choice_label(&mono_choices, draft.talkback_input)),
        move |choice| Message::ControlRoomTalkbackInput(choice_index(&tb_choices, &choice)),
    )
    .style(theme::pick_list)
    .text_size(text_size)
    .width(Length::Fixed(pick_w));

    let lb_choices = mono_choices.clone();
    let lb_pick = pick_list(
        lb_choices.clone(),
        Some(choice_label(&mono_choices, draft.listenback_input)),
        move |choice| Message::ControlRoomListenbackInput(choice_index(&lb_choices, &choice)),
    )
    .style(theme::pick_list)
    .text_size(text_size)
    .width(Length::Fixed(pick_w));

    let ext_pick_choices = ext_choices.clone();
    let ext_pick = pick_list(
        ext_pick_choices.clone(),
        Some(choice_label(&ext_choices, draft.external_input)),
        move |choice| Message::ControlRoomExternalInput(choice_index(&ext_pick_choices, &choice)),
    )
    .style(theme::pick_list)
    .text_size(text_size)
    .width(Length::Fixed(pick_w));

    let body = column![
        text("CONTROL ROOM SETTINGS")
            .color(theme::TEXT_PRIMARY)
            .size(text_size),
        section("TALKBACK"),
        row_with("Input", tb_pick.into()),
        row_with(
            "Dim",
            db_control(draft.talkback_dim_db, Message::ControlRoomTalkbackDim, scale),
        ),
        section("LISTENBACK"),
        row_with("Input", lb_pick.into()),
        row_with(
            "Dim",
            db_control(
                draft.listenback_dim_db,
                Message::ControlRoomListenbackDim,
                scale,
            ),
        ),
        section("MAIN OUT"),
        row_with(
            "Recall",
            db_control(draft.recall_db, Message::ControlRoomRecallDb, scale),
        ),
        row_with(
            "Dim",
            db_control(draft.main_dim_db, Message::ControlRoomMainDim, scale),
        ),
        row_with("External Input", ext_pick.into()),
        row_with(
            "Gain",
            db_control(
                draft.external_gain_db,
                Message::ControlRoomExternalGain,
                scale,
            ),
        ),
        row_with(
            "Speaker B",
            checkbox(draft.speaker_b_linked)
                .label("Link to Main")
                .text_size(text_size)
                .on_toggle(Message::ControlRoomSpeakerBLinked)
                .into(),
        ),
        section("SOLO / PFL MODE"),
        checkbox(draft.pfl_replaces_solo)
            .label("Live Mode, PFL replaces Solo")
            .text_size(text_size)
            .width(Length::Fill)
            .on_toggle(Message::ControlRoomPflReplacesSolo),
        checkbox(draft.exclusive_solo_pfl)
            .label("Exclusive Solo/PFL Mode")
            .text_size(text_size)
            .width(Length::Fill)
            .on_toggle(Message::ControlRoomExclusiveSoloPfl),
    ]
    .spacing(theme::SPACE_MD * scale)
    .width(Length::Fill);

    let footer = row![
        hint(
            button(text("default").size(text_size))
                .padding([theme::SPACE_SM * scale, theme::SPACE_LG * scale])
                .style(theme::ghost_button)
                .on_press(Message::ResetControlRoomSettings),
            "restore the default values",
            scale,
        ),
        iced::widget::Space::new().width(Length::Fill),
        button(text("Cancel").size(text_size))
            .padding([theme::SPACE_SM * scale, theme::SPACE_LG * scale])
            .style(theme::ghost_button)
            .on_press(Message::CancelControlRoomSettings),
        button(text("OK").size(text_size))
            .padding([theme::SPACE_SM * scale, theme::SPACE_LG * scale])
            .style(theme::toggle_button(true, theme::ACCENT))
            .on_press(Message::ApplyControlRoomSettings),
    ]
    .spacing(theme::SPACE_MD * scale)
    .align_y(iced::Alignment::Center)
    .width(Length::Fill);

    let card = opaque(
        container(
            column![body, footer]
                .spacing(theme::SPACE_LG * scale)
                .width(Length::Fill),
        )
        .style(theme::dialog)
        .padding(theme::SPACE_XL * scale)
        .width(Length::Fixed(content_w + 2.0 * theme::SPACE_XL * scale)),
    );

    opaque(
        mouse_area(
            container(card)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(Background::Color(Color {
                        a: 0.55,
                        ..Color::BLACK
                    })),
                    ..container::Style::default()
                }),
        )
        .on_press(Message::CancelControlRoomSettings),
    )
}

fn mixer_view(state: &TotalControl, fx_width: Option<f32>) -> (Element<'_, Message>, bool) {
    // Snapshot once for all three inline-panel width tweens.
    let anim_now = Instant::now();
    // Grouped by pair, TotalMix-style: a linked pair (the default) shows
    // as ONE strip driving both channels; a split pair shows its two
    // channels independently — see `RmeDevice::input_pair_linked`. Pairs
    // never straddle a channel-type boundary (the device profile always
    // groups same-type channels together), so iterating by pair instead
    // of by individual channel doesn't change where the type dividers
    // land.
    // No channel-type dividers — used to insert a 1px `rule::vertical`
    // between channel-type groups (Mic/Instrument/Line/ADAT), which read
    // as a wider gap there (`SPACE_MD` + the rule + `SPACE_MD`, ~13px)
    // than between two strips of the same type (`SPACE_MD` alone, ~6px).
    // The user caught the inconsistency directly, comparing against
    // Software Playback/Hardware Outputs (see their own comment below —
    // neither ever had dividers, so they were already uniform): removed
    // so every gap on every row is the same `SPACE_MD`, everywhere.
    let mut input_strips = row![].spacing(theme::SPACE_MD);
    let mut input_width = 0.0f32;
    let mut input_item_count = 0usize;
    let mut input_open_x: Option<(ChannelId, f32)> = None;
    let n_input_pairs = state.device.inputs().len() / 2;
    for pair in 0..n_input_pairs {
        let l = pair * 2;
        let linked = state.device.input_pair_linked(pair);
        let full_right = rendered_strip_width(state, ChannelId::Input(l + 1));
        let extra = stereo_extra_width(
            state,
            ChannelId::Input(l),
            linked,
            full_right,
            anim_now,
        );
        let mut channels = vec![l];
        if extra > 0.0 {
            channels.push(l + 1);
        }
        for &ch_idx in &channels {
            let cid = ChannelId::Input(ch_idx);
            let right_anim = ch_idx == l + 1 && extra < full_right - 0.5;
            let mut params = strip_params(state, cid, state.sel_out);
            if linked && ch_idx == l {
                // Combined "AN1/2"-style label for the linked bus. EQ
                // (idx < 4 only) still only reaches this representative
                // (left) channel while shown combined — split the pair
                // to reach the right channel's own EQ.
                if let Some(right) = state.device.inputs().get(l + 1) {
                    params.name = pair_bus_label(&params.name, &right.name);
                }
                // Real TotalMix splits a linked pair's meter into two
                // thin bars (this channel + its sibling), not one bar
                // showing only the left/even channel — previously the
                // right channel's own real level was captured but never
                // displayed at all while linked (found 2026-09-07).
                params.meter2 = state.input_meters.get(l + 1).map(MeterAnim::frame);
                params.meter2_available = state.device.has_input_meter(l + 1);
            }
            let strip_widget: Element<'_, Message> = if right_anim {
                container(strip::strip(finish_params(state, params)))
                    .width(Length::Fixed(extra.max(0.0)))
                    .clip(true)
                    .into()
            } else {
                strip::strip(finish_params(state, params))
            };
            let mut item_width = if right_anim {
                extra.max(0.0)
            } else {
                rendered_strip_width(state, cid)
            };
            // Settings/EQ push the row instead of overlaying it (see
            // `with_flyout`'s doc comment for why) — done here, by
            // widening this loop iteration's own item, rather than in
            // `with_flyout`, since ordinary `row!` layout already pushes
            // every later sibling over for free once this one item is
            // wider.
            if right_anim {
                input_strips = input_strips.push(strip_widget);
            } else if let Some(panel_w) = flyout_slot(
                state,
                cid,
                strip::FlyoutKind::Settings,
                item_width,
                anim_now,
            ) {
                // Settings matches the strip's own width.  The width is
                // animated, so the panel slides out and every later strip
                // is pushed right a frame at a time.
                input_strips = input_strips.push(
                    row![strip_widget, settings_popover(state, cid, panel_w, state.ui_scale)]
                        .spacing(0.0),
                );
                item_width += panel_w;
            } else if let Some(panel_w) = flyout_slot(
                state,
                cid,
                strip::FlyoutKind::Eq,
                strip::EQ_FLYOUT_W * state.ui_scale,
                anim_now,
            ) {
                input_strips = input_strips.push(
                    row![strip_widget, eq_popover(state, cid, panel_w, state.ui_scale)]
                        .spacing(0.0),
                );
                item_width += panel_w;
            } else if let Some(panel_w) = flyout_slot(
                state,
                cid,
                strip::FlyoutKind::Dynamics,
                strip::DYN_FLYOUT_W * state.ui_scale,
                anim_now,
            ) {
                input_strips = input_strips.push(
                    row![strip_widget, dynamics_popover(state, cid, panel_w, state.ui_scale)]
                        .spacing(0.0),
                );
                item_width += panel_w;
            } else if let Some(panel_w) = flyout_slot(
                state,
                cid,
                strip::FlyoutKind::Trim,
                strip::FLYOUT_W,
                anim_now,
            ) {
                input_strips = input_strips.push(
                    row![strip_widget, trim_popover(state, cid, panel_w, state.ui_scale)]
                        .spacing(0.0),
                );
                item_width += panel_w;
            } else {
                input_strips = input_strips.push(strip_widget);
            }
            input_width += item_width;
            input_item_count += 1;
            if state.flyout_open.contains(&(cid, strip::FlyoutKind::Route))
                || route_visible(state, cid, anim_now)
            {
                // Gaps placed so far (`item_count - 1`, spacing is between
                // items) plus the content accumulated up to and including this
                // strip is exactly its right edge on screen.
                input_open_x = Some((
                    cid,
                    input_width + (input_item_count - 1) as f32 * theme::SPACE_MD,
                ));
            }
        }
    }
    input_width += input_item_count.saturating_sub(1) as f32 * theme::SPACE_MD;

    // Same pair-grouping idea, no channel-type dividers to worry about.
    let mut pb_strips = row![].spacing(theme::SPACE_MD);
    let mut pb_width = 0.0f32;
    let mut pb_item_count = 0usize;
    let mut pb_open_x: Option<(ChannelId, f32)> = None;
    let n_pb_pairs = state.device.playbacks().len() / 2;
    for pair in 0..n_pb_pairs {
        let l = pair * 2;
        let linked = state.device.playback_linked(pair);
        let full_right = rendered_strip_width(state, ChannelId::Playback(l + 1));
        let extra = stereo_extra_width(
            state,
            ChannelId::Playback(l),
            linked,
            full_right,
            anim_now,
        );
        let mut channels = vec![l];
        if extra > 0.0 {
            channels.push(l + 1);
        }
        for &ch_idx in &channels {
            let cid = ChannelId::Playback(ch_idx);
            let right_anim = ch_idx == l + 1 && extra < full_right - 0.5;
            let mut params = strip_params(state, cid, state.sel_out);
            if linked && ch_idx == l {
                if let Some(right) = state.device.playbacks().get(l + 1) {
                    params.name = pair_bus_label(&params.name, &right.name);
                }
                params.meter2 = state.playback_meters.get(l + 1).map(MeterAnim::frame);
                params.meter2_available = playback_meter_available(state, l + 1);
            }
            let strip_widget: Element<'_, Message> = if right_anim {
                container(strip::strip(finish_params(state, params)))
                    .width(Length::Fixed(extra.max(0.0)))
                    .clip(true)
                    .into()
            } else {
                strip::strip(finish_params(state, params))
            };
            let mut item_width = if right_anim {
                extra.max(0.0)
            } else {
                rendered_strip_width(state, cid)
            };
            if right_anim {
                pb_strips = pb_strips.push(strip_widget);
            } else if let Some(panel_w) = flyout_slot(
                state,
                cid,
                strip::FlyoutKind::Settings,
                item_width,
                anim_now,
            ) {
                pb_strips = pb_strips.push(
                    row![strip_widget, settings_popover(state, cid, panel_w, state.ui_scale)]
                        .spacing(0.0),
                );
                item_width += panel_w;
            } else if let Some(panel_w) = flyout_slot(
                state,
                cid,
                strip::FlyoutKind::Trim,
                strip::FLYOUT_W,
                anim_now,
            ) {
                pb_strips = pb_strips.push(
                    row![strip_widget, trim_popover(state, cid, panel_w, state.ui_scale)]
                        .spacing(0.0),
                );
                item_width += panel_w;
            } else {
                pb_strips = pb_strips.push(strip_widget);
            }
            pb_width += item_width;
            pb_item_count += 1;
            if state.flyout_open.contains(&(cid, strip::FlyoutKind::Route))
                || route_visible(state, cid, anim_now)
            {
                pb_open_x = Some((
                    cid,
                    pb_width + (pb_item_count - 1) as f32 * theme::SPACE_MD,
                ));
            }
        }
    }
    pb_width += pb_item_count.saturating_sub(1) as f32 * theme::SPACE_MD;

    // Grouped by pair, TotalMix-style: a linked pair (the default) shows
    // as ONE bus strip driving both channels; a split pair shows its two
    // channels as independent strips — see `RmeDevice::output_linked`.
    // Outputs have no Route flyout (a single master, not a per-submix
    // crosspoint), only Settings (the STEREO toggle) — pushed the same
    // way Input/Playback do, no `with_flyout`/open_x tracking needed.
    let mut out_strips = row![].spacing(theme::SPACE_MD);
    let mut out_width = 0.0f32;
    let mut out_item_count = 0usize;
    for pair in state.device.output_display_order() {
        // Add/clear: the moving pair leaves a gap in this row that shrinks or
        // grows at the existing 1 px/ms speed. The moving strip itself is in
        // the overlay above the row and panel.
        if let Some(anim) = state.assign_anim.as_ref() {
            if let Some((_, moving_pair)) = anim.moving() {
                if moving_pair == pair {
                    let gap = assign_gap_width(state, anim, pair, state.ui_scale, anim_now);
                    let old_count = anim.old.iter().flatten().count();
                    let new_count = anim.new.iter().flatten().count();
                    if new_count < old_count {
                        // The returning pair fills the opening gap
                        // gradually, clipped from the right, instead of
                        // appearing only when the gap is complete.
                        out_strips = out_strips.push(output_row_clipped_pair(
                            state,
                            pair,
                            gap,
                            state.ui_scale,
                        ));
                        out_width += rendered_pair_width(state, pair, state.ui_scale);
                    } else {
                        // A leaving pair is extra content that overflows
                        // under the Control Room as the gap closes.
                        out_strips = out_strips.push(
                            iced::widget::Space::new()
                                .width(Length::Fixed(gap))
                                .height(Length::Fixed(strip::STRIP_H * state.ui_scale)),
                        );
                    }
                    out_item_count += 1;
                    continue;
                }
            }
        }
        // Assigned monitoring outputs are rendered inside the Control Room.
        if state.is_output_assigned(pair) {
            continue;
        }
        let l = pair * 2;
        let linked = state.device.output_linked(pair);
        let full_right = rendered_strip_width(state, ChannelId::Output(l + 1));
        let extra = stereo_extra_width(
            state,
            ChannelId::Output(l),
            linked,
            full_right,
            anim_now,
        );
        let mut channels = vec![l];
        if extra > 0.0 {
            channels.push(l + 1);
        }
        for &ch_idx in &channels {
            let cid = ChannelId::Output(ch_idx);
            let right_anim = ch_idx == l + 1 && extra < full_right - 0.5;
            let mut params = strip_params(state, cid, state.sel_out);
            if linked && ch_idx == l {
                if let Some(right) = state.device.outputs().get(l + 1) {
                    params.name = pair_bus_label(&params.name, &right.name);
                }
                params.meter2 = state.output_meters.get(l + 1).map(MeterAnim::frame);
                params.meter2_available = output_meter_available(state, l + 1);
            }
            let strip_widget: Element<'_, Message> = if right_anim {
                container(strip::strip(finish_params(state, params)))
                    .width(Length::Fixed(extra.max(0.0)))
                    .clip(true)
                    .into()
            } else {
                strip::strip(finish_params(state, params))
            };
            let mut item_width = if right_anim {
                extra.max(0.0)
            } else {
                rendered_strip_width(state, cid)
            };
            if right_anim {
                out_strips = out_strips.push(strip_widget);
            } else if let Some(panel_w) = flyout_slot(
                state,
                cid,
                strip::FlyoutKind::Settings,
                item_width,
                anim_now,
            ) {
                out_strips = out_strips.push(
                    row![strip_widget, settings_popover(state, cid, panel_w, state.ui_scale)]
                        .spacing(0.0),
                );
                item_width += panel_w;
            } else if let Some(panel_w) = flyout_slot(
                state,
                cid,
                strip::FlyoutKind::Eq,
                strip::EQ_FLYOUT_W * state.ui_scale,
                anim_now,
            ) {
                out_strips = out_strips.push(
                    row![strip_widget, eq_popover(state, cid, panel_w, state.ui_scale)]
                        .spacing(0.0),
                );
                item_width += panel_w;
            } else if let Some(panel_w) = flyout_slot(
                state,
                cid,
                strip::FlyoutKind::Dynamics,
                strip::DYN_FLYOUT_W * state.ui_scale,
                anim_now,
            ) {
                out_strips = out_strips.push(
                    row![strip_widget, dynamics_popover(state, cid, panel_w, state.ui_scale)]
                        .spacing(0.0),
                );
                item_width += panel_w;
            } else {
                out_strips = out_strips.push(strip_widget);
            }
            out_width += item_width;
            out_item_count += 1;
        }
    }
    out_width += out_item_count.saturating_sub(1) as f32 * theme::SPACE_MD;

    // The Control Room is flush with the right edge of the mixer area, the
    // way TotalMix keeps it against the Control Strip, rather than packed
    // after the last output strip. When the strips are wider than the
    // window it stays at the end of the row and the page scrolls.
    let scale = state.ui_scale;
    // Width of the channel strips alone, so the HARDWARE OUTPUTS rule stops
    // at the last channel instead of running into the Control Room.
    let mut strips_width = 0.0f32;
    let cr_w = control_room_visual_width(state, scale, anim_now);
    let natural_out = if state.device.capabilities().protocol_features {
        out_width + theme::SPACE_MD + cr_w
    } else {
        out_width
    };
    let natural_width = input_width.max(pb_width).max(natural_out);
    let side = sidebar_px(state);
    let main_space = state.window_width - side - 2.0 * theme::SPACE_XL * scale;
    let fx_panel = fx_width.unwrap_or(0.0);
    let fx_open = fx_width.is_some() && state.device.capabilities().fx && fx_panel > 0.5;
    let fx_extent = fx_panel + theme::SPACE_MD * scale;
    // The reserve shrinks continuously once the mixer reaches its natural
    // width. The Control Room therefore keeps its position while the FX
    // column starts covering it, instead of jumping to the right when the
    // layout switches from a sibling column to an overlay.
    let reserve = if fx_open {
        fx_reserve(main_space, natural_width, fx_extent)
    } else {
        0.0
    };
    let available = main_space - reserve;
    let fx_overlay = fx_open && reserve < fx_extent;
    let out_row: Element<'_, Message> = if state.device.capabilities().protocol_features {
        strips_width = out_width;
        out_width = natural_out.max(available);
        // The strips are the row; the Control Room rides above them, pinned
        // to the right edge, so a slideout widening the panel eats into the
        // channel row instead of pushing it.
        let row = iced::widget::Stack::new()
            .push(
                row![out_strips]
                    .width(Length::Fixed(out_width))
                    .height(Length::Fixed(strip::STRIP_H * scale)),
            )
            .push(
                container(control_room_panel(state, scale, anim_now))
                    .width(Length::Fill)
                    .height(Length::Fixed(strip::STRIP_H * scale))
                    .align_x(iced::Alignment::End)
                    .align_y(iced::Alignment::Start),
            );
        let row = row.width(Length::Fixed(out_width));
        // The Assign menu opens inside this row, so it stays anchored to
        // the Control Room panel whatever the window size or scroll is.
        if matches!(state.assign_menu, AssignMenu::Closed) {
            row.into()
        } else {
            iced::widget::Stack::new()
                .push(row)
                .push(assign_menu_layer(state))
                .into()
        }
    } else {
        out_strips.into()
    };
    // TotalMix's "2 Row" (Options panel): inputs and playbacks side by
    // side, outputs underneath. Always honored once chosen: when the two
    // blocks are wider than the window, the top row scrolls sideways like
    // any other over-wide strip row. (It used to fall back to the stacked
    // layout instead, so on a narrower window the switch lit up and
    // nothing changed.)
    let side_gap = theme::SPACE_XXL * scale;
    let two_rows = match state.row_mode {
        RowMode::Two => true,
        RowMode::Three => false,
        // Retired Auto prefs open in the manual's default 3-row layout.
        RowMode::Auto => false,
    };
    let sources: Element<'_, Message> = if two_rows {
        let side_by_side = row![
            column![
                container(section_header("HARDWARE INPUTS", scale))
                    .width(Length::Fixed(input_width)),
                with_flyout(state, input_strips.into(), input_open_x),
            ]
            .spacing(theme::SPACE_LG)
            .width(Length::Fixed(input_width)),
            column![
                container(section_header("SOFTWARE PLAYBACK", scale))
                    .width(Length::Fixed(pb_width)),
                with_flyout(state, pb_strips.into(), pb_open_x),
            ]
            .spacing(theme::SPACE_LG)
            .width(Length::Fixed(pb_width)),
        ]
        .spacing(side_gap);
        side_by_side.into()
    } else {
        // Every section title spans its own strips, the way the Control
        // Room's does, so a break appears wherever the row has slack.
        column![
            container(section_header("HARDWARE INPUTS", scale))
                .width(Length::Fixed(input_width)),
            with_flyout(state, input_strips.into(), input_open_x),
            container(section_header("SOFTWARE PLAYBACK", scale))
                .width(Length::Fixed(pb_width)),
            with_flyout(state, pb_strips.into(), pb_open_x),
        ]
        .spacing(theme::SPACE_LG)
        .into()
    };
    // Two section titles share this line: HARDWARE OUTPUTS over the channel
    // strips, `control room` starting at the section's leftmost channel.
    let outputs_header: Element<'_, Message> = if state.device.capabilities().protocol_features {
        let cr_w = control_room_visual_width(state, scale, anim_now);
        // The title starts exactly at the section's left edge — the panel's
        // own left edge — so the two line up at any window width. The slack
        // between the strips and the section is the break in the line.
        let break_w = (out_width - cr_w - strips_width).max(0.0);
        row![
            container(section_header("HARDWARE OUTPUTS", scale))
                .width(Length::Fixed(strips_width)),
            iced::widget::Space::new().width(Length::Fixed(break_w)),
            container(section_header("CONTROL ROOM", scale))
                .width(Length::Fixed(cr_w)),
        ]
        .spacing(0.0)
        .width(Length::Fixed(out_width))
        .into()
    } else {
        section_header("HARDWARE OUTPUTS", scale)
    };
    let body = column![sources, outputs_header, out_row]
    .spacing(theme::SPACE_LG)
    // Give the page's bidirectional scrollable the real row width, so a
    // row wider than the window scrolls instead of clipping its right-hand
    // strips or making the last visible strip look narrower.
    .width(Length::Fixed(input_width.max(pb_width).max(out_width)));

    (page(body), fx_overlay)
}

fn matrix_view(state: &TotalControl) -> Element<'_, Message> {
    let scale = state.ui_scale;
    // The caption reads out the cell under the pointer, and is the how-to
    // the rest of the time (the gestures are not discoverable otherwise).
    let caption = match matrix::hover_readout(state) {
        Some(readout) => text(readout).color(theme::TEXT_PRIMARY),
        None => text(
            "Drag a cell up or down to set its gain (Shift: fine) \u{00b7} \
             Ctrl+click: 0 dB / off \u{00b7} double-click: reset \u{00b7} Tab for Quick",
        )
        .color(theme::TEXT_SEC),
    }
    .size(theme::TEXT_XS * scale);
    let body = column![
        section_header("MATRIX MIXER", scale),
        row![
            caption,
            iced::widget::Space::new().width(Length::Fill),
            iced::widget::button(text("MONO MODE").size(theme::TEXT_XS * scale))
                .padding([theme::SPACE_TIGHT * scale, theme::SPACE_MD * scale])
                .style(theme::toggle_button(state.matrix_mono, theme::ACCENT))
                .on_press(Message::ToggleMatrixMono),
        ]
        .align_y(iced::Alignment::Center)
        .spacing(theme::SPACE_MD),
        matrix::view(state),
    ]
    .spacing(theme::SPACE_LG)
    .width(Length::Fill)
    .height(Length::Fill);

    // Not `page()`: the grid scrolls inside itself, under a fixed header
    // (see `matrix::view`), which a page-wide scrollable would defeat.
    mouse_area(
        container(body)
            .style(theme::root)
            .padding([theme::SPACE_LG, theme::SPACE_XL])
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .on_press(Message::ClearSelection)
    .into()
}

/// Every channel selectable as a Quick Control source, in display order —
/// inputs then playbacks, matching `channel_order`. Outputs are excluded:
/// the Quick view's destination block is always `Output(state.sel_out)`,
/// picked via the existing Submix selector in the top bar rather than a
/// second picker here.
fn quick_channel_options(state: &TotalControl) -> Vec<(ChannelId, String)> {
    let mut opts = Vec::new();
    for (i, ch) in state.device.inputs().iter().enumerate() {
        let cid = ChannelId::Input(i);
        let name = user_label(state, cid).unwrap_or(short_label(&ch.name));
        opts.push((cid, format!("{name} · IN")));
    }
    for (i, ch) in state.device.playbacks().iter().enumerate() {
        let cid = ChannelId::Playback(i);
        let name = user_label(state, cid).unwrap_or(short_label(&ch.name));
        opts.push((cid, format!("{name} · PB")));
    }
    opts
}

/// How much bigger than a normal strip the two Quick Control blocks are:
/// the point of this view is that the one source and one destination that
/// matter are large, easy targets, not a dense grid. A plain multiple of
/// `scale`, like every other dimension, so Ctrl+wheel zoom grows the strips
/// too, and the view scrolls once they no longer fit. Sizing them to the
/// window instead would cancel the zoom out: the fit would hand back exactly
/// the room the zoom had just taken away.
///
/// 1.8 rather than 2 so the whole group still fits, centered, in the default
/// 1280x800 window at 100%. That is now the floor: a taller window grows
/// the strips up to `QUICK_SCALE_MAX` (see `quick_fit`), where at 1080p a
/// fixed 1.8 left the bottom half of the view empty.
const QUICK_SCALE_MULT: f32 = 1.8;
/// The largest the Quick strips grow on a tall window.
const QUICK_SCALE_MAX: f32 = 3.0;

/// The strip multiple that fills `height` (the view's height at zoom 1),
/// between `QUICK_SCALE_MULT` and `QUICK_SCALE_MAX`. Measured at zoom 1
/// and then multiplied by the zoom, so the zoom keeps growing the strips
/// (and the view scrolls once they no longer fit) instead of being
/// cancelled by a fit that hands back the room it took.
fn quick_fit(height: f32) -> f32 {
    ((height - QUICK_CHROME) / QUICK_STRIP_UNIT).clamp(QUICK_SCALE_MULT, QUICK_SCALE_MAX)
}

/// The Quick content's height at `scale` 1, split between the part that does
/// not grow with the strips (the header, the picker, the captions) and the
/// part the two strips take per unit of strip scale. Only used to choose
/// between the centered form and the scrolling one, never to size anything,
/// and deliberately a little high: erring high means scrolling a touch
/// early, while erring low would pick the centered form for a group that
/// does not fit and clip it off at both ends.
const QUICK_CHROME: f32 = 110.0;
const QUICK_STRIP_UNIT: f32 = 330.0;

/// A Quick block: the picker over the strip, with the panel the strip has
/// open (Settings, EQ, Trim or the route list) to the strip's right. The
/// panel is drawn at the strips' own scale and fills the strip's height, so
/// it stays proportional to them; the picker keeps the strip's width.
fn quick_block<'a>(
    state: &'a TotalControl,
    cid: ChannelId,
    heading: Element<'a, Message>,
    picker: Element<'a, Message>,
    strip_widget: Element<'a, Message>,
    strip_w: f32,
    big_scale: f32,
) -> Element<'a, Message> {
    use strip::FlyoutKind::{Dynamics, Eq, Route, Settings, Trim};
    let now = Instant::now();
    let mut panel: Option<(Element<'a, Message>, f32)> = None;
    let candidates = [
        (Settings, strip_w),
        (Eq, strip::EQ_FLYOUT_W * big_scale),
        (Dynamics, strip::DYN_FLYOUT_W * big_scale),
        (Trim, strip::FLYOUT_W * big_scale),
        (Route, strip::FLYOUT_W * big_scale),
    ];
    for (kind, full_view) in candidates {
        let full_state = flyout_width(state, cid, kind);
        let Some(w) = flyout_slot(state, cid, kind, full_state, now) else {
            continue;
        };
        let ratio = if full_state > 0.0 {
            (w / full_state).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let panel_w = full_view * ratio;
        let content = match kind {
            Settings => settings_popover(state, cid, panel_w, big_scale),
            Eq => eq_popover(state, cid, panel_w, big_scale),
            Dynamics => dynamics_popover(state, cid, panel_w, big_scale),
            Trim => trim_popover(state, cid, panel_w, big_scale),
            Route => route_popover(state, panel_w, big_scale),
        };
        panel = Some((content, panel_w));
        break;
    }
    let gap = theme::SPACE_MD * state.ui_scale;
    let head = container(column![heading, picker].spacing(gap).align_x(iced::Alignment::Center))
        .width(Length::Fixed(strip_w))
        .center_x(Length::Fixed(strip_w));
    let body: Element<'a, Message> = match panel {
        // A stack, not a row: the strip alone sets the height, and the
        // panel's layer fills it (a `Fill` panel in a row would take all
        // the room the page offers).
        Some((panel, panel_w)) => iced::widget::stack![
            row![strip_widget, iced::widget::Space::new().width(panel_w + gap)],
            row![iced::widget::Space::new().width(strip_w + gap), panel]
                .height(Length::Fill),
        ]
        .into(),
        None => strip_widget,
    };
    column![head, body].spacing(gap).into()
}

fn quick_view(state: &TotalControl) -> Element<'_, Message> {
    let scale = state.ui_scale;
    let options = quick_channel_options(state);
    let labels: Vec<String> = options.iter().map(|(_, l)| l.clone()).collect();
    let cids: Vec<ChannelId> = options.iter().map(|(c, _)| *c).collect();
    let current = options
        .iter()
        .find(|(cid, _)| *cid == state.quick_channel)
        .map(|(_, l)| l.clone());

    let quick_channel = state.quick_channel;
    // Never derive the destination from a submix that belongs to the
    // previous device; a stale selector may reach the view for one frame
    // during a switch.
    let sel_out = state
        .sel_out
        .min(state.device.output_pair_count().saturating_sub(1));
    let dest_cid = ChannelId::Output(sel_out * 2);

    // Rebuilt on every layout pass: the content sits behind a `responsive`
    // (the centered form needs the available size, which a `Length::Fill`
    // cannot provide inside the page's scrollable), so it only captures
    // owned or copyable data and clones what it needs per call.
    // The destination picker lists the submix buses by the same names the
    // rest of the UI uses for them (a user label on an output strip
    // included), and selecting one is exactly the top bar's Submix field.
    let out_labels = state.pair_labels();
    let display_order = state.device.output_display_order();
    let dest_labels: Vec<String> = display_order
        .iter()
        .map(|&i| {
            user_label(state, ChannelId::Output(i * 2))
                .map(str::to_string)
                .unwrap_or_else(|| out_labels.get(i).cloned().unwrap_or_default())
        })
        .collect();
    let selected_display = state.device.display_pair_index(sel_out);

    let build_content = move |big_scale: f32| -> Element<'_, Message> {
        let labels_for_pick = labels.clone();
        let cids = cids.clone();
        let display_order_for_pick = display_order.clone();
        let strip_w = strip::STRIP_W * big_scale;
        fn styled<'p>(
            p: iced::widget::PickList<'p, String, Vec<String>, String, Message>,
            scale: f32,
            width: f32,
        ) -> iced::widget::PickList<'p, String, Vec<String>, String, Message> {
            p.style(theme::pick_list)
                .menu_style(theme::menu)
                .padding([theme::SPACE_SM * scale, theme::SPACE_MD * scale])
                .text_size(theme::TEXT_MD * scale)
                .width(Length::Fixed(width))
        }
        let picker_style = |p| styled(p, scale, strip_w);
        let source_picker = picker_style(
            pick_list(labels.clone(), current.clone(), move |label| {
                let idx = labels_for_pick
                    .iter()
                    .position(|l| *l == label)
                    .unwrap_or(0);
                Message::QuickChannelSelected(cids[idx])
            })
            .placeholder("select..."),
        );
        let dest_for_pick = dest_labels.clone();
        let dest_picker = picker_style(pick_list(
            dest_labels.clone(),
            dest_labels.get(selected_display).cloned(),
            move |label| {
                let display = dest_for_pick
                    .iter()
                    .position(|l| *l == label)
                    .unwrap_or(0);
                Message::SelectOutput(
                    display_order_for_pick
                        .get(display)
                        .copied()
                        .unwrap_or(display),
                )
            },
        ));

        // `sel_out` indexes the 6 *submix pairs* (`OUT_LABELS`), but
        // `device.outputs()` is a flat list of individual physical
        // channels, two per pair (`build_outputs`: index `2*i`/`2*i+1` =
        // left/right of pair `i`) — so the pair's representative channel
        // is `sel_out * 2`, not `sel_out` itself.
        let mut source_params = strip_params(state, quick_channel, sel_out);
        source_params.scale = big_scale;
        let mut dest_params = strip_params(state, dest_cid, sel_out);
        dest_params.scale = big_scale;
        // Named and metered as the Mixer's output strip is: a linked pair
        // is one "AN1/2" bus with both channels' meters, not its left half.
        if state.device.output_linked(sel_out) {
            let l = sel_out * 2;
            if let Some(right) = state.device.outputs().get(l + 1) {
                dest_params.name = pair_bus_label(&dest_params.name, &right.name);
            }
            dest_params.meter2 = state.output_meters.get(l + 1).map(MeterAnim::frame);
            dest_params.meter2_available = output_meter_available(state, l + 1);
        }

        // Each block is caption, picker, strip: the picker sits on the
        // strip it chooses, so the two columns read as one sentence (this
        // source, into this destination) and the destination no longer
        // has to be changed from the top bar.
        let caption = |label: &'static str| {
            text(label)
                .color(theme::TEXT_SEC)
                .size(theme::TEXT_XS * scale)
        };
        let source_block = quick_block(
            state,
            quick_channel,
            caption("SOURCE").into(),
            source_picker.into(),
            strip::strip(finish_params(state, source_params)),
            strip_w,
            big_scale,
        );
        let dest_block = quick_block(
            state,
            dest_cid,
            caption("DESTINATION").into(),
            dest_picker.into(),
            strip::strip(finish_params(state, dest_params)),
            strip_w,
            big_scale,
        );

        let connector = text("\u{2192}")
            .color(theme::TEXT_SEC)
            .size(theme::TEXT_XL * scale);

        let blocks = row![source_block, connector, dest_block]
            .spacing(theme::SPACE_XL * big_scale)
            .align_y(iced::Alignment::Center);

        column![
            section_header("QUICK CONTROL", scale),
            text("Choose a source and a destination, then set how loud the source is there. Tab switches to the Mixer.")
                .color(theme::TEXT_SEC)
                .size(theme::TEXT_XS * scale),
            blocks,
        ]
        .spacing(theme::SPACE_LG)
        .align_x(iced::Alignment::Center)
        .width(Length::Fill)
        .into()
    };

    // One centered group when it fits, as the view is meant to be read as a
    // single sentence instead of a page of controls; the scrolling page the
    // other views use is the fallback for small windows and a zoomed-in UI.
    // Centered *horizontally* only: centering it vertically too left a dead
    // band between the top bar and the QUICK CONTROL header that read as a
    // layout bug (asked to be removed, 2026-10-16). The height is per unit
    // of `scale`, so it grows with the zoom the same way the content it
    // measures does.
    let content = responsive(move |size| {
        let fit = quick_fit(size.height);
        let needed = (QUICK_CHROME + QUICK_STRIP_UNIT * fit) * scale;
        if size.height >= needed {
            container(build_content(scale * fit))
                .center_x(Length::Fill)
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            scrollable(build_content(scale * fit))
                .direction(scrollable::Direction::Vertical(theme::thin_scrollbar()))
                .width(Length::Fill)
                .style(theme::scrollable)
                .into()
        }
    });

    mouse_area(
        container(content)
            .style(theme::root)
            .padding([theme::SPACE_LG, theme::SPACE_XL])
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .on_press(Message::ClearSelection)
    .into()
}

#[cfg(test)]
mod tests {
    use super::{
        any_channel_muted, any_channel_soloed, apply_pending_resize,
        channel_trim, trim_factor, trim_meter_levels,
        apply_prefs, channel_is_muted, channel_is_soloed, choose_startup_kind,
        global_feedback, global_feedback_is_typed, global_messages, global_state_commands,
        input_gain_knob_label, TotalControl,
        choose_startup_target,
        frames_ms, converter_frames, is_undoable, new, new_with_device,
        apply_startup_sync, apply_control_room_assignments, attach_device_service, prefs_of, mute_armed, power_sum_output_meters,
        reorder_pair_volumes, fx_reserve, fx_slot,
        uc_min_buffer_frames, uc_usb_frames, ucx_min_buffer_frames,
        ucx_urb_frames, ucx_usb_frames, RtlEstimate, update, zoom, ChannelId, DeviceHandle,
        DeviceKind, DeviceTarget, EditField, MeterAnim, Message, RowMode, View,
        ZOOM_STEP, AssignMenu, CUE_DEST, CONTROL_ROOM_W, control_room_width,
        control_room_visual_width,
        is_control_room_cid, strip_params, ResetMix, OptionsMenu,
        FX_ECHO_CONTROLS_H, FX_FADER_W, FX_HEADER_H, FX_KNOBS_W, FX_REV_CONTROLS_H,
        FX_REV_TAIL_H, FX_ROW_H, FX_TYPE_H, FX_W, StartupMismatch,
    };
    use crate::osc::OscOutbound;
    use crate::sidebar;
    use crate::theme;
    use crate::widgets::strip;
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};
    use totalcontrol_core::fireface::UcxDump;
    use totalcontrol_core::RmeDevice;

    #[test]
    fn page_feedback_uses_classic_addresses_and_val_companions() {
        let mut state = new(true, None, None);
        state.device.settings_mut().main_out = Some(0);
        let (tx, mut rx) = iced::futures::channel::mpsc::channel(4096);
        state.osc_tx = Some(tx);
        super::notify_rme_page(&mut state, 1);

        let mut addrs = Vec::new();
        while let Ok(message) = rx.try_recv() {
            if let crate::osc::OscOutbound::Raw { addr, .. } = message {
                addrs.push(addr);
            }
        }
        assert!(addrs.iter().any(|a| a == "/1/volume1"), "{addrs:?}");
        assert!(addrs.iter().any(|a| a == "/1/volume1Val"), "{addrs:?}");
        assert!(addrs.iter().any(|a| a == "/1/mute/1/1"), "{addrs:?}");
        assert!(addrs.iter().any(|a| a == "/1/trackname1"), "{addrs:?}");
        assert!(addrs.iter().any(|a| a == "/1/labelSubmix"), "{addrs:?}");
        assert!(addrs.iter().any(|a| a == "/1/mastervolume"), "{addrs:?}");
        assert!(
            !addrs.iter().any(|a| a.starts_with("/input/")),
            "native addresses leaked: {addrs:?}"
        );
    }

    #[test]
    fn osc_page2_toggles_follow_the_confirmed_model() {
        let mut state = new(true, None, None);
        state.rme.bus = crate::osc_rme::Bus::Input;
        state.rme.channel = 0;
        let mute = |on| Message::OscRmeIn("/2/mute".into(), vec![rosc::OscType::Float(on)]);
        let _ = update(&mut state, mute(1.0));
        assert!(state.device.inputs()[0].mute);
        let _ = update(&mut state, mute(1.0));
        assert!(!state.device.inputs()[0].mute);
        let _ = update(&mut state, mute(0.0));
        assert!(!state.device.inputs()[0].mute);
    }

    #[test]
    fn the_service_switch_keeps_the_model_and_meters_alive() {
        let mut state = new(true, None, None);
        attach_device_service(&mut state);
        assert!(matches!(state.device, DeviceHandle::Mirror(_)));
        assert!(state.device_service.is_some());
        assert!(!state.device.inputs().is_empty());

        let bus = state.meter_bus.as_ref().expect("meter bus").clone();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && bus.lock().unwrap().at.is_none() {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(bus.lock().unwrap().at.is_some(), "the service publishes meters");

        let _ = state.device.set_volume(ChannelId::Output(0), 0, 0.25);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut updated = false;
        while Instant::now() < deadline && !updated {
            if let (Some(rx), DeviceHandle::Mirror(m)) =
                (&mut state.device_updates, &mut state.device)
            {
                while let Ok(snapshot) = rx.try_recv() {
                    m.update(snapshot);
                }
            }
            updated = (state.device.outputs()[0].volume - 0.25).abs() < 0.001;
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(updated, "the mirror follows the service snapshot");
    }

    #[test]
    fn transport_counter_deltas_saturate_and_total_only_audio_errors() {
        use super::transport_delta;
        use totalcontrol_core::fireface::abi::HwdepCounters;
        let base = HwdepCounters {
            out_urbs: 100,
            out_gaps: 1,
            out_shorts: 2,
            out_status_errors: 3,
            urb_errors: 4,
            recoveries: 5,
            events_dropped: 6,
            ..HwdepCounters::default()
        };
        let now = HwdepCounters {
            out_urbs: 120,
            out_gaps: 3,
            out_shorts: 2,
            out_status_errors: 5,
            urb_errors: 4,
            recoveries: 5,
            events_dropped: 9,
            ..HwdepCounters::default()
        };
        let d = transport_delta(&now, &base);
        assert_eq!(d.out_urbs, 20);
        assert_eq!(d.out_gaps, 2);
        assert_eq!(d.out_shorts, 0);
        assert_eq!(d.out_status_errors, 2);
        assert_eq!(
            d.out_gaps + d.out_shorts + d.out_status_errors + d.urb_errors + d.recoveries,
            4
        );
        // A reset/restart baseline can be newer than the reading.
        let back = transport_delta(&base, &now);
        assert_eq!(back.out_gaps, 0);
    }

    #[test]
    fn a_host_edit_reaches_the_saved_scene_with_the_service() {
        let mut state = new(true, None, None);
        attach_device_service(&mut state);
        state.host_scene = Some(state.device.capture_scene());

        let cid = ChannelId::Playback(0);
        let out = 0;
        let initial = state.device.volume(cid, out).unwrap_or(0.0);
        let target = if initial > 0.5 {
            initial - 0.25
        } else {
            initial + 0.25
        };
        assert!(
            (target - initial).abs() > 0.1,
            "the test needs a visible change"
        );
        let _ = update(&mut state, Message::VolumeChanged(cid, out, target));

        // The mirror learns the value from the service snapshot; the saved
        // host scene must be merged from that confirmation, not from the
        // stale local model right after the command.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saved = 0.0;
        while Instant::now() < deadline {
            let _ = update(&mut state, Message::DeviceUpdated);
            saved = state
                .host_scene
                .as_ref()
                .and_then(|scene| scene.playbacks.first())
                .and_then(|pb| pb.volumes.get(out))
                .copied()
                .unwrap_or(0.0);
            if (saved - target).abs() < 0.001 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            (saved - target).abs() < 0.001,
            "host scene kept {saved}, expected the confirmed {target}"
        );
    }

    #[test]
    fn osc_commands_go_through_the_host_scene_merge() {
        let mut state = new(true, None, None);
        state.host_scene = Some(state.device.capture_scene());
        let _ = update(
            &mut state,
            Message::OscRmeIn("/1/volume1".into(), vec![rosc::OscType::Float(0.5)]),
        );
        assert!(
            state.host_scene.as_ref().unwrap().inputs[0].volumes[0] > 0.0,
            "an OSC fader move must reach the saved host scene"
        );
        let _ = update(
            &mut state,
            Message::OscRmeIn("/2/lowcutEnable".into(), vec![rosc::OscType::Float(1.0)]),
        );
        assert!(
            state.host_scene.as_ref().unwrap().inputs[0]
                .eq
                .unwrap()
                .low_cut_enabled,
            "an OSC low-cut switch must reach the saved host scene"
        );
    }

    #[test]
    fn uc_latency_chain_counts_queue_and_endpoint() {
        assert_eq!(uc_usb_frames(44_100, (48, 2)), (96, 48));
        assert_eq!(uc_usb_frames(96_000, (48, 2)), (192, 96));
        assert_eq!(uc_min_buffer_frames(44_100, (48, 2)), 192);
        assert!((frames_ms(256, 44_100) - 5.805).abs() < 0.01);
    }

    #[test]
    fn ucx_latency_chain_counts_queue_and_device_lead() {
        // Eight 1 ms ISO packets per URB at the default 48-frame size.
        assert_eq!(ucx_urb_frames(48_000, 48), 48);
        assert_eq!(ucx_urb_frames(96_000, 48), 96);
        assert_eq!(ucx_usb_frames(48_000, (48, 2)), (144, 60));
        assert_eq!(ucx_usb_frames(96_000, (48, 2)), (288, 60));
        // nurbs + 2 URBs of ring margin.
        assert_eq!(ucx_min_buffer_frames(48_000, (48, 2)), 192);
        assert_eq!(ucx_min_buffer_frames(96_000, (48, 2)), 384);
        assert!((frames_ms(144, 48_000) - 3.0).abs() < 0.01);
        assert!((frames_ms(60, 48_000) - 1.25).abs() < 0.01);
        // Half of the measured 25-frame converter round trip, 23 at 4x.
        assert_eq!(converter_frames(44_100), 13);
        assert_eq!(converter_frames(48_000), 13);
        assert_eq!(converter_frames(96_000), 13);
        assert_eq!(converter_frames(192_000), 12);
    }

    #[test]
    fn ucx_latency_line_is_an_rtl_estimate() {
        let rtl = RtlEstimate {
            usb: 144,
            lead: 60,
            converter: 13,
            graph: 128,
            rate: 44_100,
        };
        assert_eq!(rtl.total(), 345);
        assert_eq!(rtl.summary(), "7.8 ms RTL estimate (±1.2 ms)");
        assert_eq!(
            rtl.breakdown(),
            "3.3 ms USB + 1.4 ms device lead + 0.3 ms converter + 2.9 ms graph"
        );
    }

    #[test]
    #[ignore = "requires both Fireface UC and UCX connected"]
    fn manual_switch_between_connected_units() {
        let targets = super::DeviceHandle::available_targets();
        let uc = targets
            .iter()
            .find(|target| target.kind == DeviceKind::Uc)
            .cloned()
            .expect("connect the Fireface UC");
        let ucx = targets
            .iter()
            .find(|target| target.kind == DeviceKind::Ucx)
            .cloned()
            .expect("connect the Fireface UCX");

        let mut state = new_with_device(
            false,
            None,
            None,
            Some(DeviceKind::Ucx),
            ucx.serial.clone(),
        );
        assert_eq!(state.device_target.as_ref(), Some(&ucx));
        assert_eq!(state.device.canonical_model(), "Fireface UCX");

        super::switch_device(&mut state, uc.clone()).expect("switch to UC");
        assert_eq!(state.device_target.as_ref(), Some(&uc));
        assert_eq!(state.device.canonical_model(), "Fireface UC");
        assert_eq!(state.device.inputs().len(), 18);
        assert!(state.host_scene.is_some());

        super::switch_device(&mut state, ucx.clone()).expect("switch back to UCX");
        assert_eq!(state.device_target.as_ref(), Some(&ucx));
        assert_eq!(state.device.canonical_model(), "Fireface UCX");
        assert_eq!(state.device.inputs().len(), 18);
    }

    #[test]
    fn serial_selects_the_exact_unit_not_just_the_model() {
        let a = DeviceTarget {
            kind: DeviceKind::Ucx,
            serial: Some("A".into()),
        };
        let b = DeviceTarget {
            kind: DeviceKind::Ucx,
            serial: Some("B".into()),
        };
        let available = vec![a.clone(), b.clone()];
        assert_eq!(
            choose_startup_target(&available, Some(DeviceKind::Ucx), Some("B")),
            Some(b.clone())
        );
        assert_eq!(
            choose_startup_target(&available, Some(DeviceKind::Ucx), Some("missing")),
            Some(a)
        );
    }

    #[test]
    fn one_connected_device_beats_a_stale_device_preference() {
        // The saved prefs still said Babyface, but only the UC is present.
        assert_eq!(
            choose_startup_kind(&[DeviceKind::Uc], Some(DeviceKind::Babyface)),
            Some(DeviceKind::Uc)
        );
    }

    #[test]
    fn explicit_preference_wins_when_it_is_still_connected() {
        assert_eq!(
            choose_startup_kind(
                &[DeviceKind::Uc, DeviceKind::Ucx],
                Some(DeviceKind::Ucx)
            ),
            Some(DeviceKind::Ucx)
        );
    }

    #[test]
    fn existing_device_is_kept_when_another_one_appears() {
        // Startup policy is only chosen when the current device is absent;
        // when both are present, the saved/current model wins.
        assert_eq!(
            choose_startup_kind(
                &[DeviceKind::Uc, DeviceKind::Ucx],
                Some(DeviceKind::Uc)
            ),
            Some(DeviceKind::Uc)
        );
    }

    #[test]
    fn a_fresh_profile_opens_the_ucx_simulation() {
        assert_eq!(
            choose_startup_kind(
                &[DeviceKind::Mock, DeviceKind::MockUcx, DeviceKind::MockUc],
                None
            ),
            Some(DeviceKind::MockUcx),
            "no hardware and no preference: the UCX simulation is the default"
        );
    }

    #[test]
    fn only_one_device_is_selected_even_without_prefs() {
        assert_eq!(
            choose_startup_kind(&[DeviceKind::Ucx], None),
            Some(DeviceKind::Ucx)
        );
    }

    #[test]
    fn a_saved_simulation_gives_way_to_a_connected_device() {
        assert_eq!(
            choose_startup_kind(&[DeviceKind::Uc], Some(DeviceKind::Mock)),
            Some(DeviceKind::Uc)
        );
    }

    #[test]
    fn the_simulation_is_used_when_nothing_is_connected() {
        assert_eq!(
            choose_startup_kind(
                &[DeviceKind::Mock, DeviceKind::MockUcx, DeviceKind::MockUc],
                Some(DeviceKind::Mock)
            ),
            Some(DeviceKind::Mock),
            "the saved simulation stays while no real device is present"
        );
    }

    #[test]
    fn output_meter_reorder_moves_pairs_not_playback_channels() {
        // UC matrix/display order: AN1/2, AN3/4, AN5/6, Phones, ...
        let order = [1usize, 2, 3, 0, 4, 5, 6, 7, 8];
        // Playback 1/2 (channels 0 and 1) routed to Phones (display pair 3).
        let mut channels = vec![vec![0.0f32; 9]; 2];
        channels[0][3] = 1.0;
        channels[1][3] = 1.0;
        reorder_pair_volumes(&mut channels, &order);
        assert_eq!(channels.len(), 2, "playback channels must not move");
        // Phones is internal pair 0; AN5/6 (internal pair 3) must stay dark.
        assert_eq!(channels[0][0], 1.0);
        assert_eq!(channels[1][0], 1.0);
        assert_eq!(channels[0][3], 0.0);
        assert_eq!(channels[1][3], 0.0);
    }

    #[test]
    fn output_meters_reads_the_right_pair_for_every_individual_channel() {
        // 3 output pairs (6 individual channels): pair 0 = ch0/1, pair 1
        // = ch2/3, pair 2 = ch4/5. One input, routed at full volume into
        // pair 2 only (and silent into pairs 0/1), reading a fixed 1.0
        // meter level — deterministic, unlike the mock backend's own
        // randomized meters.
        let input_volumes = vec![vec![0.0, 0.0, 1.0]];
        let input_meters = vec![1.0];
        let out = power_sum_output_meters(&input_volumes, &input_meters, &[], &[], 6);

        assert_eq!(out.len(), 6);
        assert_eq!(out[0], 0.0, "pair 0 (ch0) has no signal routed to it");
        assert_eq!(out[1], 0.0, "pair 0 (ch1) has no signal routed to it");
        assert_eq!(out[2], 0.0, "pair 1 (ch2) has no signal routed to it");
        assert_eq!(out[3], 0.0, "pair 1 (ch3) has no signal routed to it");
        assert_eq!(out[4], 1.0, "pair 2 (ch4) should read the full routed signal");
        assert_eq!(out[5], 1.0, "pair 2 (ch5) should read the full routed signal");
    }

    #[test]
    fn output_meters_never_goes_out_of_bounds_for_pairs_past_the_volumes_array() {
        // A device with more output *pairs* than any single source's
        // `volumes` array happens to have entries for (shouldn't happen
        // in practice, but the old bug's failure mode was exactly an
        // out-of-bounds pair read silently going to 0.0 instead of
        // panicking) — must stay silent-zero, not panic.
        let input_volumes = vec![vec![1.0]]; // only 1 pair's worth of data
        let input_meters = vec![1.0];
        let out = power_sum_output_meters(&input_volumes, &input_meters, &[], &[], 12);
        assert_eq!(out.len(), 12);
        assert_eq!(out[0], 1.0);
        assert_eq!(out[1], 1.0);
        for level in &out[2..] {
            assert_eq!(*level, 0.0);
        }
    }

    #[test]
    fn assign_menu_survives_hover_and_collapse_ticks() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::ToggleAssign);
        assert_eq!(state.assign_menu, AssignMenu::Destinations);
        let _ = update(
            &mut state,
            Message::StripHovered(Some(ChannelId::Input(0))),
        );
        assert_eq!(
            state.assign_menu,
            AssignMenu::Destinations,
            "hovering the strip row must not close the menu"
        );
        let _ = update(&mut state, Message::CollapseTick);
        assert_eq!(state.assign_menu, AssignMenu::Destinations);
        let _ = update(&mut state, Message::ToggleAssign);
        assert_eq!(state.assign_menu, AssignMenu::Closed);
    }

    #[test]
    fn assign_menu_survives_the_global_pointer_press_and_applies_a_choice() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::ToggleAssign);
        assert_eq!(state.assign_menu, AssignMenu::Destinations);
        // The app-wide subscription publishes this for every press, including
        // a press on a menu card.  It must not close the menu on mouse-down.
        let _ = update(&mut state, Message::PointerPressed);
        assert_eq!(state.assign_menu, AssignMenu::Destinations);
        let _ = update(&mut state, Message::AssignDestinations(1));
        assert_eq!(state.assign_menu, AssignMenu::Outputs(1));
        let _ = update(&mut state, Message::PointerPressed);
        assert_eq!(state.assign_menu, AssignMenu::Outputs(1));
        // The release then chooses the output and closes the menu.
        let _ = update(&mut state, Message::AssignOutput(1, Some(3)));
        assert_eq!(state.assign_menu, AssignMenu::Closed);
        assert_eq!(state.assign[1], Some(3));
        // A press on a real control outside still closes the menu through
        // that control's own message.
        let _ = update(&mut state, Message::ToggleAssign);
        let _ = update(&mut state, Message::StripClicked(ChannelId::Input(0)));
        assert_eq!(state.assign_menu, AssignMenu::Closed);
    }

    #[test]
    fn assign_output_moves_between_destinations_and_none_returns_it() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::AssignOutput(2, Some(3)));
        assert_eq!(state.assign[2], Some(3));
        // Moving the same output to Main Out B clears the Phones 1 slot.
        let _ = update(&mut state, Message::AssignOutput(1, Some(3)));
        assert_eq!(state.assign[1], Some(3));
        assert_eq!(state.assign[2], None);
        assert_eq!(
            state.device.settings().main_b_out,
            Some(3),
            "assigning Speaker B must reach the device model"
        );
        // None clears the slot, so the pair is no longer hidden from the
        // Hardware Outputs row.
        let _ = update(&mut state, Message::AssignOutput(1, None));
        assert_eq!(state.assign[1], None);
        assert_eq!(
            state.device.settings().main_b_out,
            None,
            "clearing Speaker B must clear the device assignment too"
        );
        assert!(!state.device.settings().speaker_b);
        assert!(
            !state.assigned_pairs().any(|p| p == 3),
            "the pair must return to the Hardware Outputs row"
        );
    }

    #[test]
    fn options_menu_opens_the_reset_submenu_and_settings_sheet() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::ToggleOptions);
        assert_eq!(state.options_menu, OptionsMenu::Root);
        let _ = update(&mut state, Message::OptionsReset);
        assert_eq!(state.options_menu, OptionsMenu::Reset);
        let _ = update(&mut state, Message::OptionsBack);
        assert_eq!(state.options_menu, OptionsMenu::Root);
        let _ = update(&mut state, Message::OptionsSettings);
        assert_eq!(state.options_menu, OptionsMenu::Closed);
        assert!(state.show_settings);
    }

    #[test]
    fn surround_settings_are_remembered_and_never_start_a_bridge_in_mock() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::SetSurround51(true));
        let _ = update(&mut state, Message::SetSurround71(true));
        assert!(state.surround_51);
        assert!(state.surround_71);
        assert!(!state.surround_51_running);
        assert!(!state.surround_71_running);
        let prefs = prefs_of(&state);
        assert!(prefs.surround_51);
        assert!(prefs.surround_71);
    }

    #[test]
    fn restored_assignments_drop_unknown_and_duplicate_pairs() {
        let mut state = new(true, None, None);
        let pairs = state.device.output_pair_count();
        state.assign[0] = Some(pairs + 8);
        state.assign[2] = Some(1);
        state.assign[3] = Some(1);

        apply_control_room_assignments(&mut state).unwrap();
        assert_eq!(state.assign[0], None);
        assert_eq!(state.assign[2], Some(1));
        assert_eq!(state.assign[3], None);
    }

    #[test]
    fn control_room_channels_are_always_stereo() {
        let mut state = new(true, None, None);
        // Split a pair, then assign it: assigning links it again.
        let _ = update(
            &mut state,
            Message::StereoLinkChanged(ChannelId::Output(0), false),
        );
        assert!(!state.device.output_linked(0));
        let _ = update(&mut state, Message::AssignOutput(0, Some(0)));
        assert!(
            state.device.output_linked(0),
            "a pair assigned to the Control Room is stereo"
        );
        // The strip and its Settings panel report it as linked.
        let p = strip_params(&state, ChannelId::Output(0), state.sel_out);
        assert!(p.stereo_linked);
        // A split request from any client is refused while assigned.
        let _ = update(
            &mut state,
            Message::StereoLinkChanged(ChannelId::Output(0), false),
        );
        assert!(state.device.output_linked(0));
        // Reset Mix "Set all channels mono" leaves the Control Room pair
        // linked and splits the unassigned pairs.
        let _ = update(&mut state, Message::ResetMix(ResetMix::AllMono));
        assert!(state.device.output_linked(0));
        assert!(!state.device.output_linked(2));
    }

    #[test]
    fn add_and_clear_start_an_assignment_animation_but_a_move_does_not() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::AssignOutput(2, Some(3)));
        let anim = state.assign_anim.as_ref().expect("an add animates");
        assert_eq!(anim.moving(), Some((2, 3)));

        // A same-count destination move snaps.
        let _ = update(&mut state, Message::AssignOutput(1, Some(3)));
        assert!(state.assign_anim.is_none());

        // Clearing animates too.
        let _ = update(&mut state, Message::AssignOutput(1, None));
        let anim = state.assign_anim.as_ref().expect("a clear animates");
        assert_eq!(anim.moving(), Some((1, 3)));
    }

    #[test]
    fn clearing_phones_2_removes_it_from_the_control_room() {
        let mut state = new(true, None, None);
        state.assign = [Some(1), None, None, Some(0), None, None, Some(0)];
        let _ = update(&mut state, Message::AssignDestinations(3));
        assert_eq!(state.assign_menu, AssignMenu::Outputs(3));
        let _ = update(&mut state, Message::AssignOutput(3, None));
        assert_eq!(state.assign[3], None);
        assert_eq!(state.assigned_pairs().collect::<Vec<_>>(), vec![1]);
        assert!(!state.is_output_assigned(0));
    }

    #[test]
    fn cue_target_is_separate_from_output_assignments() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::AssignDestinations(CUE_DEST));
        assert_eq!(state.assign_menu, AssignMenu::CueTargets);

        let _ = update(&mut state, Message::AssignCueTarget(2));
        assert_eq!(state.assign_menu, AssignMenu::Closed);
        assert_eq!(state.assign[CUE_DEST], Some(2));
        assert!(
            !state.assigned_pairs().any(|p| p == 2),
            "a Cue target is not an output assignment"
        );

        // Assigning the pair that happens to equal the Cue target index
        // must not clear the Cue target.
        let _ = update(&mut state, Message::AssignOutput(0, Some(2)));
        assert_eq!(state.assign[CUE_DEST], Some(2));
        assert!(state.is_output_assigned(2));

        // Phones 4 is a valid Control Room destination but not a Cue target.
        let _ = update(&mut state, Message::AssignCueTarget(5));
        assert_eq!(state.assign[CUE_DEST], Some(2));
    }

    #[test]
    fn the_cue_target_does_not_widen_the_control_room() {
        let mut state = new(true, None, None);
        state.assign = [None; 7];
        state.assign[CUE_DEST] = Some(2);
        assert_eq!(
            control_room_width(&state, 1.0),
            CONTROL_ROOM_W + 2.0 * theme::SPACE_SM,
            "only the six output assignments count"
        );
    }

    #[test]
    fn control_room_default_resets_the_draft_without_closing() {
        let mut state = new(true, None, None);
        state.control_room_settings_open = true;
        state.control_room_draft.talkback_input = Some(2);
        state.control_room_draft.recall_db = -3.0;
        let _ = update(&mut state, Message::ResetControlRoomSettings);
        assert!(state.control_room_settings_open);
        assert_eq!(state.control_room_draft.talkback_input, None);
        assert_eq!(state.control_room_draft.recall_db, -10.0);
    }

    #[test]
    fn control_room_header_grows_with_an_open_flyout() {
        let mut state = new(true, None, None);
        state.assign = [None; 7];
        state.assign[0] = Some(2);
        let now = Instant::now();
        let before = control_room_visual_width(&state, 1.0, now);
        let cid = ChannelId::Output(4);
        state
            .flyout_open
            .insert((cid, strip::FlyoutKind::Settings));
        let after = control_room_visual_width(&state, 1.0, now);
        assert!(
            (after - before - strip::STRIP_W).abs() < 1e-3,
            "the header must follow the open settings slideout"
        );
    }

    #[test]
    fn control_room_channels_ignore_rename_start() {
        let mut state = new(true, None, None);
        state.assign[0] = Some(2);
        let cid = ChannelId::Output(4);
        assert!(is_control_room_cid(&state, cid));
        let _ = update(&mut state, Message::RenameStart(cid));
        assert_eq!(state.renaming, None);
    }

    #[test]
    fn a_refused_write_is_shown_and_expires() {
        let mut state = new(true, None, None);
        super::fail(&mut state, "set_input_phantom", &"busy");
        let (text, _) = state.notice.clone().expect("a notice");
        assert_eq!(text, "Could not apply input phantom: busy");
        // The same failure repeated (a drag) just refreshes it.
        super::fail(&mut state, "set_input_phantom", &"busy");
        assert!(state.notice.is_some());
        state.notice = Some((text, Instant::now() - Duration::from_secs(6)));
        let _ = update(&mut state, Message::Tick);
        assert!(state.notice.is_none());
    }

    #[test]
    fn a_window_closed_behind_our_back_is_forgotten_or_ends_the_app() {
        let mut state = new(true, None, None);
        let id = iced::window::Id::unique();
        state.window_id = Some(id);
        state.tray_active = true;
        let _ = update(&mut state, Message::WindowClosed(id));
        assert_eq!(state.window_id, None);
        assert!(state.window_hidden, "with a tray the app lives on, hidden");
        // Another window's close is none of our business.
        state.window_id = Some(id);
        let _ = update(&mut state, Message::WindowClosed(iced::window::Id::unique()));
        assert_eq!(state.window_id, Some(id));
    }

    #[test]
    fn closing_hides_in_the_tray_only_when_there_is_one() {
        // Without a tray (the mock never has one) there would be no way back
        // to a hidden window: closing must quit, not hide.
        let mut state = new(true, None, None);
        assert!(!state.tray_active && state.keep_in_tray);
        let _ = update(&mut state, Message::CloseRequested);
        assert!(!state.window_hidden);
    }

    #[test]
    fn keep_in_tray_defaults_on_and_only_an_opt_out_is_saved() {
        let mut state = new(true, None, None);
        assert_eq!(prefs_of(&state).keep_in_tray, None);
        let _ = update(&mut state, Message::SetKeepInTray(false));
        let prefs = prefs_of(&state);
        assert_eq!(prefs.keep_in_tray, Some(false));
        apply_prefs(&mut state, prefs);
        assert!(!state.keep_in_tray);
        assert!(!is_undoable(&Message::SetKeepInTray(true)));
    }

    #[test]
    fn zoom_in_steps_up_from_default() {
        let mut state = new(true, None, None);
        assert_eq!(state.ui_scale, crate::theme::SCALE_DEFAULT);
        zoom(&mut state, ZOOM_STEP);
        assert!(
            (state.ui_scale - (crate::theme::SCALE_DEFAULT + ZOOM_STEP)).abs() < 1e-6,
            "one zoom-in should add exactly ZOOM_STEP: {}",
            state.ui_scale
        );
    }

    #[test]
    fn zoom_clamps_to_min_and_max() {
        let mut state = new(true, None, None);
        for _ in 0..200 {
            zoom(&mut state, -ZOOM_STEP);
        }
        assert_eq!(state.ui_scale, crate::theme::SCALE_MIN);
        for _ in 0..400 {
            zoom(&mut state, ZOOM_STEP);
        }
        assert_eq!(state.ui_scale, crate::theme::SCALE_MAX);
    }

    #[test]
    fn window_resize_does_not_change_the_zoom() {
        // The whole point of fixed geometry: a resize must NOT rescale.
        // `zoom` is the only thing that moves `ui_scale`; a resize just
        // updates `window_width` (see `apply_pending_resize`).
        let mut state = new(true, None, None);
        let before = state.ui_scale;
        state.pending_resize_width = Some(700.0);
        apply_pending_resize(&mut state);
        assert_eq!(state.ui_scale, before);
        assert_eq!(state.window_width, 700.0);
    }

    #[test]
    fn attack_rises_fast() {
        let mut m = MeterAnim::new();
        m.step(1.0);
        assert!(
            m.frame().value > 0.5,
            "one attack tick should jump most of the way: {}",
            m.frame().value
        );
    }

    #[test]
    fn release_falls_at_least_four_times_faster() {
        let mut m = MeterAnim::new();
        m.step(1.0); // reach a peak first
        let peak = m.frame().value;
        m.step(0.0);
        let drop = peak - m.frame().value;
        let old_first_drop = peak * 0.22;
        assert!(
            drop >= old_first_drop * 4.0,
            "one release tick should drop at least 4x the old first tick: {drop} vs {old_first_drop}"
        );
    }

    #[test]
    fn peak_is_held_for_two_seconds() {
        let mut m = MeterAnim::new();
        m.step(1.0);
        for _ in 0..39 {
            m.step(0.0); // ~1.95 s at the 50 ms nominal tick
        }
        assert!(
            m.frame().peak > 0.99,
            "peak should still be held after ~1.95 s: {}",
            m.frame().peak
        );
        for _ in 0..5 {
            m.step(0.0);
        }
        assert!(
            m.frame().peak < 0.99,
            "peak should start falling after the 2 s hold: {}",
            m.frame().peak
        );
    }

    #[test]
    fn a_new_peak_restarts_the_hold() {
        let mut m = MeterAnim::new();
        m.step(1.0);
        m.step(0.0);
        m.step(0.0);
        m.step(1.0);
        assert_eq!(m.peak_hold_ms, 0.0);
        assert_eq!(m.peak_release_ms, 0.0);
    }

    #[test]
    fn the_peak_glides_down_after_the_hold() {
        let mut m = MeterAnim::new();
        m.step(1.0);
        for _ in 0..41 {
            m.step(0.0); // pass the 2 s hold
        }
        let before_peak = m.frame().peak;
        m.step(0.0);
        let peak_drop = before_peak - m.frame().peak;
        assert!(
            peak_drop > 0.0 && peak_drop < before_peak * 0.5,
            "peak marker should glide down, not snap: {peak_drop} of {before_peak}"
        );
        assert!(
            m.frame().value < 0.001,
            "the bar itself should already have snapped to zero"
        );
    }

    // ── Right sidebar: Undo/Redo, Groups, Layout ──────────────────────
    // Interactive click-testing (`--mock` + synthetic clicks) turned out
    // to be unreliable in this sandbox for this pass — see the
    // `project_bus_redesign_2026_09` memory. These exercise the same
    // `update()` codepath a real click would, directly, which needs no
    // window/focus/click-coordinate cooperation from the environment.

    #[test]
    fn undo_reverts_a_mute() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(0);
        let _ = update(&mut state, Message::Mute(cid, true));
        assert!(state.device.inputs()[0].mute);
        let _ = update(&mut state, Message::Undo);
        assert!(!state.device.inputs()[0].mute, "undo should revert the mute");
    }

    #[test]
    fn undo_does_not_roll_back_device_settings() {
        let mut state = new(true, None, None);
        state.device = DeviceHandle::open_mock_ucx();
        state.host_scene = Some(state.device.capture_scene());
        // A mixer edit creates the undo step.
        let _ = update(
            &mut state,
            Message::VolumeChanged(ChannelId::Input(0), 0, 0.5),
        );
        // The option is not an undo step, and it changes the device.
        let _ = update(&mut state, Message::WordClockTerminationChanged(true));
        assert!(state.device.settings().word_clock_termination);
        let _ = update(&mut state, Message::Undo);
        assert!(
            state.device.settings().word_clock_termination,
            "undoing a mixer edit must not roll back a device setting"
        );
        assert!(state
            .host_scene
            .as_ref()
            .unwrap()
            .settings
            .word_clock_termination);
    }

    #[test]
    fn redo_reapplies_after_undo() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(0);
        let _ = update(&mut state, Message::Mute(cid, true));
        let _ = update(&mut state, Message::Undo);
        assert!(!state.device.inputs()[0].mute);
        let _ = update(&mut state, Message::Redo);
        assert!(state.device.inputs()[0].mute, "redo should reapply the mute");
    }

    #[test]
    fn a_new_mutating_action_clears_the_redo_stack() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(0);
        let _ = update(&mut state, Message::Mute(cid, true));
        let _ = update(&mut state, Message::Undo);
        assert!(!state.redo_stack.is_empty(), "sanity: undo should have populated redo");
        let _ = update(&mut state, Message::Mute(cid, true));
        assert!(
            state.redo_stack.is_empty(),
            "a fresh mutating action should invalidate the old redo history"
        );
    }

    #[test]
    fn ui_only_messages_do_not_grow_the_undo_stack() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::Tick);
        let _ = update(&mut state, Message::StripHovered(Some(ChannelId::Input(0))));
        assert!(
            state.undo_stack.is_empty(),
            "Tick/hover are UI-only and shouldn't be undoable steps"
        );
    }

    #[test]
    fn service_edges_do_not_grow_the_undo_stack() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::DeviceUpdated);
        let _ = update(&mut state, Message::MeterWake);
        assert!(
            state.undo_stack.is_empty(),
            "a model snapshot or meter wake is not a user edit"
        );
    }

    // `Input(0)`/`Input(4)` deliberately — channels 0/1 are a *stereo
    // link* pair by default in mock (`input_pair_link`, see
    // `mock.rs::test_input_pair_linked_by_default_and_moves_both_channels`),
    // which propagates independently of anything Groups-related and
    // would confound these tests (caught by an earlier failing run of
    // this exact test using `Input(0)`/`Input(1)` — the stereo-pair
    // mechanism was moving "b" too, for a reason that had nothing to do
    // with the group). Channels 0 and 4 are in different pairs.

    #[test]
    fn group_mute_link_propagates_to_members() {
        let mut state = new(true, None, None);
        let a = ChannelId::Input(0);
        let b = ChannelId::Input(4);
        state.selected.insert(a);
        state.selected.insert(b);
        let _ = update(&mut state, Message::GroupEditToggle);
        let _ = update(&mut state, Message::GroupLinkToggle(0, sidebar::GroupLink::Mute));
        assert_eq!(state.groups[0].members.len(), 2);
        assert!(state.groups[0].mute_linked);
        // Clear the selection used to assign membership — isolates
        // group-driven propagation from the pre-existing multi-select
        // one, which would also propagate here and mask a group-only bug.
        state.selected.clear();

        let _ = update(&mut state, Message::Mute(a, true));
        assert!(state.device.inputs()[0].mute);
        assert!(
            state.device.inputs()[4].mute,
            "muting one mute-linked group member should mute the other"
        );
    }

    #[test]
    fn group_solo_link_does_not_propagate_mute() {
        // A solo-linked-only group shouldn't cross-propagate mute, and
        // vice versa — the three link types are independent.
        let mut state = new(true, None, None);
        let a = ChannelId::Input(0);
        let b = ChannelId::Input(4);
        state.selected.insert(a);
        state.selected.insert(b);
        let _ = update(&mut state, Message::GroupEditToggle);
        let _ = update(&mut state, Message::GroupLinkToggle(0, sidebar::GroupLink::Solo));
        state.selected.clear();

        let _ = update(&mut state, Message::Mute(a, true));
        assert!(state.device.inputs()[0].mute);
        assert!(
            !state.device.inputs()[4].mute,
            "solo-linked (not mute-linked) group shouldn't propagate mute"
        );
    }

    #[test]
    fn group_fader_link_moves_members_by_the_same_relative_delta() {
        let mut state = new(true, None, None);
        let a = ChannelId::Input(0);
        let b = ChannelId::Input(4);
        state.selected.insert(a);
        state.selected.insert(b);
        let _ = update(&mut state, Message::GroupEditToggle);
        let _ = update(&mut state, Message::GroupLinkToggle(0, sidebar::GroupLink::Fader));
        state.selected.clear();

        // Both start at the default unity volume (1.0) — an equal
        // relative move should land them at the same place.
        let _ = update(&mut state, Message::VolumeChanged(a, 0, 0.5));
        let vol_a = state.device.inputs()[0].volumes[0];
        let vol_b = state.device.inputs()[4].volumes[0];
        assert!((vol_a - 0.5).abs() < 1e-4);
        assert!(
            (vol_b - 0.5).abs() < 1e-4,
            "fader-linked member starting at the same volume should track exactly: {vol_b}"
        );
    }

    #[test]
    fn group_clear_removes_all_membership() {
        let mut state = new(true, None, None);
        let a = ChannelId::Input(0);
        state.selected.insert(a);
        let _ = update(&mut state, Message::GroupEditToggle);
        let _ = update(&mut state, Message::GroupLinkToggle(0, sidebar::GroupLink::Mute));
        assert!(!state.groups[0].members.is_empty());

        let _ = update(&mut state, Message::GroupClear);
        assert!(state.groups.iter().all(|g| g.members.is_empty()));
        assert!(!state.group_editing);
    }

    /// The chip's word maps the connection state in priority order: a
    /// disconnect outranks everything, then the mock's own "Simulated"
    /// (it is the loudest fact about the connection, even when the mock
    /// imitates a class-compliant card). The two real words only exist
    /// on real hardware, where they were verified live.
    #[test]
    fn device_state_reports_the_personality_and_a_disconnect() {
        let mut state = new(true, None, None);
        assert_eq!(crate::sidebar::device_state(&state).1, "Simulated");

        // The mock keeps its own word even when it imitates the
        // class-compliant feature set (`TOTALCONTROL_MOCK_CC=1 --mock`).
        state.device.set_protocol_features(false);
        assert_eq!(crate::sidebar::device_state(&state).1, "Simulated");

        state.device.set_connected(false);
        let _ = update(&mut state, Message::Tick);
        assert!(!state.connected, "the tick probe must see the disconnect");
        assert_eq!(crate::sidebar::device_state(&state).1, "Disconnected");
        assert_eq!(
            crate::sidebar::device_state(&state).0,
            crate::theme::MRED,
            "a disconnect is the red state, not the amber ones"
        );

        // `--mock` was explicit here, so the probe must not try to
        // "reconnect" to hardware that was never asked for: force the
        // throttle open and tick again.
        state.last_conn_check = None;
        let _ = update(&mut state, Message::Tick);
        assert!(!state.connected, "explicit --mock never reconnects");
    }

    #[test]
    fn class_compliant_mode_drops_protocol_messages() {
        let mut state = new(true, None, None);

        // Trim is mixer-side, so it lands in both grammars.
        let _ = update(&mut state, Message::TrimChanged(ChannelId::Input(0), -6.0));
        assert_eq!(state.device.inputs()[0].trim, -6.0);

        let pushed = state.undo_stack.len();
        state.device.set_protocol_features(false);
        let _ = update(&mut state, Message::TrimChanged(ChannelId::Input(0), -3.0));
        assert_eq!(
            state.device.inputs()[0].trim, -3.0,
            "trim is mixer-side and works in both grammars"
        );
        // The second change belongs to the same gesture, so it adds no
        // second undo step.
        assert_eq!(state.undo_stack.len(), pushed);

        // On the class-compliant personality a protocol setter can only
        // fail, so the renderers hide the control and `update` drops what a
        // stale key binding or an OSC message would still send.
        let pushed = state.undo_stack.len();
        let _ = update(&mut state, Message::EqEnabled(ChannelId::Input(0), true));
        assert!(
            !state.device.inputs()[0].eq.as_ref().unwrap().enabled,
            "the EQ must not engage behind a hidden button"
        );
        assert_eq!(
            state.undo_stack.len(),
            pushed,
            "a dropped message must not have pushed a scene"
        );

        // Messages that are not protocol-bound still land, and still push.
        let _ = update(&mut state, Message::Mute(ChannelId::Input(0), true));
        assert!(state.device.inputs()[0].mute);
        assert_eq!(state.undo_stack.len(), pushed + 1, "a real action still pushes a scene");
    }

    #[test]
    fn toggle_sidebar_flips_and_is_not_undoable() {
        let mut state = new(true, None, None);
        assert!(state.sidebar_open, "sidebar starts expanded");
        let _ = update(&mut state, Message::ToggleSidebar);
        assert!(!state.sidebar_open);
        let _ = update(&mut state, Message::ToggleSidebar);
        assert!(state.sidebar_open);
        assert!(
            state.undo_stack.is_empty(),
            "collapsing/expanding the sidebar is UI chrome, not a device change"
        );
    }

    #[test]
    fn the_old_default_sidebar_width_migrates_to_the_new_one() {
        let mut state = new(true, None, None);
        apply_prefs(
            &mut state,
            crate::prefs::Prefs {
                sidebar_width: Some(sidebar::OLD_DEFAULT_WIDTH),
                ..Default::default()
            },
        );
        assert_eq!(state.sidebar_width, sidebar::DEFAULT_WIDTH);

        let mut custom = new(true, None, None);
        apply_prefs(
            &mut custom,
            crate::prefs::Prefs {
                sidebar_width: Some(250.0),
                ..Default::default()
            },
        );
        assert_eq!(custom.sidebar_width, 250.0);
    }

    #[test]
    fn sidebar_resize_clamps_and_is_not_undoable() {
        let mut state = new(true, None, None);
        assert_eq!(state.sidebar_width, sidebar::DEFAULT_WIDTH);

        // Dragging the strip left (negative dx) widens the column.
        let _ = update(&mut state, Message::SidebarResize(-40.0));
        assert_eq!(state.sidebar_width, sidebar::DEFAULT_WIDTH + 40.0);

        // Clamped both ends, so a fast drag past the window edge can't
        // produce a zero or negative width to lay the mixer out against.
        let _ = update(&mut state, Message::SidebarResize(-10_000.0));
        assert_eq!(state.sidebar_width, sidebar::MAX_WIDTH);
        let _ = update(&mut state, Message::SidebarResize(10_000.0));
        assert_eq!(state.sidebar_width, sidebar::MIN_WIDTH);

        // One message per drag step, so this is the assertion that
        // matters: a resize must not push a `Scene` per mouse move.
        assert!(
            state.undo_stack.is_empty(),
            "resizing the sidebar is UI chrome, not a device change"
        );
    }

    #[test]
    fn eq_plot_drag_updates_frequency_and_gain_together() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::EqBandPoint(ChannelId::Input(0), 0, 2_500, 6.5));
        let eq = state.device.inputs()[0].eq.unwrap();
        assert_eq!(eq.bands[0].freq_hz, 2_500);
        assert!((eq.bands[0].gain_db - 6.5).abs() < 0.01);
    }

    #[test]
    fn eq_plot_double_click_resets_the_band() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::EqBandPoint(ChannelId::Input(0), 0, 2_500, 6.5));
        let _ = update(&mut state, Message::EqBandQ(ChannelId::Input(0), 0, 3.0));
        let _ = update(&mut state, Message::EqBandReset(ChannelId::Input(0), 0));
        let eq = state.device.inputs()[0].eq.unwrap();
        assert_eq!(eq.bands[0].freq_hz, 1_000);
        assert!(eq.bands[0].gain_db.abs() < 0.01);
        assert!((eq.bands[0].q - 0.7).abs() < 0.01);
    }

    #[test]
    fn global_mute_master_clears_the_active_mutes_then_restores_them() {
        let mut state = new(true, None, None);
        assert!(!any_channel_muted(&state), "mock starts fully unmuted");
        let a = ChannelId::Input(0);
        let b = ChannelId::Playback(2);
        let _ = update(&mut state, Message::Mute(a, true));
        let _ = update(&mut state, Message::Mute(b, true));
        assert!(any_channel_muted(&state), "the master lights with any mute");

        let _ = update(&mut state, Message::GlobalMuteToggle);
        assert!(!any_channel_muted(&state), "first press clears every mute");

        let _ = update(&mut state, Message::GlobalMuteToggle);
        assert!(channel_is_muted(&state, a));
        assert!(channel_is_muted(&state, b));
        assert!(state.last_cleared_mutes.is_empty(), "the remembered set was used up");
    }

    #[test]
    fn cue_is_exclusive_across_outputs() {
        let mut state = new(true, None, None);
        let a = ChannelId::Output(0); // pair 0
        let b = ChannelId::Output(2); // pair 1

        let _ = update(&mut state, Message::CueChanged(a, true));
        assert!(state.device.outputs()[0].cue);
        assert!(state.device.outputs()[1].cue, "cue mirrors across a pair");
        assert!(!state.device.outputs()[2].cue);

        // Engaging CUE on a different output silently disengages `a`
        // (there's only one AN1/2 monitor bus to share) rather than
        // both reading as cued at once.
        let _ = update(&mut state, Message::CueChanged(b, true));
        assert!(!state.device.outputs()[0].cue, "a should no longer be cued");
        assert!(!state.device.outputs()[1].cue);
        assert!(state.device.outputs()[2].cue);
        assert!(state.device.outputs()[3].cue);

        let _ = update(&mut state, Message::CueChanged(b, false));
        assert!(!state.device.outputs()[2].cue, "explicit off clears it");
        assert!(!state.device.outputs()[3].cue);
    }

    #[test]
    fn ucx_device_options_reach_the_mock_and_the_saved_scene() {
        let mut state = new(true, None, None);
        state.device = DeviceHandle::open_mock_ucx();
        state.host_scene = Some(state.device.capture_scene());

        let _ = update(&mut state, Message::WordClockTerminationChanged(true));
        let _ = update(&mut state, Message::SpdifProfessionalChanged(true));
        let _ = update(&mut state, Message::WordClockSingleSpeedChanged(true));

        let settings = state.device.settings();
        assert!(settings.word_clock_termination);
        assert!(settings.spdif_professional);
        assert!(settings.word_clock_single_speed);
        let saved = &state.host_scene.as_ref().unwrap().settings;
        assert!(saved.word_clock_termination);
        assert!(saved.spdif_professional);
        assert!(saved.word_clock_single_speed);
    }

    #[test]
    fn eq_for_record_and_optical_out_format_toggles_reach_device_settings() {
        let mut state = new(true, None, None);
        assert!(!state.device.settings().eq_for_record);
        assert!(!state.device.settings().optical_out_spdif);

        let _ = update(&mut state, Message::EqForRecordChanged(true));
        assert!(state.device.settings().eq_for_record);
        assert!(!state.device.settings().optical_out_spdif);

        let _ = update(&mut state, Message::OpticalOutFormatChanged(true));
        assert!(state.device.settings().optical_out_spdif);

        let _ = update(&mut state, Message::EqForRecordChanged(false));
        assert!(!state.device.settings().eq_for_record);
        assert!(
            state.device.settings().optical_out_spdif,
            "toggling EQ-for-Record must not stomp the SPDIF flag (shared settings-word register)"
        );
    }

    #[test]
    fn phase_changed_reaches_the_input_model() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(1); // AN2

        let _ = update(&mut state, Message::PhaseChanged(cid, true));
        assert!(state.device.inputs()[1].phase);

        let _ = update(&mut state, Message::PhaseChanged(cid, false));
        assert!(!state.device.inputs()[1].phase);
    }

    #[test]
    fn phantom_changed_mirrors_across_a_linked_input_pair() {
        // User-caught: toggling 48V on a linked AN1/2 strip only ever
        // touched its own (left) channel — Message::Phantom called
        // set_phantom directly, bypassing the new pair-aware
        // set_input_phantom entirely.
        let mut state = new(true, None, None);
        assert!(state.device.input_pair_linked(0));
        let _ = update(&mut state, Message::Phantom(0, false));
        let _ = update(&mut state, Message::Phantom(1, false));

        let _ = update(&mut state, Message::Phantom(0, true));
        assert!(state.device.inputs()[0].phantom);
        assert!(
            state.device.inputs()[1].phantom,
            "48V must mirror to AN2 when the pair is linked"
        );
    }

    #[test]
    fn linked_input_strip_balance_is_derived_from_the_split_levels() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(0);
        let out = state.sel_out;

        let _ = update(&mut state, Message::StereoLinkChanged(cid, false));
        state.device.set_input_volume(0, 0, out, 0.1).unwrap();
        state.device.set_input_volume(0, 1, out, 0.5).unwrap();
        let _ = update(&mut state, Message::StereoLinkChanged(cid, true));

        let params = super::strip_params(&state, ChannelId::Input(0), out);
        assert!((params.vol - 0.5).abs() < 1e-6);
        assert_eq!(params.pan, 80);
    }

    #[test]
    fn linked_input_strip_shows_the_louder_channel() {
        let mut state = new(true, None, None);
        let out = state.sel_out;
        state.device.set_input_pair_linked(0, false).unwrap();
        state
            .device
            .set_input_volume(0, 0, out, 0.1)
            .unwrap();
        state
            .device
            .set_input_volume(0, 1, out, 0.5)
            .unwrap();
        state.device.set_input_pair_linked(0, true).unwrap();

        let params = super::strip_params(&state, ChannelId::Input(0), out);
        assert!((params.vol - 0.5).abs() < 1e-6);
    }

    #[test]
    fn linked_playback_strip_shows_the_louder_channel() {
        let mut state = new(true, None, None);
        let out = state.sel_out;
        state.device.set_playback_linked(0, false).unwrap();
        state
            .device
            .set_playback_volume(0, 0, out, 0.1)
            .unwrap();
        state
            .device
            .set_playback_volume(0, 1, out, 0.5)
            .unwrap();
        state.device.set_playback_linked(0, true).unwrap();

        let params = super::strip_params(&state, ChannelId::Playback(0), out);
        assert!((params.vol - 0.5).abs() < 1e-6);
        assert_eq!(params.pan, 80);
    }

    #[test]
    fn unlinking_a_playback_pair_splits_both_channels() {
        // A playback pair's link is its hardware stereo split: the STEREO
        // switch is the one control for it (the strip's "SP" button was the
        // same state a second time, and is gone).
        let mut state = new(true, None, None);
        let cid = ChannelId::Playback(0); // PB1 (channels 0/1)

        let _ = update(&mut state, Message::StereoLinkChanged(cid, false));
        assert!(state.device.playbacks()[0].split);
        assert!(state.device.playbacks()[1].split, "split mirrors across both channels of the pair");
        assert!(
            state.stereo_anim.contains_key(&ChannelId::Playback(0)),
            "the split must animate the rest of the row instead of snapping"
        );

        let _ = update(&mut state, Message::StereoLinkChanged(cid, true));
        assert!(!state.device.playbacks()[0].split);
        assert!(!state.device.playbacks()[1].split);
    }

    #[test]
    fn opening_a_flyout_on_one_strip_does_not_close_another_strips_flyout() {
        // User-caught regression: TotalControl used to force a single global
        // `Option<(ChannelId, FlyoutKind)>`, so opening Settings on a
        // second strip silently closed whatever was open on the first —
        // real TotalMix allows several channel-settings panels open at
        // once.
        let mut state = new(true, None, None);
        let a = ChannelId::Input(0);
        let b = ChannelId::Input(2);

        let _ = update(&mut state, Message::ToggleFlyout(a, strip::FlyoutKind::Settings));
        let _ = update(&mut state, Message::ToggleFlyout(b, strip::FlyoutKind::Settings));
        assert!(state.flyout_open.contains(&(a, strip::FlyoutKind::Settings)));
        assert!(state.flyout_open.contains(&(b, strip::FlyoutKind::Settings)));

        // Re-pressing the same strip's own trigger still closes just it.
        let _ = update(&mut state, Message::ToggleFlyout(a, strip::FlyoutKind::Settings));
        assert!(!state.flyout_open.contains(&(a, strip::FlyoutKind::Settings)));
        assert!(state.flyout_open.contains(&(b, strip::FlyoutKind::Settings)));
    }

    #[test]
    fn a_second_kind_on_the_same_strip_replaces_the_first_but_route_stays_exclusive_globally() {
        let mut state = new(true, None, None);
        let a = ChannelId::Input(0);
        let b = ChannelId::Input(2);

        // Settings then EQ on the SAME strip: the width morphs directly
        // from the Settings width to the EQ width, without a full
        // slide-out then slide-in.
        let _ = update(&mut state, Message::ToggleFlyout(a, strip::FlyoutKind::Settings));
        let _ = update(&mut state, Message::ToggleFlyout(a, strip::FlyoutKind::Eq));
        assert!(!state.flyout_open.contains(&(a, strip::FlyoutKind::Settings)));
        assert!(state.flyout_open.contains(&(a, strip::FlyoutKind::Eq)));
        let eq_anim = state
            .flyout_anim
            .get(&(a, strip::FlyoutKind::Eq))
            .expect("EQ keeps an in-flight width tween");
        assert!(
            eq_anim.prev > 0.0,
            "EQ starts from the Settings width, not zero"
        );
        assert!(
            eq_anim.value > eq_anim.prev,
            "EQ target is wider than the Settings panel it replaces"
        );

        // Route stays exclusive with itself across DIFFERENT strips too
        // (the Stack overlay only positions one popover) — but doesn't
        // touch `a`'s still-open EQ.
        let _ = update(&mut state, Message::ToggleFlyout(a, strip::FlyoutKind::Route));
        let _ = update(&mut state, Message::ToggleFlyout(b, strip::FlyoutKind::Route));
        assert!(
            !state.flyout_open.contains(&(a, strip::FlyoutKind::Route)),
            "opening b's Route flyout must close a's"
        );
        assert!(state.flyout_open.contains(&(b, strip::FlyoutKind::Route)));
        assert!(
            state.flyout_open.contains(&(a, strip::FlyoutKind::Eq)),
            "a's EQ flyout must survive b's Route opening"
        );
    }

    #[test]
    fn channels_held_by_the_mute_master_show_as_armed() {
        let mut state = new(true, None, None);
        let a = ChannelId::Input(0);
        let other = ChannelId::Playback(0);
        let _ = update(&mut state, Message::Mute(a, true));
        assert!(!mute_armed(&state, a), "muted, not armed");
        let _ = update(&mut state, Message::GlobalMuteToggle);
        assert!(mute_armed(&state, a), "held off by the master");
        assert!(!mute_armed(&state, other), "never muted, never armed");
        // A fresh mute makes the master's next press clear that one, so the
        // old set is no longer shown as coming back.
        let _ = update(&mut state, Message::Mute(other, true));
        assert!(!mute_armed(&state, a));
    }

    #[test]
    fn global_mute_master_does_nothing_with_nothing_to_restore() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::GlobalMuteToggle);
        assert!(!any_channel_muted(&state), "it never mutes everything");
    }

    #[test]
    fn global_solo_toggle_clears_active_solos_without_touching_mute() {
        let mut state = new(true, None, None);
        let a = ChannelId::Input(0);
        let b = ChannelId::Input(4);
        let _ = update(&mut state, Message::Solo(a, true));
        let _ = update(&mut state, Message::Solo(b, true));
        let _ = update(&mut state, Message::Mute(a, true));
        assert!(any_channel_soloed(&state));

        let _ = update(&mut state, Message::GlobalSoloToggle);
        assert!(!any_channel_soloed(&state));
        assert!(channel_is_muted(&state, a), "solo-clear must not touch mute state");
    }

    #[test]
    fn global_solo_toggle_restores_the_previously_cleared_solos() {
        let mut state = new(true, None, None);
        let a = ChannelId::Input(0);
        let b = ChannelId::Input(4);
        let _ = update(&mut state, Message::Solo(a, true));
        let _ = update(&mut state, Message::Solo(b, true));

        let _ = update(&mut state, Message::GlobalSoloToggle);
        assert!(!any_channel_soloed(&state), "first press clears");

        let _ = update(&mut state, Message::GlobalSoloToggle);
        assert!(channel_is_soloed(&state, a), "second press restores a");
        assert!(channel_is_soloed(&state, b), "second press restores b");
    }

    #[test]
    fn global_solo_toggle_soloing_a_new_channel_after_a_clear_replaces_the_restore_set() {
        let mut state = new(true, None, None);
        // Input(0) and Input(8) are each half of a hardware-linked pair
        // by default (see `mock.rs::test_input_pair_linked_by_default`),
        // so soloing either one mirrors onto its sibling too — accounted
        // for below rather than fought.
        let a = ChannelId::Input(0);
        let c = ChannelId::Input(8);
        let _ = update(&mut state, Message::Solo(a, true));
        let _ = update(&mut state, Message::GlobalSoloToggle);
        assert!(!any_channel_soloed(&state));

        // Solo a different channel manually before pressing the global
        // toggle again — the toggle should clear *that* one (and its
        // linked sibling), not try to restore the stale `a`/`Input(1)`
        // pair from before.
        let _ = update(&mut state, Message::Solo(c, true));
        let _ = update(&mut state, Message::GlobalSoloToggle);
        assert!(!any_channel_soloed(&state));
        assert!(state.last_cleared_solos.contains(&c));
        assert!(!state.last_cleared_solos.contains(&a));
        assert!(!state.last_cleared_solos.contains(&ChannelId::Input(1)));
    }

    #[test]
    fn global_mute_toggle_is_undoable() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::Mute(ChannelId::Input(0), true));
        let _ = update(&mut state, Message::GlobalMuteToggle);
        assert!(!any_channel_muted(&state));
        let _ = update(&mut state, Message::Undo);
        assert!(channel_is_muted(&state, ChannelId::Input(0)));
    }

    #[test]
    fn a_mix_change_marks_the_recalled_snapshot_modified() {
        let mut state = new(true, None, None);
        state.active_snapshot = Some(1);
        assert!(!state.snapshot_dirty);
        let _ = update(&mut state, Message::VolumeChanged(ChannelId::Input(0), 0, 0.5));
        assert!(state.snapshot_dirty, "a fader drag step is a mix change");
        let _ = update(&mut state, Message::SnapshotClicked(1));
        assert!(!state.snapshot_dirty, "recalling the slot clears it");
        let _ = update(&mut state, Message::ToggleSidebar);
        assert!(!state.snapshot_dirty, "UI-only messages do not count");
    }

    #[test]
    fn renaming_a_strip_sets_then_clears_its_label() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(0);
        let _ = update(&mut state, Message::RenameStart(cid));
        assert_eq!(state.renaming, Some(cid));
        let _ = update(&mut state, Message::RenameChanged("  Vocal  ".into()));
        let _ = update(&mut state, Message::RenameCommit);
        assert_eq!(state.renaming, None);
        assert_eq!(state.labels.get(&cid).map(String::as_str), Some("Vocal"));
        assert!(state.undo_stack.is_empty(), "a label is not part of the mix");

        // Editing starts from the current label; an empty commit restores
        // the hardware name.
        let _ = update(&mut state, Message::RenameStart(cid));
        assert_eq!(state.rename_buf, "Vocal");
        let _ = update(&mut state, Message::RenameChanged(String::new()));
        let _ = update(&mut state, Message::RenameCommit);
        assert!(state.labels.get(&cid).is_none());
    }

    #[test]
    fn a_click_away_accepts_the_typed_name() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(0);
        let _ = update(&mut state, Message::RenameStart(cid));
        // The press that opened the field does not count as a click away.
        let _ = update(&mut state, Message::PointerPressed);
        assert_eq!(state.renaming, Some(cid));
        let _ = update(&mut state, Message::RenameChanged("Kick".into()));
        // A later click: the field reports it lost focus.
        let _ = update(&mut state, Message::FieldFocus(EditField::Rename, cid, false));
        assert_eq!(state.renaming, None);
        assert_eq!(state.labels.get(&cid).map(String::as_str), Some("Kick"));
    }

    #[test]
    fn a_click_inside_the_field_keeps_it_open() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(0);
        let _ = update(&mut state, Message::RenameStart(cid));
        let _ = update(&mut state, Message::FieldFocus(EditField::Rename, cid, true));
        assert_eq!(state.renaming, Some(cid));
    }

    #[test]
    fn a_stale_focus_answer_does_not_commit_a_newer_edit() {
        let mut state = new(true, None, None);
        let (a, b) = (ChannelId::Input(0), ChannelId::Input(2));
        let _ = update(&mut state, Message::RenameStart(a));
        let _ = update(&mut state, Message::RenameChanged("Snare".into()));
        // Opening another name accepts the first one.
        let _ = update(&mut state, Message::RenameStart(b));
        assert_eq!(state.labels.get(&a).map(String::as_str), Some("Snare"));
        assert_eq!(state.renaming, Some(b));
        let _ = update(&mut state, Message::FieldFocus(EditField::Rename, a, false));
        assert_eq!(state.renaming, Some(b), "the answer about `a` is ignored");
    }

    #[test]
    fn a_click_away_applies_a_typed_db_value() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(4);
        let _ = update(&mut state, Message::EditStart(cid, "0.0".into()));
        let _ = update(&mut state, Message::EditChanged("-12".into()));
        let _ = update(&mut state, Message::FieldFocus(EditField::Db, cid, false));
        assert_eq!(state.editing, None);
        let db = 20.0 * state.device.inputs()[4].volumes[state.sel_out].log10();
        assert!((db + 12.0).abs() < 0.1, "{db}");
    }

    #[test]
    fn escape_cancels_a_rename_without_touching_the_label() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Playback(0);
        state.labels.insert(cid, "DAW".into());
        let _ = update(&mut state, Message::RenameStart(cid));
        let _ = update(&mut state, Message::RenameChanged("oops".into()));
        let _ = update(&mut state, Message::EscapePressed);
        assert_eq!(state.renaming, None);
        assert_eq!(state.labels.get(&cid).map(String::as_str), Some("DAW"));
    }

    #[test]
    fn prefs_round_trip_through_the_state() {
        let mut state = new(true, None, None);
        state.view = View::Matrix;
        state.sidebar_open = false;
        state.sidebar_width = 300.0;
        state.row_mode = RowMode::Two;
        state.ui_scale = 1.25;
        state.sidebar_panels_open.groups = false;
        state.show_original_names = true;
        state.labels.insert(ChannelId::Output(2), "Monitors".into());
        let saved = prefs_of(&state);

        let mut fresh = new(true, None, None);
        apply_prefs(&mut fresh, saved.clone());
        assert_eq!(prefs_of(&fresh), saved);
    }

    #[test]
    fn an_older_two_row_choice_survives_the_new_layout_setting() {
        let mut state = new(true, None, None);
        let old: crate::prefs::Prefs = serde_json::from_str(r#"{ "two_row": true }"#).unwrap();
        apply_prefs(&mut state, old);
        assert_eq!(state.row_mode, RowMode::Two);
        let old: crate::prefs::Prefs = serde_json::from_str(r#"{ "two_row": false }"#).unwrap();
        apply_prefs(&mut state, old);
        assert_eq!(state.row_mode, RowMode::Three, "false was only the old default");
        assert!(!serde_json::to_string(&prefs_of(&state)).unwrap().contains("two_row"));
    }

    #[test]
    fn the_fx_column_pushes_until_the_mixer_reaches_its_natural_width() {
        let natural = 900.0;
        let fx = FX_W + theme::SPACE_MD;
        assert_eq!(fx_reserve(natural + fx + 100.0, natural, fx), fx);
        assert_eq!(fx_reserve(natural + fx, natural, fx), fx);
        assert_eq!(fx_reserve(natural + fx - 50.0, natural, fx), fx - 50.0);
        assert_eq!(fx_reserve(natural, natural, fx), 0.0);
        assert_eq!(fx_reserve(natural - 50.0, natural, fx), 0.0);
    }

    #[test]
    fn the_fx_column_uses_the_same_width_tween() {
        let mut state = new(true, None, None);
        state.device = DeviceHandle::open_mock_ucx();
        state.host_scene = Some(state.device.capture_scene());
        let now = Instant::now();
        assert_eq!(fx_slot(&state, now), Some(FX_W * state.ui_scale));

        state.show_fx_panel = false;
        state.fx_anim = Some(strip::CollapseAnim {
            prev: FX_W,
            value: 0.0,
            since: now,
        });
        assert!(fx_slot(&state, now).is_some(), "still visible while closing");
        assert!(
            fx_slot(&state, now + Duration::from_millis(200)).is_none(),
            "gone once the tween settles"
        );
    }

    #[test]
    fn the_fx_column_keeps_the_measured_proportions() {
        // 160 px column at RME scale, mapped at the 90/80 strip ratio.
        assert!((FX_W - 160.0 * 1.125).abs() < 0.01);
        // The inset plus the knob block and the fader column fill the width.
        assert!((theme::SPACE_SM + FX_KNOBS_W + FX_FADER_W - FX_W).abs() < 0.01);
        // Each band is the measured height at the same ratio.
        assert!((FX_HEADER_H - 32.0 * 1.125).abs() < 0.01);
        assert!((FX_TYPE_H - 28.0 * 1.125).abs() < 0.01);
        assert!((FX_REV_CONTROLS_H - 220.0 * 1.125).abs() < 0.01);
        assert!((FX_REV_TAIL_H - 87.0 * 1.125).abs() < 0.2);
        assert!((FX_ECHO_CONTROLS_H - 320.0 * 1.125).abs() < 0.2);
        // 70 px knob pitch.
        assert!((FX_ROW_H - 70.0 * 1.125).abs() < 0.01);
    }

    #[test]
    fn fx_knob_rows_fit_their_control_bands() {
        // Reverb: 3.5 px top pad, three rows.
        assert!(3.5 + 3.0 * FX_ROW_H <= FX_REV_CONTROLS_H + 0.01);
        // Echo: 4 px top pad, four rows and the 24 px high-cut picker row.
        assert!(4.0 + 4.0 * FX_ROW_H + 24.0 <= FX_ECHO_CONTROLS_H + 0.01);
    }

    #[test]
    fn the_fx_panels_build_at_every_zoom() {
        // Construction only: catches a zero divisor (the BPM derivation) or a
        // control with no valid range before the panel is ever shown.
        let state = new(true, None, None);
        for scale in [0.75, 1.0, 1.5] {
            let _ = super::reverb_panel(&state, scale);
            let _ = super::echo_panel(&state, scale);
        }
    }

    #[test]
    fn the_window_size_is_kept_and_written_at_close() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::WindowSize(1500.0, 900.0));
        assert_eq!(state.window_width, 1500.0, "the layout width follows");
        assert!(!state.prefs_dirty, "a resize step does not rewrite the file");
        assert_eq!(prefs_of(&state).window_size, Some((1500.0, 900.0)));
        let _ = update(&mut state, Message::SaveNow);
        assert!(!state.prefs_dirty, "flushed at close");
    }

    #[test]
    fn hand_edited_prefs_are_clamped_to_reachable_values() {
        let mut state = new(true, None, None);
        apply_prefs(
            &mut state,
            crate::prefs::Prefs {
                sidebar_width: Some(10_000.0),
                ui_scale: Some(9.0),
                labels: vec![(ChannelId::Input(0), "   ".into())],
                ..Default::default()
            },
        );
        assert_eq!(state.sidebar_width, sidebar::MAX_WIDTH);
        assert_eq!(state.ui_scale, theme::SCALE_MAX);
        assert!(state.labels.is_empty(), "a blank label is no label");
    }

    #[test]
    fn layout_changes_mark_prefs_dirty_and_mix_changes_do_not() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::Mute(ChannelId::Input(0), true));
        assert!(!state.prefs_dirty);
        let _ = update(&mut state, Message::SetRowMode(RowMode::Two));
        assert!(state.prefs_dirty);
        // No `prefs_path` in tests, so the flush writes nothing.
        let _ = update(&mut state, Message::Tick);
        assert!(!state.prefs_dirty);
    }

    #[test]
    fn a_wheel_spin_or_matrix_drag_is_one_undo_step() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(4);
        let before = state.device.inputs()[4].volumes[0];
        // A burst of `VolumeChanged` with no press: a wheel spin, or a
        // Matrix cell drag.
        for v in [0.9, 0.8, 0.7, 0.6] {
            let _ = update(&mut state, Message::VolumeChanged(cid, 0, v));
        }
        assert_eq!(state.undo_stack.len(), 1, "one step for the whole gesture");
        let _ = update(&mut state, Message::Undo);
        assert!((state.device.inputs()[4].volumes[0] - before).abs() < 1e-6);
    }

    #[test]
    fn matrix_drag_then_toggle_undoes_step_by_step() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(4);
        for v in [0.9, 0.7, 0.5] {
            let _ = update(&mut state, Message::VolumeChanged(cid, 1, v));
        }
        let _ = update(&mut state, Message::RangeCleared(cid));
        let _ = update(&mut state, Message::VolumeChanged(cid, 1, 0.0));
        let _ = update(&mut state, Message::RangeCleared(cid));
        let _ = update(&mut state, Message::Undo);
        assert!((state.device.inputs()[4].volumes[1] - 0.5).abs() < 1e-6);
        let _ = update(&mut state, Message::Undo);
        assert!((state.device.inputs()[4].volumes[1] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn hovering_the_matrix_is_not_an_edit() {
        // It once was: every cell the pointer crossed pushed an empty undo
        // step, so Undo "undid" hovers and seemed to do nothing.
        let mut state = new(true, None, None);
        state.active_snapshot = Some(1);
        for col in 0..4 {
            let _ = update(&mut state, Message::MatrixHover(ChannelId::Input(0), col, true));
            let _ = update(&mut state, Message::MatrixHover(ChannelId::Input(0), col, false));
        }
        assert!(state.undo_stack.is_empty());
        assert!(!state.snapshot_dirty);
        assert!(!state.prefs_dirty);
    }

    #[test]
    fn a_late_exit_does_not_clear_the_next_cells_hover() {
        let mut state = new(true, None, None);
        let a = ChannelId::Input(0);
        let _ = update(&mut state, Message::MatrixHover(a, 0, true));
        let _ = update(&mut state, Message::MatrixHover(a, 1, true));
        let _ = update(&mut state, Message::MatrixHover(a, 0, false));
        assert_eq!(state.matrix_hover, Some((a, 1)));
    }

    #[test]
    fn a_knob_turn_is_one_undo_step() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Playback(0);
        for p in [10, 20, 30, 40, 50] {
            let _ = update(&mut state, Message::PanChanged(cid, 0, p));
        }
        assert_eq!(state.undo_stack.len(), 1);
    }

    #[test]
    fn a_channel_width_turn_is_one_undo_step() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(4);
        let before = state.device.inputs()[4].width;
        for width in [0.9, 0.8, 0.7, 0.6] {
            let _ = update(&mut state, Message::ChannelWidthChanged(cid, width));
        }
        assert_eq!(state.undo_stack.len(), 1, "one step for the whole gesture");
        let _ = update(&mut state, Message::Undo);
        assert!((state.device.inputs()[4].width - before).abs() < 1e-6);
    }

    #[test]
    fn letting_go_ends_the_gesture() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(4);
        let _ = update(&mut state, Message::VolumeChanged(cid, 0, 0.5));
        let _ = update(&mut state, Message::RangeCleared(cid));
        let _ = update(&mut state, Message::VolumeChanged(cid, 0, 0.4));
        assert_eq!(state.undo_stack.len(), 2, "two drags, two steps");
    }

    #[test]
    fn a_different_control_starts_a_new_step() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::VolumeChanged(ChannelId::Input(4), 0, 0.5));
        let _ = update(&mut state, Message::VolumeChanged(ChannelId::Input(5), 0, 0.5));
        let _ = update(&mut state, Message::Mute(ChannelId::Input(0), true));
        assert_eq!(state.undo_stack.len(), 3);
    }

    #[test]
    fn a_simulated_session_keeps_its_snapshots_in_memory() {
        let mut state = new(true, None, None);
        assert_eq!(state.stored_snapshots, [false; 8], "never reads the real slots");
        let cid = ChannelId::Input(4);
        let _ = update(&mut state, Message::SnapshotClicked(3));
        let _ = update(&mut state, Message::VolumeChanged(cid, 0, 0.5));
        let _ = update(&mut state, Message::SnapshotStore);
        assert!(state.stored_snapshots[2]);
        assert!(state.mock_snapshots[2].is_some(), "stored in memory, not on disk");
        let _ = update(&mut state, Message::VolumeChanged(cid, 0, 0.1));
        let _ = update(&mut state, Message::SnapshotClicked(3));
        assert!((state.device.inputs()[4].volumes[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn sample_rates_read_as_people_say_them() {
        assert_eq!(super::rate_label(48_000), "48 kHz");
        assert_eq!(super::rate_label(44_100), "44.1 kHz");
        assert_eq!(super::rate_label(176_400), "176.4 kHz");
        assert_eq!(super::rate_label(192_000), "192 kHz");
    }

    #[test]
    fn a_card_rate_change_is_detected_across_a_closed_stream() {
        assert!(!super::card_rate_changed(None, Some(48_000)));
        assert!(super::card_rate_changed(Some(48_000), Some(44_100)));
        assert!(!super::card_rate_changed(Some(48_000), Some(48_000)));
        assert!(!super::card_rate_changed(Some(48_000), None));
        // The last rate is retained while closed, so the reopen at a new
        // rate is detected.
        assert!(super::card_rate_changed(Some(48_000), Some(44_100)));
    }

    #[test]
    fn the_first_observed_rate_reapplies_if_it_differs_from_the_device_model() {
        // TotalControl opened at 44.1 kHz, then PipeWire opened the card at 48 kHz.
        assert!(super::first_rate_needs_reapply(None, Some(48_000), 44_100));
        assert!(super::rate_needs_mixer_reapply(None, Some(48_000), 44_100));
        // First observation agrees with the model: no reapply yet.
        assert!(!super::first_rate_needs_reapply(None, Some(44_100), 44_100));
        // No open stream: nothing to do.
        assert!(!super::first_rate_needs_reapply(None, None, 44_100));
    }

    #[test]
    fn the_mock_simulates_the_audio_engine() {
        let mut state = new(true, None, None);
        let e = state.engine.expect("the mock has a simulated engine");
        assert_eq!((e.effective_rate(), e.effective_quantum()), (48_000, 256));
        let _ = update(&mut state, Message::EngineRate(96_000));
        let _ = update(&mut state, Message::EngineQuantum(128));
        let e = state.engine.unwrap();
        assert_eq!((e.effective_rate(), e.effective_quantum()), (96_000, 128));
        assert!(state.prefs_dirty, "the choice is remembered");
        assert!(state.undo_stack.is_empty(), "not part of the mix");
        // Back to PipeWire's own values.
        let _ = update(&mut state, Message::EngineQuantum(0));
        assert_eq!(state.engine.unwrap().effective_quantum(), 256);
    }

    #[test]
    fn a_simulated_session_never_changes_the_remembered_engine_choice() {
        // A choice made with `--mock` once landed in the preferences and
        // was forced on the real PipeWire at the next real launch.
        let mut state = new(true, None, None);
        apply_prefs(
            &mut state,
            crate::prefs::Prefs {
                engine_force_quantum: Some(0),
                ..Default::default()
            },
        );
        super::restore_engine(&mut state);
        let _ = update(&mut state, Message::EngineQuantum(128));
        assert_eq!(state.engine.unwrap().effective_quantum(), 128, "simulated");
        assert_eq!(state.engine_force_quantum, 0, "remembered choice untouched");
        assert_eq!(prefs_of(&state).engine_force_quantum, Some(0));
    }

    #[test]
    fn quick_strips_fill_a_tall_view_within_bounds() {
        use super::{quick_fit, QUICK_SCALE_MAX, QUICK_SCALE_MULT};
        assert_eq!(quick_fit(500.0), QUICK_SCALE_MULT, "a short view keeps the floor");
        assert_eq!(quick_fit(5000.0), QUICK_SCALE_MAX, "a huge one stops at the cap");
        let f = quick_fit(1000.0);
        assert!(f > QUICK_SCALE_MULT && f < QUICK_SCALE_MAX, "1080p grows: {f}");
    }

    #[test]
    fn the_initial_zoom_follows_the_monitor_height() {
        use super::initial_zoom_for;
        assert_eq!(initial_zoom_for(1080.0), 1.0);
        assert_eq!(initial_zoom_for(768.0), 1.0);
        assert_eq!(initial_zoom_for(1200.0), 1.1);
        assert_eq!(initial_zoom_for(1440.0), 1.25);
        assert_eq!(initial_zoom_for(2160.0), 1.5);
    }

    #[test]
    fn the_first_launch_picks_a_zoom_from_the_monitor_and_later_ones_keep_the_users() {
        let mut state = new(true, None, None);
        apply_prefs(&mut state, crate::prefs::Prefs::default());
        assert!(state.initial_zoom_pending, "nothing saved: ask the monitor");
        let _ = update(&mut state, Message::MonitorSize(Some(iced::Size::new(2560.0, 1440.0))));
        assert_eq!(state.ui_scale, 1.25);
        assert!(!state.initial_zoom_pending && state.prefs_dirty);
        let _ = update(&mut state, Message::MonitorSize(Some(iced::Size::new(3840.0, 2160.0))));
        assert_eq!(state.ui_scale, 1.25, "asked once only");

        let mut saved = new(true, None, None);
        apply_prefs(&mut saved, crate::prefs::Prefs { ui_scale: Some(1.0), ..Default::default() });
        assert!(!saved.initial_zoom_pending, "a saved zoom is never overridden");
    }

    #[test]
    fn a_narrow_window_never_auto_folds_the_sidebar() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::WindowSize(1400.0, 900.0));
        assert!(state.sidebar_open);
        let _ = update(&mut state, Message::WindowSize(700.0, 900.0));
        assert!(state.sidebar_open, "narrow window keeps the right panel visible");
        let _ = update(&mut state, Message::WindowSize(1400.0, 900.0));
        assert!(state.sidebar_open);
        // The user can still collapse it explicitly; resizing never reopens it.
        let _ = update(&mut state, Message::ToggleSidebar);
        assert!(!state.sidebar_open);
        let _ = update(&mut state, Message::WindowSize(700.0, 900.0));
        let _ = update(&mut state, Message::WindowSize(1400.0, 900.0));
        assert!(!state.sidebar_open, "the user's own fold is respected");
    }

    #[test]
    fn the_sidebar_stops_growing_with_the_zoom() {
        let mut state = new(true, None, None);
        state.ui_scale = 1.75;
        assert_eq!(super::sidebar_scale(&state), 1.25);
        state.ui_scale = 1.0;
        assert_eq!(super::sidebar_scale(&state), 1.0);
    }

    #[test]
    fn the_tick_slows_down_at_rest_and_speeds_up_on_any_activity() {
        use super::{tick_interval, TICK_ACTIVE, TICK_HIDDEN, TICK_IDLE, TICK_METERS};
        let mut state = new(true, None, None);
        state.last_activity = Instant::now() - Duration::from_secs(10);
        assert_eq!(tick_interval(&state), TICK_IDLE, "nothing happened for a while");

        // The user doing anything wakes it, even a message that changes
        // nothing visible.  With no drag or animation that is the meter
        // rate, not the interaction rate.
        let _ = update(&mut state, Message::ToggleSidebarPanel(sidebar::Panel::Groups));
        assert_eq!(tick_interval(&state), TICK_METERS);

        // Background polls do not count as activity.
        state.last_activity = Instant::now() - Duration::from_secs(10);
        let _ = update(&mut state, Message::EnginePoll);
        let _ = update(&mut state, Message::EngineRead(None, None, false, None));
        assert_eq!(tick_interval(&state), TICK_IDLE);

        // A pending window resize and a running collapse animation keep it
        // at the interaction rate.
        state.pending_resize_width = Some(1200.0);
        assert_eq!(tick_interval(&state), TICK_ACTIVE);
        state.pending_resize_width = None;
        state.drag_range = Some((ChannelId::Input(0), 0.0, 1.0));
        assert_eq!(tick_interval(&state), TICK_ACTIVE);

        // A hidden tray window only needs housekeeping.
        state.drag_range = None;
        state.window_hidden = true;
        assert_eq!(tick_interval(&state), TICK_HIDDEN);
    }

    #[test]
    fn live_meters_do_not_force_the_view_tick() {
        // The mock generates levels, so a Tick sees a live signal.  Meter
        // keyframes are interpolated by the fader canvases; they must not
        // wake the whole-view rebuild.
        use super::{tick_interval, TICK_IDLE};
        let mut state = new(true, None, None);
        state.last_activity = Instant::now() - Duration::from_secs(10);
        let before = state.last_activity;
        let _ = update(&mut state, Message::Tick);
        assert_eq!(
            state.last_activity, before,
            "meter motion belongs to the canvas, not to the view tick"
        );
        assert_eq!(tick_interval(&state), TICK_IDLE);
    }

    #[test]
    fn meter_ballistics_do_not_depend_on_the_tick_rate() {
        // One second of a held signal then a second of silence, stepped at
        // three different tick rates: the meter ends in the same place.
        let run = |dt_ms: f32| {
            let mut m = MeterAnim::new();
            let n = (1000.0 / dt_ms) as usize;
            for _ in 0..n {
                m.step_for(1.0, dt_ms);
            }
            let held = m.value;
            for _ in 0..n {
                m.step_for(0.0, dt_ms);
            }
            (held, m.value)
        };
        let (h50, r50) = run(50.0);
        // The rates that exist: 30 Hz with a signal, 20 Hz as it was, and 10 Hz
        // for a slow machine; not 4 Hz, where the changing release curve is
        // sampled too coarsely to compare (and meters are not decaying then).
        for dt in [33.0, 100.0] {
            let (h, r) = run(dt);
            assert!((h - h50).abs() < 0.02, "attack at {dt} ms: {h} vs {h50}");
            assert!((r - r50).abs() < 0.03, "release at {dt} ms: {r} vs {r50}");
        }
    }

    #[test]
    fn the_card_line_shows_clock_rate_and_buffer() {
        let mut state = new(true, None, None);
        assert_eq!(
            crate::sidebar::clock_line(&state, "Internal"),
            "Internal, 48 kHz, 256"
        );
        state.card_stream = Some(totalcontrol_core::pw_clock::CardStream {
            rate: 44_100,
            period: 512,
            buffer: 1536,
        });
        assert_eq!(
            crate::sidebar::clock_line(&state, "Internal"),
            "Internal, 44.1 kHz, 512"
        );
    }

    #[test]
    fn dim_toggle_flips_the_device_dim() {
        let mut state = new(true, None, None);
        assert!(!state.device.settings().dim);
        let _ = update(&mut state, Message::DimToggle);
        assert!(state.device.settings().dim);
        let _ = update(&mut state, Message::DimToggle);
        assert!(!state.device.settings().dim);
        assert!(state.undo_stack.is_empty(), "Dim is not part of a scene");
    }

    #[test]
    fn about_modal_messages_are_not_undoable() {
        // Both are pure UI, so neither may push an undo step. Checked through
        // the predicate rather than by dispatching, because dispatching
        // `OpenRepoUrl` would launch the machine's browser.
        assert!(!is_undoable(&Message::ToggleAbout));
        assert!(!is_undoable(&Message::OpenRepoUrl));

        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::ToggleAbout);
        assert!(state.show_about);
        assert!(state.undo_stack.is_empty());
    }

    #[test]
    fn escape_closes_the_about_modal_before_an_edit() {
        // The modal is the topmost thing on screen, so Escape must dismiss
        // it without also abandoning the inline edit underneath it.
        let mut state = new(true, None, None);
        state.show_about = true;
        state.editing = Some(ChannelId::Input(0));
        let _ = update(&mut state, Message::EscapePressed);
        assert!(!state.show_about);
        assert_eq!(state.editing, Some(ChannelId::Input(0)));

        let _ = update(&mut state, Message::EscapePressed);
        assert_eq!(state.editing, None);
    }

    #[test]
    fn escape_closes_the_device_popover_after_the_modal() {
        let mut state = new(true, None, None);
        state.show_about = true;
        state.show_device_panel = true;
        let _ = update(&mut state, Message::EscapePressed);
        assert!(!state.show_about);
        assert!(state.show_device_panel);
        let _ = update(&mut state, Message::EscapePressed);
        assert!(!state.show_device_panel);
    }

    #[test]
    fn layout_clicked_recalls_the_stored_collapsed_set() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(0);
        state.layouts[0] = Some(HashSet::from([cid]));
        let _ = update(&mut state, Message::LayoutClicked(1));
        assert_eq!(state.active_layout, Some(1));
        assert!(state.collapsed.contains(&cid));
    }

    /// Hardware probe for the UC live-rate path.  Ignored by default.
    /// Run with TotalControl stopped and music playing:
    /// `cargo test -p totalcontrol-gui live_rate_switch_probe -- --ignored --nocapture`
    /// It enables Phones loopback, captures the RME input before and after a
    /// 48 → 44.1 kHz switch, and prints the per-channel peak.  A non-zero
    /// post-switch peak means the UC is still producing audio.
    #[test]
    #[ignore]
    fn live_rate_switch_probe() {
        use std::process::Command;

        fn peaks(_rate: u32) -> Vec<f32> {
            let src = Command::new("pactl")
                .args(["list", "short", "sources"])
                .output()
                .ok()
                .and_then(|out| {
                    String::from_utf8_lossy(&out.stdout).lines().find_map(|line| {
                        if line.contains("RME") && line.contains("pro-input-0") {
                            line.split_whitespace().next().map(str::to_owned)
                        } else {
                            None
                        }
                    })
                });
            let Some(src) = src else { return vec![] };
            let out = Command::new("timeout")
                .args([
                    "2",
                    "pw-record",
                    "--target",
                    &src,
                    "--format",
                    "s32",
                    "--channels",
                    "18",
                    "--raw",
                    "-",
                ])
                .output();
            let Ok(out) = out else { return vec![] };
            let mut peaks = vec![0.0f32; 18];
            for frame in out.stdout.chunks_exact(18 * 4) {
                for (ch, bytes) in frame.chunks_exact(4).enumerate() {
                    let sample = i32::from_le_bytes(bytes.try_into().unwrap()).unsigned_abs()
                        as f32
                        / 2_147_483_648.0;
                    if sample > peaks[ch] {
                        peaks[ch] = sample;
                    }
                }
            }
            peaks
        }

        let mut state = new_with_device(false, None, None, None, None);
        apply_startup_sync(&mut state);

        // Park the existing player if the caller supplied it, then start a
        // known tone after the scene is in the UC shadow.  The tone starts
        // the engine and latches the scene.
        if let (Ok(stream), Some(sink)) = (
            std::env::var("TOTALCONTROL_PROBE_STREAM"),
            totalcontrol_core::pw_clock::fireface_sink_name(),
        ) {
            let _ = Command::new("pactl")
                .args(["move-sink-input", &stream, &sink])
                .status();
            std::thread::sleep(Duration::from_secs(1));
        }
        let sink_idx = Command::new("pactl")
            .args(["list", "short", "sinks"])
            .output()
            .ok()
            .and_then(|out| {
                String::from_utf8_lossy(&out.stdout).lines().find_map(|line| {
                    if line.contains("RME") && line.contains("pro-output-0") {
                        line.split_whitespace().next().map(str::to_owned)
                    } else {
                        None
                    }
                })
            });
        let mut tone = if std::env::var_os("TOTALCONTROL_PROBE_PLAYER").is_some() {
            None
        } else {
            sink_idx.and_then(|idx| {
                Command::new("pw-play")
                    .args([
                        "--target",
                        &idx,
                        "--rate=48000",
                        "--channels=2",
                        "/tmp/opencode/tone.wav",
                    ])
                    .spawn()
                    .ok()
            })
        };
        std::thread::sleep(Duration::from_secs(1));

        // The cable is now Phones → input 5.  Set the test fader to 0 dB
        // after the tone has started; the probe scene is not saved.
        state
            .device
            .set_volume(ChannelId::Output(0), 0, 1.0)
            .expect("Phones left volume");
        state
            .device
            .set_volume(ChannelId::Output(1), 0, 1.0)
            .expect("Phones right volume");

        let before_rate = state
            .card_stream
            .map(|s| s.rate)
            .or_else(|| state.engine.map(|e| e.effective_rate()))
            .filter(|r| *r > 0)
            .unwrap_or(48_000);
        println!("probe: before card rate {before_rate}");
        println!("probe: before peaks {:?}", peaks(before_rate));

        if std::env::var_os("TOTALCONTROL_PROBE_SCENE_ONLY").is_some() {
            // The caller has parked the player so the UC engine is idle.
            // Re-write the scene with the test Phones level; the next engine
            // start (player return) latches it.
            let scene = state.device.capture_scene();
            if let Err(e) = state.device.apply_scene(&scene) {
                log::warn!("probe scene reapply failed: {e:?}");
            }
            println!("probe: scene-only mode, skipping rate change");
        } else {
            let _ = update(&mut state, Message::EngineRate(44_100));
            std::thread::sleep(Duration::from_secs(2));
            let _ = update(&mut state, Message::Tick);
        }

        let after_rate = totalcontrol_core::pw_clock::card_stream(state.device.card_id())
            .map(|s| s.rate)
            .or_else(|| state.engine.map(|e| e.effective_rate()))
            .unwrap_or(0);
        println!("probe: after card rate {after_rate}");
        println!("probe: after peaks {:?}", peaks(after_rate));
        if let Some(child) = tone.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    // A click anywhere on a channel selects it; on a hardware output it
    // also shows that output's submix — including clicks on the strip's
    // own controls, which used to consume the press without selecting.
    #[test]
    fn a_trim_lowers_the_shown_fader_and_keeps_the_stored_send() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(4);
        state.device.set_volume(cid, 0, 0.5).unwrap();
        let _ = update(&mut state, Message::TrimChanged(cid, -6.0));
        // Stored send unchanged …
        let raw = state.device.volume(cid, 0).unwrap();
        assert!((raw - 0.5).abs() < 1e-6, "the send itself must not move");
        // … while the level shown next to the fader is 6 dB lower.
        let shown = raw * trim_factor(channel_trim(&state, cid));
        assert!((shown - 0.5 * 0.501_187).abs() < 1e-3, "shown {shown}");
    }

    #[test]
    fn trim_scales_the_meter_reading() {
        let levels = trim_meter_levels(&[1.0, 0.5], &[-6.0, 0.0]);
        assert!((levels[0] - 0.501_187).abs() < 1e-5);
        assert!((levels[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_fader_move_with_trim_active_writes_the_gain_shown() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(4);
        let _ = update(&mut state, Message::TrimChanged(cid, -6.0));
        let _ = update(&mut state, Message::VolumeChanged(cid, 0, 0.5));
        let raw = state.device.volume(cid, 0).unwrap();
        let shown = raw * trim_factor(channel_trim(&state, cid));
        assert!((shown - 0.5).abs() < 1e-3, "shown {shown}");
    }

    #[test]
    fn trim_and_zero_restores_the_exact_send() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Input(4);
        state.device.set_volume(cid, 1, 0.3).unwrap();
        let before = state.device.volume(cid, 1).unwrap();
        let _ = update(&mut state, Message::TrimChanged(cid, -12.0));
        let _ = update(&mut state, Message::TrimReset(cid));
        assert_eq!(state.device.volume(cid, 1).unwrap(), before);
    }

    #[test]
    fn a_linked_pair_shares_one_trim() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::TrimChanged(ChannelId::Input(0), -6.0));
        assert_eq!(state.device.inputs()[0].trim, -6.0);
        assert_eq!(state.device.inputs()[1].trim, -6.0, "linked partner");
    }

    #[test]
    fn an_unlinked_pair_keeps_separate_trims() {
        let mut state = new(true, None, None);
        let _ = update(
            &mut state,
            Message::StereoLinkChanged(ChannelId::Input(0), false),
        );
        let _ = update(&mut state, Message::TrimChanged(ChannelId::Input(0), -6.0));
        assert_eq!(state.device.inputs()[0].trim, -6.0);
        assert_eq!(state.device.inputs()[1].trim, 0.0);
    }

    #[test]
    fn playback_trim_works_and_survives_osc() {
        let mut state = new(true, None, None);
        let cid = ChannelId::Playback(1);
        let _ = update(&mut state, Message::TrimChanged(cid, -6.0));
        assert_eq!(state.device.playbacks()[1].trim, -6.0);
    }

    #[test]
    fn trim_clamps_to_attenuation_only() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::TrimChanged(ChannelId::Input(4), 6.0));
        assert_eq!(state.device.inputs()[4].trim, 0.0, "no boost");
        let _ = update(&mut state, Message::TrimChanged(ChannelId::Input(4), -99.0));
        assert_eq!(state.device.inputs()[4].trim, -65.0, "floor");
    }

    #[test]
    fn a_plain_click_selects_the_strip_and_shows_its_submix() {
        let mut state = new(true, None, None);
        state.sel_out = 0;
        let out = state.device.outputs().len() - 1;
        let cid = ChannelId::Output(out);
        let _ = update(&mut state, Message::StripClicked(cid));
        assert!(state.selected.contains(&cid));
        assert_eq!(state.focus_channel, Some(cid));
        assert_eq!(
            state.sel_out,
            if state.device.outputs_one_per_pair() { out } else { out / 2 }
        );
    }

    #[test]
    fn a_strip_control_click_selects_the_strip_and_shows_its_submix() {
        let mut state = new(true, None, None);
        state.sel_out = 0;
        let out = state.device.outputs().len() - 1;
        let cid = ChannelId::Output(out);
        let _ = update(&mut state, Message::ToggleFlyout(cid, strip::FlyoutKind::Settings));
        assert_eq!(state.focus_channel, Some(cid));
        assert!(state.selected.contains(&cid));
        assert_eq!(
            state.sel_out,
            if state.device.outputs_one_per_pair() { out } else { out / 2 }
        );
    }

    #[test]
    fn a_strip_control_click_on_an_input_keeps_the_submix() {
        let mut state = new(true, None, None);
        state.sel_out = 2;
        let cid = ChannelId::Input(1);
        let _ = update(&mut state, Message::Mute(cid, true));
        assert!(state.selected.contains(&cid));
        assert_eq!(state.sel_out, 2);
    }

    #[test]
    fn a_stale_output_id_from_the_old_topology_cannot_panic_the_strip_renderer() {
        let state = new(true, None, None);
        assert_eq!(state.device.outputs().len(), 12);
        let p = strip_params(&state, ChannelId::Output(12), state.sel_out);
        assert!(p.name.is_empty());
    }

    #[test]
    fn selecting_a_missing_submix_is_clamped_to_the_current_device() {
        let mut state = new(true, None, None);
        let _ = update(&mut state, Message::SelectOutput(12));
        assert_eq!(
            state.sel_out,
            state.device.output_pair_count().saturating_sub(1)
        );
    }

    #[test]
    fn switching_devices_drops_channel_ids_from_the_previous_topology() {
        let mut state = new(true, None, None);
        super::switch_device(&mut state, DeviceTarget::kind_only(DeviceKind::MockUcx))
            .expect("switch to the UCX simulation");

        let stale = ChannelId::Output(12);
        state.selected.insert(stale);
        state.focus_channel = Some(stale);
        state.select_anchor = Some(stale);
        state.hovered_strip = Some(stale);
        state.matrix_hover = Some((stale, 0));
        state.pending_flyout = Some((stale, strip::FlyoutKind::Settings));
        state
            .flyout_open
            .insert((stale, strip::FlyoutKind::Settings));

        super::switch_device(&mut state, DeviceTarget::kind_only(DeviceKind::Mock))
            .expect("switch back to the Babyface simulation");

        assert!(state.selected.is_empty());
        assert_eq!(state.focus_channel, None);
        assert_eq!(state.select_anchor, None);
        assert_eq!(state.hovered_strip, None);
        assert_eq!(state.matrix_hover, None);
        assert_eq!(state.pending_flyout, None);
        assert!(state.flyout_open.is_empty());
        assert!(state.sel_out < state.device.output_pair_count());
        let _ = super::quick_view(&state);
    }

    #[test]
    fn a_strip_control_click_on_a_selected_strip_keeps_the_group() {
        let mut state = new(true, None, None);
        let a = ChannelId::Input(0);
        let b = ChannelId::Input(4);
        state.selected.insert(a);
        state.selected.insert(b);
        let _ = update(&mut state, Message::Solo(a, true));
        assert!(state.selected.contains(&a) && state.selected.contains(&b));
    }

    fn apply_global(state: &mut TotalControl, command: crate::osc_global::GlobalCommand) {
        for message in global_messages(state, &command) {
            let _ = update(state, message);
        }
    }

    #[test]
    fn global_osc_channel_mute_reaches_the_mock() {
        let mut state = new(true, None, None);
        state.device = DeviceHandle::open_mock_ucx();
        apply_global(
            &mut state,
            crate::osc_global::GlobalCommand::Channel {
                bus: crate::osc_global::Bus::Input,
                channel: 0,
                control: crate::osc_global::ChannelControl::Mute(true),
            },
        );
        assert!(state.device.inputs()[0].mute);
        apply_global(
            &mut state,
            crate::osc_global::GlobalCommand::Channel {
                bus: crate::osc_global::Bus::Input,
                channel: 0,
                control: crate::osc_global::ChannelControl::Mute(false),
            },
        );
        assert!(!state.device.inputs()[0].mute);
    }

    #[test]
    fn global_osc_output_volume_is_native_db() {
        let mut state = new(true, None, None);
        state.device = DeviceHandle::open_mock_ucx();
        apply_global(
            &mut state,
            crate::osc_global::GlobalCommand::Channel {
                bus: crate::osc_global::Bus::Output,
                channel: 0,
                control: crate::osc_global::ChannelControl::Volume(-6.0),
            },
        );
        let volume = state.device.outputs()[0].volume;
        assert!((volume - 0.501).abs() < 0.01, "expected -6 dB, got {volume}");
    }

    #[test]
    fn global_osc_mix_fader_reaches_the_send() {
        let mut state = new(true, None, None);
        state.device = DeviceHandle::open_mock_ucx();
        apply_global(
            &mut state,
            crate::osc_global::GlobalCommand::Mix {
                source: crate::osc_global::Bus::Input,
                channel: 1,
                submix: 0,
                control: crate::osc_global::MixControl::Fader(-12.0),
            },
        );
        let volume = state.device.inputs()[1].volumes[0];
        assert!((volume - 0.251).abs() < 0.01, "expected -12 dB, got {volume}");
    }

    #[test]
    fn global_osc_control_room_changes_the_mock() {
        let mut state = new(true, None, None);
        state.device = DeviceHandle::open_mock_ucx();
        apply_global(
            &mut state,
            crate::osc_global::GlobalCommand::ControlRoom(
                crate::osc_global::ControlRoomControl::Dim(true),
            ),
        );
        assert!(state.device.settings().dim);
        apply_global(
            &mut state,
            crate::osc_global::GlobalCommand::ControlRoom(
                crate::osc_global::ControlRoomControl::MainMono(true),
            ),
        );
        assert!(state.device.settings().mono);
    }

    #[test]
    fn global_typed_feedback_is_not_echoed_twice() {
        use crate::osc_global::{Bus, ChannelControl, GlobalCommand, MixControl};
        assert!(global_feedback_is_typed(&GlobalCommand::Channel {
            bus: Bus::Output,
            channel: 0,
            control: ChannelControl::Volume(-6.0),
        }));
        assert!(!global_feedback_is_typed(&GlobalCommand::Channel {
            bus: Bus::Input,
            channel: 0,
            control: ChannelControl::Volume(-6.0),
        }));
        assert!(global_feedback_is_typed(&GlobalCommand::Mix {
            source: Bus::Input,
            channel: 0,
            submix: 0,
            control: MixControl::Fader(-6.0),
        }));
        assert!(!global_feedback_is_typed(&GlobalCommand::ControlRoom(
            crate::osc_global::ControlRoomControl::Dim(true)
        )));
    }

    #[test]
    fn global_state_queries_cover_channels_matrix_and_status() {
        use crate::osc_global::{GlobalCommand, SendControl};
        let mut state = new(true, None, None);
        state.device = DeviceHandle::open_mock_ucx();

        let channel = global_state_commands(
            &state,
            &SendControl::SendChannel {
                bus: crate::osc_global::Bus::Input,
                channel: 0,
            },
        );
        assert!(channel.iter().any(|c| matches!(
            c,
            GlobalCommand::Channel {
                bus: crate::osc_global::Bus::Input,
                channel: 0,
                control: crate::osc_global::ChannelControl::Mute(_)
            }
        )));
        assert!(channel.iter().any(|c| matches!(
            c,
            GlobalCommand::Channel {
                control: crate::osc_global::ChannelControl::Name(_),
                ..
            }
        )));

        let state_messages = global_state_commands(&state, &SendControl::SendState);
        assert!(state_messages.iter().any(|c| matches!(
            c,
            GlobalCommand::Status(crate::osc_global::StatusControl::Device(_))
        )));
        assert_eq!(
            state_messages
                .iter()
                .filter(|c| matches!(c, GlobalCommand::SnapshotLoad { .. }))
                .count(),
            8
        );

        let all_mix = global_state_commands(&state, &SendControl::SendMix(1));
        let one_submix = global_state_commands(
            &state,
            &SendControl::SendSubmix {
                submix: 0,
                mode: 1,
            },
        );
        assert!(!one_submix.is_empty());
        assert!(all_mix.len() > one_submix.len());
        assert!(one_submix.iter().all(|c| matches!(
            c,
            GlobalCommand::Mix { submix: 0, .. }
        )));

        let all = global_state_commands(&state, &SendControl::SendAll(2));
        assert!(all.len() > all_mix.len());
        assert!(all.iter().any(|c| matches!(
            c,
            GlobalCommand::Channel {
                bus: crate::osc_global::Bus::Output,
                ..
            }
        )));
    }

    #[test]
    fn global_feedback_uses_native_paths_and_values() {
        use crate::osc_global::{Bus, ChannelControl, GlobalCommand, MixControl};
        let volume = global_feedback(&OscOutbound::Volume(
            ChannelId::Input(2),
            4,
            0.25118864,
        ));
        assert!(matches!(
            volume,
            Some(GlobalCommand::Mix {
                source: Bus::Input,
                channel: 2,
                submix: 8,
                control: MixControl::Fader(db),
            }) if (db + 12.0).abs() < 0.01
        ));
        let pan = global_feedback(&OscOutbound::Pan(ChannelId::Output(0), 0, -50));
        assert!(matches!(
            pan,
            Some(GlobalCommand::Channel {
                bus: Bus::Output,
                channel: 0,
                control: ChannelControl::Balance(value),
            }) if (value + 0.5).abs() < 0.001
        ));
    }

    #[test]
    fn global_osc_mode_is_persisted() {
        let mut state = new(true, None, None);
        let _ = update(
            &mut state,
            Message::SetOscMode(crate::osc::OscDialect::Global),
        );
        assert_eq!(state.osc_dialect, crate::osc::OscDialect::Global);
        assert_eq!(prefs_of(&state).osc_mode, crate::osc::OscDialect::Global);
        state.osc_enabled = true;
        let _ = update(
            &mut state,
            Message::SetOscMode(crate::osc::OscDialect::Classic),
        );
        assert_eq!(
            state.osc_config.map(|c| c.dialect),
            Some(crate::osc::OscDialect::Classic)
        );
    }

    #[test]
    fn linked_gain_knob_always_shows_both_values() {
        assert_eq!(input_gain_knob_label(40, None, false, true), "40\n40");
        assert_eq!(input_gain_knob_label(40, Some(40), false, true), "40\n40");
        assert_eq!(input_gain_knob_label(40, Some(35), false, true), "40\n35");
        assert_eq!(input_gain_knob_label(7, Some(5), true, true), "3.5\n2.5");
        assert_eq!(input_gain_knob_label(40, Some(35), false, false), "40");
    }

    #[test]
    fn linked_gain_moves_both_preamps_by_the_same_step() {
        let mut state = new(true, None, None);
        state.device.set_input_pair_linked(0, true).unwrap();
        state.device.set_input_gain(0, 10).unwrap();
        state.device.set_input_gain_right(0, 20).unwrap();

        let _ = update(&mut state, Message::Gain(0, 15));

        assert_eq!(state.device.inputs()[0].gain, Some(15));
        assert_eq!(
            state.device.inputs()[0].gain_right,
            Some(25),
            "the right preamp must follow the linked left control by the same step"
        );
    }

    #[test]
    fn unlinked_gain_leaves_the_right_preamp_alone() {
        let mut state = new(true, None, None);
        state.device.set_input_pair_linked(0, false).unwrap();
        state.device.set_input_gain(0, 10).unwrap();
        state.device.set_input_gain_right(0, 20).unwrap();

        let _ = update(&mut state, Message::Gain(0, 15));

        assert_eq!(state.device.inputs()[0].gain, Some(15));
        assert_eq!(state.device.inputs()[0].gain_right, Some(20));
    }

    #[test]
    fn use_device_applies_the_full_decoded_scene() {
        let mut state = new(true, None, None);
        let mut saved = state.device.capture_scene();
        saved.outputs[0].volume = 0.25;
        let mut device = saved.clone();
        device.outputs[0].volume = 0.5;
        state.startup_mismatch = Some(StartupMismatch {
            items: vec!["output 0 volume".into()],
            device_state: Default::default(),
            totalcontrol_scene: saved,
            totalcontrol_gain: Vec::new(),
            ucx_dump: Some(UcxDump::new(HashMap::new())),
            device_scene: Some(device),
        });
        let _ = update(&mut state, Message::MismatchUseDevice);
        assert!(state.startup_mismatch.is_none());
        assert!((state.device.outputs()[0].volume - 0.5).abs() < 1e-6);
    }

    #[test]
    fn use_totalcontrol_applies_the_saved_scene() {
        let mut state = new(true, None, None);
        let mut saved = state.device.capture_scene();
        saved.outputs[0].volume = 0.25;
        let mut device = saved.clone();
        device.outputs[0].volume = 0.5;
        state.startup_mismatch = Some(StartupMismatch {
            items: vec!["output 0 volume".into()],
            device_state: Default::default(),
            totalcontrol_scene: saved,
            totalcontrol_gain: Vec::new(),
            ucx_dump: Some(UcxDump::new(HashMap::new())),
            device_scene: Some(device),
        });
        let _ = update(&mut state, Message::MismatchUseTotalcontrol);
        assert!(state.startup_mismatch.is_none());
        assert!((state.device.outputs()[0].volume - 0.25).abs() < 1e-6);
    }

}


impl Drop for TotalControl {
    fn drop(&mut self) {
        stop_surround_bridge(self);
    }
}
