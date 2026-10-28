//! Userspace Fireface UC/UCX control backend over `snd-hwdep`.
//!
//! The kernel driver is a transport core.  This module is the device model:
//! it speaks the UC vendor-request protocol and the UCX IF1 register
//! protocol, decodes meters, and implements [`crate::device::RmeDevice`].
//!
//! See `docs/PROTOCOL-COMPLETE.md` and `docs/HWDEP-ABI.md`.

pub mod abi;
pub mod device;
pub mod hwdep;
pub mod models;
pub mod protocol;
pub mod settings;

pub use device::{ControlTransport, DeviceIdentity, DeviceInfo, Fireface, Model, ReadableState};
pub use models::ModelSpec;
pub use hwdep::{class_compliant_cards, enumerate, ClassCompliantCard, Direction, Event, Hwdep};
pub use settings::ChannelSettings;
