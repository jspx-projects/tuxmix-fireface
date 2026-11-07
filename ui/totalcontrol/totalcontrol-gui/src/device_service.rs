//! Device service — the first stage of the device/UI split.
//!
//! The device currently lives on the UI thread.  This module introduces the
//! boundary that will own it in the next stage:
//!
//! - one thread owns the `DeviceHandle`;
//! - the UI will send [`Command`]s instead of calling the device;
//! - the UI will read a [`Snapshot`] of the channel model;
//! - meter canvases will read [`MeterSnapshot`] without a view rebuild.
//!
//! Stage 1 keeps the service off the running path (the UI still uses the
//! device directly) so the tree stays green while the interface is wired in
//! stage 2.  The mock tests exercise the whole boundary.
//!
//! Temporary switch: `TOTALCONTROL_DEVICE_SERVICE=1` is reserved for the stage-2
//! wiring; it is deliberately not read yet.

#![allow(dead_code)]

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use totalcontrol_core::{
    ChannelId, DeviceSettings, Dynamics, Echo, Error, InputChannel, InputLevel,
    PlaybackChannel, OutputChannel, RmeDevice, Reverb, Scene, Sensitivity, TalkbackConfig,
};
use totalcontrol_core::fireface::abi::HwdepCounters;
use totalcontrol_core::channel::{EqBandType, InputEq};
use tokio::sync::Notify;

use crate::app::DeviceHandle;

/// Wakes the UI subscription when a model snapshot is published.  The
/// snapshot itself travels through the `std` channel on `DeviceService`;
/// this is only the "there is something to drain" edge, so an idle mixer
/// does not rebuild its view at the polling rate.
static SNAPSHOT_NOTIFY: OnceLock<Arc<Notify>> = OnceLock::new();

fn snapshot_notify() -> Arc<Notify> {
    Arc::clone(SNAPSHOT_NOTIFY.get_or_init(|| Arc::new(Notify::new())))
}

/// Wakes the UI subscription when meter activity starts after silence.  A
/// meter frame itself never publishes a message; this edge only tells an
/// idle UI to draw the first frame of a new burst.
static METER_NOTIFY: OnceLock<Arc<Notify>> = OnceLock::new();

fn meter_notify() -> Arc<Notify> {
    Arc::clone(METER_NOTIFY.get_or_init(|| Arc::new(Notify::new())))
}

/// The meter-wake stream consumed by `app::subscription`.  It yields once
/// when the service sees the first activity after an idle period.
pub fn meter_stream() -> impl iced::futures::Stream<Item = ()> {
    iced::futures::stream::unfold(meter_notify(), |notify| async move {
        notify.notified().await;
        Some(((), notify))
    })
}

/// The model-update stream consumed by `app::subscription`.  It yields only
/// when the service has actually published a snapshot.
pub fn update_stream() -> impl iced::futures::Stream<Item = ()> {
    iced::futures::stream::unfold(snapshot_notify(), |notify| async move {
        notify.notified().await;
        Some(((), notify))
    })
}

/// A command sent from the UI to the device service.
pub enum Command {
    SetVolume(ChannelId, usize, f32),
    SetMute(ChannelId, bool),
    ApplyScene(Box<Scene>),
    /// A control that needs the device thread.  Used by the mirror for
    /// setters that have no dedicated variant.
    Run(Box<dyn FnOnce(&mut DeviceHandle) + Send>),
    Stop,
}

/// Static properties of the device, captured once when the service starts.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub model_name: String,
    pub canonical_model: String,
    pub capabilities: totalcontrol_core::DeviceCapabilities,
    pub protocol_features: bool,
    pub low_cut_supported: bool,
    pub phase_supported: bool,
    pub output_balance_supported: bool,
    pub loopback_supported: bool,
    pub stereo_split_supported: bool,
    pub matrix_uses_display_order: bool,
    pub output_pair_count: usize,
    pub output_display_order: Vec<usize>,
    pub is_uc: bool,
    pub is_ucx: bool,
    pub is_uc_family: bool,
    pub is_mock: bool,
    pub kind: crate::app::DeviceKind,
    pub target: crate::app::DeviceTarget,
    pub card_id: &'static str,
    pub firmware: Option<u32>,
    pub serial: Option<String>,
    pub rate_follows: bool,
    pub has_sensitivity: bool,
    pub outputs_one_per_pair: bool,
    /// Per-input meter availability on the backend that was captured.
    pub input_meters: Vec<bool>,
    /// The backend supplies its own software-playback metering, so the
    /// PipeWire sink tap is not needed.
    pub device_playback_meters: bool,
}

/// The channel model as the UI should draw it.  Replaced wholesale when the
/// service sees a change; cheap enough to clone at the UI's housekeeping
/// rate.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub scene: Scene,
    pub info: DeviceInfo,
    pub connected: bool,
    pub hardware_volume: Option<f32>,
    pub panel_selection: Option<(bool, usize, usize)>,
    pub at: Instant,
}

/// The UI-side copy of the model.
///
/// Getters return references into the local copy, so the view never locks or
/// blocks on the device thread.  Setters enqueue a command; the next
/// [`Snapshot`] replaces the local copy.  The service updates it from
/// `DeviceService::take_updates`.
pub struct Mirror {
    scene: Scene,
    info: DeviceInfo,
    commands: Sender<Command>,
    meters: Arc<Mutex<MeterSnapshot>>,
    connected: bool,
    hardware_volume: Option<f32>,
    panel_selection: Option<(bool, usize, usize)>,
}

impl Mirror {
    pub fn new(
        initial: Snapshot,
        commands: Sender<Command>,
        meters: Arc<Mutex<MeterSnapshot>>,
    ) -> Self {
        Self {
            scene: initial.scene,
            info: initial.info,
            commands,
            meters,
            connected: initial.connected,
            hardware_volume: initial.hardware_volume,
            panel_selection: initial.panel_selection,
        }
    }

