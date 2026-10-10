//! Windows: global low-level hooks to capture input, `SendInput` to replay it.

mod apps;
mod clipboard;
mod cursor;
mod displays;
mod hardware;
mod hook;
mod inject;
mod keymap;
mod media;
pub mod session;
mod virtual_files;

pub use virtual_files::data_object;

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hk_core::apps::{MediaCommand, RunningApp};
use hk_core::clipboard::{ClipboardData, SharedFile};
use hk_core::geometry::Point;
use hk_core::input::InputEvent;
use hk_core::layout::Monitor;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    ProcessPowerThrottling, SetProcessInformation,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, KF_FLAG_DEFAULT, QUNS_BUSY, QUNS_PRESENTATION_MODE,
    QUNS_RUNNING_D3D_FULL_SCREEN, SHGetKnownFolderPath, SHQueryUserNotificationState,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, SetCursorPos,
};

use crate::{Capture, Error, FileSource, InputHandler, Platform};

/// How far the parked cursor stays from the corner of the primary monitor.
/// Movement is read as an offset from the parked position, and Windows limits
/// the cursor to the desktop, so this also limits the distance of one mouse
/// report. Reports never get close to this at normal polling rates.
const PARK_INSET: i32 = 300;

/// Says yes at most once in a given time, for reporting something that could otherwise fill
/// the log many times a second.
struct Occasionally {
    every: Duration,
    last: Cell<Option<Instant>>,
}

impl Occasionally {
    const fn every(every: Duration) -> Self {
        Self {
            every,
            last: Cell::new(None),
        }
    }

    fn due(&self) -> bool {
        let due = self.last.get().is_none_or(|at| at.elapsed() >= self.every);
        if due {
            self.last.set(Some(Instant::now()));
        }
        due
    }
}

#[derive(Debug, Clone)]
pub struct NativePlatform {
    role: Role,
    replayer: inject::Replayer,
}

#[derive(Debug, Clone, Copy)]
enum Role {
    /// The user's own agent.
    Agent,
    /// Running as the system while the screen is locked. It only stays for testing when the
    /// screen is unlocked, which is the only way to try it out on a PC in use.
    Helper { stays_when_unlocked: bool },
}

pub fn native() -> Result<NativePlatform, Error> {
    Ok(starting_up(Role::Agent))
}

/// This computer, for the helper that runs while the screen is locked.
pub fn native_for_lock_screen(stays_when_unlocked: bool) -> Result<NativePlatform, Error> {
    Ok(starting_up(Role::Helper {
        stays_when_unlocked,
    }))
}

fn starting_up(role: Role) -> NativePlatform {
    // Without this, Windows reports scaled coordinates on high-DPI monitors.
    // It fails harmlessly if the awareness was already set.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    keep_running_at_full_speed();
    let may_send = match role {
        Role::Agent => || true,
        Role::Helper { .. } => session::follow_input_desktop,
    };
    NativePlatform {
        role,
        replayer: inject::Replayer::start(may_send),
    }
}

/// Windows can run a program with no window in front on its slowest processors, at their
/// slowest, to save power. Every key press and mouse movement on the computer waits on this one.
fn keep_running_at_full_speed() {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    // SAFETY: the size given is that of the structure. Windows ignores this if it's too old
    // to know about it, and everything still works.
    let _ = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&raw const state).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
}

fn move_cursor(to: Point) {
    if let Err(error) = unsafe { SetCursorPos(to.x.round() as i32, to.y.round() as i32) } {
        tracing::warn!(%error, "moving the cursor failed");
    }
}

impl Platform for NativePlatform {
    fn monitors(&self) -> Vec<Monitor> {
        displays::enumerate()
    }

    fn model(&self) -> Option<String> {
        hardware::model()
    }

