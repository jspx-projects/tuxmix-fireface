//! OSC (Open Sound Control) control surface.
//!
//! Lets an external tool — a hardware controller, a script, a second UI —
//! read and write mixer state over UDP alongside the GUI. Opt-in via
//! `--osc` (see `main.rs`); off by default, and bound to loopback only, so
//! enabling it never silently exposes control to the LAN.
//!
//! Address space (v2 — the whole mixer and device-control surface):
//!
//! Matrix channels:
//! - `/input/<id>/volume/<out>` f   `/playback/<id>/volume/<out>` f
//! - `/input/<id>/pan/<out>` f      `/playback/<id>/pan/<out>` f  (-100..100)
//! - `/input|playback|output/<id>/mute` f|i|T|F
//! - `/input|playback|output/<id>/solo` f|i|T|F
//! - `/output/<id>/volume` f
//! - `/input|playback|output/<id>/stereo` bool
//!
//! Input controls:
//! - `/input/<id>/gain` f (raw preamp units, matches the GUI)
//! - `/input/<id>/phantom|pad|instrument|phase|sensitivity` bool
//! - `/input/<id>/trim` f (dB)
//! - `/input/<id>/eq` bool
//! - `/input/<id>/eq/<band>/type` int|string (0/off,1/bell,2/lowshelf,3/highshelf)
//! - `/input/<id>/eq/<band>/freq|gain|q` f
//! - `/input/<id>/lowcut` bool
//! - `/input/<id>/lowcut/freq` f
//! - `/input/<id>/lowcut/slope` int (6/12/18/24)
//!
//! Output controls:
//! - `/output/<id>/loopback` bool
//! - `/output/<id>/cue` bool
//!
//! Global / navigation:
//! - `/dim`, `/recall`, `/speaker_b`, `/mono`, `/mute_fx`,
//!   `/input_link`, `/eq_for_record`, `/optical_out_spdif`,
//!   `/spdif_professional`, `/word_clock_single_speed`, `/ms_proc`,
//!   `/an12`, `/pitch`, `/width` (bool/f)
//! - `/clock_source` string; `/sample_rate` int; `/line_in_ref`,
//!   `/line_out_ref`, `/phones_ref` int
//! - `/submix` int (internal output-pair index; 0 = Phones on UCX)
//! - `/snapshot/<1-8>/recall`, `/snapshot/<1-8>/store`, `/save`
//! - `/meters` — reply with `/meter/input|playback|output/<id>` f
//!
//! Channel indices are the internal ones (the same ids the GUI's scenes
//! use).  Output 0 is Phones on the UCX; `/submix` follows that order.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use iced::futures::channel::mpsc;
use iced::futures::{SinkExt, Stream, StreamExt};
use iced::stream;
use rosc::{OscMessage, OscPacket, OscType};
use tokio::net::UdpSocket;

use tuxmix_core::channel::{EqBandType, InputLevel};
use tuxmix_core::ChannelId;

use crate::app::Message;

/// Bind/target ports for the OSC bridge — `Hash` because it's threaded
/// through `Subscription::run_with` as the recipe's identifying data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OscConfig {
    pub recv_port: u16,
    pub send_port: u16,
    pub send_host: IpAddr,
}

/// A control command decoded from an incoming OSC message.
#[derive(Debug, Clone, PartialEq)]
pub enum OscCommand {
    Volume(ChannelId, usize, f32),
    Pan(ChannelId, usize, i8),
    Mute(ChannelId, bool),
    Solo(ChannelId, bool),
    OutputVolume(usize, f32),
    Gain(usize, u32),
    Phantom(usize, bool),
    Pad(usize, bool),
    Instrument(usize, bool),
    Sensitivity(usize, bool),
    Level(usize, u16),
    Autoset(usize, bool),
    Phase(usize, bool),
    Trim(usize, f32),
    EqEnabled(ChannelId, bool),
    EqBandType(ChannelId, usize, EqBandType),
    EqBandFreq(ChannelId, usize, u16),
    EqBandQ(ChannelId, usize, f32),
    EqBandGain(ChannelId, usize, f32),
    LowCutEnabled(ChannelId, bool),
    LowCutFreq(ChannelId, u16),
    LowCutSlope(ChannelId, u8),
    Loopback(usize, bool),
    Cue(usize, bool),
    FxReturn(usize, f32),
    Stereo(ChannelId, bool),
    Dim(bool),
    SpeakerB(bool),
    ControlRoomRecall,
    Mono(bool),
    MuteFx(bool),
    InputLink(bool),
    EqForRecord(bool),
    OpticalOutSpdif(bool),
    SpdifProfessional(bool),
    WordClockSingleSpeed(bool),
    MsProc(bool),
    An12(bool),
    Pitch(f32),
    Width(f32),
    ClockSource(String),
    SampleRate(u32),
    LineInRef(u16),
    LineOutRef(u16),
    PhonesRef(u16),
    Submix(usize),
    SnapshotRecall(u8),
    SnapshotStore(u8),
    SaveNow,
    Meters,
}

