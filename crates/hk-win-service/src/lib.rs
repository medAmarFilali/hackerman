//! Lets Hackerman work at the lock screen and the sign-in screen. It's optional, and turned
//! on for each PC in Settings, since it needs administrator permission and means that using
//! a paired PC is like sitting at this one's keyboard, even at the sign-in screen.
//!
//! Windows keeps those screens on a protected desktop that only the system itself can type
//! into. So three programs take turns, all of them this same file:
//!
//! - The **app and agent** run as the user, as they always have. When the screen locks, the
//!   agent stops, and the app waits for it to unlock.
//! - The **service** runs as the system from Program Files, starting with Windows. It
//!   watches the screen, keeps a copy of the settings of the user who set it up, and starts and
//!   stops the helper. It never touches the network or any input.
//! - The **helper** is the agent again, running as the system on the protected desktop in
//!   place of the user's, with the same identity on the same port. To other PCs, this PC
//!   simply reconnects. It only receives keyboard and mouse input.
//!
//! The service has to run from Program Files. Hackerman itself is installed in a folder the
//! user's programs can write to, so a service running from there would let any of them run
//! anything as the system. For the same reason, an administrator has to approve each device
//! that can reach the lock screen: see the [`approval`] module.

#![cfg(windows)]

mod approval;
mod helper;
mod install;
mod paths;
mod supervise;

use anyhow::{Context, Result};

pub use install::{
    Elevated, Status, install_and_report, remove_and_report, set_up_for_this_user, status,
    take_down,
};
pub use paths::program_folder;
pub use supervise::run_service;

/// Starting the program with this runs it as the Windows service.
pub const SERVICE_ARGUMENT: &str = "--service";
/// Sets up the service. Needs administrator rights.
pub const INSTALL_ARGUMENT: &str = "--install-lock-screen";
/// Removes the service and everything it keeps. Needs administrator rights.
pub const REMOVE_ARGUMENT: &str = "--remove-lock-screen";
/// Along with the agent argument, runs the agent as the helper.
pub const HELPER_ARGUMENT: &str = "--lock-screen";

const HELPER_CHANNEL: &str = r"\\.\pipe\hackerman-lock-screen";

/// Runs as the helper. Where it keeps things and how it's reached can be given for testing.
pub fn run_helper(arguments: &[String]) -> Result<()> {
    let value = |name: &str| {
        arguments
            .iter()
            .position(|argument| argument == name)
            .and_then(|at| arguments.get(at + 1))
    };
    let config_dir = match value("--config-dir") {
        Some(dir) => dir.into(),
        None => paths::helper_settings()?,
    };
    let channel = value("--channel").map_or(HELPER_CHANNEL, String::as_str);
    let mut options = hk_daemon::lock_screen::options(config_dir, channel.to_string());
    if let Some(port) = value("--port") {
        options.port = Some(port.parse().context("invalid --port")?);
    }
    if arguments
        .iter()
        .any(|argument| argument == "--no-discovery")
    {
        options.discovery = false;
    }
    let stays_when_unlocked = arguments
        .iter()
        .any(|argument| argument == "--stay-when-unlocked");
    hk_daemon::run_logging_to_file(
        options,
        hk_platform::native_for_lock_screen(stays_when_unlocked)?,
    )
}
