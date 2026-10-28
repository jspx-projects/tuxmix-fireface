//! Capability flags shared by device backends and the GUI.
//!
//! These describe what the *connection* can actually drive, not what the
//! model's hardware could do.  A backend that has not implemented a feature
//! must report it as unavailable; this is the single place the UI should
//! consult instead of inferring behaviour from a model enum.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DeviceCapabilities {
    /// RME vendor-mode DSP features: EQ, dynamics, trim and the related
    /// settings surface.
    pub protocol_features: bool,
    pub eq: bool,
    /// The backend can toggle the low cut independently of the main EQ.
    pub low_cut: bool,
    pub loopback: bool,
    pub phase: bool,
    /// Input phase works without an EQ section (the UC stores it in the
    /// matrix sign; the UCX has a channel register).
    pub phase_without_eq: bool,
    /// The output pair has its own phase registers (`base + 0x07` /
    /// `base + 0x107`); false when only the input phase path is mapped.
    pub output_phase: bool,
    pub output_balance: bool,
    pub stereo_split: bool,
    /// Per-cell routing can be edited on the 36 x 18 matrix.
    pub matrix_cells: bool,
    pub class_compliant: bool,
    /// The backend can drive the 48 V switch.  A false value must hide the
    /// control even when the channel type is a microphone.
    pub input_phantom: bool,
    pub input_pad: bool,
    /// The backend can drive the three-value UCX input Level.
    pub input_level: bool,
    /// The backend can drive AutoSet (UCX inputs 1/2).
    pub input_autoset: bool,
    /// The backend can drive per-channel dynamics / Auto Level.
    pub dynamics: bool,
    /// The backend can drive the global Reverb/Echo block and FX Return.
    pub fx: bool,
    /// Global DSP controls; false means the backend only stores the value
    /// and cannot make the device do anything with it.
    pub pitch: bool,
    pub width: bool,
    /// Per-channel stereo width (the pair's 2x2 send matrix).  The UCX
    /// backend implements it; the global `width` flag stays for the units
    /// that have one device-wide control.
    pub channel_width: bool,
    pub ms_proc: bool,
    pub an12: bool,
    pub input_link: bool,
    pub eq_for_record: bool,
    pub optical_out: bool,
}