    pub fn meters(&self) -> MeterSnapshot {
        *self.meters.lock().unwrap()
    }

    pub fn meter_bus(&self) -> Arc<Mutex<MeterSnapshot>> {
        Arc::clone(&self.meters)
    }

    /// Driver-side transport health as last reported by the service.
    pub fn transport_counters(&self) -> Option<HwdepCounters> {
        self.meters.lock().ok()?.counters
    }

    /// Apply a snapshot published by the service.  Returns false once the
    /// service has stopped.
    pub fn update(&mut self, snapshot: Snapshot) -> bool {
        self.scene = snapshot.scene;
        self.info = snapshot.info;
        self.connected = snapshot.connected;
        self.hardware_volume = snapshot.hardware_volume;
        self.panel_selection = snapshot.panel_selection;
        true
    }

    pub fn inputs(&self) -> &[totalcontrol_core::InputChannel] {
        &self.scene.inputs
    }

    pub fn playbacks(&self) -> &[totalcontrol_core::PlaybackChannel] {
        &self.scene.playbacks
    }

    pub fn outputs(&self) -> &[totalcontrol_core::OutputChannel] {
        &self.scene.outputs
    }

    pub fn settings(&self) -> &totalcontrol_core::DeviceSettings {
        &self.scene.settings
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    pub fn hardware_volume(&self) -> Option<f32> {
        self.hardware_volume
    }

    pub fn panel_selection(&self) -> Option<(bool, usize, usize)> {
        self.panel_selection
    }

    pub fn set_volume(&self, channel: ChannelId, output: usize, value: f32) -> bool {
        self.commands
            .send(Command::SetVolume(channel, output, value))
            .is_ok()
    }

    pub fn set_mute(&self, channel: ChannelId, mute: bool) -> bool {
        self.commands.send(Command::SetMute(channel, mute)).is_ok()
    }

    fn run(&self, command: impl FnOnce(&mut DeviceHandle) + Send + 'static) {
        let _ = self.commands.send(Command::Run(Box::new(command)));
    }

    pub fn set_line_in_ref(&self, code: u16) {
        self.run(move |d| {
            let _ = d.set_line_in_ref(code);
        });
    }

    pub fn set_line_out_ref(&self, code: u16) {
        self.run(move |d| {
            let _ = d.set_line_out_ref(code);
        });
    }

    pub fn set_phones_ref(&self, code: u16) {
        self.run(move |d| {
            let _ = d.set_phones_ref(code);
        });
    }

    pub fn set_line_in_ref_58(&self, code: u16) {
        self.run(move |d| {
            let _ = d.set_line_in_ref_58(code);
        });
    }

    pub fn set_spdif_professional(&self, on: bool) {
        self.run(move |d| {
            let _ = d.set_spdif_professional(on);
        });
    }

    pub fn set_word_clock_single_speed(&self, on: bool) {
        self.run(move |d| {
            let _ = d.set_word_clock_single_speed(on);
        });
    }

    pub fn set_word_clock_termination(&self, on: bool) {
        self.run(move |d| {
            let _ = d.set_word_clock_termination(on);
        });
    }

    pub fn set_input_ms_proc(&self, idx: usize, on: bool) {
        self.run(move |d| {
            let _ = d.set_input_ms_proc(idx, on);
        });
    }

    pub fn set_playback_ms_proc(&self, idx: usize, on: bool) {
        self.run(move |d| {
            let _ = d.set_playback_ms_proc(idx, on);
        });
    }

    pub fn set_talkback(&self, on: bool) {
        self.set_talkback_with(on, &TalkbackConfig::default());
    }

    pub fn set_talkback_with(&self, on: bool, config: &TalkbackConfig) {
        let config = *config;
        self.run(move |d| {
            let _ = d.set_talkback_with(on, &config);
        });
    }

    pub fn set_talkback_receive(&self, output: usize, on: bool) {
        self.run(move |d| {
            let _ = d.set_talkback_receive(output, on);
        });
    }

    /// Push the local settings block to the service.  Link groupings and
    /// the Control Room mono flag are host-model settings with no dedicated
    /// trait setter; without this the next service snapshot would replace
    /// the local edit with the old value.
    pub fn sync_settings(&self) {
        let settings = self.scene.settings.clone();
        self.run(move |d| {
            *d.settings_mut() = settings;
        });
    }
}

impl RmeDevice for Mirror {
    fn open() -> Result<Self, Error> {
        unreachable!("the mirror is constructed from a snapshot")
    }

    fn model_name(&self) -> &str {
        &self.info.model_name
    }

    fn capabilities(&self) -> totalcontrol_core::DeviceCapabilities {
        self.info.capabilities
    }

    fn protocol_features(&self) -> bool {
        self.info.protocol_features
    }

    fn canonical_model(&self) -> &str {
        &self.info.canonical_model
    }

    fn low_cut_supported(&self) -> bool {
        self.info.low_cut_supported
    }

    fn phase_supported(&self) -> bool {
        self.info.phase_supported
    }

    fn output_balance_supported(&self) -> bool {
        self.info.output_balance_supported
    }

    fn loopback_supported(&self) -> bool {
        self.info.loopback_supported
    }

    fn stereo_split_supported(&self) -> bool {
        self.info.stereo_split_supported
    }

    fn matrix_uses_display_order(&self) -> bool {
        self.info.matrix_uses_display_order
    }

