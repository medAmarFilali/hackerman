//! The service itself: it watches the screen, and starts and stops the helpers to match.

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
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
use windows::core::Owned;
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
/// How often to look again at who's signed in, while it isn't the user who set this up. Windows
/// can take a moment after someone signs in to say who they are.
const CHECK_USER_EVERY: Duration = Duration::from_secs(10);
/// A helper that stops sooner than this after starting didn't get going at all.
const QUICK_EXIT: Duration = Duration::from_secs(10);

/// How long to wait before starting the input helper again, given how long it ran. One that
/// stops straight away, as it does if something else has taken its pipe's name, would otherwise
/// be started again every few seconds for as long as that lasts.
fn try_again_after(ran_for: Option<Duration>) -> Duration {
    match ran_for {
        Some(ran) if ran < QUICK_EXIT => Duration::from_secs(60),
        _ => TRY_AGAIN_AFTER,
    }
}

define_windows_service!(service_entry, service_main);

/// Hands this process over to Windows, which calls back with the service running. Started any
/// other way, like from a terminal, this fails straight away.
pub fn run_service() -> Result<()> {
    service_dispatcher::start(HELPER_SERVICE, service_entry)
        .context("this only runs when Windows starts it as a service")
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = start_logging("service.log") {
        // There's nowhere to report it. The service can do its job without a log.
        drop(error);
    }
    if let Err(error) = run() {
        tracing::error!("{error:#}");
    }
}

