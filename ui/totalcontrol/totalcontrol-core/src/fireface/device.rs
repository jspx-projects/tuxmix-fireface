//! Fireface UC/UCX userspace device model over the kernel hwdep transport.
//!
//! This is the replacement for the old `ucx_alsa` backend: the kernel no
//! longer exposes device-model ALSA controls, so all routing, fader, preamp,
//! meter and scene logic lives here.
//!
//! Both models share the TotalControl [`RmeDevice`] surface, but their protocol
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
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::channel::{
    ChannelId, ChannelType, Dynamics, Echo, EqBandType, InputChannel, InputEq, InputLevel,
    OutputChannel, PlaybackChannel, Reverb, Sensitivity,
};
use crate::device::{
    DeviceSettings, RmeDevice, TalkbackConfig, output_balance_from_volumes,
    output_balance_to_volumes,
};
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
/// Pause after a multi-word IF1 burst.  The device drains a 63-word
/// burst slower than a single register; TotalMix's Windows capture spaces
/// its connect bursts by roughly 16-33 ms.  5 ms per burst overflows the
/// receive FIFO (ETIMEDOUT).
const UCX_WRITE_BURST_GAP: Duration = Duration::from_millis(16);
/// Largest IF1 burst the hwdep ABI accepts.  Scene writes are coalesced
/// into chunks of this size so the pacing applies once per burst rather
/// than once per register.
const UCX_WRITE_CHUNK: usize = HWDEP_MAX_WORDS;

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

    /// Transport health counters, when the backend has them.
    fn counters(&self) -> Option<HwdepCounters> {
        None
    }
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

    fn counters(&self) -> Option<HwdepCounters> {
        self.hw.counters().ok()
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
    inputs_a_peak: [u32; 18],
    inputs_b_peak: [u32; 18],
    playback_peak: [u32; 18],
    pre_fader_peak: [u32; 18],
    post_fader_peak: [u32; 18],
    fx_in: u64,
    fx_out: u64,
    fx_peak_in: u32,
    fx_peak_out: u32,
}

impl MeterBank {
    fn apply(&mut self, p: &MeterPacket) {
        match p.kind {
            MeterKind::InputsA => {
                self.inputs_a.copy_from_slice(&p.power[..18]);
                self.inputs_a_peak.copy_from_slice(&p.peak);
            }
            MeterKind::InputsB => {
                self.inputs_b.copy_from_slice(&p.power[..18]);
                self.inputs_b_peak.copy_from_slice(&p.peak);
            }
            MeterKind::Playback => {
                self.playback.copy_from_slice(&p.power[..18]);
                self.playback_peak.copy_from_slice(&p.peak);
            }
            MeterKind::OutputsPreFader => {
                self.pre_fader.copy_from_slice(&p.power[..18]);
                self.pre_fader_peak.copy_from_slice(&p.peak);
                self.fx_in = p.fx_power[0];
                self.fx_out = p.fx_power[1];
                self.fx_peak_in = p.fx_peak[0];
                self.fx_peak_out = p.fx_peak[1];
            }
            MeterKind::OutputsPostFader => {
                self.post_fader.copy_from_slice(&p.power[..18]);
                self.post_fader_peak.copy_from_slice(&p.peak);
                self.fx_in = p.fx_power[0];
                self.fx_out = p.fx_power[1];
                self.fx_peak_in = p.fx_peak[0];
                self.fx_peak_out = p.fx_peak[1];
            }
        }
    }

    fn fx(&self, out: bool) -> f32 {
        let p = if out { self.fx_out } else { self.fx_in };
        db_to_linear(protocol::power_dbfs(p))
    }

    fn fx_peak_level(&self, out: bool) -> f32 {
        let p = if out {
            self.fx_peak_out
        } else {
            self.fx_peak_in
        };
        db_to_linear(protocol::peak_dbfs(p))
    }

    fn input(&self, i: usize) -> f32 {
        let p = self.inputs_a.get(i).copied().unwrap_or(0)
            .max(self.inputs_b.get(i).copied().unwrap_or(0));
        db_to_linear(protocol::power_dbfs(p))
    }

    fn input_peak(&self, i: usize) -> f32 {
        let p = self.inputs_a_peak.get(i).copied().unwrap_or(0)
            .max(self.inputs_b_peak.get(i).copied().unwrap_or(0));
        db_to_linear(protocol::peak_dbfs(p))
    }

    fn playback(&self, i: usize) -> f32 {
        db_to_linear(protocol::power_dbfs(self.playback.get(i).copied().unwrap_or(0)))
    }

    fn playback_peak(&self, i: usize) -> f32 {
        db_to_linear(protocol::peak_dbfs(
            self.playback_peak.get(i).copied().unwrap_or(0),
        ))
    }

    fn output(&self, i: usize, post: bool) -> f32 {
        let p = if post {
            self.post_fader.get(i).copied().unwrap_or(0)
        } else {
            self.pre_fader.get(i).copied().unwrap_or(0)
        };
        db_to_linear(protocol::power_dbfs(p))
    }

