//! Fireface UC/UCX ALSA backend for TuxMix.
//!
//! **Naming note:** the module/type name is historical. It is the shared
//! backend for both the active Fireface UC and the parked UCX; it is not a
//! UCX-only implementation. Keep device-specific protocol and feature rules
//! out of the common path; see `docs/DEVICE-PROFILES.md`.
//!
//! The kernel driver exposes the mixer as ALSA simple-mixer elements:
//!
//! - `Output N Mix`, index `source` — matrix crosspoint (0..8192 = 0 dB)
//! - `Output N Master`, `Output N Mute`, `Output N Loopback`
//! - `Input N Gain`, `Input N Phantom`, `Input 3/4 Pad/Instrument`
//! - `Clock Source`, `Optical Output Format`, `SPDIF Output Format`,
//!   `Sample Rate`, ...
//!
//! The hardware **output** pair order is Phones-first (confirmed on
//! hardware); the **software playback** row follows TotalMix (AN1/2,
//! AN3/4, AN5/6, PH 7/8, SPDIF, AS 1/2, ADAT 3/4…7/8); the input order is
//! Analog 1–8, SPDIF, ADAT.
//!
//! This backend intentionally implements only the v1 surface (routing, gains,
//! mutes, phantom/PAD, clock).  EQ/FX/phase/trim are not exposed by the
//! current driver, so [`RmeDevice::protocol_features`] is false and the UI
//! does not offer them.  Input metering is pre-fader and comes from the
//! device's IF1 `0x85` meter packet, falling back to the IF5 capture PCM.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use alsa::ctl::{ElemId, ElemIface};
use alsa::hctl::{Elem, HCtl};
use alsa::mixer::{Selem, SelemChannelId};
use log::info;

use crate::channel::{ChannelId, InputChannel, OutputChannel, PlaybackChannel};
use crate::device::{DeviceSettings, RmeDevice};
use crate::error::Error;
use crate::mixer::AlsaMixer;
use crate::profile::DeviceProfile;
use crate::profiles::uc;
use crate::profiles::ucx::PROFILE;
use crate::scene::Scene;

/// Output pairs: Phones, AN1/2, AN3/4, AN5/6, SPDIF, ADAT 1/2 … 7/8.
const PAIRS: usize = 9;
/// Mono output blocks (2 per pair).
const BLOCKS: usize = 18;
/// Matrix sources (18 inputs + 18 playbacks).
const SOURCES: usize = 36;
/// 0 dB in the driver's raw matrix/master scale.
const UNITY: i64 = 0x2000;

fn mono() -> SelemChannelId {
    SelemChannelId::mono()
}

fn raw_volume(volume: f32) -> i64 {
    (volume.clamp(0.0, 1.0) * UNITY as f32).round() as i64
}

/// Fader linear amplitude -> the UCX's 0.1 dB hardware-volume value.
/// 0.0 is -infinity, represented by the bottom of the control.
fn linear_to_db10(volume: f32) -> i32 {
    let v = volume.clamp(0.0, 1.0);
    if v <= 0.0 {
        -650
    } else {
        (200.0 * v.log10()).round().clamp(-650.0, 60.0) as i32
    }
}

/// The UCX's 0.1 dB hardware-volume value -> linear amplitude.
fn db10_to_linear(db: i32) -> f32 {
    if db <= -650 {
        0.0
    } else {
        10f32.powf((db as f32 / 10.0) / 20.0)
    }
}

/// Read the driver's read-only `Sample Rate` control.  The meter must open
/// the capture PCM at the rate the unit is actually running; hardcoding
/// 48 kHz would leave the meters dead after any rate change.
fn read_sample_rate(hctl: &HCtl) -> Option<u32> {
    let mut id = ElemId::new(ElemIface::Mixer);
    let name = CString::new("Sample Rate").ok()?;
    id.set_name(name.as_c_str());
    id.set_index(0);
    let elem = hctl.find_elem(&id)?;
    let value = elem.read().ok()?;
    u32::try_from(value.get_integer(0)?).ok()
}

/// The driver's IF1 0x85 meter packet: 61 32-bit words.
///
/// Windows reference layout: words 0..35 are the 18 hardware input meters
/// (even = level, odd = peak/hold; AN1/2 = 0/2, AN5 = 8), words 36..53 are
/// the output bank, and word 60 is a rolling status counter.  Do not treat
/// the input bank as software playback: doing so makes an idle playback
/// strip show the live analog input.
const DEVICE_METER_WORDS: usize = 61;

/// The 0x85 meter words are **full 32-bit linear levels**; bit 31 is the
/// sign/clip flag.  Verified with input 1: gain 0 reads ≈0.50e6 (-73 dBFS)
/// and gain 65 reads ≈0.4–1.5e9 (-14…-3 dBFS).  Input 2 at 33 dB reads
/// ≈16e6 (-42 dBFS), and inputs 3–8 read ≈0.4e6 (-74 dBFS).  A 24-bit
/// scale made the noise floor look like signal and clipped input 1.
fn meter_norm(raw: u32) -> f32 {
    ((raw & 0x7fff_ffff) as f32 / 2_147_483_648.0).clamp(0.0, 1.0)
}

/// Matrix source indices on both UC and UCX: hardware inputs 0..17,
/// software playback 18..35.
fn matrix_input_source(_is_uc: bool, idx: usize) -> usize {
    idx
}

fn matrix_playback_source(_is_uc: bool, dev: usize) -> usize {
    18 + dev
}

/// Internal output channel -> matrix display channel for the UC.
fn matrix_display_channel(order: &[usize], idx: usize) -> usize {
    let pair = idx / 2;
    let side = idx & 1;
    order.iter().position(|&p| p == pair).unwrap_or(pair) * 2 + side
}

/// Split a mono input's volume across a stereo output pair.
fn pan_split(volume: f32, pan: i8) -> (f32, f32) {
    let v = volume.clamp(0.0, 1.0);
    let p = pan.clamp(-100, 100) as f32 / 100.0;
    let l = v * if p > 0.0 { 1.0 - p } else { 1.0 };
    let r = v * if p < 0.0 { 1.0 + p } else { 1.0 };
    (l, r)
}

