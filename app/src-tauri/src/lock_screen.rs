//! Working at the lock screen, as far as the app goes: the switch in Settings, and not
//! mistaking the agent stepping aside for a problem. The rest is in crates/hk-win-service.

use hk_platform::Platform;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LockScreenStatus {
    /// Whether this operating system can do this at all.
    available: bool,
    on: bool,
    running: bool,
    /// It's from this version of Hackerman.
    up_to_date: bool,
    /// Paired PCs that can't reach the lock screen yet.
    not_allowed_yet: Vec<String>,
}

#[cfg(windows)]
#[tauri::command]
pub fn lock_screen_status() -> LockScreenStatus {
    let status = hk_win_service::status();
    LockScreenStatus {
        available: true,
        on: status.installed,
        running: status.running,
        up_to_date: status.up_to_date,
        not_allowed_yet: status.not_allowed_yet,
    }
}

#[cfg(not(windows))]
#[tauri::command]
pub fn lock_screen_status() -> LockScreenStatus {
    LockScreenStatus {
        available: false,
        on: false,
        running: false,
        up_to_date: false,
        not_allowed_yet: Vec::new(),
    }
}

/// Turns it on, which also brings it up to date and allows every PC this user is paired with,
/// or off. Windows asks the user first, so this
/// says whether it happened: they can say no.
#[cfg(windows)]
#[tauri::command]
pub async fn set_lock_screen(on: bool) -> Result<bool, String> {
    use hk_win_service::{Elevated, set_up_for_this_user, take_down};

    let change = if on { set_up_for_this_user } else { take_down };
    let outcome = tauri::async_runtime::spawn_blocking(change)
        .await
        .map_err(|error| error.to_string())?;
    match outcome {
        Elevated::Done => Ok(true),
        Elevated::Declined => Ok(false),
        Elevated::Failed => {
            let folder = hk_win_service::program_folder()
                .map(|folder| folder.display().to_string())
                .unwrap_or_default();
            Err(format!(
                "That didn't work. The reason should be in setup.log in {folder}."
            ))
        }
    }
}

#[cfg(not(windows))]
#[tauri::command]
pub async fn set_lock_screen(_on: bool) -> Result<bool, String> {
    Err("This only works on Windows so far.".to_string())
}

/// Whether the agent should be stopped right now, with the helper running in its place. Until
/// that changes, the agent being gone isn't a problem and it shouldn't be started again.
pub fn has_taken_over() -> bool {
    hk_platform::native().is_ok_and(|platform| platform.should_step_aside())
}
