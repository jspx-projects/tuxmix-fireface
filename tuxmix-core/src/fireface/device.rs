//! Fireface UC/UCX userspace device model over the kernel hwdep transport.
//!
//! This is the replacement for the old `ucx_alsa` backend: the kernel no
//! longer exposes device-model ALSA controls, so all routing, fader, preamp,
//! meter and scene logic lives here.
//!
//! Both models share the TuxMix [`RmeDevice`] surface, but their protocol
//! paths are kept separate:
//!
//! - **UC**: EP0 vendor requests, 14-bit linear matrix gains, 6-bit output
//!   attenuator, no EQ/dynamics.
//! - **UCX**: IF1 register words, 15-bit signed matrix coefficients, channel
//!   DSP settings, 244-byte meter packets.
//!
//! Uncertain or undecoded behaviour is marked `TODO`/`Uncertain` in the code
//! and collected in `docs/PROTOCOL-COMPLETE.md`.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::channel::{
    ChannelId, ChannelType, Dynamics, Echo, EqBandType, InputChannel, InputEq, InputLevel,
    OutputChannel, PlaybackChannel, Reverb, Sensitivity,
};
use crate::device::{DeviceSettings, RmeDevice};
use crate::error::Error;
use crate::scene::{MatrixOverride, Scene};

use super::abi::*;
use super::hwdep::{self, Direction, Event, Hwdep};
use super::protocol::{self, uc, ucx, MeterKind, MeterPacket, Source};
use super::settings::ChannelSettings;

const PAIRS: usize = 9;
const BLOCKS: usize = 18;
/// Minimum gap between UCX IF1 register writes.  Without it, a full scene
/// restore can fill the device's receive FIFO and `usb_bulk_msg` reports
/// ETIMEDOUT even though the endpoint itself is healthy.  The original
/// 1 ms was enough for input-only scenes; writing the 18 output EQ blocks
/// as well doubles the burst count and needs more headroom.
const UCX_WRITE_GAP: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Model {
    Uc,
    Ucx,
}

impl Model {
    pub fn name(self) -> &'static str {
        super::models::for_model(self).name
    }

    fn from_abi(model: u32) -> Option<Self> {
        match model {
            MODEL_UC => Some(Model::Uc),
            MODEL_UCX => Some(Model::Ucx),
            _ => None,
        }
    }
}

/// Identity and capabilities reported by the kernel hwdep node.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub card: i32,
    pub model: Model,
    pub iso: bool,
    pub caps: u32,
    pub channels: usize,
    pub inputs: usize,
    pub outputs: usize,
    pub playback: usize,
    pub matrix_sources: usize,
    pub meter_bytes: usize,
    pub state_ep: u32,
    pub meter_ep: u32,
    pub status_ep: u32,
    pub rate: u32,
    pub firmware: u32,
    pub name: String,
    pub serial: String,
}

impl DeviceInfo {
    fn from_abi(card: i32, info: HwdepInfo) -> Option<Self> {
        let model = Model::from_abi(info.model)?;
        Some(Self {
            card,
            model,
            iso: info.transport == XPORT_ISO,
            caps: info.caps,
            channels: info.channels as usize,
            inputs: info.inputs as usize,
            outputs: info.outputs as usize,
            playback: info.playback as usize,
            matrix_sources: info.matrix_sources as usize,
            meter_bytes: info.meter_bytes as usize,
            state_ep: info.state_ep,
            meter_ep: info.meter_ep,
            status_ep: info.status_ep,
            rate: info.rate,
            firmware: info.firmware,
            name: info.name_str(),
            serial: info.serial_str(),
        })
    }
}

/// Stable identity of one physical Fireface unit.  The card index changes
/// across reconnects and personality switches; the serial does not.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeviceIdentity {
    pub model: Model,
    pub serial: String,
}

impl DeviceInfo {
    pub fn identity(&self) -> DeviceIdentity {
        DeviceIdentity {
            model: self.model,
            serial: self.serial.clone(),
        }
    }
}

/// The raw transport a [`Fireface`] drives.  Kept as a trait so the device
/// model can be tested without hardware and so a future direct-libusb
/// transport can plug in without duplicating the model.
pub trait ControlTransport: Send {
    fn info(&self) -> &DeviceInfo;
    fn vendor(
        &self,
        request: u8,
        direction: Direction,
        value: u16,
        index: u16,
        data: &[u8],
    ) -> Result<Vec<u8>, Error>;
    fn write_words(&self, words: &[(u16, u16)]) -> Result<(), Error>;
    fn wait(&self, timeout: Duration) -> Result<bool, Error>;
    fn read_event(&self) -> Result<Event, Error>;
    fn is_connected(&self) -> bool;
}

struct HwdepTransport {
    hw: Hwdep,
    info: DeviceInfo,
}

impl HwdepTransport {
    fn new(hw: Hwdep, info: DeviceInfo) -> Self {
        Self { hw, info }
    }
}

impl ControlTransport for HwdepTransport {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn vendor(
        &self,
        request: u8,
        direction: Direction,
        value: u16,
        index: u16,
        data: &[u8],
    ) -> Result<Vec<u8>, Error> {
        Ok(self.hw.vendor(request, direction, value, index, data)?)
    }

    fn write_words(&self, words: &[(u16, u16)]) -> Result<(), Error> {
        Ok(self.hw.write_words(words)?)
    }

    fn wait(&self, timeout: Duration) -> Result<bool, Error> {
        Ok(self.hw.wait(timeout)?)
    }

    fn read_event(&self) -> Result<Event, Error> {
        Ok(self.hw.read_event()?)
    }

    fn is_connected(&self) -> bool {
        self.hw.is_connected()
    }
}

/// A decoded UCX meter bank.  The device rotates five packet types; a bank
/// keeps the latest value of each.
#[derive(Clone, Debug, Default)]
struct MeterBank {
    inputs_a: [u64; 18],
    inputs_b: [u64; 18],
    playback: [u64; 18],
    pre_fader: [u64; 18],
    post_fader: [u64; 18],
}

impl MeterBank {
    fn apply(&mut self, p: &MeterPacket) {
        match p.kind {
            MeterKind::InputsA => self.inputs_a.copy_from_slice(&p.power[..18]),
            MeterKind::InputsB => self.inputs_b.copy_from_slice(&p.power[..18]),
            MeterKind::Playback => self.playback.copy_from_slice(&p.power[..18]),
            MeterKind::OutputsPreFader => {
                self.pre_fader.copy_from_slice(&p.power[..18])
            }
            MeterKind::OutputsPostFader => {
                self.post_fader.copy_from_slice(&p.power[..18])
            }
        }
    }

    fn input(&self, i: usize) -> f32 {
        let p = self.inputs_a.get(i).copied().unwrap_or(0)
            .max(self.inputs_b.get(i).copied().unwrap_or(0));
        db_to_linear(protocol::power_dbfs(p))
    }

    fn playback(&self, i: usize) -> f32 {
        db_to_linear(protocol::power_dbfs(self.playback.get(i).copied().unwrap_or(0)))
    }

    fn output(&self, i: usize, post: bool) -> f32 {
        let p = if post {
            self.post_fader.get(i).copied().unwrap_or(0)
        } else {
            self.pre_fader.get(i).copied().unwrap_or(0)
        };
        db_to_linear(protocol::power_dbfs(p))
    }
}