    fn output_display_order(&self) -> Vec<usize> {
        self.info.output_display_order.clone()
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    fn begin_metering(&mut self) {
        self.run(|d| d.begin_metering());
    }

    fn output_pair_count(&self) -> usize {
        self.info.output_pair_count
    }

    fn inputs(&self) -> &[InputChannel] {
        &self.scene.inputs
    }

    fn inputs_mut(&mut self) -> &mut [InputChannel] {
        &mut self.scene.inputs
    }

    fn playbacks(&self) -> &[PlaybackChannel] {
        &self.scene.playbacks
    }

    fn playbacks_mut(&mut self) -> &mut [PlaybackChannel] {
        &mut self.scene.playbacks
    }

    fn outputs(&self) -> &[OutputChannel] {
        &self.scene.outputs
    }

    fn outputs_mut(&mut self) -> &mut [OutputChannel] {
        &mut self.scene.outputs
    }

    fn settings(&self) -> &DeviceSettings {
        &self.scene.settings
    }

    fn settings_mut(&mut self) -> &mut DeviceSettings {
        &mut self.scene.settings
    }

    fn set_volume(
        &mut self,
        channel: ChannelId,
        output: usize,
        volume: f32,
    ) -> Result<(), Error> {
        self.commands
            .send(Command::SetVolume(channel, output, volume))
            .map_err(|_| stopped())?;
        Ok(())
    }

    fn volume(&self, channel: ChannelId, output: usize) -> Result<f32, Error> {
        match channel {
            ChannelId::Input(i) => self
                .scene
                .inputs
                .get(i)
                .and_then(|c| c.volumes.get(output))
                .copied()
                .ok_or_else(|| missing(channel)),
            ChannelId::Playback(i) => self
                .scene
                .playbacks
                .get(i)
                .and_then(|c| c.volumes.get(output))
                .copied()
                .ok_or_else(|| missing(channel)),
            ChannelId::Output(i) => self
                .scene
                .outputs
                .get(i)
                .map(|c| c.volume)
                .ok_or_else(|| missing(channel)),
        }
    }

    fn set_pan(&mut self, channel: ChannelId, output: usize, pan: i8) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_pan(channel, output, pan);
        });
        Ok(())
    }

    fn pan(&self, channel: ChannelId, output: usize) -> Result<i8, Error> {
        match channel {
            ChannelId::Input(i) => self
                .scene
                .inputs
                .get(i)
                .and_then(|c| c.pans.get(output))
                .copied()
                .ok_or_else(|| missing(channel)),
            ChannelId::Playback(i) => self
                .scene
                .playbacks
                .get(i)
                .and_then(|c| c.pans.get(output))
                .copied()
                .ok_or_else(|| missing(channel)),
            ChannelId::Output(i) => self
                .scene
                .outputs
                .get(i)
                .map(|c| c.balance)
                .ok_or_else(|| missing(channel)),
        }
    }

    fn set_mute(&mut self, channel: ChannelId, mute: bool) -> Result<(), Error> {
        self.commands
            .send(Command::SetMute(channel, mute))
            .map_err(|_| stopped())?;
        Ok(())
    }

    fn mute(&self, channel: ChannelId) -> Result<bool, Error> {
        match channel {
            ChannelId::Input(i) => self.scene.inputs.get(i).map(|c| c.mute),
            ChannelId::Playback(i) => self.scene.playbacks.get(i).map(|c| c.mute),
            ChannelId::Output(i) => self.scene.outputs.get(i).map(|c| c.mute),
        }
        .ok_or_else(|| missing(channel))
    }

    fn set_solo(&mut self, channel: ChannelId, solo: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_solo(channel, solo);
        });
        Ok(())
    }

    fn solo(&self, channel: ChannelId) -> Result<bool, Error> {
        match channel {
            ChannelId::Input(i) => self.scene.inputs.get(i).map(|c| c.solo),
            ChannelId::Playback(i) => self.scene.playbacks.get(i).map(|c| c.solo),
            ChannelId::Output(i) => self.scene.outputs.get(i).map(|c| c.solo),
        }
        .ok_or_else(|| missing(channel))
    }

    fn capture_scene(&self) -> Scene {
        // Dim and Speaker B are live monitoring states, not mixer scene
        // state.  The service snapshot keeps them so the Control Room
        // buttons can latch, but a saved scene must not carry them: the
        // output fader values already include Dim's attenuation.
        let mut scene = self.scene.clone();
        scene.settings.dim = false;
        scene.settings.speaker_b = false;
        scene
    }

    fn apply_scene(&mut self, scene: &Scene) -> Result<(), Error> {
        self.scene = scene.clone();
        let scene = scene.clone();
        self.run(move |d| {
            let _ = d.apply_scene(&scene);
        });
        Ok(())
    }

    fn poll_events(&mut self) -> Result<(), Error> {
        // The service thread polls; the mirror has no transport.
        Ok(())
    }

    fn set_clock_source(&mut self, source: &str) -> Result<(), Error> {
        let source = source.to_string();
        self.run(move |d| {
            let _ = d.set_clock_source(&source);
        });
        Ok(())
    }

    fn set_sensitivity(&mut self, idx: usize, sensitivity: Sensitivity) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_sensitivity(idx, sensitivity);
        });
        Ok(())
    }

    fn set_pitch(&mut self, pitch_percent: f32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_pitch(pitch_percent);
        });
        Ok(())
    }

    // The remaining controls have no required trait body; forward the ones
    // the mixer uses.  Each one becomes a `Run` command on the device
    // thread, so the model snapshot is the single source of truth.
    fn set_output_linked(&mut self, pair: usize, linked: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_output_linked(pair, linked);
        });
        Ok(())
    }

    fn set_input_pair_linked(&mut self, pair: usize, linked: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_input_pair_linked(pair, linked);
        });
        Ok(())
    }

    fn set_playback_linked(&mut self, pair: usize, linked: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_playback_linked(pair, linked);
        });
        Ok(())
    }

    fn set_output_volume(
        &mut self,
        pair: usize,
        which: usize,
        volume: f32,
    ) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_output_volume(pair, which, volume);
        });
        Ok(())
    }

    fn set_output_mute(&mut self, pair: usize, which: usize, mute: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_output_mute(pair, which, mute);
        });
        Ok(())
    }

    fn set_output_solo(&mut self, pair: usize, which: usize, solo: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_output_solo(pair, which, solo);
        });
        Ok(())
    }

    fn set_input_volume(
        &mut self,
        pair: usize,
        which: usize,
        output: usize,
        volume: f32,
    ) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_input_volume(pair, which, output, volume);
        });
        Ok(())
    }

    fn set_input_mute(&mut self, pair: usize, which: usize, mute: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_input_mute(pair, which, mute);
        });
        Ok(())
    }

    fn set_input_solo(&mut self, pair: usize, which: usize, solo: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_input_solo(pair, which, solo);
        });
        Ok(())
    }

    fn set_playback_volume(
        &mut self,
        pair: usize,
        which: usize,
        output: usize,
        volume: f32,
    ) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_playback_volume(pair, which, output, volume);
        });
        Ok(())
    }

    fn set_playback_mute(&mut self, pair: usize, which: usize, mute: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_playback_mute(pair, which, mute);
        });
        Ok(())
    }

    fn set_playback_solo(&mut self, pair: usize, which: usize, solo: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_playback_solo(pair, which, solo);
        });
        Ok(())
    }

    fn set_phantom(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_phantom(idx, on);
        });
        Ok(())
    }

    fn set_pad(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_pad(idx, on);
        });
        Ok(())
    }

    fn set_gain(&mut self, idx: usize, gain: u32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_gain(idx, gain);
        });
        Ok(())
    }

    fn set_input_gain(&mut self, idx: usize, gain: u32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_input_gain(idx, gain);
        });
        Ok(())
    }

    fn set_input_gain_right(&mut self, idx: usize, gain: u32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_input_gain_right(idx, gain);
        });
        Ok(())
    }

    fn set_autoset(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_autoset(idx, on);
        });
        Ok(())
    }

    fn set_instrument(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_instrument(idx, on);
        });
        Ok(())
    }

    fn set_input_instrument(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_input_instrument(idx, on);
        });
        Ok(())
    }

    fn set_input_phantom(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_input_phantom(idx, on);
        });
        Ok(())
    }

    fn set_input_pad(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_input_pad(idx, on);
        });
        Ok(())
    }

    fn set_input_level(&mut self, idx: usize, level: InputLevel) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_input_level(idx, level);
        });
        Ok(())
    }

    fn set_ref_level(&mut self, idx: usize, raw: u16) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_ref_level(idx, raw);
        });
        Ok(())
    }

    fn set_stereo_split(&mut self, pb: usize, split: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_stereo_split(pb, split);
        });
        Ok(())
    }

    fn set_channel_fx_send(&mut self, channel: ChannelId, db: f32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_channel_fx_send(channel, db);
        });
        Ok(())
    }

    fn set_fx_send(&mut self, db: f32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_fx_send(db);
        });
        Ok(())
    }

    fn set_fx_return(&mut self, output: usize, db: f32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_fx_return(output, db);
        });
        Ok(())
    }

    fn set_trim(&mut self, channel: ChannelId, db: f32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_trim(channel, db);
        });
        Ok(())
    }

    fn set_phase(&mut self, idx: usize, invert: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_phase(idx, invert);
        });
        Ok(())
    }

    fn set_output_phase(&mut self, idx: usize, invert: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_output_phase(idx, invert);
        });
        Ok(())
    }

    fn set_no_trim(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_no_trim(idx, on);
        });
        Ok(())
    }

    fn set_width(&mut self, width: f32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_width(width);
        });
        Ok(())
    }

    fn set_channel_width(&mut self, channel: ChannelId, width: f32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_channel_width(channel, width);
        });
        Ok(())
    }

    fn set_ms_proc(&mut self, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_ms_proc(on);
        });
        Ok(())
    }



    fn set_an12(&mut self, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_an12(on);
        });
        Ok(())
    }

    fn set_input_link(&mut self, linked: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_input_link(linked);
        });
        Ok(())
    }

    fn set_eq_enabled(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_eq_enabled(idx, on);
        });
        Ok(())
    }

    fn set_eq_band_freq(&mut self, idx: usize, band: usize, freq: u16) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_eq_band_freq(idx, band, freq);
        });
        Ok(())
    }

    fn set_eq_band_q(&mut self, idx: usize, band: usize, q: f32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_eq_band_q(idx, band, q);
        });
        Ok(())
    }

    fn set_eq_band_gain(&mut self, idx: usize, band: usize, gain: f32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_eq_band_gain(idx, band, gain);
        });
        Ok(())
    }

    fn set_eq_band_type(
        &mut self,
        idx: usize,
        band: usize,
        kind: EqBandType,
    ) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_eq_band_type(idx, band, kind);
        });
        Ok(())
    }

    fn set_eq_low_cut_enabled(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_eq_low_cut_enabled(idx, on);
        });
        Ok(())
    }

    fn set_eq_low_cut_freq(&mut self, idx: usize, freq: u16) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_eq_low_cut_freq(idx, freq);
        });
        Ok(())
    }

    fn set_eq_low_cut_slope(&mut self, idx: usize, slope: u8) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_eq_low_cut_slope(idx, slope);
        });
        Ok(())
    }

    fn set_output_eq(&mut self, idx: usize, eq: &InputEq) -> Result<(), Error> {
        let eq = eq.clone();
        self.run(move |d| {
            let _ = d.set_output_eq(idx, &eq);
        });
        Ok(())
    }

    fn apply_input_eq(&mut self, idx: usize, eq: &InputEq) -> Result<(), Error> {
        let eq = eq.clone();
        self.run(move |d| {
            let _ = d.apply_input_eq(idx, &eq);
        });
        Ok(())
    }

    fn set_dynamics(&mut self, channel: ChannelId, dynamics: &Dynamics) -> Result<(), Error> {
        let dynamics = dynamics.clone();
        self.run(move |d| {
            let _ = d.set_dynamics(channel, &dynamics);
        });
        Ok(())
    }

    fn set_reverb(&mut self, reverb: &Reverb) -> Result<(), Error> {
        let reverb = reverb.clone();
        self.run(move |d| {
            let _ = d.set_reverb(&reverb);
        });
        Ok(())
    }

    fn set_echo(&mut self, echo: &Echo) -> Result<(), Error> {
        let echo = echo.clone();
        self.run(move |d| {
            let _ = d.set_echo(&echo);
        });
        Ok(())
    }

    fn set_sample_rate(&mut self, rate: u32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_sample_rate(rate);
        });
        Ok(())
    }

    fn set_optical_out_format(&mut self, spdif: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_optical_out_format(spdif);
        });
        Ok(())
    }




    fn set_eq_for_record(&mut self, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_eq_for_record(on);
        });
        Ok(())
    }

    fn set_loopback(&mut self, out: usize, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_loopback(out, on);
        });
        Ok(())
    }

    fn set_loopback_channel(&mut self, channel: usize, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_loopback_channel(channel, on);
        });
        Ok(())
    }

    fn set_cue(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_cue(idx, on);
        });
        Ok(())
    }

    fn set_dim(&mut self, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_dim(on);
        });
        Ok(())
    }

    fn set_dim_with(&mut self, on: bool, amount_db: f32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_dim_with(on, amount_db);
        });
        Ok(())
    }

    fn set_main_recall(&mut self, db: f32) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_main_recall(db);
        });
        Ok(())
    }

    fn set_main_out(&mut self, pair: Option<usize>) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_main_out(pair);
        });
        Ok(())
    }

    fn set_main_out_b(&mut self, pair: Option<usize>) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_main_out_b(pair);
        });
        Ok(())
    }

    fn set_mute_fx(&mut self, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_mute_fx(on);
        });
        Ok(())
    }

    fn set_external_input(
        &mut self,
        on: bool,
        source: Option<usize>,
        gain_db: f32,
    ) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_external_input(on, source, gain_db);
        });
        Ok(())
    }

    fn set_cue_target(&mut self, pair: Option<usize>) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_cue_target(pair);
        });
        Ok(())
    }

    fn set_pfl(&mut self, channel: ChannelId, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_pfl(channel, on);
        });
        Ok(())
    }

    fn set_pfl_mode(&mut self, pfl: bool, exclusive: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_pfl_mode(pfl, exclusive);
        });
        Ok(())
    }

    fn set_speaker_b(&mut self, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_speaker_b(on);
        });
        Ok(())
    }

    fn set_mono(&mut self, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_mono(on);
        });
        Ok(())
    }

    fn set_speaker_b_linked(&mut self, on: bool) -> Result<(), Error> {
        self.run(move |d| {
            let _ = d.set_speaker_b_linked(on);
        });
        Ok(())
    }
}

