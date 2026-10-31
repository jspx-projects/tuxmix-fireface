//! Device protocol encodings shared by the Fireface backends.
//!
//! The UC and UCX look similar but are **not** interchangeable: the UC uses
//! EP0 vendor requests for the matrix, while the UCX uses IF1 register
//! words.  The two submodules therefore keep separate constants and
//! encodings even where the same dB curve happens to apply.
//!
//! Sources:
//! - `docs/PROTOCOL-COMPLETE.md` (this repository's consolidated map)
//! - agfline/RME-Fireface-UC-Drivers (UC facts)
//! - OroboroModular/rme-fireface-ucx-linux-driver (UCX facts)
//! No code from those projects is copied; their public protocol notes are
//! used as factual references and all uncertainties are listed in the docs.

use std::fmt;

pub const INPUTS: usize = 18;
pub const OUTPUTS: usize = 18;
pub const PLAYBACK: usize = 18;
pub const PAIRS: usize = 9;
pub const MATRIX_SOURCES: usize = INPUTS + PLAYBACK;

/// 0 dB in the UC's 14-bit linear mixer scale.
pub const UC_UNITY_LEVEL14: u32 = 0x8000;

/// UC vendor request numbers (also used by the UCX where they agree).
pub mod req {
    pub const SETTINGS: u8 = 0x10;
    pub const STATUS: u8 = 0x11;
    pub const MIXER: u8 = 0x12;
    pub const MUTE: u8 = 0x13;
    pub const UNMUTE: u8 = 0x14;
    pub const LOOPBACK: u8 = 0x15;
    pub const CHAN_INIT: u8 = 0x16;
    pub const LEVELS: u8 = 0x17;
    pub const PGA: u8 = 0x1a;
    pub const DDS: u8 = 0x1b;
    pub const REVISION: u8 = 0x1c;
    pub const ENABLE: u8 = 0x20;
    /// UCX Windows path: stream-start/stop support.
    pub const STREAM29: u8 = 0x1d;
    pub const SERIAL_LO: u8 = 0x1e;
    pub const SERIAL_HI: u8 = 0x1f;
}

/// A matrix source: a hardware input or a software playback channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    Input(usize),
    Playback(usize),
}

impl Source {
    pub fn index(self) -> usize {
        match self {
            Source::Input(i) => i,
            Source::Playback(p) => p,
        }
    }
}

/// A decoded UCX meter packet kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeterKind {
    InputsA,
    InputsB,
    Playback,
    OutputsPreFader,
    OutputsPostFader,
}

impl MeterKind {
    pub fn from_tag(tag: u32) -> Option<Self> {
        Some(match tag {
            0x1111_1111 => MeterKind::InputsA,
            0x5555_5555 => MeterKind::InputsB,
            0x2222_2222 => MeterKind::Playback,
            0x3333_3333 => MeterKind::OutputsPreFader,
            0x6666_6666 => MeterKind::OutputsPostFader,
            _ => return None,
        })
    }
}

/// A 244-byte UCX `0x85` meter packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeterPacket {
    pub kind: MeterKind,
    /// Mean-square per channel: 512 × (24-bit sample)²; full scale 2^55.
    pub power: [u64; 20],
    /// Peak per channel: 16 × |24-bit sample|; full scale 2^27.
    pub peak: [u32; 18],
    /// FX bus power: in and out.
    pub fx_power: [u64; 2],
    /// FX bus peak: in and out.
    pub fx_peak: [u32; 2],
}

impl MeterPacket {
    pub const LEN: usize = 244;