    fn desktop_folder(&self) -> Option<PathBuf> {
        let path =
            unsafe { SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None) }.ok()?;
        let text = unsafe { path.to_string() }.ok();
        // Windows allocated this for the caller to free.
        unsafe { CoTaskMemFree(Some(path.0.cast())) };
        text.map(PathBuf::from)
    }

    fn park_point(&self) -> Point {
        // Near the top left of the primary monitor, which always starts at (0, 0). Windows'
        // notifications come up at the bottom right, and one that came up under the parked
        // cursor would show the cursor, and stay for as long as it was over it.
        let (width, height) =
            unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        Point::new(
            f64::from(PARK_INSET.min(width / 2)),
            f64::from(PARK_INSET.min(height / 2)),
        )
    }

    fn park_cursor(&self, at: Point) {
        move_cursor(at);
        cursor::hide_at(at);
    }

    fn restore_cursor(&self, to: Point) {
        cursor::show();
        move_cursor(to);
    }

    fn should_step_aside(&self) -> bool {
        match self.role {
            Role::Agent => session::helper_takes_over(),
            // The service stops it too. This is in case that's held up: it shouldn't go on
            // running as the system with someone using the computer.
            Role::Helper {
                stays_when_unlocked: false,
            } => session::screen_in_use(),
            Role::Helper {
                stays_when_unlocked: true,
            } => false,
        }
    }

    fn inject(&self, event: &InputEvent) {
        self.replayer.replay(*event);
    }

    fn finish_injecting(&self) {
        self.replayer.finish();
    }

    fn start_capture(&self, handler: InputHandler) -> Result<Capture, Error> {
        hook::Hooks::start(handler).map(Capture::new)
    }

    fn fullscreen_app_in_front(&self) -> bool {
        matches!(
            unsafe { SHQueryUserNotificationState() },
            Ok(QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE)
        )
    }

    fn watch_clipboard(&self, on_change: Box<dyn FnMut() + Send>) -> Result<Capture, Error> {
        clipboard::Watcher::start(on_change).map(Capture::new)
    }

    fn read_clipboard(&self) -> Result<ClipboardData, Error> {
        clipboard::read()
    }

    fn write_clipboard(&self, data: &ClipboardData) -> Result<(), Error> {
        clipboard::write(data)
    }

    fn clipboard_files(&self) -> Result<Vec<PathBuf>, Error> {
        clipboard::files()
    }

    fn write_clipboard_files(
        &self,
        files: Vec<SharedFile>,
        source: Arc<dyn FileSource>,
    ) -> Result<(), Error> {
        virtual_files::put_on_clipboard(files, source)
    }

    fn running_apps(&self) -> Vec<RunningApp> {
        // The system's helper has no apps of its own, and no business with the user's.
        match self.role {
            Role::Agent => apps::running(),
            Role::Helper { .. } => Vec::new(),
        }
    }

    fn app_icon(&self, app: &str) -> Option<Vec<u8>> {
        apps::icon(app)
    }

    fn media_command(&self, app: &str, command: MediaCommand) -> Result<(), Error> {
        match self.role {
            Role::Agent => media::command(app, command),
            Role::Helper { .. } => Err(Error::Media("the screen is locked".to_string())),
        }
    }
}