/// A state change to echo out to any connected OSC client — sent both for
/// GUI-originated changes and for changes that came in as an `OscCommand`,
/// so a controller and the GUI never drift out of sync with each other.
#[derive(Debug, Clone, PartialEq)]
pub enum OscOutbound {
    Volume(ChannelId, usize, f32),
    Pan(ChannelId, usize, i8),
    Mute(ChannelId, bool),
    Solo(ChannelId, bool),
    OutputVolume(usize, f32),
    /// Any of the v2 controls.  Address and arguments are already in
    /// their wire form, which keeps the feedback path from growing a
    /// variant per knob.
    Raw {
        addr: String,
        args: Vec<OscType>,
    },
}

fn channel_prefix(id: ChannelId) -> (&'static str, usize) {
    match id {
        ChannelId::Input(i) => ("input", i),
        ChannelId::Playback(i) => ("playback", i),
        ChannelId::Output(i) => ("output", i),
    }
}

/// input/playback/output — everywhere mute/solo are addressable.
fn parse_any_channel(kind: &str, id: usize) -> Option<ChannelId> {
    match kind {
        "input" => Some(ChannelId::Input(id)),
        "playback" => Some(ChannelId::Playback(id)),
        "output" => Some(ChannelId::Output(id)),
        _ => None,
    }
}

/// input/playback only — outputs have no per-output-pair matrix routing
/// (`OutputChannel` is a single scalar volume, not a `Vec` per pair), so
/// `/output/<id>/volume/<out>` is deliberately not a valid address.
fn parse_matrix_channel(kind: &str, id: usize) -> Option<ChannelId> {
    match kind {
        "input" => Some(ChannelId::Input(id)),
        "playback" => Some(ChannelId::Playback(id)),
        _ => None,
    }
}

/// input/output only — the UCX DSP runs on hardware inputs and outputs,
/// not on software playbacks.
fn parse_eq_channel(kind: &str, id: usize) -> Option<ChannelId> {
    match kind {
        "input" => Some(ChannelId::Input(id)),
        "output" => Some(ChannelId::Output(id)),
        _ => None,
    }
}

fn arg_f32(v: &OscType) -> Option<f32> {
    match v {
        OscType::Float(f) => Some(*f),
        OscType::Double(d) => Some(*d as f32),
        OscType::Int(i) => Some(*i as f32),
        _ => None,
    }
}

fn arg_bool(v: &OscType) -> Option<bool> {
    match v {
        OscType::Bool(b) => Some(*b),
        OscType::Int(i) => Some(*i != 0),
        OscType::Float(f) => Some(*f >= 0.5),
        _ => None,
    }
}

fn arg_i32(v: &OscType) -> Option<i32> {
    match v {
        OscType::Int(i) => Some(*i),
        OscType::Float(f) => Some(f.round() as i32),
        OscType::Double(d) => Some(d.round() as i32),
        _ => None,
    }
}

fn arg_str(v: &OscType) -> Option<&str> {
    match v {
        OscType::String(s) => Some(s),
        _ => None,
    }
}

fn arg_band_type(v: &OscType) -> Option<EqBandType> {
    if let Some(s) = arg_str(v) {
        return match s.to_ascii_lowercase().as_str() {
            "off" => Some(EqBandType::Off),
            "bell" | "peak" => Some(EqBandType::Bell),
            "lowshelf" | "low" => Some(EqBandType::LowShelf),
            "highshelf" | "high" => Some(EqBandType::HighShelf),
            _ => None,
        };
    }
    match arg_i32(v)? {
        0 => Some(EqBandType::Off),
        1 => Some(EqBandType::Bell),
        2 => Some(EqBandType::LowShelf),
        3 => Some(EqBandType::HighShelf),
        _ => None,
    }
}