fn missing(channel: ChannelId) -> Error {
    Error::InvalidChannel(format!("{channel:?} is not in the mirror"))
}

fn stopped() -> Error {
    Error::InvalidChannel("the device service has stopped".into())
}

/// Meter values written by the service and read by the fader canvases.
///
/// `None` for `at` means no meter packet has arrived yet.  The canvases use
/// `at` to interpolate between snapshots without a UI message.
#[derive(Clone, Copy, Debug, Default)]
pub struct MeterSnapshot {
    pub inputs: [f32; 18],
    pub playbacks: [f32; 18],
    pub outputs: [f32; 18],
    /// Peak-hold values for the same channels, held for two seconds and
    /// then falling at 20 dB/s while the service polls at the device
    /// rate.  A canvas may only draw 30 frames a second; these catch
    /// transients that fall between those frames.
    pub input_peaks: [f32; 18],
    pub playback_peaks: [f32; 18],
    pub output_peaks: [f32; 18],
    pub fx: [f32; 2],
    pub fx_overload: [bool; 2],
    /// Driver transport health, when the backend exposes it.
    pub counters: Option<HwdepCounters>,
    /// Whether anything is visibly moving.  The service derives this from
    /// the levels, the held peaks and the FX values; a canvas that sees
    /// `false` stops scheduling frames and waits for [`meter_stream`].
    pub active: bool,
    pub at: Option<Instant>,
}