/// Recover an approximate pan from a pair of crosspoint values.
fn pan_from_raw(l: i64, r: i64) -> i8 {
    let l = l.max(0) as f32;
    let r = r.max(0) as f32;
    let sum = l + r;
    if sum < 0.5 {
        0
    } else {
        (((r - l) / sum) * 100.0)
            .round()
            .clamp(-100.0, 100.0) as i8
    }
}

/// Shared ALSA-backed implementation for the Fireface UC and UCX.
///
/// The historical name is kept for API stability. Device-specific protocol
/// behavior must be selected from `profile`/the card and must not assume UCX
/// register maps, `0x0c` bulk words or UCX meter layouts.
pub struct UcxAlsa {
    /// Which RME model/profile this backend is driving.
    profile: &'static DeviceProfile,
    mixer: AlsaMixer,
    /// Raw control handle for the 36-channel matrix.  The simple-mixer API
    /// only names nine channels, so it cannot address sources 9–35.
    hctl: HCtl,
    /// Pre-fader input metering: the driver's capture PCM carries the raw
    /// input stream, so its levels are independent of the matrix/faders.
    capture_meter: Option<crate::capture_meter::CaptureMeter>,
    /// Control-only Pro Audio mode: input meters from PipeWire's UC source
    /// (fan-out, no exclusive ALSA capture), not the ALSA capture PCM.
    input_tap: Option<crate::pw_input::InputTap>,
    /// Physical Phones hardware volume, linear 0..1, read from the driver's
    /// IF1 register reader. `None` until the first valid state packet.
    hw_volume: Option<f32>,
    inputs: Vec<InputChannel>,
    playbacks: Vec<PlaybackChannel>,
    outputs: Vec<OutputChannel>,
    settings: DeviceSettings,
    muted: HashSet<ChannelId>,
    soloed: HashSet<ChannelId>,
    /// Saved raw crosspoints (one per output block) while a channel is muted.
    mute_saved: HashMap<ChannelId, Vec<i64>>,
    /// Saved (hardware-volume dB, matrix master) for UC output mute.
    output_mute_saved: HashMap<usize, (i32, i64)>,
    /// Throttle for readback of the output masters, so a turn of the
    /// physical encoder moves the GUI's Phones slider instead of leaving
    /// it at the value read when the app opened.
    last_output_poll: Instant,
    /// Throttle for the preamp readback: the front panel changes `Input N
    /// Gain` too, so the GUI must re-read the ALSA cache, not only update it
    /// when the user turns the GUI knob.
    last_preamp_poll: Instant,
    /// Last non-silent 0x85 input level per channel and when it was seen.
    /// The device interleaves silent/alternate meter pages, so hold the
    /// last real level briefly instead of dropping the VU to zero on every
    /// page.
    input_hold: Mutex<Vec<(f32, Instant)>>,
}

