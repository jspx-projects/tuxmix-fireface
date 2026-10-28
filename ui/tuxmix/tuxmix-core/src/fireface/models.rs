//! Per-model configuration for the hwdep Fireface backend.
//!
//! Wire encoding lives in `protocol.rs`; channel topology lives in
//! `crate::profiles`.  This file is the one place that combines those with
//! the model identity and capability surface.  Generic device code must
//! consult a `ModelSpec` rather than switch on `Model` for configuration.

use crate::capabilities::DeviceCapabilities;
use crate::profile::DeviceProfile;

use super::device::Model;

pub struct ModelSpec {
    pub model: Model,
    pub name: &'static str,
    pub card_id: &'static str,
    pub profile: &'static DeviceProfile,
    pub capabilities: DeviceCapabilities,
}

pub const UC: ModelSpec = ModelSpec {
    model: Model::Uc,
    name: "Fireface UC",
    card_id: "FirefaceUC",
    profile: &crate::profiles::uc::PROFILE,
    capabilities: DeviceCapabilities {
        protocol_features: false,
        eq: false,
        low_cut: false,
        loopback: true,
        phase: true,
        phase_without_eq: true,
        output_balance: true,
        stereo_split: true,
        matrix_cells: true,
        class_compliant: false,
        input_phantom: true,
        input_pad: true,
        input_level: false,
        input_autoset: false,
        dynamics: false,
        fx: false,
        pitch: false,
        width: true,
        ms_proc: true,
        an12: true,
        input_link: true,
        eq_for_record: false,
        optical_out: true,
    },
};

pub const UCX: ModelSpec = ModelSpec {
    model: Model::Ucx,
    name: "Fireface UCX",
    card_id: "FirefaceUCX",
    profile: &crate::profiles::ucx::PROFILE,
    capabilities: DeviceCapabilities {
        protocol_features: true,
        eq: true,
        low_cut: true,
        loopback: true,
        phase: true,
        phase_without_eq: false,
        output_balance: true,
        stereo_split: true,
        matrix_cells: false,
        class_compliant: false,
        // The UCX phantom register is measured on inputs 1/2 (`NN08`).
        // There is no PAD; Level is the pad-equivalent.
        input_phantom: true,
        input_pad: false,
        input_level: true,
        input_autoset: true,
        dynamics: true,
        fx: true,
        // These globals currently have no UCX write path in this backend.
        pitch: false,
        width: false,
        ms_proc: false,
        an12: false,
        input_link: true,
        eq_for_record: true,
        optical_out: true,
    },
};

pub fn for_model(model: Model) -> &'static ModelSpec {
    match model {
        Model::Uc => &UC,
        Model::Ucx => &UCX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uc_exposes_routing_phase_and_balance_without_dsp() {
        let caps = UC.capabilities;
        assert!(caps.matrix_cells);
        assert!(caps.loopback);
        assert!(caps.phase);
        assert!(caps.phase_without_eq);
        assert!(caps.output_balance);
        assert!(!caps.protocol_features);
        assert!(!caps.eq);
    }

    #[test]
    fn ucx_exposes_measured_strip_and_dsp_controls() {
        let caps = UCX.capabilities;
        assert!(caps.protocol_features);
        assert!(caps.eq);
        assert!(caps.phase);
        assert!(caps.loopback);
        assert!(!caps.phase_without_eq);
        assert!(caps.output_balance);
        assert!(!caps.matrix_cells);
        assert!(caps.input_phantom);
        assert!(!caps.input_pad);
        assert!(caps.input_level);
        assert!(caps.input_autoset);
        assert!(caps.dynamics);
        assert!(caps.fx);
        assert!(!caps.pitch);
        assert!(!caps.width);
        assert!(!caps.ms_proc);
        assert!(!caps.an12);
        assert!(caps.input_link);
        assert!(caps.eq_for_record);
        assert!(caps.optical_out);
    }
}
