//! The service itself: it watches the screen, and starts and stops the helper to match.

use std::ffi::OsString;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hk_daemon::lock_screen;
use hk_platform::session::{self, ConsoleState, HELPER_SERVICE};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use crate::approval::Approval;
use crate::helper::Helper;
use crate::paths;

/// How long the lock screen can be up before the helper is on its way.
const CHECK_EVERY: Duration = Duration::from_millis(500);
/// How long a change to the user's settings, like a forgotten device, can take to arrive.
const COPY_EVERY: Duration = Duration::from_secs(10);
/// How soon to try a helper again, after it stops or won't start.
const TRY_AGAIN_AFTER: Duration = Duration::from_secs(3);

define_windows_service!(service_entry, service_main);

/// Hands this process over to Windows, which calls back with the service running. Started any
/// other way, like from a terminal, this fails straight away.
pub fn run_service() -> Result<()> {
    service_dispatcher::start(HELPER_SERVICE, service_entry)
        .context("this only runs when Windows starts it as a service")
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = start_logging() {
        // There's nowhere to report it. The service can do its job without a log.
        drop(error);
    }
    if let Err(error) = run() {
        tracing::error!("{error:#}");
    }
}

fn start_logging() -> Result<()> {
    let log = File::create(paths::data_folder()?.join("service.log"))?;
    tracing_subscriber::fmt()
        .with_env_filter(hk_daemon::log_filter())
        .with_ansi(false)
        .with_writer(Arc::new(log))
        .init();
    Ok(())
}

fn run() -> Result<()> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(HELPER_SERVICE, handler)?;
    let set_state = |current_state, controls_accepted| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::ZERO,
            process_id: None,
        })
    };
    let accepted = ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN;
    set_state(ServiceState::Running, accepted)?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "the service started");

    supervise(&stop_rx)?;

    tracing::info!("the service stopped");
    set_state(ServiceState::Stopped, ServiceControlAccept::empty())?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Nothing,
    Start,
    Stop,
}

/// What to do about the helper, given which session is on the screen and what's going on there,
/// the session the helper is running in if it's running, and whether it has settings to run
/// from. Windows not saying about the screen means it's in the middle of something, so
/// nothing changes.
fn next_step(console: Option<(u32, ConsoleState)>, helper: Option<u32>, ready: bool) -> Step {
    let Some((session, state)) = console else {
        return Step::Nothing;
    };
    let wanted = ready && state.needs_helper();
    match helper {
        // In the wrong session, it can't reach the screen. Started again next time round.
        Some(running_in) if !wanted || running_in != session => Step::Stop,
        None if wanted => Step::Start,
        _ => Step::Nothing,
    }
}

/// Whether to bring the helper's copy of the settings up to date. It goes on doing that while
/// someone is using the computer, since it could restart at any moment, and one last time as
/// the screen locks. Nothing's changing after that, until someone is using it again.
fn copy_is_due(state: ConsoleState, previous: Option<ConsoleState>, since_copy: Duration) -> bool {
    match state {
        ConsoleState::Unlocked => since_copy >= COPY_EVERY,
        ConsoleState::Locked => previous != Some(ConsoleState::Locked),
        ConsoleState::NoUser => false,
    }
}

/// Does a job on a thread of its own, unless the last one is still going.
///
/// The settings come from a folder other programs can write to, and one of those can make
/// reading a file there take as long as it likes. That mustn't be able to hold up the loop
/// below, or the helper would stay running after the screen is unlocked.
#[derive(Clone, Default)]
struct OneAtATime(Arc<AtomicBool>);

impl OneAtATime {
    fn run_unless_busy(&self, job: impl FnOnce() + Send + 'static) -> bool {
        /// Even a job that panics counts as finished.
        struct Finished(Arc<AtomicBool>);
        impl Drop for Finished {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }

        if self.0.swap(true, Ordering::SeqCst) {
            return false;
        }
        let finished = Finished(self.0.clone());
        std::thread::spawn(move || {
            let _finished = finished;
            job();
        });
        true
    }
}

fn supervise(stop: &Receiver<()>) -> Result<()> {
    let program = std::env::current_exe().context("can't tell where this program is")?;
    let settings = paths::helper_settings()?;
    let copying = OneAtATime::default();
    let mut helper: Option<Helper> = None;
    let mut last_start: Option<Instant> = None;
    let mut last_copy: Option<Instant> = None;
    let mut previous_state = None;

    loop {
        let console = session::console_session()
            .and_then(|session| Some((session, session::console_state(session)?)));

        if helper.as_ref().is_some_and(Helper::has_exited) {
            tracing::info!("the helper stopped");
            helper = None;
        }

        let ready = lock_screen::has_settings(&settings);
        match next_step(console, helper.as_ref().map(Helper::session), ready) {
            Step::Nothing => {}
            Step::Stop => {
                tracing::info!("stopping the helper");
                helper = None;
            }
            Step::Start => {
                let may_try = last_start.is_none_or(|at| at.elapsed() >= TRY_AGAIN_AFTER);
                if let (true, Some((session, _))) = (may_try, console) {
                    last_start = Some(Instant::now());
                    tracing::info!(session, "starting the helper");
                    helper = Helper::start(&program, session)
                        .inspect_err(|error| tracing::error!("{error:#}"))
                        .ok();
                }
            }
        }

        if let Some((_, state)) = console {
            let since_copy = last_copy.map_or(Duration::MAX, |at| at.elapsed());
            if copy_is_due(state, previous_state, since_copy) {
                let settings = settings.clone();
                let started = copying.run_unless_busy(move || {
                    if let Err(error) = copy_settings(&settings) {
                        tracing::warn!("can't copy the settings: {error:#}");
                    }
                });
                if started {
                    last_copy = Some(Instant::now());
                }
            }
            previous_state = Some(state);
        }

        match stop.recv_timeout(CHECK_EVERY) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => return Ok(()),
        }
    }
}

