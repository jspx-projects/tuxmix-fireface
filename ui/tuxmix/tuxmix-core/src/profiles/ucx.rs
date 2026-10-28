//! Fireface UCX profile — channel topology and card detection.
//!
//! The UCX has two different pair orders:
//!
//! - Hardware **outputs** are Phones-first, then AN1/2, AN3/4, AN5/6,
//!   SPDIF, AS 1/2, ADAT 3/4, ADAT 5/6, ADAT 7/8 (TuxMix's output row).
//! - **Software playback** follows TotalMix's own row: AN1/2, AN3/4,
//!   AN5/6, PH 7/8, SPDIF, AS 1/2, ADAT 3/4, ADAT 5/6, ADAT 7/8.
//!
//! "AS 1/2" is the first ADAT pair when the optical input is running SPDIF
//! (the optical input auto-detects); this unit is configured that way.
//! The label is therefore configuration-dependent and should eventually be
//! read back from the device state at open.
//!
//! The driver's matrix sources are 0–17 hardware inputs, 18–35 playback;
//! the output blocks are 0–17 in the hardware output pair order.

use crate::channel::ChannelType;
use crate::profile::{DeviceProfile, InputSpec, OutputPairSpec};

pub const PROFILE: DeviceProfile = DeviceProfile {
    card_substring: "Fireface UCX",
    model_name: "Fireface UCX",
    inputs: &[
        InputSpec {
            name: "AN1",
            channel_type: ChannelType::Mic,
        },
        InputSpec {
            name: "AN2",
            channel_type: ChannelType::Mic,
        },
        InputSpec {
            name: "AN3",
            channel_type: ChannelType::Line,
        },
        InputSpec {
            name: "AN4",
            channel_type: ChannelType::Line,
        },
        InputSpec {
            name: "AN5",
            channel_type: ChannelType::Line,
        },
        InputSpec {
            name: "AN6",
            channel_type: ChannelType::Line,
        },
        InputSpec {
            name: "AN7",
            channel_type: ChannelType::Line,
        },
        InputSpec {
            name: "AN8",
            channel_type: ChannelType::Line,
        },
        InputSpec {
            name: "SPDIF",
            channel_type: ChannelType::SPDIF,
        },
        InputSpec {
            name: "SPDIF",
            channel_type: ChannelType::SPDIF,
        },
        InputSpec {
            name: "ADAT 1",
            channel_type: ChannelType::ADAT,
        },
        InputSpec {
            name: "ADAT 2",
            channel_type: ChannelType::ADAT,
        },
        InputSpec {
            name: "ADAT 3",
            channel_type: ChannelType::ADAT,
        },
        InputSpec {
            name: "ADAT 4",
            channel_type: ChannelType::ADAT,
        },
        InputSpec {
            name: "ADAT 5",
            channel_type: ChannelType::ADAT,
        },
        InputSpec {
            name: "ADAT 6",
            channel_type: ChannelType::ADAT,
        },
        InputSpec {
            name: "ADAT 7",
            channel_type: ChannelType::ADAT,
        },
        InputSpec {
            name: "ADAT 8",
            channel_type: ChannelType::ADAT,
        },
    ],
    outputs: &[
        OutputPairSpec {
            left: "Phones 7",
            right: "Phones 8",
        },
        OutputPairSpec {
            left: "AN1",
            right: "AN2",
        },
        OutputPairSpec {
            left: "AN3",
            right: "AN4",
        },
        OutputPairSpec {
            left: "AN5",
            right: "AN6",
        },
        OutputPairSpec {
            left: "SPDIF",
            right: "SPDIF",
        },
        OutputPairSpec {
            left: "AS 1",
            right: "AS 2",
        },
        OutputPairSpec {
            left: "ADAT 3",
            right: "ADAT 4",
        },
        OutputPairSpec {
            left: "ADAT 5",
            right: "ADAT 6",
        },
        OutputPairSpec {
            left: "ADAT 7",
            right: "ADAT 8",
        },
    ],
    // TotalMix's software-playback row, in device order.  Playback id 0/1
    // is the first pair (AN1/2); Phones is the fourth pair; the first
    // optical pair is labelled AS 1/2 because the optical input is SPDIF.
    playback_pairs: &[
        OutputPairSpec {
            left: "AN1",
            right: "AN2",
        },
        OutputPairSpec {
            left: "AN3",
            right: "AN4",
        },
        OutputPairSpec {
            left: "AN5",
            right: "AN6",
        },
        OutputPairSpec {
            left: "Phones 7",
            right: "Phones 8",
        },
        OutputPairSpec {
            left: "SPDIF",
            right: "SPDIF",
        },
        OutputPairSpec {
            left: "AS 1",
            right: "AS 2",
        },
        OutputPairSpec {
            left: "ADAT 3",
            right: "ADAT 4",
        },
        OutputPairSpec {
            left: "ADAT 5",
            right: "ADAT 6",
        },
        OutputPairSpec {
            left: "ADAT 7",
            right: "ADAT 8",
        },
    ],
    // TotalMix's bottom Hardware Outputs row: AN1/2, AN3/4, AN5/6,
    // Phones 7/8, SPDIF, AS 1/2, ADAT 3/4, ADAT 5/6, ADAT 7/8.
    output_order: &[1, 2, 3, 0, 4, 5, 6, 7, 8],
};

#[cfg(test)]
mod tests {
    use super::PROFILE;

    #[test]
    fn totalmix_software_playback_row() {
        let names: Vec<&str> = PROFILE.playback_pairs.iter().map(|p| p.left).collect();
        assert_eq!(
            names,
            [
                "AN1", "AN3", "AN5", "Phones 7", "SPDIF", "AS 1", "ADAT 3", "ADAT 5", "ADAT 7"
            ]
        );
    }

    #[test]
    fn totalmix_hardware_output_row() {
        // The display row skips the Phones block; TuxMix appends it last.
        assert_eq!(PROFILE.output_order, &[1, 2, 3, 0, 4, 5, 6, 7, 8]);
    }
}
