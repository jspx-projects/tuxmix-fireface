//! Minimal safe wrapper around the driver's `snd-hwdep` node.
//!
//! The `alsa` crate has no hwdep wrapper, so this module uses `libc` for
//! `ioctl(2)`, `poll(2)` and `read(2)` directly.  It is deliberately small:
//! all device semantics are in the protocol/device modules.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::mem::{size_of, MaybeUninit};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::Duration;

use super::abi::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub kind: u32,
    pub seq: u32,
    pub flags: u32,
    pub data: Vec<u8>,
}

pub struct Hwdep {
    file: File,
    path: PathBuf,
    card: i32,
}

impl Hwdep {
    /// Open `/dev/snd/hwC{card}D0` and verify that it is our ABI.
    pub fn open(card: i32) -> io::Result<Self> {
        let path = PathBuf::from(format!("/dev/snd/hwC{card}D0"));
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let hwdep = Self { file, path, card };
        let info = hwdep.info()?;
        if info.abi != ABI_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: hwdep ABI {} (expected {})",
                    hwdep.path.display(),
                    info.abi,
                    ABI_VERSION
                ),
            ));
        }
        Ok(hwdep)
    }

    pub fn card(&self) -> i32 {
        self.card
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn info(&self) -> io::Result<HwdepInfo> {
        let mut info = MaybeUninit::<HwdepInfo>::zeroed();
        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                IOCTL_INFO as libc::c_ulong,
                info.as_mut_ptr(),
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { info.assume_init() })
    }

    /// One EP0 vendor request.  For [`Direction::In`] the returned vector is
    /// the payload the device sent; for [`Direction::Out`] it is empty.
    pub fn vendor(
        &self,
        request: u8,
        direction: Direction,
        value: u16,
        index: u16,
        data: &[u8],
    ) -> io::Result<Vec<u8>> {
        if data.len() > HWDEP_MAX_DATA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "vendor request payload too large",
            ));
        }
        let mut v: HwdepVendor = unsafe { MaybeUninit::zeroed().assume_init() };
        v.request = request;
        v.direction = match direction {
            Direction::Out => 0,
            Direction::In => 1,
        };
        v.value = value;
        v.index = index;
        v.length = data.len() as u16;
        v.data[..data.len()].copy_from_slice(data);

        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                IOCTL_VENDOR as libc::c_ulong,
                &mut v,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(match direction {
            Direction::Out => Vec::new(),
            Direction::In => v.data[..v.length as usize].to_vec(),
        })
    }

    /// Send one or more IF1 register words.  The kernel adds the odd parity
    /// bit and splits the burst into device-sized transfers.
    pub fn write_words(&self, words: &[(u16, u16)]) -> io::Result<()> {
        for chunk in words.chunks(HWDEP_MAX_WORDS) {
            let mut w: HwdepWords = unsafe { MaybeUninit::zeroed().assume_init() };
            w.count = chunk.len() as u32;
            for (i, &(addr, value)) in chunk.iter().enumerate() {
                w.words[i] = HwdepWord { addr, value };
            }
            let ret = unsafe {
                libc::ioctl(
                    self.file.as_raw_fd(),
                    IOCTL_WRITE_WORDS as libc::c_ulong,
                    &w,
                )
            };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn counters(&self) -> io::Result<HwdepCounters> {
        let mut c = MaybeUninit::<HwdepCounters>::zeroed();
        let ret = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                IOCTL_COUNTERS as libc::c_ulong,
                c.as_mut_ptr(),
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { c.assume_init() })
    }

    /// Wait until at least one event is ready.  Returns `false` on timeout.
    pub fn wait(&self, timeout: Duration) -> io::Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let ret = unsafe { libc::poll(&mut pfd, 1, ms) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        if ret == 0 {
            return Ok(false);
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "hwdep poll reported an error/hangup",
            ));
        }
        Ok(true)
    }

    /// Read exactly one event record.  Call [`Self::wait`] first (or accept
    /// blocking).
    pub fn read_event(&self) -> io::Result<Event> {
        let mut buf = [0u8; size_of::<HwdepEventHdr>() + EVENT_MAX];
        let mut file = &self.file;
        let n = file.read(&mut buf)?;
        if n < size_of::<HwdepEventHdr>() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short hwdep event",
            ));
        }
        let hdr = HwdepEventHdr {
            kind: u32::from_ne_bytes(buf[0..4].try_into().unwrap()),
            size: u32::from_ne_bytes(buf[4..8].try_into().unwrap()),
            seq: u32::from_ne_bytes(buf[8..12].try_into().unwrap()),
            flags: u32::from_ne_bytes(buf[12..16].try_into().unwrap()),
        };
        let size = hdr.size as usize;
        if size > EVENT_MAX || n < size_of::<HwdepEventHdr>() + size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad hwdep event length",
            ));
        }
        Ok(Event {
            kind: hdr.kind,
            seq: hdr.seq,
            flags: hdr.flags,
            data: buf[size_of::<HwdepEventHdr>()..size_of::<HwdepEventHdr>() + size]
                .to_vec(),
        })
    }

    /// True if the card still exists and the driver still answers INFO.
    pub fn is_connected(&self) -> bool {
        self.info().is_ok()
    }
}