fn copy_settings(to: &Path) -> Result<()> {
    let approval = Approval::load()?;
    lock_screen::copy_settings(&approval.settings_from, to, &approval.devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCKED: Option<(u32, ConsoleState)> = Some((1, ConsoleState::Locked));
    const SIGN_IN: Option<(u32, ConsoleState)> = Some((1, ConsoleState::NoUser));
    const IN_USE: Option<(u32, ConsoleState)> = Some((1, ConsoleState::Unlocked));

    #[test]
    fn the_helper_starts_when_the_screen_locks_or_nobody_is_signed_in() {
        assert_eq!(next_step(LOCKED, None, true), Step::Start);
        assert_eq!(next_step(SIGN_IN, None, true), Step::Start);
    }

    #[test]
    fn the_helper_stops_when_someone_is_using_the_computer() {
        assert_eq!(next_step(IN_USE, Some(1), true), Step::Stop);
        assert_eq!(next_step(IN_USE, None, true), Step::Nothing);
    }

    #[test]
    fn a_running_helper_is_left_alone_while_it_is_needed() {
        assert_eq!(next_step(LOCKED, Some(1), true), Step::Nothing);
        assert_eq!(next_step(SIGN_IN, Some(1), true), Step::Nothing);
    }

    /// Someone else signing in, or logging out, gives the screen a new session.
    #[test]
    fn a_helper_in_a_session_no_longer_on_the_screen_is_stopped() {
        assert_eq!(
            next_step(Some((2, ConsoleState::NoUser)), Some(1), true),
            Step::Stop
        );
        assert_eq!(
            next_step(Some((2, ConsoleState::NoUser)), None, true),
            Step::Start
        );
    }

    #[test]
    fn nothing_changes_while_windows_is_in_the_middle_of_something() {
        assert_eq!(next_step(None, Some(1), true), Step::Nothing);
        assert_eq!(next_step(None, None, true), Step::Nothing);
    }

    /// Without this computer's identity it would be a stranger to every other PC.
    #[test]
    fn no_helper_without_settings_to_run_from() {
        assert_eq!(next_step(LOCKED, None, false), Step::Nothing);
        assert_eq!(next_step(SIGN_IN, None, false), Step::Nothing);
        assert_eq!(next_step(LOCKED, Some(1), false), Step::Stop);
    }

    #[test]
    fn settings_are_copied_while_in_use_and_once_as_the_screen_locks() {
        use ConsoleState::{Locked, NoUser, Unlocked};
        let (soon, later) = (Duration::from_secs(1), COPY_EVERY);
        assert!(!copy_is_due(Unlocked, Some(Unlocked), soon));
        assert!(copy_is_due(Unlocked, Some(Unlocked), later));
        assert!(
            copy_is_due(Unlocked, None, Duration::MAX),
            "right at the start"
        );

        assert!(copy_is_due(Locked, Some(Unlocked), soon), "just locked");
        assert!(
            copy_is_due(Locked, None, soon),
            "was already locked at the start"
        );
        assert!(!copy_is_due(Locked, Some(Locked), later));

        assert!(!copy_is_due(NoUser, None, later));
        assert!(!copy_is_due(NoUser, Some(Unlocked), later));
    }

    /// The loop that stops the helper has to carry on, however long a copy takes.
    #[test]
    fn a_copy_that_never_finishes_holds_nothing_up() {
        let copying = OneAtATime::default();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel();

        let started = Instant::now();
        assert!(copying.run_unless_busy(move || {
            let _ = release_rx.recv();
            let _ = done_tx.send(());
        }));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "it doesn't wait"
        );

        // Another isn't started on top of it.
        assert!(!copying.run_unless_busy(|| panic!("this shouldn't run")));

        drop(release_tx);
        done_rx.recv().unwrap();
        // Once that one is over, the next can go ahead.
        let waited = Instant::now();
        while !copying.run_unless_busy(|| {}) {
            assert!(waited.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_copy_that_panics_doesnt_stop_later_ones() {
        let copying = OneAtATime::default();
        assert!(copying.run_unless_busy(|| panic!("something went badly wrong")));
        let waited = Instant::now();
        while !copying.run_unless_busy(|| {}) {
            assert!(waited.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