fn db_to_linear(db: f32) -> f32 {
    if db == f32::NEG_INFINITY {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

/// The UCX output meter packet is in DSP/display order (AN1/2 first,
/// Phones fourth); TuxMix's output model is Phones-first.  `order` is the
/// profile's display -> internal pair list.
fn reorder_output_meters(raw: &[f32], order: &[usize]) -> Vec<f32> {
    let mut out = vec![0.0; raw.len()];
    for (display, &internal) in order.iter().enumerate() {
        let src = display * 2;
        let dst = internal * 2;
        if src + 1 < raw.len() && dst + 1 < out.len() {
            out[dst] = raw[src];
            out[dst + 1] = raw[src + 1];
        }
    }
    out
}

/// Copy the UI EQ model into the hardware register model.  Shared by
/// inputs and outputs; UCX hardware exposes the same DSP block on both.
fn apply_eq_to_settings(eq: &InputEq, s: &mut ChannelSettings) {
    s.eq.on = eq.enabled;
    s.low_cut.on = eq.low_cut_enabled;
    s.low_cut.freq = eq.low_cut_freq_hz.clamp(20, 500);
    s.low_cut.slope = match eq.low_cut_slope_db_oct {
        6 => 0,
        12 => 1,
        18 => 2,
        24 => 3,
        _ => 1,
    };
    for (i, band) in eq.bands.iter().enumerate() {
        s.eq.bands[i].freq = band.freq_hz.clamp(20, 20_000);
        s.eq.bands[i].q10 = (band.q * 10.0).round().clamp(7.0, 50.0) as u8;
        s.eq.bands[i].gain_tenths =
            (band.gain_db * 10.0).round().clamp(-200.0, 200.0) as i16;
        s.eq.bands[i].kind = match band.band_type {
            EqBandType::Off | EqBandType::Bell => 0,
            EqBandType::LowShelf | EqBandType::HighShelf => {
                if i == 1 { 0 } else { 1 }
            }
            // Band 3 only: Hi Cut (low pass) = 2.
            EqBandType::LowPass => 2,
        };
    }
}

/// Reorder scene channels into the current profile's channel order by name.
/// Scenes from an older profile can have the same channel count but a
/// different row order (the UCX playback row was once Phones-first).  Names
/// are the stable topology key; IDs are local indices and must never be
/// copied from a scene.
fn scene_order_by_name(current: &[String], saved: &[String]) -> Vec<usize> {
    use std::collections::VecDeque;
    let mut buckets: HashMap<&str, VecDeque<usize>> = HashMap::new();
    for (idx, name) in saved.iter().enumerate() {
        buckets.entry(name.as_str()).or_default().push_back(idx);
    }
    current
        .iter()
        .enumerate()
        .map(|(fallback, name)| {
            buckets
                .get_mut(name.as_str())
                .and_then(VecDeque::pop_front)
                .unwrap_or(fallback)
        })
        .collect()
}


/// The device state that can be read back.
///
/// The matrix, masters, loopback, mute, options word, references and the
/// preamp switches are write-only.  This snapshot is therefore partial:
/// the UC reads `0x10` and `0x17` completely; both models carry only the
/// gain/volume channels the front panel or state stream reported.
#[derive(Clone, Debug, Default)]
pub struct ReadableState {
    /// True for the UCX: only input gain and output volume are read back.
    /// The clock, format, reference and preamp-switch registers are
    /// unit-owned and write-only, so they are not compared.
    pub ucx: bool,
    pub clock_source: String,
    pub optical_out_spdif: bool,
    pub spdif_professional: bool,
    pub phantom: [bool; 2],
    pub pad: [bool; 2],
    pub instrument: [bool; 2],
    pub input_gain: Vec<(usize, u32)>,
    pub output_volume: Vec<(usize, f32)>,
}

impl ReadableState {
    /// Parameters whose readable device value differs from `scene`.
    pub fn differences(&self, scene: &Scene) -> Vec<String> {
        let mut out = Vec::new();
        if !self.ucx {
            if !self.clock_source.is_empty()
                && self.clock_source != scene.settings.clock_source
            {
                out.push(format!(
                    "Clock source: device {}, TuxMix {}",
                    self.clock_source, scene.settings.clock_source
                ));
            }
            if self.optical_out_spdif != scene.settings.optical_out_spdif {
                out.push(format!(
                    "Optical output: device {}, TuxMix {}",
                    if self.optical_out_spdif { "SPDIF" } else { "ADAT" },
                    if scene.settings.optical_out_spdif { "SPDIF" } else { "ADAT" }
                ));
            }
            if self.spdif_professional != scene.settings.spdif_professional {
                out.push(format!(
                    "SPDIF format: device {}, TuxMix {}",
                    if self.spdif_professional { "professional" } else { "consumer" },
                    if scene.settings.spdif_professional { "professional" } else { "consumer" }
                ));
            }
            for i in 0..2 {
                if let Some(input) = scene.inputs.get(i) {
                    if self.phantom[i] != input.phantom {
                        out.push(format!(
                            "AN{} 48 V: device {}, TuxMix {}",
                            i + 1,
                            self.phantom[i],
                            input.phantom
                        ));
                    }
                }
            }
            for (slot, i) in (2..4).enumerate() {
                if let Some(input) = scene.inputs.get(i) {
                    if self.pad[slot] != input.pad {
                        out.push(format!(
                            "AN{} PAD: device {}, TuxMix {}",
                            i + 1,
                            self.pad[slot],
                            input.pad
                        ));
                    }
                    if self.instrument[slot] != input.instrument {
                        out.push(format!(
                            "AN{} Inst: device {}, TuxMix {}",
                            i + 1,
                            self.instrument[slot],
                            input.instrument
                        ));
                    }
                }
            }
        }
        for &(idx, gain) in &self.input_gain {
            if let Some(input) = scene.inputs.get(idx) {
                if input.gain != Some(gain) {
                    let (dev, ours) = if idx < 2 {
                        (format!("{gain} dB"), format!("{} dB", input.gain.unwrap_or(0)))
                    } else {
                        (
                            format!("{:.1} dB", gain as f32 / 2.0),
                            format!(
                                "{:.1} dB",
                                input.gain.unwrap_or(0) as f32 / 2.0
                            ),
                        )
                    };
                    out.push(format!("AN{} gain: device {dev}, TuxMix {ours}", idx + 1));
                }
            }
        }
        for &(idx, volume) in &self.output_volume {
            if let Some(output) = scene.outputs.get(idx) {
                let dev_db = linear_db(volume);
                let ours_db = linear_db(output.volume);
                if (dev_db - ours_db).abs() > 0.05 {
                    out.push(format!(
                        "{} volume: device {dev_db:.1} dB, TuxMix {ours_db:.1} dB",
                        output.name
                    ));
                }
            }
        }
        out
    }
}

fn linear_db(v: f32) -> f32 {
    if v <= 0.0 {
        -120.0
    } else {
        20.0 * v.log10()
    }
}

fn pan_split(volume: f32, pan: i8) -> (f32, f32) {
    let v = volume.clamp(0.0, 1.0);
    let p = pan.clamp(-100, 100) as f32 / 100.0;
    let l = v * if p > 0.0 { 1.0 - p } else { 1.0 };
    let r = v * if p < 0.0 { 1.0 + p } else { 1.0 };
    (l, r)
}

/// The same balance law for hardware outputs, whose faders reach +6 dB.
fn pan_split_output(volume: f32, pan: i8) -> (f32, f32) {
    let v = volume.clamp(0.0, 2.0);
    let p = pan.clamp(-100, 100) as f32 / 100.0;
    let l = v * if p > 0.0 { 1.0 - p } else { 1.0 };
    let r = v * if p < 0.0 { 1.0 + p } else { 1.0 };
    (l, r)
}

fn output_balance(left: f32, right: f32) -> i8 {
    let peak = left.max(right);
    if peak <= 0.0 {
        0
    } else {
        (((right - left) / peak) * 100.0)
            .round()
            .clamp(-100.0, 100.0) as i8
    }
}

/// A Fireface UC or UCX opened over the kernel hwdep transport.
pub struct Fireface {
    transport: Mutex<Box<dyn ControlTransport>>,
    info: DeviceInfo,
    model: Model,
    spec: &'static super::models::ModelSpec,
    inputs: Vec<InputChannel>,
    playbacks: Vec<PlaybackChannel>,
    outputs: Vec<OutputChannel>,
    settings: DeviceSettings,
    muted: HashSet<ChannelId>,
    soloed: HashSet<ChannelId>,
    /// Saved volumes/pans for a muted input or playback, by submix pair.
    mute_saved: HashMap<ChannelId, (Vec<f32>, Vec<i8>)>,
    /// Saved output fader while the output is muted.
    output_mute_saved: HashMap<usize, f32>,
    /// Phones pair level saved while control-room Dim is engaged.
    dim_saved: Option<(f32, f32)>,
    /// UCX per-channel processing (inputs and outputs).  Output settings are
    /// kept for the protocol-complete scene model; the current RmeDevice
    /// trait only exposes input EQ.
    input_settings: Vec<ChannelSettings>,
    #[allow(dead_code)]
    output_settings: Vec<ChannelSettings>,
    /// Playback stereo image (phase per side, width) for the UCX matrix.
    #[allow(dead_code)]
    playback_phase: Vec<[bool; 2]>,
    playback_width: Vec<i16>,
    /// Latest decoded UCX meters.
    meters: MeterBank,
    /// First four UC/UCX preamps: gain is in `inputs[i].gain`, Hi-Z in
    /// `inputs[i].instrument`, AutoSet has no UI yet.
    #[allow(dead_code)]
    autoset: [bool; 4],
    /// UC channels whose gain/volume were reported by the front panel
    /// since the driver last started.  These are the only input-gain and
    /// output-volume values that can be compared at open.
    device_input_gain: HashMap<usize, u32>,
    device_output_volume: HashMap<usize, f32>,
    /// UCX IF1 heartbeat sequence and next refresh time.  Without this the
    /// unit does not stream live state deltas such as the Phones knob.
    ucx_heartbeat: u16,
    ucx_heartbeat_due: Instant,
    /// During the first moments after connect the unit can still deliver
    /// `0x8b` output-volume events for a front-panel move made while TuxMix
    /// was not running.  TotalMix's connect write wins over those stale
    /// events; ignore them until the host scene has been reapplied.
    ignore_output_until: Option<Instant>,
    /// Mono matrix cells that the per-pair model cannot express. Key is
    /// `(source 0..35, output block 0..17)`; 18..35 are software playbacks.
    matrix_overrides: HashMap<(usize, usize), f32>,
    /// UC input metering through the PipeWire source tap.  The PCM device
    /// stays with PipeWire and the DAW.  The UCX uses its 0x85 input pages.
    input_tap: Option<crate::pw_input::InputTap>,
}

impl Fireface {
    /// Open the first Fireface found through the hwdep ABI.
    pub fn open() -> Result<Self, Error> {
        Self::open_model(None)
    }

    pub fn open_uc() -> Result<Self, Error> {
        Self::open_model(Some(Model::Uc))
    }

    pub fn open_ucx() -> Result<Self, Error> {
        Self::open_model(Some(Model::Ucx))
    }

    pub fn open_model(model: Option<Model>) -> Result<Self, Error> {
        for found in hwdep::enumerate() {
            let Some(info) = DeviceInfo::from_abi(found.card, found.info) else {
                continue;
            };
            if model.is_some_and(|m| m != info.model) {
                continue;
            }
            let hw = Hwdep::open(found.card)?;
            let transport = HwdepTransport::new(hw, info.clone());
            return Self::from_transport(Box::new(transport), info);
        }
        Err(Error::DeviceNotFound {
            model: model
                .map(|m| m.name().to_string())
                .unwrap_or_else(|| "Fireface".into()),
        })
    }

    pub fn open_card(card: i32) -> Result<Self, Error> {
        let hw = Hwdep::open(card)?;
        let info = DeviceInfo::from_abi(card, hw.info()?)
            .ok_or_else(|| Error::DeviceNotFound { model: format!("card {card}") })?;
        let transport = HwdepTransport::new(hw, info.clone());
        Self::from_transport(Box::new(transport), info)
    }

    /// Open the unit with this stable identity, not just the first unit of
    /// its model.  Used by the GUI for selection and reconnect; a missing
    /// serial falls back to the first matching model.
    pub fn open_identity(identity: &DeviceIdentity) -> Result<Self, Error> {
        for found in hwdep::enumerate() {
            let Some(info) = DeviceInfo::from_abi(found.card, found.info) else {
                continue;
            };
            if info.model != identity.model {
                continue;
            }
            if !identity.serial.is_empty() && info.serial != identity.serial {
                continue;
            }
            let hw = Hwdep::open(found.card)?;
            let transport = HwdepTransport::new(hw, info.clone());
            return Self::from_transport(Box::new(transport), info);
        }
        Err(Error::DeviceNotFound {
            model: identity.model.name().to_string(),
        })
    }

    pub fn identity(&self) -> DeviceIdentity {
        self.info.identity()
    }

    /// List every hwdep-visible Fireface without opening a mixer.
    pub fn enumerate() -> Vec<DeviceInfo> {
        hwdep::enumerate()
            .into_iter()
            .filter_map(|d| DeviceInfo::from_abi(d.card, d.info))
            .collect()
    }

    fn from_transport(
        transport: Box<dyn ControlTransport>,
        info: DeviceInfo,
    ) -> Result<Self, Error> {
        let model = info.model;
        let spec = super::models::for_model(model);
        let profile = spec.profile;
        let mut inputs = profile.build_inputs();
        let playbacks = profile.build_playbacks();
        let mut outputs = profile.build_outputs();

        for (i, input) in inputs.iter_mut().enumerate() {
            match model {
                Model::Uc => {
                    if i < 4 {
                        input.gain = Some(0);
                        // Inputs 3/4 use half-dB steps; the model stores
                        // the raw 0..36 value the UI already displays as
                        // 0.0..18.0 dB.
                        input.gain_max = Some(if i < 2 { 65 } else { 36 });
                    }
                    if i < 2 {
                        input.has_pad = false;
                    } else if i < 4 {
                        input.has_pad = true;
                        input.has_instrument = true;
                    }
                }
                Model::Ucx => {
                    if i < 4 {
                        input.gain = Some(0);
                        // The wire value is 0.1 dB for every front input.
                        // AN1/2 reach 65 dB in 1 dB steps; AN3/4 reach
                        // 12 dB in 0.5 dB steps, stored as raw half-dB
                        // units (0..24) like the UC.
                        input.gain_max = Some(if i < 2 { 65 } else { 24 });
                    }
                    if i == 2 || i == 3 {
                        input.has_instrument = true;
                        // Measured baseline after restore: NN08 = 1 (+4 dBu).
                        input.level = Some(InputLevel::Plus4dBu);
                        input.ref_level = 1;
                    }
                    // UCX has Dynamics + Auto Level on every hardware input.
                    input.dynamics = Some(Dynamics::default());
                    input.eq = Some(InputEq::default());
                }
            }
        }
        if model == Model::Ucx {
            // TotalMix FX exposes EQ/low cut and dynamics on every hardware
            // output too.
            for output in outputs.iter_mut() {
                output.eq = Some(InputEq::default());
                output.dynamics = Some(Dynamics::default());
            }
        }

        let mut settings = Scene::new("").settings;
        settings.clock_sources = spec
            .clock_sources
            .iter()
            .map(|source| (*source).to_string())
            .collect();
        settings.sample_rate = if info.rate > 0 { info.rate } else { 48_000 };
        if model == Model::Ucx {
            settings.reverb = Some(Reverb::default());
            settings.echo = Some(Echo::default());
        }
        // The analog inputs 1-4 are mono; keep the first two pairs unlinked
        // until the device state says otherwise.
        settings.input_link = false;
        settings.input_pair_link = vec![false, false];

        let input_settings = vec![ChannelSettings::default(); info.inputs.max(18)];
        let output_settings = vec![ChannelSettings::default(); info.outputs.max(18)];
        let playback_phase = vec![[false, false]; playbacks.len()];
        let playback_width = vec![100; playbacks.len()];

        // Control-only: PipeWire and the DAW own the PCM device.  UC input
        // meters come from the PipeWire source tap; the UCX uses its own
        // 0x85 meter pages.  Unit tests must not spawn a real `pw-record`
        // child: it makes the tests depend on the host sound server (and
        // can hang its Drop/join if PipeWire is mid-reconfigure).
        let input_tap = match model {
            Model::Uc
                if !cfg!(test)
                    && std::env::var_os("TUXMIX_DEFER_METER_TAP").is_none() =>
            {
                let selector = if info.serial.is_empty() {
                    info.name.clone()
                } else {
                    info.serial.clone()
                };
                Some(crate::pw_input::InputTap::start(&selector))
            }
            _ => None,
        };

        let mut dev = Self {
            transport: Mutex::new(transport),
            info,
            model,
            spec,
            inputs,
            playbacks,
            outputs,
            settings,
            muted: HashSet::new(),
            soloed: HashSet::new(),
            mute_saved: HashMap::new(),
            output_mute_saved: HashMap::new(),
            dim_saved: None,
            input_settings,
            output_settings,
            playback_phase,
            playback_width,
            meters: MeterBank::default(),
            autoset: [false; 4],
            device_input_gain: HashMap::new(),
            device_output_volume: HashMap::new(),
            ucx_heartbeat: 0,
            ucx_heartbeat_due: Instant::now(),
            ignore_output_until: None,
            matrix_overrides: HashMap::new(),
            input_tap,
        };
        if model == Model::Uc {
            dev.read_uc_levels();
            dev.read_uc_settings();
            // Pick up front-panel changes queued while TuxMix was closed.
            dev.drain_events();
        }
        Ok(dev)
    }

    pub fn model(&self) -> Model {
        self.model
    }

    pub fn spec(&self) -> &'static super::models::ModelSpec {
        self.spec
    }

    /// Ignore output-volume status events for a short connect window.
    ///
    /// The UC can deliver a `0x8b` for a front-panel move made while TuxMix
    /// was not running after `begin_metering()` has started the engine.  If
    /// that event were applied, it would overwrite the host scene in the
    /// model and then the autosave would persist the device value.  TotalMix
    /// treats its connect write as authoritative; do the same until the
    /// startup scene has been reapplied.
    pub fn suppress_front_panel_for(&mut self, duration: Duration) {
        self.ignore_output_until = Some(Instant::now() + duration);
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn is_uc(&self) -> bool {
        self.model == Model::Uc
    }

    fn output_order(&self) -> &'static [usize] {
        self.spec.profile.output_order
    }

    fn transport(&self) -> std::sync::MutexGuard<'_, Box<dyn ControlTransport>> {
        self.transport.lock().unwrap()
    }

    fn vendor_out(&self, req: u8, value: u16, index: u16) -> Result<(), Error> {
        self.transport()
            .vendor(req, Direction::Out, value, index, &[])?;
        Ok(())
    }

    fn vendor_in(
        &self,
        req: u8,
        value: u16,
        index: u16,
        len: usize,
    ) -> Result<Vec<u8>, Error> {
        self.transport()
            .vendor(req, Direction::In, value, index, &vec![0u8; len])
    }

    fn write_word(&self, addr: u16, value: u16) -> Result<(), Error> {
        let result = self.transport().write_words(&[(addr, value)]);
        if let Err(e) = &result {
            log::warn!("IF1 word write 0x{addr:04x}=0x{value:04x} failed: {e:?}");
        }
        if self.model == Model::Ucx && result.is_ok() {
            std::thread::sleep(UCX_WRITE_GAP);
        }
        result
    }

    fn write_words(&self, words: &[(u16, u16)]) -> Result<(), Error> {
        let result = self.transport().write_words(words);
        if let Err(e) = &result {
            log::warn!("IF1 burst of {} words starting 0x{:04x} failed: {e:?}", words.len(), words.first().map(|w| w.0).unwrap_or(0));
        }
        if self.model == Model::Ucx && result.is_ok() {
            std::thread::sleep(UCX_WRITE_GAP);
        }
        result
    }

    // ── UC write paths ────────────────────────────────────────────────

    fn matrix_source_index(channel: ChannelId) -> Option<usize> {
        match channel {
            ChannelId::Input(i) => Some(i),
            ChannelId::Playback(i) => Some(18 + i),
            ChannelId::Output(_) => None,
        }
    }

    fn matrix_source_channel(source: usize) -> Option<ChannelId> {
        match source {
            0..=17 => Some(ChannelId::Input(source)),
            18..=35 => Some(ChannelId::Playback(source - 18)),
            _ => None,
        }
    }

    /// Gain the per-pair channel model implies for one mono cell.
    fn matrix_default(&self, channel: ChannelId, block: usize) -> Option<f32> {
        if block >= BLOCKS {
            return None;
        }
        let pair = block / 2;
        match channel {
            ChannelId::Input(i) => {
                let ch = self.inputs.get(i)?;
                let volume = *ch.volumes.get(pair)?;
                if i < 4 {
                    // The four analog inputs are mono: the pair value is
                    // split across the output sides by the pan law.
                    let (left, right) =
                        pan_split(volume, ch.pans.get(pair).copied().unwrap_or(0));
                    Some(if block % 2 == 0 { left } else { right })
                } else if block % 2 == i % 2 {
                    Some(volume)
                } else {
                    Some(0.0)
                }
            }
            ChannelId::Playback(i) => {
                let ch = self.playbacks.get(i)?;
                let volume = *ch.volumes.get(pair)?;
                let (left, right) =
                    pan_split(volume, ch.pans.get(pair).copied().unwrap_or(0));
                Some(if block % 2 == i % 2 {
                    if i % 2 == 0 { left } else { right }
                } else {
                    0.0
                })
            }
            ChannelId::Output(_) => None,
        }
    }

    /// A natural cell is the one the stereo-pair model already writes:
    /// a playback or stereo input channel into its own side of the output
    /// pair.  The four mono analog inputs are pan-derived instead, so a
    /// per-cell edit there is an override on that side only.
    fn matrix_natural(channel: ChannelId, block: usize) -> bool {
        match channel {
            ChannelId::Input(i) => i >= 4 && block % 2 == i % 2,
            ChannelId::Playback(i) => block % 2 == i % 2,
            ChannelId::Output(_) => false,
        }
    }

    /// Drop mono-input cell overrides when the pan/volume changes: the
    /// pair model is again the truth for both sides.
    fn clear_mono_input_cells(&mut self, idx: usize) {
        if idx < 4 {
            self.matrix_overrides
                .retain(|(source, _), _| *source != idx);
        }
    }

    /// Phase is a per-source property on the UC: TotalMix writes the
    /// sign bit of every matrix cell carrying that source, it does not
    /// have a separate phase register.  Only input sources have a phase
    /// control in TuxMix today.
    fn matrix_source_phase(&self, source: usize) -> bool {
        self.inputs.get(source).is_some_and(|input| input.phase)
    }

    fn uc_write_matrix(&self, block: usize, source: usize, linear: f32) -> Result<(), Error> {
        let address = (block as u16) * uc::MIX_STRIDE + source as u16;
        let (mut value, index) = uc::matrix_word(linear, address);
        if self.matrix_source_phase(source) && value != 0 {
            value = value.wrapping_neg();
        }
        self.vendor_out(protocol::req::MIXER, value, index)
    }

    fn uc_write_master(&self, block: usize, linear: f32) -> Result<(), Error> {
        let (value, index) = uc::matrix_word(linear, uc::MIX_MASTER + block as u16);
        self.vendor_out(protocol::req::MIXER, value, index)
    }

    /// Reproduce the app-level connect sequence TotalMix sends to the UC.
    ///
    /// The order is taken from the Windows capture
    /// `capture/uc-fp-20261026/UC-FP3_startup_mismatch.pcapng`:
    ///
    /// 1. 30 × `0x15` with index 0..29 and value 0 (mute/dim bank reset);
    /// 2. for each output pair, all except Phones in descending internal
    ///    order, then Phones last:
    ///    - `0x1a` for the left and right channel (the value the front
    ///      panel displays, 1 dB code);
    ///    - `0x12` master for the left and right block (`992 + block`)
    ///      carrying the *same* level as the `0x1a` write;
    ///    - the 36 crosspoints for the left block and the right block,
    ///      interleaved source by source.
    ///
    /// TotalMix writes 714 requests at connect: 30 × `0x15`, 18 × `0x1a`
    /// and 666 × `0x12`.  Writing all `0x1a` values first and then all
    /// `0x12` values (the previous implementation) leaves the `0x12`
    /// masters at 0 dB and makes the unit jump to full level at engine
    /// start while the front panel still shows the old fader position.
    fn apply_uc_full_matrix(&mut self) -> Result<(), Error> {
        // TotalMix resets the 30-entry mute/dim bank before the mixer push.
        for idx in 0..30u16 {
            self.vendor_out(protocol::req::LOOPBACK, 0, idx)?;
        }

        // Internal pair 0 is Phones.  TotalMix writes it last; the other
        // pairs go in descending internal order (ADAT 7/8 first).
        let mut pairs: Vec<usize> = (1..PAIRS).rev().collect();
        pairs.push(0);

        let sources = self.inputs.len() + self.playbacks.len();
        for pair in pairs {
            let left = pair * 2;
            let right = left + 1;
            let left_block = uc::matrix_block(left, self.output_order());
            let right_block = uc::matrix_block(right, self.output_order());

            // The front-panel code and the 14-bit master must carry the
            // same level.  TotalMix writes both for every output fader.
            for idx in [left, right] {
                let out = &self.outputs[idx];
                let gain = if out.mute { 0.0 } else { out.volume };
                if out.mute {
                    self.output_mute_saved.entry(idx).or_insert(out.volume);
                }
                self.uc_write_output_volume(idx, gain)?;
            }
            for (idx, block) in [(left, left_block), (right, right_block)] {
                let out = &self.outputs[idx];
                let gain = if out.mute { 0.0 } else { out.volume };
                self.uc_write_master(block, gain)?;
            }

            // TotalMix interleaves the two hardware blocks: L source 0,
            // R source 0, L source 1, R source 1, ...
            for source in 0..sources {
                let Some(channel) = Self::matrix_source_channel(source) else {
                    continue;
                };
                for block in [left_block, right_block] {
                    let gain = self.matrix_cell(channel, block).unwrap_or(0.0)
                        * self.trim_gain(channel);
                    self.uc_write_matrix(block, source, gain)?;
                }
            }
        }
        Ok(())
    }

    fn uc_write_input(&self, idx: usize, pair: usize, _volume: f32) -> Result<(), Error> {
        let pan = self.inputs[idx].pans.get(pair).copied().unwrap_or(0);
        if idx < 4 {
            let volume = self.inputs[idx].volumes.get(pair).copied().unwrap_or(0.0)
                * self.trim_gain(ChannelId::Input(idx));
            let (l, r) = pan_split(volume, pan);
            // `pair` is the matrix display column (AN1/2 first), which is
            // already the raw UC matrix block order.  Do not run it through
            // matrix_block(): that maps the internal Phones-first order and
            // would shift every crosspoint.
            self.uc_write_matrix(pair * 2, idx, l)?;
            return self.uc_write_matrix(pair * 2 + 1, idx, r);
        }
        // A stereo strip: one source per side, balance only.
        let base = idx & !1;
        let other = base ^ 1;
        let left = self.inputs[base].volumes.get(pair).copied().unwrap_or(0.0)
            * self.trim_gain(ChannelId::Input(base));
        let right = self.inputs[other].volumes.get(pair).copied().unwrap_or(0.0)
            * self.trim_gain(ChannelId::Input(other));
        let balance = pan.clamp(-100, 100) as f32 / 100.0;
        let gl = left * (1.0 - balance.max(0.0));
        let gr = right * (1.0 + balance.min(0.0));
        self.uc_write_matrix(pair * 2, base, gl)?;
        self.uc_write_matrix(pair * 2 + 1, other, gr)
    }

    fn uc_write_playback(&self, idx: usize, pair: usize, volume: f32) -> Result<(), Error> {
        let dev = self.playbacks[idx].id;
        let block = pair * 2 + (dev % 2);
        let pan = self.playbacks[idx].pans.get(pair).copied().unwrap_or(0);
        let volume = volume * self.trim_gain(ChannelId::Playback(idx));
        let (left, right) = pan_split(volume, pan);
        let gain = if dev % 2 == 0 { left } else { right };
        // `pair` is the display column; the raw UC block order is the same.
        self.uc_write_matrix(block, 18 + dev, gain)
    }

    fn uc_write_output_volume(&self, idx: usize, volume: f32) -> Result<(), Error> {
        let Some(channel) = uc::encoder_channel(idx, self.output_order()) else {
            return Err(Error::InvalidChannel(format!("output {idx}")));
        };
        let raw = uc::output_attenuation(volume);
        self.vendor_out(protocol::req::PGA, raw as u16, channel as u16)
    }

    /// Keep the 14-bit `0x12` output master in step with the `0x1a`
    /// front-panel code.  TotalMix writes both on every output-fader move;
    /// the `0x12` master is what survives an engine restart, so writing
    /// only `0x1a` lets a later stream start jump back to the old master.
    fn uc_write_output_master(&self, idx: usize, volume: f32) -> Result<(), Error> {
        let block = uc::matrix_block(idx, self.output_order());
        self.uc_write_master(block, volume)
    }

    /// TotalMix repeats an output master into four submix banks when the
    /// output balance changes (captured 0x12 indices 1001/17385/33769/50153
    /// for SPDIF, and their +1).  Bank bits live in `wIndex` 14–15; the
    /// output-master low level bits are zero for these levels.
    fn uc_write_output_master_banks(&self, idx: usize, volume: f32) -> Result<(), Error> {
        let block = uc::matrix_block(idx, self.output_order());
        let (value, index) = uc::matrix_word(volume, uc::MIX_MASTER + block as u16);
        for bank in [0x0000u16, 0x4000, 0x8000, 0xc000] {
            self.vendor_out(protocol::req::MIXER, value, index | bank)?;
        }
        Ok(())
    }

    fn uc_write_switch(&mut self, mask: u16, on: bool) -> Result<(), Error> {
        // The UC stores the switches and the reference levels in one
        // register.  A single-bit masked write is accepted by EP0 but the
        // switch bit does not change; write the complete register instead.
        let mask = mask as u32;
        let d = self.vendor_in(protocol::req::LEVELS, 0, 0, 4)?;
        if d.len() < 4 {
            return Err(Error::Io(std::io::Error::other(
                "short 0x17 read while changing a switch",
            )));
        }
        let current = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
        let next = if on {
            current | mask
        } else {
            current & !mask
        };
        // The UC settings dialog uses wIndex 4032 (0x0fc0) for the
        // reference-level fields, but that mask excludes the 48 V/PAD/Inst
        // bits.  Switch changes need the complete register (wIndex 0xffff),
        // otherwise EP0 accepts the write and the switch bit does not move.
        self.vendor_out(protocol::req::LEVELS, next as u16, 0xffff)?;

        // Keep the model tied to the register the unit actually accepted.
        let d = self.vendor_in(protocol::req::LEVELS, 0, 0, 4)?;
        let readback = u32::from_le_bytes([
            d[0],
            d[1],
            d[2],
            d.get(3).copied().unwrap_or(0),
        ]);
        if (readback & mask != 0) != on {
            return Err(Error::Io(std::io::Error::other(format!(
                "0x17 switch mask 0x{mask:04x} did not read back as {on}"
            ))));
        }
        self.apply_uc_levels(readback);
        Ok(())
    }

    /// Apply the readable parts of the UC `0x17` register to the model.
    fn apply_uc_levels(&mut self, value: u32) {
        self.settings.line_out_ref = ((value >> 6) & 0x3) as u16;
        self.settings.line_in_ref = ((value >> 8) & 0x3) as u16;
        self.settings.phones_ref = ((value >> 10) & 0x3) as u16;
        for input in self.inputs.iter_mut().take(2) {
            input.phantom = false;
        }
        for (i, input) in self.inputs.iter_mut().enumerate() {
            if i < 2 {
                input.phantom = value & (1 << i) != 0;
            }
            if (2..4).contains(&i) {
                input.pad = value & (1 << (5 - i)) != 0;
                input.instrument = value & (1 << (7 - i)) != 0;
            }
        }
    }

    /// Read the UC switch register at open.  The matrix, gains and output
    /// volumes have no on-demand readback; this is the one parameter block
    /// that can be recovered before applying the host state.
    fn read_uc_levels(&mut self) {
        if let Ok(d) = self.vendor_in(protocol::req::LEVELS, 0, 0, 4) {
            if d.len() >= 4 {
                self.apply_uc_levels(u32::from_le_bytes([d[0], d[1], d[2], d[3]]));
            }
        }
    }

    /// Read the UC settings register at open.  Only the bits that map to
    /// `DeviceSettings` are applied; the host scene is written afterwards.
    fn read_uc_settings(&mut self) {
        if let Ok(d) = self.vendor_in(protocol::req::SETTINGS, 0, 0, 4) {
            if d.len() >= 2 {
                let v = u16::from_le_bytes([d[0], d[1]]);
                self.settings.clock_source = match (v >> 1) & 7 {
                    0 => "Internal",
                    1 => "SPDIF Coaxial",
                    2 => "Optical",
                    3 => "Word Clock",
                    _ => "LTC",
                }
                .to_string();
                self.settings.spdif_professional = v & 0x0080 != 0;
                self.settings.optical_out_spdif = v & 0x0400 != 0;
                self.settings.word_clock_single_speed = v & 0x0040 != 0;
            }
        }
    }

    fn uc_options_word(&self) -> u16 {
        let clock = match self.settings.clock_source.as_str() {
            "SPDIF Coaxial" | "SPDIF" => 1,
            "Optical" | "ADAT" => 2,
            "Word Clock" => 3,
            "LTC" => 4,
            _ => 0,
        };
        let mut v = 0x0001 | ((clock & 7) << 1);
        if self.settings.spdif_professional {
            v |= 0x0080;
        }
        if self.settings.optical_out_spdif {
            v |= 0x0400;
        }
        if self.settings.word_clock_single_speed {
            v |= 0x0040;
        }
        v
    }

    fn uc_write_options(&self) -> Result<(), Error> {
        self.vendor_out(protocol::req::SETTINGS, self.uc_options_word(), 1487)
    }

    // ── UCX write paths ───────────────────────────────────────────────

    fn ucx_write_matrix(
        &self,
        dsp_output: u8,
        source: Source,
        linear: f64,
    ) -> Result<(), Error> {
        let reg = ucx::matrix_reg(dsp_output, source);
        let value = ucx::matrix_gain(linear);
        self.write_word(reg, value)?;
        // TotalMix keeps a second copy of every playback fader at +0x180
        // (`0x4020` → `0x41A0`, `0x4061` → `0x41E1`). Writing it keeps the
        // unit's own mirror in step (measured H86).
        if matches!(source, Source::Playback(_)) {
            self.write_word(reg.wrapping_add(0x180), value)?;
        }
        Ok(())
    }

    /// Gains of an input pair into one routing, after trim and balance.
    fn ucx_input_pair_gains(&self, base: usize, pair: usize) -> (f32, f32) {
        let other = base ^ 1;
        let pan = self.inputs[base].pans.get(pair).copied().unwrap_or(0);
        let left = self.inputs[base].volumes.get(pair).copied().unwrap_or(0.0)
            * self.dest_trim(ChannelId::Input(base), pair * 2);
        let right = self.inputs[other].volumes.get(pair).copied().unwrap_or(0.0)
            * self.dest_trim(ChannelId::Input(other), pair * 2 + 1);
        let balance = pan.clamp(-100, 100) as f32 / 100.0;
        (
            left * (1.0 - balance.max(0.0)),
            right * (1.0 + balance.min(0.0)),
        )
    }

    /// Rewrite one routing's 2x2 matrix for an input pair, applying the
    /// pair's width.  Silent routings are left alone, matching the unit
    /// (a full width sweep with every send at -inf writes nothing).
    fn ucx_write_input_pair(&self, base: usize, pair: usize) -> Result<(), Error> {
        let dsp = ucx::output_pair_dsp(pair)
            .ok_or_else(|| Error::InvalidChannel(format!("output pair {pair}")))?;
        let other = base ^ 1;
        let (gl, gr) = self.ucx_input_pair_gains(base, pair);
        if gl <= 0.0 && gr <= 0.0 {
            return Ok(());
        }
        let width = self.inputs[base].width.clamp(-1.0, 1.0);
        let theta = (1.0 - width) * std::f32::consts::FRAC_PI_4;
        let (sin, cos) = theta.sin_cos();
        self.ucx_write_matrix(dsp as u8, Source::Input(base), (gl * cos) as f64)?;
        self.ucx_write_matrix(dsp as u8, Source::Input(other), (gr * sin) as f64)?;
        self.ucx_write_matrix(
            dsp as u8 + 1,
            Source::Input(base),
            (gl * sin) as f64,
        )?;
        self.ucx_write_matrix(
            dsp as u8 + 1,
            Source::Input(other),
            (gr * cos) as f64,
        )
    }

    fn ucx_write_input(&self, idx: usize, pair: usize, _volume: f32) -> Result<(), Error> {
        let dsp = ucx::output_pair_dsp(pair)
            .ok_or_else(|| Error::InvalidChannel(format!("output pair {pair}")))?;
        let pan = self.inputs[idx].pans.get(pair).copied().unwrap_or(0);
        let base = idx & !1;
        if idx < 4 {
            let volume = self.inputs[idx].volumes.get(pair).copied().unwrap_or(0.0);
            if self.inputs[base].width != 1.0 {
                self.ucx_write_input_pair(base, pair)?;
            } else {
                let (l, r) = pan_split(volume, pan);
                let (l, r) = (
                    l * self.dest_trim(ChannelId::Input(idx), pair * 2),
                    r * self.dest_trim(ChannelId::Input(idx), pair * 2 + 1),
                );
                let source = Source::Input(idx);
                self.ucx_write_matrix(dsp as u8, source, l as f64)?;
                self.ucx_write_matrix(dsp as u8 + 1, source, r as f64)?;
            }
            let table = ucx::fader_table_reg(dsp as u8, idx);
            let level = ucx::linear_to_tenths(
                volume * self.dest_trim(ChannelId::Input(idx), pair * 2),
            ) as i16;
            self.write_word(table, ucx::fader_level(level))?;
            return self.write_word(table, ucx::fader_pan(pan as i16));
        }
        // Stereo strip: source `base` on the left, `base^1` on the right.
        let other = base ^ 1;
        let (gl, gr) = self.ucx_input_pair_gains(base, pair);
        if self.inputs[base].width != 1.0 {
            self.ucx_write_input_pair(base, pair)?;
        } else {
            self.ucx_write_matrix(dsp as u8, Source::Input(base), gl as f64)?;
            self.ucx_write_matrix(dsp as u8 + 1, Source::Input(other), gr as f64)?;
        }
        let t0 = ucx::fader_table_reg(dsp as u8, base);
        let t1 = ucx::fader_table_reg(dsp as u8, other);
        self.write_word(t0, ucx::fader_level(ucx::linear_to_tenths(gl) as i16))?;
        self.write_word(t0, ucx::fader_pan(0))?;
        self.write_word(t1, ucx::fader_level(ucx::linear_to_tenths(gr) as i16))?;
        self.write_word(t1, ucx::fader_pan(0))
    }

    /// Rewrite one routing's 2x2 matrix for a playback pair.
    fn ucx_write_playback_pair(&self, base: usize, pair: usize) -> Result<(), Error> {
        let dsp = ucx::output_pair_dsp(pair)
            .ok_or_else(|| Error::InvalidChannel(format!("output pair {pair}")))?;
        let other = base ^ 1;
        let gl = self.playbacks[base].volumes.get(pair).copied().unwrap_or(0.0)
            * self.dest_trim(ChannelId::Playback(base), pair * 2);
        let gr = self.playbacks[other].volumes.get(pair).copied().unwrap_or(0.0)
            * self.dest_trim(ChannelId::Playback(other), pair * 2 + 1);
        if gl <= 0.0 && gr <= 0.0 {
            return Ok(());
        }
        let width = self.playbacks[base].width.clamp(-1.0, 1.0);
        let theta = (1.0 - width) * std::f32::consts::FRAC_PI_4;
        let (sin, cos) = theta.sin_cos();
        let dev_l = self.playbacks[base].id;
        let dev_r = self.playbacks[other].id;
        self.ucx_write_matrix(dsp as u8, Source::Playback(dev_l), (gl * cos) as f64)?;
        self.ucx_write_matrix(dsp as u8, Source::Playback(dev_r), (gr * sin) as f64)?;
        self.ucx_write_matrix(
            dsp as u8 + 1,
            Source::Playback(dev_l),
            (gl * sin) as f64,
        )?;
        self.ucx_write_matrix(
            dsp as u8 + 1,
            Source::Playback(dev_r),
            (gr * cos) as f64,
        )
    }

    fn ucx_write_playback(&self, idx: usize, pair: usize, volume: f32) -> Result<(), Error> {
        let dev = self.playbacks[idx].id;
        let dsp = ucx::output_pair_dsp(pair)
            .ok_or_else(|| Error::InvalidChannel(format!("output pair {pair}")))?;
        if self.playbacks[idx & !1].width != 1.0 {
            return self.ucx_write_playback_pair(idx & !1, pair);
        }
        let side = dev % 2;
        let volume = volume * self.dest_trim(ChannelId::Playback(idx), pair * 2 + side);
        self.ucx_write_matrix(
            (dsp + side) as u8,
            Source::Playback(dev),
            volume as f64,
        )
    }

    fn ucx_write_output_volume(&self, idx: usize, volume: f32) -> Result<(), Error> {
        let pair = idx / 2;
        let side = idx % 2;
        let dsp = ucx::output_pair_dsp(pair)
            .ok_or_else(|| Error::InvalidChannel(format!("output {idx}")))?;
        let channel = ucx::output_channel(dsp + side);
        let value = ucx::linear_to_tenths(volume);
        self.write_word(
            ucx::channel_reg(channel, ucx::offset::MUTE_OR_FADER),
            value,
        )
    }

    /// The output mode word: bit 0 = mute, bit 1 = this pair is the Main
    /// Out. Both bits share `0xNN02`, so every write composes the whole
    /// word rather than clobbering the Main flag with a mute.
    fn ucx_write_output_mode(&self, idx: usize, mute: bool) -> Result<(), Error> {
        let pair = idx / 2;
        let side = idx % 2;
        let dsp = ucx::output_pair_dsp(pair)
            .ok_or_else(|| Error::InvalidChannel(format!("output {idx}")))?;
        let channel = ucx::output_channel(dsp + side);
        let monitoring = self.settings.main_out == Some(pair)
            || self.settings.main_b_out == Some(pair);
        self.write_word(
            ucx::channel_reg(channel, ucx::offset::OUTPUT_MUTE),
            (mute as u16) | ((monitoring as u16) << 1),
        )
    }

    fn ucx_write_output_mute(&self, idx: usize, mute: bool) -> Result<(), Error> {
        self.ucx_write_output_mode(idx, mute)
    }

    /// Write the gain in 0.1 dB units (`NN07`).
    fn ucx_write_input_gain(&self, idx: usize, tenths_db: u32) -> Result<(), Error> {
        let channel = ucx::input_channel(idx);
        self.write_word(
            ucx::channel_reg(channel, ucx::offset::GAIN_REFLEVEL_OR_PHASE),
            tenths_db.min(0xffff) as u16,
        )
    }

    fn ucx_write_input_hiz(&self, idx: usize, on: bool) -> Result<(), Error> {
        self.write_word(
            ucx::channel_reg(ucx::input_channel(idx), ucx::offset::HI_Z),
            on as u16,
        )
    }

    fn ucx_write_input_phase(&self, idx: usize, on: bool) -> Result<(), Error> {
        self.write_word(
            ucx::channel_reg(ucx::input_channel(idx), ucx::offset::INPUT_PHASE),
            on as u16,
        )
    }

    /// 48 V (inputs 1/2) and Level (inputs 3/4) share `NN08`.
    fn ucx_write_input_48v(&self, idx: usize, on: bool) -> Result<(), Error> {
        self.write_word(
            ucx::channel_reg(ucx::input_channel(idx), ucx::offset::INPUT_LEVEL_OR_48V),
            on as u16,
        )
    }

    fn ucx_write_input_level(&self, idx: usize, level: InputLevel) -> Result<(), Error> {
        self.write_word(
            ucx::channel_reg(ucx::input_channel(idx), ucx::offset::INPUT_LEVEL_OR_48V),
            level.code(),
        )
    }

    fn ucx_write_autoset(&self, idx: usize, on: bool) -> Result<(), Error> {
        self.write_word(
            ucx::channel_reg(ucx::input_channel(idx), ucx::offset::AUTOSET),
            on as u16,
        )
    }

    /// Write the 12 dynamics / Auto Level registers of one hardware input
    /// or output.  The measured UCX block is `NN60..NN67` + `NN80..NN83`.
    fn ucx_write_dynamics(
        &mut self,
        channel: ChannelId,
        dynamics: &Dynamics,
    ) -> Result<(), Error> {
        let (idx, is_output) = match channel {
            ChannelId::Input(i) => (i, false),
            ChannelId::Output(i) => (i, true),
            ChannelId::Playback(_) => {
                return Err(Error::InvalidChannel(
                    "playback channels have no dynamics on the UCX".into(),
                ))
            }
        };
        if is_output {
            if idx >= self.output_settings.len() {
                return Err(Error::InvalidChannel(format!("output {idx}")));
            }
        } else if idx >= self.input_settings.len() {
            return Err(Error::InvalidChannel(format!("input {idx}")));
        }
        let mut settings = if is_output {
            self.output_settings[idx]
        } else {
            self.input_settings[idx]
        };
        let d = dynamics.clamped();
        settings.dynamics.on = d.on;
        settings.dynamics.gain_tenths = (d.gain_db * 10.0).round() as i16;
        settings.dynamics.attack_ms = d.attack_ms;
        settings.dynamics.release_ms = d.release_ms;
        settings.dynamics.comp_threshold_tenths =
            (d.comp_threshold_db * 10.0).round() as i16;
        settings.dynamics.comp_ratio10 = (d.comp_ratio * 10.0).round() as u8;
        settings.dynamics.exp_threshold_tenths =
            (d.exp_threshold_db * 10.0).round() as i16;
        settings.dynamics.exp_ratio10 = (d.exp_ratio * 10.0).round() as u8;
        settings.auto_level.on = d.auto_level;
        settings.auto_level.max_gain_tenths = (d.auto_max_gain_db * 10.0).round() as u16;
        settings.auto_level.headroom_tenths = (d.auto_headroom_db * 10.0).round() as u16;
        settings.auto_level.rise_tenths = (d.auto_rise_s * 10.0).round() as u16;
        if is_output {
            self.output_settings[idx] = settings;
        } else {
            self.input_settings[idx] = settings;
        }
        let channel_no = if is_output {
            let dsp = ucx::output_pair_dsp(idx / 2)
                .ok_or_else(|| Error::InvalidChannel(format!("output {idx}")))?;
            ucx::output_channel(dsp + idx % 2)
        } else {
            ucx::input_channel(idx)
        };
        let dyn_offsets = [
            (ucx::offset::DYN_ON, settings.dynamics.on as u16),
            (ucx::offset::DYN_GAIN, settings.dynamics.gain_tenths as u16),
            (ucx::offset::DYN_ATTACK, settings.dynamics.attack_ms.saturating_add(1)),
            (ucx::offset::DYN_RELEASE, settings.dynamics.release_ms.saturating_add(1)),
            (ucx::offset::COMP_THRESHOLD, settings.dynamics.comp_threshold_tenths as u16),
            (ucx::offset::COMP_RATIO, settings.dynamics.comp_ratio10 as u16),
            (ucx::offset::EXP_THRESHOLD, settings.dynamics.exp_threshold_tenths as u16),
            (ucx::offset::EXP_RATIO, settings.dynamics.exp_ratio10 as u16),
            (ucx::offset::AUTO_LEVEL_ON, settings.auto_level.on as u16),
            (ucx::offset::AUTO_LEVEL_MAX_GAIN, settings.auto_level.max_gain_tenths),
            (ucx::offset::AUTO_LEVEL_HEADROOM, settings.auto_level.headroom_tenths),
            (ucx::offset::AUTO_LEVEL_RISE_TIME, settings.auto_level.rise_tenths),
        ];
        let words: Vec<(u16, u16)> = dyn_offsets
            .into_iter()
            .map(|(off, value)| (ucx::channel_reg(channel_no, off), value))
            .collect();
        self.write_words(&words)
    }

    fn ucx_write_reverb(&self, reverb: &Reverb) -> Result<(), Error> {
        let words = [
            (0x3c00u16, reverb.on as u16),
            (0x3c01, reverb.kind as u16),
            (0x3c02, reverb.pre_delay_ms),
            (0x3c03, reverb.low_cut_hz),
            (0x3c04, (reverb.room_scale * 100.0).round().clamp(0.0, 65535.0) as u16),
            // Type-specific block: Attack/Hold/Release for the envelope
            // algorithms, Reverb Time/High Damp for the Space ones.
            (0x3c05, reverb.attack_ms),
            (0x3c06, reverb.hold_ms),
            (0x3c07, reverb.release_ms),
            (0x3c08, reverb.high_cut_hz),
            (0x3c09, reverb.reverb_time_ds),
            (0x3c0a, reverb.high_damp_hz),
            (0x3c0b, reverb.smoothness as u16),
            (0x3c0c, (reverb.volume_db * 10.0).round() as i16 as u16),
            (0x3c0d, reverb.width.round().clamp(0.0, 65535.0) as u16),
        ];
        self.write_words(&words)
    }

    fn ucx_write_echo(&self, echo: &Echo) -> Result<(), Error> {
        let words = [
            (0x3c20u16, echo.on as u16),
            (0x3c21, echo.kind as u16),
            (0x3c22, echo.delay_ms / 10),
            (0x3c23, echo.feedback as u16),
            (0x3c24, echo.high_cut as u16),
            (0x3c25, (echo.volume_db * 10.0).round() as i16 as u16),
            (0x3c26, echo.width.round().clamp(0.0, 65535.0) as u16),
        ];
        self.write_words(&words)
    }

    fn ucx_write_fx_return(&self, idx: usize, db: f32) -> Result<(), Error> {
        let pair = idx / 2;
        let dsp = ucx::output_pair_dsp(pair)
            .ok_or_else(|| Error::InvalidChannel(format!("output {idx}")))?;
        let channel = ucx::output_channel(dsp + idx % 2);
        self.write_word(
            ucx::channel_reg(channel, ucx::offset::FX_RETURN),
            (db * 10.0).round() as i16 as u16,
        )
    }

    /// UCX stereo/mono mode.  Measured value 0 = mono; the stereo value is
    /// presumed 1.
    fn ucx_write_input_stereo(&self, idx: usize, linked: bool) -> Result<(), Error> {
        self.write_word(
            ucx::channel_reg(ucx::input_channel(idx), ucx::offset::INPUT_STEREO),
            linked as u16,
        )
    }

    fn ucx_write_output_stereo(&self, idx: usize, linked: bool) -> Result<(), Error> {
        let pair = idx / 2;
        let dsp = ucx::output_pair_dsp(pair)
            .ok_or_else(|| Error::InvalidChannel(format!("output {idx}")))?;
        let channel = ucx::output_channel(dsp + idx % 2);
        self.write_word(
            ucx::channel_reg(channel, ucx::offset::OUTPUT_STEREO),
            linked as u16,
        )
    }

    fn ucx_write_channel_settings(
        &self,
        idx: usize,
        is_output: bool,
        settings: &ChannelSettings,
    ) -> Result<(), Error> {
        let channel = if is_output {
            let dsp = ucx::output_pair_dsp(idx / 2)
                .ok_or_else(|| Error::InvalidChannel(format!("output {idx}")))?;
            ucx::output_channel(dsp + idx % 2)
        } else {
            ucx::input_channel(idx)
        };
        let words: Vec<(u16, u16)> = settings
            .registers()
            .into_iter()
            .map(|(off, value)| (ucx::channel_reg(channel, off), value))
            .collect();
        self.write_words(&words)
    }

    fn ucx_write_clock(&self, clock: u16) -> Result<(), Error> {
        self.vendor_out(protocol::req::SETTINGS, clock << 1, 0x0006)
    }

    fn ucx_control_bits(&self, value: u16, mask: u16) -> Result<(), Error> {
        self.vendor_out(protocol::req::SETTINGS, value, mask)
    }

    fn sync_input_eq(&mut self, idx: usize) {
        if !self.is_uc() {
            if let Some(eq) = self.inputs.get(idx).and_then(|i| i.eq) {
                apply_eq_to_settings(&eq, &mut self.input_settings[idx]);
            }
        }
    }

    fn sync_output_eq(&mut self, idx: usize) {
        if !self.is_uc() {
            if let Some(eq) = self.outputs.get(idx).and_then(|o| o.eq) {
                apply_eq_to_settings(&eq, &mut self.output_settings[idx]);
            }
        }
    }

    pub fn firmware(&self) -> u32 {
        self.info.firmware
    }

    pub fn serial(&self) -> &str {
        &self.info.serial
    }

    pub fn hardware_volume(&self) -> Option<f32> {
        self.outputs.first().map(|o| o.volume)
    }

    /// Snapshot the UC values that have an on-demand readback or a queued
    /// front-panel event.  Returns `None` for the UCX, whose readback path
    /// is not implemented yet.
    pub fn readable_state(&self) -> Option<ReadableState> {
        if !self.is_uc() && self.model != Model::Ucx {
            return None;
        }
        let mut input_gain: Vec<(usize, u32)> =
            self.device_input_gain.iter().map(|(&i, &g)| (i, g)).collect();
        input_gain.sort_unstable_by_key(|(i, _)| *i);
        let mut output_volume: Vec<(usize, f32)> = self
            .device_output_volume
            .iter()
            .map(|(&i, &v)| (i, v))
            .collect();
        output_volume.sort_unstable_by_key(|(i, _)| *i);
        if !self.is_uc() {
            // The UCX reports only the gain and output-volume maps.  The
            // options word, references and preamp switches are write-only.
            return Some(ReadableState {
                ucx: true,
                input_gain,
                output_volume,
                ..ReadableState::default()
            });
        }
        Some(ReadableState {
            ucx: false,
            clock_source: self.settings.clock_source.clone(),
            optical_out_spdif: self.settings.optical_out_spdif,
            spdif_professional: self.settings.spdif_professional,
            phantom: [
                self.inputs.first().map(|i| i.phantom).unwrap_or(false),
                self.inputs.get(1).map(|i| i.phantom).unwrap_or(false),
            ],
            pad: [
                self.inputs.get(2).map(|i| i.pad).unwrap_or(false),
                self.inputs.get(3).map(|i| i.pad).unwrap_or(false),
            ],
            instrument: [
                self.inputs.get(2).map(|i| i.instrument).unwrap_or(false),
                self.inputs.get(3).map(|i| i.instrument).unwrap_or(false),
            ],
            input_gain,
            output_volume,
        })
    }

    fn write_uc_reference(&mut self, shift: u16, code: u16) -> Result<(), Error> {
        let d = self.vendor_in(protocol::req::LEVELS, 0, 0, 4)?;
        if d.len() < 4 {
            return Err(Error::Io(std::io::Error::other(
                "short 0x17 read while changing a reference level",
            )));
        }
        let mut word = u32::from_le_bytes([d[0], d[1], d[2], d[3]]) as u16;
        word &= !(0x3 << shift);
        word |= (code & 0x3) << shift;
        self.vendor_out(protocol::req::LEVELS, word, 0xffff)?;
        self.apply_uc_levels(word as u32);
        Ok(())
    }

    /// The UCX settings word (`0x10` / wIndex 1487), composed from the model.
    /// Measured bits (H69–H101), and no others were ever observed:
    /// bits 1–2 clock source, `0x008` word-clock input termination, `0x040`
    /// word-clock out single speed, `0x080` SPDIF professional, `0x100`
    /// EQ for Record *disabled* (active low), `0x400` optical out ADAT
    /// (0 = SPDIF).
    fn ucx_options_word(&self) -> u16 {
        // Index 1487 is a full-word update: leave the clock source in place
        // when an unrelated option is toggled.
        let clock = match self.settings.clock_source.as_str() {
            "SPDIF Coaxial" | "SPDIF" => 1,
            "Optical" | "ADAT" => 2,
            "Word Clock" => 3,
            _ => 0,
        };
        let mut word = (clock & 0x3) << 1;
        if self.settings.word_clock_termination {
            word |= 0x0008;
        }
        if self.settings.word_clock_single_speed {
            word |= 0x0040;
        }
        if self.settings.spdif_professional {
            word |= 0x0080;
        }
        if !self.settings.eq_for_record {
            word |= 0x0100;
        }
        if !self.settings.optical_out_spdif {
            word |= 0x0400;
        }
        word
    }

    fn ucx_write_options(&self) -> Result<(), Error> {
        self.vendor_out(protocol::req::SETTINGS, self.ucx_options_word(), 1487)
    }

    /// UC Gain-tab Line In reference: 0 Lo Gain, 1 +4 dBu, 3 −10 dBV.
    pub fn set_line_in_ref(&mut self, code: u16) -> Result<(), Error> {
        self.write_uc_reference(8, code)
    }

    /// UC Gain-tab Line Out reference: 1 −10 dBV, 2 Hi Gain, 3 +4 dBu.
    pub fn set_line_out_ref(&mut self, code: u16) -> Result<(), Error> {
        if self.is_uc() {
            return self.write_uc_reference(6, code);
        }
        // UCX: one control, three output pairs (0 = −10 dBV, 1 = +4 dBu,
        // 2 = Hi Gain).
        for reg in [0x1208, 0x1308, 0x1408, 0x1508, 0x1608, 0x1708] {
            self.write_word(reg, code)?;
        }
        self.settings.line_out_ref = code;
        Ok(())
    }

    /// UC Gain-tab Phones reference: 1 Hi Gain, 2 −10 dBV, 3 +4 dBu.
    pub fn set_phones_ref(&mut self, code: u16) -> Result<(), Error> {
        if self.is_uc() {
            return self.write_uc_reference(10, code);
        }
        // UCX: the Phones/Main pair has its own reference (`0x1808`).
        self.write_word(0x1808, code)?;
        self.write_word(0x1908, code)?;
        self.settings.phones_ref = code;
        Ok(())
    }

    /// UCX Line In reference for inputs 5–8: one shared group (`0x0407` to
    /// `0x0707`), 0 = −10 dBV.
    pub fn set_line_in_ref_58(&mut self, code: u16) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Line In 5-8 reference does not exist on the UC".into(),
            ));
        }
        for reg in [0x0407, 0x0507, 0x0607, 0x0707] {
            self.write_word(reg, code)?;
        }
        self.settings.line_in_ref_58 = code;
        Ok(())
    }

    pub fn set_spdif_professional(&mut self, on: bool) -> Result<(), Error> {
        self.settings.spdif_professional = on;
        if self.is_uc() {
            self.uc_write_options()
        } else {
            self.ucx_write_options()
        }
    }

    pub fn set_word_clock_single_speed(&mut self, on: bool) -> Result<(), Error> {
        self.settings.word_clock_single_speed = on;
        if self.is_uc() {
            self.uc_write_options()
        } else {
            self.ucx_write_options()
        }
    }

    /// Word-clock input termination (`0x008` in the UCX options word).
    /// The UC has no termination bit.
    pub fn set_word_clock_termination(&mut self, on: bool) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Word clock termination does not exist on the UC".into(),
            ));
        }
        self.settings.word_clock_termination = on;
        self.ucx_write_options()
    }

    /// Re-apply the readable/queued device values after the user chooses
    /// "Device -> TuxMix".  Unreadable matrix, master, loopback and mute
    /// values stay at the TuxMix scene.
    pub fn restore_readable(&mut self, state: &ReadableState) -> Result<(), Error> {
        if !self.is_uc() {
            // UCX: only the gain and output-volume maps are read back.
            for &(idx, gain) in &state.input_gain {
                self.set_gain(idx, gain)?;
            }
            for &(idx, volume) in &state.output_volume {
                self.set_volume(ChannelId::Output(idx), 0, volume)?;
            }
            return Ok(());
        }
        if !state.clock_source.is_empty() {
            self.set_clock_source(&state.clock_source)?;
        }
        self.set_optical_out_format(state.optical_out_spdif)?;
        self.settings.spdif_professional = state.spdif_professional;
        self.uc_write_options()?;
        for i in 0..2 {
            self.set_phantom(i, state.phantom[i])?;
        }
        for (slot, i) in (2..4).enumerate() {
            self.set_pad(i, state.pad[slot])?;
            self.set_instrument(i, state.instrument[slot])?;
        }
        for &(idx, gain) in &state.input_gain {
            self.set_gain(idx, gain)?;
        }
        for &(idx, volume) in &state.output_volume {
            self.set_volume(ChannelId::Output(idx), 0, volume)?;
        }
        Ok(())
    }

    pub fn playback_meters(&self) -> Option<Vec<f32>> {
        match self.model {
            Model::Ucx => Some(
                (0..self.playbacks.len())
                    .map(|i| self.meters.playback(i))
                    .collect(),
            ),
            Model::Uc => None,
        }
    }

    pub fn output_meters(&self, post: bool) -> Option<Vec<f32>> {
        match self.model {
            Model::Ucx => Some(
                (0..self.outputs.len())
                    .map(|i| self.meters.output(i, post))
                    .collect(),
            ),
            Model::Uc => None,
        }
    }

    /// Output meters in TuxMix's internal (Phones-first) channel order.
    /// The device reports them in display/DSP order (AN1/2 first), so the
    /// profile's display order maps each pair back.  Used by the GUI when
    /// the unit supplies real post-fader meters.
    pub fn output_meters_internal(&self, post: bool) -> Option<Vec<f32>> {
        let raw = self.output_meters(post)?;
        Some(reorder_output_meters(
            &raw,
            self.spec.profile.output_order,
        ))
    }

    fn uc_status_event(&mut self, data: &[u8]) {
        if !self.is_uc() || data.len() < 4 {
            return;
        }
        // Byte 3 bit 7 is the output-latch flag, not an encoder channel.
        if data[3] & 0x80 != 0 {
            return;
        }
        let chan = (data[3] & 0x1f) as usize;
        let raw = (data[2] & 0x7f) as u32;
        if chan <= 0x15 {
            log::debug!(
                "UC 0x8b channel={chan} value={raw} both={}",
                data[2] & 0x80 != 0
            );
        }
        if chan < 4 {
            let max = if chan < 2 { 65 } else { 36 };
            let gain = raw.min(max) as u32;
            if let Some(input) = self.inputs.get_mut(chan) {
                // Inputs 1/2 report dB; inputs 3/4 report half-dB units,
                // which is exactly the model's raw gain representation.
                input.gain = Some(gain);
            }
            self.device_input_gain.insert(chan, gain);
            return;
        }
        if !(4..=21).contains(&chan) {
            return;
        }
        if let Some(until) = self.ignore_output_until {
            if Instant::now() < until {
                log::debug!(
                    "UC startup: ignoring queued output status channel={chan} value={raw}"
                );
                return;
            }
        }
        let order = self.spec.profile.output_order;
        let Some(idx) = uc::output_from_encoder(chan as u8, order) else {
            return;
        };
        let volume = uc::output_linear(raw as u8);
        if let Some(output) = self.outputs.get_mut(idx) {
            output.volume = volume;
        }
        self.device_output_volume.insert(idx, volume);
        // Bit 7 in the value byte means the unit applied the level to both
        // sides of the encoder pair.
        if data[2] & 0x80 != 0 {
            if let Some(output) = self.outputs.get_mut(idx ^ 1) {
                output.volume = volume;
            }
            self.device_output_volume.insert(idx ^ 1, volume);
        }
    }

    fn apply_ucx_state_word(&mut self, addr: u16, value: u16) {
        if let Some(idx) = ucx::input_gain_index(addr) {
            // The unit reports physical gain moves here in 0.1 dB units.
            // Inputs 1/2 use whole dB; inputs 3/4 use 0.5 dB steps.
            let raw = value as u32;
            let (gain, max) = if idx < 2 {
                ((raw + 5) / 10, 65)
            } else {
                ((raw + 2) / 5, 24)
            };
            let gain = gain.min(max);
            if let Some(input) = self.inputs.get_mut(idx) {
                input.gain = Some(gain);
            }
            self.device_input_gain.insert(idx, gain);
            return;
        }
        if let Some(idx) = ucx::output_index_from_state(addr) {
            let volume = ucx::tenths_to_linear(value);
            if let Some(output) = self.outputs.get_mut(idx) {
                output.volume = volume;
            }
            self.device_output_volume.insert(idx, volume);
            return;
        }
        match addr {
            ucx::global::DIM => self.settings.dim = value != 0,
            ucx::global::MONITOR_FLAGS => {
                self.settings.ms_proc = value & 0x02 != 0;
            }
            _ => {}
        }
    }

    fn drain_events(&mut self) {
        // The queue can hold events accumulated while TuxMix was closed.
        // Drain it completely in bounded batches so a status event from the
        // front panel is not left behind a run of meter packets.
        for _batch in 0..64 {
            let mut events = Vec::new();
            {
                let t = self.transport();
                for _ in 0..256 {
                    match t.wait(Duration::ZERO) {
                        Ok(true) => match t.read_event() {
                            Ok(e) => events.push(e),
                            Err(_) => break,
                        },
                        _ => break,
                    }
                }
            }
            if events.is_empty() {
                break;
            }
            let full_batch = events.len() == 256;
            for event in events {
                match event.kind {
                    EV_METER => {
                        if let Ok(p) = MeterPacket::parse(&event.data) {
                            self.meters.apply(&p);
                        }
                    }
                    EV_STATUS => self.uc_status_event(&event.data),
                    EV_STATE => {
                        // The IF1 state stream is mostly status words.  Decode
                        // the global monitor flags we know; do not guess at
                        // per-channel register meaning from a poll.
                        for w in event.data.chunks_exact(4) {
                            let raw = u32::from_le_bytes(w.try_into().unwrap());
                            let addr = ((raw >> 16) & 0x7fff) as u16;
                            let value = raw as u16;
                            self.apply_ucx_state_word(addr, value);
                        }
                    }
                    EV_LATCH => {
                        // A latch means the device silenced its outputs after
                        // an OUT underrun.  Leave the model untouched; expose
                        // it to the UI as muted hardware?  The kernel
                        // auto-recovers.
                    }
                    _ => {}
                }
            }
            if !full_batch {
                break;
            }
        }
    }

    /// Trim (T) gain factor: 1.0 at 0 dB, lower when the channel is
    /// attenuated.  Applied when a send is written, never stored in the
    /// model.
    fn trim_gain(&self, channel: ChannelId) -> f32 {
        let db = match channel {
            ChannelId::Input(i) => self.inputs.get(i).map(|c| c.trim).unwrap_or(0.0),
            ChannelId::Playback(i) => self.playbacks.get(i).map(|c| c.trim).unwrap_or(0.0),
            ChannelId::Output(_) => 0.0,
        };
        10f32.powf(db.clamp(-65.0, 0.0) / 20.0)
    }

    /// The trim factor for one send into `output`.  A No-Trim destination
    /// is excluded, so the send keeps its untrimmed level.
    fn dest_trim(&self, channel: ChannelId, output: usize) -> f32 {
        if self.outputs.get(output).is_some_and(|o| o.no_trim) {
            1.0
        } else {
            self.trim_gain(channel)
        }
    }

    fn apply_input_volume(&mut self, idx: usize, output: usize, volume: f32) -> Result<(), Error> {
        if idx >= self.inputs.len() || output >= PAIRS {
            return Err(Error::InvalidChannel(format!("input {idx}/{output}")));
        }
        let volume = volume.clamp(0.0, 1.0);
        self.inputs[idx].volumes[output] = volume;
        self.clear_mono_input_cells(idx);
        if self.muted.contains(&ChannelId::Input(idx)) {
            if let Some((volumes, _)) = self.mute_saved.get_mut(&ChannelId::Input(idx)) {
                if let Some(slot) = volumes.get_mut(output) {
                    *slot = volume;
                }
            }
            return Ok(());
        }
        if self.is_uc() {
            self.uc_write_input(idx, output, volume)?;
        } else {
            self.ucx_write_input(idx, output, volume)?;
        }
        Ok(())
    }

    fn apply_playback_volume(
        &mut self,
        idx: usize,
        output: usize,
        volume: f32,
    ) -> Result<(), Error> {
        if idx >= self.playbacks.len() || output >= PAIRS {
            return Err(Error::InvalidChannel(format!("playback {idx}/{output}")));
        }
        let volume = volume.clamp(0.0, 1.0);
        self.playbacks[idx].volumes[output] = volume;
        if self.muted.contains(&ChannelId::Playback(idx)) {
            if let Some((volumes, _)) = self.mute_saved.get_mut(&ChannelId::Playback(idx)) {
                if let Some(slot) = volumes.get_mut(output) {
                    *slot = volume;
                }
            }
            return Ok(());
        }
        if self.is_uc() {
            self.uc_write_playback(idx, output, volume)?;
        } else {
            self.ucx_write_playback(idx, output, volume)?;
        }
        Ok(())
    }
}