impl UcxAlsa {
    fn find(&self, name: &str, index: u32) -> Option<Selem<'_>> {
        self.mixer.find_selem(name, index)
    }

    /// Read the driver's "Device Meters" control (61 words from IF1 0x85).
    fn read_device_meters(&self) -> Option<Vec<u32>> {
        let mut id = ElemId::new(ElemIface::Mixer);
        let name = CString::new("Device Meters").ok()?;
        id.set_name(name.as_c_str());
        id.set_index(0);
        let elem = self.hctl.find_elem(&id)?;
        let value = elem.read().ok()?;
        let mut out = Vec::with_capacity(DEVICE_METER_WORDS);
        for i in 0..DEVICE_METER_WORDS {
            out.push(u32::try_from(value.get_integer(i as u32)?).ok()?);
        }
        Some(out)
    }

    /// Driver-computed peaks of the IF5 OUT stream: the host software
    /// playback channels in device slot order.  The UCX 0x85 meter packet
    /// contains hardware inputs only, so the kernel driver measures the
    /// audio it sends and exposes it as "Playback Meters".
    fn read_playback_meters(&self) -> Option<Vec<u32>> {
        let mut id = ElemId::new(ElemIface::Mixer);
        let name = CString::new("Playback Meters").ok()?;
        id.set_name(name.as_c_str());
        id.set_index(0);
        let elem = self.hctl.find_elem(&id)?;
        let value = elem.read().ok()?;
        let mut out = Vec::with_capacity(18);
        for i in 0..18 {
            out.push(u32::try_from(value.get_integer(i as u32)?).ok()?);
        }
        Some(out)
    }

    pub fn device_playback_meters(&self) -> Option<Vec<f32>> {
        let w = self.read_playback_meters()?;
        let mut out = vec![0.0; self.playbacks.len()];
        for (i, level) in out.iter_mut().enumerate() {
            if let Some(&raw) = w.get(i) {
                *level = meter_norm(raw);
            }
        }
        Some(out)
    }

    // The 0x85 packet does not contain hardware-output meters.  Output VUs
    // are computed from the matrix plus the device input meters and the
    // driver's IF5 OUT peaks (see the GUI's `output_meters_from`).

    fn read_switch(&self, name: &str) -> Option<bool> {
        let s = self.find(name, 0)?;
        s.get_playback_switch(mono()).ok().map(|v| v != 0)
    }

    fn write_switch(&self, name: &str, on: bool) -> Result<(), Error> {
        if let Some(s) = self.find(name, 0) {
            s.set_playback_switch(mono(), if on { 1 } else { 0 })?;
        }
        Ok(())
    }

    fn read_vol(&self, name: &str, index: u32) -> Option<i64> {
        let s = self.find(name, index)?;
        s.get_playback_volume(mono()).ok()
    }

    fn write_vol(&self, name: &str, index: u32, raw: i64) -> Result<(), Error> {
        if let Some(s) = self.find(name, index) {
            s.set_playback_volume(mono(), raw)?;
        }
        Ok(())
    }

    fn read_int_ctl(&self, name: &str) -> Option<i32> {
        let mut id = ElemId::new(ElemIface::Mixer);
        let cname = CString::new(name).ok()?;
        id.set_name(cname.as_c_str());
        id.set_index(0);
        let elem = self.hctl.find_elem(&id)?;
        let value = elem.read().ok()?;
        value.get_integer(0)
    }

    fn write_int_ctl(&self, name: &str, value: i32) -> Result<(), Error> {
        let mut id = ElemId::new(ElemIface::Mixer);
        let cname = CString::new(name)
            .map_err(|_| Error::MixerElementNotFound(name.to_string()))?;
        id.set_name(cname.as_c_str());
        id.set_index(0);
        let elem = self
            .hctl
            .find_elem(&id)
            .ok_or_else(|| Error::MixerElementNotFound(name.to_string()))?;
        let mut v = elem.read()?;
        v.set_integer(0, value)
            .ok_or_else(|| Error::InvalidChannel(name.to_string()))?;
        elem.write(&v)?;
        Ok(())
    }

    /// The physical Phones volume is exposed by the driver in 0.1 dB units
    /// on the first output's two hardware-volume controls.  Convert the
    /// average to a linear 0..1 value for the TuxMix fader (`db_text`
    /// turns it back into dB).
    fn read_hw_volume(&self) -> Option<f32> {
        let l = self.read_int_ctl("Output 1 Hardware Volume")?;
        let r = self.read_int_ctl("Output 2 Hardware Volume")?;
        let db = (l + r) as f32 / 20.0; /* 0.1 dB -> average dB */
        Some(10f32.powf(db / 20.0))
    }

    pub fn hardware_volume(&self) -> Option<f32> {
        self.hw_volume
    }

    fn row_elem(&self, block: usize) -> Option<Elem<'_>> {
        let mut id = ElemId::new(ElemIface::Mixer);
        let name = CString::new(format!("Output {} Mix Volume", block + 1)).ok()?;
        id.set_name(name.as_c_str());
        id.set_index(0);
        self.hctl.find_elem(&id)
    }

    fn read_crosspoint(&self, block: usize, source: usize) -> Option<i64> {
        let elem = self.row_elem(block)?;
        let value = elem.read().ok()?;
        value.get_integer(source as u32).map(|v| v as i64)
    }

    fn write_crosspoint(&self, block: usize, source: usize, raw: i64) -> Result<(), Error> {
        let elem = self.row_elem(block).ok_or_else(|| {
            Error::MixerElementNotFound(format!("Output {} Mix Volume", block + 1))
        })?;
        let mut value = elem.read()?;
        value
            .set_integer(source as u32, raw as i32)
            .ok_or_else(|| Error::InvalidChannel(format!("matrix source {source}")))?;
        elem.write(&value)?;
        Ok(())
    }

    /// Write an entire UC matrix output row in one ALSA control update.
    /// Individual read-modify-write calls can race with another source's
    /// last update; this makes the restored scene the final row value.
    fn write_uc_matrix_row(&self, block: usize) -> Result<(), Error> {
        let pair = block / 2;
        let right = block & 1;
        let mut vals = vec![0i64; SOURCES];
        for idx in 0..self.inputs.len() {
            let (l, r) = pan_split(
                self.inputs[idx].volumes[pair],
                self.inputs[idx].pans[pair],
            );
            vals[idx] = raw_volume(if right == 0 { l } else { r });
        }
        for idx in 0..self.playbacks.len() {
            let dev = self.playbacks[idx].id;
            if pair * 2 + (dev % 2) == block {
                vals[18 + dev] = raw_volume(self.playbacks[idx].volumes[pair]);
            }
        }
        let elem = self.row_elem(block).ok_or_else(|| {
            Error::MixerElementNotFound(format!("Output {} Mix Volume", block + 1))
        })?;
        let mut value = elem.read()?;
        for (src, raw) in vals.iter().enumerate() {
            value
                .set_integer(src as u32, *raw as i32)
                .ok_or_else(|| Error::InvalidChannel(format!("matrix source {src}")))?;
        }
        elem.write(&value)?;
        Ok(())
    }

    fn read_enum(&self, name: &str) -> Option<(u32, Vec<String>)> {
        let s = self.find(name, 0)?;
        let current = s.get_enum_item(mono()).ok()?;
        let count = s.get_enum_items().ok()?;
        let names = (0..count)
            .filter_map(|i| s.get_enum_item_name(i).ok())
            .collect();
        Some((current, names))
    }

    fn write_enum(&self, name: &str, item: u32) -> Result<(), Error> {
        if let Some(s) = self.find(name, 0) {
            s.set_enum_item(mono(), item)?;
        }
        Ok(())
    }

    fn refresh_outputs(&mut self) {
        for b in 0..BLOCKS {
            if !(self.is_uc() && self.outputs[b].mute) {
                if let Some(db) =
                    self.read_int_ctl(&format!("Output {} Hardware Volume", b + 1))
                {
                    self.outputs[b].volume = db10_to_linear(db);
                } else if let Some(raw) =
                    self.read_vol(&format!("Output {} Master", b + 1), 0)
                {
                    self.outputs[b].volume = raw as f32 / UNITY as f32;
                }
            }
            if !self.is_uc() {
                self.outputs[b].mute =
                    self.read_switch(&format!("Output {} Mute", b + 1)).unwrap_or(false);
            }
            self.outputs[b].loopback =
                self.read_switch(&format!("Output {} Loopback", b + 1)).unwrap_or(false);
        }
    }

    fn is_uc(&self) -> bool {
        self.profile.model_name == "Fireface UC"
    }

    /// Internal output channel -> matrix column order.  The UC matrix is in
    /// display order (AN1/2 first); the hardware-volume array is Phones-first.
    fn matrix_channel(&self, idx: usize) -> usize {
        if self.is_uc() {
            matrix_display_channel(self.profile.output_order, idx)
        } else {
            idx
        }
    }

    /// Matrix source order: hardware inputs 0..17, software playback
    /// 18..35 (confirmed by the Windows mute capture on the UC).
    fn input_source(&self, idx: usize) -> usize {
        matrix_input_source(self.is_uc(), idx)
    }

    fn playback_source(&self, dev: usize) -> usize {
        matrix_playback_source(self.is_uc(), dev)
    }

    fn refresh_crosspoints(&mut self) {
        for idx in 0..self.inputs.len() {
            for p in 0..PAIRS {
                let src = self.input_source(idx);
                let l = self.read_crosspoint(p * 2, src).unwrap_or(0);
                let r = self.read_crosspoint(p * 2 + 1, src).unwrap_or(0);
                self.inputs[idx].volumes[p] = l.max(r) as f32 / UNITY as f32;
                self.inputs[idx].pans[p] = pan_from_raw(l, r);
            }
        }
        for idx in 0..self.playbacks.len() {
            let dev = self.playbacks[idx].id;
            for p in 0..PAIRS {
                let block = p * 2 + (dev % 2);
                let raw = self.read_crosspoint(block, self.playback_source(dev)).unwrap_or(0);
                self.playbacks[idx].volumes[p] = raw as f32 / UNITY as f32;
            }
        }
    }

    fn refresh_preamp(&mut self) {
        for i in 0..4.min(self.inputs.len()) {
            // Read the raw ctl element, not the simple-mixer cache: the
            // driver updates its input_gain[] from the 0x8d state stream
            // without calling snd_ctl_notify(), so a cached Selem would
            // stay at the value read when the app opened.
            if let Some(v) = self.read_int_ctl(&format!("Input {} Gain", i + 1)) {
                // The control's maximum disambiguates the new half-dB
                // representation for inputs 3/4 (max 24) from the old
                // whole-dB one (max 18/12) without a driver version query.
                let max = self
                    .find(&format!("Input {} Gain", i + 1), 0)
                    .map(|s| s.get_playback_volume_range().1 as u32)
                    .unwrap_or(if i < 2 { 65 } else { 36 });
                self.inputs[i].gain = Some(v.max(0) as u32);
                self.inputs[i].gain_max = Some(max);
            }
            if i < 2 {
                self.inputs[i].phantom =
                    self.read_switch(&format!("Input {} Phantom", i + 1)).unwrap_or(false);
            } else {
                if let Some(v) = self.read_switch(&format!("Input {} Pad", i + 1)) {
                    self.inputs[i].pad = v;
                    self.inputs[i].has_pad = true;
                }
                if let Some(v) = self.read_switch(&format!("Input {} Instrument", i + 1)) {
                    self.inputs[i].instrument = v;
                    self.inputs[i].has_instrument = true;
                }
            }
        }
    }

    fn refresh_settings(&mut self) {
        if let Some((current, names)) = self.read_enum("Clock Source") {
            if let Some(name) = names.get(current as usize) {
                self.settings.clock_source = name.clone();
            }
            self.settings.clock_sources = names;
        }
        if let Some((current, _)) = self.read_enum("Optical Output Format") {
            self.settings.optical_out_spdif = current == 1;
        }
        if let Some((current, _)) = self.read_enum("SPDIF Output Format") {
            self.settings.spdif_professional = current == 1;
        }
        if let Some(raw) = self.read_vol("Sample Rate", 0) {
            self.settings.sample_rate = raw as u32;
        }
    }

    fn set_input_crosspoints(&self, idx: usize, output: usize, volume: f32) -> Result<(), Error> {
        let pan = self.inputs[idx].pans[output];
        let (l, r) = pan_split(volume, pan);
        let src = self.input_source(idx);
        self.write_crosspoint(output * 2, src, raw_volume(l))?;
        self.write_crosspoint(output * 2 + 1, src, raw_volume(r))?;
        Ok(())
    }

    fn set_playback_crosspoint(
        &self,
        idx: usize,
        output: usize,
        volume: f32,
    ) -> Result<(), Error> {
        let dev = self.playbacks.get(idx).map(|p| p.id).unwrap_or(idx);
        let block = output * 2 + (dev % 2);
        self.write_crosspoint(block, self.playback_source(dev), raw_volume(volume))?;
        Ok(())
    }

    fn mute_channel(&mut self, channel: ChannelId, source: usize, on: bool) -> Result<(), Error> {
        if on {
            if self.muted.insert(channel) {
                let saved: Vec<i64> = (0..BLOCKS)
                    .map(|b| self.read_crosspoint(b, source).unwrap_or(0))
                    .collect();
                self.mute_saved.insert(channel, saved);
                for b in 0..BLOCKS {
                    self.write_crosspoint(b, source, 0)?;
                }
            }
        } else if self.muted.remove(&channel) {
            let saved = self.mute_saved.remove(&channel).unwrap_or_default();
            for (b, raw) in saved.iter().enumerate().take(BLOCKS) {
                self.write_crosspoint(b, source, *raw)?;
            }
        }
        Ok(())
    }

    fn scene(&self) -> Scene {
        Scene {
            name: "Auto".into(),
            model: self.canonical_model().to_string(),
            matrix_overrides: Vec::new(),
            inputs: self.inputs.clone(),
            playbacks: self.playbacks.clone(),
            outputs: self.outputs.clone(),
            settings: self.settings.clone(),
        }
    }

    fn write_all(&mut self) -> Result<(), Error> {
        self.muted.clear();
        self.mute_saved.clear();

        for b in 0..BLOCKS {
            let db = linear_to_db10(self.outputs[b].volume);
            if self
                .write_int_ctl(&format!("Output {} Hardware Volume", b + 1), db)
                .is_err()
            {
                self.write_vol(
                    &format!("Output {} Hardware Volume", b + 1),
                    0,
                    db as i64,
                )?;
            }
            self.write_switch(&format!("Output {} Mute", b + 1), self.outputs[b].mute)?;
            self.write_switch(
                &format!("Output {} Loopback", b + 1),
                self.outputs[b].loopback,
            )?;
        }

        for idx in 0..self.inputs.len() {
            for p in 0..PAIRS {
                self.set_input_crosspoints(idx, p, self.inputs[idx].volumes[p])?;
            }
            if idx < 4 {
                self.write_vol(
                    &format!("Input {} Gain", idx + 1),
                    0,
                    self.inputs[idx].gain.unwrap_or(0) as i64,
                )?;
                if idx < 2 {
                    self.write_switch(
                        &format!("Input {} Phantom", idx + 1),
                        self.inputs[idx].phantom,
                    )?;
                } else {
                    self.write_switch(
                        &format!("Input {} Pad", idx + 1),
                        self.inputs[idx].pad,
                    )?;
                    self.write_switch(
                        &format!("Input {} Instrument", idx + 1),
                        self.inputs[idx].instrument,
                    )?;
                }
            }
        }

        for idx in 0..self.playbacks.len() {
            for p in 0..PAIRS {
                self.set_playback_crosspoint(idx, p, self.playbacks[idx].volumes[p])?;
            }
        }

        for idx in 0..self.inputs.len() {
            if self.inputs[idx].mute {
                self.mute_channel(ChannelId::Input(idx), self.input_source(idx), true)?;
            }
            if self.inputs[idx].solo {
                self.soloed.insert(ChannelId::Input(idx));
            }
        }
        for idx in 0..self.playbacks.len() {
            if self.playbacks[idx].mute {
                let dev = self.playbacks[idx].id;
                self.mute_channel(ChannelId::Playback(idx), self.playback_source(dev), true)?;
            }
            if self.playbacks[idx].solo {
                self.soloed.insert(ChannelId::Playback(idx));
            }
        }

        if let Some((_, names)) = self.read_enum("Clock Source") {
            if let Some(item) = names.iter().position(|n| n == &self.settings.clock_source) {
                self.write_enum("Clock Source", item as u32)?;
            }
        }
        self.write_enum(
            "Optical Output Format",
            if self.settings.optical_out_spdif { 1 } else { 0 },
        )?;
        self.write_enum(
            "SPDIF Output Format",
            if self.settings.spdif_professional { 1 } else { 0 },
        )?;
        Ok(())
    }
    pub fn meters_available(&self) -> bool {
        // The UCX backend always has the IF1 `Device Meters` control.  Do
        // not gate the input VUs on the capture PCM: at IF5 alt 2 the
        // capture open fails (EIO), while 0x85 metering works.  If 0x85
        // ever fails, `meters()` falls back to capture or reports zero.
        true
    }
}

