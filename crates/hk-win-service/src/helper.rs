//! Starts the helper as the system on the protected desktop. A process's account and desktop
//! are both chosen when it's created, so this is about getting those right.

use std::path::Path;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary,
    TokenSessionId,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken,
    PROCESS_INFORMATION, ResumeThread, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::core::{HSTRING, Owned, PCWSTR, PWSTR};

use crate::HELPER_ARGUMENT;

/// A running helper, which stops when this is dropped.
pub(crate) struct Helper {
    process: Owned<HANDLE>,
    /// Windows ends what's in this job when the job is closed, which also happens if the
    /// service itself goes, so that a helper can't outlive it.
    _job: Owned<HANDLE>,
    session: u32,
}

/// Text as Windows wants it, ending in a zero, in memory it's allowed to change.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

impl Helper {
    /// Starts it in a session, which should be the one on the physical screen. Only the
    /// system can do this, so it only works when running as the service.
    pub(crate) fn start(program: &Path, session: u32) -> Result<Self> {
        let mut command_line = wide(&format!(
            "\"{}\" --agent {HELPER_ARGUMENT}",
            program.display()
        ));
        // Every program has a desktop within a "window station". This is the protected
        // one for the screens that come before anyone can use the computer.
        let mut desktop = wide(r"winsta0\Winlogon");
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };

        // SAFETY: each handle is checked and owned as soon as it exists, so it's closed once.
        // The pointers are to values that outlive the calls, and the sizes are of those values.
        unsafe {
            // This service's own token says it's the system. A copy can be pointed at the
            // session on the screen, where services don't otherwise run.
            let mut own = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &raw mut own)
                .context("can't read this service's token")?;
            let own = Owned::new(own);
            let mut token = HANDLE::default();
            DuplicateTokenEx(
                *own,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &raw mut token,
            )
            .context("can't copy this service's token")?;
            let token = Owned::new(token);
            SetTokenInformation(
                *token,
                TokenSessionId,
                (&raw const session).cast(),
                size_of::<u32>() as u32,
            )
            .context("can't point the token at that session")?;

            let job = Owned::new(CreateJobObjectW(None, PCWSTR::null()).context("no job")?);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                *job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .context("can't set up the job")?;

            // Held back until it's in the job, so there's no moment it could slip away.
            let mut started = PROCESS_INFORMATION::default();
            CreateProcessAsUserW(
                Some(*token),
                &HSTRING::from(program),
                Some(PWSTR(command_line.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW | CREATE_SUSPENDED,
                None,
                PCWSTR::null(),
                &raw const startup,
                &raw mut started,
            )
            .context("can't start the helper")?;
            let process = Owned::new(started.hProcess);
            let thread = Owned::new(started.hThread);

            if let Err(error) = AssignProcessToJobObject(*job, *process) {
                let _ = TerminateProcess(*process, 1);
                return Err(error).context("can't put the helper in its job");
            }
            ResumeThread(*thread);

            Ok(Self {
                process,
                _job: job,
                session,
            })
        }
    }

    pub(crate) fn session(&self) -> u32 {
        self.session
    }

    pub(crate) fn has_exited(&self) -> bool {
        // SAFETY: the handle is valid for as long as this exists.
        unsafe { WaitForSingleObject(*self.process, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        // It has nothing to save or tidy up that it hasn't already: the files it keeps are
        // replaced in one step, and connections end by themselves.
        // SAFETY: the handle is valid for as long as this exists.
        let _ = unsafe { TerminateProcess(*self.process, 0) };
    }
}