impl RmeDevice for Fireface {
    fn model_name(&self) -> &str {
        self.model.name()
    }

    fn canonical_model(&self) -> &str {
        self.model.name()
    }

    fn capabilities(&self) -> crate::capabilities::DeviceCapabilities {
        self.spec.capabilities
    }

    fn protocol_features(&self) -> bool {
        self.spec.capabilities.protocol_features
    }

    fn loopback_supported(&self) -> bool {
        self.spec.capabilities.loopback
    }

    fn phase_supported(&self) -> bool {
        self.spec.capabilities.phase
    }

    fn output_balance_supported(&self) -> bool {
        self.spec.capabilities.output_balance
    }

    fn stereo_split_supported(&self) -> bool {
        self.spec.capabilities.stereo_split
    }

    fn class_compliant(&self) -> bool {
        self.spec.capabilities.class_compliant
    }

    fn is_connected(&self) -> bool {
        self.transport().is_connected()
    }

    fn output_pair_count(&self) -> usize {
        PAIRS
    }

    fn output_display_order(&self) -> Vec<usize> {
        self.spec.profile.output_order.to_vec()
    }

    fn matrix_uses_display_order(&self) -> bool {
        self.is_uc()
    }

    fn open() -> Result<Self, Error>
    where
        Self: Sized,
    {
        Fireface::open()
    }

