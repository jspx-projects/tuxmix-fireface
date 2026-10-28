//! Per-input meters from PipeWire's source node.
//!
//! The device exposes no playback-meter registers, and the kernel
//! driver's capture stream only carries a mixed-down playback tap (one
//! stereo pair for every PB channel), so per-PB levels have to come from
//! the host mixer. PipeWire exposes the card sink's monitor, which is
//! the mixed playback at the sink's own channel count: 2 channels in the
//! default stereo profile (PB1 only), 12 channels for a Babyface in the
//! "Pro Audio" profile (all six PB pairs), or 18 for a UCX in "Pro Audio"
//! (all nine pairs), with the same order as the device's playback channels.
//!
//! `pw-record` is the capture child: it ships with PipeWire, needs no
//! build dependency, and can write raw S32LE to stdout. The sink is
//! discovered through `pactl --format=json list sinks` (PipeWire's
//! Pulse compatibility, present wherever `pactl` is), and the child is
//! told to capture the source directly. The target is the source's
//! Pulse index, not its name: with a name, a suspended target silently
//! falls back to the default source instead (caught live: the Pro Audio
//! record linked to the laptop speakers while the Babyface was
//! suspended). If either tool or the RME source is missing, the meters
//! simply stay unavailable and the rest of the app is unaffected.
//!
//! The tap is disabled in mock mode: the mock generates its own playback
//! levels and must not depend on the host's audio server.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// The largest RME Pro Audio playback layout: 18 channels on the UCX
/// (nine pairs); the Babyface uses 12 (six pairs). Anything past this is
/// not an RME layout and is ignored.
const MAX_CHANNELS: usize = 18;
/// Frames per read: at 48 kHz this is ~21 ms, enough for a smooth meter
/// and small enough to keep the mutex cheap.
const PERIOD_FRAMES: usize = 1024;
/// How long to wait before re-discovering the monitor after a failure
/// or after the capture child exits (profile switch, unplug, etc.).
const RETRY: Duration = Duration::from_secs(2);

/// A captured PipeWire sink: its Pulse name and index (the index is the
/// `pw-record` target), plus the monitor's rate and channel count.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Monitor {
    name: String,
    index: u32,
    rate: u32,
    channels: usize,
}

/// Owns the capture child and its reader thread. `drain()` is the only
/// thing the UI touches; the rest (discovery, restart, cleanup) happens
/// on the worker thread.
pub struct InputTap {
    levels: Arc<Mutex<[f32; MAX_CHANNELS]>>,
    channels: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    handle: Option<JoinHandle<()>>,
    watch: Option<JoinHandle<()>>,
}

impl InputTap {
    /// Start discovering and capturing the RME input source whose Pulse
    /// name contains `selector`.  The selector is the unit serial for the
    /// hwdep backend, or the model name for the legacy ALSA backend.  This
    /// matters with two units connected: selecting the widest RME source
    /// would display the other unit's inputs.  Cheap: the actual work runs
    /// on a background thread, and a missing `pactl`/`pw-record` just
    /// leaves the meters at zero channels.
    pub fn start(selector: &str) -> Self {
        let levels = Arc::new(Mutex::new([0.0; MAX_CHANNELS]));
        let channels = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let wanted: Arc<Mutex<Option<Monitor>>> = Arc::new(Mutex::new(None));

        let handle = {
            let levels = Arc::clone(&levels);
            let channels = Arc::clone(&channels);
            let stop = Arc::clone(&stop);
            let child = Arc::clone(&child);
            let wanted = Arc::clone(&wanted);
            std::thread::spawn(move || run(levels, channels, stop, child, wanted))
        };
        let watch = {
            let stop = Arc::clone(&stop);
            let child = Arc::clone(&child);
            let wanted = Arc::clone(&wanted);
            let selector = selector.to_owned();
            std::thread::spawn(move || watch_monitor(wanted, child, stop, selector))
        };

        Self {
            levels,
            channels,
            stop,
            child,
            handle: Some(handle),
            watch: Some(watch),
        }
    }