    pub fn parse(b: &[u8]) -> Result<Self, ProtocolError> {
        if b.len() != Self::LEN {
            return Err(ProtocolError::MeterLength(b.len()));
        }
        let tag = u32::from_le_bytes(b[240..244].try_into().unwrap());
        let kind = MeterKind::from_tag(tag)
            .ok_or(ProtocolError::MeterTag(tag))?;
        let mut power = [0u64; 20];
        for (i, p) in power.iter_mut().enumerate() {
            *p = u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        }
        let mut peak = [0u32; 18];
        for (i, p) in peak.iter_mut().enumerate() {
            *p = u32::from_le_bytes(b[160 + i * 4..164 + i * 4].try_into().unwrap());
        }
        let fx_power = [power[18], power[19]];
        let fx_peak = [
            u32::from_le_bytes(b[232..236].try_into().unwrap()),
            u32::from_le_bytes(b[236..240].try_into().unwrap()),
        ];
        Ok(Self {
            kind,
            power,
            peak,
            fx_power,
            fx_peak,
        })
    }
}

/// RMS dBFS from a UCX power value.  Values at or below 31 are "below range".
pub fn power_dbfs(power: u64) -> f32 {
    if power <= 31 {
        return f32::NEG_INFINITY;
    }
    10.0 * (power as f64 / (1u64 << 55) as f64).log10() as f32
}