impl RmeDevice for UcxAlsa {
    fn model_name(&self) -> &str {
        self.profile.model_name
    }

    fn canonical_model(&self) -> &str {
        self.profile.model_name
    }

    fn protocol_features(&self) -> bool {
        // The v1 driver exposes routing, gains, mutes, phantom/PAD, clock
        // and host-side pre-fader input metering; no EQ/FX/phase/trim yet.
        false
    }

    fn stereo_split_supported(&self) -> bool {
        // The playback pair split is implemented as a model/grouping flag
        // over the existing matrix crosspoints, so it does not need the
        // full RME feature set.
        true
    }

    fn class_compliant(&self) -> bool {
        // This is our proprietary kernel driver, not snd-usb-audio's
        // class-compliant personality.  The reduced feature set is a v1
        // limitation, not a mode change.
        false
    }

    fn is_connected(&self) -> bool {
        if self.profile.model_name == "Fireface UC" {
            AlsaMixer::resolve_card_exact(self.profile.model_name).is_some()
        } else {
            AlsaMixer::resolve_card(self.profile.card_substring).is_some()
        }
    }

    fn output_pair_count(&self) -> usize {
        PAIRS
    }

    fn output_display_order(&self) -> Vec<usize> {
        self.profile.output_order.to_vec()
    }

