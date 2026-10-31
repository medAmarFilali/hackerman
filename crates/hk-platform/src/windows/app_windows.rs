//! Bringing an app's windows to the front, and moving them between screens.

use std::collections::HashMap;

use hk_core::apps::{AppAction, place_on_other_screen};
use hk_core::geometry::Rect;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_MOUSE, SendInput};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowPlacement, IsHungAppWindow, IsIconic, SW_RESTORE,
    SW_SHOWMAXIMIZED, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, SetForegroundWindow,
    SetWindowPlacement, ShowWindowAsync, SwitchToThisWindow, WINDOWPLACEMENT,
};

use super::{apps, displays};
use crate::Error;

pub fn run(app: &str, action: &AppAction) -> Result<(), Error> {
    match action {
        AppAction::BringUp { monitor } => bring_up(app, monitor.as_deref()),
        AppAction::MoveTo { from, to } => move_to(app, from, to),
    }
}

/// Which monitor each of an app's windows is on, frontmost first.
fn windows_of(app: &str, monitors: &HashMap<isize, String>) -> Vec<(HWND, Option<String>)> {
    apps::app_windows()
        .into_iter()
        .filter(|found| found.app == app)
        .map(|found| {
            // SAFETY: a window that has gone since is reported as the nearest monitor or none. A
            // minimized one counts as being where it was before it was minimized.
            let handle = unsafe { MonitorFromWindow(found.window, MONITOR_DEFAULTTONEAREST) };
            (found.window, monitors.get(&(handle.0 as isize)).cloned())
        })
        .collect()
}

fn bring_up(app: &str, monitor: Option<&str>) -> Result<(), Error> {
    let windows = windows_of(app, &displays::ids_by_handle());
    let chosen = windows
        .iter()
        .find(|(_, on)| on.as_deref() == monitor && monitor.is_some())
        .or_else(|| windows.first());
    let Some(&(window, _)) = chosen else {
        return Err(Error::AppWindows(format!("{app} has no window to show")));
    };
    // SAFETY: these only ask things of a window, and a window that has gone since isn't an
    // error that matters. The input is an empty mouse movement, the size of what's given.
    unsafe {
        if IsIconic(window).as_bool() {
            let _ = ShowWindowAsync(window, SW_RESTORE);
        }
        // Windows only lets the program that last had input choose what goes in front, in case
        // it's something the user didn't ask for. This was the user asking, on another computer
        // or in Hackerman's window, and input this program sends itself counts.
        SendInput(
            &[INPUT {
                r#type: INPUT_MOUSE,
                ..Default::default()
            }],
            size_of::<INPUT>() as i32,
        );
        if !SetForegroundWindow(window).as_bool() || GetForegroundWindow() != window {
            // It said no. This one doesn't ask.
            SwitchToThisWindow(window, true);
        }
    }
    Ok(())
}

