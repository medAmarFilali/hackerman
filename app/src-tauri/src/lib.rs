//! The desktop app: a window and tray icon showing what the background agent is
//! doing. All the real work happens in the agent, which is this same program
//! started again with [`AGENT_ARGUMENT`], and which keeps running when the window
//! is closed. That also means there's only one file to copy to another computer.

mod lock_screen;
mod notifications;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use hk_ipc::{PairingStage, Request, UiState};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// Tells this program to run as the background agent instead of the app.
pub const AGENT_ARGUMENT: &str = "--agent";

/// How long to wait for a newly started agent before telling the user.
const AGENT_START_TIMEOUT: Duration = Duration::from_secs(10);
/// Starting the program with this keeps the window hidden, for when Windows starts it.
const HIDDEN_ARGUMENT: &str = "--hidden";
/// Starting the program with this tells the running copy to quit.
pub const QUIT_ARGUMENT: &str = "--quit";

/// How long to wait for the agent to stop before quitting anyway.
const QUIT_TIMEOUT: Duration = Duration::from_secs(2);

const TRAY_ID: &str = "main";

struct Bridge {
    requests: UnboundedSender<Request>,
    latest: Mutex<Option<UiState>>,
    /// Set once the user quits, so a stopping agent isn't mistaken for a crashed one.
    quitting: AtomicBool,
    /// The tray menu's item for turning sharing on or off, once the tray exists.
    sharing_item: Mutex<Option<MenuItem<tauri::Wry>>>,
}

#[tauri::command]
fn send_request(request: Request, bridge: State<'_, Bridge>) {
    let _ = bridge.requests.send(request);
}

/// Shows a folder in File Explorer. This only ever opens a window on a folder.
#[tauri::command]
fn open_folder(path: String) {
    let folder = std::path::Path::new(&path);
    if !folder.is_dir() {
        tracing::warn!(path, "can't show a folder that isn't there");
        return;
    }
    show_in_file_manager(folder);
}

#[tauri::command]
fn open_log_folder() {
    match hk_daemon::default_config_dir() {
        Ok(folder) => show_in_file_manager(&folder),
        Err(error) => tracing::warn!("can't find the log folder: {error:#}"),
    }
}

fn show_in_file_manager(folder: &std::path::Path) {
    #[cfg(windows)]
    let result = std::process::Command::new("explorer.exe")
        .arg(folder)
        .spawn();
    #[cfg(not(windows))]
    let result = std::process::Command::new("xdg-open").arg(folder).spawn();
    if let Err(error) = result {
        tracing::warn!(%error, "can't open a file manager");
    }
}

#[tauri::command]
fn current_state(bridge: State<'_, Bridge>) -> Option<UiState> {
    lock(&bridge.latest).clone()
}

