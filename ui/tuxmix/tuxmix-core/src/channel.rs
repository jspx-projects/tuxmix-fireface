use serde::{Deserialize, Serialize};
use strum::{Display, EnumString};

/// Identifies a specific channel on an RME device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString)]
pub enum ChannelId {
    Input(usize),
    Playback(usize),
    Output(usize),
}

/// The type of an input channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display)]
pub enum ChannelType {
    Mic,
    Instrument,
    Line,
    SPDIF,
    ADAT,
}

/// A single physical hardware input channel.
///
/// Each channel can be routed to every hardware output pair with
/// its own volume and pan — this is the submix (matrix) model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputChannel {
    pub id: usize,
    pub name: String,
    pub channel_type: ChannelType,
    /// Volume per output pair (0.0 – 1.0). Length = number of output pairs.
    pub volumes: Vec<f32>,
    /// Pan per output pair (-100 .. 100). Length = number of output pairs.
    pub pans: Vec<i8>,
    /// FX send in dB (-65..0, 0 = max). Per channel on the UCX (`NN01`).
    #[serde(default = "default_fx_send_channel_db")]
    pub fx_send_db: f32,
    pub phantom: bool, // 48V
    pub pad: bool,
    /// Hi-Z instrument input switch (Inputs 3/4 on the UC/UCX).
    #[serde(default)]
    pub instrument: bool,
    /// Runtime capability flags, filled by backends that expose the
    /// corresponding hardware switch.
    #[serde(skip)]
    pub has_pad: bool,
    #[serde(skip)]
    pub has_instrument: bool,
    pub sensitivity: Option<Sensitivity>,
    /// UCX input reference level (manual: `-10 dBV`, `+4 dBu`, `LoGain`).
    /// `None` on channels without a software Level control.
    #[serde(default)]
    pub level: Option<InputLevel>,
    /// AutoSet automatic gain reduction (UCX inputs 1/2).
    #[serde(default)]
    pub autoset: bool,
    /// Per-channel dynamics / Auto Level (UCX inputs and outputs).
    #[serde(default)]
    pub dynamics: Option<Dynamics>,
    /// Preamp gain in dB (device-specific range — see `gain_max`).
    /// `None` for channel types with no gain control
    /// (Line/SPDIF/ADAT).
    pub gain: Option<u32>,
    /// Hardware-reported maximum for `gain`, read from the ALSA
    /// control's own range at attach time. `None` alongside `gain`.
    pub gain_max: Option<u32>,
    /// Phase Ø invert flag (bitwise-NOT of the crosspoint coefficient).
    /// `#[serde(default)]` so old scene JSON still loads.
    #[serde(default)]
    pub phase: bool,
    /// Ref-level code (Instr 3/4: +4dBu / -10dBV / Boost) as the raw
    /// Ref-level code (Instr 3/4): 0 = unset, 1 = +4dBu, 2 = -10dBV,
    /// 3 = Boost (see `usb::REF_*`; labeled cap_reflevel2.pcap).
    #[serde(default)]
    pub ref_level: u16,
    /// Hardware 3-band + low-cut DSP EQ. `Some` only for the 4 analog
    /// inputs (AN1-AN4 on the Babyface Pro FS — the only channels with
    /// an EQ strip); `None` for every other input type.
    /// `#[serde(default)]` so old scene JSON without EQ still loads.
    #[serde(default)]
    pub eq: Option<InputEq>,
    pub mute: bool,
    pub solo: bool,
    /// Trim (T), dB on the master curve (-65..+6, see
    /// `RmeDevice::set_trim`) — every hardware input has one (TotalMix
    /// shows a "T" button on all of them, analog and digital alike),
    /// unlike `gain` which only applies to Mic/Instrument. `set_trim`'s
    /// own trait default is a silent no-op (no register on the ALSA
    /// path), so this field is the only place a value set through it
    /// persists for the UI to read back — mirrored locally by whichever
    /// backend's `set_trim` override actually knows what to do with it,
    /// same as `gain`. `#[serde(default)]` so old scene JSON still loads.
    #[serde(default)]
    pub trim: f32,
    /// Host-side stereo width, -1..+1 (+1 = full stereo).  Applied to the
    /// pair's 2x2 send matrix when a send is written.
    #[serde(default = "default_width")]
    pub width: f32,
    /// Mid/side processing on the pair.  Inputs also carry `NN05`; the
    /// send matrix is rewritten with the M/S encoder.
    #[serde(default)]
    pub ms_proc: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display)]