impl OscCommand {
    pub fn parse(addr: &str, args: &[OscType]) -> Option<Self> {
        let parts: Vec<&str> = addr.split('/').filter(|s| !s.is_empty()).collect();
        match parts.as_slice() {
            [kind, id, "volume", out] => {
                let id: usize = id.parse().ok()?;
                let out: usize = out.parse().ok()?;
                let v = arg_f32(args.first()?)?;
                Some(OscCommand::Volume(
                    parse_matrix_channel(kind, id)?,
                    out,
                    v.clamp(0.0, 2.0),
                ))
            }
            [kind, id, "pan", out] => {
                let id: usize = id.parse().ok()?;
                let out: usize = out.parse().ok()?;
                let v = arg_f32(args.first()?)?.clamp(-100.0, 100.0);
                Some(OscCommand::Pan(
                    parse_matrix_channel(kind, id)?,
                    out,
                    v as i8,
                ))
            }
            [kind, id, "mute"] => {
                let id: usize = id.parse().ok()?;
                let v = arg_bool(args.first()?)?;
                Some(OscCommand::Mute(parse_any_channel(kind, id)?, v))
            }
            [kind, id, "solo"] => {
                let id: usize = id.parse().ok()?;
                let v = arg_bool(args.first()?)?;
                Some(OscCommand::Solo(parse_any_channel(kind, id)?, v))
            }
            ["output", id, "volume"] => {
                let id: usize = id.parse().ok()?;
                let v = arg_f32(args.first()?)?;
                Some(OscCommand::OutputVolume(id, v.clamp(0.0, 2.0)))
            }
            [kind, id, "stereo"] => {
                let id: usize = id.parse().ok()?;
                let on = arg_bool(args.first()?)?;
                Some(OscCommand::Stereo(parse_any_channel(kind, id)?, on))
            }
            ["input", id, "gain"] => {
                let id: usize = id.parse().ok()?;
                let v = arg_f32(args.first()?)?.max(0.0).round().min(65.0);
                Some(OscCommand::Gain(id, v as u32))
            }
            ["input", id, "phantom"] => {
                Some(OscCommand::Phantom(id.parse().ok()?, arg_bool(args.first()?)?))
            }
            ["input", id, "pad"] => {
                Some(OscCommand::Pad(id.parse().ok()?, arg_bool(args.first()?)?))
            }
            ["input", id, "instrument"] => Some(OscCommand::Instrument(
                id.parse().ok()?,
                arg_bool(args.first()?)?,
            )),
            ["input", id, "level"] => Some(OscCommand::Level(
                id.parse().ok()?,
                arg_i32(args.first()?)? as u16,
            )),
            ["input", id, "autoset"] => Some(OscCommand::Autoset(
                id.parse().ok()?,
                arg_bool(args.first()?)?,
            )),
            ["input", id, "sensitivity"] => Some(OscCommand::Sensitivity(
                id.parse().ok()?,
                arg_bool(args.first()?)?,
            )),
            ["input", id, "phase"] => {
                Some(OscCommand::Phase(id.parse().ok()?, arg_bool(args.first()?)?))
            }
            ["input", id, "trim"] => {
                Some(OscCommand::Trim(id.parse().ok()?, arg_f32(args.first()?)?))
            }
            [kind, id, "eq"] => {
                Some(OscCommand::EqEnabled(parse_eq_channel(kind, id.parse().ok()?)?, arg_bool(args.first()?)?))
            }
            [kind, id, "eq", band, "type"] => Some(OscCommand::EqBandType(
                parse_eq_channel(kind, id.parse().ok()?)?,
                band.parse().ok()?,
                arg_band_type(args.first()?)?,
            )),
            [kind, id, "eq", band, "freq"] => Some(OscCommand::EqBandFreq(
                parse_eq_channel(kind, id.parse().ok()?)?,
                band.parse().ok()?,
                arg_f32(args.first()?)?.round().clamp(20.0, 20_000.0) as u16,
            )),
            [kind, id, "eq", band, "q"] => Some(OscCommand::EqBandQ(
                parse_eq_channel(kind, id.parse().ok()?)?,
                band.parse().ok()?,
                arg_f32(args.first()?)?.clamp(0.05, 10.0),
            )),
            [kind, id, "eq", band, "gain"] => Some(OscCommand::EqBandGain(
                parse_eq_channel(kind, id.parse().ok()?)?,
                band.parse().ok()?,
                arg_f32(args.first()?)?.clamp(-24.0, 24.0),
            )),
            [kind, id, "lowcut"] => Some(OscCommand::LowCutEnabled(
                parse_eq_channel(kind, id.parse().ok()?)?,
                arg_bool(args.first()?)?,
            )),
            [kind, id, "lowcut", "freq"] => Some(OscCommand::LowCutFreq(
                parse_eq_channel(kind, id.parse().ok()?)?,
                arg_f32(args.first()?)?.round().clamp(20.0, 500.0) as u16,
            )),
            [kind, id, "lowcut", "slope"] => {
                let slope = arg_i32(args.first()?)?;
                if ![6, 12, 18, 24].contains(&slope) {
                    return None;
                }
                Some(OscCommand::LowCutSlope(parse_eq_channel(kind, id.parse().ok()?)?, slope as u8))
            }
            ["output", id, "loopback"] => Some(OscCommand::Loopback(
                id.parse().ok()?,
                arg_bool(args.first()?)?,
            )),
            ["output", id, "cue"] => {
                Some(OscCommand::Cue(id.parse().ok()?, arg_bool(args.first()?)?))
            }
            ["output", id, "fxreturn"] => Some(OscCommand::FxReturn(
                id.parse().ok()?,
                arg_f32(args.first()?)?,
            )),
            ["dim"] => Some(OscCommand::Dim(arg_bool(args.first()?)?)),
            ["speaker_b"] => Some(OscCommand::SpeakerB(arg_bool(args.first()?)?)),
            ["recall"] => Some(OscCommand::ControlRoomRecall),
            ["mono"] => Some(OscCommand::Mono(arg_bool(args.first()?)?)),
            ["mute_fx"] => Some(OscCommand::MuteFx(arg_bool(args.first()?)?)),
            ["input_link"] => Some(OscCommand::InputLink(arg_bool(args.first()?)?)),
            ["eq_for_record"] => Some(OscCommand::EqForRecord(arg_bool(args.first()?)?)),
            ["optical_out_spdif"] => {
                Some(OscCommand::OpticalOutSpdif(arg_bool(args.first()?)?))
            }
            ["spdif_professional"] => {
                Some(OscCommand::SpdifProfessional(arg_bool(args.first()?)?))
            }
            ["word_clock_single_speed"] => Some(OscCommand::WordClockSingleSpeed(
                arg_bool(args.first()?)?,
            )),
            ["ms_proc"] => Some(OscCommand::MsProc(arg_bool(args.first()?)?)),
            ["an12"] => Some(OscCommand::An12(arg_bool(args.first()?)?)),
            ["pitch"] => Some(OscCommand::Pitch(arg_f32(args.first()?)?)),
            ["width"] => Some(OscCommand::Width(arg_f32(args.first()?)?)),
            ["clock_source"] => Some(OscCommand::ClockSource(arg_str(args.first()?)?.into())),
            ["sample_rate"] => Some(OscCommand::SampleRate(arg_i32(args.first()?)? as u32)),
            ["line_in_ref"] => Some(OscCommand::LineInRef(arg_i32(args.first()?)? as u16)),
            ["line_out_ref"] => Some(OscCommand::LineOutRef(arg_i32(args.first()?)? as u16)),
            ["phones_ref"] => Some(OscCommand::PhonesRef(arg_i32(args.first()?)? as u16)),
            ["submix"] => Some(OscCommand::Submix(arg_i32(args.first()?)? as usize)),
            ["snapshot", n, "recall"] => {
                let n: u8 = n.parse().ok()?;
                (1..=8).contains(&n).then_some(OscCommand::SnapshotRecall(n))
            }
            ["snapshot", n, "store"] => {
                let n: u8 = n.parse().ok()?;
                (1..=8).contains(&n).then_some(OscCommand::SnapshotStore(n))
            }
            ["save"] => Some(OscCommand::SaveNow),
            ["meters"] => Some(OscCommand::Meters),
            _ => None,
        }
    }