    fn matrix_uses_display_order(&self) -> bool {
        self.profile.model_name == "Fireface UC"
    }

    fn open() -> Result<Self, Error> {
        Self::open_profile(&PROFILE)
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

    fn set_volume(
        &mut self,
        channel: ChannelId,
        output: usize,
        volume: f32,
    ) -> Result<(), Error> {
        let v = volume.clamp(0.0, 1.0);
        match channel {
            ChannelId::Output(idx) => {
                if idx >= BLOCKS {
                    return Err(Error::InvalidChannel(format!("output {idx}")));
                }
                // The output fader is the UCX's hardware output volume,
                // not the DSP master.  Writing it via the 0x0c register
                // channel also updates the unit's LCD when this output is
                // the one selected on the front panel.
                let hw = format!("Output {} Hardware Volume", idx + 1);
                let db = linear_to_db10(v);
                if self.write_int_ctl(&hw, db).is_ok() {
                    self.outputs[idx].volume = v;
                    if idx < 2 {
                        self.hw_volume = Some(v);
                    }
                } else {
                    self.write_vol(&hw, 0, db as i64)?;
                    self.outputs[idx].volume = v;
                }
            }
            ChannelId::Input(idx) => {
                if idx >= self.inputs.len() || output >= PAIRS {
                    return Err(Error::InvalidChannel(format!("input {idx}/{output}")));
                }
                if self.muted.contains(&channel) {
                    let pan = self.inputs[idx].pans[output];
                    let (l, r) = pan_split(v, pan);
                    if let Some(saved) = self.mute_saved.get_mut(&channel) {
                        saved[output * 2] = raw_volume(l);
                        saved[output * 2 + 1] = raw_volume(r);
                    }
                } else {
                    self.set_input_crosspoints(idx, output, v)?;
                }
                self.inputs[idx].volumes[output] = v;
            }
            ChannelId::Playback(idx) => {
                if idx >= self.playbacks.len() || output >= PAIRS {
                    return Err(Error::InvalidChannel(format!("playback {idx}/{output}")));
                }
                if self.muted.contains(&channel) {
                    let dev = self.playbacks[idx].id;
                    let block = output * 2 + (dev % 2);
                    if let Some(saved) = self.mute_saved.get_mut(&channel) {
                        saved[block] = raw_volume(v);
                    }
                } else {
                    self.set_playback_crosspoint(idx, output, v)?;
                }
                self.playbacks[idx].volumes[output] = v;
            }
        }
        Ok(())
    }

    fn set_stereo_split(&mut self, pb: usize, split: bool) -> Result<(), Error> {
        if pb >= self.playbacks.len() {
            return Err(Error::InvalidChannel(format!("Playback {pb}")));
        }
        // Both mono channels of a playback pair share the split state.
        // The matrix crosspoints stay per-channel; the flag controls the
        // strip grouping and which channel a linked pair fader writes.
        self.playbacks[pb].split = split;
        self.playbacks[pb ^ 1].split = split;
        Ok(())
    }

    fn volume(&self, channel: ChannelId, output: usize) -> Result<f32, Error> {
        match channel {
            ChannelId::Output(idx) => {
                self.outputs
                    .get(idx)
                    .map(|o| o.volume)
                    .ok_or_else(|| Error::InvalidChannel(format!("output {idx}")))
            }
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

    fn set_pan(&mut self, channel: ChannelId, output: usize, pan: i8) -> Result<(), Error> {
        if let ChannelId::Input(idx) = channel {
            if idx >= self.inputs.len() || output >= PAIRS {
                return Err(Error::InvalidChannel(format!("input {idx}/{output}")));
            }
            self.inputs[idx].pans[output] = pan.clamp(-100, 100);
            if idx < 4 {
                /* Mono inputs 1-4 have a real UCX pan/matrix send.  It is
                 * relative to the output currently selected on the unit;
                 * the GUI's selected output must match that submix. */
                self.write_int_ctl(&format!("Input {} Pan", idx + 1),
                                   pan.clamp(-100, 100) as i32)?;
            } else if !self.muted.contains(&channel) {
                let volume = self.inputs[idx].volumes[output];
                let (l, r) = pan_split(volume, pan);
                self.write_crosspoint(output * 2, idx, raw_volume(l))?;
                self.write_crosspoint(output * 2 + 1, idx, raw_volume(r))?;
            }
        }
        Ok(())
    }

    fn pan(&self, channel: ChannelId, output: usize) -> Result<i8, Error> {
        if let ChannelId::Input(idx) = channel {
            return self
                .inputs
                .get(idx)
                .and_then(|c| c.pans.get(output))
                .copied()
                .ok_or_else(|| Error::InvalidChannel(format!("input {idx}/{output}")));
        }
        Ok(0)
    }

    fn set_mute(&mut self, channel: ChannelId, mute: bool) -> Result<(), Error> {
        match channel {
            ChannelId::Output(idx) => {
                if idx >= BLOCKS {
                    return Err(Error::InvalidChannel(format!("output {idx}")));
                }
                if self.is_uc() {
                    let master = self.matrix_channel(idx);
                    let hw_name = format!("Output {} Hardware Volume", idx + 1);
                    let master_name =
                        format!("Output {} Master Volume", master + 1);
                    if mute {
                        if !self.output_mute_saved.contains_key(&idx) {
                            let hw = self.read_int_ctl(&hw_name).unwrap_or(-570);
                            let m = self.read_vol(&master_name, 0).unwrap_or(UNITY);
                            self.output_mute_saved.insert(idx, (hw, m));
                        }
                        /* Raw 0x3f is the captured UC mute code. */
                        self.write_int_ctl(&hw_name, -570)?;
                        self.write_vol(&master_name, 0, 0)?;
                    } else if let Some((hw, m)) = self.output_mute_saved.remove(&idx) {
                        self.write_int_ctl(&hw_name, hw)?;
                        self.write_vol(&master_name, 0, m)?;
                    }
                } else {
                    self.write_switch(&format!("Output {} Mute", idx + 1), mute)?;
                }
                self.outputs[idx].mute = mute;
            }
            ChannelId::Input(idx) => {
                if idx >= self.inputs.len() {
                    return Err(Error::InvalidChannel(format!("input {idx}")));
                }
                self.mute_channel(channel, self.input_source(idx), mute)?;
                self.inputs[idx].mute = mute;
            }
            ChannelId::Playback(idx) => {
                if idx >= self.playbacks.len() {
                    return Err(Error::InvalidChannel(format!("playback {idx}")));
                }
                let dev = self.playbacks[idx].id;
                self.mute_channel(channel, self.playback_source(dev), mute)?;
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
        // v1: solo is a UI/model state only.  Hardware solo would require
        // saving and muting every other channel; the driver has no solo
        // register.
        Ok(())
    }

    fn solo(&self, channel: ChannelId) -> Result<bool, Error> {
        Ok(self.soloed.contains(&channel))
    }

    fn set_phantom(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        if idx < 2 {
            self.write_switch(&format!("Input {} Phantom", idx + 1), on)?;
            if let Some(input) = self.inputs.get_mut(idx) {
                input.phantom = on;
            }
        }
        Ok(())
    }

    fn set_pad(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        if (2..4).contains(&idx) {
            self.write_switch(&format!("Input {} Pad", idx + 1), on)?;
            if let Some(input) = self.inputs.get_mut(idx) {
                input.pad = on;
            }
        }
        Ok(())
    }

    fn set_instrument(&mut self, idx: usize, on: bool) -> Result<(), Error> {
        if (2..4).contains(&idx) {
            self.write_switch(&format!("Input {} Instrument", idx + 1), on)?;
            if let Some(input) = self.inputs.get_mut(idx) {
                input.instrument = on;
            }
        }
        Ok(())
    }

    fn set_gain(&mut self, idx: usize, gain: u32) -> Result<(), Error> {
        if idx < 4 {
            let max = if idx < 2 {
                65
            } else {
                self.find(&format!("Input {} Gain", idx + 1), 0)
                    .map(|s| s.get_playback_volume_range().1 as u32)
                    .unwrap_or(12)
            };
            let gain = gain.min(max);
            self.write_vol(&format!("Input {} Gain", idx + 1), 0, gain as i64)?;
            if let Some(input) = self.inputs.get_mut(idx) {
                input.gain = Some(gain);
            }
        }
        Ok(())
    }

    fn set_pitch(&mut self, pitch_percent: f32) -> Result<(), Error> {
        self.settings.pitch_percent = pitch_percent;
        Ok(())
    }

    fn set_loopback(&mut self, out: usize, on: bool) -> Result<(), Error> {
        if out >= PAIRS {
            return Err(Error::InvalidChannel(format!("output pair {out}")));
        }
        self.write_switch(&format!("Output {} Loopback", out * 2 + 1), on)?;
        self.write_switch(&format!("Output {} Loopback", out * 2 + 2), on)?;
        self.outputs[out * 2].loopback = on;
        self.outputs[out * 2 + 1].loopback = on;
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
        self.settings.dim = on;
        Ok(())
    }

    fn set_eq_for_record(&mut self, on: bool) -> Result<(), Error> {
        self.settings.eq_for_record = on;
        Ok(())
    }

    fn set_optical_out_format(&mut self, spdif: bool) -> Result<(), Error> {
        self.write_enum("Optical Output Format", if spdif { 1 } else { 0 })?;
        self.settings.optical_out_spdif = spdif;
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

    fn set_sensitivity(
        &mut self,
        _idx: usize,
        _sensitivity: crate::channel::Sensitivity,
    ) -> Result<(), Error> {
        Ok(())
    }

    fn set_clock_source(&mut self, source: &str) -> Result<(), Error> {
        let (_, names) = self
            .read_enum("Clock Source")
            .ok_or_else(|| Error::MixerElementNotFound("Clock Source".into()))?;
        let item = names
            .iter()
            .position(|n| n == source)
            .ok_or_else(|| Error::InvalidChannel(format!("clock source {source}")))?;
        self.write_enum("Clock Source", item as u32)?;
        self.settings.clock_source = source.to_string();
        Ok(())
    }

    fn capture_scene(&self) -> Scene {
        self.scene()
    }

    fn apply_scene(&mut self, scene: &Scene) -> Result<(), Error> {
        scene.check_compatible(self.canonical_model())?;
        if scene.inputs.len() != self.inputs.len()
            || scene.playbacks.len() != self.playbacks.len()
            || scene.outputs.len() != self.outputs.len()
        {
            return Err(Error::InvalidChannel(
                "scene channel count does not match Fireface UCX".into(),
            ));
        }
        self.inputs = scene.inputs.clone();
        self.playbacks = scene.playbacks.clone();
        self.outputs = scene.outputs.clone();
        self.settings = scene.settings.clone();
        self.soloed.clear();
        self.write_all()?;
        if self.is_uc() {
            /* Keep playback routing last: it is the part most likely to be
             * clobbered by a stale input-crosspoint pass. */
            for idx in 0..self.playbacks.len() {
                for p in 0..PAIRS {
                    self.set_playback_crosspoint(idx, p, self.playbacks[idx].volumes[p])?;
                }
            }
            for block in 0..BLOCKS {
                self.write_uc_matrix_row(block)?;
            }
        }
        Ok(())
    }

    fn poll_events(&mut self) -> Result<(), Error> {
        let _ = self.mixer.handle_events();
        // The UCX front-panel encoder writes the output masters directly.
        // Without this readback the GUI would keep showing the value from
        // open (often 0 dB) while the hardware sat at a much lower level.
        // 100 ms is fast enough to follow a turn and cheap enough for 18
        // masters + mutes + loopback flags.
        let now = Instant::now();
        if now.duration_since(self.last_output_poll) >= Duration::from_millis(100) {
            self.last_output_poll = now;
            self.refresh_outputs();
        }
        if now.duration_since(self.last_preamp_poll) >= Duration::from_millis(100) {
            self.last_preamp_poll = now;
            self.refresh_preamp();
        }
        self.hw_volume = self.read_hw_volume();
        Ok(())
    }

    fn meters(&self) -> Option<Vec<f32>> {
        // Prefer the device's own 0x85 input meters.  They are pre-fader and
        // keep working when the IF5 capture stream is unavailable (alt 2
        // currently returns EIO).  Each input occupies an even/odd word
        // pair; the even word is the level, so input i is word 2*i:
        // AN1 = 0, AN2 = 2, … AN5 = 8, … ADAT 8 = 34.
        // Control-only Pro Audio mode: PipeWire fans the UC source out to
        // us and to the DAW; no exclusive ALSA capture.
        if let Some(tap) = self.input_tap.as_ref() {
            let (levels, channels) = tap.drain();
            if channels > 0 {
                return Some(levels[..self.inputs.len().min(levels.len())].to_vec());
            }
        }
        // UCX: prefer the device's own 0x85 input meters.  The UC's 324-byte
        // packet layout is not yet decoded reliably, so keep the capture
        // fallback for the UC.
        if self.profile.model_name != "Fireface UC" {
            if let Some(w) = self.read_device_meters() {
            const SILENT: f32 = 0.0005; // ~-66 dBFS
            const HOLD: Duration = Duration::from_millis(500);
            let now = Instant::now();
            let mut hold = self.input_hold.lock().unwrap();
            let mut out = vec![0.0; self.inputs.len()];
            for (i, level) in out.iter_mut().enumerate() {
                let new = w.get(2 * i).map(|&raw| meter_norm(raw)).unwrap_or(0.0);
                let slot = &mut hold[i];
                if new > SILENT {
                    // Peak-hold across the interleaved meter pages, so a
                    // page that reports a lower instantaneous value does
                    // not make the VU flicker.
                    if new >= slot.0 || now.duration_since(slot.1) >= HOLD {
                        *slot = (new, now);
                    }
                } else if now.duration_since(slot.1) >= HOLD {
                    *slot = (0.0, now);
                }
                *level = slot.0;
            }
            return Some(out);
            }
        }
        // Fallback: the IF5 capture stream, same identity order.
        let cm = self.capture_meter.as_ref()?;
        if !cm.is_connected() {
            return None;
        }
        let peaks = cm.drain();
        crate::capture_meter::CaptureMeter::debug_log(&peaks);
        Some(peaks)
    }
}

impl UcxAlsa {
    /// Open the Fireface UC (same ALSA control surface, different profile).
    pub fn open_uc() -> Result<Self, Error> {
        Self::open_profile(&uc::PROFILE)
    }

    /// Restart the capture meter at `rate`.  The capture PCM shares the
    /// device clock with playback; keeping it open at the old rate makes
    /// the kernel driver lock playback to that rate, so a sample-rate
    /// change must release and reopen it.
    pub fn set_meter_rate(&mut self, rate: u32) {
        self.capture_meter = None;
        if std::env::var_os("TUXMIX_NO_CAPTURE").is_none() {
            self.capture_meter = crate::capture_meter::CaptureMeter::start(
                self.mixer.card_name(),
                rate,
                self.inputs.len(),
            );
        }
    }

    pub(crate) fn open_profile(profile: &'static DeviceProfile) -> Result<Self, Error> {
        let mixer = if profile.model_name == "Fireface UC" {
            AlsaMixer::open_by_card_exact(profile.model_name)?
        } else {
            AlsaMixer::open_by_card_name(profile.card_substring)?
        };
        let hctl = HCtl::new(mixer.card_name(), false)?;
        hctl.load()?;
        if mixer.find_selem("Output 1 Mix", 0).is_none() {
            return Err(Error::MixerElementNotFound("Output 1 Mix".into()));
        }

        let meter_rate = read_sample_rate(&hctl).unwrap_or(48_000);
        // In control-only mode the DAW (Reaper/JACK/PipeWire Pro Audio) owns
        // the capture PCM.  Opening it here would steal the only capture
        // substream and break the DAW.  Input meters are then unavailable
        // until the device meter stream is wired in.
        let control_only = std::env::var_os("TUXMIX_NO_CAPTURE").is_some();
        let capture_meter = if control_only {
            log::info!("control-only mode: input meters use the PipeWire source tap");
            None
        } else {
            crate::capture_meter::CaptureMeter::start(
                mixer.card_name(),
                meter_rate,
                profile.inputs.len(),
            )
        };
        let input_tap = if control_only {
            Some(crate::pw_input::InputTap::start(profile.model_name))
        } else {
            None
        };
        if capture_meter.is_none() && std::env::var_os("TUXMIX_NO_CAPTURE").is_none() {
            log::warn!("UCX input meter: capture PCM did not open");
        } else if let Some(_) = capture_meter.as_ref() {
            log::info!(
                "UCX pre-fader input meter: {} channels at {} Hz",
                profile.inputs.len(),
                meter_rate
            );
        }

        // The UCX's analog inputs 1–4 are mono; only inputs 5/6 onward are
        // stereo pairs.  TotalMix's stereo-link toggle is a GUI grouping, not
        // a device register, so the safe first-connect default is to match
        // the hardware topology: leave pairs 0 and 1 (inputs 1–4) unlinked
        // and let the later pairs use the default linked state.  Nothing is
        // written to the device here; the gains/outputs/mixer are read back.
        let mut settings = Scene::new("").settings;
        settings.input_link = false;
        settings.input_pair_link = vec![false, false];

        let mut dev = Self {
            mixer,
            hctl,
            capture_meter,
            input_tap,
            profile,
            hw_volume: None,
            inputs: profile.build_inputs(),
            playbacks: profile.build_playbacks(),
            outputs: profile.build_outputs(),
            settings,
            muted: HashSet::new(),
            soloed: HashSet::new(),
            mute_saved: HashMap::new(),
            output_mute_saved: HashMap::new(),
            last_output_poll: Instant::now(),
            last_preamp_poll: Instant::now(),
            input_hold: Mutex::new(vec![(0.0, Instant::now()); profile.inputs.len()]),
        };
        dev.refresh_outputs();
        dev.refresh_crosspoints();
        dev.refresh_preamp();
        dev.refresh_settings();
        info!(
            "Opened {} via ALSA: {} inputs, {} playbacks, {} output pairs",
            profile.model_name,
            dev.inputs.len(),
            dev.playbacks.len(),
            PAIRS
        );
        Ok(dev)
    }

}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uc_matrix_source_order_is_inputs_then_playback() {
        assert_eq!(matrix_input_source(true, 0), 0);
        assert_eq!(matrix_input_source(true, 17), 17);
        assert_eq!(matrix_playback_source(true, 0), 18);
        assert_eq!(matrix_playback_source(true, 17), 35);
    }

    #[test]
    fn ucx_matrix_source_order_is_inputs_then_playback() {
        assert_eq!(matrix_input_source(false, 0), 0);
        assert_eq!(matrix_input_source(false, 17), 17);
        assert_eq!(matrix_playback_source(false, 0), 18);
        assert_eq!(matrix_playback_source(false, 17), 35);
    }

    #[test]
    fn uc_output_matrix_order_maps_phones_to_7_8() {
        let order = [1, 2, 3, 0, 4, 5, 6, 7, 8];
        // Phones is internal pair 0; matrix columns 7/8.
        assert_eq!(matrix_display_channel(&order, 0), 6);
        assert_eq!(matrix_display_channel(&order, 1), 7);
        // AN5/6 is internal pair 3; matrix columns 5/6.
        assert_eq!(matrix_display_channel(&order, 6), 4);
        assert_eq!(matrix_display_channel(&order, 7), 5);
        // AN1/2 is internal pair 1; matrix columns 1/2.
        assert_eq!(matrix_display_channel(&order, 2), 0);
        assert_eq!(matrix_display_channel(&order, 3), 1);
    }

    #[test]
    fn meter_norm_uses_signed_31_bit_scale() {
        assert_eq!(meter_norm(0), 0.0);
        assert!((meter_norm(1 << 30) - 0.5).abs() < 1e-6);
        assert_eq!(meter_norm(0x8000_0000), 0.0);
        assert_eq!(meter_norm(0xffff_ffff), 1.0);
    }
}