pub enum Sensitivity {
    Minus10dBV,
    Plus4dBu,
}

/// Reference-level presets of a Fireface input.
///
/// The UCX manual gives three values for inputs 3/4 and 5–8: `-10 dBV`,
/// `+4 dBu` and `LoGain`.  The wire values are measured for inputs 3/4
/// (`NN08`: 1 = +4 dBu, 2 = LoGain, 0 = −10 dBV inferred).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display)]
pub enum InputLevel {
    Minus10dBV,
    Plus4dBu,
    LoGain,
}

impl InputLevel {
    /// Raw `NN08` code.  `-10 dBV` = 0 is inferred from the three-value enum.
    pub fn code(self) -> u16 {
        match self {
            InputLevel::Minus10dBV => 0,
            InputLevel::Plus4dBu => 1,
            InputLevel::LoGain => 2,
        }
    }

    pub fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            0 => InputLevel::Minus10dBV,
            1 => InputLevel::Plus4dBu,
            2 => InputLevel::LoGain,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            InputLevel::Minus10dBV => "-10dBV",
            InputLevel::Plus4dBu => "+4dBu",
            InputLevel::LoGain => "LoGain",
        }
    }
}

/// Per-channel Compressor / Expander / Auto Level state (UCX manual §25.3.3).
///
/// Values use the UI units; the device encoding is 0.1 dB / ×10 / ms + 1 and
/// is applied in the Fireface backend.  `on` is the shared Dynamics button;
/// `auto_level` is the independent Auto Level button.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Dynamics {
    pub on: bool,
    /// CompEx gain, −30..+30 dB.
    pub gain_db: f32,
    /// CompEx attack, 0..200 ms.
    pub attack_ms: u16,
    /// CompEx release, 100..999 ms.
    pub release_ms: u16,
    /// Compressor threshold, −60..0 dB.
    pub comp_threshold_db: f32,
    /// Compressor ratio, 1..10.
    pub comp_ratio: f32,
    /// Expander threshold, −99..−30 dB.
    pub exp_threshold_db: f32,
    /// Expander ratio, 1..10.
    pub exp_ratio: f32,
    pub auto_level: bool,
    /// Auto Level max gain, 0..18 dB.
    pub auto_max_gain_db: f32,
    /// Auto Level headroom, 3..12 dB.
    pub auto_headroom_db: f32,
    /// Auto Level rise time, 0.1..9.9 s.
    pub auto_rise_s: f32,
}

impl Default for Dynamics {
    fn default() -> Self {
        Self {
            on: false,
            gain_db: 0.0,
            attack_ms: 10,
            release_ms: 300,
            comp_threshold_db: -30.0,
            comp_ratio: 1.0,
            exp_threshold_db: -60.0,
            exp_ratio: 1.0,
            auto_level: false,
            auto_max_gain_db: 6.0,
            auto_headroom_db: 6.0,
            auto_rise_s: 5.0,
        }
    }
}