pub fn peak_dbfs(peak: u32) -> f32 {
    if peak == 0 {
        return f32::NEG_INFINITY;
    }
    20.0 * (peak as f64 / (1u64 << 27) as f64).log10() as f32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    MeterLength(usize),
    MeterTag(u32),
    BadRange(&'static str),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::MeterLength(n) => write!(f, "meter packet is {n} bytes, expected 244"),
            ProtocolError::MeterTag(t) => write!(f, "unknown meter tag {t:#010x}"),
            ProtocolError::BadRange(s) => write!(f, "value out of range: {s}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

pub mod uc {
    //! Fireface UC: EP0 vendor requests, 14-bit linear matrix gains.
    use super::{Source, UC_UNITY_LEVEL14};

    pub const MIX_STRIDE: u16 = 36;
    pub const MIX_MASTER: u16 = 992;
    pub const PLAYBACK_BASE: u16 = 18;
    /// The UC gain word is 14-bit: `level14 = wValue << 2 | (wIndex >> 14) & 3`.
    /// The ALSA/GUI value is the 12-bit `wValue` part, so its full scale is
    /// 0 dB (0x2000) and the low two level bits stay zero.
    pub const UNITY_WVALUE: u16 = (UC_UNITY_LEVEL14 >> 2) as u16;

    /// 16 output blocks in the matrix are in TotalMix *display* order
    /// (AN1/2 first, Phones last); the hardware-volume array is Phones-first.
    /// This maps an internal (Phones-first) block index to the matrix column.
    pub fn matrix_block(internal_block: usize, output_order: &[usize]) -> usize {
        let pair = internal_block / 2;
        let side = internal_block & 1;
        output_order
            .iter()
            .position(|&p| p == pair)
            .unwrap_or(pair)
            * 2
            + side
    }

    /// Internal (Phones-first) output channel -> UC encoder index.
    pub fn encoder_channel(internal_idx: usize, output_order: &[usize]) -> Option<u8> {
        let pair = internal_idx / 2;
        let side = internal_idx & 1;
        let slot = output_order.iter().position(|&p| p == pair)?;
        Some((4 + 2 * slot + side) as u8)
    }

    /// UC encoder index -> internal (Phones-first) output channel.
    pub fn output_from_encoder(channel: u8, output_order: &[usize]) -> Option<usize> {
        if !(4..=21).contains(&channel) {
            return None;
        }
        let slot = ((channel - 4) / 2) as usize;
        let side = ((channel - 4) & 1) as usize;
        Some(output_order.get(slot)? * 2 + side)
    }

    /// UC output attenuator code -> linear gain.  Raw 0 = +6 dB,
    /// 6 = 0 dB, 63 = about −57 dB.
    pub fn output_linear(raw: u8) -> f32 {
        let db = 6.0 - raw.min(63) as f32;
        10f32.powf(db / 20.0)
    }

    /// Linear 0..1 -> (`wValue`, matrix address) for one crosspoint.
    pub fn matrix_word(linear: f32, address: u16) -> (u16, u16) {
        let level14 = (linear.clamp(0.0, 1.0) * UC_UNITY_LEVEL14 as f32)
            .round()
            .clamp(0.0, 65535.0) as u32;
        let value = (level14 >> 2) as u16;
        let index = (((level14 & 3) << 14) as u16) | (address & 0x3fff);
        (value, index)
    }

    /// Linear gain -> UC output attenuator code (0 = +6 dB, 6 = 0 dB,
    /// 63 = −57 dB).  The UC step is 1 dB.
    pub fn output_attenuation(linear: f32) -> u8 {
        let db = if linear <= 0.0 {
            -65.0
        } else {
            20.0 * linear.clamp(0.0, 2.0).log10()
        };
        (6.0 - db).round().clamp(0.0, 63.0) as u8
    }

    pub fn word_for_source(source: Source) -> u16 {
        let base = match source {
            Source::Input(i) => i as u16,
            Source::Playback(p) => PLAYBACK_BASE + p as u16,
        };
        base
    }

    pub fn matrix_address(out: usize, source: Source) -> u16 {
        out as u16 * MIX_STRIDE + word_for_source(source)
    }

    pub fn master_address(out: usize) -> u16 {
        MIX_MASTER + out as u16
    }

    /// The UC matrix has no readback; the GUI value is a linear model.
    pub fn linear_from_wvalue(wvalue: u16) -> f32 {
        wvalue as f32 / UNITY_WVALUE as f32
    }
}

pub mod ucx {
    //! Fireface UCX: IF1 register words.  See `docs/PROTOCOL-COMPLETE.md`.
    use super::Source;

    pub const CHANNELS: usize = 18;
    /// DSP channel number: inputs 0x00..0x11, outputs 0x12..0x23.
    pub const OUTPUT_DSP_BASE: u8 = 0x12;
    pub const MATRIX_BASE: u16 = 0x4000;
    pub const MATRIX_STRIDE: u16 = 0x40;
    pub const PLAYBACK_SOURCE_BASE: u16 = 0x20;
    pub const FX_BUS: [u8; 2] = [30, 31];

    /// Offsets inside a 256-register channel block.
    pub mod offset {
        pub const MUTE_OR_FADER: u8 = 0x00;
        pub const FX_SEND_OR_BALANCE: u8 = 0x01;
        /// Input block: stereo/mono mode.  Output block: mute.
        pub const INPUT_STEREO: u8 = 0x02;
        pub const OUTPUT_MUTE: u8 = 0x02;
        pub const FX_RETURN: u8 = 0x03;
        /// Output block: stereo/mono mode.
        pub const OUTPUT_STEREO: u8 = 0x04;
        pub const MS_PROC: u8 = 0x05;
        pub const INPUT_PHASE: u8 = 0x06;
        pub const GAIN_REFLEVEL_OR_PHASE: u8 = 0x07;
        /// Input block: 48 V (1/2) or Level (3/4).  Output block: Level.
        pub const INPUT_LEVEL_OR_48V: u8 = 0x08;
        pub const OUTPUT_REF_LEVEL: u8 = 0x08;
        pub const HI_Z: u8 = 0x09;
        pub const AUTOSET: u8 = 0x0a;

        pub const LOW_CUT_ON: u8 = 0x20;
        pub const LOW_CUT_FREQ: u8 = 0x21;
        pub const LOW_CUT_SLOPE: u8 = 0x22;

        pub const EQ_ON: u8 = 0x40;
        pub const EQ1_TYPE: u8 = 0x41;
        pub const EQ3_TYPE: u8 = 0x48;
        pub const DYN_ON: u8 = 0x60;
        pub const DYN_GAIN: u8 = 0x61;
        pub const DYN_ATTACK: u8 = 0x62;
        pub const DYN_RELEASE: u8 = 0x63;
        pub const COMP_THRESHOLD: u8 = 0x64;
        pub const COMP_RATIO: u8 = 0x65;
        pub const EXP_THRESHOLD: u8 = 0x66;
        pub const EXP_RATIO: u8 = 0x67;

        pub const AUTO_LEVEL_ON: u8 = 0x80;
        pub const AUTO_LEVEL_MAX_GAIN: u8 = 0x81;
        pub const AUTO_LEVEL_HEADROOM: u8 = 0x82;
        pub const AUTO_LEVEL_RISE_TIME: u8 = 0x83;

        pub const fn eq_band(band: usize) -> (u8, u8, u8) {
            match band {
                0 => (0x42, 0x43, 0x44),
                1 => (0x45, 0x46, 0x47),
                _ => (0x49, 0x4a, 0x4b),
            }
        }
    }

    pub mod global {
        pub const MUTE_ENABLE: u16 = 0x3d01;
        pub const DIM: u16 = 0x3d03;
        pub const MONITOR_MASK: u16 = 0x3f98;
        pub const MONITOR_FLAGS: u16 = 0x3f9e;
        /// Vendor-driver IF1 heartbeat.  The unit only streams live state
        /// deltas (including the front-panel Phones volume) while this is
        /// being refreshed.
        pub const HEARTBEAT: u16 = 0x3f00;
    }

    pub const MINUS_INF_TENTHS: i16 = -650;

    pub fn channel_reg(dsp_channel: u8, offset: u8) -> u16 {
        ((dsp_channel as u16) << 8) | offset as u16
    }

    pub fn input_channel(input: usize) -> u8 {
        input as u8
    }

    pub fn output_channel(dsp_output: usize) -> u8 {
        OUTPUT_DSP_BASE + dsp_output as u8
    }

    /// TuxMix output pairs are Phones-first; the UCX DSP outputs are
    /// AN1/2, AN3/4, AN5/6, PH7/8, SPDIF, AS1/2, ADAT3/4….
    pub fn output_pair_dsp(pair: usize) -> Option<usize> {
        Some(match pair {
            0 => 6,  // Phones 7/8
            1 => 0,  // AN 1/2
            2 => 2,  // AN 3/4
            3 => 4,  // AN 5/6
            4 => 8,  // SPDIF
            5 => 10, // AS 1/2 (ADAT 1/2)
            6 => 12, // ADAT 3/4
            7 => 14, // ADAT 5/6
            8 => 16, // ADAT 7/8
            _ => return None,
        })
    }

    /// Map an IF1 input-gain state address (`0x0007`, `0x0107`,
    /// `0x0207`, `0x0307`) to the input index.
    pub fn input_gain_index(addr: u16) -> Option<usize> {
        if addr & 0xff != offset::GAIN_REFLEVEL_OR_PHASE as u16 {
            return None;
        }
        let channel = (addr >> 8) as usize;
        (channel < 4).then_some(channel)
    }

    /// Map an IF1 output-volume state address (channel 0x12..0x23, offset
    /// 0) to TuxMix's Phones-first output-channel index.
    pub fn output_index_from_state(addr: u16) -> Option<usize> {
        if addr as u8 != 0 {
            return None;
        }
        let dsp = (addr >> 8).checked_sub(OUTPUT_DSP_BASE as u16)? as usize;
        if dsp >= CHANNELS {
            return None;
        }
        let dsp_pair = dsp / 2;
        let side = dsp & 1;
        let pair = match dsp_pair {
            3 => 0,  // Phones 7/8
            0 => 1,  // AN1/2
            1 => 2,  // AN3/4
            2 => 3,  // AN5/6
            4 => 4,  // SPDIF
            5 => 5,  // AS 1/2
            6 => 6,  // ADAT 3/4
            7 => 7,  // ADAT 5/6
            8 => 8,  // ADAT 7/8
            _ => return None,
        };
        Some(pair * 2 + side)
    }

    pub fn source_index(source: Source) -> u16 {
        match source {
            Source::Input(i) => i as u16,
            Source::Playback(p) => PLAYBACK_SOURCE_BASE + p as u16,
        }
    }

    pub fn matrix_reg(dsp_output: u8, source: Source) -> u16 {
        MATRIX_BASE + dsp_output as u16 * MATRIX_STRIDE + source_index(source)
    }

    /// The unit's own fader/pan record for hardware input `input` in output
    /// `output`'s submix (two fields on the same address).
    pub fn fader_table_reg(dsp_output: u8, input: usize) -> u16 {
        channel_reg(output_channel(dsp_output as usize),
                    0xe0 + input as u8)
    }

    /// A matrix coefficient: bit 15 = coarse scale, bits 14..0 = signed.
    pub fn matrix_gain(linear: f64) -> u16 {
        let fine = (linear * 32768.0).round();
        if (-16384.0..=16383.0).contains(&fine) {
            return (fine as i16 as u16) & 0x7fff;
        }
        let coarse = (linear * 4096.0).round().clamp(-16384.0, 16383.0);
        0x8000 | ((coarse as i16 as u16) & 0x7fff)
    }

    pub fn matrix_gain_db(db: f64) -> u16 {
        if db.is_nan() || db == f64::NEG_INFINITY {
            return 0;
        }
        matrix_gain(10f64.powf(db / 20.0))
    }

    pub fn matrix_linear(raw: u16) -> f64 {
        let n = (raw & 0x7fff) as i32;
        let signed = if n & 0x4000 != 0 { n - 0x8000 } else { n };
        signed as f64 / if raw & 0x8000 != 0 { 4096.0 } else { 32768.0 }
    }

    pub fn matrix_db(raw: u16) -> f64 {
        20.0 * matrix_linear(raw).abs().log10()
    }

    pub fn db_to_tenths(db: f32) -> u16 {
        if db.is_nan() || db <= MINUS_INF_TENTHS as f32 / 10.0 {
            return MINUS_INF_TENTHS as u16;
        }
        ((db * 10.0).round() as i16) as u16
    }

    pub fn tenths_to_db(v: u16) -> f32 {
        let t = v as i16;
        if t <= MINUS_INF_TENTHS {
            f32::NEG_INFINITY
        } else {
            t as f32 / 10.0
        }
    }

    pub fn linear_to_tenths(linear: f32) -> u16 {
        if linear <= 0.0 {
            MINUS_INF_TENTHS as u16
        } else {
            db_to_tenths(20.0 * linear.log10())
        }
    }

    pub fn tenths_to_linear(tenths: u16) -> f32 {
        let db = tenths_to_db(tenths);
        if db == f32::NEG_INFINITY {
            0.0
        } else {
            10f32.powf(db / 20.0)
        }
    }

    /// Fader/pan table field (same address, bit 3 selects pan).
    pub fn fader_level(tenths: i16) -> u16 {
        (tenths.clamp(MINUS_INF_TENTHS, 60) << 4) as u16
    }

    pub fn fader_pan(pan: i16) -> u16 {
        (((pan.clamp(-100, 100) << 4) as u16) | 0x8) as u16
    }

    /// Clock/options control register (request 16, masked write).
    pub fn control_word(clock: u8, word_termination: bool,
                        word_single_speed: bool, spdif_professional: bool,
                        eq_dyn_for_record: bool, optical_out_spdif: bool) -> u16 {
        let mut v = ((clock & 3) as u16) << 1;
        if word_termination {
            v |= 0x0008;
        }
        if word_single_speed {
            v |= 0x0040;
        }
        if spdif_professional {
            v |= 0x0080;
        }
        if !eq_dyn_for_record {
            v |= 0x0100;
        }
        if optical_out_spdif {
            v |= 0x0400;
        }
        v
    }

    pub const CONTROL_MASK: u16 = 0x05cf;
    pub const RATE_FAMILY_MASK: u16 = 0x0030;

    pub fn rate_family(rate: u32) -> u16 {
        match rate {
            32000 | 64000 | 128000 => 0,
            44100 | 88200 | 176400 => 1,
            _ => 2,
        }
    }

    pub fn playback_mask(channels: usize) -> (u16, u16) {
        let mask = ((1u32 << channels.min(30)) - 1) << 2;
        ((mask >> 16) as u16, mask as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ucx_matrix_values_match_captures() {
        assert_eq!(ucx::matrix_gain(1.0), 0x9000);
        assert_eq!(ucx::matrix_gain(0.5), 0x8800);
        assert_eq!(ucx::matrix_gain(-0.5), 0x4000);
        assert_eq!(ucx::matrix_gain(-1.0), 0xf000);
        assert!((ucx::matrix_db(0x258f) + 10.65).abs() < 0.01);
        assert!((ucx::matrix_linear(0x258f) - 0.2934).abs() < 1e-4);
    }

    #[test]
    fn ucx_addresses_match_captures() {
        assert_eq!(ucx::matrix_reg(2, Source::Input(0)), 0x4080);
        assert_eq!(ucx::matrix_reg(2, Source::Playback(0)), 0x40a0);
        assert_eq!(ucx::matrix_reg(3, Source::Playback(1)), 0x40e1);
        assert_eq!(ucx::fader_table_reg(0, 6), 0x12e6);
        assert_eq!(ucx::fader_table_reg(3, 1), 0x15e1);
    }

    #[test]
    fn ucx_fader_fields_match_captures() {
        assert_eq!(ucx::fader_level(-106), 0xf960);
        assert_eq!(ucx::fader_level(-650), 0xd760);
        assert_eq!(ucx::fader_pan(-100), 0xf9c8);
        assert_eq!(ucx::fader_pan(100), 0x0648);
        assert_eq!(ucx::fader_pan(0), 0x0008);
    }

    #[test]
    fn uc_matrix_word_round_trip() {
        let (value, index) = uc::matrix_word(1.0, 36 * 3 + 18);
        assert_eq!(value, 0x2000);
        assert_eq!(index, 126);
        assert!((uc::linear_from_wvalue(value) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn uc_encoder_and_internal_output_are_inverse() {
        let order = [1, 2, 3, 0, 4, 5, 6, 7, 8];
        for internal in 0..18 {
            let enc = uc::encoder_channel(internal, &order).unwrap();
            assert_eq!(uc::output_from_encoder(enc, &order), Some(internal));
        }
        assert_eq!(uc::output_from_encoder(3, &order), None);
        assert_eq!(uc::output_from_encoder(22, &order), None);
    }

    #[test]
    fn uc_output_attenuator_code_and_linear_round_trip() {
        for raw in 0..=63u8 {
            let linear = uc::output_linear(raw);
            assert_eq!(uc::output_attenuation(linear), raw);
        }
        assert!((uc::output_linear(0) - 1.9953).abs() < 0.001);
        assert!((uc::output_linear(6) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn ucx_output_pair_order() {
        assert_eq!(ucx::output_pair_dsp(0), Some(6));
        assert_eq!(ucx::output_pair_dsp(1), Some(0));
        assert_eq!(ucx::output_pair_dsp(8), Some(16));
    }

    #[test]
    fn meter_packet_parses() {
        let mut b = [0u8; 244];
        b[0..8].copy_from_slice(&(512u64 * 100 * 100).to_le_bytes());
        b[144..152].copy_from_slice(&(512u64 * 40 * 40).to_le_bytes());
        b[152..160].copy_from_slice(&(512u64 * 30 * 30).to_le_bytes());
        b[160..164].copy_from_slice(&(16u32 * 100).to_le_bytes());
        b[232..236].copy_from_slice(&(16u32 * 40).to_le_bytes());
        b[236..240].copy_from_slice(&(16u32 * 30).to_le_bytes());
        b[240..244].copy_from_slice(&0x1111_1111u32.to_le_bytes());
        let p = MeterPacket::parse(&b).unwrap();
        assert_eq!(p.kind, MeterKind::InputsA);
        assert_eq!(p.power[0], 512 * 10_000);
        assert_eq!(p.peak[0], 1600);
        assert_eq!(p.fx_power, [512 * 1_600, 512 * 900]);
        assert_eq!(p.fx_peak, [640, 480]);
    }
}