/// A level or held peak above this keeps the meter canvases animating.
/// The visible meter floor is -60 dB (0.001); the threshold sits a little
/// below it so a decaying peak stops shortly after it leaves the scale.
const METER_ACTIVE_FLOOR: f32 = 0.0005;

/// Whether the meter snapshot has anything worth drawing.  `at` is not a
/// change signal: it advances every device packet even in silence.
fn meter_activity(m: &MeterSnapshot) -> bool {
    let hot = |values: &[f32]| values.iter().any(|v| *v > METER_ACTIVE_FLOOR);
    hot(&m.inputs)
        || hot(&m.playbacks)
        || hot(&m.outputs)
        || hot(&m.input_peaks)
        || hot(&m.playback_peaks)
        || hot(&m.output_peaks)
        || hot(&m.fx)
}

/// A running device service.
pub struct DeviceService {
    commands: Sender<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
    meters: Arc<Mutex<MeterSnapshot>>,
    updates: Option<Receiver<Snapshot>>,
    join: Option<JoinHandle<()>>,
}

/// Whether the device service owns the device.  The running application
/// defaults to it; `TOTALCONTROL_DEVICE_SERVICE=0` selects the old direct path.
/// Unit tests stay on the direct path unless they call
/// `attach_device_service` themselves, so the suite does not spawn a
/// service thread per case.
pub fn requested() -> bool {
    match std::env::var_os("TOTALCONTROL_DEVICE_SERVICE") {
        Some(value) => value != "0" && value != "false",
        None => !cfg!(test),
    }
}

