//! Userspace ABI for the kernel driver's `snd-hwdep` control node.
//!
//! This file mirrors `kernel/ffuc-uapi.h` and must be updated with it.
//! The kernel owns USB and PCM; this ABI carries raw vendor requests,
//! IF1 register words and raw meter/state/status events.  All parsing and
//! device policy lives in this crate.

use std::mem::size_of;

pub const ABI_VERSION: u32 = 2;
pub const HWDEP_MAX_DATA: usize = 64;
pub const HWDEP_MAX_WORDS: usize = 63;
pub const EVENT_MAX: usize = 512;

pub const MODEL_UNKNOWN: u32 = 0;
pub const MODEL_UC: u32 = 1;
pub const MODEL_UCX: u32 = 2;

pub const XPORT_INTERRUPT: u32 = 0;
pub const XPORT_ISO: u32 = 1;

pub const CAP_VENDOR: u32 = 1 << 0;
pub const CAP_WORDS: u32 = 1 << 1;
pub const CAP_METERS: u32 = 1 << 2;
pub const CAP_STATUS: u32 = 1 << 3;
pub const CAP_PCM: u32 = 1 << 4;

pub const EV_METER: u32 = 1;
pub const EV_STATE: u32 = 2;
pub const EV_STATUS: u32 = 3;
pub const EV_LATCH: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HwdepInfo {
    pub abi: u32,
    pub model: u32,
    pub transport: u32,
    pub caps: u32,
    pub channels: u32,
    pub inputs: u32,
    pub outputs: u32,
    pub playback: u32,
    pub matrix_sources: u32,
    pub meter_bytes: u32,
    pub state_ep: u32,
    pub meter_ep: u32,
    pub status_ep: u32,
    pub rate: u32,
    pub firmware: u32,
    pub card: u32,
    pub pcm_device: u32,
    pub name: [u8; 32],
    pub serial: [u8; 32],
    pub reserved: [u32; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HwdepVendor {
    pub request: u8,
    pub direction: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
    pub flags: u16,
    pub data: [u8; HWDEP_MAX_DATA],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HwdepWord {
    pub addr: u16,
    pub value: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HwdepWords {
    pub count: u32,
    pub flags: u32,
    pub words: [HwdepWord; HWDEP_MAX_WORDS],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HwdepEventHdr {
    pub kind: u32,
    pub size: u32,
    pub seq: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HwdepCounters {
    pub out_urbs: u32,
    pub out_gaps: u32,
    pub out_shorts: u32,
    pub out_status_errors: u32,
    pub urb_errors: u32,
    pub recoveries: u32,
    pub events_dropped: u32,
    pub meter_urbs: u32,
    pub state_urbs: u32,
    pub status_urbs: u32,
}

/// Linux `_IOC` encoding (the generic asm-generic layout used on x86_64,
/// arm64 and most modern architectures).
const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> u64 {
    ((dir as u64) << 30)
        | ((ty as u64) << 8)
        | (nr as u64)
        | ((size as u64) << 16)
}

const IOC_READ: u32 = 2;
const IOC_WRITE: u32 = 1;

pub const IOCTL_INFO: u64 = ioc(IOC_READ, b'F', 0x10, size_of::<HwdepInfo>());
pub const IOCTL_VENDOR: u64 = ioc(IOC_READ | IOC_WRITE, b'F', 0x11, size_of::<HwdepVendor>());
pub const IOCTL_WRITE_WORDS: u64 = ioc(IOC_WRITE, b'F', 0x12, size_of::<HwdepWords>());
pub const IOCTL_COUNTERS: u64 = ioc(IOC_READ, b'F', 0x13, size_of::<HwdepCounters>());

impl HwdepInfo {
    pub fn name_str(&self) -> String {
        cstr(&self.name)
    }

    pub fn serial_str(&self) -> String {
        cstr(&self.serial)
    }
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_sizes_match_the_c_header() {
        // ffuc_hwdep_info: 17*4 + 32 + 32 + 16*4 = 196
        assert_eq!(size_of::<HwdepInfo>(), 196);
        // ffuc_hwdep_vendor: 1 + 1 + 2 + 2 + 2 + 2 + 64 = 74, aligned to 2
        assert_eq!(size_of::<HwdepVendor>(), 74);
        assert_eq!(size_of::<HwdepWords>(), 4 + 4 + 63 * 4);
        assert_eq!(size_of::<HwdepCounters>(), 40);
    }

    #[test]
    fn ioc_number_is_stable() {
        // _IOR('F', 0x10, 196) = 0x80c44610
        assert_eq!(IOCTL_INFO, 0x80c4_4610);
    }
}
