//! Notification delivery (US6, contracts/notifications.md): channels read from
//! a mounted Secret directory, one payload shape per provider, and a bounded,
//! best-effort queue. Implements the core `Notifier` port.
//!
//! A channel URL is a credential. It is held in [`Secret`] and never appears
//! in `Debug` output, errors or logs; channels are named by their file name.

mod channels;
pub mod payload;
mod secret;
mod worker;

pub use channels::{Channel, ChannelError, ChannelSet, Provider, load_dir, parse_channel};
pub use secret::Secret;
pub use worker::{BuildError, Delivery, NotifyHandle, Report, Settings};