    /// Turn the wire command into the ordinary GUI message, so OSC goes
    /// through the same handlers, undo/redo, host-scene merge and device
    /// writes as a click or key press.
    pub fn into_message(self) -> Option<Message> {
        Some(match self {
            OscCommand::Volume(cid, out, v) => Message::VolumeChanged(cid, out, v),
            OscCommand::Pan(cid, out, p) => Message::PanChanged(cid, out, p),
            OscCommand::Mute(cid, m) => Message::Mute(cid, m),
            OscCommand::Solo(cid, s) => Message::Solo(cid, s),
            OscCommand::OutputVolume(id, v) => {
                Message::VolumeChanged(ChannelId::Output(id), id, v)
            }
            OscCommand::Gain(idx, g) => Message::Gain(idx, g),
            OscCommand::Phantom(idx, on) => Message::Phantom(idx, on),
            OscCommand::Pad(idx, on) => Message::Pad(idx, on),
            OscCommand::Instrument(idx, on) => Message::Instrument(idx, on),
            OscCommand::Level(idx, code) => Message::LevelChanged(
                idx,
                InputLevel::from_code(code).unwrap_or(InputLevel::Plus4dBu),
            ),
            OscCommand::Autoset(idx, on) => Message::Autoset(idx, on),
            OscCommand::Sensitivity(idx, plus4) => Message::Sensitivity(idx, plus4),
            OscCommand::Phase(idx, on) => Message::PhaseChanged(ChannelId::Input(idx), on),
            OscCommand::Trim(idx, db) => Message::TrimChanged(idx, db),
            OscCommand::EqEnabled(cid, on) => Message::EqEnabled(cid, on),
            OscCommand::EqBandType(cid, band, t) => Message::EqBandType(cid, band, t),
            OscCommand::EqBandFreq(cid, band, f) => Message::EqBandFreq(cid, band, f),
            OscCommand::EqBandQ(cid, band, q) => Message::EqBandQ(cid, band, q),
            OscCommand::EqBandGain(cid, band, g) => Message::EqBandGain(cid, band, g),
            OscCommand::LowCutEnabled(cid, on) => Message::EqLowCutEnabled(cid, on),
            OscCommand::LowCutFreq(cid, f) => Message::EqLowCutFreq(cid, f),
            OscCommand::LowCutSlope(cid, s) => Message::EqLowCutSlope(cid, s),
            OscCommand::Loopback(idx, on) => Message::LoopbackChanged(idx, on),
            OscCommand::Cue(idx, on) => Message::CueChanged(ChannelId::Output(idx), on),
            OscCommand::FxReturn(idx, db) => Message::FxReturnChanged(idx, db),
            OscCommand::Stereo(cid, on) => Message::StereoLinkChanged(cid, on),
            OscCommand::Dim(on) => Message::Dim(on),
            OscCommand::SpeakerB(on) => Message::SpeakerB(on),
            OscCommand::ControlRoomRecall => Message::ControlRoomRecall,
            OscCommand::Mono(on) => Message::ControlRoomMono(on),
            OscCommand::MuteFx(on) => Message::ControlRoomMuteFx(on),
            OscCommand::InputLink(on) => Message::InputLinkChanged(on),
            OscCommand::EqForRecord(on) => Message::EqForRecordChanged(on),
            OscCommand::OpticalOutSpdif(on) => Message::OpticalOutFormatChanged(on),
            OscCommand::SpdifProfessional(on) => Message::SpdifProfessionalChanged(on),
            OscCommand::WordClockSingleSpeed(on) => Message::WordClockSingleSpeedChanged(on),
            OscCommand::MsProc(on) => Message::MsProcChanged(on),
            OscCommand::An12(on) => Message::An12Changed(on),
            OscCommand::Pitch(v) => Message::PitchChanged(v),
            OscCommand::Width(v) => Message::WidthChanged(v),
            OscCommand::ClockSource(s) => Message::ClockSourceSelected(s),
            OscCommand::SampleRate(r) => Message::SampleRateSelected(r),
            OscCommand::LineInRef(v) => Message::LineInRefSelected(v),
            OscCommand::LineOutRef(v) => Message::LineOutRefSelected(v),
            OscCommand::PhonesRef(v) => Message::PhonesRefSelected(v),
            OscCommand::Submix(i) => Message::SelectOutput(i),
            OscCommand::SnapshotRecall(n) => Message::SnapshotClicked(n),
            OscCommand::SnapshotStore(n) => Message::SnapshotStoreTo(n),
            OscCommand::SaveNow => Message::SaveNow,
            OscCommand::Meters => Message::OscMeters,
        })
    }
}

