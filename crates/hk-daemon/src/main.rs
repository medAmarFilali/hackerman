//! Runs the Hackerman agent from a terminal, for development.
//!
//! ```text
//! hackerman-daemon [options]
//!   --config-dir <path>  Where to keep settings and saved data
//!   --port <number>      UDP port to listen on (0 picks any free port)
//!   --channel <name>     Local channel name the app connects to
//!   --no-capture         Don't capture this computer's input
//!   --no-discovery       Don't announce this device or look for others
//!   --simulate <count>   Simulate a device with this many monitors instead of
//!                        using the real computer. Also logs pairing codes, so
//!                        it can pair with no app open.
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use hk_daemon::{Options, default_options, log_filter, run};
use hk_platform::fake::FakePlatform;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(log_filter())
        .init();

    let mut options = default_options()?;
    let simulate = parse_arguments(&mut options)?;

    match simulate {
        Some(screens) => {
            options.capture = false;
            options.headless_pairing = true;
            run_until_stopped(options, FakePlatform::with_screens(screens)).await
        }
        None => run_until_stopped(options, hk_platform::native()?).await,
    }
}

async fn run_until_stopped(options: Options, platform: impl hk_platform::Platform) -> Result<()> {
    tokio::select! {
        result = run(options, platform) => result,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("stopping");
            Ok(())
        }
    }
}

/// Applies command-line options. Returns the number of monitors to simulate, if any.
fn parse_arguments(options: &mut Options) -> Result<Option<usize>> {
    let mut simulate = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .with_context(|| format!("{argument} needs a value"))
        };
        match argument.as_str() {
            "--config-dir" => options.config_dir = PathBuf::from(value()?),
            "--port" => options.port = Some(value()?.parse().context("invalid --port")?),
            "--channel" => options.channel = value()?,
            "--no-capture" => options.capture = false,
            "--no-discovery" => options.discovery = false,
            "--simulate" => simulate = Some(value()?.parse().context("invalid --simulate")?),
            other => bail!("unknown option {other}"),
        }
    }
    Ok(simulate)
}