/// One Fireface control card discovered through the hwdep ABI.  Unlike an
/// ALSA card-name lookup this stays valid for several units and tells the
/// caller which model/transport it found.
#[derive(Clone, Debug)]
pub struct Discovered {
    pub card: i32,
    pub info: HwdepInfo,
}

pub fn enumerate() -> Vec<Discovered> {
    let mut out = Vec::new();
    for card in candidate_cards() {
        let Ok(hw) = Hwdep::open(card) else {
            continue;
        };
        let Ok(info) = hw.info() else {
            continue;
        };
        if info.model == MODEL_UC || info.model == MODEL_UCX {
            out.push(Discovered { card, info });
        }
    }
    out
}

/// Cards listed as RME/Fireface in `/proc/asound/cards`, with a fallback to
/// scanning the first 32 card slots on systems without procfs.
fn candidate_cards() -> Vec<i32> {
    let mut cards = Vec::new();
    if let Ok(text) = std::fs::read_to_string("/proc/asound/cards") {
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            let Some((num, rest)) = line.split_once(':') else {
                continue;
            };
            let haystack = format!("{rest}\n{}", lines.peek().copied().unwrap_or(""));
            if !(haystack.contains("Fireface") || haystack.contains("RME")) {
                continue;
            }
            if let Ok(n) = num
                .trim()
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_start_matches('[')
                .parse::<i32>()
            {
                cards.push(n);
            }
        }
    }
    if cards.is_empty() {
        cards.extend(0..32);
    }
    cards
}

/// A Fireface that is present in Class Compliant (UAC2) mode and therefore
/// has no RME hwdep node.  TuxMix cannot control its mixer; the card must be
/// used through stock ALSA/PipeWire.  This type exists so the UI can say so
/// instead of silently reporting "no device".
#[derive(Clone, Debug)]
pub struct ClassCompliantCard {
    pub card: i32,
    pub name: String,
}

/// Scan `/proc/asound/cards` for Fireface UC/UCX cards that are *not* our
/// vendor-mode hwdep device.  In class-compliant mode the stock
/// `snd-usb-audio` driver owns the card; the RME mixer is unavailable.
pub fn class_compliant_cards() -> Vec<ClassCompliantCard> {
    let Ok(text) = std::fs::read_to_string("/proc/asound/cards") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((num, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(card) = parse_card_number(num) else {
            continue;
        };
        let lower = rest.to_lowercase();
        if !(lower.contains("fireface") && (lower.contains("ucx") || lower.contains("uc"))) {
            continue;
        }
        if !lower.contains("usb-audio") {
            continue;
        }
        if let Ok(hw) = Hwdep::open(card) {
            if let Ok(info) = hw.info() {
                if info.model == MODEL_UC || info.model == MODEL_UCX {
                    continue; // proprietary mode, not CC
                }
            }
        }
        out.push(ClassCompliantCard {
            card,
            name: rest.trim().to_string(),
        });
    }
    out
}

fn parse_card_number(s: &str) -> Option<i32> {
    s.trim()
        .split_whitespace()
        .next()?
        .trim_start_matches('[')
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_maps_to_the_abi() {
        assert_eq!(Direction::Out as u8, 0);
        assert_eq!(Direction::In as u8, 1);
    }
}