impl OscOutbound {
    fn into_packet(self) -> OscPacket {
        let (addr, args) = match self {
            OscOutbound::Volume(id, out, v) => {
                if let ChannelId::Output(i) = id {
                    // Outputs have a single master, not a per-submix
                    // matrix: keep the public `/output/<id>/volume`
                    // address instead of the matrix form.
                    (format!("/output/{i}/volume"), vec![OscType::Float(v)])
                } else {
                    let (kind, i) = channel_prefix(id);
                    (format!("/{kind}/{i}/volume/{out}"), vec![OscType::Float(v)])
                }
            }
            OscOutbound::Pan(id, out, p) => {
                let (kind, i) = channel_prefix(id);
                (
                    format!("/{kind}/{i}/pan/{out}"),
                    vec![OscType::Float(p as f32)],
                )
            }
            OscOutbound::Mute(id, m) => {
                let (kind, i) = channel_prefix(id);
                (
                    format!("/{kind}/{i}/mute"),
                    vec![OscType::Float(if m { 1.0 } else { 0.0 })],
                )
            }
            OscOutbound::Solo(id, s) => {
                let (kind, i) = channel_prefix(id);
                (
                    format!("/{kind}/{i}/solo"),
                    vec![OscType::Float(if s { 1.0 } else { 0.0 })],
                )
            }
            OscOutbound::OutputVolume(id, v) => {
                (format!("/output/{id}/volume"), vec![OscType::Float(v)])
            }
            OscOutbound::Raw { addr, args } => (addr, args),
        };
        OscPacket::Message(OscMessage { addr, args })
    }
}