fn move_to(app: &str, from: &str, to: &str) -> Result<(), Error> {
    let monitors = displays::ids_by_handle();
    let area_of = |id: &str| {
        monitors
            .iter()
            .find(|(_, known)| known.as_str() == id)
            .and_then(|(&handle, _)| Area::of(HMONITOR(handle as *mut _)))
    };
    let (Some(from_area), Some(to_area)) = (area_of(from), area_of(to)) else {
        return Err(Error::AppWindows("that screen isn't there".to_string()));
    };

    let windows: Vec<HWND> = windows_of(app, &monitors)
        .into_iter()
        .filter(|(_, on)| on.as_deref() == Some(from))
        .map(|(window, _)| window)
        .collect();
    if windows.is_empty() {
        return Err(Error::AppWindows(format!(
            "{app} has no window on that screen"
        )));
    }
    let mut last_error = None;
    let mut moved = 0;
    // From the back to the front, so that they end up in the same order.
    for &window in windows.iter().rev() {
        match move_window(window, &from_area, &to_area) {
            Ok(()) => moved += 1,
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        // An administrator's window can't be moved by an ordinary program, and it shouldn't stop
        // the others.
        Some(error) if moved == 0 => Err(error),
        _ => Ok(()),
    }
}

/// Going through a window's "placement" covers all of its states with one call. A maximized
/// window fills the screen its normal position is on, and a minimized one comes back there. Its
/// size isn't scaled here for a screen with other scaling: Windows tells the window about that as
/// it arrives, and an app that knows about it then scales itself.
fn move_window(window: HWND, from: &Area, to: &Area) -> Result<(), Error> {
    let fail = |error: windows::core::Error| Error::AppWindows(error.to_string());
    let mut placement = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: only asks whether it's stuck, which would hold up what comes next indefinitely.
    if unsafe { IsHungAppWindow(window) }.as_bool() {
        return Err(Error::AppWindows("that app isn't responding".to_string()));
    }
    // SAFETY: the length is set as Windows asks, and it fills in the rest.
    unsafe { GetWindowPlacement(window, &raw mut placement) }.map_err(fail)?;

    // It keeps the state it's in, and doesn't take the keyboard from whatever has it, unless
    // it's maximized. Windows has no way of maximizing a window without that.
    let state = placement.showCmd as i32;
    placement.showCmd = if state == SW_SHOWMAXIMIZED.0 {
        SW_SHOWMAXIMIZED.0 as u32
    } else if state == SW_SHOWMINNOACTIVE.0 || unsafe { IsIconic(window) }.as_bool() {
        SW_SHOWMINNOACTIVE.0 as u32
    } else {
        SW_SHOWNOACTIVATE.0 as u32
    };

    let window_area = from.to_screen(placement.rcNormalPosition);
    let placed = place_on_other_screen(window_area, from.work, to.work);
    placement.rcNormalPosition = to.normal_position_of(placed);
    // SAFETY: the structure was filled in by Windows, with only these two members changed.
    unsafe { SetWindowPlacement(window, &raw const placement) }.map_err(fail)
}

/// Where a screen is. A window's normal position counts from the corner of the usable area of the
/// screen, leaving out a taskbar at its top or left, rather than from the corner of the screen.
struct Area {
    work: Rect,
    /// How far the usable area's corner is from the screen's.
    offset: (i32, i32),
}

impl Area {
    fn of(monitor: HMONITOR) -> Option<Self> {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: the size is set as Windows asks, and it fills in the rest.
        unsafe { GetMonitorInfoW(monitor, &raw mut info) }
            .as_bool()
            .then(|| Self {
                work: rect_of(info.rcWork),
                offset: (
                    info.rcWork.left - info.rcMonitor.left,
                    info.rcWork.top - info.rcMonitor.top,
                ),
            })
    }

    fn to_screen(&self, normal: RECT) -> Rect {
        let rect = rect_of(normal);
        Rect::new(
            rect.x + f64::from(self.offset.0),
            rect.y + f64::from(self.offset.1),
            rect.width,
            rect.height,
        )
    }

    fn normal_position_of(&self, rect: Rect) -> RECT {
        let left = rect.x as i32 - self.offset.0;
        let top = rect.y as i32 - self.offset.1;
        RECT {
            left,
            top,
            right: left + rect.width as i32,
            bottom: top + rect.height as i32,
        }
    }
}

fn rect_of(rect: RECT) -> Rect {
    Rect::new(
        f64::from(rect.left),
        f64::from(rect.top),
        f64::from(rect.right - rect.left),
        f64::from(rect.bottom - rect.top),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_position_counts_from_the_usable_area() {
        // A taskbar 40 pixels high along the top of this screen.
        let area = Area {
            work: Rect::new(1920.0, 40.0, 1920.0, 1040.0),
            offset: (0, 40),
        };
        let normal = RECT {
            left: 2000,
            top: 100,
            right: 2800,
            bottom: 700,
        };
        let on_screen = area.to_screen(normal);
        assert_eq!(on_screen, Rect::new(2000.0, 140.0, 800.0, 600.0));
        let back = area.normal_position_of(on_screen);
        assert_eq!(
            (back.left, back.top, back.right, back.bottom),
            (2000, 100, 2800, 700)
        );
    }

    fn pump_messages() {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW,
        };
        let mut message = MSG::default();
        // SAFETY: the message is a plain structure for Windows to fill in, and is only passed on.
        unsafe {
            while PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&raw const message);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    /// A real window going through the real calls, between two made-up areas of one real screen,
    /// as a computer with one screen can't do more. It shows up on screen, so run it on a desktop
    /// nobody's looking at:
    /// `cargo test -p hk-platform --lib real_window -- --ignored --test-threads=1`
    #[test]
    #[ignore = "opens a real window"]
    fn a_real_window_moves_in_any_state_and_stays_in_that_state() {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, IsZoomed, SW_MINIMIZE, SW_RESTORE, SW_SHOWMAXIMIZED,
            ShowWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
        };
        use windows::core::w;

        let _platform = crate::imp::native().unwrap();
        // SAFETY: a window of a class that's always there, with nothing pointing into this.
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Hackerman test"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                300,
                200,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        let placement = || {
            let mut placement = WINDOWPLACEMENT {
                length: size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };
            // SAFETY: the length is set as Windows asks, and it fills in the rest.
            unsafe { GetWindowPlacement(window, &raw mut placement) }.unwrap();
            placement
        };
        let area = |x: f64, y: f64| Area {
            work: Rect::new(x, y, 800.0, 600.0),
            offset: (0, 0),
        };
        let normal_spot = || {
            let rect = placement().rcNormalPosition;
            (
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
            )
        };

        // Normal, from the top left of one area to the top left of another.
        move_window(window, &area(100.0, 100.0), &area(200.0, 50.0)).unwrap();
        assert_eq!(normal_spot(), (200, 50, 300, 200));
        // SAFETY: only reads a flag.
        assert!(!unsafe { IsIconic(window) }.as_bool());

        // Minimized, and still minimized afterwards, coming back in the new place.
        // SAFETY: the window is this test's own.
        let _ = unsafe { ShowWindow(window, SW_MINIMIZE) };
        move_window(window, &area(200.0, 50.0), &area(120.0, 80.0)).unwrap();
        // SAFETY: as above.
        assert!(unsafe { IsIconic(window) }.as_bool());
        assert_eq!(normal_spot(), (120, 80, 300, 200));
        let _ = unsafe { ShowWindow(window, SW_RESTORE) };
        assert!(!unsafe { IsIconic(window) }.as_bool());
        assert_eq!(normal_spot(), (120, 80, 300, 200));

        // Maximized, and still maximized afterwards.
        let _ = unsafe { ShowWindow(window, SW_SHOWMAXIMIZED) };
        move_window(window, &area(120.0, 80.0), &area(60.0, 40.0)).unwrap();
        assert!(unsafe { IsZoomed(window) }.as_bool());
        assert_eq!(normal_spot(), (60, 40, 300, 200));

        // The whole way through, as the agent does it, finding the window by its program. On a
        // desktop nobody's looking at nothing can come to the front, but it can be restored.
        let me = std::env::current_exe().unwrap();
        let app = me.file_stem().unwrap().to_str().unwrap().to_lowercase();
        let _ = unsafe { ShowWindow(window, SW_MINIMIZE) };
        run(&app, &AppAction::BringUp { monitor: None }).unwrap();
        let start = std::time::Instant::now();
        while unsafe { IsIconic(window) }.as_bool() && start.elapsed().as_secs() < 5 {
            // Restoring is asked for without waiting, and this thread is the window's own.
            pump_messages();
        }
        assert!(!unsafe { IsIconic(window) }.as_bool(), "it was restored");

        // This computer's one real screen, from itself to itself, as there's no other.
        let monitors = displays::ids_by_handle();
        let screen = monitors.values().next().unwrap();
        run(
            &app,
            &AppAction::MoveTo {
                from: screen.clone(),
                to: screen.clone(),
            },
        )
        .unwrap();
        assert!(
            run(
                &app,
                &AppAction::MoveTo {
                    from: "no such screen".into(),
                    to: screen.clone(),
                },
            )
            .is_err()
        );

        let _ = unsafe { DestroyWindow(window) };
    }

    #[test]
    fn asking_about_an_app_that_is_not_there_says_so() {
        let action = AppAction::BringUp { monitor: None };
        assert!(run("no-such-app-here", &action).is_err());
        let action = AppAction::MoveTo {
            from: "no such screen".into(),
            to: "nor this one".into(),
        };
        assert!(run("no-such-app-here", &action).is_err());
    }
}
