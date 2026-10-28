//! `tuxmix-core` — Abstraction layer for RME audio interfaces on Linux.
//!
//! This crate provides a hardware-agnostic interface for controlling
//! RME audio interfaces through ALSA mixer controls.
//!
//! # Design
//!
//! The main entrypoint is the [`RmeDevice`] trait, implemented by each
//! supported RME interface (e.g. [`BabyfacePro`]).
//!
//! A device exposes:
//! - [`InputChannel`]s (physical hardware inputs)
//! - [`PlaybackChannel`]s (software playback streams)
//! - Global [`DeviceSettings`] (clock, SPDIF, etc.)

#[cfg(feature = "alsa")]
pub mod babyface;
#[cfg(feature = "alsa")]
pub mod fireface;
#[cfg(feature = "alsa")]
pub mod ucx_alsa;
#[cfg(feature = "alsa")]
pub(crate) mod capture_meter;
pub mod capabilities;
pub mod channel;
pub(crate) mod curves;
pub mod device;
pub mod error;
#[cfg(feature = "alsa")]
pub mod mixer;
pub mod mock;
pub mod panel;
pub mod profile;
pub mod profiles;
pub mod pw_clock;
pub mod pw_input;
pub mod pw_meter;
pub mod scene;
#[cfg(feature = "usb")]
pub mod usb;

#[cfg(feature = "alsa")]
pub use babyface::BabyfacePro;
#[cfg(feature = "alsa")]
pub use fireface::{DeviceIdentity as FirefaceIdentity, DeviceInfo as FirefaceInfo, Fireface, Model as FirefaceModel};
#[cfg(feature = "alsa")]
pub use ucx_alsa::UcxAlsa;
pub use capabilities::DeviceCapabilities;
pub use channel::{
    ChannelId, ChannelType, Dynamics, Echo, InputChannel, InputLevel, OutputChannel,
    PlaybackChannel, Reverb, Sensitivity,
};
pub use device::{DeviceSettings, RmeDevice};
pub use error::Error;
#[cfg(feature = "alsa")]
pub use mixer::AlsaMixer;
pub use mock::MockBabyfacePro;
pub use scene::Scene;
#[cfg(feature = "usb")]
pub use usb::BabyfaceProUsb;
