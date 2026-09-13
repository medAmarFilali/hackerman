//! Setting up and removing the service, and asking Windows for permission to do that.

use std::ffi::OsStr;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hk_daemon::lock_screen;
use hk_platform::session::{HELPER_SERVICE, helper_service_running};
use windows::Win32::Foundation::{ERROR_CANCELLED, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken,
    WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    FOLDERID_System, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{HSTRING, Owned, PCWSTR, w};
use windows_service::service::{
    Service, ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState,
    ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::approval::Approval;
use crate::{INSTALL_ARGUMENT, REMOVE_ARGUMENT, SERVICE_ARGUMENT, paths};

const DISPLAY_NAME: &str = "Hackerman Lock Screen";
const DESCRIPTION: &str = "Lets the keyboard and mouse of your paired PCs reach this PC's lock \
                           screen and sign-in screen. Turned on and off in Hackerman's Settings.";
const FIREWALL_RULE: &str = "Hackerman Lock Screen";
/// How long to wait for the service to stop, and for its files to be free afterwards.
const WAIT_FOR_STOP: Duration = Duration::from_secs(15);

/// Whether the service is set up on this computer, as the app shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub installed: bool,
    pub running: bool,
    /// It's from this version of Hackerman. It doesn't update along with the app, since
    /// changing it takes administrator permission.
    pub up_to_date: bool,
    /// The names of paired devices that can't reach the lock screen yet, because they were
    /// paired after it was set up. Allowing one takes administrator permission.
    pub not_allowed_yet: Vec<String>,
}

/// Anyone can ask this, without any special rights.
pub fn status() -> Status {
    let installed = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|manager| manager.open_service(HELPER_SERVICE, ServiceAccess::QUERY_STATUS))
        .is_ok();
    let version = paths::program_folder()
        .ok()
        .and_then(|folder| std::fs::read_to_string(folder.join("version.txt")).ok());
    Status {
        installed,
        running: helper_service_running(),
        up_to_date: version.as_deref() == Some(env!("CARGO_PKG_VERSION")),
        not_allowed_yet: if installed {
            not_allowed_yet().unwrap_or_default()
        } else {
            Vec::new()
        },
    }
}

fn not_allowed_yet() -> Result<Vec<String>> {
    let approved = Approval::load()?.devices;
    let paired = lock_screen::paired_devices(&hk_daemon::default_config_dir()?)?;
    Ok(paired
        .into_iter()
        .filter(|(device, _)| !approved.contains(device))
        .map(|(_, name)| name)
        .collect())
}

/// Sets the service up, or brings it up to date, for the user running this. That also approves
/// the devices they're paired with now. Windows asks the user for permission first, so don't
/// call this where waiting holds anything up.
pub fn set_up_for_this_user() -> Elevated {
    match hk_daemon::default_config_dir() {
        Ok(settings) => run_elevated(&format!("{INSTALL_ARGUMENT} \"{}\"", settings.display())),
        Err(error) => {
            tracing::warn!("can't find this user's settings: {error:#}");
            Elevated::Failed
        }
    }
}

/// Removes the service. Windows asks the user for permission first, so don't call this where
/// waiting holds anything up.
pub fn take_down() -> Elevated {
    run_elevated(REMOVE_ARGUMENT)
}

/// How asking for administrator permission went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevated {
    Done,
    /// The user said no at Windows' prompt.
    Declined,
    Failed,
}

/// Runs this program with an argument and administrator rights, which Windows asks the user
/// for, and waits for it to finish.
fn run_elevated(argument: &str) -> Elevated {
    let Ok(program) = std::env::current_exe() else {
        return Elevated::Failed;
    };
    let (program, argument) = (HSTRING::from(program.as_path()), HSTRING::from(argument));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(program.as_ptr()),
        lpParameters: PCWSTR(argument.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: the strings outlive the calls, and the handle Windows gives back is owned here.
    unsafe {
        // The shell's own documentation asks for this. It may have been done already.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        if let Err(error) = ShellExecuteExW(&raw mut info) {
            return if error.code() == ERROR_CANCELLED.to_hresult() {
                Elevated::Declined
            } else {
                tracing::warn!(%error, "can't ask for administrator permission");
                Elevated::Failed
            };
        }
        let process = Owned::new(info.hProcess);
        WaitForSingleObject(*process, INFINITE);
        let mut code = 1;
        if GetExitCodeProcess(*process, &raw mut code).is_err() || code != 0 {
            return Elevated::Failed;
        }
    }
    Elevated::Done
}