impl Dynamics {
    pub fn clamped(mut self) -> Self {
        self.gain_db = self.gain_db.clamp(-30.0, 30.0);
        self.attack_ms = self.attack_ms.min(200);
        self.release_ms = self.release_ms.clamp(100, 999);
        self.comp_threshold_db = self.comp_threshold_db.clamp(-60.0, 0.0);
        self.comp_ratio = self.comp_ratio.clamp(1.0, 10.0);
        self.exp_threshold_db = self.exp_threshold_db.clamp(-99.0, -30.0);
        self.exp_ratio = self.exp_ratio.clamp(1.0, 10.0);
        self.auto_max_gain_db = self.auto_max_gain_db.clamp(0.0, 18.0);
        self.auto_headroom_db = self.auto_headroom_db.clamp(3.0, 12.0);
        self.auto_rise_s = self.auto_rise_s.clamp(0.1, 9.9);
        self
    }
}

/// Global Reverb settings (UCX manual §25.6).  Registers `0x3c00..0x3c0d`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Reverb {
    pub on: bool,
    /// Algorithm index (measured 3 = one of the room/short types; the full
    /// name→index list is still an oracle item).
    pub kind: u8,
    /// 0..999 ms.
    pub pre_delay_ms: u16,
    /// 20..500 Hz.
    pub low_cut_hz: u16,
    /// 0.5..3.0; 0.01 units.
    pub room_scale: f32,
    /// 5000..20000 Hz.
    pub high_cut_hz: u16,
    /// 0..100.
    pub smoothness: u8,
    /// Signed dB, 0.1 dB units on the wire.
    pub volume_db: f32,
    /// 0 = mono, 100 = stereo; 0.01 units.
    pub width: f32,
    /// Envelope only: attack, 5..400 ms (`0x3c05`).
    #[serde(default = "default_attack_ms")]
    pub attack_ms: u16,
    /// Envelope and Gated: hold, 5..400 ms (`0x3c06`).
    #[serde(default = "default_hold_ms")]
    pub hold_ms: u16,
    /// Envelope and Gated: release, 5..500 ms (`0x3c07`).
    #[serde(default = "default_release_ms")]
    pub release_ms: u16,
    /// Space only: reverb time in 0.1 s units (`0x3c09`).
    #[serde(default = "default_reverb_time_ds")]
    pub reverb_time_ds: u16,
    /// Space only: high damp in Hz (`0x3c0a`).
    #[serde(default = "default_high_damp_hz")]
    pub high_damp_hz: u16,
}

/// A channel whose send was never set is closed (-65 dB), not at unity.
pub fn default_fx_send_channel_db() -> f32 {
    -65.0
}

/// Stereo width centre: +1.0 is full stereo, the no-op value.
fn default_width() -> f32 {
    1.0
}

fn default_attack_ms() -> u16 {
    20
}
fn default_hold_ms() -> u16 {
    40
}
fn default_release_ms() -> u16 {
    100
}
fn default_reverb_time_ds() -> u16 {
    20
}
fn default_high_damp_hz() -> u16 {
    12_000
}

impl Default for Reverb {
    fn default() -> Self {
        Self {
            on: false,
            kind: 0,
            pre_delay_ms: 22,
            low_cut_hz: 49,
            room_scale: 1.5,
            high_cut_hz: 11_900,
            smoothness: 93,
            volume_db: -15.8,
            width: 52.0,
            attack_ms: default_attack_ms(),
            hold_ms: default_hold_ms(),
            release_ms: default_release_ms(),
            reverb_time_ds: default_reverb_time_ds(),
            high_damp_hz: default_high_damp_hz(),
        }
    }
}

/// Global Echo settings.  Registers `0x3c20..0x3c26`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Echo {
    pub on: bool,
    /// 0 = Stereo Echo, 1 = Stereo Cross (confirmed), 2 = Pong Echo
    /// (0/2 inferred from the manual's order).
    pub kind: u8,
    /// 0.01 s units on the wire.
    pub delay_ms: u16,
    /// 0..100 %.
    pub feedback: u8,
    /// Raw high-cut value/index (the unit is not decoded).
    pub high_cut: u8,
    /// Signed dB, 0.1 dB units on the wire.
    pub volume_db: f32,
    /// 0 = mono, 100 = stereo; 0.01 units.
    pub width: f32,
}

