//! Who is at this computer's screen. Windows shows one session on the physical screen at
//! a time, called the console session, and it can be locked or have nobody signed in.
//!
//! The lock screen and the sign-in screen are on a protected desktop of their own, which
//! only a program running as the system itself can type into. So while one of them is up,
//! a helper started by a Windows service runs instead of the normal agent: see
//! `crates/hk-win-service`.

use std::cell::Cell;

use windows::Win32::Foundation::{GENERIC_ALL, HANDLE};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTS_SESSIONSTATE_LOCK, WTSFreeMemory,
    WTSGetActiveConsoleSessionId, WTSINFOEXW, WTSQuerySessionInformationW, WTSSessionInfoEx,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT,
    SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW, HDESK,
    OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::core::{HSTRING, PCWSTR, PWSTR};

/// The name of the Windows service that looks after the helper.
pub const HELPER_SERVICE: &str = "HackermanLockScreen";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleState {
    /// Someone is signed in and using it.
    Unlocked,
    /// Someone is signed in, and the lock screen is up.
    Locked,
    /// The sign-in screen is up, as it is after a restart.
    NoUser,
}

impl ConsoleState {
    /// Whether this is a job for the helper rather than the normal agent.
    pub fn needs_helper(self) -> bool {
        matches!(self, Self::Locked | Self::NoUser)
    }
}

/// The session on the physical screen. There's none for a moment while Windows switches.
pub fn console_session() -> Option<u32> {
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    (session != u32::MAX).then_some(session)
}

/// What's going on in a session, if Windows says. It doesn't always say about sessions
/// other than the caller's own, unless the caller is the system.
pub fn console_state(session: u32) -> Option<ConsoleState> {
    session_details(session).map(|(someone_signed_in, flags)| classify(someone_signed_in, flags))
}

/// Whether someone is signed in to a session, and the flags saying whether it's locked.
fn session_details(session: u32) -> Option<(bool, i32)> {
    let mut buffer = PWSTR::null();
    let mut size = 0;
    // SAFETY: Windows allocates the buffer and hands back a pointer to it, which is what
    // the two pointers given here are for.
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            session,
            WTSSessionInfoEx,
            &raw mut buffer,
            &raw mut size,
        )
    }
    .ok()?;
    // SAFETY: asked for this class of information, this is what the buffer holds. Level 1
    // is the only level there is, and says which member of the union is filled in. The
    // buffer stays until it's freed below.
    let details = unsafe {
        let info = &*buffer.0.cast::<WTSINFOEXW>();
        (info.Level == 1).then(|| {
            let details = &info.Data.WTSInfoExLevel1;
            (details.UserName[0] != 0, details.SessionFlags)
        })
    };
    // SAFETY: Windows allocated this for the caller to free, and nothing refers to it now.
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    details
}

/// Windows can also say it doesn't know whether a session is locked. Only saying it is
/// locked counts: wrongly stopping the agent would stop everything on a PC in use.
fn classify(someone_signed_in: bool, flags: i32) -> ConsoleState {
    if !someone_signed_in {
        ConsoleState::NoUser
    } else if flags == WTS_SESSIONSTATE_LOCK as i32 {
        ConsoleState::Locked
    } else {
        ConsoleState::Unlocked
    }
}

/// Whether the service that starts the helper is running. Without it, nothing would take
/// over from the agent, which may as well carry on.
pub fn helper_service_running() -> bool {
    // SAFETY: each handle is checked before use and closed once, and the status is a plain
    // structure for Windows to fill in.
    unsafe {
        let Ok(manager) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) else {
            return false;
        };
        let mut running = false;
        let name = HSTRING::from(HELPER_SERVICE);
        if let Ok(service) = OpenServiceW(manager, &name, SERVICE_QUERY_STATUS) {
            let mut status = SERVICE_STATUS::default();
            running = QueryServiceStatus(service, &raw mut status).is_ok()
                && status.dwCurrentState == SERVICE_RUNNING;
            let _ = CloseServiceHandle(service);
        }
        let _ = CloseServiceHandle(manager);
        running
    }
}

/// Whether someone is signed in and using the screen, as far as Windows will say.
pub(super) fn screen_in_use() -> bool {
    console_session().and_then(console_state) == Some(ConsoleState::Unlocked)
}