    /// A tap that never runs: mock mode, where the mock's own playback
    /// meters are authoritative.
    pub fn disabled() -> Self {
        Self {
            levels: Arc::new(Mutex::new([0.0; MAX_CHANNELS])),
            channels: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(true)),
            child: Arc::new(Mutex::new(None)),
            handle: None,
            watch: None,
        }
    }

    /// Peak levels since the last call, plus the monitor's channel count
    /// (0 while no monitor is being captured). Levels past the channel
    /// count are always zero; the UI gates availability on the count.
    pub fn drain(&self) -> ([f32; MAX_CHANNELS], usize) {
        let channels = self.channels.load(Ordering::Relaxed);
        let Ok(mut peaks) = self.levels.lock() else {
            return ([0.0; MAX_CHANNELS], channels);
        };
        let out = *peaks;
        *peaks = [0.0; MAX_CHANNELS];
        (out, channels)
    }
}

impl Drop for InputTap {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Killing the child closes its stdout, which unblocks the reader
        // thread even if it is waiting for the next period.
        if let Ok(mut slot) = self.child.lock() {
            if let Some(mut child) = slot.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if let Some(watch) = self.watch.take() {
            let _ = watch.join();
        }
    }
}

/// Worker loop: wait for the watchdog's current monitor, spawn a capture
/// for it, read until the child exits or the app stops, then retry. If
/// the watchdog replaced the monitor while the child was reading, it
/// also killed the child, so the loop restarts immediately with the new
/// one.
fn run(
    levels: Arc<Mutex<[f32; MAX_CHANNELS]>>,
    channels: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    slot: Arc<Mutex<Option<Child>>>,
    wanted: Arc<Mutex<Option<Monitor>>>,
) {
    while !stop.load(Ordering::Relaxed) {
        let Some(monitor) = wanted.lock().ok().and_then(|w| w.clone()) else {
            sleep_interruptible(&stop, Duration::from_millis(200));
            continue;
        };
        let Some(mut child) = spawn_record(&monitor) else {
            sleep_interruptible(&stop, RETRY);
            continue;
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            sleep_interruptible(&stop, RETRY);
            continue;
        };
        // A profile switch between the clone above and the spawn is a
        // narrow race: the watchdog may have already killed the old
        // child and moved on, so re-check and drop this one instead of
        // leaving it unlinked forever.
        let still_wanted = wanted
            .lock()
            .ok()
            .is_some_and(|w| w.as_ref() == Some(&monitor));
        if !still_wanted {
            let _ = child.kill();
            let _ = child.wait();
            continue;
        }
        channels.store(monitor.channels, Ordering::Relaxed);
        if let Ok(mut slot) = slot.lock() {
            *slot = Some(child);
        }
        read_peaks(stdout, &levels, monitor.channels, &stop);
        channels.store(0, Ordering::Relaxed);
        if let Ok(mut slot) = slot.lock() {
            if let Some(mut child) = slot.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let unchanged = wanted
            .lock()
            .ok()
            .is_some_and(|w| w.as_ref() == Some(&monitor));
        if unchanged {
            sleep_interruptible(&stop, RETRY);
        }
    }
}

/// Keep `wanted` in sync with what Pulse/PipeWire currently exposes. A
/// profile switch replaces the sink node (and its channel count), and
/// the old `pw-record` child stays alive but unlinked, so it has to be
/// killed for the reader loop to pick the new monitor up.
fn watch_monitor(
    wanted: Arc<Mutex<Option<Monitor>>>,
    slot: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
    selector: String,
) {
    loop {
        let discovered = discover_source(&selector);
        let changed = {
            let Ok(mut wanted) = wanted.lock() else {
                return;
            };
            if *wanted != discovered {
                *wanted = discovered;
                true
            } else {
                false
            }
        };
        if changed {
            // Killing closes stdout, which unblocks `read_peaks`.
            if let Ok(mut slot) = slot.lock() {
                if let Some(mut child) = slot.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        sleep_interruptible(&stop, RETRY);
    }
}

/// Ask Pulse/PipeWire for its sink list and pick the widest RME
/// monitor. `pactl` prints one JSON object per sink; the monitor name
/// carries the card and profile, and `sample_specification` is a string
/// like `"s32le 18ch 48000Hz"`.
fn discover_source(selector: &str) -> Option<Monitor> {
    let output = Command::new("pactl")
        .args(["--format=json", "list", "sources"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_source(&output.stdout, selector)
}

fn normalise(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn parse_source(json: &[u8], selector: &str) -> Option<Monitor> {
    let sources: serde_json::Value = serde_json::from_slice(json).ok()?;
    let sources = sources.as_array()?;
    let mut best: Option<Monitor> = None;
    for source in sources {
        let Some(name) = source.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if !name.contains("RME_") || name.contains(".monitor") || !name.contains("alsa_input.") {
            continue;
        }
        if !selector.is_empty() && !normalise(name).contains(&normalise(selector)) {
            continue;
        }
        let Some(spec) = source.get("sample_specification").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some((rate, channels)) = parse_spec(spec) else {
            continue;
        };
        if channels == 0 || channels > MAX_CHANNELS {
            continue;
        }
        let Some(index) = source.get("index").and_then(|v| v.as_u64()) else {
            continue;
        };
        let candidate = Monitor {
            name: name.to_owned(),
            index: index as u32,
            rate,
            channels,
        };
        if best
            .as_ref()
            .is_none_or(|b| candidate.channels > b.channels)
        {
            best = Some(candidate);
        }
    }
    best
}

/// `"s32le 12ch 48000Hz"` -> `(48000, 12)`. Unknown tokens are ignored;
/// a missing rate or channel count is a parse failure.
fn parse_spec(spec: &str) -> Option<(u32, usize)> {
    let mut rate = None;
    let mut channels = None;
    for token in spec.split_whitespace() {
        if let Some(n) = token.strip_suffix("ch") {
            channels = n.parse::<usize>().ok();
        } else if let Some(n) = token.strip_suffix("Hz") {
            rate = n.parse::<u32>().ok();
        }
    }
    Some((rate?, channels?))
}

fn spawn_record(monitor: &Monitor) -> Option<Child> {
    Command::new("pw-record")
        .arg("--target")
        // The index (PipeWire's Pulse object serial) is the reliable
        // target while the source is suspended; a name falls back to the
        // default sink, which may be an entirely different card.
        .arg(monitor.index.to_string())
        .arg("--format")
        .arg("s32")
        // Do not pin the meter tap to the rate seen at discovery: that
        // would hold the whole graph at 48 kHz when the device switches to
        // 88.2 kHz or higher.  The tap follows the graph rate; the rate in
        // `Monitor` is still watched so the monitor is respawned when the
        // device changes speed.
        .arg("--channels")
        .arg(monitor.channels.to_string())
        .arg("--media-category")
        .arg("Capture")
        .arg("--raw")
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

/// Read raw S32LE frames from the child and accumulate per-channel
/// peaks into `levels`. Returns on EOF/error (child exit, profile
/// switch) or when the app is stopping.
fn read_peaks(
    mut out: impl Read,
    levels: &Mutex<[f32; MAX_CHANNELS]>,
    channels: usize,
    stop: &AtomicBool,
) {
    if channels == 0 {
        return;
    }
    let frame_bytes = channels * 4;
    let mut buf = vec![0u8; PERIOD_FRAMES * frame_bytes];
    while !stop.load(Ordering::Relaxed) {
        if out.read_exact(&mut buf).is_err() {
            return;
        }
        let mut local = [0f32; MAX_CHANNELS];
        add_peaks(&mut local, &buf, channels);
        if let Ok(mut peaks) = levels.lock() {
            for (slot, value) in peaks.iter_mut().zip(local.iter()) {
                if *value > *slot {
                    *slot = *value;
                }
            }
        }
    }
}

/// Fold one interleaved S32LE buffer into `local` (max absolute value
/// per channel, scaled to 0..=1). Split out so the conversion is
/// testable without a PipeWire server.
///
/// Each buffer's mean is subtracted per channel first. The source capture
/// of a suspended/idle sink carries a constant DC offset here (measured
/// live: about 0.07 FS on one channel and 0.28 FS on the other), which
/// otherwise lights the meter permanently. Removing the per-buffer mean
/// is a cheap DC blocker and also removes any very-low-frequency rumble
/// below the buffer rate, which is fine for a level meter.
fn add_peaks(local: &mut [f32; MAX_CHANNELS], bytes: &[u8], channels: usize) {
    let frame_bytes = channels * 4;
    if frame_bytes == 0 {
        return;
    }
    let frames = bytes.len() / frame_bytes;
    if frames == 0 {
        return;
    }
    let mut sums = [0f64; MAX_CHANNELS];
    for frame in bytes.chunks_exact(frame_bytes) {
        for (ch, sample) in frame.chunks_exact(4).enumerate() {
            if ch >= MAX_CHANNELS {
                break;
            }
            sums[ch] += i32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]) as f64;
        }
    }
    let means = sums.map(|sum| sum / frames as f64);
    for frame in bytes.chunks_exact(frame_bytes) {
        for (ch, sample) in frame.chunks_exact(4).enumerate() {
            if ch >= MAX_CHANNELS {
                break;
            }
            let value =
                i32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]) as f64 - means[ch];
            let level = (value.abs() / i32::MAX as f64) as f32;
            if level > local[ch] {
                local[ch] = level;
            }
        }
    }
}

/// Sleep in short steps so `Drop` does not wait the full retry delay.
fn sleep_interruptible(stop: &AtomicBool, total: Duration) {
    let mut left = total;
    while !stop.load(Ordering::Relaxed) && !left.is_zero() {
        let step = left.min(Duration::from_millis(100));
        std::thread::sleep(step);
        left -= step;
    }
}

#[cfg(test)]
mod tests {
    use super::{add_peaks, parse_source, parse_spec, MAX_CHANNELS};

    #[test]
    fn picks_the_widest_rme_sink() {
        let json = br#"[
            {"index":11,"name":"alsa_output.pci-0.HiFi",
             "sample_specification":"s32le 2ch 48000Hz"},
            {"index":2628,"name":"alsa_input.usb-RME_Babyface_Pro__x.mono-fallback",
             "sample_specification":"s32le 2ch 48000Hz"},
            {"index":2629,"name":"alsa_input.usb-RME_Babyface_Pro__x.pro-input-0",
             "sample_specification":"s32le 12ch 48000Hz"},
            {"index":2630,"name":"alsa_input.usb-RME_Fireface_UCX__x.pro-input-0",
             "sample_specification":"s32le 18ch 48000Hz"}
        ]"#;
        let monitor = parse_source(json, "Fireface UCX").expect("an RME sink");
        assert_eq!(monitor.index, 2630);
        assert_eq!(monitor.channels, 18);
        assert_eq!(monitor.rate, 48_000);
        assert!(monitor.name.contains("Fireface_UCX"));
    }

    #[test]
    fn ignores_non_rme_sinks() {
        let json = br#"[
            {"index":11,"name":"alsa_output.pci-0.HiFi",
             "sample_specification":"s32le 2ch 48000Hz"},
            {"index":12,"name":"alsa_output.usb-Other",
             "sample_specification":"s32le 12ch 48000Hz"}
        ]"#;
        assert!(parse_source(json, "Fireface").is_none());
    }

    #[test]
    fn selects_the_requested_unit_not_the_widest_other_card() {
        let json = br#"[
            {"index":2629,"name":"alsa_input.usb-RME_Fireface_UC_Win__22946692__303004A897837A8-00.pro-input-0",
             "sample_specification":"s32le 18ch 48000Hz"},
            {"index":2630,"name":"alsa_input.usb-RME_Fireface_UCX__23455873__99A75B559E6A6C8-00.pro-input-0",
             "sample_specification":"s32le 18ch 48000Hz"}
        ]"#;
        let monitor = parse_source(json, "303004A897837A8").expect("the UC source");
        assert_eq!(monitor.index, 2629);
        assert!(monitor.name.contains("Fireface_UC_Win"));
        assert!(parse_source(json, "missing").is_none());
    }

    #[test]
    fn parses_the_sample_specification() {
        assert_eq!(parse_spec("s32le 2ch 48000Hz"), Some((48_000, 2)));
        assert_eq!(parse_spec("s32le 12ch 96000Hz"), Some((96_000, 12)));
        assert_eq!(parse_spec("garbage"), None);
    }

    #[test]
    fn folds_interleaved_samples_into_per_channel_peaks() {
        // Three frames: ch0 alternates +/-1000 (mean 0, peak 1000), ch1
        // is a constant 500 (pure DC, must be removed).
        let mut bytes = Vec::new();
        for (left, right) in [(1000i32, 500i32), (0, 500), (-1000, 500)] {
            bytes.extend_from_slice(&left.to_le_bytes());
            bytes.extend_from_slice(&right.to_le_bytes());
        }
        let mut peaks = [0.0f32; MAX_CHANNELS];
        add_peaks(&mut peaks, &bytes, 2);
        assert!((peaks[0] - 1000.0 / i32::MAX as f32).abs() < 1e-9);
        assert_eq!(peaks[1], 0.0);
        assert_eq!(peaks[2], 0.0);
    }

    #[test]
    fn keeps_a_full_scale_alternating_signal() {
        let mut bytes = Vec::new();
        for value in [i32::MAX, i32::MIN, i32::MAX, i32::MIN] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let mut peaks = [0.0f32; MAX_CHANNELS];
        add_peaks(&mut peaks, &bytes, 1);
        assert!(peaks[0] > 0.99);
    }
}