impl Default for Echo {
    fn default() -> Self {
        Self {
            on: false,
            kind: 1,
            delay_ms: 600,
            feedback: 20,
            high_cut: 0,
            volume_db: -10.0,
            width: 100.0,
        }
    }
}

fn default_fx_return_db() -> f32 {
    -65.0
}

/// One EQ band's type (`mixer.c`'s `bf_eq_type_texts`: "Off"/"Bell"/
/// "Low Shelf"/"High Shelf" — enum items 0-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display, Default)]
pub enum EqBandType {
    #[default]
    Off,
    Bell,
    LowShelf,
    HighShelf,
    /// Band 3 only: low-pass (the manual's Hi Cut). Device value 2.
    LowPass,
}

/// One of the 3 parametric bands in an [`InputEq`] strip.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    pub band_type: EqBandType,
    /// 20 – 20000 Hz.
    pub freq_hz: u16,
    /// Q factor, 0.05 – 10.0 (ALSA raw is Q x 100).
    pub q: f32,
    /// -24.0 – +24.0 dB (ALSA raw is dB x 10).
    pub gain_db: f32,
}

impl Default for EqBand {
    fn default() -> Self {
        Self {
            band_type: EqBandType::Off,
            freq_hz: 1000,
            q: 0.7,
            gain_db: 0.0,
        }
    }
}

/// Hardware 3-band parametric EQ + low cut on one analog input strip
/// (`eq.c`: 3 bands, a shared enable, and a low-cut filter).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InputEq {
    pub enabled: bool,
    pub bands: [EqBand; 3],
    /// 20 – 20000 Hz.
    pub low_cut_freq_hz: u16,
    /// 6, 12, 18, or 24 dB/octave.
    pub low_cut_slope_db_oct: u8,
    /// Low cut is independent of the main EQ bypass (`enabled`), as on
    /// RME's hardware.  Old scenes predate this field and load as off.
    #[serde(default)]
    pub low_cut_enabled: bool,
}

impl Default for InputEq {
    fn default() -> Self {
        Self {
            enabled: false,
            // Match the hardware/TotalMix starting positions (bass,
            // mid, treble).  The bands are Off, but these frequencies
            // are what the red/green/blue response segments divide on.
            bands: [
                EqBand {
                    freq_hz: 80,
                    ..EqBand::default()
                },
                EqBand::default(),
                EqBand {
                    freq_hz: 5_000,
                    ..EqBand::default()
                },
            ],
            low_cut_freq_hz: 20,
            low_cut_slope_db_oct: 6,
            low_cut_enabled: false,
        }
    }
}

/// A single software playback channel (from the computer to the device).
///
/// Same submix model: one volume + pan per hardware output pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackChannel {
    pub id: usize,
    pub name: String,
    /// Volume per output pair (0.0 – 1.0).
    pub volumes: Vec<f32>,
    /// Pan per output pair (-100 .. 100).
    pub pans: Vec<i8>,
    /// FX send in dB (-65..0, 0 = max). Per channel on the UCX (`0x47A0`
    /// family, linear on the wire).
    #[serde(default = "default_fx_send_channel_db")]
    pub fx_send_db: f32,
    /// Stereo-split flag: the strip's L/R crosspoints are rewritten to
    /// 0x2000/0x0000 (split mono) instead of 0x1000/0x1000 (-6 dB pair).
    #[serde(default)]
    pub split: bool,
    /// Trim (T) in dB, 0 to -65: a negative value attenuates every send
    /// of this channel (see `RmeDevice::set_trim`).
    #[serde(default)]
    pub trim: f32,
    /// Host-side stereo width, -1..+1 (+1 = full stereo).  Applied to the
    /// pair's 2x2 send matrix when a send is written.
    #[serde(default = "default_width")]
    pub width: f32,
    /// Mid/side processing on the pair; the send matrix is rewritten with
    /// the M/S encoder.  Playback has no register, only the matrix.
    #[serde(default)]
    pub ms_proc: bool,
    pub mute: bool,
    pub solo: bool,
}