/// Whether the normal agent should stop and leave this computer to the helper: this
/// session isn't the one on the screen, or it's locked.
pub(super) fn helper_takes_over() -> bool {
    let Some(console) = console_session() else {
        return false;
    };
    let mut own = 0;
    // SAFETY: the session number is a plain value for Windows to fill in.
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &raw mut own) }.is_err() {
        return false;
    }
    let locked_here = own == console && console_state(own) == Some(ConsoleState::Locked);
    (own != console || locked_here) && helper_service_running()
}

thread_local! {
    /// The desktop this thread was last moved to, to close once it's moved to another.
    static THREAD_DESKTOP: Cell<Option<HDESK>> = const { Cell::new(None) };
}

/// Moves this thread to whichever desktop is taking input, since input can only be sent
/// from there, and says whether to send any. The helper starts on the lock screen's desktop,
/// but there are others while the session is locked, like a screen saver's.
///
/// It never goes to a user's own desktop, even if it's still running when the screen is
/// unlocked. Input from the system could reach things that the user's programs can't. If the
/// thread can't be moved it stays where it is, and Windows refuses input from the wrong desktop.
pub(super) fn follow_input_desktop() -> bool {
    // SAFETY: the handle is checked, and closed as soon as nothing is using it. Windows
    // refuses to move a thread that has windows or hooks, which is reported as an error.
    unsafe {
        let access = DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0);
        let Ok(desktop) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, access) else {
            return true;
        };
        if desktop_name(desktop).is_none_or(|name| is_users_desktop(&name)) {
            let _ = CloseDesktop(desktop);
            return false;
        }
        if SetThreadDesktop(desktop).is_err() {
            let _ = CloseDesktop(desktop);
            return true;
        }
        if let Some(previous) = THREAD_DESKTOP.replace(Some(desktop)) {
            let _ = CloseDesktop(previous);
        }
        true
    }
}

fn desktop_name(desktop: HDESK) -> Option<String> {
    let mut name = [0u16; 64];
    // SAFETY: the size given is that of the buffer, in bytes, and the handle is valid.
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            size_of_val(&name) as u32,
            None,
        )
    }
    .ok()?;
    let end = name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..end]))
}

/// What Windows calls the desktop that people's programs are on.
fn is_users_desktop(name: &str) -> bool {
    name.eq_ignore_ascii_case("Default")
}

#[cfg(test)]
mod tests {
    use windows::Win32::System::RemoteDesktop::WTS_SESSIONSTATE_UNLOCK;

    use super::*;

    #[test]
    fn only_being_told_a_session_is_locked_counts() {
        assert_eq!(classify(true, 0), ConsoleState::Locked);
        assert_eq!(classify(true, 1), ConsoleState::Unlocked);
        assert_eq!(classify(true, -1), ConsoleState::Unlocked, "unknown");
        assert_eq!(classify(false, 0), ConsoleState::NoUser);
        assert_eq!(classify(false, 1), ConsoleState::NoUser);
    }

    #[test]
    fn the_helper_only_sends_input_to_the_protected_desktops() {
        assert!(is_users_desktop("Default"));
        assert!(is_users_desktop("default"));
        assert!(!is_users_desktop("Winlogon"));
        assert!(!is_users_desktop("Screen-saver"));
    }

    #[test]
    fn the_helper_is_for_the_lock_and_sign_in_screens_only() {
        assert!(ConsoleState::Locked.needs_helper());
        assert!(ConsoleState::NoUser.needs_helper());
        assert!(!ConsoleState::Unlocked.needs_helper());
    }

    /// Only looks. Run on demand, with the screen unlocked and this PC not set up for the
    /// lock screen: `cargo test -p hk-platform session -- --ignored --nocapture`
    #[test]
    #[ignore = "depends on what this computer is doing at the time"]
    fn this_computer_is_in_use_and_nothing_takes_over() {
        let console = console_session().expect("a session on the screen");
        let state = console_state(console);
        println!(
            "console session {console}: {state:?}, service running: {}",
            helper_service_running()
        );
        assert_eq!(state, Some(ConsoleState::Unlocked));
        assert!(!helper_takes_over());
        // Exactly what Windows documents, so this is reading the right thing: anything
        // that isn't "locked" would also have counted as unlocked above.
        let (someone_signed_in, flags) = session_details(console).unwrap();
        assert!(someone_signed_in);
        assert_eq!(flags, WTS_SESSIONSTATE_UNLOCK as i32);
    }
}