/// These use the real cursor and real input hooks on the machine running them, so
/// they only run on demand, and with the screen unlocked since nothing can draw
/// over the lock screen: `cargo test -p hk-platform -- --ignored --test-threads=1`
#[cfg(test)]
mod real_machine_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, GetCursorInfo, GetCursorPos,
    };

    use super::*;

    fn cursor_position() -> Point {
        let mut point = POINT::default();
        unsafe { GetCursorPos(&mut point) }.unwrap();
        Point::new(f64::from(point.x), f64::from(point.y))
    }

    fn cursor_visible() -> bool {
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetCursorInfo(&mut info) }.unwrap();
        info.flags == CURSOR_SHOWING && !info.hCursor.is_invalid()
    }

    fn settle() {
        std::thread::sleep(Duration::from_millis(150));
    }

    /// Only looks. Run with `--nocapture` to see what it finds:
    /// `cargo test -p hk-platform apps_on_this -- --ignored --nocapture`
    #[test]
    #[ignore = "depends on what's open on this computer"]
    fn the_apps_on_this_computer_make_sense() {
        let platform = native().unwrap();
        let monitors: Vec<String> = platform.monitors().into_iter().map(|m| m.id).collect();
        let start = std::time::Instant::now();
        let apps = platform.running_apps();
        println!("{} apps in {:?}", apps.len(), start.elapsed());
        for app in &apps {
            let start = std::time::Instant::now();
            let icon = platform.app_icon(&app.id);
            println!(
                "{:<28} {:<24} monitors: {} minimized: {} media: {:?} icon: {:?} ({:?})",
                app.id,
                app.name,
                app.monitors.len(),
                app.minimized,
                app.media,
                icon.as_ref().map(Vec::len),
                start.elapsed(),
            );
            assert!(!app.id.is_empty() && !app.name.is_empty());
            for monitor in &app.monitors {
                assert!(monitors.contains(monitor), "{monitor} isn't a monitor here");
            }
            if let Some(icon) = icon {
                assert_eq!(&icon[..4], b"\x89PNG");
                assert!(
                    icon.len() <= hk_core::apps::MAX_ICON_BYTES,
                    "{}",
                    icon.len()
                );
            }
        }
        assert!(
            !apps
                .iter()
                .any(|app| app.id == "hackerman" || app.id == "shellexperiencehost"),
            "these aren't apps someone opened"
        );
    }

    /// Only looks, so it runs with the others.
    #[test]
    fn the_desktop_folder_is_a_folder_that_exists() {
        let desktop = native().unwrap().desktop_folder().expect("a Desktop");
        assert!(desktop.is_absolute(), "{}", desktop.display());
        assert!(desktop.is_dir(), "{}", desktop.display());
    }

    #[test]
    #[ignore = "replaces what's on the real clipboard: see scripts/run-isolated.ps1"]
    fn the_clipboard_round_trips_and_its_own_writes_are_not_reported() {
        let platform = native().unwrap();
        let before = platform.read_clipboard().unwrap();

        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        let watcher = platform
            .watch_clipboard(Box::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }))
            .unwrap();
        settle();

        let text = ClipboardData {
            text: Some("Hackerman clipboard test: é ✓".into()),
            ..Default::default()
        };
        platform.write_clipboard(&text).unwrap();
        settle();
        let text_back = platform.read_clipboard().unwrap();

        let rich = ClipboardData {
            text: Some("bold".into()),
            html: Some("<b>bold</b>".into()),
            image: None,
        };
        platform.write_clipboard(&rich).unwrap();
        settle();
        let rich_back = platform.read_clipboard().unwrap();

        // A 2x2 picture, encoded the same way as one coming from another device.
        let pixels: Vec<u8> = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [9, 9, 9, 255],
        ]
        .concat();
        let image = clipboard::tests_png(2, 2, &pixels);
        platform
            .write_clipboard(&ClipboardData {
                image: Some(image),
                ..Default::default()
            })
            .unwrap();
        settle();
        let image_back = platform.read_clipboard().unwrap();
        let reported = seen.load(Ordering::Relaxed);

        // Put back what was there before asserting anything.
        drop(watcher);
        if !before.is_empty() {
            platform.write_clipboard(&before).unwrap();
        }

        assert_eq!(text_back.text, text.text);
        assert_eq!(rich_back.text.as_deref(), Some("bold"));
        assert!(rich_back.html.unwrap_or_default().contains("<b>bold</b>"));
        let image_back = image_back.image.expect("the image should come back");
        assert_eq!((image_back.width, image_back.height), (2, 2));
        assert_eq!(reported, 0, "writing here must not look like a new copy");
    }

    #[test]
    #[ignore = "replaces what's on the real clipboard: see scripts/run-isolated.ps1"]
    fn copies_by_other_programs_are_noticed_unless_they_are_private() {
        use arboard::{Clipboard, SetExtWindows};

        let platform = native().unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        let watcher = platform
            .watch_clipboard(Box::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }))
            .unwrap();
        settle();

        // Going straight through arboard is what any other program looks like.
        let mut other_program = Clipboard::new().unwrap();
        other_program.set_text("an ordinary copy").unwrap();
        settle();
        let after_ordinary = seen.load(Ordering::Relaxed);
        let ordinary = platform.read_clipboard().unwrap();

        // What password managers do.
        other_program
            .set()
            .exclude_from_monitoring()
            .text("hunter2")
            .unwrap();
        settle();
        let after_private = seen.load(Ordering::Relaxed);
        let private = platform.read_clipboard().unwrap();
        drop(watcher);

        assert!(after_ordinary >= 1, "an ordinary copy should be noticed");
        assert_eq!(ordinary.text.as_deref(), Some("an ordinary copy"));
        assert_eq!(after_private, after_ordinary, "private content isn't news");
        assert!(private.is_empty(), "private content must never be read");
    }

    #[test]
    #[ignore = "reads real desktop state"]
    fn fullscreen_detection_answers() {
        // Whichever it is, asking must work and not take long.
        let platform = native().unwrap();
        let start = std::time::Instant::now();
        let _ = platform.fullscreen_app_in_front();
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[test]
    #[ignore = "moves the real cursor"]
    fn injected_positions_land_on_the_exact_pixel() {
        let platform = native().unwrap();
        let start = cursor_position();
        let monitors = platform.monitors();
        let mut wrong = Vec::new();
        for monitor in &monitors {
            let b = monitor.bounds;
            for wanted in [
                Point::new(b.x, b.y),
                Point::new(b.x + 123.0, b.y + 77.0),
                Point::new(b.x + b.width / 2.0, b.y + b.height / 2.0),
                Point::new(b.right() - 1.0, b.bottom() - 1.0),
            ] {
                platform.inject(&InputEvent::PointerPosition {
                    x: wanted.x,
                    y: wanted.y,
                });
                // Windows delivers injected input a moment later.
                let mut landed = cursor_position();
                for _ in 0..50 {
                    if landed == wanted {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                    landed = cursor_position();
                }
                if landed != wanted {
                    wrong.push((wanted, landed));
                }
            }
        }
        platform.restore_cursor(start);
        assert!(!monitors.is_empty());
        assert!(wrong.is_empty(), "wanted, landed: {wrong:?}");
    }

    #[test]
    #[ignore = "moves the real cursor and installs real input hooks"]
    fn parked_cursor_is_invisible_until_restored() {
        let platform = native().unwrap();
        let start = cursor_position();
        let capture = platform.start_capture(Box::new(|_| false)).unwrap();

        let park = platform.park_point();
        platform.park_cursor(park);
        settle();
        let at_park = cursor_position();
        let hidden = !cursor_visible();

        platform.restore_cursor(start);
        settle();
        let back = cursor_position();
        let visible_again = cursor_visible();
        drop(capture);

        assert_eq!(at_park, park);
        assert_eq!(back, start);
        assert!(hidden, "the parked cursor should be hidden");
        assert!(visible_again, "the restored cursor should be visible");
    }

    #[test]
    #[ignore = "moves the real cursor and installs real input hooks"]
    fn the_hook_ignores_input_this_program_injected() {
        let platform = native().unwrap();
        let start = cursor_position();
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        let capture = platform
            .start_capture(Box::new(move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }))
            .unwrap();
        settle();

        // Without this filter, replaying a peer's input would feed back into the
        // switcher and could send it straight back to the peer.
        for offset in 0..20 {
            platform.inject(&InputEvent::PointerPosition {
                x: start.x + f64::from(offset),
                y: start.y,
            });
        }
        settle();
        let count = seen.load(Ordering::Relaxed);
        drop(capture);
        platform.restore_cursor(start);

        // A few real mouse movements may slip in if someone's using the machine.
        assert!(count < 5, "the hook saw {count} injected events");
    }

    #[test]
    #[ignore = "installs real input hooks"]
    fn hooks_can_be_stopped_and_started_again() {
        let platform = native().unwrap();
        for _ in 0..3 {
            let capture = platform.start_capture(Box::new(|_| false)).unwrap();
            settle();
            drop(capture);
        }
    }
}