fn is_elevated() -> bool {
    // SAFETY: the token is owned as soon as it exists, and the structure is what's asked for.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token).is_err() {
            return false;
        }
        let token = Owned::new(token);
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0;
        GetTokenInformation(
            *token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut size,
        )
        .is_ok()
            && elevation.TokenIsElevated != 0
    }
}

/// Sets up the service, or brings it up to date. On failure, takes it all away again rather
/// than leave half of it, and leaves the reason where the user can read it. That's in Program
/// Files because this runs with administrator rights, and shouldn't be writing to a place
/// where the user's programs could have put a link to anywhere else.
pub fn install_and_report(arguments: &[String]) -> Result<()> {
    if !is_elevated() {
        bail!("this needs administrator rights");
    }
    let settings_from = arguments
        .iter()
        .position(|argument| argument == INSTALL_ARGUMENT)
        .and_then(|at| arguments.get(at + 1))
        .map(PathBuf::from)
        .context("which user's settings to use wasn't given")?;
    let result = install(&settings_from);
    if let Err(error) = &result {
        let _ = remove();
        let _ = leave_a_note(&format!("{error:#}"));
    }
    result
}

/// Removes the service and everything it keeps, including its copy of the private key.
pub fn remove_and_report() -> Result<()> {
    if !is_elevated() {
        bail!("this needs administrator rights");
    }
    let result = remove();
    if let Err(error) = &result {
        let _ = leave_a_note(&format!("{error:#}"));
    }
    result
}

fn leave_a_note(text: &str) -> Result<()> {
    let folder = paths::program_folder()?;
    std::fs::create_dir_all(&folder)?;
    std::fs::write(folder.join("setup.log"), text)?;
    Ok(())
}

fn install(settings_from: &Path) -> Result<()> {
    // Before changing anything, in case there's nothing to set up from.
    let paired = lock_screen::paired_devices(settings_from)?;
    if !lock_screen::has_settings(settings_from) {
        bail!(
            "Hackerman hasn't been set up in {}",
            settings_from.display()
        );
    }

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("can't reach Windows' list of services")?;
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::STOP
        | ServiceAccess::START
        | ServiceAccess::CHANGE_CONFIG;
    let existing = manager.open_service(HELPER_SERVICE, access).ok();
    if let Some(service) = &existing {
        stop(service)?;
    }

    let source = std::env::current_exe().context("can't tell where this program is")?;
    let target = paths::service_program()?;
    let folder = paths::program_folder()?;
    std::fs::create_dir_all(&folder)
        .with_context(|| format!("can't create {}", folder.display()))?;
    replace_file(&source, &target)?;
    std::fs::write(folder.join("version.txt"), env!("CARGO_PKG_VERSION"))
        .context("can't note the version")?;
    let _ = std::fs::remove_file(folder.join("setup.log"));

    // From nothing, every time: see create_private_folder. The service keeps a log there, and
    // its helper did too, which can take a moment to be free after they stop.
    let data = paths::data_folder()?;
    remove_folder(&data)?;
    paths::create_private_folder(&data)?;

    // This computer's identity is only ever copied here, with an administrator's permission.
    let helper_settings = paths::helper_settings()?;
    lock_screen::copy_identity(settings_from, &helper_settings)?;
    let devices: Vec<_> = paired.into_iter().map(|(device, _)| device).collect();
    lock_screen::copy_settings(settings_from, &helper_settings, &devices)?;
    Approval {
        settings_from: settings_from.to_path_buf(),
        devices,
    }
    .save()?;

    let service = match existing {
        Some(service) => service,
        None => manager
            .create_service(
                &ServiceInfo {
                    name: HELPER_SERVICE.into(),
                    display_name: DISPLAY_NAME.into(),
                    service_type: ServiceType::OWN_PROCESS,
                    start_type: ServiceStartType::AutoStart,
                    error_control: ServiceErrorControl::Normal,
                    executable_path: target.clone(),
                    launch_arguments: vec![SERVICE_ARGUMENT.into()],
                    dependencies: vec![],
                    account_name: None, // The system's own account.
                    account_password: None,
                },
                access,
            )
            .context("can't create the service")?,
    };
    service
        .set_description(DESCRIPTION)
        .context("can't describe the service")?;

    // It can reach other PCs without this, but they couldn't reach it. Like the first time
    // Hackerman runs, this is for private networks only.
    allow_through_firewall(&target);

    service
        .start(&[] as &[&OsStr])
        .context("can't start the service")?;
    Ok(())
}