    fn inputs(&self) -> &[InputChannel] {
        &self.inputs
    }

    fn inputs_mut(&mut self) -> &mut [InputChannel] {
        &mut self.inputs
    }

    fn playbacks(&self) -> &[PlaybackChannel] {
        &self.playbacks
    }

    fn playbacks_mut(&mut self) -> &mut [PlaybackChannel] {
        &mut self.playbacks
    }

    fn outputs(&self) -> &[OutputChannel] {
        &self.outputs
    }

    fn outputs_mut(&mut self) -> &mut [OutputChannel] {
        &mut self.outputs
    }

    fn settings(&self) -> &DeviceSettings {
        &self.settings
    }

    fn settings_mut(&mut self) -> &mut DeviceSettings {
        &mut self.settings
    }

    fn set_output_volume(
        &mut self,
        pair: usize,
        which: usize,
        volume: f32,
    ) -> Result<(), Error> {
        let (left, right) = (pair * 2, pair * 2 + 1);
        if left >= self.outputs.len() || right >= self.outputs.len() {
            return Err(Error::InvalidChannel(format!("output pair {pair}")));
        }
        if !self.output_linked(pair) {
            return self.set_volume(
                ChannelId::Output(if which == 0 { left } else { right }),
                0,
                volume,
            );
        }
        if !self.is_uc() {
            self.set_volume(ChannelId::Output(left), 0, volume)?;
            return self.set_volume(ChannelId::Output(right), 0, volume);
        }
        // The UC output fader is a pair level; balance is the difference in
        // the two channel volumes.  Move both by the same dB offset so a
        // fader drag cannot flatten an existing balance.
        let target = volume.clamp(0.0, 2.0);
        let current = self.outputs[left]
            .volume
            .max(self.outputs[right].volume);
        let (new_left, new_right) = if target <= 0.0 {
            (0.0, 0.0)
        } else if current <= 0.0 {
            (target, target)
        } else {
            let delta = linear_db(target) - linear_db(current);
            (
                db_to_linear(linear_db(self.outputs[left].volume) + delta).clamp(0.0, 2.0),
                db_to_linear(linear_db(self.outputs[right].volume) + delta).clamp(0.0, 2.0),
            )
        };
        self.set_volume(ChannelId::Output(left), 0, new_left)?;
        self.set_volume(ChannelId::Output(right), 0, new_right)
    }

    fn set_volume(
        &mut self,
        channel: ChannelId,
        output: usize,
        volume: f32,
    ) -> Result<(), Error> {
        // Hardware outputs run to +6 dB, like TotalMix's output faders.
        // Crosspoints and software playback stay at or below 0 dB.
        let v = match channel {
            ChannelId::Output(_) => volume.clamp(0.0, 2.0),
            _ => volume.clamp(0.0, 1.0),
        };
        match channel {
            ChannelId::Output(idx) => {
                if idx >= BLOCKS {
                    return Err(Error::InvalidChannel(format!("output {idx}")));
                }
                if self.outputs[idx].mute {
                    self.outputs[idx].volume = v;
                    self.output_mute_saved.insert(idx, v);
                    return Ok(());
                }
                if self.is_uc() {
                    self.uc_write_output_volume(idx, v)?;
                    self.uc_write_output_master(idx, v)?;
                } else {
                    self.ucx_write_output_volume(idx, v)?;
                }
                self.outputs[idx].volume = v;
            }
            ChannelId::Input(idx) => self.apply_input_volume(idx, output, v)?,
            ChannelId::Playback(idx) => self.apply_playback_volume(idx, output, v)?,
        }
        Ok(())
    }

    fn volume(&self, channel: ChannelId, output: usize) -> Result<f32, Error> {
        match channel {
            ChannelId::Output(idx) => self
                .outputs
                .get(idx)
                .map(|o| o.volume)
                .ok_or_else(|| Error::InvalidChannel(format!("output {idx}"))),
            ChannelId::Input(idx) => self
                .inputs
                .get(idx)
                .and_then(|c| c.volumes.get(output))
                .copied()
                .ok_or_else(|| Error::InvalidChannel(format!("input {idx}/{output}"))),
            ChannelId::Playback(idx) => self
                .playbacks
                .get(idx)
                .and_then(|c| c.volumes.get(output))
                .copied()
                .ok_or_else(|| Error::InvalidChannel(format!("playback {idx}/{output}"))),
        }
    }

    fn matrix_cells_supported(&self) -> bool {
        self.spec.capabilities.matrix_cells
    }

    fn begin_metering(&mut self) {
        if self.is_uc() && self.input_tap.is_none() {
            let selector = if self.info.serial.is_empty() {
                self.info.name.clone()
            } else {
                self.info.serial.clone()
            };
            self.input_tap = Some(crate::pw_input::InputTap::start(&selector));
        }
    }

    fn matrix_cell(&self, channel: ChannelId, block: usize) -> Option<f32> {
        if !self.is_uc() {
            return self.volume(channel, block / 2).ok();
        }
        let source = Self::matrix_source_index(channel)?;
        if let Some(gain) = self.matrix_overrides.get(&(source, block)) {
            return Some(*gain);
        }
        self.matrix_default(channel, block)
    }

    fn set_matrix_cell(
        &mut self,
        channel: ChannelId,
        block: usize,
        gain: f32,
    ) -> Result<(), Error> {
        if block >= BLOCKS {
            return Err(Error::InvalidChannel(format!("output block {block}")));
        }
        let gain = gain.clamp(0.0, 1.0);
        let source = Self::matrix_source_index(channel)
            .ok_or_else(|| Error::InvalidChannel("output is not a matrix source".into()))?;
        if !self.is_uc() {
            return self.set_volume(channel, block / 2, gain);
        }
        self.uc_write_matrix(block, source, gain * self.trim_gain(channel))?;
        if Self::matrix_natural(channel, block) {
            let pair = block / 2;
            match channel {
                ChannelId::Input(i) => {
                    if let Some(slot) =
                        self.inputs.get_mut(i).and_then(|c| c.volumes.get_mut(pair))
                    {
                        *slot = gain;
                    }
                }
                ChannelId::Playback(i) => {
                    if let Some(slot) = self
                        .playbacks
                        .get_mut(i)
                        .and_then(|c| c.volumes.get_mut(pair))
                    {
                        *slot = gain;
                    }
                }
                ChannelId::Output(_) => {}
            }
            self.matrix_overrides.remove(&(source, block));
        } else if self
            .matrix_default(channel, block)
            .is_some_and(|default| (default - gain).abs() < 1e-6)
        {
            self.matrix_overrides.remove(&(source, block));
        } else {
            self.matrix_overrides.insert((source, block), gain);
        }
        Ok(())
    }

