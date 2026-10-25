//! The Hackerman background agent: it finds and pairs with other devices, keeps
//! connections to them, and decides which device gets this computer's keyboard
//! and mouse.
//!
//! It runs as its own process, started by the app or from the command line.

mod clipboard;
mod config;
mod engine;
mod files;
pub mod lock_screen;
mod pairing;
mod screens;
mod session;
mod shared;

use std::fs::File;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

pub use config::{DEFAULT_PORT, default_config_dir};
pub use engine::{Options, run};

const STOPPING_TIMEOUT: Duration = Duration::from_secs(2);

/// The agent's normal settings, for this user and this computer.
pub fn default_options() -> Result<Options> {
    Ok(Options {
        config_dir: default_config_dir()?,
        port: None,
        channel: hk_ipc::default_channel_name(),
        capture: true,
        discovery: true,
        headless_pairing: false,
    })
}

/// Runs the agent the way the app starts it: with no console, logging to a file.
pub fn run_in_background() -> Result<()> {
    run_logging_to_file(default_options()?, hk_platform::native()?)
}

/// Runs an agent with no console, logging to a file in its own folder.
pub fn run_logging_to_file(options: Options, platform: impl hk_platform::Platform) -> Result<()> {
    std::fs::create_dir_all(&options.config_dir)
        .with_context(|| format!("can't create {}", options.config_dir.display()))?;
    let log = File::create(options.config_dir.join("agent.log")).context("can't create the log")?;
    tracing_subscriber::fmt()
        .with_env_filter(log_filter())
        .with_ansi(false)
        .with_writer(Arc::new(log))
        .init();

    let runtime = tokio::runtime::Runtime::new().context("can't start the async runtime")?;
    let result = runtime.block_on(async {
        let result = run(options, platform).await;
        if let Err(error) = &result {
            tracing::error!("{error:#}");
        }
        result
    });
    // Reading a file can be waiting on a disk or another program, and quitting doesn't
    // wait long for that.
    runtime.shutdown_timeout(STOPPING_TIMEOUT);
    result
}

/// Log level from `RUST_LOG` (e.g. `RUST_LOG=debug`), defaulting to info.
pub fn log_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}
