//! Fireface UC profile.
//!
//! The UC has the same 18-in / 18-out TotalMix **topology** as the UCX, with
//! no EQ/dynamics/FX section. The topology data is shared below, but protocol,
//! register, meter and feature rules are not: see `docs/DEVICE-PROFILES.md`.
//! The active UC path is interrupt IF5; the ISO IF0 description in
//! `docs/DRIVER-SPEC.md` is the parked UCX path.

use crate::profile::DeviceProfile;

use super::ucx;

pub const PROFILE: DeviceProfile = DeviceProfile {
    card_substring: "Fireface UC",
    model_name: "Fireface UC",
    // The channel names and TotalMix row order are shared with the UCX.
    // Only the DSP/FX feature set differs, and that lives in the backend.
    inputs: ucx::PROFILE.inputs,
    outputs: ucx::PROFILE.outputs,
    playback_pairs: ucx::PROFILE.playback_pairs,
    output_order: &[1, 2, 3, 0, 4, 5, 6, 7, 8],
};


#[cfg(test)]
mod tests {
    use super::PROFILE;

    #[test]
    fn hardware_output_row_keeps_phones_after_an5_6() {
        assert_eq!(PROFILE.output_order, &[1, 2, 3, 0, 4, 5, 6, 7, 8]);
    }
}