    fn output_peak(&self, i: usize, post: bool) -> f32 {
        let p = if post {
            self.post_fader_peak.get(i).copied().unwrap_or(0)
        } else {
            self.pre_fader_peak.get(i).copied().unwrap_or(0)
        };
        db_to_linear(protocol::peak_dbfs(p))
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
/// Phones fourth); TotalControl's output model is Phones-first.  `order` is the
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
        // Band 2 is fixed to Bell.  Band 1 has Bell and Low Shelf; band 3
        // has Bell, High Shelf and Hi Cut.  Anything else is normalised to
        // Bell so a legacy scene or caller cannot write a type the unit
        // does not have.
        s.eq.bands[i].kind = match (i, band.band_type) {
            (1, _) => 0,
            (0, EqBandType::LowShelf) => 1,
            (0, _) => 0,
            (2, EqBandType::HighShelf) => 1,
            (2, EqBandType::LowPass) => 2,
            (2, _) => 0,
            _ => 0,
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
                    "Clock source: device {}, TotalControl {}",
                    self.clock_source, scene.settings.clock_source
                ));
            }
            if self.optical_out_spdif != scene.settings.optical_out_spdif {
                out.push(format!(
                    "Optical output: device {}, TotalControl {}",
                    if self.optical_out_spdif { "SPDIF" } else { "ADAT" },
                    if scene.settings.optical_out_spdif { "SPDIF" } else { "ADAT" }
                ));
            }
            if self.spdif_professional != scene.settings.spdif_professional {
                out.push(format!(
                    "SPDIF format: device {}, TotalControl {}",
                    if self.spdif_professional { "professional" } else { "consumer" },
                    if scene.settings.spdif_professional { "professional" } else { "consumer" }
                ));
            }
            for i in 0..2 {
                if let Some(input) = scene.inputs.get(i) {
                    if self.phantom[i] != input.phantom {
                        out.push(format!(
                            "AN{} 48 V: device {}, TotalControl {}",
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
                            "AN{} PAD: device {}, TotalControl {}",
                            i + 1,
                            self.pad[slot],
                            input.pad
                        ));
                    }
                    if self.instrument[slot] != input.instrument {
                        out.push(format!(
                            "AN{} Inst: device {}, TotalControl {}",
                            i + 1,
                            self.instrument[slot],
                            input.instrument
                        ));
                    }
                }
            }
        }
        if !self.ucx {
            for &(idx, gain) in &self.input_gain {
                if let Some(input) = scene.inputs.get(idx) {
                    if input.gain != Some(gain) {
                        let (dev, ours) = if idx < 2 {
                            (format!("{gain} dB"), format!("{} dB", input.gain.unwrap_or(0)))
                        } else {
                            (
                                format!("{:.1} dB", gain as f32 / 2.0),
                                format!("{:.1} dB", input.gain.unwrap_or(0) as f32 / 2.0),
                            )
                        };
                        out.push(format!(
                            "AN{} gain: device {dev}, TotalControl {ours}",
                            idx + 1
                        ));
                    }
                }
            }
        }
        for &(idx, volume) in &self.output_volume {
            if let Some(output) = scene.outputs.get(idx) {
                let dev_db = linear_db(volume);
                let ours_db = linear_db(output.volume);
                if (dev_db - ours_db).abs() > 0.05 {
                    out.push(format!(
                        "{} volume: device {dev_db:.1} dB, TotalControl {ours_db:.1} dB",
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

/// A Fireface UC or UCX opened over the kernel hwdep transport.
pub struct Fireface {
    transport: Arc<Mutex<Box<dyn ControlTransport>>>,
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
    /// FX-return levels saved while Control Room Mute FX is engaged.
    mute_fx_saved: Option<Vec<(usize, f32)>>,
    /// Target pairs and FX-return levels saved while External Input is
    /// engaged.  The matrix itself is restored from the channel model.
    external_input_saved: Option<(Vec<usize>, Vec<(usize, f32)>)>,
    /// Cue target pair and its FX-return levels while PFL is active.
    pfl_saved: Option<(usize, Vec<(usize, f32)>)>,
    pfl_active: HashSet<ChannelId>,
    pfl_target: Option<usize>,
    pfl_mode: bool,
    pfl_exclusive: bool,
    /// Speaker pairs and their saved levels while control-room Dim is
    /// engaged.  RME's Dim attenuates the monitor speakers (Main and
    /// Speaker B), not the Phones outputs.
    dim_saved: Vec<(usize, f32, f32)>,
    /// Attenuation applied by the current control-room Dim.
    dim_amount_db: f32,
    /// Speaker B follows Main Out level changes.
    speaker_b_linked: bool,
    /// Register words replaced by the current Talkback activation, in write
    /// order.  Restored when Talkback is released.
    talkback_saved: Option<Vec<(u16, u16)>>,
    talkback_active: bool,
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
    /// Raw words from the last requested UCX state dump.
    state_words: HashMap<u16, u16>,
    /// Pre-connect snapshot once the startup dump completes.
    pre_connect_words: HashMap<u16, u16>,
    startup_dump_pending: bool,
    startup_dump_started: Instant,
    startup_dump_timeout: Duration,
    /// Load a scene into the model without issuing device writes.
    dry_run: bool,
    /// UCX IF1 heartbeat sequence and next refresh time.  Without this the
    /// unit does not stream live state deltas such as the Phones knob.
    ucx_heartbeat: u16,
    ucx_heartbeat_due: Instant,
    /// During the first moments after connect the unit can still deliver
    /// `0x8b` output-volume events for a front-panel move made while TotalControl
    /// was not running.  TotalMix's connect write wins over those stale
    /// events; ignore them until the host scene has been reapplied.
    ignore_output_until: Option<Instant>,
    /// Mono matrix cells that the per-pair model cannot express. Key is
    /// `(source 0..35, output block 0..17)`; 18..35 are software playbacks.
    matrix_overrides: HashMap<(usize, usize), f32>,
    /// UC input metering through the PipeWire source tap.  The PCM device
    /// stays with PipeWire and the DAW.  The UCX uses its 0x85 input pages.
    input_tap: Option<crate::pw_input::InputTap>,
    /// UCX IF1 words collected during one scene apply.  `Some` while a
    /// batch is open; `write_word`/`write_words` then append instead of
    /// sending one USB transfer per register.  `vendor_out`/`vendor_in`
    /// flush it first, so control transfers keep their original order.
    write_batch: Arc<Mutex<Option<Vec<(u16, u16)>>>>,
    /// IF1 writer for a real unit.  Live writes are queued here so the UI
    /// thread never sleeps through the device's burst pacing.
    async_writer: Option<Arc<AsyncWriter>>,
}

/// Flushes the open UCX scene batch when `apply_scene` returns early.
/// It holds its own handles into the device, so it does not borrow `self`
/// across the mutable scene update.
struct WriteBatchGuard {
    transport: Arc<Mutex<Box<dyn ControlTransport>>>,
    batch: Arc<Mutex<Option<Vec<(u16, u16)>>>>,
    /// Real-unit writer.  `None` in tests and on the UC, where writes are
    /// synchronous.
    writer: Option<Arc<AsyncWriter>>,
    /// Only the guard that opened the batch flushes and clears it.  Nested
    /// guards (a live control change inside `apply_scene`, or one setter
    /// calling another) leave the outer batch open.
    owns: bool,
    active: bool,
}

impl WriteBatchGuard {
    fn flush(&self) -> Result<(), Error> {
        let words = {
            let mut slot = self.batch.lock().unwrap();
            match slot.as_mut() {
                Some(batch) => std::mem::take(batch),
                None => return Ok(()),
            }
        };
        if words.is_empty() {
            return Ok(());
        }
        match &self.writer {
            Some(writer) => writer.send_words(words),
            None => Fireface::send_words_paced(&self.transport, &words),
        }
    }

    /// Wait until the writer has sent everything queued so far.
    fn wait(&self) -> Result<(), Error> {
        match &self.writer {
            Some(writer) => writer.flush(),
            None => Ok(()),
        }
    }

    /// Queue the batch.  Live controls return here; the writer paces the
    /// transfer off the UI thread.
    fn finish(mut self) -> Result<(), Error> {
        self.active = false;
        if !self.owns {
            return Ok(());
        }
        let result = self.flush();
        *self.batch.lock().unwrap() = None;
        result
    }

    /// Queue the batch and wait for the device writes to finish.  Scene
    /// apply uses this so its completion and errors stay synchronous.
    fn finish_wait(mut self) -> Result<(), Error> {
        self.active = false;
        if !self.owns {
            return Ok(());
        }
        let result = self.flush().and_then(|()| self.wait());
        *self.batch.lock().unwrap() = None;
        result
    }
}

impl Drop for WriteBatchGuard {
    fn drop(&mut self) {
        if !self.active || !self.owns {
            return;
        }
        if let Err(e) = self.flush() {
            log::warn!("Flushing the coalesced IF1 scene failed: {e:?}");
        }
        *self.batch.lock().unwrap() = None;
    }
}

/// Close a live-write batch, preferring the caller's own error when the
/// setter failed part-way through.
fn finish_live_batch<T>(
    batch: Option<WriteBatchGuard>,
    result: Result<T, Error>,
) -> Result<T, Error> {
    match batch {
        Some(batch) => {
            let flushed = batch.finish();
            match result {
                Ok(value) => {
                    flushed?;
                    Ok(value)
                }
                Err(e) => {
                    if let Err(flush_err) = flushed {
                        log::warn!("Flushing a live IF1 change failed: {flush_err:?}");
                    }
                    Err(e)
                }
            }
        }
        None => result,
    }
}

enum WriterCommand {
    Words(Vec<(u16, u16)>),
    Flush(Sender<Result<(), String>>),
    Shutdown,
}

/// Pacer for real-unit IF1 writes.  The UI thread queues words and returns;
/// this thread owns the burst cadence, so a fader or gain drag never sleeps
/// inside `update`.
struct AsyncWriter {
    tx: Sender<WriterCommand>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl AsyncWriter {
    fn new(transport: Arc<Mutex<Box<dyn ControlTransport>>>) -> Option<Self> {
        let (tx, rx) = mpsc::channel();
        match thread::Builder::new()
            .name("totalcontrol-if1-writer".into())
            .spawn(move || writer_loop(rx, transport))
        {
            Ok(join) => Some(Self {
                tx,
                join: Mutex::new(Some(join)),
            }),
            Err(e) => {
                log::warn!("Starting the IF1 writer failed: {e:?}");
                None
            }
        }
    }

    fn send_words(&self, words: Vec<(u16, u16)>) -> Result<(), Error> {
        self.tx
            .send(WriterCommand::Words(words))
            .map_err(|_| writer_stopped())
    }

    fn flush(&self) -> Result<(), Error> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(WriterCommand::Flush(tx))
            .map_err(|_| writer_stopped())?;
        match rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(Error::Io(std::io::Error::other(e))),
            Err(_) => Err(writer_stopped()),
        }
    }
}

impl Drop for AsyncWriter {
    fn drop(&mut self) {
        let _ = self.tx.send(WriterCommand::Shutdown);
        if let Some(join) = self.join.lock().unwrap().take() {
            let _ = join.join();
        }
    }
}

fn writer_stopped() -> Error {
    Error::Io(std::io::Error::other("IF1 writer stopped"))
}

fn writer_loop(rx: Receiver<WriterCommand>, transport: Arc<Mutex<Box<dyn ControlTransport>>>) {
    let mut pending: Vec<(u16, u16)> = Vec::new();
    loop {
        match rx.recv() {
            Ok(WriterCommand::Words(words)) => {
                pending.extend(words);
                // Coalesce everything already queued before spending the
                // burst gap, then send it as one paced batch.
                loop {
                    match rx.try_recv() {
                        Ok(WriterCommand::Words(more)) => {
                            pending.extend(more);
                            if pending.len() >= UCX_WRITE_CHUNK {
                                break;
                            }
                        }
                        Ok(WriterCommand::Flush(reply)) => {
                            let result = flush_pending(&transport, &mut pending);
                            let _ = reply.send(result);
                        }
                        Ok(WriterCommand::Shutdown) => {
                            let _ = flush_pending(&transport, &mut pending);
                            return;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            let _ = flush_pending(&transport, &mut pending);
                            return;
                        }
                    }
                }
                let _ = flush_pending(&transport, &mut pending);
            }
            Ok(WriterCommand::Flush(reply)) => {
                let result = flush_pending(&transport, &mut pending);
                let _ = reply.send(result);
            }
            Ok(WriterCommand::Shutdown) => {
                let _ = flush_pending(&transport, &mut pending);
                return;
            }
            Err(_) => {
                let _ = flush_pending(&transport, &mut pending);
                return;
            }
        }
    }
}

fn flush_pending(
    transport: &Arc<Mutex<Box<dyn ControlTransport>>>,
    pending: &mut Vec<(u16, u16)>,
) -> Result<(), String> {
    if pending.is_empty() {
        return Ok(());
    }
    let words = std::mem::take(pending);
    match Fireface::send_words_paced(transport, &words) {
        Ok(()) => Ok(()),
        Err(e) => {
            let message = format!("{e:?}");
            log::warn!("IF1 writer failed: {message}");
            Err(message)
        }
    }
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
            return Self::from_transport_async(Box::new(transport), info);
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
        Self::from_transport_async(Box::new(transport), info)
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
            return Self::from_transport_async(Box::new(transport), info);
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

    #[cfg(test)]
    fn from_transport(
        transport: Box<dyn ControlTransport>,
        info: DeviceInfo,
    ) -> Result<Self, Error> {
        Self::from_transport_with_writer(transport, info, false)
    }

    /// Driver-side transport health, when the hwdep transport is in use.
    pub fn transport_counters(&self) -> Option<HwdepCounters> {
        self.transport.lock().ok().and_then(|t| t.counters())
    }

    fn from_transport_async(
        transport: Box<dyn ControlTransport>,
        info: DeviceInfo,
    ) -> Result<Self, Error> {
        Self::from_transport_with_writer(transport, info, true)
    }

    fn from_transport_with_writer(
        transport: Box<dyn ControlTransport>,
        info: DeviceInfo,
        async_writer: bool,
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
                    && std::env::var_os("TOTALCONTROL_DEFER_METER_TAP").is_none() =>
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
            transport: Arc::new(Mutex::new(transport)),
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
            mute_fx_saved: None,
            external_input_saved: None,
            pfl_saved: None,
            pfl_active: HashSet::new(),
            pfl_target: None,
            pfl_mode: false,
            pfl_exclusive: false,
            dim_saved: Vec::new(),
            dim_amount_db: -20.0,
            speaker_b_linked: false,
            talkback_saved: None,
            talkback_active: false,
            input_settings,
            output_settings,
            playback_phase,
            playback_width,
            meters: MeterBank::default(),
            autoset: [false; 4],
            device_input_gain: HashMap::new(),
            device_output_volume: HashMap::new(),
            state_words: HashMap::new(),
            pre_connect_words: HashMap::new(),
            startup_dump_pending: false,
            startup_dump_started: Instant::now(),
            startup_dump_timeout: Duration::from_millis(2500),
            dry_run: false,
            ucx_heartbeat: 0,
            ucx_heartbeat_due: Instant::now(),
            ignore_output_until: None,
            matrix_overrides: HashMap::new(),
            input_tap,
            write_batch: Arc::new(Mutex::new(None)),
            async_writer: None,
        };
        if async_writer {
            dev.async_writer = AsyncWriter::new(Arc::clone(&dev.transport)).map(Arc::new);
        }

        if model == Model::Uc {
            dev.read_uc_levels();
            dev.read_uc_settings();
            // Pick up front-panel changes queued while TotalControl was closed.
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
    /// The UC can deliver a `0x8b` for a front-panel move made while TotalControl
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
        if self.dry_run {
            return Ok(());
        }
        // A vendor request cannot be reordered behind queued IF1 words.
        self.flush_write_batch()?;
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
        self.flush_write_batch()?;
        self.transport()
            .vendor(req, Direction::In, value, index, &vec![0u8; len])
    }

    fn write_word(&self, addr: u16, value: u16) -> Result<(), Error> {
        if self.dry_run {
            return Ok(());
        }
        if self.model == Model::Ucx {
            if self.queue_words(&[(addr, value)]) {
                return Ok(());
            }
            return self.send_or_queue(&[(addr, value)]);
        }
        let result = self.transport().write_words(&[(addr, value)]);
        if let Err(e) = &result {
            log::warn!("IF1 word write 0x{addr:04x}=0x{value:04x} failed: {e:?}");
        }
        result
    }

    fn write_words(&self, words: &[(u16, u16)]) -> Result<(), Error> {
        if self.dry_run {
            return Ok(());
        }
        if self.model == Model::Ucx {
            if self.queue_words(words) {
                return Ok(());
            }
            return self.send_or_queue(words);
        }
        let result = self.transport().write_words(words);
        if let Err(e) = &result {
            log::warn!(
                "IF1 burst of {} words starting 0x{:04x} failed: {e:?}",
                words.len(),
                words.first().map(|w| w.0).unwrap_or(0)
            );
        }
        result
    }

    /// Hand a live UCX write to the writer, or send it inline when no
    /// writer is running (tests, and the synchronous fallback).
    fn send_or_queue(&self, words: &[(u16, u16)]) -> Result<(), Error> {
        match &self.async_writer {
            Some(writer) => writer.send_words(words.to_vec()),
            None => self.write_words_paced(words),
        }
    }

    /// Wait until the background writer has sent everything queued.
    #[cfg(test)]
    fn flush_writer(&self) -> Result<(), Error> {
        match &self.async_writer {
            Some(writer) => writer.flush(),
            None => Ok(()),
        }
    }

    /// Append words to the open scene batch.  `false` means no batch is
    /// open, so the caller must send them now.
    fn queue_words(&self, words: &[(u16, u16)]) -> bool {
        if words.is_empty() {
            return false;
        }
        let mut slot = self.write_batch.lock().unwrap();
        match slot.as_mut() {
            Some(batch) => {
                batch.extend_from_slice(words);
                true
            }
            None => false,
        }
    }

    /// Send a word list in ABI-sized bursts, pacing each burst so the
    /// device FIFO can drain.  The hwdep layer splits at the same limit,
    /// but without the gap, so keep the chunking here.
    fn send_words_paced(
        transport: &Mutex<Box<dyn ControlTransport>>,
        words: &[(u16, u16)],
    ) -> Result<(), Error> {
        for chunk in words.chunks(UCX_WRITE_CHUNK) {
            let result = transport.lock().unwrap().write_words(chunk);
            if let Err(e) = &result {
                log::warn!(
                    "IF1 burst of {} words starting 0x{:04x} failed: {e:?}",
                    chunk.len(),
                    chunk.first().map(|w| w.0).unwrap_or(0)
                );
                return result;
            }
            // Small bursts are the live-parameter path; a full 63-word
            // scene burst needs the device's drain time.
            let gap = if chunk.len() <= 8 {
                UCX_WRITE_GAP
            } else {
                UCX_WRITE_BURST_GAP
            };
            std::thread::sleep(gap);
        }
        Ok(())
    }

    fn write_words_paced(&self, words: &[(u16, u16)]) -> Result<(), Error> {
        Self::send_words_paced(&self.transport, words)
    }

    /// Start coalescing UCX IF1 words.  A no-op for the UC, whose writes
    /// are vendor requests rather than register bursts.
    fn begin_write_batch(&self) -> Option<WriteBatchGuard> {
        if self.dry_run || self.model != Model::Ucx {
            return None;
        }
        let mut slot = self.write_batch.lock().unwrap();
        let owns = slot.is_none();
        if owns {
            *slot = Some(Vec::with_capacity(UCX_WRITE_CHUNK));
        }
        drop(slot);
        Some(WriteBatchGuard {
            transport: Arc::clone(&self.transport),
            batch: Arc::clone(&self.write_batch),
            writer: self.async_writer.as_ref().map(Arc::clone),
            owns,
            active: true,
        })
    }

    /// Send everything queued so far but keep the batch open.
    fn flush_write_batch(&self) -> Result<(), Error> {
        if self.dry_run {
            return Ok(());
        }
        let words = {
            let mut slot = self.write_batch.lock().unwrap();
            match slot.as_mut() {
                Some(batch) => std::mem::take(batch),
                None => Vec::new(),
            }
        };
        if let Some(writer) = &self.async_writer {
            // A vendor request must not overtake words already queued by
            // a live control change.
            if !words.is_empty() {
                writer.send_words(words)?;
            }
            return writer.flush();
        }
        if words.is_empty() {
            return Ok(());
        }
        self.write_words_paced(&words)
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
    /// control in TotalControl today.
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
        // One coefficient, one cell. The address six output blocks later
        // (reg + 0x180) is another output's own matrix cell — e.g.
        // 0x4020 is AN 1/2 and 0x41A0 is Phones — so it must not be
        // written here; doing so replaced Phones' playback send with
        // AN 1/2's on every scene apply.
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
        if self.inputs[base].ms_proc {
            // M/S encoder on top of the width matrix:
            //   L' = (L + R) / 2, R' = (L - R) / 2
            let a = gl * (cos + sin) * 0.5;
            let b = gr * (sin + cos) * 0.5;
            let c = gl * (cos - sin) * 0.5;
            let d = gr * (sin - cos) * 0.5;
            self.ucx_write_matrix(dsp as u8, Source::Input(base), a as f64)?;
            self.ucx_write_matrix(dsp as u8, Source::Input(other), b as f64)?;
            self.ucx_write_matrix(dsp as u8 + 1, Source::Input(base), c as f64)?;
            return self.ucx_write_matrix(dsp as u8 + 1, Source::Input(other), d as f64);
        }
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
            if self.inputs[base].width != 1.0 || self.inputs[base].ms_proc {
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
        if self.inputs[base].width != 1.0 || self.inputs[base].ms_proc {
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
        if self.playbacks[base].ms_proc {
            let a = gl * (cos + sin) * 0.5;
            let b = gr * (sin + cos) * 0.5;
            let c = gl * (cos - sin) * 0.5;
            let d = gr * (sin - cos) * 0.5;
            self.ucx_write_matrix(dsp as u8, Source::Playback(dev_l), a as f64)?;
            self.ucx_write_matrix(dsp as u8, Source::Playback(dev_r), b as f64)?;
            self.ucx_write_matrix(dsp as u8 + 1, Source::Playback(dev_l), c as f64)?;
            return self.ucx_write_matrix(
                dsp as u8 + 1,
                Source::Playback(dev_r),
                d as f64,
            );
        }
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
        if self.playbacks[idx & !1].width != 1.0 || self.playbacks[idx & !1].ms_proc {
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
        let monitoring = self.settings.main_out == Some(pair)
            || self.settings.main_b_out == Some(pair);
        self.ucx_write_output_mode_value(
            idx,
            (mute as u16) | ((monitoring as u16) << 1),
        )
    }

    fn ucx_write_output_mode_value(&self, idx: usize, value: u16) -> Result<(), Error> {
        let pair = idx / 2;
        let side = idx % 2;
        let dsp = ucx::output_pair_dsp(pair)
            .ok_or_else(|| Error::InvalidChannel(format!("output {idx}")))?;
        let channel = ucx::output_channel(dsp + side);
        self.write_word(
            ucx::channel_reg(channel, ucx::offset::OUTPUT_MUTE),
            value,
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

    fn ucx_write_input_gain_right(&self, idx: usize, tenths_db: u32) -> Result<(), Error> {
        let channel = ucx::input_channel(idx);
        self.write_word(
            ucx::channel_reg(channel, ucx::offset::GAIN_REFLEVEL_OR_PHASE) + 0x100,
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

    /// Fold a channel's dynamics / Auto Level values into the cached
    /// `ChannelSettings` without writing them.  `ucx_write_channel_settings`
    /// writes the same 12 registers, so a scene apply can update the cache
    /// first and then send the whole block once.
    fn ucx_update_dynamics_settings(
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
        Ok(())
    }

    /// Write the 12 dynamics / Auto Level registers of one hardware input
    /// or output.  The measured UCX block is `NN60..NN67` + `NN80..NN83`.
    fn ucx_write_dynamics(
        &mut self,
        channel: ChannelId,
        dynamics: &Dynamics,
    ) -> Result<(), Error> {
        self.ucx_update_dynamics_settings(channel, dynamics)?;
        let (idx, is_output) = match channel {
            ChannelId::Input(i) => (i, false),
            ChannelId::Output(i) => (i, true),
            ChannelId::Playback(_) => unreachable!("checked above"),
        };
        let settings = if is_output {
            self.output_settings[idx]
        } else {
            self.input_settings[idx]
        };
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
    /// Ask the unit for its full state dump and drain it.  The UCX does not
    /// send this dump spontaneously on Linux: the host must write
    /// `0x3f99 = 0x0812` after the state endpoint is running.  The dump is
    /// then buffered by the kernel and arrives as ordinary `0x8d` words.
    /// Request the pre-connect dump and return immediately.  Normal event
    /// polling drains it; `startup_dump_complete` reports when the snapshot
    /// is ready.
    pub fn begin_startup_state_dump(&mut self, timeout: Duration) -> Result<(), Error> {
        self.state_words.clear();
        self.pre_connect_words.clear();
        self.startup_dump_pending = true;
        self.startup_dump_started = Instant::now();
        self.startup_dump_timeout = timeout;
        self.write_word(0x3f99, 0x0812)?;
        self.flush_write_batch()
    }

    pub fn startup_dump_pending(&self) -> bool {
        self.startup_dump_pending
    }

    pub fn startup_dump_complete(&self) -> bool {
        !self.startup_dump_pending
    }

    pub fn pre_connect_words(&self) -> &HashMap<u16, u16> {
        &self.pre_connect_words
    }

    fn finish_startup_dump(&mut self) {
        if !self.startup_dump_pending {
            return;
        }
        self.startup_dump_pending = false;
        self.pre_connect_words = self.state_words.clone();
        log::info!(
            "captured UCX startup dump: {} words in {:?}",
            self.pre_connect_words.len(),
            self.startup_dump_started.elapsed()
        );
    }

    fn record_ucx_readable_word(&mut self, addr: u16, value: u16) {
        if let Some(idx) = ucx::input_gain_index(addr) {
            let raw = value as u32;
            let (gain, max) = if idx < 2 {
                ((raw + 5) / 10, 65)
            } else {
                ((raw + 2) / 5, 24)
            };
            self.device_input_gain.insert(idx, gain.min(max));
            return;
        }
        if let Some(idx) = ucx::output_index_from_state(addr) {
            let volume = ucx::tenths_to_linear(value);
            self.device_output_volume.insert(idx, volume);
        }
    }

    /// Load a scene into the model without writing any device words.  Used
    /// while the pre-connect dump is in flight, so the window can show the
    /// saved scene without contaminating the dump.
    pub fn load_scene_model(&mut self, scene: &Scene) -> Result<(), Error> {
        self.dry_run = true;
        let result = self.apply_scene(scene);
        self.dry_run = false;
        result
    }

    pub fn capture_state_dump(&mut self, timeout: Duration) -> Result<(), Error> {
        self.startup_dump_pending = false;
        self.state_words.clear();
        self.write_word(0x3f99, 0x0812)?;
        self.flush_write_batch()?;

        let started = Instant::now();
        let mut last_len = 0;
        let mut last_change = Instant::now();
        while started.elapsed() < timeout {
            self.poll_events()?;
            if self.state_words.len() != last_len {
                last_len = self.state_words.len();
                last_change = Instant::now();
            }
            if self.state_words.contains_key(&0x3d00)
                && self.state_words.len() > 1500
                && last_change.elapsed() > Duration::from_millis(120)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.poll_events()?;
        Ok(())
    }

    /// Raw state words from the last capture or live state word.
    pub fn state_words(&self) -> &HashMap<u16, u16> {
        &self.state_words
    }

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

    /// Mark one hardware output as a Talkback receiver.  The flag is
    /// host-side; it only changes which outputs [`Self::set_talkback_with`]
    /// rewrites.
    pub fn set_talkback_receive(&mut self, output: usize, on: bool) -> Result<(), Error> {
        let Some(out) = self.outputs.get_mut(output) else {
            return Err(Error::InvalidChannel(format!("output {output}")));
        };
        out.talkback = on;
        Ok(())
    }

    /// Toggle Talkback using the measured UCX rewrite.
    ///
    /// The source route uses the host configuration; active sends into
    /// receiving outputs use Talkback Dim, active sends into Main use
    /// Listenback Dim, and the affected FX returns are muted.  Every word
    /// replaced by the activation is saved and written back on release.
    pub fn set_talkback_with(
        &mut self,
        on: bool,
        config: &TalkbackConfig,
    ) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Talkback is not mapped on the UC".into(),
            ));
        }
        if on == self.talkback_active {
            return Ok(());
        }
        if !on {
            if let Some(saved) = self.talkback_saved.take() {
                self.write_words(&saved)?;
            }
            self.talkback_active = false;
            return Ok(());
        }

        // Source None is the reference app's "no activation burst" case.
        if config.source.is_none() {
            self.talkback_active = true;
            self.talkback_saved = Some(Vec::new());
            return Ok(());
        }

        let source = config.source.unwrap();
        if source >= self.inputs.len() {
            return Err(Error::InvalidChannel(format!("talkback input {source}")));
        }
        let main_pair = self.settings.main_out;
        let receivers: Vec<usize> = (0..self.output_pair_count())
            .filter(|pair| {
                self.outputs
                    .get(pair * 2)
                    .is_some_and(|out| out.talkback)
                    || self
                        .outputs
                        .get(pair * 2 + 1)
                        .is_some_and(|out| out.talkback)
            })
            .collect();

        // (address, previous value, activation value), de-duplicated by
        // address in case a source route and a normal send share a cell.
        let mut edits: Vec<(u16, u16, u16)> = Vec::new();

        fn push_edit(edits: &mut Vec<(u16, u16, u16)>, addr: u16, old: u16, new: u16) {
            // Fader records deliberately use the same address twice (level
            // then pan), so do not coalesce by address.
            edits.push((addr, old, new));
        }

        fn source_block(
            this: &Fireface,
            edits: &mut Vec<(u16, u16, u16)>,
            pair: usize,
            input: usize,
        ) {
            let Some(dsp) = ucx::output_pair_dsp(pair) else {
                return;
            };
            let volume = this
                .inputs
                .get(input)
                .and_then(|ch| ch.volumes.get(pair))
                .copied()
                .unwrap_or(0.0);
            if volume <= 0.0 {
                return;
            }
            let pan = this
                .inputs
                .get(input)
                .and_then(|ch| ch.pans.get(pair))
                .copied()
                .unwrap_or(0);
            let level = ucx::fader_level(ucx::MINUS_INF_TENTHS);
            let pan_word = ucx::fader_pan(pan as i16);
            // TotalMix's measured source split is 0.700 per side at
            // centre, not the 0.7071 used by the width matrix.
            let matrix = ucx::matrix_gain(volume as f64 * 0.7);
            for dsp_side in [dsp, dsp + 1] {
                let level_addr = ucx::fader_table_reg(dsp_side as u8, input);
                push_edit(edits, level_addr, level, level);
                push_edit(edits, level_addr, pan_word, pan_word);
                let matrix_addr =
                    ucx::matrix_reg(dsp_side as u8, Source::Input(input));
                push_edit(edits, matrix_addr, 0, matrix);
            }
        }

        fn add_sends(
            this: &Fireface,
            edits: &mut Vec<(u16, u16, u16)>,
            pair: usize,
            dim_db: f32,
        ) {
            let Some(dsp) = ucx::output_pair_dsp(pair) else {
                return;
            };
            let factor = 10f64.powf((dim_db as f64) / 20.0).clamp(0.0, 1.0);
            if factor >= 0.999 {
                return;
            }
            // The per-pair model indexes volumes by output pair; the wire
            // address uses the DSP output's side.  Keep those two mappings
            // distinct (pair 2 -> DSP 2, but model block 4).
            for side in 0..2 {
                let model_block = pair * 2 + side;
                let dsp_side = dsp + side;
                for (idx, ch) in this.inputs.iter().enumerate() {
                    let gain = this
                        .matrix_cell(ChannelId::Input(idx), model_block)
                        .unwrap_or(0.0);
                    if gain != 0.0 {
                        let addr = ucx::matrix_reg(dsp_side as u8, Source::Input(ch.id));
                        push_edit(
                            edits,
                            addr,
                            ucx::matrix_gain(gain as f64),
                            ucx::matrix_gain(gain as f64 * factor),
                        );
                    }
                }
                for (idx, ch) in this.playbacks.iter().enumerate() {
                    let gain = this
                        .matrix_cell(ChannelId::Playback(idx), model_block)
                        .unwrap_or(0.0);
                    if gain != 0.0 {
                        let addr = ucx::matrix_reg(dsp_side as u8, Source::Playback(ch.id));
                        push_edit(
                            edits,
                            addr,
                            ucx::matrix_gain(gain as f64),
                            ucx::matrix_gain(gain as f64 * factor),
                        );
                    }
                }
            }
        }

        for &pair in &receivers {
            add_sends(self, &mut edits, pair, config.dim_db);
        }
        if let Some(pair) = main_pair {
            add_sends(self, &mut edits, pair, config.listenback_dim_db);
        }

        // Source routes are written after the attenuated normal sends so a
        // source that is also a normal send cannot overwrite the talkback
        // route with the dimmed value.  The reference app omits the source
        // block when Listenback uses the same input: the captured form is
        // the short 36-byte burst.
        if config.listenback != Some(source) {
            for &pair in &receivers {
                source_block(self, &mut edits, pair, source);
            }
        }
        if let Some(listen) = config.listenback {
            if let Some(pair) = main_pair {
                if listen != source {
                    source_block(self, &mut edits, pair, listen);
                }
            }
        }

        let fx_pairs: Vec<usize> = receivers
            .iter()
            .copied()
            .chain(main_pair.into_iter())
            .collect();
        for pair in fx_pairs {
            for output in [pair * 2, pair * 2 + 1] {
                let Some(out) = self.outputs.get(output) else {
                    continue;
                };
                let old = (out.fx_return_db.clamp(-65.0, 6.0) * 10.0).round() as i16 as u16;
                let pair = output / 2;
                let dsp = ucx::output_pair_dsp(pair)
                    .ok_or_else(|| Error::InvalidChannel(format!("output {output}")))?;
                let addr = ucx::channel_reg(
                    ucx::output_channel(dsp + output % 2),
                    ucx::offset::FX_RETURN,
                );
                push_edit(&mut edits, addr, old, 0xfd76);
            }
        }

        let old_flag = (self.settings.mono as u16) << 1
            | ((self.settings.external_input as u16) << 5);
        let flag_addr = 0x3f9e;
        push_edit(&mut edits, flag_addr, old_flag, old_flag | 1);

        let old: Vec<(u16, u16)> = edits.iter().map(|(a, o, _)| (*a, *o)).collect();
        let new: Vec<(u16, u16)> = edits.iter().map(|(a, _, n)| (*a, *n)).collect();
        self.talkback_saved = Some(old);
        self.talkback_active = true;
        self.write_words(&new)
    }

    /// Compatibility entry point for callers that have no Control Room
    /// configuration yet.  A missing source deliberately produces no burst.
    pub fn set_talkback(&mut self, on: bool) -> Result<(), Error> {
        self.set_talkback_with(on, &TalkbackConfig::default())
    }

    /// Toggle M/S processing on an input pair: `NN05` on both channels and
    /// the M/S send rewrite.  Mono inputs reject it.
    pub fn set_input_ms_proc(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "MS Proc is not mapped on the UC".into(),
            ));
        }
        if idx >= self.inputs.len() {
            return Err(Error::InvalidChannel(format!("input {idx}")));
        }
        if idx < 4 && !self.input_pair_linked(idx / 2) {
            return Err(Error::InvalidChannel(
                "MS Proc needs a linked stereo pair".into(),
            ));
        }
        let base = idx & !1;
        let other = base ^ 1;
        for i in [base, other] {
            if let Some(input) = self.inputs.get_mut(i) {
                input.ms_proc = on;
            }
            self.write_word(
                ucx::channel_reg(ucx::input_channel(i), ucx::offset::MS_PROC),
                on as u16,
            )?;
        }
        for pair in 0..PAIRS {
            self.ucx_write_input_pair(base, pair)?;
        }
        Ok(())
    }

    /// Toggle M/S processing on a software-playback pair.  There is no
    /// register: every routing with a live send is rewritten.
    pub fn set_playback_ms_proc(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "MS Proc is not mapped on the UC".into(),
            ));
        }
        let base = idx & !1;
        let other = base ^ 1;
        if other >= self.playbacks.len() {
            return Err(Error::InvalidChannel(format!("playback {idx}")));
        }
        self.playbacks[base].ms_proc = on;
        self.playbacks[other].ms_proc = on;
        for pair in 0..PAIRS {
            self.ucx_write_playback_pair(base, pair)?;
        }
        Ok(())
    }

    /// Re-apply the readable/queued device values after the user chooses
    /// "Device -> TotalControl".  Unreadable matrix, master, loopback and mute
    /// values stay at the TotalControl scene.
    pub fn restore_readable(&mut self, state: &ReadableState) -> Result<(), Error> {
        if !self.is_uc() {
            // UCX: input gain is not reported at connect, so it stays with
            // the scene.  Only the output-volume map is restored.
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

    /// Device peak words, in the same channel order as the power meters.
    /// A mixer meter is a peak meter: the power word is mean square (RMS),
    /// which reads several dB lower than the peak for music.
    pub fn input_peaks(&self) -> Option<Vec<f32>> {
        match self.model {
            Model::Ucx => Some(
                (0..self.inputs.len())
                    .map(|i| self.meters.input_peak(i))
                    .collect(),
            ),
            Model::Uc => None,
        }
    }

    pub fn playback_peaks(&self) -> Option<Vec<f32>> {
        match self.model {
            Model::Ucx => Some(
                (0..self.playbacks.len())
                    .map(|i| self.meters.playback_peak(i))
                    .collect(),
            ),
            Model::Uc => None,
        }
    }

    pub fn output_peaks(&self, post: bool) -> Option<Vec<f32>> {
        match self.model {
            Model::Ucx => Some(
                (0..self.outputs.len())
                    .map(|i| self.meters.output_peak(i, post))
                    .collect(),
            ),
            Model::Uc => None,
        }
    }

    pub fn output_peaks_internal(&self, post: bool) -> Option<Vec<f32>> {
        let raw = self.output_peaks(post)?;
        Some(reorder_output_meters(
            &raw,
            self.spec.profile.output_order,
        ))
    }

    pub fn fx_peaks(&self) -> Option<[f32; 2]> {
        match self.model {
            Model::Ucx => Some([
                self.meters.fx_peak_level(false),
                self.meters.fx_peak_level(true),
            ]),
            Model::Uc => None,
        }
    }

    /// FX bus meters: `[send, return]`.
    pub fn fx_meters(&self) -> Option<[f32; 2]> {
        match self.model {
            Model::Ucx => Some([self.meters.fx(false), self.meters.fx(true)]),
            Model::Uc => None,
        }
    }

    /// FX bus overload flags: `[send, return]`.
    pub fn fx_overload(&self) -> Option<[bool; 2]> {
        match self.model {
            Model::Ucx => Some([
                self.meters.fx_peak_in >= 1 << 27,
                self.meters.fx_peak_out >= 1 << 27,
            ]),
            Model::Uc => None,
        }
    }

    /// Output meters in TotalControl's internal (Phones-first) channel order.
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
        self.state_words.insert(addr, value);
        if self.startup_dump_pending {
            // Pre-connect snapshot only.  Do not let late dump words
            // overwrite the scene loaded into the model.  Speaker B is a
            // runtime monitor state that survives a scene load, so its
            // state must be taken from the unit even while the rest of the
            // dump is only recorded.
            if addr == 0x3d00 {
                self.settings.speaker_b = value != 0;
            }
            self.record_ucx_readable_word(addr, value);
            if addr == 0x3d46 && self.state_words.len() > 1500 {
                self.finish_startup_dump();
            }
            return;
        }
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
            // The odd channel is the right side of the pair.  Keep the
            // left channel's `gain_right` alias in step so the GUI can show
            // both gains of a linked pair.
            if idx % 2 == 1 {
                if let Some(left) = self.inputs.get_mut(idx - 1) {
                    left.gain_right = Some(gain);
                }
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
        if addr as u8 == 1 {
            if let Some(idx) = ucx::output_index_from_state(addr & 0xff00) {
                let balance = (value as i16).clamp(-100, 100) as i8;
                if let Some(output) = self.outputs.get_mut(idx) {
                    output.balance = balance;
                }
                if self.output_linked(idx / 2) {
                    let left = idx & !1;
                    for i in [left, left + 1] {
                        if let Some(output) = self.outputs.get_mut(i) {
                            output.balance = balance;
                        }
                    }
                }
                return;
            }
        }
        match addr {
            0x3d00 => self.settings.speaker_b = value != 0,
            ucx::global::DIM => self.settings.dim = value != 0,
            ucx::global::MONITOR_FLAGS => {
                self.settings.mono = value & 0x02 != 0;
            }
            _ => {}
        }
    }

    fn drain_events(&mut self) {
        // The queue can hold events accumulated while TotalControl was closed.
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

    /// The level a channel's send should have on the device after mute and
    /// the current solo set.  The model volume is never changed by solo.
    fn input_send_effective(&self, idx: usize, output: usize) -> f32 {
        let channel = ChannelId::Input(idx);
        if self.muted.contains(&channel)
            || (!self.soloed.is_empty() && !self.soloed.contains(&channel))
        {
            0.0
        } else {
            self.inputs
                .get(idx)
                .and_then(|c| c.volumes.get(output))
                .copied()
                .unwrap_or(0.0)
        }
    }

    fn playback_send_effective(&self, idx: usize, output: usize) -> f32 {
        let channel = ChannelId::Playback(idx);
        if self.muted.contains(&channel)
            || (!self.soloed.is_empty() && !self.soloed.contains(&channel))
        {
            0.0
        } else {
            self.playbacks
                .get(idx)
                .and_then(|c| c.volumes.get(output))
                .copied()
                .unwrap_or(0.0)
        }
    }

    /// Write one input send without changing the fader model.  The UC/UCX
    /// register writers read the channel volumes, so the two sides of a
    /// stereo input are staged together and restored afterwards.
    fn write_input_send(&mut self, idx: usize, output: usize) -> Result<(), Error> {
        if idx >= self.inputs.len() || output >= PAIRS {
            return Err(Error::InvalidChannel(format!("input {idx}/{output}")));
        }
        let (base, other) = if idx < 4 {
            (idx, idx)
        } else {
            let base = idx & !1;
            (base, base ^ 1)
        };
        if other >= self.inputs.len() {
            return Err(Error::InvalidChannel(format!("input {idx}/{output}")));
        }
        let saved_base = self.inputs[base].volumes[output];
        let saved_other = self.inputs[other].volumes[output];
        let eff_base = self.input_send_effective(base, output);
        let eff_other = if other == base {
            eff_base
        } else {
            self.input_send_effective(other, output)
        };
        self.inputs[base].volumes[output] = eff_base;
        self.inputs[other].volumes[output] = eff_other;
        let level = self.input_send_effective(idx, output);
        let result = if self.is_uc() {
            self.uc_write_input(idx, output, level)
        } else {
            self.ucx_write_input(idx, output, level)
        };
        self.inputs[base].volumes[output] = saved_base;
        self.inputs[other].volumes[output] = saved_other;
        result
    }

    /// Write one playback send without changing the fader model.  Pair
    /// writers (width/MS processing) read both channel volumes, so both are
    /// staged and restored.
    fn write_playback_send(&mut self, idx: usize, output: usize) -> Result<(), Error> {
        if idx >= self.playbacks.len() || output >= PAIRS {
            return Err(Error::InvalidChannel(format!("playback {idx}/{output}")));
        }
        let base = idx & !1;
        let other = base ^ 1;
        if other >= self.playbacks.len() {
            return Err(Error::InvalidChannel(format!("playback {idx}/{output}")));
        }
        let saved_base = self.playbacks[base].volumes[output];
        let saved_other = self.playbacks[other].volumes[output];
        let eff_base = self.playback_send_effective(base, output);
        let eff_other = self.playback_send_effective(other, output);
        self.playbacks[base].volumes[output] = eff_base;
        self.playbacks[other].volumes[output] = eff_other;
        let level = self.playback_send_effective(idx, output);
        let result = if self.is_uc() {
            self.uc_write_playback(idx, output, level)
        } else {
            self.ucx_write_playback(idx, output, level)
        };
        self.playbacks[base].volumes[output] = saved_base;
        self.playbacks[other].volumes[output] = saved_other;
        result
    }

    fn write_input_channel(&mut self, idx: usize) -> Result<(), Error> {
        for output in 0..PAIRS {
            self.write_input_send(idx, output)?;
        }
        Ok(())
    }

    fn write_playback_channel(&mut self, idx: usize) -> Result<(), Error> {
        for output in 0..PAIRS {
            self.write_playback_send(idx, output)?;
        }
        Ok(())
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
        self.write_input_send(idx, output)
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
        self.write_playback_send(idx, output)
    }

    /// When Speaker B is linked to Main, mirror a Main Out level change onto
    /// the corresponding Speaker B channel.
    fn mirror_speaker_b_from_main(&mut self, idx: usize, volume: f32) -> Result<(), Error> {
        if !self.speaker_b_linked || self.settings.main_out != Some(idx / 2) {
            return Ok(());
        }
        let Some(b) = self.settings.main_b_out else {
            return Ok(());
        };
        if b == idx / 2 {
            return Ok(());
        }
        let b_idx = b * 2 + idx % 2;
        if b_idx >= self.outputs.len() {
            return Ok(());
        }
        if self.outputs[b_idx].mute {
            self.outputs[b_idx].volume = volume;
            self.output_mute_saved.insert(b_idx, volume);
            return Ok(());
        }
        if self.is_uc() {
            self.uc_write_output_volume(b_idx, volume)?;
            self.uc_write_output_master(b_idx, volume)?;
        } else {
            self.ucx_write_output_volume(b_idx, volume)?;
        }
        self.outputs[b_idx].volume = volume;
        Ok(())
    }

    /// Write the preamp state that the UCX keeps in the unit but does not
    /// report at connect.  Without this the GUI can show a gain or 48 V
    /// state that the hardware does not have.
    ///
    /// Input gain is normally learned from the `0x8d` stream, but the unit
    /// only emits that word when the encoder moves; at startup the saved
    /// scene is the only value available.  The same applies to 48 V, Inst
    /// and Level, which have no state word at all.
    fn apply_ucx_preamp_state(&mut self) -> Result<(), Error> {
        let inputs = self.inputs.len().min(4);
        for idx in 0..inputs {
            if let Some(gain) = self.inputs[idx].gain {
                self.set_input_gain(idx, gain)?;
            }
            if let Some(gain_right) = self.inputs[idx].gain_right {
                // The right side stays independent even while the pair is
                // linked; `/2/gain` only moves the left side.
                self.set_input_gain_right(idx, gain_right)?;
            }
        }
        for idx in 0..inputs.min(2) {
            let phantom = self.inputs[idx].phantom;
            self.set_input_phantom(idx, phantom)?;
        }
        for idx in 2..inputs {
            let input = self.inputs[idx].clone();
            if input.has_instrument {
                self.set_input_instrument(idx, input.instrument)?;
            }
            if input.has_pad {
                self.set_input_pad(idx, input.pad)?;
            }
            if let Some(level) = input.level {
                self.set_input_level(idx, level)?;
            }
        }
        Ok(())
    }

    /// Write the UCX device group that has no host readback: the line and
    /// phones reference levels and the options word (clock source, optical
    /// format, SPDIF professional, word clock and EQ+D).
    fn apply_ucx_device_settings(&mut self) -> Result<(), Error> {
        self.set_line_out_ref(self.settings.line_out_ref)?;
        self.set_phones_ref(self.settings.phones_ref)?;
        self.set_line_in_ref_58(self.settings.line_in_ref_58)?;
        self.ucx_write_options()
    }

    /// Current monitor flag word (`0x3f9e`): bit 0 Talkback, bit 1 Mono,
    /// bit 5 External Input.
    fn monitor_flags(&self) -> u16 {
        (self.talkback_active as u16)
            | ((self.settings.mono as u16) << 1)
            | ((self.settings.external_input as u16) << 5)
    }

    /// Write the model's normal sends back into one output pair.
    fn restore_pair_from_model(&mut self, pair: usize) -> Result<(), Error> {
        for idx in 0..self.inputs.len() {
            self.write_input_send(idx, pair)?;
        }
        for idx in 0..self.playbacks.len() {
            self.write_playback_send(idx, pair)?;
        }
        Ok(())
    }

    fn release_external_input(&mut self) -> Result<(), Error> {
        let Some((pairs, fx)) = self.external_input_saved.take() else {
            self.settings.external_input = false;
            return Ok(());
        };
        for pair in pairs {
            self.restore_pair_from_model(pair)?;
        }
        for (idx, db) in fx {
            self.ucx_write_fx_return(idx, db)?;
        }
        self.settings.external_input = false;
        self.write_word(0x3f9e, self.monitor_flags())?;
        Ok(())
    }

    /// Rewrite the cue target so it contains only the active PFL inputs at
    /// unity; all other sources are silent.
    fn rewrite_pfl_target(&mut self) -> Result<(), Error> {
        let Some((target, _)) = self.pfl_saved else {
            return Ok(());
        };
        let dsp = ucx::output_pair_dsp(target)
            .ok_or_else(|| Error::InvalidChannel(format!("cue target {target}")))?;
        let sources = self.inputs.len() + self.playbacks.len();
        let mut writes = Vec::new();
        for source in 0..sources {
            let Some(channel) = Self::matrix_source_channel(source) else {
                continue;
            };
            let source = match channel {
                ChannelId::Input(i) => Source::Input(i),
                ChannelId::Playback(p) => Source::Playback(p),
                ChannelId::Output(_) => continue,
            };
            for block in [dsp, dsp + 1] {
                writes.push((ucx::matrix_reg(block as u8, source), 0));
            }
        }
        for channel in &self.pfl_active {
            let ChannelId::Input(idx) = *channel else {
                continue;
            };
            if idx < 4 {
                for block in [dsp, dsp + 1] {
                    writes.push((
                        ucx::matrix_reg(block as u8, Source::Input(idx)),
                        ucx::matrix_gain(1.0),
                    ));
                }
            } else {
                let base = idx & !1;
                if base + 1 < self.inputs.len() {
                    writes.push((
                        ucx::matrix_reg(dsp as u8, Source::Input(base)),
                        ucx::matrix_gain(1.0),
                    ));
                    writes.push((
                        ucx::matrix_reg((dsp + 1) as u8, Source::Input(base + 1)),
                        ucx::matrix_gain(1.0),
                    ));
                }
            }
        }
        self.write_words(&writes)
    }

    fn release_pfl(&mut self) -> Result<(), Error> {
        let Some((target, fx)) = self.pfl_saved.take() else {
            self.pfl_active.clear();
            return Ok(());
        };
        self.restore_pair_from_model(target)?;
        for (idx, db) in fx {
            self.ucx_write_fx_return(idx, db)?;
        }
        for channel in std::mem::take(&mut self.pfl_active) {
            if let ChannelId::Input(idx) = channel {
                if let Some(input) = self.inputs.get_mut(idx) {
                    input.solo = false;
                }
            }
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
        let batch = self.begin_write_batch();
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
                self.mirror_speaker_b_from_main(idx, v)?;
            }
            ChannelId::Input(idx) => self.apply_input_volume(idx, output, v)?,
            ChannelId::Playback(idx) => self.apply_playback_volume(idx, output, v)?,
        }
        finish_live_batch(batch, Ok(()))
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
        let batch = self.begin_write_batch();
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
                    let (gl, gr) = output_balance_to_volumes(current, pan);
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
        finish_live_batch(batch, Ok(()))
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
                    Ok(output_balance_from_volumes(
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
                if let Some(eq) = saved.eq {
                    self.apply_input_eq(right, &eq)?;
                }
                if let Some(dynamics) = saved.dynamics {
                    self.set_dynamics(ChannelId::Input(right), &dynamics)?;
                }
                let _ = self.set_channel_fx_send(ChannelId::Input(right), saved.fx_send_db);
            }
        }
        // The linked balance is derived from the two channel volumes, as
        // on an output pair.  The mono pans stay as they are when linking;
        // splitting puts them hard left/right.
        if !linked {
            for output in 0..self.output_pair_count() {
                self.set_pan(ChannelId::Input(left), output, -100)?;
                self.set_pan(ChannelId::Input(right), output, 100)?;
            }
        }
        Ok(())
    }

    fn set_output_linked(&mut self, pair: usize, linked: bool) -> Result<(), Error> {
        let was_linked = self.output_linked(pair);
        if was_linked == linked {
            return Ok(());
        }
        let (left, right) = (pair * 2, pair * 2 + 1);
        let left_volume = self.outputs[left].volume;
        let right_volume = self.outputs[right].volume;
        let pair_volume = left_volume.max(right_volume);
        let balance = if was_linked && !self.is_uc() {
            // A linked UCX pair keeps its balance in the register; its two
            // volumes are already flattened to the pair level.
            self.outputs[left].balance
        } else {
            output_balance_from_volumes(left_volume, right_volume)
        };
        if !self.is_uc() {
            self.ucx_write_output_stereo(left, linked)?;
            self.ucx_write_output_stereo(right, linked)?;
        }
        let saved_output = self.outputs.get(left).cloned();
        let saved_mute = saved_output.as_ref().map(|o| o.mute);
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
            if let Some(saved) = saved_output {
                if let Some(eq) = saved.eq {
                    self.set_output_eq(right, &eq)?;
                }
                if let Some(dynamics) = saved.dynamics {
                    self.set_dynamics(ChannelId::Output(right), &dynamics)?;
                }
                let _ = self.set_fx_return(right, saved.fx_return_db);
            }
            if self.is_uc() {
                // No balance register: splitting comes back from the two
                // volumes; rewriting the balance here restores L/R.
                self.set_pan(ChannelId::Output(left), 0, balance)?;
            } else {
                // UCX: the linked fader is the pair level and the balance
                // is its own register. Flatten the volumes, then write the
                // balance derived from the split channels.
                self.set_volume(ChannelId::Output(left), 0, pair_volume)?;
                self.set_volume(ChannelId::Output(right), 0, pair_volume)?;
                self.set_pan(ChannelId::Output(left), 0, balance)?;
            }
        } else if !self.is_uc() {
            // UCX split: turn the linked level/balance into the two channel
            // volumes, then clear the now-unused balance register.
            let (l, r) = output_balance_to_volumes(pair_volume, balance);
            self.set_volume(ChannelId::Output(left), 0, l)?;
            self.set_volume(ChannelId::Output(right), 0, r)?;
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
                    // Zero the sends before marking the channel muted.  The
                    // apply helper records a muted channel's zero in
                    // `mute_saved` instead of writing it; inserting `muted`
                    // first therefore saved zeroes and unmuting restored the
                    // fader to zero.
                    for p in 0..PAIRS {
                        let volume = self.inputs[idx].volumes[p];
                        self.apply_input_volume(idx, p, 0.0)?;
                        self.inputs[idx].volumes[p] = volume;
                    }
                    self.muted.insert(channel);
                } else {
                    if let Some((volumes, pans)) = self.mute_saved.remove(&channel) {
                        self.muted.remove(&channel);
                        self.inputs[idx].pans.clone_from(&pans);
                        for (p, volume) in volumes.iter().enumerate().take(PAIRS) {
                            self.apply_input_volume(idx, p, *volume)?;
                        }
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
                    // See the input branch: mark the channel muted only after
                    // the zero writes, or `mute_saved` would hold zeroes and
                    // the second press would drop the fader to zero.
                    for p in 0..PAIRS {
                        let volume = self.playbacks[idx].volumes[p];
                        self.apply_playback_volume(idx, p, 0.0)?;
                        self.playbacks[idx].volumes[p] = volume;
                    }
                    self.muted.insert(channel);
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
        // Solo is a monitoring state, not a fader move.  While any channel
        // is soloed, every other input/playback send is written as zero; the
        // fader model keeps its level and unsolo restores the sends.
        let was_soloed = self.soloed.contains(&channel);
        match channel {
            ChannelId::Output(idx) => {
                if let Some(o) = self.outputs.get_mut(idx) {
                    o.solo = solo;
                }
                return Ok(());
            }
            ChannelId::Input(idx) => {
                if let Some(c) = self.inputs.get_mut(idx) {
                    c.solo = solo;
                } else {
                    return Ok(());
                }
            }
            ChannelId::Playback(idx) => {
                if let Some(c) = self.playbacks.get_mut(idx) {
                    c.solo = solo;
                } else {
                    return Ok(());
                }
            }
        }
        if solo == was_soloed {
            return Ok(());
        }
        let had_solo = !self.soloed.is_empty();
        if solo {
            self.soloed.insert(channel);
        } else {
            self.soloed.remove(&channel);
        }
        // A solo change rewrites many sends.  Coalesce them into the same
        // 63-word bursts as a scene write; one paced write per cell would
        // take seconds on the UCX.
        let _batch = self.begin_write_batch();

        if solo {
            if had_solo {
                // Another solo is already active and its sends are already
                // zeroed: restore only this newly soloed channel.
                match channel {
                    ChannelId::Input(idx) => self.write_input_channel(idx)?,
                    ChannelId::Playback(idx) => self.write_playback_channel(idx)?,
                    ChannelId::Output(_) => {}
                }
            } else {
                // First solo: silence every other channel.
                for idx in 0..self.inputs.len() {
                    if channel != ChannelId::Input(idx) {
                        self.write_input_channel(idx)?;
                    }
                }
                for idx in 0..self.playbacks.len() {
                    if channel != ChannelId::Playback(idx) {
                        self.write_playback_channel(idx)?;
                    }
                }
            }
        } else if self.soloed.is_empty() {
            // Last solo released: restore every send from the model.
            for idx in 0..self.inputs.len() {
                self.write_input_channel(idx)?;
            }
            for idx in 0..self.playbacks.len() {
                self.write_playback_channel(idx)?;
            }
        } else {
            // Other solos remain: this channel goes back to zero.
            match channel {
                ChannelId::Input(idx) => self.write_input_channel(idx)?,
                ChannelId::Playback(idx) => self.write_playback_channel(idx)?,
                ChannelId::Output(_) => {}
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
        self.set_dim_with(on, -20.0)
    }

    fn set_dim_with(&mut self, on: bool, amount_db: f32) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Dim is not decoded on the UC".into(),
            ));
        }
        if on == self.settings.dim {
            if on {
                let amount = amount_db.clamp(-65.0, 0.0);
                if (amount - self.dim_amount_db).abs() > f32::EPSILON {
                    self.dim_amount_db = amount;
                    let saved = self.dim_saved.clone();
                    let dim = move |v: f32| {
                        if v <= 0.0 {
                            0.0
                        } else {
                            db_to_linear(linear_db(v) + amount)
                        }
                    };
                    for (pair, left_level, right_level) in saved {
                        self.set_volume(
                            ChannelId::Output(pair * 2),
                            0,
                            dim(left_level),
                        )?;
                        self.set_volume(
                            ChannelId::Output(pair * 2 + 1),
                            0,
                            dim(right_level),
                        )?;
                    }
                }
            }
            return Ok(());
        }
        if !on {
            let saved = std::mem::take(&mut self.dim_saved);
            for (saved_pair, left_level, right_level) in saved {
                self.set_volume(ChannelId::Output(saved_pair * 2), 0, left_level)?;
                self.set_volume(ChannelId::Output(saved_pair * 2 + 1), 0, right_level)?;
            }
            self.write_word(ucx::global::DIM, 0)?;
            self.settings.dim = false;
            return Ok(());
        }
        let Some(pair) = self.settings.main_out else {
            return Err(Error::InvalidChannel(
                "Dim needs an assigned Main Out".into(),
            ));
        };
        let mut pairs = vec![pair];
        if let Some(b) = self.settings.main_b_out {
            if b != pair {
                pairs.push(b);
            }
        }
        let mut saved = Vec::with_capacity(pairs.len());
        for pair in pairs {
            let left = pair * 2;
            let right = left + 1;
            if right >= self.outputs.len() {
                return Err(Error::InvalidChannel(format!("Speaker pair {pair}")));
            }
            saved.push((
                pair,
                self.outputs[left].volume,
                self.outputs[right].volume,
            ));
        }
        self.dim_saved = saved;
        self.dim_amount_db = amount_db.clamp(-65.0, 0.0);
        let amount = self.dim_amount_db;
        let dim = move |v: f32| {
            if v <= 0.0 {
                0.0
            } else {
                db_to_linear(linear_db(v) + amount)
            }
        };
        let saved = self.dim_saved.clone();
        for (pair, left_level, right_level) in saved {
            self.set_volume(ChannelId::Output(pair * 2), 0, dim(left_level))?;
            self.set_volume(ChannelId::Output(pair * 2 + 1), 0, dim(right_level))?;
        }
        self.write_word(ucx::global::DIM, 1)?;
        self.settings.dim = true;
        Ok(())
    }

    fn set_main_recall(&mut self, db: f32) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Recall is not decoded on the UC".into(),
            ));
        }
        let Some(pair) = self.settings.main_out else {
            return Err(Error::InvalidChannel(
                "Recall needs an assigned Main Out".into(),
            ));
        };
        let left = pair * 2;
        let right = left + 1;
        if right >= self.outputs.len() {
            return Err(Error::InvalidChannel(format!("Main Out pair {pair}")));
        }
        let level = db_to_linear(db.clamp(-65.0, 6.0));
        if self.settings.dim {
            if let Some(entry) = self.dim_saved.iter_mut().find(|(p, _, _)| *p == pair) {
                entry.1 = level;
                entry.2 = level;
                let dimmed = db_to_linear(linear_db(level) + self.dim_amount_db);
                self.set_volume(ChannelId::Output(left), 0, dimmed)?;
                self.set_volume(ChannelId::Output(right), 0, dimmed)?;
                if self.speaker_b_linked {
                    if let Some(b) = self.settings.main_b_out.filter(|b| *b != pair) {
                        if let Some(entry) =
                            self.dim_saved.iter_mut().find(|(p, _, _)| *p == b)
                        {
                            entry.1 = level;
                            entry.2 = level;
                            self.set_volume(ChannelId::Output(b * 2), 0, dimmed)?;
                            self.set_volume(ChannelId::Output(b * 2 + 1), 0, dimmed)?;
                        }
                    }
                }
                return Ok(());
            }
        }
        self.set_volume(ChannelId::Output(left), 0, level)?;
        self.set_volume(ChannelId::Output(right), 0, level)
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
        if let Some(pair) = pair {
            if pair >= BLOCKS / 2 {
                return Err(Error::InvalidChannel(format!("output pair {pair}")));
            }
        }
        let previous = self.settings.main_out;
        if previous == pair {
            return Ok(());
        }
        // Mute FX and External Input saved the old target levels; restore
        // them before the assignment moves.
        if self.settings.mute_fx {
            self.set_mute_fx(false)?;
        }
        if self.settings.external_input {
            self.release_external_input()?;
        }
        // Dim saved the levels of the old Main and Speaker B pairs.
        // Release it before moving or clearing the assignment, so those
        // pairs return to their saved levels.
        if self.settings.speaker_b {
            self.set_speaker_b(false)?;
        }
        if self.settings.dim {
            self.set_dim(false)?;
        }
        self.settings.main_out = pair;
        if let Some(old) = previous {
            if old < BLOCKS / 2 {
                let mute = self.outputs.get(old * 2).map(|o| o.mute).unwrap_or(false);
                self.ucx_write_output_mode(old * 2, mute)?;
                self.ucx_write_output_mode(old * 2 + 1, mute)?;
            }
        }
        if let Some(pair) = pair {
            let mute = self.outputs.get(pair * 2).map(|o| o.mute).unwrap_or(false);
            self.ucx_write_output_mode(pair * 2, mute)?;
            self.ucx_write_output_mode(pair * 2 + 1, mute)?;
        }
        Ok(())
    }

    /// Assign Main Out B: bit 1 on the chosen pair, plus the control-room
    /// state word.  The captured state word is `0` while Speaker B is off
    /// and `6` while it is active.
    fn set_main_out_b(&mut self, pair: Option<usize>) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Assign is not decoded on the UC".into(),
            ));
        }
        if let Some(pair) = pair {
            if pair >= BLOCKS / 2 {
                return Err(Error::InvalidChannel(format!("output pair {pair}")));
            }
        }
        let previous = self.settings.main_b_out;
        if previous == pair {
            return Ok(());
        }
        if self.settings.mute_fx {
            self.set_mute_fx(false)?;
        }
        if self.settings.external_input {
            self.release_external_input()?;
        }
        if self.settings.speaker_b {
            self.set_speaker_b(false)?;
        }
        if self.settings.dim {
            self.set_dim(false)?;
        }
        self.settings.main_b_out = pair;
        if let Some(old) = previous {
            if old < BLOCKS / 2 {
                let mute = self.outputs.get(old * 2).map(|o| o.mute).unwrap_or(false);
                self.ucx_write_output_mode(old * 2, mute)?;
                self.ucx_write_output_mode(old * 2 + 1, mute)?;
            }
        }
        match pair {
            Some(pair) => {
                let mute = self.outputs.get(pair * 2).map(|o| o.mute).unwrap_or(false);
                self.ucx_write_output_mode(pair * 2, mute)?;
                self.ucx_write_output_mode(pair * 2 + 1, mute)?;
                self.write_word(0x3d00, if self.settings.speaker_b { 1 } else { 0 })?;
            }
            None => {
                // Clearing the assignment also leaves Speaker B.  The
                // captured clear writes the old pair's mode words to zero
                // and 0x3d00 to zero.
                self.settings.speaker_b = false;
                self.write_word(0x3d00, 0)?;
            }
        }
        Ok(())
    }

    /// Control Room Mute FX: the assigned Main Out and Speaker B FX returns
    /// drop to `-65 dB` while it is engaged, and the exact previous levels
    /// are restored afterwards.  Phones is not part of the monitor-speaker
    /// pair.
    fn set_mute_fx(&mut self, on: bool) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Mute FX is not decoded on the UC".into(),
            ));
        }
        let Some(main) = self.settings.main_out else {
            self.settings.mute_fx = false;
            self.mute_fx_saved = None;
            return Ok(());
        };
        let mut pairs = vec![main];
        if let Some(b) = self.settings.main_b_out {
            if b != main {
                pairs.push(b);
            }
        }
        if on == self.settings.mute_fx {
            return Ok(());
        }
        if on {
            let mut saved = Vec::with_capacity(pairs.len() * 2);
            for pair in pairs {
                for side in 0..2 {
                    let idx = pair * 2 + side;
                    let Some(output) = self.outputs.get(idx) else {
                        continue;
                    };
                    saved.push((idx, output.fx_return_db));
                }
            }
            self.mute_fx_saved = Some(saved);
            let saved = self.mute_fx_saved.clone().unwrap_or_default();
            for (idx, _) in saved {
                self.ucx_write_fx_return(idx, -65.0)?;
            }
            self.settings.mute_fx = true;
            return Ok(());
        }

        let saved = self.mute_fx_saved.take().unwrap_or_else(|| {
            pairs
                .iter()
                .flat_map(|pair| [pair * 2, pair * 2 + 1])
                .filter_map(|idx| self.outputs.get(idx).map(|out| (idx, out.fx_return_db)))
                .collect()
        });
        for (idx, db) in saved {
            self.ucx_write_fx_return(idx, db)?;
        }
        self.settings.mute_fx = false;
        Ok(())
    }

    /// Control Room External Input: route the selected hardware input pair
    /// into the Main Out and Speaker B submixes at the configured gain,
    /// mute those pairs' FX returns, and set monitor flag bit 5.
    fn set_external_input(
        &mut self,
        on: bool,
        source: Option<usize>,
        gain_db: f32,
    ) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "External Input is not decoded on the UC".into(),
            ));
        }
        let _batch = self.begin_write_batch();
        if self.settings.external_input {
            self.release_external_input()?;
        }
        if !on {
            return Ok(());
        }
        let Some(main) = self.settings.main_out else {
            return Err(Error::InvalidChannel(
                "External Input needs an assigned Main Out".into(),
            ));
        };
        let source = source
            .ok_or_else(|| Error::InvalidChannel("External Input needs a source pair".into()))?;
        let left = source * 2;
        let right = left + 1;
        if right >= self.inputs.len() {
            return Err(Error::InvalidChannel(format!(
                "External Input source pair {source}"
            )));
        }
        if self.settings.mute_fx {
            self.set_mute_fx(false)?;
        }
        let mut pairs = vec![main];
        if let Some(b) = self.settings.main_b_out {
            if b != main {
                pairs.push(b);
            }
        }
        let gain = 10f32.powf(gain_db.clamp(-65.0, 0.0) / 20.0).clamp(0.0, 1.0);
        let mut writes = Vec::new();
        let mut fx_saved = Vec::new();
        for pair in &pairs {
            let dsp = ucx::output_pair_dsp(*pair)
                .ok_or_else(|| Error::InvalidChannel(format!("output pair {pair}")))?;
            let sources = self.inputs.len() + self.playbacks.len();
            for source in 0..sources {
                let Some(channel) = Self::matrix_source_channel(source) else {
                    continue;
                };
                let source = match channel {
                    ChannelId::Input(i) => Source::Input(i),
                    ChannelId::Playback(p) => Source::Playback(p),
                    ChannelId::Output(_) => continue,
                };
                for block in [dsp, dsp + 1] {
                    writes.push((ucx::matrix_reg(block as u8, source), 0));
                }
            }
            writes.push((
                ucx::matrix_reg(dsp as u8, Source::Input(left)),
                ucx::matrix_gain(gain as f64),
            ));
            writes.push((
                ucx::matrix_reg((dsp + 1) as u8, Source::Input(right)),
                ucx::matrix_gain(gain as f64),
            ));
            for side in 0..2 {
                let idx = pair * 2 + side;
                if let Some(out) = self.outputs.get(idx) {
                    fx_saved.push((idx, out.fx_return_db));
                }
            }
        }
        for (idx, _) in &fx_saved {
            let dsp = ucx::output_pair_dsp(idx / 2)
                .ok_or_else(|| Error::InvalidChannel(format!("output {idx}")))?;
            writes.push((
                ucx::channel_reg(ucx::output_channel(dsp + idx % 2), ucx::offset::FX_RETURN),
                0xfd76,
            ));
        }
        self.settings.external_input = true;
        writes.push((0x3f9e, self.monitor_flags()));
        self.write_words(&writes)?;
        self.external_input_saved = Some((pairs, fx_saved));
        Ok(())
    }

    fn set_cue_target(&mut self, pair: Option<usize>) -> Result<(), Error> {
        if let Some(pair) = pair {
            if pair >= BLOCKS / 2 {
                return Err(Error::InvalidChannel(format!("cue target {pair}")));
            }
        }
        if self.pfl_saved.is_some() {
            self.release_pfl()?;
        }
        self.pfl_target = pair;
        Ok(())
    }

    fn set_pfl_mode(&mut self, pfl: bool, exclusive: bool) -> Result<(), Error> {
        self.pfl_mode = pfl;
        self.pfl_exclusive = exclusive;
        if !pfl && self.pfl_saved.is_some() {
            self.release_pfl()?;
        }
        Ok(())
    }

    fn set_pfl(&mut self, channel: ChannelId, on: bool) -> Result<(), Error> {
        if !self.pfl_mode {
            return self.set_solo(channel, on);
        }
        let ChannelId::Input(idx) = channel else {
            return Ok(());
        };
        let _batch = self.begin_write_batch();
        if idx >= self.inputs.len() {
            return Err(Error::InvalidChannel(format!("input {idx}")));
        }
        if on {
            if self.pfl_exclusive
                && !self
                    .pfl_active
                    .iter()
                    .any(|active| matches!(active, ChannelId::Input(i) if i / 2 == idx / 2))
            {
                for active in std::mem::take(&mut self.pfl_active) {
                    if let ChannelId::Input(i) = active {
                        if let Some(input) = self.inputs.get_mut(i) {
                            input.solo = false;
                        }
                    }
                }
            }
            if let Some(input) = self.inputs.get_mut(idx) {
                input.solo = true;
            }
            self.pfl_active.insert(channel);
            if self.pfl_saved.is_none() {
                let target = self
                    .pfl_target
                    .ok_or_else(|| Error::InvalidChannel("PFL needs a Cue/PFL target".into()))?;
                let mut fx_saved = Vec::new();
                for side in 0..2 {
                    let out = target * 2 + side;
                    if let Some(output) = self.outputs.get(out) {
                        fx_saved.push((out, output.fx_return_db));
                    }
                }
                for (out, _) in &fx_saved {
                    self.ucx_write_fx_return(*out, -65.0)?;
                }
                self.pfl_saved = Some((target, fx_saved));
            }
            self.rewrite_pfl_target()?;
        } else {
            self.pfl_active.remove(&channel);
            if let Some(input) = self.inputs.get_mut(idx) {
                input.solo = false;
            }
            if self.pfl_active.is_empty() {
                self.release_pfl()?;
            } else {
                self.rewrite_pfl_target()?;
            }
        }
        Ok(())
    }

    fn set_speaker_b(&mut self, on: bool) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Speaker B does not exist on the UC".into(),
            ));
        }
        let Some(main) = self.settings.main_out else {
            return Err(Error::InvalidChannel(
                "Speaker B needs an assigned Main Out".into(),
            ));
        };
        let Some(b) = self.settings.main_b_out else {
            return Err(Error::InvalidChannel(
                "Speaker B needs an assigned output".into(),
            ));
        };
        // The mode word carries mute in bit 0 and the monitor routing in
        // bit 1.  While Speaker B is active the Main pair carries bit 1 and
        // the B pair does not; while it is inactive the reverse holds.
        for idx in [main * 2, main * 2 + 1] {
            let mute = self.outputs.get(idx).is_some_and(|o| o.mute);
            self.ucx_write_output_mode_value(
                idx,
                mute as u16 | if on { 2 } else { 0 },
            )?;
        }
        for idx in [b * 2, b * 2 + 1] {
            let mute = self.outputs.get(idx).is_some_and(|o| o.mute);
            self.ucx_write_output_mode_value(
                idx,
                mute as u16 | if on { 0 } else { 2 },
            )?;
        }
        self.write_word(0x3d00, if on { 1 } else { 0 })?;
        self.settings.speaker_b = on;
        Ok(())
    }

    fn set_mono(&mut self, on: bool) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Mono does not exist on the UC".into(),
            ));
        }
        // Monitor flags: bit 0 Talkback, bit 1 Mono Main Out.  The mask
        // word is 0x83ff while Mono is active and 0xffff when it is idle.
        let flags = self.monitor_flags() & !2 | ((on as u16) << 1);
        self.write_words(&[
            (0x3f9e, flags),
            (0x3f98, if on { 0x83ff } else { 0xffff }),
        ])?;
        self.settings.mono = on;
        Ok(())
    }

    fn set_speaker_b_linked(&mut self, on: bool) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Speaker B does not exist on the UC".into(),
            ));
        }
        self.speaker_b_linked = on;
        if on {
            if let (Some(main), Some(b)) = (self.settings.main_out, self.settings.main_b_out) {
                if main != b {
                    for side in 0..2 {
                        let idx = main * 2 + side;
                        if idx < self.outputs.len() {
                            let volume = self.outputs[idx].volume;
                            self.set_volume(ChannelId::Output(b * 2 + side), 0, volume)?;
                        }
                    }
                    // If the link is engaged while Dim is active, the B
                    // pair adopts the Main pair's saved pre-Dim levels as
                    // well as its current dimmed levels.
                    if self.settings.dim {
                        if let Some((_, left, right)) =
                            self.dim_saved.iter().find(|(pair, _, _)| *pair == main).copied()
                        {
                            match self.dim_saved.iter_mut().find(|(pair, _, _)| *pair == b) {
                                Some(entry) => {
                                    entry.1 = left;
                                    entry.2 = right;
                                }
                                None => self.dim_saved.push((b, left, right)),
                            }
                        }
                    }
                }
            }
        }
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

    fn set_input_phase_right(&mut self, idx: usize, invert: bool) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Right input phase is not mapped on the UC".into(),
            ));
        }
        if idx >= self.inputs.len() {
            return Err(Error::InvalidChannel(format!("input {idx}")));
        }
        self.write_word(
            ucx::channel_reg(ucx::input_channel(idx), ucx::offset::INPUT_PHASE) + 0x100,
            invert as u16,
        )?;
        self.inputs[idx].phase_r = invert;
        Ok(())
    }

    fn set_input_gain_right(&mut self, idx: usize, gain: u32) -> Result<(), Error> {
        if self.is_uc() {
            return Err(Error::InvalidChannel(
                "Right input gain is not mapped on the UC".into(),
            ));
        }
        if idx >= 4 {
            return Err(Error::InvalidChannel(format!("input {idx} has no preamp")));
        }
        let max = if idx < 2 { 65 } else { 24 };
        let gain = gain.min(max);
        let tenths = if idx < 2 { gain * 10 } else { gain * 5 };
        self.ucx_write_input_gain_right(idx, tenths)?;
        if let Some(input) = self.inputs.get_mut(idx) {
            input.gain_right = Some(gain);
        }
        // The right-side register is the odd channel's gain.  Keep the
        // channel model in step so a linked-pair display can read either
        // side.
        if idx % 2 == 0 {
            if let Some(right) = self.inputs.get_mut(idx + 1) {
                right.gain = Some(gain);
            }
        }
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
        let batch = self.begin_write_batch();
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
        finish_live_batch(batch, Ok(()))
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
        // The FX return fader tops out at 0 dB on the unit.  Positive
        // values are ignored by the device, so keep the model and the
        // write inside the readable range.
        let db = db.clamp(-65.0, 0.0);
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
        let supported = match band {
            0 => matches!(
                band_type,
                EqBandType::Off | EqBandType::Bell | EqBandType::LowShelf
            ),
            1 => matches!(band_type, EqBandType::Off | EqBandType::Bell),
            2 => matches!(
                band_type,
                EqBandType::Off | EqBandType::Bell | EqBandType::HighShelf | EqBandType::LowPass
            ),
            _ => false,
        };
        if !supported {
            return Err(Error::InvalidChannel(format!(
                "EQ input {idx} band {} has no {band_type} type",
                band + 1
            )));
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
        // Dim and Speaker B are runtime monitor states.  The fader values
        // in the scene already include Dim's attenuation, so carrying the
        // flag would make the next scene write dim the same pair again.
        let mut settings = self.settings.clone();
        settings.dim = false;
        settings.speaker_b = false;
        settings.external_input = false;
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
            settings,
        }
    }

    fn apply_scene(&mut self, scene: &Scene) -> Result<(), Error> {
        // The UCX scene is thousands of register words.  Queue them and
        // send 63-word bursts with the device's drain gap; sending each
        // word separately made a connect take several seconds.
        let started = Instant::now();
        let batch = self.begin_write_batch();
        let normalized = if self.model == Model::Ucx {
            let mut scene = scene.clone();
            scene.normalize_ucx_pairs();
            Some(scene)
        } else {
            None
        };
        let scene = normalized.as_ref().unwrap_or(scene);
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
        // The Control Room assignments are application configuration, not
        // mixer scene values.  Keep the live assignment when a scene is
        // recalled or reapplied so Dim and Recall keep addressing the pair
        // the user assigned.
        let live_main_out = self.settings.main_out;
        let live_main_b_out = self.settings.main_b_out;
        let live_speaker_b = self.settings.speaker_b;
        self.settings = scene.settings.clone();
        self.settings.main_out = live_main_out;
        self.settings.main_b_out = live_main_b_out;
        self.settings.speaker_b = live_speaker_b;
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
            // The UCX does not report input gain at connect, so the scene is
            // authoritative for the preamp group.  Live encoder moves still
            // update the model through the 0x8d stream.  Scenes written
            // before the half-dB model stored inputs 3/4 as whole dB with
            // `gain_max = 12`; convert those to raw half-dB units (0..24).
            for (idx, input) in self.inputs.iter_mut().enumerate() {
                if scene
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
                let mute = self.outputs[idx].mute;
                self.set_volume(ChannelId::Output(idx), 0, volume)?;
                // Scene recall must also clear a mute left on the unit.
                self.set_mute(ChannelId::Output(idx), mute)?;
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
                    // `channel_settings` already includes the dynamics and
                    // Auto Level block; fold them in instead of writing
                    // those 12 registers a second time.
                    if let Some(dynamics) = self.outputs[idx].dynamics {
                        self.ucx_update_dynamics_settings(
                            ChannelId::Output(idx),
                            &dynamics,
                        )?;
                    }
                    let s = self.output_settings[idx];
                    self.ucx_write_channel_settings(idx, true, &s)?;
                } else if let Some(dynamics) = self.outputs[idx].dynamics {
                    self.ucx_write_dynamics(ChannelId::Output(idx), &dynamics)?;
                }
                let fx = self.outputs[idx].fx_return_db;
                self.set_fx_return(idx, fx)?;
            }
        }
        for idx in 0..self.inputs.len() {
            if !self.is_uc() {
                // Right-side phase and preamp gain are separate registers
                // from the left side; a split pair carries both.
                if self.inputs[idx].phase_r {
                    self.set_input_phase_right(idx, true)?;
                }
                if idx < 4 {
                    if let Some(gain) = self.inputs[idx].gain_right {
                        self.set_input_gain_right(idx, gain)?;
                    }
                }
                // `ucx_write_input` rewrites both sides of a stereo pair
                // from either channel, so the odd channel would repeat the
                // same 9-pair row.  Its scene values are already in the
                // model; only the first side needs the device write.
                if idx < 4 || idx % 2 == 0 {
                    for p in 0..PAIRS {
                        let volume = self.inputs[idx].volumes[p];
                        self.apply_input_volume(idx, p, volume)?;
                    }
                }
                let linked = self.input_pair_linked(idx / 2);
                self.ucx_write_input_stereo(idx, linked)?;
                self.write_word(
                    ucx::channel_reg(ucx::input_channel(idx), ucx::offset::MS_PROC),
                    self.inputs[idx].ms_proc as u16,
                )?;
                if idx < 2 {
                    self.ucx_write_autoset(idx, self.inputs[idx].autoset)?;
                }
            }
            self.set_mute(ChannelId::Input(idx), self.inputs[idx].mute)?;
            if !self.is_uc() {
                if self.inputs[idx].eq.is_some() {
                    self.sync_input_eq(idx);
                    if let Some(dynamics) = self.inputs[idx].dynamics {
                        self.ucx_update_dynamics_settings(
                            ChannelId::Input(idx),
                            &dynamics,
                        )?;
                    }
                    let s = self.input_settings[idx];
                    self.ucx_write_channel_settings(idx, false, &s)?;
                } else if let Some(dynamics) = self.inputs[idx].dynamics {
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
            self.set_mute(ChannelId::Playback(idx), self.playbacks[idx].mute)?;
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
            // Speaker B is a runtime monitor state and is kept across a
            // scene load.  Dim is also runtime, but its attenuation is
            // already in the output fader values; applying the flag again
            // would dim the Main Out a second time.
            self.settings.dim = false;
            self.dim_saved.clear();
            self.settings.external_input = false;
            self.external_input_saved = None;
            for channel in std::mem::take(&mut self.pfl_active) {
                if let ChannelId::Input(idx) = channel {
                    if let Some(input) = self.inputs.get_mut(idx) {
                        input.solo = false;
                    }
                }
            }
            self.pfl_saved = None;
            self.write_word(ucx::global::DIM, 0)?;
            self.set_mono(self.settings.mono)?;
            // The scene has just written its FX-return levels.  Force a
            // fresh Mute FX engagement if the scene requests it.
            let mute_fx = self.settings.mute_fx;
            self.settings.mute_fx = false;
            self.mute_fx_saved = None;
            self.set_mute_fx(mute_fx)?;
        }
        if !self.is_uc() {
            self.apply_ucx_preamp_state()?;
            self.apply_ucx_device_settings()?;
        }
        let result = match batch {
            Some(batch) => batch.finish_wait(),
            None => Ok(()),
        };
        if result.is_ok() {
            if self.dry_run {
                log::info!(
                    "loaded the {} mixer scene model in {:?}",
                    self.model.name(),
                    started.elapsed()
                );
            } else {
                log::info!(
                    "applied the {} mixer scene in {:?}",
                    self.model.name(),
                    started.elapsed()
                );
            }
        }
        result
    }

    fn poll_events(&mut self) -> Result<(), Error> {
        if self.startup_dump_pending
            && self.startup_dump_started.elapsed() > self.startup_dump_timeout
        {
            log::warn!("UCX startup state dump timed out");
            self.finish_startup_dump();
        }
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

    /// All IF1 words written so far, flattened across batch boundaries.
    fn written_words(writes: &Arc<Mutex<Vec<Write>>>) -> Vec<(u16, u16)> {
        writes
            .lock()
            .unwrap()
            .iter()
            .filter_map(|w| match w {
                Write::Words(v) => Some(v.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn open(model: Model) -> (Fireface, Arc<Mutex<Vec<Write>>>) {
        let (t, writes) = MockTransport::new(model);
        let info = t.info.clone();
        (Fireface::from_transport(Box::new(t), info).unwrap(), writes)
    }

    fn open_async(model: Model) -> (Fireface, Arc<Mutex<Vec<Write>>>) {
        let (t, writes) = MockTransport::new(model);
        let info = t.info.clone();
        (
            Fireface::from_transport_with_writer(Box::new(t), info, true).unwrap(),
            writes,
        )
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
    fn ucx_meter_peaks_are_full_scale_amplitude() {
        let mut bank = MeterBank::default();
        let mut p = MeterPacket {
            kind: MeterKind::Playback,
            power: [0; 20],
            peak: [0; 18],
            fx_power: [0; 2],
            fx_peak: [0; 2],
        };
        // Peak full scale is 2^27; -20 dBFS is one tenth of it.
        p.peak[0] = (1 << 27) / 10;
        let a = (1u64 << 23) / 10;
        p.power[0] = 512 * a * a / 2;
        bank.apply(&p);
        assert!((bank.playback_peak(0) - 0.1).abs() < 0.002);
        // The power word stays the mean-square reading, below the peak.
        assert!(bank.playback(0) < bank.playback_peak(0));
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
        // Measured with the front encoder on L.5/L.6 (the AN5/6 pair).
        assert_eq!(ucx::output_index_from_state(0x1600), Some(6));
        assert_eq!(ucx::output_index_from_state(0x1700), Some(7));
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
    fn ucx_output_balance_state_word_updates_the_pair() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.apply_ucx_state_word(0x1201, (-50i16) as u16);
        assert_eq!(dev.outputs()[2].balance, -50);
        assert_eq!(dev.outputs()[3].balance, -50);
        assert_eq!(dev.pan(ChannelId::Output(2), 0).unwrap(), -50);
    }

    #[test]
    fn ucx_output_balance_has_its_own_register() {
        let (mut dev, writes) = open(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_pan(ChannelId::Output(2), 0, -50).unwrap();
        let w = written_words(&writes);
        assert!(w.contains(&(0x1201, (-50i16) as u16)));
        assert!(w.contains(&(0x1301, (-50i16) as u16)));
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
            let w = written_words(&writes);
            // AN1/2 is pair 1: DSP outputs 0/1, sources 0/1.
            assert!(w.contains(&(0x4000, mixed)));
            assert!(w.contains(&(0x4001, mixed)));
            assert!(w.contains(&(0x4040, mixed)));
            assert!(w.contains(&(0x4041, mixed)));
        }
        assert_eq!(dev.inputs()[0].width, 0.0);
        assert_eq!(dev.inputs()[1].width, 0.0);

        // +1 restores the diagonals and clears the cross terms.
        writes.lock().unwrap().clear();
        dev.set_channel_width(ChannelId::Input(0), 1.0).unwrap();
        {
            let w = written_words(&writes);
            assert!(w.contains(&(0x4000, ucx::matrix_gain(1.0))));
            assert!(w.contains(&(0x4001, 0)));
            assert!(w.contains(&(0x4040, 0)));
        }

        // A routing with no live send is left alone.
        dev.set_volume(ChannelId::Input(0), 1, 0.0).unwrap();
        dev.set_volume(ChannelId::Input(1), 1, 0.0).unwrap();
        writes.lock().unwrap().clear();
        dev.set_channel_width(ChannelId::Input(0), 0.5).unwrap();
        let w = written_words(&writes);
        assert!(!w
            .iter()
            .any(|(reg, _)| (0x4000..=0x4041).contains(reg)));
    }

    #[test]
    fn ucx_live_width_change_is_one_burst() {
        let (mut dev, writes) = open(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_channel_width(ChannelId::Input(0), 0.0).unwrap();
        let w = writes.lock().unwrap();
        assert_eq!(w.len(), 1, "one USB burst for the whole width update");
        match &w[0] {
            Write::Words(words) => assert!(words.len() > 8),
            other => panic!("expected IF1 words, got {other:?}"),
        }
    }

    #[test]
    fn async_writer_flushes_live_words() {
        let (mut dev, writes) = open_async(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_channel_width(ChannelId::Input(0), 0.0).unwrap();
        dev.flush_writer().unwrap();
        let w = written_words(&writes);
        let mixed = ucx::matrix_gain(std::f32::consts::FRAC_1_SQRT_2 as f64);
        assert!(w.contains(&(0x4000, mixed)));
    }

    #[test]
    fn async_writer_orders_words_before_vendor_requests() {
        let (mut dev, writes) = open_async(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_gain(0, 10).unwrap();
        dev.set_sample_rate(48_000).unwrap();
        let w = writes.lock().unwrap();
        let words = w
            .iter()
            .position(|x| matches!(x, Write::Words(_)))
            .expect("gain word");
        let vendor = w
            .iter()
            .position(|x| matches!(x, Write::Vendor(..)))
            .expect("rate vendor");
        assert!(words < vendor, "queued words must precede the vendor request");
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
            let w = written_words(&writes);
            assert!(w.contains(&(0x4000, trimmed)));
        }

        writes.lock().unwrap().clear();
        dev.set_no_trim(2, true).unwrap();
        {
            let w = written_words(&writes);
            assert!(w.contains(&(0x4000, ucx::matrix_gain(1.0))));
        }
        assert!(dev.outputs()[2].no_trim);

        writes.lock().unwrap().clear();
        dev.set_no_trim(2, false).unwrap();
        {
            let w = written_words(&writes);
            assert!(w.contains(&(0x4000, trimmed)));
        }
        assert!(!dev.outputs()[2].no_trim);
    }

    #[test]
    fn ucx_control_room_speaker_b_and_dim_write_registers() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.set_main_out(Some(1)).unwrap();
        dev.set_main_out_b(Some(2)).unwrap();
        writes.lock().unwrap().clear();
        dev.set_speaker_b(true).unwrap();
        {
            let w = writes.lock().unwrap();
            assert!(w.contains(&Write::Words(vec![(0x1202, 2)])));
            assert!(w.contains(&Write::Words(vec![(0x1302, 2)])));
            assert!(w.contains(&Write::Words(vec![(0x1402, 0)])));
            assert!(w.contains(&Write::Words(vec![(0x1502, 0)])));
            assert!(w.contains(&Write::Words(vec![(0x3d00, 1)])));
        }
        assert!(dev.settings().speaker_b);

        writes.lock().unwrap().clear();
        dev.set_speaker_b(false).unwrap();
        {
            let w = writes.lock().unwrap();
            assert!(w.contains(&Write::Words(vec![(0x1202, 0)])));
            assert!(w.contains(&Write::Words(vec![(0x1302, 0)])));
            assert!(w.contains(&Write::Words(vec![(0x1402, 2)])));
            assert!(w.contains(&Write::Words(vec![(0x1502, 2)])));
            assert!(w.contains(&Write::Words(vec![(0x3d00, 0)])));
        }
        assert!(!dev.settings().speaker_b);

        dev.set_main_out(Some(3)).unwrap();
        let before = (dev.outputs()[6].volume, dev.outputs()[7].volume);
        let untouched = (dev.outputs()[0].volume, dev.outputs()[1].volume);
        writes.lock().unwrap().clear();
        dev.set_dim_with(true, -30.0).unwrap();
        assert!(writes
            .lock()
            .unwrap()
            .contains(&Write::Words(vec![(0x3d03, 1)])));
        assert!(dev.settings().dim);
        assert!(dev.outputs()[6].volume < before.0);
        assert!(dev.outputs()[7].volume < before.1);
        assert!((dev.outputs()[0].volume - untouched.0).abs() < 1e-6);
        assert!((dev.outputs()[1].volume - untouched.1).abs() < 1e-6);

        writes.lock().unwrap().clear();
        dev.set_dim(false).unwrap();
        assert!(writes
            .lock()
            .unwrap()
            .contains(&Write::Words(vec![(0x3d03, 0)])));
        assert!((dev.outputs()[6].volume - before.0).abs() < 1e-6);
        assert!((dev.outputs()[7].volume - before.1).abs() < 1e-6);
        assert!(!dev.settings().dim);
    }

    #[test]
    fn ucx_state_word_reports_speaker_b_active() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.apply_ucx_state_word(0x3d00, 6);
        assert!(dev.settings().speaker_b);
        dev.apply_ucx_state_word(0x3d00, 0);
        assert!(!dev.settings().speaker_b);
    }

    #[test]
    fn ucx_startup_dump_reports_speaker_b_active() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.startup_dump_pending = true;
        dev.apply_ucx_state_word(0x3d00, 1);
        assert!(
            dev.settings().speaker_b,
            "the pre-connect dump must expose a Speaker B state set elsewhere"
        );
        dev.apply_ucx_state_word(0x3d00, 0);
        assert!(!dev.settings().speaker_b);
    }

    #[test]
    fn ucx_dim_affects_main_and_speaker_b_but_not_phones() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_main_out(Some(1)).unwrap();
        dev.set_main_out_b(Some(2)).unwrap();
        let phones = (dev.outputs()[0].volume, dev.outputs()[1].volume);
        let main_before = (dev.outputs()[2].volume, dev.outputs()[3].volume);
        let b_before = (dev.outputs()[4].volume, dev.outputs()[5].volume);

        dev.set_dim_with(true, -20.0).unwrap();
        assert!(dev.outputs()[2].volume < main_before.0);
        assert!(dev.outputs()[3].volume < main_before.1);
        assert!(dev.outputs()[4].volume < b_before.0);
        assert!(dev.outputs()[5].volume < b_before.1);
        assert!((dev.outputs()[0].volume - phones.0).abs() < 1e-6);
        assert!((dev.outputs()[1].volume - phones.1).abs() < 1e-6);

        dev.set_dim(false).unwrap();
        assert!((dev.outputs()[2].volume - main_before.0).abs() < 1e-6);
        assert!((dev.outputs()[3].volume - main_before.1).abs() < 1e-6);
        assert!((dev.outputs()[4].volume - b_before.0).abs() < 1e-6);
        assert!((dev.outputs()[5].volume - b_before.1).abs() < 1e-6);
    }

    #[test]
    fn ucx_speaker_b_link_follows_main_volume_changes() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_main_out(Some(1)).unwrap();
        dev.set_main_out_b(Some(2)).unwrap();
        dev.set_output_volume(1, 0, 0.4).unwrap();
        dev.set_output_volume(2, 0, 0.8).unwrap();

        dev.set_speaker_b_linked(true).unwrap();
        assert!((dev.outputs()[4].volume - 0.4).abs() < 1e-6);
        assert!((dev.outputs()[5].volume - 0.4).abs() < 1e-6);

        dev.set_output_volume(1, 0, 0.25).unwrap();
        assert!((dev.outputs()[4].volume - 0.25).abs() < 1e-6);
        assert!((dev.outputs()[5].volume - 0.25).abs() < 1e-6);

        dev.set_speaker_b_linked(false).unwrap();
        dev.set_output_volume(1, 0, 0.1).unwrap();
        assert!((dev.outputs()[4].volume - 0.25).abs() < 1e-6);
        assert!((dev.outputs()[5].volume - 0.25).abs() < 1e-6);
    }

    #[test]
    fn ucx_speaker_b_link_follows_dim() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_main_out(Some(1)).unwrap();
        dev.set_main_out_b(Some(2)).unwrap();
        dev.set_output_volume(1, 0, db_to_linear(-10.0)).unwrap();
        dev.set_speaker_b_linked(true).unwrap();

        dev.set_dim_with(true, -20.0).unwrap();
        let dimmed = db_to_linear(-30.0);
        assert!((dev.outputs()[4].volume - dimmed).abs() < 1e-6);
        assert!((dev.outputs()[5].volume - dimmed).abs() < 1e-6);

        dev.set_dim(false).unwrap();
        let level = db_to_linear(-10.0);
        assert!((dev.outputs()[4].volume - level).abs() < 1e-6);
        assert!((dev.outputs()[5].volume - level).abs() < 1e-6);
    }

    #[test]
    fn ucx_speaker_b_link_during_dim_adopts_the_main_saved_level() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_main_out(Some(1)).unwrap();
        dev.set_main_out_b(Some(2)).unwrap();
        dev.set_output_volume(1, 0, 0.4).unwrap();
        dev.set_output_volume(2, 0, 0.8).unwrap();

        dev.set_dim_with(true, -20.0).unwrap();
        dev.set_speaker_b_linked(true).unwrap();
        // B follows Main's current (dimmed) level while linked.
        let dimmed = 0.4 * 0.1;
        assert!((dev.outputs()[4].volume - dimmed).abs() < 1e-6);
        assert!((dev.outputs()[5].volume - dimmed).abs() < 1e-6);

        // The saved pre-Dim level is Main's, not B's old independent level.
        dev.set_dim(false).unwrap();
        assert!((dev.outputs()[4].volume - 0.4).abs() < 1e-6);
        assert!((dev.outputs()[5].volume - 0.4).abs() < 1e-6);
    }

    #[test]
    fn ucx_speaker_b_mode_word_keeps_the_output_mute_bit() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.set_main_out(Some(1)).unwrap();
        dev.set_main_out_b(Some(2)).unwrap();
        dev.outputs[4].mute = true;
        dev.outputs[5].mute = true;
        writes.lock().unwrap().clear();
        dev.set_speaker_b(true).unwrap();
        let words = written_words(&writes);
        assert!(words.contains(&(0x1402, 1)));
        assert!(words.contains(&(0x1502, 1)));
    }

    #[test]
    fn ucx_recall_sets_main_out_and_respects_dim() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_main_out(Some(3)).unwrap();

        dev.set_main_recall(-10.0).unwrap();
        let level = db_to_linear(-10.0);
        assert!((dev.outputs()[6].volume - level).abs() < 1e-6);
        assert!((dev.outputs()[7].volume - level).abs() < 1e-6);

        dev.set_dim_with(true, -20.0).unwrap();
        let dimmed = db_to_linear(-30.0);
        assert!((dev.outputs()[6].volume - dimmed).abs() < 1e-6);
        assert!((dev.outputs()[7].volume - dimmed).abs() < 1e-6);

        dev.set_main_recall(-15.0).unwrap();
        let recalled = db_to_linear(-15.0);
        let recalled_dimmed = db_to_linear(-35.0);
        assert!((dev.outputs()[6].volume - recalled_dimmed).abs() < 1e-6);
        assert!((dev.outputs()[7].volume - recalled_dimmed).abs() < 1e-6);

        dev.set_dim(false).unwrap();
        assert!((dev.outputs()[6].volume - recalled).abs() < 1e-6);
        assert!((dev.outputs()[7].volume - recalled).abs() < 1e-6);
    }

    #[test]
    fn ucx_clearing_main_out_b_clears_the_assignment_and_monitor_state() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.set_main_out(Some(1)).unwrap();
        dev.set_main_out_b(Some(6)).unwrap();
        dev.settings.speaker_b = true;
        writes.lock().unwrap().clear();

        dev.set_main_out_b(None).unwrap();
        assert_eq!(dev.settings().main_b_out, None);
        assert!(!dev.settings().speaker_b);
        let words = written_words(&writes);
        assert!(words.contains(&(0x3d00, 0)));
    }

    #[test]
    fn ucx_mono_writes_the_monitor_flags() {
        let (mut dev, writes) = open(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_mono(true).unwrap();
        let words = written_words(&writes);
        assert!(words.contains(&(0x3f9e, 2)));
        assert!(words.contains(&(0x3f98, 0x83ff)));
        assert!(dev.settings().mono);

        writes.lock().unwrap().clear();
        dev.set_mono(false).unwrap();
        let words = written_words(&writes);
        assert!(words.contains(&(0x3f9e, 0)));
        assert!(words.contains(&(0x3f98, 0xffff)));
        assert!(!dev.settings().mono);
    }

    #[test]
    fn ucx_mono_preserves_an_active_talkback_flag() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.talkback_active = true;
        writes.lock().unwrap().clear();
        dev.set_mono(true).unwrap();
        let words = written_words(&writes);
        assert!(words.contains(&(0x3f9e, 3)));
    }

    #[test]
    fn ucx_control_room_scene_does_not_carry_runtime_speaker_b() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.settings.speaker_b = true;
        let scene = dev.capture_scene();
        assert!(!scene.settings.speaker_b);
    }

    #[test]
    fn ucx_control_room_scene_does_not_carry_runtime_external_input() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.settings.external_input = true;
        let scene = dev.capture_scene();
        assert!(!scene.settings.external_input);
    }

    #[test]
    fn ucx_scene_apply_keeps_the_live_speaker_b_state() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.settings.speaker_b = true;
        let mut scene = dev.capture_scene();
        scene.settings.speaker_b = false;
        dev.apply_scene(&scene).unwrap();
        assert!(dev.settings().speaker_b);
    }

    #[test]
    fn ucx_scene_apply_restores_mono_and_mute_fx() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.set_main_out(Some(1)).unwrap();
        dev.settings.mono = true;
        dev.settings.mute_fx = true;
        let scene = dev.capture_scene();
        dev.settings.mono = false;
        dev.settings.mute_fx = false;
        writes.lock().unwrap().clear();
        dev.apply_scene(&scene).unwrap();
        let words = written_words(&writes);
        assert!(words.contains(&(0x3f9e, 2)));
        assert!(words.contains(&(0x3f98, 0x83ff)));
        assert!(words.contains(&(0x1203, 0xfd76)));
        assert!(dev.settings().mono);
        assert!(dev.settings().mute_fx);
    }

    #[test]
    fn ucx_mute_fx_restores_main_and_speaker_b_fx_returns() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.set_main_out(Some(1)).unwrap();
        dev.set_main_out_b(Some(2)).unwrap();
        // Internal order is Phones, AN1/2, AN3/4...; main is AN1/2 and
        // Speaker B is AN3/4, so Phones must not be touched.
        dev.outputs[2].fx_return_db = -12.3;
        dev.outputs[3].fx_return_db = -12.3;
        dev.outputs[4].fx_return_db = -45.5;
        dev.outputs[5].fx_return_db = -45.5;
        dev.set_mute_fx(true).unwrap();
        writes.lock().unwrap().clear();
        dev.set_mute_fx(false).unwrap();
        let words = written_words(&writes);
        assert!(words.contains(&(0x1203, (-123i16) as u16)));
        assert!(words.contains(&(0x1303, (-123i16) as u16)));
        assert!(words.contains(&(0x1403, (-455i16) as u16)));
        assert!(words.contains(&(0x1503, (-455i16) as u16)));
        assert!(!words.iter().any(|(r, _)| *r == 0x1803 || *r == 0x1903));
    }

    #[test]
    fn ucx_external_input_routes_the_source_pair_and_sets_the_monitor_flag() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.set_main_out(Some(1)).unwrap();
        dev.set_main_out_b(Some(2)).unwrap();
        writes.lock().unwrap().clear();
        dev.set_external_input(true, Some(0), -6.0).unwrap();
        let words = written_words(&writes);
        // Main (AN1/2) and Speaker B (AN3/4) carry the selected Mic 1/2 pair
        // at -6 dB; the Source A side goes to the left block, Source B to
        // the right.
        let gain = ucx::matrix_gain(10f64.powf(-6.0 / 20.0));
        assert!(words.contains(&(ucx::matrix_reg(0, Source::Input(0)), gain)));
        assert!(words.contains(&(ucx::matrix_reg(1, Source::Input(1)), gain)));
        assert!(words.contains(&(ucx::matrix_reg(2, Source::Input(0)), gain)));
        assert!(words.contains(&(ucx::matrix_reg(3, Source::Input(1)), gain)));
        // Both FX returns of both monitor pairs are muted.
        for reg in [0x1203, 0x1303, 0x1403, 0x1503] {
            assert!(words.contains(&(reg, 0xfd76)), "missing FX mute {reg:04x}");
        }
        assert!(words.contains(&(0x3f9e, 0x20)));
        assert!(dev.settings().external_input);

        writes.lock().unwrap().clear();
        dev.set_external_input(false, None, 0.0).unwrap();
        let words = written_words(&writes);
        assert!(words.contains(&(0x3f9e, 0)));
        assert!(!dev.settings().external_input);
    }

    #[test]
    fn ucx_pfl_routes_the_input_at_unity_and_restores_the_target() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.set_cue_target(Some(0)).unwrap();
        dev.set_pfl_mode(true, true).unwrap();
        writes.lock().unwrap().clear();
        dev.set_pfl(ChannelId::Input(0), true).unwrap();
        let words = written_words(&writes);
        assert!(words.contains(&(ucx::matrix_reg(6, Source::Input(0)), ucx::matrix_gain(1.0))));
        assert!(dev.inputs[0].solo);
        assert!(words.contains(&(0x1803, 0xfd76)));
        assert!(words.contains(&(0x1903, 0xfd76)));

        writes.lock().unwrap().clear();
        dev.set_pfl(ChannelId::Input(0), false).unwrap();
        assert!(!dev.inputs[0].solo);
        // The target FX returns come back and the listening set is empty.
        assert!(words.contains(&(0x1803, 0xfd76)));
        assert!(words.contains(&(0x1903, 0xfd76)));
        assert!(dev.pfl_active.is_empty());
    }

    #[test]
    fn ucx_pfl_exclusive_keeps_only_one_input_pair() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_cue_target(Some(0)).unwrap();
        dev.set_pfl_mode(true, true).unwrap();
        dev.set_pfl(ChannelId::Input(0), true).unwrap();
        dev.set_pfl(ChannelId::Input(2), true).unwrap();
        assert!(dev.pfl_active.contains(&ChannelId::Input(2)));
        assert!(!dev.pfl_active.contains(&ChannelId::Input(0)));
        assert!(!dev.inputs[0].solo);
        assert!(dev.inputs[2].solo);
        dev.set_pfl(ChannelId::Input(2), false).unwrap();
        assert!(dev.pfl_active.is_empty());
    }

    #[test]
    fn ucx_scene_apply_normalises_linked_eq_and_clamps_fx_return() {
        let (mut dev, writes) = open(Model::Ucx);
        let mut scene = dev.capture_scene();
        scene.settings.input_pair_link = vec![true, false, false];
        scene.inputs[0].eq = Some(InputEq {
            enabled: true,
            low_cut_enabled: true,
            ..Default::default()
        });
        scene.inputs[1].eq = Some(InputEq::default());
        scene.outputs[2].fx_return_db = 6.0;
        scene.outputs[3].fx_return_db = 6.0;
        writes.lock().unwrap().clear();
        dev.apply_scene(&scene).unwrap();
        let words = written_words(&writes);
        assert!(words.contains(&(0x0020, 1)));
        assert!(words.contains(&(0x0120, 1)));
        assert!(words.contains(&(0x0040, 1)));
        assert!(words.contains(&(0x0140, 1)));
        assert!(words.contains(&(0x1203, 0)));
        assert!(words.contains(&(0x1303, 0)));
        assert!(dev.inputs()[1].eq.unwrap().enabled);
        assert_eq!(dev.outputs()[2].fx_return_db, 0.0);
    }

    #[test]
    fn ucx_load_scene_model_is_write_free() {
        let (mut dev, writes) = open(Model::Ucx);
        let mut scene = dev.capture_scene();
        scene.outputs[0].volume = 0.25;
        writes.lock().unwrap().clear();
        dev.load_scene_model(&scene).unwrap();
        assert!(
            written_words(&writes).is_empty(),
            "dry-run scene load wrote device words"
        );
        assert!((dev.outputs()[0].volume - 0.25).abs() < 1e-6);
    }

    #[test]
    fn ucx_startup_dump_capture_does_not_overwrite_the_model() {
        let (mut dev, _writes) = open(Model::Ucx);
        let before = dev.inputs()[0].gain;
        for addr in 0x8000..0x8000 + 1500 {
            dev.state_words.insert(addr, 0);
        }
        dev.startup_dump_pending = true;
        dev.startup_dump_started = Instant::now();
        dev.apply_ucx_state_word(0x0007, 300);
        assert_eq!(dev.inputs()[0].gain, before);
        assert_eq!(dev.device_input_gain.get(&0), Some(&30));
        dev.apply_ucx_state_word(0x3d46, 0);
        assert!(!dev.startup_dump_pending);
        assert!(dev.pre_connect_words.contains_key(&0x3d46));
        assert_eq!(dev.inputs()[0].gain, before);
    }

    #[test]
    fn ucx_changing_the_dim_amount_updates_the_active_dim() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_main_out(Some(3)).unwrap();
        let level = db_to_linear(-10.0);
        dev.outputs[6].volume = level;
        dev.outputs[7].volume = level;

        dev.set_dim_with(true, -20.0).unwrap();
        let dimmed = db_to_linear(-30.0);
        assert!((dev.outputs()[6].volume - dimmed).abs() < 1e-6);
        assert!((dev.outputs()[7].volume - dimmed).abs() < 1e-6);

        dev.set_dim_with(true, -30.0).unwrap();
        let deeper = db_to_linear(-40.0);
        assert!((dev.outputs()[6].volume - deeper).abs() < 1e-6);
        assert!((dev.outputs()[7].volume - deeper).abs() < 1e-6);

        dev.set_dim(false).unwrap();
        assert!((dev.outputs()[6].volume - level).abs() < 1e-6);
        assert!((dev.outputs()[7].volume - level).abs() < 1e-6);
    }

    #[test]
    fn ucx_reassigning_main_out_releases_dim_first() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_main_out(Some(3)).unwrap();
        let level = db_to_linear(-10.0);
        dev.outputs[6].volume = level;
        dev.outputs[7].volume = level;
        dev.set_dim_with(true, -20.0).unwrap();
        assert!(dev.settings().dim);

        dev.set_main_out(Some(1)).unwrap();
        assert!(!dev.settings().dim);
        assert!((dev.outputs()[6].volume - level).abs() < 1e-6);
        assert!((dev.outputs()[7].volume - level).abs() < 1e-6);
        assert_eq!(dev.settings().main_out, Some(1));
    }

    #[test]
    fn ucx_dim_without_a_main_out_does_not_fall_back_to_phones() {
        let (mut dev, _writes) = open(Model::Ucx);
        let before = (dev.outputs()[0].volume, dev.outputs()[1].volume);

        let error = dev.set_dim_with(true, -20.0).unwrap_err();
        assert!(matches!(error, Error::InvalidChannel(_)));
        assert!(!dev.settings().dim);
        assert!((dev.outputs()[0].volume - before.0).abs() < 1e-6);
        assert!((dev.outputs()[1].volume - before.1).abs() < 1e-6);
    }

    #[test]
    fn ucx_recall_without_a_main_out_does_not_fall_back_to_phones() {
        let (mut dev, _writes) = open(Model::Ucx);
        let before = (dev.outputs()[0].volume, dev.outputs()[1].volume);

        let error = dev.set_main_recall(-10.0).unwrap_err();
        assert!(matches!(error, Error::InvalidChannel(_)));
        assert!((dev.outputs()[0].volume - before.0).abs() < 1e-6);
        assert!((dev.outputs()[1].volume - before.1).abs() < 1e-6);
    }

    #[test]
    fn ucx_scene_apply_does_not_dim_the_main_out_twice() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_main_out(Some(3)).unwrap();
        let level = db_to_linear(-20.0);
        dev.outputs[6].volume = level;
        dev.outputs[7].volume = level;

        let mut scene = dev.capture_scene();
        // A scene written while Dim was active used to carry the flag true.
        scene.settings.dim = true;
        dev.apply_scene(&scene).unwrap();
        assert!(!dev.settings().dim);
        assert!((dev.outputs()[6].volume - level).abs() < 1e-6);
        assert!((dev.outputs()[7].volume - level).abs() < 1e-6);

        // A stream-start or rate-change reapply must leave the level alone.
        dev.apply_scene(&scene).unwrap();
        assert!(!dev.settings().dim);
        assert!((dev.outputs()[6].volume - level).abs() < 1e-6);
        assert!((dev.outputs()[7].volume - level).abs() < 1e-6);
    }

    #[test]
    fn ucx_captured_scene_does_not_persist_dim() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_main_out(Some(3)).unwrap();
        dev.set_dim_with(true, -20.0).unwrap();

        let scene = dev.capture_scene();
        assert!(!scene.settings.dim);
    }

    #[test]
    fn ucx_scene_apply_keeps_the_live_main_out_assignment() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_main_out(Some(1)).unwrap();

        let mut scene = dev.capture_scene();
        // A scene can carry an older assignment; the live Control Room
        // configuration wins.
        scene.settings.main_out = Some(3);
        dev.apply_scene(&scene).unwrap();
        assert_eq!(dev.settings().main_out, Some(1));
    }

    #[test]
    fn ucx_clearing_main_out_clears_the_assignment() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_main_out(Some(1)).unwrap();
        dev.set_main_out(None).unwrap();
        assert_eq!(dev.settings().main_out, None);
    }

    #[test]
    fn ucx_playback_mute_preserves_the_fader_level() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_playback_volume(0, 0, 0, 0.4).unwrap();
        let before = dev.playbacks()[0].volumes.clone();
        assert!(before[0] > 0.0);

        dev.set_mute(ChannelId::Playback(0), true).unwrap();
        assert!(dev.playbacks()[0].mute);
        assert_eq!(dev.playbacks()[0].volumes, before, "muting keeps the fader");

        dev.set_mute(ChannelId::Playback(0), false).unwrap();
        assert!(!dev.playbacks()[0].mute);
        assert_eq!(
            dev.playbacks()[0].volumes,
            before,
            "unmuting restores the fader, not zero"
        );
    }

    #[test]
    fn ucx_input_mute_preserves_the_fader_level() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_input_volume(0, 0, 0, 0.35).unwrap();
        let before = dev.inputs()[0].volumes.clone();

        dev.set_mute(ChannelId::Input(0), true).unwrap();
        assert!(dev.inputs()[0].mute);
        assert_eq!(dev.inputs()[0].volumes, before, "muting keeps the fader");

        dev.set_mute(ChannelId::Input(0), false).unwrap();
        assert!(!dev.inputs()[0].mute);
        assert_eq!(
            dev.inputs()[0].volumes,
            before,
            "unmuting restores the fader, not zero"
        );
    }

    #[test]
    fn ucx_solo_rewrites_sends_without_moving_the_fader() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.set_playback_volume(1, 0, 0, 0.5).unwrap();
        let fader = dev.playbacks()[2].volumes[0];
        assert!(fader > 0.0);

        let dsp = ucx::output_pair_dsp(0).unwrap() as u8;
        let reg = ucx::matrix_reg(dsp, Source::Playback(dev.playbacks()[2].id));
        writes.lock().unwrap().clear();

        dev.set_playback_solo(0, 0, true).unwrap();
        assert_eq!(
            dev.playbacks()[2].volumes[0],
            fader,
            "solo must not move the non-soloed fader"
        );
        assert!(
            writes.lock().unwrap().iter().any(|w| {
                matches!(w, Write::Words(v) if v.iter().any(|(a, value)| *a == reg && *value == 0))
            }),
            "soloing another playback pair writes a zero send"
        );

        writes.lock().unwrap().clear();
        dev.set_playback_solo(0, 0, false).unwrap();
        assert_eq!(
            dev.playbacks()[2].volumes[0],
            fader,
            "unsolo must not move the fader"
        );
        assert!(
            writes.lock().unwrap().iter().any(|w| {
                matches!(w, Write::Words(v) if v.iter().any(|(a, value)| *a == reg && *value != 0))
            }),
            "unsolo restores the non-soloed send"
        );
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
    fn ucx_linked_input_fader_preserves_the_channel_difference() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_input_pair_linked(0, false).unwrap();
        dev.set_input_volume(0, 0, 0, 0.1).unwrap();
        dev.set_input_volume(0, 1, 0, 0.5).unwrap();
        dev.set_input_pair_linked(0, true).unwrap();
        assert!((dev.volume(ChannelId::Input(0), 0).unwrap() - 0.1).abs() < 1e-6);
        assert!((dev.volume(ChannelId::Input(1), 0).unwrap() - 0.5).abs() < 1e-6);

        dev.set_input_volume(0, 0, 0, 0.25).unwrap();
        assert!((dev.volume(ChannelId::Input(0), 0).unwrap() - 0.05).abs() < 1e-6);
        assert!((dev.volume(ChannelId::Input(1), 0).unwrap() - 0.25).abs() < 1e-6);
    }

    #[test]
    fn ucx_input_link_copies_fx_eq_and_dynamics() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_input_pair_linked(0, false).unwrap();
        dev.set_channel_fx_send(ChannelId::Input(0), -30.0)
            .unwrap();
        dev.set_channel_fx_send(ChannelId::Input(1), -10.0)
            .unwrap();

        let mut eq = InputEq::default();
        eq.enabled = true;
        eq.low_cut_enabled = true;
        eq.low_cut_freq_hz = 120;
        eq.low_cut_slope_db_oct = 18;
        eq.bands[0].band_type = EqBandType::Bell;
        eq.bands[0].gain_db = 4.0;
        eq.bands[0].freq_hz = 250;
        eq.bands[0].q = 1.4;
        dev.apply_input_eq(0, &eq).unwrap();

        let dynamics = Dynamics {
            on: true,
            gain_db: 3.0,
            attack_ms: 20,
            release_ms: 400,
            comp_threshold_db: -24.0,
            comp_ratio: 2.5,
            exp_threshold_db: -40.0,
            exp_ratio: 1.8,
            auto_level: true,
            auto_max_gain_db: 9.0,
            auto_headroom_db: 6.0,
            auto_rise_s: 2.0,
        };
        dev.set_dynamics(ChannelId::Input(0), &dynamics).unwrap();

        dev.set_input_pair_linked(0, true).unwrap();
        assert_eq!(dev.inputs()[1].fx_send_db, -30.0);
        assert_eq!(dev.inputs()[1].eq, Some(eq));
        assert_eq!(dev.inputs()[1].dynamics, Some(dynamics));
    }

    #[test]
    fn ucx_output_link_copies_fx_eq_and_dynamics() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_output_linked(1, false).unwrap();

        let mut eq = InputEq::default();
        eq.enabled = true;
        eq.low_cut_enabled = true;
        eq.low_cut_freq_hz = 80;
        eq.low_cut_slope_db_oct = 12;
        eq.bands[2].band_type = EqBandType::HighShelf;
        eq.bands[2].gain_db = -3.0;
        eq.bands[2].freq_hz = 6_000;
        eq.bands[2].q = 0.9;
        dev.set_output_eq(2, &eq).unwrap();

        let dynamics = Dynamics {
            on: true,
            gain_db: -2.0,
            attack_ms: 30,
            release_ms: 500,
            comp_threshold_db: -18.0,
            comp_ratio: 3.0,
            exp_threshold_db: -45.0,
            exp_ratio: 2.0,
            auto_level: false,
            auto_max_gain_db: 6.0,
            auto_headroom_db: 6.0,
            auto_rise_s: 5.0,
        };
        dev.set_dynamics(ChannelId::Output(2), &dynamics).unwrap();
        dev.set_fx_return(2, -12.0).unwrap();

        dev.set_output_linked(1, true).unwrap();
        assert_eq!(dev.outputs()[3].eq, Some(eq));
        assert_eq!(dev.outputs()[3].dynamics, Some(dynamics));
        assert_eq!(dev.outputs()[3].fx_return_db, -12.0);
    }

    #[test]
    fn ucx_input_link_derives_balance_from_the_split_levels() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_input_pair_linked(0, false).unwrap();
        dev.set_input_volume(0, 0, 0, 0.1).unwrap();
        dev.set_input_volume(0, 1, 0, 0.5).unwrap();
        dev.set_input_pair_linked(0, true).unwrap();
        assert!((dev.volume(ChannelId::Input(0), 0).unwrap() - 0.1).abs() < 1e-6);
        assert!((dev.volume(ChannelId::Input(1), 0).unwrap() - 0.5).abs() < 1e-6);

        dev.set_input_balance(0, 0, -32).unwrap();
        assert!((dev.volume(ChannelId::Input(0), 0).unwrap() - 0.5).abs() < 1e-6);
        assert!((dev.volume(ChannelId::Input(1), 0).unwrap() - 0.34).abs() < 1e-6);
    }

    #[test]
    fn ucx_output_link_round_trip_preserves_balance_and_split_volumes() {
        let (mut dev, _writes) = open(Model::Ucx);
        let left = ChannelId::Output(2);
        let right = ChannelId::Output(3);

        dev.set_output_volume(1, 0, 0.8).unwrap();
        dev.set_pan(left, 0, -40).unwrap();
        dev.set_output_linked(1, false).unwrap();
        assert!((dev.volume(left, 0).unwrap() - 0.8).abs() < 1e-6);
        assert!((dev.volume(right, 0).unwrap() - 0.48).abs() < 1e-6);
        assert_eq!(dev.outputs()[2].balance, 0);

        dev.set_output_volume(1, 0, 0.3).unwrap();
        dev.set_output_volume(1, 1, 0.6).unwrap();
        dev.set_output_linked(1, true).unwrap();
        assert!((dev.volume(left, 0).unwrap() - 0.6).abs() < 1e-6);
        assert!((dev.volume(right, 0).unwrap() - 0.6).abs() < 1e-6);
        assert_eq!(dev.pan(left, 0).unwrap(), 50);

        dev.set_output_linked(1, false).unwrap();
        assert!((dev.volume(left, 0).unwrap() - 0.3).abs() < 1e-6);
        assert!((dev.volume(right, 0).unwrap() - 0.6).abs() < 1e-6);
        assert_eq!(dev.outputs()[2].balance, 0);
    }

    #[test]
    fn uc_output_link_round_trip_keeps_the_derived_balance() {
        let (mut dev, _writes) = open(Model::Uc);
        dev.set_output_linked(0, false).unwrap();
        dev.set_output_volume(0, 0, 0.2).unwrap();
        dev.set_output_volume(0, 1, 0.8).unwrap();
        dev.set_output_linked(0, true).unwrap();
        assert_eq!(dev.pan(ChannelId::Output(0), 0).unwrap(), 75);
        assert!((dev.volume(ChannelId::Output(0), 0).unwrap() - 0.2).abs() < 1e-6);
        assert!((dev.volume(ChannelId::Output(1), 0).unwrap() - 0.8).abs() < 1e-6);
        dev.set_output_linked(0, false).unwrap();
        assert!((dev.volume(ChannelId::Output(0), 0).unwrap() - 0.2).abs() < 1e-6);
        assert!((dev.volume(ChannelId::Output(1), 0).unwrap() - 0.8).abs() < 1e-6);
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
        // A front-panel move made while TotalControl was not running can arrive
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
    fn ucx_input_ms_proc_writes_nn05_and_the_ms_matrix() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.settings.input_pair_link = vec![true, true];
        dev.inputs[0].volumes[0] = 0.5;
        dev.inputs[1].volumes[0] = 0.5;
        dev.inputs[0].width = 1.0;
        writes.lock().unwrap().clear();
        dev.set_input_ms_proc(0, true).unwrap();
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Words(vec![(0x0005, 1)])));
        assert!(w.contains(&Write::Words(vec![(0x0105, 1)])));
        // Pair 0 is Phones 7/8 (DSP 6/7).  For gl = gr = 0.5 the M/S
        // encoder writes L/2, R/2, L/2, -R/2 = ±0.25.
        let ms = ucx::matrix_gain(0.25);
        let neg = ucx::matrix_gain(-0.25);
        assert!(w.contains(&Write::Words(vec![(0x4180, ms)])));
        assert!(w.contains(&Write::Words(vec![(0x4181, ms)])));
        assert!(w.contains(&Write::Words(vec![(0x41C0, ms)])));
        assert!(w.contains(&Write::Words(vec![(0x41C1, neg)])));
        drop(w);

        writes.lock().unwrap().clear();
        dev.set_input_ms_proc(0, false).unwrap();
        let w = writes.lock().unwrap();
        assert!(w.contains(&Write::Words(vec![(0x0005, 0)])));
        assert!(w.contains(&Write::Words(vec![(0x4180, ucx::matrix_gain(0.5))])));
        assert!(w.contains(&Write::Words(vec![(0x41C1, ucx::matrix_gain(0.5))])));
    }

    #[test]
    fn ucx_playback_ms_proc_rewrites_the_matrix() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.playbacks[0].volumes[0] = 0.5;
        dev.playbacks[1].volumes[0] = 0.5;
        dev.playbacks[0].width = 1.0;
        writes.lock().unwrap().clear();
        dev.set_playback_ms_proc(0, true).unwrap();
        let w = writes.lock().unwrap();
        let ms = ucx::matrix_gain(0.25);
        let neg = ucx::matrix_gain(-0.25);
        assert!(w.contains(&Write::Words(vec![(0x41A0, ms)])));
        assert!(w.contains(&Write::Words(vec![(0x41A1, ms)])));
        assert!(w.contains(&Write::Words(vec![(0x41E0, ms)])));
        assert!(w.contains(&Write::Words(vec![(0x41E1, neg)])));
        // No +0x180 "copy": 0x4320 is ADAT 3/4's own cell.
        assert!(!w.contains(&Write::Words(vec![(0x4320, ms)])));
    }

    #[test]
    fn ucx_scene_apply_keeps_each_playback_send_in_its_own_output_cell() {
        let (mut dev, writes) = open(Model::Ucx);
        let mut scene = dev.capture_scene();
        // Playback 1/2 sends loud into Phones (internal pair 0, DSP 6) and
        // quieter into ADAT 3/4 (internal pair 6, DSP 12).
        for ch in 0..2 {
            scene.playbacks[ch].volumes[0] = 1.0;
            scene.playbacks[ch].volumes[6] = 0.25;
        }
        writes.lock().unwrap().clear();
        dev.apply_scene(&scene).unwrap();
        let words = written_words(&writes);
        let last = |addr: u16| {
            words
                .iter()
                .rev()
                .find(|(a, _)| *a == addr)
                .map(|(_, v)| *v)
        };
        // The last write to each cell is that output's own coefficient, not
        // a neighbouring output's "copy".
        assert_eq!(last(0x41A0), Some(ucx::matrix_gain(1.0)));
        assert_eq!(last(0x4320), Some(ucx::matrix_gain(0.25)));
    }

    #[test]
    fn ucx_talkback_writes_the_dynamic_source_and_dims() {
        let (mut dev, writes) = open(Model::Ucx);
        // Internal pair 1 is Main (DSP 0); pair 2 is the receiver (DSP 2),
        // matching the addresses in the controlled captures.
        dev.settings.main_out = Some(1);
        dev.settings.ms_proc = true;
        dev.outputs[4].talkback = true;
        dev.outputs[5].talkback = true;
        for input in &mut dev.inputs {
            input.volumes.fill(0.0);
        }
        for playback in &mut dev.playbacks {
            playback.volumes.fill(0.0);
        }
        dev.inputs[0].volumes[2] = 1.0;
        dev.inputs[0].pans[2] = 0;
        dev.playbacks[0].volumes[1] = 1.0;
        dev.playbacks[1].volumes[1] = 1.0;
        dev.playbacks[0].volumes[2] = 1.0;
        dev.playbacks[1].volumes[2] = 1.0;
        dev.outputs[2].fx_return_db = -65.0;
        dev.outputs[3].fx_return_db = -65.0;
        dev.outputs[4].fx_return_db = -65.0;
        dev.outputs[5].fx_return_db = -65.0;
        dev.settings.mono = true;
        writes.lock().unwrap().clear();

        dev.set_talkback_with(
            true,
            &TalkbackConfig {
                source: Some(0),
                dim_db: -20.0,
                listenback: None,
                listenback_dim_db: -20.0,
            },
        )
        .unwrap();
        let words = written_words(&writes);
        assert!(words.contains(&(0x14e0, 0xd760)));
        assert!(words.contains(&(0x14e0, 0x0008)));
        assert!(words.contains(&(0x15e0, 0xd760)));
        assert!(words.contains(&(0x4080, 0x8b33)));
        assert!(words.contains(&(0x40a0, ucx::matrix_gain(0.1))));
        assert!(words.contains(&(0x4061, ucx::matrix_gain(0.1))));
        assert!(words.contains(&(0x1403, 0xfd76)));
        assert!(words.contains(&(0x1503, 0xfd76)));
        assert!(words.contains(&(0x3f9e, 0x0003)));

        writes.lock().unwrap().clear();
        dev.set_talkback_with(false, &TalkbackConfig::default())
            .unwrap();
        let words = written_words(&writes);
        assert!(words.contains(&(0x4080, 0)));
        assert!(words.contains(&(0x4061, ucx::matrix_gain(1.0))));
        assert!(words.contains(&(0x3f9e, 0x0002)));
        assert!(words.contains(&(0x1403, 0xfd76)));
    }

    #[test]
    fn ucx_talkback_listenback_same_source_omits_the_second_block() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.settings.main_out = Some(1);
        dev.outputs[4].talkback = true;
        for input in &mut dev.inputs {
            input.volumes.fill(0.0);
        }
        for playback in &mut dev.playbacks {
            playback.volumes.fill(0.0);
        }
        dev.inputs[0].volumes[1] = 1.0;
        dev.inputs[0].volumes[2] = 1.0;
        writes.lock().unwrap().clear();
        dev.set_talkback_with(
            true,
            &TalkbackConfig {
                source: Some(0),
                dim_db: -20.0,
                listenback: Some(0),
                listenback_dim_db: -20.0,
            },
        )
        .unwrap();
        let words = written_words(&writes);
        assert!(!words.iter().any(|(addr, _)| *addr == 0x14e0));
        assert!(!words.iter().any(|(addr, _)| *addr == 0x12e1));
    }

    #[test]
    fn ucx_talkback_listenback_different_source_adds_the_main_block() {
        let (mut dev, writes) = open(Model::Ucx);
        dev.settings.main_out = Some(1);
        dev.outputs[4].talkback = true;
        for input in &mut dev.inputs {
            input.volumes.fill(0.0);
        }
        for playback in &mut dev.playbacks {
            playback.volumes.fill(0.0);
        }
        dev.inputs[0].volumes[2] = 1.0;
        dev.inputs[1].volumes[1] = 1.0;
        writes.lock().unwrap().clear();
        dev.set_talkback_with(
            true,
            &TalkbackConfig {
                source: Some(0),
                dim_db: -20.0,
                listenback: Some(1),
                listenback_dim_db: -20.0,
            },
        )
        .unwrap();
        let words = written_words(&writes);
        assert!(words.contains(&(0x12e1, 0xd760)));
        assert!(words.contains(&(0x4001, 0x8b33)));
    }

    #[test]
    fn ucx_talkback_without_source_writes_nothing() {
        let (mut dev, writes) = open(Model::Ucx);
        writes.lock().unwrap().clear();
        dev.set_talkback_with(true, &TalkbackConfig::default())
            .unwrap();
        assert!(written_words(&writes).is_empty());
        dev.set_talkback_with(false, &TalkbackConfig::default())
            .unwrap();
        assert!(written_words(&writes).is_empty());
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
    fn ucx_state_dump_keeps_every_raw_word() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.apply_ucx_state_word(0x0000, 1);
        dev.apply_ucx_state_word(0x1200, 0xff86);
        assert_eq!(dev.state_words().get(&0x0000), Some(&1));
        assert_eq!(dev.state_words().get(&0x1200), Some(&0xff86));
    }

    #[test]
    fn ucx_right_gain_keeps_the_pair_alias_in_step() {
        let (mut dev, _writes) = open(Model::Ucx);
        dev.set_input_gain_right(0, 35).unwrap();
        assert_eq!(dev.inputs()[0].gain_right, Some(35));
        assert_eq!(dev.inputs()[1].gain, Some(35));
        dev.apply_ucx_state_word(0x0107, 150);
        assert_eq!(dev.inputs()[1].gain, Some(15));
        assert_eq!(dev.inputs()[0].gain_right, Some(15));
    }

    #[test]
    fn ucx_scene_gain_wins_over_a_stale_state_word() {
        // The UCX does not report gain at connect, so the scene is the
        // authoritative preamp value; the state stream only updates live
        // encoder moves after the scene has been applied.
        let (mut dev, writes) = open(Model::Ucx);
        dev.apply_ucx_state_word(0x0207, 15);
        let mut scene = dev.capture_scene();
        scene.inputs[2].gain = Some(0);
        writes.lock().unwrap().clear();
        dev.apply_scene(&scene).unwrap();
        assert_eq!(dev.inputs()[2].gain, Some(0));
        let words = written_words(&writes);
        assert!(words.iter().any(|(addr, value)| *addr == 0x0207 && *value == 0));
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
        assert!(
            !differences.iter().any(|item| item.contains("AN1 gain")),
            "UCX input gain is not trusted at connect"
        );
        assert!(differences.iter().any(|item| item.contains("volume")));
        assert!(
            !differences.iter().any(|item| item.contains("Clock")),
            "the UCX does not read back the options word"
        );

        writes.lock().unwrap().clear();
        dev.restore_readable(&state).unwrap();
        let w = writes.lock().unwrap();
        assert!(
            !w.iter().any(|write| matches!(
                write,
                Write::Words(words) if words.iter().any(|(addr, _)| *addr == 0x0007)
            )),
            "choosing the device must not restore a gain the unit did not report at connect"
        );
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
        let (mut dev, writes) = open(Model::Ucx);
        let mut scene = dev.capture_scene();
        scene.inputs[0].gain = Some(20);
        scene.inputs[0].phantom = true;
        scene.inputs[0].autoset = true;
        scene.inputs[2].level = Some(InputLevel::LoGain);
        scene.inputs[2].ref_level = 2;
        scene.inputs[2].instrument = true;
        writes.lock().unwrap().clear();
        dev.apply_scene(&scene).unwrap();

        let inputs = dev.inputs();
        assert_eq!(inputs[0].gain, Some(20));
        assert!(inputs[0].phantom);
        assert!(inputs[0].autoset);
        assert_eq!(inputs[2].level, Some(InputLevel::LoGain));
        assert_eq!(inputs[2].ref_level, 2);
        assert!(inputs[2].instrument);

        // The UCX stores these but does not report them at connect, so the
        // scene must be written to make the GUI and hardware agree.
        let words = written_words(&writes);
        let last = |addr: u16| {
            words
                .iter()
                .rev()
                .find(|(a, _)| *a == addr)
                .map(|(_, v)| *v)
        };
        assert_eq!(last(0x0007), Some(200), "input 1 gain 20.0 dB");
        assert_eq!(last(0x0008), Some(1), "input 1 48 V");
        assert_eq!(last(0x0209), Some(1), "input 3 instrument");
        assert_eq!(last(0x0208), Some(2), "input 3 LoGain level");
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
        scene.settings.line_out_ref = 1;
        scene.settings.phones_ref = 2;
        scene.settings.line_in_ref_58 = 3;
        writes.lock().unwrap().clear();
        dev.apply_scene(&scene).unwrap();

        let settings = dev.settings();
        assert_eq!(settings.clock_source, "Word Clock");
        assert!(settings.optical_out_spdif);
        assert!(settings.spdif_professional);
        assert!(settings.word_clock_single_speed);
        assert!(settings.word_clock_termination);
        assert!(settings.eq_for_record);
        // These are device-owned and unreadable, so the scene is written
        // at connect to keep the GUI and hardware in step.
        let words = written_words(&writes);
        let last = |addr: u16| {
            words
                .iter()
                .rev()
                .find(|(a, _)| *a == addr)
                .map(|(_, v)| *v)
        };
        assert_eq!(last(0x1208), Some(1));
        assert_eq!(last(0x1808), Some(2));
        assert_eq!(last(0x0407), Some(3));
        assert!(writes
            .lock()
            .unwrap()
            .iter()
            .any(|w| matches!(w, Write::Vendor(protocol::req::SETTINGS, 0x00CE, 1487))));
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
    fn ucx_scene_apply_is_sent_in_paced_bursts() {
        let (mut dev, writes) = open(Model::Ucx);
        let scene = dev.capture_scene();
        writes.lock().unwrap().clear();
        let start = Instant::now();
        dev.apply_scene(&scene).unwrap();
        let elapsed = start.elapsed();
        let w = writes.lock().unwrap();
        let bursts: Vec<&Vec<(u16, u16)>> = w
            .iter()
            .filter_map(|x| match x {
                Write::Words(v) => Some(v),
                Write::Vendor(..) => None,
            })
            .collect();
        let words: usize = bursts.iter().map(|v| v.len()).sum();
        assert!(words > 1000, "scene shrank to {words} words");
        assert!(
            bursts.iter().all(|v| !v.is_empty() && v.len() <= HWDEP_MAX_WORDS),
            "a burst exceeded the ABI limit"
        );
        assert!(
            bursts.iter().any(|v| v.len() > 1),
            "words were still sent one at a time"
        );
        assert!(
            w.len() < 100,
            "scene still used {} USB transfers",
            w.len()
        );
        // Full bursts at the 16 ms drain gap, plus the loopback vendors.
        assert!(elapsed < Duration::from_secs(2), "scene took {elapsed:?}");
    }

    #[test]
    fn ucx_eq_band_types_are_band_specific() {
        let mut eq = InputEq::default();
        eq.bands[0].band_type = EqBandType::HighShelf;
        eq.bands[1].band_type = EqBandType::Bell;
        eq.bands[2].band_type = EqBandType::LowShelf;
        let mut s = ChannelSettings::default();
        apply_eq_to_settings(&eq, &mut s);
        assert_eq!(s.eq.bands[0].kind, 0, "band 1 has no high shelf");
        assert_eq!(s.eq.bands[1].kind, 0, "band 2 is fixed to Bell");
        assert_eq!(s.eq.bands[2].kind, 0, "band 3 has no low shelf");

        eq.bands[0].band_type = EqBandType::LowShelf;
        eq.bands[2].band_type = EqBandType::HighShelf;
        apply_eq_to_settings(&eq, &mut s);
        assert_eq!(s.eq.bands[0].kind, 1);
        assert_eq!(s.eq.bands[1].kind, 0);
        assert_eq!(s.eq.bands[2].kind, 1);

        eq.bands[2].band_type = EqBandType::LowPass;
        apply_eq_to_settings(&eq, &mut s);
        assert_eq!(s.eq.bands[2].kind, 2);
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