/// Logs to a file where only the system and administrators can read it.
pub(crate) fn start_logging(name: &str) -> Result<()> {
    let log = File::create(paths::data_folder()?.join(name))?;
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

/// What to do about a helper, given which session is on the screen, the session the helper is
/// running in if it's running, and whether it's wanted there. Windows not saying about the
/// screen means it's in the middle of something, so nothing changes.
fn next_step(console: Option<u32>, helper: Option<u32>, wanted: bool) -> Step {
    let Some(session) = console else {
        return Step::Nothing;
    };
    match helper {
        // In the wrong session, it can't reach the screen. Started again next time round.
        Some(running_in) if !wanted || running_in != session => Step::Stop,
        None if wanted => Step::Start,
        _ => Step::Nothing,
    }
}

/// The lock-screen helper is for the lock and sign-in screens, once it has settings to run
/// from.
fn wants_lock_screen_helper(state: ConsoleState, ready: bool) -> bool {
    ready && state.needs_helper()
}

/// Who is using the computer, as far as the input helper goes.
#[derive(Default)]
struct SignedIn {
    session: Option<u32>,
    /// Their account, if they're the user who set this up.
    approved: Option<String>,
    checked: Option<Instant>,
}

impl SignedIn {
    /// The account of whoever is signed in to this session, if it's the one that set this up.
    fn approved(&mut self, session: u32) -> Option<String> {
        let due = self.session != Some(session)
            || (self.approved.is_none()
                && self
                    .checked
                    .is_none_or(|at| at.elapsed() >= CHECK_USER_EVERY));
        if due {
            let first_look = self.session != Some(session);
            self.session = Some(session);
            self.checked = Some(Instant::now());
            self.approved = approved_account(session)
                .inspect_err(|error| tracing::warn!("{error:#}"))
                .ok()
                .flatten();
            if first_look && self.approved.is_none() {
                tracing::info!(
                    session,
                    "the user who set this up isn't the one signed in, so there's no input helper"
                );
            }
        }
        self.approved.clone()
    }

    fn forget(&mut self) {
        *self = Self::default();
    }
}

/// The account signed in to a session, if it's the user who set this up. The input helper only
/// ever works for them: they're the one an administrator agreed to it for. Only what Windows
/// says about the account counts, since anything the user can change, they could change to look
/// like someone else.
fn approved_account(session: u32) -> Result<Option<String>> {
    let Some(approved) = Approval::load()?.account else {
        return Ok(None);
    };
    // SAFETY: the token is owned as soon as it exists. Only the system can ask for it.
    let token = unsafe {
        let mut token = HANDLE::default();
        WTSQueryUserToken(session, &raw mut token)
            .context("can't tell who is signed in to the session on the screen")?;
        Owned::new(token)
    };
    let account = session::account_of(*token).context("can't tell which account is signed in")?;
    Ok((account == approved).then_some(account))
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
    let mut input_helper: Option<Helper> = None;
    let mut last_start: Option<Instant> = None;
    let mut last_input_start: Option<Instant> = None;
    let mut last_copy: Option<Instant> = None;
    let mut previous_state = None;
    let mut signed_in = SignedIn::default();
    let mut input_wait = TRY_AGAIN_AFTER;
    let may_try =
        |last: Option<Instant>, wait: Duration| last.is_none_or(|at| at.elapsed() >= wait);

    loop {
        let console = session::console_session()
            .and_then(|session| Some((session, session::console_state(session)?)));
        let console_session = console.map(|(session, _)| session);

        if helper.as_ref().is_some_and(Helper::has_exited) {
            tracing::info!("the lock-screen helper stopped");
            helper = None;
        }
        if input_helper.as_ref().is_some_and(Helper::has_exited) {
            input_wait = try_again_after(last_input_start.map(|at| at.elapsed()));
            tracing::info!(
                again_in_s = input_wait.as_secs(),
                "the input helper stopped"
            );
            input_helper = None;
        }

        let ready = lock_screen::has_settings(&settings);
        let wanted = console.is_some_and(|(_, state)| wants_lock_screen_helper(state, ready));
        match next_step(
            console_session,
            helper.as_ref().map(Helper::session),
            wanted,
        ) {
            Step::Nothing => {}
            Step::Stop => {
                tracing::info!("stopping the lock-screen helper");
                helper = None;
            }
            Step::Start => {
                if let (true, Some(session)) =
                    (may_try(last_start, TRY_AGAIN_AFTER), console_session)
                {
                    last_start = Some(Instant::now());
                    tracing::info!(session, "starting the lock-screen helper");
                    helper = Helper::start_at_lock_screen(&program, session)
                        .inspect_err(|error| tracing::error!("{error:#}"))
                        .ok();
                }
            }
        }

        let account = match console {
            Some((session, ConsoleState::Unlocked)) => signed_in.approved(session),
            _ => {
                signed_in.forget();
                None
            }
        };
        let running_in = input_helper.as_ref().map(Helper::session);
        match next_step(console_session, running_in, account.is_some()) {
            Step::Nothing => {}
            Step::Stop => {
                tracing::info!("stopping the input helper");
                input_helper = None;
            }
            Step::Start => {
                if let (true, Some(session), Some(account)) = (
                    may_try(last_input_start, input_wait),
                    console_session,
                    &account,
                ) {
                    last_input_start = Some(Instant::now());
                    tracing::info!(session, "starting the input helper");
                    input_helper = Helper::start_for_input(&program, session, account)
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

    /// What to do about the lock-screen helper.
    fn lock_screen(console: Option<(u32, ConsoleState)>, helper: Option<u32>, ready: bool) -> Step {
        let wanted = console.is_some_and(|(_, state)| wants_lock_screen_helper(state, ready));
        next_step(console.map(|(session, _)| session), helper, wanted)
    }

    #[test]
    fn the_helper_starts_when_the_screen_locks_or_nobody_is_signed_in() {
        assert_eq!(lock_screen(LOCKED, None, true), Step::Start);
        assert_eq!(lock_screen(SIGN_IN, None, true), Step::Start);
    }

    #[test]
    fn the_helper_stops_when_someone_is_using_the_computer() {
        assert_eq!(lock_screen(IN_USE, Some(1), true), Step::Stop);
        assert_eq!(lock_screen(IN_USE, None, true), Step::Nothing);
    }

    #[test]
    fn a_running_helper_is_left_alone_while_it_is_needed() {
        assert_eq!(lock_screen(LOCKED, Some(1), true), Step::Nothing);
        assert_eq!(lock_screen(SIGN_IN, Some(1), true), Step::Nothing);
    }

    /// Someone else signing in, or logging out, gives the screen a new session.
    #[test]
    fn a_helper_in_a_session_no_longer_on_the_screen_is_stopped() {
        assert_eq!(
            lock_screen(Some((2, ConsoleState::NoUser)), Some(1), true),
            Step::Stop
        );
        assert_eq!(
            lock_screen(Some((2, ConsoleState::NoUser)), None, true),
            Step::Start
        );
    }

    #[test]
    fn nothing_changes_while_windows_is_in_the_middle_of_something() {
        assert_eq!(lock_screen(None, Some(1), true), Step::Nothing);
        assert_eq!(lock_screen(None, None, true), Step::Nothing);
    }

    /// Without this computer's identity it would be a stranger to every other PC.
    #[test]
    fn no_helper_without_settings_to_run_from() {
        assert_eq!(lock_screen(LOCKED, None, false), Step::Nothing);
        assert_eq!(lock_screen(SIGN_IN, None, false), Step::Nothing);
        assert_eq!(lock_screen(LOCKED, Some(1), false), Step::Stop);
    }

    /// The input helper is wanted while the user who set this up is using the computer, which
    /// the loop works out from the screen being unlocked and who's signed in.
    #[test]
    fn the_input_helper_comes_and_goes_with_the_user_who_set_this_up() {
        assert_eq!(next_step(Some(1), None, true), Step::Start);
        assert_eq!(next_step(Some(1), Some(1), true), Step::Nothing);
        assert_eq!(
            next_step(Some(1), Some(1), false),
            Step::Stop,
            "locked, or someone else"
        );
        assert_eq!(
            next_step(Some(2), Some(1), true),
            Step::Stop,
            "another session"
        );
        assert_eq!(next_step(None, Some(1), false), Step::Nothing);
    }

    #[test]
    fn a_helper_that_keeps_stopping_straight_away_is_tried_less_often() {
        let soon = Duration::from_secs(1);
        assert_eq!(try_again_after(None), TRY_AGAIN_AFTER, "it hasn't run yet");
        assert_eq!(
            try_again_after(Some(Duration::from_secs(600))),
            TRY_AGAIN_AFTER
        );
        assert!(try_again_after(Some(soon)) > Duration::from_secs(30));
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