/// A physical hardware output (stereo pair) on the device.
///
/// Each output has a master volume, mute, and solo — the "bottom row"
/// in TotalMix. Inputs and playbacks route INTO these outputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputChannel {
    pub id: usize,
    pub name: String,
    /// Master volume for this output as linear gain.  1.0 = 0 dB;
    /// 2.0 = +6 dB is valid on hardware outputs.
    pub volume: f32,
    /// Per-output EQ/low cut on hardware whose DSP runs on outputs too
    /// (UCX: every hardware output).  `None` means the output has no EQ.
    #[serde(default)]
    pub eq: Option<InputEq>,
    /// Loopback flag (the input signal is fed back into the playback
    /// path) — bReq 0x15, one mono channel per side (AN1/2 = ch 0/1).
    #[serde(default)]
    pub loopback: bool,
    /// Per-output dynamics / Auto Level (UCX hardware outputs only).
    #[serde(default)]
    pub dynamics: Option<Dynamics>,
    /// UCX output balance, signed −100..+100 (`0xNN01`).  Independent of
    /// the pair's volume.
    #[serde(default)]
    pub balance: i8,
    /// UCX per-output FX Return, 0.1 dB signed (`0xNN03`).
    #[serde(default = "default_fx_return_db")]
    pub fx_return_db: f32,
    /// Output phase, one side each (`base + 0x07`, `base + 0x107`).
    #[serde(default)]
    pub phase_l: bool,
    #[serde(default)]
    pub phase_r: bool,
    /// Excludes this output from Trim Gain changes.  Host-side: the trim
    /// factor is 1 for sends into a No-Trim destination.
    #[serde(default)]
    pub no_trim: bool,
    pub mute: bool,
    pub solo: bool,
    /// CUE engaged — this output's own dedicated playback pair (`PB
    /// {pair+1}`) is exclusively monitored through the AN1/2 bus, every
    /// other playback pair muted there. See [`RmeDevice::set_cue`].
    /// Exclusive across every output (only one can be cued at a time,
    /// same as solo), not persisted in scenes — a momentary monitoring
    /// state, not a mix setting.
    #[serde(default, skip_serializing)]
    pub cue: bool,
}

impl InputChannel {
    pub fn new(id: usize, name: &str, channel_type: ChannelType, outputs: usize) -> Self {
        Self {
            id,
            name: name.to_string(),
            channel_type,
            volumes: vec![1.0; outputs],
            pans: vec![0; outputs],
            fx_send_db: default_fx_send_channel_db(),
            phantom: false,
            pad: false,
            instrument: false,
            has_pad: false,
            has_instrument: false,
            sensitivity: None,
            level: None,
            autoset: false,
            dynamics: None,
            gain: None,
            gain_max: None,
            phase: false,
            ref_level: 0,
            eq: None,
            mute: false,
            solo: false,
            trim: 0.0,
            width: 1.0,
            ms_proc: false,
        }
    }
}

impl PlaybackChannel {
    pub fn new(id: usize, name: &str, outputs: usize) -> Self {
        Self {
            id,
            name: name.to_string(),
            volumes: vec![1.0; outputs],
            pans: vec![0; outputs],
            fx_send_db: default_fx_send_channel_db(),
            split: false,
            trim: 0.0,
            width: 1.0,
            ms_proc: false,
            mute: false,
            solo: false,
        }
    }
}

impl OutputChannel {
    pub fn new(id: usize, name: &str) -> Self {
        Self {
            id,
            name: name.to_string(),
            volume: 1.0,
            eq: None,
            loopback: false,
            dynamics: None,
            balance: 0,
            fx_return_db: default_fx_return_db(),
            phase_l: false,
            phase_r: false,
            no_trim: false,
            mute: false,
            solo: false,
            cue: false,
        }
    }
}