/// One human-readable line per OSC message crossing the bridge in either
/// direction — for `Message::OscLog`/the debug log panel in `app.rs`. Built
/// from the raw address + args, *before* `OscCommand::parse` gets a chance
/// to reject anything — so a controller sending a malformed or unmapped
/// address (e.g. a typo, or a channel index the device doesn't have) still
/// shows up here, which is the actual point of a debug log: seeing why
/// nothing happened, not just confirming when something did.
fn log_line(dir: &str, addr: &str, args: &[OscType]) -> String {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = elapsed.as_secs() % 86400;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!(
        "{h:02}:{m:02}:{s:02}.{:03} {dir} {addr} {args:?}",
        elapsed.subsec_millis()
    )
}

/// Runs the OSC bridge as an iced `Subscription` recipe: binds a UDP
/// socket on loopback, decodes incoming packets into `Message::OscCommand`,
/// and drains an internal channel (handed back to the app via
/// `Message::OscReady`) to send outgoing feedback packets. Every message in
/// either direction also gets echoed out as a `Message::OscLog` line, for
/// the debug log panel — a no-op from the bridge's own perspective if that
/// panel is never opened, since the log is a plain bounded ring buffer on
/// the `update()` side, not something this worker needs to know about.
///
/// Never touches `state.device` — the `alsa` mixer handle it wraps has no
/// `Send`/`Sync` impl, so it must stay owned by the single update-loop
/// thread. This worker only ever exchanges plain values with `update()`,
/// the same way the existing global keyboard/mouse listener does.
pub fn worker(config: &OscConfig) -> impl Stream<Item = Message> {
    let config = *config;
    stream::channel(100, async move |mut output| {
        let recv_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), config.recv_port);
        let socket = match UdpSocket::bind(recv_addr).await {
            Ok(s) => s,
            Err(err) => {
                log::error!("OSC: failed to bind {recv_addr}: {err}");
                return;
            }
        };
        let send_addr = SocketAddr::new(config.send_host, config.send_port);

        let (tx, mut rx) = mpsc::channel::<OscOutbound>(4096);
        if output.send(Message::OscReady(tx)).await.is_err() {
            return;
        }

        log::info!("OSC: listening on {recv_addr}, feedback to {send_addr}");
        let mut buf = [0u8; rosc::decoder::MTU];
        loop {
            tokio::select! {
                res = socket.recv_from(&mut buf) => {
                    let Ok((size, _from)) = res else { continue };
                    let Ok((_, packet)) = rosc::decoder::decode_udp(&buf[..size]) else { continue };
                    if let OscPacket::Message(msg) = packet {
                        let _ = output
                            .send(Message::OscLog(log_line("IN ", &msg.addr, &msg.args)))
                            .await;
                        if let Some(cmd) = OscCommand::parse(&msg.addr, &msg.args) {
                            if output.send(Message::OscCommand(cmd)).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                outbound = rx.next() => {
                    let Some(outbound) = outbound else { continue };
                    let packet = outbound.into_packet();
                    if let OscPacket::Message(ref msg) = packet {
                        let _ = output
                            .send(Message::OscLog(log_line("OUT", &msg.addr, &msg.args)))
                            .await;
                    }
                    if let Ok(bytes) = rosc::encoder::encode(&packet) {
                        let _ = socket.send_to(&bytes, send_addr).await;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_matrix_volume() {
        let cmd = OscCommand::parse("/input/2/volume/0", &[OscType::Float(0.75)]);
        assert_eq!(cmd, Some(OscCommand::Volume(ChannelId::Input(2), 0, 0.75)));
    }

    #[test]
    fn parses_playback_pan_with_int_arg() {
        let cmd = OscCommand::parse("/playback/1/pan/3", &[OscType::Int(-50)]);
        assert_eq!(cmd, Some(OscCommand::Pan(ChannelId::Playback(1), 3, -50)));
    }

    #[test]
    fn parses_mute_bool_and_float_forms() {
        assert_eq!(
            OscCommand::parse("/input/0/mute", &[OscType::Bool(true)]),
            Some(OscCommand::Mute(ChannelId::Input(0), true))
        );
        assert_eq!(
            OscCommand::parse("/output/5/mute", &[OscType::Float(1.0)]),
            Some(OscCommand::Mute(ChannelId::Output(5), true))
        );
    }

    #[test]
    fn parses_solo() {
        let cmd = OscCommand::parse("/input/4/solo", &[OscType::Int(1)]);
        assert_eq!(cmd, Some(OscCommand::Solo(ChannelId::Input(4), true)));
    }

    #[test]
    fn parses_output_volume() {
        let cmd = OscCommand::parse("/output/1/volume", &[OscType::Float(0.5)]);
        assert_eq!(cmd, Some(OscCommand::OutputVolume(1, 0.5)));
    }

    #[test]
    fn rejects_output_matrix_address() {
        // Outputs have no per-output-pair routing — `/output/<id>/volume/<out>`
        // is not a valid address, unlike inputs/playbacks.
        assert_eq!(
            OscCommand::parse("/output/1/volume/0", &[OscType::Float(0.5)]),
            None
        );
    }

    #[test]
    fn rejects_unknown_and_malformed_addresses() {
        assert_eq!(
            OscCommand::parse("/input/2/gain/0", &[OscType::Float(0.5)]),
            None
        );
        assert_eq!(
            OscCommand::parse("/input/volume/0", &[OscType::Float(0.5)]),
            None
        );
        assert_eq!(OscCommand::parse("/input/2/volume/0", &[]), None);
        assert_eq!(
            OscCommand::parse("/bogus/2/volume/0", &[OscType::Float(0.5)]),
            None
        );
    }

    #[test]
    fn parses_input_and_eq_controls() {
        assert_eq!(
            OscCommand::parse("/input/2/gain", &[OscType::Float(30.4)]),
            Some(OscCommand::Gain(2, 30))
        );
        assert_eq!(
            OscCommand::parse("/input/1/phantom", &[OscType::Bool(true)]),
            Some(OscCommand::Phantom(1, true))
        );
        assert_eq!(
            OscCommand::parse("/input/1/phase", &[OscType::Int(1)]),
            Some(OscCommand::Phase(1, true))
        );
        assert_eq!(
            OscCommand::parse("/input/0/trim", &[OscType::Float(-3.5)]),
            Some(OscCommand::Trim(0, -3.5))
        );
        assert_eq!(
            OscCommand::parse("/input/0/eq/1/type", &[OscType::String("bell".into())]),
            Some(OscCommand::EqBandType(ChannelId::Input(0), 1, EqBandType::Bell))
        );
        assert_eq!(
            OscCommand::parse("/input/0/eq/2/gain", &[OscType::Float(6.0)]),
            Some(OscCommand::EqBandGain(ChannelId::Input(0), 2, 6.0))
        );
        assert_eq!(
            OscCommand::parse("/input/0/lowcut", &[OscType::Bool(true)]),
            Some(OscCommand::LowCutEnabled(ChannelId::Input(0), true))
        );
        assert_eq!(
            OscCommand::parse("/input/0/lowcut/freq", &[OscType::Float(120.0)]),
            Some(OscCommand::LowCutFreq(ChannelId::Input(0), 120))
        );
        assert_eq!(
            OscCommand::parse("/input/0/lowcut/slope", &[OscType::Int(18)]),
            Some(OscCommand::LowCutSlope(ChannelId::Input(0), 18))
        );
        assert_eq!(
            OscCommand::parse("/output/2/eq/1/freq", &[OscType::Float(2500.0)]),
            Some(OscCommand::EqBandFreq(ChannelId::Output(2), 1, 2_500))
        );
        assert_eq!(
            OscCommand::parse("/output/2/lowcut", &[OscType::Bool(true)]),
            Some(OscCommand::LowCutEnabled(ChannelId::Output(2), true))
        );
    }

    #[test]
    fn parses_output_and_global_controls() {
        assert_eq!(
            OscCommand::parse("/output/2/loopback", &[OscType::Bool(true)]),
            Some(OscCommand::Loopback(2, true))
        );
        assert_eq!(
            OscCommand::parse("/playback/1/stereo", &[OscType::Bool(false)]),
            Some(OscCommand::Stereo(ChannelId::Playback(1), false))
        );
        assert_eq!(
            OscCommand::parse("/dim", &[OscType::Int(1)]),
            Some(OscCommand::Dim(true))
        );
        assert_eq!(
            OscCommand::parse("/clock_source", &[OscType::String("Optical".into())]),
            Some(OscCommand::ClockSource("Optical".into()))
        );
        assert_eq!(
            OscCommand::parse("/sample_rate", &[OscType::Int(96_000)]),
            Some(OscCommand::SampleRate(96_000))
        );
        assert_eq!(
            OscCommand::parse("/submix", &[OscType::Int(2)]),
            Some(OscCommand::Submix(2))
        );
        assert_eq!(
            OscCommand::parse("/snapshot/3/recall", &[]),
            Some(OscCommand::SnapshotRecall(3))
        );
        assert_eq!(
            OscCommand::parse("/snapshot/3/store", &[]),
            Some(OscCommand::SnapshotStore(3))
        );
        assert_eq!(OscCommand::parse("/save", &[]), Some(OscCommand::SaveNow));
        assert_eq!(OscCommand::parse("/meters", &[]), Some(OscCommand::Meters));
    }

    #[test]
    fn outbound_output_volume_uses_the_master_address() {
        let packet = OscOutbound::Volume(ChannelId::Output(2), 2, 0.5).into_packet();
        let bytes = rosc::encoder::encode(&packet).unwrap();
        let (_, decoded) = rosc::decoder::decode_udp(&bytes).unwrap();
        let OscPacket::Message(msg) = decoded else {
            panic!("expected a message");
        };
        assert_eq!(msg.addr, "/output/2/volume");
        assert_eq!(
            OscCommand::parse(&msg.addr, &msg.args),
            Some(OscCommand::OutputVolume(2, 0.5))
        );
    }

    #[test]
    fn raw_outbound_round_trips() {
        let packet = OscOutbound::Raw {
            addr: "/input/0/lowcut".into(),
            args: vec![OscType::Float(1.0)],
        }
        .into_packet();
        let bytes = rosc::encoder::encode(&packet).unwrap();
        let (_, decoded) = rosc::decoder::decode_udp(&bytes).unwrap();
        let OscPacket::Message(msg) = decoded else {
            panic!("expected a message");
        };
        assert_eq!(msg.addr, "/input/0/lowcut");
        assert_eq!(
            OscCommand::parse(&msg.addr, &msg.args),
            Some(OscCommand::LowCutEnabled(ChannelId::Input(0), true))
        );
    }

    #[test]
    fn outbound_round_trips_through_encode_decode() {
        let packet = OscOutbound::Volume(ChannelId::Input(3), 1, 0.42).into_packet();
        let bytes = rosc::encoder::encode(&packet).unwrap();
        let (_, decoded) = rosc::decoder::decode_udp(&bytes).unwrap();
        let OscPacket::Message(msg) = decoded else {
            panic!("expected a message");
        };
        assert_eq!(msg.addr, "/input/3/volume/1");
        let cmd = OscCommand::parse(&msg.addr, &msg.args);
        assert_eq!(cmd, Some(OscCommand::Volume(ChannelId::Input(3), 1, 0.42)));
    }
}
