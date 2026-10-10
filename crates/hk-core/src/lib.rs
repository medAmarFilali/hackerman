//! Platform-independent logic for Hackerman: device identity, geometry, input
//! events, the monitor layout, and the logic that decides which device receives
//! input.
//!
//! Nothing in this crate talks to the operating system, so all of it can be
//! unit tested on any machine.

#![forbid(unsafe_code)]

pub mod apps;
pub mod clipboard;
pub mod device;
pub mod geometry;
pub mod input;
pub mod layout;
pub mod rules;
pub mod switcher;