fn remove() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("can't reach Windows' list of services")?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    if let Ok(service) = manager.open_service(HELPER_SERVICE, access) {
        stop(&service)?;
        service.delete().context("can't remove the service")?;
    }
    remove_firewall_rule();

    // These go even if one can't, and the first problem is what gets reported.
    let program = remove_folder(&paths::program_folder()?);
    let data = remove_folder(&paths::data_folder()?);
    program.and(data)
}

fn stop(service: &Service) -> Result<()> {
    let state = service
        .query_status()
        .context("can't read the service's state")?;
    if state.current_state != ServiceState::Stopped {
        // It may already be stopping, in which case this fails and the wait below is enough.
        let _ = service.stop();
    }
    let started = Instant::now();
    while service
        .query_status()
        .context("can't read the service's state")?
        .current_state
        != ServiceState::Stopped
    {
        if started.elapsed() > WAIT_FOR_STOP {
            bail!("the service didn't stop");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Runs the same step again for a while, as a program that has just stopped can hold on to
/// its files for a moment.
fn keep_trying(mut step: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    let started = Instant::now();
    loop {
        match step() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if started.elapsed() < WAIT_FOR_STOP => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(error),
        }
    }
}

fn replace_file(from: &Path, to: &Path) -> Result<()> {
    keep_trying(|| std::fs::copy(from, to).map(|_| ()))
        .with_context(|| format!("can't copy this program to {}", to.display()))
}

fn remove_folder(folder: &Path) -> Result<()> {
    keep_trying(|| std::fs::remove_dir_all(folder))
        .with_context(|| format!("can't remove {}", folder.display()))
}

/// Windows' firewall tool, by its full path: this runs with administrator rights, and which
/// program a bare name finds depends on things that other programs can change.
fn netsh() -> Result<Command> {
    let mut command = Command::new(paths::known_folder(&FOLDERID_System, None)?.join("netsh.exe"));
    command.creation_flags(CREATE_NO_WINDOW.0);
    command.args(["advfirewall", "firewall"]);
    Ok(command)
}

fn allow_through_firewall(program: &Path) {
    remove_firewall_rule();
    let result = netsh().and_then(|mut command| {
        let added = command
            .args(["add", "rule"])
            .arg(format!("name={FIREWALL_RULE}"))
            .args(["dir=in", "action=allow", "protocol=UDP", "profile=private"])
            .arg(format!("program={}", program.display()))
            .status()?;
        if !added.success() {
            bail!("{added}");
        }
        Ok(())
    });
    if let Err(error) = result {
        // Not worth failing over: it can still connect out to the other PCs by itself.
        let _ = leave_a_note(&format!("can't add a firewall rule: {error:#}"));
    }
}

fn remove_firewall_rule() {
    if let Ok(mut command) = netsh() {
        let _ = command
            .args(["delete", "rule"])
            .arg(format!("name={FIREWALL_RULE}"))
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only looks. On demand, on a PC not set up for the lock screen:
    /// `cargo test -p hk-win-service -- --ignored --nocapture`
    #[test]
    #[ignore = "depends on what's set up on this computer"]
    fn anyone_can_see_that_it_is_not_set_up() {
        let status = status();
        println!("{status:?}");
        assert_eq!(
            status,
            Status {
                installed: false,
                running: false,
                up_to_date: false,
                not_allowed_yet: vec![],
            }
        );
    }

    #[test]
    fn something_gone_already_counts_as_done() {
        let mut calls = 0;
        keep_trying(|| {
            calls += 1;
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        })
        .unwrap();
        assert_eq!(calls, 1);
    }

    #[test]
    fn being_busy_is_tried_again() {
        let mut calls = 0;
        keep_trying(|| {
            calls += 1;
            if calls < 3 {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(calls, 3);
    }
}