pub fn run() {
    let (requests_tx, requests_rx) = mpsc::unbounded_channel();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, arguments, _| {
            if arguments.iter().any(|argument| argument == QUIT_ARGUMENT) {
                quit_everything(app);
            } else {
                show_window(app);
            }
        }))
        .manage(Bridge {
            requests: requests_tx,
            latest: Mutex::new(None),
            quitting: AtomicBool::new(false),
            sharing_item: Mutex::new(None),
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![HIDDEN_ARGUMENT]),
        ))
        .invoke_handler(tauri::generate_handler![
            send_request,
            current_state,
            open_folder,
            open_log_folder,
            lock_screen::lock_screen_status,
            lock_screen::set_lock_screen
        ])
        .on_window_event(|window, event| {
            // Closing the window leaves everything running, reachable from the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(move |app| {
            // Getting this far means no other copy was running, so nothing to quit.
            if std::env::args().any(|argument| argument == QUIT_ARGUMENT) {
                app.handle().exit(0);
                return Ok(());
            }
            build_tray(app)?;
            if !std::env::args().any(|argument| argument == HIDDEN_ARGUMENT) {
                show_window(app.handle());
            }
            tauri::async_runtime::spawn(follow_agent(app.handle().clone(), requests_rx));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("the app failed to start");
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Hackerman", true, None::<&str>)?;
    let sharing = MenuItem::with_id(app, "sharing", "Disable Hackerman", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Hackerman", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &sharing, &quit])?;
    *lock(&app.state::<Bridge>().sharing_item) = Some(sharing.clone());

    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("Hackerman")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_window(app),
            "sharing" => toggle_sharing(app),
            "quit" => quit_everything(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn toggle_sharing(app: &AppHandle) {
    let bridge = app.state::<Bridge>();
    let enabled = lock(&bridge.latest).as_ref().map(|state| state.enabled);
    if let Some(enabled) = enabled {
        let _ = bridge
            .requests
            .send(Request::SetEnabled { enabled: !enabled });
    }
}

/// Makes the tray say whether sharing is on.
fn show_sharing_in_tray(app: &AppHandle, enabled: bool) {
    let item = lock(&app.state::<Bridge>().sharing_item).clone();
    if let Some(item) = item {
        let label = if enabled {
            "Disable Hackerman"
        } else {
            "Enable Hackerman"
        };
        let _ = item.set_text(label);
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let tooltip = if enabled {
            "Hackerman"
        } else {
            "Hackerman (disabled)"
        };
        let _ = tray.set_tooltip(Some(tooltip));
    }
}

fn quit_everything(app: &AppHandle) {
    let bridge = app.state::<Bridge>();
    bridge.quitting.store(true, Ordering::SeqCst);
    let _ = bridge.requests.send(Request::Shutdown);
    // `follow_agent` exits as soon as the agent has gone. This is in case it doesn't.
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(QUIT_TIMEOUT).await;
        app.exit(0);
    });
}

fn is_quitting(app: &AppHandle) -> bool {
    app.state::<Bridge>().quitting.load(Ordering::SeqCst)
}

/// Keeps a connection to the agent, starting it if necessary.
async fn follow_agent(app: AppHandle, mut requests: UnboundedReceiver<Request>) {
    let channel = hk_ipc::default_channel_name();
    let mut waiting_since = None;
    let mut started_agent = false;

    loop {
        let mut connection = match hk_ipc::connect(&channel).await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::debug!(%error, "agent not reachable");
                if is_quitting(&app) {
                    app.exit(0);
                    return;
                }
                // The agent stepped aside for the lock screen, and comes back after it.
                if lock_screen::has_taken_over() {
                    started_agent = false;
                    waiting_since = None;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                if !started_agent {
                    started_agent = true;
                    if let Err(error) = start_agent() {
                        let _ = app.emit(
                            "agent-problem",
                            format!("Couldn't start Hackerman's background agent: {error}"),
                        );
                    }
                }
                let since = *waiting_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() > AGENT_START_TIMEOUT {
                    let message = format!(
                        "Hackerman's background agent isn't responding. Its log may say why: {}",
                        log_path()
                    );
                    let _ = app.emit("agent-problem", message);
                    // Try starting it again the next time round.
                    started_agent = false;
                    waiting_since = None;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        waiting_since = None;

        loop {
            tokio::select! {
                state = connection.recv::<UiState>() => match state {
                    Ok(Some(state)) => {
                        on_state(&app, state);
                    }
                    Ok(None) | Err(_) => break,
                },
                request = requests.recv() => {
                    let Some(request) = request else { return };
                    if connection.send(&request).await.is_err() {
                        break;
                    }
                }
            }
        }

        if is_quitting(&app) {
            app.exit(0);
            return;
        }
        if !lock_screen::has_taken_over() {
            let _ = app.emit(
                "agent-problem",
                "Lost contact with the background agent. Restarting it…",
            );
        }
        started_agent = false;
    }
}

fn on_state(app: &AppHandle, state: UiState) {
    // Not held while using the window or tray. Those can wait for the main thread,
    // which could be waiting for this lock.
    let previous = lock(&app.state::<Bridge>().latest).replace(state.clone());

    // Someone needs to see the code on this screen, even if the window is hidden.
    if showing_code(Some(&state)) && !showing_code(previous.as_ref()) {
        show_window(app);
    }
    // Likewise for files waiting on an answer.
    let asking = |state: &UiState| {
        state
            .incoming_files
            .as_ref()
            .map(|asking| asking.id.clone())
    };
    if asking(&state).is_some() && asking(&state) != previous.as_ref().and_then(asking) {
        show_window(app);
    }
    if previous.as_ref().map(|previous| previous.enabled) != Some(state.enabled) {
        show_sharing_in_tray(app, state.enabled);
    }
    // Nothing's new in the first state.
    if let Some(previous) = &previous
        && state.rules.notify_files
    {
        let arrivals = notifications::arrivals(previous, &state);
        // The window shows this for itself, if it's in front. Asking about the window
        // waits for the main thread, which isn't worth doing for every state.
        if !arrivals.is_empty() && !window_in_front(app) {
            for arrival in arrivals {
                notifications::show(app, arrival);
            }
        }
    }

    let _ = app.emit("agent-state", &state);
}

fn window_in_front(app: &AppHandle) -> bool {
    app.get_webview_window("main")
        .is_some_and(|window| window.is_focused().unwrap_or(false))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn showing_code(state: Option<&UiState>) -> bool {
    matches!(
        state
            .and_then(|state| state.pairing.as_ref())
            .map(|pairing| &pairing.stage),
        Some(PairingStage::ShowCode { .. })
    )
}

fn log_path() -> String {
    hk_daemon::default_config_dir()
        .map(|dir| dir.join("agent.log").display().to_string())
        .unwrap_or_else(|_| "agent.log".into())
}

fn start_agent() -> std::io::Result<()> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg(AGENT_ARGUMENT);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// No console window, and independent of this process.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn().map(|_| ())
}
