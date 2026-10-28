//! UCX per-channel DSP settings (low cut, 3-band EQ, dynamics, auto-level).
//!
//! The register offsets and units are factual protocol data from the public
//! UCX reverse-engineering notes (see `docs/PROTOCOL-COMPLETE.md`).  The
//! UC has no equivalent DSP and never uses this module.

use super::protocol::ucx::offset;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LowCut {
    pub on: bool,
    /// Hz, 20..500.
    pub freq: u16,
    /// 0..3 = 6/12/18/24 dB per octave.
    pub slope: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Band {
    /// Hardware kind: 0 Bell, 1 Shelf, 2 Low Pass (band 3 only).
    pub kind: u8,
    /// Signed tenths of a dB, ±200.
    pub gain_tenths: i16,
    /// Hz, 20..20000.
    pub freq: u16,
    /// Q × 10, 7..50.
    pub q10: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Eq {
    pub on: bool,
    pub bands: [Band; 3],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dynamics {
    pub on: bool,
    pub gain_tenths: i16,
    pub attack_ms: u16,
    pub release_ms: u16,
    pub comp_threshold_tenths: i16,
    pub comp_ratio10: u8,
    pub exp_threshold_tenths: i16,
    pub exp_ratio10: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoLevel {
    pub on: bool,
    pub max_gain_tenths: u16,
    pub headroom_tenths: u16,
    /// Tenths of a second, 1..99.
    pub rise_tenths: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelSettings {
    pub low_cut: LowCut,
    pub eq: Eq,
    pub dynamics: Dynamics,
    pub auto_level: AutoLevel,
}

impl Default for ChannelSettings {
    fn default() -> Self {
        // The state the TotalMix settings probes found and restored.
        Self {
            low_cut: LowCut {
                on: false,
                freq: 20,
                slope: 1,
            },
            eq: Eq {
                on: false,
                bands: [
                    Band {
                        kind: 1,
                        gain_tenths: 0,
                        freq: 80,
                        q10: 10,
                    },
                    Band {
                        kind: 0,
                        gain_tenths: 0,
                        freq: 1000,
                        q10: 10,
                    },
                    Band {
                        kind: 1,
                        gain_tenths: 0,
                        freq: 5000,
                        q10: 10,
                    },
                ],
            },
            dynamics: Dynamics {
                on: false,
                gain_tenths: 0,
                attack_ms: 10,
                release_ms: 300,
                comp_threshold_tenths: -300,
                comp_ratio10: 10,
                exp_threshold_tenths: -600,
                exp_ratio10: 10,
            },
            auto_level: AutoLevel {
                on: false,
                max_gain_tenths: 60,
                headroom_tenths: 60,
                rise_tenths: 50,
            },
        }
    }
}

impl ChannelSettings {
    pub fn clamped(self) -> Self {
        let band = |b: Band, max_kind: u8| Band {
            kind: b.kind.min(max_kind),
            gain_tenths: b.gain_tenths.clamp(-200, 200),
            freq: b.freq.clamp(20, 20000),
            q10: b.q10.clamp(7, 50),
        };
        let l = self.low_cut;
        let e = self.eq;
        let d = self.dynamics;
        let a = self.auto_level;
        Self {
            low_cut: LowCut {
                on: l.on,
                freq: l.freq.clamp(20, 500),
                slope: l.slope.min(3),
            },
            eq: Eq {
                on: e.on,
                bands: [
                    band(e.bands[0], 1),
                    band(e.bands[1], 0),
                    band(e.bands[2], 2),
                ],
            },
            dynamics: Dynamics {
                on: d.on,
                gain_tenths: d.gain_tenths.clamp(-300, 300),
                attack_ms: d.attack_ms.min(200),
                release_ms: d.release_ms.clamp(100, 999),
                comp_threshold_tenths: d.comp_threshold_tenths.clamp(-600, 0),
                comp_ratio10: d.comp_ratio10.clamp(10, 100),
                exp_threshold_tenths: d.exp_threshold_tenths.clamp(-990, -300),
                exp_ratio10: d.exp_ratio10.clamp(10, 100),
            },
            auto_level: AutoLevel {
                on: a.on,
                max_gain_tenths: a.max_gain_tenths.min(180),
                headroom_tenths: a.headroom_tenths.clamp(30, 120),
                rise_tenths: a.rise_tenths.clamp(1, 99),
            },
        }
    }

    /// (register offset, value) for all 27 settings registers.
    pub fn registers(&self) -> [(u8, u16); 27] {
        let (g1, f1, q1) = offset::eq_band(0);
        let (g2, f2, q2) = offset::eq_band(1);
        let (g3, f3, q3) = offset::eq_band(2);
        let [b1, b2, b3] = self.eq.bands;
        let d = self.dynamics;
        let a = self.auto_level;
        [
            (offset::LOW_CUT_ON, self.low_cut.on as u16),
            (offset::LOW_CUT_FREQ, self.low_cut.freq),
            (offset::LOW_CUT_SLOPE, self.low_cut.slope as u16),
            (offset::EQ_ON, self.eq.on as u16),
            (offset::EQ1_TYPE, b1.kind as u16),
            (g1, b1.gain_tenths as u16),
            (f1, b1.freq),
            (q1, b1.q10 as u16),
            (g2, b2.gain_tenths as u16),
            (f2, b2.freq),
            (q2, b2.q10 as u16),
            (offset::EQ3_TYPE, b3.kind as u16),
            (g3, b3.gain_tenths as u16),
            (f3, b3.freq),
            (q3, b3.q10 as u16),
            (offset::DYN_ON, d.on as u16),
            (offset::DYN_GAIN, d.gain_tenths as u16),
            // Measured on the UCX: value ≈ ms + 1 for both fields.
            (offset::DYN_ATTACK, d.attack_ms.saturating_add(1)),
            (offset::DYN_RELEASE, d.release_ms.saturating_add(1)),
            (offset::COMP_THRESHOLD, d.comp_threshold_tenths as u16),
            (offset::COMP_RATIO, d.comp_ratio10 as u16),
            (offset::EXP_THRESHOLD, d.exp_threshold_tenths as u16),
            (offset::EXP_RATIO, d.exp_ratio10 as u16),
            (offset::AUTO_LEVEL_ON, a.on as u16),
            (offset::AUTO_LEVEL_MAX_GAIN, a.max_gain_tenths),
            (offset::AUTO_LEVEL_HEADROOM, a.headroom_tenths),
            (offset::AUTO_LEVEL_RISE_TIME, a.rise_tenths),
        ]
    }

    pub fn set_register(&mut self, off: u8, value: u16) -> bool {
        let i = value as i16;
        let (g1, f1, q1) = offset::eq_band(0);
        let (g2, f2, q2) = offset::eq_band(1);
        let (g3, f3, q3) = offset::eq_band(2);
        match off {
            offset::LOW_CUT_ON => self.low_cut.on = value != 0,
            offset::LOW_CUT_FREQ => self.low_cut.freq = value,
            offset::LOW_CUT_SLOPE => self.low_cut.slope = value as u8,
            offset::EQ_ON => self.eq.on = value != 0,
            offset::EQ1_TYPE => self.eq.bands[0].kind = value as u8,
            offset::EQ3_TYPE => self.eq.bands[2].kind = value as u8,
            o if o == g1 => self.eq.bands[0].gain_tenths = i,
            o if o == f1 => self.eq.bands[0].freq = value,
            o if o == q1 => self.eq.bands[0].q10 = value as u8,
            o if o == g2 => self.eq.bands[1].gain_tenths = i,
            o if o == f2 => self.eq.bands[1].freq = value,
            o if o == q2 => self.eq.bands[1].q10 = value as u8,
            o if o == g3 => self.eq.bands[2].gain_tenths = i,
            o if o == f3 => self.eq.bands[2].freq = value,
            o if o == q3 => self.eq.bands[2].q10 = value as u8,
            offset::DYN_ON => self.dynamics.on = value != 0,
            offset::DYN_GAIN => self.dynamics.gain_tenths = i,
            offset::DYN_ATTACK => self.dynamics.attack_ms = value.saturating_sub(1),
            offset::DYN_RELEASE => self.dynamics.release_ms = value.saturating_sub(1),
            offset::COMP_THRESHOLD => self.dynamics.comp_threshold_tenths = i,
            offset::COMP_RATIO => self.dynamics.comp_ratio10 = value as u8,
            offset::EXP_THRESHOLD => self.dynamics.exp_threshold_tenths = i,
            offset::EXP_RATIO => self.dynamics.exp_ratio10 = value as u8,
            offset::AUTO_LEVEL_ON => self.auto_level.on = value != 0,
            offset::AUTO_LEVEL_MAX_GAIN => self.auto_level.max_gain_tenths = value,
            offset::AUTO_LEVEL_HEADROOM => self.auto_level.headroom_tenths = value,
            offset::AUTO_LEVEL_RISE_TIME => self.auto_level.rise_tenths = value,
            _ => return false,
        }
        true
    }

    /// The parameters that differ from `old`, as (offset, value).
    pub fn changed_from(&self, old: &Self) -> Vec<(u8, u16)> {
        self.registers()
            .into_iter()
            .zip(old.registers())
            .filter(|(new, old)| new != old)
            .map(|(new, _)| new)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_round_trip() {
        let mut s = ChannelSettings::default();
        s.low_cut.on = true;
        s.eq.bands[2].kind = 2;
        s.eq.bands[1].gain_tenths = -35;
        s.dynamics.comp_threshold_tenths = -450;
        let mut back = ChannelSettings::default();
        for (off, value) in s.registers() {
            assert!(back.set_register(off, value), "offset {off:#04x}");
        }
        assert_eq!(back, s);
    }
}