impl DeviceService {
    /// Take ownership of `device` and start the service thread.
    pub fn spawn(mut device: DeviceHandle) -> Self {
        let initial = live_scene(&device);
        let info = capture_info(&device);
        let snapshot = Arc::new(Mutex::new(Snapshot {
            scene: initial,
            info,
            connected: device.is_connected(),
            hardware_volume: device.hardware_volume(),
            panel_selection: device.panel_selection(),
            at: Instant::now(),
        }));
        let meters = Arc::new(Mutex::new(MeterSnapshot::default()));
        let (commands, rx) = mpsc::channel();
        let (updates_tx, updates_rx) = mpsc::channel();

        let thread_snapshot = Arc::clone(&snapshot);
        let thread_meters = Arc::clone(&meters);
        let join = thread::Builder::new()
            .name("totalcontrol-device-service".into())
            .spawn(move || {
                run(
                    &mut device,
                    rx,
                    thread_snapshot,
                    thread_meters,
                    updates_tx,
                );
            })
            .expect("starting the device service");

        Self {
            commands,
            snapshot,
            meters,
            updates: Some(updates_rx),
            join: Some(join),
        }
    }

    /// The current model snapshot.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap().clone()
    }

    /// Take the update receiver; the UI drains it and applies the snapshots
    /// to its `Mirror`.
    pub fn take_updates(&mut self) -> Option<Receiver<Snapshot>> {
        self.updates.take()
    }

    /// A clone of the command sender, for a `Mirror`.
    pub fn command_sender(&self) -> Sender<Command> {
        self.commands.clone()
    }

    /// The shared meter bus, for a `Mirror` and the fader canvases.
    pub fn meter_bus(&self) -> Arc<Mutex<MeterSnapshot>> {
        Arc::clone(&self.meters)
    }

    /// The current meter snapshot.
    pub fn meters(&self) -> MeterSnapshot {
        *self.meters.lock().unwrap()
    }

    /// Queue a command.  Returns false once the service has stopped.
    pub fn send(&self, command: Command) -> bool {
        self.commands.send(command).is_ok()
    }

    /// Ask the thread to stop and wait for it.
    pub fn stop(mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for DeviceService {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Capture the live model for the service snapshot.  `capture_scene`
/// deliberately strips runtime monitor states for persistence; the UI
/// needs those states to latch the Control Room buttons.
fn live_scene(device: &DeviceHandle) -> Scene {
    let mut scene = device.capture_scene();
    scene.settings.dim = device.settings().dim;
    scene.settings.speaker_b = device.settings().speaker_b;
    scene
}

fn capture_info(device: &DeviceHandle) -> DeviceInfo {
    DeviceInfo {
        model_name: device.model_name().to_string(),
        canonical_model: device.canonical_model().to_string(),
        capabilities: device.capabilities(),
        protocol_features: device.protocol_features(),
        low_cut_supported: device.low_cut_supported(),
        phase_supported: device.phase_supported(),
        output_balance_supported: device.output_balance_supported(),
        loopback_supported: device.loopback_supported(),
        stereo_split_supported: device.stereo_split_supported(),
        matrix_uses_display_order: device.matrix_uses_display_order(),
        output_pair_count: device.output_pair_count(),
        output_display_order: device.output_display_order(),
        is_uc: device.is_uc_model(),
        is_ucx: device.is_ucx_model(),
        is_uc_family: device.is_uc_family(),
        is_mock: device.is_mock(),
        kind: device.kind(),
        target: device.target(),
        card_id: device.card_id(),
        firmware: device.firmware(),
        serial: device.serial().map(str::to_owned),
        rate_follows: device.rate_follows_sound_server(),
        has_sensitivity: device.has_sensitivity_control(),
        outputs_one_per_pair: device.outputs_one_per_pair(),
        input_meters: (0..device.inputs().len())
            .map(|i| device.has_input_meter(i))
            .collect(),
        device_playback_meters: device.has_device_playback_meters(),
    }
}

fn run(
    device: &mut DeviceHandle,
    rx: Receiver<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
    meters: Arc<Mutex<MeterSnapshot>>,
    updates: Sender<Snapshot>,
) {
    let mut last_scene = Instant::now();
    let mut last_connected = device.is_connected();
    let mut last_model = snapshot.lock().unwrap().scene.clone();
    let mut last_hw_volume = device.hardware_volume();
    let mut last_panel_selection = device.panel_selection();
    let mut last_meter_active = false;
    let mut last_overload = [false; 2];
    let mut last_counters = Instant::now() - Duration::from_secs(1);
    let mut peak_at = [[Instant::now(); 18]; 3];

    loop {
        // The timeout paces the polling loop at about 120 Hz while still
        // waking immediately for a command.
        match rx.recv_timeout(Duration::from_millis(8)) {
            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(command) => apply(device, command),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        while let Ok(command) = rx.try_recv() {
            if matches!(command, Command::Stop) {
                return;
            }
            apply(device, command);
        }

        // State/status events feed the model and the front panel.
        let _ = device.poll_events();

        // Meters are published independently of the model snapshot and of
        // the UI's view tick.
        {
            let now = Instant::now();
            let inputs = device.input_meters();
            let playbacks = device
                .device_playback_meters()
                .unwrap_or_else(|| device.playback_meters());
            let mut outputs = device.device_output_meters().unwrap_or_default();
            let input_peaks = device.input_peaks().unwrap_or_else(|| inputs.clone());
            let playback_peaks = device
                .device_playback_peaks()
                .unwrap_or_else(|| playbacks.clone());
            let mut output_peaks = device
                .device_output_peaks()
                .unwrap_or_else(|| outputs.clone());
            // A muted output reads zero even if a backend's post-fader
            // packet is taken before its mute stage, preserving the
            // direct-path display.
            for (i, channel) in device.outputs().iter().enumerate() {
                if channel.mute {
                    if let Some(level) = outputs.get_mut(i) {
                        *level = 0.0;
                    }
                    if let Some(peak) = output_peaks.get_mut(i) {
                        *peak = 0.0;
                    }
                }
            }
            let fx = device.fx_meters().unwrap_or([0.0, 0.0]);
            let overload = device.fx_overload().unwrap_or([false, false]);
            let mut m = meters.lock().unwrap();
            for (slot, value) in m.inputs.iter_mut().zip(inputs) {
                *slot = value;
            }
            for (slot, value) in m.playbacks.iter_mut().zip(playbacks) {
                *slot = value;
            }
            for (slot, value) in m.outputs.iter_mut().zip(outputs) {
                *slot = value;
            }
            hold_peaks(&mut m.input_peaks, &input_peaks, &mut peak_at[0], now);
            hold_peaks(&mut m.playback_peaks, &playback_peaks, &mut peak_at[1], now);
            hold_peaks(&mut m.output_peaks, &output_peaks, &mut peak_at[2], now);
            m.fx = fx;
            m.fx_overload = overload;
            m.at = Some(now);
            // The transport counters are cumulative and change slowly; a
            // quarter-second poll is enough for the device sheet.
            if last_counters.elapsed() >= Duration::from_millis(250) {
                last_counters = Instant::now();
                m.counters = device.transport_counters();
            }
            let active = meter_activity(&m);
            m.active = active;
            drop(m);
            // Only edges wake the UI: while a burst is running the canvases
            // schedule their own frames.  A latched overload flag must not
            // keep the loop alive, so overload changes are their own edge.
            let overload_changed = overload != last_overload;
            if (active && !last_meter_active) || overload_changed {
                meter_notify().notify_one();
            }
            last_meter_active = active;
            last_overload = overload;
        }

        // The model changes rarely (a control command, a front-panel move,
        // a gain readback).  A 20 Hz capture is plenty for the UI.
        if last_scene.elapsed() >= Duration::from_millis(50) {
            last_scene = Instant::now();
            let connected = device.is_connected();
            let scene = live_scene(device);
            let hardware_volume = device.hardware_volume();
            let panel_selection = device.panel_selection();
            if scene != last_model
                || connected != last_connected
                || hardware_volume != last_hw_volume
                || panel_selection != last_panel_selection
            {
                last_model = scene.clone();
                last_connected = connected;
                last_hw_volume = hardware_volume;
                last_panel_selection = panel_selection;
                let update = {
                    let mut s = snapshot.lock().unwrap();
                    s.scene = scene;
                    s.connected = connected;
                    s.hardware_volume = hardware_volume;
                    s.panel_selection = panel_selection;
                    s.at = Instant::now();
                    s.clone()
                };
                let _ = updates.send(update);
                snapshot_notify().notify_one();
            }
        }
    }
}

/// Hold each channel's peak for two seconds, then fall at 20 dB/s.  The
/// service sees every device packet, so a transient shorter than a canvas
/// frame still reaches the needle.
fn hold_peaks(
    peaks: &mut [f32; 18],
    fresh: &[f32],
    peak_at: &mut [Instant; 18],
    now: Instant,
) {
    for (i, slot) in peaks.iter_mut().enumerate() {
        let value = fresh.get(i).copied().unwrap_or(0.0).max(0.0);
        if value >= *slot {
            *slot = value;
            peak_at[i] = now;
        } else {
            let held = now.duration_since(peak_at[i]).as_secs_f32();
            if held > 2.0 {
                let decayed = *slot * 10f32.powf(-(held - 2.0));
                *slot = decayed.max(value);
            }
        }
    }
}

fn apply(device: &mut DeviceHandle, command: Command) {
    match command {
        Command::SetVolume(channel, output, value) => {
            let _ = device.set_volume(channel, output, value);
        }
        Command::SetMute(channel, mute) => {
            let _ = device.set_mute(channel, mute);
        }
        Command::ApplyScene(scene) => {
            let _ = device.apply_scene(&scene);
        }
        Command::Run(f) => f(device),
        Command::Stop => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, DeviceService};
    use crate::app::DeviceHandle;
    use std::thread;
    use std::time::{Duration, Instant};
    use totalcontrol_core::{ChannelId, MockBabyfacePro, RmeDevice};

    fn mock() -> DeviceHandle {
        DeviceHandle::Mock(MockBabyfacePro::open_ucx().expect("mock opens"))
    }

    fn wait_for(mut check: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if check() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn the_service_publishes_a_snapshot_and_applies_commands() {
        let service = DeviceService::spawn(mock());

        assert!(
            wait_for(|| !service.snapshot().scene.outputs.is_empty()),
            "the initial snapshot arrives"
        );
        assert!(service.snapshot().connected);

        assert!(service.send(Command::SetVolume(ChannelId::Output(0), 0, 0.25)));
        assert!(
            wait_for(|| (service.snapshot().scene.outputs[0].volume - 0.25).abs() < 0.001),
            "the model snapshot follows the volume command"
        );

        assert!(service.send(Command::SetMute(ChannelId::Input(0), true)));
        assert!(
            wait_for(|| service.snapshot().scene.inputs[0].mute),
            "the model snapshot follows the mute command"
        );

        service.stop();
    }

    #[test]
    fn meters_are_published_without_the_model_changing() {
        let service = DeviceService::spawn(mock());
        assert!(
            wait_for(|| service.meters().at.is_some()),
            "the meter snapshot is written"
        );
        assert!(service.meters().inputs.iter().any(|v| *v >= 0.0));
        service.stop();
    }

    #[test]
    fn the_mirror_forwards_the_captured_capabilities() {
        let device = mock();
        let caps = device.capabilities();
        let order = device.output_display_order();
        let model = device.canonical_model().to_string();
        let service = DeviceService::spawn(device);
        let info = service.snapshot().info;
        assert_eq!(info.capabilities, caps);
        assert!(info.capabilities.fx, "the UCX mock has an FX bus");
        assert_eq!(info.output_display_order, order);
        assert_eq!(info.canonical_model, model);
        service.stop();
    }

    #[test]
    fn a_live_signal_marks_the_meter_bus_active() {
        let service = DeviceService::spawn(mock());
        assert!(
            wait_for(|| service.meters().active),
            "the mock's live levels mark the bus active"
        );
        service.stop();
    }

    #[test]
    fn meter_activity_ignores_silence_and_follows_levels_peaks_and_overload() {
        let mut m = super::MeterSnapshot::default();
        assert!(!super::meter_activity(&m), "silence is idle");
        m.inputs[0] = super::METER_ACTIVE_FLOOR * 0.5;
        assert!(!super::meter_activity(&m), "below the floor is idle");
        m.inputs[0] = super::METER_ACTIVE_FLOOR * 2.0;
        assert!(super::meter_activity(&m), "a level wakes the bus");
        m.inputs[0] = 0.0;
        m.output_peaks[17] = 0.01;
        assert!(super::meter_activity(&m), "a held peak keeps it awake");
        m.output_peaks[17] = 0.0;
        m.fx_overload[1] = true;
        assert!(
            !super::meter_activity(&m),
            "a latched overload is a static indicator, not ongoing movement"
        );
    }

    #[test]
    fn peak_hold_keeps_a_transient_and_then_falls() {
        let start = Instant::now();
        let mut peaks = [0.0; 18];
        let mut at = [start; 18];
        super::hold_peaks(&mut peaks, &[0.8], &mut at, start);
        assert!((peaks[0] - 0.8).abs() < 1e-6);

        // The bar may be at zero between frames; the peak is still held.
        super::hold_peaks(&mut peaks, &[0.0], &mut at, start + Duration::from_millis(900));
        assert!((peaks[0] - 0.8).abs() < 1e-6);

        // Still held at 1.9 s: the default TotalMix hold is two seconds.
        super::hold_peaks(&mut peaks, &[0.0], &mut at, start + Duration::from_millis(1900));
        assert!((peaks[0] - 0.8).abs() < 1e-6);

        // After the two-second hold it falls at 20 dB/s (a factor of ten
        // per second).
        super::hold_peaks(&mut peaks, &[0.0], &mut at, start + Duration::from_millis(2900));
        assert!((peaks[0] - 0.8 * 0.1f32.powf(0.9)).abs() < 1e-4);
    }

    #[test]
    fn runtime_dim_latches_in_the_mirror_but_not_the_saved_scene() {
        let mut service = DeviceService::spawn(mock());
        let updates = service.take_updates().expect("the update receiver");
        let mut mirror = super::Mirror::new(
            service.snapshot(),
            service.command_sender(),
            service.meter_bus(),
        );

        assert!(!mirror.settings().dim);
        assert!(mirror.set_dim(true).is_ok());
        assert!(
            wait_for(|| {
                while let Ok(snapshot) = updates.try_recv() {
                    mirror.update(snapshot);
                }
                mirror.settings().dim
            }),
            "the live Dim flag reaches the mirror so the button can latch"
        );
        assert!(
            !mirror.capture_scene().settings.dim,
            "the saved scene must not carry the runtime Dim flag"
        );

        assert!(mirror.set_dim(false).is_ok());
        assert!(
            wait_for(|| {
                while let Ok(snapshot) = updates.try_recv() {
                    mirror.update(snapshot);
                }
                !mirror.settings().dim
            }),
            "turning Dim off clears the live flag"
        );
        service.stop();
    }

    #[test]
    fn a_mirror_applies_updates_and_sends_commands() {
        let mut service = DeviceService::spawn(mock());
        let updates = service.take_updates().expect("the update receiver");
        let mut mirror = super::Mirror::new(
            service.snapshot(),
            service.command_sender(),
            service.meter_bus(),
        );
        assert!(!mirror.outputs().is_empty());
        assert_eq!(mirror.info().output_pair_count, mirror.outputs().len() / 2);

        assert!(mirror.set_volume(ChannelId::Output(0), 0, 0.25));
        assert!(
            wait_for(|| {
                while let Ok(snapshot) = updates.try_recv() {
                    mirror.update(snapshot);
                }
                (mirror.outputs()[0].volume - 0.25).abs() < 0.001
            }),
            "the mirror follows the service snapshot"
        );

        // A trait setter that goes through `Command::Run`.
        assert!(mirror.set_eq_band_gain(0, 0, -3.0).is_ok());
        assert!(
            wait_for(|| {
                while let Ok(snapshot) = updates.try_recv() {
                    mirror.update(snapshot);
                }
                mirror.inputs()[0]
                    .eq
                    .is_some_and(|eq| (eq.bands[0].gain_db + 3.0).abs() < 0.01)
            }),
            "a mirror setter reaches the service and comes back in the snapshot"
        );
        service.stop();
    }
}