    fn set_pan(&mut self, channel: ChannelId, output: usize, pan: i8) -> Result<(), Error> {
        let pan = pan.clamp(-100, 100);
        match channel {
            ChannelId::Input(idx) => {
                if idx >= self.inputs.len() || output >= PAIRS {
                    return Err(Error::InvalidChannel(format!("input {idx}/{output}")));
                }
                self.inputs[idx].pans[output] = pan;
                self.clear_mono_input_cells(idx);
                if idx >= 4 {
                    self.inputs[idx ^ 1].pans[output] = pan;
                }
                if self.muted.contains(&ChannelId::Input(idx)) {
                    if let Some((_, pans)) = self.mute_saved.get_mut(&ChannelId::Input(idx)) {
                        if let Some(slot) = pans.get_mut(output) {
                            *slot = pan;
                        }
                    }
                    return Ok(());
                }
                let volume = self.inputs[idx].volumes[output];
                if self.is_uc() {
                    self.uc_write_input(idx, output, volume)?;
                } else {
                    self.ucx_write_input(idx, output, volume)?;
                }
            }
            ChannelId::Playback(idx) => {
                if idx >= self.playbacks.len() || output >= PAIRS {
                    return Err(Error::InvalidChannel(format!("playback {idx}/{output}")));
                }
                self.playbacks[idx].pans[output] = pan;
                let linked = self.playback_linked(idx / 2);
                if linked {
                    self.playbacks[idx ^ 1].pans[output] = pan;
                }
                if self.muted.contains(&ChannelId::Playback(idx)) {
                    for channel in [idx, if linked { idx ^ 1 } else { idx }] {
                        if let Some((_, pans)) =
                            self.mute_saved.get_mut(&ChannelId::Playback(channel))
                        {
                            if let Some(slot) = pans.get_mut(output) {
                                *slot = pan;
                            }
                        }
                    }
                    return Ok(());
                }
                if self.is_uc() {
                    let volume = self.playbacks[idx].volumes[output];
                    self.uc_write_playback(idx, output, volume)?;
                    if linked {
                        let other = idx ^ 1;
                        let volume = self.playbacks[other].volumes[output];
                        self.uc_write_playback(other, output, volume)?;
                    }
                }
            }
            ChannelId::Output(idx) => {
                if !self.output_balance_supported() || idx >= self.outputs.len() {
                    return Ok(());
                }
                let pair = idx / 2;
                let left = pair * 2;
                let right = left + 1;
                if self.is_uc() {
                    // The UC has no balance register: balance is the
                    // difference between the pair's two volumes, so a split
                    // pair has none.
                    if !self.output_linked(pair) {
                        return Ok(());
                    }
                    let current = self.outputs[left]
                        .volume
                        .max(self.outputs[right].volume);
                    let (gl, gr) = pan_split_output(current, pan);
                    self.outputs[left].volume = gl;
                    self.outputs[right].volume = gr;
                    if self.outputs[left].mute || self.outputs[right].mute {
                        self.output_mute_saved.insert(left, gl);
                        self.output_mute_saved.insert(right, gr);
                        return Ok(());
                    }
                    self.uc_write_output_volume(left, gl)?;
                    self.uc_write_output_master_banks(left, gl)?;
                    self.uc_write_output_volume(right, gr)?;
                    self.uc_write_output_master_banks(right, gr)?;
                } else {
                    // UCX: balance is its own register, independent of the
                    // two output volumes (measured `0xNN01`).
                    self.outputs[left].balance = pan;
                    self.outputs[right].balance = pan;
                    for ch in [left, right] {
                        let dsp = ucx::output_pair_dsp(ch / 2)
                            .ok_or_else(|| Error::InvalidChannel(format!("output {ch}")))?;
                        let channel = ucx::output_channel(dsp + ch % 2);
                        self.write_word(
                            ucx::channel_reg(channel, ucx::offset::FX_SEND_OR_BALANCE),
                            pan as i16 as u16,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn pan(&self, channel: ChannelId, output: usize) -> Result<i8, Error> {
        match channel {
            ChannelId::Input(idx) => self
                .inputs
                .get(idx)
                .and_then(|c| c.pans.get(output))
                .copied()
                .ok_or_else(|| Error::InvalidChannel(format!("input {idx}/{output}"))),
            ChannelId::Playback(idx) => self
                .playbacks
                .get(idx)
                .and_then(|c| c.pans.get(output))
                .copied()
                .ok_or_else(|| Error::InvalidChannel(format!("playback {idx}/{output}"))),
            ChannelId::Output(idx) => {
                if !self.output_balance_supported() || !self.output_linked(idx / 2) {
                    return Ok(0);
                }
                if self.is_uc() {
                    let left = idx & !1;
                    let right = left + 1;
                    Ok(output_balance(
                        self.outputs[left].volume,
                        self.outputs[right].volume,
                    ))
                } else {
                    Ok(self.outputs.get(idx).map(|o| o.balance).unwrap_or(0))
                }
            }
        }
    }

    fn set_input_pair_linked(&mut self, pair: usize, linked: bool) -> Result<(), Error> {
        let (left, right) = (pair * 2, pair * 2 + 1);
        if !self.is_uc() {
            self.ucx_write_input_stereo(left, linked)?;
            self.ucx_write_input_stereo(right, linked)?;
        }
        let saved = self.inputs.get(left).cloned();
        {
            let v = &mut self.settings.input_pair_link;
            if v.len() <= pair {
                v.resize(pair + 1, true);
            }
            v[pair] = linked;
        }
        if let Some(saved) = saved {
            if linked {
                for output in 0..self.output_pair_count() {
                    let volume = saved.volumes.get(output).copied().unwrap_or(1.0);
                    self.set_volume(ChannelId::Input(right), output, volume)?;
                }
                self.set_mute(ChannelId::Input(right), saved.mute)?;
                self.set_solo(ChannelId::Input(right), saved.solo)?;
                if saved.channel_type == ChannelType::Mic {
                    self.set_input_phantom(left, saved.phantom)?;
                }
                if saved.has_pad {
                    self.set_input_pad(left, saved.pad)?;
                }
                if saved.has_instrument {
                    self.set_input_instrument(left, saved.instrument)?;
                }
                if let Some(gain) = saved.gain {
                    self.set_input_gain(left, gain)?;
                }
                if let Some(level) = saved.level {
                    if !self.is_uc() {
                        self.set_input_level(right, level)?;
                        self.set_input_level(left, level)?;
                    }
                }
                if !self.is_uc() && left < 2 {
                    self.set_autoset(right, saved.autoset)?;
                    self.set_autoset(left, saved.autoset)?;
                }
            }
        }
        for output in 0..self.output_pair_count() {
            if linked {
                self.set_pan(ChannelId::Input(left), output, 0)?;
                self.set_pan(ChannelId::Input(right), output, 0)?;
            } else {
                self.set_pan(ChannelId::Input(left), output, -100)?;
                self.set_pan(ChannelId::Input(right), output, 100)?;
            }
        }
        Ok(())
    }

    fn set_output_linked(&mut self, pair: usize, linked: bool) -> Result<(), Error> {
        let (left, right) = (pair * 2, pair * 2 + 1);
        if !self.is_uc() {
            self.ucx_write_output_stereo(left, linked)?;
            self.ucx_write_output_stereo(right, linked)?;
        }
        let saved_mute = self.outputs.get(left).map(|o| o.mute);
        {
            let v = &mut self.settings.output_link;
            if v.len() <= pair {
                v.resize(pair + 1, true);
            }
            v[pair] = linked;
        }
        if linked {
            if let Some(mute) = saved_mute {
                self.set_mute(ChannelId::Output(right), mute)?;
            }
            self.set_pan(ChannelId::Output(left), 0, 0)?;
        }
        Ok(())
    }

    fn set_mute(&mut self, channel: ChannelId, mute: bool) -> Result<(), Error> {
        match channel {
            ChannelId::Output(idx) => {
                if idx >= BLOCKS {
                    return Err(Error::InvalidChannel(format!("output {idx}")));
                }
                if mute {
                    let saved = self.outputs[idx].volume;
                    self.output_mute_saved.entry(idx).or_insert(saved);
                    if self.is_uc() {
                        self.uc_write_output_volume(idx, 0.0)?;
                        // The `0x12` master is the copy that survives an
                        // engine restart; mute it too, not just the display
                        // code.
                        self.uc_write_output_master(idx, 0.0)?;
                    } else {
                        self.ucx_write_output_mute(idx, true)?;
                    }
                } else {
                    if let Some(saved) = self.output_mute_saved.remove(&idx) {
                        if self.is_uc() {
                            self.uc_write_output_volume(idx, saved)?;
                            self.uc_write_output_master(idx, saved)?;
                        } else {
                            self.ucx_write_output_mute(idx, false)?;
                        }
                        self.outputs[idx].volume = saved;
                    } else if !self.is_uc() {
                        self.ucx_write_output_mute(idx, false)?;
                    }
                }
                self.outputs[idx].mute = mute;
            }
            ChannelId::Input(idx) => {
                if idx >= self.inputs.len() {
                    return Err(Error::InvalidChannel(format!("input {idx}")));
                }
                if mute {
                    if !self.is_uc() {
                        self.write_word(
                            ucx::channel_reg(ucx::input_channel(idx), ucx::offset::MUTE_OR_FADER),
                            1,
                        )?;
                    }
                    let saved = (
                        self.inputs[idx].volumes.clone(),
                        self.inputs[idx].pans.clone(),
                    );
                    self.mute_saved.insert(channel, saved);
                    self.muted.insert(channel);
                    for p in 0..PAIRS {
                        let volume = self.inputs[idx].volumes[p];
                        self.apply_input_volume(idx, p, 0.0)?;
                        self.inputs[idx].volumes[p] = volume;
                    }
                } else if let Some((volumes, pans)) = self.mute_saved.remove(&channel) {
                    self.muted.remove(&channel);
                    self.inputs[idx].pans.clone_from(&pans);
                    for (p, volume) in volumes.iter().enumerate().take(PAIRS) {
                        self.apply_input_volume(idx, p, *volume)?;
                    }
                    if !self.is_uc() {
                        self.write_word(
                            ucx::channel_reg(ucx::input_channel(idx), ucx::offset::MUTE_OR_FADER),
                            0,
                        )?;
                    }
                }
                self.inputs[idx].mute = mute;
            }
            ChannelId::Playback(idx) => {
                if idx >= self.playbacks.len() {
                    return Err(Error::InvalidChannel(format!("playback {idx}")));
                }
                if mute {
                    let saved = self.playbacks[idx].volumes.clone();
                    self.mute_saved.insert(channel, (saved.clone(), vec![0; PAIRS]));
                    self.muted.insert(channel);
                    for p in 0..PAIRS {
                        let volume = self.playbacks[idx].volumes[p];
                        self.apply_playback_volume(idx, p, 0.0)?;
                        self.playbacks[idx].volumes[p] = volume;
                    }
                } else if let Some((volumes, _)) = self.mute_saved.remove(&channel) {
                    self.muted.remove(&channel);
                    for (p, volume) in volumes.iter().enumerate().take(PAIRS) {
                        self.apply_playback_volume(idx, p, *volume)?;
                    }
                }
                self.playbacks[idx].mute = mute;
            }
        }
        Ok(())
    }

    fn mute(&self, channel: ChannelId) -> Result<bool, Error> {
        match channel {
            ChannelId::Output(idx) => self
                .outputs
                .get(idx)
                .map(|o| o.mute)
                .ok_or_else(|| Error::InvalidChannel(format!("output {idx}"))),
            ChannelId::Input(idx) => self
                .inputs
                .get(idx)
                .map(|c| c.mute)
                .ok_or_else(|| Error::InvalidChannel(format!("input {idx}"))),
            ChannelId::Playback(idx) => self
                .playbacks
                .get(idx)
                .map(|c| c.mute)
                .ok_or_else(|| Error::InvalidChannel(format!("playback {idx}"))),
        }
    }

    fn set_solo(&mut self, channel: ChannelId, solo: bool) -> Result<(), Error> {
        if solo {
            self.soloed.insert(channel);
        } else {
            self.soloed.remove(&channel);
        }
        match channel {
            ChannelId::Output(idx) => {
                if let Some(o) = self.outputs.get_mut(idx) {
                    o.solo = solo;
                }
            }
            ChannelId::Input(idx) => {
                if let Some(c) = self.inputs.get_mut(idx) {
                    c.solo = solo;
                }
            }
            ChannelId::Playback(idx) => {
                if let Some(c) = self.playbacks.get_mut(idx) {
                    c.solo = solo;
                }
            }
        }
        Ok(())
    }

    fn solo(&self, channel: ChannelId) -> Result<bool, Error> {
        Ok(self.soloed.contains(&channel))
    }

    fn set_phantom(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        if self.is_uc() {
            if idx < 2 {
                self.uc_write_switch(1 << idx, on)?;
                if let Some(input) = self.inputs.get_mut(idx) {
                    input.phantom = on;
                }
                return Ok(());
            }
            return Err(Error::InvalidChannel(format!(
                "input {idx} has no phantom switch on the UC"
            )));
        }
        // UCX phantom power is bulk `NN08`, inputs 1/2 only (measured).
        if idx < 2 {
            self.ucx_write_input_48v(idx, on)?;
            if let Some(input) = self.inputs.get_mut(idx) {
                input.phantom = on;
            }
            return Ok(());
        }
        Err(Error::InvalidChannel(format!(
            "input {idx} has no phantom switch on the UCX"
        )))
    }

    fn set_pad(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        if !self.is_uc() {
            return Err(Error::InvalidChannel(
                "the UCX has no pad; it has Hi-Z on AN3/4".into(),
            ));
        }
        if (2..4).contains(&idx) {
            self.uc_write_switch(1 << (5 - idx), on)?;
            if let Some(input) = self.inputs.get_mut(idx) {
                input.pad = on;
            }
            return Ok(());
        }
        Err(Error::InvalidChannel(format!("input {idx} has no pad")))
    }

    fn set_instrument(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        if !(2..4).contains(&idx) {
            return Err(Error::InvalidChannel(format!(
                "input {idx} has no instrument switch"
            )));
        }
        if self.is_uc() {
            self.uc_write_switch(1 << (7 - idx), on)?;
        } else {
            self.ucx_write_input_hiz(idx, on)?;
        }
        if let Some(input) = self.inputs.get_mut(idx) {
            input.instrument = on;
        }
        Ok(())
    }

    fn set_gain(&mut self, idx: usize, gain: u32) -> Result<(), Error> {
        if idx >= 4 {
            return Err(Error::InvalidChannel(format!("input {idx} has no preamp")));
        }
        // Inputs 1/2 use whole dB (0..65).  Inputs 3/4 use half-dB units:
        // 0..36 on the UC, 0..24 on the UCX.  The UCX wire value is 0.1 dB.
        let max = if idx < 2 {
            65
        } else if self.is_uc() {
            36
        } else {
            24
        };
        let gain = gain.min(max);
        if self.is_uc() {
            self.vendor_out(protocol::req::PGA, gain as u16, idx as u16)?;
        } else {
            let tenths = if idx < 2 { gain * 10 } else { gain * 5 };
            self.ucx_write_input_gain(idx, tenths)?;
        }
        if let Some(input) = self.inputs.get_mut(idx) {
            input.gain = Some(gain);
        }
        Ok(())
    }

    fn set_pitch(&mut self, _pitch_percent: f32) -> Result<(), Error> {
        Err(Error::InvalidChannel(
            "varispeed pitch is not mapped for the UC/UCX".into(),
        ))
    }

    fn set_sample_rate(&mut self, rate: u32) -> Result<(), Error> {
        self.settings.sample_rate = rate;
        if self.is_uc() {
            // The UC's rate follows the host stream through the ALSA path;
            // nothing is written here.
            return Ok(());
        }
        // Measured H82–H98: `0x10` wIndex 48 carries the base family only,
        // and 2x/4x reuse it (the multiplier comes from the streaming
        // alternate setting). The word is sticky, so it must be written
        // explicitly — a mixer snapshot does not set the rate.
        let word = match rate {
            32_000 => 0x0000,
            44_100 | 88_200 | 176_400 => 0x0010,
            48_000 | 96_000 | 192_000 => 0x0020,
            _ => return Ok(()),
        };
        self.vendor_out(protocol::req::SETTINGS, word, 48)?;
        Ok(())
    }

    fn set_loopback(&mut self, out: usize, on: bool) -> Result<(), Error> {
        if out >= PAIRS {
            return Err(Error::InvalidChannel(format!("output pair {out}")));
        }
        if self.is_uc() {
            for side in 0..2 {
                let block =
                    uc::matrix_block(out * 2 + side, self.output_order());
                self.vendor_out(
                    protocol::req::LOOPBACK,
                    on as u16,
                    block as u16,
                )?;
            }
        } else {
            let dsp = ucx::output_pair_dsp(out)
                .ok_or_else(|| Error::InvalidChannel(format!("output pair {out}")))?;
            for side in 0..2 {
                self.vendor_out(
                    protocol::req::LOOPBACK,
                    on as u16,
                    (dsp + side) as u16,
                )?;
            }
        }
        self.outputs[out * 2].loopback = on;
        self.outputs[out * 2 + 1].loopback = on;
        Ok(())
    }

    fn set_loopback_channel(&mut self, channel: usize, on: bool) -> Result<(), Error> {
        if channel >= BLOCKS {
            return Err(Error::InvalidChannel(format!("output {channel}")));
        }
        if !self.is_uc() {
            return self.set_loopback(channel / 2, on);
        }
        // TotalMix loops a stereo hardware output as a unit.  A linked
        // AN3/4 pair therefore loops both sides, which is the documented
        // behavior, not a bug.  Only a split (unlinked) pair can loop one
        // side on its own.
        if self.output_linked(channel / 2) {
            return self.set_loopback(channel / 2, on);
        }
        let block = uc::matrix_block(channel, self.output_order());
        self.vendor_out(protocol::req::LOOPBACK, on as u16, block as u16)?;
        self.outputs[channel].loopback = on;
        Ok(())
    }

    fn set_ms_proc(&mut self, on: bool) -> Result<(), Error> {
        self.settings.ms_proc = on;
        Ok(())
    }

    fn set_an12(&mut self, on: bool) -> Result<(), Error> {
        self.settings.an12 = on;
        Ok(())
    }

    fn set_dim(&mut self, on: bool) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Dim is not decoded on the UC".into(),
            ));
        }
        if on == self.settings.dim {
            return Ok(());
        }
        if on {
            // Dim applies to the Phones pair.  TuxMix uses a fixed -20 dB
            // offset; the RME dialog amount is not read back.
            let left = self.outputs[0].volume;
            let right = self.outputs[1].volume;
            self.dim_saved = Some((left, right));
            let db = |v: f32| {
                if v <= 0.0 {
                    0.0
                } else {
                    db_to_linear(linear_db(v) - 20.0)
                }
            };
            self.set_volume(ChannelId::Output(0), 0, db(left))?;
            self.set_volume(ChannelId::Output(1), 0, db(right))?;
            self.write_word(ucx::global::DIM, 1)?;
        } else {
            if let Some((left, right)) = self.dim_saved.take() {
                self.set_volume(ChannelId::Output(0), 0, left)?;
                self.set_volume(ChannelId::Output(1), 0, right)?;
            }
            self.write_word(ucx::global::DIM, 0)?;
        }
        self.settings.dim = on;
        Ok(())
    }

    /// Control Room Assign: the Main Out is marked by bit 1 of the output
    /// mode word (`0xNN02`), and only one pair carries it. The old pair's
    /// bit is cleared, the new one's set; each side of the pair gets its
    /// own word, composed with that channel's mute (capture H51/H52).
    fn set_main_out(&mut self, pair: Option<usize>) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Assign is not decoded on the UC".into(),
            ));
        }
        let Some(pair) = pair else {
            return Ok(());
        };
        if pair >= BLOCKS / 2 {
            return Err(Error::InvalidChannel(format!("output pair {pair}")));
        }
        let previous = self.settings.main_out;
        if previous == Some(pair) {
            return Ok(());
        }
        self.settings.main_out = Some(pair);
        if let Some(old) = previous {
            if old < BLOCKS / 2 {
                let mute = self.outputs.get(old * 2).map(|o| o.mute).unwrap_or(false);
                self.ucx_write_output_mode(old * 2, mute)?;
                self.ucx_write_output_mode(old * 2 + 1, mute)?;
            }
        }
        let mute = self.outputs.get(pair * 2).map(|o| o.mute).unwrap_or(false);
        self.ucx_write_output_mode(pair * 2, mute)?;
        self.ucx_write_output_mode(pair * 2 + 1, mute)?;
        Ok(())
    }

    /// Assign Main Out B: bit 1 on the chosen pair, plus the control-room
    /// state word `0x3d00 = 3` (capture H60).
    fn set_main_out_b(&mut self, pair: Option<usize>) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Assign is not decoded on the UC".into(),
            ));
        }
        let Some(pair) = pair else {
            return Ok(());
        };
        if pair >= BLOCKS / 2 {
            return Err(Error::InvalidChannel(format!("output pair {pair}")));
        }
        let previous = self.settings.main_b_out;
        if previous == Some(pair) {
            return Ok(());
        }
        self.settings.main_b_out = Some(pair);
        if let Some(old) = previous {
            if old < BLOCKS / 2 && self.settings.main_out != Some(old) {
                let mute = self.outputs.get(old * 2).map(|o| o.mute).unwrap_or(false);
                self.ucx_write_output_mode(old * 2, mute)?;
                self.ucx_write_output_mode(old * 2 + 1, mute)?;
            }
        }
        let mute = self.outputs.get(pair * 2).map(|o| o.mute).unwrap_or(false);
        self.ucx_write_output_mode(pair * 2, mute)?;
        self.ucx_write_output_mode(pair * 2 + 1, mute)?;
        self.write_word(0x3d00, 3)?;
        Ok(())
    }

    /// Control Room Mute FX: the control-room outputs' FX returns drop to
    /// `0xFD76` (−65 dB) while it is engaged, and are restored from the
    /// model afterwards (measured H90; no flag register).
    fn set_mute_fx(&mut self, on: bool) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Mute FX is not decoded on the UC".into(),
            ));
        }
        const MUTED: u16 = 0xFD76;
        for reg in [0x1203u16, 0x1303, 0x1803, 0x1903] {
            let value = if on {
                MUTED
            } else {
                // Restore the model's return for that channel, in 0.1 dB.
                let idx = match reg {
                    0x1203 => 0,
                    0x1303 => 1,
                    0x1803 => 6,
                    _ => 7,
                };
                (self.outputs[idx].fx_return_db * 10.0).round() as i16 as u16
            };
            self.write_word(reg, value)?;
        }
        self.settings.mute_fx = on;
        Ok(())
    }

    fn set_speaker_b(&mut self, on: bool) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Speaker B does not exist on the UC".into(),
            ));
        }
        // Phones/Main mode word plus the control-room state word.
        self.write_word(0x1802, if on { 2 } else { 0 })?;
        self.write_word(0x1902, if on { 2 } else { 0 })?;
        self.write_word(0x3d00, if on { 8 } else { 0 })?;
        self.settings.speaker_b = on;
        Ok(())
    }

    fn set_eq_for_record(&mut self, on: bool) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "EQ for Record does not exist on the UC".into(),
            ));
        }
        // Control register bit 8 is the inverse of the dialog's option.
        self.ucx_control_bits(if on { 0 } else { 0x0100 }, 0x0100)?;
        self.settings.eq_for_record = on;
        Ok(())
    }

    fn set_optical_out_format(&mut self, spdif: bool) -> Result<(), Error> {
        self.settings.optical_out_spdif = spdif;
        if self.is_uc() {
            self.uc_write_options()?;
        } else {
            // Optical format is bit 10 of the settings word, not an index
            // (H101): `0x0400` is ADAT, 0 is SPDIF.
            self.ucx_write_options()?;
        }
        Ok(())
    }

    fn set_cue(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        if let Some(output) = self.outputs.get_mut(idx) {
            output.cue = on;
        }
        Ok(())
    }

    fn set_input_link(&mut self, linked: bool) -> Result<(), Error> {
        self.settings.input_link = linked;
        Ok(())
    }

    fn set_trim(&mut self, channel: ChannelId, db: f32) -> Result<(), Error> {
        let db = db.clamp(-65.0, 0.0);
        let volumes = match channel {
            ChannelId::Input(idx) => {
                let c = self
                    .inputs
                    .get_mut(idx)
                    .ok_or_else(|| Error::InvalidChannel(format!("input {idx}")))?;
                c.trim = db;
                c.volumes.clone()
            }
            ChannelId::Playback(idx) => {
                let c = self
                    .playbacks
                    .get_mut(idx)
                    .ok_or_else(|| Error::InvalidChannel(format!("playback {idx}")))?;
                c.trim = db;
                c.volumes.clone()
            }
            ChannelId::Output(_) => {
                return Err(Error::InvalidChannel(
                    "trim applies to inputs and playbacks".into(),
                ))
            }
        };
        // Rewrite the sends that carry signal so the device picks up the
        // new gain.  Silent sends stay silent and need no write.
        for (out, v) in volumes.iter().enumerate() {
            if *v > 0.0 {
                self.set_volume(channel, out, *v)?;
            }
        }
        Ok(())
    }

    fn set_phase(&mut self, idx: usize, invert: bool) -> Result<(), Error> {
        if idx >= self.inputs.len() {
            return Err(Error::InvalidChannel(format!("input {idx}")));
        }
        if self.is_uc() {
            self.inputs[idx].phase = invert;
            // The UC has no phase register: the sign of every routed
            // matrix cell is the phase.  Rewrite the source's active cells
            // so the change also survives later fader/pan writes, which go
            // through the phase-aware uc_write_matrix().
            let channel = ChannelId::Input(idx);
            for block in 0..BLOCKS {
                let gain = self.matrix_cell(channel, block).unwrap_or(0.0);
                if gain > 0.0 {
                    let trimmed = gain * self.trim_gain(channel);
                    self.uc_write_matrix(block, idx, trimmed)?;
                }
            }
            return Ok(());
        }
        self.ucx_write_input_phase(idx, invert)?;
        self.inputs[idx].phase = invert;
        Ok(())
    }

    fn set_output_phase(&mut self, idx: usize, invert: bool) -> Result<(), Error> {
        // `base + 0x07` / `base + 0x107`: one word per side, no send
        // recompute (measured Q103).
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Output phase is not mapped on the UC".into(),
            ));
        }
        if idx >= self.outputs.len() {
            return Err(Error::InvalidChannel(format!("output {idx}")));
        }
        let dsp = ucx::output_pair_dsp(idx / 2)
            .ok_or_else(|| Error::InvalidChannel(format!("output {idx}")))?;
        let channel = ucx::output_channel(dsp + idx % 2);
        self.write_word(
            ucx::channel_reg(channel, ucx::offset::GAIN_REFLEVEL_OR_PHASE),
            invert as u16,
        )?;
        let out = &mut self.outputs[idx];
        if idx % 2 == 0 {
            out.phase_l = invert;
        } else {
            out.phase_r = invert;
        }
        Ok(())
    }

    fn set_no_trim(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        if idx >= self.outputs.len() {
            return Err(Error::InvalidChannel(format!("output {idx}")));
        }
        self.outputs[idx].no_trim = on;
        // A linked pair is one monitor destination: both sides carry the
        // flag.  A split pair keeps the two mono channels independent.
        if self.output_linked(idx / 2) {
            let other = idx ^ 1;
            if let Some(out) = self.outputs.get_mut(other) {
                out.no_trim = on;
            }
        }
        // Re-write the sends into this output so the exclusion applies to
        // what is already playing, not only to the next fader move.
        let pair = idx / 2;
        let sends: Vec<(ChannelId, f32)> = self
            .inputs
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                c.volumes
                    .get(pair)
                    .filter(|v| **v > 0.0)
                    .map(|v| (ChannelId::Input(i), *v))
            })
            .chain(self.playbacks.iter().enumerate().filter_map(|(i, c)| {
                c.volumes
                    .get(pair)
                    .filter(|v| **v > 0.0)
                    .map(|v| (ChannelId::Playback(i), *v))
            }))
            .collect();
        for (channel, volume) in sends {
            self.set_volume(channel, pair, volume)?;
        }
        Ok(())
    }

    fn set_channel_fx_send(&mut self, channel: ChannelId, db: f32) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Per-channel FX send is not decoded on the UC".into(),
            ));
        }
        let db = db.clamp(-65.0, 0.0);
        match channel {
            ChannelId::Input(idx) => {
                if idx >= self.inputs.len() {
                    return Err(Error::InvalidChannel(format!("input {idx}")));
                }
                // Input send is `NN01`, 0.1 dB signed (measured).
                let ch = ucx::input_channel(idx);
                self.write_word(
                    ucx::channel_reg(ch, ucx::offset::FX_SEND_OR_BALANCE),
                    (db * 10.0).round() as i16 as u16,
                )?;
                self.inputs[idx].fx_send_db = db;
                Ok(())
            }
            ChannelId::Playback(idx) => {
                if idx >= self.playbacks.len() {
                    return Err(Error::InvalidChannel(format!("playback {idx}")));
                }
                // Playback send is the `0x47A0` family, stride +2 per
                // channel, an amplitude word rather than dB (measured H86).
                let linear = 10f32.powf(db / 20.0).clamp(0.0, 1.0);
                let value = ucx::matrix_gain(linear as f64);
                let base = 0x47a0u16 + 2 * idx as u16;
                self.write_word(base, value)?;
                self.write_word(base + 0x41, value)?;
                self.playbacks[idx].fx_send_db = db;
                Ok(())
            }
            ChannelId::Output(_) => Ok(()),
        }
    }

    fn set_stereo_split(&mut self, pb: usize, split: bool) -> Result<(), Error> {
        if pb >= self.playbacks.len() {
            return Err(Error::InvalidChannel(format!("playback {pb}")));
        }
        self.playbacks[pb].split = split;
        self.playbacks[pb ^ 1].split = split;
        Ok(())
    }

    fn set_width(&mut self, width: f32) -> Result<(), Error> {
        self.settings.width = width;
        for w in &mut self.playback_width {
            *w = (width * 100.0).round().clamp(-100.0, 100.0) as i16;
        }
        Ok(())
    }

    fn set_channel_width(&mut self, channel: ChannelId, width: f32) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Channel width is not mapped on the UC".into(),
            ));
        }
        let width = width.clamp(-1.0, 1.0);
        match channel {
            ChannelId::Input(idx) => {
                let base = idx & !1;
                let other = base ^ 1;
                if other >= self.inputs.len() {
                    return Err(Error::InvalidChannel(format!("input {idx}")));
                }
                self.inputs[base].width = width;
                self.inputs[other].width = width;
                for pair in 0..PAIRS {
                    self.ucx_write_input_pair(base, pair)?;
                }
            }
            ChannelId::Playback(idx) => {
                let base = idx & !1;
                let other = base ^ 1;
                if other >= self.playbacks.len() {
                    return Err(Error::InvalidChannel(format!("playback {idx}")));
                }
                self.playbacks[base].width = width;
                self.playbacks[other].width = width;
                for pair in 0..PAIRS {
                    self.ucx_write_playback_pair(base, pair)?;
                }
            }
            ChannelId::Output(_) => {
                return Err(Error::InvalidChannel(
                    "width applies to inputs and playbacks".into(),
                ))
            }
        }
        Ok(())
    }

    fn set_ref_level(&mut self, idx: usize, raw: u16) -> Result<(), Error> {
        if !self.is_uc() && (2..4).contains(&idx) {
            self.ucx_write_input_level(
                idx,
                InputLevel::from_code(raw).unwrap_or(InputLevel::Plus4dBu),
            )?;
        }
        if let Some(input) = self.inputs.get_mut(idx) {
            input.ref_level = raw;
            input.level = InputLevel::from_code(raw);
        }
        Ok(())
    }

    fn set_sensitivity(
        &mut self,
        idx: usize,
        sensitivity: Sensitivity,
    ) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "the UC has no software reference-level switch".into(),
            ));
        }
        let level = match sensitivity {
            Sensitivity::Minus10dBV => InputLevel::Minus10dBV,
            Sensitivity::Plus4dBu => InputLevel::Plus4dBu,
        };
        self.set_input_level(idx, level)
    }

    fn set_input_level(&mut self, idx: usize, level: InputLevel) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "the UC has no software reference-level switch".into(),
            ));
        }
        // Measured on the UCX: Level is `NN08` on inputs 3/4.  The manual
        // also gives Level on 5–8, but that register was not captured.
        if !(2..4).contains(&idx) {
            return Err(Error::InvalidChannel(format!(
                "input {idx} has no measured Level control on the UCX"
            )));
        }
        self.ucx_write_input_level(idx, level)?;
        if let Some(input) = self.inputs.get_mut(idx) {
            input.level = Some(level);
            input.ref_level = level.code();
        }
        Ok(())
    }

    fn set_autoset(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "AutoSet is not decoded on the UC".into(),
            ));
        }
        if idx >= 2 {
            return Err(Error::InvalidChannel(format!(
                "input {idx} has no AutoSet on the UCX"
            )));
        }
        self.ucx_write_autoset(idx, on)?;
        if let Some(input) = self.inputs.get_mut(idx) {
            input.autoset = on;
        }
        Ok(())
    }

    fn set_dynamics(&mut self, channel: ChannelId, dynamics: &Dynamics) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Dynamics do not exist on the UC".into(),
            ));
        }
        self.ucx_write_dynamics(channel, dynamics)?;
        match channel {
            ChannelId::Input(idx) => {
                if let Some(input) = self.inputs.get_mut(idx) {
                    input.dynamics = Some(dynamics.clamped());
                }
            }
            ChannelId::Output(idx) => {
                if let Some(output) = self.outputs.get_mut(idx) {
                    output.dynamics = Some(dynamics.clamped());
                }
            }
            ChannelId::Playback(_) => {}
        }
        Ok(())
    }

    fn set_reverb(&mut self, reverb: &Reverb) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Reverb does not exist on the UC".into(),
            ));
        }
        self.ucx_write_reverb(reverb)?;
        self.settings.reverb = Some(*reverb);
        Ok(())
    }

    fn set_echo(&mut self, echo: &Echo) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel("Echo does not exist on the UC".into()));
        }
        self.ucx_write_echo(echo)?;
        self.settings.echo = Some(*echo);
        Ok(())
    }

    fn set_fx_return(&mut self, output: usize, db: f32) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "FX Return does not exist on the UC".into(),
            ));
        }
        if output >= self.outputs.len() {
            return Err(Error::InvalidChannel(format!("output {output}")));
        }
        let db = db.clamp(-65.0, 6.0);
        let pair = output / 2;
        if self.output_linked(pair) {
            let left = pair * 2;
            let right = left + 1;
            self.ucx_write_fx_return(left, db)?;
            self.ucx_write_fx_return(right, db)?;
            self.outputs[left].fx_return_db = db;
            self.outputs[right].fx_return_db = db;
        } else {
            self.ucx_write_fx_return(output, db)?;
            self.outputs[output].fx_return_db = db;
        }
        Ok(())
    }

    fn set_eq_enabled(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        if self.is_uc() || idx >= self.inputs.len() {
            return Err(Error::InvalidChannel(format!("input {idx} has no EQ")));
        }
        if let Some(eq) = self.inputs[idx].eq.as_mut() {
            eq.enabled = on;
        }
        self.sync_input_eq(idx);
        let s = self.input_settings[idx];
        self.ucx_write_channel_settings(idx, false, &s)
    }

    fn set_eq_band_type(
        &mut self,
        idx: usize,
        band: usize,
        band_type: EqBandType,
    ) -> Result<(), Error> {
        if self.is_uc() || idx >= self.inputs.len() || band >= 3 {
            return Err(Error::InvalidChannel(format!("EQ input {idx} band {band}")));
        }
        let Some(eq) = self.inputs[idx].eq.as_mut() else {
            return Err(Error::InvalidChannel(format!("input {idx} has no EQ")));
        };
        eq.bands[band].band_type = band_type;
        if band_type == EqBandType::Off {
            eq.bands[band].gain_db = 0.0;
        }
        self.sync_input_eq(idx);
        let s = self.input_settings[idx];
        self.ucx_write_channel_settings(idx, false, &s)
    }

    fn set_eq_band_freq(
        &mut self,
        idx: usize,
        band: usize,
        freq_hz: u16,
    ) -> Result<(), Error> {
        if self.is_uc() || idx >= self.inputs.len() || band >= 3 {
            return Err(Error::InvalidChannel(format!("EQ input {idx} band {band}")));
        }
        if let Some(eq) = self.inputs[idx].eq.as_mut() {
            eq.bands[band].freq_hz = freq_hz.clamp(20, 20000);
        }
        self.sync_input_eq(idx);
        let s = self.input_settings[idx];
        self.ucx_write_channel_settings(idx, false, &s)
    }

    fn set_eq_band_q(&mut self, idx: usize, band: usize, q: f32) -> Result<(), Error> {
        if self.is_uc() || idx >= self.inputs.len() || band >= 3 {
            return Err(Error::InvalidChannel(format!("EQ input {idx} band {band}")));
        }
        if let Some(eq) = self.inputs[idx].eq.as_mut() {
            eq.bands[band].q = q.clamp(0.7, 5.0);
        }
        self.sync_input_eq(idx);
        let s = self.input_settings[idx];
        self.ucx_write_channel_settings(idx, false, &s)
    }

    fn set_eq_band_gain(&mut self, idx: usize, band: usize, gain_db: f32) -> Result<(), Error> {
        if self.is_uc() || idx >= self.inputs.len() || band >= 3 {
            return Err(Error::InvalidChannel(format!("EQ input {idx} band {band}")));
        }
        if let Some(eq) = self.inputs[idx].eq.as_mut() {
            eq.bands[band].gain_db = gain_db.clamp(-20.0, 20.0);
        }
        self.sync_input_eq(idx);
        let s = self.input_settings[idx];
        self.ucx_write_channel_settings(idx, false, &s)
    }

    fn set_eq_low_cut_enabled(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        if self.is_uc() || idx >= self.inputs.len() {
            return Err(Error::InvalidChannel(format!("input {idx} has no low cut")));
        }
        if let Some(eq) = self.inputs[idx].eq.as_mut() {
            eq.low_cut_enabled = on;
        }
        self.sync_input_eq(idx);
        let s = self.input_settings[idx];
        self.ucx_write_channel_settings(idx, false, &s)
    }

    fn set_eq_low_cut_freq(&mut self, idx: usize, freq_hz: u16) -> Result<(), Error> {
        if self.is_uc() || idx >= self.inputs.len() {
            return Err(Error::InvalidChannel(format!("input {idx} has no low cut")));
        }
        if let Some(eq) = self.inputs[idx].eq.as_mut() {
            eq.low_cut_freq_hz = freq_hz.clamp(20, 500);
        }
        self.sync_input_eq(idx);
        let s = self.input_settings[idx];
        self.ucx_write_channel_settings(idx, false, &s)
    }

    fn set_eq_low_cut_slope(&mut self, idx: usize, slope_db_oct: u8) -> Result<(), Error> {
        if self.is_uc() || idx >= self.inputs.len() {
            return Err(Error::InvalidChannel(format!("input {idx} has no low cut")));
        }
        if let Some(eq) = self.inputs[idx].eq.as_mut() {
            eq.low_cut_slope_db_oct = slope_db_oct.clamp(6, 24);
        }
        self.sync_input_eq(idx);
        let s = self.input_settings[idx];
        self.ucx_write_channel_settings(idx, false, &s)
    }

    fn set_output_eq(&mut self, idx: usize, eq: &InputEq) -> Result<(), Error> {
        if self.is_uc() || idx >= self.outputs.len() {
            return Err(Error::InvalidChannel(format!("output {idx} has no EQ")));
        }
        self.outputs[idx].eq = Some(*eq);
        self.sync_output_eq(idx);
        let s = self.output_settings[idx];
        self.ucx_write_channel_settings(idx, true, &s)
    }

    fn set_clock_source(&mut self, source: &str) -> Result<(), Error> {
        let index = match source {
            "Internal" => 0,
            "SPDIF Coaxial" | "SPDIF" => 1,
            "Optical" | "ADAT" => 2,
            "Word Clock" => 3,
            "LTC" => 4,
            _ => {
                return Err(Error::InvalidChannel(format!(
                    "clock source {source}"
                )))
            }
        };
        if self.is_uc() {
            self.settings.clock_source = source.to_string();
            self.uc_write_options()?;
        } else {
            // The UCX clock field is two bits (mask 0x0006).  Index 4 is
            // the UC's LTC source and does not exist on this unit.
            if index > 3 {
                return Err(Error::InvalidChannel(format!(
                    "clock source {source} does not exist on the UCX"
                )));
            }
            self.ucx_write_clock(index)?;
            self.settings.clock_source = source.to_string();
        }
        Ok(())
    }

    fn capture_scene(&self) -> Scene {
        Scene {
            name: "Auto".into(),
            model: self.canonical_model().to_string(),
            matrix_overrides: self
                .matrix_overrides
                .iter()
                .map(|(&(source, block), &gain)| MatrixOverride {
                    source,
                    block,
                    gain,
                })
                .collect(),
            inputs: self.inputs.clone(),
            playbacks: self.playbacks.clone(),
            outputs: self.outputs.clone(),
            settings: self.settings.clone(),
        }
    }

    fn apply_scene(&mut self, scene: &Scene) -> Result<(), Error> {
        scene.check_compatible(self.canonical_model())?;
        if scene.inputs.len() != self.inputs.len()
            || scene.playbacks.len() != self.playbacks.len()
            || scene.outputs.len() != self.outputs.len()
        {
            return Err(Error::InvalidChannel(
                "scene channel count does not match this device".into(),
            ));
        }
        // The device-owned group (preamp gain, phantom/PAD/Inst, reference
        // levels, clock/rate/optical) is not part of the connect write.
        // The UC reads it at connect, so its live values win over a stale
        // scene.  On the UCX, gain is read from the 0x8d stream; the
        // switches, references and options have no readback, so the scene
        // keeps their last written values.  Only the mixer group is applied.
        let device_inputs: Vec<_> = self
            .inputs
            .iter()
            .map(|i| {
                (
                    i.phantom,
                    i.pad,
                    i.instrument,
                    i.gain_max,
                    i.has_pad,
                    i.has_instrument,
                    i.ref_level,
                    i.autoset,
                )
            })
            .collect();
        // Input gain has no on-demand read.  Keep a front-panel value if
        // one was queued; otherwise leave the scene's last known value in
        // place for display (it is never written at connect).
        let known_gains = self.device_input_gain.clone();
        let device_clock = self.settings.clock_source.clone();
        let device_clock_sources = self.settings.clock_sources.clone();
        let device_optical = self.settings.optical_out_spdif;
        let device_spdif_pro = self.settings.spdif_professional;
        let device_rate = self.settings.sample_rate;
        let device_line_in = self.settings.line_in_ref;
        let device_line_out = self.settings.line_out_ref;
        let device_phones = self.settings.phones_ref;
        let device_wc_single = self.settings.word_clock_single_speed;
        let device_reverb = self.settings.reverb;
        let device_echo = self.settings.echo;

        // Keep the live topology authoritative.  Older scene files can
        // carry a different row order (notably the UCX software-playback
        // row); map their values into the current channel order by name,
        // then restore the current names and IDs below.
        let current_input_names: Vec<String> =
            self.inputs.iter().map(|c| c.name.clone()).collect();
        let current_playback_names: Vec<String> =
            self.playbacks.iter().map(|c| c.name.clone()).collect();
        let current_output_names: Vec<String> =
            self.outputs.iter().map(|c| c.name.clone()).collect();
        let scene_input_names: Vec<String> =
            scene.inputs.iter().map(|c| c.name.clone()).collect();
        let scene_playback_names: Vec<String> =
            scene.playbacks.iter().map(|c| c.name.clone()).collect();
        let scene_output_names: Vec<String> =
            scene.outputs.iter().map(|c| c.name.clone()).collect();
        let input_order = scene_order_by_name(&current_input_names, &scene_input_names);
        let playback_order =
            scene_order_by_name(&current_playback_names, &scene_playback_names);
        let output_order = scene_order_by_name(&current_output_names, &scene_output_names);
        let current_inputs = self.inputs.clone();
        let current_playbacks = self.playbacks.clone();
        let current_outputs = self.outputs.clone();

        self.inputs = input_order
            .iter()
            .filter_map(|&idx| scene.inputs.get(idx).cloned())
            .collect();
        self.playbacks = playback_order
            .iter()
            .filter_map(|&idx| scene.playbacks.get(idx).cloned())
            .collect();
        self.outputs = output_order
            .iter()
            .filter_map(|&idx| scene.outputs.get(idx).cloned())
            .collect();
        for (current, saved) in current_inputs.iter().zip(self.inputs.iter_mut()) {
            saved.id = current.id;
            saved.name = current.name.clone();
            saved.channel_type = current.channel_type;
            saved.has_pad = current.has_pad;
            saved.has_instrument = current.has_instrument;
            saved.gain_max = current.gain_max;
            saved.sensitivity = current.sensitivity;
            // Older scenes predate input EQ persistence and deserialize
            // `eq` as None.  Do not let that erase the profile's EQ
            // topology: the EQ/phase controls disappear and the EQ
            // setters become no-ops.  Keep the live topology and apply
            // its default (flat/off) state until a scene carries EQ.
            if saved.eq.is_none() {
                saved.eq = current.eq.clone();
            }
            if saved.dynamics.is_none() {
                saved.dynamics = current.dynamics;
            }
        }
        for (current, saved) in current_playbacks.iter().zip(self.playbacks.iter_mut()) {
            saved.id = current.id;
            saved.name = current.name.clone();
        }
        for (current, saved) in current_outputs.iter().zip(self.outputs.iter_mut()) {
            saved.id = current.id;
            saved.name = current.name.clone();
            // Same legacy-scene rule as inputs: keep the live topology
            // when the saved output predates EQ persistence.
            if saved.eq.is_none() {
                saved.eq = current.eq;
            }
            if saved.dynamics.is_none() {
                saved.dynamics = current.dynamics;
            }
        }
        self.settings = scene.settings.clone();
        self.matrix_overrides.clear();
        if self.is_uc() {
            for (idx, (input, saved)) in self.inputs.iter_mut().zip(device_inputs).enumerate() {
                if let Some(gain) = known_gains.get(&idx) {
                    input.gain = Some(*gain);
                }
                input.phantom = saved.0;
                input.pad = saved.1;
                input.instrument = saved.2;
                input.gain_max = saved.3;
                input.has_pad = saved.4;
                input.has_instrument = saved.5;
                input.ref_level = saved.6;
                input.level = InputLevel::from_code(saved.6);
                input.autoset = saved.7;
            }
        } else {
            // The gain is read back from the 0x8d stream; use the value the
            // unit reported if it has arrived.  Otherwise keep the scene's
            // value.  Scenes written before the half-dB model stored inputs
            // 3/4 as whole dB with `gain_max = 12`; convert those to raw
            // half-dB units (0..24).
            for (idx, input) in self.inputs.iter_mut().enumerate() {
                if let Some(gain) = known_gains.get(&idx) {
                    input.gain = Some(*gain);
                } else if scene
                    .inputs
                    .get(idx)
                    .is_some_and(|saved| saved.gain_max == Some(12))
                    && input.gain_max == Some(24)
                {
                    input.gain = input.gain.map(|gain| (gain * 2).min(24));
                }
                if (2..4).contains(&idx) {
                    if input.level.is_none() {
                        input.level = InputLevel::from_code(input.ref_level);
                    }
                    if let Some(level) = input.level {
                        input.ref_level = level.code();
                    }
                }
            }
        }
        // The UC reads its options word at connect, so the live values win
        // over a scene that may predate a change made elsewhere.  No UCX
        // options-word readback is exposed (request 0x10 returns a status
        // word, not the options).  There the scene is the only record: keep
        // its values so a restart does not reset the visible settings.
        self.settings.clock_sources = device_clock_sources;
        self.settings.sample_rate = device_rate;
        if !self
            .settings
            .clock_sources
            .iter()
            .any(|source| *source == self.settings.clock_source)
        {
            // A scene written for the UC can carry LTC, which the UCX does
            // not offer.
            self.settings.clock_source = self
                .settings
                .clock_sources
                .first()
                .cloned()
                .unwrap_or_else(|| "Internal".into());
        }
        if self.is_uc() {
            self.settings.clock_source = device_clock;
            self.settings.optical_out_spdif = device_optical;
            self.settings.spdif_professional = device_spdif_pro;
            self.settings.line_in_ref = device_line_in;
            self.settings.line_out_ref = device_line_out;
            self.settings.phones_ref = device_phones;
            self.settings.word_clock_single_speed = device_wc_single;
        }
        if self.settings.reverb.is_none() {
            self.settings.reverb = device_reverb;
        }
        if self.settings.echo.is_none() {
            self.settings.echo = device_echo;
        }
        self.soloed.clear();
        self.muted.clear();
        self.mute_saved.clear();
        self.output_mute_saved.clear();

        // Seed mono matrix cells before the UC connect write.  On the UC
        // the full-matrix sequence is the only writer of those cells; the
        // other backends write each override explicitly below.
        for cell in &scene.matrix_overrides {
            if self.is_uc() && cell.block < BLOCKS {
                self.matrix_overrides
                    .insert((cell.source, cell.block), cell.gain.clamp(0.0, 1.0));
            }
        }

        if self.is_uc() {
            // Exact TotalMix app-level connect order: 0x15 bank reset,
            // then each output pair's 0x1a + 0x12 master + crosspoints,
            // Phones last.  This must happen before the meter tap opens
            // the PCM, because the UC latches routing at engine start.
            self.apply_uc_full_matrix()?;
            // Loopback is per stereo hardware output when the pair is
            // linked (both sides move together), and per channel only
            // when the pair has been split.  TotalMix sends these after
            // the matrix, not before it.
            for pair in 0..PAIRS {
                if self.output_linked(pair) {
                    let loopback = self.outputs[pair * 2].loopback
                        || self.outputs[pair * 2 + 1].loopback;
                    // The 30-entry reset above already cleared every
                    // loopback, so only write the enabled ones.
                    if loopback {
                        self.set_loopback(pair, true)?;
                    }
                } else {
                    for side in 0..2 {
                        let idx = pair * 2 + side;
                        if self.outputs[idx].loopback {
                            self.set_loopback_channel(idx, true)?;
                        }
                    }
                }
            }
        } else {
            for idx in 0..self.outputs.len() {
                let volume = self.outputs[idx].volume;
                self.set_volume(ChannelId::Output(idx), 0, volume)?;
                if self.outputs[idx].mute {
                    self.set_mute(ChannelId::Output(idx), true)?;
                }
            }
            // Balance and stereo mode are separate registers from volume.
            for pair in 0..PAIRS {
                let linked = self.output_linked(pair);
                let left = pair * 2;
                let right = left + 1;
                self.ucx_write_output_stereo(left, linked)?;
                self.ucx_write_output_stereo(right, linked)?;
                if linked {
                    let balance = self.outputs[left].balance;
                    self.set_pan(ChannelId::Output(left), 0, balance)?;
                }
            }
            for pair in 0..PAIRS {
                let loopback = self.outputs[pair * 2].loopback
                    || self.outputs[pair * 2 + 1].loopback;
                self.set_loopback(pair, loopback)?;
            }
            // Output phase is a per-side register pair; only the inverted
            // sides need a write (the unit defaults to normal).
            for idx in 0..self.outputs.len() {
                let inverted = if idx % 2 == 0 {
                    self.outputs[idx].phase_l
                } else {
                    self.outputs[idx].phase_r
                };
                if inverted {
                    self.set_output_phase(idx, true)?;
                }
            }
            for idx in 0..self.outputs.len() {
                if self.outputs[idx].eq.is_some() {
                    self.sync_output_eq(idx);
                    let s = self.output_settings[idx];
                    self.ucx_write_channel_settings(idx, true, &s)?;
                }
                if let Some(dynamics) = self.outputs[idx].dynamics {
                    self.ucx_write_dynamics(ChannelId::Output(idx), &dynamics)?;
                }
                let fx = self.outputs[idx].fx_return_db;
                self.set_fx_return(idx, fx)?;
            }
        }
        for idx in 0..self.inputs.len() {
            if !self.is_uc() {
                for p in 0..PAIRS {
                    let volume = self.inputs[idx].volumes[p];
                    self.apply_input_volume(idx, p, volume)?;
                }
                let linked = self.input_pair_linked(idx / 2);
                self.ucx_write_input_stereo(idx, linked)?;
                if idx < 2 {
                    self.ucx_write_autoset(idx, self.inputs[idx].autoset)?;
                }
            }
            if self.inputs[idx].mute {
                self.set_mute(ChannelId::Input(idx), true)?;
            }
            if !self.is_uc() && self.inputs[idx].eq.is_some() {
                self.sync_input_eq(idx);
                let s = self.input_settings[idx];
                self.ucx_write_channel_settings(idx, false, &s)?;
            }
            if !self.is_uc() {
                if let Some(dynamics) = self.inputs[idx].dynamics {
                    self.ucx_write_dynamics(ChannelId::Input(idx), &dynamics)?;
                }
            }
        }
        for idx in 0..self.playbacks.len() {
            if !self.is_uc() {
                for p in 0..PAIRS {
                    let volume = self.playbacks[idx].volumes[p];
                    self.apply_playback_volume(idx, p, volume)?;
                }
            }
            if self.playbacks[idx].mute {
                self.set_mute(ChannelId::Playback(idx), true)?;
            }
        }
        if !self.is_uc() {
            for cell in &scene.matrix_overrides {
                let Some(channel) = Self::matrix_source_channel(cell.source) else {
                    continue;
                };
                self.set_matrix_cell(channel, cell.block, cell.gain)?;
            }
            if let Some(reverb) = self.settings.reverb {
                self.ucx_write_reverb(&reverb)?;
            }
            if let Some(echo) = self.settings.echo {
                self.ucx_write_echo(&echo)?;
            }
            let speaker_b = self.settings.speaker_b;
            self.set_speaker_b(speaker_b)?;
            let dim = self.settings.dim;
            self.settings.dim = false;
            self.dim_saved = None;
            self.write_word(ucx::global::DIM, 0)?;
            if dim {
                self.set_dim(true)?;
            }
        }
        Ok(())
    }

    fn poll_events(&mut self) -> Result<(), Error> {
        if self.model == Model::Ucx && Instant::now() >= self.ucx_heartbeat_due {
            let sequence = self.ucx_heartbeat;
            if let Err(e) = self.write_word(ucx::global::HEARTBEAT, sequence) {
                log::warn!("UCX heartbeat failed: {e:?}");
            }
            self.ucx_heartbeat = (sequence + 1) & 0x0f;
            self.ucx_heartbeat_due = Instant::now() + Duration::from_millis(100);
        }
        self.drain_events();
        Ok(())
    }

    fn meters(&self) -> Option<Vec<f32>> {
        match self.model {
            Model::Ucx => Some(
                (0..self.inputs.len())
                    .map(|i| self.meters.input(i))
                    .collect(),
            ),
            Model::Uc => {
                let tap = self.input_tap.as_ref()?;
                let (levels, channels) = tap.drain();
                if channels == 0 {
                    return None;
                }
                Some(levels[..self.inputs.len().min(levels.len())].to_vec())
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Write {
        Vendor(u8, u16, u16),
        Words(Vec<(u16, u16)>),
    }

    struct MockTransport {
        info: DeviceInfo,
        writes: Arc<Mutex<Vec<Write>>>,
        levels: Arc<Mutex<u32>>,
    }

    impl MockTransport {
        fn new(model: Model) -> (Self, Arc<Mutex<Vec<Write>>>) {
            let writes = Arc::new(Mutex::new(Vec::new()));
            let levels = Arc::new(Mutex::new(0x3480));
            let info = DeviceInfo {
                card: 0,
                model,
                iso: false,
                caps: CAP_VENDOR | CAP_WORDS | CAP_METERS | CAP_STATUS | CAP_PCM,
                channels: 18,
                inputs: 18,
                outputs: 18,
                playback: 18,
                matrix_sources: 36,
                meter_bytes: 244,
                state_ep: 0x8d,
                meter_ep: 0x85,
                status_ep: 0x8b,
                rate: 48000,
                firmware: 1,
                name: model.name().into(),
                serial: "TEST".into(),
            };
            (
                Self {
                    info,
                    writes: Arc::clone(&writes),
                    levels,
                },
                writes,
            )
        }
    }

    impl ControlTransport for MockTransport {
        fn info(&self) -> &DeviceInfo {
            &self.info
        }

        fn vendor(
            &self,
            request: u8,
            direction: Direction,
            value: u16,
            index: u16,
            _data: &[u8],
        ) -> Result<Vec<u8>, Error> {
            self.writes
                .lock()
                .unwrap()
                .push(Write::Vendor(request, value, index));
            if request == protocol::req::LEVELS {
                if direction == Direction::In {
                    return Ok(self.levels.lock().unwrap().to_le_bytes().to_vec());
                }
                *self.levels.lock().unwrap() = value as u32;
            }
            Ok(Vec::new())
        }

        fn write_words(&self, words: &[(u16, u16)]) -> Result<(), Error> {
            self.writes
                .lock()
                .unwrap()
                .push(Write::Words(words.to_vec()));
            Ok(())
        }

        fn wait(&self, _timeout: Duration) -> Result<bool, Error> {
            Ok(false)
        }

        fn read_event(&self) -> Result<Event, Error> {
            Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "no events").into())
        }

        fn is_connected(&self) -> bool {
            true
        }
    }

    fn open(model: Model) -> (Fireface, Arc<Mutex<Vec<Write>>>) {
        let (t, writes) = MockTransport::new(model);
        let info = t.info.clone();
        (Fireface::from_transport(Box::new(t), info).unwrap(), writes)
    }

    #[test]
    fn ucx_output_meters_are_mapped_to_internal_order() {
        let mut raw = vec![0.0f32; 18];
        raw[0] = 0.1; // AN1 L (display pair 0)
        raw[1] = 0.2;
        raw[6] = 0.3; // Phones L (display pair 3)
        raw[7] = 0.4;
        let out = reorder_output_meters(&raw, &[1, 2, 3, 0, 4, 5, 6, 7, 8]);
        assert_eq!(out[0], 0.3, "internal pair 0 is Phones");
        assert_eq!(out[1], 0.4);
        assert_eq!(out[2], 0.1, "internal pair 1 is AN1/2");
        assert_eq!(out[3], 0.2);
    }

    #[test]
    fn ucx_output_fader_and_mute_go_to_the_dsp_channel() {
        let (mut dev, writes) = open(Model::Ucx);
        // Output channel 2 = AN1 (pair 1, side 0) -> DSP output 0 -> 0x12.
        dev.set_volume(ChannelId::Output(2), 0, 0.5).unwrap();
        dev.set_mute(ChannelId::Output(2), true).unwrap();
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Words(vec![(0x1200, 0xfd76)])) ||
                w.iter().any(|x| matches!(x, Write::Words(v) if v[0].0 == 0x1200)));
        assert!(w.iter().any(|x| matches!(x, Write::Words(v) if v[0] == (0x1202, 1))));
    }

    #[test]
    fn scene_channel_order_is_mapped_by_name_not_position() {
        // An older UCX scene had Phones first, then AN1/AN2.  The current
        // profile has AN1/AN2 first, so positional cloning would swap them.
        let current = vec!["PCM AN1".to_string(), "PCM AN2".to_string(), "PCM Phones 7".to_string()];
        let saved = vec!["PCM Phones 7".to_string(), "PCM AN1".to_string(), "PCM AN2".to_string()];
        assert_eq!(scene_order_by_name(&current, &saved), vec![1, 2, 0]);
    }

    #[test]
    fn ucx_poll_events_sends_the_if1_heartbeat() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.poll_events().unwrap();
        let w = writes.lock().unwrap();
        assert!(w.iter().any(|x| matches!(x, Write::Words(v) if v == &vec![(0x3f00, 0)])));
    }

    #[test]
    fn ucx_front_panel_volume_updates_the_phones_fader() {
        let (mut dev, _writes) = open(Model::Ucx);
        // -33.0 dB, signed tenths, as captured from the physical Phones knob.
        dev.apply_ucx_state_word(0x1800, 0xfeb6);
        dev.apply_ucx_state_word(0x1900, 0xfeb6);
        let expected = ucx::tenths_to_linear(0xfeb6);
        assert!((dev.outputs()[0].volume - expected).abs() < 1e-5);
        assert!((dev.outputs()[1].volume - expected).abs() < 1e-5);
        assert!((dev.hardware_volume().unwrap() - expected).abs() < 1e-5);
    }

    #[test]
    fn ucx_state_volume_addresses_map_to_phones_first_outputs() {
        assert_eq!(ucx::output_index_from_state(0x1800), Some(0));
        assert_eq!(ucx::output_index_from_state(0x1900), Some(1));
        assert_eq!(ucx::output_index_from_state(0x1200), Some(2));
        assert_eq!(ucx::output_index_from_state(0x2300), Some(17));
        assert_eq!(ucx::output_index_from_state(0x1802), None);
    }

    #[test]
    fn ucx_input_matrix_and_fader_table() {
        let (mut dev, writes) = open(Model::Ucx);
        // Input 4 (AN5) into pair 1 (AN1/2, DSP outputs 0/1).
        dev.set_volume(ChannelId::Input(4), 1, 0.5).unwrap();
        let w = writes.lock().unwrap();
        let words: Vec<(u16, u16)> = w
            .iter()
            .filter_map(|x| match x {
                Write::Words(v) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        // Input 4 (AN5 L) -> DSP left (0), input 5 (AN5 R) -> DSP right (1).
        assert!(words.contains(&(0x4004, ucx::matrix_gain(0.5))));
        assert!(words.contains(&(0x4045, ucx::matrix_gain(1.0))));
        assert!(words.iter().any(|(a, _)| *a == 0x12e4));
        assert!(words.iter().any(|(a, _)| *a == 0x12e5));
    }

    #[test]
    fn ucx_output_eq_writes_output_channel_settings() {
        let (mut dev, writes) = open(Model::Ucx);
        assert!(dev.outputs()[2].eq.is_some());
        let mut eq = dev.outputs()[2].eq.unwrap();
        eq.bands[0].gain_db = 6.0;
        writes.lock().unwrap().clear();
        dev.set_output_eq(2, &eq).unwrap();
        assert_eq!(dev.outputs()[2].eq.unwrap().bands[0].gain_db, 6.0);
        assert_eq!(
            dev.capture_scene().outputs[2]
                .eq
                .unwrap()
                .bands[0]
                .gain_db,
            6.0
        );
        let dsp = ucx::output_pair_dsp(1).unwrap();
        let addr = ucx::channel_reg(ucx::output_channel(dsp), ucx::offset::eq_band(0).0);
        let w = writes.lock().unwrap();
        assert!(
            w.iter()
                .any(|x| matches!(x, Write::Words(v) if v.contains(&(addr, 60)))),
            "output EQ gain register not written"
        );
    }

    #[test]
    fn ucx_low_cut_enable_writes_its_register() {
        let (mut dev, writes) = open(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_eq_low_cut_enabled(0, true).unwrap();
        assert!(dev.inputs()[0].eq.unwrap().low_cut_enabled);
        let addr = ucx::channel_reg(ucx::input_channel(0), ucx::offset::LOW_CUT_ON);
        let w = writes.lock().unwrap();
        assert!(
            w.iter()
                .any(|x| matches!(x, Write::Words(v) if v.contains(&(addr, 1)))),
            "low-cut enable register not written"
        );
    }

    #[test]
    fn ucx_preamp_and_loopback() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.set_gain(0, 48).unwrap();
        dev.set_loopback(0, true).unwrap();
        let w = writes.lock().unwrap();
        assert!(w.iter().any(|x| matches!(x, Write::Words(v) if v == &vec![(0x0007, 480)])));
        assert!(w.contains(&Write::Vendor(protocol::req::LOOPBACK, 1, 6)));
        assert!(w.contains(&Write::Vendor(protocol::req::LOOPBACK, 1, 7)));
    }

    #[test]
    fn ucx_measured_front_panel_controls_write_their_registers() {
        let (mut dev, writes) = open(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_phantom(0, true).unwrap();
        dev.set_phantom(1, true).unwrap();
        dev.set_input_level(2, InputLevel::LoGain).unwrap();
        dev.set_input_level(3, InputLevel::Minus10dBV).unwrap();
        dev.set_autoset(0, true).unwrap();
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Words(vec![(0x0008, 1)])));
        assert!(w.contains(&Write::Words(vec![(0x0108, 1)])));
        assert!(w.contains(&Write::Words(vec![(0x0208, 2)])));
        assert!(w.contains(&Write::Words(vec![(0x0308, 0)])));
        assert!(w.contains(&Write::Words(vec![(0x000a, 1)])));
    }

    #[test]
    fn ucx_dynamics_write_the_measured_block() {
        let (mut dev, writes) = open(Model::Ucx);
        let d = Dynamics {
            on: true,
            gain_db: 3.0,
            attack_ms: 20,
            release_ms: 500,
            comp_threshold_db: -25.5,
            comp_ratio: 2.5,
            exp_threshold_db: -60.5,
            exp_ratio: 4.0,
            auto_level: true,
            auto_max_gain_db: 12.0,
            auto_headroom_db: 9.0,
            auto_rise_s: 2.5,
        };
        writes.lock().unwrap().clear();
        dev.set_dynamics(ChannelId::Input(0), &d).unwrap();
        let w = writes.lock().unwrap();
        let words: Vec<(u16, u16)> = w
            .iter()
            .filter_map(|x| match x {
                Write::Words(v) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(words.contains(&(0x0060, 1)));
        assert!(words.contains(&(0x0061, 30)));
        // Measured UCX encoding: value = ms + 1.
        assert!(words.contains(&(0x0062, 21)));
        assert!(words.contains(&(0x0063, 501)));
        assert!(words.contains(&(0x0064, (-255i16) as u16)));
        assert!(words.contains(&(0x0065, 25)));
        assert!(words.contains(&(0x0066, (-605i16) as u16)));
        assert!(words.contains(&(0x0067, 40)));
        assert!(words.contains(&(0x0080, 1)));
        assert!(words.contains(&(0x0081, 120)));
        assert!(words.contains(&(0x0082, 90)));
        assert!(words.contains(&(0x0083, 25)));
        assert_eq!(dev.inputs()[0].dynamics, Some(d));
    }

    #[test]
    fn ucx_output_balance_has_its_own_register() {
        let (mut dev, writes) = open(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_pan(ChannelId::Output(2), 0, -50).unwrap();
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Words(vec![(0x1201, (-50i16) as u16)])));
        assert!(w.contains(&Write::Words(vec![(0x1301, (-50i16) as u16)])));
        drop(w);
        assert_eq!(dev.outputs()[2].balance, -50);
        assert_eq!(dev.outputs()[3].balance, -50);
        assert_eq!(dev.pan(ChannelId::Output(2), 0).unwrap(), -50);
        // The pair's volume registers are untouched by a balance move.
        assert_eq!(dev.outputs()[2].volume, 1.0);
    }

    #[test]
    fn ucx_channel_width_rewrites_the_pair_matrix() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.set_volume(ChannelId::Input(0), 1, 1.0).unwrap();
        dev.set_volume(ChannelId::Input(1), 1, 1.0).unwrap();
        writes.lock().unwrap().clear();
        dev.set_channel_width(ChannelId::Input(0), 0.0).unwrap();
        // At width 0 the pair is an equal-power sum: cos = sin = 0.7071.
        let mixed = ucx::matrix_gain(std::f32::consts::FRAC_1_SQRT_2 as f64);
        {
            let w = writes.lock().unwrap();
            // AN1/2 is pair 1: DSP outputs 0/1, sources 0/1.
            assert!(w.contains(&Write::Words(vec![(0x4000, mixed)])));
            assert!(w.contains(&Write::Words(vec![(0x4001, mixed)])));
            assert!(w.contains(&Write::Words(vec![(0x4040, mixed)])));
            assert!(w.contains(&Write::Words(vec![(0x4041, mixed)])));
        }
        assert_eq!(dev.inputs()[0].width, 0.0);
        assert_eq!(dev.inputs()[1].width, 0.0);

        // +1 restores the diagonals and clears the cross terms.
        writes.lock().unwrap().clear();
        dev.set_channel_width(ChannelId::Input(0), 1.0).unwrap();
        {
            let w = writes.lock().unwrap();
            assert!(w.contains(&Write::Words(vec![(0x4000, ucx::matrix_gain(1.0))])));
            assert!(w.contains(&Write::Words(vec![(0x4001, 0)])));
            assert!(w.contains(&Write::Words(vec![(0x4040, 0)])));
        }

        // A routing with no live send is left alone.
        dev.set_volume(ChannelId::Input(0), 1, 0.0).unwrap();
        dev.set_volume(ChannelId::Input(1), 1, 0.0).unwrap();
        writes.lock().unwrap().clear();
        dev.set_channel_width(ChannelId::Input(0), 0.5).unwrap();
        let w = writes.lock().unwrap();
        assert!(!w.iter().any(|x| matches!(
            x,
            Write::Words(v) if v.iter().any(|(reg, _)| (0x4000..=0x4041).contains(reg))
        )));
    }

    #[test]
    fn ucx_output_phase_writes_one_register_per_side() {
        let (mut dev, writes) = open(Model::Ucx);
        writes.lock().unwrap().clear();
        // AN 1/2 is internal pair 1, DSP channels 0x12/0x13.
        dev.set_output_phase(2, true).unwrap();
        dev.set_output_phase(3, true).unwrap();
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Words(vec![(0x1207, 1)])));
        assert!(w.contains(&Write::Words(vec![(0x1307, 1)])));
        drop(w);
        assert!(dev.outputs()[2].phase_l);
        assert!(dev.outputs()[3].phase_r);

        writes.lock().unwrap().clear();
        dev.set_output_phase(2, false).unwrap();
        assert!(writes.lock().unwrap().contains(&Write::Words(vec![(0x1207, 0)])));
        assert!(!dev.outputs()[2].phase_l);
    }

    #[test]
    fn ucx_no_trim_excludes_the_destination_from_trim() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.set_trim(ChannelId::Input(0), -6.0).unwrap();
        dev.set_volume(ChannelId::Input(0), 1, 1.0).unwrap();
        let trimmed = ucx::matrix_gain(10f64.powf(-6.0 / 20.0));
        {
            let w = writes.lock().unwrap();
            assert!(w.contains(&Write::Words(vec![(0x4000, trimmed)])));
        }

        writes.lock().unwrap().clear();
        dev.set_no_trim(2, true).unwrap();
        {
            let w = writes.lock().unwrap();
            assert!(w.contains(&Write::Words(vec![(0x4000, ucx::matrix_gain(1.0))])));
        }
        assert!(dev.outputs()[2].no_trim);

        writes.lock().unwrap().clear();
        dev.set_no_trim(2, false).unwrap();
        {
            let w = writes.lock().unwrap();
            assert!(w.contains(&Write::Words(vec![(0x4000, trimmed)])));
        }
        assert!(!dev.outputs()[2].no_trim);
    }

    #[test]
    fn ucx_control_room_speaker_b_and_dim_write_registers() {        let (mut dev, writes) = open(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_speaker_b(true).unwrap();
        {
            let w = writes.lock().unwrap();
            assert!(w.contains(&Write::Words(vec![(0x1802, 2)])));
            assert!(w.contains(&Write::Words(vec![(0x1902, 2)])));
            assert!(w.contains(&Write::Words(vec![(0x3d00, 8)])));
        }
        assert!(dev.settings().speaker_b);

        let before = (dev.outputs()[0].volume, dev.outputs()[1].volume);
        writes.lock().unwrap().clear();
        dev.set_dim(true).unwrap();
        {
            let w = writes.lock().unwrap();
            assert!(w.contains(&Write::Words(vec![(0x3d03, 1)])));
            assert!(w.iter().any(|x| matches!(x, Write::Words(v) if v[0].0 == 0x1800)));
            assert!(w.iter().any(|x| matches!(x, Write::Words(v) if v[0].0 == 0x1900)));
        }
        assert!(dev.settings().dim);
        assert!(dev.outputs()[0].volume < before.0);

        writes.lock().unwrap().clear();
        dev.set_dim(false).unwrap();
        assert!(writes.lock().unwrap().contains(&Write::Words(vec![(0x3d03, 0)])));
        assert!((dev.outputs()[0].volume - before.0).abs() < 1e-6);
        assert!((dev.outputs()[1].volume - before.1).abs() < 1e-6);
        assert!(!dev.settings().dim);
    }

    #[test]
    fn ucx_input_mute_writes_nn00() {
        let (mut dev, writes) = open(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_mute(ChannelId::Input(0), true).unwrap();
        assert!(writes
            .lock()
            .unwrap()
            .contains(&Write::Words(vec![(0x0000, 1)])));
        writes.lock().unwrap().clear();
        dev.set_mute(ChannelId::Input(0), false).unwrap();
        assert!(writes
            .lock()
            .unwrap()
            .contains(&Write::Words(vec![(0x0000, 0)])));
    }

    #[test]
    fn ucx_stereo_mode_registers_are_written_on_split() {
        let (mut dev, writes) = open(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_input_pair_linked(1, false).unwrap();
        dev.set_output_linked(1, false).unwrap();
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Words(vec![(0x0202, 0)])));
        assert!(w.contains(&Write::Words(vec![(0x0302, 0)])));
        assert!(w.contains(&Write::Words(vec![(0x1204, 0)])));
        assert!(w.contains(&Write::Words(vec![(0x1304, 0)])));
    }

    #[test]
    fn ucx_fx_blocks_and_return_write_their_registers() {
        let (mut dev, writes) = open(Model::Ucx);
        let mut reverb = Reverb::default();
        reverb.on = true;
        reverb.pre_delay_ms = 120;
        reverb.volume_db = -6.0;
        let mut echo = Echo::default();
        echo.on = true;
        echo.delay_ms = 660;
        echo.feedback = 40;
        writes.lock().unwrap().clear();
        dev.set_reverb(&reverb).unwrap();
        dev.set_echo(&echo).unwrap();
        dev.set_fx_return(2, -12.0).unwrap();
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Words(vec![
            (0x3c00, 1),
            (0x3c01, reverb.kind as u16),
            (0x3c02, 120),
            (0x3c03, reverb.low_cut_hz),
            (0x3c04, (reverb.room_scale * 100.0).round() as u16),
            (0x3c05, reverb.attack_ms),
            (0x3c06, reverb.hold_ms),
            (0x3c07, reverb.release_ms),
            (0x3c08, reverb.high_cut_hz),
            (0x3c09, reverb.reverb_time_ds),
            (0x3c0a, reverb.high_damp_hz),
            (0x3c0b, reverb.smoothness as u16),
            (0x3c0c, (-60i16) as u16),
            (0x3c0d, reverb.width.round() as u16),
        ])));
        assert!(w.contains(&Write::Words(vec![
            (0x3c20, 1),
            (0x3c21, 1),
            (0x3c22, 66),
            (0x3c23, 40),
            (0x3c24, 0),
            (0x3c25, (-100i16) as u16),
            (0x3c26, 100),
        ])));
        assert!(w.contains(&Write::Words(vec![(0x1203, (-120i16) as u16)])));
        assert_eq!(dev.outputs()[2].fx_return_db, -12.0);
        assert_eq!(dev.settings().reverb, Some(reverb));
        assert_eq!(dev.settings().echo, Some(echo));
    }

    #[test]
    fn uc_input_matrix_uses_display_columns() {
        let (mut dev, writes) = open(Model::Uc);
        // Matrix column 0 is AN1/2, which is raw UC block 0.
        dev.set_volume(ChannelId::Input(0), 0, 0.5).unwrap();
        let w = writes.lock().unwrap();
        let (value, index) = uc::matrix_word(0.5, 0);
        assert!(w.contains(&Write::Vendor(protocol::req::MIXER, value, index)));
    }

    #[test]
    fn uc_playback_matrix_uses_display_columns() {
        let (mut dev, writes) = open(Model::Uc);
        // Matrix column 1 is AN3/4; playback 1 left = source 18, raw block 2.
        dev.set_volume(ChannelId::Playback(0), 1, 0.5).unwrap();
        let w = writes.lock().unwrap();
        let (value, index) = uc::matrix_word(0.5, 2 * 36 + 18);
        assert!(w.contains(&Write::Vendor(protocol::req::MIXER, value, index)));
    }

    #[test]
    fn uc_playback_pan_attenuates_the_matching_side() {
        let (mut dev, writes) = open(Model::Uc);
        writes.lock().unwrap().clear();
        dev.set_pan(ChannelId::Playback(0), 2, 50).unwrap();

        let (left_value, left_index) = uc::matrix_word(0.5, 4 * 36 + 18);
        let (right_value, right_index) = uc::matrix_word(1.0, 5 * 36 + 19);
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Vendor(
            protocol::req::MIXER,
            left_value,
            left_index
        )));
        assert!(w.contains(&Write::Vendor(
            protocol::req::MIXER,
            right_value,
            right_index
        )));
        assert_eq!(dev.pan(ChannelId::Playback(0), 2).unwrap(), 50);
        assert_eq!(dev.pan(ChannelId::Playback(1), 2).unwrap(), 50);
        assert_eq!(dev.matrix_cell(ChannelId::Playback(0), 4), Some(0.5));
        assert_eq!(dev.matrix_cell(ChannelId::Playback(1), 5), Some(1.0));
    }

    #[test]
    fn uc_output_balance_uses_unequal_levels_and_a_fader_preserves_it() {
        let (mut dev, writes) = open(Model::Uc);
        // Output pair 4 = SPDIF 9/10 in the hardware order.
        dev.set_output_volume(4, 0, 1.0).unwrap();
        writes.lock().unwrap().clear();
        dev.set_pan(ChannelId::Output(8), 0, 50).unwrap();

        assert_eq!(dev.pan(ChannelId::Output(8), 0).unwrap(), 50);
        assert!((dev.outputs()[8].volume - 0.5).abs() < 1e-6);
        assert!((dev.outputs()[9].volume - 1.0).abs() < 1e-6);

        let left_encoder = uc::encoder_channel(8, dev.output_order()).unwrap() as u16;
        let right_encoder = uc::encoder_channel(9, dev.output_order()).unwrap() as u16;
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Vendor(
            protocol::req::PGA,
            uc::output_attenuation(0.5) as u16,
            left_encoder
        )));
        assert!(w.contains(&Write::Vendor(
            protocol::req::PGA,
            uc::output_attenuation(1.0) as u16,
            right_encoder
        )));
        drop(w);

        // The linked fader moves both sides by the same dB offset, so the
        // balance survives a fader drag.
        writes.lock().unwrap().clear();
        dev.set_output_volume(4, 0, 0.5).unwrap();
        assert!((dev.outputs()[8].volume - 0.25).abs() < 1e-6);
        assert!((dev.outputs()[9].volume - 0.5).abs() < 1e-6);
        assert_eq!(dev.pan(ChannelId::Output(8), 0).unwrap(), 50);
    }

    #[test]
    fn uc_phase_is_supported_even_though_protocol_features_is_false() {
        let (dev, _writes) = open(Model::Uc);
        assert!(!dev.protocol_features());
        assert!(dev.phase_supported());
    }

    #[test]
    fn uc_phase_sets_the_matrix_sign_bit_and_survives_fader_moves() {
        let (mut dev, writes) = open(Model::Uc);
        dev.set_volume(ChannelId::Input(0), 0, 1.0).unwrap();

        writes.lock().unwrap().clear();
        dev.set_phase(0, true).unwrap();
        let (value, index) = uc::matrix_word(1.0, 0);
        assert!(writes.lock().unwrap().contains(&Write::Vendor(
            protocol::req::MIXER,
            value.wrapping_neg(),
            index
        )));
        assert!(dev.inputs()[0].phase);

        // Moving the fader after engaging phase must not silently undo it.
        writes.lock().unwrap().clear();
        dev.set_volume(ChannelId::Input(0), 0, 0.5).unwrap();
        let (value, index) = uc::matrix_word(0.5, 0);
        assert!(writes.lock().unwrap().contains(&Write::Vendor(
            protocol::req::MIXER,
            value.wrapping_neg(),
            index
        )));

        writes.lock().unwrap().clear();
        dev.set_phase(0, false).unwrap();
        let (value, index) = uc::matrix_word(0.5, 0);
        assert!(writes
            .lock()
            .unwrap()
            .contains(&Write::Vendor(protocol::req::MIXER, value, index)));
        assert!(!dev.inputs()[0].phase);
    }

    #[test]
    fn uc_matrix_cell_writes_one_mono_side() {
        let (mut dev, writes) = open(Model::Uc);
        // Playback 1 (source 18) into AN4 (block 3), the side its stereo
        // pair does not normally reach.
        dev.set_matrix_cell(ChannelId::Playback(0), 3, 0.5)
            .unwrap();
        let (value, index) = uc::matrix_word(0.5, 3 * 36 + 18);
        assert!(writes
            .lock()
            .unwrap()
            .contains(&Write::Vendor(protocol::req::MIXER, value, index)));
        assert_eq!(dev.matrix_cell(ChannelId::Playback(0), 3), Some(0.5));
        // The pair model is untouched for the off-side cell.
        assert_eq!(dev.volume(ChannelId::Playback(0), 1).unwrap(), 1.0);
    }

    #[test]
    fn uc_matrix_natural_cell_updates_the_pair_model() {
        let (mut dev, _writes) = open(Model::Uc);
        dev.set_matrix_cell(ChannelId::Playback(0), 2, 0.25)
            .unwrap();
        assert_eq!(dev.volume(ChannelId::Playback(0), 1).unwrap(), 0.25);
        assert_eq!(dev.matrix_cell(ChannelId::Playback(0), 2), Some(0.25));
    }

    #[test]
    fn uc_mono_input_cell_is_independent_and_cleared_by_pan() {
        let (mut dev, _writes) = open(Model::Uc);
        // AN1 is mono: its pair value is pan-derived (both sides 1.0 at
        // centre).  A mono-cell edit changes only the addressed side.
        dev.set_matrix_cell(ChannelId::Input(0), 2, 0.5).unwrap();
        assert_eq!(dev.matrix_cell(ChannelId::Input(0), 2), Some(0.5));
        assert_eq!(dev.matrix_cell(ChannelId::Input(0), 3), Some(1.0));
        // Moving the pan/volume returns both sides to the pair model.
        dev.set_volume(ChannelId::Input(0), 1, 0.8).unwrap();
        assert_eq!(dev.matrix_cell(ChannelId::Input(0), 2), Some(0.8));
        assert_eq!(dev.matrix_cell(ChannelId::Input(0), 3), Some(0.8));
    }

    #[test]
    fn uc_loopback_follows_the_output_pair_link() {
        let (mut dev, writes) = open(Model::Uc);
        // Outputs default to linked: AN3 loopback moves the AN3/4 pair.
        dev.set_loopback_channel(4, true).unwrap();
        {
            let w = writes.lock().unwrap();
            assert!(w.contains(&Write::Vendor(protocol::req::LOOPBACK, 1, 2)));
            assert!(w.contains(&Write::Vendor(protocol::req::LOOPBACK, 1, 3)));
        }
        assert!(dev.outputs()[4].loopback);
        assert!(dev.outputs()[5].loopback);

        // Split AN3/4: loopback then applies to AN3 only.  Clear the
        // inherited AN4 loopback first so the per-channel write is visible.
        dev.set_output_linked(2, false).unwrap();
        dev.set_loopback_channel(5, false).unwrap();
        writes.lock().unwrap().clear();
        dev.set_loopback_channel(4, true).unwrap();
        {
            let w = writes.lock().unwrap();
            assert!(w.contains(&Write::Vendor(protocol::req::LOOPBACK, 1, 2)));
            assert!(!w.contains(&Write::Vendor(protocol::req::LOOPBACK, 1, 3)));
        }
        assert!(dev.outputs()[4].loopback);
        assert!(!dev.outputs()[5].loopback);
    }

    #[test]
    fn uc_scene_carries_off_side_matrix_cells() {
        let (mut dev, _writes) = open(Model::Uc);
        dev.set_matrix_cell(ChannelId::Playback(0), 3, 0.5)
            .unwrap();
        let scene = dev.capture_scene();
        assert_eq!(scene.matrix_overrides.len(), 1);
        let cell = scene.matrix_overrides[0];
        assert_eq!((cell.source, cell.block), (18, 3));
        assert!((cell.gain - 0.5).abs() < 1e-6);
    }

    #[test]
    fn uc_scene_writes_the_full_matrix_and_masters() {
        let (mut dev, writes) = open(Model::Uc);
        let scene = dev.capture_scene();
        writes.lock().unwrap().clear();
        dev.apply_scene(&scene).unwrap();
        let w = writes.lock().unwrap();
        let mixer = w
            .iter()
            .filter(|x| matches!(x, Write::Vendor(protocol::req::MIXER, _, _)))
            .count();
        // 36 sources × 18 blocks, plus the 18 matrix masters: the same
        // 666-request connect write TotalMix performs.
        assert_eq!(mixer, 36 * 18 + 18);
        assert!(w.iter().any(|x| matches!(
            x,
            Write::Vendor(protocol::req::MIXER, _, idx)
                if (idx & 0x3fff) == uc::MIX_MASTER
        )));
    }

    #[test]
    fn uc_scene_connect_sequence_matches_totalmix() {
        let (mut dev, writes) = open(Model::Uc);
        let scene = dev.capture_scene();
        writes.lock().unwrap().clear();
        dev.apply_scene(&scene).unwrap();
        let w = writes.lock().unwrap();

        // Exact TotalMix connect count: 30 × 0x15, 18 × 0x1a, 666 × 0x12.
        assert_eq!(w.len(), 714);
        for (i, write) in w[..30].iter().enumerate() {
            assert_eq!(
                *write,
                Write::Vendor(protocol::req::LOOPBACK, 0, i as u16)
            );
        }

        // The first pair is ADAT 7/8: encoder channels 20/21, masters
        // 1008/1009, then the two blocks interleaved.
        assert_eq!(w[30], Write::Vendor(protocol::req::PGA, 6, 20));
        assert_eq!(w[31], Write::Vendor(protocol::req::PGA, 6, 21));
        assert_eq!(w[32], Write::Vendor(protocol::req::MIXER, 0x2000, 1008));
        assert_eq!(w[33], Write::Vendor(protocol::req::MIXER, 0x2000, 1009));
        assert_eq!(w[34], Write::Vendor(protocol::req::MIXER, 0x2000, 576));
        assert_eq!(w[35], Write::Vendor(protocol::req::MIXER, 0x2000, 612));

        // Phones is the last pair, with masters 998/999.
        let phones = w.len() - 76;
        assert_eq!(w[phones], Write::Vendor(protocol::req::PGA, 6, 10));
        assert_eq!(w[phones + 1], Write::Vendor(protocol::req::PGA, 6, 11));
        assert_eq!(
            w[phones + 2],
            Write::Vendor(protocol::req::MIXER, 0x2000, 998)
        );
        assert_eq!(
            w[phones + 3],
            Write::Vendor(protocol::req::MIXER, 0x2000, 999)
        );
    }

    #[test]
    fn uc_output_volume_uses_the_encoder_channel() {
        let (mut dev, writes) = open(Model::Uc);
        // Internal output 0 = Phones; the UC encoder puts it at slot 3.
        // The fader path sets both linked sides; use it here so the two
        // master blocks are written.
        dev.set_output_volume(0, 0, 1.0).unwrap();
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Vendor(protocol::req::PGA, 6, 10)));
        // The 14-bit master must follow the front-panel code.
        assert!(w.contains(&Write::Vendor(protocol::req::MIXER, 0x2000, 998)));
        assert!(w.contains(&Write::Vendor(protocol::req::MIXER, 0x2000, 999)));
    }

    #[test]
    fn uc_output_volume_covers_the_full_hardware_range() {
        let (mut dev, writes) = open(Model::Uc);
        dev.set_volume(ChannelId::Output(0), 0, 2.0).unwrap();
        dev.set_volume(ChannelId::Output(0), 0, 0.1).unwrap();
        let w = writes.lock().unwrap();
        // +6 dB = raw 0, -20 dB = raw 26, both on the Phones encoder.
        assert!(w.contains(&Write::Vendor(protocol::req::PGA, 0, 10)));
        assert!(w.contains(&Write::Vendor(protocol::req::PGA, 26, 10)));
    }

    #[test]
    fn uc_front_panel_output_volume_becomes_the_phones_fader() {
        let (mut dev, _writes) = open(Model::Uc);
        // Phones is encoder pair 3, channels 10/11.  Raw 0 = +6 dB.
        dev.uc_status_event(&[0, 0, 0x80, 10]);
        assert!((dev.outputs()[0].volume - 1.9953).abs() < 0.001);
        assert!((dev.outputs()[1].volume - 1.9953).abs() < 0.001);
        // Raw 6 = 0 dB on the left channel only.
        dev.uc_status_event(&[0, 0, 6, 10]);
        assert!((dev.outputs()[0].volume - 1.0).abs() < 1e-6);
        assert!((dev.outputs()[1].volume - 1.9953).abs() < 0.001);
        // Real PH frame for -20 dB: 00 00 9a 0a.  The low 7 bits are
        // the 6-dB-per-step attenuator code; bit 7 is the output type.
        dev.uc_status_event(&[0, 0, 0x9a, 10]);
        assert!((dev.outputs()[0].volume - 0.1).abs() < 0.001);
        assert!((dev.outputs()[1].volume - 0.1).abs() < 0.001);
    }

    #[test]
    fn uc_startup_suppression_ignores_queued_output_events() {
        let (mut dev, _writes) = open(Model::Uc);
        let before = dev.outputs()[0].volume;
        dev.suppress_front_panel_for(Duration::from_millis(50));
        // A front-panel move made while TuxMix was not running can arrive
        // after the connect write.  It must not replace the host scene.
        dev.uc_status_event(&[0, 0, 0x9a, 10]);
        assert!((dev.outputs()[0].volume - before).abs() < 1e-6);
        std::thread::sleep(Duration::from_millis(60));
        dev.uc_status_event(&[0, 0, 0x9a, 10]);
        assert!((dev.outputs()[0].volume - 0.1).abs() < 0.001);
    }

    #[test]
    fn uc_front_panel_half_db_gain_is_preserved() {
        let (mut dev, _writes) = open(Model::Uc);
        dev.uc_status_event(&[0, 0, 3, 2]);
        assert_eq!(dev.inputs()[2].gain, Some(3));
    }

    #[test]
    fn uc_reference_and_options_use_the_windows_registers() {
        let (mut dev, writes) = open(Model::Uc);
        writes.lock().unwrap().clear();
        dev.set_line_in_ref(3).unwrap();
        dev.set_spdif_professional(true).unwrap();
        let w = writes.lock().unwrap();
        // Line In -10 dBV: bits 9:8 = 11, base 0x3000 -> 0x3780.
        assert!(w.contains(&Write::Vendor(protocol::req::LEVELS, 0x3780, 0xffff)));
        // Options word: commit + SPDIF professional, wIndex 1487.
        assert!(w.contains(&Write::Vendor(protocol::req::SETTINGS, 0x0081, 1487)));
    }

    #[test]
    fn uc_scene_apply_writes_only_the_mixer_group() {
        let (mut dev, writes) = open(Model::Uc);
        let mut scene = dev.capture_scene();
        scene.settings.clock_source = "SPDIF Coaxial".into();
        scene.settings.optical_out_spdif = true;
        scene.inputs[0].gain = Some(20);
        scene.inputs[0].phantom = true;
        scene.inputs[2].pad = true;
        scene.inputs[2].instrument = true;
        writes.lock().unwrap().clear();
        dev.apply_scene(&scene).unwrap();

        let w = writes.lock().unwrap();
        // Device-owned writes must not be part of a scene apply.
        assert!(!w.iter().any(|x| matches!(x, Write::Vendor(protocol::req::LEVELS, ..))));
        assert!(!w.iter().any(|x| matches!(
            x,
            Write::Vendor(protocol::req::SETTINGS, _, 1487)
        )));
        assert!(!w.iter().any(|x| matches!(
            x,
            Write::Vendor(protocol::req::PGA, _, 0..=3)
        )));
        // The mixer still goes through.
        assert!(w.iter().any(|x| matches!(
            x,
            Write::Vendor(protocol::req::MIXER, ..)
        )));
        // Switches/clock are device-known, so the scene's values were not
        // displayed.  Input gain has no readback; the last known value is
        // left for display and still not written.
        assert_eq!(dev.inputs()[0].gain, Some(20));
        assert!(!dev.inputs()[0].phantom);
        assert_eq!(dev.settings().clock_source, "Internal");
    }

    #[test]
    fn ucx_input_gain_state_words_update_the_model() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.apply_ucx_state_word(0x0007, 400);
        dev.apply_ucx_state_word(0x0207, 15);
        assert_eq!(dev.inputs()[0].gain, Some(40));
        assert_eq!(dev.inputs()[2].gain, Some(3));
        assert_eq!(dev.inputs()[2].gain_max, Some(24));
    }

    #[test]
    fn ucx_set_gain_writes_tenths_db_and_keeps_half_db_units() {
        let (mut dev, writes) = open(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_gain(0, 40).unwrap();
        dev.set_gain(2, 3).unwrap();
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Words(vec![(0x0007, 400)])));
        assert!(w.contains(&Write::Words(vec![(0x0207, 15)])));
        assert_eq!(dev.inputs()[2].gain, Some(3));
    }

    #[test]
    fn ucx_scene_adopts_a_read_back_gain_over_the_saved_value() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.apply_ucx_state_word(0x0207, 15);
        let mut scene = dev.capture_scene();
        scene.inputs[2].gain = Some(0);
        dev.apply_scene(&scene).unwrap();
        assert_eq!(dev.inputs()[2].gain, Some(3));
    }

    #[test]
    fn ucx_old_scene_gain_max_12_is_migrated_to_half_db_units() {
        let (mut dev, _writes) = open(Model::Ucx);
        let mut scene = dev.capture_scene();
        scene.inputs[2].gain = Some(6);
        scene.inputs[2].gain_max = Some(12);
        dev.apply_scene(&scene).unwrap();
        assert_eq!(dev.inputs()[2].gain, Some(12));
        assert_eq!(dev.inputs()[2].gain_max, Some(24));
    }

    #[test]
    fn ucx_readable_state_reports_gain_and_volume_only() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.apply_ucx_state_word(0x0007, 400);
        dev.apply_ucx_state_word(0x1800, 0xfdda);
        let state = dev.readable_state().expect("UCX readable state");
        assert!(state.ucx);
        assert_eq!(state.input_gain, vec![(0, 40)]);
        assert_eq!(state.output_volume.len(), 1);

        let mut scene = dev.capture_scene();
        scene.inputs[0].gain = Some(0);
        scene.outputs[0].volume = 1.0;
        let differences = state.differences(&scene);
        assert!(differences.iter().any(|item| item.contains("AN1 gain")));
        assert!(differences.iter().any(|item| item.contains("volume")));
        assert!(
            !differences.iter().any(|item| item.contains("Clock")),
            "the UCX does not read back the options word"
        );

        writes.lock().unwrap().clear();
        dev.restore_readable(&state).unwrap();
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Words(vec![(0x0007, 400)])));
        assert!(w.iter().any(|write| matches!(
            write,
            Write::Words(words) if words.iter().any(|(addr, _)| *addr == 0x1800)
        )));
    }

    #[test]
    fn ucx_options_word_composes_the_measured_bits() {
        let (mut dev, writes) = open(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_word_clock_termination(true).unwrap();
        dev.set_spdif_professional(true).unwrap();
        dev.set_word_clock_single_speed(true).unwrap();
        dev.set_eq_for_record(true).unwrap();
        dev.set_optical_out_format(true).unwrap();
        // EQ+D on clears bit 8; SPDIF optical clears bit 10.
        assert!(writes
            .lock()
            .unwrap()
            .contains(&Write::Vendor(protocol::req::SETTINGS, 0x00C8, 1487)));
        dev.set_eq_for_record(false).unwrap();
        dev.set_optical_out_format(false).unwrap();
        dev.set_word_clock_termination(false).unwrap();
        assert!(writes
            .lock()
            .unwrap()
            .contains(&Write::Vendor(protocol::req::SETTINGS, 0x05C0, 1487)));
    }

    #[test]
    fn ucx_option_writes_keep_the_clock_source() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.set_clock_source("Word Clock").unwrap();
        writes.lock().unwrap().clear();
        dev.set_optical_out_format(true).unwrap();
        dev.set_word_clock_termination(true).unwrap();
        // Word Clock = index 3 (bits 1–2), EQ+D off, SPDIF optical,
        // termination on.
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Vendor(protocol::req::SETTINGS, 0x0106, 1487)));
        assert!(w.contains(&Write::Vendor(protocol::req::SETTINGS, 0x010E, 1487)));
    }

    #[test]
    fn ucx_clock_sources_exclude_ltc() {
        let (dev, _writes) = open(Model::Ucx);
        assert_eq!(
            dev.settings().clock_sources,
            vec!["Internal", "SPDIF Coaxial", "Optical", "Word Clock"]
        );
        let (mut dev, _writes) = open(Model::Ucx);
        assert!(dev.set_clock_source("LTC").is_err());
        assert_eq!(dev.settings().clock_source, "Internal");
    }

    #[test]
    fn ucx_scene_keeps_its_preamp_group_at_connect() {
        let (mut dev, _writes) = open(Model::Ucx);
        let mut scene = dev.capture_scene();
        scene.inputs[0].gain = Some(20);
        scene.inputs[0].phantom = true;
        scene.inputs[0].autoset = true;
        scene.inputs[2].level = Some(InputLevel::LoGain);
        scene.inputs[2].ref_level = 2;
        scene.inputs[2].instrument = true;
        dev.apply_scene(&scene).unwrap();

        let inputs = dev.inputs();
        assert_eq!(inputs[0].gain, Some(20));
        assert!(inputs[0].phantom);
        assert!(inputs[0].autoset);
        assert_eq!(inputs[2].level, Some(InputLevel::LoGain));
        assert_eq!(inputs[2].ref_level, 2);
        assert!(inputs[2].instrument);
    }

    #[test]
    fn ucx_scene_keeps_its_device_group_at_connect() {
        // The UCX options word has no readback, so the saved scene is the
        // only record of the device-owned settings.  Applying it must not
        // replace them with a fresh model's defaults.
        let (mut dev, writes) = open(Model::Ucx);
        let mut scene = dev.capture_scene();
        scene.settings.clock_source = "Word Clock".into();
        scene.settings.optical_out_spdif = true;
        scene.settings.spdif_professional = true;
        scene.settings.word_clock_single_speed = true;
        scene.settings.word_clock_termination = true;
        scene.settings.eq_for_record = true;
        writes.lock().unwrap().clear();
        dev.apply_scene(&scene).unwrap();

        let settings = dev.settings();
        assert_eq!(settings.clock_source, "Word Clock");
        assert!(settings.optical_out_spdif);
        assert!(settings.spdif_professional);
        assert!(settings.word_clock_single_speed);
        assert!(settings.word_clock_termination);
        assert!(settings.eq_for_record);
        // Device-owned settings are never part of the connect write.
        assert!(!writes
            .lock()
            .unwrap()
            .iter()
            .any(|w| matches!(w, Write::Vendor(protocol::req::SETTINGS, _, 1487))));
    }

    #[test]
    fn ucx_scene_without_eq_keeps_input_eq_topology() {
        // Old scene files predate input EQ and deserialize `eq` as None.
        // Applying one must not wipe the UCX profile's EQ (and with it the
        // phase control, which is gated on `eq.is_some()`).
        let (mut dev, _writes) = open(Model::Ucx);
        assert!(dev.inputs()[0].eq.is_some());
        let mut scene = dev.capture_scene();
        for input in &mut scene.inputs {
            input.eq = None;
        }
        dev.apply_scene(&scene).unwrap();
        assert!(dev.inputs()[0].eq.is_some());
        assert!(dev.inputs().iter().all(|i| i.eq.is_some()));
    }

    #[test]
    fn uc_switch_write_is_a_full_register_read_modify_write() {
        let (mut dev, writes) = open(Model::Uc);
        dev.set_pad(2, true).unwrap();
        let w = writes.lock().unwrap();
        // 0x3480 is the initial register; PAD on AN3 is bit 3.
        assert!(w.contains(&Write::Vendor(protocol::req::LEVELS, 0x3488, 0xffff)));
        assert!(dev.inputs()[2].pad);
    }

    #[test]
    fn uc_inputs_three_and_four_use_half_db_codes() {
        let (mut dev, writes) = open(Model::Uc);
        dev.set_gain(2, 36).unwrap();
        dev.set_gain(3, 1).unwrap();
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Vendor(protocol::req::PGA, 36, 2)));
        assert!(w.contains(&Write::Vendor(protocol::req::PGA, 1, 3)));
        assert_eq!(dev.inputs()[2].gain_max, Some(36));
    }

    #[test]
    fn meter_bank_reads_the_latest_power() {
        let mut b = [0u8; 244];
        b[0..8].copy_from_slice(&(512u64 * 1000 * 1000).to_le_bytes());
        b[240..244].copy_from_slice(&0x2222_2222u32.to_le_bytes());
        let p = MeterPacket::parse(&b).unwrap();
        let mut bank = MeterBank::default();
        bank.apply(&p);
        assert!(bank.playback(0) > 0.0);
        assert_eq!(bank.output(0, false), 0.0);
    }
}
